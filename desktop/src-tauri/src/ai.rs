//! Bridge to the optional local AI helper (`phonecast-ai-helper`), a
//! separate process so a crash there can never take the stream down.
//!
//! The app listens on a loopback port, starts the helper with that port and
//! a one-time token, forwards audio levels (and, when the sound classifier
//! is on, 16 kHz mono audio), and relays what the helper says to the UI as
//! `ai-*` events. Everything is off until the streamer enables it; turning
//! it off kills the process.

use crate::audio::Mixer;
use crate::diag;
use serde_json::{json, Value};
use std::collections::VecDeque;
use std::hash::{BuildHasher, Hasher};
use std::io::{BufRead, BufReader, Write};
use std::net::{TcpListener, TcpStream};
use std::path::PathBuf;
use std::process::{Child, Command, Stdio};
use std::sync::{Arc, Mutex};
use std::time::Duration;
use tauri::{AppHandle, Emitter, Manager};

pub const HELPER_EXE: &str = "phonecast-ai-helper.exe";

struct Proc {
    child: Child,
    sock: TcpStream,
    generation: u64,
}

pub struct Ai {
    app: AppHandle,
    mixer: Arc<Mixer>,
    settings: Mutex<Value>,
    proc: Mutex<Option<Proc>>,
    status: Mutex<Value>,
    chat: Mutex<VecDeque<Value>>,
    modlog: Mutex<VecDeque<String>>,
    generation: Mutex<u64>,
}

fn settings_path() -> PathBuf {
    let base = std::env::var("APPDATA")
        .map(PathBuf::from)
        .unwrap_or_else(|_| PathBuf::from("."));
    base.join("PhoneCast").join("ai.json")
}

fn random_token() -> String {
    let s = std::collections::hash_map::RandomState::new();
    let mut h = s.build_hasher();
    h.write_u64(std::process::id() as u64);
    let a = h.finish();
    let mut h2 = s.build_hasher();
    h2.write_u64(a ^ 0x9e37_79b9_7f4a_7c15);
    format!("{a:016x}{:016x}", h2.finish())
}

impl Ai {
    pub fn new(app: AppHandle, mixer: Arc<Mixer>) -> Arc<Ai> {
        let settings = std::fs::read(settings_path())
            .ok()
            .and_then(|b| serde_json::from_slice(&b).ok())
            .unwrap_or_else(|| json!({ "enabled": false }));
        let ai = Arc::new(Ai {
            app,
            mixer,
            settings: Mutex::new(settings),
            proc: Mutex::new(None),
            status: Mutex::new(Value::Null),
            chat: Mutex::new(VecDeque::new()),
            modlog: Mutex::new(VecDeque::new()),
            generation: Mutex::new(0),
        });
        // Pure mode: no AI helper at all (no CPU while streaming).
        if ai.enabled() && !crate::live::pure() {
            let a = ai.clone();
            std::thread::Builder::new()
                .name("ai-start".into())
                .spawn(move || {
                    if let Err(e) = a.start() {
                        diag::log("ai", format!("helper did not start: {e}"));
                        a.emit_toast(format!("AI helper did not start: {e}"));
                    }
                })
                .ok();
        }
        ai
    }

    fn enabled(&self) -> bool {
        self.settings.lock().unwrap()["enabled"].as_bool() == Some(true)
    }

    pub fn settings_json(&self) -> Value {
        self.settings.lock().unwrap().clone()
    }

    pub fn status_json(&self) -> Value {
        let running = self.proc.lock().unwrap().is_some();
        json!({
            "running": running,
            "helper": self.status.lock().unwrap().clone(),
            "helperPath": find_helper(&self.app).map(|p| p.display().to_string()),
            "chat": self.chat.lock().unwrap().iter().cloned().collect::<Vec<_>>(),
            "modlog": self.modlog.lock().unwrap().iter().cloned().collect::<Vec<_>>(),
        })
    }

    /// Saves settings, then starts/stops/updates the helper accordingly.
    pub fn update_settings(self: &Arc<Self>, v: Value) -> Result<(), String> {
        let enabled = v["enabled"].as_bool() == Some(true);
        {
            let mut s = self.settings.lock().unwrap();
            *s = v.clone();
            let p = settings_path();
            if let Some(d) = p.parent() {
                std::fs::create_dir_all(d).ok();
            }
            std::fs::write(p, serde_json::to_vec_pretty(&v).unwrap_or_default())
                .map_err(|e| e.to_string())?;
        }
        if !enabled {
            self.stop();
            return Ok(());
        }
        let running = self.proc.lock().unwrap().is_some();
        if !running {
            self.start()?;
        } else {
            self.send(&json!({ "t": "settings", "settings": v }));
            self.apply_tap();
        }
        Ok(())
    }

    /// Classifier on → the mixer also hands us 16 kHz mono audio.
    fn apply_tap(&self) {
        let s = self.settings.lock().unwrap();
        let on = s["enabled"].as_bool() == Some(true)
            && s["highlights"]["enabled"].as_bool() != Some(false)
            && s["highlights"]["classifier"].as_bool() == Some(true);
        self.mixer.set_ai_tap(true, on);
    }

    /// Forwards any message to the helper (approve, download, login…).
    pub fn send(&self, m: &Value) -> bool {
        let mut g = self.proc.lock().unwrap();
        let Some(p) = g.as_mut() else { return false };
        let mut line = m.to_string();
        line.push('\n');
        if p.sock.write_all(line.as_bytes()).is_err() {
            diag::log("ai", "helper socket closed");
            return false;
        }
        true
    }

    pub fn start(self: &Arc<Self>) -> Result<(), String> {
        if self.proc.lock().unwrap().is_some() {
            return Ok(());
        }
        let exe = find_helper(&self.app).ok_or_else(|| {
            format!("{HELPER_EXE} is not installed next to PhoneCast (build desktop/ai-helper)")
        })?;
        let listener =
            TcpListener::bind(("127.0.0.1", 0)).map_err(|e| format!("no loopback port: {e}"))?;
        let port = listener.local_addr().map_err(|e| e.to_string())?.port();
        let token = random_token();
        let mut cmd = Command::new(&exe);
        cmd.arg("--port")
            .arg(port.to_string())
            .arg("--token")
            .arg(&token)
            .stdin(Stdio::null())
            .stdout(Stdio::null())
            .stderr(Stdio::null());
        #[cfg(windows)]
        {
            use std::os::windows::process::CommandExt;
            cmd.creation_flags(0x0800_0000); // CREATE_NO_WINDOW
        }
        let mut child = cmd
            .spawn()
            .map_err(|e| format!("could not start helper: {e}"))?;
        listener.set_nonblocking(true).ok();
        let deadline = std::time::Instant::now() + Duration::from_secs(10);
        let sock = loop {
            match listener.accept() {
                Ok((s, _)) => break s,
                Err(_) if std::time::Instant::now() < deadline => {
                    std::thread::sleep(Duration::from_millis(50));
                }
                Err(_) => {
                    child.kill().ok();
                    return Err("helper did not connect".into());
                }
            }
        };
        // On Windows an accepted socket inherits the listener's non-blocking
        // mode: the reader then hit "would block" on its first empty read,
        // took that for the helper quitting and stopped it ~3 ms after start.
        sock.set_nonblocking(false).ok();
        sock.set_nodelay(true).ok();
        sock.set_read_timeout(Some(Duration::from_secs(5))).ok();
        let mut reader = BufReader::new(sock.try_clone().map_err(|e| e.to_string())?);
        let mut hello = String::new();
        reader.read_line(&mut hello).map_err(|e| e.to_string())?;
        let hello: Value = serde_json::from_str(&hello).unwrap_or(Value::Null);
        if hello["t"] != "hello" || hello["token"] != token {
            child.kill().ok();
            return Err("unexpected process connected to the helper port".into());
        }
        sock.set_read_timeout(None).ok();
        diag::log(
            "ai",
            format!(
                "helper {} started (pid {}, port {port})",
                hello["version"].as_str().unwrap_or("?"),
                child.id()
            ),
        );
        let generation = {
            let mut g = self.generation.lock().unwrap();
            *g += 1;
            *g
        };
        *self.proc.lock().unwrap() = Some(Proc {
            child,
            sock,
            generation,
        });
        self.send(&json!({ "t": "settings", "settings": self.settings_json() }));
        self.apply_tap();

        // Reader: helper → UI events.
        let a = self.clone();
        std::thread::Builder::new()
            .name("ai-reader".into())
            .spawn(move || {
                for line in reader.lines() {
                    let Ok(line) = line else { break };
                    if let Ok(v) = serde_json::from_str::<Value>(&line) {
                        a.on_message(v);
                    }
                }
                a.on_exit(generation);
            })
            .map_err(|e| e.to_string())?;

        // Audio levels 10×/s.
        let a = self.clone();
        std::thread::Builder::new()
            .name("ai-audio".into())
            .spawn(move || {
                let mut idle = 0u32;
                loop {
                    std::thread::sleep(Duration::from_millis(100));
                    let alive = a
                        .proc
                        .lock()
                        .unwrap()
                        .as_ref()
                        .is_some_and(|p| p.generation == generation);
                    if !alive {
                        return;
                    }
                    let Some((game, mic, pcm)) = a.mixer.take_ai_tap() else {
                        idle += 1;
                        if idle > 50 {
                            return;
                        }
                        continue;
                    };
                    let mut m = json!({ "t": "audio", "game": game, "mic": mic });
                    if let Some(p) = pcm {
                        use base64_lite::encode;
                        m["pcm"] = Value::String(encode(&p));
                    }
                    a.send(&m);
                }
            })
            .map_err(|e| e.to_string())?;
        self.app.emit("ai-state", json!({ "running": true })).ok();
        Ok(())
    }

    fn on_exit(&self, generation: u64) {
        let mut g = self.proc.lock().unwrap();
        if g.as_ref().is_some_and(|p| p.generation == generation) {
            if let Some(mut p) = g.take() {
                p.child.kill().ok();
                p.child.wait().ok();
            }
            drop(g);
            self.mixer.set_ai_tap(false, false);
            *self.status.lock().unwrap() = Value::Null;
            diag::log("ai", "helper stopped");
            let msg = if self.enabled() {
                "AI helper stopped unexpectedly. Turn it off and on again to restart it."
            } else {
                "AI helper stopped"
            };
            self.app
                .emit("ai-state", json!({ "running": false, "message": msg }))
                .ok();
        }
    }

    /// Kills the helper. Called when the streamer turns the AI off and
    /// when the app closes.
    pub fn stop(&self) {
        let taken = self.proc.lock().unwrap().take();
        if let Some(mut p) = taken {
            let mut line = json!({ "t": "shutdown" }).to_string();
            line.push('\n');
            p.sock.write_all(line.as_bytes()).ok();
            std::thread::sleep(Duration::from_millis(100));
            p.child.kill().ok();
            p.child.wait().ok();
            diag::log("ai", "helper stopped (disabled)");
        }
        self.mixer.set_ai_tap(false, false);
        *self.status.lock().unwrap() = Value::Null;
        self.app.emit("ai-state", json!({ "running": false })).ok();
    }

    fn on_message(&self, v: Value) {
        match v["t"].as_str().unwrap_or("") {
            "status" => {
                *self.status.lock().unwrap() = v.clone();
                self.app.emit("ai-status", v).ok();
            }
            "chat" => {
                let mut c = self.chat.lock().unwrap();
                c.push_back(v.clone());
                if c.len() > 200 {
                    c.pop_front();
                }
                drop(c);
                self.app.emit("ai-chat", v).ok();
            }
            "modLog" => {
                let line = format!(
                    "{} {}",
                    chrono::Local::now().format("%H:%M:%S"),
                    v["line"].as_str().unwrap_or("")
                );
                diag::log("ai", &line);
                let mut m = self.modlog.lock().unwrap();
                m.push_back(line.clone());
                if m.len() > 200 {
                    m.pop_front();
                }
                drop(m);
                self.app.emit("ai-modlog", json!({ "line": line })).ok();
            }
            "highlight" => {
                diag::log(
                    "ai",
                    format!(
                        "highlight score {:.1}: {}",
                        v["score"].as_f64().unwrap_or(0.0),
                        v["reason"].as_str().unwrap_or("")
                    ),
                );
                self.app.emit("ai-highlight", v).ok();
            }
            "faq" => {
                self.app.emit("ai-faq", v).ok();
            }
            "deviceCode" => {
                self.app.emit("ai-device-code", v).ok();
            }
            "toast" => self.emit_toast(v["text"].as_str().unwrap_or("").to_string()),
            _ => {}
        }
    }

    fn emit_toast(&self, text: String) {
        self.app.emit("ai-toast", json!({ "text": text })).ok();
    }

    /// Opens or closes the always-on-top chat overlay window.
    pub fn set_overlay(&self, on: bool) -> Result<(), String> {
        if let Some(w) = self.app.get_webview_window("overlay") {
            if on {
                w.show().ok();
                w.set_focus().ok();
            } else {
                w.close().map_err(|e| e.to_string())?;
            }
            return Ok(());
        }
        if !on {
            return Ok(());
        }
        tauri::WebviewWindowBuilder::new(
            &self.app,
            "overlay",
            tauri::WebviewUrl::App("overlay.html".into()),
        )
        .title("OSSG chat")
        .inner_size(340.0, 460.0)
        .min_inner_size(220.0, 160.0)
        .always_on_top(true)
        .decorations(false)
        .transparent(true)
        .skip_taskbar(true)
        .build()
        .map(|_| ())
        .map_err(|e| e.to_string())
    }
}

/// The helper lives next to the app (installed) or in its own target
/// folder (development).
pub fn find_helper(app: &AppHandle) -> Option<PathBuf> {
    let mut candidates = Vec::new();
    if let Ok(exe) = std::env::current_exe() {
        if let Some(dir) = exe.parent() {
            candidates.push(dir.join(HELPER_EXE));
            for profile in ["release", "debug"] {
                candidates.push(
                    dir.join("..")
                        .join("..")
                        .join("..")
                        .join("ai-helper")
                        .join("target")
                        .join(profile)
                        .join(HELPER_EXE),
                );
            }
        }
    }
    if let Ok(res) = app.path().resource_dir() {
        candidates.push(res.join(HELPER_EXE));
    }
    candidates.into_iter().find(|p| p.is_file())
}

/// Tiny base64 encoder (standard alphabet) so the app needs no extra crate.
mod base64_lite {
    const T: &[u8; 64] = b"ABCDEFGHIJKLMNOPQRSTUVWXYZabcdefghijklmnopqrstuvwxyz0123456789+/";
    pub fn encode(data: &[u8]) -> String {
        let mut out = String::with_capacity(data.len().div_ceil(3) * 4);
        for c in data.chunks(3) {
            let b = [c[0], *c.get(1).unwrap_or(&0), *c.get(2).unwrap_or(&0)];
            let n = (b[0] as u32) << 16 | (b[1] as u32) << 8 | b[2] as u32;
            out.push(T[(n >> 18) as usize & 63] as char);
            out.push(T[(n >> 12) as usize & 63] as char);
            out.push(if c.len() > 1 {
                T[(n >> 6) as usize & 63] as char
            } else {
                '='
            });
            out.push(if c.len() > 2 {
                T[n as usize & 63] as char
            } else {
                '='
            });
        }
        out
    }

    #[cfg(test)]
    mod tests {
        #[test]
        fn matches_reference() {
            assert_eq!(super::encode(b""), "");
            assert_eq!(super::encode(b"f"), "Zg==");
            assert_eq!(super::encode(b"fo"), "Zm8=");
            assert_eq!(super::encode(b"foo"), "Zm9v");
            assert_eq!(super::encode(b"foobar"), "Zm9vYmFy");
        }
    }
}
