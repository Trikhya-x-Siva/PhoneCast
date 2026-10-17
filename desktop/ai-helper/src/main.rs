// No console window in release builds.
#![cfg_attr(not(debug_assertions), windows_subsystem = "windows")]

//! The helper process. Started by PhoneCast as
//! `phonecast-ai-helper --port <port> --token <token>`; connects back to the
//! app on 127.0.0.1 and exits as soon as the socket closes or the app stops
//! sending audio, so it can never outlive the app.

use base64::Engine;
use phonecast_ai_helper::proto::{
    ChatMessage, FromHelper, ModelStatus, Settings, Status, ToHelper,
};
use phonecast_ai_helper::score::{Config as ScoreConfig, Detector};
use phonecast_ai_helper::spam::{Incoming, SpamConfig, SpamFilter};
use phonecast_ai_helper::{faq::Faq, models, sysusage::Usage, twitch, youtube};
use std::collections::HashMap;
use std::io::{BufRead, BufReader, BufWriter, Write};
use std::net::TcpStream;
use std::path::PathBuf;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{mpsc, Arc, Mutex};
use std::time::{Duration, Instant};

struct Out {
    w: Mutex<BufWriter<TcpStream>>,
}

impl Out {
    fn send(&self, m: &FromHelper) {
        if let Ok(mut w) = self.w.lock() {
            if serde_json::to_writer(&mut *w, m).is_ok() {
                w.write_all(b"\n").ok();
                w.flush().ok();
            }
        }
    }
    fn toast(&self, text: impl Into<String>) {
        self.send(&FromHelper::Toast { text: text.into() });
    }
    fn modlog(&self, line: impl Into<String>) {
        self.send(&FromHelper::ModLog { line: line.into() });
    }
}

#[derive(Default)]
struct ModelState {
    state: String,
    progress: f64,
    detail: String,
}

struct Reader {
    stop: Arc<AtomicBool>,
    key: String,
}

struct PendingFaq {
    platform: String,
    answer: String,
}

struct State {
    settings: Settings,
    detector: Detector,
    spam: SpamFilter,
    faq: Faq,
    faq_error: String,
    twitch_token: Option<Arc<Mutex<twitch::Token>>>,
    twitch_state: String,
    youtube_state: String,
    twitch_reader: Option<Reader>,
    youtube_reader: Option<Reader>,
    login_cancel: Option<Arc<AtomicBool>>,
    models: HashMap<&'static str, ModelState>,
    pending: HashMap<u64, PendingFaq>,
    next_id: u64,
    #[cfg(feature = "yamnet")]
    classifier: Option<Arc<Mutex<phonecast_ai_helper::yamnet::Classifier>>>,
    #[cfg(feature = "llm")]
    llm: Option<Arc<phonecast_ai_helper::llm::Rephraser>>,
}

struct App {
    out: Arc<Out>,
    st: Arc<Mutex<State>>,
    start: Instant,
    last_from_app: Arc<Mutex<Instant>>,
    pcm_tx: Mutex<Option<mpsc::Sender<Vec<f32>>>>,
    clock: Mutex<Option<(f64, Instant)>>,
}

fn ai_dir() -> PathBuf {
    let base = std::env::var("APPDATA")
        .map(PathBuf::from)
        .unwrap_or_else(|_| PathBuf::from("."));
    base.join("PhoneCast").join("ai")
}

fn faq_path() -> PathBuf {
    ai_dir().join("faq.yaml")
}

const SAMPLE_FAQ: &str = r#"# PhoneCast chat helper: questions and answers.
# Viewers type "!ask <question>" (or you approve a suggested reply in the
# AI tools panel). Keywords in "q" and "also" are matched against the
# question; "a" is the reply. Edit freely and click "Reload FAQ".
- q: What phone do you play on?
  also: [which phone, phone model, your device]
  a: I play on my phone and stream it with PhoneCast (free, open source).
- q: What is your sensitivity?
  also: [sens, dpi, settings]
  a: Default sensitivity, I just practise a lot :)
- q: When do you stream?
  also: [schedule, stream times, next stream]
  a: Most evenings, follow to get notified.
"#;

impl App {
    /// Seconds on the helper clock, or on the app's clock once it sends one.
    fn now(&self) -> f64 {
        if let Some((ts, at)) = *self.clock.lock().unwrap() {
            return ts + at.elapsed().as_secs_f64();
        }
        self.start.elapsed().as_secs_f64()
    }

    fn sync_clock(&self, ts: f64) {
        *self.clock.lock().unwrap() = Some((ts, Instant::now()));
    }

    fn handle(self: &Arc<Self>, msg: ToHelper) {
        match msg {
            ToHelper::Settings { settings } => self.apply_settings(settings),
            ToHelper::Audio { game, mic, pcm, ts } => {
                if let Some(ts) = ts {
                    self.sync_clock(ts);
                }
                let hl = self
                    .st
                    .lock()
                    .unwrap()
                    .detector
                    .on_audio(self.now(), game, mic);
                if let Some(h) = hl {
                    self.highlight(h);
                }
                if let Some(b64) = pcm {
                    if let Ok(bytes) = base64::engine::general_purpose::STANDARD.decode(b64) {
                        let samples: Vec<f32> = bytes
                            .as_chunks::<2>()
                            .0
                            .iter()
                            .map(|c| i16::from_le_bytes(*c) as f32 / 32768.0)
                            .collect();
                        if let Some(tx) = self.pcm_tx.lock().unwrap().as_ref() {
                            tx.send(samples).ok();
                        }
                    }
                }
            }
            ToHelper::Download { model } => self.download(&model),
            ToHelper::RemoveModel { model } => {
                if let Some(spec) = models::by_id(&model) {
                    spec.remove();
                    self.unload_model(spec.id);
                    self.set_model(spec.id, "missing", 0.0, "");
                    self.out.toast(format!("{} removed", spec.name));
                }
            }
            ToHelper::TwitchLogin => self.twitch_login(),
            ToHelper::TwitchLogout => {
                twitch::forget_token();
                let mut s = self.st.lock().unwrap();
                s.twitch_token = None;
                stop_reader(&mut s.twitch_reader);
                s.twitch_state = "logged out".into();
                drop(s);
                self.out.toast("Twitch login removed");
            }
            ToHelper::Approve { id } => self.approve(id),
            ToHelper::Dismiss { id } => {
                self.st.lock().unwrap().pending.remove(&id);
            }
            ToHelper::ReloadFaq => {
                self.load_faq();
                let s = self.st.lock().unwrap();
                if s.faq_error.is_empty() {
                    self.out
                        .toast(format!("FAQ loaded: {} answers", s.faq.len()));
                } else {
                    self.out.toast(format!("FAQ problem: {}", s.faq_error));
                }
            }
            ToHelper::NewSession => self.st.lock().unwrap().detector.reset_session(),
            ToHelper::ClipResult { ok, detail } => {
                if !ok {
                    self.out.modlog(format!("clip not saved: {detail}"));
                }
            }
            ToHelper::Shutdown => std::process::exit(0),
        }
    }

    fn highlight(&self, h: phonecast_ai_helper::score::Highlight) {
        self.out.modlog(format!(
            "highlight: score {:.1} ({}){}",
            h.score,
            h.reason,
            h.label
                .as_ref()
                .map(|l| format!(", sound: {l}"))
                .unwrap_or_default()
        ));
        self.out.send(&FromHelper::Highlight {
            score: h.score,
            label: h.label,
            seconds_ago: h.seconds_ago,
            length: h.length,
            reason: h.reason,
        });
    }

    fn apply_settings(self: &Arc<Self>, settings: Settings) {
        let mut s = self.st.lock().unwrap();
        let hs = &settings.highlights;
        s.detector.set_config(ScoreConfig {
            sensitivity: hs.sensitivity,
            clip_seconds: hs.clip_seconds.clamp(10, 120),
            max_per_session: hs.max_per_session,
            cooldown_seconds: 45.0,
        });
        let sp = &settings.chat.spam;
        s.spam.set_config(SpamConfig {
            max_per_user_10s: if sp.enabled {
                sp.max_per_user_10s.max(1)
            } else {
                u32::MAX
            },
            flag_links: sp.enabled && sp.flag_links,
            caps_ratio: if sp.enabled { sp.caps_ratio } else { 0.0 },
            emote_ratio: if sp.enabled { sp.emote_ratio } else { 0.0 },
            dup_window_s: if sp.enabled { 60.0 } else { 0.0 },
        });
        let chat_on = settings.enabled && settings.chat.enabled;

        // Twitch reader: (re)start when enabled and logged in.
        let want_twitch = chat_on && settings.chat.twitch.enabled;
        let have = s.twitch_token.is_some();
        if want_twitch && have && s.twitch_reader.is_none() {
            let stop = Arc::new(AtomicBool::new(false));
            let token = s.twitch_token.clone().unwrap();
            let app = self.clone();
            let st2 = stop.clone();
            std::thread::Builder::new()
                .name("twitch".into())
                .spawn(move || {
                    twitch::run(
                        token,
                        st2,
                        &|state| app.st.lock().unwrap().twitch_state = state,
                        &|m, emote| app.on_chat(m, emote),
                    )
                })
                .ok();
            s.twitch_reader = Some(Reader {
                stop,
                key: String::new(),
            });
        } else if !want_twitch {
            stop_reader(&mut s.twitch_reader);
            s.twitch_state = if have {
                "off".into()
            } else {
                "not logged in".into()
            };
        } else if want_twitch && !have {
            s.twitch_state = "not logged in".into();
        }

        // YouTube reader: restart when the channel changes.
        let want_yt = chat_on && settings.chat.youtube.enabled;
        let channel = settings.chat.youtube.channel.trim().to_string();
        let changed = s.youtube_reader.as_ref().is_some_and(|r| r.key != channel);
        if !want_yt || changed || channel.is_empty() {
            stop_reader(&mut s.youtube_reader);
            s.youtube_state = if want_yt && channel.is_empty() {
                "enter your channel handle".into()
            } else {
                "off".into()
            };
        }
        if want_yt && !channel.is_empty() && s.youtube_reader.is_none() {
            let stop = Arc::new(AtomicBool::new(false));
            let app = self.clone();
            let st2 = stop.clone();
            let ch = channel.clone();
            std::thread::Builder::new()
                .name("youtube".into())
                .spawn(move || {
                    youtube::run(
                        ch,
                        st2,
                        &|state| app.st.lock().unwrap().youtube_state = state,
                        &|m, emote| app.on_chat(m, emote),
                    )
                })
                .ok();
            s.youtube_reader = Some(Reader { stop, key: channel });
        }

        let classifier_on = settings.enabled && hs.enabled && hs.classifier;
        let llm_on = chat_on && settings.chat.faq.llm;
        s.settings = settings;
        drop(s);

        self.load_faq();
        self.ensure_model_loaded("yamnet", classifier_on);
        self.ensure_model_loaded("llm", llm_on);
    }

    fn load_faq(&self) {
        let p = faq_path();
        if !p.exists() {
            std::fs::create_dir_all(ai_dir()).ok();
            std::fs::write(&p, SAMPLE_FAQ).ok();
        }
        let mut s = self.st.lock().unwrap();
        match std::fs::read_to_string(&p)
            .map_err(|e| e.to_string())
            .and_then(|t| Faq::from_yaml(&t))
        {
            Ok(f) => {
                s.faq = f;
                s.faq_error.clear();
            }
            Err(e) => s.faq_error = e,
        }
    }

    fn on_chat(self: &Arc<Self>, mut m: ChatMessage, emote_share: f64) {
        let t = self.now();
        let (privileged, faq_on, auto, can_post) = {
            let mut s = self.st.lock().unwrap();
            m.spam = s.spam.check(
                t,
                &Incoming {
                    user: &m.user,
                    text: &m.text,
                    emote_share,
                    privileged: m.privileged,
                },
            );
            if m.spam.is_none() {
                if let Some(h) = s.detector.on_chat(t) {
                    drop(s);
                    self.highlight(h);
                    s = self.st.lock().unwrap();
                }
            }
            (
                m.privileged,
                s.settings.chat.faq.enabled,
                s.settings.chat.faq.auto_reply_on_ask,
                m.platform == "twitch" && s.twitch_token.is_some(),
            )
        };
        if let Some(reason) = &m.spam {
            self.out.modlog(format!(
                "[{}] {}: {} — {}",
                m.platform, m.user, reason, m.text
            ));
        }
        self.out.send(&FromHelper::Chat(m.clone()));
        if m.spam.is_some() || !faq_on {
            return;
        }
        let _ = privileged;
        let text = m.text.trim();
        let asked = text
            .strip_prefix("!ask")
            .map(|q| q.trim())
            .filter(|q| !q.is_empty());
        let question = asked.or_else(|| text.contains('?').then_some(text));
        let Some(question) = question else { return };
        let answer = self.st.lock().unwrap().faq.answer(question);
        let Some(a) = answer else { return };
        let answer = self.rephrase(question, &a.answer);
        let reply = format!("@{} {}", m.user, answer);
        let post_now = asked.is_some() && auto && can_post;
        let id = {
            let mut s = self.st.lock().unwrap();
            s.next_id += 1;
            s.next_id
        };
        if post_now {
            let token = self.st.lock().unwrap().twitch_token.clone().unwrap();
            let t = token.lock().unwrap().clone();
            match twitch::send_chat(&t, &reply) {
                Ok(()) => self
                    .out
                    .modlog(format!("auto-replied to {}: {reply}", m.user)),
                Err(e) => {
                    self.out.modlog(format!("reply failed: {e}"));
                    self.out.toast(format!("Twitch reply failed: {e}"));
                    return;
                }
            }
        } else {
            self.st.lock().unwrap().pending.insert(
                id,
                PendingFaq {
                    platform: m.platform.clone(),
                    answer: reply.clone(),
                },
            );
        }
        self.out.send(&FromHelper::Faq {
            id,
            platform: m.platform,
            user: m.user,
            question: question.to_string(),
            answer: reply,
            posted: post_now,
        });
    }

    fn rephrase(&self, question: &str, answer: &str) -> String {
        #[cfg(feature = "llm")]
        {
            let llm = self.st.lock().unwrap().llm.clone();
            if let Some(l) = llm {
                return l.rephrase(question, answer);
            }
        }
        let _ = question;
        answer.to_string()
    }

    fn approve(&self, id: u64) {
        let (p, token) = {
            let mut s = self.st.lock().unwrap();
            let Some(p) = s.pending.remove(&id) else {
                return;
            };
            (p, s.twitch_token.clone())
        };
        if p.platform != "twitch" {
            // YouTube cannot be posted to without a Google OAuth client.
            return;
        }
        let Some(token) = token else {
            self.out.toast("Log in to Twitch first");
            return;
        };
        let t = token.lock().unwrap().clone();
        match twitch::send_chat(&t, &p.answer) {
            Ok(()) => self.out.modlog(format!("posted: {}", p.answer)),
            Err(e) => self.out.toast(format!("Twitch reply failed: {e}")),
        }
    }

    fn twitch_login(self: &Arc<Self>) {
        let client_id = self
            .st
            .lock()
            .unwrap()
            .settings
            .chat
            .twitch
            .client_id
            .trim()
            .to_string();
        if client_id.is_empty() {
            self.out.toast("Paste your Twitch app's Client ID first");
            return;
        }
        if let Some(c) = self.st.lock().unwrap().login_cancel.take() {
            c.store(true, Ordering::Relaxed);
        }
        let cancel = Arc::new(AtomicBool::new(false));
        self.st.lock().unwrap().login_cancel = Some(cancel.clone());
        let app = self.clone();
        std::thread::Builder::new()
            .name("twitch-login".into())
            .spawn(move || {
                app.st.lock().unwrap().twitch_state = "waiting for login".into();
                let dc = match twitch::start_device_login(&client_id) {
                    Ok(dc) => dc,
                    Err(e) => {
                        app.out.toast(format!("Twitch login failed: {e}"));
                        app.st.lock().unwrap().twitch_state = format!("error: {e}");
                        return;
                    }
                };
                app.out.send(&FromHelper::DeviceCode {
                    url: dc.verification_uri.clone(),
                    code: dc.user_code.clone(),
                });
                match twitch::wait_for_device_login(&client_id, &dc, &cancel) {
                    Ok(t) => {
                        if let Err(e) = twitch::save_token(&t) {
                            app.out.toast(format!("Could not save the login: {e}"));
                        }
                        let login = t.login.clone();
                        let settings = {
                            let mut s = app.st.lock().unwrap();
                            s.twitch_token = Some(Arc::new(Mutex::new(t)));
                            s.twitch_state = "logged in".into();
                            s.settings.clone()
                        };
                        app.out.toast(format!("Twitch: logged in as {login}"));
                        app.apply_settings(settings);
                    }
                    Err(e) => {
                        app.out.toast(format!("Twitch login: {e}"));
                        app.st.lock().unwrap().twitch_state = "not logged in".into();
                    }
                }
            })
            .ok();
    }

    fn set_model(&self, id: &'static str, state: &str, progress: f64, detail: &str) {
        let mut s = self.st.lock().unwrap();
        let m = s.models.entry(id).or_default();
        m.state = state.into();
        m.progress = progress;
        m.detail = detail.into();
    }

    fn model_supported(id: &str) -> Result<(), &'static str> {
        match id {
            "yamnet" => {
                if cfg!(feature = "yamnet") {
                    Ok(())
                } else {
                    Err("this build of the helper was compiled without the sound classifier (feature `yamnet`)")
                }
            }
            "llm" => {
                if cfg!(feature = "llm") {
                    Ok(())
                } else {
                    Err("this build of the helper was compiled without local LLM support (feature `llm`)")
                }
            }
            _ => Err("unknown model"),
        }
    }

    fn download(self: &Arc<Self>, model: &str) {
        let Some(spec) = models::by_id(model) else {
            return;
        };
        if let Err(why) = Self::model_supported(spec.id) {
            self.set_model(spec.id, "unsupported", 0.0, why);
            self.out.toast(why);
            return;
        }
        if spec.is_ready() {
            self.set_model(spec.id, "ready", 1.0, "");
            self.ensure_model_loaded(spec.id, true);
            return;
        }
        let already = self
            .st
            .lock()
            .unwrap()
            .models
            .get(spec.id)
            .is_some_and(|m| m.state == "downloading");
        if already {
            return;
        }
        self.set_model(spec.id, "downloading", 0.0, "");
        let app = self.clone();
        std::thread::Builder::new()
            .name(format!("download-{}", spec.id))
            .spawn(move || {
                let mut last = Instant::now();
                let r = spec.download(&mut |p| {
                    if last.elapsed() > Duration::from_millis(300) {
                        last = Instant::now();
                        app.set_model(spec.id, "downloading", p, "");
                    }
                });
                match r {
                    Ok(_) => {
                        app.set_model(spec.id, "ready", 1.0, "");
                        app.out
                            .toast(format!("{} downloaded and verified", spec.name));
                        app.ensure_model_loaded(spec.id, true);
                    }
                    Err(e) => {
                        app.set_model(spec.id, "error", 0.0, &e);
                        app.out.toast(format!("{}: {e}", spec.name));
                    }
                }
            })
            .ok();
    }

    fn unload_model(&self, id: &str) {
        #[allow(unused_mut)]
        let mut s = self.st.lock().unwrap();
        #[cfg(feature = "yamnet")]
        if id == "yamnet" {
            s.classifier = None;
        }
        #[cfg(feature = "llm")]
        if id == "llm" {
            s.llm = None;
        }
        let _ = (&s, id);
    }

    /// Loads a downloaded model into memory when its feature is on, and
    /// frees it when the feature is off (so RAM goes back to the game).
    fn ensure_model_loaded(self: &Arc<Self>, id: &'static str, wanted: bool) {
        let Some(spec) = models::by_id(id) else {
            return;
        };
        if !wanted {
            self.unload_model(id);
            if spec.is_ready() {
                self.set_model(id, "ready", 1.0, "");
            }
            return;
        }
        if Self::model_supported(id).is_err() {
            self.set_model(
                id,
                "unsupported",
                0.0,
                Self::model_supported(id).err().unwrap_or(""),
            );
            return;
        }
        if !spec.is_ready() {
            let st = self
                .st
                .lock()
                .unwrap()
                .models
                .get(id)
                .map(|m| m.state.clone());
            if st.as_deref() != Some("downloading") {
                self.set_model(id, "missing", 0.0, "");
            }
            return;
        }
        self.load_into_memory(id, spec);
    }

    #[allow(unused_variables)]
    fn load_into_memory(self: &Arc<Self>, id: &'static str, spec: models::ModelSpec) {
        #[cfg(feature = "yamnet")]
        if id == "yamnet" {
            if self.st.lock().unwrap().classifier.is_some() {
                return;
            }
            match phonecast_ai_helper::yamnet::Classifier::load(&spec.path()) {
                Ok(c) => {
                    self.st.lock().unwrap().classifier = Some(Arc::new(Mutex::new(c)));
                    self.set_model(id, "ready", 1.0, "listening");
                }
                Err(e) => self.set_model(id, "error", 0.0, &e),
            }
        }
        #[cfg(feature = "llm")]
        if id == "llm" {
            if self.st.lock().unwrap().llm.is_some() {
                return;
            }
            let app = self.clone();
            self.set_model(id, "ready", 1.0, "loading…");
            std::thread::Builder::new()
                .name("llm-load".into())
                .spawn(
                    move || match phonecast_ai_helper::llm::Rephraser::load(&spec.path()) {
                        Ok(r) => {
                            app.st.lock().unwrap().llm = Some(Arc::new(r));
                            app.set_model(id, "ready", 1.0, "loaded");
                        }
                        Err(e) => app.set_model(id, "error", 0.0, &e),
                    },
                )
                .ok();
        }
    }

    fn status(&self, usage: &mut Usage) -> Status {
        let (cpu, ram) = usage.sample();
        let s = self.st.lock().unwrap();
        let models = models::ALL
            .iter()
            .map(|m| {
                let st = s.models.get(m.id);
                let state = st
                    .map(|x| x.state.clone())
                    .unwrap_or_else(|| if m.is_ready() { "ready" } else { "missing" }.into());
                ModelStatus {
                    id: m.id.into(),
                    name: m.name.into(),
                    size_mb: m.size_mb(),
                    state,
                    progress: st.map(|x| x.progress).unwrap_or(0.0),
                    detail: st.map(|x| x.detail.clone()).unwrap_or_default(),
                }
            })
            .collect();
        Status {
            cpu_percent: cpu,
            ram_mb: ram,
            score: s.detector.last_score(),
            clips: s.detector.clips_saved(),
            twitch: s.twitch_state.clone(),
            youtube: s.youtube_state.clone(),
            twitch_user: s
                .twitch_token
                .as_ref()
                .map(|t| t.lock().unwrap().login.clone()),
            models,
            faq_entries: s.faq.len(),
            faq_path: faq_path().display().to_string(),
        }
    }
}

fn stop_reader(r: &mut Option<Reader>) {
    if let Some(r) = r.take() {
        r.stop.store(true, Ordering::Relaxed);
    }
}

/// Runs the classifier on 1 s windows every 0.5 s when it is loaded.
#[cfg(feature = "yamnet")]
fn classifier_thread(app: Arc<App>, rx: mpsc::Receiver<Vec<f32>>) {
    let mut buf: Vec<f32> = Vec::new();
    let mut since_run = 0usize;
    while let Ok(chunk) = rx.recv() {
        since_run += chunk.len();
        buf.extend(chunk);
        if buf.len() > 16_000 {
            let cut = buf.len() - 16_000;
            buf.drain(..cut);
        }
        if buf.len() < 15_600 || since_run < 8_000 {
            continue;
        }
        since_run = 0;
        let c = app.st.lock().unwrap().classifier.clone();
        let Some(c) = c else {
            buf.clear();
            continue;
        };
        let r = c.lock().unwrap().classify(&buf);
        match r {
            Ok(Some((label, conf))) => {
                app.st
                    .lock()
                    .unwrap()
                    .detector
                    .on_label(app.now(), &label, conf)
            }
            Ok(None) => {}
            Err(e) => app.set_model("yamnet", "error", 0.0, &e),
        }
    }
}

fn main() {
    let args: Vec<String> = std::env::args().collect();
    let arg = |k: &str| {
        args.iter()
            .position(|a| a == k)
            .and_then(|i| args.get(i + 1))
            .cloned()
    };
    let (Some(port), Some(token)) = (arg("--port"), arg("--token")) else {
        eprintln!("usage: phonecast-ai-helper --port <port> --token <token>");
        std::process::exit(2);
    };
    let stream = match TcpStream::connect(("127.0.0.1", port.parse().unwrap_or(0))) {
        Ok(s) => s,
        Err(e) => {
            eprintln!("cannot reach PhoneCast: {e}");
            std::process::exit(1);
        }
    };
    stream.set_nodelay(true).ok();
    let reader = BufReader::new(stream.try_clone().expect("clone socket"));
    let out = Arc::new(Out {
        w: Mutex::new(BufWriter::new(stream)),
    });
    out.send(&FromHelper::Hello {
        token,
        version: env!("CARGO_PKG_VERSION").into(),
    });

    let app = Arc::new(App {
        out: out.clone(),
        st: Arc::new(Mutex::new(State {
            settings: Settings::default(),
            detector: Detector::new(ScoreConfig::default()),
            spam: SpamFilter::new(SpamConfig::default()),
            faq: Faq::empty(),
            faq_error: String::new(),
            twitch_token: twitch::load_token().map(|t| Arc::new(Mutex::new(t))),
            twitch_state: "off".into(),
            youtube_state: "off".into(),
            twitch_reader: None,
            youtube_reader: None,
            login_cancel: None,
            models: HashMap::new(),
            pending: HashMap::new(),
            next_id: 0,
            #[cfg(feature = "yamnet")]
            classifier: None,
            #[cfg(feature = "llm")]
            llm: None,
        })),
        start: Instant::now(),
        last_from_app: Arc::new(Mutex::new(Instant::now())),
        pcm_tx: Mutex::new(None),
        clock: Mutex::new(None),
    });

    #[cfg(feature = "yamnet")]
    {
        let (tx, rx) = mpsc::channel();
        *app.pcm_tx.lock().unwrap() = Some(tx);
        let a = app.clone();
        std::thread::Builder::new()
            .name("classifier".into())
            .spawn(move || classifier_thread(a, rx))
            .ok();
    }

    // Status every 2 s; exit if the app went silent (it sends audio 10×/s).
    {
        let a = app.clone();
        std::thread::Builder::new()
            .name("status".into())
            .spawn(move || {
                let mut usage = Usage::new();
                loop {
                    std::thread::sleep(Duration::from_secs(2));
                    if a.last_from_app.lock().unwrap().elapsed() > Duration::from_secs(30) {
                        std::process::exit(0);
                    }
                    let st = a.status(&mut usage);
                    a.out.send(&FromHelper::Status(st));
                }
            })
            .ok();
    }

    for line in reader.lines() {
        let Ok(line) = line else { break };
        *app.last_from_app.lock().unwrap() = Instant::now();
        match serde_json::from_str::<ToHelper>(&line) {
            Ok(m) => app.handle(m),
            Err(e) => out.modlog(format!("bad message from app: {e}")),
        }
    }
    // Socket closed: the app is gone.
    std::process::exit(0);
}
