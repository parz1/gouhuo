// SPDX-License-Identifier: MPL-2.0

//! 端到端：一个人说话，同频道的另一个人听见。
//!
//! 走的是完整的真路径 —— 真 TLS 握手、从握手派生的语音密钥、真 UDP、
//! 真 ChaCha20-Poly1305。这条链上任何一环接错（密钥方向反了、头部被改了、
//! 转发给了错的人），这里都会红。
//!
//! 这些测试证明的是「**语音密钥不需要第二次握手**」这个设计成立：
//! 客户端和服务端各自从同一条 TLS 连接里导出密钥，从不交换任何东西，
//! 而它们必须一模一样。

use std::io::{Read, Write};
use std::net::{SocketAddr, TcpListener, TcpStream, UdpSocket};
use std::sync::Arc;
use std::time::Duration;

use protocol::control::{
    decode_frame, encode_frame, server_message, Authenticate, ClientMessage, Hello, ServerMessage,
    Welcome, PROTOCOL_VERSION,
};
use protocol::{VoiceCipher, VoiceHeader, FLAG_KEEPALIVE};
use rustls::pki_types::ServerName;
use rustls::{ClientConnection, StreamOwned};
use server::conn::Hub;
use server::state::{Config, Server};
use transport::{client_config, derive_voice_key, server_config, ServerCert, DOWNSTREAM, UPSTREAM};
use voice_core::identity::Identity;

const RECV_TIMEOUT: Duration = Duration::from_secs(5);

struct TestServer {
    tcp: SocketAddr,
    udp: SocketAddr,
    fingerprint: protocol::Fingerprint,
    hub: Arc<Hub>,
}

fn start() -> TestServer {
    let cert = ServerCert::generate().unwrap();
    let fingerprint = cert.fingerprint();
    let tls = Arc::new(server_config(&cert).unwrap());
    let listener = TcpListener::bind("127.0.0.1:0").unwrap();
    let tcp = listener.local_addr().unwrap();
    let voice_socket = UdpSocket::bind("127.0.0.1:0").unwrap();
    let udp = voice_socket.local_addr().unwrap();

    let hub = Arc::new(Hub::new(Server::new(Config::default()), voice_socket));
    let voice_hub = Arc::clone(&hub);
    std::thread::spawn(move || voice_hub.run_voice());
    let accept_hub = Arc::clone(&hub);
    std::thread::spawn(move || server::accept_loop(listener, tls, accept_hub));

    TestServer {
        tcp,
        udp,
        fingerprint,
        hub,
    }
}

/// 一个会说话的客户端：控制面走 TLS，语音走自己的 UDP socket。
struct Client {
    control: StreamOwned<ClientConnection, TcpStream>,
    buffered: Vec<u8>,
    udp: UdpSocket,
    /// 发出去的包用这把封。跟服务端的 upstream 是同一把。
    seal: VoiceCipher,
    /// 收到的包用这把开。跟服务端的 downstream 是同一把。
    open: VoiceCipher,
    welcome: Welcome,
    seq: u32,
}

impl Client {
    fn join(server: &TestServer, name: &str) -> Self {
        let config = Arc::new(client_config(server.fingerprint).unwrap());
        let mut conn =
            ClientConnection::new(config, ServerName::try_from("gouhuo").unwrap()).unwrap();
        let mut sock = TcpStream::connect(server.tcp).unwrap();
        conn.complete_io(&mut sock).expect("TLS 握手失败");

        // 密钥在这里就有了 —— 跟服务端没交换过任何东西。
        let seal = VoiceCipher::new(derive_voice_key(&conn, UPSTREAM).unwrap().as_bytes());
        let open = VoiceCipher::new(derive_voice_key(&conn, DOWNSTREAM).unwrap().as_bytes());

        sock.set_read_timeout(Some(RECV_TIMEOUT)).unwrap();
        let udp = UdpSocket::bind("127.0.0.1:0").unwrap();
        udp.set_read_timeout(Some(RECV_TIMEOUT)).unwrap();
        udp.connect(server.udp).unwrap();

        let mut client = Self {
            control: StreamOwned::new(conn, sock),
            buffered: Vec::new(),
            udp,
            seal,
            open,
            welcome: Welcome::default(),
            seq: 0,
        };

        let identity = Identity::generate().unwrap();
        client.send(
            Hello {
                protocol_version: PROTOCOL_VERSION,
                client_version: "gouhuo test".into(),
                public_key: identity.public_key().0.to_vec(),
                probe_only: false,
            }
            .into(),
        );
        let nonce = match client.recv().payload {
            Some(server_message::Payload::Challenge(c)) => c.nonce,
            other => panic!("没收到 Challenge：{other:?}"),
        };
        client.send(
            Authenticate {
                signature: identity.sign(&nonce).to_vec(),
                invite_code: String::new(),
                desired_name: name.into(),
            }
            .into(),
        );
        client.welcome = match client.recv().payload {
            Some(server_message::Payload::Welcome(w)) => w,
            other => panic!("没收到 Welcome：{other:?}"),
        };
        client
    }

    fn send(&mut self, message: ClientMessage) {
        let mut out = Vec::new();
        encode_frame(&message, &mut out).unwrap();
        self.control.write_all(&out).unwrap();
        self.control.flush().unwrap();
    }

    fn recv(&mut self) -> ServerMessage {
        loop {
            if let Some((message, used)) = decode_frame::<ServerMessage>(&self.buffered).unwrap() {
                self.buffered.drain(..used);
                return message;
            }
            let mut chunk = [0u8; 8192];
            let n = self.control.read(&mut chunk).expect("控制面等超时");
            assert_ne!(n, 0, "服务端把连接关了");
            self.buffered.extend_from_slice(&chunk[..n]);
        }
    }

    fn header(&self, flags: u8) -> VoiceHeader {
        VoiceHeader {
            session: self.welcome.session_id,
            seq: self.seq,
            timestamp: self.seq.wrapping_mul(960),
            flags,
        }
    }

    /// 发一个保活包，让服务端学到我们的 UDP 地址，并等它回一个。
    ///
    /// 只听不说的人**必须**做这件事，否则服务端永远不知道往哪儿发给他。
    fn keepalive(&mut self) {
        let header = self.header(FLAG_KEEPALIVE);
        self.seq += 1;
        let mut wire = Vec::new();
        self.seal.seal(header, &[], &mut wire).unwrap();
        self.udp.send(&wire).unwrap();

        let echoed = self.recv_voice().expect("保活没回来 —— UDP 那条路没通");
        assert!(echoed.0.is_keepalive(), "回来的不是保活包");
        assert_eq!(echoed.0.timestamp, header.timestamp, "保活要原样回时间戳");
    }

    fn speak(&mut self, payload: &[u8]) -> VoiceHeader {
        let header = self.header(0);
        self.seq += 1;
        let mut wire = Vec::new();
        self.seal.seal(header, payload, &mut wire).unwrap();
        self.udp.send(&wire).unwrap();
        header
    }

    /// 收一个语音包并解开。超时返回 None。
    fn recv_voice(&self) -> Option<(VoiceHeader, Vec<u8>)> {
        let mut buf = [0u8; 2048];
        let n = self.udp.recv(&mut buf).ok()?;
        let mut payload = Vec::new();
        let header = self.open.open(&buf[..n], &mut payload).expect("解不开");
        Some((header, payload))
    }
}

/// 最基本的那条：甲说话，乙听见，而且知道是甲说的。
#[test]
fn one_speaks_the_other_hears() {
    let server = start();
    let mut alice = Client::join(&server, "阿狸");
    let mut bob = Client::join(&server, "波波");

    // 两边都先报个到，服务端才知道往哪儿发
    alice.keepalive();
    bob.keepalive();

    let payload: Vec<u8> = (0..80u8).collect();
    let sent = alice.speak(&payload);

    let (header, got) = bob.recv_voice().expect("波波没听见阿狸说话");
    assert_eq!(got, payload, "转发过程中负载被改了");
    assert_eq!(
        header.session, alice.welcome.session_id,
        "包里的说话者不是阿狸 —— 客户端会把声音挂到错的人头上"
    );
    assert_eq!(header.seq, sent.seq, "seq 必须原样带过去，抖动缓冲靠它排序");
    assert_eq!(header.timestamp, sent.timestamp);
}

/// 说话的人不该收到自己的回声。
#[test]
fn a_speaker_does_not_hear_themselves() {
    let server = start();
    let mut alice = Client::join(&server, "阿狸");
    let mut bob = Client::join(&server, "波波");
    alice.keepalive();
    bob.keepalive();

    alice.speak(&[7; 60]);
    assert!(bob.recv_voice().is_some(), "波波该听见");

    alice
        .udp
        .set_read_timeout(Some(Duration::from_millis(300)))
        .unwrap();
    assert!(alice.recv_voice().is_none(), "阿狸听见了自己的回声");
}

/// 拿着别人的 session id 伪造的包，一个人都不该收到。
///
/// 这正是「每条连接一把密钥」买来的东西：伪造者没有阿狸的上行密钥，
/// tag 就对不上，服务端连转发都不会转。
#[test]
fn a_forged_packet_reaches_nobody() {
    let server = start();
    let mut alice = Client::join(&server, "阿狸");
    let mut bob = Client::join(&server, "波波");
    alice.keepalive();
    bob.keepalive();

    // 第三个人，有合法身份，但用错误的密钥冒充阿狸
    let mallory = Client::join(&server, "马洛里");
    let forged_header = VoiceHeader {
        session: alice.welcome.session_id,
        seq: 5000,
        timestamp: 5000 * 960,
        flags: 0,
    };
    let mut wire = Vec::new();
    VoiceCipher::new(&[0xEE; 32])
        .seal(forged_header, &[1; 60], &mut wire)
        .unwrap();
    mallory.udp.send(&wire).unwrap();

    bob.udp
        .set_read_timeout(Some(Duration::from_millis(300)))
        .unwrap();
    assert!(bob.recv_voice().is_none(), "伪造的包被转发出去了");

    // 真的阿狸说话还是正常的 —— 刚才那个包不该污染任何状态
    alice.speak(&[3; 60]);
    bob.udp.set_read_timeout(Some(RECV_TIMEOUT)).unwrap();
    let (header, _) = bob.recv_voice().expect("伪造的包把正常转发弄坏了");
    assert_eq!(header.session, alice.welcome.session_id);
}

/// 重放一个原样的包，第二次必须被丢掉。
#[test]
fn a_replayed_packet_is_dropped() {
    let server = start();
    let mut alice = Client::join(&server, "阿狸");
    let mut bob = Client::join(&server, "波波");
    alice.keepalive();
    bob.keepalive();

    let header = alice.header(0);
    let mut wire = Vec::new();
    alice.seal.seal(header, &[5; 60], &mut wire).unwrap();

    alice.udp.send(&wire).unwrap();
    assert!(bob.recv_voice().is_some(), "第一次该收到");

    alice.udp.send(&wire).unwrap();
    bob.udp
        .set_read_timeout(Some(Duration::from_millis(300)))
        .unwrap();
    assert!(bob.recv_voice().is_none(), "同一个包被转发了两次");
}

/// 走了之后就不该再能往频道里灌声音 —— 哪怕手里还攥着旧密钥。
#[test]
fn a_departed_session_can_no_longer_speak() {
    let server = start();
    let mut alice = Client::join(&server, "阿狸");
    let mut bob = Client::join(&server, "波波");
    alice.keepalive();
    bob.keepalive();

    // 先确认现在是通的
    alice.speak(&[1; 60]);
    assert!(bob.recv_voice().is_some());

    // 阿狸的控制面断了
    let stale_seal = VoiceCipher::new(
        derive_voice_key(&alice.control.conn, UPSTREAM)
            .unwrap()
            .as_bytes(),
    );
    let stale_udp = alice.udp.try_clone().unwrap();
    let stale_header = alice.header(0);
    drop(alice.control);

    // 等服务端处理完断开
    for _ in 0..200 {
        if server.hub.user_count() == 1 {
            break;
        }
        std::thread::sleep(Duration::from_millis(10));
    }
    assert_eq!(server.hub.user_count(), 1);

    let mut wire = Vec::new();
    stale_seal.seal(stale_header, &[2; 60], &mut wire).unwrap();
    stale_udp.send(&wire).unwrap();

    bob.udp
        .set_read_timeout(Some(Duration::from_millis(300)))
        .unwrap();
    assert!(
        bob.recv_voice().is_none(),
        "一个已经离开的会话还能往频道里说话"
    );
}

/// Pong 里的语音包计数要真的动 —— 客户端靠它判断 UDP 通不通、
/// 要不要退回 TCP 传语音。
#[test]
fn pong_reports_udp_packet_count() {
    use protocol::control::Ping;

    let server = start();
    let mut alice = Client::join(&server, "阿狸");

    alice.send(
        Ping {
            timestamp: 1,
            udp_packets_received: 0,
            ..Default::default()
        }
        .into(),
    );
    let Some(server_message::Payload::Pong(before)) = alice.recv().payload else {
        panic!("没收到 Pong");
    };
    assert_eq!(before.udp_packets_received, 0, "还没发过语音就不该有计数");

    alice.keepalive();
    alice.speak(&[1; 40]);
    alice.speak(&[2; 40]);

    // 给 UDP 线程一点时间
    std::thread::sleep(Duration::from_millis(100));
    alice.send(
        Ping {
            timestamp: 2,
            udp_packets_received: 0,
            ..Default::default()
        }
        .into(),
    );
    let Some(server_message::Payload::Pong(after)) = alice.recv().payload else {
        panic!("没收到第二个 Pong");
    };
    assert_eq!(
        after.udp_packets_received, 3,
        "一个保活加两个语音包，服务端该数到 3"
    );
}
