# Streaming engine v2

**Goal:** a live stream never restarts. A bitrate change happens on the
running encoder, and after a network drop only the socket reconnects. Viewers
see a short freeze at worst, never "stream ended".

The code is in `desktop/src-tauri/src/engine/`. It's additive: `live.rs`
still runs today's path. `core.rs` switches over once the integration branch
wires it in (see [API](#api-for-corers)).

## Why v1 restarts

`live.rs` runs one FFmpeg CLI process per destination (`Job::run`), fed live
MKV over stdin. Everything restarts the process. FFmpeg exits when the network
drops (reconnect = new process, new encoder, 2–20 s gap). A bitrate change
can't be made without a new process. A phone rotation restarts it too. Two
platforms on the same preset also decode, scale and encode the same picture
twice.

The research behind v2 (OBS `rtmp-stream.c`, belacoder, Moblin, FFmpeg
`nvenc.c`) agrees: robust tools keep the encoder running. They change bitrate
with `nvEncReconfigureEncoder` and reconnect only the socket.

## Design

```
phone Media ─► Source ─► encoder per (codec, size, fps, kbps) ─► Stream ─► SendQueue ─► Sender ─► platform
               │          shared by destinations on one preset     fan-out    per dest,     RTMP(S),
               │                                                              DTS-measured  socket-only
               │                                                                            reconnect
               └──◄ set_bitrate / request_idr ◄── Engine control tick (50 ms): Arbiter (DBR) ◄── backlog + throughput
```

| Module | What it does |
|---|---|
| `engine/mod.rs` | `Engine`: opens encoders (one per distinct spec), one `Sender` per destination, the 50 ms control tick, status. Moves a destination to another encoder (split or merge) at that encoder's next IDR, without reconnecting. |
| `engine/encoder.rs` | `Source` / `VideoEncoder` traits, `Stream` fan-out, `BitrateGate` (rate-limits live bitrate changes, keeping the latest value), `CliSource` (FFmpeg CLI fallback). |
| `engine/libav.rs` | Feature `libav`: in-process NVDEC → CUDA framing graph → NVENC, plus AAC, on one pipeline thread. |
| `engine/rtmp.rs` | RTMP/RTMPS publisher: handshake, chunking, `connect`/`createStream`/`publish`, TLS with SNI (rustls + ring, Mozilla roots), `SendQueue` measured in time, reconnect loop, Windows `SIO_TCP_INFO` stats, `SO_SNDBUF` set to the OS ideal send backlog (the same trick as OBS). |
| `engine/dbr.rs` | OBS-style dynamic bitrate per destination and the main-platform rule across destinations (`Arbiter`), plus the frame-drop thresholds. |
| `engine/flv.rs` | FLV/RTMP payloads: H.264 (classic), HEVC (enhanced RTMP `hvc1`, `fourCcList` in `connect`), AAC. Annex-B → length-prefixed conversion with avcC/hvcC built from in-band parameter sets. FLV reader for the CLI fallback. |
| `engine/amf.rs` | AMF0 subset. |
| `engine/fake_server.rs`, `engine/tests.rs` | Pure-Rust RTMP ingest that can throttle, stall and drop connections, plus the engine tests. |

### Encoders never restart

- **Bitrate:** `VideoEncoder::set_bitrate(kbps)` goes through a `BitrateGate`:
  at most one change per second, and the latest request wins. The libav
  backend writes `bit_rate` / `rc_max_rate` / `rc_buffer_size` into the open
  `AVCodecContext` before the next `avcodec_send_frame`.
  libavcodec's `nvenc.c reconfig_encoder()` then calls
  `nvEncReconfigureEncoder`, which forces an IDR. We also mark that frame
  `AV_PICTURE_TYPE_I` (with `forced-idr=1`) so the IDR is certain.
- **Shared encoders:** destinations with the same (codec, size, fps, framing,
  kbps) share one encoder. If DBR wants a different bitrate for one of them,
  the engine doesn't touch the shared encoder:
  - It moves that destination to an encoder already running at that bitrate,
    or opens one (at most `max_encoders`, default 4; GeForce cards allow 8
    NVENC sessions).
  - The switch happens on the new encoder's first IDR, on the same
    connection.
  - When the targets match again, the destination merges back and the spare
    encoder closes.
- **Rotation:** the libav pipeline rebuilds only the decoder and filter
  graphs. Output size and encoders stay as they are.
- **IDR on demand:** `request_idr()` is called after every (re)connect and
  whenever a queue had to skip ahead.

### Sender, queue and reconnect

- Each destination has its own `SendQueue`. Its length is the DTS span of
  queued media in ms, not a packet count. One slow platform never delays
  another.
- **Reconnect:** a write blocked for `stall_timeout` (5 s), a closed socket or
  a server error ends the connection. Only the socket is reconnected:
  - first retry after 0.5 s, then 2 s, 3 s, 4.5 s, … up to 30 s (OBS);
  - the queue is cleared and holds everything back until a keyframe;
  - the encoder is asked for an IDR, so the new connection starts with the
    sequence header, then an IDR, then audio;
  - timestamps restart at 0 for the new publish.
- **Safety cap:** a queue more than 10 s behind (link alive but useless)
  skips to the next keyframe and asks for an IDR.
- **TCP stats:** RTT, min RTT, bytes in flight, cwnd, retransmits and ideal
  send backlog are polled every 200 ms and shown in the status. DBR doesn't
  use them yet (see risks).

### Dynamic bitrate (`dbr.rs`)

Per destination, as in OBS:

- **Cut:** when the queue's DTS span is at least 200 ms, cut to the bytes
  actually sent over the last 1.5 s, minus audio, rounded down to 100 kbps.
  - Floor: 20 % of the original, and never below 500 kbps.
  - At most one cut per second per destination.
- **Recover:** after 4 s without congestion, add 10 % of the original every
  4 s, never above the original.
- **Frame drops (always on, as a fallback):**
  - backlog ≥ 700 ms: queued B-frames are dropped, and new ones until the
    next P- or I-frame;
  - backlog ≥ 900 ms: every non-keyframe is dropped until the next keyframe;
  - audio and keyframes are never dropped.

Across destinations (the user's rule): the **main** destination keeps its
quality.

- **Main congests:** secondaries are lowered first, highest first, by the
  main destination's deficit. The main destination is cut only when every
  secondary is at its floor and a 2 s grace period has passed.
- **Recovery:** secondaries don't climb back until the main destination is
  back at full bitrate.
- **One main:** if no destination is flagged main, the first one is. If
  several are, only the first counts.

## What it replaces in `live.rs`

| `live.rs` today | Engine v2 |
|---|---|
| `Job`, `Job::run`, `spawn` (FFmpeg process per destination, restart loop with 2–20 s waits) | `Engine::start` + `rtmp::Sender` (socket-only reconnect) |
| `Job::args` / `output_args` (FLV muxing by FFmpeg) | `flv.rs` + `rtmp.rs` |
| `encoder_args` (NVENC options) | Same options in `libav.rs` `open_encoder`. `CliSource` calls `encoder_args` directly (now `pub(crate)`). |
| `video_filter` (CUDA blur/fit/fill graph) | Used as is: `libav.rs` parses the same graph text with `avfilter_graph_parse_ptr` (pads renamed `[in]`/`[out]`, `hwdownload` added for Fill). `CliSource` passes it to the CLI. Now `pub(crate)`. |
| `watch_progress` (parse `-progress`) | `Engine::status()` / `status_json()` (`DestStatus` with target/sent kbps, backlog, drops, encoder stats, TCP stats) |
| `Live::push` → `SinkTx` per destination | `Engine::push` → `Source::push` (one decode for all destinations) |
| `probe_encoders` | `libav::available()` (DLLs present + `h264_nvenc` + `scale_cuda`); `CliSource` still uses the probed names |

These stay in `live.rs`: settings and DPAPI keys, presets (moving to
`presets.rs` on the `presets` branch), the PC-mode file recorder, and PC
screen mode. PC mode can feed the same encoders later: the window-capture
compositor already produces NV12 on the GPU. That's a `Source` that skips
the decoder.

## API for core.rs

```rust
use crate::engine::{self, DestConfig, Engine, EngineOptions};

// Go live (phone mode). `config` = the phone's VideoConfig,
// `rk` = Arc<dyn Fn() + Send + Sync> calling request_keyframe_limited().
// libav/NVENC when available, else the FFmpeg CLI (live.rs's probed encoders).
let source = engine::source_for(config, live.ffmpeg(), (enc.h264, enc.hevc), rk)?;
let dests: Vec<DestConfig> = /* from presets.rs: id, name, url (rtmps://…/app/key),
                                codec, w, h, fps, kbps, framing, main */;
let engine: Arc<Engine> = Engine::start(source, dests, EngineOptions::default())?;

// Core::dispatch(): replaces self.live.push(&m)
engine.push(&m);

// Status poll (UI, once a second): replaces live.status_json()
engine.status_json();

// Stop
engine.stop();
```

`Engine::push` never blocks. The libav source asks the phone for a keyframe
when its input queue overflows, and when a new encoder opens. The CLI source
does the same through `SinkTx`.

## FFmpeg libraries: binding, CI, installer

**Binding: `ffmpeg-sys-next` 9.x** (the FFI layer of `ffmpeg-next`), behind
cargo feature `libav`.

- **`rsmpeg`** is the nicer API, but its newest release (0.18) targets
  FFmpeg 8.0, and `rusty_ffmpeg` 0.17 targets 8.1. There's no FFmpeg 9
  support yet.
- **`ffmpeg-next` 9 (safe API)** has no wrappers for what this pipeline
  needs: hardware device and frames contexts, `AVBufferSrcParameters`,
  `get_format`, and live `bit_rate` changes on an open context. We'd drop to
  its `ffi` module anyway.

So `libav.rs` uses the raw FFI with all objects owned by one thread.

**Build requirements** (only with `--features libav`):

- `FFMPEG_DIR`: an FFmpeg 9 *shared* dev build with `include/`, `lib/*.lib`
  and `bin/*.dll`, e.g. BtbN `ffmpeg-n9.0-latest-win64-lgpl-shared-9.0.zip`.
  The winget/gyan "full_build" on this PC is static and has no headers.
- `LIBCLANG_PATH`, for bindgen. GitHub `windows-latest` ships LLVM in
  `C:\Program Files\LLVM\bin`.

`build.rs` adds `/DELAYLOAD` for every FFmpeg DLL in `FFMPEG_DIR/bin`, plus
`delayimp.lib`. It also bakes the DLL names into `PHONECAST_FFMPEG_DLLS`.
`libav::available()` calls `LoadLibraryW` on each one before any FFmpeg
function runs. That way the exe still starts without the DLLs and falls back
to the CLI instead of failing to launch.

**CI** (new job, proposed):

```yaml
  desktop-libav:
    runs-on: windows-latest
    steps:
      - uses: actions/checkout@v4
      - uses: dtolnay/rust-toolchain@stable
        with: { components: clippy }
      - name: FFmpeg 9 shared SDK
        shell: pwsh
        run: |
          $u = "https://github.com/BtbN/FFmpeg-Builds/releases/download/latest/ffmpeg-n9.0-latest-win64-lgpl-shared-9.0.zip"
          Invoke-WebRequest $u -OutFile ff.zip; Expand-Archive ff.zip -DestinationPath $env:RUNNER_TEMP
          "FFMPEG_DIR=$((Get-ChildItem $env:RUNNER_TEMP -Filter 'ffmpeg-n9*' -Directory)[0].FullName)" >> $env:GITHUB_ENV
          "LIBCLANG_PATH=C:\Program Files\LLVM\bin" >> $env:GITHUB_ENV
      - run: ./desktop/fetch-adb.ps1
        shell: pwsh
      - working-directory: desktop/src-tauri
        run: cargo clippy --features libav -- -D warnings
      - working-directory: desktop/src-tauri
        run: cargo test --features libav   # GPU tests are #[ignore]
```

**Installer:** `release.yml` builds with `--features libav` and the same SDK
step. `tauri.release.json` maps `FFMPEG_DIR/bin/*.dll` next to
`phonecast.exe`: avcodec, avfilter, avutil, swscale and swresample, plus
avformat because avfilter links it. That's about 60–80 MB for an LGPL
build. If they're missing (portable copy, antivirus), the app uses the
FFmpeg CLI found by `live::find_ffmpeg()`.

## FFmpeg CLI fallback

`CliSource` runs one FFmpeg process per *encoder*, not per destination. It
uses today's arguments (`video_filter` + `encoder_args`) with
`-f flv pipe:1`, and parses the FLV back into packets.

- **Same as libav:** per-destination queues with their own frame dropping,
  socket-only reconnects (the FFmpeg process keeps running through a network
  drop), shared encoders for shared presets. Main-platform priority has
  nothing to act on, because no bitrate can be lowered.
- **Different:** bitrate is fixed for the session
  (`live_bitrate() == false`), so DBR can only drop frames.
  `request_idr()` can't reach the process, so a reconnect resumes at the next
  natural keyframe (≤ 2 s GOP).

## Tests

`cargo test engine` runs everything without a GPU or network. A fake encoder
produces H.264-shaped packets in real time against the fake ingest.

| Test | Proves |
|---|---|
| `bitrate_change_never_restarts_encoder` | Throttled to 1.5 Mbps, a 4 Mbps stream is cut to ≤ 1.5 Mbps by live reconfigure: one encoder session, an IDR per change, one connection, video still flowing afterwards. |
| `reconnect_within_2s_resumes_on_idr` | After the server drops the connection, video is back in < 2 s on the same encoder. The new connection starts with the sequence header and a keyframe, has no audio before it, and its timestamps restart near 0. |
| `stalled_upload_reconnects_without_encoder_restart` | A silent link (no RST) is detected by the write timeout and replaced. Frames are dropped meanwhile and the encoder isn't restarted. |
| `shared_encoder_splits_for_secondary_and_main_keeps_quality` | Two destinations on one preset share one encoder. When the secondary congests it moves to its own encoder without reconnecting, and the main encoder is never reconfigured. |
| `dbr::steps_follow_obs_timing` | 200 ms trigger, cut to throughput minus audio, 1 s hold, +10 % at 4 s, 8 s, …, capped at the original. |
| `dbr::main_destination_keeps_quality` | Secondaries absorb the main destination's deficit, the main one is cut only after they hit the floor, and the main one recovers first. |
| `queue_drops_never_touch_audio_or_keyframes` | The 700 / 900 ms drop rules. |
| `rtmp::*`, `flv::*`, `amf::*` | Chunking with extended timestamps (parsed byte by byte), platform URLs, backoff schedule, avcC/hvcC, enhanced-RTMP tags, AAC config, FLV reader. |

## Cost at 1440p60 (RTX 4050 laptop, i5-13450HX)

Input: a real 2376×1080 H.264 phone recording, read in real time
(`-readrate 1`, 20 s). Blur framing on CUDA, NVENC p5/hq/CBR with
lookahead 20 and 2 B-frames, as in `encoder_args`. "v1" is today's shape:
one FFmpeg process per destination. "v2" is the engine's shape: one
decode, then shared or split graphs and encoders in one process. The
libav backend does the same GPU work in-process. Two runs each; CPU is the
total over all processes (16 logical cores).

| Scenario | CPU (s per 20 s) | NVENC | NVDEC | GPU 3D |
|---|---|---|---|---|
| v1, 1 destination 1440p60 | 1.8–3.3 | 47–48 % | 7 % | 4–5 % |
| v1, 2 destinations on the same 1440p60 preset | 5.2–5.4 | 67–68 % | 13 % | 9–10 % |
| **v2, same 2 destinations (shared encoder)** | **1.5–1.8** | **44–47 %** | **7–10 %** | **5 %** |
| v1, 1440p60 + 1080p60 | 3.5–7.0 | 50–56 % | 11 % | 7–8 % |
| **v2, 1440p60 + 1080p60 (one decode)** | **2.4–3.2** | **49–54 %** | **5–6 %** | **7–8 %** |

What this means:

- CPU is small either way (under 2.2 % of the PC).
- The real saving is GPU headroom for a second destination. A shared preset
  costs one encoder session instead of two (NVENC −35 %), and the phone
  stream is decoded once (NVDEC about halved).
- Not measured in v1: piping MKV into each process and encoding AAC once per
  process. v2 does both once.
- v2 adds rustls encryption in-process. That isn't measured yet; AES-GCM at
  20 Mbps should cost well under 5 % of one core.

## Open risks

- **`libav.rs` has never been compiled or run here.** This PC has no FFmpeg
  shared SDK and no libclang. Everything else is compiled, linted and tested.
  First step on a machine with the SDK: `cargo clippy --features libav`, then
  a 1440p60 run against the fake server.
- **NVENC reconfigure:** `support_dyn_bitrate` must be reported by the
  driver. If the reconfigure is refused, libavcodec logs it and keeps the old
  bitrate; frame drops still protect the stream. Check that the reconfigure
  works with `rc-lookahead=20`. If not, lookahead may need to go when DBR is
  on.
- **Licensing:** `boxblur` and `eq` in the blur graph may be GPL-only in
  FFmpeg. In that case an LGPL DLL build lacks them, and we either ship GPL
  DLLs (Apache-2.0 app + GPL DLLs needs a decision) or replace the tiny blur
  with `gblur` or our own CPU blur of the 384×216 copy.
- **First cut overshoots:** the throughput window also counts bytes that
  only filled socket buffers. The second cut, a second later, converges (seen
  in the tests). OBS behaves the same.
- **No headroom:** the cut lands exactly at measured throughput minus audio,
  so an existing backlog drains slowly. Frame drops clear it. belacoder
  aims ~10 % lower; worth trying on real Wi-Fi.
- **TCP stats aren't used by DBR yet:** RTT inflation (`RttUs` ≫
  `MinRttUs`) could trigger earlier than the 200 ms queue rule. Needs real
  Wi-Fi traces first.
- **RTMPS:** tested only over plain RTMP (fake server). TLS uses rustls with
  SNI and Mozilla roots. YouTube, Twitch, Kick and Facebook all present
  public CAs, but that hasn't been verified against the live endpoints.
- **Not built yet:** the HLS ingest and backup RTMPS URL (YouTube), and SRT
  output. The `Sender` is the place to add them; the queue and DBR are
  transport-agnostic.
