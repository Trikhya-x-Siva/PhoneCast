# Contributing to PhoneCast

Thanks for helping. PhoneCast is small on purpose; the bar for a change is
"does it keep streams smooth and the app light?"

## Reporting a problem
Open an issue with:
- Windows build (`winver`) and GPU; phone model and Android version
- What you expected and what happened
- The diagnostics log: PhoneCast → **Open diagnostics** (or
  `%APPDATA%\PhoneCast\logs`). It has no personal data or stream keys.

## Building
- **PC app:** Rust (stable), Node LTS, Visual Studio Build Tools (C++).
  `cd desktop && npm install && pwsh ./fetch-adb.ps1 && npx tauri dev`
- **Phone app:** JDK 17 and the Android SDK. `android\build-and-install.bat`
  builds a release APK and installs it on the USB-connected phone.
- **Tests:** `cargo test` in `desktop/src-tauri`. Some tests are `#[ignore]`
  because they need a sample stream or a running app; see their comments.

## Pull requests
- One change per PR, with a short description of *why*.
- Keep the GPU-only video path: no CPU frame copies in capture → encode.
- Anything that can block (disk, network, decoding) runs on its own thread
  behind a bounded queue. Never add a blocking call to the phone reader or
  the mixer.
- UI text is plain language; errors say what to do next.
- Run `cargo fmt` and `cargo clippy`.

## Code of conduct
Be kind and constructive. Harassment of any kind is not tolerated.
