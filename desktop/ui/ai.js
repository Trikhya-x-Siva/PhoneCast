// AI tools panel: local AI helper settings, live chat, spam flags, FAQ
// suggestions and highlight prompts. Everything the helper does is shown
// here; nothing is posted or saved without the settings saying so.
(() => {
  const { invoke } = window.__TAURI__.core;
  const { listen } = window.__TAURI__.event;
  const $ = (id) => document.getElementById(id);
  const esc = (s) => String(s ?? "").replace(/[&<>"]/g, (c) => ({ "&": "&amp;", "<": "&lt;", ">": "&gt;", '"': "&quot;" })[c]);
  const notify = (m) => (typeof toast === "function" ? toast(m) : console.log(m));

  const DEFAULTS = {
    enabled: false,
    highlights: { enabled: true, sensitivity: 0.5, clipSeconds: 30, askBeforeSaving: true, maxPerSession: 40, classifier: false },
    chat: {
      enabled: true,
      twitch: { enabled: false, clientId: "" },
      youtube: { enabled: false, channel: "" },
      overlay: false,
      spam: { enabled: true, maxPerUser10s: 5, flagLinks: true, capsRatio: 0.75, emoteRatio: 0.7 },
      faq: { enabled: true, autoReplyOnAsk: false, llm: false },
    },
  };
  const merge = (a, b) => {
    const out = { ...a };
    for (const [k, v] of Object.entries(b || {})) out[k] = v && typeof v === "object" && !Array.isArray(v) ? merge(a[k] || {}, v) : v;
    return out;
  };

  let settings = DEFAULTS;
  let status = null;
  let saveTimer;
  const chatList = $("aiChat");
  const pendingClips = new Map();

  // ---------------- settings form ----------------
  const F = {
    enabled: $("aiEnabled"),
    hl: $("aiHlEnabled"), sens: $("aiSens"), sensOut: $("aiSensOut"), clipLen: $("aiClipLen"), ask: $("aiAsk"),
    max: $("aiMax"), classifier: $("aiClassifier"),
    chat: $("aiChatEnabled"), tw: $("aiTwEnabled"), twId: $("aiTwClientId"), yt: $("aiYtEnabled"), ytCh: $("aiYtChannel"),
    overlay: $("aiOverlay"), spam: $("aiSpam"), spamRate: $("aiSpamRate"), spamLinks: $("aiSpamLinks"),
    faq: $("aiFaq"), faqAuto: $("aiFaqAuto"), llm: $("aiLlm"),
  };

  function fill() {
    const s = settings;
    F.enabled.checked = s.enabled;
    F.hl.checked = s.highlights.enabled;
    F.sens.value = Math.round(s.highlights.sensitivity * 100);
    F.sensOut.textContent = sensLabel(s.highlights.sensitivity);
    F.clipLen.value = String(s.highlights.clipSeconds);
    F.ask.checked = s.highlights.askBeforeSaving;
    F.max.value = s.highlights.maxPerSession;
    F.classifier.checked = s.highlights.classifier;
    F.chat.checked = s.chat.enabled;
    F.tw.checked = s.chat.twitch.enabled;
    if (document.activeElement !== F.twId) F.twId.value = s.chat.twitch.clientId;
    F.yt.checked = s.chat.youtube.enabled;
    if (document.activeElement !== F.ytCh) F.ytCh.value = s.chat.youtube.channel;
    F.overlay.checked = s.chat.overlay;
    F.spam.checked = s.chat.spam.enabled;
    F.spamRate.value = s.chat.spam.maxPerUser10s;
    F.spamLinks.checked = s.chat.spam.flagLinks;
    F.faq.checked = s.chat.faq.enabled;
    F.faqAuto.checked = s.chat.faq.autoReplyOnAsk;
    F.llm.checked = s.chat.faq.llm;
    $("aiBody").classList.toggle("off", !s.enabled);
    $("aiCard").classList.toggle("on", s.enabled);
    $("aiPanelToggle").textContent = s.enabled ? "On" : "Off";
  }

  function sensLabel(v) {
    return v < 0.25 ? "Only big moments" : v < 0.5 ? "Careful" : v < 0.75 ? "Balanced" : "Eager";
  }

  function collect() {
    return merge(settings, {
      enabled: F.enabled.checked,
      highlights: {
        enabled: F.hl.checked, sensitivity: Number(F.sens.value) / 100, clipSeconds: Number(F.clipLen.value),
        askBeforeSaving: F.ask.checked, maxPerSession: Number(F.max.value) || 0, classifier: F.classifier.checked,
      },
      chat: {
        enabled: F.chat.checked,
        twitch: { enabled: F.tw.checked, clientId: F.twId.value.trim() },
        youtube: { enabled: F.yt.checked, channel: F.ytCh.value.trim() },
        overlay: F.overlay.checked,
        spam: { enabled: F.spam.checked, maxPerUser10s: Number(F.spamRate.value) || 5, flagLinks: F.spamLinks.checked },
        faq: { enabled: F.faq.checked, autoReplyOnAsk: F.faqAuto.checked, llm: F.llm.checked },
      },
    });
  }

  async function save(immediate) {
    clearTimeout(saveTimer);
    const run = async () => {
      const next = collect();
      const wasOverlay = settings.chat.overlay;
      try {
        settings = merge(DEFAULTS, await invoke("ai_save_settings", { settings: next }));
        fill();
        if (settings.chat.overlay !== wasOverlay || (settings.chat.overlay && !settings.enabled)) {
          await invoke("ai_set_overlay", { on: settings.enabled && settings.chat.overlay }).catch((e) => notify(String(e)));
        }
      } catch (e) {
        notify(String(e));
        F.enabled.checked = settings.enabled;
        fill();
      }
    };
    if (immediate) return run();
    saveTimer = setTimeout(run, 400);
  }

  for (const el of Object.values(F)) {
    if (!el || el.tagName === "OUTPUT") continue;
    el.addEventListener("change", () => save(el.type === "checkbox"));
  }
  F.sens.addEventListener("input", () => (F.sensOut.textContent = sensLabel(F.sens.value / 100)));
  F.enabled.addEventListener("change", () => {
    if (F.enabled.checked) notify("Starting the AI helper… it runs on the CPU as a separate program.");
  });
  F.classifier.addEventListener("change", () => {
    if (F.classifier.checked) setTimeout(() => startDownload("yamnet", true), 600);
  });
  F.llm.addEventListener("change", () => {
    if (F.llm.checked) setTimeout(() => startDownload("llm", true), 600);
  });

  // ---------------- models ----------------
  function startDownload(id, onlyIfMissing) {
    const m = (status?.helper?.models || []).find((x) => x.id === id);
    if (onlyIfMissing && m && m.state !== "missing" && m.state !== "error") return;
    if (m && m.state === "unsupported") return notify(m.detail);
    if (!status?.running) return notify("Turn the AI helper on first");
    notify(`Downloading ${m ? m.name : id} (${m ? m.sizeMb : "?"} MB)…`);
    invoke("ai_send", { message: { t: "download", model: id } });
  }

  function renderModels() {
    const list = status?.helper?.models || [];
    $("aiModels").innerHTML = list
      .map((m) => {
        const state = m.state === "downloading"
          ? `<span class="warn">Downloading ${Math.round(m.progress * 100)}%</span>`
          : m.state === "ready" ? `<span class="good">Ready${m.detail ? " · " + esc(m.detail) : ""}</span>`
          : m.state === "error" ? `<span class="bad" title="${esc(m.detail)}">Problem</span>`
          : m.state === "unsupported" ? `<span class="bad" title="${esc(m.detail)}">Not in this build</span>`
          : `<span>Not downloaded</span>`;
        const btn = m.state === "ready" ? `<button class="mini" data-rm="${m.id}">Remove</button>`
          : m.state === "missing" || m.state === "error" ? `<button class="mini" data-dl="${m.id}">Download ${m.sizeMb} MB</button>` : "";
        return `<div class="ai-model"><b>${esc(m.name)}</b>${state}${btn}</div>`;
      })
      .join("");
  }
  $("aiModels").addEventListener("click", (e) => {
    const b = e.target.closest("button");
    if (!b) return;
    if (b.dataset.dl) startDownload(b.dataset.dl, false);
    if (b.dataset.rm) invoke("ai_send", { message: { t: "removeModel", model: b.dataset.rm } });
  });

  // ---------------- status ----------------
  function renderStatus() {
    const h = status?.helper;
    const running = !!status?.running;
    $("aiProc").textContent = !settings.enabled ? "Off" : !running ? "Starting…" : h ? `${h.cpuPercent}% CPU · ${h.ramMb} MB` : "Running";
    $("aiScore").style.width = `${Math.min(100, ((h?.score || 0) / 4) * 100)}%`;
    $("aiClips").textContent = h ? `${h.clips} clip${h.clips === 1 ? "" : "s"} this session` : "";
    const st = (s) => `<span class="${/connected|logged in|^live/.test(s) ? "good" : /error|not logged|enter|no live/.test(s) ? "bad" : ""}">${esc(s)}</span>`;
    $("aiTwState").innerHTML = h ? st(h.twitch) + (h.twitchUser ? ` · ${esc(h.twitchUser)}` : "") : "—";
    $("aiYtState").innerHTML = h ? st(h.youtube) : "—";
    $("aiTwLogin").textContent = h?.twitchUser ? "Log out" : "Log in to Twitch";
    $("aiFaqInfo").textContent = h ? `${h.faqEntries} answer${h.faqEntries === 1 ? "" : "s"} loaded` : "";
    renderModels();
  }

  $("aiTwLogin").onclick = () => {
    if (status?.helper?.twitchUser) invoke("ai_send", { message: { t: "twitchLogout" } });
    else {
      if (!F.twId.value.trim()) return notify("Paste the Client ID of your Twitch app first (see the hint below)");
      save(true).then(() => invoke("ai_send", { message: { t: "twitchLogin" } }));
    }
  };
  $("aiFaqOpen").onclick = () => invoke("ai_open_faq");
  $("aiFaqReload").onclick = () => invoke("ai_send", { message: { t: "reloadFaq" } });
  $("aiPanelToggle").onclick = () => {
    F.enabled.checked = !F.enabled.checked;
    F.enabled.dispatchEvent(new Event("change"));
  };
  $("aiModlogBtn").onclick = () => {
    const box = $("aiModlog");
    box.classList.toggle("hidden");
    box.textContent = (status?.modlog || []).slice(-60).join("\n") || "Nothing flagged yet.";
    box.scrollTop = box.scrollHeight;
  };

  // ---------------- chat ----------------
  function addChat(m) {
    const el = document.createElement("div");
    el.className = `ai-msg ${m.platform}${m.spam ? " spam" : ""}${m.privileged ? " mod" : ""}`;
    el.innerHTML = `<i></i><b>${esc(m.user)}</b><span>${esc(m.text)}</span>${m.spam ? `<em>${esc(m.spam)}</em>` : ""}`;
    chatList.appendChild(el);
    while (chatList.children.length > 150) chatList.firstChild.remove();
    chatList.scrollTop = chatList.scrollHeight;
  }

  function addFaq(f) {
    const el = document.createElement("div");
    el.className = "ai-faq" + (f.posted ? " posted" : "");
    const actions = f.posted ? `<small>Posted automatically</small>`
      : f.platform === "twitch"
        ? `<button class="mini accent" data-ok>Post reply</button><button class="mini" data-no>Dismiss</button>`
        : `<button class="mini accent" data-copy>Copy reply</button><button class="mini" data-no>Dismiss</button><small>YouTube replies are pasted by you</small>`;
    el.innerHTML = `<div><b>${esc(f.user)}</b> asked: ${esc(f.question)}</div><div class="ans">${esc(f.answer)}</div><div class="row">${actions}</div>`;
    el.querySelector("[data-ok]")?.addEventListener("click", () => { invoke("ai_send", { message: { t: "approve", id: f.id } }); el.remove(); });
    el.querySelector("[data-no]")?.addEventListener("click", () => { invoke("ai_send", { message: { t: "dismiss", id: f.id } }); el.remove(); });
    el.querySelector("[data-copy]")?.addEventListener("click", () => { navigator.clipboard?.writeText(f.answer); notify("Copied"); el.remove(); });
    $("aiSuggest").prepend(el);
    if (f.posted) setTimeout(() => el.remove(), 15000);
  }

  // ---------------- highlights ----------------
  // The replay buffer saves the last N seconds. The clip should start ~25 s
  // before the moment, so a late (chat-confirmed) detection asks for more.
  function clipPlan(h) {
    const len = Math.max(10, h.length || settings.highlights.clipSeconds);
    const pre = Math.min(25, len - 5);
    const post = len - pre;
    return { wait: Math.max(0, post - h.secondsAgo), seconds: len + Math.max(0, Math.round(h.secondsAgo - post)) };
  }

  async function saveClip(seconds) {
    try {
      const path = await invoke("save_clip", { seconds });
      notify(`Clip saved · ${String(path).split("\\").pop()}`);
      invoke("ai_send", { message: { t: "clipResult", ok: true, detail: String(path) } });
    } catch (e) {
      const msg = /not found|unknown command|save_clip/i.test(String(e)) ? "Replay buffer is not available in this build yet" : String(e);
      notify(msg);
      invoke("ai_send", { message: { t: "clipResult", ok: false, detail: msg } });
    }
  }

  function onHighlight(h) {
    const plan = clipPlan(h);
    const why = `${h.label ? h.label + " · " : ""}${h.reason || "spike"} (score ${Number(h.score).toFixed(1)})`;
    if (!settings.highlights.askBeforeSaving) {
      notify(`Highlight: ${why} — saving a ${plan.seconds} s clip`);
      setTimeout(() => saveClip(plan.seconds), plan.wait * 1000);
      return;
    }
    const el = document.createElement("div");
    el.className = "ai-clip";
    const until = Date.now() + 60000;
    el.innerHTML = `<div><b>Highlight?</b> ${esc(why)}</div><div class="row"><button class="mini accent" data-ok>Save ${plan.seconds} s clip</button><button class="mini" data-no>Skip</button><small></small></div>`;
    const small = el.querySelector("small");
    const tick = setInterval(() => {
      const left = Math.max(0, Math.round((until - Date.now()) / 1000));
      small.textContent = `${left} s`;
      if (!left) { clearInterval(tick); el.remove(); }
    }, 1000);
    el.querySelector("[data-ok]").onclick = () => {
      clearInterval(tick); el.remove();
      const late = Math.round((Date.now() - (until - 60000)) / 1000);
      setTimeout(() => saveClip(plan.seconds + late), Math.max(0, plan.wait - late) * 1000);
    };
    el.querySelector("[data-no]").onclick = () => { clearInterval(tick); el.remove(); };
    $("aiSuggest").prepend(el);
    pendingClips.set(el, tick);
  }

  // ---------------- events ----------------
  listen("ai-status", (e) => { status = { ...(status || {}), running: true, helper: e.payload }; renderStatus(); });
  listen("ai-state", (e) => {
    status = { ...(status || {}), running: !!e.payload.running, helper: e.payload.running ? status?.helper : null };
    if (e.payload.message) notify(e.payload.message);
    renderStatus();
  });
  listen("ai-chat", (e) => addChat(e.payload));
  listen("ai-faq", (e) => addFaq(e.payload));
  listen("ai-highlight", (e) => onHighlight(e.payload));
  listen("ai-toast", (e) => notify(e.payload.text));
  listen("ai-modlog", (e) => {
    const box = $("aiModlog");
    if (!box.classList.contains("hidden")) { box.textContent += "\n" + e.payload.line; box.scrollTop = box.scrollHeight; }
  });
  listen("ai-device-code", (e) => {
    const { url, code } = e.payload;
    const el = document.createElement("div");
    el.className = "ai-faq";
    el.innerHTML = `<div><b>Twitch login</b> — open <a href="#" data-url>${esc(url)}</a> and enter the code <code>${esc(code)}</code></div><div class="row"><button class="mini accent" data-open>Open in browser</button><button class="mini" data-no>Close</button></div>`;
    const open = () => invoke("ai_open_url", { url }).catch(() => window.open(url, "_blank"));
    el.querySelector("[data-open]").onclick = open;
    el.querySelector("[data-url]").onclick = (ev) => { ev.preventDefault(); open(); };
    el.querySelector("[data-no]").onclick = () => el.remove();
    $("aiSuggest").prepend(el);
    navigator.clipboard?.writeText(code).catch(() => {});
    notify(`Twitch code ${code} copied — enter it at ${url}`);
  });

  (async () => {
    try {
      settings = merge(DEFAULTS, await invoke("ai_get_settings"));
      status = await invoke("ai_get_status");
      for (const m of status.chat || []) addChat(m);
    } catch (e) {
      console.warn("AI panel unavailable", e);
    }
    fill();
    renderStatus();
    if (settings.enabled && settings.chat.overlay) invoke("ai_set_overlay", { on: true }).catch(() => {});
  })();
})();
