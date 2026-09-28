// SPDX-License-Identifier: MPL-2.0

//! 提示音：有人进出频道时响的那一声，以及念名字的那段 TTS 的去处。
//!
//! # 必须走语音链路的播放混音
//!
//! 最省事的做法是另开一路系统声音播出去（`PlaySound` 之类）。**不行**：
//! 回声消除只认得自己混出去的那一路。外放的人那边，另一路播出来的提示音
//! 会被麦克风收进去，AEC 不知道它是「自己放的」，于是原样发给频道里所有人 ——
//! 每有人进出一次，全频道听两遍叮咚。
//!
//! 所以提示音跟别人的声音一样，混进播放线程的那一帧里，再交给 APM 当参考信号。
//! [`CueQueue`] 就是往那一帧里塞东西的口子。
//!
//! # 声音是现场合成的
//!
//! 两个短音，不带任何音频文件：安装包体积是红线，一个 wav 就要几十 KB，
//! 而这点声音几十行代码就能生成，想改音高改长短也只是改个数。

use std::collections::VecDeque;
use std::sync::Mutex;

use crate::audio::SAMPLE_RATE;

/// 队列里最多攒多少秒。
///
/// 服务器一重启，二十个人几秒内全部重连进来，每人一声叮咚加一句 TTS
/// 能排出去半分钟。超过这个长度的新提示直接丢掉：晚了十几秒才念出来的
/// 「某某进来了」已经没有意义，反而盖住别人说话。
pub const MAX_QUEUED_SECONDS: f32 = 6.0;

/// 提示音的种类。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Chime {
    /// 有人进了我所在的频道：两个音往上走。
    CameIn,
    /// 有人离开了我所在的频道：两个音往下走。
    WentOut,
}

/// 合成一声提示音，48 kHz 单声道，峰值 [`CHIME_PEAK`]。
///
/// 两个短正弦音，每个带升降包络（不带的话起止处是个台阶，听起来是「咔」）。
/// 上行 / 下行是约定俗成的「来 / 走」，不用看屏幕就分得清。
pub fn chime(kind: Chime) -> Vec<f32> {
    // E5 → A5 是一个纯四度，比三度干净，也不像门铃。
    let (first, second) = match kind {
        Chime::CameIn => (659.25, 880.0),
        Chime::WentOut => (880.0, 659.25),
    };
    let mut out = tone(first, 0.09);
    out.resize(out.len() + samples_for(0.02), 0.0);
    out.extend(tone(second, 0.12));
    out
}

/// 提示音的峰值。
///
/// 别人说话一般在 -20 dBFS 上下，0.25 大约 -12 dBFS：听得见，但不会比人声
/// 突兀太多。用户那边还有音量滑条可以再调。
pub const CHIME_PEAK: f32 = 0.25;

fn samples_for(seconds: f32) -> usize {
    (seconds * SAMPLE_RATE as f32) as usize
}

fn tone(freq: f32, seconds: f32) -> Vec<f32> {
    let n = samples_for(seconds);
    // 5 ms 起、剩下的慢慢收：短音的尾巴拖一点听起来更像「叮」而不是「嘀」。
    let attack = samples_for(0.005);
    (0..n)
        .map(|i| {
            let t = i as f32 / SAMPLE_RATE as f32;
            let envelope = if i < attack {
                i as f32 / attack as f32
            } else {
                let rest = (n - i) as f32 / (n - attack) as f32;
                rest * rest
            };
            (2.0 * std::f32::consts::PI * freq * t).sin() * envelope * CHIME_PEAK
        })
        .collect()
}

/// 等着被混进播放那一帧的提示音。
///
/// 播放线程每一帧调一次 [`CueQueue::mix_into`]；别的线程随时 [`CueQueue::push`]。
/// 几段提示**按先后顺序接着播**，不叠在一起：「叮咚」之后才是「某某进来了」，
/// 两个人同时进来就是两句话挨着念。
#[derive(Default)]
pub struct CueQueue {
    samples: Mutex<VecDeque<f32>>,
}

impl CueQueue {
    pub fn new() -> Self {
        Self::default()
    }

    /// 排一段声音，乘上 `gain`（0–1）。队列已经满了就丢掉这一段，
    /// 返回 `false`。见 [`MAX_QUEUED_SECONDS`]。
    pub fn push(&self, samples: &[f32], gain: f32) -> bool {
        let gain = if gain.is_finite() {
            gain.clamp(0.0, 1.0)
        } else {
            0.0
        };
        if gain == 0.0 || samples.is_empty() {
            return true;
        }
        let limit = (MAX_QUEUED_SECONDS * SAMPLE_RATE as f32) as usize;
        let mut queue = self.samples.lock().expect("cue queue poisoned");
        if queue.len() + samples.len() > limit {
            return false;
        }
        queue.extend(samples.iter().map(|s| s * gain));
        true
    }

    /// 把队头的一帧加到 `frame` 上（是加，不是覆盖 —— 别人的声音已经在里面了）。
    ///
    /// 在播放线程上调。只拿一次锁，拷一帧，队列空的时候什么都不做。
    pub fn mix_into(&self, frame: &mut [f32]) {
        let mut queue = self.samples.lock().expect("cue queue poisoned");
        if queue.is_empty() {
            return;
        }
        let n = frame.len().min(queue.len());
        for (out, sample) in frame.iter_mut().zip(queue.drain(..n)) {
            *out += sample;
        }
    }

    /// 还剩多少个采样点没播。
    pub fn pending(&self) -> usize {
        self.samples.lock().expect("cue queue poisoned").len()
    }
}

/// 从 WAV 文件里取出单声道 f32 采样，顺便报告采样率。
///
/// 只认 16 位 PCM —— TTS 引擎吐出来的就是这个。多声道取平均。
/// 解析不了返回 `None`，调用方就当这次没念成。
pub fn decode_wav(bytes: &[u8]) -> Option<(Vec<f32>, u32)> {
    if bytes.len() < 12 || &bytes[0..4] != b"RIFF" || &bytes[8..12] != b"WAVE" {
        return None;
    }
    let mut format: Option<(u16, u16, u32, u16)> = None;
    let mut pos = 12;
    while pos + 8 <= bytes.len() {
        let id = &bytes[pos..pos + 4];
        let len = u32::from_le_bytes(bytes[pos + 4..pos + 8].try_into().ok()?) as usize;
        let body_start = pos + 8;
        // 有的实现会在流式输出时把 data 块长度写成 0 或者 0xFFFFFFFF。
        // 按「一直到文件尾」处理，比直接判坏更有用。
        let body_end = body_start.saturating_add(len).min(bytes.len());
        let body = &bytes[body_start..body_end];
        match id {
            b"fmt " if body.len() >= 16 => {
                format = Some((
                    u16::from_le_bytes([body[0], body[1]]),
                    u16::from_le_bytes([body[2], body[3]]),
                    u32::from_le_bytes([body[4], body[5], body[6], body[7]]),
                    u16::from_le_bytes([body[14], body[15]]),
                ));
            }
            b"data" => {
                let (tag, channels, rate, bits) = format?;
                // 1 = PCM；0xFFFE = WAVE_FORMAT_EXTENSIBLE，里面装的也是 PCM
                if !(tag == 1 || tag == 0xFFFE) || bits != 16 || channels == 0 || rate == 0 {
                    return None;
                }
                let data = if len == 0 { &bytes[body_start..] } else { body };
                let channels = channels as usize;
                let samples = data
                    .chunks_exact(2 * channels)
                    .map(|frame| {
                        let sum: f32 = frame
                            .as_chunks::<2>()
                            .0
                            .iter()
                            .map(|&s| i16::from_le_bytes(s) as f32 / 32768.0)
                            .sum();
                        sum / channels as f32
                    })
                    .collect();
                return Some((samples, rate));
            }
            _ => {}
        }
        // 块长度是奇数时后面有一个填充字节
        pos = body_start.saturating_add(len + (len & 1));
    }
    None
}

/// 去掉首尾的静音，两头各留 `margin` 秒。
///
/// TTS 合成出来的音频前后都垫着几百毫秒的空白。前面那段是纯延迟 ——
/// 人已经进来了，要等半秒才开始念；后面那段会把下一句往后推。
pub fn trim_silence(samples: &[f32], margin: f32) -> &[f32] {
    const THRESHOLD: f32 = 0.004; // 约 -48 dBFS，比合成器的底噪高、比最轻的辅音低
    let keep = samples_for(margin);
    let Some(first) = samples.iter().position(|s| s.abs() > THRESHOLD) else {
        return &[];
    };
    let last = samples
        .iter()
        .rposition(|s| s.abs() > THRESHOLD)
        .unwrap_or(first);
    let start = first.saturating_sub(keep);
    let end = (last + 1 + keep).min(samples.len());
    &samples[start..end]
}

/// 把 `from` Hz 的采样换成 48 kHz。线性插值。
///
/// 对 TTS 这种窄带人声够了：它本来就只有 11–24 kHz 的采样率，
/// 插值带来的那点高频镜像落在它根本没有内容的频段上。
pub fn resample_to_48k(input: &[f32], from: u32) -> Vec<f32> {
    if from == SAMPLE_RATE || input.is_empty() || from == 0 {
        return input.to_vec();
    }
    let ratio = from as f64 / SAMPLE_RATE as f64;
    let out_len = ((input.len() as f64) / ratio).floor() as usize;
    (0..out_len)
        .map(|i| {
            let pos = i as f64 * ratio;
            let idx = pos.floor() as usize;
            let frac = (pos - idx as f64) as f32;
            let a = input[idx];
            let b = *input.get(idx + 1).unwrap_or(&a);
            a + (b - a) * frac
        })
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::audio::FRAME_SAMPLES;

    #[test]
    fn chimes_are_short_and_not_too_loud() {
        for kind in [Chime::CameIn, Chime::WentOut] {
            let sound = chime(kind);
            let seconds = sound.len() as f32 / SAMPLE_RATE as f32;
            assert!((0.15..0.4).contains(&seconds), "{kind:?} 长 {seconds} 秒");
            let peak = sound.iter().fold(0.0f32, |m, s| m.max(s.abs()));
            assert!(peak <= CHIME_PEAK + 1e-6, "{kind:?} 峰值 {peak}");
            assert!(peak > CHIME_PEAK * 0.5, "{kind:?} 几乎没声音");
        }
    }

    /// 起止处不能是台阶，否则听起来是「咔」。
    #[test]
    fn chimes_fade_in_and_out() {
        let sound = chime(Chime::CameIn);
        assert!(sound[0].abs() < 1e-3);
        assert!(sound[sound.len() - 1].abs() < 1e-2);
    }

    #[test]
    fn came_in_and_went_out_sound_different() {
        assert_ne!(chime(Chime::CameIn), chime(Chime::WentOut));
    }

    #[test]
    fn the_queue_plays_cues_back_to_back_and_adds_to_the_frame() {
        let queue = CueQueue::new();
        queue.push(&vec![0.5; FRAME_SAMPLES / 2], 1.0);
        queue.push(&vec![0.25; FRAME_SAMPLES], 1.0);

        let mut frame = vec![0.1; FRAME_SAMPLES];
        queue.mix_into(&mut frame);
        // 前半帧是第一段，后半帧接着第二段；原来的 0.1 还在（是加上去的）
        assert!((frame[0] - 0.6).abs() < 1e-6);
        assert!((frame[FRAME_SAMPLES - 1] - 0.35).abs() < 1e-6);
        assert_eq!(queue.pending(), FRAME_SAMPLES / 2);

        let mut frame = vec![0.0; FRAME_SAMPLES];
        queue.mix_into(&mut frame);
        assert!((frame[0] - 0.25).abs() < 1e-6);
        assert_eq!(frame[FRAME_SAMPLES - 1], 0.0, "排空之后不能再有声音");
        assert_eq!(queue.pending(), 0);
    }

    #[test]
    fn gain_is_applied_and_zero_means_nothing_is_queued() {
        let queue = CueQueue::new();
        queue.push(&[1.0; 4], 0.5);
        let mut frame = [0.0; 4];
        queue.mix_into(&mut frame);
        assert_eq!(frame, [0.5; 4]);

        queue.push(&[1.0; 4], 0.0);
        assert_eq!(queue.pending(), 0);
        queue.push(&[1.0; 4], f32::NAN);
        assert_eq!(queue.pending(), 0);
    }

    /// 一下子涌进来一大堆提示，超过的丢掉，不能越攒越长。
    #[test]
    fn a_flood_of_cues_is_capped() {
        let queue = CueQueue::new();
        let one = chime(Chime::CameIn);
        let mut accepted = 0;
        for _ in 0..100 {
            if queue.push(&one, 1.0) {
                accepted += 1;
            }
        }
        let limit = (MAX_QUEUED_SECONDS * SAMPLE_RATE as f32) as usize;
        assert!(queue.pending() <= limit);
        assert!(accepted < 100, "一百声全收下了");
        assert!(accepted > 5, "才收了 {accepted} 声，门槛太低");
    }

    fn wav(channels: u16, rate: u32, samples: &[i16]) -> Vec<u8> {
        let data: Vec<u8> = samples.iter().flat_map(|s| s.to_le_bytes()).collect();
        let mut out = Vec::new();
        out.extend_from_slice(b"RIFF");
        out.extend_from_slice(&(36 + data.len() as u32).to_le_bytes());
        out.extend_from_slice(b"WAVE");
        out.extend_from_slice(b"fmt ");
        out.extend_from_slice(&16u32.to_le_bytes());
        out.extend_from_slice(&1u16.to_le_bytes());
        out.extend_from_slice(&channels.to_le_bytes());
        out.extend_from_slice(&rate.to_le_bytes());
        out.extend_from_slice(&(rate * 2 * channels as u32).to_le_bytes());
        out.extend_from_slice(&(2 * channels).to_le_bytes());
        out.extend_from_slice(&16u16.to_le_bytes());
        out.extend_from_slice(b"data");
        out.extend_from_slice(&(data.len() as u32).to_le_bytes());
        out.extend_from_slice(&data);
        out
    }

    #[test]
    fn decodes_mono_wav() {
        let (samples, rate) = decode_wav(&wav(1, 22050, &[0, 16384, -16384])).unwrap();
        assert_eq!(rate, 22050);
        assert_eq!(samples, vec![0.0, 0.5, -0.5]);
    }

    #[test]
    fn stereo_is_averaged_to_mono() {
        let (samples, _) = decode_wav(&wav(2, 16000, &[16384, 0, -16384, -16384])).unwrap();
        assert_eq!(samples, vec![0.25, -0.5]);
    }

    #[test]
    fn skips_chunks_it_does_not_know() {
        let mut bytes = wav(1, 16000, &[16384]);
        // 在 fmt 和 data 之间塞一个 LIST 块（奇数长度，带填充字节）
        let insert_at = 12 + 8 + 16;
        let junk = [b'L', b'I', b'S', b'T', 3, 0, 0, 0, 1, 2, 3, 0];
        bytes.splice(insert_at..insert_at, junk);
        let (samples, _) = decode_wav(&bytes).unwrap();
        assert_eq!(samples, vec![0.5]);
    }

    #[test]
    fn rejects_what_it_cannot_play() {
        assert!(decode_wav(b"not a wav").is_none());
        assert!(decode_wav(&[]).is_none());
        let mut eight_bit = wav(1, 16000, &[0]);
        eight_bit[34] = 8; // bits per sample
        assert!(decode_wav(&eight_bit).is_none());
    }

    #[test]
    fn silence_is_trimmed_from_both_ends_with_a_margin() {
        let margin = samples_for(0.01);
        let mut samples = vec![0.0; 10_000];
        samples.extend(vec![0.3; 500]);
        samples.extend(vec![0.001; 10_000]); // 底噪也算静音
        let trimmed = trim_silence(&samples, 0.01);
        assert_eq!(trimmed.len(), 500 + 2 * margin);
        assert!(trim_silence(&[0.0; 100], 0.01).is_empty());
        // 余量不能越界
        assert_eq!(trim_silence(&[0.5, 0.5], 0.01).len(), 2);
    }

    #[test]
    fn resampling_keeps_the_duration() {
        let input = vec![0.0; 22050];
        let out = resample_to_48k(&input, 22050);
        assert!((out.len() as i64 - 48000).abs() <= 1, "{}", out.len());
        assert_eq!(resample_to_48k(&[1.0, 2.0], SAMPLE_RATE), vec![1.0, 2.0]);
    }

    #[test]
    fn resampling_interpolates_between_samples() {
        // 24 kHz → 48 kHz：每两个输出点里有一个落在中间
        let out = resample_to_48k(&[0.0, 1.0, 0.0], 24000);
        assert_eq!(&out[..4], &[0.0, 0.5, 1.0, 0.5]);
    }
}
