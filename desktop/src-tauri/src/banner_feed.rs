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

// ---------------- pop-ups ("moments") ----------------
//
// A pop-up (Subscribe, Like, Follow, Discord, own message) is made as four
// 5 s pieces of the 20 s banner loop, each starting at a loop position
// (0, 5, 10, 15 s) with the pop-up drawn over the very same banner frames.
// The feed swaps one in when the loop reaches that position and goes on
// with the loop right after it: no jump in the banner or the character.

/// Frames per pop-up piece (5 s at 30 fps); pieces start at multiples.
pub const MOMENT_FRAMES: usize = 150;
pub const MOMENT_NAMES: &[&str] = &["subscribe", "like", "follow", "discord", "custom"];

struct Moments {
    /// Asked for with "Show now".
    wanted: Option<String>,
    /// Automatic: every `every` seconds, these in turn.
    auto: bool,
    every: Duration,
    names: Vec<String>,
    turn: usize,
    last: Option<Instant>,
    /// On screen now (status).
    playing: Option<String>,
}

static MOMENTS: std::sync::Mutex<Moments> = std::sync::Mutex::new(Moments {
    wanted: None,
    auto: false,
    every: Duration::from_secs(180),
    names: Vec::new(),
    turn: 0,
    last: None,
    playing: None,
});

pub fn moment_path(dir: &std::path::Path, name: &str, at: usize) -> PathBuf {
    dir.join(format!("moment-{name}-{at}.h264"))
}

/// Pop-ups made for the current banner.
pub fn moments_ready() -> Vec<String> {
    let Some(dir) = crate::live::banner_frames_dir() else {
        return Vec::new();
    };
    MOMENT_NAMES
        .iter()
        // All four pieces: the feed needs the one for its loop position.
        .filter(|n| (0..4).all(|k| moment_path(&dir, n, k * MOMENT_FRAMES).exists()))
        .map(|n| n.to_string())
        .collect()
}

/// "Show now": plays at the next loop position (within 5 s).
pub fn show_moment(name: &str) -> Result<(), String> {
    if !is_on() {
        return Err("Turn the banner on to show pop-ups".into());
    }
    if !moments_ready().iter().any(|n| n == name) {
        return Err("That pop-up is still being made (or switched off)".into());
    }
    MOMENTS.lock().unwrap().wanted = Some(name.to_string());
    Ok(())
}

pub fn set_auto(auto: bool, every_secs: u64, names: Vec<String>) {
    let mut m = MOMENTS.lock().unwrap();
    m.auto = auto;
    m.every = Duration::from_secs(every_secs.clamp(30, 3600));
    m.names = names
        .into_iter()
        .filter(|n| MOMENT_NAMES.contains(&n.as_str()))
        .collect();
    // The first automatic one comes a full interval after switching on.
    m.last.get_or_insert_with(Instant::now);
}

/// (playing now, waiting to play)
pub fn moment_status() -> (Option<String>, Option<String>) {
    let m = MOMENTS.lock().unwrap();
    (m.playing.clone(), m.wanted.clone())
}

/// The pop-up to play now, if any (a click first, else an automatic one).
fn next_moment(ready: &[String]) -> Option<String> {
    let mut m = MOMENTS.lock().unwrap();
    // A click waits (is not dropped) until its pop-up is ready here.
    if let Some(w) = m.wanted.clone() {
        if ready.contains(&w) {
            m.wanted = None;
            m.last = Some(Instant::now());
            return Some(w);
        }
        return None;
    }
    if !m.auto || m.last.is_some_and(|t| t.elapsed() < m.every) {
        return None;
    }
    let list: Vec<String> = m
        .names
        .iter()
        .filter(|n| ready.contains(n))
        .cloned()
        .collect();
    if list.is_empty() {
        return None;
    }
    let n = list[m.turn % list.len()].clone();
    m.turn += 1;
    m.last = Some(Instant::now());
    Some(n)
}

/// Real-time pace, a little ahead: written frames wait in FFmpeg's
/// buffers, so an unpaced feed ran seconds ahead and a switch showed only
/// after those buffered frames had played.
struct Pace {
    clock: Instant,
    sent: u32,
}

impl Pace {
    fn wait(&mut self) {
        let frame = Duration::from_micros(1_000_000 / FEED_FPS);
        let due = (self.clock + frame * self.sent)
            .checked_sub(LEAD)
            .unwrap_or(self.clock);
        let now = Instant::now();
        if now < due {
            std::thread::sleep(due - now);
        } else if now > due + Duration::from_secs(1) {
            // FFmpeg stalled (start-up, busy GPU): no burst after.
            self.clock = now;
            self.sent = 0;
        }
        self.sent += 1;
    }
}

fn read_stream(path: &std::path::Path) -> Option<Vec<crate::scenes::Frame>> {
    let data = std::fs::read(path).ok()?;
    let frames: Vec<crate::scenes::Frame> = crate::scenes::split_access_units(&data, false)
        .into_iter()
        .map(|(k, d)| (k, std::sync::Arc::new(d)))
        .collect();
    (!frames.is_empty() && frames[0].0).then_some(frames)
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
            let mut pace = Pace {
                clock: Instant::now(),
                sent: 0,
            };
            'feed: loop {
                let gen = GEN.load(Ordering::SeqCst);
                let Some(path) = source() else {
                    std::thread::sleep(Duration::from_millis(200));
                    continue;
                };
                let Some(frames) = read_stream(&path) else {
                    std::thread::sleep(Duration::from_millis(200));
                    continue;
                };
                if shown.as_ref() != Some(&path) {
                    crate::diag::log("live", format!("banner feed: {}", path.display()));
                    shown = Some(path.clone());
                }
                // Pop-ups only over the moving banner itself.
                let is_loop = loop_path().as_ref() == Some(&path);
                let dir = path.parent().map(|d| d.to_path_buf()).unwrap_or_default();
                let mut ready = if is_loop { moments_ready() } else { Vec::new() };
                // Loop this banner until something changes (switch on its
                // own frame boundary: the next one starts with a keyframe).
                loop {
                    let mut i = 0;
                    while i < frames.len() {
                        if is_loop && i.is_multiple_of(MOMENT_FRAMES) {
                            // Pop-ups made while live count from the next position
                            // (a few file checks every 5 s).
                            ready = moments_ready();
                        }
                        if !ready.is_empty() && i.is_multiple_of(MOMENT_FRAMES) {
                            if let Some(name) = next_moment(&ready) {
                                if let Some(m) = read_stream(&moment_path(&dir, &name, i)) {
                                    crate::diag::log("live", format!("banner pop-up: {name}"));
                                    MOMENTS.lock().unwrap().playing = Some(name);
                                    for (_, f) in &m {
                                        pace.wait();
                                        if sock.write_all(f).is_err() {
                                            MOMENTS.lock().unwrap().playing = None;
                                            break 'feed;
                                        }
                                        if GEN.load(Ordering::SeqCst) != gen {
                                            MOMENTS.lock().unwrap().playing = None;
                                            continue 'feed;
                                        }
                                    }
                                    MOMENTS.lock().unwrap().playing = None;
                                    // The loop goes on where the pop-up's piece ends.
                                    i += m.len();
                                    continue;
                                }
                                // Piece unreadable: try again at the next position.
                                MOMENTS.lock().unwrap().wanted.get_or_insert(name);
                            }
                        }
                        pace.wait();
                        if sock.write_all(&frames[i].1).is_err() {
                            break 'feed;
                        }
                        if GEN.load(Ordering::SeqCst) != gen {
                            continue 'feed;
                        }
                        i += 1;
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

    /// A pop-up asked for while live plays within 5 s, for 5 s, then the
    /// loop goes on, with no frame lost:
    /// `cargo test banner_moment_plays -- --ignored --nocapture`
    #[test]
    #[ignore]
    fn banner_moment_plays() {
        let ffmpeg = crate::live::find_ffmpeg().expect("ffmpeg");
        let tmp = std::env::temp_dir().join("phonecast_moment_test");
        std::fs::remove_dir_all(&tmp).ok();
        let v = tmp.join("PhoneCast").join("banner_frames").join("v1");
        std::fs::create_dir_all(&v).unwrap();
        std::fs::write(v.parent().unwrap().join("current.txt"), "v1").unwrap();
        std::fs::write(v.join("loop.mp4"), b"x").unwrap();
        std::env::set_var("APPDATA", &tmp);
        let es = |color: &str, frames: u32, out: &std::path::Path| {
            assert!(Command::new(&ffmpeg)
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
                    "0"
                ])
                .args([
                    "-x264-params",
                    "aud=1:repeat-headers=1",
                    "-pix_fmt",
                    "yuv420p",
                    "-f",
                    "h264"
                ])
                .arg(out)
                .status()
                .unwrap()
                .success());
        };
        es("red", 600, &v.join("loop.h264"));
        for at in [0, 150, 300, 450] {
            es("lime", 150, &moment_path(&v, "subscribe", at));
        }
        set_on(true);
        let port = start().expect("feed");
        let mut child = Command::new(&ffmpeg)
            .args(["-v", "error", "-re", "-f", "lavfi", "-i", "testsrc2=s=1920x1080:r=60"])
            .args(["-probesize", "32768", "-analyzeduration", "0", "-f", "h264", "-framerate", "30", "-i"])
            .arg(format!("tcp://127.0.0.1:{port}"))
            .args(["-t", "14", "-filter_complex"])
            .arg("[1:v]scale=1920:208,pad=1920:1080:0:872,fps=60[bg];[0:v]crop=1920:872:0:0[g];[bg][g]overlay=0:0:shortest=1,crop=2:2:960:1000,format=rgb24")
            .args(["-f", "rawvideo", "-"])
            .stdout(std::process::Stdio::piped())
            .spawn()
            .unwrap();
        std::thread::spawn(|| {
            std::thread::sleep(Duration::from_secs(2));
            show_moment("subscribe").unwrap();
        });
        let mut out = Vec::new();
        use std::io::Read;
        child.stdout.take().unwrap().read_to_end(&mut out).unwrap();
        child.wait().unwrap();
        let s: String = out
            .chunks(12)
            .map(|p| {
                if p[0] > 150 && p[1] < 100 {
                    'R'
                } else if p[1] > 150 && p[0] < 100 {
                    'G'
                } else {
                    '?'
                }
            })
            .collect();
        println!("{} frames: {s}", s.len());
        assert!(s.len() >= 14 * 60 - 5, "frames missing");
        assert!(!s.contains('?'), "mixed frames");
        let g0 = s.find('G').expect("pop-up never showed");
        let g1 = s.rfind('G').unwrap();
        println!(
            "pop-up from {:.2} s to {:.2} s",
            g0 as f64 / 60.0,
            g1 as f64 / 60.0
        );
        assert!(g0 < 60 * 8, "pop-up came too late");
        let len = (g1 - g0 + 1) as f64 / 60.0;
        assert!((len - 5.0).abs() < 0.2, "pop-up lasted {len:.2} s");
        assert!(s[g0..=g1].chars().all(|c| c == 'G'), "pop-up broken up");
    }
}
