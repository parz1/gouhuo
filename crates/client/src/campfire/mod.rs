// SPDX-License-Identifier: GPL-3.0-or-later

//! 篝火场景里「谁坐哪块石头」。
//!
//! 围着火一圈 8 个座位。规矩只有几条，都是为了「瞄一眼就知道谁是谁」：
//!
//! - **自己永远坐最靠近镜头的那块**（0 号）。
//! - **坐下了就不挪**。有人走了，他的位子空着，别人不往里挤 ——
//!   大家很快会记住「老张在左边」，一重排这个就全废了。
//! - 新来的坐**离已经坐着的人最远**的空位，所以两个人时是面对面，
//!   不会挤在一边。
//! - 满了（自己 + 7 个人）之后来的人排队，不在圈里画，界面上显示「还有 N 人」。
//!   有人走了，排最前面的那个补进空出来的位子。
//!
//! 座位在画面上的位置是界面按比例算的（campfire.slint）；底图和石头的像素画
//! 在这里画好交给界面，见 [`scene`] 和 [`stone`]。

use std::cell::RefCell;
use std::collections::HashMap;

use slint::{Color, Image};

pub mod raster;
pub mod scene;
pub mod stone;

/// 一圈几个座位。
pub const SEATS: usize = 8;

/// 自己的座位号。
const MY_SEAT: usize = 0;

#[derive(Debug, Default)]
pub struct SeatMap {
    /// 座位号 → 坐着谁（会话 id）。
    seats: [Option<u32>; SEATS],
    /// 没座位的人，按来的先后。
    waiting: Vec<u32>,
    /// 这份座位表是哪个频道的。换了频道就从头排。
    channel: Option<u32>,
}

impl SeatMap {
    /// 按频道里现在有谁，更新座位。
    ///
    /// `present` 是频道里所有人（含自己）。它的顺序只决定**同一次**新来的几个人
    /// 谁先挑座位；已经坐下的人不受它影响。
    pub fn update(&mut self, channel: u32, me: u32, present: &[u32]) {
        if self.channel != Some(channel) {
            *self = Self {
                channel: Some(channel),
                ..Self::default()
            };
        }

        // 走了的人：座位空出来，队也不排了。
        for seat in &mut self.seats {
            if seat.is_some_and(|id| !present.contains(&id)) {
                *seat = None;
            }
        }
        self.waiting.retain(|id| present.contains(id));

        if present.contains(&me) && self.seats[MY_SEAT] != Some(me) {
            self.seats[MY_SEAT] = Some(me);
        }

        // 先让排队的人补空位，再轮到这次新来的。
        let seated = |map: &Self, id: u32| map.seats.contains(&Some(id));
        let newcomers: Vec<u32> = present
            .iter()
            .copied()
            .filter(|&id| id != me && !seated(self, id) && !self.waiting.contains(&id))
            .collect();
        let queue: Vec<u32> = self.waiting.drain(..).chain(newcomers).collect();
        for id in queue {
            match self.best_free_seat() {
                Some(seat) => self.seats[seat] = Some(id),
                None => self.waiting.push(id),
            }
        }
    }

    /// 这个人坐几号。没座位（在排队，或者根本不在频道里）是 `None`。
    pub fn seat_of(&self, id: u32) -> Option<usize> {
        self.seats.iter().position(|&s| s == Some(id))
    }

    /// 没座位的人，按来的先后。
    pub fn waiting(&self) -> &[u32] {
        &self.waiting
    }

    /// 离已经坐着的人最远的空位。一样远的挑号小的，这样结果是确定的。
    ///
    /// 0 号留给自己，别人不坐 —— 哪怕自己暂时不在（比如刚进频道、
    /// 名单还没同步到自己那条）。
    fn best_free_seat(&self) -> Option<usize> {
        let taken: Vec<usize> = (0..SEATS).filter(|&i| self.seats[i].is_some()).collect();
        (0..SEATS)
            .filter(|&i| i != MY_SEAT && self.seats[i].is_none())
            .max_by_key(|&i| {
                let nearest = taken
                    .iter()
                    .map(|&t| ring_distance(i, t))
                    .min()
                    .unwrap_or(SEATS);
                // max_by_key 碰到一样大的取最后一个，所以把号取反，让小号赢。
                (nearest, SEATS - i)
            })
    }
}

/// 圈上两个座位隔几格。
fn ring_distance(a: usize, b: usize) -> usize {
    let d = a.abs_diff(b) % SEATS;
    d.min(SEATS - d)
}

/// 刻在石头上的那一个字。
///
/// 「老张」「小鱼」「阿杰」这种，刻第二个字才认得出是谁。
pub fn glyph(name: &str) -> String {
    let chars: Vec<char> = name.chars().collect();
    let pick = match chars.as_slice() {
        ['老' | '小' | '阿', second] => *second,
        [first, ..] => *first,
        [] => '?',
    };
    pick.to_uppercase().collect()
}

/// 这个人的石头长什么样（形状、颜色都从它来）。
///
/// 按公钥算，不按昵称、不按会话 id：同一个人改了名、重连了、
/// 换到别人的电脑上看，都还是同一块石头。
pub fn stone_seed(public_key: &[u8]) -> u32 {
    // FNV-1a，只为了把公钥摊成一个稳定的数，不需要密码学强度。
    public_key.iter().fold(0x811c_9dc5_u32, |h, &b| {
        (h ^ b as u32).wrapping_mul(0x0100_0193)
    })
}

/// 石头的颜色。压得很灰 —— 石头就该是石头，分辨人靠刻的字和名字，颜色只是帮一把。
pub fn stone_tint(seed: u32) -> Color {
    let (r, g, b) = hsl_to_rgb((seed % 360) as f32, 0.16, 0.42);
    Color::from_rgb_u8(r, g, b)
}

/// 石头大小（逻辑像素），跟 campfire.slint 里的 `s` 同一个公式。
fn stone_size(scene: (f32, f32)) -> f32 {
    (scene.0.min(scene.1) * 0.125).clamp(34.0, 62.0)
}

/// n 号座位在圈上的角度，跟 campfire.slint 里的 `angle()` 一样。
fn seat_angle(seat: usize) -> f32 {
    (112.5 + seat as f32 * 45.0).to_radians()
}

/// (种子, 座位号, 宽几格, 高几格) → (平时, 说话时)
type StoneCache = HashMap<(u32, usize, usize, usize), (Image, Image)>;

thread_local! {
    /// 画好的石头：(种子, 座位号, 宽几格, 高几格) → (平时, 说话时)。
    /// 窗口大小一变格数就跟着变，旧的就用不上了，攒多了清掉。
    static STONES: RefCell<StoneCache> = RefCell::new(HashMap::new());
}

/// 某人坐在 n 号座位上时的两张石头图（平时的，说话时的）。
///
/// `scene` 是篝火画面的逻辑尺寸。还不知道（界面还没报上来）时给空图。
pub fn stone_images(seed: u32, seat: usize, scene: (f32, f32)) -> (Image, Image) {
    if scene.0 <= 0.0 || scene.1 <= 0.0 {
        return (Image::default(), Image::default());
    }
    let s = stone_size(scene);
    // 石头是扁的：比原型里量出来的比例（宽 1.28 s、高 0.76 s）
    let w = (s * 1.28 / raster::CELL).round() as usize;
    let h = (s * 0.76 / raster::CELL).round() as usize;
    STONES.with(|cache| {
        let mut cache = cache.borrow_mut();
        if cache.len() > 256 {
            cache.clear();
        }
        cache
            .entry((seed, seat, w, h))
            .or_insert_with(|| {
                let tint = stone_tint(seed);
                let tint = raster::rgb(tint.red(), tint.green(), tint.blue());
                // 火在圈中间：从座位往圆心的方向
                let a = seat_angle(seat);
                let light = (-a.cos(), -a.sin());
                (
                    Image::from_rgba8(stone::sprite(seed, tint, w, h, light, false)),
                    Image::from_rgba8(stone::sprite(seed, tint, w, h, light, true)),
                )
            })
            .clone()
    })
}

/// 篝火的底图：一个频道一堆柴、一团一直在烧的火、现在的画面尺寸。
///
/// 柴堆按频道定，同一个频道里大家看到的是同一堆。火按时间往前烧，
/// [`Stage::advance`] 一次就是动画的一帧。
#[derive(Default)]
pub struct Stage {
    /// 逻辑像素。界面还没报上来之前是 0。
    size: (f32, f32),
    channel: Option<u32>,
    pile: Vec<scene::Log>,
    fire: Option<scene::Fire>,
    /// 火苗前后那两层，隔几帧才重画一次。
    layers: Option<scene::Layers>,
    frames: u32,
}

/// 火光、炭、木头上的暗火每几帧重画一次。它们变得慢，10 帧里重画 3 次看不出区别，
/// 省下的是每帧一大半的时间。
const LAYERS_EVERY: u32 = 3;

impl Stage {
    pub fn size(&self) -> (f32, f32) {
        self.size
    }

    /// 尺寸变了返回 true。变了之后火要重新点：火星的位置是按格算的，
    /// 格数一变，旧的火星全在错的地方。
    pub fn set_size(&mut self, size: (f32, f32)) -> bool {
        if self.size == size {
            return false;
        }
        self.size = size;
        self.fire = None;
        self.layers = None;
        true
    }

    /// 换了频道返回 true。
    pub fn set_channel(&mut self, channel: u32) -> bool {
        if self.channel == Some(channel) {
            return false;
        }
        self.channel = Some(channel);
        self.pile = scene::pile(channel as u64);
        self.fire = None;
        self.layers = None;
        true
    }

    fn geometry(&self) -> Option<scene::Geometry> {
        (self.size.0 > 0.0 && self.size.1 > 0.0 && self.channel.is_some())
            .then(|| scene::Geometry::new(self.size.0, self.size.1))
    }

    /// 火往前烧 `dt` 秒，画一帧。
    pub fn advance(&mut self, dt: f32) -> Image {
        let Some(g) = self.geometry() else {
            return Image::default();
        };
        let seed = self.channel.unwrap_or(0) as u64;
        let fire = self
            .fire
            .get_or_insert_with(|| scene::Fire::warmed_up(seed, &g));
        fire.step(dt, &g);
        if self.layers.is_none() || self.frames % LAYERS_EVERY == 0 {
            self.layers = Some(scene::layers(&g, &self.pile, fire.clock));
        }
        self.frames = self.frames.wrapping_add(1);
        let layers = self.layers.as_ref().expect("刚画过");
        Image::from_rgba8(scene::frame(&g, layers, fire))
    }

    /// 不往前烧，把现在的样子画出来（静止画面、尺寸刚变时用）。
    pub fn still(&mut self) -> Image {
        self.advance(0.0)
    }
}

fn hsl_to_rgb(hue: f32, saturation: f32, lightness: f32) -> (u8, u8, u8) {
    let c = (1.0 - (2.0 * lightness - 1.0).abs()) * saturation;
    let h = hue / 60.0;
    let x = c * (1.0 - (h % 2.0 - 1.0).abs());
    let (r, g, b) = match h as u32 {
        0 => (c, x, 0.0),
        1 => (x, c, 0.0),
        2 => (0.0, c, x),
        3 => (0.0, x, c),
        4 => (x, 0.0, c),
        _ => (c, 0.0, x),
    };
    let m = lightness - c / 2.0;
    let to_u8 = |v: f32| ((v + m) * 255.0).round().clamp(0.0, 255.0) as u8;
    (to_u8(r), to_u8(g), to_u8(b))
}

#[cfg(test)]
mod tests {
    use super::*;

    const ME: u32 = 1;

    fn map_with(present: &[u32]) -> SeatMap {
        let mut map = SeatMap::default();
        map.update(10, ME, present);
        map
    }

    #[test]
    fn me_sits_closest_to_camera() {
        let map = map_with(&[5, ME, 7]);
        assert_eq!(map.seat_of(ME), Some(0));
    }

    #[test]
    fn two_people_sit_face_to_face() {
        let map = map_with(&[ME, 2]);
        assert_eq!(map.seat_of(2), Some(4));
    }

    #[test]
    fn newcomers_spread_out() {
        let map = map_with(&[ME, 2, 3, 4]);
        // 对面，然后左右两边
        assert_eq!(map.seat_of(2), Some(4));
        assert_eq!(map.seat_of(3), Some(2));
        assert_eq!(map.seat_of(4), Some(6));
    }

    /// 有人走了，别人一个都不许动。
    #[test]
    fn leaving_does_not_move_anyone_else() {
        let mut map = map_with(&[ME, 2, 3, 4]);
        let before: Vec<_> = [ME, 2, 4].iter().map(|&id| map.seat_of(id)).collect();
        map.update(10, ME, &[ME, 2, 4]);
        let after: Vec<_> = [ME, 2, 4].iter().map(|&id| map.seat_of(id)).collect();
        assert_eq!(before, after);
        assert_eq!(map.seat_of(3), None);
    }

    #[test]
    fn nobody_else_takes_my_seat() {
        // 名单还没同步到自己那条的时候
        let others: Vec<u32> = (2..=9).collect();
        let map = map_with(&others);
        assert!(map.seats[0].is_none());
        assert_eq!(map.waiting(), &[9]);
    }

    #[test]
    fn ninth_person_waits_and_fills_the_first_gap() {
        let everyone: Vec<u32> = (1..=10).collect();
        let mut map = map_with(&everyone);
        assert_eq!(map.waiting(), &[9, 10]);

        let freed = map.seat_of(4).unwrap();
        let rest: Vec<u32> = everyone.iter().copied().filter(|&id| id != 4).collect();
        map.update(10, ME, &rest);
        assert_eq!(map.seat_of(9), Some(freed));
        assert_eq!(map.waiting(), &[10]);
    }

    #[test]
    fn waiting_person_who_leaves_is_forgotten() {
        let everyone: Vec<u32> = (1..=9).collect();
        let mut map = map_with(&everyone);
        map.update(10, ME, &everyone[..8]);
        assert!(map.waiting().is_empty());
    }

    #[test]
    fn switching_channel_starts_over() {
        let mut map = map_with(&[ME, 2, 3]);
        map.update(11, ME, &[ME, 3]);
        assert_eq!(map.seat_of(3), Some(4));
        assert_eq!(map.seat_of(2), None);
    }

    #[test]
    fn glyph_skips_common_prefixes() {
        assert_eq!(glyph("老张"), "张");
        assert_eq!(glyph("小鱼"), "鱼");
        assert_eq!(glyph("阿杰"), "杰");
        // 三个字的不跳：「阿斯顿」刻「阿」比刻「斯」好认
        assert_eq!(glyph("阿斯顿"), "阿");
        assert_eq!(glyph("冬瓜"), "冬");
        assert_eq!(glyph("kate"), "K");
        assert_eq!(glyph(""), "?");
    }

    #[test]
    fn same_key_same_stone() {
        assert_eq!(stone_seed(&[1, 2, 3]), stone_seed(&[1, 2, 3]));
        assert_ne!(stone_seed(&[1, 2, 3]), stone_seed(&[3, 2, 1]));
    }
}
