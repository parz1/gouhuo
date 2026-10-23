// SPDX-License-Identifier: MPL-2.0
//! 抖动缓冲。两种：
//!
//! - **固定深度**（[`JitterConfig::fixed`]）：M1 那个故意写得很朴素的版本，
//!   给出延迟的物理下限和一个诚实的基线。测量工具（latency-probe）还在用它，
//!   CI 卡的协议延迟就是用它量的。
//! - **自适应**（[`JitterConfig::adaptive`]）：语音链路用的。深度跟着网络走，
//!   延迟能涨也能降。
//!
//! # 固定深度的毛病
//!
//! 攒够 `target_frames` 帧才开始播；播着播着缓冲被抽空了（网络抖了一下），
//! 就退回预缓冲、从缓冲里最早的一帧重新对表。这一下的代价是**延迟永久地涨了
//! 抖动的那么多**，而且再也不下来 —— 网络好了，包照样准时到，只是每个包都要在
//! 缓冲里多等那么久。抖几次，延迟就是一级一级的台阶。
//!
//! # 自适应版怎么做
//!
//! 三件事，都跟 WebRTC 的 NetEQ 是一个思路：
//!
//! 1. **该缓冲多深，从到达时间里算出来。** 每个包的「相对迟到」= 它的传输延迟
//!    减去最近 2 秒里最小的传输延迟。把相对迟到记进一个会慢慢遗忘的直方图，
//!    目标深度取它的 97% 分位：97% 的包赶得上，剩下的 3% 交给 PLC。
//!    网络变差，分位数几百毫秒内就涨上去；变好了，几秒钟忘掉。
//!
//! 2. **缓冲空了不重新对表，而是停一下等它**（[`Playout::Stall`]）：调用方用 PLC
//!    顶上这一帧，序号不前进。等到了接着播，多出来的延迟交给第 3 条还回去。
//!    停太久（[`Adaptive::max_stall_frames`]）才真的放弃这一段。
//!
//! 3. **攒多了就加速播**：缓冲比目标多出一截时，调用方把几帧解出来、
//!    删掉一个基音周期再播（见 [`crate::timescale`] 和 `playout`）。
//!    这是延迟能降下来的那一半。缓冲只管报「现在有多少、目标是多少」，
//!    加不加速由 `playout` 决定 —— 它还知道自己手里攒着多少已经解出来的音频。
//!
//! 另外，发送端说完一句会发一个 terminator（[`JitterBuffer::end_at`]）。播到那儿
//! 就回到空闲；下一句按「发送时刻 + 目标延迟」起播，句子之间的停顿因此也保留住了，
//! 而且每一句开头都是一次免费的延迟重置 —— 停顿里没人会注意到缓冲变浅了。

use std::collections::{BTreeMap, VecDeque};

#[derive(Debug, Clone)]
pub enum Playout {
    /// 预缓冲中，或者这个人现在没在说话。输出静音。不计入延迟统计。
    Prebuffering,
    /// 正常出帧。`sent_ms` 是发送端采到这一帧的时刻（它自己的时钟），
    /// 没有时间信息时是 NaN。
    Frame {
        seq: u32,
        sent_ms: f64,
        payload: Vec<u8>,
    },
    /// 序号缺失但后续帧已到 —— 真丢包，走 PLC。
    Lost { seq: u32 },
    /// 该播的那一帧还没到，后面的也没到：用 PLC 顶上，**序号不前进**，
    /// 等它来了接着播。只有自适应模式会出这个。
    Stall,
    /// 这一段结束了（缓冲被抽空、或者说完了）。回到预缓冲。
    Underrun,
}

/// 自适应模式的参数。
#[derive(Debug, Clone, Copy)]
pub struct Adaptive {
    /// 一帧多少毫秒。算延迟要用。
    pub frame_ms: f64,
    /// 目标深度取相对迟到的哪个分位。0.97 = 97% 的包赶得上。
    pub quantile: f64,
    /// 目标深度的下限（帧）。
    pub min_frames: usize,
    /// 目标深度的上限（帧）。网络再烂也不为它攒超过这么多 ——
    /// 语音延迟过了几百毫秒就没法对话了，宁可丢包。
    pub max_target_frames: usize,
    /// 缓冲空了最多停着等多少帧，过了就放弃这一段、回到预缓冲。
    pub max_stall_frames: usize,
}

impl Adaptive {
    pub fn for_frame_ms(frame_ms: f64) -> Self {
        Self {
            frame_ms,
            quantile: 0.97,
            min_frames: 1,
            // 250 ms
            max_target_frames: (250.0 / frame_ms).ceil() as usize,
            // 200 ms：Opus 的 PLC 这么长以后早就淡成静音了，再等也没有意义。
            max_stall_frames: (200.0 / frame_ms).ceil() as usize,
        }
    }
}

#[derive(Debug, Clone, Copy)]
pub struct JitterConfig {
    /// 固定模式：起播深度（帧），延迟就是 `target_frames * 帧长`，一分不少。
    /// 自适应模式：还没攒够到达统计之前用的初始深度。
    pub target_frames: usize,
    /// 缓冲上限，防止对端猛灌把内存吃了。超了丢最旧的。
    pub max_frames: usize,
    /// `Some` = 自适应模式。
    pub adaptive: Option<Adaptive>,
}

impl JitterConfig {
    pub fn fixed(target_frames: usize) -> Self {
        let target = target_frames.max(1);
        Self {
            target_frames: target,
            max_frames: target * 8 + 8,
            adaptive: None,
        }
    }

    pub fn adaptive(frame_ms: f64) -> Self {
        let adaptive = Adaptive::for_frame_ms(frame_ms);
        Self {
            target_frames: 2,
            // 一秒。延迟尖峰过后包成串地到，得装得下；再多就是对端在猛灌。
            max_frames: (1000.0 / frame_ms).ceil() as usize,
            adaptive: Some(adaptive),
        }
    }
}

#[derive(Debug, Clone, Copy, Default)]
pub struct JitterStats {
    pub pushed: u64,
    /// 播放中到达但序号已经过去了 —— 网络抖动超出了缓冲深度。
    pub late: u64,
    pub duplicate: u64,
    /// 缓冲满被迫丢弃的最旧帧。
    pub overflow: u64,
    pub played: u64,
    pub lost: u64,
    pub underruns: u64,
    /// 停着等迟到的帧、用 PLC 顶上的次数（自适应模式）。
    pub stalls: u64,
    pub prebuffer_ticks: u64,
    pub max_depth: usize,
}

struct Entry {
    sent_ms: f64,
    payload: Vec<u8>,
}

pub struct JitterBuffer {
    cfg: JitterConfig,
    map: BTreeMap<u32, Entry>,
    next_seq: u32,
    playing: bool,
    /// 连续停了几帧。
    stalled: usize,
    /// 这一段说到哪个序号为止（terminator 的序号，它本身不是音频）。
    end_seq: Option<u32>,
    /// 收到过的最大序号。算「前面还攒着多少」用。
    highest_seq: Option<u32>,
    delay: DelayEstimator,
    pub stats: JitterStats,
}

impl JitterBuffer {
    pub fn new(cfg: JitterConfig) -> Self {
        Self {
            cfg,
            map: BTreeMap::new(),
            next_seq: 0,
            playing: false,
            stalled: 0,
            end_seq: None,
            highest_seq: None,
            delay: DelayEstimator::new(cfg.adaptive.map_or(0.97, |a| a.quantile)),
            stats: JitterStats::default(),
        }
    }

    pub fn depth(&self) -> usize {
        self.map.len()
    }

    pub fn is_playing(&self) -> bool {
        self.playing
    }

    pub fn is_adaptive(&self) -> bool {
        self.cfg.adaptive.is_some()
    }

    /// 收到一帧，没有时间信息。固定模式用。
    ///
    /// 注意：seq 的 u32 回绕这里没处理。20 ms 帧下要跑 994 天才绕一圈，
    /// 一次通话不可能跨这个跨度，而重连会换会话从头来。
    pub fn push(&mut self, seq: u32, payload: Vec<u8>) {
        self.insert(seq, f64::NAN, payload);
    }

    /// 收到一帧。`payload` 是**已解密**的 Opus 负载。
    ///
    /// - `sent_ms`：发送端采到这一帧的时刻，从包头的采样时间戳换算来，
    ///   用的是发送端自己的时钟 —— 只拿来算差值，跟本地时钟差多少无所谓
    /// - `arrived_ms`：本地收到的时刻
    ///
    /// **不能拿 seq 当发送时刻**：发送端不说话时不发包，seq 也不涨，
    /// 停顿之后的第一个包会被当成迟到了整整一个停顿那么久。
    pub fn push_at(&mut self, seq: u32, sent_ms: f64, payload: Vec<u8>, arrived_ms: f64) {
        if self.cfg.adaptive.is_some() && sent_ms.is_finite() && arrived_ms.is_finite() {
            self.delay.observe(sent_ms, arrived_ms);
        }
        self.insert(seq, sent_ms, payload);
    }

    fn insert(&mut self, seq: u32, sent_ms: f64, payload: Vec<u8>) {
        self.stats.pushed += 1;

        if self.playing && seq < self.next_seq {
            self.stats.late += 1;
            return;
        }
        if self.map.contains_key(&seq) {
            self.stats.duplicate += 1;
            return;
        }
        if self.map.len() >= self.cfg.max_frames {
            self.stats.overflow += 1;
            if let Some(&oldest) = self.map.keys().next() {
                self.map.remove(&oldest);
            }
        }
        self.map.insert(seq, Entry { sent_ms, payload });
        self.highest_seq = Some(self.highest_seq.map_or(seq, |h| h.max(seq)));
        self.stats.max_depth = self.stats.max_depth.max(self.map.len());
    }

    /// 发送端说完了：`seq` 是 terminator 自己的序号。播到它就回到空闲。
    pub fn end_at(&mut self, seq: u32) {
        self.end_seq = Some(seq);
    }

    /// 现在该缓冲多深（帧）。固定模式就是配置的那个数。
    pub fn target_frames(&self) -> usize {
        let Some(adaptive) = self.cfg.adaptive else {
            return self.cfg.target_frames;
        };
        match self.delay.quantile() {
            Some(ms) => ((ms / adaptive.frame_ms).ceil() as usize)
                .clamp(adaptive.min_frames, adaptive.max_target_frames),
            None => self.cfg.target_frames,
        }
    }

    /// 现在的目标延迟，毫秒。
    pub fn target_ms(&self) -> f64 {
        let frame_ms = self.cfg.adaptive.map_or(f64::NAN, |a| a.frame_ms);
        self.target_frames() as f64 * frame_ms
    }

    /// 这一帧该在「发送后多久」播出来：最近的最小传输延迟 + 目标深度。
    /// 起播用的就是这个数（见 `ready_to_start`），加速也往这个数上收。
    /// 固定模式、或者还没见过带时间的包时是 `None`。
    pub fn target_delay_ms(&self) -> Option<f64> {
        self.cfg.adaptive?;
        Some(self.delay.min_delay()? + self.target_ms())
    }

    /// 前面还攒着多少帧：从下一个要播的到收到过的最大序号（含中间丢的）。
    /// 没在播的时候是 0。
    pub fn level(&self) -> usize {
        match (self.playing, self.highest_seq) {
            (true, Some(high)) if high >= self.next_seq => (high - self.next_seq + 1) as usize,
            _ => 0,
        }
    }

    pub fn buffered_frames(&self) -> usize {
        self.level().max(self.map.len())
    }

    /// Adaptive live voice must not replay an arbitrarily old queue after an
    /// outage. Discards are local policy, not inferred network packet loss.
    pub fn discard_excess(&mut self, max_frames: usize) -> u64 {
        if self.cfg.adaptive.is_none() || self.buffered_frames() <= max_frames {
            return 0;
        }
        let Some(high) = self.highest_seq else {
            return 0;
        };
        let first = high.saturating_sub(max_frames.saturating_sub(1) as u32);
        let before = self.map.len();
        self.map.retain(|&seq, _| seq >= first);
        if self.playing {
            self.next_seq = self.next_seq.max(first);
        }
        self.stalled = 0;
        (before - self.map.len()) as u64
    }

    /// 接下来连续有几帧已经到了（从下一个要播的开始数）。
    /// 加速要一口气解几帧出来，先问一下够不够。
    pub fn ready_ahead(&self) -> usize {
        let mut n = 0;
        while self.map.contains_key(&self.next_seq.wrapping_add(n)) {
            n += 1;
        }
        n as usize
    }

    /// 播放线程每个帧周期调一次。固定模式用这个。
    pub fn pop(&mut self) -> Playout {
        self.pop_at(f64::NAN)
    }

    /// 播放线程每个帧周期调一次。`now_ms` 跟 [`JitterBuffer::push_at`] 的
    /// `arrived_ms` 是同一个时钟；自适应模式靠它决定什么时候起播。
    pub fn pop_at(&mut self, now_ms: f64) -> Playout {
        if !self.playing {
            if !self.ready_to_start(now_ms) {
                self.stats.prebuffer_ticks += 1;
                return Playout::Prebuffering;
            }
            // 起播：对表到缓冲里最早的一帧。欠载重启后同样走这里重新对表。
            self.next_seq = *self.map.keys().next().expect("ready_to_start 判过非空");
            self.playing = true;
            self.stalled = 0;
        }

        // 说完了：terminator 自己不是音频，播到它就回到空闲。
        // 下一句（如果已经到了）按自己的发送时刻重新起播，停顿就保留住了。
        if self.end_seq.is_some_and(|end| self.next_seq >= end) {
            let end = self.end_seq.take().expect("just checked");
            // 这一句里比 terminator 还早、却还没播的，已经没用了。
            self.map.retain(|&seq, _| seq > end);
            self.playing = false;
            return Playout::Underrun;
        }

        if let Some(entry) = self.map.remove(&self.next_seq) {
            let seq = self.next_seq;
            self.next_seq = self.next_seq.wrapping_add(1);
            self.stalled = 0;
            self.stats.played += 1;
            return Playout::Frame {
                seq,
                sent_ms: entry.sent_ms,
                payload: entry.payload,
            };
        }

        if !self.map.is_empty() {
            let seq = self.next_seq;
            self.next_seq = self.next_seq.wrapping_add(1);
            self.stalled = 0;
            self.stats.lost += 1;
            return Playout::Lost { seq };
        }

        // 缓冲空了。
        if let Some(adaptive) = self.cfg.adaptive {
            if self.stalled < adaptive.max_stall_frames {
                self.stalled += 1;
                self.stats.stalls += 1;
                return Playout::Stall;
            }
        }
        self.stats.underruns += 1;
        self.playing = false;
        Playout::Underrun
    }

    /// 没在播的时候，现在能不能起播。
    fn ready_to_start(&self, now_ms: f64) -> bool {
        let Some((_, first)) = self.map.iter().next() else {
            return false;
        };
        let (Some(adaptive), true) = (self.cfg.adaptive, now_ms.is_finite()) else {
            // 固定模式：数帧。
            return self.map.len() >= self.cfg.target_frames;
        };
        let (Some(min_delay), true) = (self.delay.min_delay(), first.sent_ms.is_finite()) else {
            return self.map.len() >= self.cfg.target_frames;
        };
        // 这一帧最早可能到的时刻是 发送时刻 + 最近的最小传输延迟；
        // 从那儿再等目标深度那么久就起播。它到得晚的话，等的就少，
        // 总延迟还是同一个数 —— 这正是「延迟由目标决定，不由第一个包的运气决定」。
        let _ = adaptive;
        now_ms >= first.sent_ms + min_delay + self.target_ms()
    }
}

/// 从到达时间里估计该缓冲多深。见模块文档第 1 条。
struct DelayEstimator {
    /// 最近 [`MIN_WINDOW_MS`] 里的传输延迟，单调递增 —— 队头就是窗口里的最小值。
    recent: VecDeque<(f64, f64)>,
    /// 相对迟到的直方图，每格 [`BUCKET_MS`]。会慢慢遗忘，见 [`FORGET`]。
    histogram: Vec<f64>,
    /// 下一个样本记多重。遗忘不是把旧的全乘一遍 `FORGET`（每个包都要扫一遍
    /// 直方图），而是让新样本越记越重 —— 比例一样，只有分位数用得着，
    /// 而分位数只看比例。攒得太大了整体缩一次。
    weight: f64,
    quantile: f64,
    /// 缓存的分位数。每 [`REQUANTILE_EVERY`] 个包才重算一次：播放线程每拍都要问，
    /// 而它几十毫秒才会变一点。
    cached: Option<f64>,
    /// 上次重算之后又来了几个包。
    since_requantile: u64,
}

const REQUANTILE_EVERY: u64 = 8;

/// 「最小传输延迟」看多长的窗口。
///
/// 太长的话，路由一换、基础延迟整体涨了，还要拿几分钟前的最小值去比，
/// 每个包都显得迟到了一大截；太短的话，最小值本身就在抖。2 秒是 WebRTC 的取值。
const MIN_WINDOW_MS: f64 = 2000.0;
const BUCKET_MS: f64 = 2.0;
const MAX_TRACKED_MS: f64 = 1000.0;

/// 每来一个包，旧的统计乘一次这个。10 ms 一包时时间常数约 2 秒：
/// 网络变差时几百毫秒内分位数就涨上去（新样本权重够），变好了几秒钟忘掉。
const FORGET: f64 = 0.995;

impl DelayEstimator {
    fn new(quantile: f64) -> Self {
        Self {
            recent: VecDeque::new(),
            histogram: vec![0.0; (MAX_TRACKED_MS / BUCKET_MS) as usize + 1],
            weight: 1.0,
            quantile,
            cached: None,
            since_requantile: 0,
        }
    }

    fn observe(&mut self, sent_ms: f64, arrived_ms: f64) {
        let delay = arrived_ms - sent_ms;
        while self.recent.back().is_some_and(|&(_, d)| d >= delay) {
            self.recent.pop_back();
        }
        self.recent.push_back((arrived_ms, delay));
        while self
            .recent
            .front()
            .is_some_and(|&(t, _)| t < arrived_ms - MIN_WINDOW_MS)
        {
            self.recent.pop_front();
        }
        let min = self.recent.front().map_or(delay, |&(_, d)| d);

        let relative = (delay - min).clamp(0.0, MAX_TRACKED_MS);
        self.weight /= FORGET;
        let bucket = (relative / BUCKET_MS) as usize;
        let last = self.histogram.len() - 1;
        self.histogram[bucket.min(last)] += self.weight;
        if self.weight > 1e30 {
            for w in &mut self.histogram {
                *w /= self.weight;
            }
            self.weight = 1.0;
        }

        self.since_requantile += 1;
        if self.cached.is_none() || self.since_requantile >= REQUANTILE_EVERY {
            self.since_requantile = 0;
            self.cached = self.compute_quantile();
        }
    }

    fn min_delay(&self) -> Option<f64> {
        self.recent.front().map(|&(_, d)| d)
    }

    /// 相对迟到的分位数，毫秒。还没见过包就是 `None`。
    fn quantile(&self) -> Option<f64> {
        self.cached
    }

    fn compute_quantile(&self) -> Option<f64> {
        let q = self.quantile;
        let total: f64 = self.histogram.iter().sum();
        if total <= 0.0 {
            return None;
        }
        let mut seen = 0.0;
        for (i, weight) in self.histogram.iter().enumerate() {
            seen += weight;
            if seen >= q * total {
                return Some((i + 1) as f64 * BUCKET_MS);
            }
        }
        Some(MAX_TRACKED_MS)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn buf(target: usize) -> JitterBuffer {
        JitterBuffer::new(JitterConfig::fixed(target))
    }

    fn played(p: &Playout) -> Option<u32> {
        match p {
            Playout::Frame { seq, .. } => Some(*seq),
            _ => None,
        }
    }

    #[test]
    fn prebuffers_before_playing() {
        let mut b = buf(3);
        b.push(0, vec![0]);
        assert!(matches!(b.pop(), Playout::Prebuffering));
        b.push(1, vec![1]);
        assert!(matches!(b.pop(), Playout::Prebuffering));
        b.push(2, vec![2]);
        assert_eq!(played(&b.pop()), Some(0));
        assert_eq!(played(&b.pop()), Some(1));
        assert_eq!(played(&b.pop()), Some(2));
        assert_eq!(b.stats.prebuffer_ticks, 2);
    }

    #[test]
    fn reorders_within_depth() {
        let mut b = buf(3);
        for seq in [2u32, 0, 1] {
            b.push(seq, vec![seq as u8]);
        }
        assert_eq!(played(&b.pop()), Some(0));
        assert_eq!(played(&b.pop()), Some(1));
        assert_eq!(played(&b.pop()), Some(2));
        assert_eq!(b.stats.late, 0);
    }

    #[test]
    fn gap_becomes_plc_not_stall() {
        let mut b = buf(2);
        b.push(0, vec![0]);
        b.push(2, vec![2]); // 1 号丢了
        assert_eq!(played(&b.pop()), Some(0));
        assert!(matches!(b.pop(), Playout::Lost { seq: 1 }));
        assert_eq!(played(&b.pop()), Some(2));
        assert_eq!(b.stats.lost, 1);
    }

    #[test]
    fn underrun_reenters_prebuffer() {
        let mut b = buf(2);
        b.push(0, vec![0]);
        b.push(1, vec![1]);
        assert_eq!(played(&b.pop()), Some(0));
        assert_eq!(played(&b.pop()), Some(1));
        assert!(matches!(b.pop(), Playout::Underrun));
        assert!(!b.is_playing());
        // 重新攒够才播 —— 这一次重新预缓冲就是固定缓冲的延迟阶梯
        b.push(2, vec![2]);
        assert!(matches!(b.pop(), Playout::Prebuffering));
        b.push(3, vec![3]);
        assert_eq!(played(&b.pop()), Some(2));
        assert_eq!(b.stats.underruns, 1);
    }

    #[test]
    fn late_packet_dropped_while_playing() {
        let mut b = buf(2);
        b.push(5, vec![5]);
        b.push(6, vec![6]);
        assert_eq!(played(&b.pop()), Some(5));
        b.push(4, vec![4]); // 抖动超出深度
        assert_eq!(b.stats.late, 1);
        assert_eq!(played(&b.pop()), Some(6));
    }

    #[test]
    fn duplicate_is_counted_not_played_twice() {
        let mut b = buf(2);
        b.push(0, vec![0]);
        b.push(0, vec![0]);
        b.push(1, vec![1]);
        assert_eq!(b.stats.duplicate, 1);
        assert_eq!(played(&b.pop()), Some(0));
        assert_eq!(played(&b.pop()), Some(1));
    }

    #[test]
    fn overflow_drops_oldest() {
        let cfg = JitterConfig {
            target_frames: 2,
            max_frames: 4,
            adaptive: None,
        };
        let mut b = JitterBuffer::new(cfg);
        for seq in 0..6u32 {
            b.push(seq, vec![seq as u8]);
        }
        assert_eq!(b.stats.overflow, 2);
        assert_eq!(b.depth(), 4);
        assert_eq!(played(&b.pop()), Some(2));
    }

    // ---- 自适应 ----

    const FRAME: f64 = 10.0;

    fn adaptive() -> JitterBuffer {
        JitterBuffer::new(JitterConfig::adaptive(FRAME))
    }

    /// 喂一段到达时间：第 k 帧在 `k * FRAME + owd + jitter(k)` 到。
    fn feed(b: &mut JitterBuffer, frames: std::ops::Range<u32>, jitter: impl Fn(u32) -> f64) {
        for k in frames {
            let sent = k as f64 * FRAME;
            b.push_at(k, sent, vec![0], sent + 20.0 + jitter(k));
        }
    }

    #[test]
    fn a_clean_network_needs_only_a_shallow_buffer() {
        let mut b = adaptive();
        feed(&mut b, 0..500, |_| 0.0);
        assert_eq!(b.target_frames(), 1);
    }

    #[test]
    fn the_target_grows_with_jitter_and_comes_back_down() {
        let mut b = adaptive();
        // 每 5 帧里有一帧晚到 35 ms：20% 的包要等 35 ms，97% 分位就是 35 ms 上下
        feed(&mut b, 0..500, |k| if k % 5 == 0 { 35.0 } else { 0.0 });
        let rough = b.target_frames();
        assert!((4..=5).contains(&rough), "网络抖的时候目标 {rough} 帧");

        // 网络好了：几秒钟后目标回到最浅
        feed(&mut b, 500..1500, |_| 0.0);
        assert_eq!(b.target_frames(), 1, "网络好了目标没降下来");
    }

    /// 零星一两个特别晚的包不该把目标拉上去 —— 那是 3% 里的，交给 PLC。
    #[test]
    fn a_rare_outlier_does_not_move_the_target() {
        let mut b = adaptive();
        feed(&mut b, 0..500, |k| if k == 250 { 300.0 } else { 0.0 });
        assert_eq!(b.target_frames(), 1);
    }

    /// 只看相对迟到：基础延迟大不代表要缓冲深。
    #[test]
    fn a_long_but_steady_path_needs_no_extra_buffer() {
        let mut b = adaptive();
        for k in 0..500u32 {
            let sent = k as f64 * FRAME;
            b.push_at(k, sent, vec![0], sent + 180.0);
        }
        assert_eq!(b.target_frames(), 1);
    }

    #[test]
    fn the_target_is_capped() {
        let mut b = adaptive();
        feed(&mut b, 0..500, |k| if k % 2 == 0 { 900.0 } else { 0.0 });
        assert_eq!(
            b.target_frames(),
            Adaptive::for_frame_ms(FRAME).max_target_frames
        );
    }

    /// 起播按时间：第一个包到得晚，就等得少，总延迟不变。
    #[test]
    fn playout_starts_at_send_time_plus_target() {
        let mut b = adaptive();
        // 上一句：最小传输延迟 20 ms，网络干净，目标 1 帧 = 10 ms。播完、收尾。
        feed(&mut b, 0..300, |_| 0.0);
        b.end_at(300);
        while !matches!(b.pop_at(1e9), Playout::Underrun) {}

        // 下一句的第一个包晚到了 5 ms
        let sent = 301.0 * FRAME;
        b.push_at(301, sent, vec![0], sent + 25.0);
        assert!(matches!(b.pop_at(sent + 29.0), Playout::Prebuffering));
        assert_eq!(
            played(&b.pop_at(sent + 30.0)),
            Some(301),
            "发送时刻 + 最小延迟 20 + 目标 10 就该起播"
        );
    }

    #[test]
    fn an_empty_buffer_stalls_instead_of_resyncing() {
        let mut b = adaptive();
        feed(&mut b, 0..3, |_| 0.0);
        let now = 1000.0;
        for k in 0..3 {
            assert_eq!(played(&b.pop_at(now)), Some(k));
        }
        // 3 号还没到：停着等，序号不前进
        assert!(matches!(b.pop_at(now), Playout::Stall));
        assert!(matches!(b.pop_at(now), Playout::Stall));
        assert!(b.is_playing());
        // 它来了：接着播 3 号，而不是被当成迟到丢掉
        b.push_at(3, 30.0, vec![3], now);
        assert_eq!(played(&b.pop_at(now)), Some(3));
        assert_eq!(b.stats.stalls, 2);
        assert_eq!(b.stats.late, 0);
    }

    #[test]
    fn a_stall_that_lasts_too_long_gives_up() {
        let mut b = adaptive();
        feed(&mut b, 0..1, |_| 0.0);
        let now = 1000.0;
        assert_eq!(played(&b.pop_at(now)), Some(0));
        let limit = Adaptive::for_frame_ms(FRAME).max_stall_frames;
        for _ in 0..limit {
            assert!(matches!(b.pop_at(now), Playout::Stall));
        }
        assert!(matches!(b.pop_at(now), Playout::Underrun));
        assert!(!b.is_playing());
    }

    /// 说完了就收尾，不停着等一个永远不会来的包。
    #[test]
    fn a_terminator_ends_the_talkspurt_without_stalling() {
        let mut b = adaptive();
        feed(&mut b, 0..3, |_| 0.0);
        b.end_at(3);
        let now = 1000.0;
        for k in 0..3 {
            assert_eq!(played(&b.pop_at(now)), Some(k));
        }
        assert!(matches!(b.pop_at(now), Playout::Underrun));
        assert_eq!(b.stats.stalls, 0);
        assert_eq!(b.stats.lost, 0, "terminator 自己不该被当成丢了一帧");
    }

    /// 下一句已经到了也要先收尾，再按它自己的发送时刻起播 —— 停顿要保留。
    #[test]
    fn the_pause_between_sentences_is_kept() {
        let mut b = adaptive();
        feed(&mut b, 0..3, |_| 0.0);
        b.end_at(3);
        // 下一句：发送端停了 500 ms 才又开口（序号接着 4）
        let sent = 3.0 * FRAME + 500.0;
        b.push_at(4, sent, vec![4], sent + 20.0);

        let mut now = 50.0;
        for k in 0..3 {
            assert_eq!(played(&b.pop_at(now)), Some(k));
            now += FRAME;
        }
        assert!(matches!(b.pop_at(now), Playout::Underrun));
        // 停顿还没过完，不能马上接着播下一句
        assert!(matches!(b.pop_at(now + FRAME), Playout::Prebuffering));
        assert_eq!(played(&b.pop_at(sent + 20.0 + FRAME)), Some(4));
    }

    #[test]
    fn level_counts_what_is_waiting_including_gaps() {
        let mut b = adaptive();
        feed(&mut b, 0..5, |_| 0.0);
        b.push_at(8, 80.0, vec![8], 100.0);
        assert_eq!(b.level(), 0, "没在播的时候是 0");
        assert_eq!(played(&b.pop_at(1000.0)), Some(0));
        // 下一个要播 1 号，收到过的最大是 8 号：前面还有 8 帧（5–7 是空的）
        assert_eq!(b.level(), 8);
        assert_eq!(b.ready_ahead(), 4);
    }
}
