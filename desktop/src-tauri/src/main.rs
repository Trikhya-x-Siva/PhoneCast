// No console window in release builds.
#![cfg_attr(not(debug_assertions), windows_subsystem = "windows")]

mod adb;
mod ai;
mod audio;
mod banner_feed;
mod capture;
mod clips;
mod compositor;
mod core;
mod diag;
mod discovery;
mod engine;
mod gamemode;
mod hotkeys;
mod live;
mod mkv;
mod prefs;
mod presets;
mod replay;
mod scenes;
mod setup;
mod sinks;
mod test_control;

use crate::core::Core;
use serde_json::{json, Value};
use std::sync::Arc;
use tauri::{Manager, State};

type CoreState<'a> = State<'a, Arc<Core>>;

#[tauri::command]
fn get_info(core: CoreState) -> Value {
    json!({
        "previewPort": *core.preview_port.lock().unwrap(),
        "phonePort": core::phone_port(),
        "lanIp": core::lan_ip(),
        "recordingsDir": core::recordings_dir().display().to_string(),
    })
}

#[tauri::command]
fn set_bitrate(core: CoreState, mbps: f64) -> bool {
    core.set_bitrate((mbps * 1_000_000.0) as u32)
}

#[tauri::command]
fn request_keyframe(core: CoreState) -> bool {
    // From the preview: rate-limited (it used to ask ~2x per second while
    // its decoder lagged, and every keyframe pixelated the stream).
    core.request_keyframe_for_preview()
}

#[tauri::command(async)]
fn start_recording(core: CoreState) -> Result<String, String> {
    core.start_recording()
}

#[tauri::command(async)]
fn stop_recording(core: CoreState) -> Result<String, String> {
    core.stop_recording()
}

#[tauri::command]
fn set_audio_channel(
    core: CoreState,
    source: String,
    gain: Option<f32>,
    muted: Option<bool>,
) -> bool {
    match audio::Source::parse(&source) {
        Some(s) => {
            core.mixer.set_channel(s, gain, muted);
            true
        }
        None => false,
    }
}

#[tauri::command(async)]
fn set_mic_device(core: CoreState, id: Option<String>, name: Option<String>) {
    core.set_mic(id, name);
}

#[tauri::command]
fn set_voice_app(core: CoreState, id: String) {
    core.set_voice_app(&id);
}

#[tauri::command(async)]
fn save_clip(core: CoreState, seconds: u32) -> Result<(), String> {
    core.save_clip(seconds)
}

#[tauri::command(async)]
fn set_game_mode(core: CoreState, on: bool) {
    core.set_game_mode(on);
}

/// Saves the banner made in the banner editor: the PNG as the raw body, its
/// (small) settings as JSON in the `x-spec` header. Runs off the window's
/// main thread: big payloads there froze the window ("Not responding").
#[tauri::command(async)]
fn save_banner(request: tauri::ipc::Request<'_>) -> Result<(), String> {
    let tauri::ipc::InvokeBody::Raw(png) = request.body() else {
        return Err("expected the banner image".into());
    };
    let spec: serde_json::Value = request
        .headers()
        .get("x-spec")
        .and_then(|v| v.to_str().ok())
        .and_then(|s| serde_json::from_str(&urlencoding_decode(s)).ok())
        .ok_or("missing banner settings")?;
    if !png.starts_with(&[0x89, b'P', b'N', b'G', 0x0d, 0x0a, 0x1a, 0x0a]) || png.len() > 8 << 20 {
        return Err("Not a PNG image".into());
    }
    let p = live::banner_path();
    if let Some(d) = p.parent() {
        std::fs::create_dir_all(d).map_err(|e| e.to_string())?;
    }
    std::fs::write(&p, png).map_err(|e| e.to_string())?;
    std::fs::write(p.with_extension("json"), spec.to_string()).map_err(|e| e.to_string())?;
    diag::log("live", format!("banner saved ({} KB)", png.len() / 1024));
    Ok(())
}

/// Saves a moving banner: its frames (JPEG or PNG back to back in the body,
/// sizes in `x-sizes`, 30 fps) become one looping video file the stream
/// plays under the game. No frames = a still banner.
#[tauri::command(async)]
fn save_banner_frames(core: CoreState, request: tauri::ipc::Request<'_>) -> Result<(), String> {
    let tauri::ipc::InvokeBody::Raw(body) = request.body() else {
        return Err("expected raw frame data".into());
    };
    let sizes: Vec<usize> = request
        .headers()
        .get("x-sizes")
        .and_then(|v| v.to_str().ok())
        .unwrap_or("")
        .split(',')
        .filter(|s| !s.is_empty())
        .map(|s| s.parse().map_err(|_| "bad frame size".to_string()))
        .collect::<Result<_, _>>()?;
    if sizes.iter().sum::<usize>() != body.len() || sizes.len() > 1200 {
        return Err("frame data does not match its sizes".into());
    }
    let root = live::banner_frames_root();
    std::fs::create_dir_all(&root).map_err(|e| e.to_string())?;
    let set_current = |name: &str| -> Result<(), String> {
        let tmp = root.join("current.tmp");
        std::fs::write(&tmp, name).map_err(|e| e.to_string())?;
        std::fs::rename(&tmp, root.join("current.txt")).map_err(|e| e.to_string())
    };
    // Older versions: keep the newest few (a running stream may still be
    // reading one), delete the rest.
    let prune = |keep: &str| {
        let mut old: Vec<_> = std::fs::read_dir(&root)
            .into_iter()
            .flatten()
            .flatten()
            .filter(|e| e.path().is_dir() && e.file_name() != keep)
            .map(|e| e.file_name().to_string_lossy().into_owned())
            .collect();
        old.sort();
        let n = old.len().saturating_sub(2);
        for name in &old[..n] {
            std::fs::remove_dir_all(root.join(name)).ok();
        }
    };
    if sizes.is_empty() {
        set_current("")?;
        prune("");
        return Ok(());
    }
    let name = format!(
        "v{}",
        std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .map(|d| d.as_millis())
            .unwrap_or(0)
    );
    let dir = root.join(&name);
    std::fs::create_dir_all(&dir).map_err(|e| e.to_string())?;
    let mut at = 0;
    let mut ext = "png";
    for (i, n) in sizes.iter().enumerate() {
        let f = &body[at..at + n];
        ext = if f.starts_with(&[0xFF, 0xD8]) {
            "jpg"
        } else if f.starts_with(&[0x89, b'P', b'N', b'G']) {
            "png"
        } else {
            return Err("frame is not a JPEG or PNG".into());
        };
        std::fs::write(dir.join(format!("{i:04}.{ext}")), f).map_err(|e| e.to_string())?;
        at += n;
    }
    // One small looping video: decoding it costs far less than reading
    // hundreds of images, and it is a single file to swap.
    let ffmpeg = core
        .live
        .ffmpeg()
        .ok_or("FFmpeg is not installed on this PC")?;
    let num = |k: &str| {
        request
            .headers()
            .get(k)
            .and_then(|v| v.to_str().ok())
            .and_then(|v| v.parse::<u32>().ok())
    };
    // The channel's character clip (playing on the phone) on the strip's
    // left, looped to exactly the banner's 20 s.
    let clip = match (num("x-w"), num("x-h")) {
        (Some(w), Some(h)) if sizes.len() == 600 => {
            clips::prepare(&ffmpeg, "gaming", w, h, Some(480)).unwrap_or_else(|e| {
                diag::log("live", format!("banner clip not used: {e}"));
                None
            })
        }
        _ => None,
    };
    let mut cmd = std::process::Command::new(&ffmpeg);
    cmd.args([
        "-hide_banner",
        "-loglevel",
        "error",
        "-y",
        "-framerate",
        "30",
        "-i",
    ])
    .arg(dir.join(format!("%04d.{ext}")));
    // One picture, two files (FFmpeg needs a split to use it twice).
    let (mut map, mut map2) = ("0:v".to_string(), "0:v".to_string());
    if let Some(c) = &clip {
        cmd.args(["-framerate", &clips::CLIP_FPS.to_string(), "-i"])
            .arg(c.dir.join("%04d.png"))
            .args([
                "-filter_complex",
                "[0:v][1:v]overlay=x=0:y=0:eof_action=pass,format=yuv420p,split=2[b][b2]",
            ]);
        map = "[b]".into();
        map2 = "[b2]".into();
    }
    // loop.mp4 for a still-banner fallback; loop.h264 (keyframe every
    // second, headers on each) for the live feed, which switches banners
    // at any frame.
    cmd.args(["-map", &map])
        .args([
            "-c:v", "libx264", "-preset", "veryfast", "-crf", "14", "-pix_fmt", "yuv420p", "-g",
            "60", "-an",
        ])
        .arg(dir.join("loop.mp4"))
        .args(["-map", &map2])
        .args([
            "-c:v",
            "libx264",
            "-preset",
            "veryfast",
            "-crf",
            "16",
            "-pix_fmt",
            "yuv420p",
            "-g",
            "30",
            "-bf",
            "0",
            "-x264-params",
            "aud=1:repeat-headers=1",
            "-an",
            "-f",
            "h264",
        ])
        .arg(dir.join("loop.h264"));
    #[cfg(windows)]
    {
        use std::os::windows::process::CommandExt;
        cmd.creation_flags(0x0800_0000);
    }
    let out = cmd.output().map_err(|e| e.to_string())?;
    if !out.status.success() {
        return Err(format!(
            "could not make the banner loop: {}",
            String::from_utf8_lossy(&out.stderr).trim()
        ));
    }
    for i in 0..sizes.len() {
        std::fs::remove_file(dir.join(format!("{i:04}.{ext}"))).ok();
    }
    set_current(&name)?;
    prune(&name);
    banner_feed::changed();
    diag::log(
        "live",
        format!(
            "moving banner saved: {} frames, {} KB",
            sizes.len(),
            body.len() / 1024
        ),
    );
    Ok(())
}

/// The plain strip shown while the banner is switched off (one PNG, the
/// banner's size), kept as a 1 s H.264 loop for the live feed.
#[tauri::command(async)]
fn save_banner_plain(core: CoreState, request: tauri::ipc::Request<'_>) -> Result<(), String> {
    let tauri::ipc::InvokeBody::Raw(png) = request.body() else {
        return Err("expected the plain strip".into());
    };
    if !png.starts_with(&[0x89, b'P', b'N', b'G']) || png.len() > 8 << 20 {
        return Err("Not a PNG image".into());
    }
    let ffmpeg = core
        .live
        .ffmpeg()
        .ok_or("FFmpeg is not installed on this PC")?;
    let root = live::banner_frames_root();
    std::fs::create_dir_all(&root).map_err(|e| e.to_string())?;
    let src = root.join("plain.png");
    std::fs::write(&src, png).map_err(|e| e.to_string())?;
    let tmp = root.join("plain.tmp.h264");
    let mut cmd = std::process::Command::new(ffmpeg);
    cmd.args([
        "-hide_banner",
        "-loglevel",
        "error",
        "-y",
        "-loop",
        "1",
        "-framerate",
        "30",
        "-i",
    ])
    .arg(&src)
    .args([
        "-frames:v",
        "30",
        "-c:v",
        "libx264",
        "-preset",
        "veryfast",
        "-crf",
        "16",
        "-pix_fmt",
        "yuv420p",
        "-g",
        "30",
        "-bf",
        "0",
        "-x264-params",
        "aud=1:repeat-headers=1",
        "-f",
        "h264",
    ])
    .arg(&tmp);
    #[cfg(windows)]
    {
        use std::os::windows::process::CommandExt;
        cmd.creation_flags(0x0800_0000);
    }
    let out = cmd.output().map_err(|e| e.to_string())?;
    if !out.status.success() {
        return Err(String::from_utf8_lossy(&out.stderr).trim().to_string());
    }
    std::fs::rename(&tmp, banner_feed::plain_path()).map_err(|e| e.to_string())?;
    banner_feed::changed();
    Ok(())
}

/// One pop-up piece: 150 JPEG frames (5 s) of the banner loop from frame
/// `x-at` (0, 150, 300 or 450) with the pop-up drawn over them; kept as an
/// H.264 piece next to the current banner's loop, with the character clip
/// laid over at the same loop position.
#[tauri::command(async)]
fn save_banner_moment(core: CoreState, request: tauri::ipc::Request<'_>) -> Result<(), String> {
    let tauri::ipc::InvokeBody::Raw(body) = request.body() else {
        return Err("expected raw frame data".into());
    };
    let h = |k: &str| {
        request
            .headers()
            .get(k)
            .and_then(|v| v.to_str().ok())
            .unwrap_or("")
            .to_string()
    };
    let name = h("x-name");
    if !banner_feed::MOMENT_NAMES.contains(&name.as_str()) {
        return Err("unknown pop-up".into());
    }
    let at: usize = h("x-at").parse().map_err(|_| "bad position")?;
    if !at.is_multiple_of(banner_feed::MOMENT_FRAMES) || at >= 600 {
        return Err("bad position".into());
    }
    let sizes: Vec<usize> = h("x-sizes")
        .split(',')
        .filter(|s| !s.is_empty())
        .map(|s| s.parse().map_err(|_| "bad frame size".to_string()))
        .collect::<Result<_, _>>()?;
    if sizes.len() != banner_feed::MOMENT_FRAMES || sizes.iter().sum::<usize>() != body.len() {
        return Err("frame data does not match its sizes".into());
    }
    let dir = live::banner_frames_dir().ok_or("save the banner first")?;
    let tmp = dir.join(format!("tmp-{name}-{at}"));
    std::fs::remove_dir_all(&tmp).ok();
    std::fs::create_dir_all(&tmp).map_err(|e| e.to_string())?;
    let mut off = 0;
    for (i, n) in sizes.iter().enumerate() {
        std::fs::write(tmp.join(format!("{i:04}.jpg")), &body[off..off + n])
            .map_err(|e| e.to_string())?;
        off += n;
    }
    let ffmpeg = core
        .live
        .ffmpeg()
        .ok_or("FFmpeg is not installed on this PC")?;
    let num = |k: &str| h(k).parse::<u32>().ok();
    let clip = match (num("x-w"), num("x-h")) {
        (Some(w), Some(hh)) => clips::prepare(&ffmpeg, "gaming", w, hh, Some(480))
            .ok()
            .flatten(),
        _ => None,
    };
    let out = banner_feed::moment_path(&dir, &name, at);
    let part = out.with_extension("part");
    let mut cmd = std::process::Command::new(&ffmpeg);
    cmd.args([
        "-hide_banner",
        "-loglevel",
        "error",
        "-y",
        "-framerate",
        "30",
        "-i",
    ])
    .arg(tmp.join("%04d.jpg"));
    if let Some(c) = &clip {
        // The clip at the same moment of the loop (24 fps clip, 30 fps loop).
        let first = at * clips::CLIP_FPS as usize / 30 + 1;
        cmd.args([
            "-framerate",
            &clips::CLIP_FPS.to_string(),
            "-start_number",
            &first.to_string(),
            "-i",
        ])
        .arg(c.dir.join("%04d.png"))
        .args([
            "-filter_complex",
            "[0:v][1:v]overlay=x=0:y=0:eof_action=pass,format=yuv420p[b]",
            "-map",
            "[b]",
        ]);
    }
    cmd.args([
        "-frames:v",
        "150",
        "-c:v",
        "libx264",
        "-preset",
        "veryfast",
        "-crf",
        "16",
        "-pix_fmt",
        "yuv420p",
        "-g",
        "30",
        "-bf",
        "0",
        "-x264-params",
        "aud=1:repeat-headers=1",
        "-an",
        "-f",
        "h264",
    ])
    .arg(&part);
    #[cfg(windows)]
    {
        use std::os::windows::process::CommandExt;
        cmd.creation_flags(0x0800_0000);
    }
    let o = cmd.output().map_err(|e| e.to_string())?;
    std::fs::remove_dir_all(&tmp).ok();
    if !o.status.success() {
        return Err(format!(
            "could not make the pop-up: {}",
            String::from_utf8_lossy(&o.stderr).trim()
        ));
    }
    std::fs::rename(&part, &out).map_err(|e| e.to_string())
}

/// "Show now" for a banner pop-up (plays within 5 s).
#[tauri::command(async)]
fn show_banner_moment(name: String) -> Result<(), String> {
    banner_feed::show_moment(&name)
}

/// Automatic pop-ups: on/off, how often, which ones.
#[tauri::command(async)]
fn set_banner_moments(auto: bool, every: u64, names: Vec<String>) {
    banner_feed::set_auto(auto, every, names);
}

/// Pure 1080p60 mode on/off.
#[tauri::command(async)]
fn set_pure_mode(core: CoreState, on: bool) {
    core.set_pure(on);
}

/// Banner on (true) or the plain strip (false), live: no restart.
#[tauri::command(async)]
fn set_banner_on(core: CoreState, on: bool) {
    core.set_banner_on(on);
}

/// Encodes a scene (JPEG frames back to back in the body, sizes in
/// `x-sizes`, scene name in `x-name`, design hash in `x-design`).
#[tauri::command(async)]
fn prepare_scene(core: CoreState, request: tauri::ipc::Request<'_>) -> Result<(), String> {
    let tauri::ipc::InvokeBody::Raw(body) = request.body() else {
        return Err("expected raw frame data".into());
    };
    let h = |k: &str| {
        request
            .headers()
            .get(k)
            .and_then(|v| v.to_str().ok())
            .unwrap_or("")
            .to_string()
    };
    let sizes: Vec<usize> = h("x-sizes")
        .split(',')
        .filter(|s| !s.is_empty())
        .map(|s| s.parse().map_err(|_| "bad frame size".to_string()))
        .collect::<Result<_, _>>()?;
    if sizes.is_empty() || sizes.iter().sum::<usize>() != body.len() || sizes.len() > 600 {
        return Err("frame data does not match its sizes".into());
    }
    let mut frames = Vec::with_capacity(sizes.len());
    let mut at = 0;
    for n in sizes {
        frames.push(&body[at..at + n]);
        at += n;
    }
    // Action clip: a second keyframe where its hold starts (x-hold, seconds).
    let hold = h("x-hold").parse::<f64>().ok().filter(|t| *t > 0.0);
    core.prepare_scene(&h("x-name"), &h("x-design"), &frames, hold)
}

/// Saves the scene music (the file's bytes as the body) and loads it.
#[tauri::command(async)]
fn set_scene_music(core: CoreState, request: tauri::ipc::Request<'_>) -> Result<f64, String> {
    let tauri::ipc::InvokeBody::Raw(body) = request.body() else {
        return Err("expected the music file".into());
    };
    if body.len() > 200 << 20 {
        return Err("That music file is too big (200 MB max)".into());
    }
    let path = crate::core::Core::scene_music_path();
    if body.is_empty() {
        std::fs::remove_file(&path).ok();
        core.mixer.set_scene_music(Vec::new());
        return Ok(0.0);
    }
    std::fs::write(&path, body).map_err(|e| e.to_string())?;
    core.load_scene_music()
}

#[tauri::command(async)]
fn scene_ready(core: CoreState, name: String, design: String) -> bool {
    core.scene_ready(&name, &design)
}

#[tauri::command(async)]
fn set_scene(core: CoreState, name: Option<String>, design: String) -> Result<(), String> {
    core.user_set_scene(name.as_deref(), &design)
}

/// Privacy cover on/off ("Be right back" while a non-game app is on screen).
#[tauri::command(async)]
fn set_privacy_cover(core: CoreState, on: bool) {
    core.set_privacy_cover(on);
}

/// Decodes the %XX escapes of `encodeURIComponent` (headers are ASCII).
fn urlencoding_decode(s: &str) -> String {
    let b = s.as_bytes();
    let mut out = Vec::with_capacity(b.len());
    let mut i = 0;
    while i < b.len() {
        if b[i] == b'%' && i + 2 < b.len() {
            if let Ok(v) = u8::from_str_radix(&s[i + 1..i + 3], 16) {
                out.push(v);
                i += 3;
                continue;
            }
        }
        out.push(b[i]);
        i += 1;
    }
    String::from_utf8_lossy(&out).into_owned()
}

/// Folder of the banner's pictures (logo, own image, scene art, poses).
fn banner_assets_dir() -> std::path::PathBuf {
    live::banner_path().with_file_name("art").join("banner")
}

/// Only these names: the asset name comes from the UI and becomes a path.
fn asset_name_ok(name: &str) -> bool {
    matches!(
        name,
        "logo" | "image" | "sceneBg" | "stripBg" | "poseWave" | "poseShush"
    )
}

/// The banner editor's last settings (or null). `_loop` says whether the
/// moving banner is already made (then the app need not re-render it).
#[tauri::command(async)]
fn get_banner() -> serde_json::Value {
    let mut v: serde_json::Value = std::fs::read(live::banner_path().with_extension("json"))
        .ok()
        .and_then(|b| serde_json::from_slice(&b).ok())
        .unwrap_or(serde_json::Value::Null);
    if let Some(o) = v.as_object_mut() {
        o.insert("_loop".into(), live::banner_frames_dir().is_some().into());
        // Scenes with the channel's own character clip (drawn without a pose).
        o.insert("_clips".into(), clips::available().into());
    }
    v
}

/// One banner picture's bytes (binary, not JSON).
#[tauri::command(async)]
fn get_banner_asset(name: String) -> Result<tauri::ipc::Response, String> {
    if !asset_name_ok(&name) {
        return Err("unknown banner picture".into());
    }
    std::fs::read(banner_assets_dir().join(&name))
        .map(tauri::ipc::Response::new)
        .map_err(|e| e.to_string())
}

/// Saves one banner picture (raw body; empty body removes it).
#[tauri::command(async)]
fn save_banner_asset(request: tauri::ipc::Request<'_>) -> Result<(), String> {
    let tauri::ipc::InvokeBody::Raw(body) = request.body() else {
        return Err("expected the picture".into());
    };
    let name = request
        .headers()
        .get("x-name")
        .and_then(|v| v.to_str().ok())
        .unwrap_or("")
        .to_string();
    if !asset_name_ok(&name) || body.len() > 30 << 20 {
        return Err("unknown or too big banner picture".into());
    }
    let dir = banner_assets_dir();
    std::fs::create_dir_all(&dir).map_err(|e| e.to_string())?;
    if body.is_empty() {
        std::fs::remove_file(dir.join(&name)).ok();
        return Ok(());
    }
    std::fs::write(dir.join(&name), body).map_err(|e| e.to_string())
}

#[tauri::command]
fn set_replay_seconds(core: CoreState, seconds: u32) {
    core.set_replay_seconds(seconds);
}

#[tauri::command]
fn set_clip_look(core: CoreState, look: String) {
    core.set_clip_look(&look);
}

#[tauri::command]
fn open_clips() {
    let dir = replay::clips_dir();
    std::fs::create_dir_all(&dir).ok();
    std::process::Command::new("explorer").arg(dir).spawn().ok();
}

#[tauri::command]
fn get_hotkeys(core: CoreState) -> Value {
    core.hotkeys_json()
}

/// `bindings`: action id -> key text. Returns the problems found, if any.
#[tauri::command(async)]
fn set_hotkeys(core: CoreState, bindings: hotkeys::Bindings) -> Value {
    let probs = core.set_hotkeys(&bindings);
    json!({
        "problems": probs.iter().map(|(id, p)| json!({"id": id, "problem": p})).collect::<Vec<_>>(),
        "hotkeys": core.hotkeys_json(),
    })
}

#[tauri::command]
fn set_audio_option(core: CoreState, name: String, on: bool) {
    core.mixer.set_option(&name, on);
}

#[tauri::command(async)]
fn get_live_settings(core: CoreState) -> Value {
    core.live.settings_json()
}

#[tauri::command(async)]
fn save_live_settings(core: CoreState, settings: Value) -> Result<Value, String> {
    core.live.update_settings(&settings)?;
    Ok(core.live.settings_json())
}

/// While live: apply the saved settings now (2-3 s pause, same broadcast).
#[tauri::command(async)]
fn apply_live_now(core: CoreState) -> Result<(), String> {
    core.inner().clone().switch_live()
}

/// Validates an unsaved Go live edit: platform limits + upload budget.
#[tauri::command(async)]
fn check_live_settings(core: CoreState, settings: Value) -> Result<Value, String> {
    core.live.check_settings(&settings)
}

// ---------------- first-run setup wizard ----------------

type Install = std::sync::Mutex<Option<setup::DownloadState>>;

/// Encoder probe (runs FFmpeg a few times, so off the UI thread).
#[tauri::command]
async fn setup_probe(core: CoreState<'_>) -> Result<Value, String> {
    let core = core.inner().clone();
    tauri::async_runtime::spawn_blocking(move || core.live.encoder_info())
        .await
        .map_err(|e| e.to_string())
}

/// Starts the one-click FFmpeg install; poll `setup_install_progress`.
#[tauri::command]
fn setup_install_ffmpeg(core: CoreState, install: State<Install>) -> Value {
    let mut g = install.lock().unwrap();
    if let Some(st) = g.as_ref() {
        let j = st.json();
        if !j["failed"].as_bool().unwrap_or(false) && !j["done"].as_bool().unwrap_or(false) {
            return j;
        }
    }
    let st = setup::DownloadState::default();
    *g = Some(st.clone());
    let core = core.inner().clone();
    std::thread::spawn(move || {
        if setup::install_ffmpeg(&st).is_ok() {
            core.live.reload_ffmpeg();
        }
    });
    json!({ "percent": 0, "message": "Starting…" })
}

#[tauri::command]
fn setup_install_progress(install: State<Install>) -> Value {
    install
        .lock()
        .unwrap()
        .as_ref()
        .map(|s| s.json())
        .unwrap_or(Value::Null)
}

/// ~10 s upload test; emits `setup-upload` progress events. Saves the
/// minimum as the upload budget and returns per-platform recommendations.
#[tauri::command]
async fn setup_upload_test(app: tauri::AppHandle, core: CoreState<'_>) -> Result<Value, String> {
    use tauri::Emitter;
    let core = core.inner().clone();
    tauri::async_runtime::spawn_blocking(move || {
        let r = setup::upload_speed_test(10.0, &|f, mbps| {
            app.emit("setup-upload", json!({ "fraction": f, "mbps": mbps }))
                .ok();
        })?;
        // Plan on the sustained speed (25th percentile), not one brief dip.
        let min = (r.sustained * 10.0).floor() / 10.0;
        core.live.update_settings(&json!({ "uploadMbps": min }))?;
        let hevc = core.live.settings_json()["hevc"].as_bool().unwrap_or(false);
        Ok(json!({
            "minMbps": min,
            "medianMbps": (r.median * 10.0).round() / 10.0,
            "maxMbps": (r.max * 10.0).round() / 10.0,
            "samples": r.samples,
            "recommend": setup::recommendations(min, hevc),
        }))
    })
    .await
    .map_err(|e| e.to_string())?
}

#[tauri::command(async)]
fn start_live(core: CoreState) -> Result<(), String> {
    core.start_live()
}

#[tauri::command(async)]
fn stop_live(core: CoreState) {
    core.stop_live();
}

#[tauri::command(async)]
fn get_sources(core: CoreState) -> Value {
    core.refresh_sources()
}

#[tauri::command(async)]
fn set_source(core: CoreState, settings: capture::SourceSettings) -> Result<(), String> {
    core.set_source(settings)
}

/// Native file dialog for a PNG overlay (runs off the UI thread).
#[tauri::command]
async fn pick_image() -> Option<String> {
    tauri::async_runtime::spawn_blocking(capture::pick_png)
        .await
        .ok()
        .flatten()
}

#[tauri::command]
fn get_log() -> Vec<String> {
    diag::recent()
}

#[tauri::command]
fn open_logs() {
    let dir = diag::logs_dir();
    std::fs::create_dir_all(&dir).ok();
    std::process::Command::new("explorer").arg(dir).spawn().ok();
}

#[tauri::command]
fn open_recordings() {
    let dir = core::recordings_dir();
    std::fs::create_dir_all(&dir).ok();
    std::process::Command::new("explorer").arg(dir).spawn().ok();
}

// ---------------- local AI helper (optional, separate process) ----------------

type AiState<'a> = State<'a, Arc<ai::Ai>>;

#[tauri::command]
fn ai_get_settings(ai: AiState) -> Value {
    ai.settings_json()
}

#[tauri::command(async)]
fn ai_save_settings(ai: AiState, settings: Value) -> Result<Value, String> {
    ai.update_settings(settings)?;
    Ok(ai.settings_json())
}

#[tauri::command]
fn ai_get_status(ai: AiState) -> Value {
    ai.status_json()
}

/// Forwards a message to the helper (download, twitchLogin, approve…).
#[tauri::command]
fn ai_send(ai: AiState, message: Value) -> bool {
    ai.send(&message)
}

#[tauri::command]
fn ai_set_overlay(ai: AiState, on: bool) -> Result<(), String> {
    ai.set_overlay(on)
}

#[tauri::command]
fn ai_open_faq(ai: AiState) {
    let path = ai.status_json()["helper"]["faqPath"]
        .as_str()
        .map(std::path::PathBuf::from)
        .unwrap_or_else(|| {
            let base = std::env::var("APPDATA")
                .map(std::path::PathBuf::from)
                .unwrap_or_else(|_| std::path::PathBuf::from("."));
            base.join("PhoneCast").join("ai").join("faq.yaml")
        });
    if let Some(d) = path.parent() {
        std::fs::create_dir_all(d).ok();
    }
    std::process::Command::new("notepad").arg(path).spawn().ok();
}

/// Opens an https link in the default browser (Twitch login page).
#[tauri::command]
fn ai_open_url(url: String) -> bool {
    if !url.starts_with("https://") || url.chars().any(|c| c.is_whitespace() || c == '"') {
        return false;
    }
    std::process::Command::new("rundll32")
        .args(["url.dll,FileProtocolHandler", &url])
        .spawn()
        .is_ok()
}

/// Puts PhoneCast in a Windows job that closes every program it started
/// (FFmpeg, the AI helper) when PhoneCast ends in any way, even a crash or
/// Task Manager. Leftover FFmpeg screen captures otherwise keep Windows'
/// limited desktop-duplication slots ("Too many open duplication sessions")
/// and the next PC-mode stream cannot start.
#[cfg(windows)]
fn close_children_with_phonecast() {
    use windows_sys::Win32::System::JobObjects::{
        AssignProcessToJobObject, CreateJobObjectW, JobObjectExtendedLimitInformation,
        SetInformationJobObject, JOBOBJECT_EXTENDED_LIMIT_INFORMATION,
        JOB_OBJECT_LIMIT_KILL_ON_JOB_CLOSE,
    };
    use windows_sys::Win32::System::Threading::GetCurrentProcess;
    // SAFETY: plain Win32 calls with valid arguments; the job handle is kept
    // open for the life of the process on purpose (closing it ends the job).
    unsafe {
        let job = CreateJobObjectW(std::ptr::null(), std::ptr::null());
        if job.is_null() {
            return;
        }
        let mut info: JOBOBJECT_EXTENDED_LIMIT_INFORMATION = std::mem::zeroed();
        info.BasicLimitInformation.LimitFlags = JOB_OBJECT_LIMIT_KILL_ON_JOB_CLOSE;
        let ok = SetInformationJobObject(
            job,
            JobObjectExtendedLimitInformation,
            &info as *const _ as *const std::ffi::c_void,
            std::mem::size_of::<JOBOBJECT_EXTENDED_LIMIT_INFORMATION>() as u32,
        );
        if ok == 0 || AssignProcessToJobObject(job, GetCurrentProcess()) == 0 {
            diag::log(
                "app",
                "could not tie child programs to PhoneCast (job object)",
            );
        }
    }
}

fn main() {
    diag::init();
    // Any internal error is written to the log before anything else, so a
    // crash always leaves its reason behind.
    let default_hook = std::panic::take_hook();
    std::panic::set_hook(Box::new(move |info| {
        let thread = std::thread::current().name().unwrap_or("?").to_string();
        diag::log(
            "crash",
            format!("internal error in thread {thread}: {info}"),
        );
        default_hook(info);
    }));
    #[cfg(windows)]
    close_children_with_phonecast();
    tauri::Builder::default()
        .setup(|app| {
            let core = Core::start(app.handle().clone()).map_err(|e| {
                format!(
                    "PhoneCast could not open port {}: {e}. Is it already running?",
                    core::phone_port()
                )
            })?;
            let ai = ai::Ai::new(app.handle().clone(), core.mixer.clone());
            if test_control::enabled() {
                test_control::spawn(app.handle().clone(), core.clone());
            }
            app.manage(core);
            app.manage(Install::default());
            app.manage(ai);
            Ok(())
        })
        .on_window_event(|window, event| {
            // Finish the recording file cleanly when the app is closed.
            if let tauri::WindowEvent::CloseRequested { .. } = event {
                if let Some(ai) = window.try_state::<Arc<ai::Ai>>() {
                    ai.stop();
                }
                if let Some(core) = window.try_state::<Arc<Core>>() {
                    core.stop_live_because("window closed");
                    core.stop_recording().ok();
                    // Give the recorder and streams a moment to close cleanly.
                    std::thread::sleep(std::time::Duration::from_millis(800));
                }
            }
        })
        .invoke_handler(tauri::generate_handler![
            get_info,
            set_bitrate,
            request_keyframe,
            start_recording,
            stop_recording,
            set_audio_channel,
            set_mic_device,
            set_voice_app,
            set_audio_option,
            get_live_settings,
            save_live_settings,
            check_live_settings,
            setup_probe,
            setup_install_ffmpeg,
            setup_install_progress,
            setup_upload_test,
            start_live,
            stop_live,
            get_sources,
            set_source,
            pick_image,
            get_log,
            open_logs,
            open_recordings,
            ai_get_settings,
            ai_save_settings,
            ai_get_status,
            ai_send,
            ai_set_overlay,
            ai_open_faq,
            ai_open_url,
            save_clip,
            set_replay_seconds,
            set_game_mode,
            set_privacy_cover,
            save_banner_plain,
            save_banner_moment,
            show_banner_moment,
            set_banner_moments,
            set_banner_on,
            set_pure_mode,
            save_banner,
            get_banner_asset,
            save_banner_asset,
            apply_live_now,
            save_banner_frames,
            prepare_scene,
            set_scene_music,
            scene_ready,
            set_scene,
            get_banner,
            set_clip_look,
            open_clips,
            get_hotkeys,
            set_hotkeys
        ])
        .build(tauri::generate_context!())
        .expect("error while running PhoneCast")
        .run(|app, event| {
            // Closing PhoneCast puts the phone's screen size back.
            if let tauri::RunEvent::Exit = event {
                if let Some(core) = app.try_state::<Arc<Core>>() {
                    // Closing PhoneCast ends the stream on every platform.
                    core.live.stop_and_wait(std::time::Duration::from_secs(12));
                    core.gamemode.restore();
                }
            }
        });
}
