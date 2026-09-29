// SPDX-License-Identifier: MPL-2.0

//! 连接处理：TLS、认证握手、消息循环。
//!
//! 这里只负责把字节搬进搬出；所有「准不准、该怎么改状态」的规则在 `state` 里。
//!
//! # 为什么是线程不是 async
//!
//! 场景是 3–20 人的频道，一台自部署的机器撑一个社群，几十条连接封顶。
//! 这个量级上 async 换不来任何东西，却要带进四十来个依赖 ——
//! 跟「单二进制、自部署零依赖」直接冲突。
//!
//! 边界划在连接处理这一层：真到了需要换的那天，改的是这个文件，
//! `state` 里那套规则一行都不用动。
//!
//! # 读写怎么分开
//!
//! rustls 的连接状态是单份的，读和写都要 `&mut`。所以：
//! - **读**：自己一个 socket 句柄（`try_clone`），阻塞读原始字节，不持锁
//! - **写**：`Mutex<Wire>` 里装着 TLS 状态和写用的 socket 句柄
//! - 读到字节之后才短暂上锁，喂给 TLS 解出明文
//!
//! # 超时为什么用看门狗而不是读超时
//!
//! `SO_RCVTIMEO` 在 Windows 上超时跟到达的数据撞车时会**丢数据** ——
//! M1 在 UDP 上已经被这个坑过一次（见 `voice_core::net`），TCP 上同样成立，
//! 只是表现为流错位而不是丢包，更难查。
//!
//! 所以读永远是阻塞的、没有超时；另起一个看门狗线程看「谁多久没动静了」，
//! 该踢的直接 `shutdown` 那条 socket —— 阻塞的读会立刻返回 0。

use std::collections::HashMap;
use std::io::{self, Read, Write};
use std::net::{Shutdown, TcpStream, UdpSocket};
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};

use protocol::control::{
    client_message, decode_frame, encode_frame, goodbye, peek_frame_len, Challenge, ClientMessage,
    Goodbye, Rejected, Role, ServerMessage, MAX_FRAME_BODY, PROTOCOL_VERSION,
};
use protocol::PublicKey;
use voice_core::identity::Identity;

use crate::state::{Broadcast, Server, SessionId};
use crate::store::Store;
use crate::voice::{Incoming, VoiceRouter};

/// 多久没收到任何东西就算掉线。客户端每隔几秒会发一次 Ping。
pub const IDLE_TIMEOUT: Duration = Duration::from_secs(30);

/// 认证必须在这个时间内走完。
///
/// 没有这个的话，一条连上来什么都不发的连接会永远占着一个线程 ——
/// 开一千条就能把服务端拖垮，而且不需要任何凭证。
pub const HANDSHAKE_TIMEOUT: Duration = Duration::from_secs(10);

/// 挑战随机数的长度。
const CHALLENGE_LEN: usize = 32;

/// 一条连接的 TLS 状态和写用的 socket。
struct Wire {
    conn: rustls::ServerConnection,
    sock: TcpStream,
}

impl Wire {
    /// 把 rustls 攒着的字节真正写出去。
    fn flush_tls(&mut self) -> io::Result<()> {
        while self.conn.wants_write() {
            self.conn.write_tls(&mut self.sock)?;
        }
        self.sock.flush()
    }

    fn send(&mut self, message: &ServerMessage) -> io::Result<()> {
        let mut frame = Vec::new();
        encode_frame(message, &mut frame).map_err(io::Error::other)?;
        self.conn.writer().write_all(&frame)?;
        self.flush_tls()
    }
}

/// 一个已登录的人。广播时通过它把消息推出去。
pub struct Peer {
    pub session: SessionId,
    wire: Arc<Mutex<Wire>>,
    /// 最后一次收到东西的时刻，看门狗要用。存成 Unix 毫秒的原子值，
    /// 免得为了读一个时间戳还要上锁。
    last_seen_ms: AtomicU64,
    /// 是被看门狗踢掉的，还是自己走的。
    ///
    /// 两种情况读线程看到的都是 `read` 返回 0，分不出来 —— 但广播给别人的
    /// `UserLeft` 理由不一样，「掉线了」和「自己退了」在界面上是两回事。
    timed_out: AtomicBool,
}

impl Peer {
    pub fn send(&self, message: &ServerMessage) {
        // 发不出去不是这里该处理的：读线程会在下一次 read 返回 0 时清理。
        // 在广播路径上做清理会让锁的顺序变复杂，而复杂的锁顺序就是死锁。
        if let Ok(mut wire) = self.wire.lock() {
            let _ = wire.send(message);
        }
    }

    fn touch(&self) {
        self.last_seen_ms.store(now_ms() as u64, Ordering::Relaxed);
    }

    fn idle_for(&self) -> Duration {
        let last = self.last_seen_ms.load(Ordering::Relaxed);
        Duration::from_millis((now_ms() as u64).saturating_sub(last))
    }

    /// 把这条连接踢掉，并先告诉对面为什么。阻塞在 read 上的线程会立刻返回 0
    /// 然后自己收拾。
    ///
    /// **原因必须先发出去再断。** 客户端靠它区分「连接没了」（该自动重连）和
    /// 「连接被拿走了」（不该重连）—— 什么都不说就断，客户端只能当成网络问题
    /// 去重连，顶号的两端就会互相踢个没完。
    fn kick(&self, reason: goodbye::Reason, detail: &str) {
        self.send(
            &Goodbye {
                reason: reason as i32,
                detail: detail.to_string(),
            }
            .into(),
        );
        self.close();
    }

    /// 同上，但标记成「超时」而不是「自己走的」。
    fn kick_idle(&self) {
        self.timed_out.store(true, Ordering::Relaxed);
        self.close();
    }

    fn close(&self) {
        if let Ok(wire) = self.wire.lock() {
            let _ = wire.sock.shutdown(Shutdown::Both);
        }
    }
}

/// 所有连接和状态的汇合点。
pub struct Hub {
    state: Mutex<Server>,
    peers: Mutex<HashMap<SessionId, Arc<Peer>>>,
    /// 存档。`None` = 不落盘（测试、或者存档打不开时只在内存里跑）。
    ///
    /// **锁的顺序永远是先 `state` 后 `store`**，见 [`Hub::mutate`]。
    store: Option<Mutex<Store>>,
    /// UDP 那一半。
    pub voice: VoiceRouter,
}

impl Hub {
    pub fn new(server: Server, voice_socket: UdpSocket) -> Self {
        Self {
            state: Mutex::new(server),
            peers: Mutex::new(HashMap::new()),
            store: None,
            voice: VoiceRouter::new(voice_socket),
        }
    }

    /// 带存档。`server` 应该是用同一份存档 `Server::restore` 出来的。
    pub fn with_store(server: Server, voice_socket: UdpSocket, store: Store) -> Self {
        Self {
            store: Some(Mutex::new(store)),
            ..Self::new(server, voice_socket)
        }
    }

    /// 改状态，并在**同一把锁里**把要落盘的部分写进存档。
    ///
    /// 写库不能挪到 [`Hub::dispatch`] 里做：那是在锁外面跑的，两个线程的
    /// 广播可能乱序。A 删了频道 X（子频道 Y 挪到根上），紧接着 C 删了 Y ——
    /// 要是 C 的那批先落盘，库里就是先删 Y、再被 A 那批「Y 挪到根上」写回来，
    /// 一个删掉的频道重启之后又冒出来。在锁里写，落盘顺序就是改状态的顺序。
    fn mutate(&self, f: impl FnOnce(&mut Server) -> Vec<Broadcast>) -> Vec<Broadcast> {
        let mut state = self.state.lock().expect("state poisoned");
        let events = f(&mut state);
        self.persist(&mut state);
        events
    }

    /// 把状态机记下的变化写进存档。调用方持着 `state` 锁（就是传进来的这个）。
    /// 见 [`Hub::mutate`]。
    fn persist(&self, state: &mut Server) {
        let changes = state.take_changes();
        let Some(store) = &self.store else { return };
        if let Err(e) = store.lock().expect("store poisoned").record(&changes) {
            // 不因为这个把服务停掉：改动在内存里照样生效，只是重启会丢。
            // 在线的人比存档要紧。
            eprintln!("存档写失败，这次的改动重启后会丢：{e}");
        }
    }

    pub fn user_count(&self) -> usize {
        self.state.lock().expect("state poisoned").user_count()
    }

    /// 把一批广播真正发出去。
    ///
    /// **先取快照再发**：持着 state 锁去写 socket 的话，一个卡住的客户端
    /// 就能把整个服务端的状态锁住。
    fn dispatch(&self, events: Vec<Broadcast>) {
        if events.is_empty() {
            return;
        }
        let peers: Vec<Arc<Peer>> = {
            let peers = self.peers.lock().expect("peers poisoned");
            peers.values().cloned().collect()
        };

        for event in events {
            match event {
                Broadcast::Everyone(msg) => {
                    for peer in &peers {
                        peer.send(&msg);
                    }
                }
                Broadcast::Others(except, msg) => {
                    for peer in peers.iter().filter(|p| p.session != except) {
                        peer.send(&msg);
                    }
                }
                Broadcast::Channel(channel, msg) => {
                    let targets = {
                        let state = self.state.lock().expect("state poisoned");
                        state.sessions_in_channel(channel)
                    };
                    for peer in &peers {
                        if targets.contains(&peer.session) {
                            peer.send(&msg);
                        }
                    }
                }
                Broadcast::One(session, msg) => {
                    if let Some(peer) = peers.iter().find(|p| p.session == session) {
                        peer.send(&msg);
                    }
                }
                Broadcast::Kick {
                    session,
                    reason,
                    detail,
                } => {
                    // 先摘语音：不然在他的连接线程收尾之前，他还能往频道里灌几帧声音。
                    self.voice.unregister(session);
                    if let Some(peer) = peers.iter().find(|p| p.session == session) {
                        peer.kick(reason, &detail);
                    }
                }
            }
        }
    }

    /// 语音转发循环。**这个函数会阻塞**，在自己的线程上跑。
    ///
    /// # 这个循环绝不能退出
    ///
    /// 它一退，全服务器的语音就哑了，而控制面还好好的 —— 用户看到的是
    /// 「大家都在线，但谁也听不见谁」，是最难报的那种故障。所以除了
    /// socket 本身被关掉，任何错误都只是丢掉这一个包继续。
    ///
    /// Windows 上尤其重要：给一个已经关掉的客户端端口发包，对方的 ICMP
    /// port unreachable 会让**下一次 `recv_from`** 报 WSAECONNRESET(10054)，
    /// 哪怕出问题的是发送、哪怕接收队列里的包好好的。照着报错退出循环，
    /// 结果就是「有人退出游戏，全频道哑了」。
    pub fn run_voice(&self) {
        // 比 MAX_DATAGRAM 大一点，这样超长的包收得到、也认得出来。
        let mut buf = [0u8; 2048];
        loop {
            let (n, from) = match self.voice.socket().recv_from(&mut buf) {
                Ok(got) => got,
                Err(e) if fatal_socket_error(&e) => {
                    eprintln!("语音 socket 关了：{e}");
                    return;
                }
                Err(_) => continue,
            };
            if n > protocol::MAX_DATAGRAM {
                continue;
            }
            let forward = match self.voice.accept(from, &buf[..n]) {
                Some(Incoming::Voice(forward)) => forward,
                Some(Incoming::Keepalive(session, header)) => {
                    self.voice.reply_keepalive(session, header);
                    continue;
                }
                None => continue,
            };

            // 只查一次状态就放开锁 —— 后面封包和发包都不持锁。
            let targets = {
                let state = self.state.lock().expect("state poisoned");
                match state.channel_of(forward.from) {
                    Some(channel) => state.sessions_in_channel(channel),
                    None => Vec::new(),
                }
            };
            if targets.len() < 2 {
                // 频道里只有他自己。不用发给任何人。
                continue;
            }
            self.voice.deliver(&forward, &targets);
        }
    }

    /// 看门狗：踢掉太久没动静的连接。
    pub fn sweep_idle(&self) {
        let stale: Vec<Arc<Peer>> = {
            let peers = self.peers.lock().expect("peers poisoned");
            peers
                .values()
                .filter(|p| p.idle_for() > IDLE_TIMEOUT)
                .cloned()
                .collect()
        };
        for peer in stale {
            peer.kick_idle();
        }
    }
}

/// 处理一条连上来的 TCP 连接，直到它断开。**这个函数会阻塞。**
pub fn serve_connection(
    sock: TcpStream,
    tls_config: Arc<rustls::ServerConfig>,
    hub: Arc<Hub>,
) -> io::Result<()> {
    sock.set_nodelay(true)?;
    let read_sock = sock.try_clone()?;

    let mut conn = rustls::ServerConnection::new(tls_config).map_err(io::Error::other)?;
    // 握手在这里一次做完，之后才拆成读写两半。
    let mut handshake_sock = sock.try_clone()?;
    conn.complete_io(&mut handshake_sock)?;

    // 语音密钥在这里就派生好。不另起一次握手 —— 这条 TLS 连接已经认证过了，
    // RFC 5705 的 exporter 保证两端算出来一模一样。上下行分开，见
    // transport::derive_voice_key 的文档。
    let upstream = transport::derive_voice_key(&conn, transport::UPSTREAM)
        .map_err(|e| io::Error::other(format!("派生上行语音密钥失败: {e}")))?;
    let downstream = transport::derive_voice_key(&conn, transport::DOWNSTREAM)
        .map_err(|e| io::Error::other(format!("派生下行语音密钥失败: {e}")))?;

    let wire = Arc::new(Mutex::new(Wire { conn, sock }));
    let mut reader = Reader::new(read_sock, Arc::clone(&wire));

    // ---- 认证 ----
    let deadline = Instant::now() + HANDSHAKE_TIMEOUT;
    let keys = VoiceKeys {
        upstream,
        downstream,
    };
    // 走到 `Some` 时人已经在状态里、也已经在 `hub.peers` 里了 ——
    // 两件事是在同一把 state 锁里做的，见 authenticate 末尾。
    let peer = match authenticate(&mut reader, &wire, &hub, &keys, deadline) {
        Ok(Some(peer)) => peer,
        // 被拒或者对面走了：Rejected 已经发过了，这里干净收场。
        Ok(None) => return Ok(()),
        Err(e) => return Err(e),
    };
    let session = peer.session;

    // ---- 消息循环 ----
    let result = message_loop(&mut reader, &wire, &hub, &peer);

    // ---- 收尾。不管怎么出来的，都要把人从状态里摘掉并广播 ----
    hub.peers.lock().expect("peers poisoned").remove(&session);
    // **先摘语音再改状态**：留着的话，一个刚被踢掉的人还能继续往频道里灌声音。
    hub.voice.unregister(session);
    let events = hub.mutate(|state| {
        if peer.timed_out.load(Ordering::Relaxed) {
            state.timeout(session)
        } else {
            state.disconnect(session)
        }
    });
    hub.dispatch(events);
    result
}

/// 这条连接派生出来的两把语音密钥。
struct VoiceKeys {
    upstream: transport::VoiceKey,
    downstream: transport::VoiceKey,
}

/// Hello -> Challenge -> Authenticate -> Welcome / Rejected。
///
/// 返回 `Ok(None)` 表示「正常地没让他进来」（版本不对、签名不对、策略不让）——
/// 该发的 `Rejected` 已经发出去了，调用方安静收场就行。
///
/// 返回 `Some` 时这个人已经登记进 `hub.peers`，调用方负责在断开时把他摘掉。
fn authenticate(
    reader: &mut Reader,
    wire: &Arc<Mutex<Wire>>,
    hub: &Arc<Hub>,
    keys: &VoiceKeys,
    deadline: Instant,
) -> io::Result<Option<Arc<Peer>>> {
    use protocol::control::rejected::Reason;

    let Some(hello) = reader.next_message(deadline)? else {
        return Ok(None);
    };
    let Some(client_message::Payload::Hello(hello)) = hello.payload else {
        // 第一条不是 Hello：要么是实现有问题，要么是在乱探。不解释，直接走。
        return Ok(None);
    };

    if hello.protocol_version != PROTOCOL_VERSION {
        reject(
            wire,
            Reason::VersionMismatch,
            format!(
                "协议版本对不上：服务端是 {PROTOCOL_VERSION}，你的客户端是 {}。升级一下",
                hello.protocol_version
            ),
        );
        return Ok(None);
    }

    let Ok(key_bytes): Result<[u8; 32], _> = hello.public_key.try_into() else {
        reject(wire, Reason::BadSignature, "公钥长度不对".into());
        return Ok(None);
    };
    let public_key = PublicKey(key_bytes);

    // 随机数必须由服务端出。让客户端自己选要签的内容等于允许重放：
    // 抓一次签名就能无限次冒充。
    let mut nonce = [0u8; CHALLENGE_LEN];
    getrandom::fill(&mut nonce).map_err(|e| io::Error::other(format!("拿不到随机数: {e}")))?;
    {
        let mut w = wire.lock().expect("wire poisoned");
        w.send(
            &Challenge {
                nonce: nonce.to_vec(),
            }
            .into(),
        )?;
    }

    let Some(auth) = reader.next_message(deadline)? else {
        return Ok(None);
    };
    let Some(client_message::Payload::Authenticate(auth)) = auth.payload else {
        return Ok(None);
    };

    let Ok(signature): Result<[u8; 64], _> = auth.signature.try_into() else {
        reject(wire, Reason::BadSignature, "签名长度不对".into());
        return Ok(None);
    };
    if !Identity::verify(&public_key, &nonce, &signature) {
        reject(
            wire,
            Reason::BadSignature,
            "签名验不过 —— 公钥和私钥对不上".into(),
        );
        return Ok(None);
    }

    // 到这里才轮到策略。密码学归密码学，策略归 state。
    // 从这里到 Welcome 写出去，**一直攥着这条连接的写锁**。
    //
    // 进状态和进 `hub.peers` 必须在同一把 state 锁里做完：Welcome 是 admit
    // 那一刻的快照，之后别人改了什么（建频道、换频道、说话）都只靠广播送达，
    // 而广播按 `hub.peers` 发。两步中间要是有空隙，空隙里的广播就漏给了新人，
    // 他的名单从此永远缺那一块 —— 集成测试在慢 CI 上偶发等不到别人建的频道，
    // 就是这个。
    //
    // 可一进 `hub.peers`，别的线程就可能往这条连接发广播了，而 Welcome
    // 必须是第一条。所以先攥住写锁：别人的 `Peer::send` 会在锁上排队，
    // 等 Welcome 写完才轮到。锁的顺序是 这条连接的 wire → state → peers，
    // 没有别的地方会攥着 state 或 peers 去等某条连接的 wire，不会死锁。
    //
    // 排队进来的广播里可能有 admit 之前就改掉、已经算进快照的东西，
    // 新人会重复收到一次。那些都是「按 id 覆盖 / 删除」的消息，重复收无害。
    let mut w = wire.lock().expect("wire poisoned");
    let admitted = {
        let mut state = hub.state.lock().expect("state poisoned");
        let admitted = state.admit(public_key, &auth.invite_code, &auth.desired_name);
        // 用管理员链接进来的，「他是管理员」和「链接作废」都要落盘。
        hub.persist(&mut state);
        admitted.map(|admitted| {
            let peer = Arc::new(Peer {
                session: admitted.session_id,
                wire: Arc::clone(wire),
                last_seen_ms: AtomicU64::new(now_ms() as u64),
                timed_out: AtomicBool::new(false),
            });
            let mut peers = hub.peers.lock().expect("peers poisoned");
            // 顶号：旧会话在状态里已经摘干净了，这里一起从 peers 里摘掉。
            let displaced = admitted.displaced.and_then(|old| peers.remove(&old));
            peers.insert(admitted.session_id, Arc::clone(&peer));
            (admitted, peer, displaced)
        })
    };
    let (admitted, peer, displaced) = match admitted {
        Ok(a) => a,
        Err(denied) => {
            let _ = w.send(&denied.to_wire().into());
            return Ok(None);
        }
    };

    // 顶号：把旧连接踢掉。攥着的是新连接的写锁，踢的是旧连接的，不冲突。
    if let Some(old_peer) = displaced {
        old_peer.kick(
            goodbye::Reason::Displaced,
            "同一个身份从别处连进了这个服务器，这边被顶下去了",
        );
    }

    // **在 Welcome 发出去之前挂上密钥**：客户端一收到 Welcome 就会开始发语音，
    // 晚一步注册，开头那几个包就全被当成「不认识的会话」丢了。
    hub.voice.register(
        admitted.session_id,
        keys.upstream.as_bytes(),
        keys.downstream.as_bytes(),
    );

    let mut welcome = admitted.welcome;
    welcome.udp_port = hub.voice.local_port() as u32;
    // 发不出去也不在这里返回错误：人已经进了状态和 peers，得走调用方的
    // 收尾把他摘掉。接下来的 read 会立刻发现连接断了。
    let _ = w.send(&welcome.into());
    // **先放写锁再广播**：广播里有发给他自己的那条，std 的 Mutex 不可重入。
    drop(w);
    hub.dispatch(admitted.broadcasts);
    Ok(Some(peer))
}

fn reject(wire: &Arc<Mutex<Wire>>, reason: protocol::control::rejected::Reason, detail: String) {
    if let Ok(mut w) = wire.lock() {
        let _ = w.send(
            &Rejected {
                reason: reason as i32,
                detail,
            }
            .into(),
        );
    }
}

fn message_loop(
    reader: &mut Reader,
    wire: &Arc<Mutex<Wire>>,
    hub: &Arc<Hub>,
    peer: &Arc<Peer>,
) -> io::Result<()> {
    use protocol::control::Pong;

    loop {
        // 登录之后不再有截止时间 —— 该不该踢由看门狗按空闲时长决定。
        let Some(message) = reader.next_message(Instant::now() + Duration::from_secs(86_400))?
        else {
            return Ok(());
        };
        peer.touch();

        let events = match message.payload {
            Some(client_message::Payload::Ping(ping)) => {
                let mut w = wire.lock().expect("wire poisoned");
                w.send(
                    &Pong {
                        timestamp: ping.timestamp,
                        // 客户端靠这个判断 UDP 到底通没通 —— 一直是 0
                        // 就说明该退回 TCP 传语音了。
                        udp_packets_received: hub.voice.packets_received(peer.session),
                    }
                    .into(),
                )?;
                Vec::new()
            }
            Some(client_message::Payload::JoinChannel(join)) => {
                hub.mutate(|state| state.join_channel(peer.session, join.channel_id))
            }
            Some(client_message::Payload::CreateChannel(req)) => {
                hub.mutate(|state| state.create_channel(peer.session, req))
            }
            Some(client_message::Payload::DeleteChannel(req)) => {
                hub.mutate(|state| state.delete_channel(peer.session, req.channel_id))
            }
            Some(client_message::Payload::SelfState(s)) => hub
                .mutate(|state| state.set_self_state(peer.session, s.self_muted, s.self_deafened)),
            Some(client_message::Payload::TextMessage(text)) => {
                hub.mutate(|state| state.text_message(peer.session, text, now_ms()))
            }
            Some(client_message::Payload::EditChannel(req)) => {
                hub.mutate(|state| state.edit_channel(peer.session, req))
            }
            Some(client_message::Payload::KickUser(req)) => {
                hub.mutate(|state| state.kick(peer.session, req.session_id, &req.reason))
            }
            Some(client_message::Payload::BanUser(req)) => {
                hub.mutate(|state| state.ban(peer.session, req.session_id, &req.reason, now_ms()))
            }
            Some(client_message::Payload::Unban(req)) => {
                hub.mutate(|state| state.unban(peer.session, &req.public_key))
            }
            Some(client_message::Payload::SetRole(req)) => {
                let role = Role::try_from(req.role).unwrap_or(Role::Unspecified);
                hub.mutate(|state| state.set_role(peer.session, req.session_id, role))
            }
            // 登录之后再发 Hello / Authenticate 是协议错误，忽略。
            Some(_) => Vec::new(),
            // 认不出来的分支：新客户端发了我们不懂的东西。**忽略，不要断开** ——
            // 这正是 protobuf 演进语义要的行为。
            None => Vec::new(),
        };
        hub.dispatch(events);
    }
}

/// 从 TLS 流里一条一条读控制消息。
///
/// 自己管一个缓冲区：TCP 会在任意位置切断，一次 read 可能拿到半条、也可能拿到两条半。
struct Reader {
    /// 只用来读的 socket 句柄。阻塞读的时候**不持锁** ——
    /// 否则一条安静的连接会把广播路径堵死。
    sock: TcpStream,
    /// TLS 状态。读到密文之后才短暂上锁，解出明文就放开。
    wire: Arc<Mutex<Wire>>,
    /// 已经解密但还没解析成消息的明文。
    buffered: Vec<u8>,
}

impl Reader {
    fn new(sock: TcpStream, wire: Arc<Mutex<Wire>>) -> Self {
        Self {
            sock,
            wire,
            buffered: Vec::with_capacity(4096),
        }
    }

    /// 取下一条消息。返回 `Ok(None)` 表示对面关了。
    fn next_message(&mut self, deadline: Instant) -> io::Result<Option<ClientMessage>> {
        loop {
            match decode_frame::<ClientMessage>(&self.buffered) {
                Ok(Some((message, used))) => {
                    self.buffered.drain(..used);
                    return Ok(Some(message));
                }
                Ok(None) => {}
                Err(e) => return Err(io::Error::other(e)),
            }
            // 先看一眼对面声称有多长，超限的当场断 —— 不要等它慢慢发完。
            if let Err(e) = peek_frame_len(&self.buffered) {
                return Err(io::Error::other(e));
            }
            if Instant::now() > deadline {
                return Err(io::Error::new(
                    io::ErrorKind::TimedOut,
                    "握手超时：连上来之后一直不说话",
                ));
            }
            if !self.fill()? {
                return Ok(None);
            }
        }
    }

    /// 读一批密文，解密，追加到明文缓冲区。返回 false 表示对面关了。
    fn fill(&mut self) -> io::Result<bool> {
        // 先看 TLS 层有没有攒着还没取走的明文。握手那一步（`complete_io`）
        // 有可能顺手把对端紧跟着发来的应用数据一起读进去了 —— 不先取干净的话
        // 就会卡在「等 socket 再来点东西」，而对端正在等我们回话。
        if self.drain_plaintext()? > 0 {
            return Ok(true);
        }

        let mut chunk = [0u8; 8192];
        let n = self.sock.read(&mut chunk)?;
        if n == 0 {
            return Ok(false);
        }

        let mut rest = &chunk[..n];
        while !rest.is_empty() {
            let consumed = {
                let mut wire = self.wire.lock().expect("wire poisoned");
                let consumed = wire.conn.read_tls(&mut rest)?;
                wire.conn.process_new_packets().map_err(io::Error::other)?;
                // 握手期间对端可能还要我们回点东西（比如 TLS 1.3 的 key update）。
                if wire.conn.wants_write() {
                    wire.flush_tls()?;
                }
                consumed
            };
            if consumed == 0 {
                // read_tls 一个字节都吃不下，而我们刚刚才把明文取空 ——
                // 再转下去就是死循环。
                return Err(io::Error::other("TLS 层卡住了：既不收字节也不出明文"));
            }
            self.drain_plaintext()?;
        }
        Ok(true)
    }

    /// 把 TLS 已经解好的明文搬进 `buffered`，返回搬了多少字节。
    fn drain_plaintext(&mut self) -> io::Result<usize> {
        let mut wire = self.wire.lock().expect("wire poisoned");
        let available = wire.conn.process_new_packets().map_err(io::Error::other)?;
        let pending = available.plaintext_bytes_to_read();
        if pending == 0 {
            return Ok(0);
        }
        // 缓冲区不能无限涨。分帧本身有上限，但攒着好几条也可能很大。
        if self.buffered.len() + pending > MAX_FRAME_BODY * 2 {
            return Err(io::Error::other("对端积压了太多没解析的控制消息"));
        }
        let start = self.buffered.len();
        self.buffered.resize(start + pending, 0);
        wire.conn.reader().read_exact(&mut self.buffered[start..])?;
        Ok(pending)
    }
}

/// 这个 socket 错误是不是「没救了」。
///
/// 只有 socket 本身没了才算。**其他一概不算** —— 见 [`Hub::run_voice`]。
fn fatal_socket_error(e: &io::Error) -> bool {
    matches!(
        e.kind(),
        io::ErrorKind::NotConnected | io::ErrorKind::BrokenPipe
    ) || e.raw_os_error() == Some(WSAENOTSOCK)
}

/// Winsock 的「这个句柄不是 socket」。socket 被关掉之后 `recv_from` 报这个。
const WSAENOTSOCK: i32 = 10038;

pub fn now_ms() -> i64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_millis() as i64)
        .unwrap_or(0)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn timeouts_are_sane() {
        assert!(
            HANDSHAKE_TIMEOUT < IDLE_TIMEOUT,
            "握手超时该比空闲超时短：一条连上来不说话的连接不该白占 30 秒"
        );
        assert!(IDLE_TIMEOUT.as_secs() >= 15, "太短会把网络抖动误判成掉线");
    }

    #[test]
    fn now_ms_looks_like_a_unix_timestamp() {
        let now = now_ms();
        // 2020-01-01 之后、2100 之前
        assert!(now > 1_577_836_800_000);
        assert!(now < 4_102_444_800_000);
    }
}
