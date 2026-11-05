// SPDX-License-Identifier: MPL-2.0

//! UDP 语音转发 —— 整个服务端的热路径。
//!
//! 一个人说话，服务端把他的包复制给同频道的其他人。没有混音，没有转码，
//! 没有 ICE：纯 C/S 转发不需要打洞，音频 SFU 就是个几百行的 UDP 转发器。
//!
//! # 为什么要解密再加密
//!
//! 语音密钥是**每条连接**从 TLS 派生的（见 `transport::derive_voice_key`），
//! 收发两端的密钥不同，所以转发不可能是原样复制。
//!
//! 另一条路是全频道共享一把密钥，服务端只管搬字节。它更快，但有个洞：
//! 所有成员都拿着同一把钥匙，**任何人都能伪造别人的 session id**，
//! 做出「某人正在说话」的假象，而服务端分辨不出来。
//!
//! 每连接一把密钥之后，AEAD 的 tag 本身就是发送者的证明 —— 验得过就一定是他。
//! 代价是服务端每包要做一次解密加 N 次加密。ChaCha20-Poly1305 在几十字节的
//! 负载上是纳秒级的，20 人全说话也就每秒两万次，对一台自部署的小机器
//! 完全不是问题。
//!
//! # 地址是学来的，不是报来的
//!
//! 客户端的 UDP 源地址服务端事先不知道（NAT 会改），只能从收到的包里学。
//! 关键是**先验签再学**：如果先记地址再验，任何人只要伪造一个包就能把
//! 别人的语音重定向到自己那里 —— 一个不需要任何密钥的窃听。
//!
//! # 锁
//!
//! 全程**不同时持有两把锁**。转发一个包要 state（谁在哪个频道）和 voice
//! （密钥和地址）两边的信息，做法是各取一次快照再放开，而不是嵌套着拿。
//! 这段代码在 UDP 线程上跑，锁一旦嵌套就是整台服务器卡死。

use std::collections::HashMap;
use std::net::{SocketAddr, UdpSocket};
use std::sync::{Arc, Mutex};
use std::time::Instant;
use voice_core::tunnel::{DatagramQueue, ReplayWindow};

use protocol::control::MemberNetwork;
use protocol::{ProtocolError, VoiceCipher, VoiceHeader, MAX_DATAGRAM};

use crate::state::SessionId;

/// 防重放窗口有多宽（单位是包）。
///
/// 64 个包在 20 ms 帧下是 1.28 秒。乱序超过这个程度的包，抖动缓冲那边
/// 本来也早就放弃它了，丢掉不可惜。
pub use voice_core::tunnel::REPLAY_WINDOW;

/// 一个人的语音状态：两把密钥、学到的地址、防重放窗口。
struct VoicePeer {
    /// 客户端发上来的包用这把开。
    tcp: bool,
    tunnel: Option<Arc<DatagramQueue>>,
    upstream: VoiceCipher,
    /// 转发给这个人的包用这把封。
    downstream: VoiceCipher,
    /// 从收到的包里学到的源地址。`None` 表示这个人还没发过语音
    /// （刚登录、或者 UDP 不通）。
    addr: Option<SocketAddr>,
    received: u64,
    /// 服务端自己量的上行丢包：语音按序号缺口算，保活另算一份。
    uplink: UplinkStats,
    /// 这个人在 Ping 里自报的 RTT 和下行丢包。旧客户端不报。
    report: SelfReport,
    replay: ReplayWindow,
    /// 保活包**单独一个窗口**。
    ///
    /// 它跟语音是两条互不相干的序号流：语音每秒推进 100，保活每 2 秒才一个。
    /// 共用一个窗口的话，保活的序号会飞快地落到窗口外，然后被当成重放丢掉 ——
    /// 表现是「UDP 时通时不通」，而语音本身看着一切正常。
    keepalive_replay: ReplayWindow,
}

/// 客户端在 Ping 里自报的网络状况。`None` = 没报（旧客户端）。
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct SelfReport {
    pub rtt_ms: Option<u32>,
    pub downlink_loss_permille: Option<u32>,
}

/// 语音统计的窗口：十秒，一秒一格。说话的人每秒一百包，十秒足够稳。
pub const VOICE_LOSS_WINDOW_SECS: usize = 10;
/// 保活统计的窗口：三十秒。保活两秒一个，窗口太短就只有几个样本。
pub const KEEPALIVE_LOSS_WINDOW_SECS: usize = 30;
/// 窗口里至少要有这么多「应到」的包，算出来的丢包率才拿出去给人看。
pub const MIN_VOICE_SAMPLES: u64 = 20;
pub const MIN_KEEPALIVE_SAMPLES: u64 = 5;

/// 按序号缺口量上行丢包。
///
/// 每个人的语音序号每包加一（见 `protocol::VoiceSequences`），静音时不发包
/// 也不推进。所以序号从 a 走到 b 就该收到 b − a 个包，少收的就是路上丢的。
/// 这是**服务端自己量的**，不靠客户端配合，旧客户端一样有数。
///
/// 保活是另一条序号流，单独量：没人说话的时候只有它能说明上行通不通。
#[derive(Debug, Default)]
pub struct UplinkStats {
    voice: GapWindow<VOICE_LOSS_WINDOW_SECS>,
    keepalive: GapWindow<KEEPALIVE_LOSS_WINDOW_SECS>,
}

impl UplinkStats {
    pub fn record(&mut self, header: VoiceHeader, now: Instant) {
        if header.is_keepalive() {
            self.keepalive.record(header.seq, now);
        } else {
            self.voice.record(header.seq, now);
        }
    }

    /// 最近窗口里的丢包千分比。有语音样本用语音，否则退回保活；都不够就是 `None`。
    pub fn loss_permille(&mut self, now: Instant) -> Option<u32> {
        if let Some(v) = self.voice.loss_permille(now, MIN_VOICE_SAMPLES) {
            return Some(v);
        }
        self.keepalive.loss_permille(now, MIN_KEEPALIVE_SAMPLES)
    }
}

/// 一秒一格的环：每格记「应到几个、实到几个」。
#[derive(Debug)]
struct GapWindow<const SECS: usize> {
    /// 窗口起点；格子按距它的秒数取模。
    epoch: Option<Instant>,
    /// 每格 (应到, 实到, 属于第几秒)。翻过一圈回到同一格时按秒号清零。
    buckets: [(u64, u64, u64); SECS],
    highest: Option<u32>,
    /// 上一个包落在第几秒。隔了整个窗口还没包，再来的包按新起点算：
    /// 中间丢的那些都发生在早已滚出窗口的秒里，不该记到现在头上。
    last_second: u64,
}

impl<const SECS: usize> Default for GapWindow<SECS> {
    fn default() -> Self {
        Self {
            epoch: None,
            buckets: [(0, 0, u64::MAX); SECS],
            highest: None,
            last_second: 0,
        }
    }
}

impl<const SECS: usize> GapWindow<SECS> {
    fn second(&mut self, now: Instant) -> u64 {
        let epoch = *self.epoch.get_or_insert(now);
        now.saturating_duration_since(epoch).as_secs()
    }

    fn bucket(&mut self, second: u64) -> &mut (u64, u64, u64) {
        let slot = &mut self.buckets[(second % SECS as u64) as usize];
        if slot.2 != second {
            *slot = (0, 0, second);
        }
        slot
    }

    fn record(&mut self, seq: u32, now: Instant) {
        let second = self.second(now);
        if second.saturating_sub(self.last_second) >= SECS as u64 {
            self.highest = None;
        }
        self.last_second = second;
        let expected = match self.highest {
            // 第一个包：只知道它到了，缺口无从谈起。
            None => 1,
            Some(high) if seq > high => u64::from(seq - high),
            // 乱序补到的旧包：之前已经按缺口算成「应到」了，现在只补「实到」。
            Some(high) if high - seq < REPLAY_WINDOW => 0,
            // 跳回很远（链路重建、序号重置）：当作新起点。
            Some(_) => 1,
        };
        if expected > 0 {
            self.highest = Some(seq);
        }
        // 窗口内的缺口全记到当前这一秒：十秒的窗口里差一两秒无所谓，
        // 窗口外的已经在上面按新起点处理掉了。
        let bucket = self.bucket(second);
        bucket.0 += expected;
        bucket.1 += 1;
    }

    fn loss_permille(&mut self, now: Instant, min_expected: u64) -> Option<u32> {
        let current = self.second(now);
        let oldest = current.saturating_sub(SECS as u64 - 1);
        let (expected, got) = self
            .buckets
            .iter()
            .filter(|b| b.2 != u64::MAX && b.2 >= oldest && b.2 <= current)
            .fold((0u64, 0u64), |acc, b| (acc.0 + b.0, acc.1 + b.1));
        if expected < min_expected {
            return None;
        }
        let lost = expected.saturating_sub(got);
        Some(((lost * 1000) / expected).min(1000) as u32)
    }
}

/// UDP 这一半。
pub struct VoiceRouter {
    socket: UdpSocket,
    peers: Mutex<HashMap<SessionId, VoicePeer>>,
    /// 反查：源地址 → 会话。只在验签通过之后才写。
    by_addr: Mutex<HashMap<SocketAddr, SessionId>>,
}

/// 一个验过签的入站包。
pub enum Incoming {
    /// 真语音，要转发给同频道的其他人。
    Voice(Forward),
    /// 保活包。**不转发**，原样回一个就行 —— 见 `protocol::FLAG_KEEPALIVE`。
    Keepalive(SessionId, VoiceHeader),
}

/// 一个要转发的语音包。
pub struct Forward {
    pub header: VoiceHeader,
    pub payload: Vec<u8>,
    pub from: SessionId,
}

impl VoiceRouter {
    pub fn new(socket: UdpSocket) -> Self {
        Self {
            socket,
            peers: Mutex::new(HashMap::new()),
            by_addr: Mutex::new(HashMap::new()),
        }
    }

    pub fn local_port(&self) -> u16 {
        self.socket.local_addr().map(|a| a.port()).unwrap_or(0)
    }

    /// 登录成功时挂上这个人的两把密钥。
    pub fn register(&self, session: SessionId, upstream: &[u8; 32], downstream: &[u8; 32]) {
        let mut peers = self.peers.lock().expect("voice peers poisoned");
        peers.insert(
            session,
            VoicePeer {
                tcp: false,
                tunnel: None,
                upstream: VoiceCipher::new(upstream),
                downstream: VoiceCipher::new(downstream),
                addr: None,
                received: 0,
                uplink: UplinkStats::default(),
                report: SelfReport::default(),
                replay: ReplayWindow::default(),
                keepalive_replay: ReplayWindow::default(),
            },
        );
    }

    /// 记下这个人在 Ping 里自报的网络状况。
    pub fn note_report(&self, session: SessionId, report: SelfReport) {
        if let Some(peer) = self
            .peers
            .lock()
            .expect("voice peers poisoned")
            .get_mut(&session)
        {
            peer.report = report;
        }
    }

    /// 所有人的网络状况，按会话 id 排好，给 `NetworkReport` 用。
    pub fn network_snapshot(&self) -> Vec<MemberNetwork> {
        self.network_snapshot_at(Instant::now())
    }

    pub fn network_snapshot_at(&self, now: Instant) -> Vec<MemberNetwork> {
        let mut peers = self.peers.lock().expect("voice peers poisoned");
        let mut out: Vec<MemberNetwork> = peers
            .iter_mut()
            .map(|(session, peer)| {
                let uplink = peer.uplink.loss_permille(now);
                MemberNetwork {
                    session_id: *session,
                    uplink_loss_permille: uplink.unwrap_or(0),
                    uplink_known: uplink.is_some(),
                    rtt_ms: peer.report.rtt_ms.unwrap_or(0),
                    rtt_known: peer.report.rtt_ms.is_some(),
                    downlink_loss_permille: peer.report.downlink_loss_permille.unwrap_or(0),
                    downlink_known: peer.report.downlink_loss_permille.is_some(),
                    tcp_voice: peer.tcp,
                }
            })
            .collect();
        out.sort_by_key(|m| m.session_id);
        out
    }

    /// 测试用：按给定时刻记一个已验过的包。
    #[cfg(test)]
    fn record_uplink_at(&self, session: SessionId, header: VoiceHeader, now: Instant) {
        if let Some(peer) = self
            .peers
            .lock()
            .expect("voice peers poisoned")
            .get_mut(&session)
        {
            peer.uplink.record(header, now);
        }
    }

    pub fn attach_tunnel(&self, session: SessionId, queue: Arc<DatagramQueue>) {
        if let Some(peer) = self.peers.lock().unwrap().get_mut(&session) {
            peer.tunnel = Some(queue);
        }
    }

    pub fn set_tcp(&self, session: SessionId, tcp: bool) {
        if let Some(peer) = self.peers.lock().unwrap().get_mut(&session) {
            peer.tcp = tcp;
            if let Some(queue) = &peer.tunnel {
                queue.clear();
            }
        }
    }

    /// TLS identity must match the encrypted header. Never learn a UDP address here.
    pub fn accept_tunnel(&self, session: SessionId, datagram: &[u8]) -> Option<Incoming> {
        if datagram.len() > MAX_DATAGRAM {
            return None;
        }
        let mut peers = self.peers.lock().unwrap();
        let peer = peers.get_mut(&session)?;
        if !peer.tcp {
            return None;
        }
        let mut payload = Vec::new();
        let header = peer.upstream.open(datagram, &mut payload).ok()?;
        if header.session != session {
            return None;
        }
        let replay = if header.is_keepalive() {
            &mut peer.keepalive_replay
        } else {
            &mut peer.replay
        };
        if !replay.accept(header.seq) {
            return None;
        }
        peer.uplink.record(header, Instant::now());
        if header.is_keepalive() {
            let mut wire = Vec::new();
            peer.downstream.seal(header, &[], &mut wire).ok()?;
            peer.tunnel.as_ref()?.push(&wire);
            return None;
        }
        Some(Incoming::Voice(Forward {
            header,
            payload,
            from: session,
        }))
    }

    /// 走人时摘掉。**必须摘** —— 留着的话，一个拿着旧密钥的人还能继续往
    /// 频道里灌语音。
    pub fn unregister(&self, session: SessionId) {
        let addr = {
            let mut peers = self.peers.lock().expect("voice peers poisoned");
            peers.remove(&session).and_then(|p| p.addr)
        };
        if let Some(addr) = addr {
            let mut by_addr = self.by_addr.lock().expect("by_addr poisoned");
            // 只有当这个地址还映射到他自己时才删 —— 中间可能已经被别人
            // （同一个 NAT 后面换了端口的另一个人）接管了。
            if by_addr.get(&addr) == Some(&session) {
                by_addr.remove(&addr);
            }
        }
    }

    /// 这个人收到过多少个语音包。填进 Pong 里，客户端据此判断 UDP 通不通。
    pub fn packets_received(&self, session: SessionId) -> u64 {
        self.peers
            .lock()
            .expect("voice peers poisoned")
            .get(&session)
            .map(|p| p.received)
            .unwrap_or(0)
    }

    /// 校验一个刚收到的数据报，通过就返回该转发的内容。
    ///
    /// 失败一律返回 `None`，**不回任何东西给对端** —— 语音面不给攻击者
    /// 任何 oracle，而且回错误本身就是一个放大攻击的入口。
    pub fn accept(&self, from: SocketAddr, datagram: &[u8]) -> Option<Incoming> {
        // 头部是明文，先看一眼它自称是谁。这只是个查表用的线索，
        // 不是凭证 —— 凭证是下面那个 tag。
        let (claimed, _) = VoiceHeader::parse(datagram).ok()?;

        let mut peers = self.peers.lock().expect("voice peers poisoned");
        let peer = peers.get_mut(&claimed.session)?;

        let mut payload = Vec::with_capacity(datagram.len());
        let header = match peer.upstream.open(datagram, &mut payload) {
            Ok(header) => header,
            // 验不过：伪造、改包、或者是上一个会话的残包。丢。
            Err(ProtocolError::Crypto) | Err(_) => return None,
        };

        let window = if header.is_keepalive() {
            &mut peer.keepalive_replay
        } else {
            &mut peer.replay
        };
        if !window.accept(header.seq) {
            return None;
        }
        peer.received += 1;
        peer.uplink.record(header, Instant::now());

        // **到这里才动地址。** 先验签再学地址，见模块文档。
        let previous = peer.addr.replace(from);
        drop(peers);

        if previous != Some(from) {
            let mut by_addr = self.by_addr.lock().expect("by_addr poisoned");
            if let Some(old) = previous {
                by_addr.remove(&old);
            }
            by_addr.insert(from, claimed.session);
        }

        if header.is_keepalive() {
            // 保活包的全部意义就是刚才那几行：让服务端学到地址、并确认
            // 上行通。负载一律丢掉，绝不转发。
            return Some(Incoming::Keepalive(claimed.session, header));
        }

        Some(Incoming::Voice(Forward {
            header,
            payload,
            from: claimed.session,
        }))
    }

    /// 原样回一个保活包。客户端收到就知道下行也通，顺便量出一次 RTT。
    pub fn reply_keepalive(&self, session: SessionId, header: VoiceHeader) {
        let outgoing = {
            let peers = self.peers.lock().expect("voice peers poisoned");
            peers.get(&session).and_then(|peer| {
                let addr = peer.addr?;
                let mut wire = Vec::with_capacity(MAX_DATAGRAM);
                peer.downstream.seal(header, &[], &mut wire).ok()?;
                Some((addr, wire))
            })
        };
        // 发的时候不持锁，同 `deliver`。
        if let Some((addr, wire)) = outgoing {
            let _ = self.socket.send_to(&wire, addr);
        }
    }

    /// 把一个包封给这些人并发出去。返回真正发出去了几个。
    ///
    /// 头部原样带过去：里面的 session / seq / timestamp 是**说话者的**，
    /// 收方的抖动缓冲全靠它排序。换的只是密钥。
    pub fn deliver(&self, forward: &Forward, targets: &[SessionId]) -> usize {
        let mut outgoing: Vec<(SocketAddr, Vec<u8>)> = Vec::with_capacity(targets.len());
        {
            let peers = self.peers.lock().expect("voice peers poisoned");
            for &target in targets {
                if target == forward.from {
                    continue;
                }
                let Some(peer) = peers.get(&target) else {
                    continue;
                };

                let mut wire = Vec::with_capacity(MAX_DATAGRAM);
                if peer
                    .downstream
                    .seal(forward.header, &forward.payload, &mut wire)
                    .is_ok()
                {
                    if peer.tcp {
                        if let Some(queue) = &peer.tunnel {
                            queue.push(&wire);
                        }
                    } else if let Some(addr) = peer.addr {
                        outgoing.push((addr, wire));
                    }
                }
            }
        }

        // 发的时候**不持锁**：一次 send_to 在发送缓冲满时会阻塞，
        // 持着锁阻塞等于把整个语音路径停掉。
        let mut sent = 0;
        for (addr, wire) in outgoing {
            if self.socket.send_to(&wire, addr).is_ok() {
                sent += 1;
            }
        }
        sent
    }

    pub fn socket(&self) -> &UdpSocket {
        &self.socket
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use protocol::FLAG_KEEPALIVE;
    use std::time::Duration;

    #[test]
    fn tunnel_requires_selected_authenticated_session_and_shares_replay_window() {
        let router = VoiceRouter::new(UdpSocket::bind("127.0.0.1:0").unwrap());
        router.register(7, &[1; 32], &[2; 32]);
        router.register(8, &[1; 32], &[3; 32]);
        let queue = Arc::new(DatagramQueue::default());
        router.attach_tunnel(7, Arc::clone(&queue));
        let cipher = VoiceCipher::new(&[1; 32]);
        let packet = |session, seq, flags| {
            let mut bytes = Vec::new();
            cipher
                .seal(
                    VoiceHeader {
                        session,
                        seq,
                        timestamp: 0,
                        flags,
                    },
                    &[1; 20],
                    &mut bytes,
                )
                .unwrap();
            bytes
        };
        assert!(router.accept_tunnel(7, &packet(7, 1, 0)).is_none());
        router.set_tcp(7, true);
        assert!(
            router.accept_tunnel(7, &packet(8, 1, 0)).is_none(),
            "TLS identity must override claimed session, even with matching keys"
        );
        let voice = packet(7, 1, 0);
        assert!(matches!(
            router.accept_tunnel(7, &voice),
            Some(Incoming::Voice(_))
        ));
        assert!(
            router
                .accept("127.0.0.1:12345".parse().unwrap(), &voice)
                .is_none(),
            "cross-path replay accepted"
        );
        assert_eq!(
            router.packets_received(7),
            0,
            "TLS must not make UDP appear healthy"
        );
        assert!(router.peers.lock().unwrap()[&7].addr.is_none());
        assert!(router
            .accept_tunnel(7, &packet(7, 2, FLAG_KEEPALIVE))
            .is_none());
        assert!(
            queue.pop(std::time::Duration::ZERO).is_some(),
            "TLS probes must echo through TLS"
        );
        router.unregister(7);
        assert!(router.accept_tunnel(7, &packet(7, 3, 0)).is_none());
    }

    #[test]
    fn tunnel_recipient_does_not_need_a_udp_address_and_path_change_flushes_audio() {
        let router = VoiceRouter::new(UdpSocket::bind("127.0.0.1:0").unwrap());
        router.register(8, &[1; 32], &[2; 32]);
        let queue = Arc::new(DatagramQueue::default());
        router.attach_tunnel(8, Arc::clone(&queue));
        router.set_tcp(8, true);
        let forward = Forward {
            header: VoiceHeader {
                session: 7,
                seq: 3,
                timestamp: 0,
                flags: 0,
            },
            payload: vec![1, 2, 3],
            from: 7,
        };
        router.deliver(&forward, &[8]);
        let packet = queue.pop(std::time::Duration::ZERO).unwrap();
        let mut payload = Vec::new();
        let header = VoiceCipher::new(&[2; 32])
            .open(&packet.bytes, &mut payload)
            .unwrap();
        assert_eq!(header.session, 7);
        assert_eq!(payload, [1, 2, 3]);
        router.set_tcp(8, false);
        assert!(!queue.valid(&packet));
        router.deliver(&forward, &[8]);
        assert!(queue.pop(std::time::Duration::ZERO).is_none());
    }

    #[test]
    fn in_order_packets_all_pass() {
        let mut w = ReplayWindow::default();
        for seq in 0..1000 {
            assert!(w.accept(seq), "seq {seq} 被误判成重放");
        }
    }

    #[test]
    fn exact_duplicates_are_dropped() {
        let mut w = ReplayWindow::default();
        assert!(w.accept(10));
        assert!(!w.accept(10), "同一个 seq 收两次，第二次必须丢");
        assert!(w.accept(11));
        assert!(!w.accept(11));
    }

    /// 乱序是正常的 —— UDP 不保证顺序，丢的不该是它们。
    #[test]
    fn out_of_order_within_the_window_is_accepted_once() {
        let mut w = ReplayWindow::default();
        assert!(w.accept(100));
        assert!(w.accept(98), "迟到一点的包该收");
        assert!(w.accept(99));
        assert!(!w.accept(98), "但同一个迟到包不能收两次");
        assert!(w.accept(101));
    }

    #[test]
    fn too_old_is_dropped() {
        let mut w = ReplayWindow::default();
        assert!(w.accept(1000));
        assert!(w.accept(1000 - REPLAY_WINDOW + 1));
        assert!(!w.accept(1000 - REPLAY_WINDOW), "正好在窗口外，丢");
        assert!(!w.accept(0));
    }

    /// 跳一大步（客户端重启、长时间静默）之后窗口要重置，而不是从此全丢。
    #[test]
    fn a_big_jump_resets_the_window() {
        let mut w = ReplayWindow::default();
        assert!(w.accept(5));
        assert!(w.accept(100_000));
        assert!(w.accept(100_001));
        assert!(!w.accept(5), "跳过去之后老包就该丢了");
    }

    /// u32 回绕之后不能变成「从此所有包都是重放」。
    #[test]
    fn wraparound_does_not_wedge_the_window() {
        let mut w = ReplayWindow::default();
        assert!(w.accept(u32::MAX - 1));
        assert!(w.accept(u32::MAX));
        assert!(w.accept(0), "回绕之后的第一个包必须收");
        assert!(w.accept(1));
        assert!(!w.accept(0), "但它仍然只能收一次");
    }

    #[test]
    fn unknown_session_is_dropped_without_touching_anything() {
        let router = VoiceRouter::new(UdpSocket::bind("127.0.0.1:0").unwrap());
        let header = VoiceHeader {
            session: 7,
            seq: 1,
            timestamp: 0,
            flags: 0,
        };
        let cipher = VoiceCipher::new(&[3u8; 32]);
        let mut wire = Vec::new();
        cipher.seal(header, &[1, 2, 3], &mut wire).unwrap();

        let from: SocketAddr = "127.0.0.1:1234".parse().unwrap();
        assert!(
            router.accept(from, &wire).is_none(),
            "没登录的会话不该被转发"
        );
    }

    /// 拿着别人的 session id 伪造一个包，必须过不了 —— 而且**不能**因此
    /// 把受害者的语音重定向到攻击者那里。
    #[test]
    fn forged_packet_does_not_hijack_the_address() {
        let router = VoiceRouter::new(UdpSocket::bind("127.0.0.1:0").unwrap());
        let up = [1u8; 32];
        let down = [2u8; 32];
        router.register(7, &up, &down);

        let header = VoiceHeader {
            session: 7,
            seq: 1,
            timestamp: 0,
            flags: 0,
        };

        // 真的那个人先出声，服务端学到他的地址
        let real: SocketAddr = "127.0.0.1:5000".parse().unwrap();
        let mut wire = Vec::new();
        VoiceCipher::new(&up)
            .seal(header, &[9; 40], &mut wire)
            .unwrap();
        assert!(router.accept(real, &wire).is_some());

        // 攻击者拿错误的密钥伪造一个同样 session 的包
        let attacker: SocketAddr = "127.0.0.1:6000".parse().unwrap();
        let mut forged = Vec::new();
        VoiceCipher::new(&[0xEE; 32])
            .seal(VoiceHeader { seq: 2, ..header }, &[9; 40], &mut forged)
            .unwrap();
        assert!(router.accept(attacker, &forged).is_none(), "伪造的包被收了");

        // 地址必须还是真人的 —— 否则就是一次不需要密钥的窃听
        let peers = router.peers.lock().unwrap();
        assert_eq!(peers[&7].addr, Some(real), "伪造的包把地址劫走了");
    }

    /// **保活和语音不能共用防重放窗口。**
    ///
    /// 语音每秒把窗口推进 100，保活每 2 秒才一个。共用一个窗口的话，
    /// 保活的序号很快就落在窗口外被当成重放丢掉 —— 表现是「UDP 时通时不通」，
    /// 而语音本身看着一切正常，极难查。
    #[test]
    fn keepalives_survive_a_flood_of_voice_packets() {
        let router = VoiceRouter::new(UdpSocket::bind("127.0.0.1:0").unwrap());
        let up = [1u8; 32];
        router.register(7, &up, &[2u8; 32]);
        let cipher = VoiceCipher::new(&up);
        let from: SocketAddr = "127.0.0.1:5000".parse().unwrap();

        let send = |seq: u32, flags: u8| {
            let mut wire = Vec::new();
            cipher
                .seal(
                    VoiceHeader {
                        session: 7,
                        seq,
                        timestamp: seq.wrapping_mul(480),
                        flags,
                    },
                    &[9; 40],
                    &mut wire,
                )
                .unwrap();
            router.accept(from, &wire)
        };

        // 保活先来一个
        assert!(matches!(
            send(0, FLAG_KEEPALIVE),
            Some(Incoming::Keepalive(..))
        ));

        // 然后灌一大堆语音，远超窗口宽度
        for seq in 0..500u32 {
            assert!(
                matches!(send(seq, 0), Some(Incoming::Voice(_))),
                "语音包 {seq} 被丢了"
            );
        }

        // 接着来的保活照样要收 —— 它有自己的窗口
        for seq in 1..5u32 {
            assert!(
                matches!(send(seq, FLAG_KEEPALIVE), Some(Incoming::Keepalive(..))),
                "语音灌了一波之后，保活 {seq} 被当成重放丢了"
            );
        }

        // 但保活自己的重放还是要挡住
        assert!(send(1, FLAG_KEEPALIVE).is_none(), "保活的重放没挡住");
    }

    #[test]
    fn unregister_forgets_the_address_too() {
        let router = VoiceRouter::new(UdpSocket::bind("127.0.0.1:0").unwrap());
        let up = [1u8; 32];
        router.register(7, &up, &[2u8; 32]);
        let from: SocketAddr = "127.0.0.1:5000".parse().unwrap();
        let mut wire = Vec::new();
        VoiceCipher::new(&up)
            .seal(
                VoiceHeader {
                    session: 7,
                    seq: 1,
                    timestamp: 0,
                    flags: 0,
                },
                &[9; 40],
                &mut wire,
            )
            .unwrap();
        router.accept(from, &wire).unwrap();
        assert_eq!(router.by_addr.lock().unwrap().len(), 1);

        router.unregister(7);
        assert!(router.peers.lock().unwrap().is_empty());
        assert!(
            router.by_addr.lock().unwrap().is_empty(),
            "走了之后地址映射也要清掉"
        );
    }

    fn voice_header(seq: u32, flags: u8) -> VoiceHeader {
        VoiceHeader {
            session: 1,
            seq,
            timestamp: 0,
            flags,
        }
    }

    #[test]
    fn uplink_loss_counts_sequence_gaps_within_the_window() {
        let mut stats = UplinkStats::default();
        let start = Instant::now();
        // 100 包里丢了 10 个：序号 0..100 跳过所有 7 的倍数之外的……简单点：跳过 10 个。
        let mut sent = 0u32;
        for seq in 0..100u32 {
            if seq % 10 == 3 {
                continue;
            }
            stats.record(
                voice_header(seq, 0),
                start + Duration::from_millis(u64::from(seq) * 10),
            );
            sent += 1;
        }
        assert_eq!(sent, 90);
        let loss = stats.loss_permille(start + Duration::from_secs(1)).unwrap();
        assert_eq!(loss, 100, "丢了 10/100，千分之 100");
    }

    #[test]
    fn uplink_loss_forgets_old_seconds_and_tolerates_reordering() {
        let mut stats = UplinkStats::default();
        let start = Instant::now();
        // 第一秒丢一半。
        for seq in (0..100u32).step_by(2) {
            stats.record(voice_header(seq, 0), start);
        }
        // 第一个包没有「前一个」可比缺口，只算应到 1 实到 1；之后每步应到 2 实到 1：
        // 应到 1 + 49×2 = 99，实到 50，丢 49/99。
        assert_eq!(stats.loss_permille(start).unwrap(), 494);
        // 十一秒之后全部补齐、另一段完整的 100 包，老的那一秒已经滚出窗口。
        let later = start + Duration::from_secs(12);
        for seq in 200..300u32 {
            stats.record(voice_header(seq, 0), later);
        }
        assert_eq!(stats.loss_permille(later).unwrap(), 0);
        // 乱序：300 之后先到 302 再到 301，应到 3 实到 3，不算丢。
        stats.record(voice_header(300, 0), later);
        stats.record(voice_header(302, 0), later);
        stats.record(voice_header(301, 0), later);
        assert_eq!(stats.loss_permille(later).unwrap(), 0);
    }

    #[test]
    fn uplink_falls_back_to_keepalives_when_nobody_talks() {
        let mut stats = UplinkStats::default();
        let start = Instant::now();
        assert_eq!(stats.loss_permille(start), None, "一个包都没有：不知道");
        // 保活每两秒一个，发了 10 个只到 8 个。
        for i in 0..10u32 {
            if i == 4 || i == 7 {
                continue;
            }
            stats.record(
                voice_header(i, FLAG_KEEPALIVE),
                start + Duration::from_secs(u64::from(i) * 2),
            );
        }
        let now = start + Duration::from_secs(20);
        assert_eq!(stats.loss_permille(now).unwrap(), 200);
        // 只有三个语音包：样本不够，仍然用保活的数。
        for seq in 0..3u32 {
            stats.record(voice_header(seq, 0), now);
        }
        assert_eq!(stats.loss_permille(now).unwrap(), 200);
    }

    #[test]
    fn network_snapshot_reports_measured_and_self_reported_values() {
        let router = VoiceRouter::new(UdpSocket::bind("127.0.0.1:0").unwrap());
        router.register(1, &[1; 32], &[2; 32]);
        router.register(2, &[3; 32], &[4; 32]);
        let now = Instant::now();
        for seq in 0..50u32 {
            if seq % 5 == 0 {
                continue;
            }
            router.record_uplink_at(2, voice_header(seq, 0), now);
        }
        router.note_report(
            2,
            SelfReport {
                rtt_ms: Some(180),
                downlink_loss_permille: Some(35),
            },
        );
        let snapshot = router.network_snapshot_at(now);
        assert_eq!(snapshot.len(), 2);
        assert_eq!(snapshot[0].session_id, 1);
        assert!(!snapshot[0].uplink_known && !snapshot[0].rtt_known);
        let two = &snapshot[1];
        assert_eq!(two.session_id, 2);
        assert!(two.uplink_known);
        // 序号 1..49 里跳过了 5 的倍数（0 本身没发）：应到 49，实到 40，丢 9/49。
        assert_eq!(two.uplink_loss_permille, 183);
        assert_eq!((two.rtt_ms, two.rtt_known), (180, true));
        assert_eq!((two.downlink_loss_permille, two.downlink_known), (35, true));
    }
}
