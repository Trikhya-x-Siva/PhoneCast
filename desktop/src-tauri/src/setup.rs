//! First-run setup helpers: install the FFmpeg encoder engine with one click
//! and measure upload speed to recommend stream quality.
//!
//! No extra crates: downloads use Windows' built-in curl.exe, checksums use
//! certutil, extraction uses tar (all present on Windows 10 1803+).

use crate::presets::{self, Platform};
use serde_json::{json, Value};
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};
use std::sync::atomic::{AtomicU8, Ordering};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

#[cfg(windows)]
use std::os::windows::process::CommandExt;

fn hide(cmd: &mut Command) {
    #[cfg(windows)]
    cmd.creation_flags(0x0800_0000);
}

/// Where PhoneCast keeps its own FFmpeg copy (checked first by `find_ffmpeg`).
pub fn ffmpeg_dir() -> PathBuf {
    let base = std::env::var("LOCALAPPDATA")
        .map(PathBuf::from)
        .unwrap_or_else(|_| PathBuf::from("."));
    base.join("PhoneCast").join("ffmpeg")
}

pub fn managed_ffmpeg() -> Option<PathBuf> {
    let p = ffmpeg_dir().join("ffmpeg.exe");
    p.exists().then_some(p)
}

fn run(program: &str, args: &[&str]) -> Result<String, String> {
    let mut cmd = Command::new(program);
    cmd.args(args).stdin(Stdio::null());
    hide(&mut cmd);
    let out = cmd.output().map_err(|e| format!("{program}: {e}"))?;
    if !out.status.success() {
        return Err(format!(
            "{program} failed: {}",
            String::from_utf8_lossy(&out.stderr).trim()
        ));
    }
    Ok(String::from_utf8_lossy(&out.stdout).into_owned())
}

/// Download state shared with the UI: 0..=100 percent, 255 = failed.
#[derive(Clone)]
pub struct DownloadState {
    pub percent: Arc<AtomicU8>,
    pub message: Arc<Mutex<String>>,
}

impl Default for DownloadState {
    fn default() -> Self {
        Self {
            percent: Arc::new(AtomicU8::new(0)),
            message: Arc::new(Mutex::new("Starting…".into())),
        }
    }
}

impl DownloadState {
    fn set(&self, percent: u8, msg: &str) {
        self.percent.store(percent, Ordering::SeqCst);
        *self.message.lock().unwrap() = msg.into();
    }

    pub fn json(&self) -> Value {
        let p = self.percent.load(Ordering::SeqCst);
        json!({
            "percent": p.min(100),
            "failed": p == 255,
            "done": p == 100,
            "message": *self.message.lock().unwrap(),
        })
    }
}

/// SHA-256 line from `certutil -hashfile` (older Windows prints it with
/// spaces between byte pairs).
fn parse_certutil(out: &str) -> String {
    out.lines()
        .map(|l| l.replace(' ', "").to_lowercase())
        .find(|l| l.len() == 64 && l.chars().all(|c| c.is_ascii_hexdigit()))
        .unwrap_or_default()
}

/// Downloads the current FFmpeg release build from gyan.dev, verifies its
/// SHA-256 against the published checksum, and installs ffmpeg.exe +
/// ffprobe.exe into `ffmpeg_dir()`. Blocking; call on a thread.
pub fn install_ffmpeg(state: &DownloadState) -> Result<PathBuf, String> {
    install_ffmpeg_to(&ffmpeg_dir(), state)
}

pub fn install_ffmpeg_to(dir: &Path, state: &DownloadState) -> Result<PathBuf, String> {
    let fail = |s: &DownloadState, m: String| {
        s.set(255, &m);
        m
    };
    state.set(1, "Checking the latest version…");
    let version = run(
        "curl.exe",
        &[
            "-sSLf",
            "https://www.gyan.dev/ffmpeg/builds/release-version",
        ],
    )
    .map_err(|e| fail(state, e))?
    .trim()
    .to_string();
    if version.is_empty() || !version.chars().all(|c| c.is_ascii_digit() || c == '.') {
        return Err(fail(state, "Could not read the FFmpeg version".into()));
    }
    let base = format!(
        "https://www.gyan.dev/ffmpeg/builds/packages/ffmpeg-{version}-essentials_build.zip"
    );
    let expected = run("curl.exe", &["-sSLf", &format!("{base}.sha256")])
        .map_err(|e| fail(state, e))?
        .trim()
        .to_lowercase();
    if expected.len() != 64 || !expected.chars().all(|c| c.is_ascii_hexdigit()) {
        return Err(fail(state, "Could not read the FFmpeg checksum".into()));
    }

    let tmp = std::env::temp_dir().join(format!("phonecast-ffmpeg-{}", std::process::id()));
    std::fs::remove_dir_all(&tmp).ok();
    std::fs::create_dir_all(&tmp).map_err(|e| fail(state, e.to_string()))?;
    let zip = tmp.join("ffmpeg.zip");
    let cleanup = || {
        std::fs::remove_dir_all(&tmp).ok();
    };

    // Download in the background and report progress from the file size.
    state.set(2, &format!("Downloading FFmpeg {version} (about 115 MB)…"));
    let mut cmd = Command::new("curl.exe");
    cmd.args(["-sSLf", "-o", &zip.display().to_string(), &base])
        .stdin(Stdio::null())
        .stderr(Stdio::null());
    hide(&mut cmd);
    let mut child = cmd.spawn().map_err(|e| fail(state, e.to_string()))?;
    let total: u64 = 115_000_000;
    loop {
        if let Some(status) = child.try_wait().map_err(|e| fail(state, e.to_string()))? {
            if !status.success() {
                cleanup();
                return Err(fail(
                    state,
                    "Download failed — check your internet connection".into(),
                ));
            }
            break;
        }
        let done = std::fs::metadata(&zip).map(|m| m.len()).unwrap_or(0);
        let pct = 2 + (done * 80 / total).min(80) as u8;
        state.set(
            pct,
            &format!("Downloading FFmpeg {version}… {} MB", done / 1_000_000),
        );
        std::thread::sleep(Duration::from_millis(300));
    }

    state.set(85, "Verifying checksum…");
    let hash_out = run(
        "certutil",
        &["-hashfile", &zip.display().to_string(), "SHA256"],
    )
    .map_err(|e| fail(state, e))?;
    if parse_certutil(&hash_out) != expected {
        cleanup();
        return Err(fail(
            state,
            "Downloaded file did not match its checksum — not installed".into(),
        ));
    }

    state.set(90, "Unpacking…");
    run(
        "tar",
        &[
            "-xf",
            &zip.display().to_string(),
            "-C",
            &tmp.display().to_string(),
        ],
    )
    .map_err(|e| fail(state, e))?;
    let bin = std::fs::read_dir(&tmp)
        .map_err(|e| fail(state, e.to_string()))?
        .flatten()
        .map(|e| e.path())
        .find(|p| p.is_dir() && p.join("bin").join("ffmpeg.exe").exists())
        .map(|p| p.join("bin"))
        .ok_or_else(|| fail(state, "Unexpected archive layout".into()))?;

    std::fs::create_dir_all(dir).map_err(|e| fail(state, e.to_string()))?;
    for f in ["ffmpeg.exe", "ffprobe.exe"] {
        std::fs::copy(bin.join(f), dir.join(f)).map_err(|e| fail(state, e.to_string()))?;
    }
    // Keep the licence next to the binaries (GPL build).
    if let Some(root) = bin.parent() {
        for f in ["LICENSE", "LICENSE.txt"] {
            let src = root.join(f);
            if src.exists() {
                std::fs::copy(src, dir.join("FFMPEG-LICENSE.txt")).ok();
            }
        }
    }
    cleanup();
    state.set(100, &format!("FFmpeg {version} installed"));
    crate::diag::log(
        "setup",
        format!("FFmpeg {version} installed to {}", dir.display()),
    );
    Ok(dir.join("ffmpeg.exe"))
}

/// One timed upload to Cloudflare's speed-test endpoint (zeros, nothing
/// identifying). Returns Mbps over the transfer itself, excluding the
/// connection setup.
fn upload_once(file: &Path) -> Result<f64, String> {
    let out = run(
        "curl.exe",
        &[
            "-s",
            "-o",
            "NUL",
            "--max-time",
            "20",
            "-w",
            "%{http_code} %{size_upload} %{time_pretransfer} %{time_total}",
            "-X",
            "POST",
            "-H",
            "Content-Type: application/octet-stream",
            "--data-binary",
            &format!("@{}", file.display()),
            "https://speed.cloudflare.com/__up",
        ],
    )?;
    let v: Vec<&str> = out.split_whitespace().collect();
    let num = |i: usize| v.get(i).and_then(|s| s.parse::<f64>().ok()).unwrap_or(0.0);
    let (code, bytes, pre, total) = (v.first().copied().unwrap_or(""), num(1), num(2), num(3));
    let secs = total - pre;
    if code != "200" || bytes <= 0.0 || secs <= 0.0 {
        return Err("Speed test failed — check your internet connection".into());
    }
    Ok(bytes * 8.0 / secs / 1e6)
}

/// Summary of several upload samples.
#[derive(Debug, Clone, PartialEq)]
pub struct UploadResult {
    pub min: f64,
    pub median: f64,
    pub max: f64,
    pub samples: usize,
}

pub fn summarize(samples: &[f64]) -> Option<UploadResult> {
    let mut v: Vec<f64> = samples
        .iter()
        .copied()
        .filter(|x| x.is_finite() && *x > 0.0)
        .collect();
    if v.is_empty() {
        return None;
    }
    v.sort_by(|a, b| a.total_cmp(b));
    Some(UploadResult {
        min: v[0],
        median: v[v.len() / 2],
        max: v[v.len() - 1],
        samples: v.len(),
    })
}

/// Measures upload for about `seconds` with back-to-back timed uploads, each
/// sized to take ~1.5 s, and reports the MINIMUM: a stream has to survive
/// the worst moment, not the average. `progress(fraction, last_mbps)`.
pub fn upload_speed_test(
    seconds: f64,
    progress: &dyn Fn(f64, f64),
) -> Result<UploadResult, String> {
    let file = std::env::temp_dir().join(format!("phonecast-speedtest-{}.bin", std::process::id()));
    let mut size: usize = 1 << 20;
    let mut samples = Vec::new();
    let start = Instant::now();
    let mut failures = 0;
    while start.elapsed().as_secs_f64() < seconds || samples.len() < 4 {
        if samples.len() >= 12 || failures >= 3 {
            break;
        }
        std::fs::write(&file, vec![0u8; size]).map_err(|e| e.to_string())?;
        match upload_once(&file) {
            Ok(mbps) => {
                samples.push(mbps);
                progress((start.elapsed().as_secs_f64() / seconds).min(0.99), mbps);
                // Next chunk: ~1.5 s at this speed, 1–25 MB.
                size = ((mbps * 1e6 / 8.0 * 1.5) as usize).clamp(1 << 20, 25 << 20);
            }
            Err(_) => failures += 1,
        }
    }
    std::fs::remove_file(&file).ok();
    let r = summarize(&samples)
        .ok_or_else(|| "Speed test failed — check your internet connection".to_string())?;
    crate::diag::log(
        "setup",
        format!(
            "upload test: min {:.1} / median {:.1} / max {:.1} Mbps ({} samples)",
            r.min, r.median, r.max, r.samples
        ),
    );
    Ok(r)
}

/// Per-platform recommendation for the wizard, based on the minimum upload.
pub fn recommendations(min_mbps: f64, hevc: bool) -> Value {
    let per: Vec<Value> = presets::PLATFORMS
        .iter()
        .filter(|p| **p != Platform::Custom)
        .map(|p| {
            let r = presets::recommend(*p, min_mbps, hevc);
            json!({
                "platform": p.id(),
                "name": p.name(),
                "preset": r.id,
                "label": r.label,
                "uploadKbps": r.upload_kbps(),
            })
        })
        .collect();
    let usable = min_mbps * presets::HEADROOM;
    let note = if min_mbps < 6.0 {
        "Your upload is slow for live streaming: 720p to one platform is the realistic choice."
    } else if usable >= 31.0 {
        "Plenty for YouTube and Twitch at the same time at full quality."
    } else if usable >= 13.0 {
        "Good for one platform at full quality; when you stream to two, PhoneCast lowers the second one automatically."
    } else {
        "Fine for one platform; streaming to several will lower the quality of all but the main one."
    };
    json!({ "platforms": per, "note": note })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn certutil_formats() {
        let new = "SHA256 hash of x.zip:\r\n60f467265b1e312373dbcd92200c2618a74850f98d3d078e94296bb3fa2047ba\r\nCertUtil: -hashfile command completed successfully.\r\n";
        let old = "SHA256 hash of file x.zip:\r\n60 f4 67 26 5b 1e 31 23 73 db cd 92 20 0c 26 18 a7 48 50 f9 8d 3d 07 8e 94 29 6b b3 fa 20 47 ba\r\nCertUtil: done\r\n";
        let want = "60f467265b1e312373dbcd92200c2618a74850f98d3d078e94296bb3fa2047ba";
        assert_eq!(parse_certutil(new), want);
        assert_eq!(parse_certutil(old), want);
        assert_eq!(parse_certutil("nothing here"), "");
    }

    #[test]
    fn upload_summary_uses_minimum() {
        let r = summarize(&[40.0, 12.0, 38.0, 41.0, f64::NAN, 0.0]).unwrap();
        assert_eq!(r.min, 12.0);
        assert_eq!(r.max, 41.0);
        assert_eq!(r.samples, 4);
        assert_eq!(r.median, 40.0);
        assert!(summarize(&[]).is_none());
    }

    #[test]
    fn recommendations_cover_each_platform() {
        let v = recommendations(50.0, true);
        let list = v["platforms"].as_array().unwrap();
        assert_eq!(list.len(), 4);
        assert_eq!(list[0]["preset"], "yt-1440p60-hevc");
        assert!(v["note"].as_str().unwrap().contains("Plenty"));
        let slow = recommendations(4.0, false);
        assert!(slow["note"].as_str().unwrap().contains("slow"));
    }

    /// Real network: `cargo test upload_speed_real -- --ignored --nocapture`
    #[test]
    #[ignore]
    fn upload_speed_real() {
        let r =
            upload_speed_test(10.0, &|f, m| println!("{:>3.0}%  {m:.1} Mbps", f * 100.0)).unwrap();
        println!("{r:?}");
        assert!(r.min > 0.0 && r.min <= r.median && r.median <= r.max);
    }

    /// Real download (~115 MB) into a temp folder, not the real install:
    /// `cargo test install_ffmpeg_real -- --ignored --nocapture`
    #[test]
    #[ignore]
    fn install_ffmpeg_real() {
        let dir = std::env::temp_dir().join("phonecast-ffmpeg-install-test");
        std::fs::remove_dir_all(&dir).ok();
        let st = DownloadState::default();
        let exe = install_ffmpeg_to(&dir, &st).unwrap();
        println!("{} {:?}", exe.display(), st.json());
        assert!(exe.exists() && dir.join("ffprobe.exe").exists());
        let v = run(exe.to_str().unwrap(), &["-hide_banner", "-version"]).unwrap();
        println!("{}", v.lines().next().unwrap_or(""));
        let enc = run(exe.to_str().unwrap(), &["-hide_banner", "-encoders"]).unwrap();
        assert!(enc.contains("h264_nvenc") && enc.contains("hevc_nvenc"));
        let protos = run(exe.to_str().unwrap(), &["-hide_banner", "-protocols"]).unwrap();
        println!("srt protocol: {}", protos.contains("srt"));
        let muxers = run(exe.to_str().unwrap(), &["-hide_banner", "-h", "muxer=fifo"]).unwrap();
        assert!(muxers.contains("attempt_recovery"));
        std::fs::remove_dir_all(&dir).ok();
    }
}
