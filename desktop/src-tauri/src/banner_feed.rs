//! Feeds the banner strip to the live encoders over a local TCP socket, so
//! it can change while live: banner on/off, a new name or game label, a new
//! design. FFmpeg reads one endless H.264 stream; the app switches what it
//! writes at the next frame (each banner starts with a keyframe and the
//! same size), so the stream never restarts.
//!
//! Sources: `loop.h264` of the current banner version (on), or
//! `plain.h264` (off: a clean strip with no banner).

use std::io::Write;
use std::net::TcpListener;
use std::path::PathBuf;
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::time::{Duration, Instant};

/// The banner streams' frame rate.
const FEED_FPS: u64 = 30;
/// How far ahead of real time the feed writes.
const LEAD: Duration = Duration::from_millis(250);

/// Banner shown (true) or the plain strip (false).
static ON: AtomicBool = AtomicBool::new(true);
/// Bumped when what should be shown changes (toggle, new banner saved).
static GEN: AtomicU64 = AtomicU64::new(0);

pub fn set_on(on: bool) {
    ON.store(on, Ordering::SeqCst);
    changed();
}

pub fn is_on() -> bool {
    ON.load(Ordering::SeqCst)
}

/// A new banner (or plain strip) was saved: feeds switch to it.
pub fn changed() {
    GEN.fetch_add(1, Ordering::SeqCst);
}

pub fn plain_path() -> PathBuf {
    crate::live::banner_frames_root().join("plain.h264")
}

fn loop_path() -> Option<PathBuf> {
    crate::live::banner_frames_dir()
        .map(|d| d.join("loop.h264"))
        .filter(|p| p.exists())
}

/// What a feed writes now.
fn source() -> Option<PathBuf> {
    let plain = Some(plain_path()).filter(|p| p.exists());
    if is_on() {
        loop_path().or(plain)
    } else {
        plain.or_else(loop_path)
    }
}

/// Starts a feed for one FFmpeg job; returns its port. The feed ends when
/// FFmpeg goes away (or never connects).
pub fn start() -> Option<u16> {
    source()?;
    let listener = TcpListener::bind("127.0.0.1:0").ok()?;
    let port = listener.local_addr().ok()?.port();
    listener.set_nonblocking(true).ok()?;
    std::thread::Builder::new()
        .name("banner-feed".into())
        .spawn(move || {
            let deadline = Instant::now() + Duration::from_secs(30);
            let mut sock = loop {
                match listener.accept() {
                    Ok((s, _)) => break s,
                    Err(_) if Instant::now() < deadline => {
                        std::thread::sleep(Duration::from_millis(20))
                    }
                    Err(_) => return,
                }
            };
            sock.set_nonblocking(false).ok();
            sock.set_nodelay(true).ok();
            sock.set_write_timeout(Some(Duration::from_secs(20))).ok();
            let mut shown = None;
            // Real-time pace, a little ahead: written frames wait in
            // FFmpeg's buffers, so an unpaced feed ran seconds ahead and a
            // switch showed only after those buffered frames had played.
            let frame = Duration::from_micros(1_000_000 / FEED_FPS);
            let mut clock = Instant::now();
            let mut sent: u32 = 0;
            'feed: loop {
                let gen = GEN.load(Ordering::SeqCst);
                let Some(path) = source() else {
                    std::thread::sleep(Duration::from_millis(200));
                    continue;
                };
                let Ok(data) = std::fs::read(&path) else {
                    std::thread::sleep(Duration::from_millis(200));
                    continue;
                };
                let frames = crate::scenes::split_access_units(&data, false);
                if frames.is_empty() || !frames[0].0 {
                    std::thread::sleep(Duration::from_millis(200));
                    continue;
                }
                if shown.as_ref() != Some(&path) {
                    crate::diag::log("live", format!("banner feed: {}", path.display()));
                    shown = Some(path);
                }
                // Loop this banner until something changes (switch on its
                // own frame boundary: the next one starts with a keyframe).
                loop {
                    for (_, f) in &frames {
                        let due = (clock + frame * sent).checked_sub(LEAD).unwrap_or(clock);
                        let now = Instant::now();
                        if now < due {
                            std::thread::sleep(due - now);
                        } else if now > due + Duration::from_secs(1) {
                            // FFmpeg stalled (start-up, busy GPU): no burst after.
                            clock = now;
                            sent = 0;
                        }
                        sent += 1;
                        if sock.write_all(f).is_err() {
                            break 'feed;
                        }
                        if GEN.load(Ordering::SeqCst) != gen {
                            continue 'feed;
                        }
                    }
                }
            }
        })
        .ok()?;
    Some(port)
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::process::Command;

    /// FFmpeg reads the feed while it switches banner -> plain -> banner:
    /// the picture must change within a second each time, with no gap.
    /// `cargo test banner_feed_switches_live -- --ignored --nocapture`
    #[test]
    #[ignore]
    fn banner_feed_switches_live() {
        let ffmpeg = crate::live::find_ffmpeg().expect("ffmpeg");
        let tmp = std::env::temp_dir().join("phonecast_feed_test");
        std::fs::remove_dir_all(&tmp).ok();
        let v = tmp.join("PhoneCast").join("banner_frames").join("v1");
        std::fs::create_dir_all(&v).unwrap();
        std::fs::write(v.parent().unwrap().join("current.txt"), "v1").unwrap();
        std::fs::write(v.join("loop.mp4"), b"x").unwrap();
        std::env::set_var("APPDATA", &tmp);
        let es = |color: &str, frames: u32, out: &std::path::Path| {
            let st = Command::new(&ffmpeg)
                .args(["-v", "error", "-y", "-f", "lavfi", "-i"])
                .arg(format!("color=c={color}:s=1920x208:r=30"))
                .args([
                    "-frames:v",
                    &frames.to_string(),
                    "-c:v",
                    "libx264",
                    "-g",
                    "30",
                    "-bf",
                    "0",
                ])
                .args([
                    "-x264-params",
                    "aud=1:repeat-headers=1",
                    "-pix_fmt",
                    "yuv420p",
                    "-f",
                    "h264",
                ])
                .arg(out)
                .status()
                .unwrap();
            assert!(st.success());
        };
        es("red", 600, &v.join("loop.h264"));
        es("blue", 30, &plain_path());
        set_on(true);
        let port = start().expect("feed");
        // Like the live job: the game (here a 60 fps test picture, real
        // time) with the banner overlaid; one luma sample per output frame.
        let mut child = Command::new(&ffmpeg)
            .args(["-v", "error", "-re", "-f", "lavfi", "-i", "testsrc2=s=1920x1080:r=60"])
            .args(["-f", "h264", "-framerate", "30", "-i"])
            .arg(format!("tcp://127.0.0.1:{port}"))
            .args(["-t", "6", "-filter_complex"])
            .arg("[1:v]scale=1920:208,pad=1920:1080:0:872,fps=60[bg];[0:v]crop=1920:872:0:0[g];[bg][g]overlay=0:0:shortest=1,crop=2:2:960:1000,format=rgb24")
            .args(["-f", "rawvideo", "-"])
            .stdout(std::process::Stdio::piped())
            .spawn()
            .unwrap();
        let t0 = Instant::now();
        std::thread::spawn(|| {
            std::thread::sleep(Duration::from_secs(2));
            set_on(false);
            std::thread::sleep(Duration::from_secs(2));
            set_on(true);
        });
        let mut out = Vec::new();
        use std::io::Read;
        child.stdout.take().unwrap().read_to_end(&mut out).unwrap();
        child.wait().unwrap();
        let px: Vec<char> = out
            .chunks(12)
            .map(|p| {
                if p[0] > 150 && p[2] < 100 {
                    'R'
                } else if p[2] > 150 && p[0] < 100 {
                    'B'
                } else {
                    '?'
                }
            })
            .collect();
        let s: String = px.iter().collect();
        println!("{} frames in {:?}: {}", px.len(), t0.elapsed(), s);
        assert!(px.len() >= 330, "frames missing");
        let first_b = s.find('B').expect("never switched off");
        let back_r = s[first_b..]
            .find('R')
            .map(|i| i + first_b)
            .expect("never switched back");
        println!(
            "off at frame {first_b} ({:.2} s), on again at {back_r} ({:.2} s)",
            first_b as f64 / 60.0,
            back_r as f64 / 60.0
        );
        assert!(first_b < 60 * 3, "off took too long");
        assert!(back_r < 60 * 5, "on took too long");
    }
}
