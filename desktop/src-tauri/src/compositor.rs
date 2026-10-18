//! GPU compositor for PC mode: one window or monitor (Windows Graphics
//! Capture) as the base layer, plus webcam, PNG and text overlays, composed
//! in Direct3D 11 at the output size and converted to NV12 on the GPU.
//!
//! How frames flow:
//!
//! 1. Windows Graphics Capture hands us a BGRA texture on its own D3D11
//!    device every time the window or screen changes. We copy it into our
//!    base texture on the GPU (no CPU copy).
//! 2. A 60 Hz ticker thread composes base + layers into a BGRA canvas,
//!    converts it to NV12 planes with two pixel-shader passes, and copies the
//!    planes to a staging texture ring.
//! 3. The previous tick's staging planes are mapped and packed into one NV12
//!    buffer (the only CPU copy of the picture: ~330 MB/s at 1440p60).
//! 4. Each FFmpeg job (preview, recording, YouTube, Twitch) reads that buffer
//!    from its own named pipe, fed by its own thread behind a bounded queue,
//!    so a slow output can never stall capture or the other outputs.
//!
//! Why not keep frames on the GPU all the way into NVENC? FFmpeg cannot map
//! D3D11 textures into CUDA from another process, and running libavcodec in
//! process would pull GPL code into an Apache-2.0 binary. So this is option
//! (c) of the design notes: GPU readback to NV12 and a raw pipe per output.
//! The CPU work per frame is one packed copy (~5.5 MB at 1440p) plus one
//! pipe write per output. Measured with the ignored `cpu_capture` test on
//! an i5-13450HX + RTX 4050 (release build, NVENC, % of the whole CPU):
//!
//! | 60 fps, one output        | 1080p | 1440p |
//! |---------------------------|-------|-------|
//! | window, no overlays       | 2.3%  | 3.1%  |
//! | window + webcam/PNG/text  | 3.8%  | 5.2%  |
//!
//! Well under the 15% budget, so a shared-texture path straight into NVENC
//! (in-process libavcodec with d3d11 hwframes) is not worth its GPL cost.

#![cfg(windows)]

use crate::capture::{Overlay, OverlayKind, Webcam};
use serde_json::{json, Value};
use std::collections::HashMap;
use std::io::Read;
use std::path::PathBuf;
use std::process::{Child, Command, Stdio};
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::sync::mpsc::{sync_channel, Receiver, RecvTimeoutError, SyncSender};
use std::sync::{Arc, Mutex, Weak};
use std::time::{Duration, Instant};

use windows::core::{Interface, PCSTR};
use windows::Win32::Graphics::Direct2D::Common::{
    D2D1_ALPHA_MODE_PREMULTIPLIED, D2D1_COLOR_F, D2D1_PIXEL_FORMAT,
};
use windows::Win32::Graphics::Direct2D::{
    D2D1CreateFactory, ID2D1Factory, D2D1_DRAW_TEXT_OPTIONS_NONE,
    D2D1_FACTORY_TYPE_SINGLE_THREADED, D2D1_FEATURE_LEVEL_DEFAULT, D2D1_RENDER_TARGET_PROPERTIES,
    D2D1_RENDER_TARGET_TYPE_DEFAULT, D2D1_RENDER_TARGET_USAGE_NONE,
};
use windows::Win32::Graphics::Direct3D::Fxc::D3DCompile;
use windows::Win32::Graphics::Direct3D::{ID3DBlob, D3D11_PRIMITIVE_TOPOLOGY_TRIANGLESTRIP};
use windows::Win32::Graphics::Direct3D11::*;
use windows::Win32::Graphics::DirectWrite::{
    DWriteCreateFactory, IDWriteFactory, DWRITE_FACTORY_TYPE_SHARED, DWRITE_FONT_STRETCH_NORMAL,
    DWRITE_FONT_STYLE_NORMAL, DWRITE_FONT_WEIGHT_BOLD, DWRITE_TEXT_METRICS,
    DWRITE_WORD_WRAPPING_NO_WRAP,
};
use windows::Win32::Graphics::Dxgi::Common::{
    DXGI_FORMAT, DXGI_FORMAT_B8G8R8A8_UNORM, DXGI_FORMAT_R8G8_UNORM, DXGI_FORMAT_R8_UNORM,
    DXGI_SAMPLE_DESC,
};
use windows::Win32::Graphics::Dxgi::IDXGISurface;
use windows_numerics::Vector2;

use windows_capture::capture::{CaptureControl, Context, GraphicsCaptureApiHandler};
use windows_capture::frame::Frame;
use windows_capture::graphics_capture_api::{GraphicsCaptureApi, InternalCaptureControl};
use windows_capture::settings::{
    ColorFormat, CursorCaptureSettings, DirtyRegionSettings, DrawBorderSettings,
    MinimumUpdateIntervalSettings, SecondaryWindowSettings, Settings,
};

// ====================================================================
// Layout maths (pure, unit-tested)
// ====================================================================

pub mod layout {
    use crate::capture::{Overlay, OverlayKind, Webcam};

    /// A rectangle in canvas pixels.
    #[derive(Clone, Copy, Debug, PartialEq)]
    pub struct Rect {
        pub x: i32,
        pub y: i32,
        pub w: i32,
        pub h: i32,
    }

    /// Largest rectangle with the source's aspect ratio that fits inside
    /// `dst_w`×`dst_h`, centred (letterbox / pillarbox).
    pub fn fit(src_w: u32, src_h: u32, dst_w: u32, dst_h: u32) -> Rect {
        if src_w == 0 || src_h == 0 || dst_w == 0 || dst_h == 0 {
            return Rect {
                x: 0,
                y: 0,
                w: dst_w as i32,
                h: dst_h as i32,
            };
        }
        let scale = (dst_w as f64 / src_w as f64).min(dst_h as f64 / src_h as f64);
        let w = ((src_w as f64 * scale).round() as i32).clamp(2, dst_w as i32);
        let h = ((src_h as f64 * scale).round() as i32).clamp(2, dst_h as i32);
        Rect {
            x: (dst_w as i32 - w) / 2,
            y: (dst_h as i32 - h) / 2,
            w,
            h,
        }
    }

    /// Keeps a rectangle fully inside the canvas (shrinking it if it is
    /// larger than the canvas).
    pub fn clamp(r: Rect, canvas_w: u32, canvas_h: u32) -> Rect {
        let (cw, ch) = (canvas_w as i32, canvas_h as i32);
        let w = r.w.clamp(2, cw.max(2));
        let h = r.h.clamp(2, ch.max(2));
        Rect {
            x: r.x.clamp(0, (cw - w).max(0)),
            y: r.y.clamp(0, (ch - h).max(0)),
            w,
            h,
        }
    }

    /// Where the webcam picture-in-picture goes. `size` is the width as a
    /// fraction of the canvas; corners keep a small margin; "free" uses the
    /// dragged position (`x`, `y` as fractions of the canvas, top-left).
    pub fn webcam_rect(cam: &Webcam, canvas_w: u32, canvas_h: u32, aspect: f32) -> Rect {
        let w = (canvas_w as f32 * cam.size.clamp(0.1, 0.6)).round() as i32;
        let h = (w as f32 / aspect.max(0.1)).round() as i32;
        let margin = (canvas_w / 60) as i32;
        let (cw, ch) = (canvas_w as i32, canvas_h as i32);
        let (x, y) = match (cam.corner.as_str(), cam.x, cam.y) {
            ("free", Some(fx), Some(fy)) => (
                (fx * canvas_w as f32).round() as i32,
                (fy * canvas_h as f32).round() as i32,
            ),
            ("tl", ..) => (margin, margin),
            ("tr", ..) => (cw - w - margin, margin),
            ("bl", ..) => (margin, ch - h - margin),
            _ => (cw - w - margin, ch - h - margin),
        };
        clamp(Rect { x, y, w, h }, canvas_w, canvas_h)
    }

    /// Where an overlay goes. Images are sized by `w` (fraction of the
    /// canvas width) and keep their aspect ratio; text has a natural size
    /// (`natural` = rendered texture size) that comes from its font size.
    pub fn overlay_rect(ov: &Overlay, canvas_w: u32, canvas_h: u32, natural: (u32, u32)) -> Rect {
        let (nw, nh) = (natural.0.max(1) as f32, natural.1.max(1) as f32);
        let (w, h) = match ov.kind {
            OverlayKind::Text { .. } => (nw.round() as i32, nh.round() as i32),
            OverlayKind::Image { .. } => {
                let w = canvas_w as f32 * ov.w.clamp(0.02, 1.0);
                (w.round() as i32, (w * nh / nw).round() as i32)
            }
        };
        clamp(
            Rect {
                x: (ov.x * canvas_w as f32).round() as i32,
                y: (ov.y * canvas_h as f32).round() as i32,
                w,
                h,
            },
            canvas_w,
            canvas_h,
        )
    }

    /// The 16:9 canvas for a base of this size: 720p, 1080p or 1440p,
    /// whichever is the largest that does not upscale the base height.
    /// 1440p is the ceiling (one CPU copy per output stays cheap there).
    pub fn canvas_for(base_h: u32) -> (u32, u32) {
        match base_h {
            h if h >= 1440 => (2560, 1440),
            h if h >= 1080 => (1920, 1080),
            _ => (1280, 720),
        }
    }

    /// Normalised device coordinates of a pixel rectangle (x0, y0 top,
    /// x1, y1 bottom), for the quad vertex shader.
    pub fn ndc(r: Rect, canvas_w: u32, canvas_h: u32) -> [f32; 4] {
        let (cw, ch) = (canvas_w as f32, canvas_h as f32);
        [
            r.x as f32 / cw * 2.0 - 1.0,
            1.0 - r.y as f32 / ch * 2.0,
            (r.x + r.w) as f32 / cw * 2.0 - 1.0,
            1.0 - (r.y + r.h) as f32 / ch * 2.0,
        ]
    }
}

// ====================================================================
// Public API
// ====================================================================

/// What the compositor captures as its bottom layer.
#[derive(Clone, Debug)]
pub enum Base {
    /// A monitor, by GDI device name (`\\.\DISPLAY1`).
    Monitor { device: String },
    /// A top-level window.
    Window { hwnd: isize },
}

/// Everything drawn on top of the base.
#[derive(Clone, Default)]
pub struct Layers {
    pub webcam: Option<Webcam>,
    pub overlays: Vec<Overlay>,
}

impl Layers {
    fn webcam_device(&self) -> Option<String> {
        self.webcam
            .as_ref()
            .filter(|c| c.enabled && !c.device.is_empty())
            .map(|c| c.device.clone())
    }
}

/// A running capture + compose session. Dropping it stops everything.
pub struct Session {
    shared: Arc<Shared>,
    control: Mutex<Option<CaptureControl<Grab, String>>>,
}

struct Shared {
    canvas: (u32, u32),
    base: Base,
    ffmpeg: Option<PathBuf>,
    gpu: Mutex<Option<Gpu>>,
    layers: Mutex<Layers>,
    webcam: Mutex<Option<WebcamFeed>>,
    pipes: Mutex<Vec<Weak<FramePipe>>>,
    stop: AtomicBool,
    closed: AtomicBool,
    error: Mutex<Option<String>>,
    // counters for the status panel
    captured: AtomicU64,
    rendered: AtomicU64,
    base_size: Mutex<(u32, u32)>,
    rates: Mutex<(Instant, u64, u64, f64, f64)>,
}

impl Session {
    /// Starts capturing `base` and composing at `canvas` size. Returns once
    /// the first frame has been captured (or after a short timeout, so a
    /// minimised window still gives a session that shows black).
    pub fn start(
        base: Base,
        canvas: (u32, u32),
        layers: Layers,
        ffmpeg: Option<PathBuf>,
    ) -> Result<Arc<Session>, String> {
        let canvas = (canvas.0 / 2 * 2, canvas.1 / 2 * 2);
        if canvas.0 < 16 || canvas.1 < 16 {
            return Err("Output size is too small".into());
        }
        let shared = Arc::new(Shared {
            canvas,
            base: base.clone(),
            ffmpeg,
            gpu: Mutex::new(None),
            layers: Mutex::new(Layers::default()),
            webcam: Mutex::new(None),
            pipes: Mutex::new(Vec::new()),
            stop: AtomicBool::new(false),
            closed: AtomicBool::new(false),
            error: Mutex::new(None),
            captured: AtomicU64::new(0),
            rendered: AtomicU64::new(0),
            base_size: Mutex::new((0, 0)),
            rates: Mutex::new((Instant::now(), 0, 0, 0.0, 0.0)),
        });
        let session = Arc::new(Session {
            shared: shared.clone(),
            control: Mutex::new(None),
        });
        session.set_layers(layers);

        let border = if GraphicsCaptureApi::is_border_settings_supported().unwrap_or(false) {
            DrawBorderSettings::WithoutBorder
        } else {
            DrawBorderSettings::Default
        };
        let cursor = CursorCaptureSettings::WithCursor;
        let control = match &base {
            Base::Window { hwnd } => {
                let w = windows_capture::window::Window::from_raw_hwnd(*hwnd as *mut _);
                if !w.is_valid() {
                    return Err("That window is no longer open".into());
                }
                let settings = Settings::new(
                    w,
                    cursor,
                    border,
                    SecondaryWindowSettings::Default,
                    MinimumUpdateIntervalSettings::Default,
                    DirtyRegionSettings::Default,
                    ColorFormat::Bgra8,
                    shared.clone(),
                );
                Grab::start_free_threaded(settings)
            }
            Base::Monitor { device } => {
                let m = windows_capture::monitor::Monitor::enumerate()
                    .map_err(|e| e.to_string())?
                    .into_iter()
                    .find(|m| m.device_name().ok().as_deref() == Some(device.as_str()))
                    .or_else(|| windows_capture::monitor::Monitor::primary().ok())
                    .ok_or("No monitor found")?;
                let settings = Settings::new(
                    m,
                    cursor,
                    border,
                    SecondaryWindowSettings::Default,
                    MinimumUpdateIntervalSettings::Default,
                    DirtyRegionSettings::Default,
                    ColorFormat::Bgra8,
                    shared.clone(),
                );
                Grab::start_free_threaded(settings)
            }
        }
        .map_err(|e| friendly_capture_error(&format!("{e:?}")))?;
        *session.control.lock().unwrap() = Some(control);

        // The 60 Hz compose + readback thread.
        let s = shared.clone();
        std::thread::Builder::new()
            .name("compositor".into())
            .spawn(move || ticker(s))
            .map_err(|e| e.to_string())?;

        // Wait briefly for the device (created on the capture thread).
        let until = Instant::now() + Duration::from_secs(3);
        while Instant::now() < until {
            if shared.gpu.lock().unwrap().is_some() {
                break;
            }
            if let Some(e) = shared.error.lock().unwrap().clone() {
                return Err(e);
            }
            std::thread::sleep(Duration::from_millis(20));
        }
        if shared.gpu.lock().unwrap().is_none() {
            return Err("Capture did not start (is the window minimised?)".into());
        }
        Ok(session)
    }

    pub fn canvas(&self) -> (u32, u32) {
        self.shared.canvas
    }

    pub fn base(&self) -> &Base {
        &self.shared.base
    }

    /// Replaces the layers; takes effect on the next frame. Starts or stops
    /// the webcam feed as needed.
    pub fn set_layers(&self, layers: Layers) {
        let want = layers.webcam_device();
        let mut cam = self.shared.webcam.lock().unwrap();
        if cam.as_ref().map(|c| &c.device) != want.as_ref() {
            *cam = None;
            if let (Some(dev), Some(ff)) = (want, self.shared.ffmpeg.clone()) {
                *cam = Some(WebcamFeed::start(ff, dev));
            }
        }
        drop(cam);
        *self.shared.layers.lock().unwrap() = layers;
    }

    /// A new named pipe that receives every composed NV12 frame. FFmpeg
    /// opens `pipe.path` as a `rawvideo` input.
    pub fn new_pipe(&self) -> Arc<FramePipe> {
        let p = FramePipe::new();
        self.shared.pipes.lock().unwrap().push(Arc::downgrade(&p));
        p
    }

    pub fn status_json(&self) -> Value {
        let s = &self.shared;
        let (cap_fps, ren_fps) = {
            let mut r = s.rates.lock().unwrap();
            let secs = r.0.elapsed().as_secs_f64();
            if secs >= 1.0 {
                let c = s.captured.load(Ordering::Relaxed);
                let n = s.rendered.load(Ordering::Relaxed);
                r.3 = (c - r.1) as f64 / secs;
                r.4 = (n - r.2) as f64 / secs;
                *r = (Instant::now(), c, n, r.3, r.4);
            }
            (r.3, r.4)
        };
        let base = *s.base_size.lock().unwrap();
        let dropped: u64 = s
            .pipes
            .lock()
            .unwrap()
            .iter()
            .filter_map(|p| p.upgrade())
            .map(|p| p.dropped.load(Ordering::Relaxed))
            .sum();
        // Rendered size of each overlay, so the preview can draw matching
        // drag boxes (text size depends on the font).
        let layers: serde_json::Map<String, Value> = s
            .gpu
            .lock()
            .unwrap()
            .as_ref()
            .map(|g| {
                g.overlays
                    .iter()
                    .map(|(id, o)| (id.clone(), json!([o.tex.w, o.tex.h])))
                    .collect()
            })
            .unwrap_or_default();
        json!({
            "canvas": { "width": s.canvas.0, "height": s.canvas.1 },
            "layers": layers,
            "base": { "width": base.0, "height": base.1 },
            "captureFps": cap_fps.round(),
            "fps": ren_fps.round(),
            "closed": s.closed.load(Ordering::SeqCst),
            "error": s.error.lock().unwrap().clone(),
            "dropped": dropped,
            "webcam": s.webcam.lock().unwrap().as_ref().map(|c| c.frames.load(Ordering::Relaxed) > 0),
        })
    }
}

impl Drop for Session {
    fn drop(&mut self) {
        self.shared.stop.store(true, Ordering::SeqCst);
        if let Some(c) = self.control.lock().unwrap().take() {
            c.stop().ok();
        }
        *self.shared.webcam.lock().unwrap() = None;
    }
}

fn friendly_capture_error(e: &str) -> String {
    let l = e.to_lowercase();
    if l.contains("unsupported") || l.contains("not supported") {
        "Window capture needs Windows 10 version 1903 or newer".into()
    } else if l.contains("access") || l.contains("denied") {
        "Windows did not allow capturing this window (protected content?)".into()
    } else {
        format!(
            "Could not start capture: {}",
            e.chars().take(120).collect::<String>()
        )
    }
}

// ====================================================================
// Capture handler (runs on windows-capture's thread)
// ====================================================================

struct Grab {
    shared: Arc<Shared>,
    last_crop_check: Instant,
}

impl GraphicsCaptureApiHandler for Grab {
    type Flags = Arc<Shared>;
    type Error = String;

    fn new(ctx: Context<Self::Flags>) -> Result<Self, Self::Error> {
        let shared = ctx.flags;
        let gpu =
            Gpu::new(ctx.device, ctx.device_context, shared.canvas, &shared.base).map_err(|e| {
                let msg = format!("GPU setup failed: {e}");
                *shared.error.lock().unwrap() = Some(msg.clone());
                msg
            })?;
        *shared.gpu.lock().unwrap() = Some(gpu);
        Ok(Grab {
            shared,
            last_crop_check: Instant::now(),
        })
    }

    fn on_frame_arrived(
        &mut self,
        frame: &mut Frame,
        control: InternalCaptureControl,
    ) -> Result<(), Self::Error> {
        if self.shared.stop.load(Ordering::SeqCst) {
            control.stop();
            return Ok(());
        }
        let mut g = self.shared.gpu.lock().unwrap();
        if let Some(gpu) = g.as_mut() {
            gpu.take_base(frame.as_raw_texture(), frame.width(), frame.height())
                .map_err(|e| e.to_string())?;
            if self.last_crop_check.elapsed() > Duration::from_millis(500) {
                self.last_crop_check = Instant::now();
                gpu.refresh_crop(&self.shared.base);
            }
            *self.shared.base_size.lock().unwrap() = (frame.width(), frame.height());
        }
        self.shared.captured.fetch_add(1, Ordering::Relaxed);
        Ok(())
    }

    fn on_closed(&mut self) -> Result<(), Self::Error> {
        self.shared.closed.store(true, Ordering::SeqCst);
        crate::diag::log("compositor", "captured window closed");
        Ok(())
    }
}

// ====================================================================
// 60 Hz compose + readback
// ====================================================================

const FRAME: Duration = Duration::from_micros(16_667);

fn ticker(shared: Arc<Shared>) {
    // Wait for the device.
    while !shared.stop.load(Ordering::SeqCst) && shared.gpu.lock().unwrap().is_none() {
        std::thread::sleep(Duration::from_millis(10));
    }
    let mut next = Instant::now();
    while !shared.stop.load(Ordering::SeqCst) {
        let frame = {
            let mut g = shared.gpu.lock().unwrap();
            let Some(gpu) = g.as_mut() else { break };
            let layers = shared.layers.lock().unwrap().clone();
            let cam = shared
                .webcam
                .lock()
                .unwrap()
                .as_ref()
                .and_then(|c| c.latest.lock().unwrap().take());
            match gpu.render(&layers, cam) {
                Ok(f) => f,
                Err(e) => {
                    *shared.error.lock().unwrap() = Some(format!("Render failed: {e}"));
                    crate::diag::log("compositor", format!("render failed: {e}"));
                    None
                }
            }
        };
        if let Some(f) = frame {
            shared.rendered.fetch_add(1, Ordering::Relaxed);
            let f = Arc::new(f);
            let mut pipes = shared.pipes.lock().unwrap();
            pipes.retain(|w| match w.upgrade() {
                Some(p) => {
                    p.push(f.clone());
                    true
                }
                None => false,
            });
        }
        next += FRAME;
        let now = Instant::now();
        if now > next + FRAME * 3 {
            next = now + FRAME; // fell far behind: resync rather than burst
        }
        if next > now {
            std::thread::sleep(next - now);
        }
    }
}

// ====================================================================
// Named pipe to one FFmpeg job
// ====================================================================

/// One output's feed: a named pipe server that re-listens whenever FFmpeg
/// reconnects, with a 3-frame queue in front of it.
pub struct FramePipe {
    pub path: String,
    tx: SyncSender<Arc<Vec<u8>>>,
    stop: Arc<AtomicBool>,
    pub dropped: AtomicU64,
}

static PIPE_SEQ: AtomicU64 = AtomicU64::new(0);

impl FramePipe {
    pub fn new() -> Arc<FramePipe> {
        let n = PIPE_SEQ.fetch_add(1, Ordering::Relaxed);
        let path = format!(
            "\\\\.\\pipe\\phonecast-{}-{}-{}",
            std::process::id(),
            n,
            Instant::now().elapsed().as_nanos() as u32 ^ (n as u32).rotate_left(7)
        );
        let (tx, rx) = sync_channel::<Arc<Vec<u8>>>(3);
        let stop = Arc::new(AtomicBool::new(false));
        let (p, s) = (path.clone(), stop.clone());
        std::thread::Builder::new()
            .name("frame-pipe".into())
            .spawn(move || pipe_server(&p, rx, &s))
            .ok();
        Arc::new(FramePipe {
            path,
            tx,
            stop,
            dropped: AtomicU64::new(0),
        })
    }

    fn push(&self, f: Arc<Vec<u8>>) {
        if self.tx.try_send(f).is_err() {
            self.dropped.fetch_add(1, Ordering::Relaxed);
        }
    }
}

impl FramePipe {
    /// Stops feeding: the reader sees the end of the stream (FFmpeg then
    /// finishes its output), and the pipe does not listen again.
    pub fn close(&self) {
        if !self.stop.swap(true, Ordering::SeqCst) {
            // Unblock ConnectNamedPipe if no reader ever connected.
            open_pipe_client(&self.path).ok();
        }
    }
}

impl Drop for FramePipe {
    fn drop(&mut self) {
        self.close();
    }
}

/// Opens the read end of a named pipe the way FFmpeg does (plain
/// `CreateFileW`). `std::fs::File::open` must not be used for pipes: it
/// connects, then reports "all pipe instances are busy" and leaves the
/// server side connected to a dead client.
pub fn open_pipe_client(path: &str) -> std::io::Result<std::fs::File> {
    use std::os::windows::io::FromRawHandle;
    use windows_sys::Win32::Foundation::INVALID_HANDLE_VALUE;
    use windows_sys::Win32::Storage::FileSystem::{
        CreateFileW, FILE_ATTRIBUTE_NORMAL, FILE_GENERIC_READ, OPEN_EXISTING,
    };
    let wide: Vec<u16> = path.encode_utf16().chain(std::iter::once(0)).collect();
    let h = unsafe {
        CreateFileW(
            wide.as_ptr(),
            FILE_GENERIC_READ,
            0,
            std::ptr::null(),
            OPEN_EXISTING,
            FILE_ATTRIBUTE_NORMAL,
            std::ptr::null_mut(),
        )
    };
    if h == INVALID_HANDLE_VALUE {
        return Err(std::io::Error::last_os_error());
    }
    Ok(unsafe { std::fs::File::from_raw_handle(h as _) })
}

fn pipe_server(path: &str, rx: Receiver<Arc<Vec<u8>>>, stop: &AtomicBool) {
    use windows_sys::Win32::Foundation::{
        CloseHandle, GetLastError, ERROR_PIPE_CONNECTED, INVALID_HANDLE_VALUE,
    };
    use windows_sys::Win32::Storage::FileSystem::{
        FlushFileBuffers, WriteFile, PIPE_ACCESS_OUTBOUND,
    };
    use windows_sys::Win32::System::Pipes::{
        ConnectNamedPipe, CreateNamedPipeW, DisconnectNamedPipe, PIPE_READMODE_BYTE,
        PIPE_TYPE_BYTE, PIPE_WAIT,
    };
    let wide: Vec<u16> = path.encode_utf16().chain(std::iter::once(0)).collect();
    while !stop.load(Ordering::SeqCst) {
        let h = unsafe {
            CreateNamedPipeW(
                wide.as_ptr(),
                PIPE_ACCESS_OUTBOUND,
                PIPE_TYPE_BYTE | PIPE_READMODE_BYTE | PIPE_WAIT,
                1,
                16 << 20,
                0,
                0,
                std::ptr::null(),
            )
        };
        if h == INVALID_HANDLE_VALUE {
            let err = unsafe { GetLastError() };
            crate::diag::log(
                "compositor",
                format!("could not create pipe {path}: error {err}"),
            );
            std::thread::sleep(Duration::from_secs(1));
            continue;
        }
        let ok = unsafe { ConnectNamedPipe(h, std::ptr::null_mut()) } != 0
            || unsafe { GetLastError() } == ERROR_PIPE_CONNECTED;
        if stop.load(Ordering::SeqCst) || !ok {
            unsafe { CloseHandle(h) };
            if !ok {
                std::thread::sleep(Duration::from_millis(100));
            }
            continue;
        }
        // Start the client on fresh frames, not a stale queue.
        while rx.try_recv().is_ok() {}
        let mut disconnected = false;
        while !stop.load(Ordering::SeqCst) {
            let f = match rx.recv_timeout(Duration::from_millis(200)) {
                Ok(f) => f,
                Err(RecvTimeoutError::Timeout) => {
                    // No frames: probe with an empty write so a client that
                    // went away is noticed and the pipe re-listens.
                    let mut n = 0u32;
                    let r =
                        unsafe { WriteFile(h, [0u8; 1].as_ptr(), 0, &mut n, std::ptr::null_mut()) };
                    if r == 0 {
                        break;
                    }
                    continue;
                }
                Err(RecvTimeoutError::Disconnected) => {
                    disconnected = true;
                    break;
                }
            };
            let mut off = 0usize;
            let mut failed = false;
            while off < f.len() {
                let mut n = 0u32;
                let r = unsafe {
                    WriteFile(
                        h,
                        f.as_ptr().add(off),
                        (f.len() - off) as u32,
                        &mut n,
                        std::ptr::null_mut(),
                    )
                };
                if r == 0 || n == 0 {
                    failed = true;
                    break;
                }
                off += n as usize;
            }
            if failed {
                break; // FFmpeg went away; listen for the next one
            }
        }
        unsafe {
            FlushFileBuffers(h);
            DisconnectNamedPipe(h);
            CloseHandle(h);
        }
        if disconnected {
            break;
        }
    }
}

// ====================================================================
// Webcam feed (FFmpeg dshow → raw BGRA on stdout)
// ====================================================================

pub const CAM_W: u32 = 1280;
pub const CAM_H: u32 = 720;

struct WebcamFeed {
    device: String,
    latest: Arc<Mutex<Option<Vec<u8>>>>,
    frames: Arc<AtomicU64>,
    stop: Arc<AtomicBool>,
    child: Arc<Mutex<Option<Child>>>,
}

impl WebcamFeed {
    fn start(ffmpeg: PathBuf, device: String) -> WebcamFeed {
        let latest = Arc::new(Mutex::new(None));
        let frames = Arc::new(AtomicU64::new(0));
        let stop = Arc::new(AtomicBool::new(false));
        let child = Arc::new(Mutex::new(None));
        let (l, f, s, c, dev) = (
            latest.clone(),
            frames.clone(),
            stop.clone(),
            child.clone(),
            device.clone(),
        );
        std::thread::Builder::new()
            .name("webcam".into())
            .spawn(move || {
                while !s.load(Ordering::SeqCst) {
                    if let Err(e) = Self::run(&ffmpeg, &dev, &l, &f, &s, &c) {
                        crate::diag::log("compositor", format!("webcam: {e}"));
                    }
                    if !s.load(Ordering::SeqCst) {
                        std::thread::sleep(Duration::from_secs(2));
                    }
                }
            })
            .ok();
        WebcamFeed {
            device,
            latest,
            frames,
            stop,
            child,
        }
    }

    fn run(
        ffmpeg: &PathBuf,
        device: &str,
        latest: &Mutex<Option<Vec<u8>>>,
        frames: &AtomicU64,
        stop: &AtomicBool,
        slot: &Mutex<Option<Child>>,
    ) -> std::io::Result<()> {
        let mut cmd = Command::new(ffmpeg);
        cmd.args(["-hide_banner", "-loglevel", "error", "-nostats"]);
        cmd.args([
            "-f",
            "dshow",
            "-rtbufsize",
            "64M",
            "-video_size",
            &format!("{CAM_W}x{CAM_H}"),
            "-framerate",
            "30",
            "-i",
            &format!("video={device}"),
            "-an",
            "-vf",
            &format!("scale={CAM_W}:{CAM_H}"),
            "-pix_fmt",
            "bgra",
            "-f",
            "rawvideo",
            "pipe:1",
        ]);
        cmd.stdin(Stdio::null())
            .stdout(Stdio::piped())
            .stderr(Stdio::null());
        #[cfg(windows)]
        {
            use std::os::windows::process::CommandExt;
            cmd.creation_flags(0x0800_0000);
        }
        let mut child = cmd.spawn()?;
        let mut out = child.stdout.take().unwrap();
        *slot.lock().unwrap() = Some(child);
        let size = (CAM_W * CAM_H * 4) as usize;
        let result = loop {
            if stop.load(Ordering::SeqCst) {
                break Ok(());
            }
            let mut buf = vec![0u8; size];
            if let Err(e) = out.read_exact(&mut buf) {
                break Err(e);
            }
            *latest.lock().unwrap() = Some(buf);
            frames.fetch_add(1, Ordering::Relaxed);
        };
        if let Some(mut c) = slot.lock().unwrap().take() {
            c.kill().ok();
            c.wait().ok();
        }
        result
    }
}

impl Drop for WebcamFeed {
    fn drop(&mut self) {
        self.stop.store(true, Ordering::SeqCst);
        if let Some(mut c) = self.child.lock().unwrap().take() {
            c.kill().ok();
            c.wait().ok();
        }
    }
}

// ====================================================================
// Direct3D 11 state
// ====================================================================

const SHADERS: &str = r#"
cbuffer Quad : register(b0) { float4 pos; float4 uv; float4 misc; };
struct V { float4 p : SV_Position; float2 t : TEXCOORD0; };
V vs(uint id : SV_VertexID) {
    float2 c = float2(id & 1, (id >> 1) & 1);
    V o;
    o.p = float4(lerp(pos.x, pos.z, c.x), lerp(pos.y, pos.w, c.y), 0, 1);
    o.t = float2(lerp(uv.x, uv.z, c.x), lerp(uv.y, uv.w, c.y));
    return o;
}
Texture2D tex : register(t0);
SamplerState smp : register(s0);
// Premultiplied-alpha layer with opacity.
float4 ps_layer(V i) : SV_Target { return tex.Sample(smp, i.t) * misc.x; }
// BT.709 limited range, Y plane.
float4 ps_y(V i) : SV_Target {
    float3 c = tex.Sample(smp, i.t).rgb;
    float y = dot(c, float3(0.2126, 0.7152, 0.0722));
    return float4((16.0 + 219.0 * y) / 255.0, 0, 0, 1);
}
// BT.709 limited range, interleaved UV plane at half resolution.
float4 ps_uv(V i) : SV_Target {
    float3 c = tex.Sample(smp, i.t).rgb;
    float u = dot(c, float3(-0.1146, -0.3854, 0.5));
    float v = dot(c, float3(0.5, -0.4542, -0.0458));
    return float4((128.0 + 224.0 * u) / 255.0, (128.0 + 224.0 * v) / 255.0, 0, 1);
}
"#;

#[repr(C)]
#[derive(Clone, Copy)]
struct Quad {
    pos: [f32; 4],
    uv: [f32; 4],
    misc: [f32; 4],
}

struct Tex {
    tex: ID3D11Texture2D,
    srv: ID3D11ShaderResourceView,
    w: u32,
    h: u32,
}

struct Target {
    tex: ID3D11Texture2D,
    rtv: ID3D11RenderTargetView,
    srv: Option<ID3D11ShaderResourceView>,
    w: u32,
    h: u32,
}

struct OverlayTex {
    key: String,
    tex: Tex,
}

struct Gpu {
    device: ID3D11Device,
    ctx: ID3D11DeviceContext,
    canvas: (u32, u32),
    vs: ID3D11VertexShader,
    ps_layer: ID3D11PixelShader,
    ps_y: ID3D11PixelShader,
    ps_uv: ID3D11PixelShader,
    cbuf: ID3D11Buffer,
    sampler: ID3D11SamplerState,
    blend: ID3D11BlendState,
    raster: ID3D11RasterizerState,
    composite: Target,
    y_plane: Target,
    uv_plane: Target,
    /// Staging ring: [i][0] = Y, [i][1] = UV.
    staging: [[ID3D11Texture2D; 2]; 2],
    ring: usize,
    frames: u64,
    base: Option<Tex>,
    /// Pixels to cut from the base (left, top, right, bottom): window frame.
    crop: (u32, u32, u32, u32),
    webcam: Option<Tex>,
    overlays: HashMap<String, OverlayTex>,
    d2d: Option<(ID2D1Factory, IDWriteFactory)>,
}

type D3DResult<T> = windows::core::Result<T>;

fn compile(src: &str, entry: &[u8], target: &[u8]) -> Result<Vec<u8>, String> {
    let mut blob: Option<ID3DBlob> = None;
    let mut err: Option<ID3DBlob> = None;
    let r = unsafe {
        D3DCompile(
            src.as_ptr() as *const _,
            src.len(),
            PCSTR::null(),
            None,
            None,
            PCSTR(entry.as_ptr()),
            PCSTR(target.as_ptr()),
            0,
            0,
            &mut blob,
            Some(&mut err),
        )
    };
    if let Err(e) = r {
        let msg = err
            .map(|b| unsafe {
                String::from_utf8_lossy(std::slice::from_raw_parts(
                    b.GetBufferPointer() as *const u8,
                    b.GetBufferSize(),
                ))
                .into_owned()
            })
            .unwrap_or_default();
        return Err(format!("shader compile failed: {e} {msg}"));
    }
    let b = blob.ok_or("shader compile returned nothing")?;
    Ok(unsafe {
        std::slice::from_raw_parts(b.GetBufferPointer() as *const u8, b.GetBufferSize()).to_vec()
    })
}

fn tex_desc(w: u32, h: u32, format: DXGI_FORMAT, bind: u32) -> D3D11_TEXTURE2D_DESC {
    D3D11_TEXTURE2D_DESC {
        Width: w,
        Height: h,
        MipLevels: 1,
        ArraySize: 1,
        Format: format,
        SampleDesc: DXGI_SAMPLE_DESC {
            Count: 1,
            Quality: 0,
        },
        Usage: D3D11_USAGE_DEFAULT,
        BindFlags: bind,
        CPUAccessFlags: 0,
        MiscFlags: 0,
    }
}

impl Gpu {
    fn new(
        device: ID3D11Device,
        ctx: ID3D11DeviceContext,
        canvas: (u32, u32),
        base: &Base,
    ) -> Result<Gpu, String> {
        // The capture thread and our ticker both use the immediate context.
        if let Ok(mt) = ctx.cast::<ID3D11Multithread>() {
            let _ = unsafe { mt.SetMultithreadProtected(true) };
        }
        let vs_bc = compile(SHADERS, b"vs\0", b"vs_5_0\0")?;
        let pl_bc = compile(SHADERS, b"ps_layer\0", b"ps_5_0\0")?;
        let py_bc = compile(SHADERS, b"ps_y\0", b"ps_5_0\0")?;
        let puv_bc = compile(SHADERS, b"ps_uv\0", b"ps_5_0\0")?;
        let r: D3DResult<Gpu> = (|| unsafe {
            let mut vs = None;
            device.CreateVertexShader(&vs_bc, None, Some(&mut vs))?;
            let mut ps_layer = None;
            device.CreatePixelShader(&pl_bc, None, Some(&mut ps_layer))?;
            let mut ps_y = None;
            device.CreatePixelShader(&py_bc, None, Some(&mut ps_y))?;
            let mut ps_uv = None;
            device.CreatePixelShader(&puv_bc, None, Some(&mut ps_uv))?;

            let mut cbuf = None;
            device.CreateBuffer(
                &D3D11_BUFFER_DESC {
                    ByteWidth: std::mem::size_of::<Quad>() as u32,
                    Usage: D3D11_USAGE_DEFAULT,
                    BindFlags: D3D11_BIND_CONSTANT_BUFFER.0 as u32,
                    CPUAccessFlags: 0,
                    MiscFlags: 0,
                    StructureByteStride: 0,
                },
                None,
                Some(&mut cbuf),
            )?;

            let mut sampler = None;
            device.CreateSamplerState(
                &D3D11_SAMPLER_DESC {
                    Filter: D3D11_FILTER_MIN_MAG_MIP_LINEAR,
                    AddressU: D3D11_TEXTURE_ADDRESS_CLAMP,
                    AddressV: D3D11_TEXTURE_ADDRESS_CLAMP,
                    AddressW: D3D11_TEXTURE_ADDRESS_CLAMP,
                    MipLODBias: 0.0,
                    MaxAnisotropy: 1,
                    ComparisonFunc: D3D11_COMPARISON_NEVER,
                    BorderColor: [0.0; 4],
                    MinLOD: 0.0,
                    MaxLOD: 0.0,
                },
                Some(&mut sampler),
            )?;

            // Premultiplied alpha: out = src + dst * (1 - src.a).
            let rt0 = D3D11_RENDER_TARGET_BLEND_DESC {
                BlendEnable: true.into(),
                SrcBlend: D3D11_BLEND_ONE,
                DestBlend: D3D11_BLEND_INV_SRC_ALPHA,
                BlendOp: D3D11_BLEND_OP_ADD,
                SrcBlendAlpha: D3D11_BLEND_ONE,
                DestBlendAlpha: D3D11_BLEND_INV_SRC_ALPHA,
                BlendOpAlpha: D3D11_BLEND_OP_ADD,
                RenderTargetWriteMask: D3D11_COLOR_WRITE_ENABLE_ALL.0 as u8,
            };
            let mut bd = D3D11_BLEND_DESC::default();
            bd.RenderTarget[0] = rt0;
            let mut blend = None;
            device.CreateBlendState(&bd, Some(&mut blend))?;

            let mut raster = None;
            device.CreateRasterizerState(
                &D3D11_RASTERIZER_DESC {
                    FillMode: D3D11_FILL_SOLID,
                    CullMode: D3D11_CULL_NONE,
                    FrontCounterClockwise: false.into(),
                    DepthBias: 0,
                    DepthBiasClamp: 0.0,
                    SlopeScaledDepthBias: 0.0,
                    DepthClipEnable: false.into(),
                    ScissorEnable: false.into(),
                    MultisampleEnable: false.into(),
                    AntialiasedLineEnable: false.into(),
                },
                Some(&mut raster),
            )?;

            let (w, h) = canvas;
            let composite = Self::target(&device, w, h, DXGI_FORMAT_B8G8R8A8_UNORM, true)?;
            let y_plane = Self::target(&device, w, h, DXGI_FORMAT_R8_UNORM, false)?;
            let uv_plane = Self::target(&device, w / 2, h / 2, DXGI_FORMAT_R8G8_UNORM, false)?;
            let st = |w: u32, h: u32, f: DXGI_FORMAT| -> D3DResult<ID3D11Texture2D> {
                let mut d = tex_desc(w, h, f, 0);
                d.Usage = D3D11_USAGE_STAGING;
                d.CPUAccessFlags = D3D11_CPU_ACCESS_READ.0 as u32;
                let mut t = None;
                device.CreateTexture2D(&d, None, Some(&mut t))?;
                Ok(t.unwrap())
            };
            let staging = [
                [
                    st(w, h, DXGI_FORMAT_R8_UNORM)?,
                    st(w / 2, h / 2, DXGI_FORMAT_R8G8_UNORM)?,
                ],
                [
                    st(w, h, DXGI_FORMAT_R8_UNORM)?,
                    st(w / 2, h / 2, DXGI_FORMAT_R8G8_UNORM)?,
                ],
            ];
            let d2d = (|| -> D3DResult<(ID2D1Factory, IDWriteFactory)> {
                let f: ID2D1Factory = D2D1CreateFactory(D2D1_FACTORY_TYPE_SINGLE_THREADED, None)?;
                let dw: IDWriteFactory = DWriteCreateFactory(DWRITE_FACTORY_TYPE_SHARED)?;
                Ok((f, dw))
            })()
            .ok();
            Ok(Gpu {
                device: device.clone(),
                ctx: ctx.clone(),
                canvas,
                vs: vs.unwrap(),
                ps_layer: ps_layer.unwrap(),
                ps_y: ps_y.unwrap(),
                ps_uv: ps_uv.unwrap(),
                cbuf: cbuf.unwrap(),
                sampler: sampler.unwrap(),
                blend: blend.unwrap(),
                raster: raster.unwrap(),
                composite,
                y_plane,
                uv_plane,
                staging,
                ring: 0,
                frames: 0,
                base: None,
                crop: (0, 0, 0, 0),
                webcam: None,
                overlays: HashMap::new(),
                d2d,
            })
        })();
        let mut gpu = r.map_err(|e| e.to_string())?;
        gpu.refresh_crop(base);
        Ok(gpu)
    }

    fn target(
        device: &ID3D11Device,
        w: u32,
        h: u32,
        format: DXGI_FORMAT,
        sampled: bool,
    ) -> D3DResult<Target> {
        let mut bind = D3D11_BIND_RENDER_TARGET.0 as u32;
        if sampled {
            bind |= D3D11_BIND_SHADER_RESOURCE.0 as u32;
        }
        let desc = tex_desc(w, h, format, bind);
        unsafe {
            let mut tex = None;
            device.CreateTexture2D(&desc, None, Some(&mut tex))?;
            let tex: ID3D11Texture2D = tex.unwrap();
            let mut rtv = None;
            device.CreateRenderTargetView(&tex, None, Some(&mut rtv))?;
            let srv = if sampled {
                let mut s = None;
                device.CreateShaderResourceView(&tex, None, Some(&mut s))?;
                s
            } else {
                None
            };
            Ok(Target {
                tex,
                rtv: rtv.unwrap(),
                srv,
                w,
                h,
            })
        }
    }

    /// A sampled BGRA texture, optionally with initial pixels (premultiplied).
    fn texture(&self, w: u32, h: u32, pixels: Option<&[u8]>, dynamic: bool) -> D3DResult<Tex> {
        let mut desc = tex_desc(
            w,
            h,
            DXGI_FORMAT_B8G8R8A8_UNORM,
            D3D11_BIND_SHADER_RESOURCE.0 as u32,
        );
        if dynamic {
            desc.Usage = D3D11_USAGE_DYNAMIC;
            desc.CPUAccessFlags = D3D11_CPU_ACCESS_WRITE.0 as u32;
        }
        let init = pixels.map(|p| D3D11_SUBRESOURCE_DATA {
            pSysMem: p.as_ptr() as *const _,
            SysMemPitch: w * 4,
            SysMemSlicePitch: 0,
        });
        unsafe {
            let mut tex = None;
            self.device.CreateTexture2D(
                &desc,
                init.as_ref().map(|i| i as *const _),
                Some(&mut tex),
            )?;
            let tex: ID3D11Texture2D = tex.unwrap();
            let mut srv = None;
            self.device
                .CreateShaderResourceView(&tex, None, Some(&mut srv))?;
            Ok(Tex {
                tex,
                srv: srv.unwrap(),
                w,
                h,
            })
        }
    }

    /// GPU copy of the captured frame into our base texture.
    fn take_base(&mut self, frame: &ID3D11Texture2D, w: u32, h: u32) -> D3DResult<()> {
        if self.base.as_ref().is_none_or(|b| b.w != w || b.h != h) {
            self.base = Some(self.texture(w, h, None, false)?);
        }
        if let Some(b) = &self.base {
            unsafe { self.ctx.CopyResource(&b.tex, frame) };
        }
        Ok(())
    }

    /// For windows: how much of the captured picture is title bar / frame.
    fn refresh_crop(&mut self, base: &Base) {
        self.crop = match base {
            Base::Window { hwnd } => window_frame_crop(*hwnd),
            Base::Monitor { .. } => (0, 0, 0, 0),
        };
    }

    fn set_quad(&self, q: &Quad) {
        unsafe {
            self.ctx
                .UpdateSubresource(&self.cbuf, 0, None, q as *const Quad as *const _, 0, 0);
        }
    }

    fn draw(&self, srv: &ID3D11ShaderResourceView, quad: &Quad, ps: &ID3D11PixelShader) {
        self.set_quad(quad);
        unsafe {
            self.ctx.PSSetShader(ps, None);
            self.ctx.PSSetShaderResources(0, Some(&[Some(srv.clone())]));
            self.ctx.Draw(4, 0);
            self.ctx.PSSetShaderResources(0, Some(&[None]));
        }
    }

    fn bind_target(&self, t: &Target) {
        unsafe {
            self.ctx
                .OMSetRenderTargets(Some(&[Some(t.rtv.clone())]), None);
            self.ctx.RSSetViewports(Some(&[D3D11_VIEWPORT {
                TopLeftX: 0.0,
                TopLeftY: 0.0,
                Width: t.w as f32,
                Height: t.h as f32,
                MinDepth: 0.0,
                MaxDepth: 1.0,
            }]));
        }
    }

    /// Composes one frame and returns the NV12 bytes of the *previous* frame
    /// (one frame of latency keeps the GPU from stalling on readback).
    fn render(
        &mut self,
        layers: &Layers,
        cam_frame: Option<Vec<u8>>,
    ) -> D3DResult<Option<Vec<u8>>> {
        let (cw, ch) = self.canvas;
        self.update_webcam(cam_frame)?;
        self.update_overlays(layers)?;

        unsafe {
            self.ctx.IASetInputLayout(None);
            self.ctx
                .IASetPrimitiveTopology(D3D11_PRIMITIVE_TOPOLOGY_TRIANGLESTRIP);
            self.ctx.VSSetShader(&self.vs, None);
            self.ctx
                .VSSetConstantBuffers(0, Some(&[Some(self.cbuf.clone())]));
            self.ctx
                .PSSetConstantBuffers(0, Some(&[Some(self.cbuf.clone())]));
            self.ctx
                .PSSetSamplers(0, Some(&[Some(self.sampler.clone())]));
            self.ctx.RSSetState(&self.raster);
        }

        // Pass 1: composite (BGRA canvas).
        self.bind_target(&self.composite);
        unsafe {
            self.ctx
                .ClearRenderTargetView(&self.composite.rtv, &[0.0, 0.0, 0.0, 1.0]);
            self.ctx.OMSetBlendState(None, None, 0xffff_ffff);
        }
        if let Some(b) = &self.base {
            let (l, t, r, btm) = self.crop;
            let (vis_w, vis_h) = (
                b.w.saturating_sub(l + r).max(1),
                b.h.saturating_sub(t + btm).max(1),
            );
            let rect = layout::fit(vis_w, vis_h, cw, ch);
            let q = Quad {
                pos: layout::ndc(rect, cw, ch),
                uv: [
                    l as f32 / b.w as f32,
                    t as f32 / b.h as f32,
                    (b.w - r) as f32 / b.w as f32,
                    (b.h - btm) as f32 / b.h as f32,
                ],
                misc: [1.0, 0.0, 0.0, 0.0],
            };
            self.draw(&b.srv, &q, &self.ps_layer);
        }
        unsafe {
            self.ctx.OMSetBlendState(&self.blend, None, 0xffff_ffff);
        }
        if let (Some(cam), Some(tex)) = (
            layers.webcam.as_ref().filter(|c| c.enabled),
            self.webcam.as_ref(),
        ) {
            let rect = layout::webcam_rect(cam, cw, ch, CAM_W as f32 / CAM_H as f32);
            let uv = if cam.mirror {
                [1.0, 0.0, 0.0, 1.0]
            } else {
                [0.0, 0.0, 1.0, 1.0]
            };
            let q = Quad {
                pos: layout::ndc(rect, cw, ch),
                uv,
                misc: [1.0, 0.0, 0.0, 0.0],
            };
            self.draw(&tex.srv, &q, &self.ps_layer);
        }
        for ov in &layers.overlays {
            let Some(ot) = self.overlays.get(&ov.id) else {
                continue;
            };
            let rect = layout::overlay_rect(ov, cw, ch, (ot.tex.w, ot.tex.h));
            let q = Quad {
                pos: layout::ndc(rect, cw, ch),
                uv: [0.0, 0.0, 1.0, 1.0],
                misc: [ov.opacity.clamp(0.0, 1.0), 0.0, 0.0, 0.0],
            };
            self.draw(&ot.tex.srv, &q, &self.ps_layer);
        }

        // Pass 2 + 3: NV12 planes.
        unsafe {
            self.ctx.OMSetBlendState(None, None, 0xffff_ffff);
        }
        let full = Quad {
            pos: [-1.0, 1.0, 1.0, -1.0],
            uv: [0.0, 0.0, 1.0, 1.0],
            misc: [1.0, 0.0, 0.0, 0.0],
        };
        let csrv = self.composite.srv.clone().unwrap();
        self.bind_target(&self.y_plane);
        self.draw(&csrv, &full, &self.ps_y);
        self.bind_target(&self.uv_plane);
        self.draw(&csrv, &full, &self.ps_uv);
        unsafe {
            self.ctx.OMSetRenderTargets(None, None);
        }

        // Copy to this tick's staging pair, read back last tick's.
        let cur = self.ring;
        let prev = (self.ring + 1) % 2;
        unsafe {
            self.ctx
                .CopyResource(&self.staging[cur][0], &self.y_plane.tex);
            self.ctx
                .CopyResource(&self.staging[cur][1], &self.uv_plane.tex);
        }
        self.ring = prev;
        self.frames += 1;
        if self.frames < 2 {
            return Ok(None);
        }
        let mut out = vec![0u8; (cw * ch * 3 / 2) as usize];
        self.read_plane(
            &self.staging[prev][0],
            cw,
            ch,
            &mut out[..(cw * ch) as usize],
        )?;
        self.read_plane(
            &self.staging[prev][1],
            cw,
            ch / 2,
            &mut out[(cw * ch) as usize..],
        )?;
        Ok(Some(out))
    }

    fn read_plane(
        &self,
        tex: &ID3D11Texture2D,
        row_bytes: u32,
        rows: u32,
        out: &mut [u8],
    ) -> D3DResult<()> {
        let mut m = D3D11_MAPPED_SUBRESOURCE::default();
        unsafe {
            self.ctx.Map(tex, 0, D3D11_MAP_READ, 0, Some(&mut m))?;
            let pitch = m.RowPitch as usize;
            let rb = row_bytes as usize;
            let src = m.pData as *const u8;
            if pitch == rb {
                std::ptr::copy_nonoverlapping(src, out.as_mut_ptr(), rb * rows as usize);
            } else {
                for r in 0..rows as usize {
                    std::ptr::copy_nonoverlapping(
                        src.add(r * pitch),
                        out.as_mut_ptr().add(r * rb),
                        rb,
                    );
                }
            }
            self.ctx.Unmap(tex, 0);
        }
        Ok(())
    }

    fn update_webcam(&mut self, frame: Option<Vec<u8>>) -> D3DResult<()> {
        let Some(px) = frame else { return Ok(()) };
        if px.len() != (CAM_W * CAM_H * 4) as usize {
            return Ok(());
        }
        if self.webcam.is_none() {
            self.webcam = Some(self.texture(CAM_W, CAM_H, None, true)?);
        }
        let tex = self.webcam.as_ref().unwrap();
        let mut m = D3D11_MAPPED_SUBRESOURCE::default();
        unsafe {
            self.ctx
                .Map(&tex.tex, 0, D3D11_MAP_WRITE_DISCARD, 0, Some(&mut m))?;
            let pitch = m.RowPitch as usize;
            let rb = (CAM_W * 4) as usize;
            let dst = m.pData as *mut u8;
            for r in 0..CAM_H as usize {
                std::ptr::copy_nonoverlapping(px.as_ptr().add(r * rb), dst.add(r * pitch), rb);
            }
            self.ctx.Unmap(&tex.tex, 0);
        }
        Ok(())
    }

    fn update_overlays(&mut self, layers: &Layers) -> D3DResult<()> {
        let ids: Vec<String> = layers.overlays.iter().map(|o| o.id.clone()).collect();
        self.overlays.retain(|id, _| ids.contains(id));
        for ov in &layers.overlays {
            let key = match &ov.kind {
                OverlayKind::Image { path } => format!("img:{path}"),
                OverlayKind::Text { text, size, color } => {
                    format!("txt:{size}:{color}:{text}")
                }
            };
            if self.overlays.get(&ov.id).is_some_and(|o| o.key == key) {
                continue;
            }
            let tex = match &ov.kind {
                OverlayKind::Image { path } => match load_png(path) {
                    Ok((w, h, px)) => self.texture(w, h, Some(&px), false).ok(),
                    Err(e) => {
                        crate::diag::log("compositor", format!("image {path}: {e}"));
                        None
                    }
                },
                OverlayKind::Text { text, size, color } => {
                    let px = (size.clamp(0.02, 0.4) * self.canvas.1 as f32).round();
                    match self.render_text(text, px, color) {
                        Ok(t) => Some(t),
                        Err(e) => {
                            crate::diag::log("compositor", format!("text: {e}"));
                            None
                        }
                    }
                }
            };
            match tex {
                Some(tex) => {
                    self.overlays.insert(ov.id.clone(), OverlayTex { key, tex });
                }
                None => {
                    // Remember the failure so we do not retry every frame.
                    if let Ok(t) = self.texture(2, 2, Some(&[0u8; 16]), false) {
                        self.overlays
                            .insert(ov.id.clone(), OverlayTex { key, tex: t });
                    }
                }
            }
        }
        Ok(())
    }

    /// Text with a soft dark shadow, rendered by DirectWrite into a texture.
    fn render_text(&self, text: &str, px: f32, color: &str) -> D3DResult<Tex> {
        let (d2d, dw) = self
            .d2d
            .as_ref()
            .ok_or_else(|| windows::core::Error::from_hresult(windows::core::HRESULT(-1)))?;
        let (r, g, b) = parse_color(color);
        let text16: Vec<u16> = text.encode_utf16().collect();
        unsafe {
            let fmt = dw.CreateTextFormat(
                windows::core::w!("Segoe UI"),
                None,
                DWRITE_FONT_WEIGHT_BOLD,
                DWRITE_FONT_STYLE_NORMAL,
                DWRITE_FONT_STRETCH_NORMAL,
                px,
                windows::core::w!("en-us"),
            )?;
            fmt.SetWordWrapping(DWRITE_WORD_WRAPPING_NO_WRAP)?;
            let layout = dw.CreateTextLayout(&text16, &fmt, 8192.0, 4096.0)?;
            let mut m = DWRITE_TEXT_METRICS::default();
            layout.GetMetrics(&mut m)?;
            let pad = (px * 0.15).ceil().max(2.0);
            let w = ((m.widthIncludingTrailingWhitespace + pad * 2.0).ceil() as u32).clamp(2, 8192);
            let h = ((m.height + pad * 2.0).ceil() as u32).clamp(2, 4096);

            let mut desc = tex_desc(
                w,
                h,
                DXGI_FORMAT_B8G8R8A8_UNORM,
                D3D11_BIND_SHADER_RESOURCE.0 as u32 | D3D11_BIND_RENDER_TARGET.0 as u32,
            );
            desc.MiscFlags = 0;
            let mut tex = None;
            self.device.CreateTexture2D(&desc, None, Some(&mut tex))?;
            let tex: ID3D11Texture2D = tex.unwrap();
            let surf: IDXGISurface = tex.cast()?;
            let rt = d2d.CreateDxgiSurfaceRenderTarget(
                &surf,
                &D2D1_RENDER_TARGET_PROPERTIES {
                    r#type: D2D1_RENDER_TARGET_TYPE_DEFAULT,
                    pixelFormat: D2D1_PIXEL_FORMAT {
                        format: DXGI_FORMAT_B8G8R8A8_UNORM,
                        alphaMode: D2D1_ALPHA_MODE_PREMULTIPLIED,
                    },
                    dpiX: 96.0,
                    dpiY: 96.0,
                    usage: D2D1_RENDER_TARGET_USAGE_NONE,
                    minLevel: D2D1_FEATURE_LEVEL_DEFAULT,
                },
            )?;
            rt.BeginDraw();
            rt.Clear(Some(&D2D1_COLOR_F {
                r: 0.0,
                g: 0.0,
                b: 0.0,
                a: 0.0,
            }));
            let shadow = rt.CreateSolidColorBrush(
                &D2D1_COLOR_F {
                    r: 0.0,
                    g: 0.0,
                    b: 0.0,
                    a: 0.75,
                },
                None,
            )?;
            let brush = rt.CreateSolidColorBrush(&D2D1_COLOR_F { r, g, b, a: 1.0 }, None)?;
            let off = (px * 0.05).max(1.0);
            for (dx, dy) in [(off, off), (-off, off), (off, -off), (-off, -off)] {
                rt.DrawTextLayout(
                    Vector2 {
                        X: pad + dx,
                        Y: pad + dy,
                    },
                    &layout,
                    &shadow,
                    D2D1_DRAW_TEXT_OPTIONS_NONE,
                );
            }
            rt.DrawTextLayout(
                Vector2 { X: pad, Y: pad },
                &layout,
                &brush,
                D2D1_DRAW_TEXT_OPTIONS_NONE,
            );
            rt.EndDraw(None, None)?;
            let mut srv = None;
            self.device
                .CreateShaderResourceView(&tex, None, Some(&mut srv))?;
            Ok(Tex {
                tex,
                srv: srv.unwrap(),
                w,
                h,
            })
        }
    }
}

/// "#rrggbb" (or "#rgb") → linear-ish floats; white when unparsable.
pub fn parse_color(s: &str) -> (f32, f32, f32) {
    let h = s.trim().trim_start_matches('#');
    let v = |a: &str| u8::from_str_radix(a, 16).ok();
    let rgb = (|| match h.len() {
        6 => Some((v(&h[0..2])?, v(&h[2..4])?, v(&h[4..6])?)),
        3 => {
            let d = |a: &str| v(a).map(|x| x * 17);
            Some((d(&h[0..1])?, d(&h[1..2])?, d(&h[2..3])?))
        }
        _ => None,
    })();
    fn conv(x: u8) -> f32 {
        x as f32 / 255.0
    }
    match rgb {
        Some((r, g, b)) => (conv(r), conv(g), conv(b)),
        None => (1.0, 1.0, 1.0),
    }
}

/// Decodes a PNG to premultiplied BGRA.
pub fn load_png(path: &str) -> Result<(u32, u32, Vec<u8>), String> {
    let file = std::fs::File::open(path).map_err(|e| e.to_string())?;
    let mut dec = png::Decoder::new(std::io::BufReader::new(file));
    dec.set_transformations(
        png::Transformations::normalize_to_color8() | png::Transformations::ALPHA,
    );
    let mut reader = dec.read_info().map_err(|e| e.to_string())?;
    let mut buf = vec![0; reader.output_buffer_size()];
    let info = reader.next_frame(&mut buf).map_err(|e| e.to_string())?;
    let (w, h) = (info.width, info.height);
    if w == 0 || h == 0 || w > 8192 || h > 8192 {
        return Err("image is empty or larger than 8192 px".into());
    }
    let n = (w * h) as usize;
    let mut out = vec![0u8; n * 4];
    match info.color_type {
        png::ColorType::Rgba => {
            for i in 0..n {
                let (r, g, b, a) = (buf[i * 4], buf[i * 4 + 1], buf[i * 4 + 2], buf[i * 4 + 3]);
                out[i * 4..i * 4 + 4].copy_from_slice(&premul(b, g, r, a));
            }
        }
        png::ColorType::GrayscaleAlpha => {
            for i in 0..n {
                let (v, a) = (buf[i * 2], buf[i * 2 + 1]);
                out[i * 4..i * 4 + 4].copy_from_slice(&premul(v, v, v, a));
            }
        }
        other => return Err(format!("unsupported PNG colour type {other:?}")),
    }
    Ok((w, h, out))
}

fn premul(b: u8, g: u8, r: u8, a: u8) -> [u8; 4] {
    let m = |c: u8| ((c as u32 * a as u32 + 127) / 255) as u8;
    [m(b), m(g), m(r), a]
}

/// How many pixels of the Windows Graphics Capture picture of a window are
/// title bar and frame (left, top, right, bottom), from the DWM frame bounds
/// vs. the client area. Zero for borderless windows.
fn window_frame_crop(hwnd: isize) -> (u32, u32, u32, u32) {
    use windows_sys::Win32::Foundation::{POINT, RECT};
    use windows_sys::Win32::Graphics::Dwm::{DwmGetWindowAttribute, DWMWA_EXTENDED_FRAME_BOUNDS};
    use windows_sys::Win32::Graphics::Gdi::ClientToScreen;
    use windows_sys::Win32::UI::WindowsAndMessaging::{GetClientRect, GetWindowRect};
    unsafe {
        let mut frame: RECT = std::mem::zeroed();
        let ok = DwmGetWindowAttribute(
            hwnd as _,
            DWMWA_EXTENDED_FRAME_BOUNDS as u32,
            &mut frame as *mut _ as *mut _,
            std::mem::size_of::<RECT>() as u32,
        ) == 0;
        if !ok && GetWindowRect(hwnd as _, &mut frame) == 0 {
            return (0, 0, 0, 0);
        }
        let mut client: RECT = std::mem::zeroed();
        if GetClientRect(hwnd as _, &mut client) == 0 {
            return (0, 0, 0, 0);
        }
        let mut origin = POINT { x: 0, y: 0 };
        if ClientToScreen(hwnd as _, &mut origin) == 0 {
            return (0, 0, 0, 0);
        }
        let (cw, ch) = (client.right - client.left, client.bottom - client.top);
        let l = (origin.x - frame.left).max(0);
        let t = (origin.y - frame.top).max(0);
        let r = (frame.right - (origin.x + cw)).max(0);
        let b = (frame.bottom - (origin.y + ch)).max(0);
        // Sanity: never crop more than a third of the picture.
        let fw = (frame.right - frame.left).max(1);
        let fh = (frame.bottom - frame.top).max(1);
        if l + r > fw / 3 || t + b > fh / 3 {
            return (0, 0, 0, 0);
        }
        (l as u32, t as u32, r as u32, b as u32)
    }
}

// ====================================================================
// Tests
// ====================================================================

#[cfg(test)]
mod tests {
    use super::layout::*;
    use super::*;
    use crate::capture::{Overlay, OverlayKind, Webcam};

    #[test]
    fn fit_letterboxes_and_pillarboxes() {
        // 4:3 into 16:9 → pillarbox
        assert_eq!(
            fit(1024, 768, 1920, 1080),
            Rect {
                x: 240,
                y: 0,
                w: 1440,
                h: 1080
            }
        );
        // tall window into 16:9
        let r = fit(500, 1000, 1920, 1080);
        assert_eq!((r.h, r.w), (1080, 540));
        assert_eq!(r.x, (1920 - 540) / 2);
        // same aspect → whole canvas
        assert_eq!(
            fit(2560, 1440, 1920, 1080),
            Rect {
                x: 0,
                y: 0,
                w: 1920,
                h: 1080
            }
        );
        // degenerate input never panics
        assert_eq!(fit(0, 0, 1280, 720).w, 1280);
    }

    #[test]
    fn webcam_corners_and_free_position() {
        let mut cam = Webcam {
            enabled: true,
            device: "c".into(),
            size: 0.25,
            corner: "br".into(),
            mirror: false,
            x: None,
            y: None,
        };
        let r = webcam_rect(&cam, 1920, 1080, 16.0 / 9.0);
        assert_eq!((r.w, r.h), (480, 270));
        assert_eq!((r.x, r.y), (1920 - 480 - 32, 1080 - 270 - 32));
        cam.corner = "tl".into();
        assert_eq!(webcam_rect(&cam, 1920, 1080, 16.0 / 9.0).x, 32);
        cam.corner = "free".into();
        cam.x = Some(0.5);
        cam.y = Some(0.9); // would hang off the bottom → clamped inside
        let r = webcam_rect(&cam, 1920, 1080, 16.0 / 9.0);
        assert_eq!((r.x, r.y), (960, 1080 - 270));
        // free without coordinates falls back to bottom-right
        cam.x = None;
        assert_eq!(webcam_rect(&cam, 1920, 1080, 16.0 / 9.0).x, 1920 - 480 - 32);
    }

    #[test]
    fn overlay_rects() {
        let img = Overlay {
            id: "a".into(),
            kind: OverlayKind::Image {
                path: "x.png".into(),
            },
            x: 0.1,
            y: 0.1,
            w: 0.2,
            opacity: 1.0,
        };
        let r = overlay_rect(&img, 1920, 1080, (400, 200));
        assert_eq!((r.x, r.y, r.w, r.h), (192, 108, 384, 192));
        let text = Overlay {
            id: "t".into(),
            kind: OverlayKind::Text {
                text: "hi".into(),
                size: 0.05,
                color: "#fff".into(),
            },
            x: 0.95,
            y: 0.0,
            w: 0.0,
            opacity: 0.5,
        };
        // natural size, clamped to stay on the canvas
        let r = overlay_rect(&text, 1920, 1080, (300, 60));
        assert_eq!((r.x, r.y, r.w, r.h), (1920 - 300, 0, 300, 60));
    }

    #[test]
    fn canvas_sizes_and_ndc() {
        assert_eq!(canvas_for(2160), (2560, 1440));
        assert_eq!(canvas_for(1440), (2560, 1440));
        assert_eq!(canvas_for(1200), (1920, 1080));
        assert_eq!(canvas_for(1080), (1920, 1080));
        assert_eq!(canvas_for(900), (1280, 720));
        let n = ndc(
            Rect {
                x: 0,
                y: 0,
                w: 1920,
                h: 1080,
            },
            1920,
            1080,
        );
        assert_eq!(n, [-1.0, 1.0, 1.0, -1.0]);
        let n = ndc(
            Rect {
                x: 960,
                y: 540,
                w: 960,
                h: 540,
            },
            1920,
            1080,
        );
        assert_eq!(n, [0.0, 0.0, 1.0, -1.0]);
    }

    #[test]
    fn colours_and_premultiply() {
        assert_eq!(parse_color("#ff0000"), (1.0, 0.0, 0.0));
        assert_eq!(parse_color("0f0"), (0.0, 1.0, 0.0));
        assert_eq!(parse_color("nonsense"), (1.0, 1.0, 1.0));
        assert_eq!(premul(255, 255, 255, 0), [0, 0, 0, 0]);
        assert_eq!(premul(200, 100, 0, 128), [100, 50, 0, 128]);
    }

    /// The pipe server accepts a client, streams frames in order and
    /// re-listens after the client goes away.
    #[test]
    fn frame_pipe_streams_and_reconnects() {
        let pipe = FramePipe::new();
        for round in 0..2u8 {
            // Give the server a moment to (re)create the pipe.
            let mut client = None;
            let mut last_err = None;
            for _ in 0..100 {
                match open_pipe_client(&pipe.path) {
                    Ok(f) => {
                        client = Some(f);
                        break;
                    }
                    Err(e) => last_err = Some(e),
                }
                std::thread::sleep(Duration::from_millis(20));
            }
            let mut client =
                client.unwrap_or_else(|| panic!("connect to {}: {last_err:?}", pipe.path));
            std::thread::sleep(Duration::from_millis(50)); // let the server drain
            for i in 0..3u8 {
                pipe.push(Arc::new(vec![round * 10 + i; 64]));
                std::thread::sleep(Duration::from_millis(5));
            }
            let mut buf = vec![0u8; 192];
            client.read_exact(&mut buf).unwrap();
            assert_eq!(buf[0], round * 10);
            assert_eq!(buf[64], round * 10 + 1);
            assert_eq!(buf[128], round * 10 + 2);
            drop(client);
            std::thread::sleep(Duration::from_millis(50));
        }
        assert_eq!(pipe.dropped.load(Ordering::Relaxed), 0);
    }

    /// End to end: launch Notepad, capture it through Windows Graphics
    /// Capture, compose at 1280×720 and check the first frame is not black.
    /// `cargo test notepad_capture -- --ignored --nocapture`
    #[test]
    #[ignore]
    fn notepad_capture() {
        let mut np = Command::new("notepad.exe").spawn().expect("notepad");
        let mut hwnd = 0isize;
        for _ in 0..100 {
            std::thread::sleep(Duration::from_millis(100));
            if let Some(w) = crate::capture::windows()
                .into_iter()
                .find(|w| w.process.eq_ignore_ascii_case("notepad.exe"))
            {
                hwnd = w.hwnd;
                break;
            }
        }
        assert_ne!(hwnd, 0, "Notepad window not found");
        let session = Session::start(
            Base::Window { hwnd },
            (1280, 720),
            Layers {
                webcam: None,
                overlays: vec![Overlay {
                    id: "t".into(),
                    kind: OverlayKind::Text {
                        text: "PhoneCast".into(),
                        size: 0.08,
                        color: "#ff4060".into(),
                    },
                    x: 0.05,
                    y: 0.05,
                    w: 0.0,
                    opacity: 1.0,
                }],
            },
            None,
        )
        .expect("session");
        let pipe = session.new_pipe();
        let mut client = None;
        for _ in 0..100 {
            if let Ok(f) = open_pipe_client(&pipe.path) {
                client = Some(f);
                break;
            }
            std::thread::sleep(Duration::from_millis(20));
        }
        let mut client = client.expect("pipe");
        let mut frame = vec![0u8; 1280 * 720 * 3 / 2];
        // Notepad needs a moment to paint and WGC a moment to deliver: wait
        // up to 4 s (240 frames) for a picture that is not black. Notepad
        // may be in dark mode, so "not black" means: plenty of pixels above
        // video black (Y 16) and a real spread of grey levels.
        let mut bright = 0;
        let mut mean = 0;
        let mut levels = 0;
        for _ in 0..240 {
            client.read_exact(&mut frame).unwrap();
            let y = &frame[..1280 * 720];
            bright = y.iter().filter(|&&v| v > 40).count();
            mean = y.iter().map(|&v| v as u64).sum::<u64>() / y.len() as u64;
            let mut seen = [false; 256];
            for &v in y {
                seen[v as usize] = true;
            }
            levels = seen.iter().filter(|&&b| b).count();
            if bright > 1280 * 720 / 20 && levels >= 16 {
                break;
            }
        }
        println!(
            "status={} mean Y={mean} bright={bright} levels={levels}",
            session.status_json()
        );
        // Keep the Y plane as a PNG for a human look.
        let out = std::env::temp_dir().join("phonecast_notepad_y.png");
        if let Ok(f) = std::fs::File::create(&out) {
            let mut enc = png::Encoder::new(std::io::BufWriter::new(f), 1280, 720);
            enc.set_color(png::ColorType::Grayscale);
            enc.set_depth(png::BitDepth::Eight);
            if let Ok(mut w) = enc.write_header() {
                w.write_image_data(&frame[..1280 * 720]).ok();
            }
            println!("saved {}", out.display());
        }
        np.kill().ok();
        np.wait().ok();
        assert!(
            bright > 1280 * 720 / 20 && levels >= 16,
            "frame is black or flat (mean Y {mean}, {levels} grey levels)"
        );
    }

    /// CPU cost of PC capture, as Task Manager shows it (percent of the
    /// whole CPU, all cores). Measures our process, every FFmpeg process
    /// (encoder and webcam reader) and the whole machine against an idle
    /// baseline, over 10 s, encoding with NVENC to a file.
    ///
    /// `PHONECAST_CPU=screen|window|full PHONECAST_CPU_SIZE=1440|1080
    ///  cargo test --release cpu_capture -- --ignored --nocapture`
    /// - screen: ddagrab at the monitor's own size (zero-copy path)
    /// - window: Notepad through the compositor, no overlays
    /// - full:   Notepad + webcam + PNG + text through the compositor
    #[test]
    #[ignore]
    fn cpu_capture() {
        let mode = std::env::var("PHONECAST_CPU").unwrap_or_else(|_| "window".into());
        let size: u32 = std::env::var("PHONECAST_CPU_SIZE")
            .ok()
            .and_then(|s| s.parse().ok())
            .unwrap_or(1440);
        let (cw, ch) = if size >= 1440 {
            (2560, 1440)
        } else {
            (1920, 1080)
        };
        let ffmpeg = crate::live::find_ffmpeg().expect("ffmpeg");
        let cores = std::thread::available_parallelism()
            .map(|n| n.get())
            .unwrap_or(1) as f64;

        // Idle baseline of the whole machine.
        let idle = system_busy(Duration::from_secs(4));

        let mut np = None;
        let mut session = None;
        let mut pipe = None;
        let mut cmd = Command::new(&ffmpeg);
        cmd.args(["-hide_banner", "-loglevel", "error", "-y"]);
        let (w, h) = if mode == "screen" {
            let m = crate::capture::monitors();
            let m = m.first().expect("monitor");
            let v = crate::capture::PcVideo {
                input: crate::capture::PcInput::Ddagrab { output: m.output },
                webcam: None,
                screen_w: m.width,
                screen_h: m.height,
            };
            cmd.args(v.inputs());
            cmd.args(v.filter(m.width, m.height, true));
            (m.width, m.height)
        } else {
            let child = Command::new("notepad.exe").spawn().expect("notepad");
            np = Some(child);
            let mut hwnd = 0isize;
            for _ in 0..100 {
                std::thread::sleep(Duration::from_millis(100));
                if let Some(w) = crate::capture::windows()
                    .into_iter()
                    .find(|w| w.process.eq_ignore_ascii_case("notepad.exe"))
                {
                    hwnd = w.hwnd;
                    break;
                }
            }
            assert_ne!(hwnd, 0, "Notepad window not found");
            let mut layers = Layers::default();
            if mode == "full" {
                let cam = crate::capture::webcams(&ffmpeg)
                    .into_iter()
                    .next()
                    .expect("a webcam");
                layers.webcam = Some(Webcam {
                    enabled: true,
                    device: cam,
                    size: 0.25,
                    corner: "br".into(),
                    mirror: false,
                    x: None,
                    y: None,
                });
                let logo =
                    std::path::Path::new(env!("CARGO_MANIFEST_DIR")).join("icons/128x128.png");
                layers.overlays = vec![
                    Overlay {
                        id: "logo".into(),
                        kind: OverlayKind::Image {
                            path: logo.display().to_string(),
                        },
                        x: 0.85,
                        y: 0.04,
                        w: 0.1,
                        opacity: 1.0,
                    },
                    Overlay {
                        id: "t".into(),
                        kind: OverlayKind::Text {
                            text: "PhoneCast".into(),
                            size: 0.06,
                            color: "#ffffff".into(),
                        },
                        x: 0.04,
                        y: 0.85,
                        w: 0.0,
                        opacity: 1.0,
                    },
                ];
            }
            let s = Session::start(
                Base::Window { hwnd },
                (cw, ch),
                layers,
                Some(ffmpeg.clone()),
            )
            .expect("session");
            let p = s.new_pipe();
            cmd.args(crate::capture::pipe_input_args(&p.path, cw, ch));
            session = Some(s);
            pipe = Some(p);
            (cw, ch)
        };
        let out = std::env::temp_dir().join("phonecast_cpu_test.mkv");
        cmd.args([
            "-c:v",
            "h264_nvenc",
            "-preset",
            "p5",
            "-rc",
            "cbr",
            "-b:v",
            "20M",
            "-g",
            "120",
            "-f",
            "matroska",
        ]);
        cmd.arg(&out);
        cmd.stdin(Stdio::null());
        cmd.stderr(Stdio::piped());
        let mut child = cmd.spawn().expect("ffmpeg");
        std::thread::sleep(Duration::from_secs(3)); // warm up (webcam opens)
        if let Ok(Some(st)) = child.try_wait() {
            let mut err = String::new();
            child.stderr.take().map(|mut e| e.read_to_string(&mut err));
            panic!("ffmpeg exited ({st}) before measuring: {err}");
        }

        let t0 = Instant::now();
        let me0 = cpu_seconds(None);
        let ff0 = ffmpeg_cpu_seconds();
        let busy = system_busy(Duration::from_secs(10));
        let wall = t0.elapsed().as_secs_f64();
        let me = (cpu_seconds(None) - me0) / wall / cores * 100.0;
        let ff = (ffmpeg_cpu_seconds() - ff0) / wall / cores * 100.0;
        if let Some(s) = &session {
            println!("compositor {}", s.status_json());
        }
        println!(
            "RESULT mode={mode} size={w}x{h} PhoneCast={me:.1}% FFmpeg={ff:.1}% \
             together={:.1}% machine={busy:.1}% (idle baseline {idle:.1}%) of {cores} threads",
            me + ff
        );
        child.kill().ok();
        child.wait().ok();
        drop(pipe);
        drop(session);
        if let Some(mut n) = np {
            n.kill().ok();
            n.wait().ok();
        }
    }

    /// Whole-machine CPU busy percentage over `d` (GetSystemTimes).
    fn system_busy(d: Duration) -> f64 {
        use windows_sys::Win32::Foundation::FILETIME;
        use windows_sys::Win32::System::Threading::GetSystemTimes;
        let ft = |f: &FILETIME| ((f.dwHighDateTime as u64) << 32 | f.dwLowDateTime as u64) as f64;
        let snap = || {
            let mut t: [FILETIME; 3] = unsafe { std::mem::zeroed() };
            unsafe { GetSystemTimes(&mut t[0], &mut t[1], &mut t[2]) };
            (ft(&t[0]), ft(&t[1]) + ft(&t[2])) // idle, kernel (incl. idle) + user
        };
        let (i0, a0) = snap();
        std::thread::sleep(d);
        let (i1, a1) = snap();
        let all = (a1 - a0).max(1.0);
        (1.0 - (i1 - i0) / all) * 100.0
    }

    /// Total CPU seconds of all running ffmpeg.exe processes.
    fn ffmpeg_cpu_seconds() -> f64 {
        use windows_sys::Win32::Foundation::{CloseHandle, FILETIME, INVALID_HANDLE_VALUE};
        use windows_sys::Win32::System::Diagnostics::ToolHelp::{
            CreateToolhelp32Snapshot, Process32FirstW, Process32NextW, PROCESSENTRY32W,
            TH32CS_SNAPPROCESS,
        };
        use windows_sys::Win32::System::Threading::{
            GetProcessTimes, OpenProcess, PROCESS_QUERY_LIMITED_INFORMATION,
        };
        let ft =
            |f: &FILETIME| ((f.dwHighDateTime as u64) << 32 | f.dwLowDateTime as u64) as f64 / 1e7;
        let mut total = 0.0;
        unsafe {
            let snap = CreateToolhelp32Snapshot(TH32CS_SNAPPROCESS, 0);
            if snap == INVALID_HANDLE_VALUE {
                return 0.0;
            }
            let mut e: PROCESSENTRY32W = std::mem::zeroed();
            e.dwSize = std::mem::size_of::<PROCESSENTRY32W>() as u32;
            let mut ok = Process32FirstW(snap, &mut e) != 0;
            while ok {
                let len = e.szExeFile.iter().position(|&c| c == 0).unwrap_or(0);
                let name = String::from_utf16_lossy(&e.szExeFile[..len]);
                if name.eq_ignore_ascii_case("ffmpeg.exe") {
                    let h = OpenProcess(PROCESS_QUERY_LIMITED_INFORMATION, 0, e.th32ProcessID);
                    if !h.is_null() {
                        let mut t: [FILETIME; 4] = std::mem::zeroed();
                        if GetProcessTimes(h, &mut t[0], &mut t[1], &mut t[2], &mut t[3]) != 0 {
                            total += ft(&t[2]) + ft(&t[3]);
                        }
                        CloseHandle(h);
                    }
                }
                ok = Process32NextW(snap, &mut e) != 0;
            }
            CloseHandle(snap);
        }
        total
    }

    #[cfg(test)]
    fn cpu_seconds(child: Option<&Child>) -> f64 {
        use std::os::windows::io::AsRawHandle;
        use windows_sys::Win32::Foundation::FILETIME;
        use windows_sys::Win32::System::Threading::{GetCurrentProcess, GetProcessTimes};
        let h = match child {
            Some(c) => c.as_raw_handle() as isize,
            None => (unsafe { GetCurrentProcess() }) as isize,
        };
        let mut t: [FILETIME; 4] = unsafe { std::mem::zeroed() };
        unsafe {
            GetProcessTimes(h as _, &mut t[0], &mut t[1], &mut t[2], &mut t[3]);
        }
        let ft =
            |f: &FILETIME| ((f.dwHighDateTime as u64) << 32 | f.dwLowDateTime as u64) as f64 / 1e7;
        ft(&t[2]) + ft(&t[3])
    }
}
