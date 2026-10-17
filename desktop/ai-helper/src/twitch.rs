//! Twitch chat through the official API: device-code login (no password
//! ever touches PhoneCast), EventSub over WebSocket for reading, Helix
//! `chat/messages` for the replies the streamer approved.
//!
//! The streamer uses their own Twitch application (a free "public" client
//! created at dev.twitch.tv), so PhoneCast ships no secret and no shared
//! quota.

use crate::proto::ChatMessage;
use crate::secret;
use serde::{Deserialize, Serialize};
use serde_json::{json, Value};
use std::path::PathBuf;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::Arc;
use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};

pub const SCOPES: &str = "user:read:chat user:write:chat user:bot channel:bot";
const EVENTSUB_WS: &str = "wss://eventsub.wss.twitch.tv/ws";

#[derive(Debug, Clone, Serialize, Deserialize, Default)]
pub struct Token {
    pub access: String,
    pub refresh: String,
    pub user_id: String,
    pub login: String,
    pub client_id: String,
}

fn token_path() -> PathBuf {
    let base = std::env::var("APPDATA")
        .map(PathBuf::from)
        .unwrap_or_else(|_| PathBuf::from("."));
    base.join("PhoneCast").join("ai").join("twitch.json")
}

pub fn load_token() -> Option<Token> {
    let enc = std::fs::read_to_string(token_path()).ok()?;
    let plain = secret::unprotect(enc.trim()).ok()?;
    serde_json::from_str(&plain).ok()
}

pub fn save_token(t: &Token) -> Result<(), String> {
    let p = token_path();
    if let Some(d) = p.parent() {
        std::fs::create_dir_all(d).map_err(|e| e.to_string())?;
    }
    let enc = secret::protect(&serde_json::to_string(t).map_err(|e| e.to_string())?)?;
    std::fs::write(p, enc).map_err(|e| e.to_string())
}

pub fn forget_token() {
    std::fs::remove_file(token_path()).ok();
}

/// What the streamer must do to log in.
#[derive(Debug, Clone)]
pub struct DeviceCode {
    pub device_code: String,
    pub user_code: String,
    pub verification_uri: String,
    pub interval: u64,
    pub expires_in: u64,
}

fn post_form(url: &str, form: &[(&str, &str)]) -> Result<Value, String> {
    let resp = ureq::post(url)
        .timeout(Duration::from_secs(20))
        .send_form(form);
    match resp {
        Ok(r) => r.into_json().map_err(|e| e.to_string()),
        Err(ureq::Error::Status(_, r)) => r.into_json().map_err(|e| e.to_string()),
        Err(e) => Err(e.to_string()),
    }
}

pub fn start_device_login(client_id: &str) -> Result<DeviceCode, String> {
    let v = post_form(
        "https://id.twitch.tv/oauth2/device",
        &[("client_id", client_id), ("scopes", SCOPES)],
    )?;
    let s = |k: &str| v[k].as_str().map(str::to_string);
    Ok(DeviceCode {
        device_code: s("device_code").ok_or_else(|| describe_error(&v))?,
        user_code: s("user_code").unwrap_or_default(),
        verification_uri: s("verification_uri").unwrap_or_default(),
        interval: v["interval"].as_u64().unwrap_or(5),
        expires_in: v["expires_in"].as_u64().unwrap_or(1800),
    })
}

fn describe_error(v: &Value) -> String {
    v["message"]
        .as_str()
        .or(v["error"].as_str())
        .unwrap_or("Twitch did not accept the request (check the Client ID)")
        .to_string()
}

/// Polls until the streamer finished in the browser. `cancel` stops it.
pub fn wait_for_device_login(
    client_id: &str,
    dc: &DeviceCode,
    cancel: &AtomicBool,
) -> Result<Token, String> {
    let deadline = Instant::now() + Duration::from_secs(dc.expires_in);
    let mut interval = dc.interval.max(1);
    while Instant::now() < deadline && !cancel.load(Ordering::Relaxed) {
        std::thread::sleep(Duration::from_secs(interval));
        let v = post_form(
            "https://id.twitch.tv/oauth2/token",
            &[
                ("client_id", client_id),
                ("device_code", &dc.device_code),
                ("grant_type", "urn:ietf:params:oauth:grant-type:device_code"),
            ],
        )?;
        if let Some(access) = v["access_token"].as_str() {
            let mut t = Token {
                access: access.into(),
                refresh: v["refresh_token"].as_str().unwrap_or_default().into(),
                client_id: client_id.into(),
                ..Default::default()
            };
            fill_user(&mut t)?;
            return Ok(t);
        }
        match v["message"].as_str().unwrap_or("") {
            "authorization_pending" => {}
            "slow_down" => interval += 5,
            "" => {}
            other => return Err(other.to_string()),
        }
    }
    Err("login cancelled or timed out".into())
}

fn helix(method: &str, path: &str, t: &Token, body: Option<Value>) -> Result<Value, String> {
    let url = format!("https://api.twitch.tv/helix/{path}");
    let req = ureq::request(method, &url)
        .timeout(Duration::from_secs(20))
        .set("Authorization", &format!("Bearer {}", t.access))
        .set("Client-Id", &t.client_id);
    let resp = match body {
        Some(b) => req.send_json(b),
        None => req.call(),
    };
    match resp {
        Ok(r) => r.into_json().map_err(|e| e.to_string()),
        Err(ureq::Error::Status(401, _)) => Err("unauthorized".into()),
        Err(ureq::Error::Status(code, r)) => {
            let v: Value = r.into_json().unwrap_or(Value::Null);
            Err(format!("Twitch error {code}: {}", describe_error(&v)))
        }
        Err(e) => Err(e.to_string()),
    }
}

fn fill_user(t: &mut Token) -> Result<(), String> {
    let v = helix("GET", "users", t, None)?;
    let u = &v["data"][0];
    t.user_id = u["id"].as_str().unwrap_or_default().into();
    t.login = u["login"].as_str().unwrap_or_default().into();
    if t.user_id.is_empty() {
        return Err("could not read the Twitch account".into());
    }
    Ok(())
}

pub fn refresh(t: &mut Token) -> Result<(), String> {
    let v = post_form(
        "https://id.twitch.tv/oauth2/token",
        &[
            ("client_id", &t.client_id),
            ("grant_type", "refresh_token"),
            ("refresh_token", &t.refresh),
        ],
    )?;
    let access = v["access_token"]
        .as_str()
        .ok_or_else(|| describe_error(&v))?;
    t.access = access.into();
    if let Some(r) = v["refresh_token"].as_str() {
        t.refresh = r.into();
    }
    Ok(())
}

/// Posts a message in the streamer's own chat.
pub fn send_chat(t: &Token, text: &str) -> Result<(), String> {
    let v = helix(
        "POST",
        "chat/messages",
        t,
        Some(json!({
            "broadcaster_id": t.user_id,
            "sender_id": t.user_id,
            "message": text.chars().take(500).collect::<String>(),
        })),
    )?;
    if v["data"][0]["is_sent"].as_bool() == Some(false) {
        return Err(v["data"][0]["drop_reason"]["message"]
            .as_str()
            .unwrap_or("message was dropped")
            .into());
    }
    Ok(())
}

/// Turns an EventSub `channel.chat.message` notification into a ChatMessage.
/// Returns None for other message types.
pub fn parse_eventsub(raw: &str) -> Option<Parsed> {
    let v: Value = serde_json::from_str(raw).ok()?;
    let kind = v["metadata"]["message_type"].as_str()?;
    match kind {
        "session_welcome" => Some(Parsed::Welcome(
            v["payload"]["session"]["id"].as_str()?.to_string(),
        )),
        "session_reconnect" => Some(Parsed::Reconnect(
            v["payload"]["session"]["reconnect_url"]
                .as_str()
                .unwrap_or(EVENTSUB_WS)
                .to_string(),
        )),
        "session_keepalive" => Some(Parsed::Keepalive),
        "notification" => {
            if v["metadata"]["subscription_type"].as_str()? != "channel.chat.message" {
                return None;
            }
            let e = &v["payload"]["event"];
            let frags = e["message"]["fragments"].as_array();
            let (emote_chars, total_chars) = frags
                .map(|f| {
                    f.iter().fold((0usize, 0usize), |(e, t), fr| {
                        let n = fr["text"].as_str().map(|s| s.chars().count()).unwrap_or(0);
                        let is_emote = matches!(fr["type"].as_str(), Some("emote" | "cheermote"));
                        (e + if is_emote { n } else { 0 }, t + n)
                    })
                })
                .unwrap_or((0, 0));
            let privileged = e["badges"].as_array().is_some_and(|b| {
                b.iter().any(|x| {
                    matches!(
                        x["set_id"].as_str(),
                        Some("moderator" | "broadcaster" | "vip")
                    )
                })
            });
            Some(Parsed::Message {
                msg: ChatMessage {
                    id: e["message_id"].as_str().unwrap_or_default().into(),
                    platform: "twitch".into(),
                    user: e["chatter_user_name"]
                        .as_str()
                        .or(e["chatter_user_login"].as_str())
                        .unwrap_or("?")
                        .into(),
                    text: e["message"]["text"].as_str().unwrap_or_default().into(),
                    privileged,
                    spam: None,
                    ts: now_ms(),
                },
                emote_share: if total_chars > 0 {
                    emote_chars as f64 / total_chars as f64
                } else {
                    0.0
                },
            })
        }
        _ => None,
    }
}

#[derive(Debug)]
pub enum Parsed {
    Welcome(String),
    Reconnect(String),
    Keepalive,
    Message { msg: ChatMessage, emote_share: f64 },
}

pub fn now_ms() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_millis() as u64)
        .unwrap_or(0)
}

/// Runs the EventSub connection until `stop` is set, calling `on_message`
/// for every chat line and `on_state` with a short human-readable state.
pub fn run(
    token: Arc<std::sync::Mutex<Token>>,
    stop: Arc<AtomicBool>,
    on_state: &dyn Fn(String),
    on_message: &dyn Fn(ChatMessage, f64),
) {
    let mut url = EVENTSUB_WS.to_string();
    let mut backoff = 2u64;
    while !stop.load(Ordering::Relaxed) {
        on_state("connecting".into());
        let conn = tungstenite::connect(&url);
        let (mut ws, _) = match conn {
            Ok(c) => c,
            Err(e) => {
                on_state(format!("error: {e}"));
                sleep_until(&stop, backoff);
                backoff = (backoff * 2).min(60);
                continue;
            }
        };
        url = EVENTSUB_WS.to_string();
        // Fail fast on a dead socket instead of blocking forever.
        if let tungstenite::stream::MaybeTlsStream::Rustls(s) = ws.get_mut() {
            s.get_mut()
                .set_read_timeout(Some(Duration::from_secs(40)))
                .ok();
        }
        let mut subscribed = false;
        loop {
            if stop.load(Ordering::Relaxed) {
                ws.close(None).ok();
                return;
            }
            let msg = match ws.read() {
                Ok(m) => m,
                Err(e) => {
                    on_state(format!("reconnecting ({e})"));
                    break;
                }
            };
            let text = match msg {
                tungstenite::Message::Text(t) => t,
                tungstenite::Message::Ping(p) => {
                    ws.send(tungstenite::Message::Pong(p)).ok();
                    continue;
                }
                tungstenite::Message::Close(_) => break,
                _ => continue,
            };
            match parse_eventsub(&text) {
                Some(Parsed::Welcome(session_id)) => {
                    if !subscribed {
                        match subscribe(&token, &session_id) {
                            Ok(()) => {
                                subscribed = true;
                                backoff = 2;
                                on_state("connected".into());
                            }
                            Err(e) => {
                                on_state(format!("error: {e}"));
                                ws.close(None).ok();
                                sleep_until(&stop, backoff);
                                backoff = (backoff * 2).min(60);
                                break;
                            }
                        }
                    }
                }
                Some(Parsed::Reconnect(new_url)) => {
                    url = new_url;
                    break;
                }
                Some(Parsed::Message { msg, emote_share }) => on_message(msg, emote_share),
                Some(Parsed::Keepalive) | None => {}
            }
        }
    }
}

fn subscribe(token: &std::sync::Mutex<Token>, session_id: &str) -> Result<(), String> {
    let body = |t: &Token| {
        json!({
            "type": "channel.chat.message",
            "version": "1",
            "condition": { "broadcaster_user_id": t.user_id, "user_id": t.user_id },
            "transport": { "method": "websocket", "session_id": session_id },
        })
    };
    let t = token.lock().unwrap().clone();
    match helix("POST", "eventsub/subscriptions", &t, Some(body(&t))) {
        Ok(_) => Ok(()),
        Err(e) if e == "unauthorized" => {
            let mut t2 = t.clone();
            refresh(&mut t2)?;
            save_token(&t2).ok();
            *token.lock().unwrap() = t2.clone();
            helix("POST", "eventsub/subscriptions", &t2, Some(body(&t2))).map(|_| ())
        }
        Err(e) => Err(e),
    }
}

fn sleep_until(stop: &AtomicBool, secs: u64) {
    for _ in 0..secs * 10 {
        if stop.load(Ordering::Relaxed) {
            return;
        }
        std::thread::sleep(Duration::from_millis(100));
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_notification() {
        let raw = r#"{"metadata":{"message_type":"notification","subscription_type":"channel.chat.message"},
          "payload":{"event":{"message_id":"m1","chatter_user_name":"Ann","badges":[{"set_id":"moderator"}],
          "message":{"text":"Kappa hi","fragments":[{"type":"emote","text":"Kappa"},{"type":"text","text":" hi"}]}}}}"#;
        match parse_eventsub(raw) {
            Some(Parsed::Message { msg, emote_share }) => {
                assert_eq!(msg.user, "Ann");
                assert_eq!(msg.text, "Kappa hi");
                assert!(msg.privileged);
                assert!((emote_share - 5.0 / 8.0).abs() < 1e-9);
            }
            other => panic!("{other:?}"),
        }
    }

    #[test]
    fn parses_session_messages() {
        let w =
            r#"{"metadata":{"message_type":"session_welcome"},"payload":{"session":{"id":"abc"}}}"#;
        assert!(matches!(parse_eventsub(w), Some(Parsed::Welcome(id)) if id == "abc"));
        let k = r#"{"metadata":{"message_type":"session_keepalive"},"payload":{}}"#;
        assert!(matches!(parse_eventsub(k), Some(Parsed::Keepalive)));
        let r = r#"{"metadata":{"message_type":"session_reconnect"},"payload":{"session":{"reconnect_url":"wss://x"}}}"#;
        assert!(matches!(parse_eventsub(r), Some(Parsed::Reconnect(u)) if u == "wss://x"));
        assert!(parse_eventsub("not json").is_none());
    }
}
