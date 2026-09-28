// SPDX-License-Identifier: GPL-3.0-or-later

//! 一个人一块像素石头。
//!
//! 形状按公钥算（同一个人在谁那里看都是同一块），朝火的那一面被照亮。
//! 每块石头画两张：平时的（压暗、褪色一点）和说话时的（亮一点、绿描边、一圈火光）。
//! 界面按说没说话挑一张，所以说话状态变的时候不用重画。
//!
//! 刻在上面的字不在这里画：像素字体还没有，先用界面的字叠上去。

use slint::{Rgba8Pixel, SharedPixelBuffer};

use super::raster::{alpha, mix, rgb, scale, Canvas, Rgba, Rng};

/// 石头四周留几格，说话时的光晕画在这里面。
pub const MARGIN: usize = 3;

/// `w` × `h` 是石头本身占几格；画出来的图每边再多 [`MARGIN`] 格。
/// `light` 是火在哪个方向（单位向量，y 朝下）。
pub fn sprite(
    seed: u32,
    tint: Rgba,
    w: usize,
    h: usize,
    light: (f32, f32),
    talking: bool,
) -> SharedPixelBuffer<Rgba8Pixel> {
    let shape = Shape::new(seed, w, h);
    let (cw, ch) = (w + 2 * MARGIN, h + 2 * MARGIN);
    let mut c = Canvas::new(cw, ch);
    let mut specks = Rng::new(seed as u64 * 31 + 5);

    // 说话时：光晕在石头底下，一格一格淡出去。不做抖动 ——
    // 抖动出来的光圈太碎，跟平滑的火光不是一个东西。
    if talking {
        for y in 0..ch {
            for x in 0..cw {
                if shape.inside(x, y) {
                    continue;
                }
                let a = match shape.distance_outside(x, y) {
                    1 => 0.5,
                    2 => 0.26,
                    3 => 0.1,
                    _ => continue,
                };
                c.blend(x as i32, y as i32, alpha(rgb(240, 138, 36), a));
            }
        }
    }

    let light_color = rgb(255, 140, 50);
    let edge = if talking {
        rgb(63, 191, 109)
    } else {
        rgb(12, 11, 14)
    };
    for y in 0..ch {
        for x in 0..cw {
            if !shape.inside(x, y) {
                continue;
            }
            let color = if shape.is_edge(x, y) {
                edge
            } else {
                let t = shape.row_fraction(y);
                let base = if specks.f() < 0.09 {
                    scale(tint, 0.58)
                } else if t < 0.3 {
                    scale(tint, 1.3)
                } else if t < 0.72 {
                    tint
                } else {
                    scale(tint, 0.7)
                };
                // 朝火的那一半被照亮
                let (dx, dy) = shape.offset(x, y);
                let lit = dx * light.0 + dy * light.1 * 1.4 > 0.26;
                mix(base, light_color, if lit { 0.3 } else { 0.0 })
            };
            // 火光只照亮说话的人：说话的亮一点，其他人暗下去、颜色也褪一点
            let color = if talking {
                scale(color, 1.2)
            } else {
                scale(desaturate(color, 0.3), 0.6)
            };
            c.blend(x as i32, y as i32, color);
        }
    }

    c.posterize(12.0, 0.0);
    c.to_buffer()
}

fn desaturate(c: Rgba, amount: f32) -> Rgba {
    let l = c[0] * 0.3 + c[1] * 0.59 + c[2] * 0.11;
    mix(c, [l, l, l, c[3]], amount)
}

struct Shape {
    w: usize,
    h: usize,
    /// 9 个方向上的半径，1 左右。
    radii: [f32; 9],
    offset: f32,
    inside: Vec<bool>,
    top: usize,
    bottom: usize,
}

impl Shape {
    fn new(seed: u32, w: usize, h: usize) -> Self {
        let mut r = Rng::new(seed as u64 * 7919 + 13);
        let offset = r.f() * std::f32::consts::TAU;
        let radii = std::array::from_fn(|_| 1.0 + (r.f() - 0.5) * 0.26);
        let (cw, ch) = (w + 2 * MARGIN, h + 2 * MARGIN);
        let mut shape = Self {
            w,
            h,
            radii,
            offset,
            inside: Vec::new(),
            top: 0,
            bottom: 0,
        };
        shape.inside = (0..ch)
            .flat_map(|y| (0..cw).map(move |x| (x, y)))
            .map(|(x, y)| shape.test(x, y))
            .collect();
        let rows: Vec<usize> = (0..ch)
            .filter(|&y| (0..cw).any(|x| shape.inside(x, y)))
            .collect();
        shape.top = rows.first().copied().unwrap_or(0);
        shape.bottom = rows.last().copied().unwrap_or(0);
        shape
    }

    /// 格子中心相对石头中心的位置，按半宽半高归一化。
    fn offset(&self, x: usize, y: usize) -> (f32, f32) {
        let cx = (self.w + 2 * MARGIN) as f32 / 2.0;
        let cy = (self.h + 2 * MARGIN) as f32 / 2.0;
        (
            (x as f32 + 0.5 - cx) / (self.w as f32 / 2.0),
            (y as f32 + 0.5 - cy) / (self.h as f32 / 2.0),
        )
    }

    fn test(&self, x: usize, y: usize) -> bool {
        let (dx, dy) = self.offset(x, y);
        // 底下压平，像是放在地上
        if dy > 0.8 {
            return false;
        }
        let angle = (dy.atan2(dx) - self.offset).rem_euclid(std::f32::consts::TAU);
        let pos = angle / std::f32::consts::TAU * 9.0;
        let i = pos.floor() as usize % 9;
        let t = pos.fract();
        // 余弦插值，边是圆的，不是多边形
        let k = (1.0 - (t * std::f32::consts::PI).cos()) / 2.0;
        let radius = self.radii[i] * (1.0 - k) + self.radii[(i + 1) % 9] * k;
        (dx * dx + dy * dy).sqrt() <= radius * 0.98
    }

    fn inside(&self, x: usize, y: usize) -> bool {
        let cw = self.w + 2 * MARGIN;
        x < cw && y < self.h + 2 * MARGIN && self.inside[y * cw + x]
    }

    fn inside_i(&self, x: i32, y: i32) -> bool {
        x >= 0 && y >= 0 && self.inside(x as usize, y as usize)
    }

    fn is_edge(&self, x: usize, y: usize) -> bool {
        let (x, y) = (x as i32, y as i32);
        !self.inside_i(x - 1, y)
            || !self.inside_i(x + 1, y)
            || !self.inside_i(x, y - 1)
            || !self.inside_i(x, y + 1)
    }

    /// 0 是石头最上面一行，1 是最下面。
    fn row_fraction(&self, y: usize) -> f32 {
        (y.saturating_sub(self.top)) as f32 / (self.bottom - self.top).max(1) as f32
    }

    /// 石头外面的一格离石头有几格远（直线距离向上取整），最多数到 MARGIN + 1。
    fn distance_outside(&self, x: usize, y: usize) -> usize {
        let m = MARGIN as i32;
        let mut best = f32::MAX;
        for oy in -m..=m {
            for ox in -m..=m {
                if self.inside_i(x as i32 + ox, y as i32 + oy) {
                    best = best.min(((ox * ox + oy * oy) as f32).sqrt());
                }
            }
        }
        if best == f32::MAX {
            MARGIN + 1
        } else {
            best.ceil() as usize
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    const TINT: Rgba = rgb(110, 100, 95);

    #[test]
    fn same_person_same_stone() {
        let a = sprite(42, TINT, 18, 14, (0.0, -1.0), false);
        let b = sprite(42, TINT, 18, 14, (0.0, -1.0), false);
        assert_eq!(a.as_slice(), b.as_slice());
        let c = sprite(43, TINT, 18, 14, (0.0, -1.0), false);
        assert_ne!(a.as_slice(), c.as_slice());
    }

    #[test]
    fn sprite_has_room_for_the_halo() {
        let s = sprite(1, TINT, 18, 14, (0.0, -1.0), true);
        assert_eq!(
            (s.width(), s.height()),
            (18 + 2 * MARGIN as u32, 14 + 2 * MARGIN as u32)
        );
    }

    #[test]
    fn stone_fills_most_of_its_box() {
        let s = sprite(1, TINT, 18, 14, (0.0, -1.0), false);
        let solid = s.as_slice().iter().filter(|p| p.a == 255).count();
        assert!(solid > 18 * 14 / 2, "石头太小了：{solid} 格");
    }

    /// 说话的那张一眼要比平时的亮、而且多一圈光。
    #[test]
    fn talking_stone_is_brighter_and_bigger() {
        let quiet = sprite(5, TINT, 18, 14, (0.0, -1.0), false);
        let talking = sprite(5, TINT, 18, 14, (0.0, -1.0), true);
        let lum = |b: &SharedPixelBuffer<Rgba8Pixel>| {
            b.as_slice()
                .iter()
                .filter(|p| p.a > 0)
                .map(|p| p.r as u32 + p.g as u32 + p.b as u32)
                .sum::<u32>()
        };
        let area =
            |b: &SharedPixelBuffer<Rgba8Pixel>| b.as_slice().iter().filter(|p| p.a > 0).count();
        assert!(lum(&talking) > lum(&quiet) * 3 / 2);
        assert!(area(&talking) > area(&quiet));
    }
}
