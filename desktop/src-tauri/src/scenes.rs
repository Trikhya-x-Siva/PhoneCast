//! Scenes: "Starting soon", "Be right back", "Thanks for watching".
//!
//! A scene replaces the phone picture inside the stream without touching
//! FFmpeg: the scene (drawn by the app's UI) is encoded once into the
//! phone's own codec and size at 60 fps, with a single keyframe at its
//! start, and PhoneCast feeds those frames instead of the phone's while the
//! scene is on (the phone's timestamps are kept, so timing never jumps).
//! Live streams, the vertical feed, the banner under the game, recordings
//! and clips all pick it up, and switching is instant: no reconnect.
//! Going back waits for the phone's next keyframe (asked for at once) and
//! re-sends the phone's codec setup with it.

use std::path::{Path, PathBuf};
use std::process::Command;
use std::sync::Arc;

#[cfg(windows)]
use std::os::windows::process::CommandExt;

/// One encoded frame: (is keyframe, Annex B bytes).
pub type Frame = (bool, Arc<Vec<u8>>);

/// What the video feed is doing.
#[derive(Default)]
pub enum SceneState {
    #[default]
    Off,
    /// Showing a scene: `frames` loop, `next` is the next frame to send.
    On {
        name: String,
        frames: Arc<Vec<Frame>>,
        next: usize,
    },
    /// Back to the game requested: keep showing the scene until the phone's
    /// next keyframe arrives.
    Leaving {
        name: String,
        frames: Arc<Vec<Frame>>,
        next: usize,
    },
    /// The 3 s avatar "action" clip on the way back to the game: plays once,
    /// then holds its last part (from the keyframe at `hold_from`) until the
    /// phone's next keyframe, which is asked for just before the hold.
    Stinger {
        frames: Arc<Vec<Frame>>,
        next: usize,
        hold_from: usize,
        asked: bool,
    },
}

impl SceneState {
    pub fn name(&self) -> Option<&str> {
        match self {
            SceneState::Off => None,
            SceneState::On { name, .. } | SceneState::Leaving { name, .. } => Some(name),
            SceneState::Stinger { .. } => Some("stinger"),
        }
    }

    /// True once, about 1/3 s before a stinger reaches its hold: time to ask
    /// the phone for the keyframe the game comes back on.
    pub fn wants_keyframe(&mut self) -> bool {
        if let SceneState::Stinger {
            next,
            hold_from,
            asked,
            ..
        } = self
        {
            if !*asked && *next + 20 >= *hold_from {
                *asked = true;
                return true;
            }
        }
        false
    }

    /// The frame to send for one phone frame. `phone_csd` is the phone's
    /// codec setup (SPS/PPS), sent again with the first phone keyframe after
    /// a scene, because the scene's own setup replaced it in the decoder.
    pub fn substitute(&mut self, key: bool, data: Arc<Vec<u8>>, phone_csd: &[u8]) -> Frame {
        let leave_now = key
            && match self {
                SceneState::Leaving { .. } => true,
                SceneState::Stinger {
                    next, hold_from, ..
                } => *next >= *hold_from,
                _ => false,
            };
        if leave_now {
            *self = SceneState::Off;
            let mut v = Vec::with_capacity(phone_csd.len() + data.len());
            v.extend_from_slice(phone_csd);
            v.extend_from_slice(&data);
            return (true, Arc::new(v));
        }
        match self {
            SceneState::Off => (key, data),
            SceneState::On { frames, next, .. } | SceneState::Leaving { frames, next, .. } => {
                let f = frames[*next].clone();
                *next = (*next + 1) % frames.len();
                f
            }
            SceneState::Stinger {
                frames,
                next,
                hold_from,
                ..
            } => {
                let f = frames[*next].clone();
                *next += 1;
                if *next >= frames.len() {
                    *next = *hold_from; // hold: loop the last part
                }
                f
            }
        }
    }
}

pub fn scenes_dir() -> PathBuf {
    crate::live::banner_path().with_file_name("scenes")
}

/// Splits an Annex B elementary stream into access units at its access unit
/// delimiters (the encoder writes one before every frame).
pub fn split_access_units(data: &[u8], hevc: bool) -> Vec<(bool, Vec<u8>)> {
    // Start positions of every start code (00 00 01, or 00 00 00 01).
    let mut starts = Vec::new();
    let mut i = 0;
    while i + 3 <= data.len() {
        if data[i] == 0 && data[i + 1] == 0 && data[i + 2] == 1 {
            let s = if i > 0 && data[i - 1] == 0 { i - 1 } else { i };
            starts.push((s, i + 3));
            i += 3;
        } else {
            i += 1;
        }
    }
    let mut out: Vec<(bool, Vec<u8>)> = Vec::new();
    for (n, &(s, body)) in starts.iter().enumerate() {
        let end = starts.get(n + 1).map(|x| x.0).unwrap_or(data.len());
        let Some(&b) = data.get(body) else { continue };
        let ty = if hevc { (b >> 1) & 0x3f } else { b & 0x1f };
        let aud = if hevc { ty == 35 } else { ty == 9 };
        let key = if hevc {
            (16..=21).contains(&ty)
        } else {
            ty == 5
        };
        if aud || out.is_empty() {
            out.push((false, Vec::new()));
        }
        let au = out.last_mut().unwrap();
        au.0 |= key;
        au.1.extend_from_slice(&data[s..end]);
    }
    out.retain(|(_, d)| !d.is_empty());
    out
}

/// Encodes JPEG frames (30 fps, drawn by the UI) into `w`×`h` at 60 fps in
/// the phone's codec, one keyframe at the start, and returns the frames.
pub fn encode(
    ffmpeg: &Path,
    jpegs: &[&[u8]],
    w: u32,
    h: u32,
    hevc: bool,
    tag: &str,
    keyframe_at: Option<f64>,
) -> Result<Vec<Frame>, String> {
    let dir = scenes_dir().join(format!("tmp-{tag}"));
    std::fs::remove_dir_all(&dir).ok();
    std::fs::create_dir_all(&dir).map_err(|e| e.to_string())?;
    for (i, j) in jpegs.iter().enumerate() {
        std::fs::write(dir.join(format!("{i:03}.jpg")), j).map_err(|e| e.to_string())?;
    }
    let out = scenes_dir().join(format!("{tag}.{}", if hevc { "hevc" } else { "h264" }));
    let vf = format!("scale={w}:{h}:flags=lanczos,fps=60,format=yuv420p");
    let attempts: [&[&str]; 2] = if hevc {
        [
            &[
                "-c:v",
                "hevc_nvenc",
                "-preset",
                "p5",
                "-rc",
                "vbr",
                "-cq",
                "20",
                "-b:v",
                "0",
                "-maxrate",
                "25M",
                "-bufsize",
                "25M",
                "-g",
                "100000",
                "-bf",
                "0",
                "-forced-idr",
                "1",
                "-aud",
                "1",
                "-f",
                "hevc",
            ],
            &[
                "-c:v",
                "libx265",
                "-preset",
                "veryfast",
                "-crf",
                "20",
                "-x265-params",
                "keyint=100000:bframes=0:aud=1:repeat-headers=1",
                "-f",
                "hevc",
            ],
        ]
    } else {
        [
            &[
                "-c:v",
                "h264_nvenc",
                "-preset",
                "p5",
                "-rc",
                "vbr",
                "-cq",
                "19",
                "-b:v",
                "0",
                "-maxrate",
                "25M",
                "-bufsize",
                "25M",
                "-g",
                "100000",
                "-bf",
                "0",
                "-forced-idr",
                "1",
                "-aud",
                "1",
                "-profile:v",
                "high",
                "-f",
                "h264",
            ],
            &[
                "-c:v",
                "libx264",
                "-preset",
                "veryfast",
                "-crf",
                "18",
                "-g",
                "100000",
                "-bf",
                "0",
                "-x264-params",
                "aud=1",
                "-profile:v",
                "high",
                "-f",
                "h264",
            ],
        ]
    };
    let mut last_err = String::new();
    for args in attempts {
        let mut cmd = Command::new(ffmpeg);
        cmd.args([
            "-hide_banner",
            "-loglevel",
            "error",
            "-y",
            "-framerate",
            "30",
            "-i",
        ])
        .arg(dir.join("%03d.jpg"))
        .args(["-vf", &vf])
        .args(args);
        if let Some(t) = keyframe_at {
            cmd.args(["-force_key_frames", &format!("0,{t:.3}")]);
        }
        cmd.arg(&out);
        #[cfg(windows)]
        cmd.creation_flags(0x0800_0000);
        match cmd.output() {
            Ok(o) if o.status.success() => {
                last_err.clear();
                break;
            }
            Ok(o) => last_err = String::from_utf8_lossy(&o.stderr).trim().to_string(),
            Err(e) => last_err = e.to_string(),
        }
    }
    std::fs::remove_dir_all(&dir).ok();
    if !last_err.is_empty() {
        return Err(format!("could not prepare the scene: {last_err}"));
    }
    let data = std::fs::read(&out).map_err(|e| e.to_string())?;
    let mut frames: Vec<Frame> = split_access_units(&data, hevc)
        .into_iter()
        .map(|(k, d)| (k, Arc::new(d)))
        .collect();
    // The loop must restart on a keyframe: drop anything before the first.
    if let Some(first) = frames.iter().position(|f| f.0) {
        frames.drain(..first);
    }
    if frames.is_empty() || !frames[0].0 {
        return Err("the prepared scene has no keyframe".into());
    }
    Ok(frames)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn splits_h264_at_access_unit_delimiters() {
        // AUD, SPS, PPS, IDR | AUD, P | AUD, P
        let s: Vec<u8> = [
            &[0, 0, 0, 1, 0x09, 0x10][..],
            &[0, 0, 0, 1, 0x67, 1, 2],
            &[0, 0, 0, 1, 0x68, 3],
            &[0, 0, 1, 0x65, 9, 9, 9],
            &[0, 0, 0, 1, 0x09, 0x30],
            &[0, 0, 1, 0x41, 7, 7],
            &[0, 0, 0, 1, 0x09, 0x30],
            &[0, 0, 1, 0x41, 8],
        ]
        .concat();
        let au = split_access_units(&s, false);
        assert_eq!(au.len(), 3);
        assert!(au[0].0 && !au[1].0 && !au[2].0);
        assert_eq!(au[0].1.len(), 6 + 7 + 6 + 7);
        assert_eq!(au.iter().map(|a| a.1.len()).sum::<usize>(), s.len());
    }

    #[test]
    fn scene_loops_and_returns_on_the_phone_keyframe() {
        let fr = |k: bool, b: u8| (k, Arc::new(vec![b]));
        let frames = Arc::new(vec![fr(true, 1), fr(false, 2)]);
        let mut st = SceneState::On {
            name: "brb".into(),
            frames: frames.clone(),
            next: 0,
        };
        let phone = |b: u8| Arc::new(vec![b]);
        assert_eq!(*st.substitute(false, phone(9), b"").1, vec![1]);
        assert_eq!(*st.substitute(false, phone(9), b"").1, vec![2]);
        assert_eq!(*st.substitute(true, phone(9), b"").1, vec![1]); // loops
        st = SceneState::Leaving {
            name: "brb".into(),
            frames,
            next: 1,
        };
        // Phone delta frames are still hidden behind the scene...
        assert_eq!(*st.substitute(false, phone(9), b"CSD").1, vec![2]);
        // ...until its keyframe, which goes out with the phone's setup.
        let (k, d) = st.substitute(true, phone(5), b"CSD");
        assert!(k);
        assert_eq!(&d[..], b"CSD\x05");
        assert!(matches!(st, SceneState::Off));
        assert_eq!(*st.substitute(false, phone(6), b"CSD").1, vec![6]);
    }
}

#[cfg(test)]
mod decode_tests {
    use super::*;
    use crate::mkv::{MkvWriter, VideoConfig, CODEC_H264};

    /// Game → scene → game through the same MKV feed the live jobs read,
    /// decoded by FFmpeg on the GPU: no decode errors, every frame comes out.
    /// `cargo test scene_switch_decodes -- --ignored --nocapture`
    #[test]
    #[ignore]
    fn scene_switch_decodes() {
        let ffmpeg = crate::live::find_ffmpeg().expect("ffmpeg");
        let tmp = std::env::temp_dir().join("phonecast_scene_test");
        std::fs::create_dir_all(&tmp).unwrap();
        // A phone-like stream: 1920x872 H.264, no B-frames, keyframe every 2 s.
        let phone = tmp.join("phone.h264");
        let st = Command::new(&ffmpeg)
            .args([
                "-hide_banner",
                "-loglevel",
                "error",
                "-y",
                "-f",
                "lavfi",
                "-i",
            ])
            .arg("testsrc2=s=1920x872:r=60:d=8")
            .args([
                "-c:v",
                "h264_nvenc",
                "-bf",
                "0",
                "-g",
                "120",
                "-aud",
                "1",
                "-f",
                "h264",
            ])
            .arg(&phone)
            .status()
            .unwrap();
        assert!(st.success());
        let data = std::fs::read(&phone).unwrap();
        let aus = split_access_units(&data, false);
        // Phone setup = the SPS/PPS before the first slice.
        let first = &aus[0].1;
        let slice = (0..first.len() - 4)
            .find(|&i| first[i..i + 3] == [0, 0, 1] && first[i + 3] & 0x1f == 5)
            .unwrap();
        let csd = first[..slice - 1].to_vec();
        // Scene frames from JPEGs (made by FFmpeg here; the app draws them).
        let jdir = tmp.join("j");
        std::fs::create_dir_all(&jdir).unwrap();
        Command::new(&ffmpeg)
            .args([
                "-hide_banner",
                "-loglevel",
                "error",
                "-y",
                "-f",
                "lavfi",
                "-i",
            ])
            .arg("testsrc=s=1920x872:r=30:d=4")
            .arg(jdir.join("%03d.jpg"))
            .status()
            .unwrap();
        let jpegs: Vec<Vec<u8>> = (1..=120)
            .map(|i| std::fs::read(jdir.join(format!("{i:03}.jpg"))).unwrap())
            .collect();
        let refs: Vec<&[u8]> = jpegs.iter().map(|j| j.as_slice()).collect();
        let frames = encode(&ffmpeg, &refs, 1920, 872, false, "test", None).unwrap();
        assert!(frames[0].0 && frames.len() >= 200, "{}", frames.len());

        let cfg = VideoConfig {
            codec: CODEC_H264,
            width: 1920,
            height: 872,
            csd: csd.clone(),
        };
        let mkv = tmp.join("switch.mkv");
        let mut w = MkvWriter::new(
            std::io::BufWriter::new(std::fs::File::create(&mkv).unwrap()),
            &cfg,
            None,
            true,
        )
        .unwrap();
        let mut st = SceneState::Off;
        let mut sent = 0;
        for (n, (key, au)) in aus.iter().enumerate() {
            if n == 90 {
                st = SceneState::On {
                    name: "brb".into(),
                    frames: Arc::new(frames.clone()),
                    next: 0,
                };
            }
            if n == 300 {
                if let SceneState::On { name, frames, next } = std::mem::take(&mut st) {
                    st = SceneState::Leaving { name, frames, next };
                }
            }
            // The phone's stream as it arrives (Annex B, no setup in frames).
            let body = if n == 0 {
                au[slice - 1..].to_vec()
            } else {
                au.clone()
            };
            let (k, d) = st.substitute(*key, Arc::new(body), &csd);
            w.write_video(n as i64 * 16_667, k, &d).unwrap();
            sent += 1;
        }
        w.end().unwrap();
        let out = Command::new(&ffmpeg)
            .args([
                "-hide_banner",
                "-loglevel",
                "error",
                "-hwaccel",
                "cuda",
                "-i",
            ])
            .arg(&mkv)
            .args(["-f", "null", "-"])
            .output()
            .unwrap();
        let err = String::from_utf8_lossy(&out.stderr);
        println!("decode errors: {err:?}");
        assert!(out.status.success() && err.trim().is_empty(), "{err}");
        let probe = Command::new(ffmpeg.with_file_name("ffprobe.exe"))
            .args(["-v", "error", "-count_frames", "-select_streams", "v"])
            .args(["-show_entries", "stream=nb_read_frames", "-of", "csv=p=0"])
            .arg(&mkv)
            .output()
            .unwrap();
        let n: usize = String::from_utf8_lossy(&probe.stdout)
            .trim()
            .parse()
            .unwrap();
        println!("sent {sent}, decoded {n}");
        assert_eq!(n, sent);
    }
}

#[cfg(test)]
mod stinger_tests {
    use super::*;

    #[test]
    fn stinger_plays_once_holds_and_returns_on_phone_keyframe() {
        let fr = |k: bool, b: u8| (k, Arc::new(vec![b]));
        // 0..3 action (keyframe at 0), 3..5 hold (keyframe at 3)
        let frames = Arc::new(vec![
            fr(true, 0),
            fr(false, 1),
            fr(false, 2),
            fr(true, 3),
            fr(false, 4),
        ]);
        let mut st = SceneState::Stinger {
            frames,
            next: 0,
            hold_from: 3,
            asked: false,
        };
        let p = |b: u8| Arc::new(vec![b]);
        assert!(st.wants_keyframe()); // short clip: ask at once
        assert!(!st.wants_keyframe());
        // A phone keyframe during the action does not cut it short.
        assert_eq!(*st.substitute(true, p(9), b"C").1, vec![0]);
        assert_eq!(*st.substitute(false, p(9), b"C").1, vec![1]);
        assert_eq!(*st.substitute(false, p(9), b"C").1, vec![2]);
        assert_eq!(*st.substitute(false, p(9), b"C").1, vec![3]);
        assert_eq!(*st.substitute(false, p(9), b"C").1, vec![4]);
        assert_eq!(*st.substitute(false, p(9), b"C").1, vec![3]); // hold loops
        let (k, d) = st.substitute(true, p(7), b"C");
        assert!(k && &d[..] == b"C\x07");
        assert!(matches!(st, SceneState::Off));
    }
}
