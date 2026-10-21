// SPDX-License-Identifier: MPL-2.0
//! Bounded, expiring datagrams and the optional authenticated TLS voice lane.
use std::collections::VecDeque;
use std::sync::{
    atomic::{AtomicU64, Ordering},
    Condvar, Mutex,
};
use std::thread::Thread;
use std::time::{Duration, Instant};

pub const MAX_AGE: Duration = Duration::from_millis(100);
const CAPACITY: usize = 128;
pub const REPLAY_WINDOW: u32 = 64;

pub struct Datagram {
    pub bytes: Vec<u8>,
    queued: Instant,
    generation: u64,
}
impl Datagram {
    pub fn remaining_ms(&self) -> u64 {
        MAX_AGE.saturating_sub(self.queued.elapsed()).as_millis() as u64
    }
    pub fn fresh(&self) -> bool {
        self.queued.elapsed() < MAX_AGE
    }
}

#[derive(Default)]
pub struct DatagramQueue {
    entries: Mutex<VecDeque<Datagram>>,
    ready: Condvar,
    writer: Mutex<Option<Thread>>,
    dropped: AtomicU64,
    generation: AtomicU64,
}
impl DatagramQueue {
    pub fn register_writer(&self) {
        *self.writer.lock().unwrap() = Some(std::thread::current());
    }
    pub fn wake_writer(&self) {
        if let Some(writer) = self.writer.lock().unwrap().as_ref() {
            writer.unpark();
        }
    }
    pub fn push(&self, bytes: &[u8]) -> bool {
        self.push_expiring(bytes, MAX_AGE)
    }
    pub fn push_expiring(&self, bytes: &[u8], remaining: Duration) -> bool {
        if remaining.is_zero() || remaining > MAX_AGE {
            return false;
        }
        if bytes.len() > protocol::MAX_DATAGRAM || bytes.is_empty() {
            return false;
        }
        let mut entries = self.entries.lock().unwrap();
        while entries.front().is_some_and(|d| !d.fresh()) || entries.len() >= CAPACITY {
            entries.pop_front();
            self.dropped.fetch_add(1, Ordering::Relaxed);
        }
        entries.push_back(Datagram {
            bytes: bytes.to_vec(),
            queued: Instant::now() - (MAX_AGE - remaining),
            generation: self.generation.load(Ordering::Relaxed),
        });
        drop(entries);
        self.ready.notify_one();
        self.wake_writer();
        true
    }
    pub fn pop(&self, wait: Duration) -> Option<Datagram> {
        let mut entries = self.entries.lock().unwrap();
        if entries.is_empty() && !wait.is_zero() {
            entries = self.ready.wait_timeout(entries, wait).unwrap().0;
        }
        while let Some(datagram) = entries.pop_front() {
            if datagram.fresh() {
                return Some(datagram);
            }
            self.dropped.fetch_add(1, Ordering::Relaxed);
        }
        None
    }
    pub fn valid(&self, datagram: &Datagram) -> bool {
        datagram.fresh() && datagram.generation == self.generation.load(Ordering::Acquire)
    }
    pub fn wake_reader(&self) {
        self.ready.notify_all();
    }
    pub fn clear(&self) {
        let mut entries = self.entries.lock().unwrap();
        self.generation.fetch_add(1, Ordering::AcqRel);
        entries.clear();
        self.ready.notify_all();
    }
    pub fn dropped(&self) -> u64 {
        self.dropped.load(Ordering::Relaxed)
    }
}

/// Bound to one authenticated connection, never redirected after reconnect.
/// Implementations must enqueue without waiting for a TLS lock or network IO.
pub trait VoiceTunnel: Send + Sync {
    fn request_tcp(&self, tcp: bool);
    fn tcp_active(&self) -> bool;
    fn send(&self, datagram: &[u8]) -> bool;
    fn send_expiring(&self, datagram: &[u8], remaining: Duration) -> bool {
        !remaining.is_zero() && self.send(datagram)
    }
    fn recv(&self, wait: Duration) -> Option<Vec<u8>>;
    fn recv_expiring(&self, wait: Duration) -> Option<(Vec<u8>, Duration)> {
        self.recv(wait).map(|bytes| (bytes, MAX_AGE))
    }
    fn clear_send(&self);
    fn dropped(&self) -> u64;
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn clearing_invalidates_a_datagram_already_taken_by_a_blocked_writer() {
        let q = DatagramQueue::default();
        q.push(&[1]);
        let pending = q.pop(Duration::ZERO).unwrap();
        assert!(q.valid(&pending));
        q.clear();
        assert!(!q.valid(&pending));
        q.push(&[2]);
        assert!(q.valid(&q.pop(Duration::ZERO).unwrap()));
    }
    #[test]
    fn overload_discards_oldest_and_expired_audio() {
        let q = DatagramQueue::default();
        for i in 0..CAPACITY + 3 {
            assert!(q.push(&[(i % 256) as u8]));
        }
        assert_eq!(q.dropped(), 3);
        assert_eq!(q.pop(Duration::ZERO).unwrap().bytes, [3]);
        q.entries.lock().unwrap().front_mut().unwrap().queued = Instant::now() - MAX_AGE;
        assert_eq!(q.pop(Duration::ZERO).unwrap().bytes, [5]);
        assert_eq!(q.dropped(), 4);
        assert!(!q.push(&vec![0; protocol::MAX_DATAGRAM + 1]));
        q.clear();
        assert!(q.pop(Duration::ZERO).is_none());
    }
}

/// 滑动窗口防重放。
///
/// 位图的第 0 位是 `highest` 本身，第 n 位是 `highest - n`。
///
/// seq 是 u32，20 ms 帧下约 994 天回绕，实际撞不上；但比较一律用
/// **回绕减法**，这样万一真跑了三年也只是窗口重置一次，而不是从此
/// 所有包都被当成重放丢掉。
#[derive(Default)]
pub struct ReplayWindow {
    highest: u32,
    bitmap: u64,
    started: bool,
}

impl ReplayWindow {
    pub fn accept(&mut self, seq: u32) -> bool {
        if !self.started {
            self.started = true;
            self.highest = seq;
            self.bitmap = 1;
            return true;
        }

        let ahead = seq.wrapping_sub(self.highest) as i32;
        if ahead > 0 {
            let step = ahead as u32;
            self.bitmap = if step >= REPLAY_WINDOW {
                1
            } else {
                (self.bitmap << step) | 1
            };
            self.highest = seq;
            true
        } else {
            let back = ahead.unsigned_abs();
            if back >= REPLAY_WINDOW {
                // 太旧了。抖动缓冲那边早就放弃它了。
                return false;
            }
            let mask = 1u64 << back;
            if self.bitmap & mask != 0 {
                return false;
            }
            self.bitmap |= mask;
            true
        }
    }
}
