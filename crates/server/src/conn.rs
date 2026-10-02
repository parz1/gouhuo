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
//! 控制面用非阻塞 socket 等待系统可读/可写事件，不使用 SO_RCVTIMEO。
//! 总认证截止时间覆盖 TLS 和应用握手；已登录连接由看门狗置关闭标志。
//! 克隆的 Winsock 句柄 shutdown 不一定唤醒已挂起的读，因此每次等待至多 100 ms。
//! 见 control_io；语音 UDP 的收包节奏没有改变。

use std::collections::HashMap;
use std::io::{self, Read, Write};
use std::net::{TcpStream, UdpSocket};
use std::sync::atomic::{AtomicBool, AtomicU64, AtomicUsize, Ordering};
use std::sync::mpsc::{sync_channel, SyncSender};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};

use protocol::control::{
    client_message, decode_frame, encode_frame, goodbye, peek_frame_len, Challenge, ClientMessage,
    Goodbye, Rejected, Role, ServerMessage, MAX_FRAME_BODY, PROTOCOL_VERSION,
};
use protocol::PublicKey;
use voice_core::identity::Identity;

use crate::control_io::{IoControl, SocketIo};
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

/// 未认证连接的上限，在 accept 启动线程之前占位。
pub const MAX_PENDING_CONNECTIONS: usize = 32;
const OUTBOUND_CAPACITY: usize = 64;

/// 从 TCP accept 到认证完成的总截止时间；shutdown 不需要 TLS 锁。
pub struct Admission {
    hub: Arc<Hub>,
    deadline: Instant,
}

impl Drop for Admission {
    fn drop(&mut self) {
        self.hub.pending.fetch_sub(1, Ordering::Relaxed);
    }
}

enum Outbound {
    Message(ServerMessage),
    Goodbye(ServerMessage),
}

/// 每连接一个有界队列，网络写入只在该连接的 writer 上发生。
fn spawn_writer(
    wire: Arc<Mutex<Wire>>,
    control: Arc<IoControl>,
) -> io::Result<SyncSender<Outbound>> {
    let (tx, rx) = sync_channel(OUTBOUND_CAPACITY);
    std::thread::Builder::new()
        .name("gouhuo-control-write".into())
        .spawn(move || {
            while let Ok(item) = rx.recv() {
                let (message, close) = match item {
                    Outbound::Message(message) => (message, false),
                    Outbound::Goodbye(message) => (message, true),
                };
                let sent = wire.lock().expect("wire poisoned").send(&message);
                if sent.is_err() || close {
                    break;
                }
            }
            control.close();
        })?;
    Ok(tx)
}

/// 挑战随机数的长度。
const CHALLENGE_LEN: usize = 32;

/// 一条连接的 TLS 状态和写用的 socket。
struct Wire {
    conn: rustls::ServerConnection,
    sock: SocketIo,
}

impl Wire {
    /// 把 rustls 攒着的字节真正写出去。
    fn flush_tls(&mut self) -> io::Result<()> {
        self.sock.begin_write();
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
    outbound: SyncSender<Outbound>,
    shutdown: Arc<IoControl>,
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
        self.enqueue(Outbound::Message(message.clone()));
    }

    fn enqueue(&self, item: Outbound) {
        // 满了就断开；不等待慢客户端，不增加无界积压。
        if self.outbound.try_send(item).is_err() {
            self.close();
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
        self.enqueue(Outbound::Goodbye(
            Goodbye {
                reason: reason as i32,
                detail: detail.to_string(),
            }
            .into(),
        ));
    }

    /// 同上，但标记成「超时」而不是「自己走的」。
    fn kick_idle(&self) {
        self.timed_out.store(true, Ordering::Relaxed);
        self.close();
    }

    fn close(&self) {
        self.shutdown.close();
    }
}

/// 所有连接和状态的汇合点。
pub struct Hub {
    /// 只串行化状态提交、快照注册和非阻塞入队，不执行 socket 写入。
    commits: Mutex<()>,
    pending: AtomicUsize,
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
            commits: Mutex::new(()),
            pending: AtomicUsize::new(0),
            state: Mutex::new(server),
            peers: Mutex::new(HashMap::new()),
            store: None,
            voice: VoiceRouter::new(voice_socket),
        }
    }

    // 新 Rust 将 fetch_update 改名为 try_update；保留旧名以兼容 Rust 1.80。
    #[allow(deprecated)]
    pub fn reserve_connection(self: &Arc<Self>) -> Option<Admission> {
        self.pending
            .fetch_update(Ordering::Relaxed, Ordering::Relaxed, |n| {
                (n < MAX_PENDING_CONNECTIONS).then_some(n + 1)
            })
            .ok()?;
        Some(Admission {
            hub: Arc::clone(self),
            deadline: Instant::now() + HANDSHAKE_TIMEOUT,
        })
    }

    /// 带存档。`server` 应该是用同一份存档 `Server::restore` 出来的。
    pub fn with_store(server: Server, voice_socket: UdpSocket, store: Store) -> Self {
        Self {
            store: Some(Mutex::new(store)),
            ..Self::new(server, voice_socket)
        }
    }

    /// 提交入场、给旧成员广播、注册新订阅者是同一次提交。
    fn admit_peer(
        &self,
        public_key: PublicKey,
        invite_code: &str,
        desired_name: &str,
        keys: (&[u8; 32], &[u8; 32]),
        outbound: SyncSender<Outbound>,
        shutdown: Arc<IoControl>,
    ) -> Result<Arc<Peer>, crate::state::Denied> {
        let _commit = self.commits.lock().expect("commits poisoned");
        let admitted = {
            let mut state = self.state.lock().expect("state poisoned");
            let admitted = state.admit(public_key, invite_code, desired_name);
            self.persist(&mut state);
            admitted
        };
        let admitted = admitted?;
        let peer = Arc::new(Peer {
            session: admitted.session_id,
            outbound,
            shutdown,
            last_seen_ms: AtomicU64::new(now_ms() as u64),
            timed_out: AtomicBool::new(false),
        });

        // 顶号：把旧连接踢掉。状态里已经摘干净了，这里只管关 socket。
        if let Some(old) = admitted.displaced {
            let old_peer = self.peers.lock().expect("peers poisoned").remove(&old);
            if let Some(old_peer) = old_peer {
                old_peer.kick(
                    goodbye::Reason::Displaced,
                    "同一个身份从别处连进了这个服务器，这边被顶下去了",
                );
            }
        }

        // **在 Welcome 发出去之前挂上密钥**：客户端一收到 Welcome 就会开始发语音，
        // 晚一步注册，开头那几个包就全被当成「不认识的会话」丢了。
        self.voice.register(admitted.session_id, keys.0, keys.1);

        let mut welcome = admitted.welcome;
        welcome.udp_port = self.voice.local_port() as u32;
        // 快照先入队，再注册订阅；提交锁内不允许其他变更插入这两步之间。
        peer.send(&welcome.into());
        // 入场事件只通知现有订阅者；新人的 Welcome 已包含这批变化。
        self.dispatch(admitted.broadcasts);
        self.peers
            .lock()
            .expect("peers poisoned")
            .insert(peer.session, Arc::clone(&peer));
        Ok(peer)
    }

    /// 改状态，并在**同一把锁里**把要落盘的部分写进存档。
    ///
    /// 写库不能挪到 [`Hub::dispatch`] 里做：那是在锁外面跑的，两个线程的
    /// 广播可能乱序。A 删了频道 X（子频道 Y 挪到根上），紧接着 C 删了 Y ——
    /// 要是 C 的那批先落盘，库里就是先删 Y、再被 A 那批「Y 挪到根上」写回来，
    /// 一个删掉的频道重启之后又冒出来。在锁里写，落盘顺序就是改状态的顺序。
    fn mutate(&self, f: impl FnOnce(&mut Server) -> Vec<Broadcast>) {
        let _commit = self.commits.lock().expect("commits poisoned");
        let mut state = self.state.lock().expect("state poisoned");
        let events = f(&mut state);
        self.persist(&mut state);
        drop(state);
        self.dispatch(events);
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
    let admission = hub
        .reserve_connection()
        .ok_or_else(|| io::Error::other("未认证连接已满"))?;
    serve_admitted(sock, tls_config, hub, admission)
}

pub fn serve_admitted(
    sock: TcpStream,
    tls_config: Arc<rustls::ServerConfig>,
    hub: Arc<Hub>,
    admission: Admission,
) -> io::Result<()> {
    let deadline = admission.deadline;
    sock.set_nodelay(true)?;
    let control = Arc::new(IoControl::new(sock.try_clone()?, Some(deadline)));
    let sock = SocketIo::new(sock, Arc::clone(&control))?;
    let read_sock = sock.try_clone()?;

    let mut conn = rustls::ServerConnection::new(tls_config).map_err(io::Error::other)?;
    // 握手在这里一次做完，之后才拆成读写两半。
    let mut handshake_sock = sock.try_clone()?;
    conn.complete_io(&mut handshake_sock)?;
    drop(handshake_sock);

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
    let keys = VoiceKeys {
        upstream,
        downstream,
    };
    let peer = match authenticate(&mut reader, &wire, &hub, &keys, deadline) {
        Ok(Some(peer)) => peer,
        // 被拒或者对面走了：Rejected 已经发过了，这里干净收场。
        Ok(None) => return Ok(()),
        Err(e) => return Err(e),
    };

    control.authenticated();
    drop(admission);
    let session = peer.session;

    // ---- 消息循环 ----
    let result = message_loop(&mut reader, &hub, &peer);

    // ---- 收尾。不管怎么出来的，都要把人从状态里摘掉并广播 ----
    hub.peers.lock().expect("peers poisoned").remove(&session);
    // **先摘语音再改状态**：留着的话，一个刚被踢掉的人还能继续往频道里灌声音。
    hub.voice.unregister(session);
    hub.mutate(|state| {
        if peer.timed_out.load(Ordering::Relaxed) {
            state.timeout(session)
        } else {
            state.disconnect(session)
        }
    });
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

    // Writer 在状态提交前启动，线程创建失败不会留下已登录的幽灵成员。
    let shutdown = Arc::clone(&wire.lock().expect("wire poisoned").sock.control);
    let outbound = spawn_writer(Arc::clone(wire), Arc::clone(&shutdown))?;
    match hub.admit_peer(
        public_key,
        &auth.invite_code,
        &auth.desired_name,
        (keys.upstream.as_bytes(), keys.downstream.as_bytes()),
        outbound,
        shutdown,
    ) {
        Ok(peer) => Ok(Some(peer)),
        Err(denied) => {
            wire.lock()
                .expect("wire poisoned")
                .send(&denied.to_wire().into())?;
            Ok(None)
        }
    }
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

fn message_loop(reader: &mut Reader, hub: &Arc<Hub>, peer: &Arc<Peer>) -> io::Result<()> {
    use protocol::control::Pong;

    loop {
        // 登录之后不再有截止时间 —— 该不该踢由看门狗按空闲时长决定。
        let Some(message) = reader.next_message(Instant::now() + Duration::from_secs(86_400))?
        else {
            return Ok(());
        };
        peer.touch();

        match message.payload {
            Some(client_message::Payload::Ping(ping)) => {
                peer.send(
                    &Pong {
                        timestamp: ping.timestamp,
                        // 客户端靠这个判断 UDP 到底通没通 —— 一直是 0
                        // 就说明该退回 TCP 传语音了。
                        udp_packets_received: hub.voice.packets_received(peer.session),
                    }
                    .into(),
                );
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
            Some(_) => (),
            // 认不出来的分支：新客户端发了我们不懂的东西。**忽略，不要断开** ——
            // 这正是 protobuf 演进语义要的行为。
            None => (),
        };
    }
}

/// 从 TLS 流里一条一条读控制消息。
///
/// 自己管一个缓冲区：TCP 会在任意位置切断，一次 read 可能拿到半条、也可能拿到两条半。
struct Reader {
    /// 只用来读的 socket 句柄。阻塞读的时候**不持锁** ——
    /// 否则一条安静的连接会把广播路径堵死。
    sock: SocketIo,
    /// TLS 状态。读到密文之后才短暂上锁，解出明文就放开。
    wire: Arc<Mutex<Wire>>,
    /// 已经解密但还没解析成消息的明文。
    buffered: Vec<u8>,
}

impl Reader {
    fn new(sock: SocketIo, wire: Arc<Mutex<Wire>>) -> Self {
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

#[cfg(test)]
mod regressions {
    use super::*;
    use crate::state::Config;
    use protocol::control::{server_message, CreateChannel, EditChannel, Hello, Pong};
    use std::net::TcpListener;
    use std::sync::mpsc::{channel, Receiver};

    fn sockets() -> (TcpStream, TcpStream) {
        let listener = TcpListener::bind("127.0.0.1:0").unwrap();
        let client = TcpStream::connect(listener.local_addr().unwrap()).unwrap();
        let (server, _) = listener.accept().unwrap();
        (server, client)
    }

    fn hub() -> Arc<Hub> {
        Arc::new(Hub::new(
            Server::new(Config::default()),
            UdpSocket::bind("127.0.0.1:0").unwrap(),
        ))
    }

    fn queued_peer(session: u32) -> (Arc<Peer>, Receiver<Outbound>, TcpStream) {
        let (shutdown, client) = sockets();
        let (outbound, rx) = sync_channel(OUTBOUND_CAPACITY);
        (
            Arc::new(Peer {
                session,
                outbound,
                shutdown: Arc::new(IoControl::new(shutdown, None)),
                last_seen_ms: AtomicU64::new(now_ms() as u64),
                timed_out: AtomicBool::new(false),
            }),
            rx,
            client,
        )
    }

    #[test]
    fn pending_admission_is_bounded_and_released() {
        let hub = hub();
        let permits: Vec<_> = (0..MAX_PENDING_CONNECTIONS)
            .map(|_| hub.reserve_connection().unwrap())
            .collect();
        assert!(hub.reserve_connection().is_none());
        drop(permits);
        assert_eq!(hub.pending.load(Ordering::Relaxed), 0);
        assert!(hub.reserve_connection().is_some());
    }

    // 四个静默位置都实际阻塞服务端 IO，deadline 通过 shutdown 唤醒。
    fn stalled_auth(stage: u8) {
        let hub = hub();
        let cert = transport::ServerCert::generate().unwrap();
        let tls = Arc::new(transport::server_config(&cert).unwrap());
        let (sock, mut client_sock) = sockets();
        client_sock
            .set_read_timeout(Some(Duration::from_secs(3)))
            .unwrap();
        let mut admission = hub.reserve_connection().unwrap();
        admission.deadline = Instant::now() + Duration::from_millis(600);
        let server_hub = Arc::clone(&hub);
        let (done, finished) = channel();
        let thread = std::thread::spawn(move || {
            let result = serve_admitted(sock, tls, server_hub, admission);
            done.send(result.is_ok()).unwrap();
        });
        let config = Arc::new(transport::client_config(cert.fingerprint()).unwrap());
        let mut conn = rustls::ClientConnection::new(
            config,
            rustls::pki_types::ServerName::try_from("gouhuo").unwrap(),
        )
        .unwrap();
        if stage == 1 {
            let mut hello = Vec::new();
            conn.write_tls(&mut hello).unwrap();
            client_sock.write_all(&hello[..5]).unwrap();
        }
        if stage >= 2 {
            conn.complete_io(&mut client_sock).unwrap();
            if stage == 3 {
                let identity = Identity::generate().unwrap();
                let mut frame = Vec::new();
                encode_frame(
                    &ClientMessage::from(Hello {
                        protocol_version: PROTOCOL_VERSION,
                        client_version: "test".into(),
                        public_key: identity.public_key().0.to_vec(),
                    }),
                    &mut frame,
                )
                .unwrap();
                conn.writer().write_all(&frame).unwrap();
                conn.complete_io(&mut client_sock).unwrap();
                let mut stream = rustls::StreamOwned::new(conn, client_sock.try_clone().unwrap());
                let mut received = Vec::new();
                let mut buf = [0; 4096];
                loop {
                    let n = stream.read(&mut buf).unwrap();
                    assert_ne!(n, 0);
                    received.extend_from_slice(&buf[..n]);
                    if let Some((message, _)) = decode_frame::<ServerMessage>(&received).unwrap() {
                        assert!(matches!(
                            message.payload,
                            Some(server_message::Payload::Challenge(_))
                        ));
                        break;
                    }
                }
                // 保持 stream 活着，不能靠客户端 Drop 冒充服务端超时。
                assert!(finished.recv_timeout(Duration::from_secs(3)).is_ok());
                thread.join().unwrap();
                assert_eq!(hub.pending.load(Ordering::Relaxed), 0);
                assert_eq!(hub.user_count(), 0);
                return;
            }
        }
        assert!(finished.recv_timeout(Duration::from_secs(3)).is_ok());
        thread.join().unwrap();
        assert_eq!(hub.pending.load(Ordering::Relaxed), 0);
        assert_eq!(hub.user_count(), 0);
    }

    #[test]
    fn silent_tcp_expires() {
        stalled_auth(0);
    }
    #[test]
    fn partial_tls_expires() {
        stalled_auth(1);
    }
    #[test]
    fn silent_after_tls_expires() {
        stalled_auth(2);
    }
    #[test]
    fn silent_after_challenge_expires() {
        stalled_auth(3);
    }

    #[test]
    fn stalled_writer_does_not_block_other_peers_or_close() {
        let hub = hub();
        let (slow, _stalled_writer, mut client) = queued_peer(1);
        let (fast, fast_rx, _fast_client) = queued_peer(2);
        hub.peers.lock().unwrap().insert(1, Arc::clone(&slow));
        hub.peers.lock().unwrap().insert(2, fast);
        let message: ServerMessage = Pong::default().into();
        for _ in 0..OUTBOUND_CAPACITY {
            slow.send(&message);
        }
        let (done, finished) = channel();
        let thread = std::thread::spawn(move || {
            hub.mutate(|_| vec![Broadcast::Everyone(message)]);
            slow.close();
            done.send(()).unwrap();
        });
        finished
            .recv_timeout(Duration::from_secs(1))
            .expect("广播或关闭被慢连接拖住");
        assert!(fast_rx.recv_timeout(Duration::from_secs(1)).is_ok());
        client
            .set_read_timeout(Some(Duration::from_secs(1)))
            .unwrap();
        assert!(matches!(client.read(&mut [0; 1]), Ok(0) | Err(_)));
        thread.join().unwrap();
    }

    #[test]
    fn concurrent_update_then_delete_is_queued_in_commit_order() {
        let hub = hub();
        let (session, channel_id) = {
            let mut state = hub.state.lock().unwrap();
            let admitted = state.admit(PublicKey([1; 32]), "", "owner").unwrap();
            let events = state.create_channel(
                admitted.session_id,
                CreateChannel {
                    name: "old".into(),
                    ..Default::default()
                },
            );
            let Broadcast::Everyone(message) = &events[0] else {
                panic!()
            };
            let Some(server_message::Payload::ChannelState(created)) = &message.payload else {
                panic!()
            };
            (admitted.session_id, created.channel.as_ref().unwrap().id)
        };
        let (peer, rx, _client) = queued_peer(session);
        hub.peers.lock().unwrap().insert(session, peer);
        let (entered, ready) = channel();
        let (release, wait) = channel();
        let first_hub = Arc::clone(&hub);
        let first = std::thread::spawn(move || {
            first_hub.mutate(|state| {
                let events = state.edit_channel(
                    session,
                    EditChannel {
                        channel_id,
                        name: "new".into(),
                        ..Default::default()
                    },
                );
                entered.send(()).unwrap();
                wait.recv().unwrap();
                events
            })
        });
        ready.recv_timeout(Duration::from_secs(1)).unwrap();
        let (attempted, attempting) = channel();
        let second_hub = Arc::clone(&hub);
        let second = std::thread::spawn(move || {
            attempted.send(()).unwrap();
            second_hub.mutate(|state| state.delete_channel(session, channel_id));
        });
        attempting.recv_timeout(Duration::from_secs(1)).unwrap();
        release.send(()).unwrap();
        first.join().unwrap();
        second.join().unwrap();
        let Outbound::Message(update) = rx.recv().unwrap() else {
            panic!()
        };
        let Outbound::Message(delete) = rx.recv().unwrap() else {
            panic!()
        };
        assert!(matches!(
            update.payload,
            Some(server_message::Payload::ChannelState(ref state)) if !state.removed
        ));
        assert!(matches!(
            delete.payload,
            Some(server_message::Payload::ChannelState(ref state)) if state.removed
        ));
        assert!(!hub
            .state
            .lock()
            .unwrap()
            .welcome_for(session)
            .channels
            .iter()
            .any(|c| c.id == channel_id));
    }
    #[test]
    fn welcome_and_subscription_cover_concurrent_commits_without_duplicates() {
        let hub = hub();
        let (old, old_rx, _client) = queued_peer(1);
        let old = hub
            .admit_peer(
                PublicKey([1; 32]),
                "",
                "old",
                (&[1; 32], &[2; 32]),
                old.outbound.clone(),
                Arc::clone(&old.shutdown),
            )
            .unwrap();
        assert!(matches!(
            old_rx.recv().unwrap(),
            Outbound::Message(ServerMessage {
                payload: Some(server_message::Payload::Welcome(_))
            })
        ));
        let (new, new_rx, _new_client) = queued_peer(2);
        // 保持一项真实状态提交暂停；新连接必须等它连广播一起提交。
        let (entered, ready) = channel();
        let (release, wait) = channel();
        let commit_hub = Arc::clone(&hub);
        let session = old.session;
        let change = std::thread::spawn(move || {
            commit_hub.mutate(|state| {
                let events = state.create_channel(
                    session,
                    CreateChannel {
                        name: "before".into(),
                        ..Default::default()
                    },
                );
                entered.send(()).unwrap();
                wait.recv().unwrap();
                events
            })
        });
        ready.recv().unwrap();
        let join_hub = Arc::clone(&hub);
        let (attempt, attempted) = channel();
        let join = std::thread::spawn(move || {
            attempt.send(()).unwrap();
            join_hub
                .admit_peer(
                    PublicKey([2; 32]),
                    "",
                    "new",
                    (&[3; 32], &[4; 32]),
                    new.outbound.clone(),
                    Arc::clone(&new.shutdown),
                )
                .unwrap()
        });
        attempted.recv().unwrap();
        release.send(()).unwrap();
        change.join().unwrap();
        let newcomer = join.join().unwrap();
        let Outbound::Message(ServerMessage {
            payload: Some(server_message::Payload::Welcome(welcome)),
        }) = new_rx.recv().unwrap()
        else {
            panic!()
        };
        assert!(welcome.channels.iter().any(|c| c.name == "before"));
        assert!(
            new_rx.try_recv().is_err(),
            "Welcome 中已有的入场不应再广播给新人"
        );
        hub.mutate(|state| {
            state.create_channel(
                session,
                CreateChannel {
                    name: "after".into(),
                    ..Default::default()
                },
            )
        });
        let Outbound::Message(ServerMessage {
            payload: Some(server_message::Payload::ChannelState(event)),
        }) = new_rx.recv().unwrap()
        else {
            panic!()
        };
        assert_eq!(event.channel.unwrap().name, "after");
        assert!(hub.peers.lock().unwrap().contains_key(&newcomer.session));
    }
}
