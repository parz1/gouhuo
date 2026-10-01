// SPDX-License-Identifier: MPL-2.0

//! Opus 编解码。只是把 `opus` crate 按我们的帧长和配置接上去。
//!
//! # 丢包了怎么办
//!
//! 两条路，都要用：
//!
//! - **带内 FEC**：编码器在每个包里塞上一帧的低码率副本。丢一个包时，
//!   用**下一个包**里的副本把它补回来 —— 音质比凭空猜好得多。
//!   代价是带宽涨一点，而且**要等下一个包到**，所以它只在抖动缓冲还有余量时管用。
//! - **PLC**（丢包隐藏）：解码器根据前一帧的频谱自己编一帧出来。
//!   连续丢几帧就会听出金属音，但总比静音的「咔哒」好。
//!
//! FEC 只有在编码器**知道**网络在丢包时才真的塞冗余 ——
//! `set_packet_loss_perc(0)` 等于关掉它。所以这个值要拿接收端反馈的丢包率去喂，
//! 见 [`VoiceEncoder::set_expected_loss`]。

use crate::audio::{FRAME_SAMPLES, SAMPLE_RATE};
use opus::{Application, Bitrate, Channels, Decoder, Encoder, Signal};

/// 一个 Opus 包最大多少字节。
///
/// 按 [`crate::audio::FRAME_MS`] 和我们设的码率，实际包在 20–120 字节之间；
/// 留到 400 是为了给编码器在音乐/瞬态上的临时冲高留余地。
pub const MAX_PACKET: usize = 400;

/// 默认码率。
///
/// 64 kbps 用于清晰、自然的人声。48 kHz 采样本身并不能保证编码后的频带；
/// 旧 32 kbps 配置在 quality-probe 中只选到 12 kHz 的 superwideband。
/// 10 ms 帧、IPv4 的固定开销为 45.6 kbps，总计约 109.6 kbps；实际值另测。
pub const DEFAULT_BITRATE: i32 = 64_000;

/// 编码复杂度。
///
/// 保留 5：64 kbps 下提高到 8 的合成信号测量已超过单核 CPU 预算。
/// 旧 32 kbps 的性能数字不能沿用，高清配置需单独验收。
pub const DEFAULT_COMPLEXITY: i32 = 5;

pub struct VoiceEncoder {
    encoder: Encoder,
    packet: Vec<u8>,
}

impl VoiceEncoder {
    pub fn new() -> Result<Self, opus::Error> {
        Self::with_bitrate(DEFAULT_BITRATE)
    }

    /// 测量工具/高音质调用方可选择码率，其余配置与线上保持一致。
    pub fn with_bitrate(bitrate: i32) -> Result<Self, opus::Error> {
        // APM 已负责降噪/增益。编码层优先保留原声，避免 VoIP 模式再次做
        // 语音可懂度处理；Auto 让编码器按实际信号选择 SILK/Hybrid/CELT。
        let mut encoder = Encoder::new(SAMPLE_RATE, Channels::Mono, Application::Audio)?;
        encoder.set_bitrate(Bitrate::Bits(bitrate))?;
        encoder.set_complexity(DEFAULT_COMPLEXITY)?;
        encoder.set_signal(Signal::Auto)?;
        encoder.set_inband_fec(true)?;
        // 先给个典型值，接收端有反馈之后再喂真的。0 等于关掉 FEC。
        encoder.set_packet_loss_perc(10)?;
        // DTX：不说话时几乎不发包。「常驻几小时」的场景里，一屋子人大部分
        // 时间都不说话，这一条直接决定静默带宽。
        encoder.set_dtx(true)?;
        Ok(Self {
            encoder,
            packet: vec![0u8; MAX_PACKET],
        })
    }

    /// 按接收端反馈的丢包率调 FEC 的力度。
    pub fn set_expected_loss(&mut self, percent: i32) -> Result<(), opus::Error> {
        self.encoder.set_packet_loss_perc(percent.clamp(0, 100))
    }

    /// 编一帧。返回的切片借用内部缓冲，下次调用就失效。
    ///
    /// DTX 生效时 Opus 会返回一个 1–2 字节的包（或者干脆 0 字节），
    /// 调用方要自己决定发不发 —— 见 [`is_dtx`]。
    pub fn encode(&mut self, frame: &[f32]) -> Result<&[u8], opus::Error> {
        debug_assert_eq!(frame.len(), FRAME_SAMPLES);
        let n = self.encoder.encode_float(frame, &mut self.packet)?;
        Ok(&self.packet[..n])
    }
}

/// 这个包是不是 DTX 产生的「没在说话」帧。
///
/// Opus 用极短的包表示舒适噪声。判据是长度而不是内容 —— 这是 RFC 6716
/// 里定的行为，不是我们猜的。
pub fn is_dtx(packet: &[u8]) -> bool {
    packet.len() <= 2
}

pub struct VoiceDecoder {
    decoder: Decoder,
}

impl VoiceDecoder {
    pub fn new() -> Result<Self, opus::Error> {
        Ok(Self {
            decoder: Decoder::new(SAMPLE_RATE, Channels::Mono)?,
        })
    }

    /// 解一帧。
    pub fn decode(&mut self, packet: &[u8], out: &mut [f32]) -> Result<(), opus::Error> {
        debug_assert_eq!(out.len(), FRAME_SAMPLES);
        self.decoder.decode_float(packet, out, false)?;
        Ok(())
    }

    /// 丢了一帧，让解码器自己编一帧出来（PLC）。
    ///
    /// **必须调**，不能直接塞静音：解码器有内部状态，跳过一帧会让它和真实
    /// 信号对不上，下一帧真包解出来就是错的。
    pub fn conceal(&mut self, out: &mut [f32]) -> Result<(), opus::Error> {
        debug_assert_eq!(out.len(), FRAME_SAMPLES);
        self.decoder.decode_float(&[], out, false)?;
        Ok(())
    }

    /// 用**下一个包**里带的 FEC 副本，把上一个丢掉的包补回来。
    ///
    /// 比 [`conceal`](Self::conceal) 好得多，但要求那个包真的带了冗余
    /// （发送端开了 FEC 而且当时认为在丢包）。
    pub fn decode_fec(&mut self, next_packet: &[u8], out: &mut [f32]) -> Result<(), opus::Error> {
        debug_assert_eq!(out.len(), FRAME_SAMPLES);
        self.decoder.decode_float(next_packet, out, true)?;
        Ok(())
    }
}

/// 编码器产生的前置延迟（采样点）。
///
/// 算端到端延迟时**必须加上它** —— M1 第一版漏了，导致报出来的数字偏小。
/// 它是编码器内部前瞻窗口的长度，跟帧长无关。
pub fn encoder_lookahead(encoder: &mut VoiceEncoder) -> u32 {
    encoder.encoder.get_lookahead().unwrap_or(0) as u32
}

#[cfg(test)]
mod tests {
    use super::*;

    /// 检查实际包和解码信号：48 kHz 的输入不能悄悄变成电话频带。
    #[test]
    fn default_preserves_high_frequency_audio() {
        let (fullband_frames, high_band) = high_frequency_level(VoiceEncoder::new().unwrap());
        let mut old = Encoder::new(SAMPLE_RATE, Channels::Mono, Application::Voip).unwrap();
        old.set_bitrate(Bitrate::Bits(32_000)).unwrap();
        old.set_complexity(5).unwrap();
        old.set_signal(Signal::Voice).unwrap();
        old.set_inband_fec(true).unwrap();
        old.set_packet_loss_perc(10).unwrap();
        old.set_dtx(true).unwrap();
        let (_, old_high_band) = high_frequency_level(VoiceEncoder {
            encoder: old,
            packet: vec![0; MAX_PACKET],
        });
        assert!(fullband_frames >= 90, "fullband frames: {fullband_frames}");
        // 至少保留输入高频幅度的 20%，并明显优于旧线上配置。
        // 这是频带回归判据，不能当作真人音质评分。
        assert!(
            high_band > 0.02 && high_band > old_high_band * 1.5,
            "12 kHz amplitude: new={high_band}, old={old_high_band}"
        );
    }

    fn high_frequency_level(mut enc: VoiceEncoder) -> (usize, f32) {
        let mut dec = VoiceDecoder::new().unwrap();
        let mut out = vec![0.0; FRAME_SAMPLES];
        let mut high_band_energy = 0.0;
        let mut fullband_frames = 0;
        for k in 0..100 {
            let frame: Vec<f32> = (0..FRAME_SAMPLES)
                .map(|i| {
                    let t = (k * FRAME_SAMPLES + i) as f32 / SAMPLE_RATE as f32;
                    0.15 * (std::f32::consts::TAU * 440.0 * t).sin()
                        + 0.1 * (std::f32::consts::TAU * 12_000.0 * t).sin()
                })
                .collect();
            let packet = enc.encode(&frame).unwrap();
            if opus::packet::get_bandwidth(packet).unwrap() == opus::Bandwidth::Fullband {
                fullband_frames += 1;
            }
            dec.decode(packet, &mut out).unwrap();
            if k >= 20 {
                // 12 kHz 的正交投影，避开编码器启动阶段；不要求相位一致。
                let (mut re, mut im) = (0.0, 0.0);
                for (i, sample) in out.iter().enumerate() {
                    let phase = std::f32::consts::TAU * 12_000.0 * i as f32 / SAMPLE_RATE as f32;
                    re += sample * phase.cos();
                    im += sample * phase.sin();
                }
                high_band_energy += (re * re + im * im).sqrt() * 2.0 / FRAME_SAMPLES as f32;
            }
        }
        (fullband_frames, high_band_energy / 80.0)
    }

    fn tone(samples: usize) -> Vec<f32> {
        (0..samples)
            .map(|i| {
                let t = i as f32 / SAMPLE_RATE as f32;
                0.3 * (2.0 * std::f32::consts::PI * 440.0 * t).sin()
            })
            .collect()
    }

    #[test]
    fn a_frame_round_trips() {
        let mut enc = VoiceEncoder::new().unwrap();
        let mut dec = VoiceDecoder::new().unwrap();
        let input = tone(FRAME_SAMPLES);

        let packet = enc.encode(&input).unwrap().to_vec();
        assert!(!packet.is_empty());
        assert!(packet.len() <= MAX_PACKET);

        let mut out = vec![0.0; FRAME_SAMPLES];
        dec.decode(&packet, &mut out).unwrap();
        // Opus 是有损的，不比对样点；只要求它确实出了声音而不是静音。
        let energy: f32 = out.iter().map(|s| s * s).sum();
        assert!(energy > 0.0, "解出来是静音");
    }

    /// 一整段语音过一遍编解码，能量不该差一个数量级。
    #[test]
    fn a_tone_survives_the_codec() {
        let mut enc = VoiceEncoder::new().unwrap();
        let mut dec = VoiceDecoder::new().unwrap();
        let frames = 50;
        let input = tone(FRAME_SAMPLES * frames);

        let mut output = Vec::with_capacity(input.len());
        let mut out = vec![0.0; FRAME_SAMPLES];
        for chunk in input.chunks(FRAME_SAMPLES) {
            let packet = enc.encode(chunk).unwrap().to_vec();
            dec.decode(&packet, &mut out).unwrap();
            output.extend_from_slice(&out);
        }

        // 跳过开头：编码器有前瞻，前几帧是在爬坡
        let skip = FRAME_SAMPLES * 10;
        let energy_in: f32 = input[skip..].iter().map(|s| s * s).sum();
        let energy_out: f32 = output[skip..].iter().map(|s| s * s).sum();
        let ratio = energy_out / energy_in;
        assert!(
            (0.5..2.0).contains(&ratio),
            "能量差太多：进去 {energy_in:.1}，出来 {energy_out:.1}"
        );
    }

    /// PLC 必须出点东西，而且**必须调**，不能跳过。
    #[test]
    fn concealment_produces_a_frame() {
        let mut enc = VoiceEncoder::new().unwrap();
        let mut dec = VoiceDecoder::new().unwrap();
        let input = tone(FRAME_SAMPLES * 5);
        let mut out = vec![0.0; FRAME_SAMPLES];

        // 先喂几帧让解码器有状态
        for chunk in input.chunks(FRAME_SAMPLES).take(4) {
            let packet = enc.encode(chunk).unwrap().to_vec();
            dec.decode(&packet, &mut out).unwrap();
        }

        let mut concealed = vec![0.0; FRAME_SAMPLES];
        dec.conceal(&mut concealed).unwrap();
        let energy: f32 = concealed.iter().map(|s| s * s).sum();
        assert!(energy > 0.0, "PLC 出的是纯静音 —— 那还不如不做");
    }

    /// 静音时 DTX 应该把包压到几乎没有。「常驻几小时」靠的就是这一条。
    #[test]
    fn dtx_shrinks_silence_to_almost_nothing() {
        let mut enc = VoiceEncoder::new().unwrap();
        let silence = vec![0.0f32; FRAME_SAMPLES];

        // 前几帧编码器还在判断，跑够久再看
        let mut sizes = Vec::new();
        for _ in 0..100 {
            sizes.push(enc.encode(&silence).unwrap().len());
        }
        let tail = &sizes[50..];
        let dtx_frames = tail.iter().filter(|&&n| is_dtx(&vec![0u8; n])).count();
        assert!(
            dtx_frames > tail.len() / 2,
            "静音时大部分帧该是 DTX，实际只有 {dtx_frames}/{}：{:?}",
            tail.len(),
            &tail[..10]
        );
    }

    /// 前瞻延迟必须报得出来 —— 算端到端时要加上它，M1 漏过一次。
    #[test]
    fn lookahead_is_reported() {
        let mut enc = VoiceEncoder::new().unwrap();
        let lookahead = encoder_lookahead(&mut enc);
        assert!(lookahead > 0, "前瞻是 0，多半是取值失败了");
        let ms = lookahead as f64 * 1000.0 / SAMPLE_RATE as f64;
        assert!((0.5..10.0).contains(&ms), "前瞻 {ms:.2} ms，不像真的");
    }

    #[test]
    fn expected_loss_is_clamped() {
        let mut enc = VoiceEncoder::new().unwrap();
        // 接收端反馈的数字不一定干净，不能因此让编码器报错
        enc.set_expected_loss(-5).unwrap();
        enc.set_expected_loss(500).unwrap();
        enc.set_expected_loss(10).unwrap();
    }
}
