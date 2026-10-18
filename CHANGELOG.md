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
- **Window and game capture**: stream one window (a game, an emulator such
  as BlueStacks for BGMI, a browser) through Windows Graphics Capture, even
  with other windows in front. The window picker lists open windows with
  their app name and finds the same app again after a restart.
- **Overlays**: add PNG images (your logo) and text, drag them into place
  and resize them right on the preview; the webcam can be dragged too.
  Composed on the GPU in Direct3D 11 and changed live, even while streaming.
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
