//! PC sources: screen (monitor), single window and webcam.
//!
//! Plain monitor capture uses FFmpeg's `ddagrab` (Desktop Duplication):
//! frames stay on the GPU and go straight into NVENC when the output size
//! equals the screen size, with the webcam composited by FFmpeg.
//!
//! Window capture, and any source with image/text overlays, goes through the
//! Direct3D compositor in `compositor.rs` (Windows Graphics Capture), which
//! feeds FFmpeg composed NV12 frames over a named pipe.

use crate::compositor::{self, FramePipe, Session};
use crate::mkv::{split_nals, VideoConfig, CODEC_H264};
use serde::{Deserialize, Serialize};
use serde_json::{json, Value};
use std::io::Read;
use std::path::PathBuf;
use std::process::{Command, Stdio};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::Arc;
use std::time::Duration;

#[cfg(windows)]
use std::os::windows::process::CommandExt;

#[derive(Clone, Serialize, Deserialize, PartialEq, Debug)]
#[serde(tag = "kind", rename_all = "lowercase")]
#[derive(Default)]
pub enum Source {
    #[default]
    Phone,
    /// A monitor, by ddagrab output index (DXGI order on the main adapter)
    /// plus its stable hardware ID, so the choice survives replugging and
    /// Windows updates that renumber displays.
    Screen {
        output: usize,
        #[serde(default)]
        id: String,
    },
    /// One top-level window (game, emulator, browser…). The handle changes
    /// between runs, so the process name and title are kept to find it again.
    Window {
        hwnd: isize,
        #[serde(default)]
        title: String,
        #[serde(default)]
        process: String,
    },
}

#[derive(Clone, Serialize, Deserialize, PartialEq, Debug)]
pub struct Webcam {
    pub enabled: bool,
    /// DirectShow device name, e.g. "Integrated Camera".
    pub device: String,
    /// Width of the picture-in-picture as a fraction of the canvas width.
    pub size: f32,
    /// "tl" | "tr" | "bl" | "br" | "free" (dragged: see `x`, `y`)
    pub corner: String,
    pub mirror: bool,
    /// Top-left position as fractions of the canvas, when `corner` is "free".
    #[serde(default)]
    pub x: Option<f32>,
    #[serde(default)]
    pub y: Option<f32>,
}

impl Default for Webcam {
    fn default() -> Self {
        Self {
            enabled: false,
            device: String::new(),
            size: 0.25,
            corner: "br".into(),
            mirror: false,
            x: None,
            y: None,
        }
    }
}

/// Something drawn on top of the picture: a PNG or a line of text.
#[derive(Clone, Serialize, Deserialize, PartialEq, Debug)]
pub struct Overlay {
    pub id: String,
    #[serde(flatten)]
    pub kind: OverlayKind,
    /// Top-left position as fractions of the canvas.
    pub x: f32,
    pub y: f32,
    /// Width as a fraction of the canvas width (images; text sizes by font).
    #[serde(default = "default_overlay_w")]
    pub w: f32,
    #[serde(default = "default_opacity")]
    pub opacity: f32,
}

fn default_overlay_w() -> f32 {
    0.2
}
fn default_opacity() -> f32 {
    1.0
}

#[derive(Clone, Serialize, Deserialize, PartialEq, Debug)]
#[serde(tag = "kind", rename_all = "lowercase")]
pub enum OverlayKind {
    Image {
        path: String,
    },
    Text {
        text: String,
        /// Font size as a fraction of the canvas height.
        size: f32,
        /// "#rrggbb"
        color: String,
    },
}

#[derive(Clone, Serialize)]
pub struct Monitor {
    pub output: usize,
    /// Stable ID from the monitor hardware (manufacturer + model), e.g.
    /// "MONITOR\GSM5B09"; falls back to the GDI device name.
    pub id: String,
    pub name: String,
    /// GDI device name, `\\.\DISPLAY1`.
    pub device: String,
    pub width: u32,
    pub height: u32,
    pub primary: bool,
}

/// A capturable top-level window.
#[derive(Clone, Serialize, Debug)]
pub struct WindowInfo {
    pub hwnd: isize,
    pub title: String,
    /// Executable name, e.g. "HD-Player.exe".
    pub process: String,
}

/// Monitors in DXGI output order of the main adapter (what ddagrab uses).
/// GDI's \\.\DISPLAYn order matches that on single-GPU systems.
#[cfg(windows)]
pub fn monitors() -> Vec<Monitor> {
    use windows_sys::Win32::Foundation::{BOOL, LPARAM, RECT};
    use windows_sys::Win32::Graphics::Gdi::{
        EnumDisplayMonitors, GetMonitorInfoW, HDC, HMONITOR, MONITORINFOEXW,
    };
    const MONITORINFOF_PRIMARY: u32 = 1;
    unsafe extern "system" fn cb(h: HMONITOR, _: HDC, _: *mut RECT, data: LPARAM) -> BOOL {
        let list = unsafe { &mut *(data as *mut Vec<Monitor>) };
        let mut info: MONITORINFOEXW = unsafe { std::mem::zeroed() };
        info.monitorInfo.cbSize = std::mem::size_of::<MONITORINFOEXW>() as u32;
        if unsafe { GetMonitorInfoW(h, &mut info as *mut _ as *mut _) } != 0 {
            let r = info.monitorInfo.rcMonitor;
            let len = info
                .szDevice
                .iter()
                .position(|&c| c == 0)
                .unwrap_or(info.szDevice.len());
            let dev = String::from_utf16_lossy(&info.szDevice[..len]);
            let id = unsafe { monitor_id(&info.szDevice) }.unwrap_or_else(|| dev.clone());
            list.push(Monitor {
                output: 0,
                id,
                name: String::new(),
                device: dev,
                width: (r.right - r.left) as u32,
                height: (r.bottom - r.top) as u32,
                primary: info.monitorInfo.dwFlags & MONITORINFOF_PRIMARY != 0,
            });
        }
        1
    }
    let mut list: Vec<Monitor> = Vec::new();
    unsafe {
        EnumDisplayMonitors(
            std::ptr::null_mut(),
            std::ptr::null(),
            Some(cb),
            &mut list as *mut _ as LPARAM,
        );
    }
    list.sort_by(|a, b| a.device.cmp(&b.device));
    // Two identical monitors share an ID; number the second one.
    let mut seen: Vec<String> = Vec::new();
    for m in list.iter_mut() {
        let n = seen.iter().filter(|s| **s == m.id).count();
        seen.push(m.id.clone());
        if n > 0 {
            m.id = format!("{}#{}", m.id, n + 1);
        }
    }
    list.into_iter()
        .enumerate()
        .map(|(i, mut m)| {
            m.output = i;
            m.name = if m.primary {
                format!("Monitor {} (main) · {}×{}", i + 1, m.width, m.height)
            } else {
                format!("Monitor {} · {}×{}", i + 1, m.width, m.height)
            };
            m
        })
        .collect()
}

/// The monitor's hardware ID for a GDI display device ("\\.\DISPLAY1").
///
/// # Safety
/// `device` must be a NUL-terminated UTF-16 string.
#[cfg(windows)]
unsafe fn monitor_id(device: &[u16]) -> Option<String> {
    use windows_sys::Win32::Graphics::Gdi::{EnumDisplayDevicesW, DISPLAY_DEVICEW};
    let mut dd: DISPLAY_DEVICEW = unsafe { std::mem::zeroed() };
    dd.cb = std::mem::size_of::<DISPLAY_DEVICEW>() as u32;
    if unsafe { EnumDisplayDevicesW(device.as_ptr(), 0, &mut dd, 0) } == 0 {
        return None;
    }
    let len = dd
        .DeviceID
        .iter()
        .position(|&c| c == 0)
        .unwrap_or(dd.DeviceID.len());
    let full = String::from_utf16_lossy(&dd.DeviceID[..len]);
    // "MONITOR\GSM5B09\{GUID}\0001": keep the hardware part only.
    let stable: String = full.split('\\').take(2).collect::<Vec<_>>().join("\\");
    (!stable.is_empty()).then_some(stable)
}

#[cfg(not(windows))]
pub fn monitors() -> Vec<Monitor> {
    Vec::new()
}

/// Visible top-level windows with a title, most recent first (Z order),
/// excluding PhoneCast itself, tool windows and cloaked (hidden UWP) ones.
#[cfg(windows)]
pub fn windows() -> Vec<WindowInfo> {
    use windows_sys::Win32::Foundation::{BOOL, HWND, LPARAM};
    use windows_sys::Win32::Graphics::Dwm::{DwmGetWindowAttribute, DWMWA_CLOAKED};
    use windows_sys::Win32::UI::WindowsAndMessaging::{
        EnumWindows, GetWindowLongPtrW, GetWindowTextW, GetWindowThreadProcessId, IsIconic,
        IsWindowVisible, GWL_EXSTYLE, WS_EX_TOOLWINDOW,
    };
    unsafe extern "system" fn cb(h: HWND, data: LPARAM) -> BOOL {
        let list = unsafe { &mut *(data as *mut Vec<WindowInfo>) };
        unsafe {
            if IsWindowVisible(h) == 0 {
                return 1;
            }
            if GetWindowLongPtrW(h, GWL_EXSTYLE) as u32 & WS_EX_TOOLWINDOW != 0 {
                return 1;
            }
            let mut cloaked: u32 = 0;
            DwmGetWindowAttribute(h, DWMWA_CLOAKED as u32, &mut cloaked as *mut _ as *mut _, 4);
            if cloaked != 0 {
                return 1;
            }
            let mut buf = [0u16; 512];
            let n = GetWindowTextW(h, buf.as_mut_ptr(), buf.len() as i32);
            if n <= 0 {
                return 1;
            }
            let title = String::from_utf16_lossy(&buf[..n as usize]);
            let mut pid = 0u32;
            GetWindowThreadProcessId(h, &mut pid);
            if pid == 0 || pid == std::process::id() {
                return 1;
            }
            let process = process_name(pid);
            let _ = IsIconic(h); // minimised windows are listed too: WGC can capture them once restored
            list.push(WindowInfo {
                hwnd: h as isize,
                title,
                process,
            });
        }
        1
    }
    let mut list: Vec<WindowInfo> = Vec::new();
    unsafe {
        EnumWindows(Some(cb), &mut list as *mut _ as LPARAM);
    }
    list.retain(|w| {
        !matches!(
            w.process.to_ascii_lowercase().as_str(),
            "textinputhost.exe" | "applicationframehost.exe" | "shellexperiencehost.exe"
        ) && w.title != "Program Manager"
            && w.title != "Settings"
    });
    list
}

#[cfg(not(windows))]
pub fn windows() -> Vec<WindowInfo> {
    Vec::new()
}

/// Executable name of a process ("game.exe"), or "" when it cannot be read.
#[cfg(windows)]
pub fn process_name(pid: u32) -> String {
    use windows_sys::Win32::System::Threading::{
        OpenProcess, QueryFullProcessImageNameW, PROCESS_QUERY_LIMITED_INFORMATION,
    };
    let mut process = String::new();
    unsafe {
        let ph = OpenProcess(PROCESS_QUERY_LIMITED_INFORMATION, 0, pid);
        if !ph.is_null() {
            let mut path = [0u16; 1024];
            let mut len = path.len() as u32;
            if QueryFullProcessImageNameW(ph, 0, path.as_mut_ptr(), &mut len) != 0 {
                let full = String::from_utf16_lossy(&path[..len as usize]);
                process = full.rsplit(['\\', '/']).next().unwrap_or("").to_string();
            }
            windows_sys::Win32::Foundation::CloseHandle(ph);
        }
    }
    process
}

#[cfg(not(windows))]
pub fn process_name(_pid: u32) -> String {
    String::new()
}

/// Finds the saved window again after a restart: by handle if it is still
/// the same window, else the first window of the same process (preferring a
/// matching title).
pub fn resolve_window(hwnd: isize, title: &str, process: &str) -> Option<WindowInfo> {
    let list = windows();
    if let Some(w) = list.iter().find(|w| w.hwnd == hwnd) {
        return Some(w.clone());
    }
    if process.is_empty() {
        return None;
    }
    let same: Vec<&WindowInfo> = list
        .iter()
        .filter(|w| w.process.eq_ignore_ascii_case(process))
        .collect();
    same.iter()
        .find(|w| w.title == title)
        .or(same.first())
        .map(|w| (*w).clone())
}

/// Native "Open" dialog for a PNG. Blocks; call from a worker thread.
#[cfg(windows)]
pub fn pick_png() -> Option<String> {
    use windows_sys::Win32::UI::Controls::Dialogs::{
        GetOpenFileNameW, OFN_FILEMUSTEXIST, OFN_NOCHANGEDIR, OPENFILENAMEW,
    };
    let filter: Vec<u16> = "PNG images (*.png)\0*.png\0All files\0*.*\0\0"
        .encode_utf16()
        .collect();
    let title: Vec<u16> = "Choose an image for the overlay\0".encode_utf16().collect();
    let mut file = vec![0u16; 2048];
    let mut ofn: OPENFILENAMEW = unsafe { std::mem::zeroed() };
    ofn.lStructSize = std::mem::size_of::<OPENFILENAMEW>() as u32;
    ofn.lpstrFilter = filter.as_ptr();
    ofn.lpstrFile = file.as_mut_ptr();
    ofn.nMaxFile = file.len() as u32;
    ofn.lpstrTitle = title.as_ptr();
    ofn.Flags = OFN_FILEMUSTEXIST | OFN_NOCHANGEDIR;
    if unsafe { GetOpenFileNameW(&mut ofn) } == 0 {
        return None;
    }
    let len = file.iter().position(|&c| c == 0).unwrap_or(0);
    Some(String::from_utf16_lossy(&file[..len]))
}

#[cfg(not(windows))]
pub fn pick_png() -> Option<String> {
    None
}

fn hide(cmd: &mut Command) {
    #[cfg(windows)]
    cmd.creation_flags(0x0800_0000);
}

/// DirectShow video devices, from `ffmpeg -list_devices`.
pub fn webcams(ffmpeg: &PathBuf) -> Vec<String> {
    let mut cmd = Command::new(ffmpeg);
    cmd.args([
        "-hide_banner",
        "-list_devices",
        "true",
        "-f",
        "dshow",
        "-i",
        "dummy",
    ]);
    cmd.stdin(Stdio::null()).stdout(Stdio::null());
    hide(&mut cmd);
    let Ok(out) = cmd.output() else {
        return Vec::new();
    };
    let text = String::from_utf8_lossy(&out.stderr);
    text.lines()
        .filter(|l| l.trim_end().ends_with("(video)"))
        .filter_map(|l| {
            let a = l.find('"')? + 1;
            let b = l[a..].find('"')? + a;
            Some(l[a..b].to_string())
        })
        .filter(|n| !n.contains("Virtual Camera")) // avoid feedback loops
        .collect()
}

/// Where FFmpeg gets the PC picture from.
#[derive(Clone)]
pub enum PcInput {
    /// FFmpeg's ddagrab captures the monitor itself.
    Ddagrab { output: usize },
    /// Composed NV12 frames from the compositor over a named pipe (one pipe
    /// per FFmpeg process; see `PcVideo::for_job`).
    Composited {
        session: Arc<Session>,
        pipe: Arc<FramePipe>,
    },
}

/// Shared FFmpeg input + filter pieces for a PC source, used by live
/// streaming, recording and the preview.
#[derive(Clone)]
pub struct PcVideo {
    pub input: PcInput,
    /// Webcam for FFmpeg to composite. Only used with `Ddagrab`; the
    /// compositor draws the webcam itself.
    pub webcam: Option<Webcam>,
    /// Native size of the source: the monitor, or the compositor canvas.
    pub screen_w: u32,
    pub screen_h: u32,
}

/// FFmpeg input arguments for a composited NV12 pipe at `w`×`h`, 60 fps.
pub fn pipe_input_args(path: &str, w: u32, h: u32) -> Vec<String> {
    [
        "-thread_queue_size",
        "64",
        "-f",
        "rawvideo",
        "-pixel_format",
        "nv12",
        "-video_size",
        &format!("{w}x{h}"),
        "-framerate",
        "60",
        "-i",
        path,
    ]
    .iter()
    .map(|s| s.to_string())
    .collect()
}

impl PcVideo {
    /// A copy for one FFmpeg process: composited sources get their own pipe.
    pub fn for_job(&self) -> PcVideo {
        let mut v = self.clone();
        if let PcInput::Composited { session, .. } = &self.input {
            v.input = PcInput::Composited {
                session: session.clone(),
                pipe: session.new_pipe(),
            };
        }
        v
    }

    /// Ends the video input for this job (composited pipe only), so FFmpeg
    /// can finish cleanly on stop. Plain ddagrab capture has no such end.
    pub fn close_input(&self) {
        if let PcInput::Composited { pipe, .. } = &self.input {
            pipe.close();
        }
    }

    pub fn is_composited(&self) -> bool {
        matches!(self.input, PcInput::Composited { .. })
    }

    /// The webcam FFmpeg itself has to composite (ddagrab path only).
    fn cam(&self) -> Option<&Webcam> {
        match self.input {
            PcInput::Ddagrab { .. } => self
                .webcam
                .as_ref()
                .filter(|c| c.enabled && !c.device.is_empty()),
            PcInput::Composited { .. } => None,
        }
    }

    /// Input arguments (video only; audio is added by the caller).
    pub fn inputs(&self) -> Vec<String> {
        match &self.input {
            PcInput::Composited { pipe, .. } => {
                pipe_input_args(&pipe.path, self.screen_w, self.screen_h)
            }
            PcInput::Ddagrab { output } => {
                let mut a: Vec<String> = [
                    // Deep queue so a busy moment never stalls the capture.
                    "-thread_queue_size",
                    "1024",
                    "-init_hw_device",
                    "d3d11va=d3d:0",
                    "-filter_hw_device",
                    "d3d",
                    "-f",
                    "lavfi",
                    "-i",
                ]
                .iter()
                .map(|s| s.to_string())
                .collect();
                a.push(format!(
                    "ddagrab=output_idx={output}:framerate=60:draw_mouse=1"
                ));
                if let Some(cam) = self.cam() {
                    a.extend(
                        [
                            "-f",
                            "dshow",
                            "-rtbufsize",
                            "64M",
                            "-pixel_format",
                            "nv12",
                            "-video_size",
                            "1280x720",
                            "-framerate",
                            "30",
                            "-i",
                        ]
                        .iter()
                        .map(|s| s.to_string()),
                    );
                    a.push(format!("video={}", cam.device));
                }
                a
            }
        }
    }

    /// Index of the FFmpeg input that carries the audio (added after `inputs()`).
    pub fn audio_index(&self) -> usize {
        if self.cam().is_some() {
            2
        } else {
            1
        }
    }

    /// Video filter chain producing `[v]` at `w`×`h`, plus the `-map` args.
    /// Zero-copy when nothing needs scaling or compositing. `nvidia` picks
    /// CUDA scaling for the pipe input (NV12 upload + GPU resize).
    pub fn filter(&self, w: u32, h: u32, nvidia: bool) -> Vec<String> {
        let native = w == self.screen_w && h == self.screen_h;
        if let PcInput::Composited { .. } = self.input {
            if native {
                return vec!["-map".into(), "0:v".into()];
            }
            if nvidia {
                return vec![
                    "-init_hw_device".into(),
                    "cuda=cu".into(),
                    "-filter_hw_device".into(),
                    "cu".into(),
                    "-filter_complex".into(),
                    format!(
                        "[0:v]hwupload_cuda,scale_cuda={w}:{h}:interp_algo=lanczos:format=nv12[v]"
                    ),
                    "-map".into(),
                    "[v]".into(),
                ];
            }
            return vec![
                "-filter_complex".into(),
                format!("[0:v]scale={w}:{h}:flags=bicubic[v]"),
                "-map".into(),
                "[v]".into(),
            ];
        }
        let cam = self.cam();
        if cam.is_none() && native {
            return vec!["-map".into(), "0:v".into()];
        }
        let mut chain = String::from("[0:v]hwdownload,format=bgra");
        if !native {
            chain.push_str(&format!(",scale={w}:{h}:force_original_aspect_ratio=decrease:flags=bicubic,pad={w}:{h}:(ow-iw)/2:(oh-ih)/2,setsar=1"));
        }
        if let Some(c) = cam {
            let cw = ((w as f32 * c.size.clamp(0.1, 0.5)) as u32) / 2 * 2;
            let margin = w / 60;
            let (x, y) = match (c.corner.as_str(), c.x, c.y) {
                ("free", Some(fx), Some(fy)) => (
                    format!("min(W-w,{})", (fx * w as f32).round() as u32),
                    format!("min(H-h,{})", (fy * h as f32).round() as u32),
                ),
                ("tl", ..) => (margin.to_string(), margin.to_string()),
                ("tr", ..) => (format!("W-w-{margin}"), margin.to_string()),
                ("bl", ..) => (margin.to_string(), format!("H-h-{margin}")),
                _ => (format!("W-w-{margin}"), format!("H-h-{margin}")),
            };
            let flip = if c.mirror { ",hflip" } else { "" };
            chain.push_str(&format!(
                "[s];[1:v]scale={cw}:-2:flags=bicubic{flip}[c];[s][c]overlay={x}:{y}"
            ));
        }
        chain.push_str(",format=nv12[v]");
        vec!["-filter_complex".into(), chain, "-map".into(), "[v]".into()]
    }

    /// Colour tags for the encoder (the compositor produces BT.709 limited).
    pub fn color_args(&self) -> Vec<String> {
        if !self.is_composited() {
            return Vec::new();
        }
        [
            "-color_range",
            "tv",
            "-colorspace",
            "bt709",
            "-color_primaries",
            "bt709",
            "-color_trc",
            "bt709",
        ]
        .iter()
        .map(|s| s.to_string())
        .collect()
    }
}

// ---------------- preview of the PC screen ----------------

/// Encodes the screen at 720p and hands Annex-B access units to the preview.
pub struct ScreenPreview {
    stop: Arc<AtomicBool>,
}

impl ScreenPreview {
    /// `epoch`: the clock frame timestamps are taken from, shared with the
    /// audio mixer so the replay buffer can line the two up.
    pub fn start(
        ffmpeg: PathBuf,
        video: PcVideo,
        epoch: std::time::Instant,
        on_config: Arc<dyn Fn(VideoConfig) + Send + Sync>,
        on_frame: Arc<dyn Fn(bool, i64, Vec<u8>) + Send + Sync>,
    ) -> Self {
        let stop = Arc::new(AtomicBool::new(false));
        let st = stop.clone();
        std::thread::Builder::new()
            .name("screen-preview".into())
            .spawn(move || {
                while !st.load(Ordering::SeqCst) {
                    if let Err(e) = Self::run(&ffmpeg, &video, epoch, &on_config, &on_frame, &st) {
                        crate::diag::log("preview", format!("screen preview: {e}"));
                    }
                    if !st.load(Ordering::SeqCst) {
                        std::thread::sleep(Duration::from_secs(2));
                    }
                }
            })
            .ok();
        Self { stop }
    }

    fn run(
        ffmpeg: &PathBuf,
        video: &PcVideo,
        epoch: std::time::Instant,
        on_config: &Arc<dyn Fn(VideoConfig) + Send + Sync>,
        on_frame: &Arc<dyn Fn(bool, i64, Vec<u8>) + Send + Sync>,
        stop: &AtomicBool,
    ) -> std::io::Result<()> {
        let (w, h) = (1280, 720);
        let mut cmd = Command::new(ffmpeg);
        cmd.args(["-hide_banner", "-loglevel", "error", "-nostats"]);
        cmd.args(video.inputs());
        cmd.args(video.filter(w, h, true));
        cmd.args([
            "-an",
            "-c:v",
            "h264_nvenc",
            "-preset",
            "p1",
            "-tune",
            "ull",
            "-rc",
            "cbr",
            "-b:v",
            "5M",
            "-g",
            "60",
            "-bf",
            "0",
            "-aud",
            "1",
            "-f",
            "h264",
            "pipe:1",
        ]);
        cmd.stdin(Stdio::null())
            .stdout(Stdio::piped())
            .stderr(Stdio::null());
        hide(&mut cmd);
        let mut child = cmd.spawn()?;
        let mut out = child.stdout.take().unwrap();
        let mut buf = Vec::with_capacity(1 << 20);
        let mut chunk = vec![0u8; 64 * 1024];
        let mut csd: Option<Vec<u8>> = None;
        // The raw H.264 pipe carries no timestamps and frames arrive in
        // bursts, so put them on a steady 60 fps grid anchored to the clock
        // (re-anchored after a pause), the way the recorder expects.
        let mut next_pts: Option<i64> = None;
        loop {
            if stop.load(Ordering::SeqCst) {
                break;
            }
            let n = out.read(&mut chunk)?;
            if n == 0 {
                break;
            }
            buf.extend_from_slice(&chunk[..n]);
            // Emit each access unit (AUD-delimited) once the next AUD has arrived.
            while let Some(second) = find_aud(&buf, 1) {
                let au: Vec<u8> = buf.drain(..second).collect();
                let mut key = false;
                let mut ps = Vec::new();
                let mut frame = Vec::new();
                for nal in split_nals(&au) {
                    match nal[0] & 0x1F {
                        9 => {}
                        7 | 8 => {
                            ps.extend_from_slice(&[0, 0, 0, 1]);
                            ps.extend_from_slice(nal);
                        }
                        t => {
                            key |= t == 5;
                            frame.extend_from_slice(&[0, 0, 0, 1]);
                            frame.extend_from_slice(nal);
                        }
                    }
                }
                if !ps.is_empty() && csd.as_ref() != Some(&ps) {
                    csd = Some(ps.clone());
                    on_config(VideoConfig {
                        codec: CODEC_H264,
                        width: w as u16,
                        height: h as u16,
                        csd: ps,
                    });
                }
                if !frame.is_empty() && csd.is_some() {
                    let now = epoch.elapsed().as_micros() as i64;
                    let pts = match next_pts {
                        Some(p) if (now - p).abs() < 200_000 => p,
                        _ => now,
                    };
                    next_pts = Some(pts + 16_667);
                    on_frame(key, pts, frame);
                }
            }
        }
        child.kill().ok();
        child.wait().ok();
        Ok(())
    }
}

impl Drop for ScreenPreview {
    fn drop(&mut self) {
        self.stop.store(true, Ordering::SeqCst);
    }
}

/// Byte offset of the `n`-th (0-based) access unit delimiter NAL.
fn find_aud(buf: &[u8], n: usize) -> Option<usize> {
    let mut seen = 0;
    let mut i = 0;
    while i + 4 < buf.len() {
        if buf[i] == 0 && buf[i + 1] == 0 && buf[i + 2] == 1 && buf[i + 3] & 0x1F == 9 {
            let start = if i > 0 && buf[i - 1] == 0 { i - 1 } else { i };
            if seen == n {
                return Some(start);
            }
            seen += 1;
            i += 3;
        } else {
            i += 1;
        }
    }
    None
}

pub fn sources_json(ffmpeg: Option<&PathBuf>) -> Value {
    json!({
        "monitors": monitors(),
        "windows": windows(),
        "webcams": ffmpeg.map(webcams).unwrap_or_default(),
    })
}

pub fn settings_path() -> PathBuf {
    let base = std::env::var("APPDATA")
        .map(PathBuf::from)
        .unwrap_or_else(|_| PathBuf::from("."));
    base.join("PhoneCast").join("source.json")
}

#[derive(Clone, Serialize, Deserialize, Default)]
pub struct SourceSettings {
    pub source: Source,
    pub webcam: Webcam,
    #[serde(default)]
    pub overlays: Vec<Overlay>,
}

impl SourceSettings {
    pub fn load() -> Self {
        std::fs::read(settings_path())
            .ok()
            .and_then(|b| serde_json::from_slice(&b).ok())
            .unwrap_or_default()
    }
    pub fn save(&self) {
        let p = settings_path();
        if let Some(d) = p.parent() {
            std::fs::create_dir_all(d).ok();
        }
        if let Ok(b) = serde_json::to_vec_pretty(self) {
            std::fs::write(p, b).ok();
        }
    }

    /// True when the picture has to go through the compositor: a window
    /// source, or a screen with image/text overlays.
    pub fn needs_compositor(&self) -> bool {
        matches!(self.source, Source::Window { .. })
            || (matches!(self.source, Source::Screen { .. }) && !self.overlays.is_empty())
    }

    pub fn layers(&self) -> compositor::Layers {
        compositor::Layers {
            webcam: Some(self.webcam.clone()),
            overlays: self.overlays.clone(),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn ddagrab(webcam: Option<Webcam>) -> PcVideo {
        PcVideo {
            input: PcInput::Ddagrab { output: 0 },
            webcam,
            screen_w: 1920,
            screen_h: 1080,
        }
    }

    #[test]
    fn aud_split() {
        let b = [
            0, 0, 0, 1, 0x09, 0xF0, 0, 0, 1, 0x65, 1, 2, 0, 0, 0, 1, 0x09, 0xF0, 0, 0, 1, 0x41, 3,
        ];
        assert_eq!(find_aud(&b, 0), Some(0));
        assert_eq!(find_aud(&b, 1), Some(12));
        assert_eq!(find_aud(&b, 2), None);
    }

    #[test]
    fn zero_copy_when_native() {
        let v = ddagrab(None);
        assert_eq!(v.filter(1920, 1080, true), vec!["-map", "0:v"]);
        assert!(v.filter(2560, 1440, true)[1].contains("scale=2560:1440"));
        assert_eq!(v.audio_index(), 1);
        let cam = Webcam {
            enabled: true,
            device: "Cam".into(),
            ..Default::default()
        };
        let v = ddagrab(Some(cam));
        assert!(v.filter(1920, 1080, true)[1].contains("overlay=W-w-32:H-h-32"));
        assert_eq!(v.inputs().last().unwrap(), "video=Cam");
        assert_eq!(v.audio_index(), 2);
        assert!(v.color_args().is_empty());
    }

    #[test]
    fn dragged_webcam_position_in_ffmpeg_filter() {
        let cam = Webcam {
            enabled: true,
            device: "Cam".into(),
            corner: "free".into(),
            x: Some(0.5),
            y: Some(0.25),
            ..Default::default()
        };
        let f = ddagrab(Some(cam)).filter(1920, 1080, true);
        assert!(
            f[1].contains("overlay=min(W-w,960):min(H-h,270)"),
            "{}",
            f[1]
        );
    }

    #[test]
    fn pipe_input_arguments() {
        let a = pipe_input_args("\\\\.\\pipe\\x", 2560, 1440);
        assert_eq!(a[a.len() - 1], "\\\\.\\pipe\\x");
        assert!(a.contains(&"nv12".to_string()));
        assert!(a.contains(&"2560x1440".to_string()));
    }

    #[test]
    fn settings_round_trip_and_compositor_need() {
        let mut s = SourceSettings {
            source: Source::Window {
                hwnd: 42,
                title: "Game".into(),
                process: "game.exe".into(),
            },
            webcam: Webcam::default(),
            overlays: vec![Overlay {
                id: "logo".into(),
                kind: OverlayKind::Image {
                    path: "C:\\logo.png".into(),
                },
                x: 0.8,
                y: 0.05,
                w: 0.15,
                opacity: 0.9,
            }],
        };
        let json = serde_json::to_string(&s).unwrap();
        assert!(json.contains("\"kind\":\"window\""));
        assert!(json.contains("\"kind\":\"image\""));
        let back: SourceSettings = serde_json::from_str(&json).unwrap();
        assert_eq!(back.source, s.source);
        assert_eq!(back.overlays, s.overlays);
        assert!(s.needs_compositor());
        s.source = Source::Screen {
            output: 0,
            id: String::new(),
        };
        assert!(s.needs_compositor()); // overlays force it
        s.overlays.clear();
        assert!(!s.needs_compositor());
        // Old settings files (no overlays, no webcam x/y) still load.
        let old = r#"{"source":{"kind":"screen","output":1},"webcam":{"enabled":false,"device":"","size":0.25,"corner":"br","mirror":false}}"#;
        let s: SourceSettings = serde_json::from_str(old).unwrap();
        assert!(s.overlays.is_empty());
        assert_eq!(s.webcam.x, None);
    }
}
