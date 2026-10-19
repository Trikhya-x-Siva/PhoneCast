// Stream profiles: pick one in Go live; it fills YouTube (copy panel),
// Twitch (automatic), Discord (announcement), the banner label, the custom
// room card and a thumbnail. Uses helpers from app.js: $, invoke, toast,
// escapeHtml. Talks to profiles.rs.
(() => {
  let data = null; // get_profiles
  let rendered = null; // last render_profile result
  let renderTimer;
  let wentLiveFor = null; // profile id announced for the current stream
  let wasLive = false;

  const lastVars = (() => {
    try {
      return JSON.parse(localStorage.getItem("profileVars") || "{}");
    } catch {
      return {};
    }
  })();
  const remember = () => {
    try {
      localStorage.setItem("profileVars", JSON.stringify(lastVars));
    } catch {}
  };

  const active = () => data?.profiles.find((p) => p.id === data.active) || data?.profiles[0];
  const el = (html) => {
    const t = document.createElement("template");
    t.innerHTML = html.trim();
    return t.content.firstElementChild;
  };

  async function load() {
    data = await invoke("get_profiles");
    renderPicker();
  }

  // ---------------- Go live: profile picker ----------------

  const picker = el(`
    <section class="pf-box" aria-label="Stream profile">
      <div class="pf-head">
        <span class="pf-label">Stream profile</span>
        <div class="pf-chips" id="pfChips"></div>
        <button class="btn ghost small" id="pfEdit" title="Edit profiles, defaults, Twitch and Discord">Edit profiles</button>
      </div>
      <div class="pf-vars" id="pfVars"></div>
      <div class="pf-preview">
        <div class="pf-title-row"><b id="pfTitle">—</b><small id="pfCount" class="mono"></small></div>
        <ul id="pfWarn" class="pf-warn"></ul>
        <div class="pf-targets" id="pfTargets"></div>
        <div class="pf-actions">
          <button class="btn ghost small" id="pfYouTube">YouTube: copy title & description</button>
          <button class="btn ghost small" id="pfThumbBtn">Thumbnail</button>
        </div>
      </div>
    </section>`);
  $("liveModal").querySelector(".sheet-sub").after(picker);

  function renderPicker() {
    const a = active();
    if (!a) return;
    $("pfChips").innerHTML = data.profiles
      .map((p) => `<button class="pf-chip${p.id === a.id ? " on" : ""}" data-id="${escapeHtml(p.id)}">${escapeHtml(p.name)}</button>`)
      .join("");
    const v = lastVars[a.id] || {};
    const field = (key, label, ph, def = "") =>
      `<label class="field"><span>${label}</span><input class="text-in" data-var="${key}" maxlength="80" placeholder="${escapeHtml(ph)}" value="${escapeHtml(v[key] ?? def)}"></label>`;
    const style = String(v.style ?? "0");
    const styles = (a.titleOptions || []).length > 1
      ? `<label class="field"><span>Title style</span><select class="text-in" data-var="style">
          ${["Normal", "Hype", "Special day"].map((n, i) => `<option value="${i}"${String(i) === style ? " selected" : ""}>${n}</option>`).join("")}
        </select></label>`
      : "";
    let html = styles + field("mode", "What you're doing", "e.g. Rank push, Classic, Scrims", a.mode);
    if (a.kind === "pc") html = field("game", "Game", "e.g. Valorant, GTA V, Minecraft", a.game) + html;
    if (a.kind === "rooms")
      html += field("room", "Room ID", "e.g. 1234567") + field("pass", "Password", "e.g. 2468") + field("map", "Map / mode", "e.g. Erangel · TPP squad");
    $("pfVars").innerHTML = html;
    scheduleRender();
  }

  function vars() {
    const out = {};
    for (const i of $("pfVars").querySelectorAll("[data-var]")) out[i.dataset.var] = i.value.trim();
    return out;
  }

  $("pfChips").addEventListener("click", async (e) => {
    const b = e.target.closest(".pf-chip");
    if (!b) return;
    data = await invoke("save_profiles", { data: { active: b.dataset.id } });
    renderPicker();
  });
  $("pfVars").addEventListener("change", () => $("pfVars").dispatchEvent(new Event("input")));
  $("pfVars").addEventListener("input", () => {
    const a = active();
    lastVars[a.id] = vars();
    remember();
    scheduleRender();
  });

  function scheduleRender() {
    clearTimeout(renderTimer);
    renderTimer = setTimeout(doRender, 150);
  }

  async function doRender() {
    const a = active();
    if (!a) return;
    try {
      rendered = await invoke("render_profile", { id: a.id, vars: vars() });
    } catch (e) {
      $("pfTitle").textContent = String(e);
      return;
    }
    const t = rendered.youtube.title;
    $("pfTitle").textContent = t;
    const n = [...t].length;
    $("pfCount").textContent = `${n}/100`;
    $("pfCount").classList.toggle("bad", n > 100);
    $("pfWarn").innerHTML = rendered.warnings.map((w) => `<li>${escapeHtml(w)}</li>`).join("");
    const tw = data.twitch.connected
      ? `<span class="pf-t ok">Twitch: title, game & tags set automatically</span>`
      : `<span class="pf-t">Twitch: <a href="#" data-open-tab="connections">connect</a> to fill it automatically</span>`;
    const dc = a.announce
      ? data.discordWebhookSet
        ? `<span class="pf-t ok">Discord: posts “I'm live”${a.announceEveryone ? " with @everyone" : ""}</span>`
        : `<span class="pf-t">Discord: <a href="#" data-open-tab="connections">add a webhook</a> to announce</span>`
      : `<span class="pf-t muted">Discord: no announcement</span>`;
    const room = rendered.banner.room && a.roomOnBanner ? `<span class="pf-t ok">Banner: shows ${escapeHtml(rendered.banner.room)}</span>` : "";
    $("pfTargets").innerHTML = `<span class="pf-t">YouTube: copy-paste (10 s)</span>${tw}${dc}${room}`;
  }

  picker.addEventListener("click", (e) => {
    const t = e.target.closest("[data-open-tab]");
    if (t) {
      e.preventDefault();
      openEditor(t.dataset.openTab);
    }
  });
  $("pfEdit").onclick = () => openEditor(active()?.id);
  $("pfYouTube").onclick = () => openCopy();
  $("pfThumbBtn").onclick = () => openCopy(true);

  // Start streaming / Apply now: fill Twitch, set the banner, then (when the
  // stream is really on air) count it and post to Discord. Runs in the
  // capture phase so it happens alongside golive.js's own handler.
  $("liveModal").addEventListener(
    "click",
    (e) => {
      if (!e.target.closest("#liveStart")) return;
      const a = active();
      if (!a) return;
      const v = vars();
      invoke("apply_profile", { id: a.id, vars: v })
        .then((r) => {
          rendered = r;
          if (r.banner.label && window.setBannerLabel) window.setBannerLabel(r.banner.label);
          if (window.setBannerMessage) window.setBannerMessage(a.roomOnBanner && r.banner.room ? r.banner.room : "");
          const m = r.result?.twitch?.message;
          if (m) toast(m);
        })
        .catch((err) => toast(`Profile: ${err}`));
      pendingLive = { id: a.id, vars: v };
    },
    true,
  );
  let pendingLive = null;

  // Watch the status the main screen already receives.
  setInterval(() => {
    const live = !!window.lastStatus?.live?.active;
    const onAir = live && (window.lastStatus.live.destinations || []).some((d) => d.state === "live");
    if (onAir && pendingLive && wentLiveFor !== pendingLive.id) {
      wentLiveFor = pendingLive.id;
      const p = pendingLive;
      invoke("profile_went_live", { id: p.id, vars: p.vars })
        .then(async (r) => {
          if (r.discord?.message) toast(r.discord.message);
          // Custom rooms: show the room card right away.
          const prof = data.profiles.find((x) => x.id === p.id);
          if (prof?.roomOnBanner && rendered?.banner?.room) invoke("show_banner_moment", { name: "custom" }).catch(() => {});
          data = await invoke("get_profiles");
          scheduleRender();
        })
        .catch((err) => toast(String(err)));
    }
    if (wasLive && !live) {
      wentLiveFor = null;
      pendingLive = null;
    }
    wasLive = live;
  }, 1000);

  // ---------------- YouTube copy panel + thumbnail ----------------

  const copySheet = el(`
    <div class="modal hidden" id="pfCopyModal" role="dialog" aria-modal="true" aria-labelledby="pfCopyTitle">
      <div class="sheet pf-sheet">
        <div class="sheet-head"><h2 id="pfCopyTitle">YouTube: this stream</h2>
          <button class="icon-btn" data-close title="Close"><svg viewBox="0 0 24 24"><path d="M18 6 6 18M6 6l12 12"/></svg></button></div>
        <p class="sheet-sub">In YouTube Studio open <b>Go live → Stream → Edit</b>, then paste each part. YouTube keeps your other settings from last time.</p>
        <div class="pf-copy" id="pfCopyBody"></div>
        <div class="pf-thumb">
          <canvas id="pfThumb" width="1280" height="720" aria-label="Thumbnail preview"></canvas>
          <div class="pf-thumb-side">
            <p class="hint">Uses the game picture on screen right now, your banner logo, and the profile's text. Change the text in <b>Edit profiles</b>.</p>
            <button class="btn ghost small" id="pfThumbRedo">Use the current game picture</button>
            <button class="btn live small" id="pfThumbSave">Save thumbnail</button>
            <button class="btn ghost small" id="pfThumbOpen">Open thumbnails folder</button>
          </div>
        </div>
        <div class="sheet-foot">
          <button class="btn ghost" id="pfStudioGuide">One-time Studio setup</button>
          <button class="btn live" id="pfOpenStudio">Open YouTube Studio</button>
        </div>
      </div>
    </div>`);
  document.body.appendChild(copySheet);

  async function openCopy(focusThumb = false) {
    await doRender();
    if (!rendered) return;
    const y = rendered.youtube;
    const rows = [
      ["Title", y.title, `${[...y.title].length}/100`],
      ["Description", y.description, `${[...y.description].length}/5000`],
      ["Tags (under “Show more”)", y.tags, `${y.tags.length}/500`],
      ["Category", "Gaming", ""],
      ["Game title", y.game || "—", ""],
    ];
    $("pfCopyBody").innerHTML = rows
      .map(
        ([k, v, n], i) => `<div class="pf-copy-row">
          <div class="pf-copy-k"><span>${escapeHtml(k)}</span><small class="mono">${n}</small></div>
          <${i === 1 ? "pre" : "div"} class="pf-copy-v">${escapeHtml(v)}</${i === 1 ? "pre" : "div"}>
          <button class="btn ghost small" data-copy="${i}">Copy</button></div>`,
      )
      .join("") + `<div class="pf-copy-row pf-fixed"><div class="pf-copy-k"><span>Always</span></div><div class="pf-copy-v">Audience: <b>No, it's not made for kids</b> · Visibility: <b>Public</b></div></div>`;
    $("pfCopyBody").querySelectorAll("[data-copy]").forEach((b) => {
      b.onclick = async () => {
        await navigator.clipboard.writeText(rows[Number(b.dataset.copy)][1]);
        b.textContent = "Copied ✓";
        setTimeout(() => (b.textContent = "Copy"), 1500);
      };
    });
    copySheet.classList.remove("hidden");
    await drawThumb(true);
    if (focusThumb) $("pfThumb").scrollIntoView({ behavior: "smooth", block: "center" });
  }
  copySheet.addEventListener("click", (e) => {
    if (e.target === copySheet || e.target.closest("[data-close]")) copySheet.classList.add("hidden");
  });
  $("pfOpenStudio").onclick = () => invoke("open_link_external", { url: "https://studio.youtube.com/" }).catch((e) => toast(String(e)));
  $("pfStudioGuide").onclick = () => {
    copySheet.classList.add("hidden");
    openEditor("youtube");
  };

  // ----- thumbnail maker -----

  let frameSnapshot = null;
  let logoImg = null;

  function snapshotFrame() {
    const v = $("video");
    if (!v || !v.width || !v.height) return null;
    const c = document.createElement("canvas");
    c.width = v.width;
    c.height = v.height;
    try {
      c.getContext("2d").drawImage(v, 0, 0);
      return c;
    } catch {
      return null;
    }
  }

  async function loadLogo() {
    if (logoImg !== null) return logoImg;
    try {
      const buf = await invoke("get_banner_asset", { name: "logo" });
      const blob = new Blob([buf]);
      logoImg = await createImageBitmap(blob);
    } catch {
      logoImg = false;
    }
    return logoImg;
  }

  function fitFont(ctx, text, weight, family, maxW, maxPx, minPx) {
    let px = maxPx;
    for (; px > minPx; px -= 4) {
      ctx.font = `${weight} ${px}px ${family}`;
      if (ctx.measureText(text).width <= maxW) break;
    }
    return px;
  }

  async function drawThumb(fresh) {
    if (!rendered) return;
    const t = rendered.thumb;
    if (fresh || !frameSnapshot) frameSnapshot = t.useFrame ? snapshotFrame() : null;
    const logo = await loadLogo();
    const c = $("pfThumb");
    const ctx = c.getContext("2d");
    const W = c.width, H = c.height;
    const c1 = t.color1 || "#8b5cf6", c2 = t.color2 || "#22d3ee";
    ctx.clearRect(0, 0, W, H);
    // Background: the game picture (cover), or the profile colours.
    if (frameSnapshot) {
      const s = Math.max(W / frameSnapshot.width, H / frameSnapshot.height);
      const w = frameSnapshot.width * s, h = frameSnapshot.height * s;
      ctx.drawImage(frameSnapshot, (W - w) / 2, (H - h) / 2, w, h);
    } else {
      const g = ctx.createLinearGradient(0, 0, W, H);
      g.addColorStop(0, c1);
      g.addColorStop(1, c2);
      ctx.fillStyle = g;
      ctx.fillRect(0, 0, W, H);
    }
    // Readability: dark on the left where the text sits, and a colour wash.
    let g = ctx.createLinearGradient(0, 0, W * 0.75, 0);
    g.addColorStop(0, "rgba(5,6,14,.86)");
    g.addColorStop(0.55, "rgba(5,6,14,.55)");
    g.addColorStop(1, "rgba(5,6,14,0)");
    ctx.fillStyle = g;
    ctx.fillRect(0, 0, W, H);
    g = ctx.createLinearGradient(0, H, 0, H * 0.55);
    g.addColorStop(0, "rgba(5,6,14,.8)");
    g.addColorStop(1, "rgba(5,6,14,0)");
    ctx.fillStyle = g;
    ctx.fillRect(0, 0, W, H);
    ctx.fillStyle = c2;
    ctx.fillRect(0, H - 14, W, 14);

    // LIVE badge
    ctx.font = `800 40px "Segoe UI", sans-serif`;
    const bw = ctx.measureText("● LIVE").width + 44;
    ctx.fillStyle = "#ef1b2e";
    roundRect(ctx, 56, 52, bw, 66, 14);
    ctx.fill();
    ctx.fillStyle = "#fff";
    ctx.textBaseline = "middle";
    ctx.fillText("● LIVE", 78, 86);

    // Headline (Latin + Tamil: Nirmala UI covers Tamil glyphs).
    const family = `"Segoe UI Black", "Segoe UI", "Nirmala UI", sans-serif`;
    const textW = logo ? 760 : 1120;
    const head = (t.headline || "").toUpperCase();
    const hp = fitFont(ctx, head, 900, family, textW, 150, 64);
    ctx.font = `900 ${hp}px ${family}`;
    ctx.lineJoin = "round";
    ctx.lineWidth = Math.round(hp / 9);
    ctx.strokeStyle = "rgba(0,0,0,.85)";
    ctx.shadowColor = c1;
    ctx.shadowBlur = 36;
    const hy = 300;
    ctx.strokeText(head, 56, hy);
    ctx.fillStyle = "#ffffff";
    ctx.fillText(head, 56, hy);
    ctx.shadowBlur = 0;

    // Subline.
    const sub = t.subline || "";
    const subFamily = `"Nirmala UI", "Segoe UI", sans-serif`;
    const sp = fitFont(ctx, sub, 800, subFamily, textW, 84, 36);
    ctx.font = `800 ${sp}px ${subFamily}`;
    ctx.lineWidth = Math.round(sp / 7);
    ctx.strokeStyle = "rgba(0,0,0,.85)";
    ctx.strokeText(sub, 56, hy + hp * 0.5 + sp * 0.75);
    ctx.fillStyle = c1;
    ctx.fillText(sub, 56, hy + hp * 0.5 + sp * 0.75);

    // Channel name
    ctx.font = `700 40px "Segoe UI", "Nirmala UI", sans-serif`;
    ctx.fillStyle = "rgba(255,255,255,.92)";
    ctx.fillText(t.channel || "", 56, H - 64);

    // Logo in a glowing ring on the right.
    if (logo) {
      const r = 190, cx = W - 270, cy = H / 2 - 10;
      ctx.save();
      ctx.shadowColor = c2;
      ctx.shadowBlur = 50;
      ctx.beginPath();
      ctx.arc(cx, cy, r + 12, 0, Math.PI * 2);
      const rg = ctx.createLinearGradient(cx - r, cy - r, cx + r, cy + r);
      rg.addColorStop(0, c1);
      rg.addColorStop(1, c2);
      ctx.fillStyle = rg;
      ctx.fill();
      ctx.restore();
      ctx.save();
      ctx.beginPath();
      ctx.arc(cx, cy, r, 0, Math.PI * 2);
      ctx.clip();
      const s = Math.max((2 * r) / logo.width, (2 * r) / logo.height);
      ctx.drawImage(logo, cx - (logo.width * s) / 2, cy - (logo.height * s) / 2, logo.width * s, logo.height * s);
      ctx.restore();
    }
  }

  function roundRect(ctx, x, y, w, h, r) {
    ctx.beginPath();
    ctx.moveTo(x + r, y);
    ctx.arcTo(x + w, y, x + w, y + h, r);
    ctx.arcTo(x + w, y + h, x, y + h, r);
    ctx.arcTo(x, y + h, x, y, r);
    ctx.arcTo(x, y, x + w, y, r);
    ctx.closePath();
  }

  $("pfThumbRedo").onclick = () => drawThumb(true);
  $("pfThumbSave").onclick = async () => {
    const blob = await new Promise((res) => $("pfThumb").toBlob(res, "image/png"));
    const bytes = new Uint8Array(await blob.arrayBuffer());
    try {
      const path = await invoke("save_thumbnail_png", bytes, { headers: { "x-profile": active().id } });
      toast(`Thumbnail saved · ${path.split("\\").pop()} (upload it in YouTube Studio)`);
    } catch (e) {
      toast(String(e));
    }
  };
  $("pfThumbOpen").onclick = () => invoke("open_thumbnails");

  // ---------------- profile editor ----------------

  const editor = el(`
    <div class="modal hidden" id="pfEditor" role="dialog" aria-modal="true" aria-labelledby="pfEdTitle">
      <div class="sheet pf-sheet pf-wide">
        <div class="sheet-head"><h2 id="pfEdTitle">Stream profiles</h2>
          <button class="icon-btn" data-close title="Close"><svg viewBox="0 0 24 24"><path d="M18 6 6 18M6 6l12 12"/></svg></button></div>
        <div class="pf-layout">
          <nav class="pf-nav" id="pfNav"></nav>
          <div class="pf-pane" id="pfPane"></div>
        </div>
        <div class="sheet-foot">
          <small class="hint" id="pfVarsHint"></small>
          <button class="btn live" id="pfSave">Save</button>
        </div>
      </div>
    </div>`);
  document.body.appendChild(editor);
  let tab = "defaults";
  let draft = null;

  async function openEditor(which) {
    data = await invoke("get_profiles");
    draft = JSON.parse(JSON.stringify({ defaults: data.defaults, profiles: data.profiles }));
    for (const p of draft.profiles) {
      const o = Array.isArray(p.titleOptions) ? p.titleOptions : [];
      p.titleOptions = [p.title, o[1] || "", o[2] || ""];
    }
    tab = which || "defaults";
    $("pfVarsHint").innerHTML =
      "Variables: " + data.variables.map((v) => `<code title="${escapeHtml(v.desc)}">{${v.name}}</code>`).join(" ");
    renderEditor();
    editor.classList.remove("hidden");
  }
  editor.addEventListener("click", (e) => {
    if (e.target === editor || e.target.closest("[data-close]")) editor.classList.add("hidden");
  });

  function renderEditor() {
    const tabs = [
      ["defaults", "Shared defaults"],
      ...draft.profiles.map((p) => [p.id, p.name]),
      ["connections", "Twitch & Discord"],
      ["youtube", "YouTube Studio setup"],
    ];
    $("pfNav").innerHTML =
      tabs.map(([id, name]) => `<button class="pf-tab${id === tab ? " on" : ""}" data-tab="${escapeHtml(id)}">${escapeHtml(name)}</button>`).join("") +
      `<button class="pf-tab add" data-add>+ New profile</button>`;
    const pane = $("pfPane");
    if (tab === "defaults") pane.innerHTML = defaultsPane();
    else if (tab === "connections") pane.innerHTML = connectionsPane();
    else if (tab === "youtube") pane.innerHTML = youtubePane();
    else pane.innerHTML = profilePane(draft.profiles.find((p) => p.id === tab));
    bindPane();
  }

  $("pfNav").addEventListener("click", (e) => {
    const b = e.target.closest("button");
    if (!b) return;
    if (b.dataset.add !== undefined) {
      const id = `custom${Date.now().toString(36)}`;
      draft.profiles.push({
        id, name: "New profile", kind: "pc", game: "", mode: "Gameplay", title: "🔴 {game} LIVE | {mode} 🔥 | Day {n}",
        titleOptions: ["🔴 {game} LIVE | {mode} 🔥 | Day {n}", "🔴 {game} LIVE | Epic Moments! | Day {n}", "🔴 {game} LIVE | Special Stream! | Day {n}"],
        description: "", tags: "", hashtags: "", bannerLabel: "{game}", twitch: { id: "", name: "" }, twitchTags: "Gaming",
        thumb: { headline: "{game}", subline: "{mode}", color1: "#8b5cf6", color2: "#22d3ee", useFrame: true },
        announce: false, announceText: "🔴 LIVE: {title}\n▶ {youtube}", announceEveryone: false, roomOnBanner: false,
        disclaimer: "⚠️ {game} © its publisher. Streamed for entertainment.", count: 0,
      });
      tab = id;
    } else tab = b.dataset.tab;
    renderEditor();
  });

  const inp = (path, label, value, opts = {}) => {
    const v = escapeHtml(value ?? "");
    const ph = escapeHtml(opts.ph || "");
    const control = opts.area
      ? `<textarea class="text-in" data-path="${path}" rows="${opts.rows || 4}" placeholder="${ph}">${v}</textarea>`
      : `<input class="text-in" data-path="${path}" ${opts.type ? `type="${opts.type}"` : ""} maxlength="${opts.max || 300}" placeholder="${ph}" value="${v}">`;
    return `<label class="field${opts.half ? " half" : ""}"><span>${label}</span>${control}${opts.hint ? `<small class="hint">${opts.hint}</small>` : ""}</label>`;
  };
  const sw = (path, label, on) =>
    `<label class="switch"><input type="checkbox" data-path="${path}" ${on ? "checked" : ""}><span class="track"><span class="knob"></span></span>${label}</label>`;

  function defaultsPane() {
    const d = draft.defaults;
    return `<h3>Shared by every stream</h3>
      <p class="hint">Fill these once. Every profile uses them; a profile only changes what's different.</p>
      <div class="pf-grid">
        ${inp("defaults.channel", "Channel name", d.channel, { half: true })}
        ${inp("defaults.youtubeHandle", "YouTube handle", d.youtubeHandle, { half: true, ph: "@yourchannel", hint: "Gives the link youtube.com/@you/live, which always points at your current stream" })}
        ${inp("defaults.twitchLogin", "Twitch username", d.twitchLogin, { half: true, ph: "filled in when you connect Twitch" })}
        ${inp("defaults.instagram", "Instagram link", d.instagram, { half: true, ph: "https://instagram.com/…" })}
        ${inp("defaults.discordInvite", "Discord invite", d.discordInvite, { half: true, ph: "https://discord.gg/…" })}
        ${inp("defaults.schedule", "Schedule", d.schedule, { half: true })}
        ${inp("defaults.goal", "Channel goal", d.goal, { half: true, ph: "Road to 1K subscribers" })}
        ${inp("defaults.email", "Business email (optional)", d.email, { half: true, ph: "collabs@…" })}
        ${inp("defaults.device", "Phone you play on", d.device, { half: true, ph: "e.g. OnePlus 13R" })}
        ${inp("defaults.pc", "PC specs", d.pc, { half: true, ph: "e.g. i5-13450HX · RTX 4050 · 16 GB" })}
      </div>
      <p class="hint">Empty fields are left out of the description automatically. Don't put a personal phone number or UPI ID here — viewers can use Super Chat and channel memberships safely instead.</p>
      ${inp("defaults.footer", "Description footer (under every description)", d.footer, { area: true, rows: 9 })}
      ${inp("defaults.rules", "Chat rules <code>{rules}</code>", d.rules, { area: true, rows: 4 })}
      <div class="pf-grid">
        ${inp("defaults.tags", "YouTube tags for every stream", d.tags, { half: true, hint: "Comma separated" })}
        ${inp("defaults.hashtags", "Hashtags for every stream <code>{tags}</code>", d.hashtags, { half: true, hint: "YouTube shows the first 3 above the title" })}
      </div>`;
  }

  function profilePane(p) {
    const i = draft.profiles.indexOf(p);
    const k = `profiles.${i}`;
    const builtin = ["bgmi", "rooms", "efootball", "fc26", "ets2", "wd2", "detroit", "pubgpc", "pc"].includes(p.id);
    return `<h3>${escapeHtml(p.name)} <small class="muted">· used ${p.count} time${p.count === 1 ? "" : "s"}</small></h3>
      <div class="pf-grid">
        ${inp(`${k}.name`, "Profile name", p.name, { half: true })}
        <label class="field half"><span>Kind</span><select data-path="${k}.kind">
          <option value="phone"${p.kind === "phone" ? " selected" : ""}>Phone game</option>
          <option value="rooms"${p.kind === "rooms" ? " selected" : ""}>Custom rooms (asks room ID & password)</option>
          <option value="pc"${p.kind === "pc" ? " selected" : ""}>PC game (asks the game each time)</option></select></label>
        ${inp(`${k}.game`, "Game <code>{game}</code>", p.game, { half: true })}
        ${inp(`${k}.mode`, "Usual mode <code>{mode}</code>", p.mode, { half: true, hint: "You can change it in Go live each time" })}
      </div>
      ${inp(`${k}.title`, "Title — normal", p.title, { max: 200, hint: "YouTube allows 100 characters, Twitch 140. Pick the style in Go live." })}
      ${inp(`${k}.titleOptions.1`, "Title — hype", (p.titleOptions || [])[1] || "", { max: 200 })}
      ${inp(`${k}.titleOptions.2`, "Title — special day (updates, festivals)", (p.titleOptions || [])[2] || "", { max: 200 })}
      ${inp(`${k}.description`, "Description (the shared footer is added below it)", p.description, { area: true, rows: 5 })}
      ${inp(`${k}.disclaimer`, "Disclaimer <code>{disclaimer}</code>", p.disclaimer, { area: true, rows: 2, hint: "Who owns the game, and “not an official event” for rooms. Keep it short and true." })}
      <div class="pf-grid">
        ${inp(`${k}.tags`, "Extra YouTube tags", p.tags, { half: true })}
        ${inp(`${k}.hashtags`, "Extra hashtags", p.hashtags, { half: true })}
        ${inp(`${k}.bannerLabel`, "Banner game label", p.bannerLabel, { half: true, max: 30 })}
        ${inp(`${k}.twitchTags`, "Twitch tags (up to 10)", p.twitchTags, { half: true })}
      </div>
      <div class="field"><span>Twitch category</span>
        <div class="key-row"><input class="text-in" id="pfCatQ" placeholder="Search Twitch, e.g. eFootball" value="${escapeHtml(p.twitch.name)}">
        <button class="btn ghost small" id="pfCatGo" data-idx="${i}">Search</button></div>
        <div id="pfCatRes" class="pf-cats"></div>
        <small class="hint">${p.twitch.id ? `Chosen: <b>${escapeHtml(p.twitch.name)}</b>` : "Picked automatically from the name on first use"}</small></div>
      <h4>Thumbnail</h4>
      <div class="pf-grid">
        ${inp(`${k}.thumb.headline`, "Big text", p.thumb.headline, { half: true, max: 40 })}
        ${inp(`${k}.thumb.subline`, "Second line", p.thumb.subline, { half: true, max: 60 })}
        ${inp(`${k}.thumb.color1`, "Colour 1", p.thumb.color1, { half: true, type: "color" })}
        ${inp(`${k}.thumb.color2`, "Colour 2", p.thumb.color2, { half: true, type: "color" })}
      </div>
      ${sw(`${k}.thumb.useFrame`, "Use the game picture as the background", p.thumb.useFrame)}
      <h4>Discord</h4>
      ${sw(`${k}.announce`, "Post “I'm live” on Discord when the stream starts", p.announce)}
      ${sw(`${k}.announceEveryone`, "Mention @everyone", p.announceEveryone)}
      ${inp(`${k}.announceText`, "Discord message", p.announceText, { area: true, rows: 3 })}
      ${p.kind === "rooms" ? sw(`${k}.roomOnBanner`, "Show the room ID and password on the stream banner", p.roomOnBanner) : ""}
      <div class="pf-row-end">
        ${builtin ? `<button class="btn ghost small" id="pfReset" data-id="${escapeHtml(p.id)}">Reset to the ready-made version</button>` : `<button class="btn ghost small bad" id="pfDelete" data-idx="${i}">Delete profile</button>`}
      </div>`;
  }

  function connectionsPane() {
    const tw = data.twitch;
    const lg = data.login;
    const twBody = tw.connected
      ? `<p>Connected as <b>${escapeHtml(tw.login)}</b>. Each stream sets your Twitch title, category, tags and language.</p>
         <button class="btn ghost small" id="pfTwOut">Disconnect Twitch</button>`
      : lg.state === "waiting"
        ? `<p class="pf-code">Go to <a href="#" data-link="${escapeHtml(lg.uri)}">${escapeHtml(lg.uri)}</a> and enter <b class="mono">${escapeHtml(lg.code)}</b></p>
           <p class="hint">Waiting for you to approve in the browser…</p><button class="btn ghost small" id="pfTwCancel">Cancel</button>`
        : `<ol class="pf-steps">
            <li>Open <a href="#" data-link="https://dev.twitch.tv/console/apps/create">dev.twitch.tv → Register Your Application</a> (log in with your Twitch account).</li>
            <li>Name: <b>PhoneCast ${escapeHtml(data.defaults.channel)}</b> · OAuth Redirect URL: <code>http://localhost</code> · Category: <b>Broadcaster Suite</b> · Client Type: <b>Public</b> → Create.</li>
            <li>Open the app (Manage), copy the <b>Client ID</b> and paste it here.</li></ol>
           <div class="key-row"><input class="text-in" id="pfTwId" placeholder="Twitch Client ID" value="${escapeHtml(data.defaults.twitchClientId)}">
           <button class="btn live small" id="pfTwIn">Connect Twitch</button></div>
           ${lg.state === "error" ? `<p class="bad">${escapeHtml(lg.error)}</p>` : ""}
           <p class="hint">PhoneCast only asks permission to change your stream info. Your password never reaches PhoneCast.</p>`;
    return `<h3>Twitch</h3>${twBody}
      <h3>Discord announcements</h3>
      <ol class="pf-steps">
        <li>In your Discord server: <b>Server Settings → Integrations → Webhooks → New Webhook</b>.</li>
        <li>Pick the channel (e.g. <code>#live-alerts</code>), then <b>Copy Webhook URL</b> and paste it here.</li></ol>
      <div class="key-row"><input class="text-in" id="pfHook" type="password" autocomplete="off" placeholder="${data.discordWebhookSet ? "Saved ✓ — paste a new link to replace it" : "https://discord.com/api/webhooks/…"}">
        <button class="btn ghost small" id="pfHookSave">Save</button><button class="btn ghost small" id="pfHookTest" ${data.discordWebhookSet ? "" : "disabled"}>Send test</button>
        ${data.discordWebhookSet ? `<button class="btn ghost small" id="pfHookDel">Remove</button>` : ""}</div>
      <p class="hint">The link is encrypted with your Windows account. Turn announcements on per profile.</p>
      <h3>Streaming in a Discord call</h3>
      <p>To show your game to friends in a Discord voice channel, use Discord's <b>Share your screen</b> and pick the <b>OSSG</b> window. That shares the live preview; nothing else is needed.</p>`;
  }

  function youtubePane() {
    const blocked = "free uc, free diamonds, sub4sub, sub for sub, telegram, whatsapp group, paytm, gpay, phonepe, hack, mod apk, aimbot, wallhack, followers, promote, check my channel, link in bio, giveaway link";
    return `<h3>YouTube Studio: one-time setup</h3>
      <p class="hint">Do this once. YouTube keeps these settings for every stream after.</p>
      <ol class="pf-steps big">
        <li><b>Turn on live streaming</b>: <a href="#" data-link="https://studio.youtube.com/">studio.youtube.com</a> → <b>Create → Go live</b>. New channels verify a phone number and wait up to 24 hours, once.</li>
        <li><b>Use one stream key forever</b>: in <b>Stream</b> tab → <b>Stream key</b>, choose <b>Default stream key</b>. Copy it into OSSG's Go live once. Don't press “Reset”.</li>
        <li><b>Stream settings → Edit</b>: Category <b>Gaming</b> and the game; Visibility <b>Public</b>; Audience <b>No, it's not made for kids</b>; no age restriction.</li>
        <li><b>Stream settings → Customisation</b>: Live chat <b>On</b>, Live chat replay <b>On</b>, Slow mode <b>5 seconds</b>, Live reactions <b>On</b>. For custom room giveaways you can set participants to <b>Subscribers</b>.</li>
        <li><b>Stream settings → Settings</b>: <b>Enable DVR</b> on, Latency <b>Low latency</b> (fast chat, keeps full quality), <b>Auto-start</b> and <b>Auto-stop</b> on, so the stream starts when OSSG connects.</li>
        <li><b>Moderators & blocked words</b>: Studio → <b>Settings → Community</b>. Add your moderators, turn on <b>Block links</b> and <b>Hold potentially inappropriate messages for review</b>, and paste these blocked words (add any abuse you see in your chat):
          <div class="pf-copy-row"><div class="pf-copy-v">${escapeHtml(blocked)}</div><button class="btn ghost small" data-copytext="${escapeHtml(blocked)}">Copy</button></div></li>
        <li><b>Defaults for the VODs</b>: Studio → <b>Settings → Upload defaults</b>: paste your shared description footer and tags, so the saved stream videos have them too.</li>
        <li><b>Each stream (10 seconds)</b>: in OSSG Go live, click <b>YouTube: copy title & description</b>, paste them into <b>Stream settings → Edit</b>, and upload the saved thumbnail. Twitch and Discord are done for you.</li>
      </ol>`;
  }

  function bindPane() {
    const pane = $("pfPane");
    pane.querySelectorAll("[data-path]").forEach((c) => {
      c.addEventListener(c.type === "checkbox" ? "change" : "input", () => {
        const parts = c.dataset.path.split(".");
        let o = draft;
        for (let j = 0; j < parts.length - 1; j++) o = o[isNaN(parts[j]) ? parts[j] : Number(parts[j])];
        o[parts.at(-1)] = c.type === "checkbox" ? c.checked : c.value;
        if (parts.at(-1) === "name") $("pfNav").querySelector(`[data-tab="${CSS.escape(tab)}"]`).textContent = c.value || "Profile";
        if (parts.at(-1) === "kind") renderEditor();
      });
    });
    pane.querySelectorAll("[data-link]").forEach((a) =>
      a.addEventListener("click", (e) => {
        e.preventDefault();
        invoke("open_link_external", { url: a.dataset.link }).catch((err) => toast(String(err)));
      }),
    );
    pane.querySelectorAll("[data-copytext]").forEach((b) =>
      b.addEventListener("click", async () => {
        await navigator.clipboard.writeText(b.dataset.copytext);
        b.textContent = "Copied ✓";
      }),
    );
    const on = (id, fn) => {
      const x = $(id);
      if (x) x.onclick = fn;
    };
    on("pfCatGo", async () => {
      const box = $("pfCatRes");
      box.textContent = "Searching…";
      try {
        const list = await invoke("twitch_search_category", { query: $("pfCatQ").value });
        const i = Number($("pfCatGo").dataset.idx);
        box.innerHTML = list.length
          ? list.map((c) => `<button class="pf-cat" data-id="${escapeHtml(c.id)}" data-name="${escapeHtml(c.name)}">${escapeHtml(c.name)}</button>`).join("")
          : "No match";
        box.querySelectorAll(".pf-cat").forEach((b) =>
          b.addEventListener("click", () => {
            draft.profiles[i].twitch = { id: b.dataset.id, name: b.dataset.name };
            renderEditor();
          }),
        );
      } catch (e) {
        box.textContent = String(e);
      }
    });
    on("pfReset", async () => {
      data = await invoke("reset_profile", { id: $("pfReset").dataset.id });
      draft = JSON.parse(JSON.stringify({ defaults: draft.defaults, profiles: data.profiles }));
      renderEditor();
      toast("Back to the ready-made version");
    });
    on("pfDelete", () => {
      draft.profiles.splice(Number($("pfDelete").dataset.idx), 1);
      tab = "defaults";
      renderEditor();
    });
    on("pfTwIn", async () => {
      try {
        data = await invoke("twitch_login", { clientId: $("pfTwId").value });
        renderEditor();
        pollLogin();
      } catch (e) {
        toast(String(e));
      }
    });
    on("pfTwCancel", async () => {
      data = await invoke("twitch_login_cancel");
      renderEditor();
    });
    on("pfTwOut", async () => {
      data = await invoke("twitch_logout");
      renderEditor();
    });
    on("pfHookSave", async () => {
      try {
        data = await invoke("save_profiles", { data: { discordWebhook: $("pfHook").value } });
        toast("Discord webhook saved");
        renderEditor();
      } catch (e) {
        toast(String(e));
      }
    });
    on("pfHookDel", async () => {
      data = await invoke("save_profiles", { data: { discordWebhook: "" } });
      renderEditor();
    });
    on("pfHookTest", async () => {
      try {
        await invoke("discord_test");
        toast("Test message sent — check your Discord channel");
      } catch (e) {
        toast(String(e));
      }
    });
  }

  let pollTimer;
  function pollLogin() {
    clearInterval(pollTimer);
    pollTimer = setInterval(async () => {
      data = await invoke("get_profiles");
      if (data.login.state !== "waiting") {
        clearInterval(pollTimer);
        if (data.twitch.connected) toast(`Twitch connected as ${data.twitch.login}`);
        if (!editor.classList.contains("hidden") && tab === "connections") renderEditor();
        scheduleRender();
      }
    }, 2000);
  }

  $("pfSave").onclick = async () => {
    // Title styles: slot 0 is always the normal title.
    for (const p of draft.profiles) {
      const o = Array.isArray(p.titleOptions) ? p.titleOptions : [];
      p.titleOptions = [p.title, o[1] || "", o[2] || ""];
    }
    try {
      data = await invoke("save_profiles", { data: { defaults: draft.defaults, profiles: draft.profiles } });
      toast("Profiles saved");
      editor.classList.add("hidden");
      renderPicker();
    } catch (e) {
      toast(String(e));
    }
  };

  load().catch((e) => console.warn("profiles", e));
})();
