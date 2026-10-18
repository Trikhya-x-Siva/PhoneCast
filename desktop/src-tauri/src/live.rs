//! Go live: YouTube, Twitch, Kick, Facebook Gaming and custom RTMP/SRT
//! servers, several at once.
//!
//! Each destination gets its own FFmpeg process fed with the phone stream
//! (as live MKV on stdin). FFmpeg decodes on the GPU, fits the picture to a
//! 16:9 canvas at a steady 60 fps, encodes with the GPU encoder (NVENC on
//! NVIDIA) with the platform preset from `presets.rs`, and sends it through
//! FFmpeg's fifo muxer: a network drop reconnects inside FFmpeg without
//! restarting the encoder. Destinations are independent: if one drops, it
//! reconnects on its own and the others keep going.

use crate::capture::PcVideo;
use crate::mkv::{MkvWriter, VideoConfig};
use crate::presets::{self, Codec, Platform, Preset};
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

/// One place to stream to.
#[derive(Serialize, Deserialize, Clone, Default, Debug)]
#[serde(default, rename_all = "camelCase")]
pub struct DestSettings {
    pub id: String,
    /// youtube | twitch | kick | facebook | custom
    pub platform: String,
    pub enabled: bool,
    /// The main platform keeps full quality when the upload is short.
    pub main: bool,
    pub preset: String,
    /// Server URL; empty = the platform's default ingest.
    pub url: String,
    /// YouTube: send to the backup ingest server instead.
    pub backup: bool,
    /// Twitch: bandwidth-test mode (viewers don't see the stream).
    pub bandwidth_test: bool,
    /// Stream key, encrypted with Windows DPAPI (hex). Never stored in plain text.
    key_enc: String,
}

/// Settings from PhoneCast 0.1 (YouTube + Twitch only), migrated on load.
#[derive(Deserialize, Clone, Default)]
#[serde(default)]
struct LegacyDest {
    enabled: bool,
    quality: String,
    key_enc: String,
}

#[derive(Serialize, Deserialize, Clone)]
#[serde(default, rename_all = "camelCase")]
pub struct LiveSettings {
    pub destinations: Vec<DestSettings>,
    /// "blur" = whole picture over a blurred copy of itself (what mobile
    /// streamers use), "fit" = whole picture with black bars,
    /// "fill" = crop the sides to fill 16:9.
    pub framing: String,
    /// Measured upload in Mbps: the minimum seen during the setup check.
    pub upload_mbps: Option<f64>,
    /// The first-run setup check has been completed (or skipped).
    pub setup_done: bool,
    #[serde(skip_serializing, alias = "youtube")]
    legacy_youtube: Option<LegacyDest>,
    #[serde(skip_serializing, alias = "twitch")]
    legacy_twitch: Option<LegacyDest>,
}

impl Default for LiveSettings {
    fn default() -> Self {
        Self {
            destinations: Vec::new(),
            framing: "blur".into(),
            upload_mbps: None,
            setup_done: false,
            legacy_youtube: None,
            legacy_twitch: None,
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
        let mut s: Self = std::fs::read(settings_path())
            .ok()
            .and_then(|b| serde_json::from_slice(&b).ok())
            .unwrap_or_default();
        s.migrate();
        s
    }

    /// Turns 0.1 settings (fixed YouTube/Twitch slots) into destinations.
    fn migrate(&mut self) {
        let yt = self.legacy_youtube.take();
        let tw = self.legacy_twitch.take();
        if !self.destinations.is_empty() {
            return;
        }
        for (pf, old) in [("youtube", yt), ("twitch", tw)] {
            let Some(old) = old else { continue };
            if !old.enabled && old.key_enc.is_empty() {
                continue;
            }
            let preset = match (pf, old.quality.as_str()) {
                ("youtube", "1080p60") => "yt-1080p60-h264",
                ("youtube", _) => "yt-1440p60-hevc",
                ("twitch", "936p60") => "tw-936p60",
                ("twitch", "720p60") => "tw-720p60",
                _ => "tw-1080p60",
            };
            self.destinations.push(DestSettings {
                id: pf.into(),
                platform: pf.into(),
                enabled: old.enabled,
                main: pf == "youtube",
                preset: preset.into(),
                key_enc: old.key_enc,
                ..Default::default()
            });
        }
    }

    fn save(&self) -> std::io::Result<()> {
        let p = settings_path();
        if let Some(d) = p.parent() {
            std::fs::create_dir_all(d)?;
        }
        std::fs::write(p, serde_json::to_vec_pretty(self)?)
    }

    /// Applies an edit from the UI. `encrypt` = false is a dry run for live
    /// validation: typed keys only count as "present".
    fn apply(&mut self, v: &Value, encrypt: bool) -> Result<(), String> {
        if let Some(f) = v.get("framing").and_then(Value::as_str) {
            self.framing = match Framing::parse(f) {
                Framing::Blur => "blur",
                Framing::Fit => "fit",
                Framing::Fill => "fill",
            }
            .into();
        }
        if let Some(m) = v.get("uploadMbps") {
            self.upload_mbps = m.as_f64().filter(|x| *x > 0.0);
        }
        if let Some(b) = v.get("setupDone").and_then(Value::as_bool) {
            self.setup_done = b;
        }
        let Some(list) = v.get("destinations").and_then(Value::as_array) else {
            return Ok(());
        };
        let mut out = Vec::new();
        let mut seen_main = false;
        for (n, o) in list.iter().enumerate() {
            let Some(pf) = o
                .get("platform")
                .and_then(Value::as_str)
                .and_then(Platform::parse)
            else {
                continue;
            };
            let mut id = o
                .get("id")
                .and_then(Value::as_str)
                .unwrap_or_default()
                .to_string();
            if id.is_empty() || out.iter().any(|d: &DestSettings| d.id == id) {
                id = format!(
                    "{}-{}-{n}",
                    pf.id(),
                    chrono::Local::now().timestamp_millis()
                );
            }
            let old = self.destinations.iter().find(|d| d.id == id);
            let s = |k: &str| {
                o.get(k)
                    .and_then(Value::as_str)
                    .unwrap_or_default()
                    .trim()
                    .to_string()
            };
            let b = |k: &str| o.get(k).and_then(Value::as_bool).unwrap_or(false);
            let mut d = DestSettings {
                id: id.clone(),
                platform: pf.id().into(),
                enabled: b("enabled"),
                main: b("main") && !seen_main,
                preset: pf.preset(&s("preset")).id.into(),
                url: s("url"),
                backup: b("backup") && pf == Platform::Youtube,
                bandwidth_test: b("bandwidthTest") && pf == Platform::Twitch,
                key_enc: old.map(|d| d.key_enc.clone()).unwrap_or_default(),
            };
            seen_main |= d.main;
            if let Some(k) = o.get("key").and_then(Value::as_str) {
                let k = k.trim();
                d.key_enc = if k.is_empty() {
                    String::new()
                } else if encrypt {
                    dpapi::protect(k)?
                } else {
                    "typed".into()
                };
            }
            out.push(d);
        }
        self.destinations = out;
        Ok(())
    }

    /// Enabled destinations as the validator sees them.
    fn inputs(&self) -> Vec<presets::DestInput> {
        self.destinations
            .iter()
            .filter(|d| d.enabled)
            .filter_map(|d| {
                let pf = Platform::parse(&d.platform)?;
                Some(presets::DestInput {
                    id: d.id.clone(),
                    platform: pf,
                    preset: d.preset.clone(),
                    main: d.main,
                    url: server_url(d, pf),
                    has_key: !d.key_enc.is_empty(),
                })
            })
            .collect()
    }
}

/// The ingest server a destination sends to.
fn server_url(d: &DestSettings, pf: Platform) -> String {
    if d.backup {
        if let Some(b) = pf.backup_url() {
            return b.into();
        }
    }
    if d.url.trim().is_empty() {
        pf.default_url().into()
    } else {
        d.url.trim().into()
    }
}

// ---------------- encoders ----------------

#[derive(Clone)]
struct Encoders {
    h264: String,
    hevc: String,
}

pub fn find_ffmpeg() -> Option<PathBuf> {
    // PhoneCast's own copy (installed by the setup wizard) comes first.
    let mut c: Vec<PathBuf> = crate::setup::managed_ffmpeg().into_iter().collect();
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

impl Encoders {
    /// HEVC only counts when a GPU encoder can do it in real time.
    fn hevc_hw(&self) -> bool {
        !self.hevc.starts_with("lib")
    }
}

/// Video encoder settings for one preset: CBR, closed GOP with IDR
/// keyframes exactly every `gop_s` seconds, the platform's B-frame limit,
/// profile/level, and global headers (needed so the fifo muxer can resend
/// the codec header after a reconnect).
fn encoder_args(enc: &str, pr: &Preset) -> Vec<String> {
    let hevc = pr.codec == Codec::Hevc;
    let b = format!("{}k", pr.kbps);
    let gop = pr.gop_frames().to_string();
    let bf = pr.max_bframes.to_string();
    let mut a: Vec<String> = vec!["-c:v".into(), enc.into()];
    let more: Vec<&str> = if enc.contains("nvenc") {
        // Highest-quality NVENC settings that still run in real time on RTX
        // 40 (p5; p4 for 4K so it keeps up alongside a second stream).
        // qres two-pass, B-frames as references and a short lookahead
        // sharpen motion; no scene-cut keyframes and forced IDR keep the
        // GOP exactly what the platform asks for.
        vec![
            "-preset",
            if pr.h >= 2160 { "p4" } else { "p5" },
            "-tune",
            "hq",
            "-rc",
            "cbr",
            "-multipass",
            "qres",
            "-spatial-aq",
            "1",
            "-temporal-aq",
            "1",
            "-rc-lookahead",
            "8",
            "-no-scenecut",
            "1",
            "-forced-idr",
            "1",
            "-bf",
            &bf,
            "-b_ref_mode",
            if pr.max_bframes >= 2 {
                "middle"
            } else {
                "disabled"
            },
        ]
    } else if enc.contains("amf") {
        vec!["-usage", "transcoding", "-quality", "quality", "-rc", "cbr"]
    } else if enc.contains("qsv") {
        vec!["-preset", "medium", "-bf", &bf]
    } else {
        vec![
            "-preset",
            "veryfast",
            "-x264-params",
            "nal-hrd=cbr",
            "-sc_threshold",
            "0",
            "-bf",
            &bf,
        ]
    };
    a.extend(more.iter().map(|s| s.to_string()));
    a.extend([
        "-profile:v".into(),
        if hevc { "main" } else { pr.profile }.into(),
    ]);
    if let (Some(level), false) = (pr.level, hevc) {
        a.extend(["-level:v".into(), level.into()]);
    }
    a.extend([
        "-b:v".into(),
        b.clone(),
        "-maxrate".into(),
        b.clone(),
        // One second of buffer: what OBS uses for CBR; keeps the bitrate
        // steady instead of bursting over a tight upload.
        "-bufsize".into(),
        b,
        "-g".into(),
        gop.clone(),
        "-keyint_min".into(),
        gop,
        "-flags:v".into(),
        "+global_header".into(),
    ]);
    a
}

/// AAC-LC at the platform's rate and bitrate.
fn audio_args(pr: &Preset) -> Vec<String> {
    vec![
        "-c:a".into(),
        "aac".into(),
        "-b:a".into(),
        format!("{}k", pr.audio.kbps),
        "-ar".into(),
        pr.audio.rate.to_string(),
        "-ac".into(),
        pr.audio.channels.to_string(),
        "-flags:a".into(),
        "+global_header".into(),
    ]
}

/// Network output through FFmpeg's fifo muxer: when the connection drops
/// (or stalls for `rw_timeout`), FFmpeg reconnects every second on its own,
/// restarts on a keyframe and drops packets meanwhile, so the encoder and
/// the PhoneCast side never restart. Verified with FFmpeg 9.0.2 against a
/// local RTMP server that was killed and restarted mid-stream.
fn fifo_output_args(container: &str, url: &str) -> Vec<String> {
    let format_opts = if container == "flv" {
        "flvflags=no_duration_filesize:rw_timeout=6000000"
    } else {
        "rw_timeout=6000000"
    };
    [
        "-f",
        "fifo",
        "-fifo_format",
        container,
        "-format_opts",
        format_opts,
        "-attempt_recovery",
        "1",
        "-recover_any_error",
        "1",
        "-max_recovery_attempts",
        "0",
        "-recovery_wait_time",
        "1",
        "-restart_with_keyframe",
        "1",
        "-drop_pkts_on_overflow",
        "1",
        "-queue_size",
        "120",
        url,
    ]
    .iter()
    .map(|s| s.to_string())
    .collect()
}

// ---------------- destination status ----------------

#[derive(Clone, Serialize)]
pub struct DestStatus {
    pub id: String,
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
    ffmpeg: Mutex<Option<PathBuf>>,
    encoders: Arc<Mutex<Option<Encoders>>>,
    started: Mutex<Option<Instant>>,
}

impl Live {
    pub fn new() -> Arc<Live> {
        let live = Arc::new(Live {
            settings: Mutex::new(LiveSettings::load()),
            dests: Mutex::new(Vec::new()),
            ffmpeg: Mutex::new(None),
            encoders: Arc::new(Mutex::new(None)),
            started: Mutex::new(None),
        });
        live.reload_ffmpeg();
        live
    }

    /// Looks for FFmpeg again (after the setup wizard installed it) and
    /// re-probes the encoders in the background.
    pub fn reload_ffmpeg(&self) {
        let ffmpeg = find_ffmpeg();
        *self.ffmpeg.lock().unwrap() = ffmpeg.clone();
        *self.encoders.lock().unwrap() = None;
        if let Some(ff) = ffmpeg {
            let enc = self.encoders.clone();
            std::thread::spawn(move || *enc.lock().unwrap() = Some(probe_encoders(&ff)));
        }
    }

    /// Encoders on this PC, probing now if the background probe hasn't
    /// finished (blocking: call off the UI thread).
    pub fn encoder_info(&self) -> Value {
        let Some(ff) = self.ffmpeg() else {
            return json!({ "ffmpeg": null });
        };
        let known = self.encoders.lock().unwrap().clone();
        let enc = known.unwrap_or_else(|| {
            let e = probe_encoders(&ff);
            *self.encoders.lock().unwrap() = Some(e.clone());
            e
        });
        json!({
            "ffmpeg": ff.display().to_string(),
            "managed": crate::setup::managed_ffmpeg().is_some_and(|m| m == ff),
            "h264": enc.h264,
            "h264Label": encoder_label(&enc.h264),
            "hevc": enc.hevc,
            "hevcHardware": enc.hevc_hw(),
            "gpu": !enc.h264.starts_with("lib"),
        })
    }

    fn hevc(&self) -> bool {
        self.encoders
            .lock()
            .unwrap()
            .as_ref()
            .is_some_and(|e| e.hevc_hw())
    }

    pub fn settings_json(&self) -> Value {
        let s = self.settings.lock().unwrap().clone();
        let enc = self.encoders.lock().unwrap().clone();
        let hevc = self.hevc();
        json!({
            "destinations": s.destinations.iter().map(|d| json!({
                "id": d.id,
                "platform": d.platform,
                "enabled": d.enabled,
                "main": d.main,
                "preset": d.preset,
                "url": d.url,
                "backup": d.backup,
                "bandwidthTest": d.bandwidth_test,
                "hasKey": !d.key_enc.is_empty(),
            })).collect::<Vec<_>>(),
            "catalog": presets::catalog_json(hevc),
            "framing": s.framing,
            "ffmpeg": self.ffmpeg().is_some(),
            "encoder": enc.as_ref().map(|e| encoder_label(&e.h264)),
            "hevc": hevc,
            "encodersKnown": enc.is_some(),
            "uploadMbps": s.upload_mbps,
            "setupDone": s.setup_done,
            "plan": presets::plan(&s.inputs(), s.upload_mbps, hevc),
        })
    }

    /// Validates an unsaved edit (live messages in the Go live window).
    pub fn check_settings(&self, v: &Value) -> Result<Value, String> {
        let mut s = self.settings.lock().unwrap().clone();
        s.apply(v, false)?;
        Ok(
            serde_json::to_value(presets::plan(&s.inputs(), s.upload_mbps, self.hevc()))
                .unwrap_or_default(),
        )
    }

    /// Updates settings. Keys are only replaced when a new one is typed;
    /// an empty string clears it.
    pub fn update_settings(&self, v: &Value) -> Result<(), String> {
        let mut s = self.settings.lock().unwrap();
        s.apply(v, true)?;
        s.save().map_err(|e| e.to_string())
    }

    pub fn is_live(&self) -> bool {
        !self.dests.lock().unwrap().is_empty()
    }

    pub fn ffmpeg(&self) -> Option<PathBuf> {
        self.ffmpeg.lock().unwrap().clone()
    }

    /// Encoder for the PC-mode recorder (file, visually lossless).
    pub fn encoders_for_file(&self) -> Option<(String, PathBuf)> {
        let ff = self.ffmpeg()?;
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
        let ffmpeg = self.ffmpeg().ok_or("FFmpeg is not installed on this PC")?;
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

        // Pre-flight: platform limits + upload budget. Blocks on errors.
        let plan = presets::plan(&s.inputs(), s.upload_mbps, enc.hevc_hw());
        if !plan.can_start {
            let first = plan
                .issues
                .iter()
                .find(|i| i.level == presets::Level::Error)
                .map(|i| i.text.clone())
                .unwrap_or_else(|| "Check the Go live settings".into());
            return Err(first);
        }
        let mut plans = Vec::new();
        for c in &plan.chosen {
            let Some(d) = s.destinations.iter().find(|d| d.id == c.dest) else {
                continue;
            };
            let Some(pf) = Platform::parse(&d.platform) else {
                continue;
            };
            let key = if d.key_enc.is_empty() {
                String::new()
            } else {
                dpapi::unprotect(&d.key_enc)?
            };
            let server = server_url(d, pf);
            let url = presets::compose_url(pf, &server, &key, d.bandwidth_test);
            let container = if server.to_ascii_lowercase().starts_with("srt://") {
                "mpegts"
            } else {
                "flv"
            };
            crate::diag::log(
                "live",
                format!(
                    "{}: preset {}{} → {}",
                    pf.name(),
                    c.preset,
                    if c.lowered {
                        " (lowered for upload)"
                    } else {
                        ""
                    },
                    server
                ),
            );
            plans.push((d.id.clone(), pf, *pf.preset(&c.preset), url, container));
        }

        let mut dests = self.dests.lock().unwrap();
        for (id, pf, p, url, container) in plans {
            let status = Arc::new(Mutex::new(DestStatus {
                id: id.clone(),
                name: pf.name(),
                state: "connecting".into(),
                fps: 0.0,
                kbps: p.kbps as f64,
                dropped: 0,
                speed: 0.0,
                error: None,
                since: None,
                seconds: 0,
                reconnects: 0,
                quality: p.label.split(" · ").next().unwrap_or(p.id).into(),
            }));
            let stop = Arc::new(AtomicBool::new(false));
            // ~15 s of buffer (video + 10 ms audio packets): rides out upload and
            // CPU hiccups instead of skipping ahead; adds no delay when all is well.
            let (tx, rx) = SinkTx::new(2400);
            let job = Job {
                ffmpeg: ffmpeg.clone(),
                url,
                encoder: if p.codec == Codec::Hevc {
                    enc.hevc.clone()
                } else {
                    enc.h264.clone()
                },
                w: p.w,
                h: p.h,
                kbps: p.kbps,
                hevc: p.codec == Codec::Hevc,
                framing: Framing::parse(&s.framing),
                pc: pc.clone(),
                file: false,
                preset: Some(p),
                container,
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
    /// Platform preset (codec, GOP, B-frames, profile, audio). None = a
    /// generic H.264 preset built from `w`/`h`/`kbps`.
    pub preset: Option<Preset>,
    /// Network container: "flv" (RTMP/RTMPS) or "mpegts" (SRT).
    pub container: &'static str,
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
        id: name.into(),
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
    /// The preset this job encodes with.
    fn spec(&self) -> Preset {
        self.preset.unwrap_or(Preset {
            id: "custom",
            label: "Custom",
            codec: if self.hevc { Codec::Hevc } else { Codec::H264 },
            w: self.w,
            h: self.h,
            fps: 60,
            kbps: self.kbps,
            gop_s: 2,
            max_bframes: 2,
            profile: "high",
            level: None,
            audio: presets::Audio {
                rate: 48_000,
                kbps: 160,
                channels: 2,
            },
        })
    }

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
            "level+info",
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
        a.extend(encoder_args(&self.encoder, &self.spec()));
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
            "level+info",
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
        a.extend(pc.filter(self.w, self.h));
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
            a.extend(encoder_args(&self.encoder, &self.spec()));
        }
        a.extend(self.output_args());
        a
    }

    fn output_args(&self) -> Vec<String> {
        if self.file {
            let mut a: Vec<String> = ["-c:a", "pcm_s16le", "-f", "matroska"]
                .iter()
                .map(|s| s.to_string())
                .collect();
            a.push(self.url.clone());
            return a;
        }
        let mut a = audio_args(&self.spec());
        a.extend(fifo_output_args(self.container, &self.url));
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
            watch_progress(child.stderr.take().unwrap(), status.clone(), !self.file);
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

/// Parses FFmpeg's output: `-progress` key=value lines become live stats,
/// `[error]` lines become the shown error, and the fifo muxer's recovery
/// messages drive the connecting → live → reconnecting state.
///
/// With the fifo muxer FFmpeg keeps encoding while the network is down, so
/// frames alone don't prove the stream is on air: a network job counts as
/// live after 3 s of frames without a connection error, or as soon as the
/// fifo reports "Recovery successful".
fn watch_progress(
    stderr: std::process::ChildStderr,
    status: Arc<Mutex<DestStatus>>,
    network: bool,
) {
    std::thread::spawn(move || {
        let mut last_frame = 0u64;
        let mut first_frame: Option<Instant> = None;
        let mut last_error: Option<Instant> = None;
        for line in BufReader::new(stderr).lines().map_while(Result::ok) {
            let line = line.trim();
            match classify(line) {
                Line::Progress(k, v) => {
                    let mut s = status.lock().unwrap();
                    match k {
                        "frame" => {
                            let f: u64 = v.parse().unwrap_or(0);
                            if f > last_frame {
                                let first = *first_frame.get_or_insert_with(Instant::now);
                                let settled = !network
                                    || (first.elapsed() >= Duration::from_secs(3)
                                        && last_error.is_none());
                                if s.state == "connecting" && settled {
                                    go_live(&mut s);
                                }
                            }
                            last_frame = f;
                        }
                        "fps" => s.fps = v.parse().unwrap_or(0.0),
                        "bitrate" => {
                            if let Ok(k) = v.trim_end_matches("kbits/s").trim().parse() {
                                s.kbps = k;
                            }
                        }
                        "drop_frames" => s.dropped = v.parse().unwrap_or(0),
                        "speed" => s.speed = v.trim_end_matches('x').trim().parse().unwrap_or(0.0),
                        _ => {}
                    }
                }
                Line::Recovered => {
                    let mut s = status.lock().unwrap();
                    crate::diag::log("live", format!("{}: reconnected", s.name));
                    go_live(&mut s);
                    s.reconnects += 1;
                    last_error = None;
                }
                Line::Error(msg) => {
                    let mut s = status.lock().unwrap();
                    crate::diag::log("live", format!("{}: ffmpeg: {msg}", s.name));
                    s.error = Some(friendly_error(msg));
                    last_error = Some(Instant::now());
                    if network && is_connection_error(msg) && s.state == "live" {
                        s.state = "reconnecting".into();
                        s.since = None;
                    }
                }
                Line::Other => {}
            }
        }
    });
}

fn go_live(s: &mut DestStatus) {
    if s.state != "live" {
        s.state = "live".into();
        s.error = None;
        s.since.get_or_insert_with(Instant::now);
        crate::diag::log("live", format!("{}: on air", s.name));
    }
}

#[derive(Debug, PartialEq)]
enum Line<'a> {
    Progress(&'a str, &'a str),
    Recovered,
    Error(&'a str),
    Other,
}

/// Sorts one stderr line (FFmpeg runs with `-loglevel level+info`).
fn classify(line: &str) -> Line<'_> {
    if line.is_empty() {
        return Line::Other;
    }
    for tag in ["[error] ", "[fatal] ", "[panic] "] {
        if let Some(i) = line.find(tag) {
            return Line::Error(line[i + tag.len()..].trim());
        }
    }
    if line.contains("Recovery successful") {
        return Line::Recovered;
    }
    if ["[info]", "[warning]", "[verbose]", "[debug]"]
        .iter()
        .any(|t| line.contains(t))
    {
        return Line::Other;
    }
    match line.split_once('=') {
        Some((k, v)) if !k.contains(' ') => Line::Progress(k, v.trim()),
        // Untagged text: treat as an error message (older behaviour).
        _ => Line::Error(line),
    }
}

/// Errors that mean the connection to the platform is down.
fn is_connection_error(msg: &str) -> bool {
    let l = msg.to_lowercase();
    [
        "error opening",
        "error writing",
        "error number -138",
        "error number -10053",
        "error number -10054",
        "i/o error",
        "connection",
        "handshake",
        "broken pipe",
        "timed out",
    ]
    .iter()
    .any(|k| l.contains(k))
}

/// FFmpeg hwaccel flags and a filter graph turning the phone picture into a
/// steady 60 fps `w`×`h` frame labelled `[v]`, never stretched.
///
/// On NVIDIA everything stays on the GPU (NVDEC → CUDA → NVENC). The blurred
/// background is made from a tiny 384×216 copy: download, blur and upload
/// of a frame that small costs well under 1 ms. Measured on an RTX 4050 with a
/// real 2376×1080 phone recording: 1440p blur at 2.2× real time.
///
/// `overlay_cuda` hands NVENC frames padded to a multiple of 16 lines, so
/// 1080p went out as 1920×1088 (and 720p as 736): platforms reject or
/// rescale that. A final `scale_cuda=format=yuv420p` gives exact-size
/// frames (checked by the `gpu_framing_sizes` test).
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
                     [bg][fg]overlay_cuda=x=(main_w-overlay_w)/2:y=(main_h-overlay_h)/2,\
                     scale_cuda=format=yuv420p,fps=60[v]"
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
        || l.contains("error number -10054")
        || l.contains("error number -138")
        || l.contains("handshake")
        || l.contains("cannot open connection")
    {
        "Connection lost — reconnecting by itself. Check your internet if it lasts.".into()
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
    fn stderr_lines_are_classified() {
        assert_eq!(classify("frame=120"), Line::Progress("frame", "120"));
        assert_eq!(classify("bitrate=N/A"), Line::Progress("bitrate", "N/A"));
        assert_eq!(
            classify("[fifo @ 000001] [info] Recovery successful"),
            Line::Recovered
        );
        assert_eq!(
            classify("[fifo @ 0001] [error] Error opening rtmp://x: Error number -138 occurred"),
            Line::Error("Error opening rtmp://x: Error number -138 occurred")
        );
        assert_eq!(classify("[info]   Stream #0:0: Video: h264"), Line::Other);
        assert_eq!(
            classify("[fifo @ 01] [warning] FIFO queue full"),
            Line::Other
        );
        assert_eq!(
            classify("[fifo @ 01] [info] Recovery failed: x"),
            Line::Other
        );
        assert_eq!(
            classify("Some untagged failure"),
            Line::Error("Some untagged failure")
        );
        assert!(is_connection_error(
            "Error opening rtmp://x: Error number -138 occurred"
        ));
        assert!(is_connection_error("Cannot read RTMP handshake response"));
        assert!(!is_connection_error("Unknown encoder 'h264_nvenc'"));
        assert!(friendly_error("Error number -138 occurred").contains("reconnecting"));
    }

    #[test]
    fn encoder_args_follow_the_preset() {
        let yt = Platform::Youtube.preset("yt-1440p60-hevc");
        let a = encoder_args("hevc_nvenc", yt).join(" ");
        for want in [
            "-rc cbr",
            "-multipass qres",
            "-b_ref_mode middle",
            "-no-scenecut 1",
            "-forced-idr 1",
            "-rc-lookahead 8",
            "-bf 2",
            "-profile:v main",
            "-b:v 24000k",
            "-maxrate 24000k",
            "-g 120",
            "-keyint_min 120",
            "-flags:v +global_header",
        ] {
            assert!(a.contains(want), "missing {want} in {a}");
        }
        let fb = Platform::Facebook.preset("fb-1080p60");
        let a = encoder_args("h264_nvenc", fb).join(" ");
        assert!(a.contains("-profile:v high") && a.contains("-level:v 4.2"));
        let kick = Platform::Kick.preset("kick-1080p60");
        assert!(encoder_args("h264_nvenc", kick)
            .join(" ")
            .contains("-profile:v main"));
        let au = audio_args(yt).join(" ");
        assert!(au.contains("-b:a 128k") && au.contains("-ar 44100") && au.contains("-ac 2"));
        let out = fifo_output_args("flv", "rtmps://x/app/key").join(" ");
        for want in [
            "-f fifo",
            "-fifo_format flv",
            "flvflags=no_duration_filesize:rw_timeout=6000000",
            "-attempt_recovery 1",
            "-recover_any_error 1",
            "-max_recovery_attempts 0",
            "-restart_with_keyframe 1",
            "-drop_pkts_on_overflow 1",
            "-queue_size 120",
        ] {
            assert!(out.contains(want), "missing {want}");
        }
        assert!(out.ends_with("rtmps://x/app/key"));
    }

    #[test]
    fn legacy_settings_migrate() {
        let old = r#"{"youtube":{"enabled":true,"quality":"1440p60","key_enc":"abcd"},
                      "twitch":{"enabled":false,"quality":"720p60","key_enc":"ef"},
                      "framing":"fit"}"#;
        let mut s: LiveSettings = serde_json::from_str(old).unwrap();
        s.migrate();
        assert_eq!(s.destinations.len(), 2);
        assert_eq!(s.destinations[0].preset, "yt-1440p60-hevc");
        assert!(s.destinations[0].main && s.destinations[0].enabled);
        assert_eq!(s.destinations[1].preset, "tw-720p60");
        assert!(!s.destinations[1].enabled);
        assert_eq!(s.framing, "fit");
        let saved = serde_json::to_string(&s).unwrap();
        assert!(!saved.contains("\"youtube\":{") && saved.contains("destinations"));
    }

    #[test]
    fn ui_edits_apply_without_losing_keys() {
        let mut s = LiveSettings::default();
        s.destinations.push(DestSettings {
            id: "yt".into(),
            platform: "youtube".into(),
            key_enc: "secret".into(),
            ..Default::default()
        });
        let edit = json!({ "destinations": [
            { "id": "yt", "platform": "youtube", "enabled": true, "main": true, "preset": "yt-1080p60-h264" },
            { "platform": "kick", "enabled": true, "main": true, "url": "rtmps://k/app", "key": "kk" },
            { "platform": "nope" }
        ]});
        s.apply(&edit, false).unwrap();
        assert_eq!(s.destinations.len(), 2);
        assert_eq!(s.destinations[0].key_enc, "secret");
        assert!(s.destinations[0].main);
        assert!(!s.destinations[1].main, "only one main");
        assert!(!s.destinations[1].id.is_empty());
        assert_eq!(s.destinations[1].preset, "kick-1080p60");
        let inputs = s.inputs();
        assert_eq!(inputs.len(), 2);
        assert_eq!(inputs[0].url, "rtmps://a.rtmps.youtube.com:443/live2");
        assert!(inputs[1].has_key);
    }

    /// Every platform preset through the real argument builder (encoder +
    /// audio + fifo output) into a local file, checked with ffprobe:
    /// codec, size, fps, keyframe interval, B-frames, profile/level, audio.
    /// `cargo test preset_outputs -- --ignored --nocapture`
    #[test]
    #[ignore]
    fn preset_outputs() {
        let ffmpeg = find_ffmpeg().expect("ffmpeg");
        let ffprobe = ffmpeg.with_file_name("ffprobe.exe");
        let enc = probe_encoders(&ffmpeg);
        println!("encoders: {} / {}", enc.h264, enc.hevc);
        let dir = std::env::temp_dir().join("phonecast-preset-test");
        std::fs::create_dir_all(&dir).unwrap();
        let probe = |file: &std::path::Path, args: &[&str]| -> String {
            let out = Command::new(&ffprobe)
                .args(["-v", "error"])
                .args(args)
                .arg(file)
                .output()
                .unwrap();
            String::from_utf8_lossy(&out.stdout).trim().to_string()
        };
        let field = |text: &str, key: &str| -> String {
            text.lines()
                .find_map(|l| l.strip_prefix(&format!("{key}=")))
                .unwrap_or("")
                .to_string()
        };
        let mut checked = 0;
        for pf in presets::PLATFORMS {
            for pr in pf.presets() {
                if pr.codec == Codec::Hevc && !enc.hevc_hw() {
                    println!("skip {} (no HEVC encoder)", pr.id);
                    continue;
                }
                let file = dir.join(format!("{}.flv", pr.id));
                std::fs::remove_file(&file).ok();
                let (_, filter) = video_filter(false, Framing::Fit, pr.w, pr.h);
                let mut a: Vec<String> = [
                    "-hide_banner",
                    "-y",
                    "-loglevel",
                    "error",
                    "-f",
                    "lavfi",
                    "-i",
                    "testsrc2=s=2376x1080:r=60",
                    "-f",
                    "lavfi",
                    "-i",
                    "sine=f=440:r=48000",
                    "-t",
                    "6.5",
                ]
                .iter()
                .map(|s| s.to_string())
                .collect();
                a.extend([
                    "-filter_complex".into(),
                    filter,
                    "-map".into(),
                    "[v]".into(),
                    "-map".into(),
                    "1:a".into(),
                ]);
                let encoder = if pr.codec == Codec::Hevc {
                    &enc.hevc
                } else {
                    &enc.h264
                };
                a.extend(encoder_args(encoder, pr));
                a.extend(audio_args(pr));
                a.extend(fifo_output_args("flv", &file.display().to_string()));
                let st = Command::new(&ffmpeg).args(&a).status().unwrap();
                assert!(st.success(), "{}: ffmpeg failed: {}", pr.id, a.join(" "));
                let v = probe(
                    &file,
                    &[
                        "-select_streams",
                        "v",
                        "-show_entries",
                        "stream=codec_name,width,height,r_frame_rate,has_b_frames,profile,level",
                        "-of",
                        "default=nw=1",
                    ],
                );
                let au = probe(
                    &file,
                    &[
                        "-select_streams",
                        "a",
                        "-show_entries",
                        "stream=codec_name,sample_rate,channels",
                        "-of",
                        "default=nw=1",
                    ],
                );
                let keys = probe(
                    &file,
                    &[
                        "-select_streams",
                        "v",
                        "-skip_frame",
                        "nokey",
                        "-show_entries",
                        "frame=pts_time",
                        "-of",
                        "csv=p=0",
                    ],
                );
                let kt: Vec<f64> = keys.lines().filter_map(|l| l.trim().parse().ok()).collect();
                let gaps: Vec<f64> = kt.windows(2).map(|w| w[1] - w[0]).collect();
                println!(
                    "{:<18} {} | {} | keyframe gaps {:?}",
                    pr.id,
                    v.replace('\n', " "),
                    au.replace('\n', " "),
                    gaps
                );
                let want_codec = if pr.codec == Codec::Hevc {
                    "hevc"
                } else {
                    "h264"
                };
                assert_eq!(field(&v, "codec_name"), want_codec, "{}", pr.id);
                assert_eq!(field(&v, "width"), pr.w.to_string(), "{}", pr.id);
                assert_eq!(field(&v, "height"), pr.h.to_string(), "{}", pr.id);
                assert_eq!(field(&v, "r_frame_rate"), "60/1", "{}", pr.id);
                let bf: u8 = field(&v, "has_b_frames").parse().unwrap_or(9);
                assert!(bf <= pr.max_bframes, "{}: {bf} B-frames", pr.id);
                if pr.codec == Codec::H264 {
                    let want = if pr.profile == "main" { "Main" } else { "High" };
                    assert_eq!(field(&v, "profile"), want, "{}", pr.id);
                }
                if pr.level == Some("4.2") {
                    assert_eq!(field(&v, "level"), "42", "{}", pr.id);
                }
                assert!(kt.len() >= 3, "{}: keyframes {kt:?}", pr.id);
                assert!(
                    gaps.iter().all(|g| (g - pr.gop_s as f64).abs() < 0.05),
                    "{}: GOP {gaps:?}",
                    pr.id
                );
                assert_eq!(field(&au, "codec_name"), "aac", "{}", pr.id);
                assert_eq!(field(&au, "channels"), "2", "{}", pr.id);
                assert_eq!(
                    field(&au, "sample_rate"),
                    pr.audio.rate.to_string(),
                    "{}",
                    pr.id
                );
                checked += 1;
                std::fs::remove_file(&file).ok();
            }
        }
        println!("{checked} presets verified");
        assert!(checked >= 14);
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
            preset: None,
            container: "flv",
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

    /// Splits an Annex-B H.264 file into (csd, access units).
    fn load_sample(path: &str) -> (Vec<u8>, Vec<(bool, Vec<u8>)>) {
        let data = std::fs::read(path).unwrap();
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
                    if let Some(au) = aus.last_mut() {
                        au.0 |= t == 5;
                        au.1.extend_from_slice(&[0, 0, 0, 1]);
                        au.1.extend_from_slice(n);
                    }
                }
            }
        }
        aus.retain(|a| !a.1.is_empty());
        (csd, aus)
    }

    /// The fifo muxer keeps one FFmpeg running through a network outage:
    /// a local RTMP server (FFmpeg -listen) is killed and restarted while a
    /// real Job streams the phone sample to it. Status must go live →
    /// reconnecting → live, and the second server must receive video.
    /// `PHONECAST_SAMPLE=<file.h264> cargo test fifo_reconnect -- --ignored --nocapture`
    #[test]
    #[ignore]
    fn fifo_reconnect() {
        let ffmpeg = find_ffmpeg().expect("ffmpeg");
        let enc = probe_encoders(&ffmpeg);
        let (csd, aus) = load_sample(&std::env::var("PHONECAST_SAMPLE").unwrap());
        let url = "rtmp://127.0.0.1:19361/live/test";
        let dir = std::env::temp_dir();
        let listen = |out: &str| {
            Command::new(&ffmpeg)
                .args([
                    "-hide_banner",
                    "-loglevel",
                    "error",
                    "-y",
                    "-listen",
                    "1",
                    "-i",
                    url,
                ])
                .args(["-c", "copy"])
                .arg(dir.join(out))
                .stdin(Stdio::null())
                .spawn()
                .unwrap()
        };
        let mut l1 = listen("phonecast_rtmp_1.flv");
        std::thread::sleep(Duration::from_millis(1500));
        let preset = *Platform::Twitch.preset("tw-720p60");
        let job = Job {
            ffmpeg: ffmpeg.clone(),
            url: url.into(),
            encoder: enc.h264.clone(),
            w: preset.w,
            h: preset.h,
            kbps: preset.kbps,
            hevc: false,
            framing: Framing::Blur,
            pc: None,
            file: false,
            preset: Some(preset),
            container: "flv",
        };
        let cfg = VideoConfig {
            codec: crate::mkv::CODEC_H264,
            width: 2376,
            height: 1080,
            csd,
        };
        let dest = spawn_job(job, cfg, Arc::new(|| {}), "Test");
        let Dest { mut tx, status, .. } = dest;
        let start = Instant::now();
        let mut audio_t = 0i64;
        let mut states: Vec<(f64, String)> = Vec::new();
        let mut l2 = None;
        for (i, (key, au)) in aus.iter().cycle().take(60 * 18).enumerate() {
            let pts = 1_000_000 + i as i64 * 16_667;
            tx.push(&Media::Video {
                pts,
                key: *key && i < aus.len(),
                data: Arc::new(au.clone()),
            });
            while audio_t < pts + 16_667 {
                tx.push(&Media::Audio {
                    pts: audio_t.max(1_000_000),
                    pcm: Arc::new(vec![0u8; 1920]),
                });
                audio_t += 10_000;
            }
            let t = start.elapsed().as_secs_f64();
            if i == 60 * 6 {
                l1.kill().ok();
                l1.wait().ok();
                println!("{t:5.1}s server killed");
            }
            if i == 60 * 10 {
                l2 = Some(listen("phonecast_rtmp_2.flv"));
                println!("{t:5.1}s server restarted");
            }
            let st = status.lock().unwrap().state.clone();
            if states.last().map(|s| &s.1) != Some(&st) {
                println!("{t:5.1}s state {st}");
                states.push((t, st));
            }
            let due = Duration::from_micros((i as u64 + 1) * 16_667);
            if let Some(w) = due.checked_sub(start.elapsed()) {
                std::thread::sleep(w);
            }
        }
        let s = status.lock().unwrap().clone();
        println!(
            "final: {} reconnects={} err={:?}",
            s.state, s.reconnects, s.error
        );
        drop(tx);
        std::thread::sleep(Duration::from_secs(2));
        if let Some(mut l) = l2 {
            l.kill().ok();
            l.wait().ok();
        }
        let names: Vec<&str> = states.iter().map(|s| s.1.as_str()).collect();
        assert!(names.starts_with(&["connecting", "live"]), "{names:?}");
        assert!(names.contains(&"reconnecting"), "{names:?}");
        assert_eq!(names.last(), Some(&"live"), "{names:?}");
        assert!(s.reconnects >= 1);
        let second = dir.join("phonecast_rtmp_2.flv");
        let out = Command::new(ffmpeg.with_file_name("ffprobe.exe"))
            .args(["-v", "error", "-select_streams", "v", "-count_packets"])
            .args([
                "-show_entries",
                "stream=codec_name,width,height,nb_read_packets",
            ])
            .args(["-of", "csv=p=0"])
            .arg(&second)
            .output()
            .unwrap();
        let probe = String::from_utf8_lossy(&out.stdout).trim().to_string();
        println!("second server got: {probe}");
        assert!(probe.starts_with("h264,1280,720"), "{probe}");
    }

    /// The GPU filter graph must produce exactly the preset size for every
    /// framing (overlay_cuda used to pad 1080 → 1088).
    /// `PHONECAST_SAMPLE=<file.h264> cargo test gpu_framing_sizes -- --ignored --nocapture`
    #[test]
    #[ignore]
    fn gpu_framing_sizes() {
        let ffmpeg = find_ffmpeg().expect("ffmpeg");
        let sample = std::env::var("PHONECAST_SAMPLE").unwrap();
        let out = std::env::temp_dir().join("phonecast_framing.flv");
        let mut bad = Vec::new();
        for (w, h) in [
            (1280, 720),
            (1664, 936),
            (1920, 1080),
            (2560, 1440),
            (3840, 2160),
        ] {
            for framing in [Framing::Blur, Framing::Fit, Framing::Fill] {
                let (hw, filter) = video_filter(true, framing, w, h);
                let st = Command::new(&ffmpeg)
                    .args(["-hide_banner", "-loglevel", "error", "-y"])
                    .args(hw)
                    .args(["-i", &sample, "-t", "1", "-filter_complex", &filter])
                    .args(["-map", "[v]", "-c:v", "h264_nvenc", "-f", "flv"])
                    .arg(&out)
                    .status()
                    .unwrap();
                assert!(st.success(), "{framing:?} {w}x{h}");
                let o = Command::new(ffmpeg.with_file_name("ffprobe.exe"))
                    .args(["-v", "error", "-select_streams", "v"])
                    .args(["-show_entries", "stream=width,height", "-of", "csv=p=0"])
                    .arg(&out)
                    .output()
                    .unwrap();
                let got = String::from_utf8_lossy(&o.stdout).trim().to_string();
                println!("{framing:?} {w}x{h} -> {got}");
                if got != format!("{w},{h}") {
                    bad.push(format!("{framing:?} {w}x{h} -> {got}"));
                }
            }
        }
        assert!(bad.is_empty(), "{bad:?}");
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
                preset: None,
                container: "flv",
            };
            let cfg = VideoConfig {
                codec: crate::mkv::CODEC_H264,
                width: 1280,
                height: 720,
                csd: csd.clone(),
            };
            let status = Arc::new(Mutex::new(DestStatus {
                id: "test".into(),
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
