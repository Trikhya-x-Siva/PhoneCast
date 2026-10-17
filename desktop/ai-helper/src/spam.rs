//! Chat spam filter: simple, explainable rules. A flagged message is still
//! shown (greyed out) and written to the mod log; nothing is deleted on the
//! platform.

use std::collections::{HashMap, VecDeque};

#[derive(Debug, Clone)]
pub struct SpamConfig {
    /// Messages one viewer may send in 10 s before the rest are flagged.
    pub max_per_user_10s: u32,
    /// Flag messages with links.
    pub flag_links: bool,
    /// Share of letters in upper case that counts as shouting (0 = off).
    pub caps_ratio: f64,
    /// Share of the message made of emotes that counts as emote spam (0 = off).
    pub emote_ratio: f64,
    /// Seconds a message is remembered for near-duplicate detection.
    pub dup_window_s: f64,
}

impl Default for SpamConfig {
    fn default() -> Self {
        Self {
            max_per_user_10s: 5,
            flag_links: true,
            caps_ratio: 0.75,
            emote_ratio: 0.7,
            dup_window_s: 60.0,
        }
    }
}

/// A chat message as the filter sees it.
#[derive(Debug, Clone)]
pub struct Incoming<'a> {
    pub user: &'a str,
    pub text: &'a str,
    /// Share of the message that is emotes (0..1), from the platform data.
    pub emote_share: f64,
    /// Moderators and the streamer are never flagged.
    pub privileged: bool,
}

#[derive(Debug)]
pub struct SpamFilter {
    cfg: SpamConfig,
    per_user: HashMap<String, VecDeque<f64>>,
    recent: VecDeque<(f64, u64, String)>,
}

impl SpamFilter {
    pub fn new(cfg: SpamConfig) -> Self {
        Self {
            cfg,
            per_user: HashMap::new(),
            recent: VecDeque::new(),
        }
    }

    pub fn set_config(&mut self, cfg: SpamConfig) {
        self.cfg = cfg;
    }

    /// Returns a short reason when the message looks like spam.
    pub fn check(&mut self, t: f64, m: &Incoming) -> Option<String> {
        let user = m.user.to_lowercase();
        // Rate per user.
        let times = self.per_user.entry(user.clone()).or_default();
        times.push_back(t);
        while times.front().is_some_and(|f| t - f > 10.0) {
            times.pop_front();
        }
        let over_rate = times.len() as u32 > self.cfg.max_per_user_10s;
        if self.per_user.len() > 5000 {
            self.per_user
                .retain(|_, v| v.back().is_some_and(|b| t - b < 60.0));
        }

        // Near-duplicates across users (copy-pasta, bots).
        let h = simhash(m.text);
        while self
            .recent
            .front()
            .is_some_and(|f| t - f.0 > self.cfg.dup_window_s)
        {
            self.recent.pop_front();
        }
        let words = m.text.split_whitespace().count();
        let dup = words >= 3
            && self
                .recent
                .iter()
                .filter(|(_, rh, ru)| hamming(*rh, h) <= 3 && *ru != user)
                .count()
                >= 1;
        let self_repeat = words >= 2
            && self
                .recent
                .iter()
                .filter(|(_, rh, ru)| hamming(*rh, h) <= 3 && *ru == user)
                .count()
                >= 2;
        self.recent.push_back((t, h, user));

        if m.privileged {
            return None;
        }
        if over_rate {
            return Some("too many messages".into());
        }
        if dup {
            return Some("same message as another viewer".into());
        }
        if self_repeat {
            return Some("repeated message".into());
        }
        if self.cfg.flag_links && has_link(m.text) {
            return Some("link".into());
        }
        if self.cfg.caps_ratio > 0.0 && is_shouting(m.text, self.cfg.caps_ratio) {
            return Some("all caps".into());
        }
        if self.cfg.emote_ratio > 0.0 && words >= 3 && m.emote_share >= self.cfg.emote_ratio {
            return Some("emote wall".into());
        }
        None
    }
}

/// 64-bit SimHash over lower-cased words: near-identical texts get hashes a
/// few bits apart.
pub fn simhash(text: &str) -> u64 {
    let mut acc = [0i32; 64];
    let mut any = false;
    for w in text.split_whitespace() {
        let w: String = w
            .chars()
            .filter(|c| c.is_alphanumeric())
            .flat_map(|c| c.to_lowercase())
            .collect();
        if w.is_empty() {
            continue;
        }
        any = true;
        let h = fnv1a(w.as_bytes());
        for (i, a) in acc.iter_mut().enumerate() {
            *a += if (h >> i) & 1 == 1 { 1 } else { -1 };
        }
    }
    if !any {
        return 0;
    }
    acc.iter()
        .enumerate()
        .fold(0u64, |h, (i, a)| if *a > 0 { h | (1 << i) } else { h })
}

pub fn hamming(a: u64, b: u64) -> u32 {
    (a ^ b).count_ones()
}

fn fnv1a(bytes: &[u8]) -> u64 {
    let mut h: u64 = 0xcbf29ce484222325;
    for b in bytes {
        h ^= *b as u64;
        h = h.wrapping_mul(0x100000001b3);
    }
    h
}

const TLDS: &[&str] = &[
    ".com", ".net", ".org", ".gg", ".tv", ".io", ".co", ".me", ".xyz", ".ru", ".link", ".to",
    ".ly", ".app", ".dev", ".info", ".biz", ".site", ".online", ".shop", ".store", ".top",
];

pub fn has_link(text: &str) -> bool {
    let lower = text.to_lowercase();
    if lower.contains("http://") || lower.contains("https://") || lower.contains("www.") {
        return true;
    }
    lower.split_whitespace().any(|tok| {
        let tok = tok.trim_matches(|c: char| !c.is_alphanumeric());
        TLDS.iter()
            .any(|tld| tok.ends_with(tld) && tok.len() > tld.len() + 1 && !tok.starts_with('.'))
            || tok.contains("discord.gg/")
    })
}

pub fn is_shouting(text: &str, ratio: f64) -> bool {
    let letters: Vec<char> = text.chars().filter(|c| c.is_alphabetic()).collect();
    if letters.len() < 10 {
        return false;
    }
    let upper = letters.iter().filter(|c| c.is_uppercase()).count();
    upper as f64 / letters.len() as f64 >= ratio
}

#[cfg(test)]
mod tests {
    use super::*;

    fn msg<'a>(user: &'a str, text: &'a str) -> Incoming<'a> {
        Incoming {
            user,
            text,
            emote_share: 0.0,
            privileged: false,
        }
    }

    #[test]
    fn normal_chat_passes() {
        let mut f = SpamFilter::new(SpamConfig::default());
        assert_eq!(f.check(0.0, &msg("ann", "gg that was close")), None);
        assert_eq!(f.check(1.0, &msg("bob", "what sens do you use?")), None);
        assert_eq!(f.check(2.0, &msg("ann", "lol")), None);
    }

    #[test]
    fn per_user_rate_limit() {
        let mut f = SpamFilter::new(SpamConfig {
            max_per_user_10s: 3,
            ..SpamConfig::default()
        });
        for i in 0..3 {
            assert_eq!(f.check(i as f64, &msg("spammer", &format!("hi {i}"))), None);
        }
        assert_eq!(
            f.check(3.0, &msg("spammer", "hi 3")).as_deref(),
            Some("too many messages")
        );
        // After 10 s the window has cleared.
        assert_eq!(f.check(20.0, &msg("spammer", "back")), None);
    }

    #[test]
    fn near_duplicate_from_other_user_is_flagged() {
        let mut f = SpamFilter::new(SpamConfig::default());
        assert_eq!(
            f.check(0.0, &msg("a", "FOLLOW ME FOR FREE SKINS click now")),
            None
        );
        let r = f.check(5.0, &msg("b", "follow me for free skins click now!!"));
        assert_eq!(r.as_deref(), Some("same message as another viewer"));
        // Short common phrases are not duplicates.
        assert_eq!(f.check(6.0, &msg("c", "gg")), None);
        assert_eq!(f.check(7.0, &msg("d", "gg")), None);
    }

    #[test]
    fn self_repeat_is_flagged_on_third_copy() {
        let mut f = SpamFilter::new(SpamConfig::default());
        assert_eq!(f.check(0.0, &msg("a", "play my song")), None);
        assert_eq!(f.check(1.0, &msg("a", "play my song")), None);
        assert_eq!(
            f.check(2.0, &msg("a", "play my song")).as_deref(),
            Some("repeated message")
        );
    }

    #[test]
    fn links_caps_and_emotes() {
        let mut f = SpamFilter::new(SpamConfig::default());
        assert_eq!(
            f.check(0.0, &msg("a", "check https://example.com"))
                .as_deref(),
            Some("link")
        );
        assert_eq!(
            f.check(1.0, &msg("b", "join discord.gg/abc")).as_deref(),
            Some("link")
        );
        assert_eq!(f.check(2.0, &msg("c", "brb. ok")), None);
        assert_eq!(
            f.check(3.0, &msg("d", "THIS IS THE BEST STREAM EVER"))
                .as_deref(),
            Some("all caps")
        );
        assert_eq!(f.check(4.0, &msg("e", "GG wp nice one")), None);
        let wall = Incoming {
            emote_share: 0.9,
            ..msg("f", "Kappa Kappa Kappa Kappa")
        };
        assert_eq!(f.check(5.0, &wall).as_deref(), Some("emote wall"));
    }

    #[test]
    fn moderators_are_never_flagged() {
        let mut f = SpamFilter::new(SpamConfig::default());
        let m = Incoming {
            privileged: true,
            ..msg("mod", "RULES: https://example.com/rules")
        };
        assert_eq!(f.check(0.0, &m), None);
    }

    #[test]
    fn simhash_distance() {
        let a = simhash("the quick brown fox jumps over the lazy dog");
        let b = simhash("The quick brown fox jumps over the lazy dog!");
        let c = simhash("completely different words in this one here");
        assert!(hamming(a, b) <= 3);
        assert!(hamming(a, c) > 10);
    }
}
