//! Platform presets and pre-flight validation.
//!
//! Every preset follows the platform's published ingest specification
//! (checked 2026-10-18), so the warnings streamers see in YouTube Studio
//! ("resolution not supported", "bitrate lower than recommended") cannot
//! come from PhoneCast's own settings. Before going live, [`plan`] checks
//! every destination against its platform's limits and the measured upload
//! speed, and lowers secondary destinations first when the upload is short.

use serde::Serialize;
use serde_json::{json, Value};

#[derive(Clone, Copy, PartialEq, Eq, Debug, Serialize)]
#[serde(rename_all = "lowercase")]
pub enum Platform {
    Youtube,
    /// YouTube Dual stream's vertical (9:16) feed, sent with the "Vertical
    /// stream key" next to the main YouTube stream (Encoder mode).
    YoutubeVertical,
    Twitch,
    Kick,
    Facebook,
    Custom,
}

pub const PLATFORMS: [Platform; 6] = [
    Platform::Youtube,
    Platform::YoutubeVertical,
    Platform::Twitch,
    Platform::Kick,
    Platform::Facebook,
    Platform::Custom,
];

#[derive(Clone, Copy, PartialEq, Eq, Debug, Serialize)]
#[serde(rename_all = "lowercase")]
pub enum Codec {
    H264,
    Hevc,
}

#[derive(Clone, Copy, Debug, PartialEq)]
pub struct Audio {
    /// AAC-LC sample rate.
    pub rate: u32,
    pub kbps: u32,
    pub channels: u8,
}

#[derive(Clone, Copy, Debug, PartialEq)]
pub struct Preset {
    pub id: &'static str,
    pub label: &'static str,
    pub codec: Codec,
    pub w: u32,
    pub h: u32,
    pub fps: u32,
    /// Video bitrate (CBR).
    pub kbps: u32,
    /// Keyframe interval in seconds (closed GOP, IDR keyframes).
    pub gop_s: u32,
    pub max_bframes: u8,
    /// H.264 profile ("high", "main"); HEVC always uses Main.
    pub profile: &'static str,
    /// H.264 level when the platform asks for one (Facebook: 4.2).
    pub level: Option<&'static str>,
    pub audio: Audio,
}

impl Preset {
    /// Upload this preset needs: video + audio + ~3% FLV/RTMP overhead.
    pub fn upload_kbps(&self) -> u32 {
        ((self.kbps + self.audio.kbps) as f64 * 1.03).ceil() as u32
    }

    pub fn gop_frames(&self) -> u32 {
        self.fps * self.gop_s
    }

    fn json(&self) -> Value {
        json!({
            "id": self.id,
            "label": self.label,
            "codec": self.codec,
            "width": self.w,
            "height": self.h,
            "fps": self.fps,
            "kbps": self.kbps,
            "uploadKbps": self.upload_kbps(),
            "gopSeconds": self.gop_s,
            "bframes": self.max_bframes,
            "audio": format!("AAC {} kbps · {} kHz", self.audio.kbps, self.audio.rate as f64 / 1000.0),
        })
    }
}

const AAC_YT: Audio = Audio {
    rate: 44_100,
    kbps: 128,
    channels: 2,
};
const AAC_48_160: Audio = Audio {
    rate: 48_000,
    kbps: 160,
    channels: 2,
};
const AAC_48_128: Audio = Audio {
    rate: 48_000,
    kbps: 128,
    channels: 2,
};

/// Builds a preset; every platform uses a 2 s GOP and at most 2 B-frames.
#[allow(clippy::too_many_arguments)]
const fn p(
    id: &'static str,
    label: &'static str,
    codec: Codec,
    (w, h, fps): (u32, u32, u32),
    kbps: u32,
    profile: &'static str,
    level: Option<&'static str>,
    audio: Audio,
) -> Preset {
    Preset {
        id,
        label,
        codec,
        w,
        h,
        fps,
        kbps,
        gop_s: 2,
        max_bframes: 2,
        profile,
        level,
        audio,
    }
}

const P1440: (u32, u32, u32) = (2560, 1440, 60);
const P1080: (u32, u32, u32) = (1920, 1080, 60);
const P936: (u32, u32, u32) = (1664, 936, 60);
const P720: (u32, u32, u32) = (1280, 720, 60);
const P2160: (u32, u32, u32) = (3840, 2160, 60);
const V1920: (u32, u32, u32) = (1080, 1920, 60);
const V1280: (u32, u32, u32) = (720, 1280, 60);

// YouTube Dual stream, vertical feed (Shorts feed, phones held upright).
// Lowered first when upload is short (the main stream keeps its quality).
// Bitrates follow YouTube's 1080p60 / 720p60 rows (the same pixel counts).
const YOUTUBE_VERTICAL: &[Preset] = &[
    p(
        "ytv-1080x1920-hevc",
        "1080×1920 vertical HEVC · 12 Mbps",
        Codec::Hevc,
        V1920,
        12_000,
        "main",
        None,
        AAC_YT,
    ),
    p(
        "ytv-720x1280-hevc",
        "720×1280 vertical HEVC · 6 Mbps (recommended for most uploads)",
        Codec::Hevc,
        V1280,
        6_000,
        "main",
        None,
        AAC_YT,
    ),
    p(
        "ytv-720x1280-hevc-lite",
        "720×1280 vertical HEVC · 3 Mbps (slow upload)",
        Codec::Hevc,
        V1280,
        3_000,
        "main",
        None,
        AAC_YT,
    ),
    p(
        "ytv-720x1280-h264",
        "720×1280 vertical H.264 · 4 Mbps",
        Codec::H264,
        V1280,
        4_000,
        "high",
        None,
        AAC_YT,
    ),
];

// Ordered best → lowest: "the next lower preset" is the next usable entry.
// YouTube bitrates are at (never below) YouTube's recommended value for that
// codec/resolution, so Studio never shows "lower than recommended".
const YOUTUBE: &[Preset] = &[
    p(
        "yt-2160p60-hevc",
        "2160p60 HEVC · 35 Mbps (needs Normal latency)",
        Codec::Hevc,
        P2160,
        35_000,
        "main",
        None,
        AAC_YT,
    ),
    p(
        "yt-1440p60-hevc",
        "1440p60 HEVC · 24 Mbps (recommended)",
        Codec::Hevc,
        P1440,
        24_000,
        "main",
        None,
        AAC_YT,
    ),
    p(
        "yt-1440p60-h264",
        "1440p60 H.264 · 34 Mbps",
        Codec::H264,
        P1440,
        34_000,
        "high",
        None,
        AAC_YT,
    ),
    // For uploads around 20-30 Mbps: YouTube serves 1440p streams with its
    // higher-quality encodes (VP9/AV1), which look sharper even to 1080p
    // viewers. Below YouTube's recommended 24 Mbps (minimum is 6), so Studio
    // shows a "lower than recommended" note; the stream works.
    p(
        "yt-1440p60-hevc-lite",
        "1440p60 HEVC · 14 Mbps (sharper on YouTube; Studio shows a bitrate note)",
        Codec::Hevc,
        P1440,
        14_000,
        "main",
        None,
        AAC_YT,
    ),
    p(
        "yt-1080p60-h264",
        "1080p60 H.264 · 17 Mbps",
        Codec::H264,
        P1080,
        17_000,
        "high",
        None,
        AAC_YT,
    ),
    p(
        "yt-1080p60-hevc",
        "1080p60 HEVC · 12 Mbps (slower upload)",
        Codec::Hevc,
        P1080,
        12_000,
        "main",
        None,
        AAC_YT,
    ),
    p(
        "yt-720p60-h264",
        "720p60 H.264 · 8 Mbps (slow upload)",
        Codec::H264,
        P720,
        8_000,
        "high",
        None,
        AAC_YT,
    ),
];

const TWITCH: &[Preset] = &[
    p(
        "tw-1080p60",
        "1080p60 · 6 Mbps (recommended)",
        Codec::H264,
        P1080,
        6_000,
        "high",
        None,
        AAC_48_160,
    ),
    p(
        "tw-936p60",
        "936p60 · 6 Mbps (sharper motion)",
        Codec::H264,
        P936,
        6_000,
        "high",
        None,
        AAC_48_160,
    ),
    p(
        "tw-720p60",
        "720p60 · 4.5 Mbps (slow upload)",
        Codec::H264,
        P720,
        4_500,
        "high",
        None,
        AAC_48_160,
    ),
];

const KICK: &[Preset] = &[
    p(
        "kick-1080p60",
        "1080p60 · 8 Mbps (recommended)",
        Codec::H264,
        P1080,
        8_000,
        "main",
        None,
        AAC_48_160,
    ),
    p(
        "kick-1080p60-6m",
        "1080p60 · 6 Mbps",
        Codec::H264,
        P1080,
        6_000,
        "main",
        None,
        AAC_48_160,
    ),
    p(
        "kick-720p60",
        "720p60 · 4.5 Mbps (slow upload)",
        Codec::H264,
        P720,
        4_500,
        "main",
        None,
        AAC_48_160,
    ),
];

const FACEBOOK: &[Preset] = &[
    p(
        "fb-1080p60",
        "1080p60 · 9 Mbps (recommended)",
        Codec::H264,
        P1080,
        9_000,
        "high",
        Some("4.2"),
        AAC_48_128,
    ),
    p(
        "fb-1080p60-6m",
        "1080p60 · 6 Mbps",
        Codec::H264,
        P1080,
        6_000,
        "high",
        Some("4.2"),
        AAC_48_128,
    ),
    p(
        "fb-720p60",
        "720p60 · 4.5 Mbps (slow upload)",
        Codec::H264,
        P720,
        4_500,
        "high",
        Some("4.2"),
        AAC_48_128,
    ),
];

const CUSTOM: &[Preset] = &[
    p(
        "c-1440p60",
        "1440p60 H.264 · 16 Mbps",
        Codec::H264,
        P1440,
        16_000,
        "high",
        None,
        AAC_48_128,
    ),
    p(
        "c-1080p60-12m",
        "1080p60 H.264 · 12 Mbps",
        Codec::H264,
        P1080,
        12_000,
        "high",
        None,
        AAC_48_128,
    ),
    p(
        "c-1080p60",
        "1080p60 H.264 · 6 Mbps (safe default)",
        Codec::H264,
        P1080,
        6_000,
        "high",
        None,
        AAC_48_128,
    ),
    p(
        "c-720p60",
        "720p60 H.264 · 4.5 Mbps",
        Codec::H264,
        P720,
        4_500,
        "high",
        None,
        AAC_48_128,
    ),
];

impl Platform {
    pub fn parse(s: &str) -> Option<Self> {
        PLATFORMS.iter().copied().find(|p| p.id() == s)
    }

    pub fn id(self) -> &'static str {
        match self {
            Platform::Youtube => "youtube",
            Platform::YoutubeVertical => "ytvertical",
            Platform::Twitch => "twitch",
            Platform::Kick => "kick",
            Platform::Facebook => "facebook",
            Platform::Custom => "custom",
        }
    }

    /// Shows a stream of any shape (not only 16:9) correctly: YouTube does
    /// (phones show a phone-shaped stream full screen); custom servers are
    /// the user's call. Twitch, Kick and Facebook expect 16:9.
    pub fn any_shape(self) -> bool {
        matches!(self, Platform::Youtube | Platform::Custom)
    }

    pub fn name(self) -> &'static str {
        match self {
            Platform::Youtube => "YouTube",
            Platform::YoutubeVertical => "YouTube vertical",
            Platform::Twitch => "Twitch",
            Platform::Kick => "Kick",
            Platform::Facebook => "Facebook Gaming",
            Platform::Custom => "Custom server",
        }
    }

    pub fn presets(self) -> &'static [Preset] {
        match self {
            Platform::Youtube => YOUTUBE,
            Platform::YoutubeVertical => YOUTUBE_VERTICAL,
            Platform::Twitch => TWITCH,
            Platform::Kick => KICK,
            Platform::Facebook => FACEBOOK,
            Platform::Custom => CUSTOM,
        }
    }

    /// Server URL filled in for the streamer, if the platform has a fixed one.
    pub fn default_url(self) -> &'static str {
        match self {
            Platform::Youtube | Platform::YoutubeVertical => {
                "rtmps://a.rtmps.youtube.com:443/live2"
            }
            Platform::Twitch => "rtmps://live.twitch.tv:443/app",
            Platform::Facebook => "rtmps://live-api-s.facebook.com:443/rtmp",
            Platform::Kick | Platform::Custom => "",
        }
    }

    /// YouTube's backup ingest (used when "backup server" is on).
    pub fn backup_url(self) -> Option<&'static str> {
        (self == Platform::Youtube).then_some("rtmps://b.rtmps.youtube.com:443/live2?backup=1")
    }

    /// Plain-language summary of the official limits, shown next to the
    /// preset picker.
    pub fn limits_text(self) -> &'static str {
        match self {
            Platform::Youtube => "H.264 or HEVC, CBR, keyframe every 2 s (max 4 s), max 2 B-frames, AAC stereo 128 kbps. 1440p60: HEVC 6–24 Mbps, H.264 8–34 Mbps. 1080p60: HEVC 4–12, H.264 6–17. 4K needs Normal latency.",
            Platform::YoutubeVertical => "The vertical (9:16) half of YouTube Dual stream: the game across the middle, a blurred copy of it above and below, your banner under the game. YouTube waits for both streams before it starts. Lowered first when upload is short.",
            Platform::Twitch => "H.264 only, CBR, keyframe every 2 s, up to 1080p60 at 6 Mbps (7.5 Mbps max). AAC up to 160 kbps.",
            Platform::Kick => "H.264 Main profile only, CBR 1–8 Mbps, up to 1080p60, keyframe every 2 s, stereo audio up to 48 kHz.",
            Platform::Facebook => "H.264 (level 4.2 for 1080p60), 1080p60 4.5–9 Mbps, 720p 2.25–6 Mbps, keyframe every 2 s (max 4 s), AAC 128 kbps, 16:9.",
            Platform::Custom => "RTMP, RTMPS or SRT. H.264 High, CBR, keyframe every 2 s, AAC 128 kbps 48 kHz. 0.5–50 Mbps, up to 2160p60.",
        }
    }

    /// Where the streamer finds the key (and URL when it is per-account).
    pub fn key_hint(self) -> &'static str {
        match self {
            Platform::Youtube => {
                "studio.youtube.com → Create → Go live → Stream → copy Stream key."
            }
            Platform::YoutubeVertical => {
                "YouTube Studio → Go live → Stream settings → turn on Dual stream → in the right-hand box pick Encoder → copy the Vertical stream key."
            }
            Platform::Twitch => {
                "dashboard.twitch.tv → Settings → Stream → copy Primary Stream key."
            }
            Platform::Kick => {
                "kick.com → Creator dashboard → Settings → Stream URL & Key → copy both."
            }
            Platform::Facebook => {
                "facebook.com/live/producer → Streaming software → copy Server URL and Stream key."
            }
            Platform::Custom => {
                "Paste the server URL (rtmp://, rtmps:// or srt://) and key from your server."
            }
        }
    }

    pub fn needs_url(self) -> bool {
        matches!(self, Platform::Kick | Platform::Custom)
    }

    pub fn preset(self, id: &str) -> &'static Preset {
        let list = self.presets();
        list.iter().find(|p| p.id == id).unwrap_or(&list[0])
    }

    /// The preset a new destination starts with on this PC.
    pub fn default_preset(self, hevc: bool) -> &'static Preset {
        let list = self.presets();
        let first_usable = list.iter().find(|p| p.codec == Codec::H264 || hevc);
        match self {
            // 4K is opt-in only.
            Platform::Youtube => {
                if hevc {
                    self.preset("yt-1440p60-hevc")
                } else {
                    self.preset("yt-1080p60-h264")
                }
            }
            Platform::Custom => self.preset("c-1080p60"),
            Platform::YoutubeVertical => {
                if hevc {
                    self.preset("ytv-720x1280-hevc")
                } else {
                    self.preset("ytv-720x1280-h264")
                }
            }
            _ => first_usable.unwrap_or(&list[0]),
        }
    }
}

/// YouTube's published ingest table: (height, fps) → (codec → (min, recommended)) kbps.
fn youtube_range(codec: Codec, h: u32, fps: u32) -> Option<(u32, u32)> {
    let hi = fps > 30;
    let r = match (h, hi, codec) {
        (2160, true, Codec::Hevc) => (10_000, 35_000),
        (2160, true, Codec::H264) => (14_000, 50_000),
        (2160, false, Codec::Hevc) => (8_000, 30_000),
        (2160, false, Codec::H264) => (11_000, 42_000),
        (1440, true, Codec::Hevc) => (6_000, 24_000),
        (1440, true, Codec::H264) => (8_000, 34_000),
        (1440, false, Codec::Hevc) => (5_000, 15_000),
        (1440, false, Codec::H264) => (7_000, 21_000),
        (1080, true, Codec::Hevc) => (4_000, 12_000),
        (1080, true, Codec::H264) => (6_000, 17_000),
        (1080, false, Codec::Hevc) => (4_000, 10_000),
        (1080, false, Codec::H264) => (5_000, 14_000),
        (720, _, Codec::Hevc) => (2_000, 6_000),
        (720, _, Codec::H264) => (3_000, 8_000),
        _ => return None,
    };
    Some(r)
}

#[derive(Clone, Copy, PartialEq, Eq, Debug, Serialize, PartialOrd, Ord)]
#[serde(rename_all = "lowercase")]
pub enum Level {
    /// Good to know; nothing to fix.
    Note,
    /// Will work, but something is not ideal.
    Warning,
    /// Going live is blocked until this is fixed.
    Error,
}

#[derive(Clone, Debug, Serialize, PartialEq)]
pub struct Issue {
    pub level: Level,
    /// Destination id (None = applies to the whole stream).
    pub dest: Option<String>,
    pub text: String,
}

fn issue(level: Level, dest: &str, text: impl Into<String>) -> Issue {
    Issue {
        level,
        dest: Some(dest.to_string()),
        text: text.into(),
    }
}

fn mbps(kbps: u32) -> String {
    let m = kbps as f64 / 1000.0;
    if (m - m.round()).abs() < 0.05 {
        format!("{m:.0} Mbps")
    } else {
        format!("{m:.1} Mbps")
    }
}

/// Checks one preset against its platform's official limits.
pub fn check_preset(platform: Platform, pr: &Preset, dest: &str) -> Vec<Issue> {
    let mut out = Vec::new();
    let name = platform.name();
    let err = |out: &mut Vec<Issue>, t: String| out.push(issue(Level::Error, dest, t));
    if pr.fps > 60 {
        err(&mut out, format!("{name} accepts at most 60 fps."));
    }
    if pr.gop_s == 0 || pr.gop_s > 4 {
        err(
            &mut out,
            format!("{name} needs a keyframe at least every 4 seconds (2 s recommended)."),
        );
    } else if pr.gop_s != 2 && platform != Platform::Custom {
        out.push(issue(
            Level::Warning,
            dest,
            format!("{name} recommends a keyframe every 2 seconds."),
        ));
    }
    if pr.max_bframes > 2 {
        err(&mut out, format!("{name} allows at most 2 B-frames."));
    }
    if pr.audio.channels != 2 {
        err(&mut out, "Stereo audio is required.".into());
    }
    if platform == Platform::YoutubeVertical {
        if pr.w * 16 != pr.h * 9 {
            err(&mut out, "The vertical stream must be 9:16.".into());
        }
    } else if pr.w * 9 != pr.h * 16 && platform != Platform::Custom {
        err(&mut out, format!("{name} expects a 16:9 picture."));
    }
    match platform {
        Platform::Youtube => {
            match youtube_range(pr.codec, pr.h, pr.fps) {
                None => err(
                    &mut out,
                    format!(
                        "YouTube does not list {}p{} as a live resolution.",
                        pr.h, pr.fps
                    ),
                ),
                Some((min, rec)) => {
                    if pr.kbps < min {
                        err(
                            &mut out,
                            format!(
                                "YouTube needs at least {} for {}p{} — this is {}.",
                                mbps(min),
                                pr.h,
                                pr.fps,
                                mbps(pr.kbps)
                            ),
                        );
                    } else if pr.kbps < rec {
                        out.push(issue(
                            Level::Note,
                            dest,
                            format!(
                                "YouTube recommends {} for this; it will show \"bitrate lower than recommended\" but the stream works.",
                                mbps(rec)
                            ),
                        ));
                    }
                }
            }
            if pr.h >= 2160 {
                out.push(issue(
                    Level::Warning,
                    dest,
                    "4K only works with YouTube latency set to Normal (Low and Ultra-low reject 4K). Set it in YouTube Studio → Stream settings before going live.",
                ));
            }
            if ![44_100, 48_000].contains(&pr.audio.rate) || pr.audio.kbps > 384 {
                err(&mut out, "YouTube needs AAC at 44.1 or 48 kHz.".into());
            }
        }
        Platform::YoutubeVertical => {
            if ![44_100, 48_000].contains(&pr.audio.rate) || pr.audio.kbps > 384 {
                err(&mut out, "YouTube needs AAC at 44.1 or 48 kHz.".into());
            }
            if pr.kbps < 1_500 {
                err(
                    &mut out,
                    "The vertical stream needs at least 1.5 Mbps.".into(),
                );
            }
        }
        Platform::Twitch => {
            if pr.codec != Codec::H264 {
                err(&mut out, "Twitch only accepts H.264.".into());
            }
            if pr.h > 1080 {
                err(&mut out, "Twitch accepts at most 1080p.".into());
            }
            if pr.kbps > 7_500 {
                err(
                    &mut out,
                    format!("Twitch's maximum is 7.5 Mbps — this is {}.", mbps(pr.kbps)),
                );
            } else if pr.kbps > 6_000 {
                out.push(issue(
                    Level::Warning,
                    dest,
                    "Above 6 Mbps only works on channels with Enhanced Broadcasting; others may buffer.",
                ));
            }
            if pr.audio.kbps > 320 {
                err(&mut out, "Twitch accepts at most 320 kbps audio.".into());
            }
        }
        Platform::Kick => {
            if pr.codec != Codec::H264 {
                err(&mut out, "Kick only accepts H.264.".into());
            }
            if pr.profile != "main" {
                err(&mut out, "Kick needs the H.264 Main profile.".into());
            }
            if pr.h > 1080 {
                err(&mut out, "Kick accepts at most 1080p60.".into());
            }
            if !(1_000..=8_000).contains(&pr.kbps) {
                err(
                    &mut out,
                    format!("Kick accepts 1–8 Mbps — this is {}.", mbps(pr.kbps)),
                );
            }
            if pr.audio.rate > 48_000 {
                err(&mut out, "Kick accepts audio up to 48 kHz.".into());
            }
        }
        Platform::Facebook => {
            if pr.codec != Codec::H264 {
                err(&mut out, "Facebook only accepts H.264.".into());
            }
            if pr.h > 1080 {
                err(&mut out, "Facebook accepts at most 1080p.".into());
            }
            let (min, max) = if pr.h >= 1080 {
                (4_500, 9_000)
            } else {
                (2_250, 6_000)
            };
            if !(min..=max).contains(&pr.kbps) {
                err(
                    &mut out,
                    format!(
                        "Facebook accepts {}–{} at {}p — this is {}.",
                        mbps(min),
                        mbps(max),
                        pr.h,
                        mbps(pr.kbps)
                    ),
                );
            }
            if pr.h >= 1080 && pr.fps > 30 && pr.level != Some("4.2") {
                err(
                    &mut out,
                    "Facebook needs H.264 level 4.2 for 1080p60.".into(),
                );
            }
            if !(128..=256).contains(&pr.audio.kbps) || ![44_100, 48_000].contains(&pr.audio.rate) {
                err(
                    &mut out,
                    "Facebook needs AAC 128–256 kbps at 44.1 or 48 kHz.".into(),
                );
            }
        }
        Platform::Custom => {
            if !(500..=50_000).contains(&pr.kbps) {
                err(&mut out, "Custom bitrate must be 0.5–50 Mbps.".into());
            }
            if pr.h > 2160 {
                err(&mut out, "At most 2160p.".into());
            }
        }
    }
    out
}

/// Checks the server URL the streamer gave (or the platform default).
pub fn check_url(platform: Platform, url: &str, dest: &str) -> Vec<Issue> {
    let u = url.trim().to_ascii_lowercase();
    let name = platform.name();
    let mut out = Vec::new();
    if u.is_empty() {
        out.push(issue(
            Level::Error,
            dest,
            format!("Paste the {name} server URL ({}).", platform.key_hint()),
        ));
        return out;
    }
    let ok_scheme = match platform {
        Platform::Custom => {
            u.starts_with("rtmp://") || u.starts_with("rtmps://") || u.starts_with("srt://")
        }
        Platform::Twitch => u.starts_with("rtmps://") || u.starts_with("rtmp://"),
        // A server on this PC (a local test or relay) needs no encryption.
        _ => u.starts_with("rtmps://") || u.starts_with("rtmp://127.0.0.1"),
    };
    if !ok_scheme {
        out.push(issue(
            Level::Error,
            dest,
            match platform {
                Platform::Custom => {
                    "The server URL must start with rtmp://, rtmps:// or srt://".to_string()
                }
                Platform::Twitch => {
                    "The Twitch server must start with rtmps:// or rtmp://".to_string()
                }
                _ => format!("The {name} server must start with rtmps:// (encrypted)."),
            },
        ));
    }
    if u.contains(char::is_whitespace) {
        out.push(issue(
            Level::Error,
            dest,
            "The server URL contains a space.",
        ));
    }
    out
}

/// Joins server URL and stream key the way OBS does.
pub fn compose_url(platform: Platform, server: &str, key: &str, bandwidth_test: bool) -> String {
    let server = server.trim().trim_end_matches('/');
    let mut key = key.trim().to_string();
    if platform == Platform::Twitch && bandwidth_test && !key.contains("bandwidthtest") {
        key.push_str("?bandwidthtest=true");
    }
    if server.to_ascii_lowercase().starts_with("srt://") {
        if key.is_empty() || server.contains("streamid=") {
            return server.to_string();
        }
        let sep = if server.contains('?') { '&' } else { '?' };
        return format!("{server}{sep}streamid={key}");
    }
    if key.is_empty() {
        server.to_string()
    } else {
        format!("{server}/{key}")
    }
}

/// One destination as configured by the streamer.
#[derive(Clone, Debug)]
pub struct DestInput {
    pub id: String,
    pub platform: Platform,
    pub preset: String,
    pub main: bool,
    pub url: String,
    pub has_key: bool,
}

#[derive(Clone, Debug, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct Chosen {
    pub dest: String,
    pub preset: String,
    pub label: String,
    pub upload_kbps: u32,
    /// True when PhoneCast picked a lower preset than the one set.
    pub lowered: bool,
}

#[derive(Clone, Debug, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct Plan {
    pub issues: Vec<Issue>,
    pub chosen: Vec<Chosen>,
    pub total_kbps: u32,
    /// Upload usable for streaming (70% of measured), if measured.
    pub budget_kbps: Option<u32>,
    pub can_start: bool,
}

/// Share of the measured upload a stream may use.
pub const HEADROOM: f64 = 0.70;

/// Validates all enabled destinations and fits them into the upload budget.
///
/// `upload_mbps` is the measured upload (the minimum over the test, not the
/// mean). When the total does not fit into 70% of it, non-main destinations
/// step down to their next lower preset first; the main destination is
/// lowered only after every other one is at its lowest.
pub fn plan(dests: &[DestInput], upload_mbps: Option<f64>, hevc: bool) -> Plan {
    let mut issues = Vec::new();
    if dests.is_empty() {
        issues.push(Issue {
            level: Level::Error,
            dest: None,
            text: "Add at least one platform to stream to.".into(),
        });
    }
    let mains = dests.iter().filter(|d| d.main).count();
    if dests.len() > 1 && mains == 0 {
        issues.push(Issue {
            level: Level::Note,
            dest: None,
            text:
                "No platform is marked Main; the first one keeps full quality if upload is short."
                    .into(),
        });
    }
    let main_idx = dests.iter().position(|d| d.main).unwrap_or(0);

    // Starting presets; HEVC falls back to the H.264 twin when the GPU can't.
    let mut cur: Vec<&'static Preset> = dests
        .iter()
        .map(|d| {
            let pr = d.platform.preset(&d.preset);
            if pr.codec == Codec::Hevc && !hevc {
                let twin = d
                    .platform
                    .presets()
                    .iter()
                    .find(|q| q.codec == Codec::H264 && q.h == pr.h && q.fps == pr.fps)
                    .unwrap_or_else(|| d.platform.default_preset(false));
                issues.push(issue(
                    Level::Warning,
                    &d.id,
                    format!(
                        "This PC's encoder can't do HEVC; using {} instead.",
                        twin.label
                    ),
                ));
                twin
            } else {
                pr
            }
        })
        .collect();

    for (d, pr) in dests.iter().zip(&cur) {
        issues.extend(check_preset(d.platform, pr, &d.id));
        let url = if d.platform.needs_url() || !d.url.trim().is_empty() {
            d.url.as_str()
        } else {
            d.platform.default_url()
        };
        issues.extend(check_url(d.platform, url, &d.id));
        if !d.has_key && d.platform != Platform::Custom {
            issues.push(issue(
                Level::Error,
                &d.id,
                format!(
                    "Add your {} stream key. {}",
                    d.platform.name(),
                    d.platform.key_hint()
                ),
            ));
        }
    }
    let same_platform = |p: Platform| dests.iter().filter(|d| d.platform == p).count() > 1;
    for p in PLATFORMS {
        if p != Platform::Custom && same_platform(p) {
            issues.push(Issue {
                level: Level::Warning,
                dest: None,
                text: format!(
                    "{} is added twice; make sure each uses a different stream key.",
                    p.name()
                ),
            });
        }
    }

    let start: Vec<&'static Preset> = cur.clone();
    let total = |c: &[&'static Preset]| c.iter().map(|p| p.upload_kbps()).sum::<u32>();
    let budget = upload_mbps
        .filter(|m| *m > 0.0)
        .map(|m| (m * 1000.0 * HEADROOM) as u32);
    if let Some(budget) = budget {
        // Lower others first (largest first), the main one last.
        let lower = |c: &mut Vec<&'static Preset>, i: usize| -> bool {
            let d = &dests[i];
            let list = d.platform.presets();
            let now = c[i];
            let next = list
                .iter()
                .skip_while(|q| q.id != now.id)
                .skip(1)
                .find(|q| q.upload_kbps() < now.upload_kbps() && (q.codec == Codec::H264 || hevc));
            match next {
                Some(n) => {
                    c[i] = n;
                    true
                }
                None => false,
            }
        };
        while total(&cur) > budget {
            let mut others: Vec<usize> = (0..dests.len()).filter(|i| *i != main_idx).collect();
            others.sort_by_key(|i| std::cmp::Reverse(cur[*i].upload_kbps()));
            let stepped = others.into_iter().any(|i| lower(&mut cur, i))
                || (!dests.is_empty() && lower(&mut cur, main_idx));
            if !stepped {
                break;
            }
        }
        let t = total(&cur);
        if t > budget {
            issues.push(Issue {
                level: Level::Error,
                dest: None,
                text: format!(
                    "Your upload ({:.1} Mbps measured) is too slow for this: even the lowest settings need {} plus 30% headroom. Stream to fewer platforms.",
                    upload_mbps.unwrap_or(0.0),
                    mbps(t)
                ),
            });
        } else if cur != start {
            issues.push(Issue {
                level: Level::Warning,
                dest: None,
                text: format!(
                    "Upload is {:.1} Mbps: quality lowered on {} so everything fits with 30% headroom.",
                    upload_mbps.unwrap_or(0.0),
                    dests
                        .iter()
                        .zip(cur.iter().zip(&start))
                        .filter(|(_, (a, b))| a.id != b.id)
                        .map(|(d, _)| d.platform.name())
                        .collect::<Vec<_>>()
                        .join(", ")
                ),
            });
        }
    } else if !dests.is_empty() {
        issues.push(Issue {
            level: Level::Note,
            dest: None,
            text: "Upload speed not measured yet — run the setup check so PhoneCast can make sure the stream fits.".into(),
        });
    }
    // Lowered presets must still be valid (they are, by construction, but
    // keep the check honest).
    for (i, (d, pr)) in dests.iter().zip(&cur).enumerate() {
        if pr.id != start[i].id {
            issues.extend(
                check_preset(d.platform, pr, &d.id)
                    .into_iter()
                    .filter(|x| x.level == Level::Error),
            );
        }
    }
    issues.sort_by_key(|i| std::cmp::Reverse(i.level));
    issues.dedup();
    let can_start = !issues.iter().any(|i| i.level == Level::Error);
    Plan {
        chosen: dests
            .iter()
            .zip(cur.iter().zip(&start))
            .map(|(d, (c, s))| Chosen {
                dest: d.id.clone(),
                preset: c.id.into(),
                label: c.label.into(),
                upload_kbps: c.upload_kbps(),
                lowered: c.id != s.id,
            })
            .collect(),
        total_kbps: total(&cur),
        budget_kbps: budget,
        issues,
        can_start,
    }
}

/// Recommended preset per platform for a measured upload, as if streaming
/// to that platform alone (the wizard's suggestion).
pub fn recommend(platform: Platform, upload_mbps: f64, hevc: bool) -> &'static Preset {
    let budget = (upload_mbps * 1000.0 * HEADROOM) as u32;
    let start = platform.default_preset(hevc);
    platform
        .presets()
        .iter()
        .skip_while(|q| q.id != start.id)
        .filter(|q| q.codec == Codec::H264 || hevc)
        .find(|q| q.upload_kbps() <= budget)
        .unwrap_or_else(|| platform.presets().last().unwrap())
}

/// Everything the Go live window needs to draw the platform pickers.
pub fn catalog_json(hevc: bool) -> Value {
    Value::Array(
        PLATFORMS
            .iter()
            .map(|p| {
                json!({
                    "id": p.id(),
                    "name": p.name(),
                    "defaultUrl": p.default_url(),
                    "backupUrl": p.backup_url(),
                    "needsUrl": p.needs_url(),
                    "keyHint": p.key_hint(),
                    "limits": p.limits_text(),
                    "defaultPreset": p.default_preset(hevc).id,
                    "presets": p.presets().iter()
                        .filter(|q| q.codec == Codec::H264 || hevc)
                        .map(|q| q.json()).collect::<Vec<_>>(),
                })
            })
            .collect(),
    )
}

#[cfg(test)]
mod tests {
    use super::*;

    fn d(id: &str, platform: Platform, preset: &str, main: bool) -> DestInput {
        DestInput {
            id: id.into(),
            platform,
            preset: preset.into(),
            main,
            url: if platform.needs_url() {
                "rtmps://abc.global-contribute.live-video.net:443/app".into()
            } else {
                String::new()
            },
            has_key: true,
        }
    }

    fn errors(p: &Plan) -> Vec<&str> {
        p.issues
            .iter()
            .filter(|i| i.level == Level::Error)
            .map(|i| i.text.as_str())
            .collect()
    }

    #[test]
    fn every_builtin_preset_passes_its_platform_limits() {
        for pf in PLATFORMS {
            for pr in pf.presets() {
                let errs: Vec<_> = check_preset(pf, pr, "x")
                    .into_iter()
                    .filter(|i| i.level == Level::Error)
                    .collect();
                assert!(errs.is_empty(), "{} {}: {errs:?}", pf.id(), pr.id);
                assert_eq!(pr.gop_s, 2);
                assert!(pr.max_bframes <= 2);
                assert_eq!(pr.audio.channels, 2);
            }
        }
    }

    #[test]
    fn youtube_presets_never_trigger_lower_than_recommended() {
        for pr in YOUTUBE.iter().filter(|p| !p.id.ends_with("-lite")) {
            let (min, rec) = youtube_range(pr.codec, pr.h, pr.fps).unwrap();
            assert!(pr.kbps >= rec, "{} is below YouTube's recommended", pr.id);
            assert!(pr.kbps >= min);
            assert!(check_preset(Platform::Youtube, pr, "x")
                .iter()
                .all(|i| i.level != Level::Note));
        }
    }

    #[test]
    fn the_real_world_failures_are_caught() {
        // "bitrate 12762 lower than recommended 13500"-style setups.
        let low = Preset {
            kbps: 5_000,
            ..*Platform::Youtube.preset("yt-1080p60-h264")
        };
        let r = check_preset(Platform::Youtube, &low, "yt");
        assert!(r
            .iter()
            .any(|i| i.level == Level::Error && i.text.contains("at least 6 Mbps")));
        // Unsupported resolution.
        let odd = Preset {
            h: 1200,
            w: 2133,
            ..*Platform::Youtube.preset("yt-1080p60-h264")
        };
        assert!(check_preset(Platform::Youtube, &odd, "yt")
            .iter()
            .any(|i| i.level == Level::Error));
        // 4K is allowed only with the latency warning.
        let uhd = Platform::Youtube.preset("yt-2160p60-hevc");
        let r = check_preset(Platform::Youtube, uhd, "yt");
        assert!(r
            .iter()
            .any(|i| i.level == Level::Warning && i.text.contains("Normal")));
        assert!(r.iter().all(|i| i.level != Level::Error));
    }

    #[test]
    fn platform_specific_rules() {
        let base = *Platform::Twitch.preset("tw-1080p60");
        let hevc = Preset {
            codec: Codec::Hevc,
            ..base
        };
        assert!(check_preset(Platform::Twitch, &hevc, "t")
            .iter()
            .any(|i| i.text.contains("only accepts H.264")));
        let fast = Preset {
            kbps: 7_000,
            ..base
        };
        let r = check_preset(Platform::Twitch, &fast, "t");
        assert!(r.iter().any(|i| i.level == Level::Warning));
        assert!(r.iter().all(|i| i.level != Level::Error));
        let too_fast = Preset {
            kbps: 8_000,
            ..base
        };
        assert!(check_preset(Platform::Twitch, &too_fast, "t")
            .iter()
            .any(|i| i.level == Level::Error));
        // Kick: Main profile only, 8 Mbps cap.
        let k = *Platform::Kick.preset("kick-1080p60");
        assert!(check_preset(
            Platform::Kick,
            &Preset {
                profile: "high",
                ..k
            },
            "k"
        )
        .iter()
        .any(|i| i.text.contains("Main profile")));
        assert!(
            check_preset(Platform::Kick, &Preset { kbps: 9_000, ..k }, "k")
                .iter()
                .any(|i| i.level == Level::Error)
        );
        // Facebook: level 4.2 for 1080p60, 9 Mbps max.
        let f = *Platform::Facebook.preset("fb-1080p60");
        assert!(
            check_preset(Platform::Facebook, &Preset { level: None, ..f }, "f")
                .iter()
                .any(|i| i.text.contains("4.2"))
        );
        assert!(
            check_preset(Platform::Facebook, &Preset { kbps: 10_000, ..f }, "f")
                .iter()
                .any(|i| i.level == Level::Error)
        );
        // Keyframes over 4 s are rejected everywhere.
        let g = Preset { gop_s: 5, ..base };
        assert!(check_preset(Platform::Twitch, &g, "t")
            .iter()
            .any(|i| i.level == Level::Error));
    }

    #[test]
    fn urls_are_checked_and_composed() {
        assert!(check_url(Platform::Kick, "", "k")[0].level == Level::Error);
        assert!(check_url(Platform::Kick, "rtmp://insecure", "k")
            .iter()
            .any(|i| i.level == Level::Error));
        assert!(check_url(Platform::Custom, "srt://1.2.3.4:9000", "c").is_empty());
        assert!(check_url(Platform::Custom, "http://x", "c")[0].level == Level::Error);
        assert_eq!(
            compose_url(
                Platform::Youtube,
                "rtmps://a.rtmps.youtube.com:443/live2/",
                "abcd",
                false
            ),
            "rtmps://a.rtmps.youtube.com:443/live2/abcd"
        );
        assert_eq!(
            compose_url(
                Platform::Youtube,
                Platform::Youtube.backup_url().unwrap(),
                "abcd",
                false
            ),
            "rtmps://b.rtmps.youtube.com:443/live2?backup=1/abcd"
        );
        assert_eq!(
            compose_url(
                Platform::Twitch,
                "rtmps://live.twitch.tv:443/app",
                "live_1",
                true
            ),
            "rtmps://live.twitch.tv:443/app/live_1?bandwidthtest=true"
        );
        assert_eq!(
            compose_url(
                Platform::Custom,
                "srt://h:9000?latency=2000000",
                "k1",
                false
            ),
            "srt://h:9000?latency=2000000&streamid=k1"
        );
    }

    #[test]
    fn missing_key_and_empty_list_block_going_live() {
        let p = plan(&[], Some(50.0), true);
        assert!(!p.can_start);
        let mut yt = d("yt", Platform::Youtube, "yt-1440p60-hevc", true);
        yt.has_key = false;
        let p = plan(&[yt], Some(50.0), true);
        assert!(!p.can_start);
        assert!(errors(&p)[0].contains("stream key"));
    }

    #[test]
    fn hevc_falls_back_to_h264_twin() {
        let p = plan(
            &[d("yt", Platform::Youtube, "yt-1440p60-hevc", true)],
            Some(100.0),
            false,
        );
        assert!(p.can_start, "{:?}", p.issues);
        assert_eq!(p.chosen[0].preset, "yt-1440p60-h264");
        assert!(p.issues.iter().any(|i| i.text.contains("can't do HEVC")));
    }

    #[test]
    fn short_upload_lowers_secondary_first_and_keeps_main() {
        let dests = [
            d("yt", Platform::Youtube, "yt-1440p60-hevc", true),
            d("tw", Platform::Twitch, "tw-1080p60", false),
        ];
        // Plenty: nothing changes.
        let p = plan(&dests, Some(60.0), true);
        assert!(p.can_start);
        assert!(p.chosen.iter().all(|c| !c.lowered));
        // 24.2 + 6.3 = ~31.6 Mbps; 70% of 43 = 30.1 → Twitch steps down only.
        let p = plan(&dests, Some(43.0), true);
        assert!(p.can_start, "{:?}", p.issues);
        assert_eq!(p.chosen[0].preset, "yt-1440p60-hevc");
        assert!(!p.chosen[0].lowered);
        assert!(p.chosen[1].lowered);
        assert!(p.total_kbps <= p.budget_kbps.unwrap());
        assert!(p
            .issues
            .iter()
            .any(|i| i.level == Level::Warning && i.text.contains("Twitch")));
    }

    #[test]
    fn main_is_lowered_only_after_others_are_at_minimum() {
        let dests = [
            d("tw", Platform::Twitch, "tw-1080p60", false),
            d("yt", Platform::Youtube, "yt-1440p60-hevc", true),
        ];
        // 70% of 25 = 17.5 Mbps: Twitch goes to 720p (4.8), YouTube must drop to fit.
        let p = plan(&dests, Some(25.0), true);
        assert!(p.can_start, "{:?}", p.issues);
        assert_eq!(p.chosen[0].preset, "tw-720p60");
        assert!(p.chosen[1].lowered);
        assert!(p.total_kbps <= p.budget_kbps.unwrap());
    }

    #[test]
    fn vertical_feed_shrinks_first_and_the_main_keeps_its_quality() {
        let dests = [
            d("yt", Platform::Youtube, "yt-1080p60-hevc", true),
            d("v", Platform::YoutubeVertical, "ytv-720x1280-hevc", false),
        ];
        // The user's measured 22.8 Mbps: 70% = 16 Mbps for both.
        let p = plan(&dests, Some(22.8), true);
        assert!(p.can_start, "{:?}", p.issues);
        assert_eq!(p.chosen[0].preset, "yt-1080p60-hevc");
        assert!(!p.chosen[0].lowered);
        assert_eq!(p.chosen[1].preset, "ytv-720x1280-hevc-lite");
        assert!(p.total_kbps <= p.budget_kbps.unwrap());
    }

    #[test]
    fn hopeless_upload_is_a_clear_error() {
        let p = plan(
            &[
                d("yt", Platform::Youtube, "yt-1440p60-hevc", true),
                d("tw", Platform::Twitch, "tw-1080p60", false),
            ],
            Some(5.0),
            true,
        );
        assert!(!p.can_start);
        assert!(errors(&p).iter().any(|e| e.contains("too slow")));
    }

    #[test]
    fn upload_includes_audio_and_overhead() {
        let pr = Platform::Twitch.preset("tw-1080p60");
        assert_eq!(
            pr.upload_kbps(),
            ((6_000 + 160) as f64 * 1.03).ceil() as u32
        );
        assert_eq!(pr.gop_frames(), 120);
    }

    #[test]
    fn recommendations_follow_upload() {
        assert_eq!(
            recommend(Platform::Youtube, 50.0, true).id,
            "yt-1440p60-hevc"
        );
        assert_eq!(
            recommend(Platform::Youtube, 50.0, false).id,
            "yt-1080p60-h264"
        );
        assert_eq!(
            recommend(Platform::Youtube, 12.0, true).id,
            "yt-720p60-h264"
        );
        assert_eq!(recommend(Platform::Twitch, 20.0, false).id, "tw-1080p60");
        assert_eq!(recommend(Platform::Twitch, 7.0, false).id, "tw-720p60");
        assert_eq!(recommend(Platform::Kick, 3.0, false).id, "kick-720p60");
    }

    /// Writes UI fixtures (catalog + a sample plan) for the browser mock.
    /// `PHONECAST_FIXTURE=<dir> cargo test dump_ui_fixture -- --ignored`
    #[test]
    #[ignore]
    fn dump_ui_fixture() {
        let dir = std::path::PathBuf::from(std::env::var("PHONECAST_FIXTURE").unwrap());
        let mut tw = d("tw", Platform::Twitch, "tw-1080p60", false);
        tw.has_key = false;
        let dests = [
            d("yt", Platform::Youtube, "yt-1440p60-hevc", true),
            tw,
            d("kick", Platform::Kick, "kick-1080p60", false),
        ];
        let plan = plan(&dests, Some(38.0), true);
        let v = json!({ "catalog": catalog_json(true), "plan": plan });
        std::fs::write(
            dir.join("fixture.json"),
            serde_json::to_vec_pretty(&v).unwrap(),
        )
        .unwrap();
    }

    #[test]
    fn catalog_hides_hevc_when_unsupported() {
        let c = catalog_json(false);
        let yt = &c[0];
        assert!(yt["presets"]
            .as_array()
            .unwrap()
            .iter()
            .all(|p| p["codec"] == "h264"));
        assert_eq!(yt["defaultPreset"], "yt-1080p60-h264");
    }
}
