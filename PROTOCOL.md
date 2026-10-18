# PhoneCast wire protocol (v1)

One TCP connection, phone = client, PC = server on port **27183**.
- USB: PC runs `adb reverse tcp:27183 tcp:27183`; phone connects to `127.0.0.1:27183`.
- Wi-Fi: phone connects to `<PC LAN IP>:27183`.

All integers are big-endian.

## Handshake (phone → PC, once)
```
"PCST" (4 bytes) | u8 version = 1 | u16 nameLen | name (UTF-8)
```

## Packet (both directions)
```
u8 type | u8 flags | i64 ptsUs | u32 len | payload[len]
```
`ptsUs` is the phone's monotonic clock (`System.nanoTime() / 1000`) for media.

### Phone → PC
| type | name | payload |
|---|---|---|
| 0x01 | VIDEO_CONFIG | u8 codec (1=H.264, 2=H.265) · u16 width · u16 height · codec config (Annex-B SPS/PPS[/VPS]) |
| 0x02 | VIDEO_FRAME | Annex-B access unit. flags bit0 = keyframe |
| 0x03 | AUDIO_CONFIG | u32 sampleRate · u8 channels · u8 format (1 = s16le) |
| 0x04 | AUDIO_PCM | interleaved PCM |
| 0x05 | STATS | UTF-8 JSON (battery, temperature, thermal status, fps, bitrate…) |
| 0x06 | PONG | i64 pcTimeUs (echoed) · i64 phoneTimeUs |
| 0x07 | MIC_PCM | phone mic PCM (48 kHz s16le mono) |
| 0x08 | FOREGROUND_APP | UTF-8 JSON `{pkg, label, game}`: the app now on screen (privacy cover) |

### PC → phone
| type | name | payload |
|---|---|---|
| 0x81 | SET_BITRATE | u32 bits per second |
| 0x82 | REQUEST_KEYFRAME | — |
| 0x83 | PING | i64 pcTimeUs |
| 0x84 | STOP | — |
| 0x85 | SET_MIC | u8 1 = send the phone mic |
| 0x86 | SET_PRIVACY | u8 1 = send no picture while a non-game app is on screen |

## Wi-Fi discovery (UDP 27185)
The phone broadcasts `PCST-DISCOVER` to 255.255.255.255:27185. Every
PhoneCast PC replies `PCST-HERE <computer name>` from its own address; the
phone lists the replies so the user never types an IP. Plain UDP, no mDNS
dependency, works on home routers that block multicast.

## Clock sync & latency
PC sends PING every second. With `t0` = PC send time, `t1` = PC receive time,
`p` = phone time in PONG: `offset = p − (t0 + t1) / 2`.
Glass-to-PC latency of a frame = `arrivalPc − (ptsUs − offset)`.

## Congestion control (phone side)
Video goes through a bounded send queue. If more than ~100 ms of video is
waiting, the phone drops queued non-key frames, asks the encoder for a
keyframe and lowers bitrate by 25%. After 3 s without congestion it raises
bitrate by 10% up to the target. Audio is never dropped.
