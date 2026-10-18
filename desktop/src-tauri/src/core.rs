//! Receives the phone stream (USB or Wi-Fi), measures latency, and feeds the
//! outputs: live preview, recorder and live streams. See PROTOCOL.md.
//!
//! Threads: the network reader only reads (so the phone never has to drop
//! frames because the PC was busy); a processing thread mixes audio and fans
//! out to the outputs, each of which runs on its own thread.

use crate::adb::{self, AdbState};
use crate::audio::{self, Mixer};
use crate::capture::{PcVideo, ScreenPreview, Source, SourceSettings};
use crate::diag;
use crate::live::{self, Live};
use crate::mkv::VideoConfig;
use crate::sinks::{self, Media, Push, RecorderInfo, SinkTx};
use serde_json::{json, Value};
use std::collections::VecDeque;
use std::io::{BufReader, Read, Write};
use std::net::{Shutdown, TcpListener, TcpStream};
use std::path::PathBuf;
use std::sync::mpsc::{sync_channel, SyncSender, TrySendError};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};
use tauri::{AppHandle, Emitter};

pub const PHONE_PORT: u16 = 27183;
const PREVIEW_PORT: u16 = 27184;

// phone -> PC
const VIDEO_CONFIG: u8 = 0x01;
const VIDEO_FRAME: u8 = 0x02;
const AUDIO_CONFIG: u8 = 0x03;
const AUDIO_PCM: u8 = 0x04;
const STATS: u8 = 0x05;
const PONG: u8 = 0x06;
// PC -> phone
const SET_BITRATE: u8 = 0x81;
const REQUEST_KEYFRAME: u8 = 0x82;
const PING: u8 = 0x83;

const FLAG_KEYFRAME: u8 = 0x01;

struct Recording {
    tx: SinkTx,
    info: Arc<Mutex<RecorderInfo>>,
    started: Instant,
    skipped: u32,
}

/// PC-mode recording: FFmpeg captures the screen itself.
struct PcRecord {
    dest: live::Dest,
    path: PathBuf,
    started: Instant,
}

#[derive(Default)]
struct Inner {
    connected: bool,
    remote: String,
    device: String,
    /// Video config of the active source (phone, or the PC screen preview).
    video: Option<VideoConfig>,
    /// Last config the phone sent, kept while the PC screen is the source.
    phone_video: Option<VideoConfig>,
    audio: Option<(u32, u8)>,
    phone_stats: Value,
    // per-interval counters
    frames: u64,
    bytes: u64,
    latency_sum_us: i64,
    latency_n: i64,
    // last computed rates
    fps: u64,
    mbps: f64,
    latency_ms: Option<f64>,
    // clock sync: (rtt, offset) samples, offset = phone - pc
    clock: VecDeque<(i64, i64)>,
    recording: Option<Recording>,
    record_error: Option<String>,
}

impl Inner {
    fn clock_offset(&self) -> Option<(i64, i64)> {
        self.clock
            .iter()
            .min_by_key(|(rtt, _)| *rtt)
            .map(|&(rtt, off)| (off, rtt))
    }
}

struct PreviewClient {
    tx: SyncSender<Arc<Vec<u8>>>,
    need_keyframe: bool,
}

pub struct Core {
    start: Instant,
    inner: Mutex<Inner>,
    phone: Mutex<Option<TcpStream>>,
    preview: Mutex<Vec<PreviewClient>>,
    adb: Arc<Mutex<AdbState>>,
    pub mixer: Arc<Mixer>,
    pub live: Arc<Live>,
    pub preview_port: Mutex<u16>,
    source: Mutex<SourceSettings>,
    screen_preview: Mutex<Option<ScreenPreview>>,
    pc_record: Mutex<Option<PcRecord>>,
    sources_cache: Mutex<Value>,
}

impl Core {
    pub fn start(app: AppHandle) -> std::io::Result<Arc<Core>> {
        let core = Arc::new(Core {
            start: Instant::now(),
            inner: Mutex::new(Inner::default()),
            phone: Mutex::new(None),
            preview: Mutex::new(Vec::new()),
            adb: Arc::new(Mutex::new(AdbState::default())),
            mixer: Mixer::start(),
            live: Live::new(),
            preview_port: Mutex::new(0),
            source: Mutex::new(SourceSettings::load()),
            screen_preview: Mutex::new(None),
            pc_record: Mutex::new(None),
            sources_cache: Mutex::new(Value::Null),
        });

        // Listen on all interfaces: 127.0.0.1 for USB (adb reverse), LAN for Wi-Fi.
        let listener = TcpListener::bind(("0.0.0.0", PHONE_PORT))?;
        let c = core.clone();
        std::thread::Builder::new()
            .name("phone-listener".into())
            .spawn(move || {
                for stream in listener.incoming().flatten() {
                    let c2 = c.clone();
                    std::thread::Builder::new()
                        .name("phone".into())
                        .spawn(move || c2.handle_phone(stream))
                        .ok();
                }
            })?;

        let preview = TcpListener::bind(("127.0.0.1", PREVIEW_PORT))
            .or_else(|_| TcpListener::bind(("127.0.0.1", 0)))?;
        *core.preview_port.lock().unwrap() = preview.local_addr()?.port();
        let c = core.clone();
        std::thread::Builder::new()
            .name("preview-listener".into())
            .spawn(move || {
                for stream in preview.incoming().flatten() {
                    let c2 = c.clone();
                    std::thread::Builder::new()
                        .name("preview".into())
                        .spawn(move || c2.serve_preview(stream))
                        .ok();
                }
            })?;

        adb::spawn(PHONE_PORT, core.adb.clone());

        let c = core.clone();
        let app2 = app.clone();
        std::thread::Builder::new()
            .name("ticker".into())
            .spawn(move || c.ticker(app))?;

        // Audio meters update 20×/s for smooth animation.
        let m = core.mixer.clone();
        std::thread::Builder::new()
            .name("levels".into())
            .spawn(move || loop {
                std::thread::sleep(Duration::from_millis(50));
                app2.emit("levels", m.levels_json()).ok();
            })?;

        let c = core.clone();
        std::thread::Builder::new()
            .name("mix-clock".into())
            .spawn(move || c.mix_clock())?;

        // Monitors/webcams list (spawns ffmpeg once) and the saved source.
        let c = core.clone();
        std::thread::Builder::new()
            .name("sources".into())
            .spawn(move || {
                c.refresh_sources();
                c.apply_source();
            })?;
        Ok(core)
    }

    // ---------------- source: phone or PC screen ----------------

    pub fn refresh_sources(&self) -> Value {
        let v = crate::capture::sources_json(self.live.ffmpeg().as_ref());
        *self.sources_cache.lock().unwrap() = v.clone();
        v
    }

    pub fn set_source(self: &Arc<Self>, s: SourceSettings) -> Result<(), String> {
        if self.live.is_live()
            || self.pc_record.lock().unwrap().is_some()
            || self.inner.lock().unwrap().recording.is_some()
        {
            return Err("Stop the stream and recording before changing the source".into());
        }
        s.save();
        *self.source.lock().unwrap() = s;
        self.apply_source();
        Ok(())
    }

    fn is_pc_mode(&self) -> bool {
        matches!(self.source.lock().unwrap().source, Source::Screen { .. })
    }

    /// The PC screen + webcam description for FFmpeg, if the source is a screen.
    fn pc_video(&self) -> Option<Arc<PcVideo>> {
        let s = self.source.lock().unwrap();
        let Source::Screen { output } = s.source else {
            return None;
        };
        let monitors = crate::capture::monitors();
        let m = monitors
            .iter()
            .find(|m| m.output == output)
            .or(monitors.first())?;
        Some(Arc::new(PcVideo {
            output: m.output,
            webcam: Some(s.webcam.clone()),
            screen_w: m.width,
            screen_h: m.height,
        }))
    }

    fn apply_source(self: &Arc<Self>) {
        // Stop any running screen preview first.
        *self.screen_preview.lock().unwrap() = None;
        match self.pc_video() {
            Some(pc) => {
                diag::log(
                    "source",
                    format!(
                        "PC screen {} ({}x{}), webcam {}",
                        pc.output,
                        pc.screen_w,
                        pc.screen_h,
                        pc.webcam.as_ref().is_some_and(|w| w.enabled)
                    ),
                );
                self.mixer.set_pc_audio(true);
                let Some(ff) = self.live.ffmpeg() else {
                    diag::log("source", "FFmpeg not installed: no PC preview");
                    return;
                };
                let weak_a = Arc::downgrade(self);
                let weak_b = Arc::downgrade(self);
                let preview = ScreenPreview::start(
                    ff,
                    PcVideo {
                        output: pc.output,
                        webcam: pc.webcam.clone(),
                        screen_w: pc.screen_w,
                        screen_h: pc.screen_h,
                    },
                    Arc::new(move |cfg: VideoConfig| {
                        if let Some(c) = weak_a.upgrade() {
                            c.inner.lock().unwrap().video = Some(cfg.clone());
                            c.broadcast_config(&cfg);
                            c.dispatch(Media::Config(cfg));
                        }
                    }),
                    Arc::new(move |key: bool, pts: i64, data: Vec<u8>| {
                        if let Some(c) = weak_b.upgrade() {
                            {
                                let mut i = c.inner.lock().unwrap();
                                i.frames += 1;
                                i.bytes += data.len() as u64;
                            }
                            c.broadcast(
                                VIDEO_FRAME,
                                if key { FLAG_KEYFRAME } else { 0 },
                                pts,
                                &data,
                            );
                            c.dispatch(Media::Video {
                                pts,
                                key,
                                data: Arc::new(data),
                            });
                        }
                    }),
                );
                *self.screen_preview.lock().unwrap() = Some(preview);
            }
            None => {
                diag::log("source", "phone");
                self.mixer.set_pc_audio(false);
                let mut i = self.inner.lock().unwrap();
                i.video = i.phone_video.clone();
                if let Some(cfg) = i.video.clone() {
                    drop(i);
                    self.broadcast_config(&cfg);
                    self.request_keyframe();
                }
            }
        }
    }

    fn broadcast_config(&self, cfg: &VideoConfig) {
        let mut payload = vec![cfg.codec];
        payload.extend_from_slice(&cfg.width.to_be_bytes());
        payload.extend_from_slice(&cfg.height.to_be_bytes());
        payload.extend_from_slice(&cfg.csd);
        self.broadcast(VIDEO_CONFIG, 0, 0, &payload);
    }

    fn now_us(&self) -> i64 {
        self.start.elapsed().as_micros() as i64
    }

    fn has_outputs(&self) -> bool {
        self.inner.lock().unwrap().recording.is_some()
            || self.pc_record.lock().unwrap().is_some()
            || self.live.is_live()
    }

    /// Keeps Discord + mic flowing to the outputs even when the phone sends no
    /// game audio: produces 10 ms mixes on the phone's clock.
    fn mix_clock(&self) {
        let mut next = Instant::now();
        loop {
            next += Duration::from_millis(10);
            match next.checked_duration_since(Instant::now()) {
                Some(wait) => std::thread::sleep(wait),
                None => next = Instant::now(),
            }
            let pc = self.is_pc_mode();
            let offset = {
                let i = self.inner.lock().unwrap();
                if !i.connected && !pc {
                    continue;
                }
                // PC mode without a phone runs on the PC clock.
                i.clock_offset()
                    .map(|(o, _)| o)
                    .or(if pc { Some(0) } else { None })
            };
            if !self.has_outputs() {
                continue;
            }
            let (Some(offset), Some(pcm)) = (offset, self.mixer.mix_without_game()) else {
                continue;
            };
            let pts = self.now_us() + offset - 40_000; // match the PC sources' buffer
            self.dispatch(Media::Audio {
                pts,
                pcm: Arc::new(pcm),
            });
        }
    }

    // ---------------- phone connection ----------------

    fn handle_phone(self: Arc<Self>, stream: TcpStream) {
        let remote = stream
            .peer_addr()
            .map(|a| a.ip().to_string())
            .unwrap_or_default();
        stream.set_nodelay(true).ok();
        // Windows' default receive buffer is tiny; a big one absorbs keyframe
        // bursts so the phone never sees "congestion" and drops frames.
        socket2::SockRef::from(&stream)
            .set_recv_buffer_size(8 << 20)
            .ok();
        let mut reader = BufReader::with_capacity(
            1 << 20,
            match stream.try_clone() {
                Ok(s) => s,
                Err(_) => return,
            },
        );

        // Handshake
        let mut magic = [0u8; 7];
        if reader.read_exact(&mut magic).is_err() || &magic[..4] != b"PCST" {
            return;
        }
        let name_len = u16::from_be_bytes([magic[5], magic[6]]) as usize;
        let mut name = vec![0u8; name_len];
        if reader.read_exact(&mut name).is_err() {
            return;
        }

        // Newest phone wins: close any previous connection.
        if let Some(old) = self.phone.lock().unwrap().replace(stream) {
            old.shutdown(Shutdown::Both).ok();
        }
        {
            let mut i = self.inner.lock().unwrap();
            i.connected = true;
            i.device = String::from_utf8_lossy(&name).into_owned();
            i.remote = if remote == "127.0.0.1" {
                "USB".into()
            } else {
                format!("Wi-Fi ({remote})")
            };
            i.clock.clear();
            diag::log("phone", format!("connected: {} via {}", i.device, i.remote));
        }

        // Processing happens on its own thread; this one only reads the socket.
        let (tx, rx) = sync_channel::<(u8, u8, i64, Vec<u8>, i64)>(4096);
        let c = self.clone();
        let proc = std::thread::Builder::new()
            .name("phone-proc".into())
            .spawn(move || {
                while let Ok((ty, flags, pts, payload, arrival)) = rx.recv() {
                    c.on_packet(ty, flags, pts, payload, arrival);
                }
            })
            .ok();

        let mut header = [0u8; 14];
        loop {
            if reader.read_exact(&mut header).is_err() {
                break;
            }
            let ty = header[0];
            let flags = header[1];
            let pts = i64::from_be_bytes(header[2..10].try_into().unwrap());
            let len = u32::from_be_bytes(header[10..14].try_into().unwrap()) as usize;
            if len > 64 << 20 {
                break; // corrupt stream
            }
            let mut payload = vec![0u8; len];
            if reader.read_exact(&mut payload).is_err() {
                break;
            }
            if tx.send((ty, flags, pts, payload, self.now_us())).is_err() {
                break;
            }
        }
        drop(tx);
        if let Some(p) = proc {
            p.join().ok();
        }

        // Only clear state if this connection is still the active one.
        let mut phone = self.phone.lock().unwrap();
        let still_active = phone
            .as_ref()
            .and_then(|s| s.peer_addr().ok())
            .is_none_or(|a| a.ip().to_string() == remote);
        if still_active {
            *phone = None;
            drop(phone);
            diag::log("phone", "disconnected");
            let mut i = self.inner.lock().unwrap();
            i.connected = false;
            i.latency_ms = None;
            i.fps = 0;
            i.mbps = 0.0;
        }
    }

    fn on_packet(&self, ty: u8, flags: u8, pts: i64, payload: Vec<u8>, arrival: i64) {
        match ty {
            VIDEO_CONFIG if payload.len() > 5 => {
                let cfg = VideoConfig {
                    codec: payload[0],
                    width: u16::from_be_bytes([payload[1], payload[2]]),
                    height: u16::from_be_bytes([payload[3], payload[4]]),
                    csd: payload[5..].to_vec(),
                };
                {
                    let mut i = self.inner.lock().unwrap();
                    if i.phone_video.as_ref() != Some(&cfg) {
                        diag::log(
                            "video",
                            format!(
                                "phone stream config: {}x{} codec {}",
                                cfg.width, cfg.height, cfg.codec
                            ),
                        );
                    }
                    i.phone_video = Some(cfg.clone());
                    if self.is_pc_mode() {
                        return; // PC screen is the source; keep the phone's config for later
                    }
                    i.video = Some(cfg.clone());
                }
                self.dispatch(Media::Config(cfg));
                self.broadcast(ty, flags, pts, &payload);
            }
            VIDEO_FRAME => {
                if self.is_pc_mode() {
                    return;
                }
                let key = flags & FLAG_KEYFRAME != 0;
                {
                    let mut i = self.inner.lock().unwrap();
                    i.frames += 1;
                    i.bytes += payload.len() as u64;
                    if let Some((offset, _)) = i.clock_offset() {
                        let lat = arrival - (pts - offset);
                        if (0..5_000_000).contains(&lat) {
                            i.latency_sum_us += lat;
                            i.latency_n += 1;
                        }
                    }
                }
                self.broadcast(ty, flags, pts, &payload);
                self.dispatch(Media::Video {
                    pts,
                    key,
                    data: Arc::new(payload),
                });
            }
            AUDIO_CONFIG if payload.len() >= 6 => {
                let rate = u32::from_be_bytes(payload[0..4].try_into().unwrap());
                self.inner.lock().unwrap().audio = Some((rate, payload[4]));
            }
            AUDIO_PCM => {
                if self.is_pc_mode() {
                    return; // PC sound replaces the phone's game audio
                }
                // Game + Discord + mic, mixed on the game audio's timeline.
                let mixed = self.mixer.mix_game(&payload);
                self.dispatch(Media::Audio {
                    pts,
                    pcm: Arc::new(mixed),
                });
            }
            STATS => {
                if let Ok(v) = serde_json::from_slice::<Value>(&payload) {
                    let mut i = self.inner.lock().unwrap();
                    // Phone-side trouble is the usual cause of "it got stuck".
                    let prev = i
                        .phone_stats
                        .get("dropped")
                        .and_then(Value::as_u64)
                        .unwrap_or(0);
                    let now = v.get("dropped").and_then(Value::as_u64).unwrap_or(0);
                    if now > prev {
                        diag::log(
                            "phone",
                            format!("phone dropped {} frames (total {now})", now - prev),
                        );
                    }
                    let fps = v.get("fps").and_then(Value::as_u64).unwrap_or(60);
                    if i.connected && fps < 50 {
                        diag::log(
                            "phone",
                            format!(
                                "phone encoder only {fps} fps (thermal {})",
                                v.get("thermal").and_then(Value::as_u64).unwrap_or(0)
                            ),
                        );
                    }
                    if v.get("thermal").and_then(Value::as_u64).unwrap_or(0) >= 2 {
                        diag::log(
                            "phone",
                            format!("phone is hot: thermal level {}", v["thermal"]),
                        );
                    }
                    i.phone_stats = v;
                }
            }
            PONG if payload.len() >= 16 => {
                let t0 = i64::from_be_bytes(payload[0..8].try_into().unwrap());
                let phone = i64::from_be_bytes(payload[8..16].try_into().unwrap());
                let rtt = arrival - t0;
                let offset = phone - (t0 + arrival) / 2;
                let mut i = self.inner.lock().unwrap();
                i.clock.push_back((rtt, offset));
                if i.clock.len() > 20 {
                    i.clock.pop_front();
                }
            }
            _ => {}
        }
    }

    /// Hands media to the recorder and live streams (never blocks).
    fn dispatch(&self, m: Media) {
        let mut want_key = false;
        {
            let mut i = self.inner.lock().unwrap();
            if let Some(r) = i.recording.as_mut() {
                if let Push::NeedKeyframe = r.tx.push(&m) {
                    r.skipped += 1;
                    want_key = true;
                    diag::log(
                        "record",
                        "recorder fell behind (disk slow?) - skipping to next keyframe",
                    );
                }
            }
        }
        if self.live.push(&m) {
            want_key = true;
        }
        if want_key {
            self.request_keyframe_limited("recorder or live stream fell behind");
        }
    }

    pub fn send_phone(&self, ty: u8, payload: &[u8]) -> bool {
        let mut msg = Vec::with_capacity(14 + payload.len());
        msg.push(ty);
        msg.push(0);
        msg.extend_from_slice(&0i64.to_be_bytes());
        msg.extend_from_slice(&(payload.len() as u32).to_be_bytes());
        msg.extend_from_slice(payload);
        let mut phone = self.phone.lock().unwrap();
        match phone.as_mut() {
            Some(s) => s.write_all(&msg).is_ok(),
            None => false,
        }
    }

    pub fn set_bitrate(&self, bps: u32) -> bool {
        self.send_phone(SET_BITRATE, &bps.to_be_bytes())
    }

    pub fn request_keyframe(&self) -> bool {
        self.send_phone(REQUEST_KEYFRAME, &[])
    }

    /// Keyframe request for "an output fell behind": at most one per second.
    /// Without the limit a slow preview asked for a keyframe on every frame,
    /// and the flood of 1–2 MB keyframes made the stream lag badly.
    fn request_keyframe_limited(&self, why: &str) {
        static LAST: Mutex<Option<Instant>> = Mutex::new(None);
        let mut last = LAST.lock().unwrap();
        if last.is_some_and(|t| t.elapsed() < Duration::from_secs(1)) {
            return;
        }
        *last = Some(Instant::now());
        diag::log("video", format!("{why} - skipping to next keyframe"));
        self.request_keyframe();
    }

    // ---------------- recording ----------------

    pub fn start_recording(self: &Arc<Self>) -> Result<String, String> {
        let dir = recordings_dir();
        std::fs::create_dir_all(&dir).map_err(|e| e.to_string())?;
        let name = chrono::Local::now()
            .format("PhoneCast_%Y-%m-%d_%H-%M-%S.mkv")
            .to_string();
        let path = dir.join(name);

        if let Some(pc) = self.pc_video() {
            // PC mode: FFmpeg captures the screen and encodes on the GPU.
            if self.pc_record.lock().unwrap().is_some() {
                return Err("Already recording".into());
            }
            let (encoder, ffmpeg) = self
                .live
                .encoders_for_file()
                .ok_or("FFmpeg is not installed on this PC")?;
            let job = live::Job {
                ffmpeg,
                url: path.display().to_string(),
                encoder,
                w: pc.screen_w,
                h: pc.screen_h,
                kbps: 0,
                hevc: false,
                framing: live::Framing::Fit,
                pc: Some(pc.clone()),
                file: true,
                preset: None,
                container: "matroska",
            };
            let cfg = VideoConfig {
                codec: crate::mkv::CODEC_H264,
                width: pc.screen_w as u16,
                height: pc.screen_h as u16,
                csd: Vec::new(),
            };
            let dest = live::spawn_job(job, cfg, Arc::new(|| {}), "Recording");
            *self.pc_record.lock().unwrap() = Some(PcRecord {
                dest,
                path: path.clone(),
                started: Instant::now(),
            });
            diag::log(
                "record",
                format!("PC recording started: {}", path.display()),
            );
            return Ok(path.display().to_string());
        }

        let mut i = self.inner.lock().unwrap();
        if i.recording.is_some() {
            return Err("Already recording".into());
        }
        let video = i.video.clone().ok_or("Phone is not streaming yet")?;
        // ~15 s of buffer so a slow disk moment never loses frames.
        let (tx, rx) = SinkTx::new(2500);
        let info = Arc::new(Mutex::new(RecorderInfo {
            path: path.clone(),
            bytes: 0,
            error: None,
        }));
        // Audio track is always the 48 kHz stereo mix (game + Discord + mic).
        sinks::spawn_recorder(
            path.clone(),
            video,
            (audio::RATE as u32, 2),
            rx,
            info.clone(),
        );
        i.recording = Some(Recording {
            tx,
            info,
            started: Instant::now(),
            skipped: 0,
        });
        i.record_error = None;
        drop(i);
        diag::log("record", format!("recording started: {}", path.display()));
        self.request_keyframe(); // the file starts on the next keyframe
        Ok(path.display().to_string())
    }

    pub fn stop_recording(&self) -> Result<String, String> {
        if let Some(r) = self.pc_record.lock().unwrap().take() {
            r.dest.stop.store(true, std::sync::atomic::Ordering::SeqCst);
            diag::log(
                "record",
                format!(
                    "PC recording stopped after {}s",
                    r.started.elapsed().as_secs()
                ),
            );
            return Ok(r.path.display().to_string());
        }
        let r = self
            .inner
            .lock()
            .unwrap()
            .recording
            .take()
            .ok_or("Not recording")?;
        let path = r.info.lock().unwrap().path.display().to_string();
        diag::log(
            "record",
            format!(
                "recording stopped after {}s, {} skips",
                r.started.elapsed().as_secs(),
                r.skipped
            ),
        );
        drop(r); // closing the queue makes the recorder finish the file
        Ok(path)
    }

    // ---------------- live ----------------

    pub fn start_live(self: &Arc<Self>) -> Result<(), String> {
        let pc = self.pc_video();
        let config = if pc.is_some() {
            None
        } else {
            self.inner.lock().unwrap().phone_video.clone()
        };
        let c = self.clone();
        self.live.start(
            config,
            pc,
            Arc::new(move || {
                c.request_keyframe();
            }),
        )
    }

    pub fn stop_live(&self) {
        self.live.stop();
    }

    // ---------------- preview (WebSocket to the UI) ----------------

    fn broadcast(&self, ty: u8, flags: u8, pts: i64, payload: &[u8]) {
        let mut clients = self.preview.lock().unwrap();
        if clients.is_empty() {
            return;
        }
        let mut msg = Vec::with_capacity(10 + payload.len());
        msg.push(ty);
        msg.push(flags);
        msg.extend_from_slice(&pts.to_be_bytes());
        msg.extend_from_slice(payload);
        let msg = Arc::new(msg);
        let is_frame = ty == VIDEO_FRAME;
        let is_key = flags & FLAG_KEYFRAME != 0;
        let mut want_key = false;
        clients.retain_mut(|c| {
            if is_frame && c.need_keyframe && !is_key {
                return true; // skip until a clean keyframe
            }
            match c.tx.try_send(msg.clone()) {
                Ok(()) => {
                    if is_key {
                        c.need_keyframe = false;
                    }
                    true
                }
                Err(TrySendError::Full(_)) => {
                    // UI fell behind: drop instead of building up delay.
                    c.need_keyframe = true;
                    want_key = true;
                    true
                }
                Err(TrySendError::Disconnected(_)) => false,
            }
        });
        drop(clients);
        if want_key {
            self.request_keyframe_limited("preview fell behind");
        }
    }

    fn serve_preview(&self, stream: TcpStream) {
        stream.set_nodelay(true).ok();
        let Ok(mut ws) = tungstenite::accept(stream) else {
            return;
        };
        // ~0.5 s of frames: a UI hiccup must not turn into dropped frames.
        let (tx, rx) = sync_channel::<Arc<Vec<u8>>>(32);

        // Current config first, then wait for a fresh keyframe.
        if let Some(cfg) = self.inner.lock().unwrap().video.clone() {
            let mut m = vec![VIDEO_CONFIG, 0];
            m.extend_from_slice(&0i64.to_be_bytes());
            m.push(cfg.codec);
            m.extend_from_slice(&cfg.width.to_be_bytes());
            m.extend_from_slice(&cfg.height.to_be_bytes());
            m.extend_from_slice(&cfg.csd);
            if ws.send(tungstenite::Message::Binary(m)).is_err() {
                return;
            }
        }
        self.preview.lock().unwrap().push(PreviewClient {
            tx,
            need_keyframe: true,
        });
        self.request_keyframe();

        while let Ok(msg) = rx.recv() {
            if ws.send(tungstenite::Message::Binary(msg.to_vec())).is_err() {
                break;
            }
        }
    }

    // ---------------- 2 Hz housekeeping ----------------

    fn ticker(&self, app: AppHandle) {
        let mut last = Instant::now();
        loop {
            std::thread::sleep(Duration::from_millis(500));
            self.send_phone(PING, &self.now_us().to_be_bytes());

            let secs = last.elapsed().as_secs_f64();
            last = Instant::now();
            let status = {
                let mut i = self.inner.lock().unwrap();
                i.fps = (i.frames as f64 / secs).round() as u64;
                i.mbps = i.bytes as f64 * 8.0 / secs / 1e6;
                i.latency_ms = (i.latency_n > 0)
                    .then(|| i.latency_sum_us as f64 / i.latency_n as f64 / 1000.0);
                i.frames = 0;
                i.bytes = 0;
                i.latency_sum_us = 0;
                i.latency_n = 0;
                self.status_json(&mut i)
            };
            app.emit("status", status).ok();
        }
    }

    fn status_json(&self, i: &mut Inner) -> Value {
        let adb = self.adb.lock().unwrap().clone();
        let pc_rec = self.pc_record.lock().unwrap().as_ref().map(|r| {
            let st = r.dest.status.lock().unwrap();
            json!({
                "path": r.path.display().to_string(),
                "seconds": r.started.elapsed().as_secs(),
                "bytes": 0,
                "skipped": 0,
                "error": if st.state == "stopped" || st.state == "error" { st.error.clone() } else { None },
            })
        });
        let recording = pc_rec.or_else(|| {
            i.recording.as_ref().map(|r| {
                let info = r.info.lock().unwrap();
                json!({
                    "path": info.path.display().to_string(),
                    "seconds": r.started.elapsed().as_secs(),
                    "bytes": info.bytes,
                    "skipped": r.skipped,
                    "error": info.error,
                })
            })
        });
        json!({
            "connected": i.connected,
            "source": self.source.lock().unwrap().clone(),
            "sources": self.sources_cache.lock().unwrap().clone(),
            "pcMode": self.is_pc_mode(),
            "device": i.device,
            "link": i.remote,
            "video": i.video.as_ref().map(|v| json!({
                "codec": if v.codec == crate::mkv::CODEC_H265 { "H.265" } else { "H.264" },
                "width": v.width, "height": v.height,
            })),
            "audio": i.audio.is_some(),
            "fps": i.fps,
            "mbps": (i.mbps * 10.0).round() / 10.0,
            "latencyMs": i.latency_ms.map(|l| l.round()),
            "rttMs": i.clock_offset().map(|(_, rtt)| (rtt as f64 / 100.0).round() / 10.0),
            "mixer": self.mixer.status_json(),
            "phone": i.phone_stats,
            "recording": recording,
            "recordError": i.record_error,
            "live": self.live.status_json(),
            "adb": adb,
        })
    }
}

pub fn recordings_dir() -> PathBuf {
    let base = std::env::var("USERPROFILE")
        .map(PathBuf::from)
        .unwrap_or_else(|_| PathBuf::from("."));
    base.join("Videos").join("PhoneCast")
}

/// Primary LAN address (what the phone should type in Wi-Fi mode).
pub fn lan_ip() -> Option<String> {
    // No packet is sent: connect() on UDP only picks the outgoing interface.
    let s = std::net::UdpSocket::bind("0.0.0.0:0").ok()?;
    s.connect("8.8.8.8:80").ok()?;
    Some(s.local_addr().ok()?.ip().to_string())
}
