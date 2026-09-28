// SPDX-License-Identifier: MPL-2.0

//! 时间伸缩：让一段人声**变短一点**，而音高不变。
//!
//! 抖动缓冲攒多了（网络抖了一下，包成串地到了），延迟就上去了。要把它降回来，
//! 只能比实时快一点地播。最粗暴的办法是直接扔掉一帧 —— 10 ms 的断口，
//! 听起来是「咔」的一声，而且可能正好扔掉一个字的辅音。
//!
//! 这里的办法（WSOLA 的一个简化，跟 WebRTC NetEQ 的 Accelerate 同一个思路）：
//! 浊音是周期性的，相邻两个基音周期长得几乎一样。找到周期 T，把连续两个周期
//! 交叉淡化成一个，整段就短了 T，而波形是连续的、音高没变。一次删掉一个周期
//! （2.5–15 ms），听感上几乎察觉不到。
//!
//! 不是周期性的（清辅音、噪声）就不动：删哪一段都会被听出来。
//! 几乎没声音的（停顿、换气）随便删，删多少都听不出来。

/// 采样率。跟整条链路一致。
const RATE: usize = 48_000;

/// 最短的基音周期：2.5 ms（400 Hz），女声和童声的上限。
pub const MIN_PERIOD: usize = RATE / 400;
/// 最长的基音周期：15 ms（约 67 Hz），低沉男声的下限。
pub const MAX_PERIOD: usize = RATE * 15 / 1000;

/// [`accelerate`] 至少要多长的输入：两个最长周期。30 ms。
pub const MIN_INPUT: usize = 2 * MAX_PERIOD;

/// 粗搜时降采样的倍数：48 kHz → 12 kHz。基音在 400 Hz 以下，12 kHz 绰绰有余，
/// 计算量少十几倍。
const DECIMATE: usize = 4;

/// 算相关的窗口，全速率下的长度。12.5 ms，比最长周期短一点，
/// 这样窗口加上最大滞后刚好落在 [`MIN_INPUT`] 里面。
const WINDOW: usize = 600;

/// 相关系数高于这个才算浊音。低了说明两段不像，硬删会听出来。
const VOICED: f32 = 0.8;

/// 低于这个 RMS（约 -50 dBFS）算没声音：怎么删都听不出来。
const SILENCE_RMS: f32 = 0.003;

/// 把 `input` 缩短一个基音周期。返回缩短后的音频；缩不了（太短、不是周期性的）
/// 返回 `None`，调用方原样播就行。
pub fn accelerate(input: &[f32]) -> Option<Vec<f32>> {
    if input.len() < MIN_INPUT {
        return None;
    }
    let rms = (input.iter().map(|s| s * s).sum::<f32>() / input.len() as f32).sqrt();

    let period = if rms < SILENCE_RMS {
        // 停顿：删最多的那一档。
        MAX_PERIOD
    } else {
        let (period, correlation) = find_period(input)?;
        if correlation < VOICED {
            return None;
        }
        period
    };
    Some(remove_period(input, period))
}

/// 在 `[MIN_PERIOD, MAX_PERIOD]` 里找基音周期，返回 (周期, 归一化相关系数)。
///
/// 先在 12 kHz 上粗搜，再回到 48 kHz 在粗搜结果附近细搜。
fn find_period(input: &[f32]) -> Option<(usize, f32)> {
    let low: Vec<f32> = input
        .as_chunks::<DECIMATE>()
        .0
        .iter()
        .map(|c| c.iter().sum::<f32>() / DECIMATE as f32)
        .collect();
    let coarse = correlations(
        &low,
        WINDOW / DECIMATE,
        MIN_PERIOD / DECIMATE,
        MAX_PERIOD / DECIMATE,
    )?;
    let center = shortest_strong_peak(&coarse)?.0 * DECIMATE;
    let lo = center.saturating_sub(DECIMATE).max(MIN_PERIOD);
    let hi = (center + DECIMATE).min(MAX_PERIOD);
    // 细搜只在粗搜结果附近几个采样里挑，不会跳到别的倍数上，取真正最像的。
    correlations(input, WINDOW, lo, hi)?
        .into_iter()
        .max_by(|a, b| a.1.total_cmp(&b.1))
}

/// 一个周期的整数倍也会很像，有时候（周期不是整数个采样时）甚至更像一点。
/// 删两个周期比删一个听得出来，所以不直接取最大值：先找出最高的相关系数，
/// 再在「跟它差不到 0.1」的局部峰值里挑滞后最短的那个。
fn shortest_strong_peak(c: &[(usize, f32)]) -> Option<(usize, f32)> {
    let best = c.iter().map(|p| p.1).fold(f32::MIN, f32::max);
    (0..c.len())
        .filter(|&i| {
            let left = i == 0 || c[i - 1].1 <= c[i].1;
            let right = i + 1 == c.len() || c[i + 1].1 <= c[i].1;
            left && right && c[i].1 >= best - 0.1
        })
        .map(|i| c[i])
        .next()
}

/// `x[0..w]` 和 `x[k..k+w]` 在 `k ∈ [lo, hi]` 上的归一化相关系数。
fn correlations(x: &[f32], w: usize, lo: usize, hi: usize) -> Option<Vec<(usize, f32)>> {
    if lo > hi || hi + w > x.len() {
        return None;
    }
    let head = &x[..w];
    let e0: f32 = head.iter().map(|s| s * s).sum();
    let out: Vec<(usize, f32)> = (lo..=hi)
        .filter_map(|k| {
            let tail = &x[k..k + w];
            let ek: f32 = tail.iter().map(|s| s * s).sum();
            let denom = (e0 * ek).sqrt();
            (denom > f32::EPSILON).then(|| {
                let dot = head.iter().zip(tail).map(|(a, b)| a * b).sum::<f32>();
                (k, dot / denom)
            })
        })
        .collect();
    (!out.is_empty()).then_some(out)
}

/// 把 `x[0..T]` 和 `x[T..2T]` 交叉淡化成一段，后面的原样接上。长度少 `T`。
///
/// 开头是 `x[0]`（权重全在前一段上），所以跟已经播出去的那部分接得上；
/// 结尾几乎就是 `x[2T-1]`，跟后面的 `x[2T..]` 也接得上。
fn remove_period(x: &[f32], period: usize) -> Vec<f32> {
    let mut out = Vec::with_capacity(x.len() - period);
    for i in 0..period {
        let w = i as f32 / period as f32;
        out.push(x[i] * (1.0 - w) + x[i + period] * w);
    }
    out.extend_from_slice(&x[2 * period..]);
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    /// 一段像浊音的信号：基频 `f0`，带几个衰减的谐波。
    fn voiced(f0: f32, len: usize) -> Vec<f32> {
        (0..len)
            .map(|i| {
                let t = i as f32 / RATE as f32;
                (1..=5)
                    .map(|h| (2.0 * std::f32::consts::PI * f0 * h as f32 * t).sin() / h as f32)
                    .sum::<f32>()
                    * 0.2
            })
            .collect()
    }

    fn noise(len: usize, seed: u64) -> Vec<f32> {
        let mut state = seed;
        (0..len)
            .map(|_| {
                state = state
                    .wrapping_mul(6364136223846793005)
                    .wrapping_add(1442695040888963407);
                ((state >> 33) as f32 / (1u64 << 31) as f32 - 0.5) * 0.4
            })
            .collect()
    }

    fn max_step(x: &[f32]) -> f32 {
        x.windows(2)
            .map(|w| (w[1] - w[0]).abs())
            .fold(0.0, f32::max)
    }

    #[test]
    fn a_voiced_segment_loses_exactly_one_period() {
        // 150 Hz → 周期 320 个采样
        let input = voiced(150.0, MIN_INPUT);
        let out = accelerate(&input).expect("浊音该能缩");
        let removed = input.len() - out.len();
        assert!(
            (318..=322).contains(&removed),
            "删掉了 {removed} 个采样，该是一个周期（320）"
        );
    }

    #[test]
    fn it_finds_the_period_across_the_whole_voice_range() {
        for f0 in [80.0, 120.0, 200.0, 300.0, 380.0] {
            let input = voiced(f0, MIN_INPUT);
            let period = RATE as f32 / f0;
            let removed = (input.len() - accelerate(&input).unwrap().len()) as f32;
            assert!(
                (removed - period).abs() <= 3.0,
                "{f0} Hz：删了 {removed}，周期是 {period}"
            );
        }
    }

    /// 缩完不能有断口：相邻采样的跳变不能比原信号里最大的跳变大。
    #[test]
    fn the_result_has_no_click() {
        let input = voiced(150.0, MIN_INPUT);
        let out = accelerate(&input).unwrap();
        assert!(
            max_step(&out) <= max_step(&input) * 1.1,
            "出现了断口：{} vs 原来 {}",
            max_step(&out),
            max_step(&input)
        );
        // 头一个采样必须原样保留，才接得上已经播出去的部分
        assert_eq!(out[0], input[0]);
    }

    /// 音高不能变：缩完之后的信号里再找一次周期，还是原来那个。
    #[test]
    fn the_pitch_is_unchanged() {
        let input = voiced(150.0, MIN_INPUT + 800);
        let out = accelerate(&input).unwrap();
        let (period, correlation) = find_period(&out).unwrap();
        assert!((318..=322).contains(&period), "音高变了：周期 {period}");
        assert!(correlation > 0.95);
    }

    #[test]
    fn noise_is_left_alone() {
        assert_eq!(accelerate(&noise(MIN_INPUT, 7)), None, "噪声硬删会被听出来");
    }

    #[test]
    fn silence_is_cut_the_most() {
        let quiet: Vec<f32> = noise(MIN_INPUT, 3).iter().map(|s| s * 0.001).collect();
        let out = accelerate(&quiet).expect("停顿随便删");
        assert_eq!(quiet.len() - out.len(), MAX_PERIOD);
        let zeros = vec![0.0; MIN_INPUT];
        assert_eq!(accelerate(&zeros).unwrap().len(), MIN_INPUT - MAX_PERIOD);
    }

    #[test]
    fn too_short_to_work_with() {
        assert_eq!(accelerate(&voiced(150.0, MIN_INPUT - 1)), None);
    }
}
