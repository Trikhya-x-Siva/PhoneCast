//! Stream profiles: set up each kind of stream once, reuse it every time.
//!
//! Two layers:
//! - **Defaults** shared by every stream: channel name, links, schedule,
//!   description footer, chat rules, common tags and hashtags.
//! - **Profiles** (BGMI, custom rooms, eFootball, PC game, …): title and
//!   description templates, tags, Twitch category, banner label, thumbnail
//!   text and colours, Discord announcement. A profile only overrides what is
//!   different; everything else comes from the defaults.
//!
//! Templates use `{name}` variables (see [`VARIABLES`]). Rendering is pure
//! and testable; network actions (Twitch channel update, Discord post) go
//! through Windows' own curl.exe with secrets passed on stdin, never on the
//! command line.
//!
//! YouTube is filled by copy-paste (the user chose not to set up a Google
//! Cloud project); YouTube Studio keeps its stream settings between streams.

use crate::live::dpapi;
use serde::{Deserialize, Serialize};
use serde_json::{json, Value};
use std::collections::HashMap;
use std::io::Write;
use std::path::PathBuf;
use std::process::{Command, Stdio};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

#[cfg(windows)]
use std::os::windows::process::CommandExt;

/// Template variables, shown in the editor.
pub const VARIABLES: &[(&str, &str)] = &[
    ("channel", "Channel name"),
    ("game", "Game name"),
    ("mode", "Mode / what you're doing (e.g. Rank push)"),
    ("n", "Stream number for this profile (counts up by itself)"),
    ("date", "Date, e.g. 19 Oct"),
    ("day", "Weekday, e.g. Saturday"),
    ("time", "Start time, e.g. 9:30 PM"),
    ("room", "Custom room ID"),
    ("pass", "Custom room password"),
    ("map", "Map"),
    ("youtube", "YouTube live link (always your current stream)"),
    ("channel_link", "YouTube channel link"),
    ("join", "YouTube membership (Join) link"),
    ("twitch", "Twitch channel link"),
    ("instagram", "Instagram link"),
    ("discord", "Discord invite"),
    ("schedule", "Your stream schedule"),
    ("rules", "Chat rules"),
    ("email", "Business email"),
    ("goal", "Channel goal"),
    ("device", "Phone you play on"),
    ("pc", "PC specs"),
    ("disclaimer", "This profile's disclaimer"),
    ("tags", "Hashtags"),
];

// ---------------- data ----------------

#[derive(Serialize, Deserialize, Clone, Debug)]
#[serde(rename_all = "camelCase", default)]
pub struct Defaults {
    pub channel: String,
    /// YouTube handle without @, used for the stable live link youtube.com/@x/live.
    pub youtube_handle: String,
    pub twitch_login: String,
    pub instagram: String,
    pub discord_invite: String,
    pub schedule: String,
    /// Business / collab email (optional).
    pub email: String,
    /// e.g. "Road to 1K subscribers".
    pub goal: String,
    /// Phone used for mobile games, e.g. "OnePlus 13R".
    pub device: String,
    /// PC specs, e.g. "Intel i5-13450HX · RTX 4050".
    pub pc: String,
    pub rules: String,
    /// Appended under every profile's description.
    pub footer: String,
    pub tags: String,
    pub hashtags: String,
    /// "ta" (Tamil). Twitch stream language.
    pub language: String,
    /// Twitch Client ID of the streamer's own public app (not a secret).
    pub twitch_client_id: String,
}

impl Default for Defaults {
    fn default() -> Self {
        Self {
            channel: "Your channel".into(),
            youtube_handle: String::new(),
            twitch_login: String::new(),
            instagram: String::new(),
            discord_invite: String::new(),
            schedule: "Daily 9 PM – 12 AM".into(),
            email: String::new(),
            goal: "Road to 1K subscribers".into(),
            device: String::new(),
            pc: String::new(),
            rules: "❌ No spam or self-promotion\n\
                    ❌ No racism, abuse or swearing\n\
                    ❌ No trash talk about other streamers\n\
                    ❌ No politics or religion\n\
                    ❌ Don't share personal info (yours or anyone's)\n\
                    ✅ Respect the mods — their decision is final"
                .into(),
            footer: "👋 Welcome to {channel}! Grab a seat — let's play together 🎮\n\n\
                     🔗 FOLLOW & CONNECT:\n\
                     ▶ YouTube: {channel_link}\n\
                     🟣 Twitch: {twitch}\n\
                     📸 Instagram: {instagram}\n\
                     💬 Discord: {discord}\n\
                     📧 Business: {email}\n\n\
                     💜 SUPPORT THE STREAM:\n\
                     ⭐ Join membership: {join}\n\
                     💬 Super Chat & Super Thanks are welcome — only if you meet the age limit. \
                     Watching, liking and chatting is support too!\n\n\
                     🎯 Goal: {goal}\n\
                     📅 Schedule: {schedule}\n\n\
                     🎮 MY SETUP:\n\
                     📱 Phone: {device}\n\
                     💻 PC: {pc}\n\n\
                     ★ CHAT RULES ★\n{rules}\n\n\
                     {disclaimer}\n\n\
                     {tags}"
                .into(),
            tags: "tamil gaming, tamil live, tamil gamer, tamil live stream".into(),
            hashtags: "#tamilgaming #tamillive".into(),
            language: "ta".into(),
            twitch_client_id: String::new(),
        }
    }
}

#[derive(Serialize, Deserialize, Clone, Debug, Default)]
#[serde(rename_all = "camelCase", default)]
pub struct Thumb {
    pub headline: String,
    pub subline: String,
    pub color1: String,
    pub color2: String,
    /// Use the current game picture as the background.
    pub use_frame: bool,
}

#[derive(Serialize, Deserialize, Clone, Debug, Default)]
#[serde(rename_all = "camelCase", default)]
pub struct TwitchCategory {
    pub id: String,
    pub name: String,
}

#[derive(Serialize, Deserialize, Clone, Debug, Default)]
#[serde(rename_all = "camelCase", default)]
pub struct Profile {
    pub id: String,
    pub name: String,
    /// "phone" | "rooms" | "pc": decides which per-stream fields are asked.
    pub kind: String,
    pub game: String,
    pub mode: String,
    pub title: String,
    pub description: String,
    pub tags: String,
    pub hashtags: String,
    pub banner_label: String,
    pub twitch: TwitchCategory,
    pub twitch_tags: String,
    pub thumb: Thumb,
    /// Post "I'm live" to Discord when the stream starts.
    pub announce: bool,
    pub announce_text: String,
    /// Mention @everyone in the Discord post.
    pub announce_everyone: bool,
    /// Custom rooms: put the room card on the banner when going live.
    pub room_on_banner: bool,
    /// Short honest disclaimer (trademark owner, viewer discretion).
    pub disclaimer: String,
    /// Stream number, counts up each time a stream starts with this profile.
    pub count: u32,
}

#[derive(Serialize, Deserialize, Clone, Debug)]
#[serde(rename_all = "camelCase", default)]
pub struct Store {
    pub defaults: Defaults,
    pub profiles: Vec<Profile>,
    pub active: String,
    /// Discord webhook URL, DPAPI-encrypted (it lets anyone post as the hook).
    discord_webhook_enc: String,
    /// Twitch token (DPAPI-encrypted JSON).
    twitch_token_enc: String,
    /// Version of the ready-made texts this store was written with
    /// (0 = the first, Tamil-language version). Field-level default so a
    /// file without it reads as 0, not as the struct default.
    #[serde(default)]
    text_version: u32,
}

/// 1 = ready-made texts in English; 2 = researched South Indian style
/// (sections, disclaimers, one profile per game).
const TEXT_VERSION: u32 = 2;

impl Default for Store {
    fn default() -> Self {
        Self {
            defaults: Defaults::default(),
            profiles: starter_profiles(),
            active: "bgmi".into(),
            discord_webhook_enc: String::new(),
            twitch_token_enc: String::new(),
            text_version: TEXT_VERSION,
        }
    }
}

fn has_tamil(s: &str) -> bool {
    s.chars().any(|c| ('\u{0B80}'..='\u{0BFF}').contains(&c))
}

/// Brings a store written by an older version up to the current
/// ready-made texts. The built-in profiles get the new templates (stream
/// counts, the chosen Twitch category and on/off switches are kept); new
/// built-in profiles are added; personal details (channel, links, keys) and
/// the user's own profiles are never touched.
fn upgrade_texts(s: &mut Store) -> bool {
    if s.text_version >= TEXT_VERSION {
        return false;
    }
    let d = Defaults::default();
    s.defaults.rules = d.rules;
    s.defaults.footer = d.footer;
    if s.text_version < 1 || has_tamil(&s.defaults.tags) {
        s.defaults.tags = d.tags;
        s.defaults.hashtags = d.hashtags;
    }
    for st in starter_profiles() {
        match s.profiles.iter_mut().find(|p| p.id == st.id) {
            Some(p) => {
                let keep = (
                    p.count,
                    p.twitch.clone(),
                    p.announce,
                    p.announce_everyone,
                    p.room_on_banner,
                );
                *p = st;
                (
                    p.count,
                    p.twitch,
                    p.announce,
                    p.announce_everyone,
                    p.room_on_banner,
                ) = keep;
                if p.twitch.name.is_empty() {
                    p.twitch = starter_profiles()
                        .into_iter()
                        .find(|x| x.id == p.id)
                        .unwrap()
                        .twitch;
                }
            }
            None => s.profiles.push(st),
        }
    }
    s.text_version = TEXT_VERSION;
    true
}

/// Ready-made profiles, modelled on how big South Indian gaming channels
/// write live titles and descriptions (researched Oct 2026): "🔴 GAME LIVE
/// TAMIL | hook | Day N #tag", a short "today" block, join instructions for
/// rooms, an honest disclaimer, 5–10 hashtags. The shared footer (links,
/// support, setup, chat rules) is added under every description.
pub fn starter_profiles() -> Vec<Profile> {
    let thumb = |h: &str, s: &str, c1: &str, c2: &str| Thumb {
        headline: h.into(),
        subline: s.into(),
        color1: c1.into(),
        color2: c2.into(),
        use_frame: true,
    };
    let cat = |name: &str| TwitchCategory {
        id: String::new(),
        name: name.into(),
    };
    let pc = |id: &str,
              name: &str,
              game: &str,
              mode: &str,
              title: &str,
              emoji: &str,
              about: &str,
              tags: &str,
              hashtags: &str,
              banner: &str,
              twitch: &str,
              disclaimer: &str,
              c1: &str,
              c2: &str| Profile {
        id: id.into(),
        name: name.into(),
        kind: "pc".into(),
        game: game.into(),
        mode: mode.into(),
        title: title.into(),
        description: format!(
            "{emoji} {} LIVE TAMIL — {{mode}}!\n\
             🎮 Game: {{game}} · 💻 Platform: PC · 🗣️ Language: Tamil + English\n\
             {about}\n\
             👇 Drop a LIKE and SUBSCRIBE if you enjoy the stream!",
            game.to_uppercase()
        ),
        tags: tags.into(),
        hashtags: hashtags.into(),
        banner_label: banner.into(),
        twitch: cat(twitch),
        twitch_tags: "Tamil, PC, English".into(),
        thumb: thumb(banner, "{mode}", c1, c2),
        announce: true,
        announce_text: format!(
            "{emoji} {} LIVE: {{title}}\n▶ {{youtube}}",
            game.to_uppercase()
        ),
        disclaimer: disclaimer.into(),
        ..Default::default()
    };
    vec![
        Profile {
            id: "bgmi".into(),
            name: "BGMI".into(),
            kind: "phone".into(),
            game: "BGMI".into(),
            mode: "Rank push".into(),
            title: "🔴 BGMI LIVE TAMIL | {mode} 🔥 | Day {n} #bgmi #bgmitamil".into(),
            description: "🔥 BGMI LIVE TAMIL — {mode}!\n\
                          🎮 Game: BGMI · 📱 Platform: Mobile · 🗣️ Language: Tamil + English\n\
                          🔴 Today: {mode} · high-kill gameplay · clutches · chat with viewers\n\
                          👇 Drop a LIKE and SUBSCRIBE if you enjoy the stream!"
                .into(),
            tags: "bgmi live tamil, bgmi tamil live, bgmi live, bgmi tamil, bgmi rank push, \
                   battlegrounds mobile india, pubg mobile tamil, bgmi gameplay tamil"
                .into(),
            hashtags: "#bgmi #bgmilive #bgmitamil #battlegroundsmobileindia #pubgmobile".into(),
            banner_label: "BGMI LIVE".into(),
            twitch: cat("Battlegrounds Mobile India"),
            twitch_tags: "Tamil, BGMI, Mobile, India, English".into(),
            thumb: thumb("BGMI LIVE", "{mode} 🔥", "#f59e0b", "#7c3aed"),
            announce: true,
            announce_text: "🔴 BGMI LIVE: {title}\n▶ {youtube}\n🟣 {twitch}".into(),
            disclaimer: "⚠️ Battlegrounds Mobile India (BGMI) is a trademark of KRAFTON, Inc. \
                         Gameplay streamed for entertainment under KRAFTON's creator guidelines."
                .into(),
            ..Default::default()
        },
        Profile {
            id: "rooms".into(),
            name: "BGMI custom rooms".into(),
            kind: "rooms".into(),
            game: "BGMI".into(),
            mode: "Open custom rooms".into(),
            title: "🔴 BGMI CUSTOM ROOM LIVE TAMIL | Everyone Can Join 🎮 | #{n} #bgmicustomroom".into(),
            description: "🎮 BGMI OPEN CUSTOM ROOMS — play with me! Everyone can join, FREE ENTRY.\n\n\
                          🗺️ Map / mode: {map}\n\n\
                          📝 HOW TO JOIN:\n\
                          1️⃣ Subscribe and stay in the live chat\n\
                          2️⃣ Room ID & Password are shown on stream when the room opens\n\
                          3️⃣ Room full? Join the next match\n\n\
                          📜 ROOM RULES:\n\
                          ✅ Fair play only\n\
                          ❌ No hacks, teaming or intentional disconnects\n\
                          ❌ Don't share the room details outside the stream\n\
                          ⚖️ Host decision is final"
                .into(),
            tags: "bgmi custom room live, bgmi custom room tamil, bgmi room match, bgmi tamil live, \
                   custom room live, bgmi live tamil, bgmi scrims tamil"
                .into(),
            hashtags: "#bgmi #bgmicustomroom #bgmiroommatch #bgmitamil #scrims".into(),
            banner_label: "CUSTOM ROOMS".into(),
            twitch: cat("Battlegrounds Mobile India"),
            twitch_tags: "Tamil, BGMI, CustomRooms, Mobile, English".into(),
            thumb: thumb("CUSTOM ROOMS", "Everyone can join!", "#22d3ee", "#7c3aed"),
            announce: true,
            announce_text: "🎮 BGMI custom rooms LIVE — everyone can join!\n{title}\nRoom details are on the stream ▶ {youtube}".into(),
            room_on_banner: true,
            disclaimer: "ℹ️ Free rooms for viewers — not an official KRAFTON event. \
                         BGMI is a trademark of KRAFTON, Inc."
                .into(),
            ..Default::default()
        },
        Profile {
            id: "efootball".into(),
            name: "eFootball".into(),
            kind: "phone".into(),
            game: "eFootball".into(),
            mode: "Friendlies + Division".into(),
            title: "🔴 eFOOTBALL LIVE TAMIL ⚽ | {mode} | Day {n} #efootball #efootballtamil".into(),
            description: "⚽ eFOOTBALL LIVE TAMIL — {mode}!\n\
                          🎮 Game: eFootball · 🗣️ Language: Tamil + English\n\
                          🤝 Friendlies with subscribers: type JOIN + your eFootball name in chat\n\
                          🔥 Squad talk, formations and pack openings — come chat!\n\
                          👇 Drop a LIKE and SUBSCRIBE if you enjoy the stream!"
                .into(),
            tags: "efootball live tamil, efootball tamil, efootball mobile tamil, efootball live, \
                   efootball friendlies, efootball 2026, efootball division"
                .into(),
            hashtags: "#efootball #efootballtamil #efootballmobile #efootball2026".into(),
            banner_label: "eFootball".into(),
            twitch: cat("eFootball"),
            twitch_tags: "Tamil, eFootball, Football, Mobile, English".into(),
            thumb: thumb("eFOOTBALL", "{mode} ⚽", "#16a34a", "#0ea5e9"),
            announce: true,
            announce_text: "⚽ eFootball LIVE: {title}\nFriendlies with subs — type JOIN in chat!\n▶ {youtube}".into(),
            disclaimer: "⚠️ eFootball™ is a trademark of KONAMI. Friendlies are for fun — \
                         not an official KONAMI event."
                .into(),
            ..Default::default()
        },
        pc(
            "fc26", "EA FC 26", "EA SPORTS FC 26", "Ultimate Team",
            "🔴 EA FC 26 LIVE TAMIL ⚽ | {mode} | Day {n} #eafc26 #fc26", "⚽",
            "🏆 Ultimate Team, pack openings and rivals — squad tips in chat!",
            "ea fc 26 tamil, fc 26 live tamil, fc 26 ultimate team, ea sports fc 26 live, fc 26 pack opening",
            "#eafc26 #fc26 #fc26tamil #ultimateteam", "EA FC 26", "EA Sports FC 26",
            "⚠️ EA SPORTS FC™ 26 © Electronic Arts. Streamed for entertainment.",
            "#22c55e", "#0ea5e9",
        ),
        pc(
            "ets2", "Euro Truck Simulator 2", "Euro Truck Simulator 2", "Chill drive",
            "🔴 EURO TRUCK SIMULATOR 2 LIVE TAMIL 🚛 | {mode} | Day {n} #ets2", "🚛",
            "🛣️ Chill late-night driving across Europe — chat and relax with me!",
            "euro truck simulator 2 tamil, ets2 tamil live, ets2 live, euro truck simulator 2 live, truck simulator tamil",
            "#ets2 #eurotrucksimulator2 #ets2tamil #trucksimulator", "ETS 2", "Euro Truck Simulator 2",
            "⚠️ Euro Truck Simulator 2 © SCS Software. Streamed for entertainment.",
            "#f97316", "#0ea5e9",
        ),
        pc(
            "wd2", "Watch Dogs 2", "Watch Dogs 2", "Story",
            "🔴 WATCH DOGS 2 LIVE TAMIL 💻 | {mode} | Part {n} #watchdogs2", "💻",
            "🕹️ Story missions, hacking and free roam in San Francisco.",
            "watch dogs 2 tamil, watch dogs 2 live tamil, watch dogs 2 gameplay tamil, watch dogs 2 live",
            "#watchdogs2 #watchdogs #pcgaming", "WATCH DOGS 2", "Watch Dogs 2",
            "⚠️ Watch Dogs® 2 © Ubisoft. Fictional game — viewer discretion advised.",
            "#06b6d4", "#a855f7",
        ),
        pc(
            "detroit", "Detroit: Become Human", "Detroit: Become Human", "Full story",
            "🔴 DETROIT: BECOME HUMAN LIVE TAMIL 🤖 | {mode} | Part {n}", "🤖",
            "🎬 Every choice changes the story — help me decide in chat!",
            "detroit become human tamil, detroit become human live tamil, detroit become human gameplay tamil",
            "#detroitbecomehuman #storygame #pcgaming", "DETROIT", "Detroit: Become Human",
            "⚠️ Detroit: Become Human © Quantic Dream / Sony Interactive Entertainment. \
             Mature story — viewer discretion advised.",
            "#38bdf8", "#1e3a8a",
        ),
        pc(
            "pubgpc", "PUBG PC", "PUBG: BATTLEGROUNDS", "Rank push",
            "🔴 PUBG PC LIVE TAMIL | {mode} 🔥 | Day {n} #pubg #pubgpc", "🔥",
            "🎯 Squad fights, chicken dinners and rank push on PC.",
            "pubg pc live tamil, pubg pc tamil, pubg battlegrounds tamil, pubg live tamil, pubg pc gameplay",
            "#pubg #pubgpc #pubgtamil #pubgbattlegrounds", "PUBG PC", "PUBG: BATTLEGROUNDS",
            "⚠️ PUBG: BATTLEGROUNDS © KRAFTON, Inc. Streamed for entertainment.",
            "#eab308", "#7c3aed",
        ),
        Profile {
            id: "pc".into(),
            name: "Other PC game".into(),
            kind: "pc".into(),
            game: String::new(),
            mode: "Gameplay".into(),
            title: "🔴 {game} LIVE TAMIL | {mode} 🔥 | Day {n}".into(),
            description: "🖥️ {game} LIVE TAMIL — {mode}!\n\
                          🎮 Game: {game} · 💻 Platform: PC · 🗣️ Language: Tamil + English\n\
                          👇 Drop a LIKE and SUBSCRIBE if you enjoy the stream!"
                .into(),
            tags: "pc gaming tamil, tamil pc gameplay, tamil live gaming".into(),
            hashtags: "#pcgaming #tamilgaming".into(),
            banner_label: "{game}".into(),
            twitch_tags: "Tamil, PC, English".into(),
            thumb: thumb("{game}", "{mode}", "#ef4444", "#7c3aed"),
            announce: false,
            announce_text: "🖥️ {game} LIVE: {title}\n▶ {youtube}".into(),
            disclaimer: "⚠️ {game} © its publisher. Streamed for entertainment.".into(),
            ..Default::default()
        },
    ]
}

fn store_path() -> PathBuf {
    let base = std::env::var("APPDATA")
        .map(PathBuf::from)
        .unwrap_or_else(|_| PathBuf::from("."));
    base.join("PhoneCast").join("profiles.json")
}

pub fn thumbnails_dir() -> PathBuf {
    let base = std::env::var("USERPROFILE")
        .map(PathBuf::from)
        .unwrap_or_else(|_| PathBuf::from("."));
    base.join("Videos").join("PhoneCast").join("Thumbnails")
}

impl Store {
    pub fn load() -> Self {
        let Some(mut s) = std::fs::read(store_path())
            .ok()
            .and_then(|b| serde_json::from_slice::<Store>(&b).ok())
        else {
            return Store::default();
        };
        if upgrade_texts(&mut s) {
            s.save().ok();
        }
        s
    }

    fn save(&self) -> Result<(), String> {
        let p = store_path();
        if let Some(d) = p.parent() {
            std::fs::create_dir_all(d).map_err(|e| e.to_string())?;
        }
        let tmp = p.with_extension("json.tmp");
        std::fs::write(
            &tmp,
            serde_json::to_vec_pretty(self).map_err(|e| e.to_string())?,
        )
        .map_err(|e| e.to_string())?;
        std::fs::rename(tmp, p).map_err(|e| e.to_string())
    }

    fn profile(&self, id: &str) -> Option<&Profile> {
        self.profiles.iter().find(|p| p.id == id)
    }
}

// ---------------- rendering ----------------

/// Values filled in at go-live time (game for PC, room details, …).
pub type Vars = HashMap<String, String>;

/// Replaces `{name}` with its value; unknown names are left as they are so
/// a typo is visible in the preview instead of silently vanishing.
pub fn fill(template: &str, vars: &Vars) -> String {
    let mut out = String::with_capacity(template.len());
    let mut rest = template;
    while let Some(start) = rest.find('{') {
        out.push_str(&rest[..start]);
        let after = &rest[start + 1..];
        match after.find('}') {
            Some(end)
                if after[..end]
                    .chars()
                    .all(|c| c.is_ascii_alphanumeric() || c == '_')
                    && end > 0 =>
            {
                let key = &after[..end];
                match vars.get(key) {
                    Some(v) => out.push_str(v),
                    None => {
                        out.push('{');
                        out.push_str(key);
                        out.push('}');
                    }
                }
                rest = &after[end + 1..];
            }
            _ => {
                out.push('{');
                rest = after;
            }
        }
    }
    out.push_str(rest);
    out
}

/// Fills a multi-line template. A line that ends in a variable with no value
/// ("📸 Instagram: {instagram}" with no Instagram) is dropped instead of
/// showing a bare label.
pub fn fill_lines(template: &str, vars: &Vars) -> String {
    let mut out: Vec<String> = Vec::new();
    for line in template.lines() {
        let t = line.trim_end();
        if let Some(open) = t.rfind('{') {
            if t.ends_with('}') {
                let key = &t[open + 1..t.len() - 1];
                if !key.is_empty()
                    && key.chars().all(|c| c.is_ascii_alphanumeric() || c == '_')
                    && vars.get(key).is_some_and(|v| v.trim().is_empty())
                    && open > 0
                {
                    continue;
                }
            }
        }
        out.push(fill(line, vars));
    }
    out.join("\n")
}

/// Collapses runs of blank lines, drops section headings left with nothing
/// under them ("🎮 MY SETUP:" when no phone or PC is filled in), and trims.
fn tidy(text: &str) -> String {
    let raw: Vec<&str> = text.lines().map(str::trim_end).collect();
    let heading = |l: &str| l.ends_with(':') || (l.starts_with('★') && l.ends_with('★'));
    let mut lines: Vec<&str> = Vec::new();
    for (i, l) in raw.iter().enumerate() {
        if heading(l) && raw.get(i + 1).is_none_or(|n| n.is_empty() || heading(n)) {
            continue;
        }
        if l.is_empty() && lines.last().is_none_or(|p| p.is_empty()) {
            continue;
        }
        lines.push(l);
    }
    lines.join("\n").trim().to_string()
}

fn variables(
    d: &Defaults,
    p: &Profile,
    extra: &Vars,
    now: chrono::DateTime<chrono::Local>,
) -> Vars {
    let mut v = Vars::new();
    let put = |v: &mut Vars, k: &str, s: &str| {
        v.insert(k.into(), s.to_string());
    };
    put(&mut v, "channel", &d.channel);
    put(&mut v, "game", &p.game);
    put(&mut v, "mode", &p.mode);
    put(&mut v, "n", &(p.count + 1).to_string());
    put(&mut v, "date", &now.format("%-d %b").to_string());
    put(&mut v, "day", &now.format("%A").to_string());
    put(&mut v, "time", &now.format("%-I:%M %p").to_string());
    put(&mut v, "schedule", &d.schedule);
    put(&mut v, "rules", &d.rules);
    let yt = if d.youtube_handle.trim().is_empty() {
        String::new()
    } else {
        format!(
            "https://youtube.com/@{}/live",
            d.youtube_handle.trim().trim_start_matches('@')
        )
    };
    put(&mut v, "youtube", &yt);
    let handle = d.youtube_handle.trim().trim_start_matches('@');
    let (chan, join) = if handle.is_empty() {
        (String::new(), String::new())
    } else {
        (
            format!("https://youtube.com/@{handle}"),
            format!("https://youtube.com/@{handle}/join"),
        )
    };
    put(&mut v, "channel_link", &chan);
    put(&mut v, "join", &join);
    put(&mut v, "email", &d.email);
    put(&mut v, "goal", &d.goal);
    put(&mut v, "device", &d.device);
    put(&mut v, "pc", &d.pc);
    let tw = if d.twitch_login.trim().is_empty() {
        String::new()
    } else {
        format!("https://twitch.tv/{}", d.twitch_login.trim())
    };
    put(&mut v, "twitch", &tw);
    put(&mut v, "instagram", &d.instagram);
    put(&mut v, "discord", &d.discord_invite);
    let hashtags = format!("{} {}", p.hashtags.trim(), d.hashtags.trim())
        .trim()
        .to_string();
    put(&mut v, "tags", &hashtags);
    for k in ["room", "pass", "map"] {
        v.entry(k.into()).or_default();
    }
    // Values typed for this stream win (game for PC, mode, room details).
    for (k, val) in extra {
        if !val.trim().is_empty() {
            v.insert(k.clone(), val.trim().to_string());
        }
    }
    let disclaimer = fill(&p.disclaimer, &v);
    v.insert("disclaimer".into(), disclaimer);
    v
}

fn split_list(s: &str) -> Vec<String> {
    s.split([',', '\n'])
        .map(|t| t.trim().to_string())
        .filter(|t| !t.is_empty())
        .collect()
}

/// Twitch tags: up to 10, ≤25 characters, letters and digits only.
pub fn twitch_tags(raw: &str) -> Vec<String> {
    let mut out: Vec<String> = Vec::new();
    for t in split_list(raw) {
        let clean: String = t.chars().filter(|c| c.is_alphanumeric()).take(25).collect();
        if !clean.is_empty() && !out.iter().any(|o| o.eq_ignore_ascii_case(&clean)) {
            out.push(clean);
        }
        if out.len() == 10 {
            break;
        }
    }
    out
}

/// YouTube tags: total under 500 characters (commas count).
fn youtube_tags(profile: &str, common: &str) -> Vec<String> {
    let mut out: Vec<String> = Vec::new();
    let mut len = 0;
    for t in split_list(profile).into_iter().chain(split_list(common)) {
        if out.iter().any(|o| o.eq_ignore_ascii_case(&t)) {
            continue;
        }
        if len + t.chars().count() + 1 > 495 {
            break;
        }
        len += t.chars().count() + 1;
        out.push(t);
    }
    out
}

/// Everything filled in for one stream. Pure: no network.
pub fn render(store: &Store, id: &str, extra: &Vars) -> Result<Value, String> {
    let p = store.profile(id).ok_or("Profile not found")?;
    let d = &store.defaults;
    let mut v = variables(d, p, extra, chrono::Local::now());
    let title = fill(&p.title, &v).trim().to_string();
    v.insert("title".into(), title.clone());
    let description = tidy(&format!(
        "{}\n\n{}",
        fill_lines(&p.description, &v),
        fill_lines(&d.footer, &v)
    ));
    let yt_tags = youtube_tags(&p.tags, &d.tags);
    let tw_title: String = title.chars().take(140).collect();
    let mut warnings = Vec::new();
    if title.chars().count() > 100 {
        warnings.push(format!(
            "YouTube titles can be 100 characters; this one is {}. YouTube will refuse it.",
            title.chars().count()
        ));
    }
    if description.chars().count() > 5000 {
        warnings.push("The description is over YouTube's 5000-character limit.".into());
    }
    if p.kind == "pc" && v.get("game").is_none_or(|g| g.is_empty()) {
        warnings.push("Type the game name for this stream.".into());
    }
    if p.kind == "rooms" && v.get("room").is_none_or(|r| r.is_empty()) {
        warnings.push("No room ID yet — you can add it later while live.".into());
    }
    for (label, text) in [("Title", &title), ("Description", &description)] {
        if let Some(s) = text.find('{') {
            let name: String = text[s..].chars().take_while(|c| *c != '}').collect();
            warnings.push(format!("{label} has an unknown variable {name}}}"));
        }
    }
    let room_card = if p.kind == "rooms" {
        let mut parts = Vec::new();
        if let Some(r) = v.get("room").filter(|s| !s.is_empty()) {
            parts.push(format!("Room ID: {r}"));
        }
        if let Some(pw) = v.get("pass").filter(|s| !s.is_empty()) {
            parts.push(format!("Pass: {pw}"));
        }
        if let Some(m) = v.get("map").filter(|s| !s.is_empty()) {
            parts.push(m.clone());
        }
        parts.join(" · ")
    } else {
        String::new()
    };
    Ok(json!({
        "profile": p.id,
        "youtube": {
            "title": title,
            "description": description,
            "tags": yt_tags.join(", "),
            "category": "Gaming",
            "game": v.get("game").cloned().unwrap_or_default(),
            "link": v.get("youtube").cloned().unwrap_or_default(),
        },
        "twitch": {
            "title": tw_title,
            "category": p.twitch.name,
            "categoryId": p.twitch.id,
            "tags": twitch_tags(&format!("{},{}", p.twitch_tags, "")),
            "language": d.language,
        },
        "discord": { "content": tidy(&fill(&p.announce_text, &v)) },
        "banner": { "label": fill(&p.banner_label, &v).trim().chars().take(30).collect::<String>(), "room": room_card },
        "thumb": {
            "headline": fill(&p.thumb.headline, &v),
            "subline": fill(&p.thumb.subline, &v),
            "color1": p.thumb.color1, "color2": p.thumb.color2, "useFrame": p.thumb.use_frame,
            "channel": d.channel,
        },
        "warnings": warnings,
    }))
}

// ---------------- HTTP through curl.exe ----------------

fn hide(cmd: &mut Command) {
    #[cfg(windows)]
    cmd.creation_flags(0x0800_0000);
}

/// Quotes a value for a curl config file.
fn cq(s: &str) -> String {
    format!("\"{}\"", s.replace('\\', "\\\\").replace('"', "\\\""))
}

/// Runs curl with a config passed on stdin (so tokens never appear in the
/// process list). Returns (HTTP status, parsed JSON body or Null).
fn curl(config: &str) -> Result<(u16, Value), String> {
    let mut cmd = Command::new("curl.exe");
    cmd.args(["-sS", "--max-time", "25", "-K", "-"])
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped());
    hide(&mut cmd);
    let mut child = cmd.spawn().map_err(|e| format!("curl: {e}"))?;
    let cfg = format!("{config}\nwrite-out = \"\\n%{{http_code}}\"\n");
    child
        .stdin
        .take()
        .unwrap()
        .write_all(cfg.as_bytes())
        .map_err(|e| e.to_string())?;
    let out = child.wait_with_output().map_err(|e| e.to_string())?;
    let text = String::from_utf8_lossy(&out.stdout).to_string();
    let (body, code) = text.rsplit_once('\n').unwrap_or(("", text.as_str()));
    let status: u16 = code.trim().parse().unwrap_or(0);
    if status == 0 {
        return Err(format!(
            "No internet connection ({})",
            String::from_utf8_lossy(&out.stderr).trim()
        ));
    }
    Ok((
        status,
        serde_json::from_str(body.trim()).unwrap_or(Value::Null),
    ))
}

fn temp_file(name: &str, data: &[u8]) -> Result<PathBuf, String> {
    let p = std::env::temp_dir().join(format!("phonecast-{}-{name}", std::process::id()));
    std::fs::write(&p, data).map_err(|e| e.to_string())?;
    Ok(p)
}

// ---------------- Twitch ----------------

const TWITCH_SCOPES: &str = "channel:manage:broadcast";

#[derive(Serialize, Deserialize, Clone, Default)]
struct TwitchToken {
    access: String,
    refresh: String,
    user_id: String,
    login: String,
    client_id: String,
}

#[derive(Default)]
struct Login {
    state: String, // "", "waiting", "done", "error"
    code: String,
    uri: String,
    error: String,
}

pub struct Profiles {
    store: Mutex<Store>,
    login: Arc<Mutex<Login>>,
}

impl Profiles {
    pub fn new() -> Arc<Self> {
        Arc::new(Self {
            store: Mutex::new(Store::load()),
            login: Arc::new(Mutex::new(Login::default())),
        })
    }

    fn token(&self) -> Option<TwitchToken> {
        let enc = self.store.lock().unwrap().twitch_token_enc.clone();
        if enc.is_empty() {
            return None;
        }
        serde_json::from_str(&dpapi::unprotect(&enc).ok()?).ok()
    }

    fn set_token(&self, t: Option<&TwitchToken>) -> Result<(), String> {
        let mut s = self.store.lock().unwrap();
        s.twitch_token_enc = match t {
            Some(t) => dpapi::protect(&serde_json::to_string(t).map_err(|e| e.to_string())?)?,
            None => String::new(),
        };
        if let Some(t) = t {
            if s.defaults.twitch_login.is_empty() {
                s.defaults.twitch_login = t.login.clone();
            }
        }
        s.save()
    }

    /// Everything the editor needs; secrets are reported as set/not set.
    pub fn json(&self) -> Value {
        let s = self.store.lock().unwrap();
        let login = self.login.lock().unwrap();
        let twitch = {
            let enc = s.twitch_token_enc.clone();
            let login = if enc.is_empty() {
                None
            } else {
                dpapi::unprotect(&enc)
                    .ok()
                    .and_then(|j| serde_json::from_str::<TwitchToken>(&j).ok())
                    .map(|t| t.login)
            };
            json!({ "connected": login.is_some(), "login": login })
        };
        json!({
            "defaults": s.defaults,
            "profiles": s.profiles,
            "active": s.active,
            "discordWebhookSet": !s.discord_webhook_enc.is_empty(),
            "twitch": twitch,
            "login": { "state": login.state, "code": login.code, "uri": login.uri, "error": login.error },
            "variables": VARIABLES.iter().map(|(k, d)| json!({"name": k, "desc": d})).collect::<Vec<_>>(),
            "thumbnailsDir": thumbnails_dir().display().to_string(),
        })
    }

    /// Saves defaults/profiles/active. `discordWebhook`: absent = keep,
    /// "" = remove, URL = replace (must be a discord.com webhook).
    pub fn save(&self, v: &Value) -> Result<Value, String> {
        {
            let mut s = self.store.lock().unwrap();
            if let Some(d) = v.get("defaults") {
                s.defaults =
                    serde_json::from_value(d.clone()).map_err(|e| format!("Defaults: {e}"))?;
            }
            if let Some(p) = v.get("profiles") {
                let mut list: Vec<Profile> =
                    serde_json::from_value(p.clone()).map_err(|e| format!("Profiles: {e}"))?;
                for (i, p) in list.iter_mut().enumerate() {
                    if p.id.trim().is_empty() {
                        p.id = format!("profile{}", i + 1);
                    }
                    if p.name.trim().is_empty() {
                        p.name = format!("Profile {}", i + 1);
                    }
                }
                if list.is_empty() {
                    return Err("Keep at least one profile".into());
                }
                s.profiles = list;
            }
            if let Some(a) = v.get("active").and_then(Value::as_str) {
                s.active = a.into();
            }
            if let Some(w) = v.get("discordWebhook").and_then(Value::as_str) {
                let w = w.trim();
                s.discord_webhook_enc = if w.is_empty() {
                    String::new()
                } else {
                    let ok = (w.starts_with("https://discord.com/api/webhooks/")
                        || w.starts_with("https://discordapp.com/api/webhooks/")
                        || w.starts_with("https://ptb.discord.com/api/webhooks/")
                        || w.starts_with("https://canary.discord.com/api/webhooks/"))
                        && !w.contains(char::is_whitespace);
                    if !ok {
                        return Err("That doesn't look like a Discord webhook link (it starts with https://discord.com/api/webhooks/)".into());
                    }
                    dpapi::protect(w)?
                };
            }
            s.save()?;
        }
        Ok(self.json())
    }

    pub fn reset_profile(&self, id: &str) -> Result<Value, String> {
        let starter = starter_profiles()
            .into_iter()
            .find(|p| p.id == id)
            .ok_or("Only the built-in profiles can be reset")?;
        {
            let mut s = self.store.lock().unwrap();
            if let Some(p) = s.profiles.iter_mut().find(|p| p.id == id) {
                let count = p.count;
                *p = starter;
                p.count = count;
            }
            s.save()?;
        }
        Ok(self.json())
    }

    pub fn render(&self, id: &str, vars: &Vars) -> Result<Value, String> {
        render(&self.store.lock().unwrap(), id, vars)
    }

    // ----- Twitch login (device code: no password ever reaches PhoneCast) -----

    pub fn twitch_login(self: &Arc<Self>, client_id: &str) -> Result<Value, String> {
        let client_id = client_id.trim().to_string();
        if client_id.len() < 20 || !client_id.chars().all(|c| c.is_ascii_alphanumeric()) {
            return Err("Paste the Client ID of your Twitch app (dev.twitch.tv → Your Console → Applications)".into());
        }
        {
            let mut s = self.store.lock().unwrap();
            s.defaults.twitch_client_id = client_id.clone();
            s.save()?;
        }
        let (status, v) = curl(&format!(
            "url = \"https://id.twitch.tv/oauth2/device\"\ndata-urlencode = {}\ndata-urlencode = {}",
            cq(&format!("client_id={client_id}")),
            cq(&format!("scopes={TWITCH_SCOPES}"))
        ))?;
        let device = v["device_code"]
            .as_str()
            .ok_or_else(|| twitch_error(status, &v))?
            .to_string();
        let interval = v["interval"].as_u64().unwrap_or(5).max(1);
        let expires = v["expires_in"].as_u64().unwrap_or(1800);
        {
            let mut l = self.login.lock().unwrap();
            *l = Login {
                state: "waiting".into(),
                code: v["user_code"].as_str().unwrap_or_default().into(),
                uri: v["verification_uri"]
                    .as_str()
                    .unwrap_or("https://www.twitch.tv/activate")
                    .into(),
                error: String::new(),
            };
        }
        let me = self.clone();
        std::thread::spawn(move || {
            let deadline = Instant::now() + Duration::from_secs(expires);
            let mut wait = interval;
            while Instant::now() < deadline {
                std::thread::sleep(Duration::from_secs(wait));
                if me.login.lock().unwrap().state != "waiting" {
                    return; // cancelled
                }
                let r = curl(&format!(
                    "url = \"https://id.twitch.tv/oauth2/token\"\ndata-urlencode = {}\ndata-urlencode = {}\ndata-urlencode = \"grant_type=urn:ietf:params:oauth:grant-type:device_code\"",
                    cq(&format!("client_id={client_id}")),
                    cq(&format!("device_code={device}"))
                ));
                let (_, v) = match r {
                    Ok(x) => x,
                    Err(_) => continue,
                };
                if let Some(access) = v["access_token"].as_str() {
                    let mut t = TwitchToken {
                        access: access.into(),
                        refresh: v["refresh_token"].as_str().unwrap_or_default().into(),
                        client_id: client_id.clone(),
                        ..Default::default()
                    };
                    let result = fill_user(&mut t).and_then(|_| me.set_token(Some(&t)));
                    let mut l = me.login.lock().unwrap();
                    match result {
                        Ok(()) => {
                            l.state = "done".into();
                            crate::diag::log(
                                "profiles",
                                format!("Twitch connected as {}", t.login),
                            );
                        }
                        Err(e) => {
                            l.state = "error".into();
                            l.error = e;
                        }
                    }
                    return;
                }
                match v["message"].as_str().unwrap_or("") {
                    "authorization_pending" | "" => {}
                    "slow_down" => wait += 5,
                    other => {
                        let mut l = me.login.lock().unwrap();
                        l.state = "error".into();
                        l.error = format!("Twitch: {other}");
                        return;
                    }
                }
            }
            let mut l = me.login.lock().unwrap();
            l.state = "error".into();
            l.error = "The Twitch code expired — click Connect again".into();
        });
        Ok(self.json())
    }

    pub fn twitch_cancel(&self) {
        let mut l = self.login.lock().unwrap();
        *l = Login::default();
    }

    pub fn twitch_logout(&self) -> Result<Value, String> {
        self.twitch_cancel();
        self.set_token(None)?;
        Ok(self.json())
    }

    /// Calls Helix, refreshing the token once if it expired.
    fn helix(
        &self,
        method: &str,
        path: &str,
        body: Option<&Value>,
    ) -> Result<(u16, Value), String> {
        let mut t = self.token().ok_or("Connect Twitch first")?;
        for attempt in 0..2 {
            let mut cfg = format!(
                "url = {}\nrequest = {}\nheader = {}\nheader = {}",
                cq(&format!("https://api.twitch.tv/helix/{path}")),
                cq(method),
                cq(&format!("Authorization: Bearer {}", t.access)),
                cq(&format!("Client-Id: {}", t.client_id)),
            );
            let tmp = match body {
                Some(b) => {
                    let p = temp_file("helix.json", b.to_string().as_bytes())?;
                    cfg.push_str(&format!(
                        "\nheader = \"Content-Type: application/json\"\ndata-binary = {}",
                        cq(&format!("@{}", p.display()))
                    ));
                    Some(p)
                }
                None => None,
            };
            let r = curl(&cfg);
            if let Some(p) = tmp {
                std::fs::remove_file(p).ok();
            }
            let (status, v) = r?;
            if status == 401 && attempt == 0 && !t.refresh.is_empty() {
                let (_, rv) = curl(&format!(
                    "url = \"https://id.twitch.tv/oauth2/token\"\ndata-urlencode = {}\ndata-urlencode = \"grant_type=refresh_token\"\ndata-urlencode = {}",
                    cq(&format!("client_id={}", t.client_id)),
                    cq(&format!("refresh_token={}", t.refresh))
                ))?;
                match rv["access_token"].as_str() {
                    Some(a) => {
                        t.access = a.into();
                        if let Some(r) = rv["refresh_token"].as_str() {
                            t.refresh = r.into();
                        }
                        self.set_token(Some(&t))?;
                        continue;
                    }
                    None => {
                        return Err("Twitch sign-in expired — click Connect Twitch again".into())
                    }
                }
            }
            return Ok((status, v));
        }
        Err("Twitch sign-in expired — click Connect Twitch again".into())
    }

    pub fn twitch_search(&self, query: &str) -> Result<Value, String> {
        let q: String = query.trim().chars().take(60).collect();
        if q.is_empty() {
            return Ok(json!([]));
        }
        let enc: String = q
            .bytes()
            .map(|b| {
                if b.is_ascii_alphanumeric() {
                    (b as char).to_string()
                } else {
                    format!("%{b:02X}")
                }
            })
            .collect();
        let (status, v) = self.helix(
            "GET",
            &format!("search/categories?first=8&query={enc}"),
            None,
        )?;
        if status != 200 {
            return Err(twitch_error(status, &v));
        }
        Ok(json!(v["data"]
            .as_array()
            .cloned()
            .unwrap_or_default()
            .iter()
            .map(|c| json!({
                "id": c["id"], "name": c["name"], "art": c["box_art_url"],
            }))
            .collect::<Vec<_>>()))
    }

    /// Sets the Twitch title, category, tags and language for this stream.
    fn update_twitch(&self, rendered: &Value) -> Result<String, String> {
        let t = self.token().ok_or("not connected")?;
        let tw = &rendered["twitch"];
        let mut body = json!({
            "title": tw["title"],
            "broadcaster_language": tw["language"],
            "tags": tw["tags"],
        });
        let mut cat = tw["categoryId"].as_str().unwrap_or_default().to_string();
        let cat_name = tw["category"].as_str().unwrap_or_default().to_string();
        if cat.is_empty() && !cat_name.is_empty() {
            // First use: look the category up by name and remember it.
            if let Some(first) = self
                .twitch_search(&cat_name)?
                .as_array()
                .and_then(|a| a.first())
                .cloned()
            {
                cat = first["id"].as_str().unwrap_or_default().to_string();
                let id = rendered["profile"].as_str().unwrap_or_default().to_string();
                let mut s = self.store.lock().unwrap();
                if let Some(p) = s.profiles.iter_mut().find(|p| p.id == id) {
                    p.twitch = TwitchCategory {
                        id: cat.clone(),
                        name: first["name"].as_str().unwrap_or_default().into(),
                    };
                }
                s.save().ok();
            }
        }
        if !cat.is_empty() {
            body["game_id"] = json!(cat);
        }
        let (status, v) = self.helix(
            "PATCH",
            &format!("channels?broadcaster_id={}", t.user_id),
            Some(&body),
        )?;
        if status == 204 || status == 200 {
            Ok(format!(
                "Twitch updated: “{}”",
                tw["title"].as_str().unwrap_or("")
            ))
        } else {
            Err(twitch_error(status, &v))
        }
    }

    // ----- go live -----

    /// Called when Start streaming is pressed: fills the platforms that can
    /// be filled automatically (Twitch) and returns the rendered texts.
    pub fn apply(&self, id: &str, vars: &Vars) -> Result<Value, String> {
        let mut rendered = self.render(id, vars)?;
        {
            let mut s = self.store.lock().unwrap();
            s.active = id.into();
            s.save().ok();
        }
        let twitch = if self.token().is_some() {
            match self.update_twitch(&rendered) {
                Ok(m) => json!({ "ok": true, "message": m }),
                Err(e) => json!({ "ok": false, "message": format!("Twitch not updated: {e}") }),
            }
        } else {
            json!({ "ok": false, "message": "" })
        };
        crate::diag::log(
            "profiles",
            format!("profile {id} applied; twitch: {}", twitch["message"]),
        );
        rendered["result"] = json!({ "twitch": twitch });
        Ok(rendered)
    }

    /// The stream really started: count it and post to Discord if enabled.
    pub fn went_live(
        &self,
        id: &str,
        vars: &Vars,
        thumbnail: Option<PathBuf>,
    ) -> Result<Value, String> {
        let rendered = self.render(id, vars)?;
        let (announce, everyone, webhook) = {
            let mut s = self.store.lock().unwrap();
            let webhook = if s.discord_webhook_enc.is_empty() {
                None
            } else {
                dpapi::unprotect(&s.discord_webhook_enc).ok()
            };
            let p = s
                .profiles
                .iter_mut()
                .find(|p| p.id == id)
                .ok_or("Profile not found")?;
            p.count += 1;
            let r = (p.announce, p.announce_everyone, webhook);
            s.save().ok();
            r
        };
        let discord = match (announce, webhook) {
            (true, Some(url)) => {
                match post_discord(&url, &rendered, everyone, thumbnail.as_ref()) {
                    Ok(()) => json!({ "ok": true, "message": "Posted on Discord" }),
                    Err(e) => {
                        json!({ "ok": false, "message": format!("Discord post failed: {e}") })
                    }
                }
            }
            (true, None) => {
                json!({ "ok": false, "message": "Add your Discord webhook in Stream profiles to post there" })
            }
            _ => json!({ "ok": true, "message": "" }),
        };
        crate::diag::log(
            "profiles",
            format!("went live with {id}; discord: {}", discord["message"]),
        );
        Ok(json!({ "discord": discord }))
    }

    /// Sends a test message to the saved Discord webhook.
    pub fn test_discord(&self) -> Result<(), String> {
        let enc = self.store.lock().unwrap().discord_webhook_enc.clone();
        if enc.is_empty() {
            return Err("Paste your Discord webhook link first".into());
        }
        let url = dpapi::unprotect(&enc)?;
        let payload = json!({ "content": "✅ PhoneCast is connected. Live announcements will appear here.", "allowed_mentions": { "parse": [] } });
        send_webhook(&url, &payload, None)
    }
}

fn fill_user(t: &mut TwitchToken) -> Result<(), String> {
    let (status, v) = curl(&format!(
        "url = \"https://api.twitch.tv/helix/users\"\nheader = {}\nheader = {}",
        cq(&format!("Authorization: Bearer {}", t.access)),
        cq(&format!("Client-Id: {}", t.client_id))
    ))?;
    let u = &v["data"][0];
    t.user_id = u["id"].as_str().unwrap_or_default().into();
    t.login = u["login"].as_str().unwrap_or_default().into();
    if t.user_id.is_empty() {
        return Err(twitch_error(status, &v));
    }
    Ok(())
}

fn twitch_error(status: u16, v: &Value) -> String {
    let msg = v["message"].as_str().or(v["error"].as_str()).unwrap_or("");
    match status {
        400 if msg.to_lowercase().contains("client") => {
            "Twitch didn't accept the Client ID — check it on dev.twitch.tv".into()
        }
        401 => "Twitch sign-in expired — click Connect Twitch again".into(),
        403 => "Twitch refused: reconnect Twitch so PhoneCast may change your stream info".into(),
        429 => "Twitch says too many requests — try again in a minute".into(),
        _ if !msg.is_empty() => format!("Twitch: {msg}"),
        _ => format!("Twitch error {status}"),
    }
}

// ---------------- Discord ----------------

fn post_discord(
    url: &str,
    rendered: &Value,
    everyone: bool,
    thumbnail: Option<&PathBuf>,
) -> Result<(), String> {
    let mut content = rendered["discord"]["content"]
        .as_str()
        .unwrap_or_default()
        .to_string();
    if everyone {
        content = format!("@everyone {content}");
    }
    let content: String = content.chars().take(1900).collect();
    let yt = &rendered["youtube"];
    let mut embed = json!({
        "title": yt["title"].as_str().unwrap_or("").chars().take(250).collect::<String>(),
        "color": 0x8b5cf6,
    });
    if let Some(link) = yt["link"].as_str().filter(|l| !l.is_empty()) {
        embed["url"] = json!(link);
    }
    let thumb = thumbnail.filter(|p| p.exists());
    if thumb.is_some() {
        embed["image"] = json!({ "url": "attachment://thumbnail.png" });
    }
    let payload = json!({
        "content": content,
        "embeds": [embed],
        "allowed_mentions": { "parse": if everyone { vec!["everyone"] } else { vec![] } },
    });
    send_webhook(url, &payload, thumb)
}

fn send_webhook(url: &str, payload: &Value, file: Option<&PathBuf>) -> Result<(), String> {
    let pj = temp_file("discord.json", payload.to_string().as_bytes())?;
    let mut cfg = format!(
        "url = {}\nform = {}",
        cq(&format!("{url}?wait=true")),
        cq(&format!("payload_json=<{}", pj.display()))
    );
    if let Some(f) = file {
        cfg.push_str(&format!(
            "\nform = {}",
            cq(&format!(
                "files[0]=@{};filename=thumbnail.png;type=image/png",
                f.display()
            ))
        ));
    }
    let r = curl(&cfg);
    std::fs::remove_file(pj).ok();
    let (status, v) = r?;
    match status {
        200 | 204 => Ok(()),
        401 | 404 => {
            Err("the webhook was deleted or the link is wrong — make a new one in Discord".into())
        }
        429 => Err("Discord says slow down — try again in a minute".into()),
        _ => Err(v["message"]
            .as_str()
            .map(str::to_string)
            .unwrap_or_else(|| format!("error {status}"))),
    }
}

/// Saves a thumbnail PNG made by the editor; returns its path.
pub fn save_thumbnail(profile: &str, png: &[u8]) -> Result<PathBuf, String> {
    if png.len() < 8 || &png[..8] != b"\x89PNG\r\n\x1a\n" || png.len() > 10 << 20 {
        return Err("expected a PNG under 10 MB".into());
    }
    let safe: String = profile
        .chars()
        .filter(|c| c.is_ascii_alphanumeric() || *c == '-' || *c == '_')
        .take(40)
        .collect();
    let dir = thumbnails_dir();
    std::fs::create_dir_all(&dir).map_err(|e| e.to_string())?;
    let path = dir.join(format!(
        "{}_{}.png",
        if safe.is_empty() { "stream" } else { &safe },
        chrono::Local::now().format("%Y-%m-%d_%H-%M")
    ));
    std::fs::write(&path, png).map_err(|e| e.to_string())?;
    // Also keep the latest one per profile at a fixed name for Discord.
    std::fs::write(
        dir.join(format!(
            "{}_latest.png",
            if safe.is_empty() { "stream" } else { &safe }
        )),
        png,
    )
    .ok();
    Ok(path)
}

pub fn latest_thumbnail(profile: &str) -> Option<PathBuf> {
    let safe: String = profile
        .chars()
        .filter(|c| c.is_ascii_alphanumeric() || *c == '-' || *c == '_')
        .take(40)
        .collect();
    let p = thumbnails_dir().join(format!(
        "{}_latest.png",
        if safe.is_empty() { "stream" } else { &safe }
    ));
    p.exists().then_some(p)
}

/// Opens an https link in the default browser (only known sites).
pub fn open_link(url: &str) -> Result<(), String> {
    const ALLOWED: &[&str] = &[
        "https://studio.youtube.com/",
        "https://www.youtube.com/",
        "https://youtube.com/",
        "https://dev.twitch.tv/",
        "https://www.twitch.tv/",
        "https://twitch.tv/",
        "https://dashboard.twitch.tv/",
        "https://support.discord.com/",
        "https://discord.com/",
    ];
    if !ALLOWED.iter().any(|a| url.starts_with(a)) || url.contains(['"', ' ', '\n']) {
        return Err("PhoneCast only opens YouTube, Twitch and Discord links".into());
    }
    let mut cmd = Command::new("rundll32.exe");
    cmd.args(["url.dll,FileProtocolHandler", url]);
    cmd.spawn().map(|_| ()).map_err(|e| e.to_string())
}

// ---------------- Tauri commands ----------------

type ProfilesState<'a> = tauri::State<'a, Arc<Profiles>>;

#[tauri::command]
pub fn get_profiles(p: ProfilesState) -> Value {
    p.json()
}

#[tauri::command(async)]
pub fn save_profiles(p: ProfilesState, data: Value) -> Result<Value, String> {
    p.save(&data)
}

#[tauri::command(async)]
pub fn reset_profile(p: ProfilesState, id: String) -> Result<Value, String> {
    p.reset_profile(&id)
}

#[tauri::command]
pub fn render_profile(p: ProfilesState, id: String, vars: Vars) -> Result<Value, String> {
    p.render(&id, &vars)
}

#[tauri::command(async)]
pub fn apply_profile(p: ProfilesState, id: String, vars: Vars) -> Result<Value, String> {
    p.apply(&id, &vars)
}

#[tauri::command(async)]
pub fn profile_went_live(p: ProfilesState, id: String, vars: Vars) -> Result<Value, String> {
    p.went_live(&id, &vars, latest_thumbnail(&id))
}

#[tauri::command(async)]
pub fn twitch_login(p: ProfilesState, client_id: String) -> Result<Value, String> {
    p.inner().twitch_login(&client_id)
}

#[tauri::command]
pub fn twitch_login_cancel(p: ProfilesState) -> Value {
    p.twitch_cancel();
    p.json()
}

#[tauri::command(async)]
pub fn twitch_logout(p: ProfilesState) -> Result<Value, String> {
    p.twitch_logout()
}

#[tauri::command(async)]
pub fn twitch_search_category(p: ProfilesState, query: String) -> Result<Value, String> {
    p.twitch_search(&query)
}

#[tauri::command(async)]
pub fn discord_test(p: ProfilesState) -> Result<(), String> {
    p.test_discord()
}

/// Raw PNG body, profile id in header `x-profile`. Returns the saved path.
#[tauri::command(async)]
pub fn save_thumbnail_png(request: tauri::ipc::Request<'_>) -> Result<String, String> {
    let tauri::ipc::InvokeBody::Raw(body) = request.body() else {
        return Err("expected the picture".into());
    };
    let profile = request
        .headers()
        .get("x-profile")
        .and_then(|v| v.to_str().ok())
        .unwrap_or("stream")
        .to_string();
    save_thumbnail(&profile, body).map(|p| p.display().to_string())
}

#[tauri::command]
pub fn open_thumbnails() {
    let dir = thumbnails_dir();
    std::fs::create_dir_all(&dir).ok();
    Command::new("explorer").arg(dir).spawn().ok();
}

#[tauri::command]
pub fn open_link_external(url: String) -> Result<(), String> {
    open_link(&url)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn vars(pairs: &[(&str, &str)]) -> Vars {
        pairs
            .iter()
            .map(|(k, v)| (k.to_string(), v.to_string()))
            .collect()
    }

    #[test]
    fn fills_known_keeps_unknown() {
        let v = vars(&[("game", "BGMI"), ("n", "12")]);
        assert_eq!(fill("🔴 {game} Day {n}", &v), "🔴 BGMI Day 12");
        assert_eq!(fill("{nope} {game}", &v), "{nope} BGMI");
        assert_eq!(fill("a { b } {game", &v), "a { b } {game");
        assert_eq!(fill("தமிழ் {game}!", &v), "தமிழ் BGMI!");
    }

    #[test]
    fn twitch_tags_are_cleaned() {
        let t = twitch_tags("Tamil, BGMI, Custom Rooms, Mobile-Gaming, tamil, averyveryverylongtagnamethatgoesonandon");
        assert_eq!(
            t,
            vec![
                "Tamil",
                "BGMI",
                "CustomRooms",
                "MobileGaming",
                "averyveryverylongtagnamet"
            ]
        );
        assert!(twitch_tags(&"a,".repeat(30)).len() <= 10);
    }

    #[test]
    fn starter_profiles_render_cleanly() {
        let mut store = Store::default();
        store.defaults.channel = "Phantom Blitz".into();
        store.defaults.youtube_handle = "@phantomblitz".into();
        for p in starter_profiles() {
            let extra = vars(&[("game", "Valorant"), ("room", "1234567"), ("pass", "99")]);
            let r = render(&store, &p.id, &extra).unwrap();
            let title = r["youtube"]["title"].as_str().unwrap();
            assert!(!title.contains('{'), "{}: {title}", p.id);
            assert!(
                title.chars().count() <= 100,
                "{}: {} chars",
                p.id,
                title.chars().count()
            );
            let desc = r["youtube"]["description"].as_str().unwrap();
            assert!(desc.contains("Phantom Blitz"));
            assert!(desc.contains("https://youtube.com/@phantomblitz"));
            let discord = r["discord"]["content"].as_str().unwrap();
            assert!(
                discord.is_empty() || !discord.contains('{'),
                "{}: {discord}",
                p.id
            );
            assert!(!desc.contains("Instagram:"), "empty link lines are dropped");
            assert!(
                r["warnings"].as_array().unwrap().is_empty(),
                "{}: {:?}",
                p.id,
                r["warnings"]
            );
            assert!(r["youtube"]["tags"].as_str().unwrap().len() < 500);
        }
        let r = render(
            &store,
            "rooms",
            &vars(&[("room", "1234567"), ("pass", "99")]),
        )
        .unwrap();
        assert_eq!(r["banner"]["room"], "Room ID: 1234567 · Pass: 99");
    }

    #[test]
    fn ready_made_texts_are_english() {
        let d = Defaults::default();
        assert!(!has_tamil(&d.rules) && !has_tamil(&d.footer));
        for p in starter_profiles() {
            for f in [
                &p.title,
                &p.description,
                &p.announce_text,
                &p.thumb.headline,
                &p.thumb.subline,
                &p.mode,
            ] {
                assert!(!has_tamil(f), "{}: {f}", p.id);
            }
        }
        let r = render(&Store::default(), "bgmi", &Vars::new()).unwrap();
        let day = r["youtube"]["title"].as_str().unwrap();
        assert!(!has_tamil(day));
    }

    #[test]
    fn old_store_gets_new_templates_and_keeps_personal_bits() {
        // A store saved by the first version: Tamil texts, no text_version.
        let old = json!({
            "defaults": { "channel": "Phantom Blitz", "youtubeHandle": "charlescj2126",
                          "rules": "• எல்லாரையும் respect பண்ணுங்க", "footer": "வணக்கம்! {channel}" },
            "profiles": [
                { "id": "bgmi", "name": "BGMI", "kind": "phone", "title": "🔴 BGMI Tamil Live | {mode} பண்ணலாம் வாங்க!",
                  "count": 7, "announce": false, "twitch": { "id": "123", "name": "Battlegrounds Mobile India" } },
                { "id": "mine", "name": "Mine", "kind": "phone", "title": "என் title" }
            ],
            "active": "bgmi"
        });
        let mut s: Store = serde_json::from_value(old).unwrap();
        assert_eq!(
            s.text_version, 0,
            "missing field must read as the old version"
        );
        assert!(upgrade_texts(&mut s));
        assert!(!has_tamil(&s.defaults.rules) && !has_tamil(&s.defaults.footer));
        assert_eq!(s.defaults.channel, "Phantom Blitz");
        assert_eq!(s.defaults.youtube_handle, "charlescj2126");
        let bgmi = s.profiles.iter().find(|p| p.id == "bgmi").unwrap();
        assert!(bgmi.title.starts_with("🔴 BGMI LIVE TAMIL"));
        assert_eq!(bgmi.count, 7, "stream count kept");
        assert!(!bgmi.announce, "user's switch kept");
        assert_eq!(bgmi.twitch.id, "123", "chosen Twitch category kept");
        let mine = s.profiles.iter().find(|p| p.id == "mine").unwrap();
        assert_eq!(
            mine.title, "என் title",
            "user's own profile is never touched"
        );
        for id in ["fc26", "ets2", "wd2", "detroit", "pubgpc"] {
            assert!(
                s.profiles.iter().any(|p| p.id == id),
                "new profile {id} added"
            );
        }
        assert!(!upgrade_texts(&mut s), "runs once");
    }

    #[test]
    fn empty_links_and_empty_sections_disappear() {
        let mut store = Store::default();
        store.defaults.channel = "Phantom Blitz".into();
        store.defaults.youtube_handle = "@charlescj2126".into();
        let r = render(&store, "bgmi", &Vars::new()).unwrap();
        let d = r["youtube"]["description"].as_str().unwrap();
        assert!(d.contains("▶ YouTube: https://youtube.com/@charlescj2126"));
        assert!(d.contains("⭐ Join membership: https://youtube.com/@charlescj2126/join"));
        assert!(!d.contains("Instagram:") && !d.contains("Twitch:") && !d.contains("Business:"));
        assert!(
            !d.contains("MY SETUP"),
            "no phone/PC filled in, so the whole section goes"
        );
        assert!(d.contains("★ CHAT RULES ★\n❌ No spam"));
        assert!(d.contains("KRAFTON"), "disclaimer included");
        assert!(!d.contains("\n\n\n"));
        store.defaults.device = "OnePlus 13R".into();
        let d2 = render(&store, "bgmi", &Vars::new()).unwrap()["youtube"]["description"]
            .as_str()
            .unwrap()
            .to_string();
        assert!(d2.contains("🎮 MY SETUP:\n📱 Phone: OnePlus 13R"));
        assert!(!d2.contains("💻 PC:"));
    }

    #[test]
    fn titles_fit_youtube_even_at_stream_999() {
        let mut store = Store::default();
        for p in store.profiles.iter_mut() {
            p.count = 998;
        }
        for p in starter_profiles() {
            let extra: Vars = [
                ("game".to_string(), "Grand Theft Auto V".to_string()),
                ("room".to_string(), "1".to_string()),
            ]
            .into();
            let t = render(&store, &p.id, &extra).unwrap()["youtube"]["title"]
                .as_str()
                .unwrap()
                .to_string();
            assert!(
                t.chars().count() <= 100,
                "{}: {} chars: {t}",
                p.id,
                t.chars().count()
            );
        }
    }
    #[test]
    fn pc_profile_asks_for_game() {
        let r = render(&Store::default(), "pc", &Vars::new()).unwrap();
        assert!(r["warnings"]
            .as_array()
            .unwrap()
            .iter()
            .any(|w| w.as_str().unwrap().contains("game name")));
    }

    /// Real request: Twitch must reject a made-up Client ID with a clear
    /// message. Proves the curl config, status parsing and error mapping.
    /// `cargo test twitch_rejects_fake_client -- --ignored --nocapture`
    #[test]
    #[ignore]
    fn twitch_rejects_fake_client() {
        let (status, v) = curl(&format!(
            "url = \"https://id.twitch.tv/oauth2/device\"\ndata-urlencode = {}\ndata-urlencode = {}",
            cq("client_id=aaaaaaaaaaaaaaaaaaaaaaaaaaaaaa"),
            cq(&format!("scopes={TWITCH_SCOPES}"))
        ))
        .unwrap();
        println!("status {status}: {v}");
        assert_eq!(status, 400);
        assert!(
            twitch_error(status, &v).contains("Client ID"),
            "{}",
            twitch_error(status, &v)
        );
    }

    /// Writes real get_profiles/render output for the UI preview harness.
    /// `PHONECAST_DUMP=<file> cargo test dump_ui_samples -- --ignored`
    #[test]
    #[ignore]
    fn dump_ui_samples() {
        let out = std::env::var("PHONECAST_DUMP").expect("PHONECAST_DUMP");
        let mut store = Store::default();
        store.defaults.channel = "Phantom Blitz".into();
        store.defaults.youtube_handle = "phantomblitz".into();
        store.defaults.instagram = "https://instagram.com/phantom.blitzz".into();
        let p = Profiles {
            store: Mutex::new(store.clone()),
            login: Arc::new(Mutex::new(Login::default())),
        };
        let mut renders = serde_json::Map::new();
        for pr in &store.profiles {
            let extra = vars(&[
                ("room", "4471920"),
                ("pass", "2468"),
                ("map", "Erangel · TPP squad"),
                ("game", "Valorant"),
            ]);
            renders.insert(pr.id.clone(), render(&store, &pr.id, &extra).unwrap());
        }
        std::fs::write(
            out,
            json!({ "profiles": p.json(), "renders": renders }).to_string(),
        )
        .unwrap();
    }

    #[test]
    fn curl_config_quoting() {
        assert_eq!(cq(r#"a "b" \c"#), r#""a \"b\" \\c""#);
    }
}
