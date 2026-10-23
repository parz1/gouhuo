// SPDX-License-Identifier: MPL-2.0
//! Bounded, ten-second receive/playout evidence. Missing playout frames are an
//! estimate, not a sender-confirmed end-to-end packet loss rate.
use std::collections::VecDeque;

const WINDOW_MS: u64 = 10_000;

pub use voice_types::{QualityLevel, ReceiveQuality};

#[derive(Default)]
struct Bucket {
    second: u64,
    received: u64,
    played: u64,
    missing: u64,
    late: u64,
    reordered: u64,
    concealed: u64,
    waits: u64,
    overflow: u64,
    stale_dropped: u64,
    burst: u64,
    jitter_ms: f64,
}

#[derive(Default)]
pub struct QualityTracker {
    buckets: VecDeque<Bucket>,
    last_packet: Option<(u32, f64, f64)>,
    loss_run: u64,
}

impl QualityTracker {
    fn bucket(&mut self, now_ms: f64) -> &mut Bucket {
        let second = (now_ms.max(0.0) as u64) / 1000;
        while self
            .buckets
            .front()
            .is_some_and(|b| b.second.saturating_add(WINDOW_MS / 1000) <= second)
        {
            self.buckets.pop_front();
        }
        if self.buckets.back().is_none_or(|b| b.second != second) {
            if self.buckets.len() == 10 {
                self.buckets.pop_front();
            }
            self.buckets.push_back(Bucket {
                second,
                ..Default::default()
            });
        }
        self.buckets.back_mut().expect("just inserted")
    }

    pub fn packet(&mut self, seq: u32, sent_ms: f64, now_ms: f64, late: bool, overflow: bool) {
        let mut reordered = false;
        let mut variation = 0.0;
        if let Some((last_seq, last_sent, last_arrived)) = self.last_packet {
            reordered = seq < last_seq;
            if sent_ms.is_finite() && last_sent.is_finite() {
                variation = ((now_ms - last_arrived) - (sent_ms - last_sent)).abs();
            }
        }
        if !reordered {
            self.last_packet = Some((seq, sent_ms, now_ms));
        }
        let bucket = self.bucket(now_ms);
        bucket.received += 1;
        bucket.late += u64::from(late);
        bucket.overflow += u64::from(overflow);
        bucket.reordered += u64::from(reordered);
        bucket.jitter_ms = bucket.jitter_ms.max(variation);
    }

    /// End markers and idle periods reset arrival/burst history, not counters.
    pub fn idle(&mut self) {
        self.last_packet = None;
        self.loss_run = 0;
    }

    pub fn played(&mut self, now_ms: f64) {
        self.loss_run = 0;
        self.bucket(now_ms).played += 1;
    }

    pub fn discarded(&mut self, now_ms: f64, frames: u64) {
        self.loss_run = 0;
        self.bucket(now_ms).stale_dropped += frames;
    }

    pub fn concealed(&mut self, now_ms: f64, missing: bool) {
        if missing {
            self.loss_run += 1;
        }
        let run = self.loss_run;
        let bucket = self.bucket(now_ms);
        bucket.concealed += 1;
        if missing {
            bucket.missing += 1;
            bucket.burst = bucket.burst.max(run);
        } else {
            bucket.waits += 1;
        }
    }

    pub fn snapshot(&self, now_ms: f64, buffered_ms: f64, target_ms: f64) -> ReceiveQuality {
        let second = (now_ms.max(0.0) as u64) / 1000;
        let mut q = ReceiveQuality {
            buffered_ms,
            target_ms,
            ..Default::default()
        };
        for b in self
            .buckets
            .iter()
            .filter(|b| b.second.saturating_add(WINDOW_MS / 1000) > second)
        {
            q.received += b.received;
            q.played += b.played;
            q.missing += b.missing;
            q.late += b.late;
            q.reordered += b.reordered;
            q.concealed += b.concealed;
            q.waits += b.waits;
            q.overflow += b.overflow;
            q.stale_dropped += b.stale_dropped;
            q.max_loss_burst = q.max_loss_burst.max(b.burst);
            q.jitter_ms = q.jitter_ms.max(b.jitter_ms);
        }
        let expected = q.played + q.missing;
        q.max_loss_burst = q.max_loss_burst.min(q.missing);
        if expected > 0 {
            q.missing_percent = q.missing as f64 * 100.0 / expected as f64;
        }
        let output = q.played + q.concealed;
        if output > 0 {
            q.plc_percent = q.concealed as f64 * 100.0 / output as f64;
        }
        if expected >= 20 {
            q.level = if q.missing_percent >= 10.0
                || q.plc_percent >= 15.0
                || q.max_loss_burst >= 5
                || q.overflow > 0
                || q.stale_dropped > 0
                || buffered_ms >= 250.0
            {
                QualityLevel::Poor
            } else if q.missing_percent >= 2.0
                || q.plc_percent >= 3.0
                || q.late >= 2
                || q.jitter_ms >= 30.0
                || buffered_ms >= 150.0
            {
                QualityLevel::Degraded
            } else {
                QualityLevel::Good
            };
        }
        q
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn idle_and_end_markers_do_not_invent_loss_or_jitter() {
        let mut q = QualityTracker::default();
        for seq in 0..100 {
            q.packet(
                seq,
                seq as f64 * 10.0,
                seq as f64 * 10.0 + 20.0,
                false,
                false,
            );
            q.played(seq as f64 * 10.0 + 40.0);
        }
        q.idle();
        q.packet(101, 7000.0, 7020.0, false, false);
        let s = q.snapshot(7020.0, 10.0, 20.0);
        assert_eq!(s.level, QualityLevel::Good);
        assert_eq!(s.missing, 0);
        assert_eq!(s.jitter_ms, 0.0);
        assert_eq!(q.snapshot(18000.0, 0.0, 20.0).level, QualityLevel::Unknown);
    }

    #[test]
    fn reordering_is_not_loss_and_waits_are_not_missing_packets() {
        let mut q = QualityTracker::default();
        for (seq, now) in [(0, 20.0), (2, 40.0), (1, 45.0), (3, 50.0)] {
            q.packet(seq, seq as f64 * 10.0, now, false, false);
        }
        q.concealed(60.0, false);
        let s = q.snapshot(60.0, 30.0, 20.0);
        assert_eq!(s.reordered, 1);
        assert_eq!(s.missing, 0);
        assert_eq!(s.waits, 1);
        assert_eq!(s.level, QualityLevel::Unknown);
    }

    #[test]
    fn burst_and_playout_damage_are_visible_then_age_out() {
        let mut q = QualityTracker::default();
        for _ in 0..95 {
            q.played(0.0);
        }
        for _ in 0..5 {
            q.concealed(100.0, true);
        }
        let s = q.snapshot(100.0, 200.0, 20.0);
        assert_eq!(s.max_loss_burst, 5);
        assert_eq!(s.missing_percent, 5.0);
        assert_eq!(s.level, QualityLevel::Poor);
        q.played(10000.0);
        assert_eq!(q.snapshot(10000.0, 0.0, 20.0).missing, 0);
        for second in 11..1000 {
            q.played(second as f64 * 1000.0);
        }
        assert!(q.buckets.len() <= 10);
    }
}
