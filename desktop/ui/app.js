// PhoneCast desktop UI: live preview (WebCodecs, GPU-decoded) + status panels.
const { invoke } = window.__TAURI__.core;
const { listen } = window.__TAURI__.event;

const VIDEO_CONFIG = 0x01;
const VIDEO_FRAME = 0x02;
const FLAG_KEYFRAME = 0x01;

const $ = (id) => document.getElementById(id);
const canvas = $("video");
const ctx = canvas.getContext("2d", { alpha: false, desynchronized: true });

// ---------------- preview ----------------

let decoder = null;
let csd = null;
let waitKey = true;
let lastFrameAt = 0;

function codecString(codec, csdBytes) {
  const nals = splitNals(csdBytes);
  if (codec === 1) {
    const sps = nals.find((n) => (n[0] & 0x1f) === 7);
    if (sps) {
      const hex = (b) => b.toString(16).padStart(2, "0");
      return `avc1.${hex(sps[1])}${hex(sps[2])}${hex(sps[3])}`;
    }
    return "avc1.640033";
  }
  // H.265: profile + level from the SPS profile_tier_level.
  const sps = nals.find((n) => ((n[0] >> 1) & 0x3f) === 33);
  if (sps && sps.length > 15) {
    const profile = sps[3] & 0x1f;
    const level = sps[14];
    return `hev1.${profile}.6.L${level}.B0`;
  }
  return "hev1.1.6.L153.B0";
}

function splitNals(d) {
  const out = [];
  let start = -1;
  for (let i = 0; i + 2 < d.length; i++) {
    if (d[i] === 0 && d[i + 1] === 0 && d[i + 2] === 1) {
      if (start >= 0) out.push(d.subarray(start, i));
      start = i + 3;
      i += 2;
    }
  }
  if (start >= 0) out.push(d.subarray(start));
  return out;
}

async function configure(codec, csdBytes) {
  if (decoder) {
    try { decoder.close(); } catch {}
  }
  csd = csdBytes;
  waitKey = true;
  const base = { codec: codecString(codec, csdBytes), optimizeForLatency: true };
  let config = { ...base, hardwareAcceleration: "prefer-hardware" };
  try {
    if (!(await VideoDecoder.isConfigSupported(config)).supported) config = base;
  } catch {
    config = base;
  }
  resetPacing();
  decoder = new VideoDecoder({
    output: schedule,
    error: (e) => {
      console.warn("decoder error", e);
      waitKey = true;
      invoke("request_keyframe");
    },
  });
  try {
    decoder.configure(config);
  } catch (e) {
    toast(`This PC can't preview ${config.codec}. Try H.264 in the phone app.`);
  }
}

// ---------------- smooth presentation ----------------
// The phone delivers frames in an uneven rhythm (8 ms, 25 ms, 8 ms… because
// the game runs at 60 fps on a 120 Hz screen). Showing them as they arrive
// looks choppy. Instead, frames go into a tiny buffer and are shown at a
// steady 60 fps cadence, one per 16.7 ms. Delay added: about one frame.

const FRAME_MS = 1000 / 60;
const MAX_QUEUE = 3; // never let delay build up beyond this
let queue = [];
let nextDue = 0;
let drawn = 0;
let previewFps = 0;
let jitterMs = 0; // how uneven the shown frames are (lower = smoother)
let lastShown = 0;

function resetPacing() {
  for (const f of queue) f.close();
  queue = [];
  nextDue = 0;
}

function schedule(frame) {
  queue.push(frame);
  // Catch up if we fell behind: drop the oldest frames, keep the newest.
  while (queue.length > MAX_QUEUE) queue.shift().close();
  lastFrameAt = performance.now();
}

function present(now) {
  // Keep one frame in reserve so an early/late arrival never causes a hitch;
  // if the reserve is all we have and we're well past due, show it anyway.
  const ready = queue.length >= 2 || (queue.length === 1 && now >= nextDue + FRAME_MS / 2);
  if (ready && now >= nextDue - 3) {
    const f = queue.shift();
    if (canvas.width !== f.displayWidth || canvas.height !== f.displayHeight) {
      canvas.width = f.displayWidth;
      canvas.height = f.displayHeight;
    }
    ctx.drawImage(f, 0, 0);
    f.close();
    drawn++;
    if (lastShown) jitterMs = jitterMs * 0.95 + Math.abs(now - lastShown - FRAME_MS) * 0.05;
    lastShown = now;
    // Keep a steady grid; resync if we stalled (e.g. phone paused).
    nextDue = now - nextDue > FRAME_MS * 2 ? now + FRAME_MS : nextDue + FRAME_MS;
  }
  requestAnimationFrame(present);
}
requestAnimationFrame(present);
setInterval(() => {
  previewFps = drawn;
  drawn = 0;
}, 1000);

function onFrame(flags, pts, payload) {
  if (!decoder || decoder.state !== "configured") return;
  const key = (flags & FLAG_KEYFRAME) !== 0;
  if (waitKey && !key) return;
  // Never let the preview lag behind: skip to the next keyframe if it piles up.
  if (!key && decoder.decodeQueueSize > 3) {
    waitKey = true;
    invoke("request_keyframe");
    return;
  }
  let data = payload;
  if (key) {
    data = new Uint8Array(csd.length + payload.length);
    data.set(csd, 0);
    data.set(payload, csd.length);
    waitKey = false;
  }
  decoder.decode(new EncodedVideoChunk({ type: key ? "key" : "delta", timestamp: Number(pts), data }));
}

function connectPreview(port) {
  const ws = new WebSocket(`ws://127.0.0.1:${port}`);
  ws.binaryType = "arraybuffer";
  ws.onmessage = (ev) => {
    const buf = ev.data;
    const view = new DataView(buf);
    const type = view.getUint8(0);
    const flags = view.getUint8(1);
    const pts = view.getBigInt64(2);
    const payload = new Uint8Array(buf, 10);
    if (type === VIDEO_CONFIG) {
      const codec = payload[0];
      const newCsd = payload.slice(5);
      if (!csd || !sameBytes(csd, newCsd) || !decoder || decoder.state === "closed") configure(codec, newCsd);
    } else if (type === VIDEO_FRAME) {
      onFrame(flags, pts, payload);
    }
  };
  ws.onclose = () => setTimeout(() => connectPreview(port), 1000);
}

function sameBytes(a, b) {
  if (a.length !== b.length) return false;
  for (let i = 0; i < a.length; i++) if (a[i] !== b[i]) return false;
  return true;
}

// ---------------- status ----------------

const THERMAL = ["Cool", "Warm", "Moderate", "Hot — throttling", "Severe", "Critical", "Shutdown"];

function cls(el, c) {
  el.classList.remove("good", "warn", "bad");
  if (c) el.classList.add(c);
}

let recording = false;
const fpsHistory = [];

function render(s) {
  const pc = !!s.pcMode;
  const streaming = (s.connected || pc) && s.video && performance.now() - lastFrameAt < 2000;
  $("pill").className = "pill" + (s.connected || pc ? " on" : "");
  $("pillText").textContent = pc ? "This PC" + (s.connected ? ` · phone on ${s.link}` : "") : s.connected ? `Connected · ${s.link}` : "Waiting for phone";
  $("screen").classList.toggle("streaming", !!streaming);
  $("empty").classList.toggle("hidden", !!streaming);
  $("hud").classList.toggle("hidden", !streaming);
  $("recBtn").disabled = !s.video;

  // Stream health
  const p = s.phone || {};
  $("fps").textContent = s.fps ?? 0;
  $("mbps").textContent = s.mbps ?? 0;
  const lat = $("latency");
  lat.textContent = s.latencyMs ?? "—";
  cls(lat, s.latencyMs == null ? null : s.latencyMs < 60 ? "good" : s.latencyMs < 120 ? "warn" : "bad");
  cls($("fps"), !(s.connected || pc) ? null : s.fps >= 57 ? "good" : s.fps >= 28 ? "warn" : "bad");
  $("videoInfo").textContent = s.video
    ? `${s.video.width}×${s.video.height} ${s.video.codec}${p.bitrateMode ? " · " + p.bitrateMode : ""}`
    : "—";
  const sm = $("smooth");
  sm.textContent = streaming
    ? `${previewFps} fps · ±${jitterMs.toFixed(1)} ms${p.paced ? " · paced" : ""}`
    : "—";
  cls(sm, !streaming ? null : previewFps >= 57 && jitterMs < 4 ? "good" : previewFps >= 45 ? "warn" : "bad");
  $("dropped").textContent = p.dropped ?? 0;
  $("rtt").textContent = s.rttMs != null ? `${s.rttMs} ms` : "—";

  fpsHistory.push(s.connected ? s.fps ?? 0 : 0);
  if (fpsHistory.length > 60) fpsHistory.shift();
  drawSpark();

  $("hudRes").textContent = s.video ? `${s.video.width}×${s.video.height}` : "—";
  $("hudFps").textContent = previewFps;
  $("hudLat").textContent = s.latencyMs ?? "—";
  $("hudMbps").textContent = s.mbps ?? 0;

  // Phone
  $("device").textContent = s.device || "—";
  const ring = $("batRing");
  if (p.battery != null) {
    ring.style.setProperty("--p", p.battery);
    ring.style.setProperty("--ring", p.battery > 30 ? "var(--good)" : p.battery > 15 ? "var(--warn)" : "var(--bad)");
    $("batPct").textContent = `${p.battery}%`;
  }
  ring.classList.toggle("charging", !!p.charging);
  const t = $("temp");
  t.textContent = p.batteryTempC != null ? `${p.batteryTempC.toFixed(1)} °C` : "—";
  cls(t, p.batteryTempC == null ? null : p.batteryTempC < 40 ? "good" : p.batteryTempC < 45 ? "warn" : "bad");
  const th = $("thermal");
  th.textContent = p.thermal != null ? THERMAL[p.thermal] ?? p.thermal : "—";
  cls(th, p.thermal == null ? null : p.thermal <= 1 ? "good" : p.thermal <= 2 ? "warn" : "bad");

  // Connection
  $("link").textContent = s.connected ? s.link : "—";
  const adb = s.adb || {};
  $("usb").textContent = !adb.found
    ? "adb not found"
    : adb.devices.length === 0
      ? "No phone on USB"
      : adb.reversed.length
        ? "Ready"
        : "Phone found, setting up…";

  // Recording
  recording = !!s.recording;
  $("recBtn").classList.toggle("active", recording);
  $("recText").textContent = recording ? "Stop" : "Record";
  $("screen").classList.toggle("recording", recording);
  $("recBadge").classList.toggle("hidden", !recording);
  if (recording) $("recTime").textContent = fmtTime(s.recording.seconds);
  if (s.recordError) toast(`Recording problem: ${s.recordError}`);

  if (s.mixer) renderMixer(s.mixer);
  renderLive(s.live);
  renderSource(s);
}

// ---------------- source: phone or this PC ----------------

let sourceTouchedAt = 0;
let sourcesKey = "";
const srcUI = {
  seg: $("sourceSeg"), pc: $("pcOptions"), monitor: $("monitorSelect"), camToggle: $("camToggle"),
  camOpts: $("camOptions"), cam: $("camSelect"), size: $("camSize"), corner: $("camCorner"), mirror: $("camMirror"),
};

function currentSource() {
  const screen = srcUI.seg.querySelector("button.on")?.dataset.v === "screen";
  return {
    source: screen ? { kind: "screen", output: Number(srcUI.monitor.value || 0) } : { kind: "phone" },
    webcam: {
      enabled: srcUI.camToggle.checked,
      device: srcUI.cam.value || "",
      size: Number(srcUI.size.value),
      corner: srcUI.corner.value,
      mirror: srcUI.mirror.checked,
    },
  };
}

let applyTimer;
function applySource() {
  sourceTouchedAt = performance.now();
  srcUI.pc.classList.toggle("hidden", srcUI.seg.querySelector("button.on")?.dataset.v !== "screen");
  srcUI.camOpts.classList.toggle("hidden", !srcUI.camToggle.checked);
  clearTimeout(applyTimer);
  applyTimer = setTimeout(async () => {
    try {
      await invoke("set_source", { settings: currentSource() });
    } catch (e) {
      toast(String(e));
    }
  }, 250);
}

for (const b of srcUI.seg.querySelectorAll("button")) {
  b.addEventListener("click", () => {
    for (const x of srcUI.seg.querySelectorAll("button")) x.classList.toggle("on", x === b);
    applySource();
  });
}
for (const el of [srcUI.monitor, srcUI.camToggle, srcUI.cam, srcUI.size, srcUI.corner, srcUI.mirror]) {
  el.addEventListener("change", applySource);
}

function renderSource(s) {
  if (!s.source) return;
  const key = JSON.stringify(s.sources);
  if (s.sources && key !== sourcesKey) {
    sourcesKey = key;
    srcUI.monitor.innerHTML = (s.sources.monitors || []).map((m) => `<option value="${m.output}">${escapeHtml(m.name)}</option>`).join("")
      || '<option value="0">No monitor found</option>';
    srcUI.cam.innerHTML = (s.sources.webcams || []).map((w) => `<option value="${escapeHtml(w)}">${escapeHtml(w)}</option>`).join("")
      || '<option value="">No camera found</option>';
  }
  if (performance.now() - sourceTouchedAt < 1500) return;
  const screen = s.source.source.kind === "screen";
  for (const x of srcUI.seg.querySelectorAll("button")) x.classList.toggle("on", (x.dataset.v === "screen") === screen);
  srcUI.pc.classList.toggle("hidden", !screen);
  if (screen) srcUI.monitor.value = String(s.source.source.output);
  const w = s.source.webcam || {};
  srcUI.camToggle.checked = !!w.enabled;
  srcUI.camOpts.classList.toggle("hidden", !w.enabled);
  if (w.device) srcUI.cam.value = w.device;
  srcUI.size.value = String(w.size ?? 0.25);
  srcUI.corner.value = w.corner || "br";
  srcUI.mirror.checked = !!w.mirror;

  // Wording changes with the source.
  $("empty").querySelector("h2").textContent = screen ? "Capturing this PC" : "Connect your phone";
  const game = strips.game?.el.querySelector(".names b");
  if (game) game.textContent = screen ? "PC sound" : "Game";
}

// ---------------- go live ----------------

let isLive = false;
let liveSettings = null;
const DEST_ICON = {
  youtube: '<div class="dest-icon yt"><svg viewBox="0 0 24 24"><path d="M22 8.5a3 3 0 0 0-2.1-2.1C18 6 12 6 12 6s-6 0-7.9.4A3 3 0 0 0 2 8.5 31 31 0 0 0 2 12a31 31 0 0 0 .1 3.5 3 3 0 0 0 2 2.1C6 18 12 18 12 18s6 0 7.9-.4a3 3 0 0 0 2.1-2.1A31 31 0 0 0 22 12a31 31 0 0 0 0-3.5z"/><path d="m10 15 5-3-5-3z"/></svg></div>',
  twitch: '<div class="dest-icon tw"><svg viewBox="0 0 24 24"><path d="M21 2H3v16h5v4l4-4h5l4-4z"/><path d="M11 11V7M16 11V7"/></svg></div>',
};
const STATE_TEXT = { live: "Live", connecting: "Connecting", reconnecting: "Reconnecting", error: "Error", stopped: "Stopped" };

function renderLive(l) {
  isLive = !!(l && l.active);
  const btn = $("liveBtn");
  btn.classList.toggle("on", isLive);
  $("liveText").textContent = isLive ? `End stream · ${fmtTime(l.seconds)}` : "Go live";
  $("screen").classList.toggle("live", isLive);
  $("liveBadge").classList.toggle("hidden", !isLive);
  $("liveCard").classList.toggle("hidden", !isLive);
  if (!isLive) return;
  $("liveTime").textContent = fmtTime(l.seconds);
  $("liveDests").innerHTML = l.destinations
    .map((d) => {
      const meta = d.state === "live"
        ? `${(d.kbps / 1000).toFixed(1)} Mbps · ${Math.round(d.fps)} fps · ${d.quality}${d.dropped ? ` · ${d.dropped} dropped` : ""}`
        : d.reconnects ? `Reconnect attempt ${d.reconnects}` : `Starting ${d.quality}`;
      return `<div class="ldest">${DEST_ICON[d.id] || ""}<b>${d.name}</b>
        <span class="state ${d.state}">${STATE_TEXT[d.state] || d.state}</span>
        <span class="meta">${meta}</span>
        ${d.error && d.state !== "live" ? `<span class="err">${escapeHtml(d.error)}</span>` : ""}</div>`;
    })
    .join("");
}

const modal = $("liveModal");
const dests = {};
for (const el of modal.querySelectorAll(".dest")) {
  const id = el.dataset.dest;
  const d = (dests[id] = {
    el,
    enabled: el.querySelector(".enabled"),
    key: el.querySelector(".key"),
    quality: el.querySelector(".quality"),
  });
  d.enabled.addEventListener("change", () => {
    el.classList.toggle("on", d.enabled.checked);
    if (d.enabled.checked && !d.key.value && !d.key.classList.contains("saved")) setTimeout(() => d.key.focus(), 200);
    updateUpload();
  });
  d.quality.addEventListener("change", updateUpload);
  el.querySelector(".reveal").addEventListener("click", (e) => {
    e.preventDefault();
    d.key.type = d.key.type === "password" ? "text" : "password";
  });
}
let framing = "fit";
for (const b of $("framing").querySelectorAll("button")) {
  b.addEventListener("click", () => {
    framing = b.dataset.v;
    for (const x of $("framing").querySelectorAll("button")) x.classList.toggle("on", x === b);
  });
}

function fillModal(s) {
  liveSettings = s;
  $("ffmpegMissing").classList.toggle("hidden", s.ffmpeg);
  $("encName").textContent = s.encoder || "your GPU";
  for (const id of ["youtube", "twitch"]) {
    const d = dests[id];
    const cfg = s[id];
    d.enabled.checked = cfg.enabled;
    d.el.classList.toggle("on", cfg.enabled);
    d.quality.innerHTML = cfg.presets.map((p) => `<option value="${p.id}" data-kbps="${p.kbps}">${p.label}</option>`).join("");
    d.quality.value = cfg.quality;
    d.key.value = "";
    d.key.type = "password";
    d.key.classList.toggle("saved", cfg.hasKey);
    d.key.placeholder = cfg.hasKey
      ? "Saved ✓ — paste a new key to replace it"
      : `Paste your ${id === "youtube" ? "YouTube" : "Twitch"} stream key`;
  }
  framing = s.framing;
  for (const x of $("framing").querySelectorAll("button")) x.classList.toggle("on", x.dataset.v === framing);
  updateUpload();
}

function updateUpload() {
  let kbps = 0;
  for (const d of Object.values(dests)) {
    if (d.enabled.checked) kbps += Number(d.quality.selectedOptions[0]?.dataset.kbps || 0) + 160;
  }
  // Leave ~30% headroom so the stream never stutters on a busy connection.
  $("uploadNeed").textContent = kbps ? `${Math.ceil((kbps * 1.3) / 1000)} Mbps` : "—";
}

function collect() {
  const out = { framing };
  for (const [id, d] of Object.entries(dests)) {
    out[id] = { enabled: d.enabled.checked, quality: d.quality.value };
    if (d.key.value.trim()) out[id].key = d.key.value.trim();
  }
  return out;
}

async function saveLive() {
  const s = await invoke("save_live_settings", { settings: collect() });
  fillModal(s);
  return s;
}

function openModal() {
  invoke("get_live_settings").then(fillModal);
  modal.classList.remove("hidden");
}
function closeModal() {
  modal.classList.add("hidden");
  for (const d of Object.values(dests)) d.key.value = ""; // never keep keys in the page
}
modal.addEventListener("click", (e) => {
  if (e.target === modal || e.target.closest("[data-close]")) closeModal();
});
document.addEventListener("keydown", (e) => {
  if (e.key === "Escape" && !modal.classList.contains("hidden")) closeModal();
});

$("liveSave").onclick = async () => {
  try {
    await saveLive();
    toast("Saved");
  } catch (e) {
    toast(String(e));
  }
};
$("liveStart").onclick = async () => {
  const btn = $("liveStart");
  btn.disabled = true;
  try {
    await saveLive();
    await invoke("start_live");
    closeModal();
    toast("Going live…");
  } catch (e) {
    toast(String(e));
  } finally {
    btn.disabled = false;
  }
};
$("liveBtn").onclick = async () => {
  if (isLive) {
    await invoke("stop_live");
    toast("Stream ended");
  } else {
    openModal();
  }
};

function drawSpark() {
  const c = $("spark");
  const w = (c.width = c.clientWidth * devicePixelRatio);
  const h = (c.height = 44 * devicePixelRatio);
  const g = c.getContext("2d");
  g.clearRect(0, 0, w, h);
  if (fpsHistory.length < 2) return;
  const x = (i) => (i / 59) * w;
  const y = (v) => h - (Math.min(v, 66) / 66) * (h - 4) - 2;
  // 60 fps guide line
  g.strokeStyle = "rgba(255,255,255,.08)";
  g.setLineDash([4 * devicePixelRatio, 4 * devicePixelRatio]);
  g.beginPath(); g.moveTo(0, y(60)); g.lineTo(w, y(60)); g.stroke();
  g.setLineDash([]);
  const off = 60 - fpsHistory.length;
  const grad = g.createLinearGradient(0, 0, 0, h);
  grad.addColorStop(0, "rgba(139,108,255,.45)");
  grad.addColorStop(1, "rgba(139,108,255,0)");
  g.beginPath();
  fpsHistory.forEach((v, i) => (i ? g.lineTo(x(i + off), y(v)) : g.moveTo(x(i + off), y(v))));
  g.lineWidth = 2 * devicePixelRatio;
  g.strokeStyle = "#8b6cff";
  g.stroke();
  g.lineTo(x(59), h); g.lineTo(x(off), h); g.closePath();
  g.fillStyle = grad;
  g.fill();
}

function fmtTime(sec) {
  const h = Math.floor(sec / 3600), m = Math.floor((sec % 3600) / 60), s = sec % 60;
  const mm = h ? String(m).padStart(2, "0") : m;
  return `${h ? h + ":" : ""}${mm}:${String(s).padStart(2, "0")}`;
}

let toastTimer;
function toast(msg) {
  const t = $("toast");
  t.textContent = msg;
  t.classList.add("show");
  clearTimeout(toastTimer);
  toastTimer = setTimeout(() => t.classList.remove("show"), 3500);
}

// ---------------- audio mixer ----------------

const strips = {};
for (const el of document.querySelectorAll(".strip")) {
  const src = el.dataset.src;
  const slider = el.querySelector("input[type=range]");
  const out = el.querySelector("output");
  const st = (strips[src] = {
    el, slider, out,
    fill: el.querySelector(".fill"),
    peakEl: el.querySelector(".peak"),
    status: el.querySelector(".status"),
    level: 0, target: 0, peak: 0, peakHold: 0,
    muted: false, touchedAt: 0,
  });
  const paint = () => {
    slider.style.setProperty("--pct", `${slider.value / 2}%`);
    out.textContent = `${slider.value}%`;
  };
  paint();
  slider.addEventListener("input", () => {
    st.touchedAt = performance.now();
    paint();
    invoke("set_audio_channel", { source: src, gain: slider.value / 100 });
  });
  slider.addEventListener("dblclick", () => {
    slider.value = 100;
    slider.dispatchEvent(new Event("input"));
  });
  el.querySelector(".mute").addEventListener("click", () => {
    st.muted = !st.muted;
    st.touchedAt = performance.now();
    el.classList.toggle("muted", st.muted);
    invoke("set_audio_channel", { source: src, muted: st.muted });
  });
}

let micListKey = "";
let voiceListKey = "";
let optionTouchedAt = 0;

function renderMixer(m) {
  const now = performance.now();
  for (const src of ["game", "discord", "mic"]) {
    const st = strips[src];
    const c = m[src];
    if (!c) continue;
    st.status.textContent = c.status;
    st.el.classList.toggle("inactive", !c.active);
    // Don't fight the user while they're adjusting.
    if (now - st.touchedAt > 1500) {
      const v = Math.round(c.gain * 100);
      if (Number(st.slider.value) !== v) {
        st.slider.value = v;
        st.slider.dispatchEvent(new Event("paint"));
        st.slider.style.setProperty("--pct", `${v / 2}%`);
        st.out.textContent = `${v}%`;
      }
      st.muted = c.muted;
      st.el.classList.toggle("muted", c.muted);
    }
  }

  const key = JSON.stringify(m.micDevices);
  const sel = $("micSelect");
  if (key !== micListKey) {
    micListKey = key;
    sel.innerHTML = '<option value="">Windows default mic</option>' +
      m.micDevices.map((d) => `<option value="${d.id.replace(/"/g, "&quot;")}">${escapeHtml(d.name)}</option>`).join("");
  }
  if (document.activeElement !== sel) sel.value = m.micDevice ?? "";

  // Where Discord runs: the app or a browser tab.
  const vkey = JSON.stringify(m.voiceApps);
  const vsel = $("voiceSelect");
  if (m.voiceApps && vkey !== voiceListKey) {
    voiceListKey = vkey;
    vsel.innerHTML = '<option value="auto">Auto · Discord app, else browser</option>' +
      m.voiceApps.map((a) => `<option value="${a.id}">${a.id === "discord" ? a.label : "Discord in " + a.label}${a.running ? "  ●" : ""}</option>`).join("");
  }
  if (document.activeElement !== vsel && m.voiceApp) vsel.value = m.voiceApp;

  if (now - optionTouchedAt > 1500) {
    $("nsToggle").checked = m.noiseSuppression;
    $("duckToggle").checked = m.ducking;
  }
}

function escapeHtml(s) {
  return s.replace(/[&<>"]/g, (c) => ({ "&": "&amp;", "<": "&lt;", ">": "&gt;", '"': "&quot;" })[c]);
}

$("micSelect").addEventListener("change", (e) => {
  invoke("set_mic_device", { id: e.target.value || null });
  toast(`Mic: ${e.target.selectedOptions[0].textContent}`);
});
$("voiceSelect").addEventListener("change", (e) => {
  invoke("set_voice_app", { id: e.target.value });
  const browser = !["auto", "discord"].includes(e.target.value);
  toast(browser
    ? "Capturing the whole browser — mute other tabs (YouTube, music) while streaming"
    : `Discord source: ${e.target.selectedOptions[0].textContent.replace("  ●", "")}`);
});
$("nsToggle").addEventListener("change", (e) => {
  optionTouchedAt = performance.now();
  invoke("set_audio_option", { name: "noiseSuppression", on: e.target.checked });
});
$("duckToggle").addEventListener("change", (e) => {
  optionTouchedAt = performance.now();
  invoke("set_audio_option", { name: "ducking", on: e.target.checked });
});

// Meters: levels arrive 20×/s; animate them at full display rate with a
// fast attack, smooth fall and a 1.2 s peak-hold marker.
function onLevels(l) {
  for (const src of ["game", "discord", "mic"]) {
    const st = strips[src];
    const pct = Math.max(0, Math.min(100, ((l[src] + 60) / 60) * 100));
    st.target = Math.max(st.target, pct);
    st.el.classList.toggle("talking", src !== "game" && l[src] > -35 && !st.muted);
  }
}

let lastMeter = performance.now();
function animateMeters(now) {
  const dt = Math.min(100, now - lastMeter);
  lastMeter = now;
  for (const st of Object.values(strips)) {
    st.level = st.target > st.level ? st.target : Math.max(st.target, st.level - dt * 0.09);
    st.target = Math.max(0, st.target - dt * 0.12);
    if (st.level >= st.peak) {
      st.peak = st.level;
      st.peakHold = now + 1200;
    } else if (now > st.peakHold) {
      st.peak = Math.max(st.level, st.peak - dt * 0.05);
    }
    st.fill.style.width = `${st.level}%`;
    st.fill.style.setProperty("--w", `${st.el.querySelector(".meter").clientWidth}px`);
    st.peakEl.style.left = `calc(${st.peak}% - 2px)`;
    st.peakEl.style.opacity = st.peak > 1 ? 0.85 : 0;
  }
  requestAnimationFrame(animateMeters);
}
requestAnimationFrame(animateMeters);

// ---------------- controls ----------------

$("recBtn").onclick = async () => {
  try {
    if (recording) {
      const path = await invoke("stop_recording");
      toast(`Saved · ${path.split("\\").pop()}`);
    } else {
      await invoke("start_recording");
      toast("Recording started");
    }
  } catch (e) {
    toast(String(e));
  }
};
$("folderBtn").onclick = () => invoke("open_recordings");
$("keyBtn").onclick = () => invoke("request_keyframe");
$("logBtn").onclick = () => invoke("open_logs");

const br = $("bitrate");
const paintBr = () => {
  $("brValue").textContent = `${br.value} Mbps`;
  br.style.setProperty("--pct", `${((br.value - br.min) / (br.max - br.min)) * 100}%`);
};
paintBr();
br.oninput = paintBr;
br.onchange = async () => {
  const ok = await invoke("set_bitrate", { mbps: Number(br.value) });
  toast(ok ? `Phone bitrate set to ${br.value} Mbps` : "Phone not connected");
};

(async () => {
  const info = await invoke("get_info");
  const wifi = info.lanIp ? info.lanIp : "PC not on a network";
  $("wifiAddr").textContent = wifi;
  $("lan").textContent = wifi;
  connectPreview(info.previewPort);
  listen("status", (e) => render(e.payload));
  listen("levels", (e) => onLevels(e.payload));
})();
