# Changelog

All notable changes to PhoneCast. Format: [Keep a Changelog](https://keepachangelog.com),
versions follow [SemVer](https://semver.org).

## [Unreleased]
### Added
- **Local AI tools (optional, off by default)** in a separate, crash-isolated
  helper process (`phonecast-ai-helper`, CPU only): auto highlight clips
  from game loudness, your voice and chat speed (with an optional 15 MB
  YAMNet sound classifier); Twitch (EventSub, device-code login) and
  YouTube live chat in the side panel and an always-on-top overlay; spam
  flags (rate, near-duplicates, links, caps, emote walls) with a mod log;
  FAQ answers from your own `faq.yaml`, posted only on `!ask` or after you
  click Post, optionally reworded by a 1.1 GB local model. Models download
  only when you switch them on and are SHA-256 checked; CPU/RAM use is shown.
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

## [0.1.0] - 2026-10-17
### Added
- Phone capture over USB or Wi-Fi with a frame pacer for steady 60 fps.
- Audio mixer: game, Discord, mic; noise suppression, gate, limiter, ducking.
- Lossless MKV recording.
- Go live to YouTube and Twitch simultaneously with a GPU-only pipeline.
- Live preview, stream health and phone health panels.
