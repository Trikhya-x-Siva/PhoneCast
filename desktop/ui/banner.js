// Banner art for the strip under the game ("Game + banner" framing).
// One drawing function for the editor preview and the streamed loop:
// BannerArt.draw(ctx, W, H, spec, t) with t = 0..1 the position in the
// loop. Every movement repeats exactly once (or a whole number of times)
// per loop, so the streamed loop never jumps.
(() => {
  const TAU = Math.PI * 2;
  const LOOP_SECONDS = 2;
  const FPS = 30;

  // Small repeatable random numbers (same sparks/bolts every loop).
  function rand(seed) {
    const x = Math.sin(seed * 127.1 + 311.7) * 43758.5453;
    return x - Math.floor(x);
  }

  function boltPath(g, cx, cy, u) {
    g.beginPath();
    g.moveTo(cx + 0.6 * u, cy - 4 * u);
    g.lineTo(cx - 2.4 * u, cy + 0.6 * u);
    g.lineTo(cx - 0.2 * u, cy + 0.6 * u);
    g.lineTo(cx - 1.0 * u, cy + 4 * u);
    g.lineTo(cx + 2.4 * u, cy - 0.9 * u);
    g.lineTo(cx + 0.2 * u, cy - 0.9 * u);
    g.closePath();
  }

  // Draws a picture into a d x d square without squashing it: a centred
  // square crop, nudged towards the top for portraits (where the face is).
  function drawSquare(g, img, x, y, d) {
    const w = img.naturalWidth || img.width, h = img.naturalHeight || img.height;
    const sz = Math.min(w, h);
    const sx = (w - sz) / 2;
    const sy = h > w ? (h - sz) * 0.18 : (h - sz) / 2;
    g.drawImage(img, sx, sy, sz, sz, x, y, d, d);
  }

  // Jagged electric arc between two points.
  function arc(g, x1, y1, x2, y2, seed, jag) {
    g.beginPath();
    g.moveTo(x1, y1);
    const n = 6;
    for (let i = 1; i < n; i++) {
      const f = i / n;
      g.lineTo(x1 + (x2 - x1) * f + (rand(seed + i) - 0.5) * jag, y1 + (y2 - y1) * f + (rand(seed + i * 7) - 0.5) * jag);
    }
    g.lineTo(x2, y2);
    g.stroke();
  }

  // The moving avatar: floating picture (or lightning emblem) in a spinning
  // gradient energy ring, with a pulsing glow, orbiting sparks and arcs.
  function avatar(g, cx, cy, d, s, t) {
    const c1 = s.color, c2 = s.color2;
    const r = d / 2;
    const float = Math.sin(t * TAU) * d * 0.035;
    const pulse = 0.5 + 0.5 * Math.sin(t * TAU * 2);
    const y = cy + float;

    // Glow behind the ring.
    const glow = g.createRadialGradient(cx, y, r * 0.6, cx, y, r * 1.55);
    glow.addColorStop(0, c2 + Math.round(70 + 60 * pulse).toString(16).padStart(2, "0"));
    glow.addColorStop(1, c1 + "00");
    g.fillStyle = glow;
    g.beginPath();
    g.arc(cx, y, r * 1.55, 0, TAU);
    g.fill();

    // Picture.
    g.save();
    g.beginPath();
    g.arc(cx, y, r, 0, TAU);
    g.closePath();
    if (s.logo) {
      g.clip();
      // Alive: a slow breathe, a little sway and a bob, all once per loop.
      const z = 1.06 + 0.035 * Math.sin(t * TAU);
      g.translate(cx, y + Math.sin(t * TAU * 2) * d * 0.012);
      g.rotate(Math.sin(t * TAU) * 0.045);
      drawSquare(g, s.logo, -r * z, -r * z, d * z);
    } else {
      g.fillStyle = "#0e1430";
      g.fill();
      g.clip();
      const bolt = g.createLinearGradient(cx - d / 3, y - r, cx + d / 3, y + r);
      bolt.addColorStop(0, c2);
      bolt.addColorStop(1, c1);
      g.fillStyle = bolt;
      g.shadowColor = c2;
      g.shadowBlur = d * (0.1 + 0.15 * pulse);
      boltPath(g, cx, y, d / 10);
      g.fill();
    }
    g.restore();

    // Spinning ring (conic gradient turns once per loop).
    const ring = g.createConicGradient(t * TAU, cx, y);
    ring.addColorStop(0, c1);
    ring.addColorStop(0.35, c2);
    ring.addColorStop(0.5, "#ffffff");
    ring.addColorStop(0.65, c2);
    ring.addColorStop(1, c1);
    g.lineWidth = Math.max(4, d * 0.055);
    g.strokeStyle = ring;
    g.shadowColor = c2;
    g.shadowBlur = d * (0.08 + 0.1 * pulse);
    g.beginPath();
    g.arc(cx, y, r, 0, TAU);
    g.stroke();
    g.shadowBlur = 0;

    // Orbiting sparks.
    for (let i = 0; i < 7; i++) {
      const a = (i / 7) * TAU + t * TAU * (i % 2 ? 1 : -1);
      const rr = r * (1.18 + 0.08 * Math.sin(t * TAU * 2 + i));
      const sx = cx + Math.cos(a) * rr, sy = y + Math.sin(a) * rr;
      const flick = 0.35 + 0.65 * Math.abs(Math.sin(t * TAU * 3 + i * 1.7));
      g.fillStyle = (i % 2 ? c2 : "#ffffff") + Math.round(flick * 255).toString(16).padStart(2, "0");
      g.beginPath();
      g.arc(sx, sy, d * 0.022 * (0.6 + flick), 0, TAU);
      g.fill();
    }

    // Two short electric arcs that flash at fixed moments of the loop.
    g.lineWidth = Math.max(2, d * 0.018);
    g.lineCap = "round";
    for (const [at, a0] of [[0.18, -0.6], [0.66, 2.4]]) {
      const k = Math.max(0, 1 - Math.abs(t - at) / 0.07);
      if (k <= 0) continue;
      g.strokeStyle = `rgba(255,255,255,${0.9 * k})`;
      g.shadowColor = c2;
      g.shadowBlur = d * 0.12;
      const x1 = cx + Math.cos(a0) * r, y1 = y + Math.sin(a0) * r;
      const x2 = cx + Math.cos(a0) * r * 1.6, y2 = y + Math.sin(a0) * r * 1.6;
      arc(g, x1, y1, x2, y2, Math.round(at * 100), d * 0.12);
      g.shadowBlur = 0;
    }
  }

  // Text with a light sweep moving across it once per loop.
  function shinyText(g, text, x, y, font, fill, glowColor, t, H) {
    g.font = font;
    const w = Math.ceil(g.measureText(text).width) + 40;
    const h = Math.ceil(H);
    const off = document.createElement("canvas");
    off.width = w;
    off.height = h;
    const o = off.getContext("2d");
    o.font = font;
    o.textBaseline = "alphabetic";
    const fg = o.createLinearGradient(0, 0, w, 0);
    fill.forEach(([p, c]) => fg.addColorStop(p, c));
    o.fillStyle = fg;
    o.fillText(text, 10, y);
    // Sweep: a soft white band crossing left to right.
    const sx = -w * 0.4 + (w * 1.8) * t;
    o.globalCompositeOperation = "source-atop";
    const sw = o.createLinearGradient(sx - w * 0.15, 0, sx + w * 0.15, 0);
    sw.addColorStop(0, "rgba(255,255,255,0)");
    sw.addColorStop(0.5, "rgba(255,255,255,0.85)");
    sw.addColorStop(1, "rgba(255,255,255,0)");
    o.fillStyle = sw;
    o.fillRect(0, 0, w, h);
    g.save();
    g.shadowColor = glowColor;
    g.shadowBlur = H * 0.12;
    g.drawImage(off, x - 10, 0);
    g.restore();
  }

  function drawBlitz(g, W, H, s, t) {
    const c1 = s.color, c2 = s.color2;
    const bg = g.createLinearGradient(0, 0, W, 0);
    bg.addColorStop(0, "#070a14");
    bg.addColorStop(0.55, "#0b1022");
    bg.addColorStop(1, "#120a24");
    g.fillStyle = bg;
    g.fillRect(0, 0, W, H);
    for (const [x, col] of [[W * 0.12, c1], [W * 0.88, c2]]) {
      const r = g.createRadialGradient(x, H * 0.6, 0, x, H * 0.6, W * 0.32);
      r.addColorStop(0, col + "44");
      r.addColorStop(1, col + "00");
      g.fillStyle = r;
      g.fillRect(0, 0, W, H);
    }
    // Speed streaks drifting right: one spacing per loop = seamless.
    const sp = 74;
    g.save();
    g.globalAlpha = 0.08;
    g.strokeStyle = "#ffffff";
    for (let i = -2; i < W / sp + 2; i++) {
      const x = i * sp + t * sp;
      if (x < W * 0.25) continue;
      g.lineWidth = 1 + (((i % 3) + 3) % 3);
      g.beginPath();
      g.moveTo(x, H);
      g.lineTo(x + H * 0.6, 0);
      g.stroke();
    }
    g.restore();
    // Top edge: gradient line with a bright spot running along it.
    const edge = g.createLinearGradient(0, 0, W, 0);
    edge.addColorStop(0, c1);
    edge.addColorStop(1, c2);
    g.fillStyle = edge;
    g.fillRect(0, 0, W, 5);
    const run = g.createRadialGradient(W * t, 2, 0, W * t, 2, W * 0.08);
    run.addColorStop(0, "rgba(255,255,255,0.9)");
    run.addColorStop(1, "rgba(255,255,255,0)");
    g.fillStyle = run;
    g.fillRect(0, 0, W, 6);

    const pad = Math.round(H * 0.12);
    const d = H - pad * 2;
    avatar(g, pad + d / 2, H / 2 + 3, d, s, t);

    const x = pad + d + pad * 1.6;
    shinyText(
      g,
      s.title || "Your channel name",
      x,
      H * 0.56,
      `italic 900 ${Math.round(H * 0.36)}px "Segoe UI Black", "Segoe UI", system-ui, sans-serif`,
      [[0, "#ffffff"], [0.55, "#e6e9ff"], [1, c2]],
      c1 + "aa",
      t,
      H,
    );
    g.font = `600 ${Math.round(H * 0.16)}px "Segoe UI", system-ui, sans-serif`;
    g.fillStyle = "#b8c0dc";
    g.fillText(s.line || "@yourhandle · Subscribe", x, H * 0.83);

    if (s.right) {
      g.font = `italic 900 ${Math.round(H * 0.2)}px "Segoe UI Black", "Segoe UI", system-ui, sans-serif`;
      const tw = g.measureText(s.right).width;
      const ph = H * 0.4, dot = ph * 0.22, pw = tw + ph * 1.2 + dot * 2.2;
      const px = W - pad * 1.6 - pw, py = H / 2 - ph / 2 + 3, sk = ph * 0.25;
      const pill = g.createLinearGradient(px, 0, px + pw, 0);
      pill.addColorStop(0, c1);
      pill.addColorStop(1, c2);
      g.save();
      g.shadowColor = c2;
      g.shadowBlur = H * 0.08 * (0.6 + 0.4 * Math.sin(t * TAU * 2));
      g.fillStyle = pill;
      g.beginPath();
      g.moveTo(px + sk, py);
      g.lineTo(px + pw, py);
      g.lineTo(px + pw - sk, py + ph);
      g.lineTo(px, py + ph);
      g.closePath();
      g.fill();
      g.restore();
      // Live dot: blinks twice per loop.
      const on = 0.35 + 0.65 * (0.5 + 0.5 * Math.cos(t * TAU * 2));
      g.fillStyle = `rgba(255,59,92,${on})`;
      g.beginPath();
      g.arc(px + ph * 0.55, py + ph / 2, dot * (0.85 + 0.15 * on), 0, TAU);
      g.fill();
      g.fillStyle = "#ffffff";
      g.fillText(s.right, px + ph * 0.55 + dot * 2.2, py + ph * 0.7);
    }
  }

  function drawClean(g, W, H, s) {
    const bg = g.createLinearGradient(0, 0, W, H);
    bg.addColorStop(0, "#0d0f17");
    bg.addColorStop(1, "#171b28");
    g.fillStyle = bg;
    g.fillRect(0, 0, W, H);
    const glow = g.createRadialGradient(W * 0.18, H, 0, W * 0.18, H, W * 0.5);
    glow.addColorStop(0, s.color + "55");
    glow.addColorStop(1, s.color + "00");
    g.fillStyle = glow;
    g.fillRect(0, 0, W, H);
    g.fillStyle = s.color;
    g.fillRect(0, 0, W, 4);
    const pad = Math.round(H * 0.22);
    let x = pad;
    if (s.logo) {
      const d = H - pad * 2;
      g.save();
      g.beginPath();
      g.arc(x + d / 2, H / 2 + 2, d / 2, 0, TAU);
      g.clip();
      drawSquare(g, s.logo, x, H / 2 + 2 - d / 2, d);
      g.restore();
      g.strokeStyle = s.color;
      g.lineWidth = 4;
      g.beginPath();
      g.arc(x + d / 2, H / 2 + 2, d / 2, 0, TAU);
      g.stroke();
      x += d + pad * 0.8;
    }
    g.fillStyle = "#f3f4fa";
    g.font = `800 ${Math.round(H * 0.34)}px "Segoe UI", system-ui, sans-serif`;
    g.fillText(s.title || "Your channel name", x, H * 0.52);
    g.fillStyle = "#aab0c4";
    g.font = `600 ${Math.round(H * 0.18)}px "Segoe UI", system-ui, sans-serif`;
    g.fillText(s.line || "YT · IG · Discord: @yourname", x, H * 0.8);
    if (s.right) {
      g.font = `800 ${Math.round(H * 0.2)}px "Segoe UI", system-ui, sans-serif`;
      const tw = g.measureText(s.right).width;
      const ph = H * 0.36, pw = tw + ph * 1.1, px = W - pad - pw, py = H / 2 - ph / 2 + 2;
      g.fillStyle = s.color;
      g.beginPath();
      g.roundRect(px, py, pw, ph, ph / 2);
      g.fill();
      g.fillStyle = "#fff";
      g.fillText(s.right, px + (pw - tw) / 2, py + ph * 0.7);
    }
  }

  function draw(g, W, H, s, t) {
    g.save();
    g.clearRect(0, 0, W, H);
    g.textBaseline = "alphabetic";
    if (s.image) {
      const k = Math.max(W / s.image.width, H / s.image.height);
      const iw = s.image.width * k, ih = s.image.height * k;
      g.drawImage(s.image, (W - iw) / 2, (H - ih) / 2, iw, ih);
    } else if (s.style === "clean") {
      drawClean(g, W, H, s);
    } else {
      drawBlitz(g, W, H, s, t);
    }
    g.restore();
  }

  /** True when the banner moves (and is streamed as a loop). */
  function animated(s) {
    return !s.image && s.style !== "clean";
  }

  window.BannerArt = { draw, animated, LOOP_SECONDS, FPS };
})();
