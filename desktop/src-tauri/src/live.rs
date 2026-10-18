//! Go live: YouTube and Twitch at the same time.
//!
//! Each destination gets its own FFmpeg process fed with the phone stream
//! (as live MKV on stdin). FFmpeg decodes on the GPU, fits the picture to a
//! 16:9 canvas at a steady 60 fps, encodes with the GPU encoder (NVENC on
//! NVIDIA) at that platform's best settings, and sends it over RTMPS.
//! Destinations are independent: if one drops, it reconnects on its own and
//! the other keeps going.

use crate::capture::PcVideo;
use crate::mkv::{MkvWriter, VideoConfig};
use crate::sinks::{Media, Push, SinkTx};
use serde::{Deserialize, Serialize};
use serde_json::{json, Value};
use std::io::{BufRead, BufReader, BufWriter};
use std::path::PathBuf;
use std::process::{Child, Command, Stdio};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::mpsc::{Receiver, RecvTimeoutError};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

#[cfg(windows)]
use std::os::windows::process::CommandExt;

// ---------------- settings ----------------

#[derive(Serialize, Deserialize, Clone, Default)]
pub struct DestSettings {
    pub enabled: bool,
    pub quality: String,
    /// Stream key, encrypted with Windows DPAPI (hex). Never stored in plain text.
    #[serde(default)]
    key_enc: String,
}

#[derive(Serialize, Deserialize, Clone)]
pub struct LiveSettings {
    pub youtube: DestSettings,
    pub twitch: DestSettings,
    /// "blur" = whole picture over a blurred copy of itself (what mobile
    /// streamers use), "fit" = whole picture with black bars,
    /// "fill" = crop the sides to fill 16:9.
    pub framing: String,
}

impl Default for LiveSettings {
    fn default() -> Self {
        Self {
            youtube: DestSettings {
                enabled: false,
                quality: "1440p60".into(),
                key_enc: String::new(),
            },
            twitch: DestSettings {
                enabled: false,
                quality: "1080p60".into(),
                key_enc: String::new(),
            },
            framing: "blur".into(),
        }
    }
}

fn settings_path() -> PathBuf {
    let base = std::env::var("APPDATA")
        .map(PathBuf::from)
        .unwrap_or_else(|_| PathBuf::from("."));
    base.join("PhoneCast").join("live.json")
}

impl LiveSettings {
    pub fn load() -> Self {
        std::fs::read(settings_path())
            .ok()
            .and_then(|b| serde_json::from_slice(&b).ok())
            .unwrap_or_default()
    }

    fn save(&self) -> std::io::Result<()> {
        let p = settings_path();
        if let Some(d) = p.parent() {
            std::fs::create_dir_all(d)?;
        }
        std::fs::write(p, serde_json::to_vec_pretty(self)?)
    }
}

struct Preset {
    id: &'static str,
    label: &'static str,
    w: u32,
    h: u32,
    kbps: u32,
    hevc: bool,
}

// YouTube: 1440p is the sweet spot for phone games. YouTube re-encodes
// 1440p+ uploads with its higher-quality VP9 codec, so viewers at 1080p get a
// visibly sharper picture than from a 1080p upload. 4K is not offered: a phone
// picture is only ~1080 px tall (4K just upscales it), it needs 35+ Mbps of
// steady upload, and YouTube rejected 4K HEVC from us in a real test.
// The first entry is the fallback for unknown/old saved choices.
const YOUTUBE: &[Preset] = &[
    Preset {
        id: "1440p60",
        label: "1440p60 · 20 Mbps (best quality, recommended)",
        w: 2560,
        h: 1440,
        kbps: 20_000,
        hevc: false,
    },
    Preset {
        id: "1080p60",
        label: "1080p60 · 13.5 Mbps (for slower upload)",
        w: 1920,
        h: 1080,
        kbps: 13_500,
        hevc: false,
    },
];

const TWITCH: &[Preset] = &[
    Preset {
        id: "1080p60",
        label: "1080p60 · 6 Mbps (recommended)",
        w: 1920,
        h: 1080,
        kbps: 6_000,
        hevc: false,
    },
    Preset {
        id: "936p60",
        label: "936p60 · 6 Mbps (sharper on slow upload)",
        w: 1664,
        h: 936,
        kbps: 6_000,
        hevc: false,
    },
    Preset {
        id: "720p60",
        label: "720p60 · 4.5 Mbps",
        w: 1280,
        h: 720,
        kbps: 4_500,
        hevc: false,
    },
];

fn preset(list: &'static [Preset], id: &str) -> &'static Preset {
    list.iter().find(|p| p.id == id).unwrap_or(&list[0])
}

// ---------------- encoders ----------------

#[derive(Clone)]
struct Encoders {
    h264: String,
    hevc: String,
}

pub fn find_ffmpeg() -> Option<PathBuf> {
    let mut c = Vec::new();
    if let Ok(exe) = std::env::current_exe() {
        if let Some(d) = exe.parent() {
            c.push(d.join("ffmpeg.exe"));
        }
    }
    if let Ok(local) = std::env::var("LOCALAPPDATA") {
        c.push(PathBuf::from(local).join("Microsoft\\WinGet\\Links\\ffmpeg.exe"));
    }
    if let Some(p) = c.into_iter().find(|p| p.exists()) {
        return Some(p);
    }
    let mut cmd = Command::new("where");
    cmd.arg("ffmpeg");
    hide(&mut cmd);
    let out = cmd.output().ok()?;
    String::from_utf8_lossy(&out.stdout)
        .lines()
        .next()
        .map(|l| PathBuf::from(l.trim()))
        .filter(|p| p.exists())
}

fn hide(cmd: &mut Command) {
    #[cfg(windows)]
    cmd.creation_flags(0x0800_0000); // CREATE_NO_WINDOW
}

/// Picks the best encoder that actually works on this PC (GPU first).
fn probe_encoders(ffmpeg: &PathBuf) -> Encoders {
    let works = |enc: &str| {
        let mut cmd = Command::new(ffmpeg);
        cmd.args([
            "-hide_banner",
            "-loglevel",
            "error",
            "-f",
            "lavfi",
            "-i",
            "color=black:s=640x360:r=30",
            "-frames:v",
            "3",
            "-pix_fmt",
            "yuv420p",
            "-c:v",
            enc,
            "-f",
            "null",
            "-",
        ]);
        cmd.stdin(Stdio::null())
            .stdout(Stdio::null())
            .stderr(Stdio::null());
        hide(&mut cmd);
        cmd.status().map(|s| s.success()).unwrap_or(false)
    };
    let pick = |list: &[&str]| list.iter().find(|e| works(e)).map(|s| s.to_string());
    Encoders {
        h264: pick(&["h264_nvenc", "h264_amf", "h264_qsv"]).unwrap_or_else(|| "libx264".into()),
        hevc: pick(&["hevc_nvenc", "hevc_amf", "hevc_qsv"]).unwrap_or_else(|| "libx265".into()),
    }
}

fn encoder_label(e: &str) -> &'static str {
    match e {
        x if x.contains("nvenc") => "NVIDIA NVENC (GPU)",
        x if x.contains("amf") => "AMD AMF (GPU)",
        x if x.contains("qsv") => "Intel Quick Sync (GPU)",
        _ => "CPU (x264)",
    }
}

fn encoder_args(enc: &str, kbps: u32, hevc: bool, uhd: bool) -> Vec<String> {
    let b = format!("{kbps}k");
    let buf = format!("{}k", kbps * 2);
    let mut a: Vec<String> = vec!["-c:v".into(), enc.into()];
    let more: &[&str] = if enc.contains("nvenc") {
        // Highest-quality NVENC settings that still run in real time on RTX 40
        // (p5; p4 for 4K so it keeps up alongside a second stream).
        if uhd {
            &[
                "-preset",
                "p4",
                "-tune",
                "hq",
                "-rc",
                "cbr",
                "-spatial-aq",
                "1",
                "-temporal-aq",
                "1",
                "-rc-lookahead",
                "20",
                "-bf",
                "2",
            ]
        } else {
            &[
                "-preset",
                "p5",
                "-tune",
                "hq",
                "-rc",
                "cbr",
                "-spatial-aq",
                "1",
                "-temporal-aq",
                "1",
                "-rc-lookahead",
                "20",
                "-bf",
                "2",
            ]
        }
    } else if enc.contains("amf") {
        &["-usage", "transcoding", "-quality", "quality", "-rc", "cbr"]
    } else if enc.contains("qsv") {
        &["-preset", "medium"]
    } else {
        &["-preset", "veryfast", "-x264-params", "nal-hrd=cbr"]
    };
    a.extend(more.iter().map(|s| s.to_string()));
    if !hevc && !enc.contains("x265") {
        a.extend(["-profile:v".into(), "high".into()]);
    }
    a.extend([
        "-b:v".into(),
        b.clone(),
        "-maxrate".into(),
        b,
        "-bufsize".into(),
        buf,
        // 2-second keyframes: what YouTube and Twitch require.
        "-g".into(),
        "120".into(),
        "-keyint_min".into(),
        "120".into(),
    ]);
    a
}

// ---------------- destination status ----------------

#[derive(Clone, Serialize)]
pub struct DestStatus {
    pub id: &'static str,
    pub name: &'static str,
    pub state: String,
    pub fps: f64,
    pub kbps: f64,
    pub dropped: u64,
    pub speed: f64,
    pub error: Option<String>,
    #[serde(skip)]
    since: Option<Instant>,
    pub seconds: u64,
    pub reconnects: u32,
    pub quality: String,
}

pub struct Dest {
    pub tx: SinkTx,
    pub status: Arc<Mutex<DestStatus>>,
    pub stop: Arc<AtomicBool>,
}

pub struct Live {
    settings: Mutex<LiveSettings>,
    dests: Mutex<Vec<Dest>>,
    ffmpeg: Option<PathBuf>,
    encoders: Arc<Mutex<Option<Encoders>>>,
    started: Mutex<Option<Instant>>,
}

impl Live {
    pub fn new() -> Arc<Live> {
        let ffmpeg = find_ffmpeg();
        let live = Arc::new(Live {
            settings: Mutex::new(LiveSettings::load()),
            dests: Mutex::new(Vec::new()),
            ffmpeg: ffmpeg.clone(),
            encoders: Arc::new(Mutex::new(None)),
            started: Mutex::new(None),
        });
        if let Some(ff) = ffmpeg {
            let enc = live.encoders.clone();
            std::thread::spawn(move || *enc.lock().unwrap() = Some(probe_encoders(&ff)));
        }
        live
    }

    pub fn settings_json(&self) -> Value {
        let s = self.settings.lock().unwrap();
        let enc = self.encoders.lock().unwrap().clone();
        let d = |d: &DestSettings, list: &'static [Preset]| {
            json!({
                "enabled": d.enabled,
                "quality": preset(list, &d.quality).id,
                "hasKey": !d.key_enc.is_empty(),
                "presets": list.iter().map(|p| json!({"id": p.id, "label": p.label, "kbps": p.kbps})).collect::<Vec<_>>(),
            })
        };
        json!({
            "youtube": d(&s.youtube, YOUTUBE),
            "twitch": d(&s.twitch, TWITCH),
            "framing": s.framing,
            "ffmpeg": self.ffmpeg.is_some(),
            "encoder": enc.as_ref().map(|e| encoder_label(&e.h264)),
        })
    }

    /// Updates settings. Keys are only replaced when a new one is typed;
    /// an empty string clears it.
    pub fn update_settings(&self, v: &Value) -> Result<(), String> {
        let mut guard = self.settings.lock().unwrap();
        let s = &mut *guard;
        for (name, d) in [("youtube", &mut s.youtube), ("twitch", &mut s.twitch)] {
            let Some(o) = v.get(name) else { continue };
            if let Some(e) = o.get("enabled").and_then(Value::as_bool) {
                d.enabled = e;
            }
            if let Some(q) = o.get("quality").and_then(Value::as_str) {
                d.quality = q.into();
            }
            if let Some(k) = o.get("key").and_then(Value::as_str) {
                let k = k.trim();
                d.key_enc = if k.is_empty() {
                    String::new()
                } else {
                    dpapi::protect(k)?
                };
            }
        }
        if let Some(f) = v.get("framing").and_then(Value::as_str) {
            s.framing = match Framing::parse(f) {
                Framing::Blur => "blur",
                Framing::Fit => "fit",
                Framing::Fill => "fill",
            }
            .into();
        }
        s.save().map_err(|e| e.to_string())
    }

    pub fn is_live(&self) -> bool {
        !self.dests.lock().unwrap().is_empty()
    }

    pub fn ffmpeg(&self) -> Option<PathBuf> {
        self.ffmpeg.clone()
    }

    /// Encoder for the PC-mode recorder (file, visually lossless).
    pub fn encoders_for_file(&self) -> Option<(String, PathBuf)> {
        let ff = self.ffmpeg.clone()?;
        let enc = self
            .encoders
            .lock()
            .unwrap()
            .clone()
            .unwrap_or_else(|| probe_encoders(&ff));
        Some((enc.h264, ff))
    }

    /// `pc`: Some for PC-screen mode (FFmpeg captures the screen itself and
    /// only audio is piped in); None for phone mode (video + audio piped).
    pub fn start(
        &self,
        config: Option<VideoConfig>,
        pc: Option<Arc<PcVideo>>,
        request_key: Arc<dyn Fn() + Send + Sync>,
    ) -> Result<(), String> {
        if self.is_live() {
            return Err("Already live".into());
        }
        let ffmpeg = self
            .ffmpeg
            .clone()
            .ok_or("FFmpeg is not installed on this PC")?;
        let config = match (&pc, config) {
            (Some(p), _) => VideoConfig {
                codec: crate::mkv::CODEC_H264,
                width: p.screen_w as u16,
                height: p.screen_h as u16,
                csd: Vec::new(),
            },
            (None, Some(c)) => c,
            (None, None) => return Err("Start streaming on the phone first".into()),
        };
        let enc = self
            .encoders
            .lock()
            .unwrap()
            .clone()
            .unwrap_or_else(|| probe_encoders(&ffmpeg));
        let s = self.settings.lock().unwrap().clone();

        let mut plans = Vec::new();
        for (id, name, d, list, url) in [
            (
                "youtube",
                "YouTube",
                &s.youtube,
                YOUTUBE,
                "rtmps://a.rtmps.youtube.com:443/live2/",
            ),
            (
                "twitch",
                "Twitch",
                &s.twitch,
                TWITCH,
                "rtmps://live.twitch.tv:443/app/",
            ),
        ] {
            if !d.enabled {
                continue;
            }
            if d.key_enc.is_empty() {
                return Err(format!("Add your {name} stream key first"));
            }
            let key = dpapi::unprotect(&d.key_enc)?;
            plans.push((id, name, preset(list, &d.quality), format!("{url}{key}")));
        }
        if plans.is_empty() {
            return Err("Turn on YouTube or Twitch first".into());
        }

        let mut dests = self.dests.lock().unwrap();
        for (id, name, p, url) in plans {
            let status = Arc::new(Mutex::new(DestStatus {
                id,
                name,
                state: "connecting".into(),
                fps: 0.0,
                kbps: 0.0,
                dropped: 0,
                speed: 0.0,
                error: None,
                since: None,
                seconds: 0,
                reconnects: 0,
                quality: p.id.into(),
            }));
            let stop = Arc::new(AtomicBool::new(false));
            // ~15 s of buffer (video + 10 ms audio packets): rides out upload and
            // CPU hiccups instead of skipping ahead; adds no delay when all is well.
            let (tx, rx) = SinkTx::new(2400);
            let job = Job {
                ffmpeg: ffmpeg.clone(),
                url,
                encoder: if p.hevc {
                    enc.hevc.clone()
                } else {
                    enc.h264.clone()
                },
                w: p.w,
                h: p.h,
                kbps: p.kbps,
                hevc: p.hevc,
                framing: Framing::parse(&s.framing),
                // Composited sources get one frame pipe per FFmpeg process.
                pc: pc.as_ref().map(|p| Arc::new(p.for_job())),
                file: false,
            };
            let (st, sp, rk, cfg) = (
                status.clone(),
                stop.clone(),
                request_key.clone(),
                config.clone(),
            );
            std::thread::Builder::new()
                .name(format!("live-{id}"))
                .spawn(move || job.run(rx, cfg, rk, st, sp))
                .map_err(|e| e.to_string())?;
            dests.push(Dest { tx, status, stop });
        }
        *self.started.lock().unwrap() = Some(Instant::now());
        Ok(())
    }

    pub fn stop(&self) {
        let mut dests = self.dests.lock().unwrap();
        for d in dests.iter() {
            d.stop.store(true, Ordering::SeqCst);
        }
        dests.clear(); // dropping the senders ends each stream cleanly
        *self.started.lock().unwrap() = None;
    }

    /// Returns true if a destination fell behind and needs a keyframe.
    pub fn push(&self, m: &Media) -> bool {
        let mut want_key = false;
        for d in self.dests.lock().unwrap().iter_mut() {
            if let Push::NeedKeyframe = d.tx.push(m) {
                want_key = true;
            }
        }
        want_key
    }

    pub fn status_json(&self) -> Value {
        let dests: Vec<DestStatus> = self
            .dests
            .lock()
            .unwrap()
            .iter()
            .map(|d| {
                let mut s = d.status.lock().unwrap().clone();
                s.seconds = s.since.map(|t| t.elapsed().as_secs()).unwrap_or(0);
                s
            })
            .collect();
        json!({
            "active": !dests.is_empty(),
            "seconds": self.started.lock().unwrap().map(|t| t.elapsed().as_secs()).unwrap_or(0),
            "destinations": dests,
        })
    }
}

/// How a picture that isn't 16:9 (phones are ~2.2:1) is fitted into the
/// 16:9 stream. Never stretches.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum Framing {
    /// Whole picture, centred, over a blurred and slightly darkened copy of
    /// itself filling the rest of the frame. Nothing cut, no black bars.
    Blur,
    /// Whole picture with black bars.
    Fit,
    /// Fills the frame by cropping the sides (edge HUD may be cut).
    Fill,
}

impl Framing {
    pub fn parse(s: &str) -> Self {
        match s {
            "fit" => Framing::Fit,
            "fill" => Framing::Fill,
            _ => Framing::Blur,
        }
    }
}

// ---------------- one destination ----------------

pub struct Job {
    pub ffmpeg: PathBuf,
    /// RTMP(S) URL, or a file path when `file` is true.
    pub url: String,
    pub encoder: String,
    pub w: u32,
    pub h: u32,
    pub kbps: u32,
    pub hevc: bool,
    pub framing: Framing,
    /// PC screen source; None = phone stream piped in as MKV.
    pub pc: Option<Arc<PcVideo>>,
    /// Record to an MKV file (visually lossless) instead of streaming.
    pub file: bool,
}

/// Runs a job on its own thread fed by the returned sender. Used by Go live
/// and by the PC-mode recorder.
pub fn spawn_job(
    job: Job,
    config: VideoConfig,
    request_key: Arc<dyn Fn() + Send + Sync>,
    name: &'static str,
) -> Dest {
    let status = Arc::new(Mutex::new(DestStatus {
        id: name,
        name,
        state: "connecting".into(),
        fps: 0.0,
        kbps: 0.0,
        dropped: 0,
        speed: 0.0,
        error: None,
        since: None,
        seconds: 0,
        reconnects: 0,
        quality: format!("{}p", job.h),
    }));
    let stop = Arc::new(AtomicBool::new(false));
    let (tx, rx) = SinkTx::new(2400);
    let (st, sp) = (status.clone(), stop.clone());
    std::thread::Builder::new()
        .name(format!("job-{name}"))
        .spawn(move || job.run(rx, config, request_key, st, sp))
        .ok();
    Dest { tx, status, stop }
}

impl Job {
    fn args(&self) -> Vec<String> {
        if let Some(pc) = &self.pc {
            return self.pc_args(pc);
        }
        let (w, h) = (self.w, self.h);
        let nvidia = self.encoder.contains("nvenc");
        let (hwaccel, filters) = video_filter(nvidia, self.framing, w, h);
        let mut a: Vec<String> = [
            "-hide_banner",
            "-y",
            "-loglevel",
            "error",
            "-nostats",
            "-progress",
            "pipe:2",
            "-stats_period",
            "1",
        ]
        .iter()
        .chain(hwaccel.iter())
        .chain(
            [
                // No "+nobuffer": with it, FFmpeg decoded the first frames before
                // their reference pictures arrived ("Missing reference picture"),
                // dropping ~1 s and glitching every stream start.
                "-fflags",
                "+genpts",
                "-analyzeduration",
                "1000000",
                "-probesize",
                "2000000",
                "-f",
                "matroska",
                "-i",
                "pipe:0",
            ]
            .iter(),
        )
        .map(|s| s.to_string())
        .collect();
        a.extend([
            "-filter_complex".into(),
            filters,
            "-map".into(),
            "[v]".into(),
            "-map".into(),
            "0:a:0".into(),
        ]);
        a.extend(encoder_args(&self.encoder, self.kbps, self.hevc, w >= 3840));
        a.extend(self.output_args());
        a
    }

    /// PC screen mode: FFmpeg captures the monitor (and webcam) itself;
    /// the mixed audio arrives as raw PCM on stdin.
    fn pc_args(&self, pc: &PcVideo) -> Vec<String> {
        let mut a: Vec<String> = [
            "-hide_banner",
            "-y",
            "-loglevel",
            "error",
            "-nostats",
            "-progress",
            "pipe:2",
            "-stats_period",
            "1",
        ]
        .iter()
        .map(|s| s.to_string())
        .collect();
        a.extend(pc.inputs());
        let audio_idx = pc.audio_index();
        a.extend(
            ["-f", "s16le", "-ar", "48000", "-ac", "2", "-i", "pipe:0"]
                .iter()
                .map(|s| s.to_string()),
        );
        a.extend(pc.filter(self.w, self.h, self.encoder.contains("nvenc")));
        a.extend(["-map".into(), format!("{audio_idx}:a")]);
        a.extend(pc.color_args());
        if self.file {
            // Visually lossless recording: quality-based rate control.
            a.extend(["-c:v".into(), self.encoder.clone()]);
            if self.encoder.contains("nvenc") {
                a.extend(
                    [
                        "-preset", "p5", "-tune", "hq", "-rc", "vbr", "-cq", "18", "-b:v", "0",
                        "-maxrate", "80M", "-bufsize", "160M", "-bf", "0", "-g", "120",
                    ]
                    .iter()
                    .map(|s| s.to_string()),
                );
            } else {
                a.extend(
                    [
                        "-b:v", "40M", "-maxrate", "40M", "-bufsize", "80M", "-g", "120",
                    ]
                    .iter()
                    .map(|s| s.to_string()),
                );
            }
        } else {
            a.extend(encoder_args(
                &self.encoder,
                self.kbps,
                self.hevc,
                self.w >= 3840,
            ));
        }
        a.extend(self.output_args());
        a
    }

    fn output_args(&self) -> Vec<String> {
        let mut a: Vec<String> = Vec::new();
        if self.file {
            a.extend(
                ["-c:a", "pcm_s16le", "-f", "matroska"]
                    .iter()
                    .map(|s| s.to_string()),
            );
        } else {
            a.extend(
                [
                    "-c:a",
                    "aac",
                    "-b:a",
                    "160k",
                    "-ar",
                    "48000",
                    "-ac",
                    "2",
                    "-f",
                    "flv",
                    "-flvflags",
                    "no_duration_filesize",
                ]
                .iter()
                .map(|s| s.to_string()),
            );
        }
        a.push(self.url.clone());
        a
    }

    fn spawn(&self) -> std::io::Result<Child> {
        let mut cmd = Command::new(&self.ffmpeg);
        cmd.args(self.args())
            .stdin(Stdio::piped())
            .stdout(Stdio::null())
            .stderr(Stdio::piped());
        // No console window, and ABOVE_NORMAL priority: in a real stream, other
        // heavy programs on the PC starved FFmpeg and the stream skipped ahead
        // every 10–20 s. The game runs on the phone, so this costs nothing.
        #[cfg(windows)]
        cmd.creation_flags(0x0800_0000 | 0x0000_8000);
        cmd.spawn()
    }

    fn run(
        self,
        rx: Receiver<Media>,
        mut config: VideoConfig,
        request_key: Arc<dyn Fn() + Send + Sync>,
        status: Arc<Mutex<DestStatus>>,
        stop: Arc<AtomicBool>,
    ) {
        let set = |f: &dyn Fn(&mut DestStatus)| f(&mut status.lock().unwrap());
        let name = status.lock().unwrap().name;
        crate::diag::log(
            "live",
            format!(
                "{name}: starting {}x{} {} kbps via {}",
                self.w, self.h, self.kbps, self.encoder
            ),
        );
        let mut attempt = 0u32;
        'outer: while !stop.load(Ordering::SeqCst) {
            let mut child = match self.spawn() {
                Ok(c) => c,
                Err(e) => {
                    set(&|s| {
                        s.state = "error".into();
                        s.error = Some(format!("Could not start FFmpeg: {e}"));
                    });
                    return;
                }
            };
            set(&|s| {
                s.state = if attempt == 0 {
                    "connecting".into()
                } else {
                    "reconnecting".into()
                };
            });
            watch_progress(child.stderr.take().unwrap(), status.clone());
            let stdin = BufWriter::with_capacity(1 << 20, child.stdin.take().unwrap());
            let mut writer = if self.pc.is_some() {
                Some(Feed::Pcm(stdin))
            } else {
                match MkvWriter::new(stdin, &config, Some((crate::audio::RATE as u32, 2)), true) {
                    Ok(w) => Some(Feed::Mkv(w)),
                    Err(_) => None,
                }
            };
            request_key();
            let mut restart_for_config = false;
            let mut sender_gone = false;

            while let Some(w) = writer.as_mut() {
                if stop.load(Ordering::SeqCst) {
                    break;
                }
                let m = match rx.recv_timeout(Duration::from_millis(500)) {
                    Ok(m) => m,
                    Err(RecvTimeoutError::Timeout) => {
                        if child.try_wait().ok().flatten().is_some() {
                            break;
                        }
                        continue;
                    }
                    Err(RecvTimeoutError::Disconnected) => {
                        sender_gone = true; // Live::stop() was called
                        break;
                    }
                };
                let ok = match (&m, w) {
                    (Media::Config(c), Feed::Mkv(_)) => {
                        if *c != config {
                            // Rotation: the stream must restart at the new size.
                            config = c.clone();
                            restart_for_config = true;
                        }
                        true
                    }
                    (Media::Video { pts, key, data }, Feed::Mkv(w)) => {
                        w.write_video(*pts, *key, data).is_ok()
                    }
                    (Media::Audio { pts, pcm }, Feed::Mkv(w)) => w.write_audio(*pts, pcm).is_ok(),
                    (Media::Audio { pcm, .. }, Feed::Pcm(out)) => {
                        use std::io::Write;
                        out.write_all(pcm).and_then(|_| out.flush()).is_ok()
                    }
                    (_, Feed::Pcm(_)) => true, // PC mode: FFmpeg captures video itself
                };
                if !ok || restart_for_config {
                    break;
                }
            }

            let graceful = sender_gone || stop.load(Ordering::SeqCst);
            if graceful {
                // Composited video arrives on a pipe that never ends by
                // itself; close it too so FFmpeg sees the end of both inputs
                // and finishes the file instead of being killed mid-write.
                if let Some(pc) = &self.pc {
                    pc.close_input();
                }
            }
            finish(&mut child, writer.take(), graceful);
            if graceful {
                break;
            }
            if restart_for_config {
                attempt += 1;
                continue;
            }
            // FFmpeg exited: network drop or platform rejected the stream.
            attempt += 1;
            let err = status.lock().unwrap().error.clone().unwrap_or_default();
            crate::diag::log(
                "live",
                format!("{name}: stream ended ({err}) - reconnect attempt {attempt}"),
            );
            set(&|s| {
                s.state = "reconnecting".into();
                s.reconnects += 1;
                s.since = None;
            });
            let wait = Duration::from_secs((2u64 << attempt.min(4)).min(20));
            let until = Instant::now() + wait;
            while Instant::now() < until {
                if stop.load(Ordering::SeqCst) {
                    break 'outer;
                }
                // Keep the queue empty while waiting so we restart with fresh video.
                while rx.try_recv().is_ok() {}
                std::thread::sleep(Duration::from_millis(100));
            }
        }
        set(&|s| {
            s.state = "stopped".into();
        });
        crate::diag::log("live", format!("{name}: stopped"));
    }
}

/// What goes into FFmpeg's stdin for a job.
enum Feed {
    Mkv(MkvWriter<BufWriter<std::process::ChildStdin>>),
    Pcm(BufWriter<std::process::ChildStdin>),
}

/// Ends FFmpeg: closing stdin lets it flush and disconnect cleanly.
fn finish(child: &mut Child, w: Option<Feed>, graceful: bool) {
    match w {
        Some(Feed::Mkv(w)) => {
            w.end().ok();
        }
        Some(Feed::Pcm(mut p)) => {
            use std::io::Write;
            p.flush().ok();
            drop(p);
        }
        None => {}
    }
    let deadline = Instant::now() + Duration::from_secs(if graceful { 5 } else { 1 });
    while Instant::now() < deadline {
        if child.try_wait().ok().flatten().is_some() {
            return;
        }
        std::thread::sleep(Duration::from_millis(50));
    }
    child.kill().ok();
    child.wait().ok();
}

/// Parses FFmpeg's `-progress` output into live stats; other lines are errors.
fn watch_progress(stderr: std::process::ChildStderr, status: Arc<Mutex<DestStatus>>) {
    std::thread::spawn(move || {
        let mut last_frame = 0u64;
        for line in BufReader::new(stderr).lines().map_while(Result::ok) {
            let line = line.trim();
            let Some((k, v)) = line.split_once('=') else {
                if !line.is_empty() {
                    let mut s = status.lock().unwrap();
                    crate::diag::log("live", format!("{}: ffmpeg: {line}", s.name));
                    s.error = Some(friendly_error(line));
                }
                continue;
            };
            let mut s = status.lock().unwrap();
            match k {
                "frame" => {
                    let f: u64 = v.parse().unwrap_or(0);
                    if f > last_frame && s.state != "live" {
                        s.state = "live".into();
                        s.error = None;
                        s.since.get_or_insert_with(Instant::now);
                        crate::diag::log("live", format!("{}: on air", s.name));
                    }
                    last_frame = f;
                }
                "fps" => s.fps = v.parse().unwrap_or(0.0),
                "bitrate" => s.kbps = v.trim_end_matches("kbits/s").trim().parse().unwrap_or(0.0),
                "drop_frames" => s.dropped = v.parse().unwrap_or(0),
                "speed" => s.speed = v.trim_end_matches('x').trim().parse().unwrap_or(0.0),
                _ => {}
            }
        }
    });
}

/// FFmpeg hwaccel flags and a filter graph turning the phone picture into a
/// steady 60 fps `w`×`h` frame labelled `[v]`, never stretched.
///
/// On NVIDIA everything stays on the GPU (NVDEC → CUDA → NVENC). The blurred
/// background is made from a tiny 384×216 copy: download, blur and upload
/// of a frame that small costs well under 1 ms. Measured on an RTX 4050 with a
/// real 2376×1080 phone recording: 1440p blur at 2.2× real time.
fn video_filter(
    nvidia: bool,
    framing: Framing,
    w: u32,
    h: u32,
) -> (&'static [&'static str], String) {
    let crop16x9 = "crop=w='min(iw,ih*16/9)':h='min(ih,iw*9/16)'";
    // Blurred, slightly darkened and more saturated so the real picture stands out.
    let soften = "boxblur=luma_radius=12:luma_power=2:chroma_radius=6:chroma_power=2,eq=brightness=-0.06:saturation=1.15";
    if nvidia {
        let fit = format!(
            "scale_cuda=w={w}:h={h}:force_original_aspect_ratio=decrease:force_divisible_by=2:interp_algo=lanczos:format=nv12"
        );
        match framing {
            Framing::Blur => (
                &["-hwaccel", "cuda", "-hwaccel_output_format", "cuda"],
                format!(
                    "[0:v:0]split=2[a][b];[a]{fit}[fg];\
                     [b]scale_cuda=w=384:h=216:force_original_aspect_ratio=increase:force_divisible_by=2:format=nv12,\
                     hwdownload,format=nv12,format=yuv420p,crop=384:216,{soften},format=nv12,hwupload_cuda,\
                     scale_cuda=w={w}:h={h}:interp_algo=bilinear:format=nv12[bg];\
                     [bg][fg]overlay_cuda=x=(main_w-overlay_w)/2:y=(main_h-overlay_h)/2,fps=60[v]"
                ),
            ),
            Framing::Fit => (
                &["-hwaccel", "cuda", "-hwaccel_output_format", "cuda"],
                format!("[0:v:0]{fit},pad_cuda={w}:{h}:(ow-iw)/2:(oh-ih)/2,fps=60[v]"),
            ),
            Framing::Fill => (
                &["-hwaccel", "cuda"],
                format!("[0:v:0]{crop16x9},hwupload_cuda,scale_cuda={w}:{h}:interp_algo=lanczos:format=nv12,fps=60[v]"),
            ),
        }
    } else {
        let fit = format!(
            "scale={w}:{h}:force_original_aspect_ratio=decrease:force_divisible_by=2:flags=bicubic"
        );
        let body = match framing {
            Framing::Blur => format!(
                "[0:v:0]split=2[a][b];[a]{fit}[fg];\
                 [b]scale=384:216:force_original_aspect_ratio=increase,crop=384:216,{soften},scale={w}:{h}:flags=bilinear[bg];\
                 [bg][fg]overlay=x=(main_w-overlay_w)/2:y=(main_h-overlay_h)/2"
            ),
            Framing::Fit => format!("[0:v:0]{fit},pad={w}:{h}:(ow-iw)/2:(oh-ih)/2:black"),
            Framing::Fill => format!("[0:v:0]{crop16x9},scale={w}:{h}:flags=bicubic"),
        };
        (
            &["-hwaccel", "auto"],
            format!("{body},fps=60,format=yuv420p,setsar=1[v]"),
        )
    }
}

fn friendly_error(line: &str) -> String {
    let l = line.to_lowercase();
    if l.contains("ddagrab") || (l.contains("operation not permitted") && l.contains("input")) {
        "Screen capture paused: the display is off or the PC is locked — it resumes by itself"
            .into()
    } else if l.contains(r"\.\pipe") || (l.contains("pipe:") && l.contains("no such file")) {
        "Capture stopped — pick the source again".into()
    } else if l.contains("dshow") || l.contains("could not find video device") {
        "Webcam not available — is another app using it?".into()
    } else if l.contains("i/o error")
        || l.contains("connection refused")
        || l.contains("error number -10053")
    {
        "Connection lost — check your internet".into()
    } else if l.contains("401")
        || l.contains("403")
        || l.contains("unauthorized")
        || l.contains("forbidden")
    {
        "The platform rejected the stream key — copy it again".into()
    } else if l.contains("no such host") || l.contains("failed to resolve") {
        "No internet connection".into()
    } else {
        line.chars().take(160).collect()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// End-to-end PC mode: FFmpeg captures the real screen, mixed PCM audio
    /// is piped in, GPU-encoded MKV comes out. Needs a display and FFmpeg.
    /// `cargo test pc_record_pipeline -- --ignored --nocapture`
    #[test]
    #[ignore]
    fn pc_record_pipeline() {
        let ffmpeg = find_ffmpeg().expect("ffmpeg");
        let enc = probe_encoders(&ffmpeg);
        let m = crate::capture::monitors();
        let mon = m.first().expect("a monitor");
        let out = std::env::temp_dir().join("phonecast_pc_test.mkv");
        std::fs::remove_file(&out).ok();
        let pc = Arc::new(PcVideo {
            input: crate::capture::PcInput::Ddagrab { output: mon.output },
            webcam: None,
            screen_w: mon.width,
            screen_h: mon.height,
        });
        let job = Job {
            ffmpeg,
            url: out.display().to_string(),
            encoder: enc.h264.clone(),
            w: mon.width,
            h: mon.height,
            kbps: 0,
            hevc: false,
            framing: Framing::Fit,
            pc: Some(pc),
            file: true,
        };
        let cfg = VideoConfig {
            codec: crate::mkv::CODEC_H264,
            width: mon.width as u16,
            height: mon.height as u16,
            csd: Vec::new(),
        };
        let dest = spawn_job(job, cfg, Arc::new(|| {}), "Test");
        let Dest {
            mut tx,
            status,
            stop,
        } = dest;
        // 4 s of 10 ms audio chunks (a 440 Hz tone) in real time.
        let start = Instant::now();
        for i in 0..400i64 {
            let pcm: Vec<u8> = (0..480)
                .flat_map(|k| {
                    let t = (i * 480 + k) as f32 / 48000.0;
                    let s = ((t * 440.0 * std::f32::consts::TAU).sin() * 8000.0) as i16;
                    [s.to_le_bytes(), s.to_le_bytes()].concat()
                })
                .collect();
            tx.push(&Media::Audio {
                pts: i * 10_000,
                pcm: Arc::new(pcm),
            });
            if let Some(w) = Duration::from_millis((i as u64 + 1) * 10).checked_sub(start.elapsed())
            {
                std::thread::sleep(w);
            }
        }
        let s = status.lock().unwrap().clone();
        println!("state={} fps={} err={:?}", s.state, s.fps, s.error);
        assert_eq!(s.state, "live");
        stop.store(true, Ordering::SeqCst);
        drop(tx);
        std::thread::sleep(Duration::from_secs(2));
        let size = std::fs::metadata(&out).map(|m| m.len()).unwrap_or(0);
        println!("wrote {} ({} KB)", out.display(), size / 1024);
        assert!(size > 200_000);
    }

    /// End to end, composed PC picture: Notepad (Windows Graphics Capture)
    /// with a PNG logo and text, through the compositor, into the recorder (MKV)
    /// and a 1440p60 live job (a local FLV file stands in for YouTube) at
    /// the same time. Checks both outputs with ffprobe: size, 60 fps, audio,
    /// no lost frames, video starting at ~0.
    /// `cargo test composed_record_and_live -- --ignored --nocapture`
    #[test]
    #[ignore]
    fn composed_record_and_live() {
        use crate::capture::{Overlay, OverlayKind, PcInput};
        use crate::compositor::{Base, Layers, Session};
        let ffmpeg = find_ffmpeg().expect("ffmpeg");
        let enc = probe_encoders(&ffmpeg);
        let mut np = Command::new("notepad.exe").spawn().expect("notepad");
        let mut hwnd = 0isize;
        for _ in 0..100 {
            std::thread::sleep(Duration::from_millis(100));
            if let Some(w) = crate::capture::windows()
                .into_iter()
                .find(|w| w.process.eq_ignore_ascii_case("notepad.exe"))
            {
                hwnd = w.hwnd;
                break;
            }
        }
        assert_ne!(hwnd, 0, "Notepad window not found");
        let logo = std::path::Path::new(env!("CARGO_MANIFEST_DIR")).join("icons/128x128.png");
        let layers = Layers {
            webcam: None,
            overlays: vec![
                Overlay {
                    id: "logo".into(),
                    kind: OverlayKind::Image {
                        path: logo.display().to_string(),
                    },
                    x: 0.85,
                    y: 0.04,
                    w: 0.1,
                    opacity: 1.0,
                },
                Overlay {
                    id: "txt".into(),
                    kind: OverlayKind::Text {
                        text: "PhoneCast test".into(),
                        size: 0.06,
                        color: "#ffcc33".into(),
                    },
                    x: 0.04,
                    y: 0.85,
                    w: 0.0,
                    opacity: 1.0,
                },
            ],
        };
        let (cw, ch) = (2560, 1440);
        let session =
            Session::start(Base::Window { hwnd }, (cw, ch), layers, None).expect("compositor");
        // One pipe per job, as in the app (an unread pipe only counts drops).
        let base = PcVideo {
            input: PcInput::Composited {
                session: session.clone(),
                pipe: session.new_pipe(),
            },
            webcam: None,
            screen_w: cw,
            screen_h: ch,
        };
        let second = base.for_job();
        let dir = std::env::temp_dir();
        let mkv = dir.join("phonecast_composed.mkv");
        let flv = dir.join("phonecast_composed.flv");
        std::fs::remove_file(&mkv).ok();
        std::fs::remove_file(&flv).ok();
        let cfg = VideoConfig {
            codec: crate::mkv::CODEC_H264,
            width: cw as u16,
            height: ch as u16,
            csd: Vec::new(),
        };
        let p = preset(YOUTUBE, "1440p60");
        let jobs = [
            Job {
                ffmpeg: ffmpeg.clone(),
                url: mkv.display().to_string(),
                encoder: enc.h264.clone(),
                w: cw,
                h: ch,
                kbps: 0,
                hevc: false,
                framing: Framing::Fit,
                pc: Some(Arc::new(base)),
                file: true,
            },
            Job {
                ffmpeg: ffmpeg.clone(),
                url: flv.display().to_string(),
                encoder: enc.h264.clone(),
                w: p.w,
                h: p.h,
                kbps: p.kbps,
                hevc: false,
                framing: Framing::Fit,
                pc: Some(Arc::new(second)),
                file: false,
            },
        ];
        let mut dests: Vec<Dest> = jobs
            .into_iter()
            .map(|j| spawn_job(j, cfg.clone(), Arc::new(|| {}), "Test"))
            .collect();
        // 8 s of 10 ms audio chunks (a 440 Hz tone) in real time.
        let start = Instant::now();
        for i in 0..800i64 {
            let pcm: Vec<u8> = (0..480)
                .flat_map(|k| {
                    let t = (i * 480 + k) as f32 / 48000.0;
                    let s = ((t * 440.0 * std::f32::consts::TAU).sin() * 8000.0) as i16;
                    [s.to_le_bytes(), s.to_le_bytes()].concat()
                })
                .collect();
            let m = Media::Audio {
                pts: i * 10_000,
                pcm: Arc::new(pcm),
            };
            for d in dests.iter_mut() {
                d.tx.push(&m);
            }
            if let Some(w) = Duration::from_millis((i as u64 + 1) * 10).checked_sub(start.elapsed())
            {
                std::thread::sleep(w);
            }
        }
        for d in &dests {
            let s = d.status.lock().unwrap().clone();
            println!("job state={} fps={} err={:?}", s.state, s.fps, s.error);
            assert_eq!(s.state, "live", "job not live: {:?}", s.error);
            d.stop.store(true, Ordering::SeqCst);
        }
        println!("compositor {}", session.status_json());
        drop(dests);
        std::thread::sleep(Duration::from_secs(3));
        np.kill().ok();
        np.wait().ok();

        let ffprobe = ffmpeg.with_file_name("ffprobe.exe");
        let probe = |file: &std::path::Path, args: &[&str]| -> String {
            let out = Command::new(&ffprobe)
                .args(["-v", "error", "-of", "csv=p=0"])
                .args(args)
                .arg(file)
                .output()
                .expect("ffprobe");
            String::from_utf8_lossy(&out.stdout).trim().to_string()
        };
        for (file, w, h) in [(&mkv, cw, ch), (&flv, p.w, p.h)] {
            let v = probe(
                file,
                &[
                    "-select_streams",
                    "v:0",
                    "-count_packets",
                    "-show_entries",
                    "stream=width,height,avg_frame_rate,nb_read_packets",
                ],
            );
            let a = probe(
                file,
                &[
                    "-select_streams",
                    "a:0",
                    "-show_entries",
                    "stream=codec_name",
                ],
            );
            let dur: f64 = probe(file, &["-show_entries", "format=duration"])
                .parse()
                .unwrap_or(0.0);
            let first = probe(
                file,
                &[
                    "-select_streams",
                    "v:0",
                    "-read_intervals",
                    "%+#1",
                    "-show_entries",
                    "packet=pts_time",
                ],
            );
            println!(
                "{}: video [{v}] audio [{a}] duration {dur:.2}s first video pts {first}",
                file.display()
            );
            let f: Vec<&str> = v.split(',').collect();
            assert_eq!(f[0].parse::<u32>().unwrap(), w, "width");
            assert_eq!(f[1].parse::<u32>().unwrap(), h, "height");
            assert_eq!(f[2], "60/1", "frame rate");
            let packets: f64 = f[3].parse().unwrap();
            assert!(!a.is_empty(), "no audio track");
            assert!(dur > 6.0, "too short: {dur}");
            let expect = dur * 60.0;
            assert!(
                packets >= expect * 0.97,
                "lost frames: {packets} of ~{expect:.0}"
            );
            let first: f64 = first.parse().unwrap_or(9.9);
            assert!(first < 0.2, "video starts late: {first}s");
        }
    }

    /// End-to-end: live MKV over stdin → FFmpeg → GPU encode → FLV file
    /// (a file stands in for YouTube). Run with
    /// `PHONECAST_SAMPLE=<file.h264> cargo test live_pipeline -- --ignored --nocapture`.
    #[test]
    #[ignore]
    fn live_pipeline() {
        let ffmpeg = find_ffmpeg().expect("ffmpeg");
        let enc = probe_encoders(&ffmpeg);
        println!("encoder: {}", enc.h264);
        let src = std::env::var("PHONECAST_SAMPLE").unwrap();
        let data = std::fs::read(&src).unwrap();
        let mut csd = Vec::new();
        let mut aus: Vec<(bool, Vec<u8>)> = Vec::new();
        for n in crate::mkv::split_nals(&data) {
            match n[0] & 0x1F {
                9 => aus.push((false, Vec::new())),
                7 | 8 if aus.len() <= 1 => {
                    csd.extend_from_slice(&[0, 0, 0, 1]);
                    csd.extend_from_slice(n);
                }
                7 | 8 => {}
                t => {
                    let au = aus.last_mut().unwrap();
                    au.0 |= t == 5;
                    au.1.extend_from_slice(&[0, 0, 0, 1]);
                    au.1.extend_from_slice(n);
                }
            }
        }
        let out = std::env::temp_dir().join("phonecast_live_test.flv");
        std::fs::remove_file(&out).ok();
        for framing in [Framing::Blur, Framing::Fit, Framing::Fill] {
            let job = Job {
                ffmpeg: ffmpeg.clone(),
                url: out.display().to_string(),
                encoder: enc.h264.clone(),
                w: 2560,
                h: 1440,
                kbps: 24_000,
                hevc: false,
                framing,
                pc: None,
                file: false,
            };
            let cfg = VideoConfig {
                codec: crate::mkv::CODEC_H264,
                width: 1280,
                height: 720,
                csd: csd.clone(),
            };
            let status = Arc::new(Mutex::new(DestStatus {
                id: "test",
                name: "Test",
                state: "connecting".into(),
                fps: 0.0,
                kbps: 0.0,
                dropped: 0,
                speed: 0.0,
                error: None,
                since: None,
                seconds: 0,
                reconnects: 0,
                quality: "1440p60".into(),
            }));
            let stop = Arc::new(AtomicBool::new(false));
            let (mut tx, rx) = SinkTx::new(800);
            let (st, sp) = (status.clone(), stop.clone());
            let t = std::thread::spawn(move || job.run(rx, cfg, Arc::new(|| {}), st, sp));
            // Feed 4 s in real time: 60 fps video + 10 ms audio chunks.
            let start = Instant::now();
            let mut audio_t = 0i64;
            for (i, (key, au)) in aus.iter().filter(|a| !a.1.is_empty()).enumerate() {
                let pts = 1_000_000 + i as i64 * 16_667;
                tx.push(&Media::Video {
                    pts,
                    key: *key,
                    data: Arc::new(au.clone()),
                });
                while audio_t < pts + 16_667 {
                    tx.push(&Media::Audio {
                        pts: audio_t.max(1_000_000),
                        pcm: Arc::new(vec![0u8; 1920]),
                    });
                    audio_t += 10_000;
                }
                let due = Duration::from_micros((i as u64 + 1) * 16_667);
                if let Some(w) = due.checked_sub(start.elapsed()) {
                    std::thread::sleep(w);
                }
            }
            let s = status.lock().unwrap().clone();
            println!(
                "{framing:?} status: {} fps={} kbps={} err={:?}",
                s.state, s.fps, s.kbps, s.error
            );
            assert_eq!(s.state, "live");
            drop(tx); // like Live::stop()
            t.join().unwrap();
            let size = std::fs::metadata(&out).map(|m| m.len()).unwrap_or(0);
            println!("{framing:?} wrote {} ({} KB)", out.display(), size / 1024);
            assert!(size > 100_000);
        }
    }
}

// ---------------- DPAPI (stream key encryption) ----------------

#[cfg(windows)]
mod dpapi {
    use windows_sys::Win32::Foundation::LocalFree;
    use windows_sys::Win32::Security::Cryptography::{
        CryptProtectData, CryptUnprotectData, CRYPT_INTEGER_BLOB,
    };

    fn call(data: &[u8], protect: bool) -> Result<Vec<u8>, String> {
        let input = CRYPT_INTEGER_BLOB {
            cbData: data.len() as u32,
            pbData: data.as_ptr() as *mut u8,
        };
        let mut out = CRYPT_INTEGER_BLOB {
            cbData: 0,
            pbData: std::ptr::null_mut(),
        };
        let ok = unsafe {
            if protect {
                CryptProtectData(
                    &input,
                    std::ptr::null(),
                    std::ptr::null(),
                    std::ptr::null(),
                    std::ptr::null(),
                    0,
                    &mut out,
                )
            } else {
                CryptUnprotectData(
                    &input,
                    std::ptr::null_mut(),
                    std::ptr::null(),
                    std::ptr::null(),
                    std::ptr::null(),
                    0,
                    &mut out,
                )
            }
        };
        if ok == 0 {
            return Err("Could not unlock the saved stream key — please paste it again".into());
        }
        let v = unsafe { std::slice::from_raw_parts(out.pbData, out.cbData as usize).to_vec() };
        unsafe { LocalFree(out.pbData as _) };
        Ok(v)
    }

    pub fn protect(s: &str) -> Result<String, String> {
        Ok(call(s.as_bytes(), true)?
            .iter()
            .map(|b| format!("{b:02x}"))
            .collect())
    }

    pub fn unprotect(hex: &str) -> Result<String, String> {
        let bytes: Vec<u8> = (0..hex.len())
            .step_by(2)
            .filter_map(|i| u8::from_str_radix(hex.get(i..i + 2)?, 16).ok())
            .collect();
        String::from_utf8(call(&bytes, false)?).map_err(|e| e.to_string())
    }
}

#[cfg(not(windows))]
mod dpapi {
    pub fn protect(s: &str) -> Result<String, String> {
        Ok(s.into())
    }
    pub fn unprotect(s: &str) -> Result<String, String> {
        Ok(s.into())
    }
}
