//! Audio mixer: game (from the phone) + Discord voice + microphone.
//!
//! - Game audio arrives from the phone with the phone's timestamps and drives
//!   the mix, so the mixed track stays in sync with the video.
//! - Discord is captured from the Discord app only (Windows per-app loopback),
//!   so other PC sounds never leak into the stream.
//! - The mic goes through AI noise suppression (RNNoise) and a noise gate.
//! - A limiter on the output prevents clipping when everyone talks at once,
//!   and optional ducking lowers the game while someone is talking.

use serde_json::{json, Value};
use std::collections::VecDeque;
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

pub const RATE: usize = 48_000;
const CHUNK_FRAMES: usize = 480; // 10 ms
/// PC sources are kept ~40 ms ahead of the mix; beyond 120 ms we drop the
/// oldest audio so delay can never build up.
const MAX_FIFO_FRAMES: usize = RATE * 120 / 1000;
const TARGET_FIFO_FRAMES: usize = RATE * 40 / 1000;

#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum Source {
    Game,
    Discord,
    Mic,
}

impl Source {
    pub fn parse(s: &str) -> Option<Self> {
        match s {
            "game" => Some(Self::Game),
            "discord" => Some(Self::Discord),
            "mic" => Some(Self::Mic),
            _ => None,
        }
    }
}

struct Channel {
    gain: f32,
    muted: bool,
    /// Peak since the last status report (post-gain, linear).
    peak: f32,
    /// Interleaved stereo f32 samples waiting to be mixed (PC sources only).
    fifo: VecDeque<f32>,
    active: bool,
    status: String,
}

impl Channel {
    fn new(status: &str) -> Self {
        Self {
            gain: 1.0,
            muted: false,
            peak: 0.0,
            fifo: VecDeque::new(),
            active: false,
            status: status.into(),
        }
    }

    fn push(&mut self, samples: &[f32]) {
        self.fifo.extend(samples.iter().copied());
        if self.fifo.len() > MAX_FIFO_FRAMES * 2 {
            let excess = self.fifo.len() - TARGET_FIFO_FRAMES * 2;
            self.fifo.drain(..excess);
        }
    }

    fn effective_gain(&self) -> f32 {
        if self.muted {
            0.0
        } else {
            self.gain
        }
    }

    fn json(&self) -> Value {
        json!({
            "gain": self.gain,
            "muted": self.muted,
            "active": self.active,
            "status": self.status,
        })
    }
}

/// Apps the "Discord" channel can capture: (id, label, process names).
/// Browsers play every tab's sound through one process, so capturing a
/// browser includes all its tabs.
pub const VOICE_APPS: &[(&str, &str, &[&str])] = &[
    (
        "discord",
        "Discord app",
        &["discord.exe", "discordptb.exe", "discordcanary.exe"],
    ),
    ("chrome", "Chrome", &["chrome.exe"]),
    ("msedge", "Edge", &["msedge.exe"]),
    ("firefox", "Firefox", &["firefox.exe"]),
    ("brave", "Brave", &["brave.exe"]),
    ("opera", "Opera", &["opera.exe"]),
];

fn db(peak: f32) -> f64 {
    if peak <= 1e-5 {
        -90.0
    } else {
        ((20.0 * peak.log10()) as f64 * 10.0).round() / 10.0
    }
}

struct State {
    game: Channel,
    discord: Channel,
    mic: Channel,
    mic_devices: Vec<(String, String)>, // (id, name)
    mic_device: Option<String>,         // None = Windows default
    mic_generation: u64,
    /// Where Discord voice comes from: "auto", "discord" or a browser id.
    voice_app: String,
    voice_generation: u64,
    voice_running: Vec<&'static str>,
    /// Process currently captured for Discord voice (excluded from PC sound).
    voice_pid: Option<u32>,
    /// PC mode: the Game strip carries the PC's own sound (everything except
    /// the voice app), captured by WASAPI process-exclude loopback.
    pc_audio: bool,
    pc_generation: u64,
    noise_suppression: bool,
    ducking: bool,
    duck: f32,
    limiter: f32,
    last_game_audio: Option<Instant>,
    /// Optional tap for the local AI helper: peak levels since the last
    /// take and, when `ai_pcm` is on, the mix as 16 kHz mono.
    ai_tap: AiTap,
}

#[derive(Default)]
struct AiTap {
    on: bool,
    pcm_on: bool,
    game_peak: f32,
    mic_peak: f32,
    pcm: Vec<u8>,
    /// Sample position within the current 3-sample decimation group.
    phase: u8,
    acc: f32,
}

pub struct Mixer {
    state: Mutex<State>,
}

impl Mixer {
    pub fn start() -> Arc<Mixer> {
        let mixer = Arc::new(Mixer {
            state: Mutex::new(State {
                game: Channel::new("Waiting for phone"),
                discord: Channel::new("Looking for Discord…"),
                mic: Channel::new("Starting…"),
                mic_devices: Vec::new(),
                mic_device: None,
                mic_generation: 0,
                voice_app: "auto".into(),
                voice_generation: 0,
                voice_running: Vec::new(),
                voice_pid: None,
                pc_audio: false,
                pc_generation: 0,
                noise_suppression: true,
                ducking: false,
                duck: 1.0,
                limiter: 1.0,
                last_game_audio: None,
                ai_tap: AiTap::default(),
            }),
        });
        #[cfg(windows)]
        {
            capture::spawn_voice(mixer.clone());
            capture::spawn_mic(mixer.clone());
            capture::spawn_pc_audio(mixer.clone());
        }
        mixer
    }

    /// Mixes one chunk of phone game audio (s16le stereo) with Discord and the
    /// mic. Returns the mixed chunk as s16le stereo, same length.
    pub fn mix_game(&self, pcm: &[u8]) -> Vec<u8> {
        let game: Vec<f32> = pcm
            .as_chunks::<2>()
            .0
            .iter()
            .map(|b| i16::from_le_bytes(*b) as f32 / 32768.0)
            .collect();
        let mut s = self.state.lock().unwrap();
        s.last_game_audio = Some(Instant::now());
        s.game.active = true;
        s.game.status = "From phone".into();
        Self::mix(&mut s, &game)
    }

    /// Produces one 10 ms mix when the phone is not supplying game audio:
    /// PC sound in PC mode, silence otherwise (Discord + mic still flow).
    /// Returns None while phone game audio is flowing.
    pub fn mix_without_game(&self) -> Option<Vec<u8>> {
        let mut s = self.state.lock().unwrap();
        if s.last_game_audio
            .is_some_and(|t| t.elapsed() < Duration::from_millis(150))
        {
            return None;
        }
        let n = CHUNK_FRAMES * 2;
        let game: Vec<f32> = if s.pc_audio {
            let take = n.min(s.game.fifo.len());
            let mut v: Vec<f32> = s.game.fifo.drain(..take).collect();
            v.resize(n, 0.0);
            v
        } else {
            s.game.active = false;
            s.game.status = "No game audio from phone".into();
            vec![0f32; n]
        };
        Some(Self::mix(&mut s, &game))
    }

    /// PC mode on/off: the Game strip becomes "PC sound".
    pub fn set_pc_audio(&self, on: bool) {
        let mut s = self.state.lock().unwrap();
        if s.pc_audio != on {
            s.pc_audio = on;
            s.pc_generation += 1;
            s.game.fifo.clear();
            if on {
                s.game.status = "PC sound · starting…".into();
            }
        }
    }

    fn mix(s: &mut State, game: &[f32]) -> Vec<u8> {
        let n = game.len();
        let take = |c: &mut Channel| -> Vec<f32> {
            let mut v: Vec<f32> = c.fifo.drain(..n.min(c.fifo.len())).collect();
            v.resize(n, 0.0);
            v
        };
        let discord = take(&mut s.discord);
        let mic = take(&mut s.mic);

        let (gg, gd, gm) = (
            s.game.effective_gain(),
            s.discord.effective_gain(),
            s.mic.effective_gain(),
        );
        let peak = |v: &[f32], g: f32| v.iter().fold(0f32, |m, x| m.max(x.abs())) * g;
        let (pg, pd, pm) = (peak(game, gg), peak(&discord, gd), peak(&mic, gm));
        s.game.peak = s.game.peak.max(pg);
        s.discord.peak = s.discord.peak.max(pd);
        s.mic.peak = s.mic.peak.max(pm);
        if s.ai_tap.on {
            s.ai_tap.game_peak = s.ai_tap.game_peak.max(pg);
            s.ai_tap.mic_peak = s.ai_tap.mic_peak.max(pm);
        }

        // Ducking: game drops 8 dB while you or a teammate talk.
        let target = if s.ducking && (pd > 0.03 || pm > 0.03) {
            0.4
        } else {
            1.0
        };
        s.duck += (target - s.duck) * if target < s.duck { 0.5 } else { 0.05 };
        let gg = gg * s.duck;

        let mut out = Vec::with_capacity(n * 2);
        let mut lim = s.limiter;
        let tap_pcm = s.ai_tap.on && s.ai_tap.pcm_on;
        for i in 0..n {
            let x = game[i] * gg + discord[i] * gd + mic[i] * gm;
            if tap_pcm {
                // 48 kHz stereo → 16 kHz mono: average 3 frames (6 samples).
                let t = &mut s.ai_tap;
                t.acc += x;
                t.phase += 1;
                if t.phase == 6 {
                    let v = (t.acc / 6.0 * 32767.0).clamp(-32768.0, 32767.0) as i16;
                    t.pcm.extend_from_slice(&v.to_le_bytes());
                    t.acc = 0.0;
                    t.phase = 0;
                }
            }
            // Peak limiter: instant attack, ~0.5 s release. Never clips.
            if x.abs() * lim > 0.97 {
                lim = 0.97 / x.abs();
            }
            let y = (x * lim * 32767.0).clamp(-32768.0, 32767.0) as i16;
            out.extend_from_slice(&y.to_le_bytes());
            lim += (1.0 - lim) * 0.00005;
        }
        s.limiter = lim;
        out
    }

    pub fn set_channel(&self, src: Source, gain: Option<f32>, muted: Option<bool>) {
        let mut s = self.state.lock().unwrap();
        let c = match src {
            Source::Game => &mut s.game,
            Source::Discord => &mut s.discord,
            Source::Mic => &mut s.mic,
        };
        if let Some(g) = gain {
            c.gain = g.clamp(0.0, 2.0);
        }
        if let Some(m) = muted {
            c.muted = m;
        }
    }

    pub fn set_mic_device(&self, id: Option<String>) {
        let mut s = self.state.lock().unwrap();
        s.mic_device = id.filter(|i| !i.is_empty());
        s.mic_generation += 1;
    }

    pub fn set_voice_app(&self, id: &str) {
        let mut s = self.state.lock().unwrap();
        if VOICE_APPS.iter().any(|a| a.0 == id) || id == "auto" {
            s.voice_app = id.into();
            s.voice_generation += 1;
        }
    }

    pub fn set_option(&self, name: &str, on: bool) {
        let mut s = self.state.lock().unwrap();
        match name {
            "noiseSuppression" => s.noise_suppression = on,
            "ducking" => s.ducking = on,
            _ => {}
        }
    }

    /// Peak levels (dBFS) since the last call, for the UI meters (~20×/s).
    pub fn levels_json(&self) -> Value {
        let mut s = self.state.lock().unwrap();
        let v = json!({ "game": db(s.game.peak), "discord": db(s.discord.peak), "mic": db(s.mic.peak) });
        s.game.peak = 0.0;
        s.discord.peak = 0.0;
        s.mic.peak = 0.0;
        v
    }

    /// Local AI helper tap. `pcm` also collects the mix as 16 kHz mono for
    /// the sound classifier (about 32 KB/s; nothing is kept beyond 2 s).
    pub fn set_ai_tap(&self, on: bool, pcm: bool) {
        let mut s = self.state.lock().unwrap();
        s.ai_tap.on = on;
        s.ai_tap.pcm_on = on && pcm;
        s.ai_tap.pcm.clear();
        s.ai_tap.phase = 0;
        s.ai_tap.acc = 0.0;
    }

    /// Peak levels (dBFS, game and mic) since the last call plus the PCM
    /// collected meanwhile; None while the tap is off.
    pub fn take_ai_tap(&self) -> Option<(f64, f64, Option<Vec<u8>>)> {
        let mut s = self.state.lock().unwrap();
        if !s.ai_tap.on {
            return None;
        }
        let t = &mut s.ai_tap;
        let out = (
            db(t.game_peak),
            db(t.mic_peak),
            t.pcm_on.then(|| std::mem::take(&mut t.pcm)),
        );
        t.game_peak = 0.0;
        t.mic_peak = 0.0;
        if t.pcm.len() > 64_000 {
            t.pcm.clear();
        }
        Some(out)
    }

    /// Settings and state for the UI.
    pub fn status_json(&self) -> Value {
        let s = self.state.lock().unwrap();
        json!({
            "game": s.game.json(),
            "discord": s.discord.json(),
            "mic": s.mic.json(),
            "micDevices": s.mic_devices.iter().map(|(id, name)| json!({"id": id, "name": name})).collect::<Vec<_>>(),
            "micDevice": s.mic_device,
            "voiceApp": s.voice_app,
            "voiceApps": VOICE_APPS.iter().map(|a| json!({
                "id": a.0, "label": a.1, "running": s.voice_running.contains(&a.0),
            })).collect::<Vec<_>>(),
            "noiseSuppression": s.noise_suppression,
            "ducking": s.ducking,
        })
    }

    fn with<R>(&self, f: impl FnOnce(&mut State) -> R) -> R {
        f(&mut self.state.lock().unwrap())
    }
}

// ---------------- Windows capture (WASAPI) ----------------

#[cfg(windows)]
mod capture {
    use super::*;
    use std::collections::VecDeque;
    use wasapi::{
        initialize_mta, AudioClient, DeviceEnumerator, Direction, SampleType, StreamMode,
        WaveFormat,
    };

    /// Captures Discord voice from the Discord app or a browser (whichever
    /// the user picked, or automatically). Follows the app if it restarts.
    pub fn spawn_voice(mixer: Arc<Mixer>) {
        std::thread::Builder::new()
            .name("voice-audio".into())
            .spawn(move || {
                initialize_mta().ok().ok();
                loop {
                    let procs = snapshot();
                    let running: Vec<&'static str> = VOICE_APPS
                        .iter()
                        .filter(|a| root_pid(&procs, a.2).is_some())
                        .map(|a| a.0)
                        .collect();
                    let (choice, generation) = mixer.with(|s| {
                        s.voice_running = running.clone();
                        (s.voice_app.clone(), s.voice_generation)
                    });
                    // Auto: the Discord app if running, otherwise the first open browser.
                    let target = if choice == "auto" {
                        VOICE_APPS.iter().find(|a| running.contains(&a.0))
                    } else {
                        VOICE_APPS.iter().find(|a| a.0 == choice)
                    };
                    match target.and_then(|a| root_pid(&procs, a.2).map(|pid| (a, pid))) {
                        Some((app, pid)) => {
                            let status = if app.0 == "discord" {
                                "Discord app".to_string()
                            } else {
                                format!("Discord in {} · includes all {} tabs", app.1, app.1)
                            };
                            mixer.with(|s| {
                                s.discord.active = true;
                                s.discord.status = status;
                                s.voice_pid = Some(pid);
                            });
                            if let Err(e) = capture_app(&mixer, pid, app.2, generation) {
                                mixer.with(|s| s.discord.status = format!("Capture error: {e}"));
                            }
                        }
                        None => mixer.with(|s| {
                            s.discord.active = false;
                            s.voice_pid = None;
                            s.discord.fifo.clear();
                            s.discord.status = match target {
                                Some(a) => format!("{} is not running", a.1),
                                None => "Open Discord (app or browser) on this PC".into(),
                            };
                        }),
                    }
                    // Re-check soon, or right away if the user changed the source.
                    for _ in 0..30 {
                        if mixer.with(|s| s.voice_generation) != generation {
                            break;
                        }
                        std::thread::sleep(Duration::from_millis(100));
                    }
                }
            })
            .ok();
    }

    /// Captures everything an app (and its child processes) plays, until it
    /// exits or the user picks another source.
    fn capture_app(
        mixer: &Arc<Mixer>,
        pid: u32,
        exes: &[&str],
        generation: u64,
    ) -> Result<(), Box<dyn std::error::Error>> {
        let format = WaveFormat::new(32, 32, &SampleType::Float, RATE, 2, None);
        let mut client = AudioClient::new_application_loopback_client(pid, true)?;
        client.initialize_client(
            &format,
            &Direction::Capture,
            &StreamMode::EventsShared {
                autoconvert: true,
                buffer_duration_hns: 0,
            },
        )?;
        let event = client.set_get_eventhandle()?;
        let capture = client.get_audiocaptureclient()?;
        client.start_stream()?;
        let mut bytes = VecDeque::new();
        let mut last_check = Instant::now();
        loop {
            capture.read_from_device_to_deque(&mut bytes)?;
            if !bytes.is_empty() {
                let samples = drain_f32(&mut bytes);
                mixer.with(|s| s.discord.push(&samples));
            }
            // Nothing arrives while the app is silent, so a timeout is normal.
            let _ = event.wait_for_event(100);
            if mixer.with(|s| s.voice_generation) != generation {
                client.stop_stream().ok();
                return Ok(());
            }
            if last_check.elapsed() > Duration::from_secs(3) {
                last_check = Instant::now();
                if root_pid(&snapshot(), exes) != Some(pid) {
                    client.stop_stream().ok();
                    return Ok(());
                }
            }
        }
    }

    /// PC mode: captures everything the PC plays except the voice app
    /// (Discord/browser), so teammates are not doubled into the Game strip.
    pub fn spawn_pc_audio(mixer: Arc<Mixer>) {
        std::thread::Builder::new()
            .name("pc-audio".into())
            .spawn(move || {
                initialize_mta().ok().ok();
                loop {
                    let (on, generation, voice_pid) =
                        mixer.with(|s| (s.pc_audio, s.pc_generation, s.voice_pid));
                    if !on {
                        std::thread::sleep(Duration::from_millis(300));
                        continue;
                    }
                    // Excluding our own process captures "everything else".
                    let exclude = voice_pid.unwrap_or_else(std::process::id);
                    mixer.with(|s| {
                        s.game.active = true;
                        s.game.status = if voice_pid.is_some() {
                            "PC sound (Discord kept separate)".into()
                        } else {
                            "PC sound".into()
                        };
                    });
                    if let Err(e) = capture_pc(&mixer, exclude, generation) {
                        mixer.with(|s| s.game.status = format!("PC sound error: {e}"));
                        std::thread::sleep(Duration::from_secs(2));
                    }
                }
            })
            .ok();
    }

    fn capture_pc(
        mixer: &Arc<Mixer>,
        exclude_pid: u32,
        generation: u64,
    ) -> Result<(), Box<dyn std::error::Error>> {
        let format = WaveFormat::new(32, 32, &SampleType::Float, RATE, 2, None);
        // include_tree = false → EXCLUDE this process tree, capture the rest.
        let mut client = AudioClient::new_application_loopback_client(exclude_pid, false)?;
        client.initialize_client(
            &format,
            &Direction::Capture,
            &StreamMode::EventsShared {
                autoconvert: true,
                buffer_duration_hns: 0,
            },
        )?;
        let event = client.set_get_eventhandle()?;
        let capture = client.get_audiocaptureclient()?;
        client.start_stream()?;
        let mut bytes = VecDeque::new();
        loop {
            capture.read_from_device_to_deque(&mut bytes)?;
            if !bytes.is_empty() {
                let samples = drain_f32(&mut bytes);
                mixer.with(|s| s.game.push(&samples));
            }
            let _ = event.wait_for_event(100);
            let (on, gen, vp) = mixer.with(|s| (s.pc_audio, s.pc_generation, s.voice_pid));
            if !on || gen != generation || vp.unwrap_or_else(std::process::id) != exclude_pid {
                client.stop_stream().ok();
                return Ok(());
            }
        }
    }

    pub fn spawn_mic(mixer: Arc<Mixer>) {
        std::thread::Builder::new()
            .name("mic-audio".into())
            .spawn(move || {
                initialize_mta().ok().ok();
                loop {
                    refresh_devices(&mixer);
                    let (generation, device) =
                        mixer.with(|s| (s.mic_generation, s.mic_device.clone()));
                    if let Err(e) = capture_mic(&mixer, generation, device) {
                        mixer.with(|s| {
                            s.mic.active = false;
                            s.mic.status = format!("Mic unavailable: {e}");
                        });
                        std::thread::sleep(Duration::from_secs(2));
                    }
                }
            })
            .ok();
    }

    fn refresh_devices(mixer: &Arc<Mixer>) {
        let Ok(e) = DeviceEnumerator::new() else {
            return;
        };
        let Ok(col) = e.get_device_collection(&Direction::Capture) else {
            return;
        };
        let mut list = Vec::new();
        for d in (&col).into_iter().flatten() {
            if let (Ok(id), Ok(name)) = (d.get_id(), d.get_friendlyname()) {
                list.push((id, name));
            }
        }
        mixer.with(|s| s.mic_devices = list);
    }

    /// Captures the mic until the user picks another device.
    fn capture_mic(
        mixer: &Arc<Mixer>,
        generation: u64,
        device_id: Option<String>,
    ) -> Result<(), Box<dyn std::error::Error>> {
        let e = DeviceEnumerator::new()?;
        let device = match &device_id {
            Some(id) => {
                let col = e.get_device_collection(&Direction::Capture)?;
                (&col)
                    .into_iter()
                    .filter_map(Result::ok)
                    .find(|d| d.get_id().ok().as_deref() == Some(id.as_str()))
                    .ok_or("selected mic not found")?
            }
            None => e.get_default_device(&Direction::Capture)?,
        };
        let name = device
            .get_friendlyname()
            .unwrap_or_else(|_| "Microphone".into());
        let mut client = device.get_iaudioclient()?;
        let format = WaveFormat::new(32, 32, &SampleType::Float, RATE, 1, None);
        let (_, min_period) = client.get_device_period()?;
        client.initialize_client(
            &format,
            &Direction::Capture,
            &StreamMode::EventsShared {
                autoconvert: true,
                buffer_duration_hns: min_period,
            },
        )?;
        let event = client.set_get_eventhandle()?;
        let capture = client.get_audiocaptureclient()?;
        client.start_stream()?;
        mixer.with(|s| {
            s.mic.active = true;
            s.mic.status = name;
        });

        let mut denoise = nnnoiseless::DenoiseState::new();
        let mut bytes = VecDeque::new();
        let mut mono: Vec<f32> = Vec::new();
        let mut gate = 0f32;
        let mut quiet_frames = 0usize;
        let mut last_devices = Instant::now();
        loop {
            capture.read_from_device_to_deque(&mut bytes)?;
            mono.extend(drain_f32(&mut bytes));
            let ns = mixer.with(|s| s.noise_suppression);
            while mono.len() >= nnnoiseless::DenoiseState::FRAME_SIZE {
                let frame: Vec<f32> = mono
                    .drain(..nnnoiseless::DenoiseState::FRAME_SIZE)
                    .collect();
                let mut clean = vec![0f32; frame.len()];
                if ns {
                    // RNNoise works on 16-bit-scaled floats.
                    let scaled: Vec<f32> = frame.iter().map(|x| x * 32768.0).collect();
                    denoise.process_frame(&mut clean, &scaled);
                    clean.iter_mut().for_each(|x| *x /= 32768.0);
                } else {
                    clean.copy_from_slice(&frame);
                }
                // Noise gate: closes after 200 ms below -50 dBFS, opens instantly.
                let rms = (clean.iter().map(|x| x * x).sum::<f32>() / clean.len() as f32).sqrt();
                if rms > 0.003 {
                    quiet_frames = 0;
                } else {
                    quiet_frames += 1;
                }
                let target = if quiet_frames < 20 { 1.0 } else { 0.0 };
                let mut stereo = Vec::with_capacity(clean.len() * 2);
                for x in clean {
                    gate += (target - gate) * if target > gate { 0.05 } else { 0.0005 };
                    let y = x * gate;
                    stereo.push(y);
                    stereo.push(y);
                }
                mixer.with(|s| s.mic.push(&stereo));
            }
            if event.wait_for_event(200).is_err() && mixer.with(|s| s.mic_generation) != generation
            {
                break;
            }
            if mixer.with(|s| s.mic_generation) != generation {
                break;
            }
            if last_devices.elapsed() > Duration::from_secs(5) {
                last_devices = Instant::now();
                refresh_devices(mixer);
            }
        }
        client.stop_stream().ok();
        Ok(())
    }

    fn drain_f32(bytes: &mut VecDeque<u8>) -> Vec<f32> {
        let n = bytes.len() / 4;
        let mut out = Vec::with_capacity(n);
        let mut b = [0u8; 4];
        for _ in 0..n {
            for x in b.iter_mut() {
                *x = bytes.pop_front().unwrap();
            }
            out.push(f32::from_le_bytes(b));
        }
        out
    }

    /// Listens to a running app for 3 s. Run with
    /// `PHONECAST_VOICE_APP=chrome cargo test voice_capture -- --ignored --nocapture`.
    #[test]
    #[ignore]
    fn voice_capture() {
        initialize_mta().ok().ok();
        let id = std::env::var("PHONECAST_VOICE_APP").unwrap_or_else(|_| "chrome".into());
        let app = VOICE_APPS.iter().find(|a| a.0 == id).expect("known app");
        let pid = root_pid(&snapshot(), app.2).expect("app is running");
        let format = WaveFormat::new(32, 32, &SampleType::Float, RATE, 2, None);
        let mut client = AudioClient::new_application_loopback_client(pid, true).unwrap();
        client
            .initialize_client(
                &format,
                &Direction::Capture,
                &StreamMode::EventsShared {
                    autoconvert: true,
                    buffer_duration_hns: 0,
                },
            )
            .unwrap();
        let event = client.set_get_eventhandle().unwrap();
        let capture = client.get_audiocaptureclient().unwrap();
        client.start_stream().unwrap();
        let (mut n, mut peak) = (0usize, 0f32);
        let mut bytes = VecDeque::new();
        let t = Instant::now();
        while t.elapsed() < Duration::from_secs(3) {
            capture.read_from_device_to_deque(&mut bytes).unwrap();
            let s = drain_f32(&mut bytes);
            n += s.len();
            peak = s.iter().fold(peak, |m, x| m.max(x.abs()));
            let _ = event.wait_for_event(100);
        }
        println!(
            "{}: pid {pid}, {} samples, peak {:.1} dBFS",
            app.1,
            n,
            db(peak)
        );
    }

    /// Running processes: (pid, parent pid, lowercase exe name).
    fn snapshot() -> Vec<(u32, u32, String)> {
        use windows_sys::Win32::Foundation::{CloseHandle, INVALID_HANDLE_VALUE};
        use windows_sys::Win32::System::Diagnostics::ToolHelp::{
            CreateToolhelp32Snapshot, Process32FirstW, Process32NextW, PROCESSENTRY32W,
            TH32CS_SNAPPROCESS,
        };
        let mut procs = Vec::new();
        unsafe {
            let snap = CreateToolhelp32Snapshot(TH32CS_SNAPPROCESS, 0);
            if snap == INVALID_HANDLE_VALUE {
                return procs;
            }
            let mut e: PROCESSENTRY32W = std::mem::zeroed();
            e.dwSize = std::mem::size_of::<PROCESSENTRY32W>() as u32;
            if Process32FirstW(snap, &mut e) != 0 {
                loop {
                    let len = e
                        .szExeFile
                        .iter()
                        .position(|&c| c == 0)
                        .unwrap_or(e.szExeFile.len());
                    let name = String::from_utf16_lossy(&e.szExeFile[..len]).to_lowercase();
                    procs.push((e.th32ProcessID, e.th32ParentProcessID, name));
                    if Process32NextW(snap, &mut e) == 0 {
                        break;
                    }
                }
            }
            CloseHandle(snap);
        }
        procs
    }

    /// The app's top-level process: capturing its tree includes the child
    /// process that actually plays audio (Discord's voice / a browser's audio service).
    fn root_pid(procs: &[(u32, u32, String)], exes: &[&str]) -> Option<u32> {
        let mine: Vec<&(u32, u32, String)> = procs
            .iter()
            .filter(|p| exes.contains(&p.2.as_str()))
            .collect();
        mine.iter()
            .find(|p| !mine.iter().any(|q| q.0 == p.1))
            .map(|p| p.0)
    }
}
