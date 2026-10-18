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
