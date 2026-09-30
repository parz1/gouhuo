// SPDX-License-Identifier: MPL-2.0
// 加入页上那堆像素篝火。跟客户端里的是同一种画法：在很小的画布上画
// （一格 = CELL 个 CSS 像素），不插值地放大；火光、柴堆、火苗三层，
// 火苗是一团团往上飘的光斑，叠起来之后压成几档硬边的颜色。
// 客户端那份在 crates/client/src/campfire/scene.rs，这里是照着它重写的，没有共用代码。

const CELL = 5;
const FRAME_MS = 100;
const BAYER = [0, 8, 2, 10, 12, 4, 14, 6, 3, 11, 1, 9, 15, 7, 13, 5];

function rng(seed) {
  let s = seed >>> 0 || 1;
  return () => {
    s = (s + 0x6d2b79f5) >>> 0;
    let t = Math.imul(s ^ (s >>> 15), 1 | s);
    t = (t + Math.imul(t ^ (t >>> 7), 61 | t)) ^ t;
    return ((t ^ (t >>> 14)) >>> 0) / 4294967296;
  };
}

// 一堆粗木头随便码着，中间高、边上低。同一个种子永远是同一堆。
function pile(seed) {
  const r = rng(seed * 131 + 7);
  const logs = [];
  for (let i = 0; i < 11; i++) {
    const z = r() * 2 - 1, x = (r() * 2 - 1) * 1.05;
    const top = (1 - Math.abs(x) / 1.4) * (1 - Math.abs(z) * 0.45);
    const h = r() * top * 1.3;
    const a = (r() - 0.5) * (h > 0.45 ? 1.7 : 0.9);
    logs.push({ x, z, h, a, len: 1.5 + r() * 1.1, w: 0.4 + r() * 0.2, tone: 0.85 + r() * 0.3, end: r() < 0.5 ? -1 : 1 });
  }
  return logs.sort((p, q) => p.z + p.h * 0.35 - (q.z + q.h * 0.35));
}

export function startFire(canvas, seed = 1) {
  const g2d = canvas.getContext("2d");
  const logs = pile(seed);
  const rand = rng(seed ^ 0x9e3779b9);
  let w = 0, h = 0, img = null, sparks = [], clock = 0, timer = null;

  function geometry() {
    return { fx: w / 2, fy: h * 0.66, bw: Math.min(w * 0.13, h * 0.17), fh: h * 0.42 };
  }
  function put(x, y, r, g, b, a = 1) {
    x |= 0; y |= 0;
    if (x < 0 || y < 0 || x >= w || y >= h || a <= 0) return;
    const i = (y * w + x) * 4, d = img.data, k = Math.min(1, a), da = d[i + 3] / 255;
    d[i] = r * k + d[i] * (1 - k); d[i + 1] = g * k + d[i + 1] * (1 - k); d[i + 2] = b * k + d[i + 2] * (1 - k);
    d[i + 3] = 255 * (k + da * (1 - k));
  }
  // 一圈一圈往外淡的光：透明度压成几档，档和档之间用有序抖动接上。
  function glow(cx, cy, rx, ry, rgb, strength, levels) {
    for (let y = Math.floor(cy - ry); y <= cy + ry; y++) {
      for (let x = Math.floor(cx - rx); x <= cx + rx; x++) {
        const d = Math.hypot((x - cx) / rx, (y - cy) / ry);
        if (d >= 1) continue;
        const dither = (BAYER[(y & 3) * 4 + (x & 3)] + 0.5) / 16;
        const a = Math.floor((1 - d) ** 1.6 * strength * levels + dither) / levels;
        put(x, y, rgb[0], rgb[1], rgb[2], a);
      }
    }
  }
  function log(l, g, flick) {
    const cx = g.fx + l.x * g.bw, cy = g.fy + l.z * g.bw * 0.42 - l.h * g.bw * 0.9;
    const half = (l.len * g.bw) / 2, width = Math.max(2, l.w * g.bw);
    const ux = Math.cos(l.a), uy = Math.sin(l.a) * 0.8, ul = Math.hypot(ux, uy);
    const dx = ux / ul, dy = uy / ul;
    const burn = Math.max(0, Math.min(1, 1 - (Math.hypot(l.x, l.z * 0.6) - l.h * 0.4) / 1.3));
    for (let y = Math.floor(cy - half - width); y <= cy + half + width; y++) {
      for (let x = Math.floor(cx - half - width); x <= cx + half + width; x++) {
        const px = x + 0.5 - cx, py = y + 0.5 - cy;
        const along = px * dx + py * dy;
        let across = -px * dy + py * dx;
        if (dx < 0) across = -across;
        if (Math.abs(along) > half || Math.abs(across) > width / 2) continue;
        const t = across / width;
        let c = t < -0.22 ? [142, 102, 70] : t < 0.2 ? [96, 64, 42] : [44, 29, 20];
        const k = l.tone * (1 - burn * 0.45);
        c = [c[0] * k, c[1] * k, c[2] * k];
        // 底下被炭烤红
        if (t > 0.3) { const hot = (0.35 + 0.15 * flick) * (0.3 + burn * 0.7); c = [c[0] + (255 - c[0]) * hot, c[1] + (112 - c[1]) * hot, c[2] + (34 - c[2]) * hot]; }
        // 截面的年轮
        if (along * l.end > half - Math.max(1, width * 0.3)) c = [150 * l.tone, 110 * l.tone, 74 * l.tone];
        put(x, y, c[0], c[1], c[2]);
      }
    }
  }
  function step(dt, g) {
    clock += dt;
    for (let n = Math.round(dt * 120); n > 0; n--) {
      const ember = rand() < 0.05;
      sparks.push({
        x: g.fx + (rand() - 0.5) * g.bw * (ember ? 1 : 1.5), y: g.fy - g.bw * 0.45 + (rand() - 0.5) * g.bw * 0.3,
        vx: (rand() - 0.5) * g.bw * 0.3, vy: (-(g.fh / 0.85) * (0.7 + rand() * 0.6)) * (ember ? 1.5 : 1),
        life: 0, max: ember ? 1.3 + rand() : 0.5 + rand() * 0.5, size: g.bw * (0.2 + rand() * 0.22), seed: rand() * 6.283, ember,
      });
    }
    for (const s of sparks) {
      s.life += dt;
      s.x += (s.vx + Math.sin(s.life * 7 + s.seed) * g.bw * 0.3) * dt;
      if (!s.ember) s.x += (g.fx - s.x) * dt * 1.5 * (1 - s.life / s.max);
      s.y += s.vy * dt;
    }
    sparks = sparks.filter(s => s.life < s.max);
  }
  function draw() {
    const g = geometry();
    const flick = Math.sin(clock * 13) * 0.5 + Math.sin(clock * 31) * 0.3 + Math.sin(clock * 7.1) * 0.2;
    img = g2d.createImageData(w, h);
    const r = Math.min(w, h) * 0.62 * (1 + flick * 0.035);
    glow(g.fx, g.fy, r, r * 0.55, [240, 138, 36], 0.5, 12);
    glow(g.fx, g.fy, g.bw * 2, g.bw * 0.76, [205, 72, 24], 0.95 + flick * 0.05, 6);
    glow(g.fx, g.fy, g.bw * 1.2, g.bw * 0.45, [255, 176, 80], 0.9, 4);
    for (const l of logs) if (l.z <= 0.1) log(l, g, flick);

    // 火苗：每团光斑往热度场里加一点，最后按热度分四档上色，边是硬的。
    const heat = new Float32Array(w * h);
    for (const s of sparks) {
      if (s.ember) continue;
      const t = s.life / s.max, size = s.size * (1 - t * 0.55), ry = size * 2.1;
      for (let y = Math.floor(s.y - size * 0.3 - ry); y <= s.y - size * 0.3 + ry; y++) {
        for (let x = Math.floor(s.x - size); x <= s.x + size; x++) {
          if (x < 0 || y < 0 || x >= w || y >= h) continue;
          const d = Math.hypot((x - s.x) / size, (y - s.y + size * 0.3) / ry);
          if (d < 1) heat[y * w + x] += (1 - t) * 0.36 * (1 - d);
        }
      }
    }
    for (let i = 0; i < heat.length; i++) {
      const v = heat[i];
      if (v < 0.13) continue;
      const c = v > 1.05 ? [255, 238, 180] : v > 0.62 ? [255, 196, 96] : v > 0.3 ? [240, 128, 40] : [180, 58, 24];
      put(i % w, (i / w) | 0, c[0], c[1], c[2]);
    }
    for (const l of logs) if (l.z > 0.1) log(l, g, flick);
    for (const s of sparks) if (s.ember) put(s.x, s.y, 255, 233, 168, (1 - s.life / s.max) * 2);
    g2d.putImageData(img, 0, 0);
  }
  function resize() {
    const cw = Math.max(8, Math.round(canvas.clientWidth / CELL)), ch = Math.max(8, Math.round(canvas.clientHeight / CELL));
    if (cw === w && ch === h) return false;
    w = canvas.width = cw; h = canvas.height = ch;
    // 火星的位置是按格算的，格数一变就重新点火：先空烧一会儿让火长出来。
    sparks = [];
    const g = geometry();
    for (let i = 0; i < 40; i++) step(1 / 30, g);
    return true;
  }
  function tick() {
    resize();
    step(FRAME_MS / 1000, geometry());
    draw();
  }
  const still = globalThis.matchMedia?.("(prefers-reduced-motion: reduce)").matches;
  // 切到后台、或者系统关了动画，就不烧了：画一帧停在那儿。
  function sync() {
    const run = !still && !document.hidden;
    if (run && timer === null) timer = setInterval(tick, FRAME_MS);
    if (!run && timer !== null) { clearInterval(timer); timer = null; }
  }
  resize(); draw();
  document.addEventListener("visibilitychange", sync);
  globalThis.addEventListener("resize", () => { if (resize()) draw(); });
  sync();
}

if (typeof document !== "undefined") {
  const canvas = document.querySelector("canvas.campfire");
  if (canvas) {
    // 每个服务器一堆自己的柴：种子取自页面上公开的服务器信息。
    let seed = 1;
    for (const ch of document.body.dataset.payload ?? "") seed = (Math.imul(seed, 31) + ch.charCodeAt(0)) >>> 0;
    startFire(canvas, seed);
  }
}
