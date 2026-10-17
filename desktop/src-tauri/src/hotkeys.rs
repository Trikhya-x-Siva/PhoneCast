//! Global hotkeys that work while a game has the focus.
//!
//! Windows' `RegisterHotKey` is used from a dedicated thread with its own
//! message loop: the system delivers the key press to that thread no matter
//! which window is in the foreground, including fullscreen games, and the
//! key is swallowed so the game does not see it. No keyboard hook is
//! installed, so nothing is read except the registered combinations.
//!
//! Bindings are plain text ("Ctrl+Shift+F8") so they can be shown and saved
//! as they are.

use serde_json::{json, Value};
use std::collections::BTreeMap;
use std::fmt;
use std::sync::atomic::{AtomicU32, AtomicU64, Ordering};
use std::sync::{Arc, Mutex};

#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum Action {
    Clip15,
    Clip30,
    Clip60,
    MuteMic,
    Record,
    Live,
}

impl Action {
    pub const ALL: [Action; 6] = [
        Action::Clip15,
        Action::Clip30,
        Action::Clip60,
        Action::MuteMic,
        Action::Record,
        Action::Live,
    ];

    pub fn id(self) -> &'static str {
        match self {
            Action::Clip15 => "clip15",
            Action::Clip30 => "clip30",
            Action::Clip60 => "clip60",
            Action::MuteMic => "muteMic",
            Action::Record => "record",
            Action::Live => "live",
        }
    }

    pub fn from_id(id: &str) -> Option<Action> {
        Action::ALL.into_iter().find(|a| a.id() == id)
    }

    pub fn label(self) -> &'static str {
        match self {
            Action::Clip15 => "Save the last 15 seconds",
            Action::Clip30 => "Save the last 30 seconds",
            Action::Clip60 => "Save the last 60 seconds",
            Action::MuteMic => "Mute / unmute the mic",
            Action::Record => "Start / stop recording",
            Action::Live => "Go live / end the stream",
        }
    }

    pub fn default_keys(self) -> &'static str {
        match self {
            Action::Clip15 => "Ctrl+Shift+F8",
            Action::Clip30 => "Ctrl+Shift+F9",
            Action::Clip60 => "Ctrl+Shift+F10",
            Action::MuteMic => "Ctrl+Shift+M",
            Action::Record => "Ctrl+Shift+R",
            Action::Live => "Ctrl+Shift+L",
        }
    }
}

/// action id -> key combination text ("" = not bound).
pub type Bindings = BTreeMap<String, String>;

pub fn defaults() -> Bindings {
    Action::ALL
        .iter()
        .map(|a| (a.id().to_string(), a.default_keys().to_string()))
        .collect()
}

/// The saved bindings with defaults filled in for actions never changed.
pub fn with_defaults(saved: &Bindings) -> Bindings {
    let mut b = defaults();
    for (k, v) in saved {
        if b.contains_key(k) {
            b.insert(k.clone(), v.trim().to_string());
        }
    }
    b
}

// ---------------- key combination text ----------------

#[derive(Clone, PartialEq, Eq, Debug)]
pub struct Combo {
    pub ctrl: bool,
    pub shift: bool,
    pub alt: bool,
    pub win: bool,
    /// Windows virtual-key code.
    pub vk: u32,
}

/// Named keys and their virtual-key codes. Letters and digits are handled
/// separately (their VK code is the ASCII code).
const KEYS: &[(&str, u32)] = &[
    ("F1", 0x70),
    ("F2", 0x71),
    ("F3", 0x72),
    ("F4", 0x73),
    ("F5", 0x74),
    ("F6", 0x75),
    ("F7", 0x76),
    ("F8", 0x77),
    ("F9", 0x78),
    ("F10", 0x79),
    ("F11", 0x7A),
    ("F12", 0x7B),
    ("F13", 0x7C),
    ("F14", 0x7D),
    ("F15", 0x7E),
    ("F16", 0x7F),
    ("F17", 0x80),
    ("F18", 0x81),
    ("F19", 0x82),
    ("F20", 0x83),
    ("F21", 0x84),
    ("F22", 0x85),
    ("F23", 0x86),
    ("F24", 0x87),
    ("Space", 0x20),
    ("Pause", 0x13),
    ("ScrollLock", 0x91),
    ("PrintScreen", 0x2C),
    ("Insert", 0x2D),
    ("Delete", 0x2E),
    ("Home", 0x24),
    ("End", 0x23),
    ("PageUp", 0x21),
    ("PageDown", 0x22),
    ("Up", 0x26),
    ("Down", 0x28),
    ("Left", 0x25),
    ("Right", 0x27),
    ("Backspace", 0x08),
    ("Tab", 0x09),
    ("Enter", 0x0D),
    ("Numpad0", 0x60),
    ("Numpad1", 0x61),
    ("Numpad2", 0x62),
    ("Numpad3", 0x63),
    ("Numpad4", 0x64),
    ("Numpad5", 0x65),
    ("Numpad6", 0x66),
    ("Numpad7", 0x67),
    ("Numpad8", 0x68),
    ("Numpad9", 0x69),
    ("NumpadMultiply", 0x6A),
    ("NumpadAdd", 0x6B),
    ("NumpadSubtract", 0x6D),
    ("NumpadDecimal", 0x6E),
    ("NumpadDivide", 0x6F),
    ("Minus", 0xBD),
    ("Equal", 0xBB),
    ("Comma", 0xBC),
    ("Period", 0xBE),
    ("Slash", 0xBF),
    ("Backquote", 0xC0),
    ("BracketLeft", 0xDB),
    ("Backslash", 0xDC),
    ("BracketRight", 0xDD),
    ("Quote", 0xDE),
    ("Semicolon", 0xBA),
];

/// Parses "Ctrl+Shift+F8". The key must be a letter, digit or a named key,
/// and a modifier is required so a hotkey can never swallow plain typing.
pub fn parse(text: &str) -> Result<Combo, String> {
    let mut c = Combo {
        ctrl: false,
        shift: false,
        alt: false,
        win: false,
        vk: 0,
    };
    let parts: Vec<&str> = text
        .split('+')
        .map(str::trim)
        .filter(|p| !p.is_empty())
        .collect();
    if parts.is_empty() {
        return Err("No key".into());
    }
    for (i, p) in parts.iter().enumerate() {
        let last = i + 1 == parts.len();
        match p.to_ascii_lowercase().as_str() {
            "ctrl" | "control" if !last => c.ctrl = true,
            "shift" if !last => c.shift = true,
            "alt" if !last => c.alt = true,
            "win" | "windows" | "meta" | "super" if !last => c.win = true,
            _ if last => {
                c.vk = key_code(p).ok_or_else(|| format!("Unknown key \"{p}\""))?;
            }
            _ => return Err(format!("Unknown modifier \"{p}\"")),
        }
    }
    if c.vk == 0 {
        return Err("No key".into());
    }
    if !(c.ctrl || c.shift || c.alt || c.win) {
        return Err("Add Ctrl, Shift, Alt or Win so the key still types normally".into());
    }
    Ok(c)
}

fn key_code(name: &str) -> Option<u32> {
    let up = name.to_ascii_uppercase();
    if up.len() == 1 {
        let b = up.as_bytes()[0];
        if b.is_ascii_alphanumeric() {
            return Some(b as u32);
        }
    }
    let n = match up.as_str() {
        "ESC" => "ESCAPE",
        "RETURN" => "ENTER",
        "DEL" => "DELETE",
        "INS" => "INSERT",
        "PGUP" => "PAGEUP",
        "PGDN" | "PGDOWN" => "PAGEDOWN",
        "ARROWUP" => "UP",
        "ARROWDOWN" => "DOWN",
        "ARROWLEFT" => "LEFT",
        "ARROWRIGHT" => "RIGHT",
        other => other,
    };
    KEYS.iter()
        .find(|(k, _)| k.eq_ignore_ascii_case(n))
        .map(|(_, vk)| *vk)
}

impl fmt::Display for Combo {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        let mut parts: Vec<&str> = Vec::new();
        if self.ctrl {
            parts.push("Ctrl");
        }
        if self.shift {
            parts.push("Shift");
        }
        if self.alt {
            parts.push("Alt");
        }
        if self.win {
            parts.push("Win");
        }
        let letter;
        let key = match KEYS.iter().find(|(_, vk)| *vk == self.vk) {
            Some((k, _)) => *k,
            None => {
                letter = char::from_u32(self.vk)
                    .map(String::from)
                    .unwrap_or_default();
                letter.as_str()
            }
        };
        parts.push(key);
        write!(f, "{}", parts.join("+"))
    }
}

/// Checks a whole set of bindings: every non-empty one must parse and no two
/// may be the same. Returns (action id, problem) pairs.
pub fn problems(b: &Bindings) -> Vec<(String, String)> {
    let mut out = Vec::new();
    let mut seen: Vec<(Combo, &str)> = Vec::new();
    for (id, text) in b {
        if text.is_empty() {
            continue;
        }
        match parse(text) {
            Err(e) => out.push((id.clone(), e)),
            Ok(c) => {
                if let Some((_, other)) = seen.iter().find(|(s, _)| *s == c) {
                    let name = Action::from_id(other).map(|a| a.label()).unwrap_or(other);
                    out.push((id.clone(), format!("Already used for \"{name}\"")));
                } else {
                    seen.push((c, id));
                }
            }
        }
    }
    out
}

// ---------------- the hotkey thread ----------------

pub struct Hotkeys {
    bindings: Mutex<Bindings>,
    /// Problems from the last registration: (action id, reason).
    failures: Mutex<Vec<(String, String)>>,
    thread_id: AtomicU32,
    /// Bumped by the thread each time it (re)registers.
    applied: AtomicU64,
    wanted: AtomicU64,
}

impl Hotkeys {
    /// Starts the hotkey thread. `on_action` runs on that thread; keep it short.
    pub fn start(saved: &Bindings, on_action: Arc<dyn Fn(Action) + Send + Sync>) -> Arc<Hotkeys> {
        let h = Arc::new(Hotkeys {
            bindings: Mutex::new(with_defaults(saved)),
            failures: Mutex::new(Vec::new()),
            thread_id: AtomicU32::new(0),
            applied: AtomicU64::new(0),
            wanted: AtomicU64::new(1),
        });
        #[cfg(windows)]
        {
            let h2 = h.clone();
            std::thread::Builder::new()
                .name("hotkeys".into())
                .spawn(move || win::run(h2, on_action))
                .ok();
        }
        #[cfg(not(windows))]
        let _ = on_action;
        h
    }

    /// Applies new bindings. Returns the problems (invalid text, duplicates,
    /// combinations another app already owns).
    pub fn set(&self, b: &Bindings) -> Vec<(String, String)> {
        let b = with_defaults(b);
        let mut probs = problems(&b);
        *self.bindings.lock().unwrap() = b;
        let gen = self.wanted.fetch_add(1, Ordering::SeqCst) + 1;
        #[cfg(windows)]
        win::wake(self.thread_id.load(Ordering::SeqCst));
        // Wait briefly for the thread to re-register so the caller can
        // report conflicts right away.
        let until = std::time::Instant::now() + std::time::Duration::from_millis(400);
        while self.applied.load(Ordering::SeqCst) < gen && std::time::Instant::now() < until {
            std::thread::sleep(std::time::Duration::from_millis(10));
        }
        probs.extend(self.failures.lock().unwrap().iter().cloned());
        probs
    }

    pub fn bindings(&self) -> Bindings {
        self.bindings.lock().unwrap().clone()
    }

    pub fn status_json(&self) -> Value {
        let b = self.bindings();
        let failures = self.failures.lock().unwrap().clone();
        let mut probs = problems(&b);
        probs.extend(failures);
        json!({
            "actions": Action::ALL.iter().map(|a| json!({
                "id": a.id(),
                "label": a.label(),
                "keys": b.get(a.id()).cloned().unwrap_or_default(),
                "default": a.default_keys(),
                "problem": probs.iter().find(|(id, _)| id == a.id()).map(|(_, p)| p.clone()),
            })).collect::<Vec<_>>(),
        })
    }
}

#[cfg(windows)]
mod win {
    use super::*;
    use windows_sys::Win32::System::Threading::GetCurrentThreadId;
    use windows_sys::Win32::UI::Input::KeyboardAndMouse::{RegisterHotKey, UnregisterHotKey};
    use windows_sys::Win32::UI::WindowsAndMessaging::{
        GetMessageW, PostThreadMessageW, MSG, WM_APP, WM_HOTKEY,
    };

    const MOD_ALT: u32 = 0x0001;
    const MOD_CONTROL: u32 = 0x0002;
    const MOD_SHIFT: u32 = 0x0004;
    const MOD_WIN: u32 = 0x0008;
    const MOD_NOREPEAT: u32 = 0x4000;

    pub fn wake(thread_id: u32) {
        if thread_id != 0 {
            unsafe { PostThreadMessageW(thread_id, WM_APP, 0, 0) };
        }
    }

    fn register(h: &Hotkeys) -> Vec<Action> {
        let bindings = h.bindings();
        let mut registered = Vec::new();
        let mut failures = Vec::new();
        for (i, a) in Action::ALL.iter().enumerate() {
            let Some(text) = bindings.get(a.id()).filter(|t| !t.is_empty()) else {
                continue;
            };
            let Ok(c) = parse(text) else {
                continue; // reported by problems()
            };
            let mut mods = MOD_NOREPEAT;
            if c.ctrl {
                mods |= MOD_CONTROL;
            }
            if c.shift {
                mods |= MOD_SHIFT;
            }
            if c.alt {
                mods |= MOD_ALT;
            }
            if c.win {
                mods |= MOD_WIN;
            }
            let ok = unsafe { RegisterHotKey(std::ptr::null_mut(), i as i32 + 1, mods, c.vk) };
            if ok != 0 {
                registered.push(*a);
            } else {
                failures.push((
                    a.id().to_string(),
                    format!("{text} is already used by another app"),
                ));
                crate::diag::log(
                    "hotkeys",
                    format!("could not register {text} for {}", a.label()),
                );
            }
        }
        crate::diag::log("hotkeys", format!("{} hotkeys active", registered.len()));
        *h.failures.lock().unwrap() = failures;
        registered
    }

    fn unregister(registered: &[Action]) {
        for a in registered {
            let i = Action::ALL.iter().position(|x| x == a).unwrap_or(0);
            unsafe { UnregisterHotKey(std::ptr::null_mut(), i as i32 + 1) };
        }
    }

    pub fn run(h: Arc<Hotkeys>, on_action: Arc<dyn Fn(Action) + Send + Sync>) {
        h.thread_id
            .store(unsafe { GetCurrentThreadId() }, Ordering::SeqCst);
        let mut registered = register(&h);
        h.applied
            .store(h.wanted.load(Ordering::SeqCst), Ordering::SeqCst);
        let mut msg: MSG = unsafe { std::mem::zeroed() };
        loop {
            let r = unsafe { GetMessageW(&mut msg, std::ptr::null_mut(), 0, 0) };
            if r <= 0 {
                break;
            }
            match msg.message {
                WM_HOTKEY => {
                    let idx = msg.wParam as usize;
                    if let Some(a) = idx.checked_sub(1).and_then(|i| Action::ALL.get(i)) {
                        on_action(*a);
                    }
                }
                WM_APP => {
                    unregister(&registered);
                    registered = register(&h);
                    h.applied
                        .store(h.wanted.load(Ordering::SeqCst), Ordering::SeqCst);
                }
                _ => {}
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_and_prints_canonically() {
        let c = parse("ctrl + shift + f8").unwrap();
        assert_eq!(c.to_string(), "Ctrl+Shift+F8");
        assert_eq!(c.vk, 0x77);
        assert_eq!(parse("Alt+m").unwrap().to_string(), "Alt+M");
        assert_eq!(parse("Win+Numpad5").unwrap().to_string(), "Win+Numpad5");
        assert_eq!(parse("Shift+Ctrl+3").unwrap().to_string(), "Ctrl+Shift+3");
        for a in Action::ALL {
            let c = parse(a.default_keys()).unwrap();
            assert_eq!(c.to_string(), a.default_keys());
        }
    }

    #[test]
    fn rejects_bad_text() {
        assert!(parse("").is_err());
        assert!(parse("F8").is_err(), "a bare key would swallow typing");
        assert!(parse("Ctrl+Banana").is_err());
        assert!(parse("Ctrl+Shift").is_err());
        assert!(parse("Hyper+F8").is_err());
    }

    #[test]
    fn finds_duplicates_and_fills_defaults() {
        let mut b = Bindings::new();
        b.insert("clip15".into(), "Ctrl+Shift+F9".into()); // same as clip30's default
        b.insert("live".into(), "".into()); // unbound
        b.insert("bogus".into(), "Ctrl+X".into()); // unknown action: ignored
        let full = with_defaults(&b);
        assert_eq!(full.len(), Action::ALL.len());
        assert_eq!(full["record"], "Ctrl+Shift+R");
        assert_eq!(full["live"], "");
        let p = problems(&full);
        assert_eq!(p.len(), 1);
        assert_eq!(p[0].0, "clip30");
        assert!(p[0].1.contains("Already used"));
        assert!(problems(&defaults()).is_empty());
    }
}
