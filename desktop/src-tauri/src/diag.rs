//! Diagnostics log: one line per notable event (phone connect, frame skips,
//! reconnects, errors) in %APPDATA%\PhoneCast\logs, kept for 7 days.
//! When a stream "got stuck somewhere", this file says where and when.

use std::collections::VecDeque;
use std::fs::{File, OpenOptions};
use std::io::Write;
use std::path::PathBuf;
use std::sync::Mutex;
use std::time::Instant;

struct Diag {
    file: Option<File>,
    recent: VecDeque<String>,
    start: Instant,
}

static DIAG: Mutex<Option<Diag>> = Mutex::new(None);

pub fn logs_dir() -> PathBuf {
    let base = std::env::var("APPDATA")
        .map(PathBuf::from)
        .unwrap_or_else(|_| PathBuf::from("."));
    base.join("PhoneCast").join("logs")
}

pub fn init() {
    let dir = logs_dir();
    std::fs::create_dir_all(&dir).ok();
    // Keep a week of logs.
    if let Ok(entries) = std::fs::read_dir(&dir) {
        let mut files: Vec<PathBuf> = entries.flatten().map(|e| e.path()).collect();
        files.sort();
        for old in files.iter().rev().skip(7) {
            std::fs::remove_file(old).ok();
        }
    }
    let name = chrono::Local::now()
        .format("phonecast-%Y-%m-%d.log")
        .to_string();
    let file = OpenOptions::new()
        .create(true)
        .append(true)
        .open(dir.join(name))
        .ok();
    *DIAG.lock().unwrap() = Some(Diag {
        file,
        recent: VecDeque::new(),
        start: Instant::now(),
    });
    log(
        "app",
        format!("PhoneCast {} started", env!("CARGO_PKG_VERSION")),
    );
}

/// `category` is a short tag: phone, video, audio, record, live, usb, preview.
pub fn log(category: &str, msg: impl AsRef<str>) {
    let line = format!(
        "{} +{:>8.3}s [{:<7}] {}",
        chrono::Local::now().format("%H:%M:%S%.3f"),
        DIAG.lock()
            .unwrap()
            .as_ref()
            .map(|d| d.start.elapsed().as_secs_f64())
            .unwrap_or(0.0),
        category,
        msg.as_ref()
    );
    let mut g = DIAG.lock().unwrap();
    let Some(d) = g.as_mut() else { return };
    if let Some(f) = d.file.as_mut() {
        writeln!(f, "{line}").ok();
    }
    d.recent.push_back(line);
    if d.recent.len() > 300 {
        d.recent.pop_front();
    }
}

/// Last few hundred lines, for the in-app view.
pub fn recent() -> Vec<String> {
    DIAG.lock()
        .unwrap()
        .as_ref()
        .map(|d| d.recent.iter().cloned().collect())
        .unwrap_or_default()
}
