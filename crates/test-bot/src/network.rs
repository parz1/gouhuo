// SPDX-License-Identifier: GPL-3.0-or-later
//! A bounded, bidirectional UDP proxy. TCP is never impaired.
use latency_probe::netem::{NetemConfig, Rng};
use std::collections::BTreeMap;
use std::io;
use std::net::{SocketAddr, UdpSocket};
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::sync::Arc;
use std::thread::JoinHandle;
use std::time::{Duration, Instant};

const QUEUE_LIMIT: usize = 512;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Direction {
    Up,
    Down,
    Both,
}

#[derive(Debug, Clone, Copy)]
pub struct Impairment {
    pub netem: NetemConfig,
    pub outage_at: Duration,
    pub outage_for: Duration,
    pub direction: Direction,
}

impl Default for Impairment {
    fn default() -> Self {
        Self {
            netem: NetemConfig {
                owd_ms: 0.0,
                jitter_ms: 0.0,
                loss: 0.0,
                seed: 1,
            },
            outage_at: Duration::ZERO,
            outage_for: Duration::ZERO,
            direction: Direction::Both,
        }
    }
}

impl Impairment {
    fn blocked(self, elapsed: Duration, up: bool) -> bool {
        elapsed >= self.outage_at
            && elapsed.saturating_sub(self.outage_at) < self.outage_for
            && (self.direction == Direction::Both
                || (up && self.direction == Direction::Up)
                || (!up && self.direction == Direction::Down))
    }
}

#[derive(Default)]
pub struct Counters {
    pub accepted: AtomicU64,
    pub accepted_bytes: AtomicU64,
    pub dropped: AtomicU64,
    pub delivered: AtomicU64,
    pub delivered_bytes: AtomicU64,
    pub overflow: AtomicU64,
    pub send_errors: AtomicU64,
}

struct Schedule {
    rng: Rng,
    cfg: NetemConfig,
    queue: BTreeMap<(Instant, u64), (Vec<u8>, SocketAddr)>,
    serial: u64,
    counters: Arc<Counters>,
}

impl Schedule {
    fn new(cfg: NetemConfig, counters: Arc<Counters>) -> Self {
        Self {
            rng: Rng::new(cfg.seed),
            cfg,
            queue: BTreeMap::new(),
            serial: 0,
            counters,
        }
    }
    fn delay(&mut self) -> Option<Duration> {
        if self.rng.next_f64() < self.cfg.loss {
            return None;
        }
        let ms =
            (self.cfg.owd_ms + self.cfg.jitter_ms * self.rng.next_gaussian()).clamp(0.0, 5000.0);
        Some(Duration::from_secs_f64(ms / 1000.0))
    }
    fn push(&mut self, bytes: &[u8], dest: SocketAddr, blocked: bool) {
        self.counters.accepted.fetch_add(1, Ordering::Relaxed);
        self.counters
            .accepted_bytes
            .fetch_add(bytes.len() as u64, Ordering::Relaxed);
        // Consume the same PRNG decisions for every input packet, including
        // outage drops, so the random loss stream is independent of the outage.
        let delay = self.delay();
        if blocked || delay.is_none() {
            self.counters.dropped.fetch_add(1, Ordering::Relaxed);
            return;
        }
        if self.queue.len() >= QUEUE_LIMIT {
            self.counters.overflow.fetch_add(1, Ordering::Relaxed);
            return;
        }
        self.serial = self.serial.wrapping_add(1);
        self.queue.insert(
            (Instant::now() + delay.unwrap(), self.serial),
            (bytes.to_vec(), dest),
        );
    }
    fn deliver(&mut self, socket: &UdpSocket, blocked: bool) {
        while self
            .queue
            .first_key_value()
            .is_some_and(|((at, _), _)| *at <= Instant::now())
        {
            let (_, (bytes, dest)) = self.queue.pop_first().unwrap();
            if blocked {
                self.counters.dropped.fetch_add(1, Ordering::Relaxed);
            } else if socket.send_to(&bytes, dest).is_ok() {
                self.counters.delivered.fetch_add(1, Ordering::Relaxed);
                self.counters
                    .delivered_bytes
                    .fetch_add(bytes.len() as u64, Ordering::Relaxed);
            } else {
                self.counters.send_errors.fetch_add(1, Ordering::Relaxed);
            }
        }
    }
}

pub struct Proxy {
    pub local: SocketAddr,
    pub up: Arc<Counters>,
    pub down: Arc<Counters>,
    stop: Arc<AtomicBool>,
    worker: Option<JoinHandle<()>>,
}

impl Proxy {
    pub fn start(server: SocketAddr, cfg: Impairment, epoch: Instant) -> io::Result<Self> {
        let local = voice_core::net::bind_voice_socket("127.0.0.1:0")?;
        let remote = voice_core::net::bind_voice_socket(if server.is_ipv4() {
            "0.0.0.0:0"
        } else {
            "[::]:0"
        })?;
        local.set_nonblocking(true)?;
        remote.set_nonblocking(true)?;
        let addr = local.local_addr()?;
        let up = Arc::new(Counters::default());
        let down = Arc::new(Counters::default());
        let stop = Arc::new(AtomicBool::new(false));
        let mut upstream = Schedule::new(cfg.netem, Arc::clone(&up));
        let mut downstream = Schedule::new(
            NetemConfig {
                seed: cfg.netem.seed ^ 0x9e3779b97f4a7c15,
                ..cfg.netem
            },
            Arc::clone(&down),
        );
        let stopping = Arc::clone(&stop);
        let worker = std::thread::Builder::new()
            .name("bot-udp-proxy".into())
            .spawn(move || {
                let mut client = None;
                let mut buf = [0; protocol::MAX_DATAGRAM + 1];
                while !stopping.load(Ordering::Relaxed) {
                    for _ in 0..64 {
                        match local.recv_from(&mut buf) {
                            Ok((n, from)) if n <= protocol::MAX_DATAGRAM => {
                                client = Some(from);
                                upstream.push(
                                    &buf[..n],
                                    server,
                                    cfg.blocked(epoch.elapsed(), true),
                                );
                            }
                            Ok(_) => {}
                            Err(e) if e.kind() == io::ErrorKind::WouldBlock => break,
                            Err(_) => {
                                upstream
                                    .counters
                                    .send_errors
                                    .fetch_add(1, Ordering::Relaxed);
                                break;
                            }
                        }
                    }
                    for _ in 0..64 {
                        match remote.recv_from(&mut buf) {
                            Ok((n, from)) if from == server && n <= protocol::MAX_DATAGRAM => {
                                if let Some(dest) = client {
                                    downstream.push(
                                        &buf[..n],
                                        dest,
                                        cfg.blocked(epoch.elapsed(), false),
                                    );
                                }
                            }
                            Ok(_) => {}
                            Err(e)
                                if matches!(
                                    e.kind(),
                                    io::ErrorKind::WouldBlock
                                        | io::ErrorKind::ConnectionReset
                                        | io::ErrorKind::ConnectionRefused
                                ) =>
                            {
                                break
                            }
                            Err(_) => {
                                downstream
                                    .counters
                                    .send_errors
                                    .fetch_add(1, Ordering::Relaxed);
                                break;
                            }
                        }
                    }
                    upstream.deliver(&remote, cfg.blocked(epoch.elapsed(), true));
                    downstream.deliver(&local, cfg.blocked(epoch.elapsed(), false));
                    std::thread::sleep(Duration::from_millis(1));
                }
            })?;
        Ok(Self {
            local: addr,
            up,
            down,
            stop,
            worker: Some(worker),
        })
    }
}

impl Drop for Proxy {
    fn drop(&mut self) {
        self.stop.store(true, Ordering::Relaxed);
        if let Some(worker) = self.worker.take() {
            let _ = worker.join();
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn seeded_loss_and_delay_decisions_repeat_and_queue_is_bounded() {
        let cfg = NetemConfig {
            loss: 0.2,
            jitter_ms: 5.0,
            owd_ms: 20.0,
            seed: 42,
        };
        let counters = Arc::new(Counters::default());
        let mut a = Schedule::new(cfg, counters);
        let mut b = Schedule::new(cfg, Arc::new(Counters::default()));
        let decisions: Vec<_> = (0..500).map(|_| a.delay()).collect();
        assert_eq!(decisions, (0..500).map(|_| b.delay()).collect::<Vec<_>>());
        assert!(decisions.iter().any(Option::is_none));
        a.cfg.loss = 0.0;
        for _ in 0..QUEUE_LIMIT + 10 {
            a.push(&[1], "127.0.0.1:1234".parse().unwrap(), false);
        }
        assert_eq!(a.queue.len(), QUEUE_LIMIT);
        assert_eq!(a.counters.overflow.load(Ordering::Relaxed), 10);
        assert_eq!(
            a.counters.accepted_bytes.load(Ordering::Relaxed),
            (QUEUE_LIMIT + 10) as u64
        );
    }
    #[test]
    fn outage_direction_and_end_are_exact() {
        let cfg = Impairment {
            outage_at: Duration::from_secs(5),
            outage_for: Duration::from_secs(10),
            direction: Direction::Up,
            ..Impairment::default()
        };
        assert!(!cfg.blocked(Duration::from_secs(4), true));
        assert!(cfg.blocked(Duration::from_secs(5), true));
        assert!(!cfg.blocked(Duration::from_secs(5), false));
        assert!(!cfg.blocked(Duration::from_secs(15), true));
    }
}
