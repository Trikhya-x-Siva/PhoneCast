# Changelog

All notable changes to PhoneCast. Format: [Keep a Changelog](https://keepachangelog.com),
versions follow [SemVer](https://semver.org).

## [Unreleased]
### Added
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
