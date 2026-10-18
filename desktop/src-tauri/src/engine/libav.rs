//! In-process video pipeline on libavcodec/libavfilter (FFmpeg 9 shared
//! libraries, feature `libav`):
//!
//! phone H.264/HEVC → NVDEC (CUDA frames) → one filter graph per output size
//! and framing (the same CUDA scale/blur/overlay graph as
//! `live::video_filter`) → one NVENC session per [`EncodeSpec`]. AAC audio is
//! encoded once and shared by every stream.
//!
//! Every FFmpeg object lives on the single pipeline thread; the handles the
//! engine holds are atomics and a mutex. Bitrate changes are applied to the
//! running NVENC session: libavcodec's `reconfig_encoder()` sees the new
//! `bit_rate`/`rc_max_rate`/`rc_buffer_size` before the next frame and calls
//! `nvEncReconfigureEncoder` (with an IDR). Phone rotation rebuilds only the
//! decoder and graphs; the encoders and their output size never change.

use super::encoder::{BitrateGate, EncodeSpec, EncoderStats, Source, Stream, VideoEncoder};
use super::flv::{aac_config, Packetizer};
use super::{Bytes, Kind, Packet, VCodec};
use crate::live::Framing;
use crate::mkv::{VideoConfig, CODEC_H265};
use crate::sinks::Media;
use ffmpeg_sys_next as ff;
use std::ffi::{CStr, CString};
use std::ptr::{null, null_mut};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::mpsc::{sync_channel, Receiver, SyncSender, TrySendError};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

const EAGAIN: i32 = -11; // AVERROR(EAGAIN) on Windows and Linux
const US: ff::AVRational = ff::AVRational {
    num: 1,
    den: 1_000_000,
};
const MS: ff::AVRational = ff::AVRational { num: 1, den: 1000 };

fn err(what: &str, code: i32) -> String {
    let mut buf = [0i8; 256];
    // SAFETY: buffer is large enough and NUL-terminated by av_strerror.
    unsafe {
        ff::av_strerror(code, buf.as_mut_ptr(), buf.len());
        format!("{what}: {}", CStr::from_ptr(buf.as_ptr()).to_string_lossy())
    }
}

/// Loads the (delay-loaded, see build.rs) FFmpeg DLLs. Must succeed before
/// any FFmpeg function is called, or the delay-load stub would crash.
fn dlls_present() -> bool {
    #[cfg(windows)]
    {
        use windows_sys::Win32::System::LibraryLoader::LoadLibraryW;
        let list = option_env!("PHONECAST_FFMPEG_DLLS").unwrap_or("");
        list.split(';').filter(|d| !d.is_empty()).all(|d| {
            let w: Vec<u16> = d.encode_utf16().chain([0]).collect();
            // SAFETY: NUL-terminated wide string; loading a DLL by name.
            unsafe { !LoadLibraryW(w.as_ptr()).is_null() }
        })
    }
    #[cfg(not(windows))]
    true
}

/// True when the FFmpeg DLLs load and NVENC + CUDA filters exist.
pub fn available() -> bool {
    static OK: std::sync::OnceLock<bool> = std::sync::OnceLock::new();
    *OK.get_or_init(|| dlls_present() && codecs_present())
}

fn codecs_present() -> bool {
    // SAFETY: lookups only (DLLs are loaded).
    unsafe {
        let enc = CString::new("h264_nvenc").unwrap();
        let f = CString::new("scale_cuda").unwrap();
        !ff::avcodec_find_encoder_by_name(enc.as_ptr()).is_null()
            && !ff::avfilter_get_by_name(f.as_ptr()).is_null()
    }
}

// ---------------- handles ----------------

struct Shared {
    spec: EncodeSpec,
    gate: Mutex<BitrateGate>,
    idr: AtomicBool,
    closed: AtomicBool,
    stats: Mutex<EncoderStats>,
}

pub struct LibavEncoder(Arc<Shared>);

impl VideoEncoder for LibavEncoder {
    fn spec(&self) -> EncodeSpec {
        self.0.spec
    }

    fn set_bitrate(&self, kbps: u32) -> bool {
        self.0.gate.lock().unwrap().request(kbps);
        true
    }

    fn request_idr(&self) {
        self.0.idr.store(true, Ordering::SeqCst);
    }

    fn stats(&self) -> EncoderStats {
        *self.0.stats.lock().unwrap()
    }

    fn close(&self) {
        self.0.closed.store(true, Ordering::SeqCst);
    }
}

enum Cmd {
    Media(Media),
    Open(Arc<Shared>, Arc<Stream>),
    Stop,
}

pub struct LibavSource {
    tx: SyncSender<Cmd>,
    thread: Mutex<Option<std::thread::JoinHandle<()>>>,
    request_phone_key: Arc<dyn Fn() + Send + Sync>,
    overflowed: AtomicBool,
}

impl LibavSource {
    pub fn new(
        config: VideoConfig,
        request_phone_key: Arc<dyn Fn() + Send + Sync>,
    ) -> Result<Self, String> {
        if !available() {
            return Err("FFmpeg libraries with NVENC are not available".into());
        }
        // ~2 s of video + audio packets; the pipeline normally keeps up.
        let (tx, rx) = sync_channel(600);
        let rk = request_phone_key.clone();
        let thread = std::thread::Builder::new()
            .name("libav-pipeline".into())
            .spawn(move || {
                let mut p = Pipeline::new(config, rk);
                p.run(rx);
            })
            .map_err(|e| e.to_string())?;
        Ok(LibavSource {
            tx,
            thread: Mutex::new(Some(thread)),
            request_phone_key,
            overflowed: AtomicBool::new(false),
        })
    }
}

impl Source for LibavSource {
    fn backend(&self) -> &'static str {
        "libav-nvenc"
    }

    fn live_bitrate(&self) -> bool {
        true
    }

    fn open(&self, spec: EncodeSpec, out: Arc<Stream>) -> Result<Arc<dyn VideoEncoder>, String> {
        let shared = Arc::new(Shared {
            spec,
            gate: Mutex::new(BitrateGate::new(spec.kbps, Duration::from_secs(1))),
            idr: AtomicBool::new(true),
            closed: AtomicBool::new(false),
            stats: Mutex::new(EncoderStats {
                sessions: 1,
                kbps: spec.kbps,
                ..Default::default()
            }),
        });
        self.tx
            .send(Cmd::Open(shared.clone(), out))
            .map_err(|_| "Video pipeline stopped".to_string())?;
        Ok(Arc::new(LibavEncoder(shared)))
    }

    fn push(&self, m: &Media) {
        match self.tx.try_send(Cmd::Media(m.clone())) {
            Ok(()) => {
                if self.overflowed.swap(false, Ordering::SeqCst) {
                    (self.request_phone_key)();
                }
            }
            // Pipeline behind (GPU busy): skip, and restart from a keyframe.
            Err(TrySendError::Full(_)) => self.overflowed.store(true, Ordering::SeqCst),
            Err(TrySendError::Disconnected(_)) => {}
        }
    }

    fn stop(&self) {
        self.tx.send(Cmd::Stop).ok();
        if let Some(t) = self.thread.lock().unwrap().take() {
            t.join().ok();
        }
    }
}

// ---------------- pipeline (single thread, owns all FFmpeg state) ----------------

struct Decoder {
    ctx: *mut ff::AVCodecContext,
    pkt: *mut ff::AVPacket,
    frame: *mut ff::AVFrame,
}

struct Graph {
    graph: *mut ff::AVFilterGraph,
    src: *mut ff::AVFilterContext,
    sink: *mut ff::AVFilterContext,
    /// Input frame size the graph was built for.
    in_size: (i32, i32),
}

struct Enc {
    shared: Arc<Shared>,
    stream: Arc<Stream>,
    ctx: *mut ff::AVCodecContext,
    pkt: *mut ff::AVPacket,
    packetizer: Packetizer,
}

struct Aac {
    ctx: *mut ff::AVCodecContext,
    pkt: *mut ff::AVPacket,
    left: Vec<f32>,
    right: Vec<f32>,
    /// Phone clock (µs) of the first sample, and samples sent since.
    start_us: Option<i64>,
    samples: i64,
    config: Bytes,
}

struct Pipeline {
    config: VideoConfig,
    hw: *mut ff::AVBufferRef,
    dec: Option<Decoder>,
    /// One graph per (width, height, framing).
    graphs: Vec<((u32, u32, Framing), Graph)>,
    encs: Vec<Enc>,
    aac: Option<Aac>,
    out_frame: *mut ff::AVFrame,
    request_phone_key: Arc<dyn Fn() + Send + Sync>,
}

unsafe extern "C" fn pick_cuda(
    _ctx: *mut ff::AVCodecContext,
    fmts: *const ff::AVPixelFormat,
) -> ff::AVPixelFormat {
    let mut p = fmts;
    while *p != ff::AVPixelFormat::AV_PIX_FMT_NONE {
        if *p == ff::AVPixelFormat::AV_PIX_FMT_CUDA {
            return *p;
        }
        p = p.add(1);
    }
    *fmts
}

impl Pipeline {
    fn new(config: VideoConfig, request_phone_key: Arc<dyn Fn() + Send + Sync>) -> Self {
        let mut hw = null_mut();
        // SAFETY: creates a CUDA device context on the default GPU.
        unsafe {
            ff::av_hwdevice_ctx_create(
                &mut hw,
                ff::AVHWDeviceType::AV_HWDEVICE_TYPE_CUDA,
                null(),
                null_mut(),
                0,
            );
        }
        Pipeline {
            config,
            hw,
            dec: None,
            graphs: Vec::new(),
            encs: Vec::new(),
            aac: None,
            // SAFETY: plain allocation.
            out_frame: unsafe { ff::av_frame_alloc() },
            request_phone_key,
        }
    }

    fn run(&mut self, rx: Receiver<Cmd>) {
        if self.hw.is_null() {
            crate::diag::log("engine", "libav: no CUDA device".to_string());
            return;
        }
        while let Ok(cmd) = rx.recv() {
            let res = match cmd {
                Cmd::Stop => break,
                Cmd::Open(shared, stream) => {
                    // Opened lazily on its first frame (needs the CUDA frames context).
                    // SAFETY: plain allocation.
                    let pkt = unsafe { ff::av_packet_alloc() };
                    let codec = shared.spec.key.codec;
                    self.encs.push(Enc {
                        shared,
                        stream,
                        ctx: null_mut(),
                        pkt,
                        packetizer: Packetizer::new(codec),
                    });
                    (self.request_phone_key)();
                    Ok(())
                }
                Cmd::Media(Media::Config(c)) => {
                    if c != self.config {
                        // Rotation: new decoder and graphs, same encoders.
                        self.config = c;
                        self.close_decoder();
                        self.close_graphs();
                    }
                    Ok(())
                }
                // SAFETY: all FFmpeg state is owned by this thread.
                Cmd::Media(Media::Video { pts, data, .. }) => unsafe { self.video(pts, &data) },
                Cmd::Media(Media::Audio { pts, pcm }) => unsafe { self.audio(pts, &pcm) },
            };
            if let Err(e) = res {
                crate::diag::log("engine", format!("libav: {e}"));
            }
            self.reap_closed();
        }
        self.close_all();
    }

    fn reap_closed(&mut self) {
        self.encs.retain_mut(|e| {
            if e.shared.closed.load(Ordering::SeqCst) {
                // SAFETY: owned by this thread; freed exactly once.
                unsafe {
                    ff::avcodec_free_context(&mut e.ctx);
                    ff::av_packet_free(&mut e.pkt);
                }
                false
            } else {
                true
            }
        });
    }

    unsafe fn open_decoder(&mut self) -> Result<(), String> {
        let id = if self.config.codec == CODEC_H265 {
            ff::AVCodecID::AV_CODEC_ID_HEVC
        } else {
            ff::AVCodecID::AV_CODEC_ID_H264
        };
        let codec = ff::avcodec_find_decoder(id);
        let ctx = ff::avcodec_alloc_context3(codec);
        (*ctx).hw_device_ctx = ff::av_buffer_ref(self.hw);
        (*ctx).get_format = Some(pick_cuda);
        (*ctx).pkt_timebase = US;
        let csd = &self.config.csd;
        if !csd.is_empty() {
            let pad = ff::AV_INPUT_BUFFER_PADDING_SIZE as usize;
            let p = ff::av_mallocz(csd.len() + pad) as *mut u8;
            std::ptr::copy_nonoverlapping(csd.as_ptr(), p, csd.len());
            (*ctx).extradata = p;
            (*ctx).extradata_size = csd.len() as i32;
        }
        let r = ff::avcodec_open2(ctx, codec, null_mut());
        if r < 0 {
            let mut c = ctx;
            ff::avcodec_free_context(&mut c);
            return Err(err("decoder", r));
        }
        self.dec = Some(Decoder {
            ctx,
            pkt: ff::av_packet_alloc(),
            frame: ff::av_frame_alloc(),
        });
        Ok(())
    }

    unsafe fn video(&mut self, pts_us: i64, data: &[u8]) -> Result<(), String> {
        if self.encs.is_empty() {
            return Ok(());
        }
        if self.dec.is_none() {
            self.open_decoder()?;
        }
        let d = self.dec.as_ref().unwrap();
        let (ctx, pkt, frame) = (d.ctx, d.pkt, d.frame);
        let r = ff::av_new_packet(pkt, data.len() as i32);
        if r < 0 {
            return Err(err("packet", r));
        }
        std::ptr::copy_nonoverlapping(data.as_ptr(), (*pkt).data, data.len());
        (*pkt).pts = pts_us;
        (*pkt).dts = pts_us;
        let r = ff::avcodec_send_packet(ctx, pkt);
        ff::av_packet_unref(pkt);
        if r < 0 && r != EAGAIN {
            (self.request_phone_key)();
            return Err(err("decode", r));
        }
        loop {
            let r = ff::avcodec_receive_frame(ctx, frame);
            if r == EAGAIN || r == ff::AVERROR_EOF {
                return Ok(());
            }
            if r < 0 {
                return Err(err("decode", r));
            }
            (*frame).pts = (*frame).best_effort_timestamp;
            let res = self.filter_and_encode(frame);
            ff::av_frame_unref(frame);
            res?;
        }
    }

    unsafe fn filter_and_encode(&mut self, frame: *mut ff::AVFrame) -> Result<(), String> {
        let mut keys: Vec<(u32, u32, Framing)> = self
            .encs
            .iter()
            .map(|e| {
                (
                    e.shared.spec.key.w,
                    e.shared.spec.key.h,
                    e.shared.spec.framing,
                )
            })
            .collect();
        keys.sort_by_key(|k| (k.0, k.1, k.2 as u8));
        keys.dedup();
        let in_size = ((*frame).width, (*frame).height);
        for key in keys {
            let mut idx = self.graphs.iter().position(|(k, _)| *k == key);
            if let Some(i) = idx.filter(|&i| self.graphs[i].1.in_size != in_size) {
                let (_, mut g) = self.graphs.remove(i);
                ff::avfilter_graph_free(&mut g.graph);
                idx = None;
            }
            let i = match idx {
                Some(i) => i,
                None => {
                    let g = self.build_graph(key, frame)?;
                    self.graphs.push((key, g));
                    self.graphs.len() - 1
                }
            };
            let g = &self.graphs[i].1;
            let flags = ff::AV_BUFFERSRC_FLAG_KEEP_REF as i32;
            let r = ff::av_buffersrc_add_frame_flags(g.src, frame, flags);
            if r < 0 {
                return Err(err("filter input", r));
            }
            let sink = g.sink;
            loop {
                let r = ff::av_buffersink_get_frame(sink, self.out_frame);
                if r == EAGAIN || r == ff::AVERROR_EOF {
                    break;
                }
                if r < 0 {
                    return Err(err("filter output", r));
                }
                let res = self.encode_all(key, sink);
                ff::av_frame_unref(self.out_frame);
                res?;
            }
        }
        Ok(())
    }

    unsafe fn build_graph(
        &self,
        (w, h, framing): (u32, u32, Framing),
        frame: *const ff::AVFrame,
    ) -> Result<Graph, String> {
        let graph = ff::avfilter_graph_alloc();
        let name = |s: &str| CString::new(s).unwrap();
        let src = ff::avfilter_graph_alloc_filter(
            graph,
            ff::avfilter_get_by_name(name("buffer").as_ptr()),
            name("in").as_ptr(),
        );
        let par = ff::av_buffersrc_parameters_alloc();
        (*par).format = ff::AVPixelFormat::AV_PIX_FMT_CUDA as i32;
        (*par).width = (*frame).width;
        (*par).height = (*frame).height;
        (*par).time_base = US;
        (*par).sample_aspect_ratio = ff::AVRational { num: 1, den: 1 };
        (*par).hw_frames_ctx = (*frame).hw_frames_ctx;
        ff::av_buffersrc_parameters_set(src, par);
        ff::av_free(par as *mut _);
        let r = ff::avfilter_init_dict(src, null_mut());
        if r < 0 {
            return Err(err("buffer source", r));
        }
        let mut sink = null_mut();
        let r = ff::avfilter_graph_create_filter(
            &mut sink,
            ff::avfilter_get_by_name(name("buffersink").as_ptr()),
            name("out").as_ptr(),
            null(),
            null_mut(),
            graph,
        );
        if r < 0 {
            return Err(err("buffer sink", r));
        }
        // The exact graph the CLI path uses, with named pads.
        let (_, desc) = crate::live::video_filter(true, framing, w, h);
        let mut desc = desc.replace("[0:v:0]", "[in]").replace("[v]", "[out]");
        if framing == Framing::Fill {
            // The CLI decodes Fill to system memory; ours always yields CUDA frames.
            desc = desc.replacen("[in]", "[in]hwdownload,format=nv12,", 1);
        }
        let mut outputs = ff::avfilter_inout_alloc();
        (*outputs).name = ff::av_strdup(name("in").as_ptr());
        (*outputs).filter_ctx = src;
        let mut inputs = ff::avfilter_inout_alloc();
        (*inputs).name = ff::av_strdup(name("out").as_ptr());
        (*inputs).filter_ctx = sink;
        let cdesc = name(&desc);
        let r = ff::avfilter_graph_parse_ptr(
            graph,
            cdesc.as_ptr(),
            &mut inputs,
            &mut outputs,
            null_mut(),
        );
        ff::avfilter_inout_free(&mut inputs);
        ff::avfilter_inout_free(&mut outputs);
        if r < 0 {
            return Err(err("filter graph", r));
        }
        // hwupload_cuda needs the device.
        for i in 0..(*graph).nb_filters as usize {
            let f = *(*graph).filters.add(i);
            (*f).hw_device_ctx = ff::av_buffer_ref(self.hw);
        }
        let r = ff::avfilter_graph_config(graph, null_mut());
        if r < 0 {
            return Err(err("filter graph config", r));
        }
        Ok(Graph {
            graph,
            src,
            sink,
            in_size: ((*frame).width, (*frame).height),
        })
    }

    unsafe fn open_encoder(e: &mut Enc, sink: *mut ff::AVFilterContext) -> Result<(), String> {
        let spec = e.shared.spec;
        let name = match spec.key.codec {
            VCodec::H264 => "h264_nvenc",
            VCodec::Hevc => "hevc_nvenc",
        };
        let cname = CString::new(name).unwrap();
        let codec = ff::avcodec_find_encoder_by_name(cname.as_ptr());
        if codec.is_null() {
            return Err(format!("{name} not available"));
        }
        let ctx = ff::avcodec_alloc_context3(codec);
        (*ctx).width = ff::av_buffersink_get_w(sink);
        (*ctx).height = ff::av_buffersink_get_h(sink);
        (*ctx).pix_fmt = ff::AVPixelFormat::AV_PIX_FMT_CUDA;
        (*ctx).sw_pix_fmt = ff::AVPixelFormat::AV_PIX_FMT_NV12;
        (*ctx).hw_frames_ctx = ff::av_buffer_ref(ff::av_buffersink_get_hw_frames_ctx(sink));
        (*ctx).time_base = ff::av_buffersink_get_time_base(sink);
        (*ctx).framerate = ff::AVRational {
            num: spec.key.fps as i32,
            den: 1,
        };
        let bps = spec.kbps as i64 * 1000;
        (*ctx).bit_rate = bps;
        (*ctx).rc_max_rate = bps;
        (*ctx).rc_buffer_size = (bps * 2) as i32;
        (*ctx).gop_size = spec.gop() as i32;
        (*ctx).keyint_min = spec.gop() as i32;
        (*ctx).max_b_frames = 2;
        let uhd = spec.key.w >= 3840;
        let mut opts: *mut ff::AVDictionary = null_mut();
        let profile = if spec.key.codec == VCodec::H264 {
            "high"
        } else {
            "main"
        };
        for (k, v) in [
            ("preset", if uhd { "p4" } else { "p5" }),
            ("tune", "hq"),
            ("rc", "cbr"),
            ("spatial-aq", "1"),
            ("temporal-aq", "1"),
            ("rc-lookahead", "20"),
            ("forced-idr", "1"),
            ("profile", profile),
        ] {
            let (k, v) = (CString::new(k).unwrap(), CString::new(v).unwrap());
            ff::av_dict_set(&mut opts, k.as_ptr(), v.as_ptr(), 0);
        }
        let r = ff::avcodec_open2(ctx, codec, &mut opts);
        ff::av_dict_free(&mut opts);
        if r < 0 {
            let mut c = ctx;
            ff::avcodec_free_context(&mut c);
            return Err(err(name, r));
        }
        e.ctx = ctx;
        Ok(())
    }

    unsafe fn encode_all(
        &mut self,
        key: (u32, u32, Framing),
        sink: *mut ff::AVFilterContext,
    ) -> Result<(), String> {
        let now = Instant::now();
        for e in self.encs.iter_mut() {
            let s = e.shared.spec;
            if (s.key.w, s.key.h, s.framing) != key || e.shared.closed.load(Ordering::SeqCst) {
                continue;
            }
            if e.ctx.is_null() {
                Self::open_encoder(e, sink)?;
            }
            let f = ff::av_frame_clone(self.out_frame);
            let mut idr = e.shared.idr.swap(false, Ordering::SeqCst);
            if let Some(kbps) = e.shared.gate.lock().unwrap().poll(now) {
                // Picked up by nvenc.c reconfig_encoder() on this frame.
                let bps = kbps as i64 * 1000;
                (*e.ctx).bit_rate = bps;
                (*e.ctx).rc_max_rate = bps;
                (*e.ctx).rc_buffer_size = (bps * 2) as i32;
                let mut st = e.shared.stats.lock().unwrap();
                st.reconfigs += 1;
                st.kbps = kbps;
                idr = true;
            }
            if idr {
                (*f).pict_type = ff::AVPictureType::AV_PICTURE_TYPE_I;
                e.shared.stats.lock().unwrap().idrs += 1;
            } else {
                (*f).pict_type = ff::AVPictureType::AV_PICTURE_TYPE_NONE;
            }
            let r = ff::avcodec_send_frame(e.ctx, f);
            let mut f = f;
            ff::av_frame_free(&mut f);
            if r < 0 && r != EAGAIN {
                return Err(err("encode", r));
            }
            let tb = (*e.ctx).time_base;
            loop {
                let r = ff::avcodec_receive_packet(e.ctx, e.pkt);
                if r == EAGAIN || r == ff::AVERROR_EOF {
                    break;
                }
                if r < 0 {
                    return Err(err("encode", r));
                }
                let p = &*e.pkt;
                let dts = ff::av_rescale_q(p.dts, tb, MS);
                let pts = ff::av_rescale_q(p.pts, tb, MS);
                let data = std::slice::from_raw_parts(p.data, p.size as usize);
                let packet = e
                    .packetizer
                    .packet(e.stream.id, dts, (pts - dts) as i32, data);
                e.shared.stats.lock().unwrap().frames += 1;
                e.stream.publish(packet);
                ff::av_packet_unref(e.pkt);
            }
        }
        Ok(())
    }

    unsafe fn open_aac(&mut self) -> Result<(), String> {
        let codec = ff::avcodec_find_encoder(ff::AVCodecID::AV_CODEC_ID_AAC);
        let ctx = ff::avcodec_alloc_context3(codec);
        (*ctx).sample_fmt = ff::AVSampleFormat::AV_SAMPLE_FMT_FLTP;
        (*ctx).sample_rate = 48000;
        ff::av_channel_layout_default(&mut (*ctx).ch_layout, 2);
        (*ctx).bit_rate = 160_000;
        (*ctx).time_base = ff::AVRational { num: 1, den: 48000 };
        let r = ff::avcodec_open2(ctx, codec, null_mut());
        if r < 0 {
            let mut c = ctx;
            ff::avcodec_free_context(&mut c);
            return Err(err("aac", r));
        }
        self.aac = Some(Aac {
            ctx,
            pkt: ff::av_packet_alloc(),
            left: Vec::new(),
            right: Vec::new(),
            start_us: None,
            samples: 0,
            config: aac_config(48000, 2).into(),
        });
        Ok(())
    }

    /// 48 kHz stereo s16 PCM → AAC, published to every encoder's stream.
    unsafe fn audio(&mut self, pts_us: i64, pcm: &[u8]) -> Result<(), String> {
        if self.encs.is_empty() {
            return Ok(());
        }
        if self.aac.is_none() {
            self.open_aac()?;
        }
        let a = self.aac.as_mut().unwrap();
        a.start_us.get_or_insert(pts_us);
        for s in pcm.chunks_exact(4) {
            a.left
                .push(i16::from_le_bytes([s[0], s[1]]) as f32 / 32768.0);
            a.right
                .push(i16::from_le_bytes([s[2], s[3]]) as f32 / 32768.0);
        }
        let n = (*a.ctx).frame_size.max(1024) as usize;
        while a.left.len() >= n {
            let f = ff::av_frame_alloc();
            (*f).nb_samples = n as i32;
            (*f).format = ff::AVSampleFormat::AV_SAMPLE_FMT_FLTP as i32;
            (*f).sample_rate = 48000;
            ff::av_channel_layout_copy(&mut (*f).ch_layout, &(*a.ctx).ch_layout);
            ff::av_frame_get_buffer(f, 0);
            std::ptr::copy_nonoverlapping(a.left.as_ptr(), (*f).data[0] as *mut f32, n);
            std::ptr::copy_nonoverlapping(a.right.as_ptr(), (*f).data[1] as *mut f32, n);
            a.left.drain(..n);
            a.right.drain(..n);
            (*f).pts = a.samples;
            a.samples += n as i64;
            let r = ff::avcodec_send_frame(a.ctx, f);
            let mut f = f;
            ff::av_frame_free(&mut f);
            if r < 0 {
                return Err(err("aac", r));
            }
            loop {
                let r = ff::avcodec_receive_packet(a.ctx, a.pkt);
                if r < 0 {
                    break;
                }
                let p = &*a.pkt;
                let base_ms = a.start_us.unwrap_or(0) / 1000;
                let dts = base_ms + p.pts * 1000 / 48000;
                let data: Bytes = std::slice::from_raw_parts(p.data, p.size as usize).into();
                for e in &self.encs {
                    e.stream.publish(Packet {
                        src: 0,
                        dts_ms: dts,
                        kind: Kind::Audio,
                        data: data.clone(),
                        config: Some(a.config.clone()),
                    });
                }
                ff::av_packet_unref(a.pkt);
            }
        }
        Ok(())
    }

    fn close_decoder(&mut self) {
        if let Some(mut d) = self.dec.take() {
            // SAFETY: owned by this thread; freed exactly once.
            unsafe {
                ff::avcodec_free_context(&mut d.ctx);
                ff::av_packet_free(&mut d.pkt);
                ff::av_frame_free(&mut d.frame);
            }
        }
    }

    fn close_graphs(&mut self) {
        for (_, mut g) in self.graphs.drain(..) {
            // SAFETY: as above.
            unsafe { ff::avfilter_graph_free(&mut g.graph) };
        }
    }

    fn close_all(&mut self) {
        for e in &self.encs {
            e.shared.closed.store(true, Ordering::SeqCst);
        }
        self.reap_closed();
        self.close_decoder();
        self.close_graphs();
        // SAFETY: owned by this thread; freed exactly once.
        unsafe {
            if let Some(mut a) = self.aac.take() {
                ff::avcodec_free_context(&mut a.ctx);
                ff::av_packet_free(&mut a.pkt);
            }
            ff::av_frame_free(&mut self.out_frame);
            ff::av_buffer_unref(&mut self.hw);
        }
    }
}
