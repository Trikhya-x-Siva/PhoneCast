//! PC sources: screen (monitor) capture and webcam, through FFmpeg.
//!
//! Screen capture uses FFmpeg's `ddagrab` (Desktop Duplication): frames stay
//! on the GPU and go straight into NVENC when the output size equals the
//! screen size. Other sizes take one GPU→CPU copy and a CPU scale.
//! The webcam is a DirectShow input composited as picture-in-picture.

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
pub enum Source {
    Phone,
    /// A monitor, by ddagrab output index (DXGI order on the main adapter).
    Screen {
        output: usize,
    },
}

impl Default for Source {
    fn default() -> Self {
        Source::Phone
    }
}

#[derive(Clone, Serialize, Deserialize, PartialEq, Debug)]
pub struct Webcam {
    pub enabled: bool,
    /// DirectShow device name, e.g. "Integrated Camera".
    pub device: String,
    /// Width of the picture-in-picture as a fraction of the canvas width.
    pub size: f32,
    /// "tl" | "tr" | "bl" | "br"
    pub corner: String,
    pub mirror: bool,
}

impl Default for Webcam {
    fn default() -> Self {
        Self {
            enabled: false,
            device: String::new(),
            size: 0.25,
            corner: "br".into(),
            mirror: false,
        }
    }
}

#[derive(Clone, Serialize)]
pub struct Monitor {
    pub output: usize,
    pub name: String,
    pub width: u32,
    pub height: u32,
    pub primary: bool,
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
        let list = unsafe { &mut *(data as *mut Vec<(String, Monitor)>) };
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
            list.push((
                dev.clone(),
                Monitor {
                    output: 0,
                    name: dev,
                    width: (r.right - r.left) as u32,
                    height: (r.bottom - r.top) as u32,
                    primary: info.monitorInfo.dwFlags & MONITORINFOF_PRIMARY != 0,
                },
            ));
        }
        1
    }
    let mut list: Vec<(String, Monitor)> = Vec::new();
    unsafe {
        EnumDisplayMonitors(
            std::ptr::null_mut(),
            std::ptr::null(),
            Some(cb),
            &mut list as *mut _ as LPARAM,
        );
    }
    list.sort_by(|a, b| a.0.cmp(&b.0));
    list.into_iter()
        .enumerate()
        .map(|(i, (_, mut m))| {
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

#[cfg(not(windows))]
pub fn monitors() -> Vec<Monitor> {
    Vec::new()
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

/// Shared FFmpeg input + filter pieces for a PC source, used by live
/// streaming, recording and the preview.
pub struct PcVideo {
    pub output: usize,
    pub webcam: Option<Webcam>,
    pub screen_w: u32,
    pub screen_h: u32,
}

impl PcVideo {
    /// Input arguments (video only; audio is added by the caller).
    pub fn inputs(&self) -> Vec<String> {
        let mut a: Vec<String> = [
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
            "ddagrab=output_idx={}:framerate=60:draw_mouse=1",
            self.output
        ));
        if let Some(cam) = self
            .webcam
            .as_ref()
            .filter(|c| c.enabled && !c.device.is_empty())
        {
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

    /// Video filter chain producing `[v]` at `w`×`h`, plus the `-map` args.
    /// Zero-copy when nothing needs scaling or compositing.
    pub fn filter(&self, w: u32, h: u32) -> Vec<String> {
        let cam = self
            .webcam
            .as_ref()
            .filter(|c| c.enabled && !c.device.is_empty());
        let native = w == self.screen_w && h == self.screen_h;
        if cam.is_none() && native {
            return vec!["-map".into(), "0:v".into()];
        }
        let mut chain = String::from("[0:v]hwdownload,format=bgra");
        if !native {
            chain.push_str(&format!(",scale={w}:{h}:force_original_aspect_ratio=decrease:flags=bicubic,pad={w}:{h}:(ow-iw)/2:(oh-ih)/2"));
        }
        if let Some(c) = cam {
            let cw = ((w as f32 * c.size.clamp(0.1, 0.5)) as u32) / 2 * 2;
            let margin = w / 60;
            let (x, y) = match c.corner.as_str() {
                "tl" => (margin.to_string(), margin.to_string()),
                "tr" => (format!("W-w-{margin}"), margin.to_string()),
                "bl" => (margin.to_string(), format!("H-h-{margin}")),
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
}

// ---------------- preview of the PC screen ----------------

/// Encodes the screen at 720p and hands Annex-B access units to the preview.
pub struct ScreenPreview {
    stop: Arc<AtomicBool>,
}

impl ScreenPreview {
    pub fn start(
        ffmpeg: PathBuf,
        video: PcVideo,
        on_config: Arc<dyn Fn(VideoConfig) + Send + Sync>,
        on_frame: Arc<dyn Fn(bool, i64, Vec<u8>) + Send + Sync>,
    ) -> Self {
        let stop = Arc::new(AtomicBool::new(false));
        let st = stop.clone();
        std::thread::Builder::new()
            .name("screen-preview".into())
            .spawn(move || {
                while !st.load(Ordering::SeqCst) {
                    if let Err(e) = Self::run(&ffmpeg, &video, &on_config, &on_frame, &st) {
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
        on_config: &Arc<dyn Fn(VideoConfig) + Send + Sync>,
        on_frame: &Arc<dyn Fn(bool, i64, Vec<u8>) + Send + Sync>,
        stop: &AtomicBool,
    ) -> std::io::Result<()> {
        let (w, h) = (1280, 720);
        let mut cmd = Command::new(ffmpeg);
        cmd.args(["-hide_banner", "-loglevel", "error", "-nostats"]);
        cmd.args(video.inputs());
        cmd.args(video.filter(w, h));
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
        let start = std::time::Instant::now();
        let mut frames = 0i64;
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
            loop {
                let Some(second) = find_aud(&buf, 1) else {
                    break;
                };
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
                    frames += 1;
                    let pts = start.elapsed().as_micros() as i64;
                    on_frame(key, pts, frame);
                }
                let _ = frames;
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
}

#[cfg(test)]
mod tests {
    use super::*;

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
        let v = PcVideo {
            output: 0,
            webcam: None,
            screen_w: 1920,
            screen_h: 1080,
        };
        assert_eq!(v.filter(1920, 1080), vec!["-map", "0:v"]);
        assert!(v.filter(2560, 1440)[1].contains("scale=2560:1440"));
        let cam = Webcam {
            enabled: true,
            device: "Cam".into(),
            ..Default::default()
        };
        let v = PcVideo {
            output: 0,
            webcam: Some(cam),
            screen_w: 1920,
            screen_h: 1080,
        };
        assert!(v.filter(1920, 1080)[1].contains("overlay=W-w-32:H-h-32"));
        assert_eq!(v.inputs().last().unwrap(), "video=Cam");
    }
}
