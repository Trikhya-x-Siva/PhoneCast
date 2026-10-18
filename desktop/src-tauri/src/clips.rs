//! The channel's own character clips (made with Veo on a green screen) for
//! the scenes: the green is cut out, the Veo mark in the corner is cropped
//! away, and the cut-out is laid over the scene art when a scene is made.
//!
//! Clips live in `art/clips/<scene>.mp4`; `art/clips/clips.json` says how to
//! use each one: `trim` (seconds dropped at the start: Veo's first frames are
//! its still reference), `pingpong` (play forward then back, for loops) and
//! `max` (longest part used, for the one-off action clip).

use std::path::{Path, PathBuf};
use std::process::Command;

#[cfg(windows)]
use std::os::windows::process::CommandExt;

/// Frame rate the cut-out frames are kept at (Veo clips are 24 fps).
pub const CLIP_FPS: u32 = 24;
/// Part of the clip's width kept (the middle): the character is centred and
/// the Veo mark sits in the bottom-right corner, outside this.
const KEEP_WIDTH: f64 = 0.66;
/// Character height as a part of the scene height (feet/body at the bottom).
const HEIGHT: f64 = 1.0;
/// Where the character's middle sits across the scene (text is on the right).
pub const CENTER_X: f64 = 0.25;
/// A scene's own drawn loop: clip loops are padded to a multiple of this.
const SCENE_LOOP_SECS: u32 = 4;

pub fn clips_dir() -> PathBuf {
    crate::live::banner_path()
        .with_file_name("art")
        .join("clips")
}

#[derive(Debug, Clone, Copy, PartialEq)]
pub struct ClipOpts {
    pub trim: f64,
    pub pingpong: bool,
    pub max: Option<f64>,
    /// Fades out the clip's corner right of and below this point (parts of
    /// the original frame, 0-1): Veo sometimes paints a box over it.
    pub hide: Option<(f64, f64)>,
}

/// The clip for a scene, if the channel has one.
pub fn clip_for(name: &str) -> Option<(PathBuf, ClipOpts)> {
    if !matches!(name, "starting" | "brb" | "thanks" | "stinger" | "gaming") {
        return None;
    }
    let path = clips_dir().join(format!("{name}.mp4"));
    if !path.is_file() {
        return None;
    }
    let cfg: serde_json::Value = std::fs::read(clips_dir().join("clips.json"))
        .ok()
        .and_then(|b| serde_json::from_slice(&b).ok())
        .unwrap_or_default();
    let c = &cfg[name];
    let opts = ClipOpts {
        trim: c["trim"].as_f64().unwrap_or(0.15).clamp(0.0, 3.0),
        pingpong: c["pingpong"].as_bool().unwrap_or(name != "stinger"),
        max: c["max"].as_f64().filter(|m| *m > 0.2),
        hide: match (c["hide"][0].as_f64(), c["hide"][1].as_f64()) {
            (Some(x), Some(y)) => Some((x.clamp(0.0, 1.0), y.clamp(0.0, 1.0))),
            _ => None,
        },
    };
    Some((path, opts))
}

/// Scene names that have a clip (the scene art then leaves the character out).
pub fn available() -> Vec<String> {
    ["starting", "brb", "thanks", "stinger", "gaming"]
        .iter()
        .filter(|n| clip_for(n).is_some())
        .map(|n| n.to_string())
        .collect()
}

/// Changes whenever a scene's clip file or its settings change.
pub fn stamp(name: &str) -> String {
    let Some((path, opts)) = clip_for(name) else {
        return "noclip".into();
    };
    let m = std::fs::metadata(&path).ok();
    let len = m.as_ref().map(|m| m.len()).unwrap_or(0);
    let modified = m
        .and_then(|m| m.modified().ok())
        .and_then(|t| t.duration_since(std::time::UNIX_EPOCH).ok())
        .map(|d| d.as_secs())
        .unwrap_or(0);
    format!(
        "clip{len:x}{modified:x}t{}p{}m{}",
        (opts.trim * 100.0) as u32,
        opts.pingpong as u8,
        opts.max.map(|m| (m * 100.0) as u32).unwrap_or(0)
    ) + &opts
        .hide
        .map(|(x, y)| format!("h{}x{}", (x * 100.0) as u32, (y * 100.0) as u32))
        .unwrap_or_default()
}

/// Cut-out frames ready to lay over a scene.
#[derive(Debug, Clone)]
pub struct Prepared {
    /// `%04d.png` frames, in play order (ping-pong already laid out).
    pub dir: PathBuf,
    pub frames: usize,
    pub x: i64,
    pub y: i64,
    /// Fade-out at the end of a one-off clip.
    pub pingpong: bool,
}

impl Prepared {
    pub fn secs(&self) -> f64 {
        self.frames as f64 / CLIP_FPS as f64
    }
}

fn even(v: f64) -> u32 {
    ((v / 2.0).round() as u32).max(1) * 2
}

fn run(cmd: &mut Command) -> Result<Vec<u8>, String> {
    #[cfg(windows)]
    cmd.creation_flags(0x0800_0000);
    let o = cmd.output().map_err(|e| e.to_string())?;
    if !o.status.success() {
        return Err(String::from_utf8_lossy(&o.stderr).trim().to_string());
    }
    Ok(o.stdout)
}

/// The clip's screen green, read from its top-left corner (each Veo clip
/// comes out a slightly different green).
fn screen_green(ffmpeg: &Path, clip: &Path, at: f64) -> Result<(u8, u8, u8), String> {
    let out = run(Command::new(ffmpeg)
        .args([
            "-hide_banner",
            "-loglevel",
            "error",
            "-ss",
            &format!("{at:.2}"),
            "-i",
        ])
        .arg(clip)
        .args([
            "-frames:v",
            "1",
            "-vf",
            "crop=16:16:16:16,scale=1:1:flags=area,format=rgb24",
            "-f",
            "rawvideo",
            "-",
        ]))?;
    match out[..] {
        [r, g, b, ..] if g > r.saturating_add(40) && g > b.saturating_add(40) => Ok((r, g, b)),
        // Not green there (the character fills the corner): the usual one.
        _ => Ok((0x1c, 0x9c, 0x4c)),
    }
}

/// Cuts the character out of `clip`, sized for a `w`x`h` scene. Kept on
/// disk, so the same clip at the same size is only cut once. `loop_frames`
/// makes the loop exactly that long (the banner's 20 s loop).
pub fn prepare(
    ffmpeg: &Path,
    name: &str,
    w: u32,
    h: u32,
    loop_frames: Option<usize>,
) -> Result<Option<Prepared>, String> {
    let Some((clip, opts)) = clip_for(name) else {
        return Ok(None);
    };
    // One cut at a time: two scenes made at once (e.g. Be right back for the
    // privacy cover and a click) removed each other's files mid-cut.
    static CUTTING: std::sync::Mutex<()> = std::sync::Mutex::new(());
    let _one = CUTTING.lock().unwrap_or_else(|e| e.into_inner());
    let (cw0, ch0) = probe_size(ffmpeg, &clip).unwrap_or((1920, 1080));
    let ch = even(h as f64 * HEIGHT);
    let cw = even(ch as f64 * cw0 as f64 / ch0 as f64 * KEEP_WIDTH);
    let place = |frames| Prepared {
        dir: PathBuf::new(),
        frames,
        x: (w as f64 * CENTER_X - cw as f64 / 2.0).round() as i64,
        y: h as i64 - ch as i64,
        pingpong: opts.pingpong,
    };
    let root = crate::scenes::scenes_dir().join(format!(
        "{name}-{}-{cw}x{ch}{}",
        stamp(name),
        loop_frames.map(|n| format!("-L{n}")).unwrap_or_default()
    ));
    let seq = root.join("seq");
    if let Ok(n) = std::fs::read_to_string(root.join("done")) {
        if let Ok(n) = n.trim().parse::<usize>() {
            return Ok(Some(Prepared {
                dir: seq,
                ..place(n)
            }));
        }
    }
    // Old cut-outs of this scene (another size or clip) are not needed.
    if let Ok(rd) = std::fs::read_dir(crate::scenes::scenes_dir()) {
        for e in rd.flatten() {
            let n = e.file_name().to_string_lossy().to_string();
            let same_kind = n.contains("-L") == loop_frames.is_some();
            if n.starts_with(&format!("{name}-clip")) && same_kind && e.path() != root {
                std::fs::remove_dir_all(e.path()).ok();
            }
        }
    }
    std::fs::remove_dir_all(&root).ok();
    let raw = root.join("raw");
    std::fs::create_dir_all(&raw).map_err(|e| e.to_string())?;
    std::fs::create_dir_all(&seq).map_err(|e| e.to_string())?;

    let (r, g, b) = screen_green(ffmpeg, &clip, opts.trim + 0.5)?;
    // Soft left/right edges (the kept middle is cut out of a wider picture).
    // Plus the corner a clip asks to hide.
    let mask = root.join("mask.png");
    let corner = opts
        .hide
        .map(|(x0, y0)| {
            let off = (1.0 - KEEP_WIDTH) / 2.0;
            format!(
                "*(1-clip(({off:.4}+X/W*{KEEP_WIDTH}-{x0:.4})/0.06\\,0\\,1)*clip((Y/H-{y0:.4})/0.06\\,0\\,1))"
            )
        })
        .unwrap_or_default();
    run(Command::new(ffmpeg)
        .args([
            "-hide_banner",
            "-loglevel",
            "error",
            "-y",
            "-f",
            "lavfi",
            "-i",
        ])
        .arg(format!("color=white:s={cw}x{ch}:d=1"))
        .args([
            "-frames:v",
            "1",
            "-vf",
            &format!("format=gray,geq=lum='255*min(1\\,min(X\\,W-X)/(W*0.14)){corner}'"),
        ])
        .arg(&mask))?;
    let mut fade = String::new();
    let mut cmd = Command::new(ffmpeg);
    cmd.args(["-hide_banner", "-loglevel", "error", "-y"])
        .args(["-ss", &format!("{:.3}", opts.trim)]);
    if let Some(m) = opts.max {
        cmd.args(["-t", &format!("{m:.3}")]);
        if !opts.pingpong {
            fade = format!(",fade=t=out:st={:.3}:d=0.25:alpha=1", (m - 0.25).max(0.0));
        }
    }
    // Two greens: the screen, and the brighter box Veo sometimes leaves in
    // a corner. Both cut-outs are merged (one chromakey would replace the
    // other's), then the green fringe is toned down.
    let filter = format!(
        "[0:v]fps={CLIP_FPS},crop=iw*{KEEP_WIDTH}:ih:iw*{off}:0,scale={cw}:{ch}:flags=lanczos,\
         format=yuva444p,split=3[s][a][b];\
         [a]chromakey=0x{r:02x}{g:02x}{b:02x}:0.10:0.06,alphaextract[a1];\
         [b]chromakey=0x12dc2c:0.10:0.05,alphaextract[a2];\
         [a1][a2]blend=all_mode=darken[al0];\
         [1:v]format=gray[m];[al0][m]blend=all_mode=multiply:shortest=1[al];\
         [s][al]alphamerge,despill=type=green:mix=0.25:expand=0{fade},format=rgba",
        off = (1.0 - KEEP_WIDTH) / 2.0
    );
    run(cmd
        .arg("-i")
        .arg(&clip)
        .args(["-loop", "1", "-i"])
        .arg(&mask)
        .args(["-filter_complex", &filter, "-fps_mode", "passthrough"])
        .arg(raw.join("%04d.png")))?;
    let n = std::fs::read_dir(&raw).map_err(|e| e.to_string())?.count();
    if n < 2 {
        return Err("the clip gave no frames".into());
    }
    let order = match loop_frames {
        Some(t) => fitted_loop(n, t),
        None => play_order(n, opts.pingpong),
    };
    for (i, src) in order.iter().enumerate() {
        let from = raw.join(format!("{:04}.png", src + 1));
        let to = seq.join(format!("{:04}.png", i + 1));
        if std::fs::hard_link(&from, &to).is_err() {
            std::fs::copy(&from, &to).map_err(|e| e.to_string())?;
        }
    }
    std::fs::write(root.join("done"), order.len().to_string()).ok();
    Ok(Some(Prepared {
        dir: seq,
        ..place(order.len())
    }))
}

/// Forward-and-back loops fitted into exactly `t` frames, played a little
/// faster or slower so a whole number of them fits (seamless at the wrap).
pub fn fitted_loop(n: usize, t: usize) -> Vec<usize> {
    let cycle: Vec<usize> = (0..n).chain((1..n.saturating_sub(1)).rev()).collect();
    let len = cycle.len().max(1);
    let k = ((t as f64 / len as f64).round() as usize).max(1);
    (0..t).map(|i| cycle[(i * k * len / t) % len]).collect()
}

/// Frame order: forward only, or forward then back with short holds at each
/// end so one loop lasts a whole number of the scene's own 4 s loops.
pub fn play_order(n: usize, pingpong: bool) -> Vec<usize> {
    if !pingpong {
        return (0..n).collect();
    }
    let unit = (SCENE_LOOP_SECS * CLIP_FPS) as usize;
    let base = 2 * n - 2;
    let pad = base.div_ceil(unit) * unit - base;
    let mut v: Vec<usize> = (0..n).collect();
    v.extend(std::iter::repeat_n(n - 1, pad / 2));
    v.extend((1..n - 1).rev());
    v.extend(std::iter::repeat_n(0, pad - pad / 2));
    v
}

fn probe_size(ffmpeg: &Path, clip: &Path) -> Option<(u32, u32)> {
    let probe = ffmpeg.with_file_name(if cfg!(windows) {
        "ffprobe.exe"
    } else {
        "ffprobe"
    });
    let out = run(Command::new(probe)
        .args([
            "-v",
            "error",
            "-select_streams",
            "v:0",
            "-show_entries",
            "stream=width,height",
            "-of",
            "csv=p=0",
        ])
        .arg(clip))
    .ok()?;
    let s = String::from_utf8_lossy(&out);
    let mut it = s.trim().split(',');
    Some((it.next()?.parse().ok()?, it.next()?.parse().ok()?))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn pingpong_fills_whole_scene_loops() {
        for n in [2, 50, 95, 120, 140] {
            let v = play_order(n, true);
            assert_eq!(v.len() % (SCENE_LOOP_SECS * CLIP_FPS) as usize, 0, "n={n}");
            assert_eq!(v[0], 0);
            assert!(v.contains(&(n - 1)));
            // No jump bigger than one frame anywhere, wrap-around included.
            for w in 0..v.len() {
                let (a, b) = (v[w], v[(w + 1) % v.len()]);
                assert!(a.abs_diff(b) <= 1, "n={n} at {w}: {a}->{b}");
            }
        }
        assert_eq!(play_order(5, false), vec![0, 1, 2, 3, 4]);
    }

    #[test]
    fn fitted_loop_is_exact_and_seamless() {
        for (n, t) in [(144, 480), (120, 480), (300, 480), (10, 480)] {
            let v = fitted_loop(n, t);
            assert_eq!(v.len(), t);
            for w in 0..t {
                let (a, b) = (v[w], v[(w + 1) % t]);
                assert!(a.abs_diff(b) <= 2, "n={n} at {w}: {a}->{b}");
            }
        }
    }

    /// Makes the real scenes from the clips in `art/clips` over a plain
    /// drawn background and saves a still of each for a look:
    /// `cargo test clip_scenes_render -- --ignored --nocapture`
    #[test]
    #[ignore]
    fn clip_scenes_render() {
        let ffmpeg = crate::live::find_ffmpeg().expect("ffmpeg");
        let tmp = std::env::temp_dir().join("phonecast_clip_test");
        std::fs::create_dir_all(&tmp).unwrap();
        let bg = tmp.join("bg.jpg");
        run(Command::new(&ffmpeg)
            .args([
                "-hide_banner",
                "-loglevel",
                "error",
                "-y",
                "-f",
                "lavfi",
                "-i",
            ])
            .arg("gradients=s=1920x872:c0=0x0b1024:c1=0x3a1a6a:n=2,format=yuvj420p")
            .args(["-frames:v", "1"])
            .arg(&bg))
        .unwrap();
        let jpg = std::fs::read(&bg).unwrap();
        for (name, n, hold) in [
            ("starting", 120, None),
            ("brb", 120, None),
            ("thanks", 120, None),
            ("stinger", 120, Some(3.0)),
        ] {
            let t = std::time::Instant::now();
            let Some(c) = prepare(&ffmpeg, name, 1920, 872, None).unwrap() else {
                println!("{name}: no clip");
                continue;
            };
            let refs: Vec<&[u8]> = (0..n).map(|_| &jpg[..]).collect();
            let frames = crate::scenes::encode(
                &ffmpeg,
                &refs,
                1920,
                872,
                false,
                &format!("cliptest-{name}"),
                hold,
                Some(&c),
            )
            .unwrap();
            let es = tmp.join(format!("{name}.h264"));
            let all: Vec<u8> = frames.iter().flat_map(|f| f.1.iter().copied()).collect();
            std::fs::write(&es, &all).unwrap();
            run(Command::new(&ffmpeg)
                .args(["-hide_banner", "-loglevel", "error", "-y", "-i"])
                .arg(&es)
                .args([
                    "-vf",
                    r"select='eq(n\,90)+eq(n\,300)',scale=640:-2,tile=2x1",
                    "-frames:v",
                    "1",
                ])
                .arg(tmp.join(format!("{name}.png"))))
            .unwrap();
            println!(
                "{name}: {} clip frames, {} scene frames, {} KB, {} ms",
                c.frames,
                frames.len(),
                all.len() / 1024,
                t.elapsed().as_millis()
            );
        }
    }
}
