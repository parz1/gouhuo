// SPDX-License-Identifier: MPL-2.0
//! Deterministic fault injection through production Opus and Playback.
#![cfg(feature = "codec")]
use voice_core::audio::FRAME_SAMPLES;
use voice_core::codec::{VoiceDecoder, VoiceEncoder};
use voice_core::jitter::JitterConfig;
use voice_core::playout::Playback;
use voice_core::quality::{QualityLevel, ReceiveQuality};

fn injected(arrival: impl Fn(u32, f64) -> Option<f64>) -> (ReceiveQuality, ReceiveQuality) {
    let mut encoder = VoiceEncoder::new().unwrap();
    let frame: Vec<f32> = (0..FRAME_SAMPLES)
        .map(|i| (i as f32 * 0.1).sin() * 0.2)
        .collect();
    let mut packets = Vec::new();
    for seq in 0..1500 {
        let sent = seq as f64 * 10.0;
        let payload = encoder.encode(&frame).unwrap().to_vec();
        if let Some(at) = arrival(seq, sent) {
            packets.push((at, seq, sent, payload));
        }
    }
    packets.sort_by(|a, b| a.0.total_cmp(&b.0));
    let mut packets = packets.into_iter().peekable();
    let mut playback = Playback::new(
        JitterConfig::adaptive(10.0),
        VoiceDecoder::new().unwrap(),
        FRAME_SAMPLES,
    );
    let mut out = vec![0.0; FRAME_SAMPLES];
    let mut damaged = ReceiveQuality::default();
    for tick in 0..1500 {
        let now = tick as f64 * 10.0;
        while packets.peek().is_some_and(|p| p.0 <= now) {
            let (at, seq, sent, payload) = packets.next().unwrap();
            playback.push(seq, sent, payload, at);
        }
        playback.pull(now, &mut out);
        assert!(out.iter().all(|x| x.is_finite()));
        if tick == 300 {
            damaged = playback.quality(now);
        }
    }
    (damaged, playback.quality(14990.0))
}

#[test]
fn burst_loss_is_measured_and_recent_quality_recovers() {
    let (bad, recovered) = injected(|seq, sent| {
        if (50..56).contains(&seq) {
            None
        } else {
            Some(sent + 20.0)
        }
    });
    assert_eq!(bad.missing, 6);
    assert_eq!(bad.max_loss_burst, 6);
    assert!(bad.concealed >= 6);
    assert_eq!(bad.level, QualityLevel::Poor);
    assert_eq!(recovered.missing, 0);
    assert_eq!(recovered.level, QualityLevel::Good, "{recovered:?}");
    assert!(
        recovered.buffered_ms < 100.0,
        "old speech must not remain queued"
    );
}

#[test]
fn jitter_and_reordering_are_observable_without_changing_the_transport() {
    let (bad, recovered) = injected(|seq, sent| {
        Some(
            sent + if (50..80).contains(&seq) && seq % 2 == 0 {
                120.0
            } else {
                20.0
            },
        )
    });
    assert!(bad.reordered > 0);
    assert!(bad.jitter_ms >= 30.0);
    assert!(matches!(
        bad.level,
        QualityLevel::Degraded | QualityLevel::Poor
    ));
    assert_eq!(recovered.level, QualityLevel::Good, "{recovered:?}");
}

#[test]
fn arrival_burst_overflows_are_visible_and_do_not_keep_old_audio_forever() {
    let (bad, recovered) = injected(|seq, sent| Some(if seq < 150 { 1500.0 } else { sent + 20.0 }));
    assert!(bad.overflow > 0);
    assert!(bad.stale_dropped > 0);
    assert_eq!(bad.level, QualityLevel::Poor);
    assert_eq!(recovered.overflow, 0);
    assert_eq!(recovered.level, QualityLevel::Good, "{recovered:?}");
    assert!(recovered.buffered_ms < 100.0);
}

#[test]
fn confirmed_end_and_long_silence_do_not_count_as_missing_audio() {
    let mut encoder = VoiceEncoder::new().unwrap();
    let packet = encoder.encode(&[0.0; FRAME_SAMPLES]).unwrap().to_vec();
    let mut playback = Playback::new(
        JitterConfig::adaptive(10.0),
        VoiceDecoder::new().unwrap(),
        FRAME_SAMPLES,
    );
    let mut out = vec![0.0; FRAME_SAMPLES];
    for tick in 0..500 {
        let now = tick as f64 * 10.0;
        if tick < 100 {
            playback.push(tick, now - 20.0, packet.clone(), now);
        }
        if tick == 100 {
            playback.end_at(100);
        }
        playback.pull(now, &mut out);
    }
    let q = playback.quality(5000.0);
    assert_eq!(q.missing, 0);
    assert_eq!(q.concealed, 0);
    assert_eq!(q.level, QualityLevel::Good);
    assert_eq!(playback.quality(15000.0).level, QualityLevel::Unknown);
}
