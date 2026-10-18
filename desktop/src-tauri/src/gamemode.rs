//! 16:9 game mode: while streaming over USB, the phone's screen is set to a
//! 16:9 size (`wm size`), so games draw a true 16:9 picture. The stream then
//! fills the whole 1920×1080 frame with the full HUD: no bars, no crop, no
//! stretch and no blurred background, like streamers with a capture card.
//! Fewer pixels to capture and encode also means less phone heat.
//!
//! The phone's own size setting is saved first and put back exactly (many
//! phones, e.g. OnePlus at "FHD+", already use a size override, so a plain
//! `wm size reset` would change the user's resolution). Three things restore
//! it, so the phone never stays in 16:9:
//! - the PC, when the phone stops streaming or disconnects;
//! - a small watcher script on the phone, when the screen capture ends for
//!   any reason (cable pulled, PC crashed);
//! - the PC at startup, from a saved note, if anything above was missed.

use std::path::PathBuf;
use std::process::Command;
use std::sync::Mutex;

#[cfg(windows)]
use std::os::windows::process::CommandExt;

use serde::{Deserialize, Serialize};

const WATCHER: &str = "/data/local/tmp/phonecast_restore.sh";

/// What was changed on which phone, kept on disk until it is restored.
#[derive(Clone, Debug, Serialize, Deserialize, PartialEq)]
pub struct Applied {
    pub serial: String,
    /// The phone's own override ("1080x2376"), or None if it had none.
    pub original: Option<String>,
    /// The 16:9 size we set ("1080x1920").
    pub applied: String,
}

#[derive(Default)]
pub struct GameMode {
    state: Mutex<Option<Applied>>,
    /// Last thing worth telling the user (shown under the switch).
    note: Mutex<String>,
}

fn note_path() -> PathBuf {
    std::env::var("APPDATA")
        .map(PathBuf::from)
        .unwrap_or_else(|_| PathBuf::from("."))
        .join("PhoneCast")
        .join("game_mode.json")
}

fn adb(serial: &str, args: &[&str]) -> Option<String> {
    let exe = crate::adb::find_adb()?;
    let mut cmd = Command::new(exe);
    cmd.args(["-s", serial]).args(args);
    #[cfg(windows)]
    cmd.creation_flags(0x0800_0000); // CREATE_NO_WINDOW
    let out = cmd.output().ok()?;
    out.status
        .success()
        .then(|| String::from_utf8_lossy(&out.stdout).into_owned())
}

/// Width × height.
pub type Size = (u32, u32);

/// Parses `wm size` output into (physical, override) as (w, h) in the
/// phone's natural (portrait) orientation.
pub fn parse_wm_size(out: &str) -> Option<(Size, Option<Size>)> {
    let size = |l: &str| -> Option<(u32, u32)> {
        let v = l.split(':').nth(1)?.trim();
        let (w, h) = v.split_once('x')?;
        Some((w.trim().parse().ok()?, h.trim().parse().ok()?))
    };
    let mut phys = None;
    let mut over = None;
    for l in out.lines() {
        if l.starts_with("Physical size") {
            phys = size(l);
        } else if l.starts_with("Override size") {
            over = size(l);
        }
    }
    Some((phys?, over))
}

/// The 16:9 size to set, keeping the current short side (so text and touch
/// keep their scale), or None when the screen is already 16:9 or wider-short
/// (tablets).
pub fn target_size(current: (u32, u32)) -> Option<(u32, u32)> {
    let (w, h) = current;
    let (short, long) = (w.min(h), w.max(h));
    let want = ((short as f64 * 16.0 / 9.0) / 2.0).round() as u32 * 2;
    if long <= want + 8 {
        return None;
    }
    // Natural orientation is portrait on phones: width = short side.
    Some(if w <= h { (short, want) } else { (want, short) })
}

/// Shell script that waits until PhoneCast's screen capture ends, then puts
/// the size back, unless the user changed it in the meantime.
fn watcher_script(a: &Applied) -> String {
    let restore = match &a.original {
        Some(o) => format!("wm size {o}"),
        None => "wm size reset".into(),
    };
    format!(
        "sleep 8\n\
         while dumpsys media_projection | grep -q ai.phonecast; do sleep 2; done\n\
         if wm size | grep -q 'Override size: {applied}'; then {restore}; fi\n",
        applied = a.applied
    )
}

impl GameMode {
    pub fn new() -> Self {
        Self::default()
    }

    pub fn note(&self) -> String {
        self.note.lock().unwrap().clone()
    }

    pub fn active(&self) -> bool {
        self.state.lock().unwrap().is_some()
    }

    fn set_note(&self, n: impl Into<String>) {
        let n = n.into();
        crate::diag::log("gamemode", n.clone());
        *self.note.lock().unwrap() = n;
    }

    /// Switches `serial` to 16:9. Safe to call again: does nothing if active.
    pub fn apply(&self, serial: &str) {
        if self.active() {
            return;
        }
        let Some(out) = adb(serial, &["shell", "wm", "size"]) else {
            self.set_note("16:9 game mode needs the USB cable (adb) — not applied");
            return;
        };
        let Some((phys, over)) = parse_wm_size(&out) else {
            self.set_note("16:9 game mode: could not read the phone's screen size");
            return;
        };
        let current = over.unwrap_or(phys);
        let Some((tw, th)) = target_size(current) else {
            self.set_note("Phone screen is already 16:9 — nothing to change");
            return;
        };
        let a = Applied {
            serial: serial.to_string(),
            original: over.map(|(w, h)| format!("{w}x{h}")),
            applied: format!("{tw}x{th}"),
        };
        // Note first: if we crash right after the change, startup restores it.
        if let Ok(b) = serde_json::to_vec_pretty(&a) {
            if let Some(d) = note_path().parent() {
                std::fs::create_dir_all(d).ok();
            }
            std::fs::write(note_path(), b).ok();
        }
        // Watcher on the phone, started before the change.
        let tmp = std::env::temp_dir().join("phonecast_restore.sh");
        let pushed = std::fs::write(&tmp, watcher_script(&a)).is_ok()
            && adb(serial, &["push", &tmp.to_string_lossy(), WATCHER]).is_some();
        if pushed {
            adb(serial, &["shell", "pkill -f 'phonecast_restor[e].sh'"]);
            adb(
                serial,
                &[
                    "shell",
                    &format!("nohup sh {WATCHER} >/dev/null 2>&1 </dev/null &"),
                ],
            );
        }
        if adb(serial, &["shell", "wm", "size", &a.applied]).is_none() {
            std::fs::remove_file(note_path()).ok();
            self.set_note("16:9 game mode: the phone refused the size change");
            return;
        }
        self.set_note(format!(
            "16:9 game mode on: phone screen {} (was {}). Restored when streaming stops.",
            a.applied,
            a.original.as_deref().unwrap_or("default")
        ));
        *self.state.lock().unwrap() = Some(a);
    }

    /// Puts the phone's own size back (if this phone is still on our size).
    pub fn restore(&self) {
        let a = self.state.lock().unwrap().take();
        let a = a.or_else(Self::saved);
        if let Some(a) = a {
            Self::restore_applied(&a);
            self.set_note(format!(
                "16:9 game mode off: phone screen back to {}",
                a.original.as_deref().unwrap_or("default")
            ));
        }
    }

    fn saved() -> Option<Applied> {
        std::fs::read(note_path())
            .ok()
            .and_then(|b| serde_json::from_slice(&b).ok())
    }

    fn restore_applied(a: &Applied) {
        let Some(out) = adb(&a.serial, &["shell", "wm", "size"]) else {
            // Phone not reachable: its watcher restores it; keep the note so
            // the next start can check again.
            return;
        };
        if let Some((_, over)) = parse_wm_size(&out) {
            if over.map(|(w, h)| format!("{w}x{h}")).as_deref() == Some(a.applied.as_str()) {
                match &a.original {
                    Some(o) => adb(&a.serial, &["shell", "wm", "size", o]),
                    None => adb(&a.serial, &["shell", "wm", "size", "reset"]),
                };
            }
        }
        adb(&a.serial, &["shell", "pkill -f 'phonecast_restor[e].sh'"]);
        std::fs::remove_file(note_path()).ok();
    }

    /// At startup: restore anything a previous run left switched.
    pub fn recover(&self) {
        if let Some(a) = Self::saved() {
            crate::diag::log("gamemode", format!("restoring after last run: {a:?}"));
            Self::restore_applied(&a);
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_wm_size() {
        let out = "Physical size: 1264x2780\nOverride size: 1080x2376\n";
        assert_eq!(parse_wm_size(out), Some(((1264, 2780), Some((1080, 2376)))));
        assert_eq!(
            parse_wm_size("Physical size: 1080x2400\n"),
            Some(((1080, 2400), None))
        );
        assert_eq!(parse_wm_size("error"), None);
    }

    #[test]
    fn targets_16x9_keeping_the_short_side() {
        assert_eq!(target_size((1080, 2376)), Some((1080, 1920)));
        assert_eq!(target_size((1264, 2780)), Some((1264, 2248)));
        assert_eq!(target_size((1080, 1920)), None); // already 16:9
        assert_eq!(target_size((1600, 2560)), None); // 16:10 tablet
    }

    #[test]
    fn watcher_restores_the_original_only_if_still_ours() {
        let a = Applied {
            serial: "x".into(),
            original: Some("1080x2376".into()),
            applied: "1080x1920".into(),
        };
        let s = watcher_script(&a);
        assert!(s.contains("Override size: 1080x1920"));
        assert!(s.contains("wm size 1080x2376"));
        let b = Applied {
            original: None,
            ..a
        };
        assert!(watcher_script(&b).contains("wm size reset"));
    }
}
