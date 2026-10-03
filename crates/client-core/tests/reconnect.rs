// SPDX-License-Identifier: MPL-2.0

//! 断线自动重连，对着**真服务端**跑。
//!
//! 客户端和服务端之间夹一个 TCP 代理，用它来演各种「网络出事了」：
//!
//! - **掐断**：代理把现有连接全关掉 —— 路由器重启、Wi-Fi 切换
//! - **拒绝**：新连接一进来就关 —— 服务器挂了还没起来
//! - **黑洞**：连接留着，字节全吞掉 —— 拔了网线、NAT 表项过期。
//!   TCP 这时候不会报任何错，只有心跳能发现
//!
//! 语音走的 UDP 不经过代理，这里只管控制面。

use std::io::{Read, Write};
use std::net::{Shutdown, SocketAddr, TcpListener, TcpStream, UdpSocket};
use std::sync::atomic::{AtomicU8, Ordering};
use std::sync::mpsc::{Receiver, RecvTimeoutError};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use client_core::{Client, Ended, Event, Options};
use protocol::Invite;
use server::conn::Hub;
use server::state::{Config, Server};
use transport::{server_config, ServerCert};
use voice_core::identity::Identity;

/// 比 against_server 的长：有的用例要等好几轮退避。
const WAIT: Duration = Duration::from_secs(15);

/// 测试用的时间参数。默认值（5 秒心跳、15 秒判死）会让一个用例跑半分钟。
fn fast() -> Options {
    Options {
        heartbeat: Duration::from_millis(50),
        liveness_timeout: Duration::from_millis(400),
        reconnect_first: Duration::from_millis(50),
        reconnect_max: Duration::from_millis(200),
    }
}

struct TestServer {
    invite: Invite,
    hub: Arc<Hub>,
}

impl TestServer {
    fn addr(&self) -> SocketAddr {
        format!("{}:{}", self.invite.host, self.invite.port)
            .parse()
            .unwrap()
    }
}

fn open_server() -> TestServer {
    let cert = ServerCert::generate().unwrap();
    let fingerprint = cert.fingerprint();
    let tls = Arc::new(server_config(&cert).unwrap());
    let listener = TcpListener::bind("127.0.0.1:0").unwrap();
    let addr = listener.local_addr().unwrap();
    let voice = UdpSocket::bind("127.0.0.1:0").unwrap();

    let hub = Arc::new(Hub::new(Server::new(Config::default()), voice));
    let voice_hub = Arc::clone(&hub);
    std::thread::spawn(move || voice_hub.run_voice());
    let accept_hub = Arc::clone(&hub);
    std::thread::spawn(move || server::accept_loop(listener, tls, accept_hub));

    TestServer {
        invite: Invite {
            host: addr.ip().to_string(),
            port: addr.port(),
            cert: fingerprint,
            code: None,
        },
        hub,
    }
}

const PASS: u8 = 0;
const REFUSE: u8 = 1;
const BLACKHOLE: u8 = 2;

/// 夹在客户端和服务端之间、能被操纵的 TCP 代理。
struct Proxy {
    addr: SocketAddr,
    mode: Arc<AtomicU8>,
    conns: Arc<Mutex<Vec<TcpStream>>>,
}

impl Proxy {
    fn start(upstream: SocketAddr) -> Self {
        let listener = TcpListener::bind("127.0.0.1:0").unwrap();
        let addr = listener.local_addr().unwrap();
        let mode = Arc::new(AtomicU8::new(PASS));
        let conns = Arc::new(Mutex::new(Vec::new()));

        let (accept_mode, accept_conns) = (Arc::clone(&mode), Arc::clone(&conns));
        std::thread::spawn(move || {
            for client in listener.incoming() {
                let Ok(client) = client else { continue };
                if accept_mode.load(Ordering::SeqCst) == REFUSE {
                    // 进来就关：客户端看到的是握手中途连接没了。
                    drop(client);
                    continue;
                }
                let Ok(server) = TcpStream::connect(upstream) else {
                    continue;
                };
                {
                    let mut list = accept_conns.lock().unwrap();
                    list.push(client.try_clone().unwrap());
                    list.push(server.try_clone().unwrap());
                }
                pump(
                    client.try_clone().unwrap(),
                    server.try_clone().unwrap(),
                    Arc::clone(&accept_mode),
                );
                pump(server, client, Arc::clone(&accept_mode));
            }
        });

        Proxy { addr, mode, conns }
    }

    fn invite(&self, server: &TestServer) -> String {
        Invite {
            host: self.addr.ip().to_string(),
            port: self.addr.port(),
            ..server.invite.clone()
        }
        .to_url()
        .unwrap()
    }

    fn set(&self, mode: u8) {
        self.mode.store(mode, Ordering::SeqCst);
    }

    /// 把现有的连接全掐掉。
    fn cut(&self) {
        for sock in self.conns.lock().unwrap().drain(..) {
            let _ = sock.shutdown(Shutdown::Both);
        }
    }
}

fn pump(mut from: TcpStream, mut to: TcpStream, mode: Arc<AtomicU8>) {
    std::thread::spawn(move || {
        let mut buf = [0u8; 16 * 1024];
        loop {
            let n = match from.read(&mut buf) {
                Ok(0) | Err(_) => break,
                Ok(n) => n,
            };
            if mode.load(Ordering::SeqCst) == BLACKHOLE {
                continue;
            }
            if to.write_all(&buf[..n]).is_err() {
                break;
            }
        }
        let _ = to.shutdown(Shutdown::Both);
        let _ = from.shutdown(Shutdown::Both);
    });
}

/// 等一个满足条件的事件，中间别的事件丢掉。
fn wait_for(events: &Receiver<Event>, mut matches: impl FnMut(&Event) -> bool) -> Event {
    let deadline = Instant::now() + WAIT;
    loop {
        let left = deadline.saturating_duration_since(Instant::now());
        match events.recv_timeout(left) {
            Ok(event) if matches(&event) => return event,
            Ok(_) => continue,
            Err(RecvTimeoutError::Timeout) => panic!("等的事件一直没来"),
            Err(RecvTimeoutError::Disconnected) => panic!("事件通道断了"),
        }
    }
}

/// 轮询到条件成立。名单是另一个线程在改，状态要等一会儿才到。
fn eventually(what: &str, mut ok: impl FnMut() -> bool) {
    let deadline = Instant::now() + WAIT;
    while Instant::now() < deadline {
        if ok() {
            return;
        }
        std::thread::sleep(Duration::from_millis(20));
    }
    panic!("一直没等到：{what}");
}

fn channel_named(client: &Client, name: &str) -> Option<u32> {
    client
        .roster()
        .channels
        .values()
        .find(|c| c.name == name)
        .map(|c| c.id)
}

/// 验收标准：网络断一下再好，不碰客户端，能回到原来的频道继续说话，
/// 闭着的麦还是闭着的。
#[test]
fn comes_back_to_the_same_channel_after_the_network_drops() {
    let server = open_server();
    let proxy = Proxy::start(server.addr());
    let (alice, alice_events) = Client::connect_with(
        &proxy.invite(&server),
        &Identity::generate().unwrap(),
        "阿狸",
        fast(),
    )
    .unwrap();
    let (bob, _bob_events) = Client::connect_with(
        &server.invite.to_url().unwrap(),
        &Identity::generate().unwrap(),
        "波波",
        fast(),
    )
    .unwrap();

    alice.create_channel("开黑", 0);
    eventually("频道建好", || channel_named(&alice, "开黑").is_some());
    let room = channel_named(&alice, "开黑").unwrap();
    alice.join_channel(room);
    alice.set_self_state(true, false);
    eventually("阿狸进了开黑、闭了麦", || {
        let roster = bob.roster();
        roster
            .users
            .values()
            .any(|u| u.name == "阿狸" && u.channel_id == room && u.self_muted)
    });
    let old_session = alice.session_id();
    let old_sequences = Arc::clone(&alice.voice_keys().sequences);
    assert_eq!(old_sequences.next(false), Some(0));
    let old_key = *alice.voice_keys().upstream.as_bytes();

    proxy.cut();

    wait_for(&alice_events, |e| matches!(e, Event::Reconnecting { .. }));
    wait_for(&alice_events, |e| matches!(e, Event::Reconnected));

    assert_ne!(alice.session_id(), old_session, "重连是一个新会话");
    assert_ne!(
        alice.voice_keys().upstream.as_bytes(),
        &old_key,
        "新连接必须换语音密钥，否则序号会撞上防重放窗口"
    );
    assert!(!Arc::ptr_eq(&old_sequences, &alice.voice_keys().sequences));
    assert_eq!(alice.voice_keys().sequences.next(false), Some(0));
    eventually(
        "波波那边看到阿狸回到开黑、还闭着麦",
        || {
            let roster = bob.roster();
            let alice_now = roster.users.get(&alice.session_id());
            alice_now.is_some_and(|u| u.channel_id == room && u.self_muted)
        },
    );
    eventually("阿狸自己也在开黑", || {
        alice.roster().my_channel() == room
    });
    eventually(
        "服务端上还是两个人，旧会话没留下幽灵",
        || server.hub.user_count() == 2,
    );
}

/// 服务器挂了一阵子：每次都连不上，但要一直试，起来了就回去。
#[test]
fn keeps_trying_until_the_server_is_back() {
    let server = open_server();
    let proxy = Proxy::start(server.addr());
    let (alice, events) = Client::connect_with(
        &proxy.invite(&server),
        &Identity::generate().unwrap(),
        "阿狸",
        fast(),
    )
    .unwrap();

    proxy.set(REFUSE);
    proxy.cut();

    // 退避在涨，而且没有因为连不上就放弃
    let third = wait_for(
        &events,
        |e| matches!(e, Event::Reconnecting { attempt, .. } if *attempt >= 3),
    );
    let Event::Reconnecting { reason, .. } = third else {
        unreachable!()
    };
    assert!(!reason.is_empty(), "要告诉用户上一次为什么没成");

    proxy.set(PASS);
    wait_for(&events, |e| matches!(e, Event::Reconnected));
    eventually("回到了服务端的名单里", || {
        server.hub.user_count() == 1
    });
    assert_eq!(alice.roster().users.len(), 1);
}

/// 网线拔了：TCP 不报错，读线程会一直阻塞。要靠心跳发现它死了。
#[test]
fn notices_a_silently_dead_connection() {
    let server = open_server();
    let proxy = Proxy::start(server.addr());
    let (_alice, events) = Client::connect_with(
        &proxy.invite(&server),
        &Identity::generate().unwrap(),
        "阿狸",
        fast(),
    )
    .unwrap();

    proxy.set(BLACKHOLE);
    wait_for(&events, |e| matches!(e, Event::Reconnecting { .. }));

    // 网回来了
    proxy.set(PASS);
    wait_for(&events, |e| matches!(e, Event::Reconnected));
}

/// 重连等待中点了取消：立刻停，不用等到下一次退避到点。
#[test]
fn cancelling_stops_the_retry_loop_at_once() {
    let server = open_server();
    let proxy = Proxy::start(server.addr());
    let slow = Options {
        reconnect_first: Duration::from_secs(30),
        reconnect_max: Duration::from_secs(30),
        ..fast()
    };
    let (alice, events) = Client::connect_with(
        &proxy.invite(&server),
        &Identity::generate().unwrap(),
        "阿狸",
        slow,
    )
    .unwrap();

    proxy.cut();
    wait_for(&events, |e| matches!(e, Event::Reconnecting { .. }));

    let asked = Instant::now();
    alice.disconnect();
    let ended = wait_for(&events, |e| matches!(e, Event::Disconnected(_)));
    assert_eq!(ended, Event::Disconnected(Ended::ByUser), "取消不是错误");
    assert!(
        asked.elapsed() < Duration::from_secs(2),
        "点了取消还在等退避：{:?}",
        asked.elapsed()
    );
}

/// 顶号不能重连：两端各自重连会互相踢个没完。
#[test]
fn being_displaced_is_final() {
    let server = open_server();
    let identity = Identity::generate().unwrap();
    let link = server.invite.to_url().unwrap();

    let (_first, first_events) = Client::connect_with(&link, &identity, "阿狸", fast()).unwrap();
    let (_second, _second_events) =
        Client::connect_with(&link, &identity.clone(), "阿狸", fast()).unwrap();

    let ended = wait_for(&first_events, |e| {
        matches!(e, Event::Reconnecting { .. } | Event::Disconnected(_))
    });
    let Event::Disconnected(Ended::Refused { headline, .. }) = ended else {
        panic!("被顶号之后不该重连，也不该当成自己退出：{ended:?}");
    };
    assert!(headline.contains("别处"), "{headline}");

    // 再等几个退避周期，确认没有在背后偷偷重连把第二个顶掉
    std::thread::sleep(Duration::from_millis(600));
    assert_eq!(server.hub.user_count(), 1);
    assert!(
        first_events
            .try_iter()
            .all(|e| !matches!(e, Event::Reconnecting { .. } | Event::Reconnected)),
        "被顶下去的那端在重连"
    );
}

/// 一条安静但活着的连接不能自己断。
///
/// 回归：握手期间设的读超时（`CONNECT_TIMEOUT`，8 秒）要在**读线程的那个句柄**上
/// 撤掉。Windows 上克隆出来的句柄各自带着读超时，只撤一个的话，连接安静 8 秒
/// 就会自己报超时断掉 —— 平时被 5 秒一次的 Pong 盖住了，所以这里把心跳关掉来量。
#[test]
fn a_quiet_connection_is_not_dropped() {
    let server = open_server();
    let silent = Options {
        heartbeat: Duration::from_secs(3600),
        liveness_timeout: Duration::from_secs(3600),
        ..fast()
    };
    let (_alice, events) = Client::connect_with(
        &server.invite.to_url().unwrap(),
        &Identity::generate().unwrap(),
        "阿狸",
        silent,
    )
    .unwrap();

    let quiet = client_core::CONNECT_TIMEOUT + Duration::from_secs(1);
    match events.recv_timeout(quiet) {
        Err(RecvTimeoutError::Timeout) => {}
        other => panic!("一条什么都没发生的连接自己出事了：{other:?}"),
    }
}

/// Domain resolution must not send UDP to a different peer than TCP.
#[test]
fn domain_voice_destination_uses_the_authenticated_tcp_peer() {
    let mut server = open_server();
    server.invite.host = "localhost".into();
    let (client, _) = Client::connect_with(
        &server.invite.to_url().unwrap(),
        &Identity::generate().unwrap(),
        "domain",
        fast(),
    )
    .unwrap();
    let (session, addr, keys) = client.voice_endpoint_session();
    assert_eq!(addr.ip(), "127.0.0.1".parse::<std::net::IpAddr>().unwrap());
    assert_eq!(addr.port(), client.udp_port());
    assert_eq!(session, client.session_id());
    assert!(Arc::ptr_eq(&keys, &client.voice_keys()));

    // Confirm this destination really reaches the authenticated UDP session,
    // rather than merely checking that an address accessor returns an IP.
    let socket = UdpSocket::bind("127.0.0.1:0").unwrap();
    socket
        .set_read_timeout(Some(Duration::from_secs(2)))
        .unwrap();
    let header = protocol::VoiceHeader {
        session,
        seq: keys.sequences.next(true).unwrap(),
        timestamp: 123,
        flags: protocol::FLAG_KEEPALIVE,
    };
    let mut wire = Vec::new();
    protocol::VoiceCipher::new(keys.upstream.as_bytes())
        .seal(header, &[], &mut wire)
        .unwrap();
    socket.send_to(&wire, addr).unwrap();
    let mut buffer = [0; 2048];
    let (n, from) = socket.recv_from(&mut buffer).unwrap();
    assert_eq!(from, addr);
    let reply = protocol::VoiceCipher::new(keys.downstream.as_bytes())
        .open(&buffer[..n], &mut Vec::new())
        .unwrap();
    assert!(reply.is_keepalive());
    assert_eq!(reply.timestamp, 123);
    client.disconnect();
}

/// Voice-only failure can explicitly refresh the session without losing channel or mute state.
#[test]
fn requested_reconnect_restores_channel_and_self_state() {
    let server = open_server();
    let (client, events) = Client::connect_with(
        &server.invite.to_url().unwrap(),
        &Identity::generate().unwrap(),
        "repair",
        fast(),
    )
    .unwrap();
    client.create_channel("repair-room", 0);
    eventually("channel", || {
        channel_named(&client, "repair-room").is_some()
    });
    let room = channel_named(&client, "repair-room").unwrap();
    client.join_channel(room);
    client.set_self_state(true, true);
    eventually("state", || {
        let r = client.roster();
        let u = r.users.get(&r.me).unwrap();
        u.channel_id == room && u.self_muted && u.self_deafened
    });
    let session = client.session_id();
    client.reconnect_transport();
    wait_for(&events, |e| matches!(e, Event::Reconnecting { .. }));
    wait_for(&events, |e| matches!(e, Event::Reconnected));
    assert_ne!(session, client.session_id());
    let (voice_session, addr, keys) = client.voice_endpoint_session();
    assert_eq!(voice_session, client.session_id());
    assert_eq!(addr.ip(), server.addr().ip());
    assert_eq!(addr.port(), client.udp_port());
    assert!(Arc::ptr_eq(&keys, &client.voice_keys()));
    eventually("restored", || {
        let r = client.roster();
        let u = r.users.get(&r.me).unwrap();
        u.channel_id == room && u.self_muted && u.self_deafened
    });
    client.disconnect();
    wait_for(&events, |e| matches!(e, Event::Disconnected(Ended::ByUser)));
    client.reconnect_transport();
    assert!(events.recv_timeout(Duration::from_millis(300)).is_err());
}
