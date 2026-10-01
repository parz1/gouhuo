// SPDX-License-Identifier: MPL-2.0

//! Offline diagnostics, not a MOS or real-device/network acceptance test.
use std::{collections::BTreeMap, error::Error, fs, path::Path};
use voice_core::{
    apm::{Apm, ApmConfig, ApmModule},
    audio::{FRAME_SAMPLES, SAMPLE_RATE},
    codec::{encoder_lookahead, VoiceDecoder, VoiceEncoder},
    jitter::JitterConfig,
    limiter::soft_limit,
    playout::Playback,
};

fn main() -> Result<(), Box<dyn Error>> {
    let args: Vec<String> = std::env::args().skip(1).collect();
    if !(2..=3).contains(&args.len()) {
        return Err("usage: quality-probe <48k-mono-pcm16.wav|--synthetic> <new-output-dir> [loss-percent:0..30]".into());
    }
    let loss: u32 = args.get(2).map(|s| s.parse()).transpose()?.unwrap_or(0);
    if loss > 30 {
        return Err("loss-percent must be 0..30".into());
    }
    let input = if args[0] == "--synthetic" {
        synthetic()
    } else {
        read_input(Path::new(&args[0]))?
    };
    if input.len() < FRAME_SAMPLES {
        return Err("input must contain at least 10 ms of audio".into());
    }
    let dir = Path::new(&args[1]);
    // A new directory prevents accidental overwrites of reference recordings.
    fs::create_dir(dir)?;
    write_wav(&dir.join("raw.wav"), &input)?;
    let mut csv = String::from("apm,bitrate,loss_percent,codec_kbps,ipv4_kbps,bands,lookahead_samples,codec_segmental_snr_db,apm_rms_dbfs,apm_peak,apm_clipped_samples,received_rms_dbfs,received_peak,received_clipped_samples,dropped_packets,plc_frames,stalls,accelerations,codec_mean_cpu_percent\n");
    for (name, cfg) in [
        ("off", ApmConfig::none()),
        ("aec", ApmConfig::only(ApmModule::EchoCancel)),
        ("full", ApmConfig::default()),
    ] {
        for bitrate in [24_000, 32_000, 48_000, 64_000] {
            csv.push_str(&run(&input, dir, name, cfg, bitrate, loss)?);
        }
    }
    fs::write(dir.join("report.csv"), &csv)?;
    println!("{csv}");
    Ok(())
}

fn read_input(path: &Path) -> Result<Vec<f32>, Box<dyn Error>> {
    let bytes = fs::read(path)?;
    // Find fmt rather than assuming the fixed 44-byte header (WAV can contain metadata).
    let mut pos = 12usize;
    let mut valid = false;
    while pos.checked_add(8).is_some_and(|end| end <= bytes.len()) {
        let len = u32::from_le_bytes(bytes[pos + 4..pos + 8].try_into()?) as usize;
        let start = pos + 8;
        let end = start.checked_add(len).ok_or("invalid WAV chunk size")?;
        if end > bytes.len() {
            return Err("truncated WAV chunk".into());
        }
        if &bytes[pos..pos + 4] == b"fmt " && len >= 16 {
            valid = u16::from_le_bytes(bytes[start..start + 2].try_into()?) == 1
                && u16::from_le_bytes(bytes[start + 2..start + 4].try_into()?) == 1
                && u32::from_le_bytes(bytes[start + 4..start + 8].try_into()?) == SAMPLE_RATE
                && u16::from_le_bytes(bytes[start + 12..start + 14].try_into()?) == 2
                && u16::from_le_bytes(bytes[start + 14..start + 16].try_into()?) == 16;
        }
        if &bytes[pos..pos + 4] == b"data" && !len.is_multiple_of(2) {
            return Err("partial PCM sample".into());
        }
        pos = end.checked_add(len & 1).ok_or("invalid WAV padding")?;
    }
    if !valid {
        return Err("use uncompressed PCM16, mono, 48000 Hz WAV (no implicit resampling)".into());
    }
    let (samples, _) = voice_core::cue::decode_wav(&bytes).ok_or("invalid WAV")?;
    Ok(samples)
}

fn run(
    input: &[f32],
    dir: &Path,
    name: &str,
    cfg: ApmConfig,
    bitrate: i32,
    loss: u32,
) -> Result<String, Box<dyn Error>> {
    let prefix = format!("{name}-{bitrate}");
    let apm = Apm::new(SAMPLE_RATE, cfg)?;
    let mut encoder = VoiceEncoder::with_bitrate(bitrate)?;
    let lookahead = encoder_lookahead(&mut encoder) as usize;
    let mut decoder = VoiceDecoder::new()?;
    let mut playback = Playback::new(
        JitterConfig::adaptive(10.0),
        VoiceDecoder::new()?,
        FRAME_SAMPLES,
    );
    let speech_frames = input.len().div_ceil(FRAME_SAMPLES);
    // Flush APM and codec state; these 400 ms are excluded from bitrate statistics.
    let total_frames = speech_frames + 40;
    let mut processed = Vec::new();
    let mut decoded = Vec::new();
    let mut received = Vec::new();
    let mut pending = std::collections::VecDeque::new();
    let mut bands = BTreeMap::<String, usize>::new();
    let mut payload_bytes = 0usize;
    let mut dropped = 0usize;
    let mut loss_credit = 0u32;
    let mut codec_time = std::time::Duration::ZERO;
    let mut silence = vec![0.0; FRAME_SAMPLES];
    let mut frame = vec![0.0; FRAME_SAMPLES];
    let mut out = vec![0.0; FRAME_SAMPLES];
    for i in 0..total_frames + 30 {
        let now = i as f64 * 10.0;
        if i < total_frames {
            frame.fill(0.0);
            let start = i * FRAME_SAMPLES;
            if start < input.len() {
                let source = &input[start..input.len().min(start + FRAME_SAMPLES)];
                frame[..source.len()].copy_from_slice(source);
            }
            silence.fill(0.0);
            // No acoustic echo path in this fixture; AEC receives a silent reference.
            apm.analyze_render(&mut silence)?;
            apm.process_capture(&mut frame)?;
            processed.extend_from_slice(&frame);
            let started = std::time::Instant::now();
            let encoded = encoder.encode(&frame)?;
            let encode_time = started.elapsed();
            let packet = encoded.to_vec();
            if i < speech_frames {
                payload_bytes += packet.len();
                *bands
                    .entry(format!("{:?}", opus::packet::get_bandwidth(&packet)?))
                    .or_default() += 1;
            }
            let started = std::time::Instant::now();
            decoder.decode(&packet, &mut out)?;
            if (20..speech_frames).contains(&i) {
                codec_time += encode_time + started.elapsed();
            }
            decoded.extend_from_slice(&out);
            loss_credit += loss;
            if loss_credit >= 100 {
                loss_credit -= 100;
                dropped += 1;
            } else {
                pending.push_back((i as u32, now, packet, now + 12.0));
            }
        }
        while pending.front().is_some_and(|p| p.3 <= now) {
            let (seq, sent, packet, arrived) = pending.pop_front().unwrap();
            playback.push(seq, sent, packet, arrived);
        }
        // A reliable terminator after all scheduled deliveries; no open-ended PLC tail.
        if i == total_frames + 2 {
            playback.end_at(total_frames as u32);
        }
        out.fill(0.0);
        playback.pull(now, &mut out);
        out.iter_mut().for_each(|s| *s = soft_limit(*s));
        received.extend_from_slice(&out);
    }
    write_wav(&dir.join(format!("{prefix}-apm.wav")), &processed)?;
    write_wav(&dir.join(format!("{prefix}-codec.wav")), &decoded)?;
    write_wav(&dir.join(format!("{prefix}-received.wav")), &received)?;
    let duration = speech_frames as f64 * 0.01;
    let codec_kbps = payload_bytes as f64 * 8.0 / duration / 1000.0;
    let snr = segmental_snr(&processed[..input.len()], &decoded[lookahead..]);
    let (apm_rms, apm_peak, apm_clips) = levels(&processed);
    let (recv_rms, recv_peak, recv_clips) = levels(&received);
    let bands = bands
        .iter()
        .map(|(b, n)| format!("{b}:{n}"))
        .collect::<Vec<_>>()
        .join(";");
    // 一路编码 + 一路直接解码，跳过前 200 ms，不包含 APM/网络/播放；短输入不报告。
    let cpu = if speech_frames > 20 {
        codec_time.as_secs_f64() / ((speech_frames - 20) as f64 * 0.01) * 100.0
    } else {
        f64::NAN
    };
    Ok(format!("{name},{bitrate},{loss},{codec_kbps:.3},{:.3},{bands},{lookahead},{snr:.3},{apm_rms:.3},{apm_peak:.6},{apm_clips},{recv_rms:.3},{recv_peak:.6},{recv_clips},{dropped},{},{},{},{cpu:.3}\n",
        codec_kbps + 45.6, playback.stats.concealed, playback.jitter().stats.stalls, playback.stats.accelerations))
}

fn levels(samples: &[f32]) -> (f64, f32, usize) {
    let energy =
        samples.iter().map(|s| f64::from(*s).powi(2)).sum::<f64>() / samples.len().max(1) as f64;
    (
        10.0 * energy.max(1e-12).log10(),
        samples.iter().fold(0.0f32, |a, s| a.max(s.abs())),
        samples.iter().filter(|s| s.abs() >= 1.0).count(),
    )
}

// Codec-only diagnostic: known look-ahead removed, no gain normalization.
// Silent frames excluded; per-frame SNR capped at [-10, 35] dB.
fn segmental_snr(reference: &[f32], output: &[f32]) -> f64 {
    let mut sum = 0.0;
    let mut count = 0;
    for (r, o) in reference
        .chunks(FRAME_SAMPLES)
        .zip(output.chunks(FRAME_SAMPLES))
    {
        if r.len() != o.len() {
            continue;
        }
        let power = r.iter().map(|s| f64::from(*s).powi(2)).sum::<f64>();
        if power / (r.len() as f64) < 1e-8 {
            continue;
        }
        let error = r
            .iter()
            .zip(o)
            .map(|(a, b)| f64::from(a - b).powi(2))
            .sum::<f64>();
        sum += (10.0 * (power / error.max(1e-20)).log10()).clamp(-10.0, 35.0);
        count += 1;
    }
    if count == 0 {
        f64::NAN
    } else {
        sum / count as f64
    }
}

fn write_wav(path: &Path, samples: &[f32]) -> Result<(), Box<dyn Error>> {
    let data_len = u32::try_from(samples.len().checked_mul(4).ok_or("WAV too large")?)?;
    let mut bytes = Vec::with_capacity(44 + data_len as usize);
    bytes.extend_from_slice(b"RIFF");
    bytes.extend_from_slice(
        &data_len
            .checked_add(36)
            .ok_or("WAV too large")?
            .to_le_bytes(),
    );
    bytes.extend_from_slice(b"WAVEfmt ");
    bytes.extend_from_slice(&16u32.to_le_bytes());
    bytes.extend_from_slice(&3u16.to_le_bytes()); // IEEE float, preserve peaks > 1 for diagnosis.
    bytes.extend_from_slice(&1u16.to_le_bytes());
    bytes.extend_from_slice(&SAMPLE_RATE.to_le_bytes());
    bytes.extend_from_slice(&(SAMPLE_RATE * 4).to_le_bytes());
    bytes.extend_from_slice(&4u16.to_le_bytes());
    bytes.extend_from_slice(&32u16.to_le_bytes());
    bytes.extend_from_slice(b"data");
    bytes.extend_from_slice(&data_len.to_le_bytes());
    for sample in samples {
        bytes.extend_from_slice(&sample.to_le_bytes());
    }
    fs::write(path, bytes)?;
    Ok(())
}

fn synthetic() -> Vec<f32> {
    let mut state = 0x2545_f491_4f6c_dd1du64;
    (0..SAMPLE_RATE * 5)
        .map(|i| {
            state ^= state << 13;
            state ^= state >> 7;
            state ^= state << 17;
            let t = i as f32 / SAMPLE_RATE as f32;
            let envelope = 0.3 + 0.7 * (std::f32::consts::TAU * 3.5 * t).sin().powi(2);
            let tones = [160.0, 440.0, 2000.0, 6000.0, 10000.0, 14000.0]
                .iter()
                .map(|f| 0.035 * (std::f32::consts::TAU * f * t).sin())
                .sum::<f32>();
            envelope * (tones + 0.02 * ((state >> 40) as f32 / 8388608.0 - 1.0))
        })
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn snr_detects_known_error_and_excludes_silence() {
        let reference = vec![0.1; FRAME_SAMPLES];
        let output = vec![0.09; FRAME_SAMPLES];
        assert!((segmental_snr(&reference, &output) - 20.0).abs() < 0.001);
        assert!(segmental_snr(&vec![0.0; FRAME_SAMPLES], &output).is_nan());
    }
}
