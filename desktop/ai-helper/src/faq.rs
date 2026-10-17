//! FAQ answers from a streamer-edited YAML file, matched with BM25 over
//! keywords. Example file:
//!
//! ```yaml
//! - q: What phone do you play on?
//!   also: [which phone, phone model, your device]
//!   a: OnePlus 13, 120 Hz, streamed with PhoneCast.
//! - q: What is your sensitivity?
//!   a: 0.6 in game, 50 DPI-equivalent. Settings are in the panel below.
//! ```

use serde::Deserialize;
use std::collections::HashMap;

#[derive(Debug, Clone, Deserialize)]
pub struct Entry {
    pub q: String,
    #[serde(default)]
    pub also: Vec<String>,
    pub a: String,
}

#[derive(Debug, Clone)]
pub struct Faq {
    entries: Vec<Entry>,
    docs: Vec<Vec<String>>,
    df: HashMap<String, usize>,
    avg_len: f64,
}

#[derive(Debug, Clone, PartialEq)]
pub struct Answer {
    pub question: String,
    pub answer: String,
    pub score: f64,
}

const STOP: &[&str] = &[
    "the", "a", "an", "is", "are", "do", "does", "you", "your", "what", "whats", "which", "how",
    "i", "me", "my", "of", "to", "in", "on", "it", "this", "that", "and", "or", "for", "with",
    "can", "u", "ur", "pls", "please", "hey", "hi",
];

pub fn tokens(text: &str) -> Vec<String> {
    text.split(|c: char| !c.is_alphanumeric())
        .filter(|w| !w.is_empty())
        .map(|w| w.to_lowercase())
        .filter(|w| !STOP.contains(&w.as_str()))
        .map(|w| stem(&w))
        .collect()
}

/// Tiny stemmer: enough to match "sensitivity"/"sens", "settings"/"setting".
fn stem(w: &str) -> String {
    let mut s = w.to_string();
    for suf in ["ings", "ing", "es", "s", "ed"] {
        if s.len() > suf.len() + 3 && s.ends_with(suf) {
            s.truncate(s.len() - suf.len());
            break;
        }
    }
    s
}

impl Faq {
    pub fn empty() -> Self {
        Self::from_entries(Vec::new())
    }

    pub fn from_yaml(text: &str) -> Result<Self, String> {
        let entries: Vec<Entry> = serde_yaml::from_str(text).map_err(|e| e.to_string())?;
        Ok(Self::from_entries(entries))
    }

    pub fn from_entries(entries: Vec<Entry>) -> Self {
        let docs: Vec<Vec<String>> = entries
            .iter()
            .map(|e| {
                let mut t = tokens(&e.q);
                for a in &e.also {
                    t.extend(tokens(a));
                }
                t
            })
            .collect();
        let mut df = HashMap::new();
        for d in &docs {
            let mut seen = std::collections::HashSet::new();
            for w in d {
                if seen.insert(w) {
                    *df.entry(w.clone()).or_insert(0) += 1;
                }
            }
        }
        let avg_len = if docs.is_empty() {
            1.0
        } else {
            docs.iter().map(|d| d.len()).sum::<usize>() as f64 / docs.len() as f64
        };
        Self {
            entries,
            docs,
            df,
            avg_len: avg_len.max(1.0),
        }
    }

    pub fn len(&self) -> usize {
        self.entries.len()
    }

    pub fn is_empty(&self) -> bool {
        self.entries.is_empty()
    }

    /// Best matching entry, or None when nothing fits well enough.
    pub fn answer(&self, question: &str) -> Option<Answer> {
        let q = tokens(question);
        if q.is_empty() || self.entries.is_empty() {
            return None;
        }
        let n = self.docs.len() as f64;
        let (k1, b) = (1.2, 0.75);
        let mut best: Option<(usize, f64, usize)> = None;
        for (i, d) in self.docs.iter().enumerate() {
            let mut score = 0.0;
            let mut hits = 0;
            for term in &q {
                let tf = d.iter().filter(|w| *w == term).count() as f64;
                if tf == 0.0 {
                    continue;
                }
                hits += 1;
                let df = *self.df.get(term).unwrap_or(&0) as f64;
                let idf = ((n - df + 0.5) / (df + 0.5) + 1.0).ln();
                let norm = k1 * (1.0 - b + b * d.len() as f64 / self.avg_len);
                score += idf * (tf * (k1 + 1.0)) / (tf + norm);
            }
            if hits > 0 && best.is_none_or(|(_, s, _)| score > s) {
                best = Some((i, score, hits));
            }
        }
        let (i, score, hits) = best?;
        // Need at least one keyword and either a strong score or most of
        // the question's keywords matched.
        let coverage = hits as f64 / q.len() as f64;
        if score < 1.0 || (coverage < 0.5 && score < 2.5) {
            return None;
        }
        Some(Answer {
            question: self.entries[i].q.clone(),
            answer: self.entries[i].a.clone(),
            score,
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    const YAML: &str = r#"
- q: What phone do you play on?
  also: [which phone, phone model, your device]
  a: OnePlus 13, streamed with PhoneCast.
- q: What is your sensitivity?
  also: [sens, dpi]
  a: 0.6 in game.
- q: When do you stream?
  also: [schedule, stream times]
  a: Weeknights 8pm.
"#;

    #[test]
    fn matches_paraphrases() {
        let f = Faq::from_yaml(YAML).unwrap();
        assert_eq!(f.len(), 3);
        let a = f.answer("which phone is that??").unwrap();
        assert!(a.answer.starts_with("OnePlus"));
        let a = f.answer("sens?").unwrap();
        assert_eq!(a.answer, "0.6 in game.");
        let a = f.answer("whats your streaming schedule").unwrap();
        assert_eq!(a.answer, "Weeknights 8pm.");
    }

    #[test]
    fn unrelated_questions_get_nothing() {
        let f = Faq::from_yaml(YAML).unwrap();
        assert_eq!(f.answer("can you say hi to my brother"), None);
        assert_eq!(f.answer("gg"), None);
        assert_eq!(f.answer(""), None);
    }

    #[test]
    fn bad_yaml_is_an_error() {
        assert!(Faq::from_yaml("- q: only a question").is_err());
        assert!(Faq::from_yaml("not: a list").is_err());
    }
}
