// SPDX-License-Identifier: GPL-3.0-or-later

//! 篝火的底图：夜色、地上的火光、一床炭、一堆粗木头、火苗。
//!
//! 画在很小的画布上（一格 = [`CELL`] 个逻辑像素），界面那边不插值地放大，
//! 所以本来就是像素画。整张图三层：
//!
//! - **后层**：夜色、火光、炭、柴堆后半截
//! - **火**：火苗和火星，叠加发光，最后抖动成硬边
//! - **前层**：柴堆前半截 —— 盖在火苗前面，火是从木头缝里窜出来的
//!
//! 尺寸全都相对 `bw`（火塘的基本宽度），窗口多大形状都一样。
//! 石头座位的位置是界面那边按同样的比例算的（campfire.slint）。

use std::f32::consts::TAU;

use slint::{Rgba8Pixel, SharedPixelBuffer};

use super::raster::{alpha, rgb, scale, Canvas, Rgba, Rng, CELL};

/// 火有多旺，0–1。以后可能做成设置，先定死。
const INTENSITY: f32 = 0.7;

/// 画面上各处的位置，单位是格。
#[derive(Clone, Copy, Debug)]
pub struct Geometry {
    pub w: usize,
    pub h: usize,
    /// 火塘中心。跟 campfire.slint 里的 fx / fy 是同一个比例。
    pub fx: f32,
    pub fy: f32,
    /// 火塘的基本宽度。
    pub bw: f32,
    /// 火苗能窜多高。
    pub fh: f32,
}

impl Geometry {
    /// `width` / `height` 是逻辑像素。
    pub fn new(width: f32, height: f32) -> Self {
        let w = (width / CELL).ceil().max(1.0) as usize;
        let h = (height / CELL).ceil().max(1.0) as usize;
        let (wf, hf) = (width / CELL, height / CELL);
        Self {
            w,
            h,
            fx: wf / 2.0,
            fy: hf * 0.55,
            bw: (wf * 0.09).min(hf * 0.13),
            fh: hf * 0.46 * INTENSITY,
        }
    }
}

/// 柴堆里的一根木头。
#[derive(Clone, Debug)]
pub struct Log {
    /// 左右，单位 bw
    x: f32,
    /// 前后，-1 最后 1 最前
    z: f32,
    /// 摞多高
    h: f32,
    /// 斜多少（弧度）
    a: f32,
    len: f32,
    w: f32,
    tone: f32,
    /// 哪一头露出年轮
    end: f32,
    seed: f32,
}

impl Log {
    /// 在火苗前面（盖住火）还是后面。
    fn in_front(&self) -> bool {
        self.z > 0.1
    }
}

/// 一堆粗木头随便码着，中间高、边上低。同一个种子永远是同一堆。
pub fn pile(seed: u64) -> Vec<Log> {
    let mut r = Rng::new(seed.wrapping_mul(131).wrapping_add(7));
    let mut logs: Vec<Log> = (0..13)
        .map(|_| {
            let z = r.f() * 2.0 - 1.0;
            let x = (r.f() * 2.0 - 1.0) * 1.05;
            let top = (1.0 - x.abs() / 1.4) * (1.0 - z.abs() * 0.45);
            let h = r.f() * top * 1.3;
            // 摞在上面的更斜，像是搭上去的
            let a = (r.f() - 0.5) * if h > 0.45 { 1.7 } else { 0.9 };
            let len = (1.5 + r.f() * 1.2).min(2.0 * (2.3 - x.abs()) / a.cos().abs().max(0.3));
            Log {
                x,
                z,
                h,
                a,
                len,
                w: 0.36 + r.f() * 0.2,
                tone: r.f(),
                end: if r.f() < 0.5 { -1.0 } else { 1.0 },
                seed: r.f() * TAU,
            }
        })
        .collect();
    // 从后往前、从下往上画
    logs.sort_by(|p, q| (p.z + p.h * 0.35).total_cmp(&(q.z + q.h * 0.35)));
    logs
}

struct Spark {
    x: f32,
    y: f32,
    vx: f32,
    vy: f32,
    life: f32,
    max: f32,
    size: f32,
    seed: f32,
    /// 火星（往上飘的小亮点），不是火苗
    ember: bool,
}

/// 火苗和火星。按时间推进，跟帧率无关。
pub struct Fire {
    sparks: Vec<Spark>,
    rng: Rng,
    spawn: f32,
    /// 用来让炭和木头上的暗火一明一暗。
    pub clock: f32,
}

impl Fire {
    pub fn new(seed: u64) -> Self {
        Self {
            sparks: Vec::new(),
            rng: Rng::new(seed),
            spawn: 0.0,
            clock: 0.0,
        }
    }

    /// 先空烧一会儿，让火长出来。静止画面用这个。
    pub fn warmed_up(seed: u64, g: &Geometry) -> Self {
        let mut fire = Self::new(seed);
        for _ in 0..60 {
            fire.step(1.0 / 30.0, g);
        }
        fire
    }

    pub fn step(&mut self, dt: f32, g: &Geometry) {
        self.clock += dt;
        self.spawn += dt * 170.0 * INTENSITY;
        let base_y = g.fy - g.bw * 0.45;
        while self.spawn >= 1.0 {
            self.spawn -= 1.0;
            let r = &mut self.rng;
            let ember = r.f() < 0.04;
            self.sparks.push(Spark {
                x: g.fx + (r.f() - 0.5) * g.bw * if ember { 1.0 } else { 1.5 },
                y: base_y + (r.f() - 0.5) * g.bw * 0.3,
                vx: (r.f() - 0.5) * g.bw * 0.3,
                vy: -(g.fh / 0.85) * (0.7 + r.f() * 0.6) * if ember { 1.5 } else { 1.0 },
                life: 0.0,
                max: if ember {
                    1.3 + r.f()
                } else {
                    0.5 + r.f() * 0.5
                },
                size: if ember {
                    1.0
                } else {
                    g.bw * (0.2 + r.f() * 0.22)
                },
                seed: r.f() * TAU,
                ember,
            });
        }
        for s in &mut self.sparks {
            s.life += dt;
            let t = s.life / s.max;
            s.x += (s.vx + (s.life * 7.0 + s.seed).sin() * g.bw * 0.3) * dt;
            if !s.ember {
                // 往中间收，火苗是尖的
                s.x += (g.fx - s.x) * dt * 1.5 * (1.0 - t);
            }
            s.y += s.vy * dt;
        }
        self.sparks.retain(|s| s.life < s.max);
    }
}

/// 火苗以外的两层：后面（夜色、火光、炭、柴堆后半截）和前面（柴堆前半截）。
///
/// 这两层也在动（火光一明一暗、木头上的暗火），但慢得多，不用每帧重画 ——
/// 见 [`super::Stage`]。每帧都画的只有火苗。
pub struct Layers {
    back: Canvas,
    front: Canvas,
}

pub fn layers(g: &Geometry, logs: &[Log], clock: f32) -> Layers {
    let flick = flicker(clock);

    let mut back = Canvas::new(g.w, g.h);
    back.vertical_gradient(rgb(13, 14, 17), rgb(24, 22, 26));
    ground_glow(&mut back, g, flick);
    embers(&mut back, g, clock, flick);
    for log in logs.iter().filter(|l| !l.in_front()) {
        draw_log(&mut back, g, log, clock, flick);
    }
    // 地上的火光：压色阶，再带一点点抖动 —— 圈和圈之间的颗粒感全靠它
    back.posterize(14.0, 2.5);

    let mut front = Canvas::new(g.w, g.h);
    for log in logs.iter().filter(|l| l.in_front()) {
        draw_log(&mut front, g, log, clock, flick);
    }
    front.harden(0.7);
    front.posterize(14.0, 14.0);

    Layers { back, front }
}

/// 在两层中间画上这一刻的火苗，合成一张图。
pub fn frame(g: &Geometry, layers: &Layers, fire: &Fire) -> SharedPixelBuffer<Rgba8Pixel> {
    let mut flames = Canvas::new(g.w, g.h);
    draw_flames(&mut flames, g, fire, flicker(fire.clock));
    flames.harden(0.7);
    flames.posterize(32.0, 32.0);

    let mut out = layers.back.clone();
    out.composite(&flames);
    out.composite(&layers.front);
    out.to_buffer()
}

/// 画一整张底图（三层一起画）。
#[cfg(test)]
pub fn render(g: &Geometry, logs: &[Log], fire: &Fire) -> SharedPixelBuffer<Rgba8Pixel> {
    frame(g, &layers(g, logs, fire.clock), fire)
}

/// 火光的明暗起伏，-1..1。几条不同频率的正弦叠起来，看不出周期。
fn flicker(t: f32) -> f32 {
    let t = t * 1000.0;
    (t * 0.013).sin() * 0.5 + (t * 0.031).sin() * 0.3 + (t * 0.0071).sin() * 0.2
}

fn ground_glow(c: &mut Canvas, g: &Geometry, flick: f32) {
    let r = (g.w.min(g.h) as f32) * (0.55 + 0.16 * INTENSITY) * (1.0 + flick * 0.035);
    let a = 0.09 + 0.24 * INTENSITY;
    c.glow(
        g.fx,
        g.fy,
        r,
        r * 0.6,
        &[
            (0.0, alpha(rgb(240, 138, 36), a)),
            (0.45, alpha(rgb(220, 100, 34), a * 0.35)),
            (1.0, alpha(rgb(220, 100, 34), 0.0)),
        ],
        false,
    );
}

fn embers(c: &mut Canvas, g: &Geometry, clock: f32, flick: f32) {
    let a = 0.45 + 0.4 * INTENSITY + flick * 0.06;
    c.glow(
        g.fx,
        g.fy,
        g.bw * 2.0,
        g.bw * 0.76,
        &[
            (0.0, alpha(rgb(255, 176, 80), a)),
            (0.5, alpha(rgb(205, 72, 24), a * 0.8)),
            (1.0, alpha(rgb(60, 20, 10), 0.0)),
        ],
        false,
    );
    let mut r = Rng::new(7);
    for _ in 0..26 {
        let (ang, d) = (r.f() * TAU, r.f().sqrt());
        let (x, y) = (
            g.fx + ang.cos() * d * 1.6 * g.bw,
            g.fy + ang.sin() * d * 0.55 * g.bw,
        );
        let size = (0.1 + r.f() * 0.12) * g.bw;
        let phase = r.f() * TAU;
        let glow = (0.4 + 0.35 * (clock * 2.0 + phase).sin()) * (0.4 + 0.6 * INTENSITY);
        c.ellipse(
            x,
            y,
            size + 0.6,
            size * 0.6 + 0.6,
            0.0,
            alpha(rgb(255, 110, 40), glow),
        );
        c.ellipse(x, y, size, size * 0.6, 0.0, rgb(34, 21, 15));
    }
}

fn draw_log(c: &mut Canvas, g: &Geometry, log: &Log, clock: f32, flick: f32) {
    let cx = g.fx + log.x * g.bw;
    let cy = g.fy + log.z * g.bw * 0.42 - log.h * g.bw * 0.9;
    let half = log.len * g.bw / 2.0;
    let w = log.w * g.bw;
    let (ux, uy) = (log.a.cos(), log.a.sin() * 0.8);
    let ul = (ux * ux + uy * uy).sqrt();
    let (dx, dy) = (ux / ul * half, uy / ul * half);
    // 法线朝上
    let (mut nx, mut ny) = (-dy / half, dx / half);
    if ny > 0.0 {
        nx = -nx;
        ny = -ny;
    }
    let seg = |c: &mut Canvas, off: f32, width: f32, from: f32, to: f32, color: Rgba| {
        let (ox, oy) = (nx * off, ny * off);
        c.capsule(
            (cx + dx * from + ox, cy + dy * from + oy),
            (cx + dx * to + ox, cy + dy * to + oy),
            width,
            color,
        );
    };
    let k = 0.85 + log.tone * 0.3;
    let wood = |r: u8, g: u8, b: u8| scale(rgb(r, g, b), k);
    // 离火越近烧得越黑
    let dist = (log.x * log.x + log.z * log.z * 0.36).sqrt() - log.h * 0.4;
    let burn = (1.0 - dist / 1.3).clamp(0.0, 1.0);

    seg(c, 0.0, w, -1.0, 1.0, wood(44, 29, 20)); // 树皮暗面
    seg(c, w * 0.12, w * 0.66, -1.0, 1.0, wood(96, 64, 42)); // 中间
                                                             // 顶上一道亮
    seg(c, w * 0.3, w * 0.16, -0.95, 0.95, wood(142, 102, 70));
    // 树皮的纹，一段一段的。木头太细（窗口小）的时候画不下，画了只是一团乱
    let dash = (w * 0.9) / half;
    let mut t = -1.0 + dash * 0.3;
    while w >= 4.0 && t < 1.0 {
        seg(
            c,
            -w * 0.05,
            w * 0.07,
            t,
            (t + dash * 0.6).min(1.0),
            alpha(rgb(30, 20, 14), 0.9),
        );
        t += dash * 1.6;
    }
    if burn > 0.0 {
        seg(
            c,
            w * 0.15,
            w * 0.72,
            -1.0,
            1.0,
            alpha(rgb(23, 16, 12), burn * 0.8),
        );
    }
    // 底下被炭烤红
    let under = (0.45 + 0.2 * flick) * (0.5 + 0.6 * INTENSITY) * (0.3 + burn * 0.7);
    seg(
        c,
        -w * 0.36,
        w * 0.16,
        -0.9,
        0.9,
        alpha(rgb(255, 112, 34), under),
    );
    // 焦黑处的暗火
    if burn > 0.25 {
        for j in 0..3 {
            let jf = j as f32;
            let t = -0.45 + jf * 0.42 + (log.seed + jf).sin() * 0.1;
            let a =
                (0.45 + 0.45 * (clock * 3.0 + log.seed * 3.0 + jf * 2.0).sin()) * burn * INTENSITY;
            if a < 0.08 {
                continue;
            }
            c.ellipse(
                cx + dx * t + nx * w * 0.12,
                cy + dy * t + ny * w * 0.12,
                w * 0.16,
                w * 0.08,
                dy.atan2(dx),
                alpha(rgb(255, 130 + 25 * j as u8, 40), a),
            );
        }
    }
    // 截面的年轮
    let (ex, ey) = (
        cx + dx * log.end * (1.0 + w * 0.3 / half),
        cy + dy * log.end * (1.0 + w * 0.3 / half),
    );
    let rot = dy.atan2(dx);
    c.ellipse(ex, ey, w * 0.26, w * 0.47, rot, wood(150, 110, 74));
    c.ellipse(ex, ey, w * 0.14, w * 0.27, rot, wood(92, 62, 40));
    c.ellipse(ex, ey, w * 0.08, w * 0.15, rot, wood(150, 110, 74));
}

fn draw_flames(c: &mut Canvas, g: &Geometry, fire: &Fire, flick: f32) {
    const COLORS: [Rgba; 4] = [
        rgb(255, 238, 180),
        rgb(255, 196, 96),
        rgb(240, 128, 40),
        rgb(180, 58, 24),
    ];
    // 一团火光的样子：中间实、边上虚，跟原型里那张光斑贴图一样
    let blob = |color: Rgba, a: f32| {
        [
            (0.0, alpha(color, a)),
            (0.45, alpha(color, a * 0.45)),
            (1.0, alpha(color, 0.0)),
        ]
    };
    // 柴堆里面那团最亮的
    let core = (0.2 + 0.06 * flick) * (0.5 + 0.5 * INTENSITY);
    c.glow(
        g.fx,
        g.fy - g.bw * 0.4,
        g.bw * 1.9,
        g.bw * 1.2,
        &blob(COLORS[2], core),
        true,
    );
    for s in &fire.sparks {
        let t = s.life / s.max;
        if s.ember {
            c.add(
                s.x as i32,
                s.y as i32,
                alpha(rgb(255, 233, 168), (1.0 - t) * 2.0),
            );
            continue;
        }
        let color = COLORS[((t * 4.0) as usize).min(3)];
        let size = s.size * (1.0 - t * 0.55);
        // 竖着拉长两倍多，像火舌
        c.glow(
            s.x,
            s.y - size * 0.3,
            size,
            size * 2.1,
            &blob(color, (1.0 - t) * 0.4),
            true,
        );
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn frame(seed: u64) -> Vec<Rgba8Pixel> {
        let g = Geometry::new(640.0, 400.0);
        let fire = Fire::warmed_up(1, &g);
        render(&g, &pile(seed), &fire).as_slice().to_vec()
    }

    #[test]
    fn same_seed_same_picture() {
        assert_eq!(frame(3), frame(3));
    }

    #[test]
    fn channels_get_different_piles() {
        assert_ne!(frame(3), frame(4));
    }

    #[test]
    fn picture_is_opaque_and_sized_in_cells() {
        let g = Geometry::new(640.0, 400.0);
        let buf = render(&g, &pile(1), &Fire::warmed_up(1, &g));
        assert_eq!((buf.width(), buf.height()), (160, 100));
        assert!(buf.as_slice().iter().all(|p| p.a == 255));
    }

    /// 火塘那里得比角落亮得多 —— 画面上最该看见的就是火。
    #[test]
    fn fire_is_the_brightest_thing() {
        let g = Geometry::new(640.0, 400.0);
        let buf = render(&g, &pile(1), &Fire::warmed_up(1, &g));
        let px = buf.as_slice();
        let lum = |x: usize, y: usize| {
            let p = px[y * 160 + x];
            p.r as u32 + p.g as u32 + p.b as u32
        };
        let brightest_near_fire = (35..55)
            .flat_map(|y| (70..90).map(move |x| (x, y)))
            .map(|(x, y)| lum(x, y))
            .max()
            .unwrap();
        assert!(
            brightest_near_fire > 600,
            "火塘附近最亮才 {brightest_near_fire}"
        );
        assert!(lum(2, 2) < 80);
    }

    /// 画一帧要多久。`cargo test --release -p client frame_cost -- --ignored --nocapture`
    #[test]
    #[ignore]
    fn frame_cost() {
        let g = Geometry::new(500.0, 470.0);
        let logs = pile(1);
        let mut fire = Fire::warmed_up(1, &g);
        let n = 300;
        let start = std::time::Instant::now();
        for _ in 0..n {
            fire.step(0.1, &g);
            std::hint::black_box(render(&g, &logs, &fire));
        }
        let per = start.elapsed().as_secs_f64() * 1000.0 / n as f64;
        println!("{}×{} 格，每帧 {per:.3} ms", g.w, g.h);
        let cached = layers(&g, &logs, fire.clock);
        let start = std::time::Instant::now();
        for _ in 0..n {
            fire.step(0.1, &g);
            std::hint::black_box(super::frame(&g, &cached, &fire));
        }
        let per = start.elapsed().as_secs_f64() * 1000.0 / n as f64;
        println!("只画火苗：每帧 {per:.3} ms");
        let start = std::time::Instant::now();
        for _ in 0..n {
            std::hint::black_box(layers(&g, &logs, fire.clock));
        }
        let per = start.elapsed().as_secs_f64() * 1000.0 / n as f64;
        println!("只画前后两层：{per:.3} ms");
    }

    #[test]
    fn fire_keeps_burning() {
        let g = Geometry::new(640.0, 400.0);
        let mut fire = Fire::new(9);
        for _ in 0..600 {
            fire.step(1.0 / 10.0, &g);
        }
        // 烧了一分钟，火苗数量稳定在一个范围里，不会越攒越多
        assert!(
            (20..200).contains(&fire.sparks.len()),
            "{}",
            fire.sparks.len()
        );
    }
}
