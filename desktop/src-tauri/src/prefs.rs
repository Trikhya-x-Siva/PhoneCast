//! User preferences that must survive restarts and Windows updates:
//! hotkeys, replay buffer length and the chosen devices (by stable ID, with
//! the human name kept so a missing device can be named in a warning).
//! Stored as %APPDATA%\PhoneCast\settings.json.

use serde::{Deserialize, Serialize};
use std::collections::BTreeMap;
use std::path::PathBuf;

#[derive(Serialize, Deserialize, Clone, Default, Debug, PartialEq, Eq)]
pub struct Device {
    pub id: String,
    pub name: String,
}

#[derive(Serialize, Deserialize, Clone, Debug)]
#[serde(default, rename_all = "camelCase")]
pub struct Prefs {
    /// Seconds kept in the replay buffer; 0 = off.
    pub replay_seconds: u32,
    /// Hotkey per action id ("clip15" .. "live"); missing = default,
    /// empty string = unbound.
    pub hotkeys: BTreeMap<String, String>,
    /// Microphone by Windows endpoint ID; None = Windows default.
    pub mic: Option<Device>,
    /// "auto", "discord" or a browser id.
    pub voice_app: String,
}

impl Default for Prefs {
    fn default() -> Self {
        Self {
            replay_seconds: crate::replay::DEFAULT_SECONDS,
            hotkeys: BTreeMap::new(),
            mic: None,
            voice_app: "auto".into(),
        }
    }
}

pub fn path() -> PathBuf {
    let base = std::env::var("APPDATA")
        .map(PathBuf::from)
        .unwrap_or_else(|_| PathBuf::from("."));
    base.join("PhoneCast").join("settings.json")
}

impl Prefs {
    pub fn load() -> Self {
        std::fs::read(path())
            .ok()
            .and_then(|b| serde_json::from_slice(&b).ok())
            .unwrap_or_default()
    }

    pub fn save(&self) {
        let p = path();
        if let Some(d) = p.parent() {
            std::fs::create_dir_all(d).ok();
        }
        if let Ok(b) = serde_json::to_vec_pretty(self) {
            if let Err(e) = std::fs::write(&p, b) {
                crate::diag::log("prefs", format!("could not save settings: {e}"));
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn missing_fields_take_defaults() {
        let p: Prefs = serde_json::from_str(r#"{"hotkeys":{"live":"Ctrl+Alt+L"}}"#).unwrap();
        assert_eq!(p.replay_seconds, 90);
        assert_eq!(p.voice_app, "auto");
        assert_eq!(p.hotkeys["live"], "Ctrl+Alt+L");
        assert!(p.mic.is_none());
    }
}
