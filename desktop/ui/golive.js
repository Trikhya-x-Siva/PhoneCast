// Go live window (platform list, presets, live validation) and the
// first-run setup check. Uses helpers from app.js: $, invoke, listen,
// toast, escapeHtml.
(() => {
  const modal = $("liveModal");
  const list = $("destList");
  let settings = null; // last state from Rust (get/save_live_settings)
  let dests = []; // working copy: [{id, platform, enabled, main, preset, url, backup, bandwidthTest, hasKey}]
  const typedKeys = new Map(); // id -> key typed in this session (never stored in the page after closing)
  let framing = "blur";
  let plan = null;
  let checkTimer;

  const PLATFORM_COLOR = { youtube: "#ff3d3d", ytvertical: "#ff3d3d", twitch: "#9146ff", kick: "#53fc18", facebook: "#1877f2", custom: "#8b6cff" };
  const ICON = {
    youtube: '<svg viewBox="0 0 24 24"><path d="M22 8.5a3 3 0 0 0-2.1-2.1C18 6 12 6 12 6s-6 0-7.9.4A3 3 0 0 0 2 8.5 31 31 0 0 0 2 12a31 31 0 0 0 .1 3.5 3 3 0 0 0 2 2.1C6 18 12 18 12 18s6 0 7.9-.4a3 3 0 0 0 2.1-2.1A31 31 0 0 0 22 12a31 31 0 0 0 0-3.5z"/><path d="m10 15 5-3-5-3z"/></svg>',
    ytvertical: '<svg viewBox="0 0 24 24"><rect x="7" y="2" width="10" height="20" rx="2"/><path d="m11 9.5 3 2.5-3 2.5z"/></svg>',
    twitch: '<svg viewBox="0 0 24 24"><path d="M21 2H3v16h5v4l4-4h5l4-4z"/><path d="M11 11V7M16 11V7"/></svg>',
    kick: '<svg viewBox="0 0 24 24"><path d="M5 3h5v6h2V6h3V3h4v6h-3v3h3v3h-3v3h3v3h-4v-3h-3v-3h-2v6H5z"/></svg>',
    facebook: '<svg viewBox="0 0 24 24"><path d="M15 3h-2a4 4 0 0 0-4 4v3H7v4h2v7h4v-7h3l1-4h-4V7a1 1 0 0 1 1-1h2z"/></svg>',
    custom: '<svg viewBox="0 0 24 24"><rect x="3" y="4" width="18" height="6" rx="1.5"/><rect x="3" y="14" width="18" height="6" rx="1.5"/><path d="M7 7h.01M7 17h.01"/></svg>',
  };
  window.PLATFORM_ICON = ICON;
  window.PLATFORM_COLOR = PLATFORM_COLOR;

  const catalog = () => settings?.catalog || [];
  const info = (pf) => catalog().find((c) => c.id === pf) || { presets: [], name: pf };
  const presetOf = (d) => {
    const c = info(d.platform);
    return c.presets.find((p) => p.id === d.preset) || c.presets.find((p) => p.id === c.defaultPreset) || c.presets[0];
  };
  const mbps = (kbps) => `${(kbps / 1000).toFixed(kbps % 1000 ? 1 : 0)} Mbps`;

  // ---------------- rendering ----------------

  function render() {
    list.innerHTML = "";
    if (!dests.length) {
      list.innerHTML = `<div class="dest-empty">No platforms yet — add one below.</div>`;
    }
    for (const d of dests) list.appendChild(card(d));
    $("addDest").innerHTML = catalog()
      .map((c) => `<button class="chip-btn" data-add="${c.id}" style="--c:${PLATFORM_COLOR[c.id]}">${ICON[c.id]}<span>${escapeHtml(c.name)}</span></button>`)
      .join("");
    renderPlan();
  }

  function card(d) {
    const c = info(d.platform);
    const el = document.createElement("div");
    el.className = "dest" + (d.enabled ? " on" : "");
    el.dataset.id = d.id;
    el.style.setProperty("--c", PLATFORM_COLOR[d.platform] || "var(--accent)");
    const p = presetOf(d);
    const urlField = c.needsUrl || d.platform === "facebook"
      ? `<label class="field"><span>Server URL</span><input type="text" class="url text-in" spellcheck="false" placeholder="${d.platform === "kick" ? "rtmps://….global-contribute.live-video.net:443/app" : d.platform === "custom" ? "rtmp://, rtmps:// or srt://" : escapeHtml(c.defaultUrl)}" value="${escapeHtml(d.url)}"></label>`
      : "";
    const extra = d.platform === "youtube"
      ? `<label class="switch"><input type="checkbox" class="backup" ${d.backup ? "checked" : ""}><span class="track"><span class="knob"></span></span>Send to YouTube's backup server</label>`
      : d.platform === "twitch"
        ? `<label class="switch" title="Twitch receives the stream but nobody sees it: use it to test your connection"><input type="checkbox" class="bwtest" ${d.bandwidthTest ? "checked" : ""}><span class="track"><span class="knob"></span></span>Bandwidth test (viewers don't see it)</label>`
        : "";
    const keyLabel = d.platform === "custom" ? "Stream key (if the server needs one)" : "Stream key";
    const saved = d.hasKey && !typedKeys.has(d.id);
    el.innerHTML = `
      <div class="dest-head">
        <div class="dest-icon">${ICON[d.platform] || ""}</div>
        <div class="dest-name"><b>${escapeHtml(c.name)}</b><small class="sum">${p ? escapeHtml(p.label) : ""}</small></div>
        <button class="star${d.main ? " on" : ""}" aria-pressed="${d.main}" title="Main platform: keeps full quality if your upload is short; the others are lowered first">
          <svg viewBox="0 0 24 24"><path d="m12 3 2.8 5.7 6.2.9-4.5 4.4 1 6.2-5.5-2.9-5.5 2.9 1-6.2L3 9.6l6.2-.9z"/></svg><span>Main</span></button>
        <label class="switch big-switch" title="Stream to ${escapeHtml(c.name)}"><input type="checkbox" class="enabled" ${d.enabled ? "checked" : ""}><span class="track"><span class="knob"></span></span></label>
        <button class="icon-btn remove" title="Remove ${escapeHtml(c.name)}"><svg viewBox="0 0 24 24"><path d="M18 6 6 18M6 6l12 12"/></svg></button>
      </div>
      <div class="dest-body">
        ${urlField}
        <label class="field"><span>${keyLabel}</span>
          <div class="key-row"><input type="password" class="key${saved ? " saved" : ""}" autocomplete="off" spellcheck="false"
            placeholder="${saved ? "Saved ✓ — paste a new key to replace it" : `Paste your ${escapeHtml(c.name)} stream key`}" value="${escapeHtml(typedKeys.get(d.id) || "")}">
            <button class="icon-btn reveal" title="Show key"><svg viewBox="0 0 24 24"><path d="M2 12s3.5-7 10-7 10 7 10 7-3.5 7-10 7S2 12 2 12z"/><circle cx="12" cy="12" r="3"/></svg></button></div>
        </label>
        <p class="where">${escapeHtml(c.keyHint || "")}</p>
        <label class="field"><span>Quality</span><select class="quality">${c.presets
          .map((q) => `<option value="${q.id}" ${q.id === p?.id ? "selected" : ""}>${escapeHtml(q.label)}</option>`)
          .join("")}</select></label>
        <p class="preset-spec">${p ? `${p.width}×${p.height} · ${p.fps} fps · ${p.codec === "hevc" ? "HEVC" : "H.264"} CBR · keyframe every ${p.gopSeconds} s · ${escapeHtml(p.audio)}` : ""}</p>
        ${extra}
        <details class="limits"><summary>${escapeHtml(c.name)}'s official limits</summary><p>${escapeHtml(c.limits || "")}</p></details>
        <ul class="dest-issues"></ul>
      </div>`;
    const set = (k, v) => {
      d[k] = v;
      scheduleCheck();
    };
    el.querySelector(".enabled").addEventListener("change", (e) => {
      set("enabled", e.target.checked);
      el.classList.toggle("on", d.enabled);
      const key = el.querySelector(".key");
      if (d.enabled && !d.hasKey && !key.value) setTimeout(() => key.focus(), 220);
    });
    el.querySelector(".star").addEventListener("click", () => {
      const was = d.main;
      for (const x of dests) x.main = false;
      d.main = !was;
      for (const s of list.querySelectorAll(".star")) {
        const on = s.closest(".dest").dataset.id === d.id && d.main;
        s.classList.toggle("on", on);
        s.setAttribute("aria-pressed", on);
      }
      scheduleCheck();
    });
    el.querySelector(".remove").addEventListener("click", () => {
      dests = dests.filter((x) => x !== d);
      typedKeys.delete(d.id);
      el.classList.add("leaving");
      setTimeout(render, 160);
      scheduleCheck();
    });
    el.querySelector(".quality").addEventListener("change", (e) => {
      set("preset", e.target.value);
      const q = presetOf(d);
      el.querySelector(".sum").textContent = q?.label || "";
      el.querySelector(".preset-spec").textContent = q
        ? `${q.width}×${q.height} · ${q.fps} fps · ${q.codec === "hevc" ? "HEVC" : "H.264"} CBR · keyframe every ${q.gopSeconds} s · ${q.audio}`
        : "";
    });
    el.querySelector(".url")?.addEventListener("input", (e) => set("url", e.target.value.trim()));
    el.querySelector(".backup")?.addEventListener("change", (e) => set("backup", e.target.checked));
    el.querySelector(".bwtest")?.addEventListener("change", (e) => set("bandwidthTest", e.target.checked));
    const key = el.querySelector(".key");
    key.addEventListener("input", () => {
      const v = key.value.trim();
      if (v) typedKeys.set(d.id, v);
      else typedKeys.delete(d.id);
      scheduleCheck();
    });
    el.querySelector(".reveal").addEventListener("click", (e) => {
      e.preventDefault();
      key.type = key.type === "password" ? "text" : "password";
    });
    return el;
  }

  const LEVEL_TEXT = { error: "Fix", warning: "Heads-up", note: "Note" };

  function renderPlan() {
    const box = $("liveIssues");
    const start = $("liveStart");
    if (!plan) {
      box.innerHTML = "";
      return;
    }
    const global = plan.issues.filter((i) => !i.dest);
    box.innerHTML = global
      .map((i) => `<li class="issue ${i.level}"><b>${LEVEL_TEXT[i.level]}</b><span>${escapeHtml(i.text)}</span></li>`)
      .join("");
    for (const el of list.querySelectorAll(".dest")) {
      const id = el.dataset.id;
      const mine = plan.issues.filter((i) => i.dest === id);
      const chosen = plan.chosen.find((c) => c.dest === id);
      const lowered = chosen?.lowered
        ? [`<li class="issue warning"><b>Lowered</b><span>Streams at ${escapeHtml(chosen.label)} so everything fits your upload.</span></li>`]
        : [];
      el.querySelector(".dest-issues").innerHTML = lowered
        .concat(mine.map((i) => `<li class="issue ${i.level}"><b>${LEVEL_TEXT[i.level]}</b><span>${escapeHtml(i.text)}</span></li>`))
        .join("");
      el.classList.toggle("has-error", mine.some((i) => i.level === "error"));
    }
    const need = plan.totalKbps ? mbps(plan.totalKbps) : "—";
    const up = settings?.uploadMbps;
    $("uploadNeed").textContent = need;
    $("uploadHave").textContent = up ? `of ${up.toFixed(1)} Mbps measured (keeps 30% spare)` : "upload not measured yet";
    $("uploadBox").classList.toggle("over", !!(plan.budgetKbps && plan.totalKbps > plan.budgetKbps));
    const enabled = dests.some((d) => d.enabled);
    start.disabled = !plan.canStart || !enabled || !settings?.ffmpeg;
    start.title = !settings?.ffmpeg
      ? "Install FFmpeg first (Setup check)"
      : plan.canStart ? "" : (plan.issues.find((i) => i.level === "error")?.text || "");
  }

  // ---------------- data ----------------

  function collect(withKeys) {
    return {
      framing,
      destinations: dests.map((d) => {
        const o = { id: d.id, platform: d.platform, enabled: d.enabled, main: d.main, preset: d.preset, url: d.url, backup: d.backup, bandwidthTest: d.bandwidthTest };
        if (withKeys && typedKeys.has(d.id)) o.key = typedKeys.get(d.id);
        return o;
      }),
    };
  }

  function scheduleCheck() {
    clearTimeout(checkTimer);
    checkTimer = setTimeout(async () => {
      try {
        plan = await invoke("check_live_settings", { settings: collect(true) });
        renderPlan();
      } catch (e) {
        console.warn(e);
      }
    }, 200);
  }

  function fill(s) {
    settings = s;
    dests = s.destinations.map((d) => ({ ...d }));
    framing = s.framing || "blur";
    plan = s.plan;
    $("ffmpegMissing").classList.toggle("hidden", s.ffmpeg);
    $("encName").textContent = s.encoder || "your GPU";
    for (const x of $("framing").querySelectorAll("button")) x.classList.toggle("on", x.dataset.v === framing);
    $("bannerEditor").classList.toggle("hidden", framing !== "banner");
    drawBanner();
    startPreview();
    render();
    scheduleCheck();
  }

  async function save() {
    const s = await invoke("save_live_settings", { settings: collect(true) });
    typedKeys.clear();
    fill(s);
    return s;
  }

  // ---------------- events ----------------

  $("addDest").addEventListener("click", (e) => {
    const b = e.target.closest("[data-add]");
    if (!b) return;
    const pf = b.dataset.add;
    const c = info(pf);
    const d = {
      id: `${pf}-${Date.now().toString(36)}`,
      platform: pf,
      enabled: true,
      main: !dests.some((x) => x.main),
      preset: c.defaultPreset,
      url: "",
      backup: false,
      bandwidthTest: false,
      hasKey: false,
    };
    dests.push(d);
    render();
    const el = list.querySelector(`[data-id="${d.id}"]`);
    el?.scrollIntoView({ block: "nearest", behavior: matchMedia("(prefers-reduced-motion: reduce)").matches ? "auto" : "smooth" });
    setTimeout(() => el?.querySelector(c.needsUrl ? ".url" : ".key")?.focus(), 220);
    scheduleCheck();
  });

  for (const b of $("framing").querySelectorAll("button")) {
    b.addEventListener("click", () => {
      framing = b.dataset.v;
      for (const x of $("framing").querySelectorAll("button")) x.classList.toggle("on", x === b);
      showBannerEditor();
    });
  }

  // ---------------- banner (strip under the game) ----------------
  // Drawn here on a 1920-wide canvas in the strip's exact shape, saved as a
  // PNG that the stream lays under the game. Re-saved shortly after each edit.
  const bn = { style: "blitz", title: "", line: "", right: "BGMI LIVE", color: "#8b5cf6", color2: "#22d3ee", logo: null, image: null };
  let bnSaveTimer;
  const canvas = $("bannerCanvas");

  function stripHeight() {
    // The phone picture's shape decides how tall the strip is at 1920x1080.
    const v = window.lastStatus && window.lastStatus.video;
    const long = v ? Math.max(v.width, v.height) : 2376;
    const short = v ? Math.min(v.width, v.height) : 1080;
    const gh = Math.round((1920 * short) / long / 2) * 2;
    return Math.max(80, 1080 - gh);
  }

  function loadImage(src) {
    return new Promise((ok, bad) => {
      const i = new Image();
      i.onload = () => ok(i);
      i.onerror = bad;
      i.src = src;
    });
  }


  // Draws one moment of the banner (t = 0..1 through the loop).
  function drawBanner(t = 0) {
    const H = stripHeight();
    if (canvas.height !== H) canvas.height = H;
    BannerArt.draw(canvas.getContext("2d"), canvas.width, H, bn, t);
  }

  // Live moving preview while the editor is open.
  let previewRaf = 0;
  function preview(now) {
    if ($("bannerEditor").classList.contains("hidden") || modal.classList.contains("hidden")) {
      previewRaf = 0;
      return;
    }
    const H = stripHeight();
    if (canvas.height !== H) canvas.height = H;
    BannerArt.bannerAt(canvas.getContext("2d"), canvas.width, H, bn, (now / 1000) % BannerArt.BANNER_LOOP_SECONDS);
    previewRaf = requestAnimationFrame(preview);
  }
  function startPreview() {
    if (!previewRaf) previewRaf = requestAnimationFrame(preview);
  }

  function toPng(c) {
    return new Promise((ok) => c.toBlob(async (b) => ok(new Uint8Array(await b.arrayBuffer())), "image/png"));
  }
  function toJpeg(c) {
    return new Promise((ok) => c.toBlob(async (b) => ok(new Uint8Array(await b.arrayBuffer())), "image/jpeg", 0.93));
  }

  // The banner design, for scenes drawn in the same style.
  window.getBannerSpec = () => bn;
  // For the main screen's "what viewers see" strip.
  Object.defineProperty(window, "currentFraming", { get: () => framing, configurable: true });
  window.bannerDesignKey = () => {
    const v = bn.assetVer || {};
    const sig = (k) => (bn[k] ? "v" + (v[k] || 0) : "");
    const str = JSON.stringify([bn.style, bn.title, bn.line, bn.right, bn.color, bn.color2, sig("logo"), sig("image"), sig("sceneBg"), sig("stripBg"), sig("poseWave"), sig("poseShush"), bn.social, bn.discord, bn.custom, bn.popups]);
    let h = 2166136261;
    for (let i = 0; i < str.length; i++) h = Math.imul(h ^ str.charCodeAt(i), 16777619);
    return (h >>> 0).toString(36);
  };

  // Saves the banner: the first frame as banner.png (plus the editor
  // settings), and for a moving banner every frame of the loop, which the
  // stream plays under the game.
  let saving = Promise.resolve();
  function saveBannerSoon() {
    drawBanner();
    startPreview();
    clearTimeout(bnSaveTimer);
    bnSaveTimer = setTimeout(() => {
      saving = saving.then(saveBannerNow).catch((e) => toast(String(e)));
    }, 800);
  }
  const ASSETS = ["logo", "image", "sceneBg", "stripBg", "poseWave", "poseShush"];
  let savedKey = null;
  async function saveBannerNow() {
    const H = stripHeight();
    const clip = (bn._clips || []).includes("gaming");
    const key = window.bannerDesignKey() + "@" + H + (clip ? "+clip" : "");
    if (key === savedKey) return; // nothing changed: keep the made banner
    const off = document.createElement("canvas");
    off.width = canvas.width;
    off.height = H;
    const g = off.getContext("2d");
    // Pictures are files of their own; the settings only say which exist.
    const spec = { style: bn.style, title: bn.title, line: bn.line, right: bn.right, color: bn.color, color2: bn.color2, assetVer: bn.assetVer || {}, savedKey: key, social: bn.social || "", discord: bn.discord || "", custom: bn.custom || "", popups: bn.popups };
    for (const k of ASSETS) spec[k] = bn[k] ? "asset" : null;
    BannerArt.draw(g, off.width, H, bn, 0);
    const first = await toPng(off);
    await invoke("save_banner", first, { headers: { "x-spec": encodeURIComponent(JSON.stringify(spec)) } });
    // Moving banner: a 20 s loop (Subscribe pop-up in its last 4 s), drawn a
    // few frames at a time so the window stays responsive.
    const n = BannerArt.animated(bn) || clip ? BannerArt.BANNER_LOOP_SECONDS * BannerArt.FPS : 0;
    const frames = [];
    for (let i = 0; i < n; i++) {
      BannerArt.bannerAt(g, off.width, H, bn, i / BannerArt.FPS);
      frames.push(await toJpeg(off));
      if (i % 6 === 5) await new Promise((r) => setTimeout(r, 0));
    }
    const total = frames.reduce((a, f) => a + f.length, 0);
    const body = new Uint8Array(total);
    let at = 0;
    for (const f of frames) {
      body.set(f, at);
      at += f.length;
    }
    await invoke("save_banner_frames", body, {
      headers: { "x-sizes": frames.map((f) => f.length).join(","), "x-fps": String(BannerArt.FPS), "x-w": String(off.width), "x-h": String(H) },
    });
    // The plain strip for "banner off" (same size, so the stream can switch).
    const pg = off.getContext("2d");
    pg.clearRect(0, 0, off.width, H);
    BannerArt.plain(pg, off.width, H, bn);
    await invoke("save_banner_plain", await toPng(off));
    // Pop-ups: each as four 5 s pieces of the loop (at 0, 5, 10, 15 s), so
    // the app can lay one in at the next of those points, seamlessly.
    const per = BannerArt.MOMENT_SECONDS * BannerArt.FPS;
    for (const name of BannerArt.momentNames(bn)) {
      $("qbState").textContent = `Making the ${name} pop-up…`;
      for (let at = 0; at < BannerArt.BANNER_LOOP_SECONDS * BannerArt.FPS; at += per) {
        const parts = [];
        for (let i = 0; i < per; i++) {
          BannerArt.momentAt(g, off.width, H, bn, name, (at + i) / BannerArt.FPS, i / per);
          parts.push(await toJpeg(off));
          if (i % 6 === 5) await new Promise((r) => setTimeout(r, 0));
        }
        const all = new Uint8Array(parts.reduce((a, f) => a + f.length, 0));
        let o = 0;
        for (const f of parts) {
          all.set(f, o);
          o += f.length;
        }
        await invoke("save_banner_moment", all, {
          headers: { "x-name": name, "x-at": String(at), "x-sizes": parts.map((f) => f.length).join(","), "x-w": String(off.width), "x-h": String(H) },
        });
      }
    }
    sendPopupPlan();
    savedKey = key;
    if ($("qbState")) $("qbState").textContent = "";
  }

  function showBannerEditor() {
    $("bannerEditor").classList.toggle("hidden", framing !== "banner");
    if (framing === "banner") saveBannerSoon();
  }

  $("bnStyle").addEventListener("change", (e) => {
    bn.style = e.target.value;
    saveBannerSoon();
  });
  for (const [id, key] of [["bnTitle", "title"], ["bnLine", "line"], ["bnRight", "right"], ["bnColor", "color"], ["bnColor2", "color2"]]) {
    $(id).addEventListener("input", (e) => {
      bn[key] = e.target.value;
      syncQuickBanner();
      saveBannerSoon();
    });
  }
  // Main screen Banner card: on/off and the texts, changeable any time
  // (also while live: the stream picks the new banner up by itself).
  function syncQuickBanner() {
    for (const [id, key] of [["qbTitle", "title"], ["qbRight", "right"]]) {
      const el = $(id);
      if (el && document.activeElement !== el) el.value = bn[key] || "";
    }
  }
  for (const [id, key, full] of [["qbTitle", "title", "bnTitle"], ["qbRight", "right", "bnRight"]]) {
    $(id).addEventListener("input", (e) => {
      bn[key] = e.target.value;
      $(full).value = bn[key];
      $("qbState").textContent = "Updating the banner…";
      saveBannerSoon();
    });
  }
  $("qbOn").addEventListener("change", (e) => {
    invoke("set_banner_on", { on: e.target.checked }).catch((err) => toast(String(err)));
  });
  $("qbEdit").addEventListener("click", () => window.openModal());
  // ---------------- banner pop-ups ----------------
  bn.popups = bn.popups || { on: true, every: 180, list: { subscribe: true, like: true, follow: false, discord: false, custom: false } };
  const POPUPS = [
    ["subscribe", "Subscribe", null],
    ["like", "Like reminder", null],
    ["follow", "Follow me", ["social", "@phantom.blitzz on Instagram"]],
    ["discord", "Join Discord", ["discord", "discord.gg/yourserver"]],
    ["custom", "Your message", ["custom", "Squad games at 9 pm!"]],
  ];
  function sendPopupPlan() {
    invoke("set_banner_moments", { auto: !!bn.popups.on, every: Number(bn.popups.every) || 180, names: BannerArt.momentNames(bn) }).catch(() => {});
  }
  function popupsChanged() {
    sendPopupPlan();
    $("qbState").textContent = "Updating the banner…";
    saveBannerSoon();
  }
  function renderPopups() {
    const box = $("qbPopups");
    if (!box) return;
    box.innerHTML = "";
    $("qbAuto").checked = !!bn.popups.on;
    $("qbEvery").value = String(bn.popups.every || 180);
    for (const [id, label, field] of POPUPS) {
      const row = document.createElement("div");
      row.className = "popup-row";
      const sw = document.createElement("label");
      sw.className = "switch";
      sw.innerHTML = `<input type="checkbox"${bn.popups.list[id] ? " checked" : ""}><span class="track"><span class="knob"></span></span>${label}`;
      sw.querySelector("input").onchange = (e) => {
        bn.popups.list[id] = e.target.checked;
        popupsChanged();
        renderPopups();
      };
      const show = document.createElement("button");
      show.className = "btn ghost small";
      show.textContent = "Show now";
      show.disabled = !bn.popups.list[id];
      show.onclick = async () => {
        try {
          await invoke("show_banner_moment", { name: id });
          toast(`${label}: on stream within 5 s`);
        } catch (e) {
          toast(String(e));
        }
      };
      row.append(sw, show);
      box.appendChild(row);
      if (field && bn.popups.list[id]) {
        const inp = document.createElement("input");
        inp.type = "text";
        inp.maxLength = 48;
        inp.placeholder = field[1];
        inp.value = bn[field[0]] || "";
        inp.className = "popup-text";
        inp.oninput = () => {
          bn[field[0]] = inp.value.trim();
          popupsChanged();
        };
        box.appendChild(inp);
      }
    }
  }
  $("qbAuto").onchange = (e) => {
    bn.popups.on = e.target.checked;
    popupsChanged();
  };
  $("qbEvery").onchange = (e) => {
    bn.popups.every = Number(e.target.value);
    sendPopupPlan();
    saveBannerSoon();
  };

  // One-tap game labels: the usual ones plus the game open on the phone.
  const LABELS = ["BGMI LIVE", "eFootball", "Free Fire", "COD Mobile", "Just chatting"];
  let chipsKey = "";
  function setLabel(v) {
    bn.right = v;
    $("qbRight").value = v;
    $("bnRight").value = v;
    $("qbState").textContent = "Updating the banner…";
    saveBannerSoon();
    renderChips(window.lastStatus || {});
  }
  function renderChips(s) {
    const app = s.privacy && s.privacy.app;
    const phoneGame = app && app.game && app.label ? app.label : null;
    const list = [...new Set([...(phoneGame ? [phoneGame] : []), ...LABELS])];
    const key = list.join("|") + "#" + bn.right;
    if (key === chipsKey) return;
    chipsKey = key;
    const box = $("qbChips");
    box.innerHTML = "";
    for (const v of list) {
      const b = document.createElement("button");
      b.className = "chip" + (v === bn.right ? " on" : "") + (v === phoneGame ? " phone" : "");
      b.textContent = v === phoneGame ? `On phone: ${v}` : v;
      b.title = v === phoneGame ? "The game open on your phone" : "";
      b.onclick = () => setLabel(v);
      box.appendChild(b);
    }
  }
  window.renderQuickBanner = (s) => {
    const m = s.bannerMoment || {};
    const live = m.playing ? `On stream now: ${m.playing}` : m.waiting ? `Coming up within 5 s: ${m.waiting}` : "";
    if ($("qbNow").textContent !== live) $("qbNow").textContent = live;
    const t = $("qbOn");
    if (document.activeElement !== t && typeof s.bannerOn === "boolean") t.checked = s.bannerOn;
    renderChips(s);
  };
  for (const [id, key] of [["bnLogo", "logo"], ["bnImage", "image"]]) {
    $(id).addEventListener("change", (e) => {
      const f = e.target.files[0];
      if (!f) return;
      (async () => {
        try {
          const bytes = new Uint8Array(await f.arrayBuffer());
          bn[key] = await loadImage(URL.createObjectURL(new Blob([bytes], { type: f.type || "image/png" })));
          await invoke("save_banner_asset", bytes, { headers: { "x-name": key } });
          bn.assetVer = { ...(bn.assetVer || {}), [key]: Date.now() };
          saveBannerSoon();
        } catch {
          toast("Could not read that image");
        }
      })();
    });
  }

  async function loadAsset(k, v) {
    if (!v) return null;
    if (typeof v === "string" && v.startsWith("data:")) {
      // Old settings kept pictures inline: move them to their own file.
      const bytes = new Uint8Array(await (await fetch(v)).arrayBuffer());
      await invoke("save_banner_asset", bytes, { headers: { "x-name": k } }).catch(() => {});
      return loadImage(v);
    }
    const buf = await invoke("get_banner_asset", { name: k });
    return loadImage(URL.createObjectURL(new Blob([buf])));
  }

  invoke("get_banner").then(async (s) => {
    if (!s) return drawBanner();
    Object.assign(bn, { style: s.style || "blitz", title: s.title || "", line: s.line || "", right: s.right ?? "BGMI LIVE", color: s.color || "#8b5cf6", color2: s.color2 || "#22d3ee", assetVer: s.assetVer || {} });
    for (const k of ASSETS) bn[k] = await loadAsset(k, s[k]).catch(() => null);
    // Already made and unchanged: no re-render at start.
    if (s._loop && s.savedKey) savedKey = s.savedKey;
    // Scenes whose character comes from the channel's own clip (laid over
    // by the app), so the drawn scene leaves room for it.
    bn._clips = s._clips || [];
    bn.social = s.social || "";
    bn.discord = s.discord || "";
    bn.custom = s.custom || "";
    if (s.popups) bn.popups = s.popups;
    window.bannerReady = true;
    renderPopups();
    sendPopupPlan();
    syncQuickBanner();
    $("bnTitle").value = bn.title;
    $("bnLine").value = bn.line;
    $("bnRight").value = bn.right;
    $("bnColor").value = bn.color;
    $("bnColor2").value = bn.color2;
    $("bnStyle").value = bn.style;
    // Re-save once on start, so the stream always has this banner even if
    // the editor was never opened (e.g. settings prepared in advance).
    saveBannerSoon();
  });

  window.openModal = () => {
    const live = !!(window.lastStatus && window.lastStatus.live && window.lastStatus.live.active);
    $("liveStart").textContent = live ? "Apply now" : "Start streaming";
    invoke("get_live_settings").then(fill);
    modal.classList.remove("hidden");
  };
  function closeModal() {
    modal.classList.add("hidden");
    typedKeys.clear(); // never keep keys in the page
    for (const k of list.querySelectorAll(".key")) k.value = "";
  }
  modal.addEventListener("click", (e) => {
    if (e.target === modal || e.target.closest("[data-close]")) closeModal();
  });
  document.addEventListener("keydown", (e) => {
    if (e.key !== "Escape") return;
    if (!modal.classList.contains("hidden")) closeModal();
    else if (!setupModal.classList.contains("hidden")) closeSetup();
  });

  $("liveSave").onclick = async () => {
    try {
      await save();
      toast("Saved");
    } catch (e) {
      toast(String(e));
    }
  };
  $("liveStart").onclick = async () => {
    const btn = $("liveStart");
    btn.disabled = true;
    const live = !!(window.lastStatus && window.lastStatus.live && window.lastStatus.live.active);
    try {
      await save();
      if (live) {
        toast("Applying the new settings: viewers see a 2-3 second pause, the stream keeps going");
        closeModal();
        await invoke("apply_live_now");
        toast("New settings are live");
      } else {
        await invoke("start_live");
        closeModal();
        toast("Going live…");
      }
    } catch (e) {
      toast(String(e));
    } finally {
      btn.disabled = false;
    }
  };
  $("liveEdit").onclick = () => window.openModal();
  $("liveSetup").onclick = () => openSetup();
  // Ending is the one live action that cannot be undone: ask first, and
  // offer the clean way out (the Thanks scene, then the end).
  const endModal = $("endModal");
  let endTimer = null;
  async function endNow() {
    clearInterval(endTimer);
    endTimer = null;
    window.setAlert("ending", null);
    await invoke("stop_live");
    // The next stream starts on the game, not on the Thanks scene.
    invoke("set_scene", { name: null, design: window.bannerDesignKey() }).catch(() => {});
    toast("Stream ended");
  }
  $("liveBtn").onclick = async () => {
    if (isLive) {
      const l = (window.lastStatus && window.lastStatus.live) || {};
      const names = (l.destinations || []).map((d) => d.name).join(" + ") || "every platform";
      $("endSub").textContent = `Ends it on ${names}${l.seconds ? ` after ${fmtTime(l.seconds)} live` : ""}.`;
      endModal.classList.remove("hidden");
    } else {
      window.openModal();
    }
  };
  endModal.addEventListener("click", (e) => {
    if (e.target === endModal || e.target.closest("[data-close]")) endModal.classList.add("hidden");
  });
  $("endNow").onclick = () => {
    endModal.classList.add("hidden");
    endNow().catch((e) => toast(String(e)));
  };
  $("endThanks").onclick = async () => {
    endModal.classList.add("hidden");
    try {
      await window.showScene("thanks");
    } catch (e) {
      toast(String(e.message || e));
    }
    let left = 15;
    const tick = () => {
      window.setAlert("ending", { level: "info", text: `Thanks for watching is on. Ending the stream in ${left} s.`, action: "Keep streaming", onAction: () => {
        clearInterval(endTimer);
        endTimer = null;
        window.setAlert("ending", null);
        toast("Still live");
      } });
      if (left-- <= 0) endNow().catch((e) => toast(String(e)));
    };
    clearInterval(endTimer);
    tick();
    endTimer = setInterval(tick, 1000);
  };

  // ---------------- setup check (first run, re-runnable) ----------------

  const setupModal = $("setupModal");
  let upload = null;

  function step(id, state, detail) {
    const el = $(id);
    el.dataset.state = state; // pending | busy | ok | warn | bad
    if (detail !== undefined) el.querySelector(".detail").innerHTML = detail;
  }

  async function runProbe() {
    step("stEnc", "busy", "Checking which encoder this PC has…");
    step("stFf", "busy", "Looking for FFmpeg…");
    const e = await invoke("setup_probe").catch(() => ({ ffmpeg: null }));
    if (!e.ffmpeg) {
      step("stEnc", "pending", "Needs FFmpeg first.");
      step("stFf", "bad", "FFmpeg (the free streaming engine) isn't installed. PhoneCast can download the official build from gyan.dev and check it.");
      $("ffInstall").classList.remove("hidden");
      return false;
    }
    $("ffInstall").classList.add("hidden");
    step("stFf", "ok", `Ready${e.managed ? " (installed by PhoneCast)" : ""}.`);
    step(
      "stEnc",
      e.gpu ? "ok" : "warn",
      e.gpu
        ? `${escapeHtml(e.h264Label)}${e.hevcHardware ? " — H.264 and HEVC, so YouTube gets the sharper HEVC presets." : " — H.264 only."}`
        : "No GPU encoder found: streams are encoded on the CPU (x264). Expect higher CPU use; 1080p or lower is safest."
    );
    return true;
  }

  $("ffInstall").onclick = async () => {
    $("ffInstall").disabled = true;
    const bar = $("stFf").querySelector(".bar");
    bar.classList.remove("hidden");
    await invoke("setup_install_ffmpeg");
    const poll = setInterval(async () => {
      const p = await invoke("setup_install_progress");
      if (!p) return;
      bar.firstElementChild.style.transform = `scaleX(${p.percent / 100})`;
      step("stFf", p.failed ? "bad" : "busy", escapeHtml(p.message));
      if (p.done || p.failed) {
        clearInterval(poll);
        $("ffInstall").disabled = false;
        bar.classList.add("hidden");
        if (p.done) {
          setTimeout(runProbe, 400);
          invoke("get_live_settings").then((s) => (settings = s));
        }
      }
    }, 400);
  };

  $("upRun").onclick = async () => {
    const btn = $("upRun");
    btn.disabled = true;
    const bar = $("stUp").querySelector(".bar");
    bar.classList.remove("hidden");
    bar.firstElementChild.style.transform = "scaleX(0)";
    step("stUp", "busy", "Measuring… keep other uploads (cloud sync, game updates) paused.");
    const off = await listen("setup-upload", (e) => {
      bar.firstElementChild.style.transform = `scaleX(${e.payload.fraction})`;
      step("stUp", "busy", `Measuring… ${e.payload.mbps.toFixed(1)} Mbps`);
    });
    try {
      upload = await invoke("setup_upload_test");
      step(
        "stUp",
        upload.minMbps >= 8 ? "ok" : "warn",
        `<b>${upload.minMbps} Mbps</b> at the slowest moment (typical ${upload.medianMbps}, best ${upload.maxMbps}). PhoneCast plans with the slowest number and keeps 30% spare.`
      );
      renderRecommend();
    } catch (e) {
      step("stUp", "bad", escapeHtml(String(e)));
    } finally {
      off();
      bar.classList.add("hidden");
      btn.disabled = false;
      btn.textContent = "Measure again";
    }
  };

  function renderRecommend() {
    if (!upload) {
      $("recList").innerHTML = "";
      step("stRec", "pending", "Measure your upload first.");
      return;
    }
    const r = upload.recommend;
    step("stRec", "ok", escapeHtml(r.note));
    $("recList").innerHTML = r.platforms
      .map((p) => `<div class="rec" style="--c:${PLATFORM_COLOR[p.platform]}"><span class="dest-icon">${ICON[p.platform]}</span><b>${escapeHtml(p.name)}</b><span>${escapeHtml(p.label)}</span></div>`)
      .join("");
  }

  async function openSetup() {
    setupModal.classList.remove("hidden");
    const s = await invoke("get_live_settings");
    settings = settings || s;
    if (s.uploadMbps && !upload) {
      step("stUp", "ok", `Last measured: <b>${s.uploadMbps.toFixed(1)} Mbps</b> at the slowest moment.`);
      $("upRun").textContent = "Measure again";
    }
    renderRecommend();
    runProbe();
  }
  function closeSetup() {
    setupModal.classList.add("hidden");
    invoke("save_live_settings", { settings: { setupDone: true } }).catch(() => {});
    if (!modal.classList.contains("hidden")) invoke("get_live_settings").then(fill);
  }
  setupModal.addEventListener("click", (e) => {
    if (e.target === setupModal || e.target.closest("[data-close]")) closeSetup();
  });
  $("setupDone").onclick = closeSetup;
  $("setupApply").onclick = async () => {
    if (!upload) return closeSetup();
    // Apply each recommendation to destinations of that platform.
    const s = await invoke("get_live_settings");
    const pick = Object.fromEntries(upload.recommend.platforms.map((p) => [p.platform, p.preset]));
    const destinations = s.destinations.map((d) => ({ ...d, preset: pick[d.platform] || d.preset }));
    await invoke("save_live_settings", { settings: { destinations, setupDone: true } });
    toast("Recommended quality applied");
    closeSetup();
  };

  // First run: open the setup check once.
  invoke("get_live_settings")
    .then((s) => {
      settings = s;
      framing = s.framing || framing;
      if (!s.setupDone) setTimeout(openSetup, 600);
    })
    .catch(() => {});
})();
