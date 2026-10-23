// SPDX-License-Identifier: MPL-2.0

//! 一个说话人的播放：抖动缓冲 + 解码 + 时间伸缩，每拍吐一帧音频。
//!
//! 抖动缓冲只管包（[`crate::jitter`]），时间伸缩只管音频（[`crate::timescale`]），
//! 这里把两边接起来，并决定**什么时候加速**：
//!
//! - 解出来的音频先进一个小 FIFO，播放线程每拍从里面取一帧
//! - 实际的播放延迟比目标延迟多出一帧，就一口气解 30 ms，删掉一个基音周期再放进
//!   FIFO —— 延迟降下来就靠这个。一直加速到只多出不到 1/4 帧为止
//! - 该播的包没到（[`Playout::Stall`]）或者丢了，让解码器自己编一帧（PLC）
//!
//! # 为什么盯延迟，不盯缓冲水位
//!
//! 第一版盯的是「缓冲里攒了几帧」，比目标多一帧就加速。模拟器一跑就露馅：
//! 水位这个量本身跟着到达抖动在跳，只能留一帧的余量，结果每次卡顿之后都停在余量的
//! 上沿，比卡之前多出 17 ms，再也回不去（同城场景，31.6 → 48.6 ms）。
//!
//! 播放延迟（现在 − 这一帧的发送时刻）就干净得多：播放时钟是均匀的，所以它只在
//! 停着等（涨）和加速（降）的时候变，不跟着到达抖动跳。而它该是多少也有现成的数 ——
//! 起播时用的「最小传输延迟 + 目标深度」（[`JitterBuffer::target_delay_ms`]）。
//! 往那个数上收，卡完之后就回到卡之前的位置。
//!
//! 语音链路、测量工具、离线模拟器用的都是这一份，所以模拟器里量出来的
//! 就是线上那套逻辑的行为。

use std::collections::VecDeque;

use crate::jitter::{JitterBuffer, JitterConfig, Playout};
use crate::timescale::{self, MIN_INPUT};

/// 采样率。跟整条链路一致。
const RATE: f64 = 48_000.0;

/// 「多出来的延迟」的平滑系数，每解一帧一次。目标延迟里的最小传输延迟会随着
/// 2 秒窗口滑动而跳几毫秒，平滑掉，免得为这个加速。时间常数约 100 ms。
const EXCESS_SMOOTHING: f64 = 0.9;

/// 延迟比目标多出几帧才开始加速。
const ACCELERATE_START: f64 = 1.0;

/// 加速到只多出几帧就停。跟开始的门槛拉开，免得在门槛附近一下开一下关。
const ACCELERATE_STOP: f64 = 0.25;

/// 解码器。抽成 trait 是为了模拟器和测试能换一个不用 Opus 的。
pub trait FrameDecoder {
    /// 解一帧到 `out`（长度 = 一帧）。失败返回 `false`，调用方填静音。
    fn decode(&mut self, payload: &[u8], out: &mut [f32]) -> bool;
    /// 丢了一帧：编一帧出来（PLC）。**必须调**，不能直接塞静音 ——
    /// 解码器有内部状态，跳过一帧会让下一个真包也解错。
    fn conceal(&mut self, out: &mut [f32]) -> bool;
}

#[cfg(feature = "codec")]
impl FrameDecoder for crate::codec::VoiceDecoder {
    fn decode(&mut self, payload: &[u8], out: &mut [f32]) -> bool {
        crate::codec::VoiceDecoder::decode(self, payload, out).is_ok()
    }

    fn conceal(&mut self, out: &mut [f32]) -> bool {
        crate::codec::VoiceDecoder::conceal(self, out).is_ok()
    }
}

#[derive(Debug, Clone, Copy, Default)]
pub struct PlaybackStats {
    /// 加速了几次。
    pub accelerations: u64,
    /// 加速一共删掉了多少毫秒 —— 也就是还回去了多少延迟。
    pub removed_ms: f64,
    /// PLC 编了几帧（丢包 + 停着等）。
    pub concealed: u64,
}

pub struct Playback<D> {
    quality: crate::quality::QualityTracker,
    jitter: JitterBuffer,
    decoder: D,
    /// 一帧多少个采样。
    frame: usize,
    /// 解出来、还没播的音频。
    fifo: VecDeque<f32>,
    scratch: Vec<f32>,
    /// 平滑过的「实际延迟 − 目标延迟」，毫秒。没在播的时候是 `None`。
    excess_ms: Option<f64>,
    /// 正在往下收延迟（见 [`ACCELERATE_START`] / [`ACCELERATE_STOP`]）。
    accelerating: bool,
    /// 一帧多少毫秒。
    frame_ms: f64,
    /// 加速一次要一口气解几帧。
    accelerate_frames: usize,
    /// 模拟器和测量工具要逐帧的延迟；线上不记。
    delays: Option<Vec<f64>>,
    pub stats: PlaybackStats,
}

impl<D: FrameDecoder> Playback<D> {
    pub fn new(cfg: JitterConfig, decoder: D, frame_samples: usize) -> Self {
        Self {
            quality: crate::quality::QualityTracker::default(),
            jitter: JitterBuffer::new(cfg),
            decoder,
            frame: frame_samples,
            fifo: VecDeque::with_capacity(MIN_INPUT * 2),
            scratch: vec![0.0; frame_samples],
            excess_ms: None,
            accelerating: false,
            frame_ms: frame_samples as f64 * 1000.0 / RATE,
            accelerate_frames: MIN_INPUT.div_ceil(frame_samples),
            delays: None,
            stats: PlaybackStats::default(),
        }
    }

    /// 开始记每一帧的延迟：从发送端采到它，到它的第一个采样开始播。
    /// 只有发送时刻和本地时钟是同一个原点时（模拟器、测量工具）才有意义。
    pub fn record_delays(&mut self) {
        self.delays = Some(Vec::new());
    }

    pub fn take_delays(&mut self) -> Vec<f64> {
        self.delays.as_mut().map(std::mem::take).unwrap_or_default()
    }

    pub fn jitter(&self) -> &JitterBuffer {
        &self.jitter
    }

    pub fn decoder(&self) -> &D {
        &self.decoder
    }

    /// 收到一帧。参数见 [`JitterBuffer::push_at`]。
    pub fn push(&mut self, seq: u32, sent_ms: f64, payload: Vec<u8>, arrived_ms: f64) {
        let late = self.jitter.stats.late;
        let overflow = self.jitter.stats.overflow;
        self.jitter.push_at(seq, sent_ms, payload, arrived_ms);
        self.quality.packet(
            seq,
            sent_ms,
            arrived_ms,
            self.jitter.stats.late > late,
            self.jitter.stats.overflow > overflow,
        );
    }

    /// 发送端说完了，见 [`JitterBuffer::end_at`]。
    pub fn end_at(&mut self, seq: u32) {
        self.jitter.end_at(seq);
        self.quality.idle();
    }

    pub fn quality(&self, now_ms: f64) -> crate::quality::ReceiveQuality {
        self.quality.snapshot(
            now_ms,
            self.jitter.buffered_frames() as f64 * self.frame_ms
                + self.fifo.len() as f64 * 1000.0 / RATE,
            self.jitter.target_frames() as f64 * self.frame_ms,
        )
    }

    /// 取这一拍要播的一帧，写进 `out`（长度 = 一帧）。
    ///
    /// 返回 `false` = 这个人现在没声音，不用混进去。
    pub fn pull(&mut self, now_ms: f64, out: &mut [f32]) -> bool {
        debug_assert_eq!(out.len(), self.frame);
        let discarded = self
            .jitter
            .discard_excess((300.0 / self.frame_ms).ceil() as usize);
        if discarded > 0 {
            self.quality.discarded(now_ms, discarded);
        }
        while self.fifo.len() < self.frame && self.refill(now_ms) {}
        if !self.jitter.is_playing() {
            self.excess_ms = None;
            self.accelerating = false;
        }
        if self.fifo.is_empty() {
            return false;
        }
        // 不够一帧（这一段刚说完，剩个尾巴）就补零。
        for sample in out.iter_mut() {
            *sample = self.fifo.pop_front().unwrap_or(0.0);
        }
        true
    }

    /// 从抖动缓冲里再取一帧解进 FIFO。缓冲给不出东西了返回 `false`。
    fn refill(&mut self, now_ms: f64) -> bool {
        match self.jitter.pop_at(now_ms) {
            Playout::Frame {
                sent_ms, payload, ..
            } => {
                // 这一帧的第一个采样要等 FIFO 里排在前面的都播完才轮到。
                let delay = now_ms + self.fifo.len() as f64 * 1000.0 / RATE - sent_ms;
                self.quality.played(now_ms);
                self.observe_delay(delay);
                if !self.decoder.decode(&payload, &mut self.scratch) {
                    self.scratch.fill(0.0);
                }
                if self.should_accelerate() {
                    self.accelerate(now_ms);
                } else {
                    self.fifo.extend(self.scratch.iter());
                }
                true
            }
            event @ (Playout::Lost { .. } | Playout::Stall) => {
                self.quality
                    .concealed(now_ms, matches!(event, Playout::Lost { .. }));
                if !self.decoder.conceal(&mut self.scratch) {
                    self.scratch.fill(0.0);
                }
                self.stats.concealed += 1;
                self.fifo.extend(self.scratch.iter());
                true
            }
            Playout::Underrun => {
                self.quality.idle();
                false
            }
            Playout::Prebuffering => false,
        }
    }

    /// 记下一帧的播放延迟，更新「多出来多少」和要不要加速。
    fn observe_delay(&mut self, delay_ms: f64) {
        if !delay_ms.is_finite() {
            return;
        }
        if let Some(delays) = &mut self.delays {
            delays.push(delay_ms);
        }
        let Some(target) = self.jitter.target_delay_ms() else {
            return;
        };
        let excess = delay_ms - target;
        let smoothed = match self.excess_ms {
            None => excess,
            Some(e) => e * EXCESS_SMOOTHING + excess * (1.0 - EXCESS_SMOOTHING),
        };
        self.excess_ms = Some(smoothed);
        if smoothed > ACCELERATE_START * self.frame_ms {
            self.accelerating = true;
        } else if smoothed < ACCELERATE_STOP * self.frame_ms {
            self.accelerating = false;
        }
    }

    /// 延迟多出来了，而且后面连续够几帧可以一起解。
    fn should_accelerate(&self) -> bool {
        self.accelerating && self.jitter.ready_ahead() + 1 >= self.accelerate_frames
    }

    /// 手里已经解好一帧（在 `scratch` 里）。再解几帧凑够 30 ms，删掉一个基音周期。
    fn accelerate(&mut self, now_ms: f64) {
        let mut chunk = self.scratch.clone();
        while chunk.len() < MIN_INPUT {
            match self.jitter.pop_at(now_ms) {
                Playout::Frame {
                    sent_ms, payload, ..
                } => {
                    self.quality.played(now_ms);
                    // 这几帧会跟着 chunk 一起被缩短，这里记的是缩之前的延迟 ——
                    // 偏大一点点（最多一个基音周期），不影响判断。
                    if let Some(delays) = &mut self.delays {
                        if sent_ms.is_finite() {
                            let queued_ms = (self.fifo.len() + chunk.len()) as f64 * 1000.0 / RATE;
                            delays.push(now_ms + queued_ms - sent_ms);
                        }
                    }
                    if !self.decoder.decode(&payload, &mut self.scratch) {
                        self.scratch.fill(0.0);
                    }
                    chunk.extend_from_slice(&self.scratch);
                }
                // should_accelerate 查过后面连续有帧，走不到这里；真走到了
                // （比如正好碰上 terminator）就拿手里这些原样播。
                _ => break,
            }
        }
        match timescale::accelerate(&chunk) {
            Some(shorter) => {
                let removed = chunk.len() - shorter.len();
                self.stats.accelerations += 1;
                self.stats.removed_ms += removed as f64 * 1000.0 / RATE;
                // 多出来的延迟立刻按删掉的量往下扣，不然平滑滤波要好一阵才反应过来，
                // 这段时间里会连着多加速好几次，冲过头。
                let removed_ms = removed as f64 * 1000.0 / RATE;
                if let Some(excess) = &mut self.excess_ms {
                    *excess -= removed_ms;
                    if *excess < ACCELERATE_STOP * self.frame_ms {
                        self.accelerating = false;
                    }
                }
                self.fifo.extend(shorter);
            }
            None => self.fifo.extend(chunk),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::jitter::JitterConfig;

    const FRAME: usize = 480;
    const FRAME_MS: f64 = 10.0;

    /// 不用 Opus 的解码器：包里装着帧号，按帧号生成一段连续的「浊音」。
    /// 加速能找到周期，跟真人声的行为一样。
    struct Tone {
        position: usize,
    }

    impl Tone {
        fn render(&mut self, start: usize, out: &mut [f32]) {
            for (i, s) in out.iter_mut().enumerate() {
                let t = (start + i) as f32 / RATE as f32;
                *s = (2.0 * std::f32::consts::PI * 150.0 * t).sin() * 0.3;
            }
            self.position = start + out.len();
        }
    }

    impl FrameDecoder for Tone {
        fn decode(&mut self, payload: &[u8], out: &mut [f32]) -> bool {
            let seq = u32::from_le_bytes(payload.try_into().unwrap()) as usize;
            self.render(seq * FRAME, out);
            true
        }
        fn conceal(&mut self, out: &mut [f32]) -> bool {
            let at = self.position;
            self.render(at, out);
            true
        }
    }

    fn playback() -> Playback<Tone> {
        let mut p = Playback::new(
            JitterConfig::adaptive(FRAME_MS),
            Tone { position: 0 },
            FRAME,
        );
        p.record_delays();
        p
    }

    fn packet(seq: u32) -> Vec<u8> {
        seq.to_le_bytes().to_vec()
    }

    /// 按到达时刻把包交给缓冲，每拍取一帧 —— 跟真的网络一样，
    /// 包没「到」之前缓冲是看不见它的。返回最后一拍之后的时刻。
    fn drive(
        p: &mut Playback<Tone>,
        frames: std::ops::Range<u32>,
        arrival: impl Fn(u32, f64) -> f64,
        mut now: f64,
        until: f64,
    ) -> f64 {
        let mut packets: Vec<(f64, u32, f64)> = frames
            .map(|k| {
                let sent = k as f64 * FRAME_MS;
                (arrival(k, sent), k, sent)
            })
            .collect();
        packets.sort_by(|a, b| a.0.total_cmp(&b.0));
        let mut next = 0;
        let mut out = vec![0.0; FRAME];
        while now < until {
            while next < packets.len() && packets[next].0 <= now {
                let (arrived, k, sent) = packets[next];
                p.push(k, sent, packet(k), arrived);
                next += 1;
            }
            p.pull(now, &mut out);
            now += FRAME_MS;
        }
        now
    }

    /// 平稳的网络：每帧都在 发送 + 20 ms 到。
    fn steady(p: &mut Playback<Tone>, frames: std::ops::Range<u32>, owd: f64) -> f64 {
        let end = frames.end as f64 * FRAME_MS + owd;
        drive(p, frames, |_, sent| sent + owd, owd, end)
    }

    #[test]
    fn a_steady_stream_plays_at_a_steady_shallow_delay() {
        let mut p = playback();
        steady(&mut p, 0..1000, 20.0);
        let delays = p.take_delays();
        let tail = &delays[delays.len() - 100..];
        let max = tail.iter().cloned().fold(f64::MIN, f64::max);
        let min = tail.iter().cloned().fold(f64::MAX, f64::min);
        assert!(max - min < 1.0, "平稳的网络延迟在跳：{min}–{max}");
        assert!(
            max <= 20.0 + 2.0 * FRAME_MS,
            "干净的网络缓冲太深了：{max} ms"
        );
        assert_eq!(p.stats.concealed, 0);
    }

    /// 网络卡了 300 ms（这段时间一个包都没到），然后一口气全到了。
    fn spike(k: u32, sent: f64) -> f64 {
        let spike_end = 500.0 * FRAME_MS + 300.0;
        if (500..530).contains(&k) {
            spike_end
        } else {
            sent + 20.0
        }
    }

    /// 验收的核心：延迟涨上去之后能降回来。
    ///
    /// 固定缓冲会从此多背着这 300 ms；这里要在几秒内还回去。
    #[test]
    fn a_delay_spike_is_paid_back() {
        let mut p = playback();
        drive(&mut p, 0..1500, spike, 20.0, 1500.0 * FRAME_MS + 20.0);
        let delays = p.take_delays();
        let before = delays[400];
        let worst = delays.iter().cloned().fold(f64::MIN, f64::max);
        assert!(worst > before + 200.0, "这次卡顿根本没造成延迟？{worst}");
        let after = *delays.last().unwrap();
        assert!(
            after < before + FRAME_MS * 2.0,
            "卡完 10 秒了延迟还是 {after:.1} ms（卡之前 {before:.1} ms）"
        );
        assert!(p.stats.accelerations > 0);
        assert!(
            p.stats.removed_ms > 200.0,
            "只还回去 {} ms",
            p.stats.removed_ms
        );
    }

    /// 包没到的那几拍用 PLC 顶上，照样有声音；到了接着播，一帧不丢。
    #[test]
    fn a_late_packet_is_waited_for_not_dropped() {
        let mut p = playback();
        // 200 号迟到 60 ms；后面的也被它堵着（同一条路），一起晚到
        drive(
            &mut p,
            0..400,
            |k, sent| {
                if (200..206).contains(&k) {
                    sent.max(200.0 * FRAME_MS) + 80.0
                } else {
                    sent + 20.0
                }
            },
            20.0,
            400.0 * FRAME_MS + 20.0,
        );
        let stats = p.jitter().stats;
        assert_eq!(stats.late, 0, "迟到的包被丢了");
        assert!(stats.stalls > 0, "没停下来等");
        assert!(p.stats.concealed > 0, "等的时候没用 PLC 顶上");
    }

    /// 固定缓冲不加速：同样的卡顿，延迟不回来。拿来对照。
    #[test]
    fn the_fixed_buffer_never_pays_back() {
        let mut p = Playback::new(JitterConfig::fixed(2), Tone { position: 0 }, FRAME);
        p.record_delays();
        drive(&mut p, 0..1500, spike, 20.0, 1500.0 * FRAME_MS + 20.0);
        let delays = p.take_delays();
        let first = delays[100];
        let last = *delays.last().unwrap();
        // 实测是 30 → 240 ms：卡顿的大部分永久留在了缓冲里。
        assert!(
            last > first + 150.0,
            "固定缓冲竟然降回来了：{first} → {last}"
        );
        assert_eq!(p.stats.accelerations, 0);
    }

    #[test]
    fn silence_between_talkspurts_is_silence() {
        let mut p = playback();
        let mut t = steady(&mut p, 0..100, 20.0);
        p.end_at(100);
        let mut out = vec![0.0; FRAME];
        // 播完剩下的，然后应该一直没声音
        for _ in 0..5 {
            p.pull(t, &mut out);
            t += FRAME_MS;
        }
        for _ in 0..50 {
            assert!(!p.pull(t, &mut out), "说完了还在出声");
            t += FRAME_MS;
        }
        assert_eq!(p.stats.concealed, 0, "说完了不该用 PLC 编东西出来");
    }
}
