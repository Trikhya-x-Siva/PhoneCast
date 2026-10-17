//! Highlight detection: an "excitement" score fused from game-audio
//! loudness, mic loudness/variance and chat message rate.
//!
//! All three inputs are compared with their own recent baseline (z-scores),
//! so a loud game or a chatty audience does not trigger by itself: only a
//! change does. Chat reacts 20–40 s after the moment, so the chat rate is
//! fused with the audio score from ~30 s earlier.
//!
//! Time is a plain `f64` in seconds (monotonic, any origin), which keeps this
//! module free of clocks and easy to test.

use std::collections::VecDeque;

/// Rolling mean / standard deviation over a time window.
#[derive(Debug, Clone)]
pub struct Rolling {
    window: f64,
    samples: VecDeque<(f64, f64)>,
    sum: f64,
    sum_sq: f64,
}

impl Rolling {
    pub fn new(window_seconds: f64) -> Self {
        Self {
            window: window_seconds,
            samples: VecDeque::new(),
            sum: 0.0,
            sum_sq: 0.0,
        }
    }

    pub fn push(&mut self, t: f64, v: f64) {
        self.samples.push_back((t, v));
        self.sum += v;
        self.sum_sq += v * v;
        while let Some(&(t0, v0)) = self.samples.front() {
            if t - t0 <= self.window {
                break;
            }
            self.samples.pop_front();
            self.sum -= v0;
            self.sum_sq -= v0 * v0;
        }
    }

    pub fn len(&self) -> usize {
        self.samples.len()
    }

    pub fn is_empty(&self) -> bool {
        self.samples.is_empty()
    }

    /// Seconds covered by the samples currently in the window.
    pub fn span(&self) -> f64 {
        match (self.samples.front(), self.samples.back()) {
            (Some(a), Some(b)) => b.0 - a.0,
            _ => 0.0,
        }
    }

    pub fn mean(&self) -> f64 {
        if self.samples.is_empty() {
            0.0
        } else {
            self.sum / self.samples.len() as f64
        }
    }

    pub fn std(&self) -> f64 {
        let n = self.samples.len() as f64;
        if n < 2.0 {
            return 0.0;
        }
        let m = self.mean();
        (self.sum_sq / n - m * m).max(0.0).sqrt()
    }

    /// z-score of `v` against this window; `floor` is the smallest standard
    /// deviation used, so a perfectly steady baseline cannot explode.
    pub fn z(&self, v: f64, floor: f64) -> f64 {
        (v - self.mean()) / self.std().max(floor)
    }
}

/// What the streamer can tune.
#[derive(Debug, Clone)]
pub struct Config {
    /// 0 = only huge spikes, 1 = trigger easily.
    pub sensitivity: f64,
    /// Length of the saved clip.
    pub clip_seconds: u32,
    /// Stop after this many clips in one session (0 = unlimited).
    pub max_per_session: u32,
    /// Minimum gap between two clips.
    pub cooldown_seconds: f64,
}

impl Default for Config {
    fn default() -> Self {
        Self {
            sensitivity: 0.5,
            clip_seconds: 30,
            max_per_session: 40,
            cooldown_seconds: 45.0,
        }
    }
}

impl Config {
    /// Fused score needed to save a clip. Sensitivity 0.5 → 2.5.
    pub fn threshold(&self) -> f64 {
        3.6 - 2.2 * self.sensitivity.clamp(0.0, 1.0)
    }

    /// Seconds of clip before and after the moment. ~25 s before when the
    /// clip is long enough, always at least 5 s after.
    pub fn pre_post(&self) -> (u32, u32) {
        let len = self.clip_seconds.max(10);
        let pre = 25.min(len - 5);
        (pre, len - pre)
    }
}

/// A moment worth clipping.
#[derive(Debug, Clone, PartialEq)]
pub struct Highlight {
    pub score: f64,
    /// Sound label from the classifier, if one was active: gunfire, cheer…
    pub label: Option<String>,
    /// How long ago the moment was (0 for an instant audio trigger, ~30 s
    /// when chat confirmed it).
    pub seconds_ago: f64,
    /// Clip length requested by the settings.
    pub length: u32,
    /// Which inputs pushed it over: for the UI's explanation.
    pub reason: String,
}

const BASELINE_S: f64 = 60.0;
const CHAT_BASELINE_S: f64 = 300.0;
const CHAT_BUCKET_S: f64 = 10.0;
/// Chat is read ~30 s after the moment it reacts to.
const CHAT_LAG_S: f64 = 30.0;
/// Minimum baseline before anything can trigger.
const WARMUP_S: f64 = 20.0;
const DB_FLOOR: f64 = -60.0;

#[derive(Debug, Clone)]
pub struct Detector {
    cfg: Config,
    game_base: Rolling,
    game_short: Rolling,
    mic_base: Rolling,
    mic_short: Rolling,
    chat_base: Rolling,
    chat_bucket_start: Option<f64>,
    chat_bucket_count: u32,
    chat_z: f64,
    /// Recent audio scores, so a late chat reaction can be matched with
    /// the sound it reacts to.
    audio_hist: VecDeque<(f64, f64, Option<String>)>,
    last_label: Option<(f64, String, f64)>,
    last_clip_t: Option<f64>,
    first_t: Option<f64>,
    clips: u32,
    last_score: f64,
}

impl Detector {
    pub fn new(cfg: Config) -> Self {
        Self {
            cfg,
            game_base: Rolling::new(BASELINE_S),
            game_short: Rolling::new(1.0),
            mic_base: Rolling::new(BASELINE_S),
            mic_short: Rolling::new(2.0),
            chat_base: Rolling::new(CHAT_BASELINE_S),
            chat_bucket_start: None,
            chat_bucket_count: 0,
            chat_z: 0.0,
            audio_hist: VecDeque::new(),
            last_label: None,
            last_clip_t: None,
            first_t: None,
            clips: 0,
            last_score: 0.0,
        }
    }

    pub fn set_config(&mut self, cfg: Config) {
        self.cfg = cfg;
    }

    pub fn config(&self) -> &Config {
        &self.cfg
    }

    pub fn clips_saved(&self) -> u32 {
        self.clips
    }

    /// Current fused score, for the UI meter.
    pub fn last_score(&self) -> f64 {
        self.last_score
    }

    pub fn reset_session(&mut self) {
        self.clips = 0;
        self.last_clip_t = None;
    }

    /// A sound label from the classifier (`conf` in 0..1).
    pub fn on_label(&mut self, t: f64, label: &str, conf: f64) {
        if conf > 0.2 {
            self.last_label = Some((t, label.to_string(), conf));
        }
    }

    /// A chat message arrived.
    pub fn on_chat(&mut self, t: f64) -> Option<Highlight> {
        let out = self.close_chat_bucket(t);
        self.chat_bucket_count += 1;
        out
    }

    /// Audio levels in dBFS, ~10×/s.
    pub fn on_audio(&mut self, t: f64, game_db: f64, mic_db: f64) -> Option<Highlight> {
        let game = game_db.max(DB_FLOOR);
        let mic = mic_db.max(DB_FLOOR);
        self.first_t.get_or_insert(t);
        self.game_short.push(t, game);
        self.mic_short.push(t, mic);

        // Game: how far the last second sits above the last minute. A jump
        // out of near-silence (menus, loading screens) is not a highlight,
        // so the term fades out below about -40 dBFS peak.
        let zg = if self.game_base.len() > 10 {
            let level = self.game_short.mean();
            self.game_base.z(level, 3.0).clamp(0.0, 4.0) * loud_enough(level, -45.0, -30.0)
        } else {
            0.0
        };
        // Mic: louder than usual, or much more varied (shouting, laughing).
        let zm = if self.mic_base.len() > 10 {
            let level = self.mic_short.mean();
            let loud = self.mic_base.z(level, 4.0).clamp(0.0, 4.0);
            let var = (self.mic_short.std() / self.mic_base.std().max(2.0) - 1.0).clamp(0.0, 3.0);
            loud.max(var) * loud_enough(level, -50.0, -35.0)
        } else {
            0.0
        };
        self.game_base.push(t, game);
        self.mic_base.push(t, mic);

        let mut audio = 0.7 * zg + 0.5 * zm;
        let mut label = None;
        if let Some((lt, l, conf)) = &self.last_label {
            if t - lt <= 2.0 {
                audio += label_weight(l) * conf;
                label = Some(l.clone());
            }
        }
        self.audio_hist.push_back((t, audio, label.clone()));
        while self
            .audio_hist
            .front()
            .is_some_and(|f| t - f.0 > CHAT_LAG_S + 15.0)
        {
            self.audio_hist.pop_front();
        }

        // Chat buckets close on the audio clock too, so a quiet chat still
        // decays.
        let late = self.close_chat_bucket(t);
        let fused = audio + 0.6 * self.chat_z;
        self.last_score = fused.max(self.delayed_score(t).0);
        if late.is_some() {
            return late;
        }

        let mut reason = Vec::new();
        if zg >= 1.0 {
            reason.push("game sound");
        }
        if zm >= 1.0 {
            reason.push("your voice");
        }
        if label.is_some() {
            reason.push("sound type");
        }
        if self.chat_z >= 1.0 {
            reason.push("chat");
        }
        self.try_trigger(t, fused, label, 0.0, &reason.join(" + "))
    }

    /// Score of the audio ~30 s ago plus the chat reaction now.
    fn delayed_score(&self, t: f64) -> (f64, f64, Option<String>) {
        let mut best = (0.0, 0.0, None);
        for (ht, a, l) in &self.audio_hist {
            let age = t - ht;
            if (CHAT_LAG_S - 12.0..=CHAT_LAG_S + 12.0).contains(&age) {
                let s = a + 0.6 * self.chat_z;
                if s > best.0 {
                    best = (s, age, l.clone());
                }
            }
        }
        best
    }

    fn close_chat_bucket(&mut self, t: f64) -> Option<Highlight> {
        let start = *self.chat_bucket_start.get_or_insert(t);
        if t - start < CHAT_BUCKET_S {
            return None;
        }
        let count = self.chat_bucket_count as f64;
        self.chat_bucket_count = 0;
        self.chat_bucket_start = Some(t);
        self.chat_z = if self.chat_base.len() >= 3 {
            // A floor of ~1.5 messages/10 s keeps a near-silent chat from
            // turning two messages into a spike.
            self.chat_base.z(count, 1.5).clamp(0.0, 4.0)
        } else {
            0.0
        };
        self.chat_base.push(t, count);
        if self.chat_z < 1.0 {
            return None;
        }
        let (score, age, label) = self.delayed_score(t);
        if age <= 0.0 {
            return None;
        }
        self.try_trigger(t, score, label, age, "chat reaction + earlier sound")
    }

    fn try_trigger(
        &mut self,
        t: f64,
        score: f64,
        label: Option<String>,
        seconds_ago: f64,
        reason: &str,
    ) -> Option<Highlight> {
        if score < self.cfg.threshold() {
            return None;
        }
        if self.first_t.is_none_or(|f| t - f < WARMUP_S) {
            return None;
        }
        let moment = t - seconds_ago;
        let cooldown = self.cfg.cooldown_seconds.max(self.cfg.clip_seconds as f64);
        if self
            .last_clip_t
            .is_some_and(|last| (moment - last).abs() < cooldown)
        {
            return None;
        }
        if self.cfg.max_per_session > 0 && self.clips >= self.cfg.max_per_session {
            return None;
        }
        self.clips += 1;
        self.last_clip_t = Some(moment);
        Some(Highlight {
            score,
            label,
            seconds_ago,
            length: self.cfg.clip_seconds,
            reason: reason.to_string(),
        })
    }
}

/// 0 below `quiet` dBFS, 1 above `loud`, linear in between.
fn loud_enough(level_db: f64, quiet: f64, loud: f64) -> f64 {
    ((level_db - quiet) / (loud - quiet)).clamp(0.0, 1.0)
}

/// How much a recognised sound adds to the score (× classifier confidence).
pub fn label_weight(label: &str) -> f64 {
    match label {
        "gunfire" | "explosion" => 1.0,
        "cheer" => 1.3,
        "laugh" => 1.1,
        "scream" => 1.2,
        _ => 0.5,
    }
}

/// Given a highlight and the clip settings, how long to wait before asking
/// the replay buffer, and how many seconds to ask for. The buffer saves the
/// *last* N seconds, so a late (chat-confirmed) detection asks for a longer
/// clip that still starts ~25 s before the moment.
pub fn clip_request(h: &Highlight, cfg: &Config) -> (f64, u32) {
    let (_, post) = cfg.pre_post();
    let wait = (post as f64 - h.seconds_ago).max(0.0);
    let extra = (h.seconds_ago - post as f64).max(0.0).round() as u32;
    (wait, h.length + extra)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn quiet(d: &mut Detector, from: f64, to: f64) -> Vec<Highlight> {
        let mut out = Vec::new();
        let mut t = from;
        while t < to {
            // Small natural wobble so the baseline has a non-zero spread.
            let w = (t * 7.0f64).sin() * 1.5;
            if let Some(h) = d.on_audio(t, -30.0 + w, -45.0 + w) {
                out.push(h);
            }
            t += 0.1;
        }
        out
    }

    #[test]
    fn rolling_window_drops_old_samples() {
        let mut r = Rolling::new(10.0);
        for i in 0..100 {
            r.push(i as f64, i as f64);
        }
        assert_eq!(r.len(), 11);
        assert!((r.mean() - 94.0).abs() < 1e-9);
        assert!(r.std() > 3.0 && r.std() < 3.3);
    }

    #[test]
    fn nothing_fires_on_steady_audio() {
        let mut d = Detector::new(Config::default());
        assert!(quiet(&mut d, 0.0, 300.0).is_empty());
        assert_eq!(d.clips_saved(), 0);
    }

    #[test]
    fn loud_game_burst_triggers_once_and_respects_cooldown() {
        let mut d = Detector::new(Config::default());
        quiet(&mut d, 0.0, 90.0);
        let mut hits = Vec::new();
        let mut t = 90.0;
        while t < 94.0 {
            if let Some(h) = d.on_audio(t, -4.0, -45.0) {
                hits.push((t, h));
            }
            t += 0.1;
        }
        assert_eq!(hits.len(), 1, "one clip per burst");
        let (at, h) = &hits[0];
        assert!(*at < 92.0, "fires within the first seconds of the burst");
        assert_eq!(h.seconds_ago, 0.0);
        assert_eq!(h.length, 30);
        assert!(h.reason.contains("game sound"));
        // A second burst 20 s later is inside the cooldown.
        quiet(&mut d, 94.0, 110.0);
        let mut again = 0;
        let mut t = 110.0;
        while t < 113.0 {
            if d.on_audio(t, -4.0, -45.0).is_some() {
                again += 1;
            }
            t += 0.1;
        }
        assert_eq!(again, 0);
        assert_eq!(d.clips_saved(), 1);
    }

    #[test]
    fn jump_out_of_silence_is_not_a_highlight() {
        let mut d = Detector::new(Config::default());
        let mut t = 0.0;
        while t < 90.0 {
            assert!(d.on_audio(t, -58.0 + (t * 7.0f64).sin(), -60.0).is_none());
            t += 0.1;
        }
        // A menu click at -42 dB is 16 dB above baseline but still quiet.
        let mut fired = false;
        while t < 93.0 {
            fired |= d.on_audio(t, -42.0, -60.0).is_some();
            t += 0.1;
        }
        assert!(!fired);
    }

    #[test]
    fn warmup_prevents_early_triggers() {
        let mut d = Detector::new(Config::default());
        quiet(&mut d, 0.0, 8.0);
        for i in 0..30 {
            assert!(d.on_audio(8.0 + i as f64 * 0.1, 0.0, 0.0).is_none());
        }
    }

    #[test]
    fn sensitivity_changes_threshold() {
        let lo = Config {
            sensitivity: 0.0,
            ..Config::default()
        };
        let hi = Config {
            sensitivity: 1.0,
            ..Config::default()
        };
        assert!(lo.threshold() > hi.threshold());
        // A modest shout: fires on high sensitivity only.
        for (cfg, expect) in [(lo, false), (hi, true)] {
            let mut d = Detector::new(cfg);
            quiet(&mut d, 0.0, 90.0);
            let mut fired = false;
            let mut t = 90.0;
            while t < 93.0 {
                fired |= d.on_audio(t, -30.0, -24.0).is_some();
                t += 0.1;
            }
            assert_eq!(fired, expect);
        }
    }

    #[test]
    fn chat_reaction_confirms_an_earlier_moderate_moment() {
        let cfg = Config {
            sensitivity: 0.5,
            ..Config::default()
        };
        let mut d = Detector::new(cfg);
        // Quiet with a steady trickle of chat: 1 message every 5 s.
        let mut t = 0.0;
        while t < 120.0 {
            d.on_audio(t, -30.0 + ((t * 7.0).sin() * 1.5), -45.0);
            if (t * 10.0).round() as i64 % 50 == 0 {
                d.on_chat(t);
            }
            t += 0.1;
        }
        // A moderate bump at t=120..123: alone it stays under the threshold.
        let mut early = 0;
        while t < 123.0 {
            early += d.on_audio(t, -22.0, -45.0).is_some() as u32;
            t += 0.1;
        }
        assert_eq!(early, 0, "moderate sound alone must not clip");
        // Chat erupts ~30 s later.
        let mut late = None;
        while t < 165.0 {
            if let Some(h) = d.on_audio(t, -30.0, -45.0) {
                late = Some((t, h));
            }
            if (148.0..158.0).contains(&t) && (t * 10.0).round() as i64 % 5 == 0 {
                if let Some(h) = d.on_chat(t) {
                    late = Some((t, h));
                }
            }
            t += 0.1;
        }
        let (at, h) = late.expect("chat burst should confirm the moment");
        assert!(h.seconds_ago > 18.0 && h.seconds_ago < 42.0, "{h:?}");
        let moment = at - h.seconds_ago;
        assert!((118.0..=126.0).contains(&moment), "moment {moment} at {at}");
        assert!(h.reason.contains("chat"));
    }

    #[test]
    fn classifier_label_boosts_score() {
        let mut d = Detector::new(Config {
            sensitivity: 0.3,
            ..Config::default()
        });
        quiet(&mut d, 0.0, 90.0);
        // Moderate sound only.
        let mut t = 90.0;
        let mut fired = false;
        while t < 92.0 {
            fired |= d.on_audio(t, -20.0, -45.0).is_some();
            t += 0.1;
        }
        assert!(!fired);
        quiet(&mut d, 92.0, 160.0);
        d.on_label(160.0, "cheer", 0.9);
        let mut hit = None;
        let mut t = 160.0;
        while t < 162.0 {
            if let Some(h) = d.on_audio(t, -20.0, -45.0) {
                hit = Some(h);
            }
            t += 0.1;
        }
        let h = hit.expect("label should push it over");
        assert_eq!(h.label.as_deref(), Some("cheer"));
    }

    #[test]
    fn per_session_cap() {
        let mut d = Detector::new(Config {
            max_per_session: 2,
            cooldown_seconds: 30.0,
            clip_seconds: 15,
            ..Config::default()
        });
        let mut t = 0.0;
        let mut n = 0;
        while t < 600.0 {
            let burst = (t % 100.0) < 3.0 && t > 50.0;
            let w = (t * 7.0f64).sin() * 1.5;
            n += d
                .on_audio(t, if burst { -3.0 } else { -30.0 + w }, -45.0 + w)
                .is_some() as u32;
            t += 0.1;
        }
        assert_eq!(n, 2);
    }

    #[test]
    fn clip_request_math() {
        let cfg = Config {
            clip_seconds: 30,
            ..Config::default()
        };
        assert_eq!(cfg.pre_post(), (25, 5));
        let now = Highlight {
            score: 3.0,
            label: None,
            seconds_ago: 0.0,
            length: 30,
            reason: String::new(),
        };
        assert_eq!(clip_request(&now, &cfg), (5.0, 30));
        let late = Highlight {
            seconds_ago: 30.0,
            ..now
        };
        assert_eq!(clip_request(&late, &cfg), (0.0, 55));
        let short = Config {
            clip_seconds: 15,
            ..Config::default()
        };
        assert_eq!(short.pre_post(), (10, 5));
    }
}
