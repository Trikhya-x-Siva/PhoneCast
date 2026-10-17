# PhoneCast — mobile game streaming, all in one

Goal: stream Android games to YouTube and Twitch with the best possible quality,
low latency, clean game audio + mic, from one lightweight app — replacing
scrcpy + OBS.

## Hardware this is built for
- Phone: OnePlus CPH2691 (Snapdragon, Android 16), panel 1264x2780 @ 120Hz,
  hardware H.264 / HEVC encoders (`c2.qti.avc.encoder`, `c2.qti.hevc.encoder`).
- PC: Intel i5-13450HX, NVIDIA RTX 4050 Laptop (NVENC + NVDEC), 16 GB RAM.

## Architecture

```
┌──────────── PHONE (PhoneCast Android) ────────────┐
│ MediaProjection ─► VirtualDisplay ─► HEVC hw enc  │   only hardware blocks
│ AudioPlaybackCapture (game audio) ─► raw PCM      │   do the work; no preview
│ timestamps from one clock (A/V stay in sync)      │   on the phone, no extra
│ foreground service + live temperature reporting   │   copies -> low battery/heat
└────────────────────────┬──────────────────────────┘
                         │ USB (adb reverse TCP), ~40–80 Mbps near-lossless
┌────────────────────────▼──────────────────────────┐
│ PC (PhoneCast Desktop: Tauri app + FFmpeg engine)  │
│ NVDEC decode ─► compositor (game, webcam, overlays,│
│                 text, alerts) on the GPU           │
│ game audio + PC mic ─► mixer (gain, noise gate,    │
│                 noise suppression, limiter, sync)  │
│ NVENC encode, one per destination:                 │
│   YouTube: up to 4K60 HEVC/AV1 (enhanced RTMP)     │
│   Twitch : 1080p60 H.264 (or 1440p if enabled)     │
│ Optional local recording (high-quality MKV)        │
└────────────────────────────────────────────────────┘
```

Why this beats scrcpy + OBS:
- No window capture step: frames go straight from the decoder into the
  compositor, so no stutter from window capture and no blur from scaling twice.
- Phone sends near-lossless video; the only "real" compression is the final
  NVENC encode, done once per platform at that platform's best settings.
- Game audio and video carry the same clock from the phone -> no drift, no
  manual sync offset.
- Heavy work is on dedicated hardware (phone encoder, NVDEC, NVENC), so the
  CPU stays nearly idle, the phone stays cooler, and the battery lasts longer.

## Connection: USB or wireless (same app, switch anytime)
- **USB** (adb reverse): most stable, phone charges while streaming. Near-lossless
  ~40–80 Mbps.
- **Wireless** (direct TCP between phone and PC, no adb needed after pairing):
  - Pairing by scanning a QR code shown in the PC app; afterwards the phone
    finds the PC automatically on the network (mDNS).
  - Low-latency mode: no buffering beyond 1 frame, frame-pacing on the phone,
    Wi-Fi low-latency lock (`WIFI_MODE_FULL_LOW_LATENCY`) so the phone's
    Wi-Fi radio doesn't doze between packets.
  - Adaptive bitrate: measures the link continuously and lowers/raises the
    phone's encoder bitrate (≈20–40 Mbps on 5/6 GHz) so the picture never
    freezes; it gets softer briefly instead.
  - Expected extra delay: ~10–30 ms on 5 GHz / 6 GHz Wi-Fi with the PC on a
    cable to the router. 2.4 GHz works but is not recommended for streaming.
  - Fallback: if no router is nearby, the phone's hotspot can be used
    (PC joins the phone's hotspot).

## Honest limits (so expectations are right)
- **4K:** the phone screen itself is 1264x2780. We capture every native pixel
  and output a 4K canvas for YouTube. YouTube processes 4K uploads at a much
  higher quality level, so this visibly improves the stream, but it is upscaled,
  not extra detail.
- **Twitch** accepts at most 1080p60 normally (1440p only with Enhanced
  Broadcasting). We send each platform its own best format.
- **Viewer delay** of 2–5 s is set by YouTube/Twitch, not by us. Our part
  (phone to stream output) targets < 60 ms.
- **Phone heat** comes mostly from the game itself. PhoneCast adds only the
  hardware encoder (a few %), shows phone temperature live, and can lower
  fps/bitrate automatically if the phone gets too hot. Phones throttle before
  any damage happens.

## Build phases
1. **Capture prototype:** Android app (screen + game audio) and a PC receiver
   that previews and records to file. Measure latency, dropped frames, A/V sync.
   Transport is abstracted from day one so USB and Wi-Fi share all code.
2. **Wireless:** QR pairing, auto-discovery, low-latency Wi-Fi lock,
   adaptive bitrate, live link-quality meter.
3. **Audio mixer** with a slider + mute per source:
   - Game audio (from the phone, already done)
   - **Discord voice** captured from the Discord app on the laptop only
     (Windows per-app loopback), so music/notification sounds stay out.
     Android blocks capturing voice-call audio from Discord *on the phone*.
   - **Your mic**: laptop/headset mic (WASAPI), or the phone mic as an option
   - Noise suppression + noise gate on the mic, limiter on the master,
     auto-ducking (game gets quieter while you or teammates talk)
   - Monitor output so you can hear the mix in your headphones
4. **Streaming:** YouTube + Twitch (simultaneous), stream keys stored locally
   and encrypted, health stats (bitrate, dropped frames, phone temp).
   Output is constant 60 fps (re-timed on the PC) so viewers never see judder.
5. **Studio UI:** scenes, webcam, image/text overlays, "Be right back" scene,
   hotkeys.
6. **Polish:** auto-reconnect, installer, settings presets.

## Toolchain needed
- Android: JDK 17 + Android SDK command-line tools (no Android Studio required).
- Desktop: Rust + MSVC Build Tools (Tauri), FFmpeg build with NVENC/NVDEC.
