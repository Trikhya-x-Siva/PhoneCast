//! Streaming engine v2: encoders that never restart, one RTMP(S) sender per
//! destination that reconnects only its socket, and OBS-style dynamic
//! bitrate with the main destination protected. See docs/engine-v2.md.
//!
//! ```text
//! phone Media ──► Source ──► encoder (codec, size, kbps) ──► Stream ──► SendQueue ──► Sender ──► platform
//!                            (shared by destinations         (fan-out)   (time-measured,  (reconnects
//!                             with the same spec)                         drops frames)    the socket)
//!                     ▲ set_bitrate / request_idr                                  │ backlog, throughput
//!                     └──────────────── Engine control tick (Arbiter, 50 ms) ◄─────┘
//! ```

// Wired into core.rs by the integration branch; until then only tests use it.
#![allow(dead_code)]

pub mod amf;
pub mod dbr;
pub mod encoder;
pub mod flv;
#[cfg(feature = "libav")]
pub mod libav;
pub mod rtmp;

#[cfg(test)]
mod fake_server;
#[cfg(test)]
mod tests;

use crate::live::Framing;
use crate::sinks::Media;
use dbr::{Arbiter, DbrConfig, Observation};
use encoder::{EncodeKey, EncodeSpec, EncoderStats, Source, Stream, VideoEncoder};
use rtmp::{DropStats, SendQueue, Sender, SenderOptions, TcpStats};
use serde::Serialize;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex, Weak};
use std::time::{Duration, Instant};

pub type Bytes = Arc<[u8]>;

/// Video frame importance, for dropping under congestion.
#[derive(Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Debug)]
pub enum Priority {
    /// Nothing references it (NVENC B-frames).
    Disposable,
    /// Referenced by later frames (P-frames).
    Ref,
    /// IDR / IRAP.
    Key,
}

#[derive(Clone, Copy, PartialEq, Eq, Hash, Debug, Serialize)]
pub enum VCodec {
    H264,
    Hevc,
}

#[derive(Clone, Debug)]
pub enum Kind {
    Video {
        codec: VCodec,
        priority: Priority,
        cts_ms: i32,
    },
    Audio,
}

/// One encoded frame. Video is length-prefixed (AVCC/HVCC); audio is raw AAC.
#[derive(Clone, Debug)]
pub struct Packet {
    /// The [`Stream`] it came from.
    pub src: u32,
    pub dts_ms: i64,
    pub kind: Kind,
    pub data: Bytes,
    /// Decoder configuration (avcC/hvcC or AudioSpecificConfig); always on
    /// keyframes and audio, so any receiver can start from there.
    pub config: Option<Bytes>,
}

impl Packet {
    pub fn priority(&self) -> Option<Priority> {
        match self.kind {
            Kind::Video { priority, .. } => Some(priority),
            Kind::Audio => None,
        }
    }

    pub fn is_key(&self) -> bool {
        self.priority() == Some(Priority::Key)
    }
}

/// Picks the encoder backend: in-process libav/NVENC when built with
/// feature `libav` and the DLLs are present, else the FFmpeg CLI.
/// `cli_encoders` are the probed FFmpeg encoder names (H.264, HEVC).
pub fn source_for(
    config: crate::mkv::VideoConfig,
    ffmpeg: Option<std::path::PathBuf>,
    cli_encoders: (String, String),
    request_phone_key: Arc<dyn Fn() + Send + Sync>,
) -> Result<Arc<dyn Source>, String> {
    #[cfg(feature = "libav")]
    match libav::LibavSource::new(config.clone(), request_phone_key.clone()) {
        Ok(s) => return Ok(Arc::new(s)),
        Err(e) => crate::diag::log(
            "engine",
            format!("libav unavailable ({e}); using FFmpeg CLI"),
        ),
    }
    let ffmpeg = ffmpeg.ok_or("FFmpeg is not installed on this PC")?;
    Ok(Arc::new(encoder::CliSource::new(
        ffmpeg,
        cli_encoders,
        config,
        request_phone_key,
    )))
}

/// One platform to stream to.
#[derive(Clone, Debug)]
pub struct DestConfig {
    pub id: String,
    pub name: String,
    /// `rtmp[s]://host[:port]/app/key`
    pub url: String,
    pub codec: VCodec,
    pub w: u32,
    pub h: u32,
    pub fps: u32,
    pub kbps: u32,
    pub framing: Framing,
    /// The platform that keeps full quality when upload is short.
    pub main: bool,
}

impl DestConfig {
    fn spec(&self, kbps: u32) -> EncodeSpec {
        EncodeSpec {
            key: EncodeKey {
                codec: self.codec,
                w: self.w,
                h: self.h,
                fps: self.fps,
            },
            kbps,
            framing: self.framing,
        }
    }
}

#[derive(Clone, Debug)]
pub struct EngineOptions {
    pub dbr: DbrConfig,
    pub sender: SenderOptions,
    /// Most encoder sessions to run at once (NVENC allows 8 on GeForce).
    pub max_encoders: usize,
    pub tick: Duration,
    pub audio_kbps: u32,
    /// Beyond this backlog a queue skips to the next keyframe.
    pub max_backlog_ms: i64,
}

impl Default for EngineOptions {
    fn default() -> Self {
        EngineOptions {
            dbr: DbrConfig::default(),
            sender: SenderOptions::default(),
            max_encoders: 4,
            tick: Duration::from_millis(50),
            audio_kbps: 160,
            max_backlog_ms: 10_000,
        }
    }
}

/// An encoder and the destinations it feeds.
struct Slot {
    spec: EncodeSpec,
    enc: Arc<dyn VideoEncoder>,
    stream: Arc<Stream>,
    members: Vec<usize>,
}

struct Groups {
    slots: Vec<Slot>,
    /// Destination → stream id of its slot.
    assign: Vec<u32>,
    queues: Vec<Arc<SendQueue>>,
}

impl Groups {
    fn slot(&self, stream: u32) -> usize {
        self.slots
            .iter()
            .position(|s| s.stream.id == stream)
            .expect("slot")
    }

    fn request_idr(&self, dest: usize) {
        if let Some(i) = self
            .slots
            .iter()
            .position(|s| s.stream.id == self.assign[dest])
        {
            self.slots[i].enc.request_idr();
        }
    }

    /// Moves a destination to another slot; it switches at that encoder's
    /// next keyframe, which is requested right away.
    fn move_dest(&mut self, dest: usize, to: usize) {
        let from = self.slot(self.assign[dest]);
        self.slots[from].members.retain(|&m| m != dest);
        let s = &mut self.slots[to];
        s.members.push(dest);
        s.stream.subscribe(&self.queues[dest]);
        self.queues[dest].switch_to(s.stream.id);
        s.enc.request_idr();
        self.assign[dest] = s.stream.id;
    }

    /// Closes encoders nobody uses any more (after queues have switched).
    fn cleanup(&mut self) {
        for s in &self.slots {
            s.stream.prune();
        }
        self.slots.retain(|s| {
            let keep = !s.members.is_empty() || s.stream.has_subscribers();
            if !keep {
                s.enc.close();
            }
            keep
        });
    }
}

struct Dest {
    cfg: DestConfig,
    queue: Arc<SendQueue>,
    sender: Mutex<Sender>,
}

#[derive(Clone, Debug, Serialize)]
pub struct DestStatus {
    pub id: String,
    pub name: String,
    pub main: bool,
    pub state: String,
    pub error: Option<String>,
    pub reconnects: u32,
    pub seconds: u64,
    pub target_kbps: u32,
    pub original_kbps: u32,
    pub sent_kbps: Option<u32>,
    pub backlog_ms: u64,
    pub dropped: DropStats,
    pub encoder: EncoderStats,
    pub tcp: Option<TcpStats>,
}

pub struct Engine {
    source: Arc<dyn Source>,
    dests: Vec<Dest>,
    groups: Arc<Mutex<Groups>>,
    arbiter: Mutex<Arbiter>,
    opts: EngineOptions,
    stop: Arc<AtomicBool>,
    control: Mutex<Option<std::thread::JoinHandle<()>>>,
}

impl Engine {
    /// Opens the encoders (one per distinct spec), connects every
    /// destination and starts the control loop.
    pub fn start(
        source: Arc<dyn Source>,
        mut dests: Vec<DestConfig>,
        opts: EngineOptions,
    ) -> Result<Arc<Engine>, String> {
        if dests.is_empty() {
            return Err("No destination to stream to".into());
        }
        if !dests.iter().any(|d| d.main) {
            dests[0].main = true;
        }
        let mut seen_main = false;
        for d in dests.iter_mut() {
            d.main &= !seen_main;
            seen_main |= d.main;
        }

        let mut g = Groups {
            slots: Vec::new(),
            assign: Vec::new(),
            queues: Vec::new(),
        };
        let fail = |g: &Groups| g.slots.iter().for_each(|s| s.enc.close());
        for (i, d) in dests.iter().enumerate() {
            let spec = d.spec(d.kbps);
            let idx = match g.slots.iter().position(|s| {
                s.spec.key == spec.key && s.spec.kbps == spec.kbps && s.spec.framing == spec.framing
            }) {
                Some(idx) => idx,
                None => {
                    let stream = Stream::new();
                    let enc = match source.open(spec, stream.clone()) {
                        Ok(e) => e,
                        Err(e) => {
                            fail(&g);
                            return Err(e);
                        }
                    };
                    g.slots.push(Slot {
                        spec,
                        enc,
                        stream,
                        members: Vec::new(),
                    });
                    g.slots.len() - 1
                }
            };
            let s = &mut g.slots[idx];
            s.members.push(i);
            let q = SendQueue::new(s.stream.id, opts.max_backlog_ms);
            s.stream.subscribe(&q);
            g.assign.push(s.stream.id);
            g.queues.push(q);
        }
        let groups = Arc::new(Mutex::new(g));

        let mut built = Vec::new();
        for (i, d) in dests.iter().enumerate() {
            let queue = groups.lock().unwrap().queues[i].clone();
            let gg = groups.clone();
            let on_ready: Arc<dyn Fn() + Send + Sync> =
                Arc::new(move || gg.lock().unwrap().request_idr(i));
            let meta = flv::Meta {
                codec: d.codec,
                width: d.w,
                height: d.h,
                fps: d.fps,
                video_kbps: d.kbps,
                audio_kbps: opts.audio_kbps,
            };
            let sender = Sender::spawn(
                d.id.clone(),
                d.url.clone(),
                meta,
                queue.clone(),
                opts.sender.clone(),
                opts.dbr.window,
                on_ready,
            );
            let sender = match sender {
                Ok(s) => s,
                Err(e) => {
                    for b in &built {
                        let b: &Dest = b;
                        b.sender.lock().unwrap().stop();
                    }
                    fail(&groups.lock().unwrap());
                    return Err(format!("{}: {e}", d.name));
                }
            };
            built.push(Dest {
                cfg: d.clone(),
                queue,
                sender: Mutex::new(sender),
            });
        }

        let plan: Vec<(u32, bool)> = dests.iter().map(|d| (d.kbps, d.main)).collect();
        let engine = Arc::new(Engine {
            source,
            dests: built,
            groups,
            arbiter: Mutex::new(Arbiter::new(opts.dbr.clone(), &plan, opts.audio_kbps)),
            opts,
            stop: Arc::new(AtomicBool::new(false)),
            control: Mutex::new(None),
        });
        let weak: Weak<Engine> = Arc::downgrade(&engine);
        let (stop, tick) = (engine.stop.clone(), engine.opts.tick);
        let t = std::thread::Builder::new()
            .name("engine-control".into())
            .spawn(move || {
                while !stop.load(Ordering::SeqCst) {
                    std::thread::sleep(tick);
                    match weak.upgrade() {
                        Some(e) => e.tick(Instant::now()),
                        None => break,
                    }
                }
            })
            .map_err(|e| e.to_string())?;
        *engine.control.lock().unwrap() = Some(t);
        for d in &dests {
            crate::diag::log(
                "engine",
                format!(
                    "{}: {}x{}@{} {:?} {} kbps{} via {}",
                    d.name,
                    d.w,
                    d.h,
                    d.fps,
                    d.codec,
                    d.kbps,
                    if d.main { " (main)" } else { "" },
                    engine.source.backend()
                ),
            );
        }
        Ok(engine)
    }

    /// Phone media in. Never blocks.
    pub fn push(&self, m: &Media) {
        self.source.push(m);
    }

    fn tick(&self, now: Instant) {
        let obs: Vec<Observation> = self
            .dests
            .iter()
            .map(|d| {
                let s = d.sender.lock().unwrap();
                let sent_kbps = s.meter.lock().unwrap().kbps(now);
                Observation {
                    connected: s.is_connected(),
                    backlog_ms: d.queue.backlog_ms(),
                    sent_kbps,
                }
            })
            .collect();
        for (d, o) in self.dests.iter().zip(&obs) {
            if let Some(p) = dbr::drop_priority(o.backlog_ms, &self.opts.dbr) {
                d.queue.drop_below(p);
            }
        }
        let mut arb = self.arbiter.lock().unwrap();
        if arb.tick(now, &obs) {
            let targets = arb.targets();
            drop(arb);
            self.apply(&targets);
        }
        self.groups.lock().unwrap().cleanup();
    }

    /// Makes each destination's encoder run at its target bitrate: change
    /// the running encoder if the destination has it to itself, otherwise
    /// move the destination to an encoder at that bitrate (opening one if
    /// needed). Encoders are never restarted.
    fn apply(&self, targets: &[u32]) {
        if !self.source.live_bitrate() {
            return; // CLI fallback: frame dropping only
        }
        let mut g = self.groups.lock().unwrap();
        for (i, &want) in targets.iter().enumerate() {
            let cur = g.slot(g.assign[i]);
            let spec = g.slots[cur].spec;
            if spec.kbps == want {
                continue;
            }
            let same = |s: &Slot| {
                s.spec.key == spec.key && s.spec.framing == spec.framing && s.spec.kbps == want
            };
            if let Some(j) = g.slots.iter().position(same) {
                g.move_dest(i, j);
                continue;
            }
            if g.slots[cur].members.len() == 1 {
                let s = &mut g.slots[cur];
                if s.enc.set_bitrate(want) {
                    s.spec.kbps = want;
                }
                continue;
            }
            if g.slots.len() >= self.opts.max_encoders {
                continue; // frame dropping has to do
            }
            let new_spec = EncodeSpec { kbps: want, ..spec };
            let stream = Stream::new();
            match self.source.open(new_spec, stream.clone()) {
                Ok(enc) => {
                    g.slots.push(Slot {
                        spec: new_spec,
                        enc,
                        stream,
                        members: Vec::new(),
                    });
                    let j = g.slots.len() - 1;
                    g.move_dest(i, j);
                }
                Err(e) => crate::diag::log("engine", format!("extra encoder failed: {e}")),
            }
        }
    }

    pub fn status(&self) -> Vec<DestStatus> {
        let now = Instant::now();
        let targets = self.arbiter.lock().unwrap().targets();
        let g = self.groups.lock().unwrap();
        self.dests
            .iter()
            .enumerate()
            .map(|(i, d)| {
                let s = d.sender.lock().unwrap();
                let st = s.status.lock().unwrap().clone();
                let enc = g
                    .slots
                    .iter()
                    .find(|x| x.stream.id == g.assign[i])
                    .map(|x| x.enc.stats())
                    .unwrap_or_default();
                let sent_kbps = s.meter.lock().unwrap().kbps(now);
                DestStatus {
                    id: d.cfg.id.clone(),
                    name: d.cfg.name.clone(),
                    main: d.cfg.main,
                    state: st.state,
                    error: st.error,
                    reconnects: st.reconnects,
                    seconds: st.live_since.map(|t| t.elapsed().as_secs()).unwrap_or(0),
                    target_kbps: targets[i],
                    original_kbps: d.cfg.kbps,
                    sent_kbps,
                    backlog_ms: d.queue.backlog_ms(),
                    dropped: d.queue.drops(),
                    encoder: enc,
                    tcp: st.tcp,
                }
            })
            .collect()
    }

    pub fn status_json(&self) -> serde_json::Value {
        serde_json::json!({
            "engine": "v2",
            "backend": self.source.backend(),
            "destinations": self.status(),
        })
    }

    /// Stats of every running encoder (for tests and diagnostics).
    pub fn encoders(&self) -> Vec<(EncodeSpec, EncoderStats)> {
        let g = self.groups.lock().unwrap();
        g.slots.iter().map(|s| (s.spec, s.enc.stats())).collect()
    }

    /// Ends every stream politely and closes the encoders.
    pub fn stop(&self) {
        self.stop.store(true, Ordering::SeqCst);
        if let Some(t) = self.control.lock().unwrap().take() {
            t.join().ok();
        }
        for d in &self.dests {
            d.sender.lock().unwrap().stop();
        }
        self.source.stop();
        let mut g = self.groups.lock().unwrap();
        for s in g.slots.drain(..) {
            s.enc.close();
        }
    }
}

impl Drop for Engine {
    fn drop(&mut self) {
        self.stop.store(true, Ordering::SeqCst);
    }
}
