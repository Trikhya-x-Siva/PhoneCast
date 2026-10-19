# OSSG UI/UX research: what to copy from the best streaming tools

Research only, written 2026-10-19. It is based on the current `desktop/ui` (index.html, style.css, app.js,
golive.js, banner.js) and `android/.../MainActivity.kt`, compared with OBS Studio, Streamlabs Desktop,
PRISM Live Studio, Meld Studio, Twitch Studio, StreamYard/Restream, Larix and open-source projects.

## What OSSG already does well (keep it)

- The Go Live sheet runs a real preflight. `presets.rs` checks each destination against the platform's
  limits, issues are tagged Fix / Heads-up / Note, the upload budget keeps 30% spare, and the "Main" star
  sets which platform keeps quality. This is better than OBS and on par with Restream.
- The Setup check measures the *slowest* upload second. That is a stronger approach than OBS's
  auto-config wizard.
- Scenes work before the phone connects, the privacy cover runs automatically, and the banner can be
  changed live. These are features competitors don't have. The work below is mostly **presentation,
  hierarchy and remote control**, not new engine features.

## Main gaps found in the current UI

1. **The streamer is holding the phone, but every live control is on the laptop.** The Android app is a
   settings form with a Start button (`MainActivity.buildUi`). It has no live controls and no status.
2. **"End stream" ends immediately on one click.** `golive.js`, `$("liveBtn").onclick` calls
   `stop_live` with no confirmation.
3. **Health shows the wrong link.** The "Stream health" card shows phone→PC fps, latency and Mbps. The
   numbers that viewers actually feel (per-platform outgoing bitrate, drops, reconnects) are one text line
   inside the Live card (`renderLive`).
4. **Warnings are 3.5 s toasts** (`toast()` in app.js). A reconnect, a recording error (`recordError`
   toasts again on every status tick) or a lowered bitrate disappear before a player looks over.
5. **The sidebar has 11 stacked cards** in one scroll (Source, Scenes, Clips, Banner, Live, Health, Phone,
   AI, Connection, Quality). Setup controls and live controls share equal weight.
6. **The preview is the phone picture, not the program.** You can't see the banner, the scene or the
   framing the viewers get.

---

## (a) Top 15 improvements, in priority order

| # | Change | Where | Why / source | Effort |
|---|---|---|---|---|
| 1 | **Phone-side live controls.** Add a persistent notification (Android MediaStyle-like actions: *BRB / Back to game*, *Mute mic*, *Clip 30 s*) and an optional small floating bubble showing LIVE time and a health dot. Commands go to the PC over the existing control channel. The player never has to alt-tab or reach for the laptop mid-match. | `CaptureService.kt` (notification), `StreamClient.kt`/`Protocol.kt` (new control msgs), Rust side handlers | Remote control is how serious streamers work: Streamlabs Controller app (https://streamlabs.com/mobile-app), Stream Deck integrations in Meld (https://meldstudio.co/blog/docs/integrations), and obs-websocket dashboards (https://github.com/obsproject/obs-websocket). OSSG's user *is* on the phone. | M |
| 2 | **Safe "End stream".** Use hold-to-end (about 1 s fill) or a confirm sheet: "End stream on YouTube + Twitch? 1:42:10 live". Offer "Switch to Thanks for watching and end in 15 s" as the default path. | `golive.js` liveBtn handler, `index.html` new mini-sheet | OBS has "Show confirmation dialog when stopping streams" (Settings → General). Streamlabs and PRISM confirm too. Accidental ends are the worst live failure. | S |
| 3 | **Persistent alert bar over the stage** in place of toasts for anything stream-affecting: `Reconnecting to Twitch (attempt 3)…`, `Bitrate lowered to 6 Mbps: network congested`, `Recording failed: disk full`, `Mic muted for 2 min`. Each alert has a severity colour, a timestamp, an action button (Retry / Lower quality / Open folder) and a dismiss control. Dedupe repeated messages. Keep toasts for confirmations only ("Clip saved"). | `index.html` (`.stage` alert slot), `app.js` new `alerts` module, replace `toast()` calls in `render()` / `renderLive()` | The YouTube Live Control Room keeps a persistent "Stream health" panel with typed issues (https://developers.google.com/youtube/v3/live/docs/liveStreams/health_status_messages). The OBS "Online Status" plugin shows dropped/recovered state on screen (https://obsproject.com/forum/resources/online-status-%E2%80%93-automatic-dropped-recovered-frames-overlay.2211/). | M |
| 4 | **OBS-style bottom status bar**, always visible: one chip per destination (platform colour, state dot, kbps, % dropped), plus encoder load (NVENC %, encode lag ms), phone link (USB/Wi-Fi, latency), CPU/GPU, disk space when recording, and a congestion square (green/yellow/red). Clicking a chip expands the per-destination details that are in the Live card today. | `index.html` new `<footer>`, `app.js renderLive` / `render`, `style.css` | The OBS status bar uses a congestion colour square plus dropped frames, kbps and CPU (https://github.com/obsproject/obs-studio, `window-basic-status-bar.cpp`). Every pro tool keeps health in peripheral vision. | M |
| 5 | **Split "Link health" from "Stream health".** Rename the current card *Phone link* (phone→PC fps/latency/jitter/drops). Make *Stream health* the outgoing side: per-platform bitrate sparkline (60 s), dropped %, reconnect count, and the live keyframe interval actually sent. Use one shared 3-state rule (good/warn/bad) for all colours. | `index.html` Stream health card, `app.js drawSpark` (multi-series), `renderLive` | YouTube and Twitch Inspector grade *ingest*, not capture (https://support.google.com/youtube/answer/3006768). Users read "Stream health" as "what viewers get". | S |
| 6 | **Big scene tiles with an ON-AIR state.** Make the 4 scene buttons 16:9 thumbnails (avatar clip still), with a red outline and "ON AIR" on the active one, the hotkey printed on the tile, and the privacy-cover state as a badge on the BRB tile ("auto"). Pin them directly under the preview (above the mixer). | `index.html` `#sceneRow`, `app.js renderScene`, `style.css` | Scene grids with thumbnails are standard in Twitch Studio, PRISM (https://guide.prismlive.com/desktop) and Meld's scene panel (https://meldstudio.co/blog/docs/layers). Text buttons are hard to hit and hard to read in peripheral vision. | S |
| 7 | **Two-mode sidebar: *Live* vs *Setup*.** Live mode (auto-selected when live): Scenes, Banner quick fields, Clips, Destinations, Chat/AI. Setup mode: Source, Quality, Connection, Phone, AI settings, Hotkeys. Use a segmented control at the top of the aside, collapsible cards, and remember the state per mode. | `index.html` `<aside>`, `app.js` | Twitch Studio and Streamlabs separate "Editor" and "Live" views. Meld uses dockable panels. This cuts scroll depth from 11 cards to about 5. | M |
| 8 | **Program preview.** Toggle *Phone* / *Program (what viewers see)*. Program renders the composited 16:9 output at about 10–15 fps from the compositor (or JPEG snapshots), so the user sees banner, framing and scene. When live, default to Program with a red "LIVE" frame. | `index.html` `.screen`, `app.js` preview, Rust compositor tap | This is OBS Studio Mode's Preview/Program split (https://obsproject.com/kb/studio-mode-guide). PRISM and Streamlabs always show the output canvas. Today the banner and framing are only visible in the Go Live editor canvas. | L |
| 9 | **One guided first-run flow.** Chain 1) Connect phone (USB auto / Wi-Fi QR), 2) Setup check (encoder, FFmpeg, upload), 3) Add platform + key, 4) a private test (Twitch bandwidth test / YouTube unlisted) with a pass/fail summary. Show a progress rail. It is re-runnable from Help. | New `onboarding` sheet in `index.html`; reuse `openSetup()` in golive.js | Twitch Studio starts with a test that picks settings, then checks devices (https://mac.softpedia.com/get/Multimedia/Twitch-Studio.shtml). The OBS Auto-Configuration Wizard is the same idea. OSSG has the pieces but scatters them across the empty state, the Go Live sheet and the Setup modal. | M |
| 10 | **QR pairing for Wi-Fi.** The desktop empty state shows a QR code (`ossg://pair?host=…&port=…&token=…`). The Android "Wi-Fi" option gets *Scan QR* next to auto-discover, and the phone then remembers the PC by name. The phone shows "Connected to <PC name> · Wi-Fi · 18 ms" with a quality dot. | `index.html` `#empty` steps, `MainActivity.kt` (`findPc`, wifiRow), discovery protocol | Android's own wireless-debugging pairing uses QR (https://developer.android.com/tools/adb#connect-to-a-device-over-wi-fi). Larix imports connection profiles by QR/link (https://softvelum.com/larix/faq/). Typing IPs is the #1 pairing failure for non-technical users. | M |
| 11 | **Pre-go-live checklist strip** in the Go Live sheet footer, as live checks rather than settings: phone connected and in game, current scene (suggest "Starting soon"), mic signal detected in last 10 s and not muted, game audio present, banner game label (with one-click chips from history: *BGMI LIVE*, *eFootball*), disk space for recording, and YouTube latency/auto-stop tips from the existing `yt-tips`. | `golive.js renderPlan`, `index.html` `.sheet-foot` | StreamYard's "backstage" and Restream Studio check camera/mic before Go Live. OSSG already validates encoding. This adds the *human* mistakes (muted mic, wrong game label). | S |
| 12 | **Show adaptive-bitrate actions.** When `engine/dbr.rs` lowers or restores bitrate, post an alert ("Network congested: Twitch 6.0 → 3.5 Mbps; restoring") and draw the target bitrate line on the sparkline. Same for the phone link "OSSG lowers it automatically" (Quality card). | `app.js` alerts + spark, status payload field | Larix exposes its ABR mode and behaviour explicitly (logarithmic descend / ladder ascend, https://softvelum.com/larix/faq/). OBS's dynamic bitrate is silent and confuses users (https://obsproject.com/forum/threads/dynamic-bitrate-breaks-the-automatically-reconnect-feature.116428). | S |
| 13 | **Banner editor as its own drawer, with game-label chips.** Move the banner editor out of the Go Live sheet into a side drawer with the live preview canvas. Add a saved list of game labels (chips plus hotkey cycle) so switching BGMI → eFootball is one tap, also from the phone notification (#1). Show "Live on stream in ~3 s" confirmation state on the chip. | `banner.js`, `golive.js showBannerEditor`, `index.html #quickBanner` | Streamlabs and PRISM edit overlays/widgets on the canvas, not inside the go-live dialog. Aitum Vertical keeps per-canvas layouts separate (https://github.com/Aitum/obs-vertical-canvas). | S |
| 14 | **Platform metadata in Go Live (title, category/game, notify).** Per-destination title and category fields set through the YouTube Data API / Twitch Helix, with "same for all" linking. Pair with the game label, so switching the label can offer "Also update Twitch category to eFootball?". | `golive.js card()`, new Rust OAuth commands | Streamlabs Go Live and PRISM let you set title/game per platform before starting (https://streamlabs.com/content-hub/post/streamlabs-desktop-dual-output-how-to). It saves a trip to 3 dashboards per stream. | L |
| 15 | **Clip and recording feedback that lasts.** After a clip, show a small thumbnail card ("Clip saved · 30 s · Open · Show in folder") stacked bottom-left for 8 s, plus a "Recent clips" list (last 10) in the Clips card. Also fix the recording-error toast that repeats every status tick. | `app.js` clip handlers, `renderReplay`, `render` (recordError) | NVIDIA ShadowPlay / Medal / Streamlabs highlight toasts give a visual confirmation and a fast path to share. Today a clip is only a 3.5 s text toast. | S |

Smaller fixes worth doing alongside:
- **Live quality switch warning per platform.** Facebook explicitly says settings should not change
  mid-broadcast (see table). Show a "Heads-up" on Facebook destinations before `apply_live_now`.
- **Android app status card while streaming:** PC name, link, phone→PC Mbps, temperature/thermal, and a
  big red Stop. Lock the form fields while running. Show a heat warning at the same thresholds the desktop
  uses (40/45 °C).
- **Header pill:** make it one combined state ("Phone: USB · 60 fps" / "LIVE 1:02:33 · 2 platforms") and
  let it open the status bar details.

---

## (b) Platform ingest table (October 2026)

Official sources are marked (O). Sources marked (3P) are third-party or relay services; verify those before
hard-coding them.

| | YouTube | Twitch | Kick | Facebook |
|---|---|---|---|---|
| Max res / fps | 2160p60 (4K). Vertical via separate key (O) | 1080p60 standard H.264. 1440p60 with Enhanced Broadcasting + HEVC for Partners/Affiliates (3P, 2025–26 rollout) | 1080p60 (3P) | 1080p60 (O, dev docs) |
| Bitrate, 1080p60 | H.264 rec 17 Mbps (min 6). HEVC/AV1 rec 12 (min 4) (O) | 6,000 kbps typical cap for H.264. EB: about 7.5 Mbps 1080p / 9 Mbps 1440p HEVC (3P) | 1,000–8,000 kbps, 6–7k sweet spot (3P) | 4,500–9,000 kbps (O) |
| Bitrate, 1080p30 | H.264 rec 14 Mbps (min 5). HEVC/AV1 rec 10 (min 4) (O) | 4,500 kbps (3P) | 4,500–6,000 (3P) | 3,000–6,000 kbps (O) |
| Bitrate, 720p60 | H.264 rec 8 Mbps (min 3). HEVC/AV1 rec 6 (min 2) (O) | 4,500 kbps (3P) | 3,000–4,500 (3P) | ≤ 4,500 (O) |
| 1440p60 / 4K60 | 1440p60 H.264 rec 34 (min 8), HEVC/AV1 24 (min 6). 4K60 H.264 rec 50 (min 14), HEVC/AV1 35 (min 10) (O) | 1440p only via EB (HEVC) | not supported | not supported |
| Keyframe interval | 2 s recommended, ≤ 4 s required (O) | 2 s (O guideline) | 2 s (3P) | 2 s recommended, never > 4 s (O) |
| Rate control | CBR (O) | CBR | CBR only, no VBR (3P) | CBR |
| Video codecs | H.264, HEVC, AV1. HDR needs HEVC (O) | H.264. HEVC/AV1 only via Enhanced Broadcasting multitrack (O blog) | H.264 only, no HEVC (3P) | H.264 (Level 4.2 for 1080p60) (O) |
| Other video | Progressive, 2 B-frames, closed GOP, Rec.709 8-bit SDR, square pixels (O) | High profile, B-frames ≤ 2 | High profile | No setting changes mid-stream; max 8 h per live (O) |
| Audio | AAC/MP3, 44.1/48 kHz, 128 kbps stereo (384 kbps 5.1) (O) | AAC 48 kHz, ≤ 160 kbps | AAC 128–160 kbps | AAC 48 kHz, 128 kbps preferred, ≤ 256 kbps (O) |
| Protocols | RTMP, RTMPS (recommended), HLS, SRT (O) | RTMP / RTMPS, Enhanced RTMP for EB | RTMPS (IVS ingest `*.global-contribute.live-video.net`) | RTMPS only (O) |

Sources: YouTube encoder settings https://support.google.com/youtube/answer/2853702 . YouTube errors
https://support.google.com/youtube/answer/3006768 . Twitch guidelines
https://help.twitch.tv/s/article/broadcasting-guidelines and EB https://blog.twitch.tv/2024/01/08/introducing-the-enhanced-broadcasting-beta/ .
Twitch 2K https://onestream.live/blog/twitch-dual-format-2k-streaming-setup/ (3P). Kick
https://support.restream.io/en/articles/7183157-stream-to-kick (3P). Facebook
https://developers.facebook.com/docs/live-video-api/reference .

**Settings that trigger dashboard warnings** (YouTube API issue IDs from the health-status docs, mapped
to their causes):

- `gopSizeLong` / `gopSizeOver`: keyframe interval > 4 s (YouTube says "set keyframe frequency to four
  seconds or less"). Also `gopSizeShort`: keyframes very frequent (for example `-g 1`/all-intra), which
  hurts quality. Twitch Inspector and Facebook flag anything that isn't 2 s or drifts from it. Use fixed GOP:
  `-g fps*2 -keyint_min fps*2`, no scene-cut keyframes, as `live.rs` already does.
- `openGop`: open GOP is not supported. NVENC/x264 must use closed GOP.
- `bitrateLow`: under the per-resolution *min* above (for example < 6 Mbps H.264 1080p60, < 4 Mbps HEVC). This often fires
  when DBR drops the bitrate or on a static BRB scene with VBR. Keep CBR with filler / `-b:v = -maxrate`.
- `bitrateHigh`: above recommended for that resolution. Twitch: > 6,000 kbps non-EB gives some viewers
  buffering. Kick/IVS: > 8,000–8,500 kbps can be rejected.
- `videoIngestionStarved` ("Video output low"): YouTube receives less than real-time. Causes are upload
  congestion or encoder overload. This is the same moment OSSG should raise "Reconnecting / degraded".
- `frameRateHigh`: > 60 fps.
- `videoResolutionSuboptimal` / `Unsupported`: non-standard sizes (for example the 2376×1080 "Phone shape").
  YouTube accepts it with a note; Twitch/Kick/Facebook should get 16:9, as golive.js already routes.
- `audioBitrateLow/High`, `audioSampleRate` (not 44.1/48 kHz), `audioTooManyChannels` (> 2), `noAudioStream`.
  A BRB scene with music removed must still send a silent AAC track.
- Backup-stream mismatches (`resolutionMismatch`, `gopMismatch`, `videoBitrateMismatch`, …): when "Send to
  YouTube's backup server" is on, primary and backup must be byte-identical encodes (same encoder output, tee'd).
- Facebook: changing resolution/bitrate mid-broadcast (OSSG's live quality switch) can end or degrade the
  broadcast. Limit the switch to the same resolution, or warn.

---

## (c) Reliability and health UX patterns worth copying

1. **Three-state health everywhere** (Excellent/Good/Poor or green/yellow/red), defined from dropped-frame %
   and bitrate vs. target. Never show raw numbers without a state. (OBS status bar congestion square;
   Upstream/LiveU dashboards.)
2. **Reconnect as a visible, counted state.** Show "Reconnecting to Twitch… attempt 3, next try in 4 s" with a
   *Retry now* button. Keep sending to other destinations. On success, post "Back live on Twitch (offline 11 s)".
   OBS exposes retry delay/max retries; Restream shows per-destination states.
3. **Degrade before failing.** Lower the non-Main destinations first (OSSG's star already does this), then
   fps, then resolution, and *say so* in the alert bar. (Larix ABR modes; OBS dynamic bitrate.)
4. **Preflight = machine checks + human checks.** Keep encoder/upload/preset validation, and add mic signal,
   muted state, scene, game label and disk space (StreamYard backstage; Twitch Studio first-run test).
5. **Private test path.** One-click "Test stream" (Twitch bandwidth test `?bandwidthtest=true`, YouTube
   unlisted) that reports ingest bitrate and keyframe interval back. Twitch Inspector does this server-side.
6. **Silence and black detectors while live.** Alert after 30 s with mic muted or game audio at
   -∞ dB, or a frozen/black frame (except during scenes).
7. **Incident timeline.** A small log ("19:02 Twitch reconnected · 19:15 bitrate lowered · 19:40 phone
   overheated (45 °C)") saved with the session, linked from "Open diagnostics". It makes post-stream review
   and bug reports easy (Twitch Inspector keeps about 7 days of sessions).
8. **Persistent vs transient messaging.** Anything affecting viewers persists until resolved. Confirmations
   auto-dismiss. Never re-toast the same error every tick.
9. **Protect irreversible actions.** Use hold-to-end for End stream, and confirm for removing a live
   destination. Make Stop on the phone notification stop *capture* only, with a PC-side prompt (the
   privacy cover keeps the stream alive).
10. **Thermal/battery as health inputs.** Treat phone heat as a first-class warning: suggest 30 fps or
    lower phone bitrate at the "warn" level. Mobile tools (Larix, Streamlabs mobile) surface device
    temperature for this reason.

---

## Visual design notes (dark gaming dashboard)

- **Palette:** keep the PhantomBlitz tokens (`--bg #070a14`, violet/cyan accents, red reserved for LIVE).
  Reserve **red only for on-air/danger**, amber for degraded, green for healthy. Platform colours appear
  only as small dots/strips, never as button fills.
- **Hierarchy:** stage plus scene tiles take about 65% width. Live-critical numbers use tabular figures
  (`font-variant-numeric: tabular-nums`) at 20–28 px. Labels are 11–12 px uppercase with +0.06em tracking.
  Body text is 13–14 px. Use a single 4 px spacing scale (already defined: `--s-1..--s-8`).
- **Density:** cards at 12–16 px padding. Group live controls in one region so the eye has a single place
  to check (OBS docks / Meld panels).
- **Motion (UI):** 120–160 ms press feedback, 200–250 ms panels with ease-out (the `--ease-out` already
  matches). No looping animation in the dashboard except the LIVE dot pulse (≥ 1.5 s period). Respect
  `prefers-reduced-motion`.
- **Motion (on-stream overlays):** lower-thirds/banner changes 300–500 ms in, 200–300 ms out, ease-out in /
  ease-in out. The "Back to game" action-clip should not exceed about 1.5 s. Keep avatar loops seamless and
  out of the game's HUD safe areas (minimap, fire buttons). Keep the banner text change as a cross-fade or
  slide, not a hard cut. Avoid constant motion in the banner strip, which pulls attention from gameplay;
  use a subtle shimmer every 20–30 s at most.
