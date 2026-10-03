// SPDX-License-Identifier: MPL-2.0

//! 一条到服务器的连接，以及界面用来驱动它的把手。
//!
//! # 断线自动重连
//!
//! 一群人挂几个小时，中间路由器抖一下、Wi-Fi 切一下、服务器重启一下，
//! 都不该要人回来手动点「加入」。所以 [`Client`] 是一个**比单条 TCP 连接活得久**
//! 的把手：底下的连接断了，它自己按退避重连，连上之后回到原来的频道、恢复
//! 闭麦 / 关耳朵，界面拿着的那个 [`Client`] 一直有效。
//!
//! 什么时候**不**重连，比什么时候重连更要紧：
//!
//! - 服务端说了 `Goodbye`（顶号、踢、封）—— 连接是被拿走的，不是丢的。
//!   顶号的两端要是各自重连，会互相踢个没完
//! - 重连时服务端明确拒绝（封了、邀请码失效、版本不对）—— 见
//!   [`ConnectError::is_retryable`]
//! - 用户自己点了离开 / 取消 —— [`Client::disconnect`]
//!
//! 除此之外一律当成网络问题，一直重试到用户取消为止。退避封顶 30 秒：
//! 挂着过夜的人，醒来时应该已经连回去了。
//!
//! # 半死的连接
//!
//! 网线拔了、NAT 表项过期了，TCP 不一定会报错 —— 读线程可能永远阻塞在
//! `read` 上。所以心跳线程同时看着「上一次收到服务端的任何东西是什么时候」，
//! 超过 [`LIVENESS_TIMEOUT`] 就自己把 socket 掐掉，让读线程醒过来走重连。
//! 服务端每个 Ping 都回 Pong，所以连接活着时这个时间不会超过一个心跳周期。
//!
//! # 用户操作只排队，不等待网络
//!
//! 认证完成后，每条连接有一个有界 FIFO 写线程。频道、文字、管理和自身状态
//! 操作只提交本地命令；返回**不代表服务端已收到或应用**，实际结果由收到的
//! 名单、频道和文字广播确认。队列耗尽会断开当前连接，沿用统一重连流程，
//! 不阻塞调用方，也不静默丢一条命令后继续使用这条连接。
//! 闭麦 / 关耳朵意愿会立即保存并在重连时恢复；文字和管理命令不会重放。
//! [`Client::connect`] 的初次握手仍会等待网络，应在前端的后台任务中调用。

use std::net::{SocketAddr, TcpStream};
use std::sync::atomic::{AtomicBool, AtomicU64, AtomicUsize, Ordering};
use std::sync::mpsc::{self, Receiver, Sender, SyncSender};
use std::sync::{Arc, Condvar, Mutex, MutexGuard};
use std::time::{Duration, Instant};

use protocol::control::{
    goodbye, server_message, Authenticate, BanUser, CreateChannel, DeleteChannel, EditChannel,
    Hello, JoinChannel, KickUser, Ping, Role, SelfState, ServerMessage, SetRole, TextMessage,
    Unban, Welcome, PROTOCOL_VERSION,
};
use protocol::Invite;
use rustls::pki_types::ServerName;
use transport::{client_config, derive_voice_key, VoiceKey, DOWNSTREAM, UPSTREAM};
use voice_core::identity::Identity;

use crate::error::{farewell, ConnectError};
use crate::roster::{ChatLine, Roster};
use crate::wire::{Reader, Wire};

/// 多久发一次心跳。
///
/// 服务端 30 秒收不到东西就踢人，所以这个值要有足够的余量 ——
/// 掉一两个包不能导致掉线。5 秒给了六次机会。
pub const HEARTBEAT: Duration = Duration::from_secs(5);

/// 多久没收到服务端的任何东西，就认定连接已经死了、掐掉重连。
///
/// 三个心跳周期：掉两个 Pong 不算事，连掉三个就不是抖动了。
/// 再长的话，用户对着一个早就断了的频道说十几秒话却没人听见；
/// 再短的话，一次正常的网络卡顿就会触发重连，反而把声音断得更久。
pub const LIVENESS_TIMEOUT: Duration = Duration::from_secs(15);

/// 建连接时等多久。
///
/// 比默认的 TCP 超时短得多：用户盯着「正在连接」这四个字，
/// 等 20 秒和等 8 秒是完全不同的体验，而地址不对的时候多等那 12 秒
/// 一点用都没有。
pub const CONNECT_TIMEOUT: Duration = Duration::from_secs(8);

/// 断线后第一次重连前等多久。之后每次翻倍，封顶 [`RECONNECT_MAX`]。
///
/// 不是 0：刚断的那一瞬间网络多半还没恢复，立刻重试只是白白失败一次。
/// 半秒对「网络抖了一下」来说足够，对用户来说几乎察觉不到。
pub const RECONNECT_FIRST: Duration = Duration::from_millis(500);

/// 重连间隔的上限。
///
/// 服务器重启、宽带重拨这种要几十秒才恢复的事，30 秒一次足够及时；
/// 再频繁就是在给一个没开的服务器刷连接。
pub const RECONNECT_MAX: Duration = Duration::from_secs(30);

/// 控制协议平时只有少量用户操作和每五秒一次的 Ping。
/// 网络停顿时同时限制消息数与字节数，不把积压无限转移到常驻客户端内存里。
const WRITE_QUEUE_MESSAGES: usize = 128;
const WRITE_QUEUE_BYTES: usize = 4 * protocol::control::MAX_FRAME_BODY;

/// 连接的时间参数。正常使用就用 [`Options::default`]；
/// 测试要把它们调短，不然一个「连接死了」的用例要跑十几秒。
#[derive(Debug, Clone, Copy)]
pub struct Options {
    pub heartbeat: Duration,
    pub liveness_timeout: Duration,
    pub reconnect_first: Duration,
    pub reconnect_max: Duration,
}

impl Default for Options {
    fn default() -> Self {
        Self {
            heartbeat: HEARTBEAT,
            liveness_timeout: LIVENESS_TIMEOUT,
            reconnect_first: RECONNECT_FIRST,
            reconnect_max: RECONNECT_MAX,
        }
    }
}

/// 界面要知道的事。
#[derive(Debug, Clone, PartialEq)]
pub enum Event {
    /// 名单或频道树变了 —— 重画。
    RosterChanged,
    /// 有人进来了。界面据此播提示音、念 TTS。
    ///
    /// 名字直接带在这儿，而不是让界面去 [`Roster`] 里查 ——
    /// 收到「谁走了」的时候，那个人已经从名单里删掉了。
    Joined {
        session: u32,
        name: String,
    },
    Left {
        session: u32,
        name: String,
    },
    /// 有人进了**我所在的**频道：从别的频道挪过来，或者刚连上就落在这儿。
    ///
    /// 跟 [`Event::Joined`] 不是一回事：那个是「连上了服务器」，不管在哪个频道。
    /// 提示音和念名字要的是这个 —— 隔壁频道进来个人，跟我没关系。
    ///
    /// **我自己换频道不算**：换到一个有五个人的频道，不该念五遍「某某进来了」。
    CameIn {
        session: u32,
        name: String,
    },
    /// 有人离开了我所在的频道：挪去了别处，或者断线走了。
    WentOut {
        session: u32,
        name: String,
    },
    /// 收到一条文字。
    Text(ChatLine),
    /// 连接断了，正在自己重连。**不是错误页** —— 界面该显示「正在重连」，
    /// 名单保持断之前的样子，给一个取消按钮（[`Client::disconnect`]）。
    ///
    /// 每次重试前发一次。`reason` 是上一次为什么没成，能直接显示。
    Reconnecting {
        attempt: u32,
        retry_in: Duration,
        reason: String,
    },
    /// 重新认证成功，恢复原频道和自身状态的命令已经排队。
    /// 名单广播随后确认服务端实际应用的状态。
    ///
    /// **会话 id、UDP 端口、语音密钥全都换了**（旧密钥的序号空间不能复用，
    /// 见 `protocol::crypto`），所以语音链路要按 [`Client`] 上的新值重起。
    Reconnected,
    /// 连接彻底结束了，不会再重连。
    Disconnected(Ended),
}

/// 连接为什么结束了。
#[derive(Debug, Clone, PartialEq)]
pub enum Ended {
    /// 用户自己点了离开，或者在重连时点了取消。**不是错误**，界面别报错。
    ByUser,
    /// 被服务端拒之门外：顶号、被踢、被封，或者重连时碰上了再试也没用的错误。
    Refused { headline: String, advice: String },
}

/// 一个会自己重连的连接把手。克隆出来的把手可以在任意线程上用，
/// 重连前后都是同一个。
#[derive(Clone)]
pub struct Client {
    shared: Arc<Shared>,
}

pub struct VoiceKeys {
    pub sequences: Arc<protocol::VoiceSequences>,
    pub upstream: VoiceKey,
    pub downstream: VoiceKey,
}

/// 一次成功的连接。重连就是换掉这一整个。
struct Link {
    shutdown_sock: TcpStream,
    #[cfg(test)]
    wire: Arc<Mutex<Wire>>,
    writer: SyncSender<PendingMessage>,
    writer_state: Arc<WriterState>,
    session_id: u32,
    udp_port: u16,
    voice_addr: SocketAddr,
    voice: Arc<VoiceKeys>,
}

struct WriterState {
    stopped: AtomicBool,
    completed: AtomicBool,
    pending_bytes: Arc<AtomicUsize>,
    error: Mutex<Option<String>>,
}

impl WriterState {
    fn fail(&self, socket: &TcpStream, message: String) {
        *self.error.lock().expect("writer error poisoned") = Some(message);
        self.stopped.store(true, Ordering::Release);
        let _ = socket.shutdown(std::net::Shutdown::Both);
    }
}

/// 字节许可随消息释放；发送失败、排队失败或队列退场都不会泄漏预算。
struct PendingMessage {
    message: protocol::control::ClientMessage,
    bytes: usize,
    budget: Arc<AtomicUsize>,
}

impl Drop for PendingMessage {
    fn drop(&mut self) {
        self.budget.fetch_sub(self.bytes, Ordering::AcqRel);
    }
}

impl Link {
    // 新 Rust 将 fetch_update 改名为 try_update；保留旧名以兼容 Rust 1.80。
    #[allow(deprecated)]
    fn send(&self, message: &protocol::control::ClientMessage) {
        if self.writer_state.stopped.load(Ordering::Acquire) {
            return;
        }
        let bytes = prost::Message::encoded_len(message);
        if bytes > protocol::control::MAX_FRAME_BODY {
            self.writer_state.fail(
                &self.shutdown_sock,
                "控制消息超过协议长度上限，正在重新连接。".into(),
            );
            return;
        }
        if self
            .writer_state
            .pending_bytes
            .fetch_update(Ordering::AcqRel, Ordering::Acquire, |used| {
                used.checked_add(bytes)
                    .filter(|&total| total <= WRITE_QUEUE_BYTES)
            })
            .is_err()
        {
            self.writer_state.fail(
                &self.shutdown_sock,
                "控制消息积压过多，正在重新连接服务器。".into(),
            );
            return;
        }
        let pending = PendingMessage {
            message: message.clone(),
            bytes,
            budget: Arc::clone(&self.writer_state.pending_bytes),
        };
        // try_send 不等待 TLS 锁或 socket。只有单个 writer 消费，保留调用顺序。
        if self.writer.try_send(pending).is_err() {
            self.writer_state.fail(
                &self.shutdown_sock,
                "控制消息队列不可用，正在重新连接服务器。".into(),
            );
        }
    }

    fn shutdown(&self) {
        // Never wait for a stalled TLS writer before interrupting the socket.
        self.writer_state.stopped.store(true, Ordering::Release);
        let _ = self.shutdown_sock.shutdown(std::net::Shutdown::Both);
    }
}

impl Drop for Link {
    fn drop(&mut self) {
        self.shutdown();
    }
}

/// Worker 不持 Link/Shared，也不持 sender：最后一个 Link 释放时可自然退场。
/// 阻塞的 TLS 发送只发生在这里；shutdown 通过独立 socket 句柄打断它。
fn writer_loop(
    wire: Arc<Mutex<Wire>>,
    socket: TcpStream,
    state: Arc<WriterState>,
    queue: Receiver<PendingMessage>,
) {
    struct Completed<'a>(&'a WriterState, &'a TcpStream);
    impl Drop for Completed<'_> {
        fn drop(&mut self) {
            self.0.stopped.store(true, Ordering::Release);
            let _ = self.1.shutdown(std::net::Shutdown::Both);
            self.0.completed.store(true, Ordering::Release);
        }
    }
    let _completed = Completed(&state, &socket);
    // Drop the queue and all byte permits before reporting completion.
    let queue = queue;
    while !state.stopped.load(Ordering::Acquire) {
        let pending = match queue.recv_timeout(Duration::from_millis(50)) {
            Ok(pending) => pending,
            Err(mpsc::RecvTimeoutError::Timeout) => continue,
            Err(mpsc::RecvTimeoutError::Disconnected) => return,
        };
        let result = match wire.lock() {
            Ok(mut wire) => {
                if state.stopped.load(Ordering::Acquire) {
                    return;
                }
                wire.send(&pending.message)
            }
            Err(_) => Err(std::io::Error::other("TLS 写入状态不可用")),
        };
        if let Err(error) = result {
            state.fail(
                &socket,
                format!("控制消息发送失败：{error}。正在重新连接服务器。"),
            );
            return;
        }
    }
}

struct Shared {
    invite: Invite,
    identity: Identity,
    desired_name: String,
    options: Options,

    link: Mutex<Arc<Link>>,
    roster: Mutex<Roster>,
    /// 自己设的闭麦 / 关耳朵。重连之后要原样告诉新会话。
    self_state: Mutex<(bool, bool)>,

    /// 用户要走了。置位之后不再重连，后台线程各自收场。
    closing: AtomicBool,
    /// 让后台线程的等待能被 [`Client::disconnect`] 立刻叫醒 ——
    /// 不然点了「取消重连」要等到下一次退避到点才有反应，最长 30 秒。
    wake: Condvar,
    wake_lock: Mutex<()>,

    /// 上一次收到服务端任何东西的时刻，相对 `epoch` 的毫秒数。
    last_heard_ms: AtomicU64,
    server_udp_received: AtomicU64,
    epoch: Instant,
}

impl Shared {
    fn link(&self) -> Arc<Link> {
        Arc::clone(&self.link.lock().expect("link poisoned"))
    }

    fn closing(&self) -> bool {
        self.closing.load(Ordering::SeqCst)
    }

    /// 睡 `d`，除非中途有人要关。返回 `false` 表示是被叫醒的 —— 该收场了。
    fn sleep(&self, d: Duration) -> bool {
        let guard = self.wake_lock.lock().expect("wake poisoned");
        let _ = self
            .wake
            .wait_timeout_while(guard, d, |_| !self.closing())
            .expect("wake poisoned");
        !self.closing()
    }

    fn heard(&self) {
        self.last_heard_ms
            .store(self.epoch.elapsed().as_millis() as u64, Ordering::Relaxed);
    }

    fn silent_for(&self) -> Duration {
        let now = self.epoch.elapsed().as_millis() as u64;
        Duration::from_millis(now.saturating_sub(self.last_heard_ms.load(Ordering::Relaxed)))
    }
}

impl Client {
    /// 按一条邀请链接连上去，走完认证。
    ///
    /// 这是用户实际经历的全部过程：粘一串东西进来，然后就在频道里了。
    ///
    /// **第一次连接不自动重试**：连不上就把原因原样返回。用户正盯着屏幕，
    /// 该让他马上知道是链接错了还是服务器没开，而不是转圈。
    /// 自动重连只管「连上过、后来断了」这一种情况。
    pub fn connect(
        link: &str,
        identity: &Identity,
        desired_name: &str,
    ) -> Result<(Self, Receiver<Event>), ConnectError> {
        Self::connect_with(link, identity, desired_name, Options::default())
    }

    pub fn connect_with(
        link: &str,
        identity: &Identity,
        desired_name: &str,
        options: Options,
    ) -> Result<(Self, Receiver<Event>), ConnectError> {
        let invite = Invite::parse(link).map_err(ConnectError::BadInvite)?;
        Self::connect_to_with(&invite, identity, desired_name, options)
    }

    pub fn connect_to(
        invite: &Invite,
        identity: &Identity,
        desired_name: &str,
    ) -> Result<(Self, Receiver<Event>), ConnectError> {
        Self::connect_to_with(invite, identity, desired_name, Options::default())
    }

    pub fn connect_to_with(
        invite: &Invite,
        identity: &Identity,
        desired_name: &str,
        options: Options,
    ) -> Result<(Self, Receiver<Event>), ConnectError> {
        let (link, reader, welcome) = establish(invite, identity, desired_name)?;

        let shared = Arc::new(Shared {
            invite: invite.clone(),
            identity: identity.clone(),
            desired_name: desired_name.to_string(),
            options,
            link: Mutex::new(Arc::new(link)),
            roster: Mutex::new(Roster::from_welcome(&welcome)),
            self_state: Mutex::new((false, false)),
            closing: AtomicBool::new(false),
            wake: Condvar::new(),
            wake_lock: Mutex::new(()),
            last_heard_ms: AtomicU64::new(0),
            server_udp_received: AtomicU64::new(0),
            epoch: Instant::now(),
        });

        let (tx, rx) = mpsc::channel();
        {
            let shared = Arc::clone(&shared);
            std::thread::Builder::new()
                .name("gouhuo-client-read".into())
                .spawn(move || run(shared, reader, tx))
                .expect("开不出读线程");
        }
        {
            let shared = Arc::clone(&shared);
            std::thread::Builder::new()
                .name("gouhuo-client-ping".into())
                .spawn(move || heartbeat(shared))
                .expect("开不出心跳线程");
        }
        Ok((Client { shared }, rx))
    }

    /// 是不是同一个连接把手（克隆出来的算同一个）。
    pub fn is_same(&self, other: &Client) -> bool {
        Arc::ptr_eq(&self.shared, &other.shared)
    }

    /// 当前这次连接的会话 id。**重连之后会变。**
    pub fn session_id(&self) -> u32 {
        self.shared.link().session_id
    }

    /// 语音要发到服务器的哪个 UDP 端口。
    pub fn udp_port(&self) -> u16 {
        self.shared.link().udp_port
    }

    /// 服务器的地址，取自邀请链接。
    pub fn server_host(&self) -> &str {
        &self.shared.invite.host
    }

    /// 当前这次连接的语音密钥。**重连之后会变。**
    pub fn voice_keys(&self) -> Arc<VoiceKeys> {
        Arc::clone(&self.shared.link().voice)
    }

    /// 同一次连接的会话、端口与密钥/序号快照，避免重连时分别取值混用新旧连接。
    pub fn voice_session(&self) -> (u32, u16, Arc<VoiceKeys>) {
        let link = self.shared.link();
        (link.session_id, link.udp_port, Arc::clone(&link.voice))
    }

    /// Voice destination and keys from the same authenticated connection.
    /// Use the TCP peer rather than resolving the invitation again: DNS may
    /// put an unreachable IPv6 address or a different server first.
    pub fn voice_endpoint_session(&self) -> (u32, SocketAddr, Arc<VoiceKeys>) {
        let link = self.shared.link();
        (link.session_id, link.voice_addr, Arc::clone(&link.voice))
    }

    /// 借出名单来画界面。**别在持有它的时候做慢事情** ——
    /// 读线程要拿同一把锁才能把新状态写进去。
    pub fn roster(&self) -> MutexGuard<'_, Roster> {
        self.shared.roster.lock().expect("roster poisoned")
    }

    /// 排队请求切换频道；返回时不保证已经切换，名单广播确认最终结果。
    pub fn join_channel(&self, channel_id: u32) {
        self.send(&JoinChannel { channel_id }.into());
    }

    /// 建一个频道，挂在 `parent_id` 下面（传 0 就是挂根频道下）。
    ///
    /// **不在本地先插一个再等确认。** 建成了服务端会广播一条 `ChannelState`
    /// 给所有人（含自己），名单那边照常处理；没建成就什么都不会发生 ——
    /// 本地先插的话，被拒时界面上会留一个只有自己看得见的幽灵频道。
    ///
    /// 能不能建由服务端按角色决定，见 [`Roster::can_create_channel`]。
    pub fn create_channel(&self, name: &str, parent_id: u32) {
        let name = name.trim();
        if name.is_empty() {
            return;
        }
        self.send(
            &CreateChannel {
                parent_id,
                name: name.to_string(),
                description: String::new(),
                max_users: 0,
                // 不指定门槛：服务端会按建的人自己的角色兜底。
                min_role: Role::Unspecified as i32,
            }
            .into(),
        );
    }

    /// 删一个频道。根频道删不掉，服务端会忽略。
    pub fn delete_channel(&self, channel_id: u32) {
        self.send(&DeleteChannel { channel_id }.into());
    }

    /// 改一个频道：名字、说明、父频道。发的是完整的目标状态 —— 不想改的传当前值，
    /// 见 [`Client::rename_channel`]。能不能改由服务端决定，改成了会广播给所有人。
    pub fn edit_channel(&self, channel_id: u32, name: &str, description: &str, parent_id: u32) {
        let name = name.trim();
        if name.is_empty() {
            return;
        }
        self.send(
            &EditChannel {
                channel_id,
                name: name.to_string(),
                description: description.trim().to_string(),
                parent_id,
            }
            .into(),
        );
    }

    /// 只改名，别的照旧。
    pub fn rename_channel(&self, channel_id: u32, name: &str) {
        let current = self
            .roster()
            .channels
            .get(&channel_id)
            .map(|c| (c.description.clone(), c.parent_id));
        if let Some((description, parent_id)) = current {
            self.edit_channel(channel_id, name, &description, parent_id);
        }
    }

    /// 踢人。理由可以空着。能不能踢见 [`Roster::can_kick`]。
    pub fn kick(&self, session_id: u32, reason: &str) {
        self.send(
            &KickUser {
                session_id,
                reason: reason.trim().to_string(),
            }
            .into(),
        );
    }

    /// 封禁：踢出去，而且这个人以后再也进不来。只有管理员能封。
    pub fn ban(&self, session_id: u32, reason: &str) {
        self.send(
            &BanUser {
                session_id,
                reason: reason.trim().to_string(),
            }
            .into(),
        );
    }

    /// 解封。公钥从 [`Roster::bans`] 里拿。
    pub fn unban(&self, public_key: &[u8]) {
        self.send(
            &Unban {
                public_key: public_key.to_vec(),
            }
            .into(),
        );
    }

    /// 改一个人的角色。只有管理员能改。
    pub fn set_role(&self, session_id: u32, role: Role) {
        self.send(
            &SetRole {
                session_id,
                role: role as i32,
            }
            .into(),
        );
    }

    /// 排队发送文字；收到服务器带归属与时间戳的广播才表示发送已确认。
    pub fn send_text(&self, body: &str) {
        let body = body.trim();
        if body.is_empty() {
            return;
        }
        // 频道、发送者、时间戳全由服务端盖章，这里填什么都会被覆盖。
        self.send(
            &TextMessage {
                channel_id: 0,
                sender_session_id: 0,
                body: body.to_string(),
                timestamp_ms: 0,
            }
            .into_client(),
        );
    }

    /// 闭麦 / 关耳朵。记下来，重连之后原样告诉新会话 ——
    /// 不然用户闭着麦断了一次线，回来就变成开着麦了。
    /// 返回只保证本地意愿已保存并尝试排队，服务端状态由广播确认。
    pub fn set_self_state(&self, self_muted: bool, self_deafened: bool) {
        // 短锁覆盖保存与入队，多线程调用不会把较旧的状态排在新状态后面。
        // 入队从不等待 TLS 写锁，网络阻塞也不延迟本地意愿。
        let mut desired = self.shared.self_state.lock().expect("self_state poisoned");
        *desired = (self_muted, self_deafened);
        self.send(
            &SelfState {
                self_muted,
                self_deafened,
            }
            .into(),
        );
    }

    /// Last server-reported UDP count, including keepalives; not a delivery acknowledgement.
    pub fn server_udp_received(&self) -> u64 {
        self.shared.server_udp_received.load(Ordering::Relaxed)
    }

    /// Rebuild a failing voice session using the existing reconnect policy and credentials.
    pub fn reconnect_transport(&self) {
        if !self.shared.closing() {
            self.shared.link().shutdown();
        }
    }

    /// 主动断开或取消重连，之后不会自动重连。
    pub fn disconnect(&self) {
        self.shared.closing.store(true, Ordering::SeqCst);
        {
            let _guard = self.shared.wake_lock.lock().expect("wake poisoned");
            self.shared.wake.notify_all();
        }
        self.shared.link().shutdown();
    }

    fn send(&self, message: &protocol::control::ClientMessage) {
        self.shared.link().send(message);
    }
}

/// 一条连接读到头的时候是怎么结束的。
enum Outcome {
    /// 界面没了，没人听了。
    Abandoned,
    /// 服务端说了再见。不重连。
    Goodbye(Ended),
    /// 连接断了，原因能直接显示。可能要重连。
    Lost(String),
}

/// 读线程的一生：读到断，决定重不重连，重连上了接着读。
fn run(shared: Arc<Shared>, mut reader: Reader, tx: Sender<Event>) {
    shared.heard();
    loop {
        let reason = match read_until_end(&shared, &mut reader, &tx) {
            Outcome::Abandoned => {
                shared.closing.store(true, Ordering::SeqCst);
                return;
            }
            Outcome::Goodbye(ended) => return finish(&shared, &tx, ended),
            Outcome::Lost(_) if shared.closing() => return finish(&shared, &tx, Ended::ByUser),
            Outcome::Lost(reason) => reason,
        };
        match reconnect(&shared, &tx, reason) {
            Some(next) => reader = next,
            None => return,
        }
    }
}

fn finish(shared: &Shared, tx: &Sender<Event>, ended: Ended) {
    shared.closing.store(true, Ordering::SeqCst);
    {
        let _guard = shared.wake_lock.lock().expect("wake poisoned");
        shared.wake.notify_all();
    }
    shared.link().shutdown();
    let _ = tx.send(Event::Disconnected(ended));
}

fn read_until_end(shared: &Shared, reader: &mut Reader, tx: &Sender<Event>) -> Outcome {
    loop {
        match reader.next::<ServerMessage>() {
            Ok(Some(message)) => {
                shared.heard();
                if let Some(server_message::Payload::Pong(pong)) = &message.payload {
                    shared
                        .server_udp_received
                        .store(pong.udp_packets_received, Ordering::Relaxed);
                }
                if let Some(server_message::Payload::Goodbye(bye)) = &message.payload {
                    let reason = goodbye::Reason::try_from(bye.reason)
                        .unwrap_or(goodbye::Reason::Unspecified);
                    let (headline, advice) = farewell(reason, &bye.detail);
                    return Outcome::Goodbye(Ended::Refused { headline, advice });
                }
                for event in apply(&shared.roster, message) {
                    if tx.send(event).is_err() {
                        return Outcome::Abandoned;
                    }
                }
            }
            Ok(None) => return Outcome::Lost(lost_reason(shared, "服务器关闭了连接".to_string())),
            Err(e) => return Outcome::Lost(lost_reason(shared, format!("连接断了：{e}"))),
        }
    }
}

fn lost_reason(shared: &Shared, fallback: String) -> String {
    shared
        .link()
        .writer_state
        .error
        .lock()
        .expect("writer error poisoned")
        .clone()
        .unwrap_or(fallback)
}

/// 第 `attempt` 次重连（从 1 起）前等多久：从 `first` 起翻倍，封顶 `max`，
/// 再上下抖 20%。
///
/// 抖动是给服务器重启那种场景的：二十个人同一秒掉线，不抖的话会在同一毫秒
/// 一齐撞上来，每一轮都是。
fn backoff(attempt: u32, first: Duration, max: Duration) -> Duration {
    let doublings = attempt.saturating_sub(1).min(16);
    let base = first.saturating_mul(1 << doublings).min(max);
    // 不值得为此引一个随机数 crate：要的只是「每个客户端不一样」。
    let nanos = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.subsec_nanos())
        .unwrap_or(0);
    let spread = 0.8 + 0.4 * (nanos % 1000) as f64 / 1000.0;
    base.mul_f64(spread)
}

/// 断了之后一直试到连上、碰上不该再试的错误、或者用户取消。
///
/// 连上了返回新的 [`Reader`]；返回 `None` 时 `Disconnected` 已经发过了。
fn reconnect(shared: &Arc<Shared>, tx: &Sender<Event>, mut reason: String) -> Option<Reader> {
    // 断之前在哪个频道。按「id 和名字都对得上」去找 —— 服务器要是重启过，
    // 频道还没持久化，同一个 id 可能已经是别的频道了。
    let wanted = {
        let roster = shared.roster.lock().expect("roster poisoned");
        let id = roster.my_channel();
        roster.channels.get(&id).map(|c| (id, c.name.clone()))
    };

    let mut attempt = 0;
    loop {
        attempt += 1;
        let retry_in = backoff(
            attempt,
            shared.options.reconnect_first,
            shared.options.reconnect_max,
        );
        let notice = Event::Reconnecting {
            attempt,
            retry_in,
            reason: reason.clone(),
        };
        if tx.send(notice).is_err() {
            shared.closing.store(true, Ordering::SeqCst);
            return None;
        }
        if !shared.sleep(retry_in) {
            finish(shared, tx, Ended::ByUser);
            return None;
        }

        match establish(&shared.invite, &shared.identity, &shared.desired_name) {
            Ok((link, reader, welcome)) => {
                if shared.closing() {
                    // 连接建到一半用户点了取消。刚连上的这条也不要了。
                    link.shutdown();
                    finish(shared, tx, Ended::ByUser);
                    return None;
                }
                install(shared, link, &welcome, wanted.as_ref());
                if tx.send(Event::Reconnected).is_err() || tx.send(Event::RosterChanged).is_err() {
                    shared.closing.store(true, Ordering::SeqCst);
                    return None;
                }
                return Some(reader);
            }
            Err(e) if e.is_retryable() => reason = e.headline(),
            Err(e) => {
                let ended = Ended::Refused {
                    headline: e.headline(),
                    advice: e.advice(),
                };
                finish(shared, tx, ended);
                return None;
            }
        }
    }
}

/// 把新连接换上去，恢复断之前的样子。
fn install(shared: &Shared, link: Link, welcome: &Welcome, wanted: Option<&(u32, String)>) {
    let link = Arc::new(link);

    // 名单整个换成新的，但**聊天记录留着** —— 那是本地的，断一次线不该清屏。
    let target = {
        let mut roster = shared.roster.lock().expect("roster poisoned");
        let chat = std::mem::take(&mut roster.chat);
        *roster = Roster::from_welcome(welcome);
        roster.chat = chat;
        wanted.and_then(|(id, name)| find_channel(&roster, *id, name))
    };

    // **先重置「上次收到」再换上链接**：顺序反过来的话，心跳线程可能正好在
    // 两步之间醒来，看到一个「已经十几秒没动静」的新连接，把它当死连接掐掉。
    shared.heard();
    shared.server_udp_received.store(0, Ordering::Relaxed);
    *shared.link.lock().expect("link poisoned") = Arc::clone(&link);

    if let Some(channel_id) = target {
        if channel_id != welcome.current_channel_id {
            link.send(&JoinChannel { channel_id }.into());
        }
    }
    // 与 set_self_state 的保存+入队共用短锁，恢复消息不会覆盖刚排队的新意愿。
    let desired = shared.self_state.lock().expect("self_state poisoned");
    let (self_muted, self_deafened) = *desired;
    if self_muted || self_deafened {
        link.send(
            &SelfState {
                self_muted,
                self_deafened,
            }
            .into(),
        );
    }
}

/// 在新名单里找断线前待的那个频道。
///
/// 先认 id + 名字；对不上（服务器重启过，id 重新分了）就按名字找，
/// 但只在名字唯一时才算数 —— 两个同名频道里随便挑一个，比待在根频道更糟。
fn find_channel(roster: &Roster, id: u32, name: &str) -> Option<u32> {
    if roster.channels.get(&id).is_some_and(|c| c.name == name) {
        return Some(id);
    }
    let mut same_name = roster.channels.values().filter(|c| c.name == name);
    match (same_name.next(), same_name.next()) {
        (Some(only), None) => Some(only.id),
        _ => None,
    }
}

/// 心跳，顺带看连接还活不活着。
fn heartbeat(shared: Arc<Shared>) {
    loop {
        if !shared.sleep(shared.options.heartbeat) {
            return;
        }
        let link = shared.link();
        if shared.silent_for() > shared.options.liveness_timeout {
            // 掐掉 socket，阻塞在 read 上的读线程会醒过来走重连。
            // 重连期间这里会反复掐那条早就死了的旧连接，无害。
            link.shutdown();
            continue;
        }
        link.send(
            &Ping {
                timestamp: now_ms(),
                udp_packets_received: 0,
            }
            .into(),
        );
    }
}

/// 建一条连接：TCP、TLS、派生语音密钥、认证。
fn establish(
    invite: &Invite,
    identity: &Identity,
    desired_name: &str,
) -> Result<(Link, Reader, Welcome), ConnectError> {
    let (sock, mut voice_addr) = connect_tcp_peer(&invite.host, invite.port)?;
    sock.set_nodelay(true)?;
    sock.set_write_timeout(Some(CONNECT_TIMEOUT))?;
    // 握手和认证期间要有读超时：对面接了 TCP 却一声不吭（半死的 NAT、
    // 不是篝火的服务），没有超时的话这里会永远等下去 —— 重连循环也跟着卡死。
    // 超时一旦触发这条连接就作废了，所以下面那条「超时会吃数据」的坑碰不到。
    sock.set_read_timeout(Some(CONNECT_TIMEOUT))?;
    let read_sock = sock.try_clone()?;

    let config =
        Arc::new(client_config(invite.cert).map_err(|e| ConnectError::Tls(e.to_string()))?);
    // 服务端证书是自签的，主机名不参与判断（我们固定的是证书本身），
    // 所以这里放什么都行。放一个固定的常量，免得给人「换个名字就能绕过」的错觉。
    let name = ServerName::try_from("gouhuo").expect("常量，不会失败");
    let mut conn = rustls::ClientConnection::new(config, name)
        .map_err(|e| ConnectError::Tls(e.to_string()))?;

    let mut handshake_sock = sock.try_clone()?;
    if let Err(e) = conn.complete_io(&mut handshake_sock) {
        let text = e.to_string();
        return Err(if text.contains("指纹对不上") {
            // rustls 会在我们自己的报错前面加一句 "unexpected error: "。这段话是要
            // 原样给用户看的，把那句只有程序员看得懂的前缀去掉。
            let detail = text.rsplit("unexpected error: ").next().unwrap_or(&text);
            ConnectError::WrongCertificate(detail.to_string())
        } else {
            ConnectError::Tls(text)
        });
    }

    // 语音密钥在这里就有了 —— 不另起握手，见 transport::derive_voice_key。
    // 每条连接一对新的：重连换了连接就换了密钥，序号从头来也不会撞上防重放窗口。
    let voice = VoiceKeys {
        sequences: Arc::new(protocol::VoiceSequences::default()),
        upstream: derive_voice_key(&conn, UPSTREAM)
            .map_err(|e| ConnectError::Tls(e.to_string()))?,
        downstream: derive_voice_key(&conn, DOWNSTREAM)
            .map_err(|e| ConnectError::Tls(e.to_string()))?,
    };

    let wire = Arc::new(Mutex::new(Wire { conn, sock }));
    let mut reader = Reader::new(read_sock, Arc::clone(&wire));

    let welcome = authenticate(
        &wire,
        &mut reader,
        identity,
        invite.code.as_deref().unwrap_or(""),
        desired_name,
    )?;

    // 认证走完就把读超时撤掉。**留着它会吃数据** ——
    // 读超时跟到达的数据撞车时，Winsock 会丢掉已经读出来的那部分，
    // 表现为 TLS 流错位（见 server::conn 的模块文档）。
    // 之后「连接还活着吗」由心跳线程看 LIVENESS_TIMEOUT 判断。
    //
    // 两个句柄都要撤：Windows 上 `try_clone` 出来的句柄各自带着读超时，
    // 只撤一个的话，读线程那个句柄会在安静 8 秒后自己报超时 —— 一条好好的
    // 连接平白断掉，还正好踩上上面那个吃数据的坑。
    wire.lock()
        .expect("wire poisoned")
        .sock
        .set_read_timeout(None)?;
    reader.clear_read_timeout()?;

    voice_addr.set_port(welcome.udp_port as u16);
    let writer_state = Arc::new(WriterState {
        stopped: AtomicBool::new(false),
        completed: AtomicBool::new(false),
        pending_bytes: Arc::new(AtomicUsize::new(0)),
        error: Mutex::new(None),
    });
    let (writer, queue) = mpsc::sync_channel(WRITE_QUEUE_MESSAGES);
    {
        let wire = Arc::clone(&wire);
        let socket = handshake_sock.try_clone()?;
        let state = Arc::clone(&writer_state);
        std::thread::Builder::new()
            .name("gouhuo-client-write".into())
            .spawn(move || writer_loop(wire, socket, state, queue))?;
    }
    let link = Link {
        voice_addr,
        shutdown_sock: handshake_sock,
        #[cfg(test)]
        wire,
        writer,
        writer_state,
        session_id: welcome.session_id,
        udp_port: welcome.udp_port as u16,
        voice: Arc::new(voice),
    };
    Ok((link, reader, welcome))
}

/// 把一条服务端消息应用到名单上，产出界面要知道的事。
fn apply(roster: &Mutex<Roster>, message: ServerMessage) -> Vec<Event> {
    let mut roster = roster.lock().expect("roster poisoned");
    match message.payload {
        Some(server_message::Payload::UserState(state)) => {
            let Some(user) = state.user else {
                return Vec::new();
            };
            let session = user.session_id;
            let name = user.name.clone();
            let now_in = user.channel_id;
            // 在改名单之前记下「我在哪」。我自己的那条 UserState 会改掉它，
            // 但那种情况下面整个跳过了。
            let mine = roster.my_channel();
            let previous = roster.users.insert(session, user);
            let mut events = Vec::new();
            if session != roster.me {
                let was_in = previous.as_ref().map(|p| p.channel_id);
                if previous.is_none() {
                    events.push(Event::Joined {
                        session,
                        name: name.clone(),
                    });
                }
                if was_in != Some(now_in) {
                    if now_in == mine {
                        events.push(Event::CameIn {
                            session,
                            name: name.clone(),
                        });
                    } else if was_in == Some(mine) {
                        events.push(Event::WentOut {
                            session,
                            name: name.clone(),
                        });
                    }
                }
            }
            events.push(Event::RosterChanged);
            events
        }
        Some(server_message::Payload::UserLeft(left)) => {
            let mine = roster.my_channel();
            let Some(user) = roster.users.remove(&left.session_id) else {
                return Vec::new();
            };
            let mut events = Vec::new();
            if user.channel_id == mine && left.session_id != roster.me {
                events.push(Event::WentOut {
                    session: left.session_id,
                    name: user.name.clone(),
                });
            }
            events.push(Event::Left {
                session: left.session_id,
                name: user.name,
            });
            events.push(Event::RosterChanged);
            events
        }
        Some(server_message::Payload::ChannelState(state)) => {
            let Some(channel) = state.channel else {
                return Vec::new();
            };
            if state.removed {
                roster.channels.remove(&channel.id);
            } else {
                roster.channels.insert(channel.id, channel);
            }
            vec![Event::RosterChanged]
        }
        Some(server_message::Payload::TextMessage(text)) => {
            let line = ChatLine {
                sender_session: text.sender_session_id,
                sender_name: roster.name_of(text.sender_session_id),
                body: text.body,
                timestamp_ms: text.timestamp_ms,
            };
            roster.push_chat(line.clone());
            vec![Event::Text(line)]
        }
        Some(server_message::Payload::BanList(list)) => {
            roster.bans = list.entries;
            vec![Event::RosterChanged]
        }
        // Pong 暂时不产生界面事件。接上 UDP 之后它会变成「语音通不通」的指示。
        Some(server_message::Payload::Pong(_)) => Vec::new(),
        // 认不出来的分支：新服务端发了我们不懂的东西。**忽略，不要断开。**
        _ => Vec::new(),
    }
}

pub(crate) fn connect_tcp(host: &str, port: u16) -> Result<TcpStream, ConnectError> {
    connect_tcp_peer(host, port).map(|(socket, _)| socket)
}

// Preserve the address of the successful connect, rather than querying a
// cloned Winsock handle after TLS authentication. A late getpeername can fail
// with WSAENOTCONN even after the welcome was read. The address is published
// only after authentication and uses the same DNS attempt as that TCP stream.
fn connect_tcp_peer(host: &str, port: u16) -> Result<(TcpStream, SocketAddr), ConnectError> {
    use std::net::ToSocketAddrs;

    let unreachable = |source: std::io::Error| ConnectError::Unreachable {
        host: host.to_string(),
        port,
        source,
    };

    let addrs: Vec<_> = (host, port)
        .to_socket_addrs()
        .map_err(unreachable)?
        .collect();
    if addrs.is_empty() {
        return Err(unreachable(std::io::Error::other("这个地址查不到")));
    }

    // 一个域名可能解析出好几个地址（IPv6 + IPv4）。挨个试，
    // 全都不通才算失败 —— 只试第一个的话，IPv6 没配好的机器会莫名其妙连不上。
    let mut last = None;
    for addr in addrs {
        match TcpStream::connect_timeout(&addr, CONNECT_TIMEOUT) {
            Ok(sock) => return Ok((sock, addr)),
            Err(e) => last = Some(e),
        }
    }
    Err(unreachable(last.expect("上面判过非空")))
}

fn authenticate(
    wire: &Arc<Mutex<Wire>>,
    reader: &mut Reader,
    identity: &Identity,
    invite_code: &str,
    desired_name: &str,
) -> Result<Welcome, ConnectError> {
    {
        let mut w = wire.lock().expect("wire poisoned");
        w.send(&protocol::control::ClientMessage::from(Hello {
            protocol_version: PROTOCOL_VERSION,
            client_version: crate::client_version(),
            public_key: identity.public_key().0.to_vec(),
        }))?;
    }

    let nonce = match reader.next::<ServerMessage>()?.and_then(|m| m.payload) {
        Some(server_message::Payload::Challenge(c)) => c.nonce,
        Some(server_message::Payload::Rejected(r)) => return Err(rejected(r)),
        // 对面回了个我们看不懂的东西 —— 多半根本不是篝火服务端
        _ => return Err(ConnectError::NotAGouhuoServer),
    };

    {
        let mut w = wire.lock().expect("wire poisoned");
        w.send(&protocol::control::ClientMessage::from(Authenticate {
            signature: identity.sign(&nonce).to_vec(),
            invite_code: invite_code.to_string(),
            desired_name: desired_name.to_string(),
        }))?;
    }

    match reader.next::<ServerMessage>()?.and_then(|m| m.payload) {
        Some(server_message::Payload::Welcome(w)) => Ok(w),
        Some(server_message::Payload::Rejected(r)) => Err(rejected(r)),
        _ => Err(ConnectError::NotAGouhuoServer),
    }
}

fn rejected(r: protocol::control::Rejected) -> ConnectError {
    ConnectError::Rejected {
        reason: protocol::control::rejected::Reason::try_from(r.reason)
            .unwrap_or(protocol::control::rejected::Reason::Unspecified),
        detail: r.detail,
    }
}

fn now_ms() -> i64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_millis() as i64)
        .unwrap_or(0)
}

#[cfg(test)]
mod tests {
    use super::*;
    use protocol::control::Channel;

    #[test]
    fn selected_tcp_address_remains_available_after_remote_close() {
        use std::io::Read;
        let listener = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
        let address = listener.local_addr().unwrap();
        let server = std::thread::spawn(move || {
            let (socket, _) = listener.accept().unwrap();
            drop(socket);
        });
        let (mut socket, selected) = connect_tcp_peer("localhost", address.port()).unwrap();
        socket
            .set_read_timeout(Some(Duration::from_secs(3)))
            .unwrap();
        assert_eq!(socket.read(&mut [0]).unwrap(), 0);
        server.join().unwrap();
        assert_eq!(selected, address);
    }

    #[test]
    fn heartbeat_leaves_room_for_lost_packets() {
        // 服务端 30 秒不收东西就踢。心跳要密到掉几个包也不会掉线。
        let server_timeout = Duration::from_secs(30);
        assert!(
            HEARTBEAT * 5 < server_timeout,
            "心跳太稀了：掉几个包就会被踢下线"
        );
    }

    #[test]
    fn liveness_tolerates_a_couple_of_lost_pongs() {
        assert!(
            LIVENESS_TIMEOUT >= HEARTBEAT * 3,
            "掉一两个 Pong 就判死，正常的网络卡顿都会触发重连"
        );
        assert!(
            LIVENESS_TIMEOUT < Duration::from_secs(30),
            "比服务端的踢人超时还长，就是在对着一条死连接说话"
        );
    }

    #[test]
    fn connect_timeout_is_short_enough_to_wait_for() {
        assert!(CONNECT_TIMEOUT.as_secs() <= 10, "用户会盯着这个时间干等");
        assert!(CONNECT_TIMEOUT.as_secs() >= 5, "太短会把慢网络误判成连不上");
    }

    #[test]
    fn backoff_doubles_and_is_capped() {
        let first = RECONNECT_FIRST;
        let max = RECONNECT_MAX;
        for attempt in 1..=40 {
            let d = backoff(attempt, first, max);
            let doublings = (attempt - 1).min(16);
            let base = first.saturating_mul(1 << doublings).min(max);
            assert!(d >= base.mul_f64(0.8), "第 {attempt} 次等得太短：{d:?}");
            assert!(d <= base.mul_f64(1.2), "第 {attempt} 次等得太长：{d:?}");
        }
        assert!(
            backoff(1, first, max) < Duration::from_secs(1),
            "第一次要快"
        );
        assert!(
            backoff(40, first, max) <= max.mul_f64(1.2),
            "封顶之后不能再涨"
        );
    }

    fn user(session: u32, name: &str, channel_id: u32) -> protocol::control::User {
        protocol::control::User {
            session_id: session,
            name: name.to_string(),
            channel_id,
            ..Default::default()
        }
    }

    /// 我（会话 1）在频道 10。
    fn roster_in_channel_10() -> Mutex<Roster> {
        let mut roster = Roster {
            me: 1,
            ..Default::default()
        };
        roster.users.insert(1, user(1, "我", 10));
        Mutex::new(roster)
    }

    fn state(u: protocol::control::User) -> ServerMessage {
        protocol::control::UserState { user: Some(u) }.into()
    }

    fn left(session: u32) -> ServerMessage {
        protocol::control::UserLeft {
            session_id: session,
            reason: 1,
        }
        .into()
    }

    fn came_in(events: &[Event]) -> Vec<&str> {
        events
            .iter()
            .filter_map(|e| match e {
                Event::CameIn { name, .. } => Some(name.as_str()),
                _ => None,
            })
            .collect()
    }

    fn went_out(events: &[Event]) -> Vec<&str> {
        events
            .iter()
            .filter_map(|e| match e {
                Event::WentOut { name, .. } => Some(name.as_str()),
                _ => None,
            })
            .collect()
    }

    #[test]
    fn someone_connecting_into_my_channel_came_in() {
        let roster = roster_in_channel_10();
        let events = apply(&roster, state(user(2, "阿狸", 10)));
        assert_eq!(came_in(&events), ["阿狸"]);
    }

    #[test]
    fn someone_connecting_elsewhere_is_none_of_my_business() {
        let roster = roster_in_channel_10();
        let events = apply(&roster, state(user(2, "阿狸", 20)));
        assert!(came_in(&events).is_empty());
        assert!(went_out(&events).is_empty());
        // 但「连上了服务器」照样报
        assert!(events.iter().any(|e| matches!(e, Event::Joined { .. })));
    }

    #[test]
    fn moving_in_and_out_of_my_channel() {
        let roster = roster_in_channel_10();
        apply(&roster, state(user(2, "阿狸", 20)));

        let events = apply(&roster, state(user(2, "阿狸", 10)));
        assert_eq!(came_in(&events), ["阿狸"]);

        let events = apply(&roster, state(user(2, "阿狸", 30)));
        assert_eq!(went_out(&events), ["阿狸"]);

        // 在两个别的频道之间挪来挪去，跟我没关系
        let events = apply(&roster, state(user(2, "阿狸", 20)));
        assert!(came_in(&events).is_empty() && went_out(&events).is_empty());
    }

    /// 闭麦、改名这类不换频道的变化，不能当成「又进来了一次」。
    #[test]
    fn a_state_change_without_moving_is_not_a_visit() {
        let roster = roster_in_channel_10();
        apply(&roster, state(user(2, "阿狸", 10)));
        let muted = protocol::control::User {
            self_muted: true,
            ..user(2, "阿狸", 10)
        };
        let events = apply(&roster, state(muted));
        assert!(came_in(&events).is_empty() && went_out(&events).is_empty());
    }

    #[test]
    fn disconnecting_from_my_channel_went_out() {
        let roster = roster_in_channel_10();
        apply(&roster, state(user(2, "阿狸", 10)));
        apply(&roster, state(user(3, "波波", 20)));

        assert_eq!(went_out(&apply(&roster, left(2))), ["阿狸"]);
        assert!(
            went_out(&apply(&roster, left(3))).is_empty(),
            "隔壁的人走了"
        );
    }

    /// 我换到一个有人的频道：那些人本来就在，不是「进来了」。
    #[test]
    fn my_own_move_announces_nobody() {
        let roster = roster_in_channel_10();
        apply(&roster, state(user(2, "阿狸", 20)));
        apply(&roster, state(user(3, "波波", 20)));

        let events = apply(&roster, state(user(1, "我", 20)));
        assert!(came_in(&events).is_empty() && went_out(&events).is_empty());

        // 挪过去之后，阿狸再走就是「从我这儿走了」
        assert_eq!(went_out(&apply(&roster, left(2))), ["阿狸"]);
    }

    fn channel(id: u32, name: &str) -> Channel {
        Channel {
            id,
            parent_id: 1,
            name: name.to_string(),
            ..Default::default()
        }
    }

    #[test]
    fn rejoins_the_same_channel_by_id_and_name() {
        let mut roster = Roster::default();
        roster.channels.insert(1, channel(1, "大厅"));
        roster.channels.insert(5, channel(5, "开黑"));
        assert_eq!(find_channel(&roster, 5, "开黑"), Some(5));
    }

    /// 服务器重启过：频道还在，但 id 重新分了。
    #[test]
    fn falls_back_to_a_unique_name() {
        let mut roster = Roster::default();
        roster.channels.insert(1, channel(1, "大厅"));
        roster.channels.insert(5, channel(5, "别的频道"));
        roster.channels.insert(7, channel(7, "开黑"));
        assert_eq!(find_channel(&roster, 5, "开黑"), Some(7));
    }

    #[test]
    fn does_not_guess_between_duplicate_names() {
        let mut roster = Roster::default();
        roster.channels.insert(1, channel(1, "大厅"));
        roster.channels.insert(6, channel(6, "开黑"));
        roster.channels.insert(7, channel(7, "开黑"));
        assert_eq!(find_channel(&roster, 5, "开黑"), None);
    }

    #[test]
    fn gives_up_when_the_channel_is_gone() {
        let mut roster = Roster::default();
        roster.channels.insert(1, channel(1, "大厅"));
        assert_eq!(find_channel(&roster, 5, "开黑"), None);
    }
}

#[cfg(test)]
mod shutdown_tests {
    use super::*;

    fn connection(name: &str) -> (Client, Receiver<Event>) {
        use server::conn::Hub;
        use server::state::{Config, Server};
        use std::net::{TcpListener, UdpSocket};
        use transport::{server_config, ServerCert};
        let cert = ServerCert::generate().unwrap();
        let listener = TcpListener::bind("127.0.0.1:0").unwrap();
        let addr = listener.local_addr().unwrap();
        let invite = Invite {
            host: addr.ip().to_string(),
            port: addr.port(),
            cert: cert.fingerprint(),
            code: None,
        };
        let tls = Arc::new(server_config(&cert).unwrap());
        let hub = Arc::new(Hub::new(
            Server::new(Config::default()),
            UdpSocket::bind("127.0.0.1:0").unwrap(),
        ));
        std::thread::spawn(move || server::accept_loop(listener, tls, hub));
        Client::connect(
            &invite.to_url().unwrap(),
            &Identity::generate().unwrap(),
            name,
        )
        .unwrap()
    }

    fn wait_for(description: &str, predicate: impl Fn() -> bool) {
        let deadline = Instant::now() + Duration::from_secs(3);
        while !predicate() {
            assert!(
                Instant::now() < deadline,
                "timed out waiting for {description}"
            );
            std::thread::sleep(Duration::from_millis(1));
        }
    }

    #[test]
    fn self_intent_and_fifo_commands_do_not_wait_for_a_blocked_tls_writer() {
        let (client, events) = connection("queued-controls");
        let link = client.shared.link();
        let guard = link.wire.lock().unwrap();
        let commands = client.clone();
        let (finished, done) = mpsc::channel();
        let gui = std::thread::spawn(move || {
            commands.set_self_state(true, true);
            commands.send_text("first queued command");
            commands.set_self_state(false, false);
            commands.send_text("second queued command");
            commands.set_self_state(true, false);
            finished.send(()).unwrap();
        });
        let immediate = done.recv_timeout(Duration::from_millis(500));
        let desired = immediate
            .is_ok()
            .then(|| *client.shared.self_state.lock().unwrap());
        // Always release the lock before asserting so a regression cannot leave
        // a stuck writer/API test thread behind.
        drop(guard);
        gui.join().unwrap();
        assert!(
            immediate.is_ok(),
            "GUI command waited for the TLS writer lock"
        );
        assert_eq!(
            desired,
            Some((true, false)),
            "latest intent was not saved before returning"
        );

        let mut texts = Vec::new();
        let deadline = Instant::now() + Duration::from_secs(3);
        while texts.len() < 2 {
            let event = events
                .recv_timeout(deadline.saturating_duration_since(Instant::now()))
                .unwrap();
            if let Event::Text(line) = event {
                texts.push(line.body);
            }
        }
        assert_eq!(texts, ["first queued command", "second queued command"]);
        wait_for("final self-state acknowledgement", || {
            client
                .roster()
                .my_user()
                .is_some_and(|me| me.self_muted && !me.self_deafened)
        });
        let weak_writer = Arc::downgrade(&link.writer_state);
        client.disconnect();
        wait_for("writer shutdown", || {
            link.writer_state.completed.load(Ordering::Acquire)
        });
        assert_eq!(link.writer_state.pending_bytes.load(Ordering::Acquire), 0);
        drop(link);
        drop(client);
        wait_for("writer ownership released", || {
            weak_writer.upgrade().is_none()
        });
    }

    #[test]
    fn a_full_control_queue_interrupts_the_link_and_preserves_latest_intent() {
        let (client, _events) = connection("bounded-controls");
        let link = client.shared.link();
        let guard = link.wire.lock().unwrap();
        // Worker may have taken the first message before blocking on the wire.
        // Two messages beyond capacity therefore guarantee overflow.
        for i in 0..WRITE_QUEUE_MESSAGES + 2 {
            client.set_self_state(i % 2 == 0, false);
        }
        client.set_self_state(true, true);
        assert!(link.writer_state.stopped.load(Ordering::Acquire));
        assert!(link
            .writer_state
            .error
            .lock()
            .unwrap()
            .as_ref()
            .is_some_and(|e| e.contains("队列")));
        assert_eq!(*client.shared.self_state.lock().unwrap(), (true, true));
        assert!(link.writer_state.pending_bytes.load(Ordering::Acquire) <= WRITE_QUEUE_BYTES);
        client.disconnect();
        drop(guard);
        wait_for("overflow writer shutdown", || {
            link.writer_state.completed.load(Ordering::Acquire)
        });
        assert_eq!(link.writer_state.pending_bytes.load(Ordering::Acquire), 0);
    }

    #[test]
    fn queued_large_control_messages_also_have_a_total_byte_budget() {
        let (client, _events) = connection("byte-budget");
        let link = client.shared.link();
        let guard = link.wire.lock().unwrap();
        let text = "x".repeat(protocol::control::MAX_FRAME_BODY / 2);
        for _ in 0..10 {
            client.send_text(&text);
        }
        assert!(link.writer_state.stopped.load(Ordering::Acquire));
        assert!(link
            .writer_state
            .error
            .lock()
            .unwrap()
            .as_ref()
            .is_some_and(|e| e.contains("积压")));
        assert!(link.writer_state.pending_bytes.load(Ordering::Acquire) <= WRITE_QUEUE_BYTES);
        client.disconnect();
        drop(guard);
        wait_for("byte-budget writer shutdown", || {
            link.writer_state.completed.load(Ordering::Acquire)
        });
        assert_eq!(link.writer_state.pending_bytes.load(Ordering::Acquire), 0);
    }

    #[test]
    fn cancelling_does_not_wait_for_the_tls_write_lock() {
        use server::conn::Hub;
        use server::state::{Config, Server};
        use std::net::{TcpListener, UdpSocket};
        use transport::{server_config, ServerCert};
        let cert = ServerCert::generate().unwrap();
        let listener = TcpListener::bind("127.0.0.1:0").unwrap();
        let addr = listener.local_addr().unwrap();
        let invite = Invite {
            host: addr.ip().to_string(),
            port: addr.port(),
            cert: cert.fingerprint(),
            code: None,
        };
        let tls = Arc::new(server_config(&cert).unwrap());
        let hub = Arc::new(Hub::new(
            Server::new(Config::default()),
            UdpSocket::bind("127.0.0.1:0").unwrap(),
        ));
        std::thread::spawn(move || server::accept_loop(listener, tls, hub));
        let (client, _) = Client::connect(
            &invite.to_url().unwrap(),
            &Identity::generate().unwrap(),
            "shutdown",
        )
        .unwrap();
        let link = client.shared.link();
        let guard = link.wire.lock().unwrap();
        let (tx, rx) = mpsc::channel();
        let worker = std::thread::spawn(move || {
            client.disconnect();
            tx.send(()).unwrap();
        });
        let result = rx.recv_timeout(Duration::from_millis(500));
        drop(guard); // Also releases a broken implementation so the test never leaves a stuck thread.
        worker.join().unwrap();
        assert!(result.is_ok(), "disconnect waited for the TLS writer lock");
    }
}
