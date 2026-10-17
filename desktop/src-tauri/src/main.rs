// No console window in release builds.
#![cfg_attr(not(debug_assertions), windows_subsystem = "windows")]

mod adb;
mod audio;
mod capture;
mod core;
mod diag;
mod live;
mod mkv;
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
        "phonePort": core::PHONE_PORT,
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
fn set_mic_device(core: CoreState, id: Option<String>) {
    core.mixer.set_mic_device(id);
}

#[tauri::command]
fn set_voice_app(core: CoreState, id: String) {
    core.mixer.set_voice_app(&id);
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

fn main() {
    diag::init();
    tauri::Builder::default()
        .setup(|app| {
            let core = Core::start(app.handle().clone()).map_err(|e| {
                format!(
                    "PhoneCast could not open port {}: {e}. Is it already running?",
                    core::PHONE_PORT
                )
            })?;
            app.manage(core);
            Ok(())
        })
        .on_window_event(|window, event| {
            // Finish the recording file cleanly when the app is closed.
            if let tauri::WindowEvent::CloseRequested { .. } = event {
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
            get_log,
            open_logs,
            open_recordings
        ])
        .run(tauri::generate_context!())
        .expect("error while running PhoneCast");
}
