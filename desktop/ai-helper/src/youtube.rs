//! YouTube live chat, read-only, through the same web endpoint the YouTube
//! page itself uses (no API key, no quota, no login). The streamer gives
//! their channel handle; the helper finds the current live video from
//! `youtube.com/@handle/live`.
//!
//! Replies on YouTube would need a Google OAuth client; PhoneCast does not
//! ship one, so FAQ answers for YouTube viewers are shown to the streamer
//! to paste, never posted.

use crate::proto::ChatMessage;
use serde_json::{json, Value};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::Arc;
use std::time::Duration;

const UA: &str = "Mozilla/5.0 (Windows NT 10.0; Win64; x64) PhoneCast/0.1";

/// Accepts `@handle`, `https://youtube.com/@handle`, a channel URL or a
/// `watch?v=` / `youtu.be` link. Returns a URL whose page names the live
/// video.
pub fn live_page_url(input: &str) -> Option<String> {
    let s = input.trim();
    if s.is_empty() {
        return None;
    }
    if let Some(id) = video_id_from_url(s) {
        return Some(format!("https://www.youtube.com/watch?v={id}"));
    }
    if let Some(h) = s.strip_prefix('@') {
        return Some(format!("https://www.youtube.com/@{h}/live"));
    }
    if s.contains("youtube.com/") {
        let path = s.split("youtube.com/").nth(1)?.trim_end_matches('/');
        let path = path.strip_suffix("/live").unwrap_or(path);
        return Some(format!("https://www.youtube.com/{path}/live"));
    }
    if !s.contains('/') && !s.contains(' ') {
        return Some(format!("https://www.youtube.com/@{s}/live"));
    }
    None
}

pub fn video_id_from_url(s: &str) -> Option<String> {
    let after = s
        .split_once("v=")
        .map(|x| x.1)
        .or_else(|| s.split_once("youtu.be/").map(|x| x.1))
        .or_else(|| s.split_once("/live/").map(|x| x.1))?;
    let id: String = after
        .chars()
        .take_while(|c| c.is_ascii_alphanumeric() || *c == '-' || *c == '_')
        .collect();
    (id.len() == 11).then_some(id)
}

/// Finds `"videoId":"…"` in a watch or /live page.
pub fn find_video_id(html: &str) -> Option<String> {
    for key in ["\"videoId\":\"", "watch?v="] {
        let mut rest = html;
        while let Some(i) = rest.find(key) {
            let cand: String = rest[i + key.len()..]
                .chars()
                .take_while(|c| c.is_ascii_alphanumeric() || *c == '-' || *c == '_')
                .collect();
            if cand.len() == 11 {
                return Some(cand);
            }
            rest = &rest[i + key.len()..];
        }
    }
    None
}

fn find_quoted(html: &str, key: &str) -> Option<String> {
    let i = html.find(key)? + key.len();
    let rest = &html[i..];
    let end = rest.find('"')?;
    Some(rest[..end].to_string())
}

/// From the live_chat page: API key, client version and first continuation.
pub fn page_params(html: &str) -> Option<(String, String, String)> {
    let key = find_quoted(html, "\"INNERTUBE_API_KEY\":\"")?;
    let ver = find_quoted(html, "\"INNERTUBE_CLIENT_VERSION\":\"")
        .or_else(|| find_quoted(html, "\"clientVersion\":\""))?;
    // The first continuation lives in ytInitialData → … → continuations.
    let cont = find_quoted(html, "\"continuation\":\"")?;
    Some((key, ver, cont))
}

#[derive(Debug, Default)]
pub struct Page {
    pub messages: Vec<(ChatMessage, f64)>,
    pub continuation: Option<String>,
    pub timeout_ms: u64,
}

/// Parses a `get_live_chat` response.
pub fn parse_chat(v: &Value) -> Page {
    let mut page = Page {
        timeout_ms: 2000,
        ..Default::default()
    };
    let lcc = &v["continuationContents"]["liveChatContinuation"];
    if let Some(conts) = lcc["continuations"].as_array() {
        for c in conts {
            for k in [
                "timedContinuationData",
                "invalidationContinuationData",
                "reloadContinuationData",
            ] {
                if let Some(s) = c[k]["continuation"].as_str() {
                    page.continuation = Some(s.into());
                    page.timeout_ms = c[k]["timeoutMs"]
                        .as_u64()
                        .unwrap_or(2000)
                        .clamp(500, 10_000);
                }
            }
        }
    }
    if let Some(actions) = lcc["actions"].as_array() {
        for a in actions {
            let r = &a["addChatItemAction"]["item"]["liveChatTextMessageRenderer"];
            if r.is_null() {
                continue;
            }
            let mut text = String::new();
            let mut emote_chars = 0usize;
            if let Some(runs) = r["message"]["runs"].as_array() {
                for run in runs {
                    if let Some(t) = run["text"].as_str() {
                        text.push_str(t);
                    } else if let Some(e) = run["emoji"].as_object() {
                        let short = e
                            .get("shortcuts")
                            .and_then(|s| s[0].as_str())
                            .or_else(|| e.get("emojiId").and_then(|s| s.as_str()))
                            .unwrap_or(":emoji:");
                        text.push_str(short);
                        emote_chars += short.chars().count();
                    }
                }
            }
            let privileged = r["authorBadges"].as_array().is_some_and(|b| {
                b.iter().any(|x| {
                    matches!(
                        x["liveChatAuthorBadgeRenderer"]["icon"]["iconType"].as_str(),
                        Some("MODERATOR" | "OWNER")
                    )
                })
            });
            let total = text.chars().count().max(1);
            page.messages.push((
                ChatMessage {
                    id: r["id"].as_str().unwrap_or_default().into(),
                    platform: "youtube".into(),
                    user: r["authorName"]["simpleText"].as_str().unwrap_or("?").into(),
                    text,
                    privileged,
                    spam: None,
                    ts: r["timestampUsec"]
                        .as_str()
                        .and_then(|s| s.parse::<u64>().ok())
                        .map(|us| us / 1000)
                        .unwrap_or_else(crate::twitch::now_ms),
                },
                emote_chars as f64 / total as f64,
            ));
        }
    }
    page
}

fn get(url: &str) -> Result<String, String> {
    ureq::get(url)
        .timeout(Duration::from_secs(20))
        .set("User-Agent", UA)
        .set("Accept-Language", "en-US,en;q=0.9")
        .call()
        .map_err(|e| e.to_string())?
        .into_string()
        .map_err(|e| e.to_string())
}

/// Reads chat until `stop` is set. `channel` is what the streamer typed.
pub fn run(
    channel: String,
    stop: Arc<AtomicBool>,
    on_state: &dyn Fn(String),
    on_message: &dyn Fn(ChatMessage, f64),
) {
    let Some(page_url) = live_page_url(&channel) else {
        on_state("error: enter your channel handle, e.g. @yourname".into());
        return;
    };
    let mut backoff = 5u64;
    while !stop.load(Ordering::Relaxed) {
        on_state("looking for your live stream".into());
        let video_id = match get(&page_url).map(|h| find_video_id(&h)) {
            Ok(Some(id)) => id,
            Ok(None) => {
                on_state("no live stream found yet (retrying)".into());
                sleep_until(&stop, 20);
                continue;
            }
            Err(e) => {
                on_state(format!("error: {e}"));
                sleep_until(&stop, backoff);
                backoff = (backoff * 2).min(120);
                continue;
            }
        };
        let html = match get(&format!("https://www.youtube.com/live_chat?v={video_id}")) {
            Ok(h) => h,
            Err(e) => {
                on_state(format!("error: {e}"));
                sleep_until(&stop, backoff);
                continue;
            }
        };
        let Some((key, ver, mut cont)) = page_params(&html) else {
            on_state("chat is not available for this stream (or YouTube changed its page)".into());
            sleep_until(&stop, 30);
            continue;
        };
        on_state("connected".into());
        backoff = 5;
        let mut seen: std::collections::VecDeque<String> = std::collections::VecDeque::new();
        while !stop.load(Ordering::Relaxed) {
            let body = json!({
                "context": { "client": { "clientName": "WEB", "clientVersion": ver, "hl": "en" } },
                "continuation": cont,
            });
            let resp = ureq::post(&format!(
                "https://www.youtube.com/youtubei/v1/live_chat/get_live_chat?key={key}&prettyPrint=false"
            ))
            .timeout(Duration::from_secs(20))
            .set("User-Agent", UA)
            .send_json(body)
            .map_err(|e| e.to_string())
            .and_then(|r| r.into_json::<Value>().map_err(|e| e.to_string()));
            let v = match resp {
                Ok(v) => v,
                Err(e) => {
                    on_state(format!("reconnecting ({e})"));
                    break;
                }
            };
            let page = parse_chat(&v);
            for (m, emote) in page.messages {
                if m.id.is_empty() || seen.contains(&m.id) {
                    continue;
                }
                seen.push_back(m.id.clone());
                if seen.len() > 500 {
                    seen.pop_front();
                }
                on_message(m, emote);
            }
            match page.continuation {
                Some(c) => cont = c,
                None => {
                    on_state("stream ended".into());
                    break;
                }
            }
            sleep_until_ms(&stop, page.timeout_ms);
        }
        sleep_until(&stop, 5);
    }
}

fn sleep_until(stop: &AtomicBool, secs: u64) {
    sleep_until_ms(stop, secs * 1000)
}

fn sleep_until_ms(stop: &AtomicBool, ms: u64) {
    let mut left = ms;
    while left > 0 && !stop.load(Ordering::Relaxed) {
        let d = left.min(100);
        std::thread::sleep(Duration::from_millis(d));
        left -= d;
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn channel_inputs() {
        assert_eq!(
            live_page_url("@phonecast").as_deref(),
            Some("https://www.youtube.com/@phonecast/live")
        );
        assert_eq!(
            live_page_url("phonecast").as_deref(),
            Some("https://www.youtube.com/@phonecast/live")
        );
        assert_eq!(
            live_page_url("https://www.youtube.com/@phonecast/live").as_deref(),
            Some("https://www.youtube.com/@phonecast/live")
        );
        assert_eq!(
            live_page_url("https://youtu.be/dQw4w9WgXcQ").as_deref(),
            Some("https://www.youtube.com/watch?v=dQw4w9WgXcQ")
        );
        assert_eq!(live_page_url(""), None);
    }

    #[test]
    fn page_extraction() {
        let html = r#"<script>var x={"INNERTUBE_API_KEY":"AIzaKEY","INNERTUBE_CLIENT_VERSION":"2.20261010.00.00"};
            ytInitialData = {"continuations":[{"timedContinuationData":{"continuation":"0ofMQ_CONT"}}]}</script>
            "videoId":"dQw4w9WgXcQ""#;
        assert_eq!(find_video_id(html).as_deref(), Some("dQw4w9WgXcQ"));
        let (k, v, c) = page_params(html).unwrap();
        assert_eq!(k, "AIzaKEY");
        assert_eq!(v, "2.20261010.00.00");
        assert_eq!(c, "0ofMQ_CONT");
    }

    #[test]
    fn chat_response() {
        let v: Value = serde_json::from_str(
            r#"{"continuationContents":{"liveChatContinuation":{
              "continuations":[{"timedContinuationData":{"timeoutMs":3000,"continuation":"NEXT"}}],
              "actions":[{"addChatItemAction":{"item":{"liveChatTextMessageRenderer":{
                 "id":"a1","timestampUsec":"1700000000000000","authorName":{"simpleText":"Bob"},
                 "authorBadges":[{"liveChatAuthorBadgeRenderer":{"icon":{"iconType":"MODERATOR"}}}],
                 "message":{"runs":[{"text":"hello "},{"emoji":{"shortcuts":[":fire:"]}}]}}}}},
                {"addChatItemAction":{"item":{"liveChatPaidMessageRenderer":{}}}}]}}}"#,
        )
        .unwrap();
        let p = parse_chat(&v);
        assert_eq!(p.continuation.as_deref(), Some("NEXT"));
        assert_eq!(p.timeout_ms, 3000);
        assert_eq!(p.messages.len(), 1);
        let (m, emote) = &p.messages[0];
        assert_eq!(m.user, "Bob");
        assert_eq!(m.text, "hello :fire:");
        assert!(m.privileged);
        assert_eq!(m.ts, 1_700_000_000_000);
        assert!(*emote > 0.4 && *emote < 0.6);
    }
}
