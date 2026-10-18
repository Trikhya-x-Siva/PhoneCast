//! Dynamic bitrate, modelled on OBS (`rtmp-stream.c`), across destinations.
//!
//! Per destination: when the send queue holds more than 200 ms of media,
//! cut the bitrate to what the connection actually carried over the last
//! ~1.5 s (minus audio, rounded down to 100 kbps). After 4 s without
//! congestion, raise it by 10 % of the original every 4 s. If the queue
//! still grows, the sender drops frames (disposable at 700 ms, all
//! non-keyframes at 900 ms); audio and keyframes are never dropped.
//!
//! Across destinations (the user's rule): the MAIN destination keeps its
//! quality. When it congests, the secondary destinations are lowered first
//! (they share the same upload); the main one is only cut once every
//! secondary is at its floor and a grace period has passed. Secondaries
//! only climb back once the main one is at full quality.

use super::Priority;
use std::collections::VecDeque;
use std::time::{Duration, Instant};

#[derive(Clone, Debug)]
pub struct DbrConfig {
    /// Queue length (DTS span) that counts as congestion.
    pub trigger_ms: u64,
    /// Calm time before (and between) increases.
    pub inc_interval: Duration,
    /// Increase step as a fraction of the original bitrate.
    pub inc_fraction: f64,
    /// Throughput measuring window.
    pub window: Duration,
    /// Drop disposable (B) frames above this backlog.
    pub drop_disposable_ms: u64,
    /// Drop every non-keyframe above this backlog.
    pub drop_ref_ms: u64,
    /// Never go below this fraction of the original bitrate...
    pub floor_fraction: f64,
    /// ...or below this (kbps).
    pub min_kbps: u32,
    /// Minimum time between two cuts of one destination (each change
    /// costs an IDR, and the queue needs time to drain).
    pub cut_hold: Duration,
    /// After lowering secondaries for the main destination, how long to
    /// wait for that to help before the main one may be cut.
    pub main_grace: Duration,
}

impl Default for DbrConfig {
    fn default() -> Self {
        DbrConfig {
            trigger_ms: 200,
            inc_interval: Duration::from_secs(4),
            inc_fraction: 0.10,
            window: Duration::from_millis(1500),
            drop_disposable_ms: 700,
            drop_ref_ms: 900,
            floor_fraction: 0.2,
            min_kbps: 500,
            cut_hold: Duration::from_secs(1),
            main_grace: Duration::from_secs(2),
        }
    }
}

/// Frames below the returned priority should be dropped from the queue.
pub fn drop_priority(backlog_ms: u64, cfg: &DbrConfig) -> Option<Priority> {
    if backlog_ms >= cfg.drop_ref_ms {
        Some(Priority::Key)
    } else if backlog_ms >= cfg.drop_disposable_ms {
        Some(Priority::Ref)
    } else {
        None
    }
}

/// Bytes actually handed to the network over a sliding window.
pub struct Meter {
    window: Duration,
    since: Instant,
    samples: VecDeque<(Instant, u64)>,
}

impl Meter {
    pub fn new(window: Duration, now: Instant) -> Self {
        Meter {
            window,
            since: now,
            samples: VecDeque::new(),
        }
    }

    /// Restart measuring (new connection).
    pub fn reset(&mut self, now: Instant) {
        self.since = now;
        self.samples.clear();
    }

    pub fn add(&mut self, now: Instant, bytes: u64) {
        self.samples.push_back((now, bytes));
        self.trim(now);
    }

    fn trim(&mut self, now: Instant) {
        while self
            .samples
            .front()
            .is_some_and(|(t, _)| now.duration_since(*t) > self.window)
        {
            self.samples.pop_front();
        }
    }

    /// kbps over the window; None until half a window has been measured.
    pub fn kbps(&mut self, now: Instant) -> Option<u32> {
        self.trim(now);
        let span = now.duration_since(self.since).min(self.window);
        if span < self.window / 2 {
            return None;
        }
        let bytes: u64 = self.samples.iter().map(|(_, b)| b).sum();
        Some((bytes as f64 * 8.0 / span.as_secs_f64() / 1000.0) as u32)
    }
}

/// What a destination's sender reports each tick.
#[derive(Clone, Copy, Debug, Default)]
pub struct Observation {
    pub connected: bool,
    pub backlog_ms: u64,
    pub sent_kbps: Option<u32>,
}

#[derive(Clone, Debug)]
pub struct DestDbr {
    pub orig: u32,
    pub cur: u32,
    pub main: bool,
    last_cut: Option<Instant>,
    next_inc: Option<Instant>,
}

impl DestDbr {
    fn floor(&self, cfg: &DbrConfig) -> u32 {
        round_down(((self.orig as f64 * cfg.floor_fraction) as u32).max(cfg.min_kbps))
            .min(self.orig)
    }

    fn can_cut(&self, now: Instant, cfg: &DbrConfig) -> bool {
        self.last_cut.is_none_or(|t| now >= t + cfg.cut_hold)
    }
}

fn round_down(kbps: u32) -> u32 {
    kbps / 100 * 100
}

/// Bitrate targets for every destination of one go-live session.
pub struct Arbiter {
    pub cfg: DbrConfig,
    pub dests: Vec<DestDbr>,
    audio_kbps: u32,
    grace_until: Option<Instant>,
}

impl Arbiter {
    /// `dests`: (original kbps, is main). At most one should be main.
    pub fn new(cfg: DbrConfig, dests: &[(u32, bool)], audio_kbps: u32) -> Self {
        Arbiter {
            cfg,
            dests: dests
                .iter()
                .map(|&(orig, main)| DestDbr {
                    orig,
                    cur: orig,
                    main,
                    last_cut: None,
                    next_inc: None,
                })
                .collect(),
            audio_kbps,
            grace_until: None,
        }
    }

    pub fn targets(&self) -> Vec<u32> {
        self.dests.iter().map(|d| d.cur).collect()
    }

    fn main(&self) -> Option<usize> {
        self.dests.iter().position(|d| d.main)
    }

    /// Feeds one round of observations; returns true if any target changed.
    // Index loops: each step reads `obs[i]` and may touch other destinations.
    #[allow(clippy::needless_range_loop)]
    pub fn tick(&mut self, now: Instant, obs: &[Observation]) -> bool {
        let cfg = self.cfg.clone();
        let main = self.main();
        let mut changed = false;
        let congested = |i: usize| obs[i].connected && obs[i].backlog_ms >= cfg.trigger_ms;

        for i in 0..self.dests.len() {
            if !congested(i) {
                continue;
            }
            self.dests[i].next_inc = Some(now + cfg.inc_interval);
            if Some(i) == main {
                // While the main one struggles, secondaries stay down too.
                for d in self.dests.iter_mut().filter(|d| !d.main) {
                    d.next_inc = Some(now + cfg.inc_interval);
                }
            }
            let Some(sent) = obs[i].sent_kbps else {
                continue;
            };
            let d = &self.dests[i];
            if !d.can_cut(now, &cfg) {
                continue;
            }
            let est = round_down(sent.saturating_sub(self.audio_kbps)).max(d.floor(&cfg));
            if est >= d.cur {
                continue;
            }
            if Some(i) == main {
                let need = d.cur - est;
                let with_room: Vec<usize> = (0..self.dests.len())
                    .filter(|&j| j != i && self.dests[j].cur > self.dests[j].floor(&cfg))
                    .collect();
                if !with_room.is_empty() {
                    self.cut_secondaries(now, need, &with_room);
                    changed = true;
                    continue;
                }
                if self.grace_until.is_some_and(|g| now < g) {
                    continue; // give the secondary cuts time to work
                }
            }
            let d = &mut self.dests[i];
            d.cur = est;
            d.last_cut = Some(now);
            changed = true;
        }
        if main.is_some_and(|m| !congested(m)) {
            self.grace_until = None;
        }

        let main_full = main.is_none_or(|m| self.dests[m].cur >= self.dests[m].orig);
        for i in 0..self.dests.len() {
            if !obs[i].connected || congested(i) {
                continue;
            }
            let d = &mut self.dests[i];
            if d.cur >= d.orig {
                d.next_inc = None;
                continue;
            }
            let next = *d.next_inc.get_or_insert(now + cfg.inc_interval);
            if now < next || (!d.main && !main_full) {
                continue;
            }
            let step = ((d.orig as f64 * cfg.inc_fraction) as u32).max(100);
            d.cur = (d.cur + step).min(d.orig);
            d.next_inc = Some(now + cfg.inc_interval);
            changed = true;
        }
        changed
    }

    /// Lowers secondaries (highest first) by `need` kbps in total.
    fn cut_secondaries(&mut self, now: Instant, mut need: u32, with_room: &[usize]) {
        let cfg = self.cfg.clone();
        let mut order = with_room.to_vec();
        order.sort_by_key(|&j| std::cmp::Reverse(self.dests[j].cur));
        for j in order {
            let d = &mut self.dests[j];
            if need == 0 || !d.can_cut(now, &cfg) {
                continue;
            }
            let floor = d.floor(&cfg);
            let cut = need.min(d.cur - floor);
            // Round the new value down, so the cut is at least `cut`.
            let new = round_down(d.cur - cut).max(floor);
            need = need.saturating_sub(d.cur - new);
            d.cur = new;
            d.last_cut = Some(now);
        }
        self.grace_until = Some(now + cfg.main_grace);
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn obs(backlog_ms: u64, sent: u32) -> Observation {
        Observation {
            connected: true,
            backlog_ms,
            sent_kbps: Some(sent),
        }
    }

    fn at(t0: Instant, ms: u64) -> Instant {
        t0 + Duration::from_millis(ms)
    }

    #[test]
    fn steps_follow_obs_timing() {
        let t0 = Instant::now();
        let mut a = Arbiter::new(DbrConfig::default(), &[(6000, true)], 160);
        // Below the trigger: nothing happens.
        assert!(!a.tick(at(t0, 0), &[obs(150, 3000)]));
        assert_eq!(a.targets(), vec![6000]);
        // 250 ms queued while 4260 kbps went out → 4100 (minus audio, rounded).
        assert!(a.tick(at(t0, 100), &[obs(250, 4260)]));
        assert_eq!(a.targets(), vec![4100]);
        // Still congested 0.5 s later: no second cut inside the 1 s hold.
        assert!(!a.tick(at(t0, 600), &[obs(300, 3000)]));
        assert_eq!(a.targets(), vec![4100]);
        // Calm from here on. The 4 s timer runs from the last congested tick.
        for ms in (700..4600).step_by(100) {
            a.tick(at(t0, ms), &[obs(20, 4200)]);
        }
        assert_eq!(a.targets(), vec![4100], "no increase before 4 s");
        a.tick(at(t0, 4600), &[obs(20, 4200)]);
        assert_eq!(a.targets(), vec![4700], "+10 % of 6000 at 4 s");
        a.tick(at(t0, 8500), &[obs(20, 4800)]);
        assert_eq!(a.targets(), vec![4700]);
        a.tick(at(t0, 8600), &[obs(20, 4800)]);
        assert_eq!(a.targets(), vec![5300]);
        a.tick(at(t0, 12600), &[obs(20, 5400)]);
        a.tick(at(t0, 16600), &[obs(20, 6000)]);
        assert_eq!(a.targets(), vec![6000], "never above the original");
        assert!(!a.tick(at(t0, 30000), &[obs(20, 6000)]));
    }

    #[test]
    fn cut_never_goes_below_floor() {
        let t0 = Instant::now();
        let mut a = Arbiter::new(DbrConfig::default(), &[(6000, true)], 160);
        a.tick(t0, &[obs(2000, 300)]);
        assert_eq!(a.targets(), vec![1200]); // 20 % of 6000
    }

    #[test]
    fn main_destination_keeps_quality() {
        let t0 = Instant::now();
        // YouTube main at 12 Mbps, Twitch 6 Mbps and Kick 6 Mbps secondary.
        let mut a = Arbiter::new(
            DbrConfig::default(),
            &[(12000, true), (6000, false), (6000, false)],
            160,
        );
        let calm = obs(10, 6000);
        // Main congests: it only gets 9 Mbps through. 3 Mbps must be freed.
        a.tick(at(t0, 0), &[obs(400, 9160), calm, calm]);
        let t = a.targets();
        assert_eq!(t[0], 12000, "main keeps its bitrate");
        assert_eq!(t[1] + t[2], 9000, "secondaries give up 3 Mbps together");
        // Still congested during the grace period: main still untouched.
        a.tick(at(t0, 1500), &[obs(500, 9160), calm, calm]);
        assert_eq!(a.targets()[0], 12000);
        // Secondaries at their floor (1200 each): the next cut lands on them
        // until nothing is left, then on main once grace has passed.
        for ms in (2600..12000).step_by(1100) {
            a.tick(at(t0, ms), &[obs(600, 4160), calm, calm]);
        }
        let t = a.targets();
        assert_eq!(&t[1..], &[1200, 1200]);
        assert_eq!(t[0], 4000, "main cut only after secondaries hit the floor");
        // Recovery: main climbs first; secondaries wait until main is full.
        let mut ms = 12000;
        while a.targets()[0] < 12000 {
            ms += 100;
            a.tick(at(t0, ms), &[obs(10, 12000), calm, calm]);
            assert_eq!(&a.targets()[1..], &[1200, 1200], "secondaries wait");
        }
        for _ in 0..50 {
            ms += 100;
            a.tick(at(t0, ms), &[obs(10, 12000), calm, calm]);
        }
        assert!(a.targets()[1] > 1200, "secondaries recover after main");
    }

    #[test]
    fn secondary_congestion_only_touches_secondary() {
        let t0 = Instant::now();
        let mut a = Arbiter::new(DbrConfig::default(), &[(12000, true), (6000, false)], 160);
        a.tick(t0, &[obs(10, 12000), obs(300, 3160)]);
        assert_eq!(a.targets(), vec![12000, 3000]);
    }

    #[test]
    fn drop_thresholds() {
        let c = DbrConfig::default();
        assert_eq!(drop_priority(650, &c), None);
        assert_eq!(drop_priority(700, &c), Some(Priority::Ref));
        assert_eq!(drop_priority(950, &c), Some(Priority::Key));
    }

    #[test]
    fn meter_measures_window() {
        let t0 = Instant::now();
        let mut m = Meter::new(Duration::from_millis(1000), t0);
        assert_eq!(m.kbps(t0), None);
        for i in 0..100 {
            m.add(at(t0, i * 10 + 10), 1250); // 1250 B per 10 ms = 1 Mbps
        }
        assert_eq!(m.kbps(at(t0, 1000)), Some(1000));
    }
}
