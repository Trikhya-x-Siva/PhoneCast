//! Messages between the PhoneCast app and the helper: one JSON object per
//! line, both ways, on a loopback TCP socket. The first line from the
//! helper is `{"t":"hello","token":"…"}` with the token the app passed on
//! the command line; anything else is dropped.

use serde::{Deserialize, Serialize};

/// Everything the streamer can switch on. Saved by the app in
/// `%APPDATA%\PhoneCast\ai.json`; the helper only receives it.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Default)]
#[serde(rename_all = "camelCase", default)]
pub struct Settings {
    pub enabled: bool,
    pub highlights: HighlightSettings,
    pub chat: ChatSettings,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
#[serde(rename_all = "camelCase", default)]
pub struct HighlightSettings {
    pub enabled: bool,
    /// 0..1
    pub sensitivity: f64,
    pub clip_seconds: u32,
    pub ask_before_saving: bool,
    pub max_per_session: u32,
    /// YAMNet sound classifier (downloaded on enable).
    pub classifier: bool,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
#[serde(rename_all = "camelCase", default)]
pub struct ChatSettings {
    pub enabled: bool,
    pub twitch: TwitchSettings,
    pub youtube: YoutubeSettings,
    pub overlay: bool,
    pub spam: SpamSettings,
    pub faq: FaqSettings,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Default)]
#[serde(rename_all = "camelCase", default)]
pub struct TwitchSettings {
    pub enabled: bool,
    /// Client ID of the streamer's own Twitch application (public client).
    pub client_id: String,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Default)]
#[serde(rename_all = "camelCase", default)]
pub struct YoutubeSettings {
    pub enabled: bool,
    /// Channel handle (`@name`), channel URL or a live video URL.
    pub channel: String,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
#[serde(rename_all = "camelCase", default)]
pub struct SpamSettings {
    pub enabled: bool,
    pub max_per_user_10s: u32,
    pub flag_links: bool,
    pub caps_ratio: f64,
    pub emote_ratio: f64,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
#[serde(rename_all = "camelCase", default)]
pub struct FaqSettings {
    pub enabled: bool,
    /// Answer `!ask …` automatically. Off: every answer waits for approval.
    pub auto_reply_on_ask: bool,
    /// Rephrase answers with the local model (downloaded on enable).
    pub llm: bool,
}

impl Default for HighlightSettings {
    fn default() -> Self {
        Self {
            enabled: true,
            sensitivity: 0.5,
            clip_seconds: 30,
            ask_before_saving: true,
            max_per_session: 40,
            classifier: false,
        }
    }
}

impl Default for ChatSettings {
    fn default() -> Self {
        Self {
            enabled: true,
            twitch: TwitchSettings::default(),
            youtube: YoutubeSettings::default(),
            overlay: false,
            spam: SpamSettings::default(),
            faq: FaqSettings::default(),
        }
    }
}

impl Default for SpamSettings {
    fn default() -> Self {
        Self {
            enabled: true,
            max_per_user_10s: 5,
            flag_links: true,
            caps_ratio: 0.75,
            emote_ratio: 0.7,
        }
    }
}

impl Default for FaqSettings {
    fn default() -> Self {
        Self {
            enabled: true,
            auto_reply_on_ask: false,
            llm: false,
        }
    }
}

/// App → helper.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(tag = "t", rename_all = "camelCase")]
pub enum ToHelper {
    Settings {
        settings: Settings,
    },
    /// Levels in dBFS, ~10×/s; `pcm` is base64 of 16 kHz mono s16le, only
    /// present while the classifier is on.
    Audio {
        game: f64,
        mic: f64,
        #[serde(default)]
        pcm: Option<String>,
        /// Optional sender clock in seconds (any origin). When present the
        /// helper follows it instead of its own clock, so a recording can be
        /// replayed faster than real time for testing.
        #[serde(default)]
        ts: Option<f64>,
    },
    /// Start downloading a model ("yamnet" or "llm").
    Download {
        model: String,
    },
    /// Remove a downloaded model.
    RemoveModel {
        model: String,
    },
    TwitchLogin,
    TwitchLogout,
    /// Post a suggested FAQ reply (the streamer approved it).
    Approve {
        id: u64,
    },
    /// Discard a suggested reply.
    Dismiss {
        id: u64,
    },
    /// Re-read faq.yaml.
    ReloadFaq,
    /// A new stream/session started: reset the per-session clip count.
    NewSession,
    /// The app saved (or failed to save) a clip we asked for.
    ClipResult {
        ok: bool,
        #[serde(default)]
        detail: String,
    },
    Shutdown,
}

/// Helper → app.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(tag = "t", rename_all = "camelCase")]
pub enum FromHelper {
    Hello {
        token: String,
        version: String,
    },
    Status(Status),
    Highlight {
        score: f64,
        #[serde(default)]
        label: Option<String>,
        seconds_ago: f64,
        length: u32,
        reason: String,
    },
    Chat(ChatMessage),
    ModLog {
        line: String,
    },
    /// A FAQ answer. `posted` = already sent (auto-reply); otherwise it
    /// waits for the streamer's approval.
    Faq {
        id: u64,
        platform: String,
        user: String,
        question: String,
        answer: String,
        posted: bool,
    },
    /// Twitch device login: show this code and link to the streamer.
    DeviceCode {
        url: String,
        code: String,
    },
    Toast {
        text: String,
    },
}

#[derive(Debug, Clone, Serialize, Deserialize, Default)]
#[serde(rename_all = "camelCase")]
pub struct Status {
    pub cpu_percent: f64,
    pub ram_mb: f64,
    pub score: f64,
    pub clips: u32,
    pub twitch: String,
    pub youtube: String,
    pub twitch_user: Option<String>,
    pub models: Vec<ModelStatus>,
    pub faq_entries: usize,
    pub faq_path: String,
}

#[derive(Debug, Clone, Serialize, Deserialize, Default)]
#[serde(rename_all = "camelCase")]
pub struct ModelStatus {
    pub id: String,
    pub name: String,
    pub size_mb: f64,
    /// missing | downloading | ready | error | unsupported
    pub state: String,
    pub progress: f64,
    pub detail: String,
}

#[derive(Debug, Clone, Serialize, Deserialize, Default)]
#[serde(rename_all = "camelCase")]
pub struct ChatMessage {
    pub id: String,
    pub platform: String,
    pub user: String,
    pub text: String,
    pub privileged: bool,
    #[serde(default)]
    pub spam: Option<String>,
    /// Unix milliseconds.
    pub ts: u64,
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn settings_round_trip_and_defaults() {
        let s: Settings = serde_json::from_str("{}").unwrap();
        assert_eq!(s, Settings::default());
        assert!(!s.enabled);
        assert!(s.highlights.ask_before_saving);
        assert!(!s.chat.faq.auto_reply_on_ask);
        let j = serde_json::to_string(&s).unwrap();
        assert!(j.contains("\"askBeforeSaving\":true"));
        let back: Settings = serde_json::from_str(&j).unwrap();
        assert_eq!(back, s);
    }

    #[test]
    fn messages_are_tagged() {
        let m: ToHelper = serde_json::from_str(r#"{"t":"audio","game":-20.5,"mic":-40}"#).unwrap();
        assert!(matches!(m, ToHelper::Audio { pcm: None, .. }));
        let j = serde_json::to_string(&FromHelper::Toast { text: "hi".into() }).unwrap();
        assert_eq!(j, r#"{"t":"toast","text":"hi"}"#);
    }
}
