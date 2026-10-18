fn main() {
    #[cfg(windows)]
    delay_load_ffmpeg();
    tauri_build::build()
}

/// With feature `libav`, FFmpeg's DLLs are delay-loaded: the app still
/// starts without them (and falls back to the FFmpeg CLI) because nothing
/// is loaded until `engine::libav::available()` has found them all.
#[cfg(windows)]
fn delay_load_ffmpeg() {
    println!("cargo:rerun-if-env-changed=FFMPEG_DIR");
    if std::env::var_os("CARGO_FEATURE_LIBAV").is_none() {
        return;
    }
    let dir = std::env::var("FFMPEG_DIR")
        .expect("feature libav needs FFMPEG_DIR (FFmpeg 9 shared build)");
    let mut dlls: Vec<String> = std::fs::read_dir(std::path::Path::new(&dir).join("bin"))
        .expect("FFMPEG_DIR/bin")
        .filter_map(|e| e.ok()?.file_name().into_string().ok())
        .filter(|n| {
            n.ends_with(".dll")
                && [
                    "avcodec-",
                    "avfilter-",
                    "avutil-",
                    "avformat-",
                    "swscale-",
                    "swresample-",
                    "postproc-",
                    "avdevice-",
                ]
                .iter()
                .any(|p| n.starts_with(p))
        })
        .collect();
    dlls.sort();
    for d in &dlls {
        println!("cargo:rustc-link-arg-bins=/DELAYLOAD:{d}");
    }
    println!("cargo:rustc-link-arg-bins=delayimp.lib");
    println!("cargo:rustc-env=PHONECAST_FFMPEG_DLLS={}", dlls.join(";"));
}
