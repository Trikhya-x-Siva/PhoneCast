# Security

## Reporting
If you find a security problem, please do not open a public issue. Use
GitHub's private "Report a vulnerability" button on the Security tab, or
email the maintainer listed in the repository profile. You will get a reply
within a week.

## What PhoneCast does with your data
- **Stream keys** are encrypted with Windows DPAPI (tied to your Windows user
  account) in `%APPDATA%\PhoneCast\live.json`. They are sent only to the
  RTMPS endpoints of YouTube and Twitch.
- **No telemetry, no accounts, no cloud.** The app makes network connections
  only to: your phone (USB/LAN), the streaming platforms you enable, and
  (when you ask it to) the FFmpeg download server.
- **Diagnostics logs** contain timings and error messages, never keys or
  audio/video content.
- The phone app needs screen-capture and audio-capture permissions only
  while streaming and shows a persistent notification the whole time.
