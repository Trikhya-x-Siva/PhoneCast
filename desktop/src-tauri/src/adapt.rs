//! Dynamic bitrate for a live destination.
//!
//! FFmpeg reports how fast it is getting data out (`speed`, 1.0x = real
//! time) and how many frames it had to drop. When the upload cannot keep up,
//! speed falls below 1x and frames start piling up — a few seconds later the
//! viewer sees a stall. This state machine turns those readings into bitrate
//! decisions: after 3 s of struggling, lower the bitrate by 20 %; after 60 s
//! of smooth output, step back up one notch. A settle period after every
//! change stops it from flapping while the encoder restarts.
//!
//! FFmpeg cannot change an encoder's bitrate while it runs, so each change
//! restarts that one destination (the caller does that); the other
//! destination keeps streaming untouched.

use std::time::{Duration, Instant};

/// Below this output speed the upload is considered to be struggling.
pub const SLOW_SPEED: f64 = 0.97;
/// How long the struggle must last before stepping down.
pub const SLOW_FOR: Duration = Duration::from_secs(3);
/// How long the stream must be smooth before stepping back up.
pub const STABLE_FOR: Duration = Duration::from_secs(60);
/// Readings are ignored for this long after a change (encoder restart).
pub const SETTLE: Duration = Duration::from_secs(10);
/// Each step lowers the bitrate to this fraction of the previous one.
pub const STEP: f64 = 0.8;
/// Never go below this fraction of the chosen quality (0.8⁴ ≈ 41 %).
pub const MAX_STEPS: u32 = 4;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Change {
    /// Lower the bitrate to this many kbps.
    Down(u32),
    /// Raise the bitrate to this many kbps.
    Up(u32),
}

pub struct Adapt {
    base_kbps: u32,
    level: u32,
    slow_since: Option<Instant>,
    stable_since: Option<Instant>,
    settle_until: Option<Instant>,
    last_dropped: u64,
}

impl Adapt {
    pub fn new(base_kbps: u32) -> Self {
        Self {
            base_kbps,
            level: 0,
            slow_since: None,
            stable_since: None,
            settle_until: None,
            last_dropped: 0,
        }
    }

    /// Bitrate the destination should run at right now.
    pub fn kbps(&self) -> u32 {
        kbps_at(self.base_kbps, self.level)
    }

    /// True while running below the chosen quality.
    pub fn reduced(&self) -> bool {
        self.level > 0
    }

    /// Call this after the encoder was (re)started so readings from the old
    /// process and the start-up burst are ignored.
    pub fn restarted(&mut self, now: Instant) {
        self.settle_until = Some(now + SETTLE);
        self.slow_since = None;
        self.stable_since = None;
    }

    /// Feed one progress report. `speed` is FFmpeg's speed (0 = unknown yet),
    /// `dropped` its cumulative drop_frames counter.
    pub fn observe(&mut self, speed: f64, dropped: u64, now: Instant) -> Option<Change> {
        let newly_dropped = dropped > self.last_dropped;
        self.last_dropped = dropped;
        if speed <= 0.0 {
            return None; // no reading yet
        }
        if self.settle_until.is_some_and(|t| now < t) {
            return None;
        }
        let struggling = speed < SLOW_SPEED || newly_dropped;
        if struggling {
            self.stable_since = None;
            let since = *self.slow_since.get_or_insert(now);
            if now.duration_since(since) >= SLOW_FOR && self.level < MAX_STEPS {
                self.level += 1;
                self.restarted(now);
                return Some(Change::Down(self.kbps()));
            }
        } else {
            self.slow_since = None;
            if self.level > 0 {
                let since = *self.stable_since.get_or_insert(now);
                if now.duration_since(since) >= STABLE_FOR {
                    self.level -= 1;
                    self.restarted(now);
                    return Some(Change::Up(self.kbps()));
                }
            }
        }
        None
    }
}

fn kbps_at(base: u32, level: u32) -> u32 {
    let v = base as f64 * STEP.powi(level as i32);
    ((v / 100.0).round() as u32) * 100
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

    #[test]
    fn steps_down_only_after_three_slow_seconds() {
        let t0 = Instant::now();
        let mut a = Adapt::new(6000);
        assert_eq!(a.kbps(), 6000);
        // One-second progress reports, all slow.
        assert_eq!(a.observe(0.9, 0, t0), None);
        assert_eq!(a.observe(0.9, 0, t0 + secs(1)), None);
        assert_eq!(a.observe(0.9, 0, t0 + secs(2)), None);
        assert_eq!(a.observe(0.9, 0, t0 + secs(3)), Some(Change::Down(4800)));
        assert!(a.reduced());
    }

    #[test]
    fn a_good_second_resets_the_slow_timer() {
        let t0 = Instant::now();
        let mut a = Adapt::new(6000);
        a.observe(0.9, 0, t0);
        a.observe(0.9, 0, t0 + secs(2));
        a.observe(1.0, 0, t0 + secs(2) + Duration::from_millis(500));
        assert_eq!(a.observe(0.9, 0, t0 + secs(3)), None);
        assert_eq!(a.observe(0.9, 0, t0 + secs(5)), None);
        assert_eq!(a.observe(0.9, 0, t0 + secs(6)), Some(Change::Down(4800)));
    }

    #[test]
    fn dropped_frames_count_as_struggling_even_at_full_speed() {
        let t0 = Instant::now();
        let mut a = Adapt::new(24_000);
        assert_eq!(a.observe(1.0, 0, t0), None);
        assert_eq!(a.observe(1.0, 3, t0 + secs(1)), None);
        assert_eq!(a.observe(1.0, 5, t0 + secs(2)), None);
        assert_eq!(a.observe(1.0, 8, t0 + secs(4)), Some(Change::Down(19_200)));
        // A steady counter is fine.
        assert_eq!(a.observe(1.0, 8, t0 + secs(20)), None);
    }

    #[test]
    fn settles_after_a_change_then_steps_up_after_a_stable_minute() {
        let t0 = Instant::now();
        let mut a = Adapt::new(6000);
        for i in 0..=3 {
            a.observe(0.8, 0, t0 + secs(i));
        }
        assert_eq!(a.kbps(), 4800);
        // During the settle period even terrible readings change nothing.
        for i in 4..13 {
            assert_eq!(a.observe(0.5, 10, t0 + secs(i)), None);
        }
        // Stable from t=13 on: back up at t=73, not before.
        for i in 13..73 {
            assert_eq!(a.observe(1.0, 10, t0 + secs(i)), None, "at {i}s");
        }
        assert_eq!(a.observe(1.0, 10, t0 + secs(73)), Some(Change::Up(6000)));
        assert!(!a.reduced());
        // At full quality nothing more happens.
        assert_eq!(a.observe(1.0, 10, t0 + secs(200)), None);
    }

    #[test]
    fn never_below_the_floor() {
        let t0 = Instant::now();
        let mut a = Adapt::new(6000);
        let mut t = t0;
        let mut changes = Vec::new();
        for _ in 0..200 {
            if let Some(c) = a.observe(0.5, 0, t) {
                changes.push(c);
            }
            t += secs(1);
        }
        assert_eq!(
            changes,
            vec![
                Change::Down(4800),
                Change::Down(3800),
                Change::Down(3100),
                Change::Down(2500)
            ]
        );
        assert_eq!(a.kbps(), 2500);
    }

    #[test]
    fn unknown_speed_is_ignored() {
        let t0 = Instant::now();
        let mut a = Adapt::new(6000);
        for i in 0..10 {
            assert_eq!(a.observe(0.0, 0, t0 + secs(i)), None);
        }
        assert_eq!(mbps_text(4800), "4.8 Mbps");
    }
}
