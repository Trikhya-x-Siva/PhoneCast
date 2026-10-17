# PhoneCast roadmap

PhoneCast is a lightweight, open-source streaming app for **phones and PCs**:
capture, mix, record and go live on YouTube and Twitch, with no accounts and
no cloud. This roadmap comes from what streamers actually ask for in the
issue trackers and idea boards of OBS Studio, Streamlabs, scrcpy, Sunshine
and Moonlight (October 2026), filtered through one rule: **everything must
stay light and never stutter**.

## Guiding principles
1. **Smooth first.** A steady 60 fps and clean audio beat any feature. Every
   output runs on its own thread; a slow disk or upload can never stall capture.
2. **No setup maze.** Plug in, pick a game, go live. Sensible defaults chosen
   from the hardware; advanced settings exist but are never required.
3. **Honest health display.** fps, delay, bitrate, dropped frames and phone
   temperature are always visible. Problems get a plain-language cause.
4. **Nothing leaves your PC** except the stream itself. Stream keys are
   encrypted locally; no accounts, no telemetry.
5. **Never touch system audio routing.** PhoneCast captures apps directly;
   it never changes your default devices (a top Sunshine/OBS complaint).

## Done (v0.1)
- Phone capture over USB or Wi-Fi at native resolution, steady 60 fps
  (frame pacer), constant bitrate, no B-frames, BT.709 colour.
- Audio mixer: game + Discord (app or browser) + mic with RNNoise noise
  suppression, noise gate, limiter, optional auto-ducking.
- Lossless MKV recording with the mixed audio; keeps recording through rotation.
- Go live to YouTube and Twitch at once, full-GPU pipeline (NVDEC → CUDA
  scale → NVENC), per-platform best presets, auto-reconnect, encrypted keys.
- Live preview with pacing, health stats, phone battery/temperature.
- Diagnostics log (`%APPDATA%\PhoneCast\logs`) for "it got stuck" reports.

## v0.2 — PC streaming (next)
The most-requested capture features, built on GPU-only paths:
- **Screen capture** of a monitor (Desktop Duplication / Windows Graphics
  Capture) straight into NVENC — no CPU copies.
- **Window capture** of one app via Windows Graphics Capture.
- **PC game capture**: borderless and most fullscreen games via WGC; known
  limits (exclusive fullscreen on some titles, HDR, anti-cheat) documented
  plainly in the UI rather than failing silently (OBS issues #13337, #14021).
- **Webcam** as a picture-in-picture source with position/size presets.
- **Overlays**: images (PNG) and text; **browser sources** for alerts and
  chat widgets (StreamElements, etc.).
- **Scenes**: a few named layouts (Game, Game + cam, Be right back) with a
  hotkey each; hotkeys work while the game has focus (OBS #4126, #10701).
- **PC audio**: capture any app's sound separately (game, music, Discord)
  with a visible master meter — the #1/#8 OBS idea-board requests —
  without VB-Cable or Voicemeeter.
- **Setup wizard**: detect GPU encoder, measure upload speed, recommend
  quality for each platform; one-click apply.

## v0.3 — reliability and streamer tools
- **Dynamic bitrate** on upload congestion with a visible health indicator
  (Streamlabs' most-marketed feature; OBS hides it).
- **Reconnect that never loses audio** (OBS #5572): outputs re-sync from the
  phone clock after any drop.
- **Replay buffer / instant clips**: hotkeys for 15 s / 30 s / 60 s clips from
  a disk-backed buffer (OBS idea #263), saved as MP4 and optionally sent to
  the phone for Shorts/TikTok.
- **Vertical output**: a second 9:16 canvas for TikTok/Shorts/Kick alongside
  the 16:9 stream (why Aitum Vertical is a top OBS plugin).
- **Multistream to more platforms**: Kick, TikTok, custom RTMP(S)/SRT.
- **Virtual camera + virtual mic**: send the mix to Discord/Zoom as a webcam
  and microphone (OBS ideas #965, #1415).
- **Phone as webcam / phone mic** for PC streams (scrcpy's most-requested
  issues #776, #4208): the phone app already streams; add a camera mode.
- **Gamepad / Stream Deck / MIDI hotkeys** for scene switches and mute.
- **ISO recording**: each source as its own track for editing.
- **Settings never reset** after Windows updates: device selection by stable
  ID with graceful fallback and a warning (OBS #12050).

## v0.4 — lightweight local AI (optional, off by default)
All of it runs on the CPU as a separate, crash-isolated helper process, so
the game and encoder keep the GPU. Everything can be disabled.
- **Auto highlight clips**: fuse game-audio loudness, mic excitement and chat
  message rate into a score; save a clip from the replay buffer when it
  spikes. Optional small audio classifier (YAMNet, ~4 MB) to recognise
  gunfire/cheers/laughter, and optional kill-feed OCR per game.
- **Chat helper**: read YouTube and Twitch chat (EventSub / live chat
  stream), show it in-app and as an in-game overlay, filter spam with
  simple rules (rate limits, near-duplicates, link filters), and answer a
  streamer-defined FAQ list — optionally rephrased by a ~1 GB local model.
- **Live captions** (later): streaming speech-to-text on CPU (Moonshine via
  sherpa-onnx), ~0.5 s delay, as an overlay.
- **Better noise suppression**: DeepFilterNet as a "high quality" option next
  to RNNoise "low latency".

## Platform and distribution
- Windows 10/11 first (GPU pipelines are Windows-specific today). Linux and
  macOS PC capture are welcome contributions; the phone↔PC protocol and UI
  are portable.
- Android 10+ phone app; iOS is not possible (no system screen+audio capture
  API for third-party apps).
- Releases built by GitHub Actions; Windows installer signed through SignPath
  Foundation once the project qualifies; Android APK on GitHub Releases,
  Obtainium and IzzyOnDroid.
- FFmpeg (GPL) is downloaded on first "Go live" rather than bundled, so the
  project itself stays Apache-2.0 and the installer stays small.

## Not planned
- Accounts, cloud sync, or any server component.
- Paid tiers. Everything is free and open source.
- Bundling proprietary SDKs (e.g. NVIDIA Maxine) — too heavy and not
  redistributable; use NVIDIA Broadcast upstream if you want it.

Have a feature in mind? Open a discussion on GitHub — the top-voted ideas
shape the next version.
