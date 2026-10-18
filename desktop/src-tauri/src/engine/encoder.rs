//! Encoders: one per distinct (codec, resolution, fps, bitrate), shared by
//! every destination that wants the same thing, each feeding a [`Stream`]
//! that fans packets out to the destinations' send queues.
//!
//! Backends ([`Source`]):
//! - `libav` (feature `libav`, [`super::libav`]): in-process NVDEC → CUDA
//!   framing (the same filter graph as `live::video_filter`) → NVENC. Bitrate
//!   changes are applied to the running encoder (`nvEncReconfigureEncoder`
//!   through libavcodec), rate-limited by [`BitrateGate`], with an IDR.
//! - [`CliSource`]: FFmpeg CLI processes, used when the FFmpeg libraries are
//!   missing. The network side is identical (reconnects never restart the
//!   encoder), but bitrate cannot change live, so DBR only drops frames.

use super::rtmp::SendQueue;
use super::{Packet, VCodec};
use crate::live::Framing;
use crate::sinks::Media;
use std::sync::atomic::{AtomicU32, Ordering};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

#[derive(Clone, Copy, PartialEq, Eq, Hash, Debug)]
pub struct EncodeKey {
    pub codec: VCodec,
    pub w: u32,
    pub h: u32,
    pub fps: u32,
}

#[derive(Clone, Copy, Debug)]
pub struct EncodeSpec {
    pub key: EncodeKey,
    pub kbps: u32,
    pub framing: Framing,
}

impl EncodeSpec {
    /// Keyframe interval: 2 s, what YouTube and Twitch require.
    pub fn gop(&self) -> u32 {
        self.key.fps * 2
    }
}

#[derive(Clone, Copy, Debug, Default, serde::Serialize)]
pub struct EncoderStats {
    /// Encoder sessions opened for this encoder. Stays 1: bitrate changes and
    /// reconnects never re-open it.
    pub sessions: u32,
    /// Live bitrate changes applied to the running session.
    pub reconfigs: u32,
    /// IDRs requested (reconnects, switches) or forced by a reconfigure.
    pub idrs: u32,
    pub frames: u64,
    pub kbps: u32,
}

pub trait VideoEncoder: Send + Sync {
    fn spec(&self) -> EncodeSpec;
    /// Changes the bitrate of the running encoder. Applied between frames,
    /// at most once per [`BitrateGate`] interval (latest value wins), and
    /// followed by an IDR. Returns false if the backend cannot do it live.
    fn set_bitrate(&self, kbps: u32) -> bool;
    /// Next frame becomes an IDR (with parameter sets).
    fn request_idr(&self);
    fn stats(&self) -> EncoderStats;
    fn close(&self);
}

/// Where encoded media comes from. One per go-live session.
pub trait Source: Send + Sync {
    fn backend(&self) -> &'static str;
    /// Whether encoders can change bitrate without restarting.
    fn live_bitrate(&self) -> bool;
    /// Opens an encoder publishing video (and the shared audio) to `out`.
    fn open(&self, spec: EncodeSpec, out: Arc<Stream>) -> Result<Arc<dyn VideoEncoder>, String>;
    /// Phone media in (the same `Media` the recorder gets).
    fn push(&self, m: &Media);
    fn stop(&self);
}

static NEXT_STREAM: AtomicU32 = AtomicU32::new(1);

/// One encoder's output: fans packets out to send queues.
pub struct Stream {
    pub id: u32,
    subs: Mutex<Vec<Arc<SendQueue>>>,
}

impl Stream {
    pub fn new() -> Arc<Stream> {
        Arc::new(Stream {
            id: NEXT_STREAM.fetch_add(1, Ordering::Relaxed),
            subs: Mutex::new(Vec::new()),
        })
    }

    pub fn subscribe(&self, q: &Arc<SendQueue>) {
        let mut s = self.subs.lock().unwrap();
        if !s.iter().any(|x| Arc::ptr_eq(x, q)) {
            s.push(q.clone());
        }
    }

    pub fn has_subscribers(&self) -> bool {
        !self.subs.lock().unwrap().is_empty()
    }

    /// Forgets queues that moved to another stream.
    pub fn prune(&self) {
        self.subs.lock().unwrap().retain(|q| q.wants(self.id));
    }

    pub fn publish(&self, mut p: Packet) {
        p.src = self.id;
        for q in self.subs.lock().unwrap().iter() {
            q.push(&p);
        }
    }
}

/// Rate limit for live bitrate changes: every change makes NVENC emit an
/// IDR (a quality dip and a burst), so apply at most one per interval and
/// keep only the latest request.
pub struct BitrateGate {
    min_interval: Duration,
    current: u32,
    pending: Option<u32>,
    last: Option<Instant>,
}

impl BitrateGate {
    pub fn new(kbps: u32, min_interval: Duration) -> Self {
        BitrateGate {
            min_interval,
            current: kbps,
            pending: None,
            last: None,
        }
    }

    pub fn current(&self) -> u32 {
        self.current
    }

    pub fn request(&mut self, kbps: u32) {
        self.pending = (kbps != self.current).then_some(kbps);
    }

    /// The value to apply now, if any (call once per frame).
    pub fn poll(&mut self, now: Instant) -> Option<u32> {
        let k = self.pending?;
        if self.last.is_some_and(|t| now < t + self.min_interval) {
            return None;
        }
        self.pending = None;
        self.current = k;
        self.last = Some(now);
        Some(k)
    }
}

// ---------------- FFmpeg CLI fallback ----------------

/// Encoders as FFmpeg CLI processes (phone MKV in on stdin, FLV out on
/// stdout, parsed back into packets). Used when the FFmpeg libraries are
/// not available. Reconnects still never restart them; bitrate is fixed.
pub struct CliSource {
    ffmpeg: std::path::PathBuf,
    /// FFmpeg encoder names for H.264 and HEVC (from `live` probing).
    encoders: (String, String),
    config: Mutex<Option<crate::mkv::VideoConfig>>,
    procs: Mutex<Vec<Arc<CliEncoder>>>,
    request_phone_key: Arc<dyn Fn() + Send + Sync>,
}

pub struct CliEncoder {
    spec: EncodeSpec,
    feed: Mutex<Option<crate::sinks::SinkTx>>,
    stats: Mutex<EncoderStats>,
    child: Mutex<Option<std::process::Child>>,
}

impl CliSource {
    pub fn new(
        ffmpeg: std::path::PathBuf,
        encoders: (String, String),
        config: crate::mkv::VideoConfig,
        request_phone_key: Arc<dyn Fn() + Send + Sync>,
    ) -> Self {
        CliSource {
            ffmpeg,
            encoders,
            config: Mutex::new(Some(config)),
            procs: Mutex::new(Vec::new()),
            request_phone_key,
        }
    }

    fn args(&self, spec: &EncodeSpec) -> Vec<String> {
        let enc = match spec.key.codec {
            VCodec::H264 => &self.encoders.0,
            VCodec::Hevc => &self.encoders.1,
        };
        let (w, h) = (spec.key.w, spec.key.h);
        let (hwaccel, filters) =
            crate::live::video_filter(enc.contains("nvenc"), spec.framing, w, h);
        let mut a: Vec<String> = ["-hide_banner", "-loglevel", "error", "-nostats"]
            .iter()
            .chain(hwaccel)
            .chain(&["-fflags", "+genpts", "-f", "matroska", "-i", "pipe:0"])
            .map(|s| s.to_string())
            .collect();
        a.extend([
            "-filter_complex".into(),
            filters,
            "-map".into(),
            "[v]".into(),
        ]);
        a.extend(["-map".into(), "0:a:0".into()]);
        let preset = crate::live::generic_preset(w, h, spec.kbps, spec.key.codec == VCodec::Hevc);
        a.extend(crate::live::encoder_args(enc, &preset));
        a.extend(
            [
                "-c:a", "aac", "-b:a", "160k", "-ar", "48000", "-ac", "2", "-f", "flv", "pipe:1",
            ]
            .iter()
            .map(|s| s.to_string()),
        );
        a
    }
}

impl Source for CliSource {
    fn backend(&self) -> &'static str {
        "ffmpeg-cli"
    }

    fn live_bitrate(&self) -> bool {
        false
    }

    fn open(&self, spec: EncodeSpec, out: Arc<Stream>) -> Result<Arc<dyn VideoEncoder>, String> {
        use std::process::{Command, Stdio};
        let config = self
            .config
            .lock()
            .unwrap()
            .clone()
            .ok_or("No phone video yet")?;
        let mut cmd = Command::new(&self.ffmpeg);
        cmd.args(self.args(&spec))
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .stderr(Stdio::null());
        #[cfg(windows)]
        {
            use std::os::windows::process::CommandExt;
            cmd.creation_flags(0x0800_0000 | 0x0000_8000); // no window, above normal
        }
        let mut child = cmd
            .spawn()
            .map_err(|e| format!("Could not start FFmpeg: {e}"))?;
        let stdin = std::io::BufWriter::with_capacity(1 << 20, child.stdin.take().unwrap());
        let stdout = child.stdout.take().unwrap();
        let (tx, rx) = crate::sinks::SinkTx::new(2400);
        let enc = Arc::new(CliEncoder {
            spec,
            feed: Mutex::new(Some(tx)),
            stats: Mutex::new(EncoderStats {
                sessions: 1,
                kbps: spec.kbps,
                ..Default::default()
            }),
            child: Mutex::new(Some(child)),
        });
        // Feeder: phone media → MKV → FFmpeg stdin.
        std::thread::spawn(move || {
            let audio = Some((crate::audio::RATE as u32, 2));
            let Ok(mut w) = crate::mkv::MkvWriter::new(stdin, &config, audio, true) else {
                return;
            };
            while let Ok(m) = rx.recv() {
                let ok = match &m {
                    Media::Video { pts, key, data } => w.write_video(*pts, *key, data).is_ok(),
                    Media::Audio { pts, pcm } => w.write_audio(*pts, pcm).is_ok(),
                    Media::Config(_) => true,
                };
                if !ok {
                    break;
                }
            }
            w.end().ok();
        });
        // Reader: FFmpeg FLV → packets.
        let e2 = enc.clone();
        std::thread::spawn(move || {
            let mut r = super::flv::FlvReader::new(std::io::BufReader::new(stdout));
            let (mut vcfg, mut acfg) = (None, None);
            while let Ok(tag) = r.next_tag() {
                match super::flv::tag_to_packet(out.id, &tag) {
                    Some(Ok(mut p)) => {
                        if p.is_key() {
                            p.config = vcfg.clone();
                        } else if p.priority().is_none() {
                            p.config = acfg.clone();
                        }
                        if p.priority().is_some() {
                            e2.stats.lock().unwrap().frames += 1;
                        }
                        out.publish(p);
                    }
                    Some(Err((true, c))) => vcfg = Some(c),
                    Some(Err((false, c))) => acfg = Some(c),
                    None => {}
                }
            }
        });
        (self.request_phone_key)();
        self.procs.lock().unwrap().push(enc.clone());
        Ok(enc)
    }

    fn push(&self, m: &Media) {
        if let Media::Config(c) = m {
            *self.config.lock().unwrap() = Some(c.clone());
        }
        let mut want_key = false;
        for p in self.procs.lock().unwrap().iter() {
            if let Some(tx) = p.feed.lock().unwrap().as_mut() {
                want_key |= matches!(tx.push(m), crate::sinks::Push::NeedKeyframe);
            }
        }
        if want_key {
            (self.request_phone_key)();
        }
    }

    fn stop(&self) {
        for p in self.procs.lock().unwrap().drain(..) {
            p.close();
        }
    }
}

impl VideoEncoder for CliEncoder {
    fn spec(&self) -> EncodeSpec {
        self.spec
    }

    fn set_bitrate(&self, _kbps: u32) -> bool {
        false
    }

    fn request_idr(&self) {
        // The CLI cannot be asked; its 2 s GOP brings a keyframe soon.
        self.stats.lock().unwrap().idrs += 1;
    }

    fn stats(&self) -> EncoderStats {
        *self.stats.lock().unwrap()
    }

    fn close(&self) {
        // Closing stdin lets FFmpeg flush and exit; kill it if it lingers.
        self.feed.lock().unwrap().take();
        if let Some(mut c) = self.child.lock().unwrap().take() {
            std::thread::spawn(move || {
                let until = Instant::now() + Duration::from_secs(3);
                while Instant::now() < until {
                    if c.try_wait().ok().flatten().is_some() {
                        return;
                    }
                    std::thread::sleep(Duration::from_millis(50));
                }
                c.kill().ok();
                c.wait().ok();
            });
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn gate_rate_limits_and_keeps_latest() {
        let t0 = Instant::now();
        let ms = |m| t0 + Duration::from_millis(m);
        let mut g = BitrateGate::new(6000, Duration::from_secs(1));
        assert_eq!(g.poll(ms(0)), None);
        g.request(5000);
        assert_eq!(g.poll(ms(0)), Some(5000));
        g.request(4000);
        g.request(3500);
        assert_eq!(g.poll(ms(500)), None, "within the interval");
        assert_eq!(g.poll(ms(1000)), Some(3500), "latest request wins");
        g.request(3500);
        assert_eq!(g.poll(ms(5000)), None, "no-op request");
        assert_eq!(g.current(), 3500);
    }
}
