//! Local test control (off unless `PHONECAST_TEST_CONTROL=1`): lets the
//! stress-test rig drive the app like a user would (scenes, banner, live)
//! and read its status, over 127.0.0.1 only. One command per line, one
//! JSON reply per line.

use std::io::{BufRead, BufReader, Write};
use std::net::TcpListener;
use std::sync::Arc;

use serde_json::{json, Value};
use tauri::{AppHandle, Emitter};

use crate::core::Core;

pub const PORT: u16 = 27199;

pub fn enabled() -> bool {
    std::env::var("PHONECAST_TEST_CONTROL").is_ok_and(|v| v == "1")
}

pub fn spawn(app: AppHandle, core: Arc<Core>) {
    let Ok(listener) = TcpListener::bind(("127.0.0.1", PORT)) else {
        crate::diag::log("test", format!("control port {PORT} busy"));
        return;
    };
    crate::diag::log("test", format!("control port {PORT} open"));
    std::thread::spawn(move || {
        for s in listener.incoming().flatten() {
            let app = app.clone();
            let core = core.clone();
            std::thread::spawn(move || {
                let mut w = match s.try_clone() {
                    Ok(w) => w,
                    Err(_) => return,
                };
                for line in BufReader::new(s).lines().map_while(Result::ok) {
                    let reply = run(&app, &core, line.trim());
                    if writeln!(w, "{reply}").is_err() {
                        break;
                    }
                }
            });
        }
    });
}

fn ok(r: Result<(), String>) -> Value {
    match r {
        Ok(()) => json!({"ok": true}),
        Err(e) => json!({"ok": false, "error": e}),
    }
}

fn run(app: &AppHandle, core: &Arc<Core>, line: &str) -> Value {
    let (cmd, arg) = line.split_once(' ').unwrap_or((line, ""));
    match cmd {
        "status" => core.status_now(),
        "scene" => ok(core.test_scene((arg != "none").then_some(arg))),
        "ready" => json!({"ok": true, "ready": core.test_ready(arg)}),
        "prepare" => ok(app.emit("test-prepare", arg).map_err(|e| e.to_string())),
        "popup" => ok(crate::banner_feed::show_moment(arg)),
        "popups" => json!({"ok": true, "ready": crate::banner_feed::moments_ready()}),
        "banner" => {
            core.set_banner_on(arg == "on");
            json!({"ok": true})
        }
        "privacy" => {
            core.set_privacy_cover(arg == "on");
            json!({"ok": true})
        }
        "settings" => match serde_json::from_str::<Value>(arg) {
            Ok(v) => ok(core.live.update_settings(&v)),
            Err(e) => json!({"ok": false, "error": e.to_string()}),
        },
        "record" => match arg {
            "start" => match core.start_recording() {
                Ok(p) => json!({"ok": true, "path": p}),
                Err(e) => json!({"ok": false, "error": e}),
            },
            _ => match core.stop_recording() {
                Ok(p) => json!({"ok": true, "path": p}),
                Err(e) => json!({"ok": false, "error": e}),
            },
        },
        "live" => match arg {
            "start" => ok(core.start_live()),
            "stop" => {
                core.stop_live_because("test rig");
                json!({"ok": true})
            }
            "switch" => ok(core.switch_live()),
            _ => json!({"ok": false, "error": "live start|stop|switch"}),
        },
        _ => json!({"ok": false, "error": format!("unknown command {cmd}")}),
    }
}
