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
// "This PC" captures a whole screen (straight into the encoder) or one
// window (through the GPU compositor), with an optional webcam and image /
// text overlays. Positions are fractions of the canvas so they work at any
// output size; the boxes over the preview let you drag and resize them.

let sourceTouchedAt = 0;
let sourcesKey = "";
let sourcesData = { monitors: [], windows: [], webcams: [] };
// Current settings as the UI knows them (overlays + webcam position live here
// between status updates so dragging never fights the backend).
let srcState = { source: { kind: "phone" }, webcam: {}, overlays: [] };
let compositorStatus = null;
const srcUI = {
  seg: $("sourceSeg"), pc: $("pcOptions"), capture: $("captureSelect"), refresh: $("refreshWindows"),
  err: $("sourceError"), hint: $("sourceHint"), camToggle: $("camToggle"),
  camOpts: $("camOptions"), cam: $("camSelect"), size: $("camSize"), corner: $("camCorner"), mirror: $("camMirror"),
  list: $("overlayList"), addImage: $("addImage"), addText: $("addText"), layer: $("overlayLayer"),
};

function captureValue(src) {
  if (src.kind === "window") return `window:${src.hwnd}`;
  if (src.kind === "screen") return `screen:${src.output}`;
  return "";
}

function currentSource() {
  const pc = srcUI.seg.querySelector("button.on")?.dataset.v === "screen";
  let source = { kind: "phone" };
  if (pc) {
    const [kind, id] = (srcUI.capture.value || "screen:0").split(":");
    if (kind === "window") {
      const w = sourcesData.windows.find((x) => String(x.hwnd) === id);
      const old = srcState.source.kind === "window" ? srcState.source : {};
      source = { kind: "window", hwnd: Number(id), title: w?.title ?? old.title ?? "", process: w?.process ?? old.process ?? "" };
    } else {
      source = { kind: "screen", output: Number(id || 0) };
    }
  }
  const cam = srcState.webcam || {};
  return {
    source,
    webcam: {
      enabled: srcUI.camToggle.checked,
      device: srcUI.cam.value || "",
      size: Number(srcUI.size.value),
      corner: srcUI.corner.value,
      mirror: srcUI.mirror.checked,
      x: srcUI.corner.value === "free" ? cam.x ?? 0.7 : null,
      y: srcUI.corner.value === "free" ? cam.y ?? 0.7 : null,
    },
    overlays: srcState.overlays,
  };
}

let applyTimer;
function applySource(immediate) {
  sourceTouchedAt = performance.now();
  const pc = srcUI.seg.querySelector("button.on")?.dataset.v === "screen";
  srcUI.pc.classList.toggle("hidden", !pc);
  srcUI.camOpts.classList.toggle("hidden", !srcUI.camToggle.checked);
  const settings = currentSource();
  srcState = settings;
  renderOverlayList();
  renderOverlayBoxes();
  clearTimeout(applyTimer);
  applyTimer = setTimeout(async () => {
    try {
      await invoke("set_source", { settings });
    } catch (e) {
      toast(String(e));
    }
  }, immediate ? 0 : 250);
}

for (const b of srcUI.seg.querySelectorAll("button")) {
  b.addEventListener("click", () => {
    for (const x of srcUI.seg.querySelectorAll("button")) x.classList.toggle("on", x === b);
    applySource();
  });
}
for (const el of [srcUI.capture, srcUI.camToggle, srcUI.cam, srcUI.size, srcUI.corner, srcUI.mirror]) {
  el.addEventListener("change", () => applySource());
}
srcUI.refresh.onclick = async () => {
  try {
    fillSources(await invoke("get_sources"));
    toast("Window list refreshed");
  } catch (e) {
    toast(String(e));
  }
};

function fillSources(sources) {
  if (!sources) return;
  const key = JSON.stringify(sources);
  if (key === sourcesKey) return;
  sourcesKey = key;
  sourcesData = { monitors: sources.monitors || [], windows: sources.windows || [], webcams: sources.webcams || [] };
  const want = srcUI.capture.value || captureValue(srcState.source);
  const screens = sourcesData.monitors.map((m) => `<option value="screen:${m.output}">${escapeHtml(m.name)}</option>`).join("")
    || '<option value="screen:0">No monitor found</option>';
  const windows = sourcesData.windows.map((w) =>
    `<option value="window:${w.hwnd}">${escapeHtml(w.title.length > 48 ? w.title.slice(0, 47) + "…" : w.title)}${w.process ? ` · ${escapeHtml(w.process)}` : ""}</option>`).join("");
  srcUI.capture.innerHTML = `<optgroup label="Screens">${screens}</optgroup>` +
    `<optgroup label="Windows (games, emulators, apps)">${windows || '<option disabled>No windows found</option>'}</optgroup>`;
  selectCapture(want);
  srcUI.cam.innerHTML = sourcesData.webcams.map((w) => `<option value="${escapeHtml(w)}">${escapeHtml(w)}</option>`).join("")
    || '<option value="">No camera found</option>';
}

// Selects a capture entry; a saved window that is not open right now gets a
// placeholder entry so the choice is not silently lost.
function selectCapture(value) {
  if (!value) return;
  if (![...srcUI.capture.options].some((o) => o.value === value)) {
    const src = srcState.source;
    if (value.startsWith("window:") && src.kind === "window") {
      const o = document.createElement("option");
      o.value = value;
      o.textContent = `${src.title || src.process || "Window"} (not open)`;
      srcUI.capture.querySelector("optgroup:last-child").appendChild(o);
    } else {
      return;
    }
  }
  srcUI.capture.value = value;
}

function renderSource(s) {
  if (!s.source) return;
  fillSources(s.sources);
  compositorStatus = s.compositor || null;
  const pc = s.source.source.kind !== "phone";
  // Errors and the window-closed state are shown even while the user is editing.
  const closed = !!compositorStatus?.closed;
  const err = s.sourceError || (closed ? "The window was closed. Pick it again or open the app." : null);
  srcUI.err.textContent = err || "";
  srcUI.err.classList.toggle("hidden", !err);
  srcUI.layer.classList.toggle("hidden", !pc);
  renderOverlayBoxes();
  if (performance.now() - sourceTouchedAt < 1500) return;

  srcState = { source: s.source.source, webcam: s.source.webcam || {}, overlays: s.source.overlays || [] };
  for (const x of srcUI.seg.querySelectorAll("button")) x.classList.toggle("on", (x.dataset.v === "screen") === pc);
  srcUI.pc.classList.toggle("hidden", !pc);
  if (pc) selectCapture(captureValue(s.source.source));
  const w = srcState.webcam;
  srcUI.camToggle.checked = !!w.enabled;
  srcUI.camOpts.classList.toggle("hidden", !w.enabled);
  if (w.device) srcUI.cam.value = w.device;
  srcUI.size.value = String(w.size ?? 0.25);
  srcUI.corner.value = w.corner || "br";
  srcUI.mirror.checked = !!w.mirror;
  renderOverlayList();

  // Wording changes with the source.
  const isWindow = s.source.source.kind === "window";
  srcUI.hint.textContent = isWindow
    ? "Captures just this window, even with other windows in front. Games: use borderless or windowed mode. Exclusive fullscreen and some anti-cheat games show black."
    : srcState.overlays.length
      ? "Captures the whole screen and draws the overlays on the GPU."
      : "Captures the whole screen straight on the GPU. For PC games use borderless/windowed fullscreen; exclusive fullscreen may show black.";
  $("empty").querySelector("h2").textContent = pc ? "Capturing this PC" : "Connect your phone";
  const game = strips.game?.el.querySelector(".names b");
  if (game) game.textContent = pc ? "PC sound" : "Game";
}

// ---------------- overlays: list in the panel ----------------

const IMG_SIZES = [["0.12", "Small"], ["0.2", "Medium"], ["0.35", "Large"], ["0.6", "Huge"]];
const TXT_SIZES = [["0.04", "Small"], ["0.06", "Medium"], ["0.09", "Large"], ["0.14", "Huge"]];

function nearest(options, v) {
  let best = options[0][0];
  for (const [o] of options) if (Math.abs(Number(o) - v) < Math.abs(Number(best) - v)) best = o;
  return best;
}

function renderOverlayList() {
  const list = srcState.overlays || [];
  srcUI.list.innerHTML = list.map((o) => {
    const isText = o.kind === "text";
    const sizes = isText ? TXT_SIZES : IMG_SIZES;
    const cur = nearest(sizes, isText ? o.size : o.w);
    const name = isText
      ? `<input class="ov-text" type="text" value="${escapeHtml(o.text || "")}" placeholder="Your text" spellcheck="false">`
      : `<span class="ov-name" title="${escapeHtml(o.path)}">${escapeHtml(o.path.split(/[\\/]/).pop())}</span>`;
    return `<div class="ov-row" data-id="${escapeHtml(o.id)}">
      <span class="ov-icon">${isText ? "T" : "🖼"}</span>
      ${name}
      <select class="ov-size" title="Size">${sizes.map(([v, l]) => `<option value="${v}"${v === cur ? " selected" : ""}>${l}</option>`).join("")}</select>
      ${isText ? `<input class="ov-color" type="color" value="${escapeHtml(o.color || "#ffffff")}" title="Colour">` : ""}
      <button class="mini ov-del" title="Remove">✕</button>
    </div>`;
  }).join("");
  for (const row of srcUI.list.querySelectorAll(".ov-row")) {
    const o = list.find((x) => x.id === row.dataset.id);
    row.querySelector(".ov-del").onclick = () => {
      srcState.overlays = list.filter((x) => x.id !== o.id);
      applySource(true);
    };
    row.querySelector(".ov-size").onchange = (e) => {
      if (o.kind === "text") o.size = Number(e.target.value);
      else o.w = Number(e.target.value);
      applySource();
    };
    row.querySelector(".ov-text")?.addEventListener("change", (e) => {
      o.text = e.target.value;
      applySource();
    });
    row.querySelector(".ov-color")?.addEventListener("input", (e) => {
      o.color = e.target.value;
      applySource();
    });
  }
}

const newId = () => Math.random().toString(36).slice(2, 10);

srcUI.addImage.onclick = async () => {
  const path = await invoke("pick_image");
  if (!path) return;
  srcState.overlays = [...(srcState.overlays || []), { id: newId(), kind: "image", path, x: 0.76, y: 0.04, w: 0.2, opacity: 1 }];
  applySource(true);
};
srcUI.addText.onclick = () => {
  srcState.overlays = [...(srcState.overlays || []), { id: newId(), kind: "text", text: "Your text here", size: 0.06, color: "#ffffff", x: 0.04, y: 0.04, w: 0, opacity: 1 }];
  applySource(true);
  setTimeout(() => srcUI.list.querySelector(".ov-row:last-child .ov-text")?.select(), 50);
};

// ---------------- overlays: drag and resize over the preview ----------------
// Each layer gets a box placed with the same fractions the compositor uses.
// The box aspect comes from the compositor's status (texture sizes).

let boxesSig = "";
let dragging = null; // { id, kind, mode: "move"|"resize", startX, startY, orig }

function canvasRect() {
  // The preview canvas is centred inside #screen; map to #screen coordinates.
  const sr = $("screen").getBoundingClientRect();
  const cr = canvas.getBoundingClientRect();
  return { left: cr.left - sr.left, top: cr.top - sr.top, width: cr.width, height: cr.height };
}

function layerBoxes() {
  const boxes = [];
  const sizes = compositorStatus?.layers || {};
  const cam = srcState.webcam || {};
  if (cam.enabled) {
    // Same maths as compositor::layout::webcam_rect on a 16:9 canvas:
    // a 16:9 picture whose width is `size` of the canvas is also `size` of
    // its height; the margin is 1/60 of the canvas width.
    const w = Math.min(0.6, Math.max(0.1, cam.size ?? 0.25));
    const h = w;
    const mx = 1 / 60;
    const my = mx * (16 / 9);
    const corner = cam.corner === "free" && cam.x != null ? "free" : cam.corner || "br";
    const pos = {
      free: [cam.x, cam.y], tl: [mx, my], tr: [1 - w - mx, my], bl: [mx, 1 - h - my],
    }[corner] || [1 - w - mx, 1 - h - my];
    boxes.push({ id: "__webcam", kind: "webcam", label: "Webcam", x: pos[0], y: pos[1], w, h });
  }
  for (const o of srcState.overlays || []) {
    const nat = sizes[o.id]; // [w, h] in canvas pixels
    const cw = compositorStatus?.canvas?.width || 1920;
    const ch = compositorStatus?.canvas?.height || 1080;
    let w, h;
    if (o.kind === "text") {
      w = nat ? nat[0] / cw : 0.25;
      h = nat ? nat[1] / ch : o.size * 1.5;
    } else {
      w = o.w;
      h = nat ? (o.w * cw * nat[1] / nat[0]) / ch : (o.w * 16) / 9;
    }
    boxes.push({ id: o.id, kind: o.kind, label: o.kind === "text" ? "Text" : "Image", x: o.x, y: o.y, w, h });
  }
  return boxes;
}

function renderOverlayBoxes() {
  const layer = srcUI.layer;
  if (layer.classList.contains("hidden") || dragging) return;
  const r = canvasRect();
  if (r.width < 10) {
    layer.innerHTML = "";
    return;
  }
  Object.assign(layer.style, { left: `${r.left}px`, top: `${r.top}px`, width: `${r.width}px`, height: `${r.height}px` });
  const boxes = layerBoxes();
  const sig = JSON.stringify(boxes);
  if (sig === boxesSig && layer.childElementCount === boxes.length) return;
  boxesSig = sig;
  layer.innerHTML = boxes.map((b) => `<div class="ov-box" data-id="${escapeHtml(b.id)}" data-kind="${b.kind}" style="left:${b.x * 100}%;top:${b.y * 100}%;width:${b.w * 100}%;height:${b.h * 100}%"><span class="tag">${b.label}</span><span class="rs" title="Resize"></span></div>`).join("");
  for (const el of layer.querySelectorAll(".ov-box")) {
    el.addEventListener("pointerdown", startDrag);
  }
}

function startDrag(e) {
  const el = e.currentTarget;
  const r = canvasRect();
  const id = el.dataset.id;
  const mode = e.target.classList.contains("rs") ? "resize" : "move";
  const box = layerBoxes().find((b) => b.id === id);
  if (!box) return;
  dragging = { id, kind: el.dataset.kind, mode, startX: e.clientX, startY: e.clientY, orig: box, el, r };
  el.setPointerCapture(e.pointerId);
  el.classList.add("active");
  e.preventDefault();
  el.addEventListener("pointermove", moveDrag);
  el.addEventListener("pointerup", endDrag);
  el.addEventListener("pointercancel", endDrag);
}

function dragResult(e) {
  const d = dragging;
  const dx = (e.clientX - d.startX) / d.r.width;
  const dy = (e.clientY - d.startY) / d.r.height;
  if (d.mode === "move") {
    return {
      x: Math.min(1 - d.orig.w, Math.max(0, d.orig.x + dx)),
      y: Math.min(1 - d.orig.h, Math.max(0, d.orig.y + dy)),
      w: d.orig.w, h: d.orig.h, scale: 1,
    };
  }
  // Resize from the bottom-right corner, keeping the aspect ratio.
  const scale = Math.max(0.15, (d.orig.w + dx) / d.orig.w);
  const w = Math.min(1 - d.orig.x, d.orig.w * scale);
  const s = w / d.orig.w;
  return { x: d.orig.x, y: d.orig.y, w, h: d.orig.h * s, scale: s };
}

function moveDrag(e) {
  if (!dragging) return;
  const p = dragResult(e);
  Object.assign(dragging.el.style, { left: `${p.x * 100}%`, top: `${p.y * 100}%`, width: `${p.w * 100}%`, height: `${p.h * 100}%` });
}

function endDrag(e) {
  const d = dragging;
  if (!d) return;
  const p = dragResult(e);
  dragging = null;
  d.el.classList.remove("active");
  if (d.kind === "webcam") {
    const cam = srcState.webcam || {};
    if (d.mode === "move") {
      cam.corner = "free";
      cam.x = p.x;
      cam.y = p.y;
      srcUI.corner.value = "free";
    } else {
      srcUI.size.value = String(nearestAny(["0.18", "0.25", "0.33", "0.45"], Math.min(0.6, Math.max(0.1, d.orig.w * p.scale))));
    }
    srcState.webcam = cam;
  } else {
    const o = (srcState.overlays || []).find((x) => x.id === d.id);
    if (!o) return;
    if (d.mode === "move") {
      o.x = p.x;
      o.y = p.y;
    } else if (o.kind === "text") {
      o.size = Math.min(0.4, Math.max(0.02, o.size * p.scale));
    } else {
      o.w = Math.min(1, Math.max(0.02, o.w * p.scale));
    }
  }
  applySource(true);
}

function nearestAny(options, v) {
  let best = options[0];
  for (const o of options) if (Math.abs(Number(o) - v) < Math.abs(Number(best) - v)) best = o;
  return best;
}

new ResizeObserver(() => renderOverlayBoxes()).observe($("screen"));

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
