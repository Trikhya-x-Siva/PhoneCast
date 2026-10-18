# Changelog

All notable changes to PhoneCast. Format: [Keep a Changelog](https://keepachangelog.com),
versions follow [SemVer](https://semver.org).

## [Unreleased]
### Added
- **Platform presets** for YouTube, Twitch, Kick, Facebook Gaming and any
  RTMP/RTMPS/SRT server, built from each platform's official ingest specs
  (codec, size, CBR bitrate, 2 s keyframes, B-frames, profile/level, AAC).
  YouTube presets sit at or above YouTube's recommended bitrate.
- **Pre-flight check** before going live: every setting is checked against
  the platform's limits, the server URL and the key, and the total against
  the measured upload with 30% spare. When the upload is short, the
  platforms not marked **Main** are lowered first.
- **Go live window**: add or remove platforms, star the Main one, pick a
  preset with its limits shown, live messages while you edit.
- **Setup check** (first run, re-runnable): encoder detection, one-click
  FFmpeg install with checksum, a 10-second upload test that plans with the
  slowest moment, recommended quality per platform, YouTube Studio tips.
### Changed
- Streams go through FFmpeg's fifo muxer: a network drop reconnects inside
  FFmpeg within seconds, without restarting the encoder.
- NVENC: two-pass (quarter resolution), B-frames as references, fixed
  keyframes (no scene-cut keyframes), short lookahead, 1 s CBR buffer.
### Fixed
- Blurred-background framing on NVIDIA sent 1920x1088 instead of 1080p
  (and 1280x736 for 720p).
- **Local AI tools (optional, off by default)** in a separate, crash-isolated
  helper process (`phonecast-ai-helper`, CPU only): auto highlight clips
  from game loudness, your voice and chat speed (with an optional 15 MB
  YAMNet sound classifier); Twitch (EventSub, device-code login) and
  YouTube live chat in the side panel and an always-on-top overlay; spam
  flags (rate, near-duplicates, links, caps, emote walls) with a mod log;
  FAQ answers from your own `faq.yaml`, posted only on `!ask` or after you
  click Post, optionally reworded by a 1.1 GB local model. Models download
  only when you switch them on and are SHA-256 checked; CPU/RAM use is shown.
- **Window and game capture**: stream one window (a game, an emulator such
  as BlueStacks for BGMI, a browser) through Windows Graphics Capture, even
  with other windows in front. The window picker lists open windows with
  their app name and finds the same app again after a restart.
- **Overlays**: add PNG images (your logo) and text, drag them into place
  and resize them right on the preview; the webcam can be dragged too.
  Composed on the GPU in Direct3D 11 and changed live, even while streaming.
- **Instant clips (replay buffer)**: the last 90 s (adjustable, 30 s to
  5 min, or off) of the phone stream or PC capture are kept in memory;
  **Ctrl+Shift+F8 / F9 / F10** save the last 15 / 30 / 60 s as an MP4 in
  `Videos\PhoneCast\Clips` without re-encoding (MKV when FFmpeg is not
  installed). A **Clips** button opens the folder; a Clips panel has the
  three buttons and the buffer length. *Clip picture: like the stream*
  instead re-encodes the clip on the GPU with the stream's 16:9 framing
  (blurred background by default) at the YouTube quality (1440p60), at low
  priority so a live stream is never slowed down.
- **Global hotkeys** that work while the game has focus: clips, mute mic
  (Ctrl+Shift+M), record (Ctrl+Shift+R), go live / end (Ctrl+Shift+L).
  A Hotkeys sheet rebinds them; conflicts with other apps are reported.
- **Settings that survive device changes**: mic (by Windows endpoint ID),
  Discord source, monitor (by hardware ID) and webcam (by name) are
  remembered in `%APPDATA%\PhoneCast\settings.json`. A missing device gets
  a plain-language warning and PhoneCast waits for it instead of quietly
  switching to another one.
- **PC streaming**: capture a monitor on the GPU (Desktop Duplication →
  NVENC, zero-copy at native size), optional webcam picture-in-picture, PC
  sound in the mixer with Discord kept separate; record and go live from
  the PC with no phone attached.
- GitHub Actions: CI on every push, Windows installer + Android APK attached
  to a draft release on version tags.
- Diagnostics log in `%APPDATA%\PhoneCast\logs` and an "Open diagnostics"
  button, for finding where a stream got stuck.
- Discord voice can be captured from a browser (Chrome, Edge, Firefox,
  Brave, Opera) as well as the Discord app, with a source picker.
- Public roadmap, contributing guide and security policy.

### Fixed
- PC-mode recordings were nearly empty (no audio reached the recorder, so
  FFmpeg never started writing). Stopping a window-capture recording or
  stream now ends FFmpeg cleanly, so files are finished instead of cut off.

## [0.1.0] - 2026-10-17
### Added
- Phone capture over USB or Wi-Fi with a frame pacer for steady 60 fps.
- Audio mixer: game, Discord, mic; noise suppression, gate, limiter, ducking.
- Lossless MKV recording.
- Go live to YouTube and Twitch simultaneously with a GPU-only pipeline.
- Live preview, stream health and phone health panels.
