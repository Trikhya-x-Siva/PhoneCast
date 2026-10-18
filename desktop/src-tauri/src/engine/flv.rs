//! FLV/RTMP media payloads: H.264 (classic), HEVC (enhanced RTMP, `hvc1`)
//! and AAC, plus turning encoder output (Annex-B) into length-prefixed
//! frames with their decoder configuration record, and a small FLV stream
//! reader for the FFmpeg CLI fallback.

use super::{Bytes, Kind, Packet, Priority, VCodec};
use crate::mkv::split_nals;
use std::io::Read;

/// `hvc1` as an enhanced-RTMP FourCC.
pub const HVC1: [u8; 4] = *b"hvc1";

/// RTMP video message body.
pub fn video_tag(codec: VCodec, key: bool, seq_header: bool, cts_ms: i32, data: &[u8]) -> Vec<u8> {
    let frame_type: u8 = if key || seq_header { 1 } else { 2 };
    let cts = cts_ms.clamp(-(1 << 23), (1 << 23) - 1);
    let si24 = (cts as u32).to_be_bytes();
    let mut out = Vec::with_capacity(data.len() + 8);
    match codec {
        VCodec::H264 => {
            out.push(frame_type << 4 | 7);
            out.push(if seq_header { 0 } else { 1 });
            out.extend_from_slice(&si24[1..]);
        }
        VCodec::Hevc => {
            // Enhanced RTMP: IsExHeader | FrameType | PacketType, then FourCC.
            let packet_type: u8 = match (seq_header, cts) {
                (true, _) => 0,  // SequenceStart
                (false, 0) => 3, // CodedFramesX (no composition time)
                (false, _) => 1, // CodedFrames
            };
            out.push(0x80 | frame_type << 4 | packet_type);
            out.extend_from_slice(&HVC1);
            if packet_type == 1 {
                out.extend_from_slice(&si24[1..]);
            }
        }
    }
    out.extend_from_slice(data);
    out
}

/// RTMP audio message body for AAC.
pub fn audio_tag(seq_header: bool, data: &[u8]) -> Vec<u8> {
    let mut out = Vec::with_capacity(data.len() + 2);
    out.push(0xAF); // AAC, 44 kHz flag (always for AAC), 16-bit, stereo
    out.push(if seq_header { 0 } else { 1 });
    out.extend_from_slice(data);
    out
}

/// AudioSpecificConfig for AAC-LC.
pub fn aac_config(rate: u32, channels: u8) -> Vec<u8> {
    let idx = [
        96000, 88200, 64000, 48000, 44100, 32000, 24000, 22050, 16000,
    ]
    .iter()
    .position(|&r| r == rate)
    .unwrap_or(3) as u8;
    let object_type = 2u8; // AAC-LC
    vec![object_type << 3 | idx >> 1, (idx & 1) << 7 | channels << 3]
}

/// Video stream description for `onMetaData`.
pub struct Meta {
    pub codec: VCodec,
    pub width: u32,
    pub height: u32,
    pub fps: u32,
    pub video_kbps: u32,
    pub audio_kbps: u32,
}

/// `@setDataFrame onMetaData` body (AMF0 data message).
pub fn metadata(m: &Meta) -> Vec<u8> {
    use super::amf::{encode_all, Amf};
    let codec_id = match m.codec {
        VCodec::H264 => 7.0,
        VCodec::Hevc => u32::from_be_bytes(HVC1) as f64,
    };
    let n = |v: u32| Amf::Number(v as f64);
    encode_all(&[
        Amf::str("@setDataFrame"),
        Amf::str("onMetaData"),
        Amf::EcmaArray(vec![
            ("width".into(), n(m.width)),
            ("height".into(), n(m.height)),
            ("framerate".into(), n(m.fps)),
            ("videocodecid".into(), Amf::Number(codec_id)),
            ("videodatarate".into(), n(m.video_kbps)),
            ("audiocodecid".into(), Amf::Number(10.0)),
            ("audiodatarate".into(), n(m.audio_kbps)),
            ("audiosamplerate".into(), Amf::Number(48000.0)),
            ("audiosamplesize".into(), Amf::Number(16.0)),
            ("stereo".into(), Amf::Bool(true)),
            ("encoder".into(), Amf::str("PhoneCast")),
        ]),
    ])
}

// ---------------- NAL helpers ----------------

fn nal_type(codec: VCodec, nal: &[u8]) -> u8 {
    match codec {
        VCodec::H264 => nal[0] & 0x1F,
        VCodec::Hevc => (nal[0] >> 1) & 0x3F,
    }
}

fn is_vcl(codec: VCodec, t: u8) -> bool {
    match codec {
        VCodec::H264 => (1..=5).contains(&t),
        VCodec::Hevc => t < 32,
    }
}

/// Priority of one VCL NAL: keyframes, reference frames, and frames
/// nothing else references (B-frames from NVENC), which can be dropped.
fn nal_priority(codec: VCodec, nal: &[u8]) -> Priority {
    let t = nal_type(codec, nal);
    match codec {
        VCodec::H264 if t == 5 => Priority::Key,
        VCodec::H264 if nal[0] & 0x60 == 0 => Priority::Disposable,
        VCodec::Hevc if (16..=21).contains(&t) => Priority::Key,
        // Even types below 16 are sub-layer non-reference pictures.
        VCodec::Hevc if t < 16 && t.is_multiple_of(2) => Priority::Disposable,
        _ => Priority::Ref,
    }
}

/// Priority of a length-prefixed (AVCC/HVCC) frame.
pub fn frame_priority(codec: VCodec, data: &[u8]) -> Priority {
    let mut best: Option<Priority> = None;
    let mut i = 0;
    while i + 4 < data.len() {
        let len = u32::from_be_bytes(data[i..i + 4].try_into().unwrap()) as usize;
        let nal = &data[i + 4..(i + 4 + len).min(data.len())];
        if !nal.is_empty() && is_vcl(codec, nal_type(codec, nal)) {
            let p = nal_priority(codec, nal);
            best = Some(best.map_or(p, |b| b.max(p)));
        }
        i += 4 + len;
    }
    best.unwrap_or(Priority::Ref)
}

/// Removes emulation-prevention bytes (00 00 03 → 00 00).
fn rbsp(nal: &[u8]) -> Vec<u8> {
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

/// AVCDecoderConfigurationRecord from one SPS and one PPS (with NAL headers).
pub fn avcc_record(sps: &[u8], pps: &[u8]) -> Vec<u8> {
    let mut r = vec![1, sps[1], sps[2], sps[3], 0xFF, 0xE1];
    r.extend_from_slice(&(sps.len() as u16).to_be_bytes());
    r.extend_from_slice(sps);
    r.push(1);
    r.extend_from_slice(&(pps.len() as u16).to_be_bytes());
    r.extend_from_slice(pps);
    if matches!(sps[1], 100 | 110 | 122 | 144) {
        // High profiles: 4:2:0, 8-bit, no SPS extensions.
        r.extend_from_slice(&[0xFD, 0xF8, 0xF8, 0]);
    }
    r
}

/// HEVCDecoderConfigurationRecord from VPS/SPS/PPS (with NAL headers).
/// Profile/tier/level come from the SPS; 4:2:0 8-bit is assumed (NVENC main).
pub fn hvcc_record(vps: &[u8], sps: &[u8], pps: &[u8]) -> Vec<u8> {
    let s = rbsp(&sps[2..]);
    let max_sub_layers = ((s[0] >> 1) & 7) + 1;
    let nested = s[0] & 1;
    let mut r = vec![1];
    r.extend_from_slice(&s[1..13]); // profile space/tier/idc, compat, constraints, level
    r.extend_from_slice(&[0xF0, 0x00, 0xFC, 0xFD, 0xF8, 0xF8, 0, 0]);
    r.push(max_sub_layers << 3 | nested << 2 | 3);
    r.push(3);
    for (t, nal) in [(32u8, vps), (33, sps), (34, pps)] {
        r.push(0x80 | t);
        r.extend_from_slice(&1u16.to_be_bytes());
        r.extend_from_slice(&(nal.len() as u16).to_be_bytes());
        r.extend_from_slice(nal);
    }
    r
}

/// Turns Annex-B access units from an encoder into RTMP-ready packets,
/// keeping the latest parameter sets as the decoder configuration record.
pub struct Packetizer {
    codec: VCodec,
    vps: Vec<u8>,
    sps: Vec<u8>,
    pps: Vec<u8>,
    config: Option<Bytes>,
}

impl Packetizer {
    pub fn new(codec: VCodec) -> Self {
        Packetizer {
            codec,
            vps: Vec::new(),
            sps: Vec::new(),
            pps: Vec::new(),
            config: None,
        }
    }

    pub fn config(&self) -> Option<Bytes> {
        self.config.clone()
    }

    /// Parameter sets from codec extradata (Annex-B), if the encoder has them.
    pub fn add_extradata(&mut self, annexb: &[u8]) {
        let mut frame = Vec::new();
        self.collect(annexb, &mut frame);
    }

    fn collect(&mut self, annexb: &[u8], frame: &mut Vec<u8>) -> Priority {
        let mut best: Option<Priority> = None;
        let mut changed = false;
        for nal in split_nals(annexb) {
            let t = nal_type(self.codec, nal);
            let slot = match (self.codec, t) {
                (VCodec::H264, 7) | (VCodec::Hevc, 33) => Some(&mut self.sps),
                (VCodec::H264, 8) | (VCodec::Hevc, 34) => Some(&mut self.pps),
                (VCodec::Hevc, 32) => Some(&mut self.vps),
                _ => None,
            };
            if let Some(slot) = slot {
                if slot.as_slice() != nal {
                    *slot = nal.to_vec();
                    changed = true;
                }
                continue;
            }
            // Access unit delimiters carry nothing RTMP needs.
            if matches!((self.codec, t), (VCodec::H264, 9) | (VCodec::Hevc, 35)) {
                continue;
            }
            if is_vcl(self.codec, t) {
                let p = nal_priority(self.codec, nal);
                best = Some(best.map_or(p, |b| b.max(p)));
            }
            frame.extend_from_slice(&(nal.len() as u32).to_be_bytes());
            frame.extend_from_slice(nal);
        }
        let ready = !self.sps.is_empty()
            && !self.pps.is_empty()
            && (self.codec == VCodec::H264 || !self.vps.is_empty());
        if ready && (changed || self.config.is_none()) {
            let rec = match self.codec {
                VCodec::H264 => avcc_record(&self.sps, &self.pps),
                VCodec::Hevc => hvcc_record(&self.vps, &self.sps, &self.pps),
            };
            self.config = Some(rec.into());
        }
        best.unwrap_or(Priority::Ref)
    }

    /// One encoded access unit → packet. Keyframes carry the configuration
    /// record so any receiver can (re)start from them.
    pub fn packet(&mut self, src: u32, dts_ms: i64, cts_ms: i32, annexb: &[u8]) -> Packet {
        let mut frame = Vec::with_capacity(annexb.len());
        let priority = self.collect(annexb, &mut frame);
        let config = if priority == Priority::Key {
            self.config.clone()
        } else {
            None
        };
        Packet {
            src,
            dts_ms,
            kind: Kind::Video {
                codec: self.codec,
                priority,
                cts_ms,
            },
            data: frame.into(),
            config,
        }
    }
}

// ---------------- FLV stream reader (FFmpeg CLI fallback) ----------------

pub struct FlvTag {
    pub kind: u8,
    pub ts_ms: u32,
    pub body: Vec<u8>,
}

/// Reads tags from an FLV byte stream (e.g. FFmpeg's `-f flv pipe:1`).
pub struct FlvReader<R: Read> {
    r: R,
    started: bool,
}

impl<R: Read> FlvReader<R> {
    pub fn new(r: R) -> Self {
        FlvReader { r, started: false }
    }

    pub fn next_tag(&mut self) -> std::io::Result<FlvTag> {
        if !self.started {
            let mut h = [0u8; 13]; // header (9) + PreviousTagSize0 (4)
            self.r.read_exact(&mut h)?;
            if &h[..3] != b"FLV" {
                return Err(std::io::Error::other("not an FLV stream"));
            }
            self.started = true;
        }
        let mut h = [0u8; 11];
        self.r.read_exact(&mut h)?;
        let size = u32::from_be_bytes([0, h[1], h[2], h[3]]) as usize;
        let ts = u32::from_be_bytes([h[7], h[4], h[5], h[6]]);
        let mut body = vec![0u8; size + 4];
        self.r.read_exact(&mut body)?;
        body.truncate(size);
        Ok(FlvTag {
            kind: h[0] & 0x1F,
            ts_ms: ts,
            body,
        })
    }
}

/// FLV tag → engine packet. Returns None for metadata and unknown tags;
/// sequence headers come back as `Err(config)` so the caller can keep them.
pub fn tag_to_packet(src: u32, tag: &FlvTag) -> Option<Result<Packet, (bool, Bytes)>> {
    let b = &tag.body;
    match tag.kind {
        9 if b.len() > 5 && b[0] & 0x80 == 0 && b[0] & 0x0F == 7 => {
            let cts = i32::from_be_bytes([b[2], b[3], b[4], 0]) >> 8;
            if b[1] == 0 {
                return Some(Err((true, b[5..].into())));
            }
            let data = &b[5..];
            Some(Ok(video_packet(src, tag.ts_ms, cts, VCodec::H264, data)))
        }
        9 if b.len() > 5 && b[0] & 0x80 != 0 && b[1..5] == HVC1 => {
            let (cts, data) = match b[0] & 0x0F {
                0 => return Some(Err((true, b[5..].into()))),
                1 if b.len() > 8 => (i32::from_be_bytes([b[5], b[6], b[7], 0]) >> 8, &b[8..]),
                3 => (0, &b[5..]),
                _ => return None,
            };
            Some(Ok(video_packet(src, tag.ts_ms, cts, VCodec::Hevc, data)))
        }
        8 if b.len() > 2 && b[0] >> 4 == 10 => {
            if b[1] == 0 {
                return Some(Err((false, b[2..].into())));
            }
            Some(Ok(Packet {
                src,
                dts_ms: tag.ts_ms as i64,
                kind: Kind::Audio,
                data: b[2..].into(),
                config: None,
            }))
        }
        _ => None,
    }
}

fn video_packet(src: u32, ts: u32, cts: i32, codec: VCodec, data: &[u8]) -> Packet {
    Packet {
        src,
        dts_ms: ts as i64,
        kind: Kind::Video {
            codec,
            priority: frame_priority(codec, data),
            cts_ms: cts,
        },
        data: data.into(),
        config: None,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    const SPS: &[u8] = &[0x67, 0x64, 0x00, 0x28, 0xAC, 0xD9];
    const PPS: &[u8] = &[0x68, 0xEB, 0xE3];

    fn annexb(nals: &[&[u8]]) -> Vec<u8> {
        nals.iter()
            .flat_map(|n| [&[0, 0, 0, 1][..], n].concat())
            .collect()
    }

    #[test]
    fn h264_keyframe_carries_avcc_and_priorities() {
        let mut p = Packetizer::new(VCodec::H264);
        let key = p.packet(1, 0, 0, &annexb(&[&[0x09, 0xF0], SPS, PPS, &[0x65, 1, 2]]));
        assert!(key.is_key());
        let cfg = key.config.clone().unwrap();
        assert_eq!(&cfg[..4], &[1, 0x64, 0x00, 0x28]);
        // Only the slice remains, length-prefixed.
        assert_eq!(&key.data[..], &[0, 0, 0, 3, 0x65, 1, 2]);
        let b = p.packet(1, 16, 0, &annexb(&[&[0x01, 9]]));
        assert_eq!(b.priority(), Some(Priority::Disposable));
        let pf = p.packet(1, 33, 0, &annexb(&[&[0x41, 9]]));
        assert_eq!(pf.priority(), Some(Priority::Ref));
        assert!(pf.config.is_none());
    }

    #[test]
    fn hevc_record_and_enhanced_tag() {
        let vps = [0x40, 0x01, 0x0C, 0x01];
        // NAL header, then vps_id/max_sub_layers/nesting, then 12 PTL bytes.
        let sps = [
            0x42, 0x01, 0x01, 0x01, 0x60, 0, 0, 0, 0x90, 0, 0, 0, 0, 0, 0x99, 0xA0,
        ];
        let pps = [0x44, 0x01, 0xC1];
        let rec = hvcc_record(&vps, &sps, &pps);
        assert_eq!(rec[1], 0x01); // Main profile
        assert_eq!(rec[12], 0x99); // level 5.1
        let tag = video_tag(VCodec::Hevc, true, true, 0, &rec);
        assert_eq!(tag[0], 0x80 | 0x10);
        assert_eq!(&tag[1..5], b"hvc1");
        let frame = video_tag(VCodec::Hevc, false, false, 33, &[1]);
        assert_eq!(frame[0], 0x80 | 0x20 | 1);
        assert_eq!(&frame[5..8], &[0, 0, 33]);
    }

    #[test]
    fn aac_config_48k_stereo() {
        assert_eq!(aac_config(48000, 2), vec![0x11, 0x90]);
    }

    #[test]
    fn flv_reader_parses_tags() {
        let mut s = b"FLV\x01\x05\x00\x00\x00\x09\x00\x00\x00\x00".to_vec();
        let body = video_tag(VCodec::H264, true, false, 0, &[0, 0, 0, 1, 0x65]);
        s.push(9);
        s.extend_from_slice(&(body.len() as u32).to_be_bytes()[1..]);
        s.extend_from_slice(&[0, 0, 40, 0, 0, 0, 0]);
        s.extend_from_slice(&body);
        s.extend_from_slice(&((body.len() + 11) as u32).to_be_bytes());
        let mut r = FlvReader::new(&s[..]);
        let tag = r.next_tag().unwrap();
        assert_eq!(tag.ts_ms, 40);
        let p = tag_to_packet(7, &tag).unwrap().ok().unwrap();
        assert!(p.is_key());
        assert_eq!(p.dts_ms, 40);
    }
}
