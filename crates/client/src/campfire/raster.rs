// SPDX-License-Identifier: GPL-3.0-or-later

//! 像素画的小画布：一格就是屏幕上的一个像素块（[`CELL`] 个逻辑像素见方）。
//!
//! 只有篝火要用的几样东西：渐变、椭圆、粗线段（木头）、软光斑（火苗）。
//! 形状的边一律不抗锯齿 —— 像素画要的就是硬边；过渡交给最后的分档和抖动。
//!
//! 内部存**预乘**的 RGBA：叠加发光（火苗一层层加亮）和普通覆盖都只要一行式子。

use slint::{Rgba8Pixel, SharedPixelBuffer};

/// 一个像素块是几个逻辑像素。界面那边按这个放大，两边必须一致。
pub const CELL: f32 = 4.0;

/// 颜色，0–1，**不**预乘。
pub type Rgba = [f32; 4];

pub const fn rgb(r: u8, g: u8, b: u8) -> Rgba {
    [r as f32 / 255.0, g as f32 / 255.0, b as f32 / 255.0, 1.0]
}

pub fn alpha(c: Rgba, a: f32) -> Rgba {
    [c[0], c[1], c[2], c[3] * a.clamp(0.0, 1.0)]
}

pub fn scale(c: Rgba, k: f32) -> Rgba {
    [c[0] * k, c[1] * k, c[2] * k, c[3]]
}

pub fn mix(a: Rgba, b: Rgba, t: f32) -> Rgba {
    let t = t.clamp(0.0, 1.0);
    [
        a[0] + (b[0] - a[0]) * t,
        a[1] + (b[1] - a[1]) * t,
        a[2] + (b[2] - a[2]) * t,
        a[3] + (b[3] - a[3]) * t,
    ]
}

/// 4×4 Bayer 矩阵，值在 -0.5..0.5。有序抖动：同一个位置每帧的阈值一样，
/// 火苗动的时候边缘不会像噪点那样闪。
pub fn bayer(x: usize, y: usize) -> f32 {
    const M: [u8; 16] = [0, 8, 2, 10, 12, 4, 14, 6, 3, 11, 1, 9, 15, 7, 13, 5];
    M[(y & 3) * 4 + (x & 3)] as f32 / 16.0 - 0.47
}

#[derive(Clone)]
pub struct Canvas {
    pub w: usize,
    pub h: usize,
    /// 预乘的 RGBA。
    px: Vec<[f32; 4]>,
}

impl Canvas {
    pub fn new(w: usize, h: usize) -> Self {
        Self {
            w,
            h,
            px: vec![[0.0; 4]; w * h],
        }
    }

    fn at(&mut self, x: i32, y: i32) -> Option<&mut [f32; 4]> {
        if x < 0 || y < 0 || x as usize >= self.w || y as usize >= self.h {
            return None;
        }
        Some(&mut self.px[y as usize * self.w + x as usize])
    }

    /// 不预乘的颜色。
    #[cfg(test)]
    pub fn pixel(&self, x: usize, y: usize) -> Rgba {
        let p = self.px[y * self.w + x];
        if p[3] <= 0.0 {
            return [0.0; 4];
        }
        [p[0] / p[3], p[1] / p[3], p[2] / p[3], p[3]]
    }

    /// 普通覆盖（source-over）。
    pub fn blend(&mut self, x: i32, y: i32, c: Rgba) {
        let a = c[3];
        if a <= 0.0 {
            return;
        }
        if let Some(p) = self.at(x, y) {
            for i in 0..3 {
                p[i] = c[i] * a + p[i] * (1.0 - a);
            }
            p[3] = a + p[3] * (1.0 - a);
        }
    }

    /// 叠加发光：越叠越亮，最后烧成白。
    pub fn add(&mut self, x: i32, y: i32, c: Rgba) {
        if c[3] <= 0.0 {
            return;
        }
        if let Some(p) = self.at(x, y) {
            for i in 0..3 {
                p[i] += c[i] * c[3];
            }
            p[3] = (p[3] + c[3]).min(1.0);
        }
    }

    pub fn vertical_gradient(&mut self, top: Rgba, bottom: Rgba) {
        for y in 0..self.h {
            let c = mix(top, bottom, y as f32 / self.h.max(1) as f32);
            for x in 0..self.w {
                self.blend(x as i32, y as i32, c);
            }
        }
    }

    /// 把 (x0..x1, y0..y1) 裁到画布里，逐格调 `f(x, y, 格子中心 x, 格子中心 y)`。
    fn each(
        &mut self,
        x0: f32,
        y0: f32,
        x1: f32,
        y1: f32,
        mut f: impl FnMut(&mut Self, i32, i32, f32, f32),
    ) {
        let xa = (x0.floor() as i32).max(0);
        let ya = (y0.floor() as i32).max(0);
        let xb = (x1.ceil() as i32).min(self.w as i32 - 1);
        let yb = (y1.ceil() as i32).min(self.h as i32 - 1);
        for y in ya..=yb {
            for x in xa..=xb {
                f(self, x, y, x as f32 + 0.5, y as f32 + 0.5);
            }
        }
    }

    /// 实心椭圆，可以转。
    pub fn ellipse(&mut self, cx: f32, cy: f32, rx: f32, ry: f32, rot: f32, c: Rgba) {
        let (s, co) = rot.sin_cos();
        let r = rx.max(ry);
        self.each(cx - r, cy - r, cx + r, cy + r, |cv, x, y, px, py| {
            let (dx, dy) = (px - cx, py - cy);
            let u = (dx * co + dy * s) / rx.max(0.01);
            let v = (-dx * s + dy * co) / ry.max(0.01);
            if u * u + v * v <= 1.0 {
                cv.blend(x, y, c);
            }
        });
    }

    /// 两头圆的粗线段。太细的线至少占一格宽，不然像素画里会断成点。
    pub fn capsule(&mut self, a: (f32, f32), b: (f32, f32), width: f32, c: Rgba) {
        let r = (width / 2.0).max(0.55);
        let (dx, dy) = (b.0 - a.0, b.1 - a.1);
        let len2 = (dx * dx + dy * dy).max(1e-6);
        self.each(
            a.0.min(b.0) - r,
            a.1.min(b.1) - r,
            a.0.max(b.0) + r,
            a.1.max(b.1) + r,
            |cv, x, y, px, py| {
                let t = (((px - a.0) * dx + (py - a.1) * dy) / len2).clamp(0.0, 1.0);
                let (qx, qy) = (a.0 + dx * t - px, a.1 + dy * t - py);
                if qx * qx + qy * qy <= r * r {
                    cv.blend(x, y, c);
                }
            },
        );
    }

    /// 软光斑：中心 `stops[0]`，往外按 `stops` 过渡到边上。`additive` 时叠加发光。
    ///
    /// 这里画的是平滑的渐变。像素画里那种一圈一圈、圈与圈之间带颗粒的光，
    /// 是最后 [`Canvas::posterize`] 压色阶压出来的。
    pub fn glow(
        &mut self,
        cx: f32,
        cy: f32,
        rx: f32,
        ry: f32,
        stops: &[(f32, Rgba)],
        additive: bool,
    ) {
        self.each(cx - rx, cy - ry, cx + rx, cy + ry, |cv, x, y, px, py| {
            let (u, v) = ((px - cx) / rx.max(0.01), (py - cy) / ry.max(0.01));
            let d = (u * u + v * v).sqrt();
            if d >= 1.0 {
                return;
            }
            let c = sample(stops, d);
            if additive {
                cv.add(x, y, c);
            } else {
                cv.blend(x, y, c);
            }
        });
    }

    /// 把另一张同样大的画布盖上来。
    pub fn composite(&mut self, top: &Canvas) {
        debug_assert_eq!((self.w, self.h), (top.w, top.h));
        for (p, t) in self.px.iter_mut().zip(&top.px) {
            let a = t[3];
            for i in 0..4 {
                p[i] = t[i] + p[i] * (1.0 - a);
            }
        }
    }

    /// 半透明一律变成要么全有要么全无，按 Bayer 抖动决定 —— 火苗的边就是这样来的。
    pub fn harden(&mut self, spread: f32) {
        for y in 0..self.h {
            for x in 0..self.w {
                let p = &mut self.px[y * self.w + x];
                if p[3] <= 0.0 {
                    continue;
                }
                if p[3] + bayer(x, y) * spread < 0.5 {
                    *p = [0.0; 4];
                } else {
                    let a = p[3];
                    for c in p.iter_mut().take(3) {
                        *c = (*c / a).min(1.0);
                    }
                    p[3] = 1.0;
                }
            }
        }
    }

    /// 颜色压成几档（`step` 是 0–255 里每档多宽），三个通道各压各的。
    ///
    /// `dither` 是压之前加的 Bayer 抖动有多大（也是 0–255 的单位）：
    ///
    /// - 跟 `step` 一样大：档与档之间是明显的棋盘格过渡（火苗、木头）
    /// - 一两个单位：只在正好卡在档边上的地方冒出零星的颗粒 —— 地上的火光就是这样，
    ///   原型里这些颗粒来自浏览器画渐变时自带的抖动，那是它看着「像素」的关键
    /// - 0：干净的色阶
    pub fn posterize(&mut self, step: f32, dither: f32) {
        for y in 0..self.h {
            let row = y * self.w;
            for x in 0..self.w {
                let p = &mut self.px[row + x];
                let a = p[3];
                if a <= 0.0 {
                    continue;
                }
                let o = bayer(x, y) * dither;
                for c in p.iter_mut().take(3) {
                    let v = *c / a * 255.0;
                    *c = (((v + o) / step).round() * step).clamp(0.0, 255.0) / 255.0 * a;
                }
            }
        }
    }

    pub fn to_buffer(&self) -> SharedPixelBuffer<Rgba8Pixel> {
        let mut buf = SharedPixelBuffer::<Rgba8Pixel>::new(self.w as u32, self.h as u32);
        for (out, p) in buf.make_mut_slice().iter_mut().zip(&self.px) {
            let a = p[3].clamp(0.0, 1.0);
            let un = |v: f32| {
                if a <= 0.0 {
                    0
                } else {
                    ((v / a).clamp(0.0, 1.0) * 255.0).round() as u8
                }
            };
            *out = Rgba8Pixel {
                r: un(p[0]),
                g: un(p[1]),
                b: un(p[2]),
                a: (a * 255.0).round() as u8,
            };
        }
        buf
    }
}

/// 按位置在几个色标之间插值。
fn sample(stops: &[(f32, Rgba)], t: f32) -> Rgba {
    let Some(first) = stops.first() else {
        return [0.0; 4];
    };
    if t <= first.0 {
        return first.1;
    }
    for pair in stops.windows(2) {
        let ((t0, c0), (t1, c1)) = (pair[0], pair[1]);
        if t <= t1 {
            return mix(c0, c1, (t - t0) / (t1 - t0).max(1e-6));
        }
    }
    stops[stops.len() - 1].1
}

/// 可复现的伪随机数（splitmix64）。柴堆、石头形状、火星都用它 ——
/// 同样的种子在谁的电脑上都画出同样的东西。
pub struct Rng(u64);

impl Rng {
    pub fn new(seed: u64) -> Self {
        Self(seed ^ 0x9E37_79B9_7F4A_7C15)
    }

    /// 0..1
    pub fn f(&mut self) -> f32 {
        self.0 = self.0.wrapping_add(0x9E37_79B9_7F4A_7C15);
        let mut z = self.0;
        z = (z ^ (z >> 30)).wrapping_mul(0xBF58_476D_1CE4_E5B9);
        z = (z ^ (z >> 27)).wrapping_mul(0x94D0_49BB_1331_11EB);
        z ^= z >> 31;
        (z >> 40) as f32 / (1u64 << 24) as f32
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn blend_over_transparent_keeps_color() {
        let mut c = Canvas::new(1, 1);
        c.blend(0, 0, rgb(200, 100, 50));
        let p = c.pixel(0, 0);
        assert!((p[0] - 200.0 / 255.0).abs() < 1e-5 && p[3] == 1.0);
    }

    #[test]
    fn additive_saturates_to_white() {
        let mut c = Canvas::new(1, 1);
        for _ in 0..10 {
            c.add(0, 0, alpha(rgb(255, 200, 100), 0.5));
        }
        c.harden(0.0);
        assert_eq!(c.pixel(0, 0), [1.0, 1.0, 1.0, 1.0]);
    }

    #[test]
    fn harden_leaves_no_half_transparent_pixels() {
        let mut c = Canvas::new(8, 8);
        c.glow(
            4.0,
            4.0,
            4.0,
            4.0,
            &[
                (0.0, rgb(255, 255, 255)),
                (1.0, alpha(rgb(255, 255, 255), 0.0)),
            ],
            true,
        );
        c.harden(0.7);
        for y in 0..8 {
            for x in 0..8 {
                let a = c.pixel(x, y)[3];
                assert!(a == 0.0 || a == 1.0);
            }
        }
    }

    #[test]
    fn thin_lines_do_not_vanish() {
        let mut c = Canvas::new(10, 3);
        c.capsule((0.5, 1.5), (9.5, 1.5), 0.1, rgb(255, 0, 0));
        assert!((0..10).all(|x| c.pixel(x, 1)[3] == 1.0));
    }

    #[test]
    fn rng_is_reproducible() {
        let (mut a, mut b) = (Rng::new(7), Rng::new(7));
        for _ in 0..100 {
            let v = a.f();
            assert!((0.0..1.0).contains(&v));
            assert_eq!(v, b.f());
        }
    }
}
