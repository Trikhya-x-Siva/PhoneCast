//! Keeps `adb reverse` set up for every USB-connected phone, so the phone app
//! can reach the PC at 127.0.0.1 with no configuration.

use std::collections::HashSet;
use std::path::PathBuf;
use std::process::Command;
use std::sync::{Arc, Mutex};
use std::time::Duration;

#[cfg(windows)]
use std::os::windows::process::CommandExt;

#[derive(Default, Clone, serde::Serialize)]
pub struct AdbState {
    pub found: bool,
    pub devices: Vec<String>,
    pub reversed: Vec<String>,
}

pub fn find_adb() -> Option<PathBuf> {
    let mut candidates = Vec::new();
    if let Ok(home) = std::env::var("ANDROID_HOME") {
        candidates.push(PathBuf::from(home).join("platform-tools").join("adb.exe"));
    }
    if let Ok(local) = std::env::var("LOCALAPPDATA") {
        candidates.push(PathBuf::from(local).join("Android\\Sdk\\platform-tools\\adb.exe"));
    }
    if let Ok(exe) = std::env::current_exe() {
        if let Some(dir) = exe.parent() {
            candidates.push(dir.join("adb.exe"));
        }
    }
    candidates
        .into_iter()
        .find(|p| p.exists())
        .or_else(|| run(&PathBuf::from("adb"), &["version"]).map(|_| PathBuf::from("adb")))
}

fn run(adb: &PathBuf, args: &[&str]) -> Option<String> {
    let mut cmd = Command::new(adb);
    cmd.args(args);
    #[cfg(windows)]
    cmd.creation_flags(0x0800_0000); // CREATE_NO_WINDOW: no console flashing
    let out = cmd.output().ok()?;
    out.status
        .success()
        .then(|| String::from_utf8_lossy(&out.stdout).into_owned())
}

pub fn spawn(port: u16, state: Arc<Mutex<AdbState>>) {
    std::thread::Builder::new()
        .name("adb".into())
        .spawn(move || {
            let mut adb = find_adb();
            let mut reversed: HashSet<String> = HashSet::new();
            loop {
                if adb.is_none() {
                    adb = find_adb();
                }
                let mut devices = Vec::new();
                if let Some(adb) = &adb {
                    if let Some(out) = run(adb, &["devices"]) {
                        devices = out
                            .lines()
                            .skip(1)
                            .filter_map(|l| l.strip_suffix("\tdevice"))
                            .map(str::to_string)
                            .collect();
                    }
                    // Re-checked every round: an adb server restart (e.g. by
                    // another tool) silently drops reverse mappings.
                    reversed.clear();
                    let spec = format!("tcp:{port}");
                    for serial in &devices {
                        let present = run(adb, &["-s", serial, "reverse", "--list"])
                            .is_some_and(|l| l.contains(&spec));
                        if present || run(adb, &["-s", serial, "reverse", &spec, &spec]).is_some() {
                            reversed.insert(serial.clone());
                        }
                    }
                }
                {
                    let mut s = state.lock().unwrap();
                    if s.devices != devices {
                        crate::diag::log("usb", format!("adb devices: {devices:?}"));
                    }
                    s.found = adb.is_some();
                    s.devices = devices;
                    s.reversed = reversed.iter().cloned().collect();
                }
                std::thread::sleep(Duration::from_secs(2));
            }
        })
        .expect("spawn adb thread");
}
