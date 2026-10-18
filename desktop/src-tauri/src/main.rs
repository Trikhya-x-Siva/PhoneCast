// No console window in release builds.
#![cfg_attr(not(debug_assertions), windows_subsystem = "windows")]

mod adb;
mod ai;
mod audio;
mod capture;
mod compositor;
mod core;
mod diag;
mod hotkeys;
mod live;
mod mkv;
mod prefs;
mod replay;
mod sinks;

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
    core.request_keyframe()
}

#[tauri::command]
fn start_recording(core: CoreState) -> Result<String, String> {
    core.start_recording()
}

#[tauri::command]
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

#[tauri::command]
fn set_mic_device(core: CoreState, id: Option<String>, name: Option<String>) {
    core.set_mic(id, name);
}

#[tauri::command]
fn set_voice_app(core: CoreState, id: String) {
    core.set_voice_app(&id);
}

#[tauri::command]
fn save_clip(core: CoreState, seconds: u32) -> Result<(), String> {
    core.save_clip(seconds)
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
#[tauri::command]
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

#[tauri::command]
fn get_live_settings(core: CoreState) -> Value {
    core.live.settings_json()
}

#[tauri::command]
fn save_live_settings(core: CoreState, settings: Value) -> Result<Value, String> {
    core.live.update_settings(&settings)?;
    Ok(core.live.settings_json())
}

#[tauri::command]
fn start_live(core: CoreState) -> Result<(), String> {
    core.start_live()
}

#[tauri::command]
fn stop_live(core: CoreState) {
    core.stop_live();
}

#[tauri::command]
fn get_sources(core: CoreState) -> Value {
    core.refresh_sources()
}

#[tauri::command]
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

#[tauri::command]
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

fn main() {
    diag::init();
    tauri::Builder::default()
        .setup(|app| {
            let core = Core::start(app.handle().clone()).map_err(|e| {
                format!(
                    "PhoneCast could not open port {}: {e}. Is it already running?",
                    core::phone_port()
                )
            })?;
            let ai = ai::Ai::new(app.handle().clone(), core.mixer.clone());
            app.manage(core);
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
                    core.stop_live();
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
            set_clip_look,
            open_clips,
            get_hotkeys,
            set_hotkeys
        ])
        .run(tauri::generate_context!())
        .expect("error while running PhoneCast");
}
