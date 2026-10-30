// SPDX-License-Identifier: MPL-2.0

//! 端到端：起一个真服务端，拿真 TLS + 真 Ed25519 身份连进去。
//!
//! `state` 的单元测试能钉住规则，但钉不住「规则有没有被正确接上线」——
//! 少发一条广播、签名验错了对象、文字消息没被服务端盖章，单元测试全都看不见。
//! 这个文件里跑的是完整的一条路：TCP → TLS（证书固定）→ 挑战应答 → 进频道 → 说话。

use std::io::{Read, Write};
use std::net::{SocketAddr, TcpListener, TcpStream};
use std::sync::Arc;
use std::time::Duration;

use protocol::control::{
    decode_frame, encode_frame, goodbye, server_message, Authenticate, ClientMessage, Hello,
    JoinChannel, Ping, Rejected, SelfState, ServerMessage, TextMessage, Welcome, PROTOCOL_VERSION,
};
use protocol::Fingerprint;
use rustls::pki_types::ServerName;
use rustls::{ClientConnection, StreamOwned};
use server::conn::Hub;
use server::state::{Config, Server};
use transport::{client_config, server_config, ServerCert};
use voice_core::identity::Identity;

/// 测试里等一条消息最多等多久。本地回环上正常是微秒级；这个值只是
/// 防止测试挂死，撞上它基本就等于「服务端没发」。
const RECV_TIMEOUT: Duration = Duration::from_secs(5);

/// 这个文件只测控制面。语音那半边在 `tests/voice.rs`。
struct TestServer {
    addr: SocketAddr,
    fingerprint: Fingerprint,
    hub: Arc<Hub>,
}

fn start(config: Config) -> TestServer {
    let cert = ServerCert::generate().unwrap();
    let fingerprint = cert.fingerprint();
    let tls = Arc::new(server_config(&cert).unwrap());
    let listener = TcpListener::bind("127.0.0.1:0").unwrap();
    let addr = listener.local_addr().unwrap();
    let voice_socket = std::net::UdpSocket::bind("127.0.0.1:0").unwrap();
    let hub = Arc::new(Hub::new(Server::new(config), voice_socket));

    let voice_hub = Arc::clone(&hub);
    std::thread::spawn(move || voice_hub.run_voice());

    let accept_hub = Arc::clone(&hub);
    std::thread::spawn(move || server::accept_loop(listener, tls, accept_hub));

    TestServer {
        addr,
        fingerprint,
        hub,
    }
}

/// 一个合成客户端。只做测试要的那点事：发一条、收一条。
struct Client {
    stream: StreamOwned<ClientConnection, TcpStream>,
    buffered: Vec<u8>,
    identity: Identity,
    session: u32,
}

impl Client {
    fn connect(server: &TestServer) -> Self {
        Self::connect_as(server, Identity::generate().unwrap())
    }

    fn connect_as(server: &TestServer, identity: Identity) -> Self {
        let config = Arc::new(client_config(server.fingerprint).unwrap());
        let mut conn =
            ClientConnection::new(config, ServerName::try_from("gouhuo").unwrap()).unwrap();
        let mut sock = TcpStream::connect(server.addr).unwrap();
        conn.complete_io(&mut sock).expect("TLS 握手失败");
        sock.set_read_timeout(Some(RECV_TIMEOUT)).unwrap();
        Self {
            stream: StreamOwned::new(conn, sock),
            buffered: Vec::new(),
            identity,
            session: 0,
        }
    }

    fn send(&mut self, message: impl Into<ClientMessage>) {
        let mut out = Vec::new();
        encode_frame(&message.into(), &mut out).unwrap();
        self.stream.write_all(&out).unwrap();
        self.stream.flush().unwrap();
    }

    fn send_raw(&mut self, message: &ClientMessage) {
        let mut out = Vec::new();
        encode_frame(message, &mut out).unwrap();
        self.stream.write_all(&out).unwrap();
        self.stream.flush().unwrap();
    }

    fn recv(&mut self) -> ServerMessage {
        loop {
            if let Some((message, used)) = decode_frame::<ServerMessage>(&self.buffered).unwrap() {
                self.buffered.drain(..used);
                return message;
            }
            let mut chunk = [0u8; 8192];
            let n = self
                .stream
                .read(&mut chunk)
                .expect("等服务端的消息等超时了 —— 它没发");
            assert_ne!(n, 0, "服务端把连接关了");
            self.buffered.extend_from_slice(&chunk[..n]);
        }
    }

    /// 一直收到 Pong 为止，中间别的消息丢掉。
    ///
    /// Ping/Pong 在这里当「栅栏」用：Pong 回来了，就说明它前面发的那些
    /// 都已经被服务端处理过了。中间夹着别的广播是正常的 ——
    /// 比如自己改了状态，服务端会把结果也回给自己。
    fn ping_barrier(&mut self, stamp: i64) {
        self.send(Ping {
            timestamp: stamp,
            udp_packets_received: 0,
            ..Default::default()
        });
        for _ in 0..16 {
            if let Some(server_message::Payload::Pong(pong)) = self.recv().payload {
                assert_eq!(pong.timestamp, stamp, "Pong 要把原样的时间戳带回来");
                return;
            }
        }
        panic!("发了 Ping 但一直没等到 Pong");
    }

    /// 走完握手。返回 Welcome，或者被拒的理由。
    fn login(&mut self, code: &str, name: &str) -> Result<Welcome, Rejected> {
        self.send(Hello {
            protocol_version: PROTOCOL_VERSION,
            client_version: "gouhuo test".into(),
            public_key: self.identity.public_key().0.to_vec(),
            probe_only: false,
        });
        self.finish_login(code, name, true)
    }

    /// 同上，但 `honest = false` 时用另一把私钥签 —— 模拟拿着别人公钥冒充。
    fn finish_login(&mut self, code: &str, name: &str, honest: bool) -> Result<Welcome, Rejected> {
        let nonce = match self.recv().payload {
            Some(server_message::Payload::Challenge(c)) => c.nonce,
            Some(server_message::Payload::Rejected(r)) => return Err(r),
            other => panic!("第一条应该是 Challenge，实际是 {other:?}"),
        };
        let signature = if honest {
            self.identity.sign(&nonce)
        } else {
            Identity::generate().unwrap().sign(&nonce)
        };
        self.send(Authenticate {
            signature: signature.to_vec(),
            invite_code: code.into(),
            desired_name: name.into(),
        });
        match self.recv().payload {
            Some(server_message::Payload::Welcome(w)) => {
                self.session = w.session_id;
                Ok(w)
            }
            Some(server_message::Payload::Rejected(r)) => Err(r),
            other => panic!("应该是 Welcome 或 Rejected，实际是 {other:?}"),
        }
    }
}

fn open_server() -> TestServer {
    start(Config::default())
}

/// 两个人先后进来，都能看见对方。
#[test]
fn two_clients_see_each_other() {
    let server = open_server();

    let mut alice = Client::connect(&server);
    let welcome = alice.login("", "阿狸").unwrap();
    assert_eq!(welcome.name, "阿狸");
    assert_eq!(welcome.users.len(), 1, "第一个人进来时只该看见自己");
    assert_eq!(
        welcome.current_channel_id, welcome.channels[0].id,
        "默认该落在根频道"
    );
    assert_ne!(welcome.udp_port, 0, "Welcome 里必须带上真的 UDP 端口");

    let mut bob = Client::connect(&server);
    let bob_welcome = bob.login("", "波波").unwrap();
    assert_eq!(bob_welcome.users.len(), 2, "第二个人该在名单里看见第一个");

    // 阿狸这边要收到「有人进来了」
    let event = alice.recv();
    let Some(server_message::Payload::UserState(state)) = event.payload else {
        panic!("阿狸没收到波波进来的通知：{event:?}");
    };
    let user = state.user.unwrap();
    assert_eq!(user.name, "波波");
    assert_eq!(user.session_id, bob_welcome.session_id);
}

/// 重名的人进来要被自动改名 —— 不能两个人在列表里长得一模一样。
#[test]
fn duplicate_names_get_a_suffix() {
    let server = open_server();
    let mut a = Client::connect(&server);
    a.login("", "阿狸").unwrap();
    let mut b = Client::connect(&server);
    let welcome = b.login("", "阿狸").unwrap();
    assert_ne!(welcome.name, "阿狸");
    assert!(welcome.name.starts_with("阿狸"), "{}", welcome.name);
}

/// **服务端必须给文字消息盖章。** 客户端填的发送者和时间一律作废，
/// 否则任何人都能冒充别人说话。
#[test]
fn text_message_is_stamped_by_the_server() {
    let server = open_server();
    let mut alice = Client::connect(&server);
    let alice_welcome = alice.login("", "阿狸").unwrap();
    let mut bob = Client::connect(&server);
    let bob_welcome = bob.login("", "波波").unwrap();
    let _ = alice.recv(); // 波波进来的通知

    // 波波发言，但谎称自己是阿狸，时间戳也是瞎填的
    bob.send_raw(
        &TextMessage {
            channel_id: 999,
            sender_session_id: alice_welcome.session_id,
            body: "我是阿狸".into(),
            timestamp_ms: 1,
        }
        .into_client(),
    );

    let got = alice.recv();
    let Some(server_message::Payload::TextMessage(text)) = got.payload else {
        panic!("阿狸没收到消息：{got:?}");
    };
    assert_eq!(text.body, "我是阿狸");
    assert_eq!(
        text.sender_session_id, bob_welcome.session_id,
        "冒充成功了 —— 服务端没有覆盖发送者"
    );
    assert_ne!(text.timestamp_ms, 1, "时间戳该由服务端盖");
    assert_eq!(
        text.channel_id, alice_welcome.current_channel_id,
        "频道也该由服务端填"
    );
}

/// 签名验不过就是进不来，哪怕公钥是真的、邀请码是对的。
#[test]
fn bad_signature_is_rejected() {
    use protocol::control::rejected::Reason;
    let server = open_server();
    let victim = Identity::generate().unwrap();

    let mut attacker = Client::connect(&server);
    // 拿着受害者的公钥打招呼……
    attacker.send(Hello {
        protocol_version: PROTOCOL_VERSION,
        client_version: "gouhuo test".into(),
        public_key: victim.public_key().0.to_vec(),
        probe_only: false,
    });
    // ……但签名是别的私钥签的
    let rejected = attacker
        .finish_login("", "冒牌货", false)
        .expect_err("签名对不上却让进来了");
    assert_eq!(rejected.reason, Reason::BadSignature as i32);
    assert_eq!(server.hub.user_count(), 0);
}

/// 没有邀请码进不来，有正确的就能进。
#[test]
fn invite_code_gates_the_door() {
    use protocol::control::rejected::Reason;
    let server = start(Config {
        require_invite: true,
        invite_code: Some("winter2026".into()),
        ..Config::default()
    });

    let mut wrong = Client::connect(&server);
    let rejected = wrong
        .login("nope", "路人")
        .expect_err("邀请码不对却让进来了");
    assert_eq!(rejected.reason, Reason::InviteRequired as i32);
    assert!(!rejected.detail.is_empty(), "拒绝要说人话，不能只给个枚举");

    let mut right = Client::connect(&server);
    right.login("winter2026", "自己人").unwrap();
    assert_eq!(server.hub.user_count(), 1);
}

/// 协议版本对不上要说清楚是谁该升级。
#[test]
fn version_mismatch_says_what_to_do() {
    use protocol::control::rejected::Reason;
    let server = open_server();
    let mut client = Client::connect(&server);
    client.send(Hello {
        protocol_version: PROTOCOL_VERSION + 7,
        client_version: "来自未来".into(),
        public_key: vec![1u8; 32],
        probe_only: false,
    });
    let Some(server_message::Payload::Rejected(rejected)) = client.recv().payload else {
        panic!("版本不对却没被拒");
    };
    assert_eq!(rejected.reason, Reason::VersionMismatch as i32);
    assert!(rejected.detail.contains("升级"), "{}", rejected.detail);
}

/// 同一个身份再连一次会把旧连接顶掉 —— 换机器、客户端崩了重开都会走到这里。
///
/// 关键是**不能变成两个人**：身份是公钥，同一把钥匙在线上只该有一个身影。
#[test]
fn same_identity_displaces_the_old_session() {
    let server = open_server();
    let identity = Identity::generate().unwrap();

    let mut first = Client::connect_as(&server, clone_identity(&identity));
    let first_welcome = first.login("", "阿狸").unwrap();

    let mut second = Client::connect_as(&server, identity);
    let second_welcome = second.login("", "阿狸").unwrap();
    assert_ne!(
        second_welcome.session_id, first_welcome.session_id,
        "顶号之后该是一个新会话"
    );

    // 旧连接先收到一句为什么 —— 客户端靠它知道**别自动重连**，
    // 不然两端会互相顶个没完 ——
    let Some(server_message::Payload::Goodbye(bye)) = first.recv().payload else {
        panic!("被顶掉之前没说为什么");
    };
    assert_eq!(bye.reason, goodbye::Reason::Displaced as i32);

    // —— 然后被服务端关掉
    let mut chunk = [0u8; 256];
    let n = first.stream.read(&mut chunk).unwrap_or(0);
    assert_eq!(n, 0, "旧连接没被关掉");

    assert_eq!(server.hub.user_count(), 1, "顶号顶成了两个人");
    assert_eq!(second_welcome.users.len(), 1);
}

/// 进频道、改自己的静音状态，别人都要看得见。
#[test]
fn channel_and_self_state_propagate() {
    let server = open_server();
    let mut alice = Client::connect(&server);
    let welcome = alice.login("", "阿狸").unwrap();
    let root = welcome.current_channel_id;

    let mut bob = Client::connect(&server);
    bob.login("", "波波").unwrap();
    let _ = alice.recv(); // 波波进来

    bob.send(SelfState {
        self_muted: false,
        self_deafened: true,
    });
    let Some(server_message::Payload::UserState(state)) = alice.recv().payload else {
        panic!("没收到状态变化");
    };
    let user = state.user.unwrap();
    assert!(user.self_deafened);
    assert!(
        user.self_muted,
        "关了耳朵就该同时闭麦 —— 服务端不能指望客户端自觉"
    );

    // 换到一个不存在的频道：应该原地不动，而不是把人弄丢、更不能断开
    bob.send(JoinChannel {
        channel_id: root + 12345,
    });
    bob.ping_barrier(7);
    assert_eq!(server.hub.user_count(), 2);
}

/// 认不出来的消息必须被忽略，而不是断开连接。
///
/// 这是 protobuf 演进语义在连接层的落点：新客户端发来一条老服务端不认识的
/// 东西，老服务端该当没看见。写死这条，将来加消息类型才不用同时升级两边。
#[test]
fn unknown_message_does_not_kill_the_connection() {
    let server = open_server();
    let mut client = Client::connect(&server);
    client.login("", "阿狸").unwrap();

    // payload = None，就是「一个我不认识的分支」解码之后的样子
    client.send_raw(&ClientMessage { payload: None });

    client.ping_barrier(99);
}

/// 走了之后别人要收到 UserLeft，人数也要减回去。
#[test]
fn leaving_is_broadcast() {
    let server = open_server();
    let mut alice = Client::connect(&server);
    alice.login("", "阿狸").unwrap();
    let mut bob = Client::connect(&server);
    let bob_welcome = bob.login("", "波波").unwrap();
    let _ = alice.recv(); // 波波进来

    drop(bob);

    let Some(server_message::Payload::UserLeft(left)) = alice.recv().payload else {
        panic!("波波走了但没人知道");
    };
    assert_eq!(left.session_id, bob_welcome.session_id);

    // 状态是在读线程里清的，给它一点时间
    for _ in 0..100 {
        if server.hub.user_count() == 1 {
            break;
        }
        std::thread::sleep(Duration::from_millis(10));
    }
    assert_eq!(server.hub.user_count(), 1);
}

/// 连上来就不说话的连接必须被握手超时清掉，不能白占一个线程。
#[test]
fn silent_connection_is_eventually_dropped() {
    let server = open_server();
    let mut client = Client::connect(&server);
    // 什么都不发。服务端应该在 HANDSHAKE_TIMEOUT 之后把连接关掉。
    client
        .stream
        .sock
        .set_read_timeout(Some(
            server::conn::HANDSHAKE_TIMEOUT + Duration::from_secs(5),
        ))
        .unwrap();
    let mut chunk = [0u8; 64];
    let n = client.stream.read(&mut chunk).unwrap_or(0);
    assert_eq!(n, 0, "不说话的连接一直没被清掉");
}

/// `Identity` 没有 Clone（私钥不该被随手复制），测试里要两份就走导出导入。
fn clone_identity(identity: &Identity) -> Identity {
    Identity::import(&identity.export()).unwrap()
}

/// 0.1.x 的协议 v1 必须在进入认证前明确拒绝。
#[test]
fn legacy_v1_is_rejected_before_authentication() {
    let server = open_server();
    let mut old = Client::connect(&server);
    old.send(Hello {
        protocol_version: 1,
        client_version: "0.1.1".into(),
        public_key: old.identity.public_key().0.to_vec(),
        probe_only: false,
    });
    let Some(server_message::Payload::Rejected(rejected)) = old.recv().payload else {
        panic!("旧客户端没有收到版本拒绝")
    };
    assert_eq!(
        rejected.reason,
        protocol::control::rejected::Reason::VersionMismatch as i32
    );
    assert_eq!(server.hub.user_count(), 0);
}
