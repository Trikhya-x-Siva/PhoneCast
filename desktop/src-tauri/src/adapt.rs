//! Dynamic bitrate for a live destination.
//!
//! Each destination has a short quality ladder that starts at the quality
//! the streamer chose (YouTube 1440p60: 20 Mbps → 14 Mbps → 1080p60 9 Mbps).
//! When the upload cannot keep up, FFmpeg's output speed falls below real
//! time and frames pile up; a few seconds later viewers see a stall. This
//! state machine turns speed readings into ladder moves: after 5 s below
//! 0.95x it steps down one rung, after 60 s of smooth output it steps back up.
//!
//! FFmpeg cannot change an encoder's bitrate or size while it runs, so every
//! move restarts that one destination (the caller does that) and viewers see
//! a short glitch. To keep those rare there is never more than one move per
//! 60 s, and readings are ignored for a settle period after any (re)start.
//! The other destination keeps streaming untouched.

use std::time::{Duration, Instant};

/// Below this output speed the upload is considered to be struggling.
pub const SLOW_SPEED: f64 = 0.95;
/// How long the struggle must last before stepping down.
pub const SLOW_FOR: Duration = Duration::from_secs(5);
/// How long the stream must be smooth before stepping back up.
pub const STABLE_FOR: Duration = Duration::from_secs(60);
/// Readings are ignored for this long after a (re)start: the encoder's
/// start-up burst says nothing about the upload.
pub const SETTLE: Duration = Duration::from_secs(10);
/// At most one ladder move (one viewer-visible restart) per this period.
pub const MIN_GAP: Duration = Duration::from_secs(60);

/// One quality step: output size and video bitrate.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Rung {
    pub w: u32,
    pub h: u32,
    pub kbps: u32,
}

impl Rung {
    /// "1080p60 · 9.0 Mbps"
    pub fn label(&self) -> String {
        format!("{}p60 · {}", self.h, mbps_text(self.kbps))
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Change {
    /// Move down to this rung.
    Down(Rung),
    /// Move back up to this rung.
    Up(Rung),
}

pub struct Adapt {
    ladder: Vec<Rung>,
    level: usize,
    slow_since: Option<Instant>,
    stable_since: Option<Instant>,
    settle_until: Option<Instant>,
    last_change: Option<Instant>,
}

impl Adapt {
    /// `ladder[0]` is the chosen quality; later rungs are lower. A ladder of
    /// one rung never changes.
    pub fn new(ladder: Vec<Rung>) -> Self {
        assert!(!ladder.is_empty(), "a ladder needs at least one rung");
        Self {
            ladder,
            level: 0,
            slow_since: None,
            stable_since: None,
            settle_until: None,
            last_change: None,
        }
    }

    /// The rung the destination should run at right now.
    pub fn rung(&self) -> Rung {
        self.ladder[self.level]
    }

    /// True while running below the chosen quality.
    pub fn reduced(&self) -> bool {
        self.level > 0
    }

    /// Call after the encoder was (re)started, for any reason, so readings
    /// from the old process and the start-up burst are ignored.
    pub fn restarted(&mut self, now: Instant) {
        self.settle_until = Some(now + SETTLE);
        self.slow_since = None;
        self.stable_since = None;
    }

    /// Feed one reading of the recent output speed (1.0 = real time).
    /// `None` = no reading yet.
    pub fn observe(&mut self, speed: Option<f64>, now: Instant) -> Option<Change> {
        let speed = speed?;
        if self.settle_until.is_some_and(|t| now < t) {
            return None;
        }
        let gap_ok = self
            .last_change
            .is_none_or(|t| now.duration_since(t) >= MIN_GAP);
        if speed < SLOW_SPEED {
            self.stable_since = None;
            let since = *self.slow_since.get_or_insert(now);
            if now.duration_since(since) >= SLOW_FOR && gap_ok && self.level + 1 < self.ladder.len()
            {
                self.level += 1;
                self.changed(now);
                return Some(Change::Down(self.rung()));
            }
        } else {
            self.slow_since = None;
            if self.level > 0 {
                let since = *self.stable_since.get_or_insert(now);
                if now.duration_since(since) >= STABLE_FOR && gap_ok {
                    self.level -= 1;
                    self.changed(now);
                    return Some(Change::Up(self.rung()));
                }
            }
        }
        None
    }

    fn changed(&mut self, now: Instant) {
        self.last_change = Some(now);
        self.restarted(now);
    }
}

/// Output speed over the last few seconds, from FFmpeg's `out_time_us`.
///
/// FFmpeg's own `speed` is an average since the process started, so after an
/// hour of good streaming a congested upload would take many minutes to show
/// up in it. This measures only the recent window instead.
pub struct SpeedMeter {
    window: Duration,
    samples: std::collections::VecDeque<(Instant, i64)>,
}

impl SpeedMeter {
    pub fn new(window: Duration) -> Self {
        Self {
            window,
            samples: std::collections::VecDeque::new(),
        }
    }

    /// Records a progress report: `out_us` of media written by `now`.
    /// Returns the speed over the window once it spans at least half of it.
    pub fn add(&mut self, now: Instant, out_us: i64) -> Option<f64> {
        self.samples.push_back((now, out_us));
        while self
            .samples
            .front()
            .is_some_and(|(t, _)| now.duration_since(*t) > self.window)
        {
            self.samples.pop_front();
        }
        let (t0, o0) = *self.samples.front()?;
        let wall = now.duration_since(t0);
        if wall < self.window / 2 {
            return None;
        }
        Some((out_us - o0).max(0) as f64 / wall.as_micros() as f64)
    }
}

/// "6.0 Mbps" style text for the UI and the log.
pub fn mbps_text(kbps: u32) -> String {
    format!("{:.1} Mbps", kbps as f64 / 1000.0)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn secs(s: u64) -> Duration {
        Duration::from_secs(s)
    }

    /// The YouTube ladder from the chosen 1440p60 preset.
    fn youtube() -> Vec<Rung> {
        vec![
            Rung {
                w: 2560,
                h: 1440,
                kbps: 20_000,
            },
            Rung {
                w: 2560,
                h: 1440,
                kbps: 14_000,
            },
            Rung {
                w: 1920,
                h: 1080,
                kbps: 9_000,
            },
        ]
    }

    /// Feeds one reading per second from `from` to `to` (exclusive) and
    /// returns the changes with the second they happened at.
    fn run(a: &mut Adapt, t0: Instant, from: u64, to: u64, speed: f64) -> Vec<(u64, Change)> {
        (from..to)
            .filter_map(|i| a.observe(Some(speed), t0 + secs(i)).map(|c| (i, c)))
            .collect()
    }

    #[test]
    fn steps_down_only_after_five_slow_seconds() {
        let t0 = Instant::now();
        let mut a = Adapt::new(youtube());
        assert_eq!(a.rung().kbps, 20_000);
        assert!(run(&mut a, t0, 0, 5, 0.9).is_empty());
        assert_eq!(
            a.observe(Some(0.9), t0 + secs(5)),
            Some(Change::Down(youtube()[1]))
        );
        assert!(a.reduced());
        assert_eq!(a.rung().kbps, 14_000);
    }

    #[test]
    fn slightly_slow_is_not_struggling() {
        let t0 = Instant::now();
        let mut a = Adapt::new(youtube());
        // 0.96x (e.g. a brief Wi-Fi wobble) never triggers a restart.
        assert!(run(&mut a, t0, 0, 300, 0.96).is_empty());
    }

    #[test]
    fn a_good_reading_resets_the_slow_timer() {
        let t0 = Instant::now();
        let mut a = Adapt::new(youtube());
        assert!(run(&mut a, t0, 0, 4, 0.9).is_empty());
        assert_eq!(a.observe(Some(1.0), t0 + secs(4)), None);
        assert!(run(&mut a, t0, 5, 10, 0.9).is_empty());
        assert_eq!(
            a.observe(Some(0.9), t0 + secs(10)),
            Some(Change::Down(youtube()[1]))
        );
    }

    #[test]
    fn walks_the_youtube_ladder_at_most_once_a_minute() {
        let t0 = Instant::now();
        let mut a = Adapt::new(youtube());
        // A badly congested upload for five minutes.
        let changes = run(&mut a, t0, 0, 300, 0.5);
        assert_eq!(
            changes,
            vec![
                (5, Change::Down(youtube()[1])),
                (65, Change::Down(youtube()[2]))
            ]
        );
        // Bottom rung: 1080p60 at 9 Mbps, and it stays there.
        assert_eq!(
            a.rung(),
            Rung {
                w: 1920,
                h: 1080,
                kbps: 9_000
            }
        );
        assert!(changes.windows(2).all(|w| w[1].0 - w[0].0 >= 60));
    }

    #[test]
    fn steps_back_up_after_a_stable_minute_one_rung_at_a_time() {
        let t0 = Instant::now();
        let mut a = Adapt::new(youtube());
        run(&mut a, t0, 0, 70, 0.5); // down twice: at 5 s and 65 s
        assert_eq!(a.rung(), youtube()[2]);
        // Upload recovers at 70 s. Settle until 75 s, stable from 75 s on.
        let ups = run(&mut a, t0, 70, 400, 1.0);
        assert_eq!(
            ups,
            vec![
                (135, Change::Up(youtube()[1])),
                (205, Change::Up(youtube()[0]))
            ]
        );
        assert!(!a.reduced());
        // At full quality nothing more happens.
        assert!(run(&mut a, t0, 400, 600, 1.0).is_empty());
    }

    #[test]
    fn readings_during_settle_are_ignored() {
        let t0 = Instant::now();
        let mut a = Adapt::new(youtube());
        a.restarted(t0); // e.g. a reconnect
        assert!(run(&mut a, t0, 0, 10, 0.1).is_empty());
        // Slow timer starts at 10 s, so the move is at 15 s.
        assert!(run(&mut a, t0, 10, 15, 0.1).is_empty());
        assert_eq!(
            a.observe(Some(0.1), t0 + secs(15)),
            Some(Change::Down(youtube()[1]))
        );
    }

    #[test]
    fn one_rung_never_changes_and_no_reading_is_ignored() {
        let t0 = Instant::now();
        let mut a = Adapt::new(vec![youtube()[0]]);
        assert!(run(&mut a, t0, 0, 300, 0.2).is_empty());
        let mut b = Adapt::new(youtube());
        for i in 0..20 {
            assert_eq!(b.observe(None, t0 + secs(i)), None);
        }
        assert_eq!(mbps_text(9_000), "9.0 Mbps");
        assert_eq!(youtube()[2].label(), "1080p60 · 9.0 Mbps");
    }

    #[test]
    fn speed_meter_sees_recent_congestion_after_a_long_good_stream() {
        let t0 = Instant::now();
        let mut m = SpeedMeter::new(secs(3));
        let mut out = 0i64;
        let mut last = None;
        // One hour at real time...
        for i in 0..3600 {
            out += 1_000_000;
            last = m.add(t0 + secs(i), out);
        }
        assert!((last.unwrap() - 1.0).abs() < 0.01);
        // ...then the upload only manages half speed: seen within seconds.
        for i in 3600..3604 {
            out += 500_000;
            last = m.add(t0 + secs(i), out);
        }
        assert!((last.unwrap() - 0.5).abs() < 0.01, "{last:?}");
        // A fresh meter needs half a window of reports before it answers.
        let mut fresh = SpeedMeter::new(secs(3));
        assert_eq!(fresh.add(t0, 0), None);
        assert_eq!(fresh.add(t0 + secs(1), 1_000_000), None);
        assert!(fresh.add(t0 + secs(2), 2_000_000).is_some());
    }
}
