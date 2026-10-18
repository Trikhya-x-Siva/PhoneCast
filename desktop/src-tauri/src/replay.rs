//! Replay buffer: the last N seconds of the active source kept in memory as
//! the encoded video access units and the mixed audio, exactly as they would
//! go to the recorder. A hotkey turns the tail of it into an MP4 clip: the
//! video is copied as-is (no re-encode, no quality loss) and the PCM audio is
//! packed as AAC by FFmpeg. Without FFmpeg the clip is saved as MKV instead.
//!
//! The buffer always starts on a keyframe, so a clip can start cleanly, and it
//! is trimmed to the newest keyframe that still leaves N seconds, so memory is
//! bounded by N seconds plus one keyframe interval.

use crate::live::ClipLook;
use crate::mkv::{MkvWriter, VideoConfig, CODEC_H265};
use crate::sinks::Media;
use std::collections::VecDeque;
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};

#[cfg(windows)]
use std::os::windows::process::CommandExt;

pub const DEFAULT_SECONDS: u32 = 90;
pub const MAX_SECONDS: u32 = 300;
/// Hard memory ceiling: a 50 Mbps phone stream is ~6 MB/s, so 90 s is
/// ~560 MB; beyond this the buffer keeps fewer seconds instead of growing.
pub const MAX_BYTES: usize = 1200 << 20;

pub struct ReplayBuffer {
    cap_us: i64,
    config: Option<VideoConfig>,
    items: VecDeque<Media>,
    bytes: usize,
    last_video_pts: Option<i64>,
}

impl ReplayBuffer {
    /// `seconds` = 0 turns the buffer off.
    pub fn new(seconds: u32) -> Self {
        Self {
            cap_us: seconds.min(MAX_SECONDS) as i64 * 1_000_000,
            config: None,
            items: VecDeque::new(),
            bytes: 0,
            last_video_pts: None,
        }
    }

    pub fn seconds(&self) -> u32 {
        (self.cap_us / 1_000_000) as u32
    }

    pub fn enabled(&self) -> bool {
        self.cap_us > 0
    }

    pub fn set_seconds(&mut self, seconds: u32) {
        self.cap_us = seconds.min(MAX_SECONDS) as i64 * 1_000_000;
        if self.cap_us == 0 {
            self.clear();
        } else {
            self.trim();
        }
    }

    /// Memory held by the buffered media.
    pub fn bytes(&self) -> usize {
        self.bytes
    }

    /// Seconds of video currently buffered.
    pub fn buffered_secs(&self) -> f64 {
        match (self.first_video_pts(), self.last_video_pts) {
            (Some(a), Some(b)) if b > a => (b - a) as f64 / 1e6,
            _ => 0.0,
        }
    }

    pub fn push(&mut self, m: &Media) {
        if self.cap_us == 0 {
            return;
        }
        match m {
            Media::Config(c) => {
                if self.config.as_ref() != Some(c) {
                    // Rotation / new source: a clip cannot span two picture sizes.
                    self.clear();
                    self.config = Some(c.clone());
                }
            }
            Media::Video { pts, key, data } => {
                if self.config.is_none() || (self.items.is_empty() && !key) {
                    return; // must start on a keyframe
                }
                self.bytes += data.len();
                self.items.push_back(m.clone());
                self.last_video_pts = Some(*pts);
                self.trim();
            }
            Media::Audio { pcm, .. } => {
                if self.items.is_empty() {
                    return;
                }
                self.bytes += pcm.len();
                self.items.push_back(m.clone());
            }
        }
    }

    fn clear(&mut self) {
        self.items.clear();
        self.bytes = 0;
        self.last_video_pts = None;
    }

    fn first_video_pts(&self) -> Option<i64> {
        self.items.iter().find_map(|m| match m {
            Media::Video { pts, .. } => Some(*pts),
            _ => None,
        })
    }

    /// Index of the newest keyframe at or before `pts_limit`, if any.
    fn keyframe_at_or_before(&self, pts_limit: i64) -> Option<usize> {
        self.items
            .iter()
            .enumerate()
            .rev()
            .find_map(|(i, m)| match m {
                Media::Video { pts, key: true, .. } if *pts <= pts_limit => Some(i),
                _ => None,
            })
    }

    /// Drops everything before the newest keyframe that still leaves `cap`
    /// seconds in the buffer, then whole keyframe groups while over the
    /// memory ceiling.
    fn trim(&mut self) {
        let Some(last) = self.last_video_pts else {
            return;
        };
        if let Some(k) = self.keyframe_at_or_before(last - self.cap_us) {
            self.drop_front(k);
        }
        while self.bytes > MAX_BYTES {
            // The next keyframe after the first item starts the next group.
            let next = self
                .items
                .iter()
                .skip(1)
                .position(|m| matches!(m, Media::Video { key: true, .. }));
            match next {
                Some(i) => self.drop_front(i + 1),
                None => break, // one group only; nothing more can go
            }
        }
    }

    fn drop_front(&mut self, n: usize) {
        for m in self.items.drain(..n) {
            self.bytes -= match &m {
                Media::Video { data, .. } => data.len(),
                Media::Audio { pcm, .. } => pcm.len(),
                Media::Config(_) => 0,
            };
        }
    }

    /// The last `seconds` of media (a little more, back to a keyframe), ready
    /// to be written to a file. None when nothing is buffered yet.
    pub fn snapshot(&self, seconds: u32) -> Option<Snapshot> {
        let config = self.config.clone()?;
        let last = self.last_video_pts?;
        let start = self
            .keyframe_at_or_before(last - seconds as i64 * 1_000_000)
            .unwrap_or(0);
        let items: Vec<Media> = self.items.range(start..).cloned().collect();
        let first = items.iter().find_map(|m| match m {
            Media::Video { pts, .. } => Some(*pts),
            _ => None,
        })?;
        Some(Snapshot {
            config,
            items,
            seconds: (last - first) as f64 / 1e6,
        })
    }
}

pub struct Snapshot {
    pub config: VideoConfig,
    pub items: Vec<Media>,
    /// Video duration covered, in seconds.
    pub seconds: f64,
}

pub fn clips_dir() -> PathBuf {
    crate::core::recordings_dir().join("Clips")
}

/// No console window; a re-encoding clip also runs at below-normal
/// priority so a live stream on the same PC always comes first.
fn hide(cmd: &mut Command, background: bool) {
    #[cfg(windows)]
    cmd.creation_flags(0x0800_0000 | if background { 0x0000_4000 } else { 0 });
    #[cfg(not(windows))]
    let _ = (cmd, background);
}

impl Snapshot {
    /// Writes the clip into `dir`. Returns the file written.
    ///
    /// `look`: None copies the original picture (instant, no quality loss);
    /// Some re-encodes it to look like the stream (16:9 framing, stream size).
    pub fn save(
        &self,
        dir: &Path,
        ffmpeg: Option<&Path>,
        label: &str,
        look: Option<&ClipLook>,
    ) -> Result<PathBuf, String> {
        std::fs::create_dir_all(dir).map_err(|e| e.to_string())?;
        let stamp = chrono::Local::now().format("%Y-%m-%d_%H-%M-%S");
        let stem = format!("Clip_{stamp}_{label}");
        let mkv = dir.join(format!("{stem}.mkv"));
        let mut w = MkvWriter::create(&mkv, &self.config, Some((crate::audio::RATE as u32, 2)))
            .map_err(|e| e.to_string())?;
        for m in &self.items {
            let r = match m {
                Media::Video { pts, key, data } => w.write_video(*pts, *key, data),
                Media::Audio { pts, pcm } => w.write_audio(*pts, pcm),
                Media::Config(_) => Ok(()),
            };
            r.map_err(|e| e.to_string())?;
        }
        w.finish().map_err(|e| e.to_string())?;

        let Some(ffmpeg) = ffmpeg else {
            return Ok(mkv); // no FFmpeg: the MKV is the clip
        };
        let mp4 = dir.join(format!("{stem}.mp4"));
        let mut cmd = Command::new(ffmpeg);
        cmd.args(["-hide_banner", "-loglevel", "error", "-y"]);
        if let Some(l) = look {
            cmd.args(l.input_args());
        }
        cmd.arg("-i").arg(&mkv);
        match look {
            Some(l) => {
                cmd.args(l.output_args());
            }
            None => {
                cmd.args(["-map", "0:v:0", "-map", "0:a:0?", "-c:v", "copy"]);
                if self.config.codec == CODEC_H265 {
                    cmd.args(["-tag:v", "hvc1"]);
                }
            }
        }
        cmd.args(["-c:a", "aac", "-b:a", "192k", "-movflags", "+faststart"]);
        cmd.arg(&mp4);
        cmd.stdin(Stdio::null()).stdout(Stdio::null());
        hide(&mut cmd, look.is_some());
        let out = cmd
            .output()
            .map_err(|e| format!("Could not run FFmpeg: {e}"))?;
        if out.status.success() && mp4.exists() {
            std::fs::remove_file(&mkv).ok();
            Ok(mp4)
        } else {
            // Keep the MKV so the clip is never lost.
            let err = String::from_utf8_lossy(&out.stderr);
            crate::diag::log(
                "clip",
                format!("MP4 conversion failed, kept MKV: {}", err.trim()),
            );
            Ok(mkv)
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::Arc;

    const FRAME: i64 = 16_667;
    const GOP: usize = 120; // 2 s keyframes, like the phone encoder

    fn cfg() -> VideoConfig {
        VideoConfig {
            codec: crate::mkv::CODEC_H264,
            width: 1080,
            height: 2376,
            csd: vec![0, 0, 0, 1, 0x67, 100, 0, 40],
        }
    }

    /// Feeds `secs` seconds of 60 fps video (keyframe every 2 s) with 10 ms
    /// audio chunks, starting at `t0` microseconds.
    fn feed(b: &mut ReplayBuffer, t0: i64, secs: i64) -> i64 {
        let mut audio_t = t0;
        let frames = secs * 60;
        for i in 0..frames {
            let pts = t0 + i * FRAME;
            b.push(&Media::Video {
                pts,
                key: (i as usize).is_multiple_of(GOP),
                data: Arc::new(vec![0u8; 1000]),
            });
            while audio_t <= pts {
                b.push(&Media::Audio {
                    pts: audio_t,
                    pcm: Arc::new(vec![0u8; 1920]),
                });
                audio_t += 10_000;
            }
        }
        t0 + (frames - 1) * FRAME
    }

    fn first_is_keyframe(b: &ReplayBuffer) -> bool {
        matches!(b.items.front(), Some(Media::Video { key: true, .. }))
    }

    #[test]
    fn keeps_at_least_cap_and_starts_on_keyframe() {
        let mut b = ReplayBuffer::new(90);
        b.push(&Media::Config(cfg()));
        feed(&mut b, 5_000_000_000, 300);
        let d = b.buffered_secs();
        assert!(d >= 90.0, "only {d:.1}s buffered");
        assert!(d < 92.1, "{d:.1}s buffered: not trimmed to the GOP");
        assert!(first_is_keyframe(&b));
        // Memory accounting matches the contents.
        let sum: usize = b
            .items
            .iter()
            .map(|m| match m {
                Media::Video { data, .. } => data.len(),
                Media::Audio { pcm, .. } => pcm.len(),
                _ => 0,
            })
            .sum();
        assert_eq!(sum, b.bytes());
        // ~90 s x (60 kB/s video + 192 kB/s audio): well under 30 MB.
        assert!(b.bytes() < 30 << 20);
    }

    #[test]
    fn ignores_frames_before_the_first_keyframe_and_audio_before_video() {
        let mut b = ReplayBuffer::new(90);
        b.push(&Media::Config(cfg()));
        b.push(&Media::Audio {
            pts: 0,
            pcm: Arc::new(vec![0; 10]),
        });
        b.push(&Media::Video {
            pts: 1,
            key: false,
            data: Arc::new(vec![0; 10]),
        });
        assert!(b.items.is_empty());
        b.push(&Media::Video {
            pts: 2,
            key: true,
            data: Arc::new(vec![0; 10]),
        });
        assert_eq!(b.items.len(), 1);
    }

    #[test]
    fn snapshot_covers_requested_seconds_from_a_keyframe() {
        let mut b = ReplayBuffer::new(90);
        b.push(&Media::Config(cfg()));
        feed(&mut b, 0, 100);
        for want in [15u32, 30, 60] {
            let s = b.snapshot(want).unwrap();
            assert!(
                s.seconds >= want as f64,
                "{want}s asked, got {:.1}",
                s.seconds
            );
            assert!(
                s.seconds < want as f64 + 2.1,
                "{want}s asked, got {:.1}",
                s.seconds
            );
            assert!(matches!(s.items[0], Media::Video { key: true, .. }));
            // Audio travels with the video.
            assert!(s.items.iter().any(|m| matches!(m, Media::Audio { .. })));
        }
        // Asking for more than is buffered gives the whole buffer.
        let s = b.snapshot(600).unwrap();
        assert!((s.seconds - b.buffered_secs()).abs() < 0.001);
    }

    #[test]
    fn short_buffer_gives_what_it_has() {
        let mut b = ReplayBuffer::new(90);
        b.push(&Media::Config(cfg()));
        feed(&mut b, 0, 5);
        let s = b.snapshot(30).unwrap();
        assert!(s.seconds > 4.5 && s.seconds < 5.0);
        assert!(ReplayBuffer::new(90).snapshot(30).is_none());
    }

    #[test]
    fn config_change_clears_and_zero_disables() {
        let mut b = ReplayBuffer::new(60);
        b.push(&Media::Config(cfg()));
        feed(&mut b, 0, 20);
        assert!(b.buffered_secs() > 19.0);
        let mut rotated = cfg();
        rotated.width = 2376;
        rotated.height = 1080;
        b.push(&Media::Config(rotated));
        assert_eq!(b.bytes(), 0);
        assert!(b.snapshot(10).is_none());
        feed(&mut b, 100_000_000, 10);
        assert!(b.buffered_secs() > 9.0);

        b.set_seconds(0);
        assert!(!b.enabled());
        assert_eq!(b.bytes(), 0);
        feed(&mut b, 200_000_000, 3);
        assert_eq!(b.bytes(), 0);

        b.set_seconds(30);
        b.push(&Media::Config(cfg()));
        feed(&mut b, 300_000_000, 50);
        assert!(b.buffered_secs() >= 30.0 && b.buffered_secs() < 32.1);
        // Shrinking trims right away.
        b.set_seconds(10);
        assert!(b.buffered_secs() >= 10.0 && b.buffered_secs() < 12.1);
        assert!(first_is_keyframe(&b));
    }

    #[test]
    fn memory_ceiling_keeps_fewer_seconds() {
        let mut b = ReplayBuffer::new(90);
        b.push(&Media::Config(cfg()));
        // 2 MB frames at 60 fps = 120 MB/s: the 1.2 GB ceiling holds ~10 s.
        let mut t = 0i64;
        for i in 0..(30 * 60) {
            b.push(&Media::Video {
                pts: t,
                key: (i as usize).is_multiple_of(GOP),
                data: Arc::new(vec![0u8; 2 << 20]),
            });
            t += FRAME;
        }
        assert!(b.bytes() <= MAX_BYTES, "{} bytes", b.bytes());
        assert!(
            b.buffered_secs() >= 8.0 && b.buffered_secs() <= 12.0,
            "{}",
            b.buffered_secs()
        );
        assert!(first_is_keyframe(&b));
    }

    #[test]
    fn saves_a_playable_mkv_without_ffmpeg() {
        let mut b = ReplayBuffer::new(30);
        b.push(&Media::Config(cfg()));
        feed(&mut b, 7_000_000_000, 10);
        let dir = std::env::temp_dir().join("phonecast_clip_test");
        std::fs::remove_dir_all(&dir).ok();
        let path = b.snapshot(5).unwrap().save(&dir, None, "5s", None).unwrap();
        assert_eq!(path.extension().unwrap(), "mkv");
        let size = std::fs::metadata(&path).unwrap().len();
        // 5 s of 1 kB frames + 192 kB/s audio, about 1.26 MB.
        assert!(size > 1_000_000, "{size} bytes");
        std::fs::remove_dir_all(&dir).ok();
    }

    /// `(width, height, seconds, has_audio)` of a media file, via ffprobe.
    fn probe(ffmpeg: &Path, file: &Path) -> (u32, u32, f64, bool) {
        let ffprobe = ffmpeg.with_file_name("ffprobe.exe");
        let out = Command::new(ffprobe)
            .args([
                "-v",
                "error",
                "-show_entries",
                "stream=codec_type,width,height:format=duration",
                "-of",
                "default=nw=1",
            ])
            .arg(file)
            .output()
            .unwrap();
        let text = String::from_utf8_lossy(&out.stdout);
        let field = |k: &str| {
            text.lines()
                .find_map(|l| l.strip_prefix(&format!("{k}=")))
                .unwrap_or("0")
                .to_string()
        };
        (
            field("width").parse().unwrap_or(0),
            field("height").parse().unwrap_or(0),
            field("duration").parse().unwrap_or(0.0),
            text.contains("codec_type=audio"),
        )
    }

    /// Average brightness (0-255) of the top 100 rows of a frame 3 s in.
    /// (A phone picture is wider than 16:9, so any bars are above and below.)
    fn top_edge_luma(ffmpeg: &Path, file: &Path) -> f64 {
        let out = Command::new(ffmpeg)
            .args(["-hide_banner", "-ss", "3", "-i"])
            .arg(file)
            .args([
                "-frames:v",
                "1",
                "-vf",
                "crop=iw:100:0:0,signalstats,metadata=print:key=lavfi.signalstats.YAVG",
                "-f",
                "null",
                "-",
            ])
            .output()
            .unwrap();
        let text = String::from_utf8_lossy(&out.stderr);
        text.lines()
            .find_map(|l| l.split("lavfi.signalstats.YAVG=").nth(1))
            .and_then(|v| v.trim().parse().ok())
            .unwrap_or(-1.0)
    }

    /// A real H.264 phone-shaped stream (2376x1080, 60 fps, 2 s keyframes)
    /// through the replay buffer, saved both ways. Needs FFmpeg (and a GPU
    /// for the fast path). `cargo test clip_pictures -- --ignored --nocapture`
    #[test]
    #[ignore]
    fn clip_pictures_original_and_like_the_stream() {
        let ffmpeg = crate::live::find_ffmpeg().expect("ffmpeg");
        let enc = crate::live::best_h264(&ffmpeg);
        println!("encoder: {enc}");
        let raw = Command::new(&ffmpeg)
            .args([
                "-hide_banner",
                "-loglevel",
                "error",
                "-f",
                "lavfi",
                "-i",
                "testsrc2=size=2376x1080:rate=60",
                "-t",
                "12",
                "-c:v",
                &enc,
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
            ])
            .stdin(Stdio::null())
            .output()
            .unwrap()
            .stdout;
        // Split into access units the way the phone sends them.
        let mut csd = Vec::new();
        let mut aus: Vec<(bool, Vec<u8>)> = Vec::new();
        for n in crate::mkv::split_nals(&raw) {
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
        let mut b = ReplayBuffer::new(90);
        b.push(&Media::Config(VideoConfig {
            codec: crate::mkv::CODEC_H264,
            width: 2376,
            height: 1080,
            csd,
        }));
        let t0 = 9_000_000_000i64; // phone clocks don't start at zero
        let mut audio_t = t0;
        for (i, (key, au)) in aus.iter().enumerate() {
            let pts = t0 + i as i64 * FRAME;
            b.push(&Media::Video {
                pts,
                key: *key,
                data: Arc::new(au.clone()),
            });
            while audio_t <= pts {
                // 440 Hz tone, s16le stereo, 10 ms.
                let pcm: Vec<u8> = (0..480)
                    .flat_map(|k| {
                        let t = ((audio_t - t0) / 10_000 * 480 + k) as f32 / 48_000.0;
                        let v = ((t * 440.0 * std::f32::consts::TAU).sin() * 8000.0) as i16;
                        [v.to_le_bytes(), v.to_le_bytes()].concat()
                    })
                    .collect();
                b.push(&Media::Audio {
                    pts: audio_t,
                    pcm: Arc::new(pcm),
                });
                audio_t += 10_000;
            }
        }
        let snap = b.snapshot(8).unwrap();
        println!("snapshot covers {:.2} s", snap.seconds);
        let dir = std::env::temp_dir().join("phonecast_clip_pictures");
        std::fs::remove_dir_all(&dir).ok();

        // Original: the phone picture copied as-is.
        let original = snap.save(&dir, Some(&ffmpeg), "orig", None).unwrap();
        let (w, h, secs, audio) = probe(&ffmpeg, &original);
        println!(
            "original: {} {w}x{h} {secs:.2} s audio={audio}",
            original.display()
        );
        assert_eq!(original.extension().unwrap(), "mp4");
        assert_eq!((w, h), (2376, 1080));
        assert!(secs > 7.9 && secs < 10.5, "{secs}");
        assert!(audio);

        // Like the stream: YouTube's 1440p60 at 20 Mbps, blurred background.
        let look = ClipLook {
            encoder: enc.clone(),
            framing: crate::live::Framing::Blur,
            w: 2560,
            h: 1440,
            kbps: 20_000,
        };
        let started = std::time::Instant::now();
        let framed = snap
            .save(&dir, Some(&ffmpeg), "stream", Some(&look))
            .unwrap();
        let (w, h, secs, audio) = probe(&ffmpeg, &framed);
        println!(
            "like the stream: {} {w}x{h} {secs:.2} s audio={audio}, saved in {:?}",
            framed.display(),
            started.elapsed()
        );
        assert_eq!(
            framed.extension().unwrap(),
            "mp4",
            "re-encode failed, MKV kept"
        );
        assert_eq!((w, h), (2560, 1440));
        assert!(secs > 7.9 && secs < 10.5, "{secs}");
        assert!(audio);
        // Above and below the picture is the blurred copy, not black bars
        // (black is ~16).
        let edge = top_edge_luma(&ffmpeg, &framed);
        println!("top-edge brightness: {edge:.1}");
        assert!(edge > 30.0, "bars look black: {edge}");

        // Fit framing for comparison: real black bars.
        let fit = ClipLook {
            framing: crate::live::Framing::Fit,
            ..look
        };
        let bars = snap.save(&dir, Some(&ffmpeg), "fit", Some(&fit)).unwrap();
        let edge = top_edge_luma(&ffmpeg, &bars);
        println!("fit top-edge brightness: {edge:.1}");
        assert!(edge < 20.0, "fit should have black bars: {edge}");
        std::fs::remove_dir_all(&dir).ok();
    }
}
