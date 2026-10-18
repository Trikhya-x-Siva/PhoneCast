//! Go live: YouTube and Twitch at the same time.
//!
//! Each destination gets its own FFmpeg process fed with the phone stream
//! (as live MKV on stdin). FFmpeg decodes on the GPU, fits the picture to a
//! 16:9 canvas at a steady 60 fps, encodes with the GPU encoder (NVENC on
//! NVIDIA) at that platform's best settings, and sends it over RTMPS.
//! Destinations are independent: if one drops, it reconnects on its own and
//! the other keeps going.

use crate::adapt::{Adapt, Change, Rung, SpeedMeter};
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

// Dynamic bitrate: the rungs a destination may drop to when its upload
// struggles, highest first. YouTube keeps 1440p (and its sharper VP9
// re-encode) at 14 Mbps before giving up resolution; Twitch follows its own
// guidance for slower connections.
const YOUTUBE_LADDER: &[Rung] = &[
    Rung {
        w: 2560,
        h: 1440,
        kbps: 14_000,
    },
    Rung {
        w: 1920,
        h: 1080,
        kbps: 9_000,
    },
];

const TWITCH_LADDER: &[Rung] = &[
    Rung {
        w: 1664,
        h: 936,
        kbps: 4_500,
    },
    Rung {
        w: 1280,
        h: 720,
        kbps: 3_500,
    },
];

/// The rungs below the chosen preset: never larger, always a lower bitrate.
fn fallback(ladder: &[Rung], p: &Preset) -> Vec<Rung> {
    ladder
        .iter()
        .filter(|r| r.h <= p.h && r.kbps < p.kbps)
        .copied()
        .collect()
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
    /// The chosen preset, e.g. "1440p60".
    pub quality: String,
    /// What is being sent right now, e.g. "1080p60" (dynamic bitrate).
    #[serde(rename = "qualityNow")]
    pub quality_now: String,
    /// Output speed over the last few seconds (FFmpeg's own `speed` field is
    /// an average since start and reacts far too slowly).
    #[serde(skip)]
    recent_speed: Option<f64>,
    /// When FFmpeg last reported progress; a long silence means it is stuck
    /// writing to a congested connection.
    #[serde(skip)]
    progress_at: Option<Instant>,
    /// Which FFmpeg process the stats belong to (a restarted one bumps it,
    /// so the old process's last lines cannot overwrite fresh stats).
    #[serde(skip)]
    gen: u64,
    /// Bitrate the encoder is set to right now (dynamic bitrate may lower it).
    #[serde(rename = "kbpsNow")]
    pub kbps_now: u32,
    /// Bitrate of the chosen quality.
    #[serde(rename = "kbpsBase")]
    pub kbps_base: u32,
    /// "good" or "reduced" (running below the chosen quality).
    pub health: &'static str,
    /// Plain-language note about the upload, shown in the Live card.
    pub note: Option<String>,
}

impl DestStatus {
    fn new(id: &'static str, name: &'static str, quality: String, kbps: u32) -> Self {
        Self {
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
            quality_now: quality.clone(),
            quality,
            recent_speed: None,
            progress_at: None,
            gen: 0,
            kbps_now: kbps,
            kbps_base: kbps,
            health: "good",
            note: None,
        }
    }
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
            let p = preset(list, &d.quality);
            let ladder = if id == "youtube" {
                YOUTUBE_LADDER
            } else {
                TWITCH_LADDER
            };
            plans.push((id, name, p, format!("{url}{key}"), fallback(ladder, p)));
        }
        if plans.is_empty() {
            return Err("Turn on YouTube or Twitch first".into());
        }

        let mut dests = self.dests.lock().unwrap();
        for (id, name, p, url, lower) in plans {
            let status = Arc::new(Mutex::new(DestStatus::new(id, name, p.id.into(), p.kbps)));
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
                pc: pc.clone(),
                file: false,
                fallback: lower,
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
    /// Lower qualities dynamic bitrate may switch to while the upload
    /// struggles (empty = always the chosen quality).
    pub fallback: Vec<Rung>,
}

/// Runs a job on its own thread fed by the returned sender. Used by Go live
/// and by the PC-mode recorder.
pub fn spawn_job(
    job: Job,
    config: VideoConfig,
    request_key: Arc<dyn Fn() + Send + Sync>,
    name: &'static str,
) -> Dest {
    let status = Arc::new(Mutex::new(DestStatus::new(
        name,
        name,
        format!("{}p", job.h),
        job.kbps,
    )));
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
    /// The chosen quality as a ladder rung.
    fn top(&self) -> Rung {
        Rung {
            w: self.w,
            h: self.h,
            kbps: self.kbps,
        }
    }

    /// FFmpeg arguments for this job at rung `r` (the chosen quality, or a
    /// lower one while dynamic bitrate is active). The framing never changes.
    fn args(&self, r: Rung) -> Vec<String> {
        if let Some(pc) = &self.pc {
            return self.pc_args(pc, r);
        }
        let (w, h, kbps) = (r.w, r.h, r.kbps);
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
        a.extend(encoder_args(&self.encoder, kbps, self.hevc, w >= 3840));
        a.extend(self.output_args());
        a
    }

    /// PC screen mode: FFmpeg captures the monitor (and webcam) itself;
    /// the mixed audio arrives as raw PCM on stdin.
    fn pc_args(&self, pc: &PcVideo, r: Rung) -> Vec<String> {
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
        let audio_idx = if pc
            .webcam
            .as_ref()
            .is_some_and(|c| c.enabled && !c.device.is_empty())
        {
            2
        } else {
            1
        };
        a.extend(
            ["-f", "s16le", "-ar", "48000", "-ac", "2", "-i", "pipe:0"]
                .iter()
                .map(|s| s.to_string()),
        );
        a.extend(pc.filter(r.w, r.h));
        a.extend(["-map".into(), format!("{audio_idx}:a")]);
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
            a.extend(encoder_args(&self.encoder, r.kbps, self.hevc, r.w >= 3840));
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

    fn spawn(&self, r: Rung) -> std::io::Result<Child> {
        let mut cmd = Command::new(&self.ffmpeg);
        cmd.args(self.args(r))
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
        // Dynamic bitrate: live outputs only (a recording has no upload).
        let adaptive = !self.file && self.kbps > 0 && !self.fallback.is_empty();
        let mut ladder = vec![self.top()];
        if adaptive {
            ladder.extend(self.fallback.iter().copied());
        }
        let mut adapt = Adapt::new(ladder);
        let mut adjusting = false;
        let mut attempt = 0u32;
        let mut gen = 0u64;
        'outer: while !stop.load(Ordering::SeqCst) {
            let rung = adapt.rung();
            let mut child = match self.spawn(rung) {
                Ok(c) => c,
                Err(e) => {
                    set(&|s| {
                        s.state = "error".into();
                        s.error = Some(format!("Could not start FFmpeg: {e}"));
                    });
                    return;
                }
            };
            adapt.restarted(Instant::now());
            gen += 1;
            set(&|s| {
                s.gen = gen;
                s.kbps_now = rung.kbps;
                s.quality_now = format!("{}p60", rung.h);
                s.recent_speed = None;
                s.progress_at = None;
                s.speed = 0.0;
                s.state = if adjusting {
                    "adjusting".into()
                } else if attempt == 0 {
                    "connecting".into()
                } else {
                    "reconnecting".into()
                };
            });
            adjusting = false;
            watch_progress(child.stderr.take().unwrap(), status.clone(), gen);
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
            let mut restart_for_bitrate = false;
            let mut sender_gone = false;
            let mut last_check = Instant::now();

            while let Some(w) = writer.as_mut() {
                if stop.load(Ordering::SeqCst) {
                    break;
                }
                // Upload health, twice a second: step down when the upload
                // struggles, back up once it has been smooth for a minute.
                if adaptive && last_check.elapsed() >= Duration::from_millis(500) {
                    let now = Instant::now();
                    last_check = now;
                    let (speed, on_air) = {
                        let s = status.lock().unwrap();
                        // No progress report for a while: FFmpeg is stuck
                        // writing to the network, the clearest sign of all.
                        let stalled = s.progress_at.is_some_and(|t| t.elapsed() > STALLED);
                        (
                            if stalled { Some(0.0) } else { s.recent_speed },
                            s.state == "live",
                        )
                    };
                    if on_air {
                        if let Some(change) = adapt.observe(speed, now) {
                            let sp = speed.unwrap_or(0.0);
                            let (note, msg) = match change {
                                Change::Down(r) => (
                                    format!("Upload struggling → lowered to {}", r.label()),
                                    format!(
                                        "{name}: upload struggling ({sp:.2}x for {} s) - lowering {} → {}",
                                        crate::adapt::SLOW_FOR.as_secs(),
                                        rung.label(),
                                        r.label()
                                    ),
                                ),
                                Change::Up(r) if adapt.reduced() => (
                                    format!("Upload recovering → raised to {}", r.label()),
                                    format!(
                                        "{name}: upload stable for 60 s - raising {} → {}",
                                        rung.label(),
                                        r.label()
                                    ),
                                ),
                                Change::Up(r) => (
                                    format!("Upload recovered → back to {}", r.label()),
                                    format!(
                                        "{name}: upload stable for 60 s - back to {}",
                                        r.label()
                                    ),
                                ),
                            };
                            crate::diag::log("live", msg);
                            let reduced = adapt.reduced();
                            set(&|s| {
                                s.note = Some(note.clone());
                                s.health = if reduced { "reduced" } else { "good" };
                            });
                            restart_for_bitrate = true;
                            break;
                        }
                    }
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
            finish(&mut child, writer.take(), graceful);
            if graceful {
                break;
            }
            if restart_for_config {
                attempt += 1;
                continue;
            }
            if restart_for_bitrate {
                // Same connection and framing, new size/bitrate; the other
                // destination is untouched. Starts again on the next keyframe.
                adjusting = true;
                set(&|s| s.state = "adjusting".into());
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

/// No progress report from FFmpeg for this long = stuck on the network.
const STALLED: Duration = Duration::from_secs(3);

/// Parses FFmpeg's `-progress` output into live stats; other lines are errors.
/// `gen` identifies this FFmpeg process; once the job restarts FFmpeg, the
/// old process's remaining lines are ignored.
fn watch_progress(stderr: std::process::ChildStderr, status: Arc<Mutex<DestStatus>>, gen: u64) {
    std::thread::spawn(move || {
        let mut last_frame = 0u64;
        let mut meter = SpeedMeter::new(Duration::from_secs(3));
        let mut out_us = 0i64;
        for line in BufReader::new(stderr).lines().map_while(Result::ok) {
            let line = line.trim();
            let Some((k, v)) = line.split_once('=') else {
                if !line.is_empty() {
                    let mut s = status.lock().unwrap();
                    crate::diag::log("live", format!("{}: ffmpeg: {line}", s.name));
                    if s.gen == gen {
                        s.error = Some(friendly_error(line));
                    }
                }
                continue;
            };
            let mut s = status.lock().unwrap();
            if s.gen != gen {
                continue;
            }
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
                "out_time_us" => out_us = v.parse().unwrap_or(out_us),
                // End of one report (once a second).
                "progress" => {
                    let now = Instant::now();
                    s.progress_at = Some(now);
                    s.recent_speed = meter.add(now, out_us);
                    s.speed = s.recent_speed.unwrap_or(0.0);
                }
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

    #[test]
    fn ladders_start_below_the_chosen_preset() {
        let r = |w, h, kbps| Rung { w, h, kbps };
        assert_eq!(
            fallback(YOUTUBE_LADDER, preset(YOUTUBE, "1440p60")),
            vec![r(2560, 1440, 14_000), r(1920, 1080, 9_000)]
        );
        assert_eq!(
            fallback(YOUTUBE_LADDER, preset(YOUTUBE, "1080p60")),
            vec![r(1920, 1080, 9_000)]
        );
        assert_eq!(
            fallback(TWITCH_LADDER, preset(TWITCH, "1080p60")),
            vec![r(1664, 936, 4_500), r(1280, 720, 3_500)]
        );
        assert_eq!(
            fallback(TWITCH_LADDER, preset(TWITCH, "720p60")),
            vec![r(1280, 720, 3_500)]
        );
    }

    #[test]
    fn every_rung_keeps_the_framing() {
        let top = preset(YOUTUBE, "1440p60");
        for framing in [Framing::Blur, Framing::Fit, Framing::Fill] {
            for nvenc in [true, false] {
                let job = Job {
                    ffmpeg: PathBuf::from("ffmpeg"),
                    url: "rtmps://example.invalid/live".into(),
                    encoder: if nvenc { "h264_nvenc" } else { "libx264" }.into(),
                    w: top.w,
                    h: top.h,
                    kbps: top.kbps,
                    hevc: false,
                    framing,
                    pc: None,
                    file: false,
                    fallback: fallback(YOUTUBE_LADDER, top),
                };
                let mut rungs = vec![job.top()];
                rungs.extend(job.fallback.iter().copied());
                let (_, want) = video_filter(nvenc, framing, 1, 1);
                // The part of the graph that decides the framing.
                let marker = match framing {
                    Framing::Blur => "boxblur",
                    Framing::Fit => "pad",
                    Framing::Fill => "crop=",
                };
                assert!(want.contains(marker));
                for r in rungs {
                    let a = job.args(r).join(" ");
                    assert!(a.contains(marker), "{framing:?} lost at {r:?}: {a}");
                    assert!(
                        a.contains(&format!("{}:{}", r.w, r.h))
                            || a.contains(&format!("w={}:h={}", r.w, r.h)),
                        "{a}"
                    );
                    assert!(a.contains(&format!("-b:v {}k", r.kbps)), "{a}");
                    assert!(
                        a.contains("-filter_complex") && a.contains("-map [v]"),
                        "{a}"
                    );
                    assert!(!a.contains("nobuffer"), "{a}");
                }
            }
        }
    }

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
            output: mon.output,
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
            fallback: Vec::new(),
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
                fallback: Vec::new(),
            };
            let cfg = VideoConfig {
                codec: crate::mkv::CODEC_H264,
                width: 1280,
                height: 720,
                csd: csd.clone(),
            };
            let status = Arc::new(Mutex::new(DestStatus::new(
                "test",
                "Test",
                "1440p60".into(),
                24_000,
            )));
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

#[cfg(test)]
mod throttle_tests {
    //! Simulated slow upload: the live job streams FLV over TCP to a local
    //! sink that reads at a limited rate (what a congested upload looks like
    //! to FFmpeg under RTMP, which also runs over TCP). The bitrate must step
    //! down while the sink is slow and step back up after it is lifted.
    //! Takes ~2.5 minutes:
    //! `cargo test slow_upload_steps_bitrate_down_and_up -- --ignored --nocapture`
    use super::*;
    use std::io::Read;
    use std::net::TcpListener;
    use std::sync::atomic::AtomicU64;

    /// Accepts connections forever; each one is drained at `limit` bytes/s
    /// (u64::MAX = unlimited) with a small receive buffer so back-pressure
    /// reaches FFmpeg quickly.
    fn slow_sink(limit: Arc<AtomicU64>) -> u16 {
        let l = TcpListener::bind("127.0.0.1:0").unwrap();
        let port = l.local_addr().unwrap().port();
        std::thread::spawn(move || {
            for stream in l.incoming().flatten() {
                socket2::SockRef::from(&stream)
                    .set_recv_buffer_size(64 * 1024)
                    .ok();
                let limit = limit.clone();
                std::thread::spawn(move || {
                    let mut s = stream;
                    let mut buf = vec![0u8; 16 * 1024];
                    let start = Instant::now();
                    let mut total = 0u64;
                    while let Ok(n) = s.read(&mut buf) {
                        if n == 0 {
                            break;
                        }
                        total += n as u64;
                        let lim = limit.load(Ordering::SeqCst);
                        if lim != u64::MAX {
                            // Sleep until `total` bytes are "allowed" by the rate.
                            let due = Duration::from_secs_f64(total as f64 / lim as f64);
                            if let Some(w) = due.checked_sub(start.elapsed()) {
                                std::thread::sleep(w);
                            }
                        }
                    }
                });
            }
        });
        port
    }

    /// 8 s of 720p60 test video as access units: (keyframe, Annex-B bytes).
    fn test_video(ffmpeg: &PathBuf, enc: &str) -> (Vec<u8>, Vec<(bool, Vec<u8>)>) {
        let mut cmd = Command::new(ffmpeg);
        cmd.args([
            "-hide_banner",
            "-loglevel",
            "error",
            "-f",
            "lavfi",
            "-i",
            "testsrc2=size=1280x720:rate=60",
            "-t",
            "8",
            // Noise makes the picture hard to compress, so the live encoder
            // really sends the bitrate it is set to (like a busy game).
            "-vf",
            "noise=alls=24:allf=t",
            "-c:v",
            enc,
            "-b:v",
            "20M",
            "-g",
            "120",
            "-bf",
            "0",
            "-aud",
            "1",
            "-f",
            "h264",
            "pipe:1",
        ]);
        cmd.stdin(Stdio::null()).stderr(Stdio::inherit());
        let data = cmd.output().unwrap().stdout;
        assert!(data.len() > 100_000, "no test video produced");
        let mut csd = Vec::new();
        let mut aus: Vec<(bool, Vec<u8>)> = Vec::new();
        for n in crate::mkv::split_nals(&data) {
            match n[0] & 0x1F {
                9 => aus.push((false, Vec::new())),
                7 | 8 if aus.len() <= 1 => {
                    csd.extend_from_slice(&[0, 0, 0, 1]);
                    csd.extend_from_slice(n);
                }
                6..=8 => {}
                t => {
                    let au = aus.last_mut().unwrap();
                    au.0 |= t == 5;
                    au.1.extend_from_slice(&[0, 0, 0, 1]);
                    au.1.extend_from_slice(n);
                }
            }
        }
        aus.retain(|a| !a.1.is_empty());
        assert!(aus[0].0, "first access unit must be a keyframe");
        (csd, aus)
    }

    #[test]
    #[ignore]
    fn slow_upload_steps_bitrate_down_and_up() {
        let ffmpeg = find_ffmpeg().expect("ffmpeg");
        let enc = probe_encoders(&ffmpeg);
        println!("encoder: {}", enc.h264);
        let (csd, aus) = test_video(&ffmpeg, &enc.h264);
        println!("{} access units of test video", aus.len());

        // The real YouTube setup: 1440p60 at 20 Mbps, blurred-background
        // framing, its real ladder. The "upload" accepts 11 Mbps: too little
        // for 20 or 14 Mbps, enough for 1080p60 at 9 Mbps.
        let top = preset(YOUTUBE, "1440p60");
        let lower = fallback(YOUTUBE_LADDER, top);
        let limit = Arc::new(AtomicU64::new(11_000_000 / 8));
        let port = slow_sink(limit.clone());
        let job = Job {
            ffmpeg: ffmpeg.clone(),
            url: format!("tcp://127.0.0.1:{port}"),
            encoder: enc.h264.clone(),
            w: top.w,
            h: top.h,
            kbps: top.kbps,
            hevc: false,
            framing: Framing::Blur,
            pc: None,
            file: false,
            fallback: lower,
        };
        let cfg = VideoConfig {
            codec: crate::mkv::CODEC_H264,
            width: 1280,
            height: 720,
            csd,
        };
        let status = Arc::new(Mutex::new(DestStatus::new(
            "youtube",
            "YouTube",
            top.id.into(),
            top.kbps,
        )));
        let stop = Arc::new(AtomicBool::new(false));
        let (mut tx, rx) = SinkTx::new(2400);
        let (st, sp) = (status.clone(), stop.clone());
        let t = std::thread::spawn(move || job.run(rx, cfg, Arc::new(|| {}), st, sp));

        // Feed the 8 s loop in real time, recording every quality change.
        let start = Instant::now();
        let mut i = 0i64;
        let mut audio_t = 0i64;
        let mut changes: Vec<(Duration, u32, String, Option<String>)> = Vec::new();
        let mut current = top.kbps;
        let mut lifted_at: Option<Duration> = None;
        let mut last_print = Instant::now();
        while start.elapsed() < Duration::from_secs(300) {
            let (key, au) = &aus[(i as usize) % aus.len()];
            let pts = 1_000_000 + i * 16_667;
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
            i += 1;
            if let Some(w) = Duration::from_micros(i as u64 * 16_667).checked_sub(start.elapsed()) {
                std::thread::sleep(w);
            }
            let s = status.lock().unwrap().clone();
            if last_print.elapsed() >= Duration::from_secs(5) {
                last_print = Instant::now();
                println!(
                    "t={:>3}s state={} speed={:.2}x now={} {} kbps health={}",
                    start.elapsed().as_secs(),
                    s.state,
                    s.speed,
                    s.quality_now,
                    s.kbps_now,
                    s.health
                );
            }
            if s.kbps_now != current {
                current = s.kbps_now;
                println!(
                    "t={:?}: {} at {} kbps - {:?}",
                    start.elapsed(),
                    s.quality_now,
                    s.kbps_now,
                    s.note
                );
                changes.push((
                    start.elapsed(),
                    s.kbps_now,
                    s.quality_now.clone(),
                    s.note.clone(),
                ));
            }
            // Once at the bottom rung, let it settle, then lift the throttle.
            if lifted_at.is_none()
                && changes.last().is_some_and(|c| {
                    c.1 == 9_000 && start.elapsed() > c.0 + Duration::from_secs(15)
                })
            {
                lifted_at = Some(start.elapsed());
                limit.store(u64::MAX, Ordering::SeqCst);
                println!("throttle lifted at {:?}", start.elapsed());
            }
            if lifted_at.is_some() && current > 9_000 {
                break; // stepped back up
            }
        }
        stop.store(true, Ordering::SeqCst);
        drop(tx);
        t.join().unwrap();

        let kbps: Vec<u32> = changes.iter().map(|c| c.1).collect();
        assert!(
            kbps.starts_with(&[14_000, 9_000, 14_000]),
            "quality changes: {changes:?}"
        );
        let (down1, _, q1, n1) = &changes[0];
        let (down2, _, q2, n2) = &changes[1];
        let (up, _, q3, n3) = &changes[2];
        assert!(
            *down1 < Duration::from_secs(45),
            "first step-down took {down1:?}"
        );
        assert_eq!(q1, "1440p60");
        assert_eq!(
            n1.as_deref(),
            Some("Upload struggling → lowered to 1440p60 · 14.0 Mbps")
        );
        // Never more than one restart a minute (one progress check of slack).
        assert!(
            *down2 - *down1 >= Duration::from_millis(59_500),
            "{down1:?} → {down2:?}"
        );
        assert_eq!(q2, "1080p60");
        assert_eq!(
            n2.as_deref(),
            Some("Upload struggling → lowered to 1080p60 · 9.0 Mbps")
        );
        assert!(
            *up - *down2 >= Duration::from_millis(59_500),
            "{down2:?} → {up:?}"
        );
        assert_eq!(q3, "1440p60");
        assert_eq!(
            n3.as_deref(),
            Some("Upload recovering → raised to 1440p60 · 14.0 Mbps")
        );
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
