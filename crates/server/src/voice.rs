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
use voice_core::tunnel::{DatagramQueue, ReplayWindow};

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
    replay: ReplayWindow,
    /// 保活包**单独一个窗口**。
    ///
    /// 它跟语音是两条互不相干的序号流：语音每秒推进 100，保活每 2 秒才一个。
    /// 共用一个窗口的话，保活的序号会飞快地落到窗口外，然后被当成重放丢掉 ——
    /// 表现是「UDP 时通时不通」，而语音本身看着一切正常。
    keepalive_replay: ReplayWindow,
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
                replay: ReplayWindow::default(),
                keepalive_replay: ReplayWindow::default(),
            },
        );
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
}
