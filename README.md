# PhoneCast

**Stream your phone or your PC to YouTube and Twitch, with no lag, no
accounts and no bloat.** A lightweight open-source alternative to
scrcpy + OBS for game streamers.

- **Phone games** (Android 10+) over USB or Wi-Fi at native resolution and a
  steady 60 fps, with the game's own audio.
- **PC games and screen** captured on the GPU, with webcam picture-in-picture.
- **Audio mixer**: game, Discord (app or browser) and mic, each with its own
  slider; AI noise suppression, limiter, auto-ducking. No virtual cables.
- **Go live** on YouTube and Twitch at the same time, each at its best
  quality, encoded on your GPU (NVIDIA, AMD, Intel). Auto-reconnect.
- **Record** in original quality while streaming.
- **Honest health display**: fps, delay, bitrate, dropped frames, phone
  temperature, and a diagnostics log that says *where* a stall happened.
- ~5 MB installer, ~30 MB RAM. Nothing leaves your PC except the stream.
- **Optional local AI tools** (off by default, CPU only, separate process):
  auto highlight clips, Twitch/YouTube chat with spam flags and an in-game
  overlay, FAQ replies you approve.

> Status: early but usable. See [ROADMAP.md](ROADMAP.md) for what's next
> and [CHANGELOG.md](CHANGELOG.md) for what changed.

## Download
Grab the latest **Windows installer** and **Android APK** from
[Releases](../../releases).

- Windows 10/11, 64-bit. A GPU with a hardware encoder is recommended.
- **Code signing:** releases are not yet signed, so Windows shows
  "Windows protected your PC" → click **More info → Run anyway**. We are
  applying for free signing through [SignPath Foundation](https://signpath.org).
- **Going live needs FFmpeg** (the encoder engine). One-time install:
  `winget install Gyan.FFmpeg` in a terminal, then restart PhoneCast.

## Quick start
### Phone
1. Install the APK on the phone (Play Protect may warn: *More details →
   Install anyway*). Turn on **USB debugging** (Settings → About → tap
   *Build number* 7× → Developer options).
2. Plug the phone in and open PhoneCast on the PC: *USB: Ready*.
3. On the phone open **PhoneCast → Start streaming → Entire screen**, then
   open your game. It appears on the PC within a second.
4. Wi-Fi instead of USB: pick **Wi-Fi** in the phone app and type the address
   the PC app shows. 5 GHz Wi-Fi recommended.

### PC
1. In the **Source** panel choose **This PC** and the monitor.
2. Optionally switch on **Webcam picture-in-picture** and pick a corner.
3. Run your game in borderless/windowed fullscreen. (Exclusive fullscreen and
   anti-cheat-protected games may show black; see the roadmap.)

### Audio
Discord on the PC (app or Chrome/Edge/Firefox) is picked up automatically
and kept separate from game sound. Choose your mic in the Mic box. Wear
headphones to avoid echo.

### Go live
Click **Go live**, paste your YouTube and/or Twitch stream key, pick a
quality, **Start streaming**. The side panel shows each platform's status.
The window shows the upload speed you need; run a speed test first.

### AI tools (optional)
Everything here is off until you switch it on, runs on the CPU in a separate
program (`phonecast-ai-helper.exe`) that is killed when you switch it off,
and sends nothing anywhere except the chat connections you enable. CPU and
RAM use are shown next to the switch.

- **Auto highlight clips** watch game loudness, your voice and how fast chat
  moves. When that spikes, PhoneCast asks "Highlight?" (or saves straight
  away if you turn *Ask before saving* off) and keeps a clip from the replay
  buffer that starts ~25 s before the moment. **Sensitivity** and **Clip
  length** are yours to tune; **Recognise sounds** adds a 15 MB sound
  classifier (YAMNet) for gunfire, cheering and laughter.
- **Chat** shows Twitch and YouTube chat in the side panel and, with
  *overlay* on, in a small window that stays above your game. Spam is
  flagged, not deleted: too many messages, copy-pasted text, links, all-caps
  and emote walls are greyed out and written to the mod log.
  - *Twitch*: one-time setup at **dev.twitch.tv/console/apps**: register an
    app (category Chat Bot, client type **Public**, redirect
    `http://localhost`) and paste its Client ID. Login happens in your
    browser with a code; PhoneCast never sees your password.
  - *YouTube*: type your channel handle (`@name`). Reading works without any
    login; posting replies on YouTube is not possible without a Google
    developer account, so answers for YouTube viewers are shown for you to
    paste.
- **FAQ answers** come from `%APPDATA%\PhoneCast\ai\faq.yaml` (click *Edit
  answers*). A reply is only ever posted when a viewer types `!ask ...` **and**
  you turned on auto-reply, or when you click **Post** on a suggestion.
  *Reword answers with a local model* downloads Qwen3-1.7B (1.1 GB, 4-bit)
  and runs it on at most 4 CPU threads.
- Model downloads happen only when you switch a feature on, show their size
  first, and are verified against a known SHA-256.

## Troubleshooting
| Problem | Fix |
|---|---|
| "No phone on USB" | Use a data cable; accept *Allow USB debugging* on the phone |
| Phone stuck on "Waiting for PC" | Open the PC app first; on Wi-Fi allow PhoneCast in Windows Firewall |
| No game sound from phone | Some games block audio capture (a game-side setting) |
| Screen capture paused | The display is off or the PC is locked; it resumes automatically |
| Webcam not available | Close other apps using the camera |
| Stream stutters | Click **Open diagnostics**: the log names the slow stage (upload, disk, phone heat) |

## Build from source
- **PC app:** Rust stable, Node LTS, Visual Studio Build Tools (C++ workload).
  ```
  cd desktop && npm install && pwsh ./fetch-adb.ps1 && npx tauri build
  ```
- **AI helper** (optional): `cd desktop/ai-helper && cargo build --release`.
  The release workflow bundles it next to the app; in development the app
  finds it in `desktop/ai-helper/target/{debug,release}`. The sound
  classifier and local model need the `yamnet` / `llm` cargo features
  (`--features yamnet,llm`; the latter needs CMake and a C++ toolchain).
- **Phone app:** JDK 17 + Android SDK. `android\build-and-install.bat`.
- Details in [CONTRIBUTING.md](CONTRIBUTING.md). Protocol between phone and
  PC: [PROTOCOL.md](PROTOCOL.md).

## How it stays smooth
- The phone renders the game into a GPU texture and re-samples it onto an
  exact 60 fps grid before encoding, so uneven game frame timing never
  reaches the stream.
- Every output (preview, recorder, each live stream) has its own thread and a
  bounded queue; a slow disk or upload skips to the next keyframe instead of
  stalling capture.
- Video never touches the CPU on the way to the encoder when sizes match;
  otherwise there is exactly one copy.

## Licence
Apache-2.0. Third-party components are listed in [NOTICE](NOTICE). FFmpeg
is not bundled; PhoneCast runs your installed copy as a separate process.
