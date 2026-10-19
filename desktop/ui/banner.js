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

  // Draws an image to cover (x, y, w, h), centred on (fx, fy) of the image.
  function cover(g, img, x, y, w, h, zoom = 1, fx = 0.5, fy = 0.5) {
    const iw = img.naturalWidth || img.width, ih = img.naturalHeight || img.height;
    const k = Math.max(w / iw, h / ih) * zoom;
    const dw = iw * k, dh = ih * k;
    g.drawImage(img, x + (w - dw) * fx, y + (h - dh) * fy, dw, dh);
  }

  // A character pose (transparent PNG) standing at the bottom-left, with a
  // soft glow behind it and a gentle breathing bob.
  function pose(g, img, cx, bottom, height, s, t, slide = 0, alpha = 1) {
    const iw = img.naturalWidth || img.width, ih = img.naturalHeight || img.height;
    const h = height * (1 + 0.012 * Math.sin(t * TAU));
    const w = (iw / ih) * h;
    const x = cx - w / 2 + slide, y = bottom - h + Math.sin(t * TAU) * height * 0.01;
    const glow = g.createRadialGradient(cx + slide, bottom - h * 0.45, 0, cx + slide, bottom - h * 0.45, h * 0.7);
    glow.addColorStop(0, s.color2 + "55");
    glow.addColorStop(0.5, s.color + "22");
    glow.addColorStop(1, s.color + "00");
    g.save();
    g.globalAlpha = alpha;
    g.fillStyle = glow;
    g.fillRect(x - w * 0.4, y - h * 0.2, w * 1.8, h * 1.3);
    g.shadowColor = s.color2;
    g.shadowBlur = height * 0.04;
    g.drawImage(img, x, y, w, h);
    g.restore();
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

  // "SUBSCRIBE" button with a ringing bell that slides in from the right.
  // p = 0..1 through the pop-up (in, hold, out).
  function subscribePopup(g, W, H, s, p) {
    const ease = (x) => 1 - Math.pow(1 - Math.min(1, Math.max(0, x)), 3);
    const inK = ease(p / 0.18), outK = ease((p - 0.82) / 0.18);
    const k = p < 0.82 ? inK : 1 - outK;
    if (k <= 0) return;
    const pad = Math.round(H * 0.12);
    const bh = H * 0.5, bw = H * 2.6;
    const x = W - pad * 1.6 - bw + (1 - k) * (bw + pad * 2), y = H / 2 - bh / 2 + 3;
    const swing = Math.sin(p * TAU * 6) * 0.35 * (p > 0.15 && p < 0.85 ? 1 : 0);
    subscribeButton(g, x, y, bh, swing, k);
  }

  // Red "SUBSCRIBE" button with a bell, at (x, y), bh tall.
  function subscribeButton(g, x, y, bh, swing, alpha) {
    const bw = bh * 5.2;
    g.save();
    g.globalAlpha = alpha;
    g.shadowColor = "#ff2d4b";
    g.shadowBlur = bh * 0.36;
    g.fillStyle = "#ff2d4b";
    g.beginPath();
    g.roundRect(x, y, bw, bh, bh * 0.22);
    g.fill();
    g.shadowBlur = 0;
    // Bell (swings while held).
    const bx = x + bh * 0.62, by = y + bh / 2, r = bh * 0.26;
    g.save();
    g.translate(bx, by - r * 0.9);
    g.rotate(swing);
    g.fillStyle = "#fff";
    g.beginPath();
    g.moveTo(-r, r * 1.3);
    g.quadraticCurveTo(-r, -r * 0.2, 0, -r * 0.35);
    g.quadraticCurveTo(r, -r * 0.2, r, r * 1.3);
    g.closePath();
    g.fill();
    g.beginPath();
    g.arc(0, r * 1.55, r * 0.28, 0, TAU);
    g.fill();
    g.restore();
    g.fillStyle = "#fff";
    g.font = `700 ${Math.round(bh * 0.44)}px "Space Grotesk", "Segoe UI Black", sans-serif`;
    g.fillText("SUBSCRIBE", x + bh * 1.15, y + bh * 0.66);
    g.restore();
  }

  function drawBlitz(g, W, H, s, t, popup, kind) {
    const c1 = s.color, c2 = s.color2;
    const bg = g.createLinearGradient(0, 0, W, 0);
    bg.addColorStop(0, "#070a14");
    bg.addColorStop(0.55, "#0b1022");
    bg.addColorStop(1, "#120a24");
    g.fillStyle = bg;
    g.fillRect(0, 0, W, H);
    if (s.stripBg) {
      // The channel's lightning strip art, drifting slowly, darkened on the
      // left so the name stays readable.
      cover(g, s.stripBg, 0, 0, W, H, 1.06, 0.5 + 0.03 * Math.sin(t * TAU), 0.55);
      const shade = g.createLinearGradient(0, 0, W * 0.6, 0);
      shade.addColorStop(0, "rgba(5,7,16,0.82)");
      shade.addColorStop(1, "rgba(5,7,16,0.15)");
      g.fillStyle = shade;
      g.fillRect(0, 0, W, H);
    }
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
    // With the channel's character clip, the app lays it over the strip's
    // left (as wide as 0.66 of a 16:9 frame at the strip's height).
    const clip = (s._clips || []).includes("gaming");
    if (!clip) avatar(g, pad + d / 2, H / 2 + 3, d, s, t);

    const x = clip ? Math.round(H * 1.1733) + pad * 0.6 : pad + d + pad * 1.6;
    // Name: bold display face, a dark shadow so it reads on phones, and a
    // light sweep across it once every 10 s (not constant motion).
    const sweep = (curSec % 10) / 1.4;
    shinyText(
      g,
      s.title || "Your channel name",
      x,
      H * 0.53,
      `700 ${Math.round(H * 0.38)}px ${DISPLAY}`,
      [[0, "#ffffff"], [0.6, "#eef0ff"], [1, c2]],
      "rgba(0,0,0,0.75)",
      sweep <= 1 ? sweep : 2,
      H,
    );
    // Second line: three short lines in turn, each sliding up into place,
    // instead of one long sentence.
    drawRotatingLine(g, x, H * 0.84, H, s, c1, c2);

    if (popup > 0) {
      if (!kind || kind === "subscribe") subscribePopup(g, W, H, s, popup);
      else pillPopup(g, W, H, s, popup, MOMENTS[kind](s));
    }
    if (!(popup > 0.1 && popup < 0.9)) {
      let right = W - pad * 1.6;
      // "BGMI LIVE" next to the LIVE badge would say LIVE twice.
      const label = (s.right || "").replace(/\s*\bLIVE\b\s*$/i, "").trim();
      if (label) {
        // Game label: clean slanted pill (the LIVE badge has the red dot).
        g.save();
        g.font = `700 ${Math.round(H * 0.2)}px ${DISPLAY}`;
        const tw = g.measureText(label).width;
        const ph = H * 0.4, pw = tw + ph * 0.9;
        const px = right - pw, py = H / 2 - ph / 2 + 3, sk = ph * 0.25;
        const pill = g.createLinearGradient(px, 0, px + pw, 0);
        pill.addColorStop(0, c1);
        pill.addColorStop(1, c2);
        g.shadowColor = c2;
        g.shadowBlur = H * 0.08 * (0.6 + 0.4 * Math.sin(t * TAU));
        g.fillStyle = pill;
        g.beginPath();
        g.moveTo(px + sk, py);
        g.lineTo(px + pw, py);
        g.lineTo(px + pw - sk, py + ph);
        g.lineTo(px, py + ph);
        g.closePath();
        g.fill();
        g.shadowBlur = 0;
        g.fillStyle = "#ffffff";
        g.textBaseline = "middle";
        g.fillText(label, px + ph * 0.45, py + ph / 2 + 1);
        g.restore();
        right = px - H * 0.08;
      }
      liveBadge(g, right, H / 2 + 3, H);
    }
  }

  // ---------- banner text helpers ----------
  const DISPLAY = `"Space Grotesk", "Segoe UI Black", "Segoe UI", system-ui, sans-serif`;
  const BODY = `"Inter", "Segoe UI", system-ui, sans-serif`;
  // Seconds into the 20 s banner loop (set by bannerAt / momentAt; the
  // editor preview follows the clock).
  let curSec = 0;

  /** The handle, the short tagline and the socials from the settings. */
  function lineParts(s) {
    const parts = (s.line || "").split("·").map((p) => p.trim()).filter(Boolean);
    const handle = parts.find((p) => p.startsWith("@")) || (s.title ? "@" + s.title.replace(/\s+/g, "") : "@yourchannel");
    const tagline = parts.filter((p) => p !== handle && !/subscribe/i.test(p)).join(" · ");
    return { handle, tagline };
  }

  function drawRotatingLine(g, x, y, H, s, c1, c2) {
    const { handle, tagline } = lineParts(s);
    const lines = [{ kind: "handle", text: handle }, { kind: "subscribe" }];
    if (tagline) lines.push({ kind: "text", text: tagline });
    const period = 20 / lines.length;
    const i = Math.floor(curSec / period) % lines.length;
    const local = curSec - Math.floor(curSec / period) * period;
    const ease = (v) => 1 - Math.pow(1 - Math.min(1, Math.max(0, v)), 3);
    const inK = ease(local / 0.45);
    const outK = local > period - 0.35 ? ease((local - (period - 0.35)) / 0.35) : 0;
    const size = Math.round(H * 0.17);
    g.save();
    // clip to the line's band so the slide never overlaps the name
    g.beginPath();
    g.rect(x - 4, y - size * 1.25, H * 9, size * 1.6);
    g.clip();
    g.globalAlpha = inK * (1 - outK);
    g.translate(0, (1 - inK) * size * 0.9 - outK * size * 0.9);
    g.textBaseline = "alphabetic";
    g.shadowColor = "rgba(0,0,0,0.8)";
    g.shadowBlur = H * 0.04;
    const L = lines[i];
    if (L.kind === "handle") {
      g.font = `600 ${size}px ${BODY}`;
      g.fillStyle = "#eef1ff";
      g.fillText(L.text, x, y);
      let ix = x + g.measureText(L.text).width + size * 0.6;
      const ic = size * 1.05;
      g.shadowBlur = 0;
      ytIcon(g, ix, y - ic * 0.82, ic);
      ix += ic * 1.55;
      if (s.social) {
        igIcon(g, ix, y - ic * 0.86, ic);
        ix += ic * 1.3;
      }
      if (s.discord) dcIcon(g, ix, y - ic * 0.86, ic);
    } else if (L.kind === "subscribe") {
      // Bell right next to "Subscribe" (rings as the line comes in), then Like.
      const ring = Math.sin(local * TAU * 2.5) * 0.45 * Math.exp(-local * 0.9);
      const r = size * 0.42;
      bell(g, x + r, y - size * 0.4, r, ring, "#ff3b5c");
      g.font = `700 ${size}px ${BODY}`;
      g.fillStyle = "#ffffff";
      const sx = x + r * 2 + size * 0.35;
      g.fillText("Subscribe", sx, y);
      let lx = sx + g.measureText("Subscribe").width + size * 0.9;
      g.font = `${Math.round(size * 0.95)}px "Segoe UI Emoji", sans-serif`;
      const pop = 1 + 0.18 * Math.max(0, Math.sin((local - 0.6) * TAU * 1.2)) * Math.exp(-Math.max(0, local - 0.6));
      g.save();
      g.translate(lx + size * 0.45, y - size * 0.35);
      g.scale(pop, pop);
      g.fillText("👍", -size * 0.45, size * 0.35);
      g.restore();
      lx += size * 1.25;
      g.font = `700 ${size}px ${BODY}`;
      g.fillStyle = "#ffffff";
      g.fillText("Like", lx, y);
    } else {
      g.font = `600 ${size}px ${BODY}`;
      g.fillStyle = "#d6dbf2";
      g.fillText(L.text, x, y);
    }
    g.restore();
  }

  // A bell (white or coloured), centre (cx, cy), radius r, swinging by `swing`.
  function bell(g, cx, cy, r, swing, color) {
    g.save();
    g.translate(cx, cy - r * 0.9);
    g.rotate(swing);
    g.fillStyle = color;
    g.beginPath();
    g.moveTo(-r, r * 1.3);
    g.quadraticCurveTo(-r, -r * 0.2, 0, -r * 0.35);
    g.quadraticCurveTo(r, -r * 0.2, r, r * 1.3);
    g.closePath();
    g.fill();
    g.beginPath();
    g.arc(0, r * 1.55, r * 0.28, 0, TAU);
    g.fill();
    g.restore();
  }

  function ytIcon(g, x, y, h) {
    const w = h * 1.4;
    g.save();
    g.fillStyle = "#ff0033";
    g.beginPath();
    g.roundRect(x, y, w, h, h * 0.26);
    g.fill();
    g.fillStyle = "#fff";
    g.beginPath();
    g.moveTo(x + w * 0.4, y + h * 0.28);
    g.lineTo(x + w * 0.4, y + h * 0.72);
    g.lineTo(x + w * 0.68, y + h * 0.5);
    g.closePath();
    g.fill();
    g.restore();
  }

  function igIcon(g, x, y, h) {
    g.save();
    const gr = g.createLinearGradient(x, y + h, x + h, y);
    gr.addColorStop(0, "#feda75");
    gr.addColorStop(0.35, "#fa7e1e");
    gr.addColorStop(0.65, "#d62976");
    gr.addColorStop(1, "#4f5bd5");
    g.fillStyle = gr;
    g.beginPath();
    g.roundRect(x, y, h, h, h * 0.28);
    g.fill();
    g.strokeStyle = "#fff";
    g.lineWidth = h * 0.09;
    g.beginPath();
    g.roundRect(x + h * 0.2, y + h * 0.2, h * 0.6, h * 0.6, h * 0.18);
    g.stroke();
    g.beginPath();
    g.arc(x + h * 0.5, y + h * 0.5, h * 0.15, 0, TAU);
    g.stroke();
    g.fillStyle = "#fff";
    g.beginPath();
    g.arc(x + h * 0.7, y + h * 0.3, h * 0.05, 0, TAU);
    g.fill();
    g.restore();
  }

  function dcIcon(g, x, y, h) {
    g.save();
    g.fillStyle = "#5865f2";
    g.beginPath();
    g.roundRect(x, y, h, h, h * 0.28);
    g.fill();
    // simplified controller-face: white rounded body, two blurple eyes
    g.fillStyle = "#fff";
    g.beginPath();
    g.roundRect(x + h * 0.18, y + h * 0.3, h * 0.64, h * 0.42, h * 0.18);
    g.fill();
    g.fillStyle = "#5865f2";
    g.beginPath();
    g.arc(x + h * 0.38, y + h * 0.5, h * 0.07, 0, TAU);
    g.arc(x + h * 0.62, y + h * 0.5, h * 0.07, 0, TAU);
    g.fill();
    g.restore();
  }

  // Pulsing red LIVE badge, its right edge at `right`.
  function liveBadge(g, right, cy, H) {
    g.save();
    const size = Math.round(H * 0.15);
    g.font = `700 ${size}px ${DISPLAY}`;
    const tw = g.measureText("LIVE").width;
    const bh = H * 0.3, bw = tw + bh * 1.05;
    const bx = right - bw, by = cy - bh / 2;
    const pulse = 0.5 + 0.5 * Math.cos((curSec % 1.6) / 1.6 * TAU);
    g.shadowColor = "rgba(255,45,75,0.7)";
    g.shadowBlur = H * 0.06 * (0.6 + 0.4 * pulse);
    g.fillStyle = "#e5193a";
    g.beginPath();
    g.roundRect(bx, by, bw, bh, bh * 0.22);
    g.fill();
    g.shadowBlur = 0;
    g.fillStyle = `rgba(255,255,255,${0.55 + 0.45 * pulse})`;
    g.beginPath();
    g.arc(bx + bh * 0.42, cy, bh * 0.13 * (0.85 + 0.15 * pulse), 0, TAU);
    g.fill();
    g.fillStyle = "#fff";
    g.textBaseline = "middle";
    g.fillText("LIVE", bx + bh * 0.72, cy + 1);
    g.restore();
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

  const SCENES = {
    starting: { title: "STARTING SOON", sub: "Grab your snacks · the match is about to begin" },
    brb: { title: "BE RIGHT BACK", sub: "Don't go anywhere · back in a moment" },
    thanks: { title: "THANKS FOR WATCHING", sub: "Subscribe & hit the bell for the next stream" },
  };

  // A full scene that replaces the game picture: navy, lightning glows,
  // drifting streaks, the avatar large in its energy ring, big gradient
  // title, a subtitle, channel line and a scene-specific animated detail.
  function drawScene(g, W, H, s, name, t) {
    const sc = SCENES[name] || SCENES.starting;
    const c1 = s.color, c2 = s.color2;
    const bg = g.createLinearGradient(0, 0, W, H);
    bg.addColorStop(0, "#060914");
    bg.addColorStop(0.6, "#0b1024");
    bg.addColorStop(1, "#150a28");
    g.fillStyle = bg;
    g.fillRect(0, 0, W, H);
    if (s.sceneBg) {
      // Slow push-in on the channel's lightning art (loops seamlessly).
      cover(g, s.sceneBg, 0, 0, W, H, 1.05 + 0.03 * Math.sin(t * TAU), 0.5, 0.5);
      g.fillStyle = "rgba(4,6,14,0.35)";
      g.fillRect(0, 0, W, H);
    }
    for (const [x, y, col, k] of [[0.22, 0.5, c1, 1], [0.85, 0.2, c2, 0.8], [0.75, 0.95, c1, 0.6]]) {
      const pr = 0.85 + 0.15 * Math.sin(t * TAU + x * 10);
      const r = g.createRadialGradient(W * x, H * y, 0, W * x, H * y, W * 0.45 * pr);
      r.addColorStop(0, col + Math.round(80 * k).toString(16).padStart(2, "0"));
      r.addColorStop(1, col + "00");
      g.fillStyle = r;
      g.fillRect(0, 0, W, H);
    }
    // Drifting diagonal streaks (one spacing per loop: seamless).
    const sp = 90;
    g.save();
    g.globalAlpha = 0.07;
    g.strokeStyle = "#fff";
    for (let i = -3; i < W / sp + 3; i++) {
      const x = i * sp + t * sp;
      g.lineWidth = 1 + (((i % 3) + 3) % 3);
      g.beginPath();
      g.moveTo(x, H);
      g.lineTo(x + H * 0.5, 0);
      g.stroke();
    }
    g.restore();
    // Floating particles.
    for (let i = 0; i < 40; i++) {
      const px = rand(i) * W, base = rand(i + 50) * H;
      const py = (base - t * H * 0.25 * (0.5 + rand(i + 9)) + H) % H;
      g.fillStyle = (i % 2 ? c2 : c1) + "99";
      g.beginPath();
      g.arc(px, py, 1.5 + rand(i + 3) * 3, 0, TAU);
      g.fill();
    }
    // The character: a full pose when one is set, else the avatar ring.
    const d = Math.min(H * 0.56, W * 0.3);
    const ax = W * 0.27, ay = H * 0.5 + (name === "brb" ? Math.sin(t * TAU) * H * 0.015 : 0);
    const p = name === "brb" ? s.poseShush || s.poseWave : s.poseWave || s.poseShush;
    const clip = (s._clips || []).includes(name);
    if (clip) {
      // The app lays the character clip over this (its middle at W * 0.25).
    } else if (p) pose(g, p, W * 0.24, H, H * 0.92, s, t);
    else avatar(g, ax, ay, d, s, t);
    // Title.
    const tx = clip ? W * 0.53 : W * 0.27 + d * 0.75;
    const titleSize = Math.round(Math.min(H * 0.13, (W - tx - W * 0.05) / (sc.title.length * 0.62)));
    shinyText(g, sc.title, tx, H * 0.47,
      `700 ${titleSize}px "Space Grotesk", "Segoe UI Black", sans-serif`,
      [[0, "#ffffff"], [0.5, "#e8eaff"], [1, c2]], c1 + "cc", t, H);
    g.fillStyle = "#c3c9e4";
    const small = Math.max(titleSize * 0.32, H * 0.034);
    g.font = `600 ${Math.round(small)}px "Segoe UI", system-ui, sans-serif`;
    g.fillText(sc.sub, tx, H * 0.47 + titleSize * 0.3 + small * 1.2);
    // Channel line.
    g.fillStyle = "#ffffff";
    const chY = H * 0.47 + titleSize * 0.3 + small * 2.9;
    g.font = `700 ${Math.round(small * 1.15)}px "Space Grotesk", "Segoe UI Black", sans-serif`;
    const ch = s.title || "Your channel";
    g.fillText(ch, tx, chY);
    const cw = g.measureText(ch + "  ").width;
    g.fillStyle = "#9aa3c7";
    g.font = `600 ${Math.round(small * 0.9)}px "Segoe UI", system-ui, sans-serif`;
    g.fillText(s.right || "", tx + cw, chY);
    // Scene detail.
    const lineY = chY + small * 1.1, lw = Math.min(W * 0.42, W - tx - W * 0.06);
    if (name === "starting") {
      // Loading bar that fills and restarts each loop, with a glowing head.
      g.fillStyle = "#ffffff18";
      g.beginPath();
      g.roundRect(tx, lineY, lw, H * 0.012, H * 0.006);
      g.fill();
      const f = t;
      const bar = g.createLinearGradient(tx, 0, tx + lw, 0);
      bar.addColorStop(0, c1);
      bar.addColorStop(1, c2);
      g.fillStyle = bar;
      g.beginPath();
      g.roundRect(tx, lineY, Math.max(H * 0.012, lw * f), H * 0.012, H * 0.006);
      g.fill();
      g.shadowColor = c2;
      g.shadowBlur = H * 0.04;
      g.fillStyle = "#fff";
      g.beginPath();
      g.arc(tx + lw * f, lineY + H * 0.006, H * 0.012, 0, TAU);
      g.fill();
      g.shadowBlur = 0;
    } else if (name === "brb") {
      // Three bouncing dots.
      for (let i = 0; i < 3; i++) {
        const b = Math.max(0, Math.sin(t * TAU * 2 - i * 0.7));
        g.fillStyle = i === 1 ? c2 : c1;
        g.beginPath();
        g.arc(tx + i * H * 0.05 + H * 0.02, lineY + H * 0.02 - b * H * 0.025, H * 0.014, 0, TAU);
        g.fill();
      }
    } else {
      const bh = Math.max(H * 0.075, small * 1.9);
      subscribeButton(g, tx, lineY, bh, Math.sin(t * TAU * 3) * 0.3, 1);
    }
  }

  function draw(g, W, H, s, t, popup, kind) {
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
      drawBlitz(g, W, H, s, t, popup || 0, kind);
    }
    g.restore();
  }

  /** True when the banner moves (and is streamed as a loop). */
  function animated(s) {
    return !s.image && s.style !== "clean";
  }

  // The 3 s "action" clip played on the way back to the game, then 1 s of
  // a still full-screen frame (looped until the game's next keyframe).
  // t = 0..1 over 4 s.
  const STINGER_SECONDS = 4, STINGER_HOLD_AT = 3;
  function stinger(g, W, H, s, t) {
    const sec = t * STINGER_SECONDS;
    const c1 = s.color, c2 = s.color2;
    const ease = (x) => 1 - Math.pow(1 - Math.min(1, Math.max(0, x)), 3);
    g.save();
    g.clearRect(0, 0, W, H);
    g.textBaseline = "alphabetic";
    const bg = g.createLinearGradient(0, 0, W, H);
    bg.addColorStop(0, "#05070f");
    bg.addColorStop(1, "#140a26");
    g.fillStyle = bg;
    g.fillRect(0, 0, W, H);
    if (s.sceneBg) cover(g, s.sceneBg, 0, 0, W, H, 1.15 - 0.1 * Math.min(1, sec / 3), 0.5, 0.5);
    // Lightning bolts crack across the screen in the first second.
    g.lineCap = "round";
    for (let b = 0; b < 3; b++) {
      const at = 0.1 + b * 0.25, k = Math.max(0, 1 - Math.abs(sec - at) / 0.18);
      if (k <= 0) continue;
      g.strokeStyle = `rgba(255,255,255,${0.95 * k})`;
      g.shadowColor = b % 2 ? c1 : c2;
      g.shadowBlur = H * 0.05;
      g.lineWidth = H * 0.008;
      arc(g, W * (0.1 + 0.35 * b), 0, W * (0.3 + 0.3 * b), H, 40 + b * 13, H * 0.25);
      g.shadowBlur = 0;
    }
    // White flash at the start.
    const flash = Math.max(0, 1 - sec / 0.25);
    if (flash > 0) {
      g.fillStyle = `rgba(230,250,255,${flash})`;
      g.fillRect(0, 0, W, H);
    }
    // The character dashes in from the left with speed trails.
    const poseImg = s.poseWave || s.poseShush;
    const inK = ease((sec - 0.15) / 0.55);
    if ((s._clips || []).includes("stinger")) {
      // The punch clip is laid over this by the app.
    } else if (poseImg && sec < 3) {
      const slide = (1 - inK) * -W * 0.6;
      for (let i = 3; i >= 1; i--) {
        if (inK >= 1) break;
        pose(g, poseImg, W * 0.27, H, H * 0.95, s, t, slide - i * W * 0.05, 0.15 * (1 - inK));
      }
      pose(g, poseImg, W * 0.27, H, H * 0.95, s, t, slide, 1);
    } else if (sec < 3) {
      avatar(g, W * 0.27 + (1 - inK) * -W * 0.5, H / 2, H * 0.6, s, t * 4);
    }
    // "BACK IN ACTION" slams in with a little shake.
    if (sec > 0.6 && sec < 3) {
      const k = ease((sec - 0.6) / 0.25);
      const sc = 1.6 - 0.6 * k;
      const shake = sec < 1.0 ? Math.sin(sec * 90) * H * 0.006 * (1 - k * 0.5) : 0;
      const size = Math.round(H * 0.13);
      g.save();
      g.translate(W * 0.68 + shake, H * 0.5);
      g.scale(sc, sc);
      g.globalAlpha = Math.min(1, k * 1.5);
      g.font = `700 ${size}px "Space Grotesk", "Segoe UI Black", sans-serif`;
      g.textAlign = "center";
      const grad = g.createLinearGradient(-W * 0.25, 0, W * 0.25, 0);
      grad.addColorStop(0, "#ffffff");
      grad.addColorStop(1, c2);
      g.shadowColor = c1;
      g.shadowBlur = H * 0.05;
      g.fillStyle = grad;
      g.fillText("BACK IN", 0, -size * 0.1);
      g.fillText("ACTION", 0, size * 0.95);
      g.shadowBlur = 0;
      g.font = `700 ${Math.round(size * 0.28)}px "Segoe UI", system-ui, sans-serif`;
      g.fillStyle = "#c9cfe8";
      g.fillText((s.title || "") + (s.right ? "  ·  " + s.right : ""), 0, size * 1.45);
      g.restore();
    }
    // Diagonal violet→cyan wipe covers the screen from 2.5 s.
    const wipe = ease((sec - 2.5) / 0.45);
    if (wipe > 0) {
      const grad = g.createLinearGradient(0, 0, W, H);
      grad.addColorStop(0, c1);
      grad.addColorStop(1, c2);
      g.fillStyle = grad;
      g.beginPath();
      const x = -W * 0.3 + wipe * W * 1.6;
      g.moveTo(x - W * 0.3, 0);
      g.lineTo(x + W * 0.1, 0);
      g.lineTo(x - W * 0.2, H);
      g.lineTo(x - W * 0.6, H);
      g.closePath();
      g.fill();
      if (wipe >= 1 || sec >= 2.95) {
        g.fillRect(0, 0, W, H);
      } else {
        g.fillRect(0, 0, Math.max(0, x - W * 0.6), H);
      }
    }
    // Hold (3..4 s): one still frame, so it loops cleanly.
    if (sec >= STINGER_HOLD_AT) {
      const grad = g.createLinearGradient(0, 0, W, H);
      grad.addColorStop(0, c1);
      grad.addColorStop(1, c2);
      g.fillStyle = grad;
      g.fillRect(0, 0, W, H);
      g.font = `700 ${Math.round(H * 0.09)}px "Space Grotesk", "Segoe UI Black", sans-serif`;
      g.textAlign = "center";
      g.fillStyle = "rgba(255,255,255,0.92)";
      g.fillText(s.title || "", W / 2, H * 0.53);
    }
    g.restore();
  }

  function scene(g, W, H, s, name, t) {
    g.save();
    g.clearRect(0, 0, W, H);
    g.textBaseline = "alphabetic";
    drawScene(g, W, H, s, name, t);
    g.restore();
  }

  // The streamed banner loop: 20 s, the Subscribe pop-up in the last 4 s.
  const BANNER_LOOP_SECONDS = 20;
  const POPUP_FROM = 16;
  /** Banner frame at second `sec` of the 20 s loop. */
  function bannerAt(g, W, H, s, sec) {
    curSec = sec % BANNER_LOOP_SECONDS;
    const t = (sec % LOOP_SECONDS) / LOOP_SECONDS;
    // With app pop-ups on, the loop itself stays clean (they are laid in).
    const p = s.popups && s.popups.on ? 0 : sec >= POPUP_FROM ? (sec - POPUP_FROM) / (BANNER_LOOP_SECONDS - POPUP_FROM) : 0;
    draw(g, W, H, s, t, p);
  }

  // Pop-ups ("moments"): 5 s each, laid over the banner loop by the app.
  const MOMENT_SECONDS = 5;
  const MOMENTS = {
    like: () => ({ icon: "👍", text: "Enjoying it? Hit LIKE", color: "#2563eb" }),
    follow: (s) => ({ icon: "📸", text: `Follow ${s.social || ""}`.trim(), color: "#d62976" }),
    discord: (s) => ({ icon: "💬", text: `Join the squad · ${s.discord || ""}`, color: "#5865f2" }),
    custom: (s) => ({ icon: "📣", text: s.custom || "", color: s.color }),
  };
  /** Which pop-ups can be shown with these settings. */
  function momentNames(s) {
    const on = (s.popups && s.popups.list) || {};
    return ["subscribe", "like", "follow", "discord", "custom"].filter((n) =>
      on[n] && (n === "follow" ? !!s.social : n === "discord" ? !!s.discord : n === "custom" ? !!s.custom : true));
  }
  /** Banner frame `sec` of the loop with pop-up `name` at `k` (0..1) of its 5 s. */
  function momentAt(g, W, H, s, name, sec, k) {
    curSec = sec % BANNER_LOOP_SECONDS;
    const t = (sec % LOOP_SECONDS) / LOOP_SECONDS;
    draw(g, W, H, s, t, Math.max(0.0001, k), name);
  }
  // A coloured pill that slides in from the right over the game label.
  function pillPopup(g, W, H, s, p, m) {
    const ease = (x) => 1 - Math.pow(1 - Math.min(1, Math.max(0, x)), 3);
    const k = p < 0.82 ? ease(p / 0.18) : 1 - ease((p - 0.82) / 0.18);
    if (k <= 0 || !m.text) return;
    const pad = Math.round(H * 0.12);
    const bh = H * 0.5;
    g.save();
    g.font = `700 ${Math.round(bh * 0.4)}px "Space Grotesk", "Segoe UI Black", sans-serif`;
    const tw = Math.min(g.measureText(m.text).width, W * 0.42);
    const bw = tw + bh * 1.6;
    const x = W - pad * 1.6 - bw + (1 - k) * (bw + pad * 2), y = H / 2 - bh / 2 + 3;
    g.globalAlpha = k;
    g.shadowColor = m.color;
    g.shadowBlur = bh * 0.36;
    g.fillStyle = m.color;
    g.beginPath();
    g.roundRect(x, y, bw, bh, bh * 0.22);
    g.fill();
    g.shadowBlur = 0;
    // A little bounce on the icon while it is held.
    const bounce = p > 0.15 && p < 0.85 ? Math.abs(Math.sin(p * Math.PI * 6)) * bh * 0.08 : 0;
    g.font = `${Math.round(bh * 0.5)}px "Segoe UI Emoji", "Segoe UI", sans-serif`;
    g.textBaseline = "middle";
    g.fillText(m.icon, x + bh * 0.3, y + bh / 2 - bounce);
    g.fillStyle = "#fff";
    g.font = `700 ${Math.round(bh * 0.4)}px "Space Grotesk", "Segoe UI Black", sans-serif`;
    g.fillText(m.text, x + bh * 1.15, y + bh / 2 + 1, tw);
    g.restore();
  }

  // "Banner off": a quiet strip with just a thin accent line and the logo
  // small on the right (the stream keeps its size; nothing restarts).
  function plain(g, W, H, s) {
    g.save();
    const bg = g.createLinearGradient(0, 0, 0, H);
    bg.addColorStop(0, "#07080d");
    bg.addColorStop(1, "#0c0e16");
    g.fillStyle = bg;
    g.fillRect(0, 0, W, H);
    g.globalAlpha = 0.6;
    g.fillStyle = s.color;
    g.fillRect(0, 0, W, 2);
    g.globalAlpha = 1;
    if (s.logo) {
      const d = Math.round(H * 0.42);
      const x = W - d - H * 0.25, y = (H - d) / 2;
      g.globalAlpha = 0.85;
      g.beginPath();
      g.arc(x + d / 2, y + d / 2, d / 2, 0, TAU);
      g.clip();
      drawSquare(g, s.logo, x, y, d);
    }
    g.restore();
  }

  window.BannerArt = { draw, bannerAt, plain, momentAt, momentNames, MOMENT_SECONDS, scene, stinger, animated, LOOP_SECONDS, BANNER_LOOP_SECONDS, FPS, SCENE_SECONDS: 4, STINGER_SECONDS, STINGER_HOLD_AT };
})();
