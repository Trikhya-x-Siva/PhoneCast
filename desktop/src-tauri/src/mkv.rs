//! Minimal streaming Matroska (MKV) writer.
//!
//! Stores the phone's H.264/H.265 video and raw PCM audio exactly as received
//! (no re-encode, no quality loss), with the phone's own timestamps so audio
//! and video stay in sync. Clusters are written as they complete, so a crash
//! still leaves a playable file.

use std::fs::File;
use std::io::{BufWriter, Seek, SeekFrom, Write};
use std::path::Path;

pub const CODEC_H264: u8 = 1;
pub const CODEC_H265: u8 = 2;

#[derive(Clone, PartialEq)]
pub struct VideoConfig {
    pub codec: u8,
    pub width: u16,
    pub height: u16,
    /// Annex-B parameter sets (SPS/PPS, plus VPS for H.265).
    pub csd: Vec<u8>,
}

pub struct MkvWriter<W: Write = BufWriter<File>> {
    out: W,
    /// Live mode: every block is flushed immediately (for feeding FFmpeg).
    live: bool,
    segment_size_pos: u64,
    segment_data_start: u64,
    duration_pos: u64,
    base_pts_us: Option<i64>,
    cluster: Vec<u8>,
    cluster_ms: i64,
    cluster_open: bool,
    last_ms: i64,
    last_video_us: Option<i64>,
    pub bytes_written: u64,
}

/// Expected frame interval. Phones deliver 60 fps games in an uneven
/// 8/25 ms rhythm (60 fps content on a 120 Hz screen); see `smooth_pts`.
const FRAME_US: i64 = 16_667;

const TRACK_VIDEO: u8 = 1;
const TRACK_AUDIO: u8 = 2;

impl MkvWriter<BufWriter<File>> {
    /// A recording file: clusters per keyframe, seekable, finalized on finish().
    pub fn create(
        path: &Path,
        video: &VideoConfig,
        audio: Option<(u32, u8)>,
    ) -> std::io::Result<Self> {
        let out = BufWriter::with_capacity(1 << 20, File::create(path)?);
        Self::new(out, video, audio, false)
    }

    pub fn finish(mut self) -> std::io::Result<()> {
        self.flush_cluster()?;
        self.out.flush()?;
        let end = self.out.stream_position()?;
        let size = end - self.segment_data_start;
        let mut sz = [0u8; 8];
        sz[0] = 0x01;
        sz[1..].copy_from_slice(&size.to_be_bytes()[1..]);
        self.out.seek(SeekFrom::Start(self.segment_size_pos))?;
        self.out.write_all(&sz)?;
        self.out.seek(SeekFrom::Start(self.duration_pos))?;
        self.out.write_all(&(self.last_ms as f64).to_be_bytes())?;
        self.out.flush()
    }
}

impl<W: Write> MkvWriter<W> {
    /// `live`: flush every block right away (low latency into a pipe).
    pub fn new(
        mut out: W,
        video: &VideoConfig,
        audio: Option<(u32, u8)>,
        live: bool,
    ) -> std::io::Result<Self> {
        // EBML header
        let mut h = Vec::new();
        uint_el(&mut h, 0x4286, 1); // EBMLVersion
        uint_el(&mut h, 0x42F7, 1); // EBMLReadVersion
        uint_el(&mut h, 0x42F2, 4); // EBMLMaxIDLength
        uint_el(&mut h, 0x42F3, 8); // EBMLMaxSizeLength
        bin_el(&mut h, 0x4282, b"matroska"); // DocType
        uint_el(&mut h, 0x4287, 4); // DocTypeVersion
        uint_el(&mut h, 0x4285, 2); // DocTypeReadVersion
        let mut buf = Vec::new();
        master(&mut buf, 0x1A45DFA3, &h);

        // Segment with size patched in finish()
        write_id(&mut buf, 0x18538067);
        let segment_size_pos = buf.len() as u64;
        buf.extend_from_slice(&[0x01, 0xFF, 0xFF, 0xFF, 0xFF, 0xFF, 0xFF, 0xFF]);
        let segment_data_start = buf.len() as u64;

        // Info (Duration patched in finish())
        let mut info = Vec::new();
        uint_el(&mut info, 0x2AD7B1, 1_000_000); // TimestampScale: 1 ms
        bin_el(&mut info, 0x4D80, b"PhoneCast");
        bin_el(&mut info, 0x5741, b"PhoneCast");
        write_id(&mut info, 0x4489);
        write_size(&mut info, 8);
        let duration_in_info = info.len();
        info.extend_from_slice(&0f64.to_be_bytes());
        write_id(&mut buf, 0x1549A966);
        write_size(&mut buf, info.len() as u64);
        let duration_pos = (buf.len() + duration_in_info) as u64;
        buf.extend_from_slice(&info);

        // Tracks
        let mut tracks = Vec::new();
        let mut v = Vec::new();
        uint_el(&mut v, 0xD7, TRACK_VIDEO as u64);
        uint_el(&mut v, 0x73C5, 1);
        uint_el(&mut v, 0x83, 1); // video
        uint_el(&mut v, 0x9C, 0); // FlagLacing off
        if video.codec == CODEC_H265 {
            bin_el(&mut v, 0x86, b"V_MPEGH/ISO/HEVC");
            bin_el(&mut v, 0x63A2, &hvcc(&video.csd));
        } else {
            bin_el(&mut v, 0x86, b"V_MPEG4/ISO/AVC");
            bin_el(&mut v, 0x63A2, &avcc(&video.csd));
        }
        let mut vv = Vec::new();
        uint_el(&mut vv, 0xB0, video.width as u64);
        uint_el(&mut vv, 0xBA, video.height as u64);
        master(&mut v, 0xE0, &vv);
        master(&mut tracks, 0xAE, &v);

        if let Some((rate, channels)) = audio {
            let mut a = Vec::new();
            uint_el(&mut a, 0xD7, TRACK_AUDIO as u64);
            uint_el(&mut a, 0x73C5, 2);
            uint_el(&mut a, 0x83, 2); // audio
            uint_el(&mut a, 0x9C, 0);
            bin_el(&mut a, 0x86, b"A_PCM/INT/LIT");
            let mut aa = Vec::new();
            float_el(&mut aa, 0xB5, rate as f64);
            uint_el(&mut aa, 0x9F, channels as u64);
            uint_el(&mut aa, 0x6264, 16);
            master(&mut a, 0xE1, &aa);
            master(&mut tracks, 0xAE, &a);
        }
        master(&mut buf, 0x1654AE6B, &tracks);

        out.write_all(&buf)?;
        Ok(Self {
            out,
            live,
            segment_size_pos,
            segment_data_start,
            duration_pos,
            base_pts_us: None,
            cluster: Vec::new(),
            cluster_ms: 0,
            cluster_open: false,
            last_ms: 0,
            last_video_us: None,
            bytes_written: buf.len() as u64,
        })
    }

    pub fn write_video(
        &mut self,
        pts_us: i64,
        keyframe: bool,
        annexb: &[u8],
    ) -> std::io::Result<()> {
        if self.base_pts_us.is_none() {
            if !keyframe {
                return Ok(()); // a file must start on a keyframe
            }
            self.base_pts_us = Some(pts_us);
        }
        let pts_us = self.smooth_pts(pts_us);
        let ms = self.ms(pts_us);
        if ms < 0 {
            return Ok(());
        }
        if !self.cluster_open || keyframe || ms - self.cluster_ms > 5000 {
            self.flush_cluster()?;
            self.open_cluster(ms);
        }
        self.block(
            TRACK_VIDEO,
            ms,
            keyframe,
            &annexb_to_length_prefixed(annexb),
        );
        if self.live {
            self.flush_cluster()?;
            self.out.flush()?;
        }
        Ok(())
    }

    pub fn write_audio(&mut self, pts_us: i64, pcm: &[u8]) -> std::io::Result<()> {
        if self.base_pts_us.is_none() {
            return Ok(());
        }
        let ms = self.ms(pts_us);
        if ms < 0 {
            return Ok(());
        }
        if self.live {
            self.open_cluster(ms);
            self.block(TRACK_AUDIO, ms, true, pcm);
            self.flush_cluster()?;
            return self.out.flush();
        }
        if !self.cluster_open || (ms - self.cluster_ms).abs() > 30_000 {
            return Ok(());
        }
        self.block(TRACK_AUDIO, ms, true, pcm);
        Ok(())
    }

    /// Ends a live stream (no seeking back: pipes can't).
    pub fn end(mut self) -> std::io::Result<()> {
        self.flush_cluster()?;
        self.out.flush()
    }

    /// Puts frames that arrive in an uneven rhythm on a steady 60 fps grid.
    /// A frame within ¾ of a frame of where it "should" be is placed exactly
    /// on the grid (nudged 1/16 toward its real time so the grid follows the
    /// phone's clock and audio stays in sync). Anything further off — a real
    /// pause, or a game running at 30 fps — keeps its true timestamp.
    fn smooth_pts(&mut self, pts_us: i64) -> i64 {
        let out = match self.last_video_us {
            Some(prev) => {
                let ideal = prev + FRAME_US;
                let diff = pts_us - ideal;
                if diff.abs() <= FRAME_US * 3 / 4 {
                    ideal + diff / 16
                } else {
                    pts_us
                }
            }
            None => pts_us,
        };
        self.last_video_us = Some(out);
        out
    }

    fn ms(&self, pts_us: i64) -> i64 {
        (pts_us - self.base_pts_us.unwrap_or(pts_us)) / 1000
    }

    fn open_cluster(&mut self, ms: i64) {
        self.cluster.clear();
        uint_el(&mut self.cluster, 0xE7, ms.max(0) as u64);
        self.cluster_ms = ms;
        self.cluster_open = true;
    }

    fn flush_cluster(&mut self) -> std::io::Result<()> {
        if !self.cluster_open {
            return Ok(());
        }
        let mut head = Vec::new();
        write_id(&mut head, 0x1F43B675);
        write_size(&mut head, self.cluster.len() as u64);
        self.out.write_all(&head)?;
        self.out.write_all(&self.cluster)?;
        self.bytes_written += (head.len() + self.cluster.len()) as u64;
        self.cluster_open = false;
        Ok(())
    }

    fn block(&mut self, track: u8, ms: i64, key: bool, data: &[u8]) {
        let rel = (ms - self.cluster_ms).clamp(i16::MIN as i64, i16::MAX as i64) as i16;
        write_id(&mut self.cluster, 0xA3);
        write_size(&mut self.cluster, 4 + data.len() as u64);
        self.cluster.push(0x80 | track);
        self.cluster.extend_from_slice(&rel.to_be_bytes());
        self.cluster.push(if key { 0x80 } else { 0x00 });
        self.cluster.extend_from_slice(data);
        self.last_ms = self.last_ms.max(ms);
    }
}

// ---------- EBML helpers ----------

fn write_id(out: &mut Vec<u8>, id: u32) {
    let b = id.to_be_bytes();
    let skip = b.iter().take_while(|&&x| x == 0).count();
    out.extend_from_slice(&b[skip..]);
}

fn write_size(out: &mut Vec<u8>, size: u64) {
    let mut len = 1;
    while len < 8 && size >= (1u64 << (7 * len)) - 1 {
        len += 1;
    }
    let v = size | (1u64 << (7 * len));
    out.extend_from_slice(&v.to_be_bytes()[8 - len..]);
}

fn uint_el(out: &mut Vec<u8>, id: u32, v: u64) {
    let b = v.to_be_bytes();
    let skip = b.iter().take_while(|&&x| x == 0).count().min(7);
    write_id(out, id);
    write_size(out, (8 - skip) as u64);
    out.extend_from_slice(&b[skip..]);
}

fn float_el(out: &mut Vec<u8>, id: u32, v: f64) {
    write_id(out, id);
    write_size(out, 8);
    out.extend_from_slice(&v.to_be_bytes());
}

fn bin_el(out: &mut Vec<u8>, id: u32, data: &[u8]) {
    write_id(out, id);
    write_size(out, data.len() as u64);
    out.extend_from_slice(data);
}

fn master(out: &mut Vec<u8>, id: u32, children: &[u8]) {
    bin_el(out, id, children);
}

// ---------- H.264 / H.265 helpers ----------

/// Splits an Annex-B byte stream into NAL units (without start codes).
pub fn split_nals(data: &[u8]) -> Vec<&[u8]> {
    let mut starts = Vec::new();
    let mut i = 0;
    while i + 3 <= data.len() {
        if data[i] == 0 && data[i + 1] == 0 && data[i + 2] == 1 {
            starts.push((i, i + 3));
            i += 3;
        } else {
            i += 1;
        }
    }
    let mut nals = Vec::new();
    for (k, &(_, begin)) in starts.iter().enumerate() {
        let mut end = starts.get(k + 1).map(|s| s.0).unwrap_or(data.len());
        while end > begin && data[end - 1] == 0 {
            end -= 1; // trailing zero of a 4-byte start code
        }
        if end > begin {
            nals.push(&data[begin..end]);
        }
    }
    if nals.is_empty() && !data.is_empty() {
        nals.push(data);
    }
    nals
}

fn annexb_to_length_prefixed(data: &[u8]) -> Vec<u8> {
    let mut out = Vec::with_capacity(data.len() + 16);
    for nal in split_nals(data) {
        out.extend_from_slice(&(nal.len() as u32).to_be_bytes());
        out.extend_from_slice(nal);
    }
    out
}

fn avcc(csd: &[u8]) -> Vec<u8> {
    let nals = split_nals(csd);
    let sps = nals
        .iter()
        .find(|n| n[0] & 0x1F == 7)
        .copied()
        .unwrap_or(&[0x67, 100, 0, 40]);
    let pps = nals
        .iter()
        .find(|n| n[0] & 0x1F == 8)
        .copied()
        .unwrap_or(&[]);
    let mut out = vec![1, sps[1], sps[2], sps[3], 0xFF, 0xE1];
    out.extend_from_slice(&(sps.len() as u16).to_be_bytes());
    out.extend_from_slice(sps);
    out.push(1);
    out.extend_from_slice(&(pps.len() as u16).to_be_bytes());
    out.extend_from_slice(pps);
    out
}

/// Removes emulation-prevention bytes (00 00 03 -> 00 00).
fn unescape(nal: &[u8]) -> Vec<u8> {
    let mut out = Vec::with_capacity(nal.len());
    let mut zeros = 0;
    for &b in nal {
        if zeros >= 2 && b == 3 {
            zeros = 0;
            continue;
        }
        zeros = if b == 0 { zeros + 1 } else { 0 };
        out.push(b);
    }
    out
}

fn hvcc(csd: &[u8]) -> Vec<u8> {
    let nals = split_nals(csd);
    let ty = |n: &[u8]| (n[0] >> 1) & 0x3F;
    let sps = nals.iter().find(|n| ty(n) == 33).copied().unwrap_or(&[]);

    // general_profile_tier_level from the SPS (after the 2-byte NAL header).
    let rbsp = if sps.len() > 2 {
        unescape(&sps[2..])
    } else {
        Vec::new()
    };
    let (sub_layers, nested, ptl) = if rbsp.len() >= 13 {
        ((rbsp[0] >> 1) & 0x07, rbsp[0] & 1, rbsp[1..13].to_vec())
    } else {
        // Main profile, level 5.1 fallback
        (0, 1, vec![0x01, 0x60, 0, 0, 0, 0x90, 0, 0, 0, 0, 0, 153])
    };

    let mut out = vec![1];
    out.extend_from_slice(&ptl); // profile byte, 4 compat, 6 constraint, level
    out.extend_from_slice(&[0xF0, 0x00, 0xFC, 0xFD, 0xF8, 0xF8, 0x00, 0x00]);
    out.push(((sub_layers + 1) << 3) | (nested << 2) | 3);

    let arrays: Vec<(u8, Vec<&[u8]>)> = [32u8, 33, 34]
        .iter()
        .map(|&t| (t, nals.iter().copied().filter(|n| ty(n) == t).collect()))
        .filter(|(_, v): &(u8, Vec<&[u8]>)| !v.is_empty())
        .collect();
    out.push(arrays.len() as u8);
    for (t, list) in arrays {
        out.push(0x80 | t);
        out.extend_from_slice(&(list.len() as u16).to_be_bytes());
        for n in list {
            out.extend_from_slice(&(n.len() as u16).to_be_bytes());
            out.extend_from_slice(n);
        }
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn sizes() {
        let mut v = Vec::new();
        write_size(&mut v, 5);
        assert_eq!(v, [0x85]);
        v.clear();
        write_size(&mut v, 127);
        assert_eq!(v, [0x40, 0x7F]);
        v.clear();
        write_size(&mut v, 300);
        assert_eq!(v, [0x41, 0x2C]);
    }

    /// End-to-end: feed a real Annex-B stream (one access unit per AUD) plus
    /// PCM into the writer. Run with
    /// `PHONECAST_SAMPLE=<file.h264|.h265> cargo test -- --ignored`.
    #[test]
    #[ignore]
    fn writes_playable_file() {
        let src = std::env::var("PHONECAST_SAMPLE").expect("PHONECAST_SAMPLE");
        let hevc = src.ends_with(".h265");
        let data = std::fs::read(&src).unwrap();
        let is_aud = |n: &[u8]| {
            if hevc {
                (n[0] >> 1) & 0x3F == 35
            } else {
                n[0] & 0x1F == 9
            }
        };
        let is_ps = |n: &[u8]| {
            if hevc {
                matches!((n[0] >> 1) & 0x3F, 32..=34)
            } else {
                matches!(n[0] & 0x1F, 7 | 8)
            }
        };
        let is_key = |n: &[u8]| {
            if hevc {
                matches!((n[0] >> 1) & 0x3F, 16..=21)
            } else {
                n[0] & 0x1F == 5
            }
        };

        // Group NALs into access units, pulling parameter sets out like Android does.
        let mut csd = Vec::new();
        let mut aus: Vec<(bool, Vec<u8>)> = Vec::new();
        for n in split_nals(&data) {
            if is_aud(n) {
                aus.push((false, Vec::new()));
                continue;
            }
            if is_ps(n) {
                if aus.len() <= 1 {
                    csd.extend_from_slice(&[0, 0, 0, 1]);
                    csd.extend_from_slice(n);
                }
                continue;
            }
            let au = aus.last_mut().unwrap();
            au.0 |= is_key(n);
            au.1.extend_from_slice(&[0, 0, 0, 1]);
            au.1.extend_from_slice(n);
        }
        let cfg = VideoConfig {
            codec: if hevc { CODEC_H265 } else { CODEC_H264 },
            width: 1280,
            height: 720,
            csd,
        };
        let out = std::path::PathBuf::from(format!("{src}.mkv"));
        let mut w = MkvWriter::create(&out, &cfg, Some((48_000, 2))).unwrap();
        let base = 5_000_000_000i64; // phone clocks don't start at zero
        let mut audio_t = base;
        for (i, (key, au)) in aus.iter().enumerate().filter(|(_, a)| !a.1.is_empty()) {
            let pts = base + i as i64 * 16_667;
            w.write_video(pts, *key, au).unwrap();
            while audio_t < pts + 16_667 {
                let pcm: Vec<u8> = (0..480)
                    .flat_map(|k| {
                        let s = ((((audio_t / 1000) as f64 + k as f64 / 48.0)
                            * 0.44
                            * std::f64::consts::TAU)
                            .sin()
                            * 8000.0) as i16;
                        [s.to_le_bytes(), s.to_le_bytes()].concat()
                    })
                    .collect();
                w.write_audio(audio_t, &pcm).unwrap();
                audio_t += 10_000;
            }
        }
        w.finish().unwrap();
        println!("wrote {}", out.display());
    }

    #[test]
    fn smooths_uneven_rhythm_but_keeps_real_pauses() {
        let path = std::env::temp_dir().join("phonecast_smooth_test.mkv");
        let cfg = VideoConfig {
            codec: CODEC_H264,
            width: 16,
            height: 16,
            csd: vec![0, 0, 0, 1, 0x67, 100, 0, 40],
        };
        let mut w = MkvWriter::create(&path, &cfg, None).unwrap();
        // The phone's measured pattern: 8 ms, 25 ms, 8 ms, 25 ms…
        let mut pts = 1_000_000i64;
        let mut out = vec![w.smooth_pts(pts)];
        for i in 0..200 {
            pts += if i % 2 == 0 { 8_300 } else { 25_000 };
            out.push(w.smooth_pts(pts));
        }
        let gaps: Vec<i64> = out.windows(2).map(|p| p[1] - p[0]).collect();
        assert!(
            gaps[4..].iter().all(|g| (g - FRAME_US).abs() < 1_500),
            "{gaps:?}"
        );
        // Stays locked to the real clock (audio sync): drift under one frame.
        assert!((out.last().unwrap() - pts).abs() < FRAME_US);

        // A real 200 ms freeze is kept, not hidden.
        pts += 200_000;
        let before = *out.last().unwrap();
        assert!(w.smooth_pts(pts) - before > 150_000);
        drop(w);
        std::fs::remove_file(path).ok();
    }

    #[test]
    fn nal_split() {
        let d = [0, 0, 0, 1, 0x67, 1, 2, 0, 0, 1, 0x68, 3];
        let n = split_nals(&d);
        assert_eq!(n, vec![&[0x67, 1, 2][..], &[0x68, 3][..]]);
    }
}
