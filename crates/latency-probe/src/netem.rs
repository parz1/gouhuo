// SPDX-License-Identifier: GPL-3.0-or-later
//! 网络仿真器。
//!
//! 包**真的**走 UDP socket（127.0.0.1），仿真器只在发送侧排队延时、
//! 抖动和丢包。这样收包侧、解析、抖动缓冲全都是真实代码路径，
//! 唯一被替换掉的是「网络有多远」。
//!
//! 为什么不直接测回环：回环延迟是 0、抖动是 0、丢包是 0，
//! 测出来的抖动缓冲行为毫无意义 —— 缓冲永远不会欠载，也永远不会迟到。

use std::cmp::Ordering;
use std::collections::BinaryHeap;
use std::io;
use std::net::{SocketAddr, UdpSocket};
use std::sync::mpsc::{self, Receiver, RecvTimeoutError, Sender};
use std::thread::JoinHandle;
use std::time::{Duration, Instant};

#[derive(Debug, Clone, Copy)]
pub struct NetemConfig {
    /// 单程延迟（毫秒）。注意这是**一跳**；纯 C/S 转发的嘴到耳要走两跳，
    /// 所以配置里给的是「客户端到服务端再到客户端」的总和。
    pub owd_ms: f64,
    /// 抖动标准差（毫秒，高斯）。
    pub jitter_ms: f64,
    /// 丢包率，0.0 到 1.0。
    pub loss: f64,
    pub seed: u64,
}

#[derive(Debug, Default, Clone, Copy)]
pub struct NetemStats {
    pub accepted: u64,
    pub dropped: u64,
    pub delivered: u64,
    /// 实际排入的延迟总和，用来核对仿真器自己有没有跑偏。
    pub delay_sum_ms: f64,
    pub delay_max_ms: f64,
}

struct Scheduled {
    at: Instant,
    /// 同一时刻入队的保持先后顺序，避免 BinaryHeap 把它们随机打乱，
    /// 那会引入我们没要求的乱序。
    tiebreak: u64,
    bytes: Vec<u8>,
}

impl PartialEq for Scheduled {
    fn eq(&self, other: &Self) -> bool {
        self.at == other.at && self.tiebreak == other.tiebreak
    }
}
impl Eq for Scheduled {}
impl Ord for Scheduled {
    fn cmp(&self, other: &Self) -> Ordering {
        // 反着比，让 BinaryHeap（大顶堆）变成最早优先。
        other
            .at
            .cmp(&self.at)
            .then_with(|| other.tiebreak.cmp(&self.tiebreak))
    }
}
impl PartialOrd for Scheduled {
    fn partial_cmp(&self, other: &Self) -> Option<Ordering> {
        Some(self.cmp(other))
    }
}

enum Msg {
    Packet(Vec<u8>),
    Stop,
}

pub struct Netem {
    tx: Sender<Msg>,
    join: JoinHandle<NetemStats>,
}

impl Netem {
    pub fn send(&self, bytes: Vec<u8>) {
        let _ = self.tx.send(Msg::Packet(bytes));
    }

    /// 停止并**排空**队列，返回统计。排空很重要：不排空的话最后几十毫秒的包
    /// 会被当成丢包，把尾部统计弄脏。
    pub fn shutdown(self) -> NetemStats {
        let _ = self.tx.send(Msg::Stop);
        self.join.join().expect("netem thread panicked")
    }
}

pub fn spawn(dest: SocketAddr, cfg: NetemConfig) -> io::Result<Netem> {
    let sock = voice_core::net::bind_voice_socket("127.0.0.1:0")?;
    let (tx, rx) = mpsc::channel::<Msg>();
    let join = std::thread::Builder::new()
        .name("netem".into())
        .spawn(move || pump(sock, dest, cfg, rx))?;
    Ok(Netem { tx, join })
}

fn pump(sock: UdpSocket, dest: SocketAddr, cfg: NetemConfig, rx: Receiver<Msg>) -> NetemStats {
    let mut rng = Rng::new(cfg.seed);
    let mut heap: BinaryHeap<Scheduled> = BinaryHeap::new();
    let mut stats = NetemStats::default();
    let mut stopping = false;
    let mut tiebreak = 0u64;

    loop {
        // 先把到点的包发出去。
        while let Some(top) = heap.peek() {
            if top.at > Instant::now() {
                break;
            }
            let item = heap.pop().expect("peeked");
            if sock.send_to(&item.bytes, dest).is_ok() {
                stats.delivered += 1;
            }
        }

        if stopping && heap.is_empty() {
            break;
        }

        let wait = match heap.peek() {
            Some(top) => top.at.saturating_duration_since(Instant::now()),
            // 空闲时也别睡死，Stop 要能及时收到。
            None => Duration::from_millis(20),
        };

        match rx.recv_timeout(wait) {
            Ok(Msg::Packet(bytes)) => {
                stats.accepted += 1;
                if cfg.loss > 0.0 && rng.next_f64() < cfg.loss {
                    stats.dropped += 1;
                    continue;
                }
                let mut delay = cfg.owd_ms;
                if cfg.jitter_ms > 0.0 {
                    delay += rng.next_gaussian() * cfg.jitter_ms;
                }
                // 真实网络不会把包送到过去；负抖动最多把延迟压到 0。
                let delay = delay.max(0.0);
                stats.delay_sum_ms += delay;
                stats.delay_max_ms = stats.delay_max_ms.max(delay);
                tiebreak += 1;
                heap.push(Scheduled {
                    at: Instant::now() + Duration::from_secs_f64(delay / 1000.0),
                    tiebreak,
                    bytes,
                });
            }
            Ok(Msg::Stop) => stopping = true,
            Err(RecvTimeoutError::Timeout) => {}
            Err(RecvTimeoutError::Disconnected) => stopping = true,
        }
    }

    stats
}

/// xorshift64*。自带随机数是为了让每次跑都能用 seed 精确复现 ——
/// 「上次 p95 是 41 ms 这次是 58 ms」必须能查出是代码变了还是骰子变了。
pub struct Rng {
    state: u64,
    spare_gaussian: Option<f64>,
}

impl Rng {
    pub fn new(seed: u64) -> Self {
        // 先用 splitmix64 把种子打散。
        //
        // 不打散的话，1、7、999 这种小种子产出的流在前几百个输出上高度相关 ——
        // 实测过：三个不同的小种子给出**一模一样**的丢包序列。
        // 那样 --seed 就成了摆设，「换几个种子看方差」也白做。
        let mut z = seed.wrapping_add(0x9E37_79B9_7F4A_7C15);
        z = (z ^ (z >> 30)).wrapping_mul(0xBF58_476D_1CE4_E5B9);
        z = (z ^ (z >> 27)).wrapping_mul(0x94D0_49BB_1331_11EB);
        Self {
            state: (z ^ (z >> 31)) | 1,
            spare_gaussian: None,
        }
    }

    pub fn next_u64(&mut self) -> u64 {
        let mut x = self.state;
        x ^= x >> 12;
        x ^= x << 25;
        x ^= x >> 27;
        self.state = x;
        x.wrapping_mul(0x2545_F491_4F6C_DD1D)
    }

    pub fn next_f64(&mut self) -> f64 {
        (self.next_u64() >> 11) as f64 / (1u64 << 53) as f64
    }

    /// Box-Muller，标准正态。
    pub fn next_gaussian(&mut self) -> f64 {
        if let Some(v) = self.spare_gaussian.take() {
            return v;
        }
        let u1 = self.next_f64().max(f64::MIN_POSITIVE);
        let u2 = self.next_f64();
        let r = (-2.0 * u1.ln()).sqrt();
        let theta = std::f64::consts::TAU * u2;
        self.spare_gaussian = Some(r * theta.sin());
        r * theta.cos()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn perfect() -> NetemConfig {
        NetemConfig {
            owd_ms: 0.0,
            jitter_ms: 0.0,
            loss: 0.0,
            seed: 1,
        }
    }

    /// 小种子必须给出真正不同的流。这个测试是那个 bug 的回归测试：
    /// 没打散种子的时候，下面这些种子的丢包数会**完全相同**。
    #[test]
    fn nearby_seeds_give_independent_streams() {
        let counts: Vec<usize> = [1u64, 2, 7, 999]
            .iter()
            .map(|&seed| {
                let mut r = Rng::new(seed);
                (0..2000).filter(|_| r.next_f64() < 0.005).count()
            })
            .collect();
        let distinct: std::collections::HashSet<_> = counts.iter().collect();
        assert!(distinct.len() >= 3, "小种子之间不够独立: {counts:?}");
        // 顺便核一下丢包率本身没跑偏：2000 × 0.5% ≈ 10。
        let mean = counts.iter().sum::<usize>() as f64 / counts.len() as f64;
        assert!((4.0..18.0).contains(&mean), "丢包率跑偏了: {counts:?}");
    }

    #[test]
    fn rng_is_reproducible() {
        let a: Vec<u64> = (0..8).map(|_| Rng::new(42).next_u64()).collect();
        let mut r = Rng::new(42);
        assert_eq!(a[0], r.next_u64());
        let b: Vec<f64> = {
            let mut r = Rng::new(7);
            (0..100).map(|_| r.next_f64()).collect()
        };
        let c: Vec<f64> = {
            let mut r = Rng::new(7);
            (0..100).map(|_| r.next_f64()).collect()
        };
        assert_eq!(b, c);
        assert!(b.iter().all(|v| (0.0..1.0).contains(v)));
    }

    #[test]
    fn gaussian_has_sane_moments() {
        let mut r = Rng::new(99);
        let n = 20_000;
        let vals: Vec<f64> = (0..n).map(|_| r.next_gaussian()).collect();
        let mean = vals.iter().sum::<f64>() / n as f64;
        let var = vals.iter().map(|v| (v - mean) * (v - mean)).sum::<f64>() / n as f64;
        assert!(mean.abs() < 0.05, "mean = {mean}");
        assert!((var - 1.0).abs() < 0.08, "var = {var}");
    }

    #[test]
    fn delivers_everything_with_a_perfect_link() {
        let sink = UdpSocket::bind("127.0.0.1:0").unwrap();
        sink.set_read_timeout(Some(Duration::from_millis(500)))
            .unwrap();
        let net = spawn(sink.local_addr().unwrap(), perfect()).unwrap();
        for i in 0..20u8 {
            net.send(vec![i; 4]);
        }
        let stats = net.shutdown();
        assert_eq!(stats.accepted, 20);
        assert_eq!(stats.dropped, 0);
        assert_eq!(stats.delivered, 20);

        let mut buf = [0u8; 64];
        let mut seen = 0;
        while sink.recv_from(&mut buf).is_ok() {
            seen += 1;
            if seen == 20 {
                break;
            }
        }
        assert_eq!(seen, 20);
    }

    #[test]
    fn applies_one_way_delay() {
        let sink = UdpSocket::bind("127.0.0.1:0").unwrap();
        sink.set_read_timeout(Some(Duration::from_millis(500)))
            .unwrap();
        let cfg = NetemConfig {
            owd_ms: 40.0,
            jitter_ms: 0.0,
            loss: 0.0,
            seed: 1,
        };
        let net = spawn(sink.local_addr().unwrap(), cfg).unwrap();
        // 取几次里最短的：CI 机器上线程偶尔被晚调度几十毫秒，只量一次会随机红。
        // 延迟被加了两遍这种真毛病，每一次都会超，最短的也躲不过。
        let mut buf = [0u8; 64];
        let fastest = (0..5)
            .map(|_| {
                let t = Instant::now();
                net.send(vec![7; 4]);
                sink.recv_from(&mut buf).expect("packet should arrive");
                t.elapsed().as_secs_f64() * 1000.0
            })
            .fold(f64::INFINITY, f64::min);
        assert!((30.0..70.0).contains(&fastest), "delay was {fastest} ms");
        net.shutdown();
    }

    #[test]
    fn drops_roughly_the_requested_fraction() {
        let sink = UdpSocket::bind("127.0.0.1:0").unwrap();
        let cfg = NetemConfig {
            owd_ms: 0.0,
            jitter_ms: 0.0,
            loss: 0.2,
            seed: 5,
        };
        let net = spawn(sink.local_addr().unwrap(), cfg).unwrap();
        for _ in 0..2000 {
            net.send(vec![0; 4]);
        }
        let stats = net.shutdown();
        assert_eq!(stats.accepted, 2000);
        let rate = stats.dropped as f64 / 2000.0;
        assert!((0.17..0.23).contains(&rate), "loss rate was {rate}");
    }
}
