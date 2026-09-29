// SPDX-License-Identifier: MPL-2.0

//! 服务端状态：谁在线、有哪些频道、谁在哪。
//!
//! # 这里没有任何 IO
//!
//! 所有方法都是「改状态 + 返回要广播什么」，不碰 socket、不碰时钟、不碰数据库。
//! 好处是这一整套规则可以用普通的单元测试盖满 —— 而规则正是最容易出错的地方
//! （重名、顶号、角色不够、频道满了）。
//!
//! 连接、TLS、线程都在 `conn` 里，那边只负责把字节搬进搬出。

use std::collections::BTreeMap;

use protocol::control::{
    goodbye, rejected, user_left, BanList, BannedUser, Channel, ChannelState, CreateChannel,
    EditChannel, Rejected, Role, ServerMessage, TextMessage, User, UserLeft, UserState, Welcome,
};
use protocol::PublicKey;

pub type SessionId = u32;
pub type ChannelId = u32;

/// 文字消息的长度上限（字节）。
///
/// 场景是发个链接报个坐标，不是写文章。限长同时也是防滥用 ——
/// 一条消息会被扇出给频道里所有人。
pub const MAX_TEXT_BYTES: usize = 2000;

/// 昵称长度上限（字节）。
pub const MAX_NAME_BYTES: usize = 64;

/// 频道名长度上限（字节）。
pub const MAX_CHANNEL_NAME_BYTES: usize = 64;

/// 频道说明长度上限（字节）。
pub const MAX_CHANNEL_DESCRIPTION_BYTES: usize = 200;

/// 一个服务器最多几个频道。
///
/// 成员就能建频道，所以这是个防滥用的闸 —— 没有它，一个人可以刷满内存，
/// 而且每建一个都要给所有人广播一次。
///
/// 128 对「3–20 人的朋友或公会」是绰绰有余的上限，同时又小到刷不出问题。
pub const MAX_CHANNELS: usize = 128;

/// 建频道要的最低角色。
///
/// 访客不行 —— 没有邀请码进来的人默认就是访客，让他们能建频道等于把
/// 防滥用的闸打开了。`Role` 的注释里写的就是「成员：能建临时频道」。
const MIN_ROLE_TO_CREATE_CHANNEL: Role = Role::Member;

/// 删、改别人建的频道要的最低角色。**建的人自己不受这条限制。**
const MIN_ROLE_TO_DELETE_OTHERS_CHANNEL: Role = Role::ChannelAdmin;

/// 踢人要的最低角色。`Role` 的注释里写的就是「频道管理：能踢人」。
const MIN_ROLE_TO_KICK: Role = Role::ChannelAdmin;

/// 踢人、封禁时附带的理由的长度上限（字节）。
pub const MAX_REASON_BYTES: usize = 200;

/// 角色的高低。**只拿来比较**，别的地方不要直接比 `as i32`。
fn rank(role: Role) -> i32 {
    role as i32
}

/// 要发给谁。
#[derive(Debug, Clone, PartialEq)]
pub enum Broadcast {
    /// 发给所有已登录的人。
    Everyone(ServerMessage),
    /// 发给除了这个人之外的所有已登录的人。
    Others(SessionId, ServerMessage),
    /// 只发给某个频道里的人。
    Channel(ChannelId, ServerMessage),
    /// 只发给一个人。
    One(SessionId, ServerMessage),
    /// 断开这个人的连接，断之前先用 `Goodbye` 告诉他为什么。
    ///
    /// 状态里已经把他摘掉了，连接层只管关 socket。**排在同一批里别的广播前面**：
    /// 先让他收到「你被踢了」，再让别人收到「他走了」。
    Kick {
        session: SessionId,
        reason: goodbye::Reason,
        detail: String,
    },
}

/// 状态机里「重启之后也该在」的那些变化。连接层每次改完状态就取走，
/// 交给存档写盘（见 `store` 和 `conn::Hub::mutate`）。
///
/// 状态机自己不碰存档，只是把变化记在这里 —— 跟 [`Broadcast`] 是同一个思路：
/// 规则在这里，IO 在外面。
#[derive(Debug, Clone, PartialEq)]
pub enum Change {
    /// 建了或者改了（改名、挪父频道）一个频道。存的是改完之后的完整样子。
    ChannelSaved(Channel),
    ChannelRemoved(ChannelId),
    /// 某个公钥的角色定下来了。之后他再进来就是这个角色，不看邀请码。
    RoleSet(PublicKey, Role),
    BanAdded(BanEntry),
    BanRemoved(PublicKey),
    /// 管理员链接被用掉了，作废。
    AdminClaimUsed,
}

/// 封禁名单里的一条。
#[derive(Debug, Clone, PartialEq)]
pub struct BanEntry {
    pub public_key: PublicKey,
    /// 被封时叫什么。只是让管理员认得出是谁，不参与任何判断。
    pub name: String,
    pub banned_at_ms: i64,
    /// 谁封的（当时的昵称）。
    pub banned_by: String,
    pub reason: String,
}

impl BanEntry {
    fn to_wire(&self) -> BannedUser {
        BannedUser {
            public_key: self.public_key.0.to_vec(),
            name: self.name.clone(),
            banned_at_ms: self.banned_at_ms,
            banned_by: self.banned_by.clone(),
            reason: self.reason.clone(),
        }
    }
}

#[derive(Debug, Clone)]
pub struct Config {
    /// 在线人数上限。
    pub max_users: usize,
    /// 要不要邀请码才能进。
    pub require_invite: bool,
    /// 当前有效的邀请码。`None` 表示没设。
    pub invite_code: Option<String>,
    /// 管理员的公钥。身份就是公钥，所以白名单也是公钥。
    pub admin_keys: Vec<PublicKey>,
}

impl Default for Config {
    fn default() -> Self {
        Self {
            max_users: 50,
            require_invite: false,
            invite_code: None,
            admin_keys: Vec::new(),
        }
    }
}

/// 服务端自己记的频道信息。
///
/// 包着线上的 [`Channel`]，外面那层是**不上线**的东西 —— 跟 [`UserInfo`]
/// 包 [`User`] 是同一个模式。
#[derive(Debug, Clone)]
struct ChannelInfo {
    wire: Channel,
    /// 谁建的。`None` = 服务器自带的根频道，谁都删不掉。
    ///
    /// 记公钥不记 session：session 一断线就没了，而「我建的频道」这件事
    /// 应该在重连之后还成立。
    created_by: Option<PublicKey>,
}

#[derive(Debug, Clone)]
struct UserInfo {
    session_id: SessionId,
    public_key: PublicKey,
    name: String,
    channel_id: ChannelId,
    role: Role,
    self_muted: bool,
    self_deafened: bool,
    server_muted: bool,
}

impl UserInfo {
    fn to_wire(&self) -> User {
        User {
            session_id: self.session_id,
            public_key: self.public_key.0.to_vec(),
            name: self.name.clone(),
            channel_id: self.channel_id,
            role: self.role as i32,
            self_muted: self.self_muted,
            self_deafened: self.self_deafened,
            server_muted: self.server_muted,
        }
    }
}

/// 认证被拒的原因。转成线上的 `Rejected` 再发出去。
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Denied {
    InviteRequired,
    Full,
    Banned,
}

impl Denied {
    pub fn to_wire(&self) -> Rejected {
        let (reason, detail) = match self {
            Denied::InviteRequired => (
                rejected::Reason::InviteRequired,
                "这个服务器要邀请码才能进，找管理员要一条邀请链接",
            ),
            Denied::Full => (rejected::Reason::Full, "服务器满了，等会儿再试"),
            Denied::Banned => (rejected::Reason::Banned, "你被这个服务器封了"),
        };
        Rejected {
            reason: reason as i32,
            detail: detail.to_string(),
        }
    }
}

/// 认证成功后返回给连接层的东西。
///
/// 里面没有秘密（公钥、会话 id、频道树都是公开信息），所以可以放心 derive Debug。
#[derive(Debug, PartialEq)]
pub struct Admitted {
    pub session_id: SessionId,
    /// 只发给这个人的 Welcome。
    pub welcome: Welcome,
    /// 要广播给别人的（新人进来了）。
    pub broadcasts: Vec<Broadcast>,
    /// 被顶掉的旧会话 —— 连接层要把那条连接关掉。
    pub displaced: Option<SessionId>,
}

/// 从存档里恢复出来的东西，见 `store` 模块。
///
/// 这里只放「重启之后还该在」的：频道树和发号游标。在线的人、会话、
/// 文字消息都不在里面 —— 那些本来就是一断就没的。
#[derive(Debug, Clone, Default, PartialEq)]
pub struct Saved {
    pub channels: Vec<Channel>,
    /// 下一个要发的频道 id。0 表示存档里没记。
    pub next_channel_id: ChannelId,
    /// 按公钥定下来的角色。
    pub roles: Vec<(PublicKey, Role)>,
    pub bans: Vec<BanEntry>,
    /// 还没被用掉的管理员链接里的码。`None` = 没有（已经有管理员了，或者从没发过）。
    pub admin_claim: Option<String>,
}

pub struct Server {
    config: Config,
    channels: BTreeMap<ChannelId, ChannelInfo>,
    users: BTreeMap<SessionId, UserInfo>,
    next_session: SessionId,
    root: ChannelId,
    /// 下一个要发的频道 id。**只增不减**，见 [`Server::next_channel_id`]。
    next_channel: ChannelId,
    /// 按公钥定下来的角色。有这一条的人进来时就是这个角色，不看邀请码 ——
    /// 被管理员提成成员的人，不该因为手里是一条旧链接就变回访客。
    granted: BTreeMap<PublicKey, Role>,
    bans: BTreeMap<PublicKey, BanEntry>,
    /// 管理员链接里的码。用它进来的第一个人成为管理员，然后作废。
    admin_claim: Option<String>,
    /// 还没被存档取走的变化，见 [`Change`]。
    changes: Vec<Change>,
}

impl Server {
    pub fn new(config: Config) -> Self {
        Self::restore(config, Saved::default())
    }

    /// 带着存档起来。
    ///
    /// **存档不可信**：它是个文件，会被手改、会坏、会是旧版本写的。所以这里
    /// 什么都兜着 —— 父频道不存在的挂到根上，成环的拆开挂到根上，名字空的
    /// 丢掉，多出 [`MAX_CHANNELS`] 的丢掉。宁可少恢复几个频道，也不能起来一个
    /// 规则被破坏的状态机：后面所有代码都假设频道树是一棵树。
    pub fn restore(config: Config, saved: Saved) -> Self {
        let mut server = Self::fresh(config);
        let root = server.root;

        let mut next = saved.next_channel_id;
        for channel in saved.channels {
            next = next.max(channel.id.wrapping_add(1));
            if channel.id == 0 || channel.name.trim().is_empty() {
                continue;
            }
            if channel.id == root {
                // 根频道是结构性的：父指向自己、谁都删不掉。只认存档里的
                // 名字和说明（将来能改名），别的一律按根频道的规矩来。
                let info = server.channels.get_mut(&root).expect("fresh 建了根");
                info.wire.name = channel.name;
                info.wire.description = channel.description;
                continue;
            }
            if server.channels.len() >= MAX_CHANNELS {
                break;
            }
            let created_by = <[u8; PublicKey::LEN]>::try_from(channel.created_by.as_slice())
                .ok()
                .map(PublicKey);
            let wire = Channel {
                created_by: created_by.map(|k| k.0.to_vec()).unwrap_or_default(),
                ..channel
            };
            server
                .channels
                .insert(wire.id, ChannelInfo { wire, created_by });
        }

        // 父频道不在、或者往上走回不到根（成环）的，挂到根上。
        let ids: Vec<ChannelId> = server.channels.keys().copied().collect();
        for id in ids {
            if id != root && !server.reaches_root(id) {
                server
                    .channels
                    .get_mut(&id)
                    .expect("刚列出来")
                    .wire
                    .parent_id = root;
            }
        }

        server.next_channel = next.max(root + 1);

        for (key, role) in saved.roles {
            // 存档里的 Unspecified 当成没记：不能凭一行坏数据给人一个奇怪的角色。
            if role != Role::Unspecified {
                server.granted.insert(key, role);
            }
        }
        for entry in saved.bans {
            server.bans.insert(entry.public_key, entry);
        }
        server.admin_claim = saved.admin_claim.filter(|code| !code.trim().is_empty());
        server
    }

    /// 取走还没落盘的变化。连接层每改一次状态就调一次。
    pub fn take_changes(&mut self) -> Vec<Change> {
        std::mem::take(&mut self.changes)
    }

    /// 有没有管理员：配置里写的，或者存档里定过的。
    ///
    /// 没有的话服务端启动时会发一条管理员链接，见 `main.rs`。
    pub fn has_admin(&self) -> bool {
        !self.config.admin_keys.is_empty() || self.granted.values().any(|r| *r == Role::Admin)
    }

    /// 还没被用掉的管理员链接里的码。
    pub fn admin_claim(&self) -> Option<&str> {
        self.admin_claim.as_deref()
    }

    /// 从 `id` 沿着父频道往上走，能不能走到根。
    fn reaches_root(&self, id: ChannelId) -> bool {
        let mut current = id;
        // 最多走频道总数那么多步：比这多就一定是在转圈。
        for _ in 0..=self.channels.len() {
            if current == self.root {
                return true;
            }
            match self.channels.get(&current) {
                Some(c) if c.wire.parent_id != current => current = c.wire.parent_id,
                _ => return false,
            }
        }
        false
    }

    /// 只有一个根频道的全新服务器。
    fn fresh(config: Config) -> Self {
        let root = 1;
        let mut channels = BTreeMap::new();
        channels.insert(
            root,
            ChannelInfo {
                wire: Channel {
                    id: root,
                    // 根频道的 parent 指向自己 —— 这样遍历树时不需要特判 Option。
                    parent_id: root,
                    name: "大厅".to_string(),
                    description: "默认频道".to_string(),
                    max_users: 0,
                    min_role: Role::Guest as i32,
                    // 空 = 服务器自带，谁都删不掉。
                    created_by: Vec::new(),
                },
                created_by: None,
            },
        );
        Self {
            config,
            channels,
            users: BTreeMap::new(),
            next_session: 1,
            root,
            // root 占了 1。
            next_channel: root + 1,
            granted: BTreeMap::new(),
            bans: BTreeMap::new(),
            admin_claim: None,
            changes: Vec::new(),
        }
    }

    pub fn root_channel(&self) -> ChannelId {
        self.root
    }

    pub fn user_count(&self) -> usize {
        self.users.len()
    }

    pub fn channel_of(&self, session: SessionId) -> Option<ChannelId> {
        self.users.get(&session).map(|u| u.channel_id)
    }

    pub fn public_key_of(&self, session: SessionId) -> Option<PublicKey> {
        self.users.get(&session).map(|u| u.public_key)
    }

    /// 频道里还有谁。语音转发要用它决定往哪儿发。
    pub fn sessions_in_channel(&self, channel: ChannelId) -> Vec<SessionId> {
        self.users
            .values()
            .filter(|u| u.channel_id == channel)
            .map(|u| u.session_id)
            .collect()
    }

    pub fn all_sessions(&self) -> Vec<SessionId> {
        self.users.keys().copied().collect()
    }

    /// 签名已经验过了，这里只做「准不准进」和「进来之后叫什么」。
    ///
    /// **验签名不在这里**：那是密码学，属于连接层；这里只管策略。
    pub fn admit(
        &mut self,
        public_key: PublicKey,
        invite_code: &str,
        desired_name: &str,
    ) -> Result<Admitted, Denied> {
        let configured_admin = self.config.admin_keys.contains(&public_key);
        if self.bans.contains_key(&public_key) && !configured_admin {
            return Err(Denied::Banned);
        }

        // 用管理员链接进来的：成为管理员，链接作废。**先比码再看别的** ——
        // 服务器要邀请码的话，管理员链接里的码本身就是通行证。
        let claimed = !invite_code.is_empty() && self.admin_claim.as_deref() == Some(invite_code);

        let role = if configured_admin || claimed {
            Role::Admin
        } else if let Some(role) = self.granted.get(&public_key) {
            *role
        } else {
            match self.check_invite(invite_code) {
                Some(role) => role,
                None => return Err(Denied::InviteRequired),
            }
        };
        // 管理员不受人数限制约束 —— 否则服务器满了管理员就进不去处理问题了，
        // 而那正是最需要管理员的时候。
        let is_admin = role == Role::Admin;

        // 同一个公钥再连一次 = 顶号。
        //
        // 换机器、断线重连、崩溃后重启都会走到这里。留着旧会话的话，用户会看到
        // 一个自己的幽灵挂在频道里，而且语音会往那个死连接发。顶掉更符合直觉。
        let displaced = self
            .users
            .values()
            .find(|u| u.public_key == public_key)
            .map(|u| u.session_id);
        let mut broadcasts = Vec::new();
        if let Some(old) = displaced {
            broadcasts.extend(self.remove_user(old, user_left::Reason::Disconnected));
        }

        if !is_admin && self.users.len() >= self.config.max_users {
            return Err(Denied::Full);
        }

        if claimed {
            self.admin_claim = None;
            self.granted.insert(public_key, Role::Admin);
            self.changes.push(Change::RoleSet(public_key, Role::Admin));
            self.changes.push(Change::AdminClaimUsed);
        }

        let session_id = self.next_session;
        self.next_session = self.next_session.wrapping_add(1).max(1);

        let name = self.unique_name(desired_name);
        let user = UserInfo {
            session_id,
            public_key,
            name,
            channel_id: self.root,
            role,
            self_muted: false,
            self_deafened: false,
            server_muted: false,
        };
        let wire = user.to_wire();
        self.users.insert(session_id, user);

        // 把新人告诉别人。他自己不用：Welcome 里已经有他了，
        // 再发一遍只会让他刚登录就多收一条没用的 UserState。
        broadcasts.push(Broadcast::Others(
            session_id,
            UserState { user: Some(wire) }.into(),
        ));
        if is_admin {
            broadcasts.push(Broadcast::One(session_id, self.ban_list()));
        }

        Ok(Admitted {
            session_id,
            welcome: self.welcome_for(session_id),
            broadcasts,
            displaced,
        })
    }

    /// 邀请码策略。返回准进来时该给的角色。
    fn check_invite(&self, provided: &str) -> Option<Role> {
        match (&self.config.invite_code, self.config.require_invite) {
            // 设了邀请码：对上了是成员，对不上就看要不要强制
            (Some(expected), require) => {
                if !provided.is_empty() && provided == expected {
                    Some(Role::Member)
                } else if require {
                    None
                } else {
                    Some(Role::Guest)
                }
            }
            // 没设邀请码但要求必须有 —— 配置矛盾，一律不放，
            // 总比「以为设了限制其实谁都能进」强。
            (None, true) => None,
            (None, false) => Some(Role::Member),
        }
    }

    /// 重名就加后缀。
    ///
    /// 直接拒绝重名对用户很不友好（「你队友先进来了所以你不能叫这个名字」），
    /// 而昵称本来就不是身份 —— 身份是公钥。
    fn unique_name(&self, desired: &str) -> String {
        let base = sanitize_name(desired);
        if !self.users.values().any(|u| u.name == base) {
            return base;
        }
        for n in 2..1000u32 {
            let candidate = format!("{base}{n}");
            if !self.users.values().any(|u| u.name == candidate) {
                return candidate;
            }
        }
        // 一千个同名，随它去 —— 昵称不是身份，撞了也不影响正确性。
        base
    }

    pub fn welcome_for(&self, session: SessionId) -> Welcome {
        let me = self.users.get(&session);
        Welcome {
            session_id: session,
            name: me.map(|u| u.name.clone()).unwrap_or_default(),
            role: me.map(|u| u.role as i32).unwrap_or(Role::Guest as i32),
            // 连接层填，它才知道 UDP 监听在哪个端口
            udp_port: 0,
            channels: self.channels.values().map(|c| c.wire.clone()).collect(),
            users: self.users.values().map(|u| u.to_wire()).collect(),
            current_channel_id: me.map(|u| u.channel_id).unwrap_or(self.root),
        }
    }

    pub fn disconnect(&mut self, session: SessionId) -> Vec<Broadcast> {
        self.remove_user(session, user_left::Reason::Disconnected)
    }

    pub fn timeout(&mut self, session: SessionId) -> Vec<Broadcast> {
        self.remove_user(session, user_left::Reason::Timeout)
    }

    fn remove_user(&mut self, session: SessionId, reason: user_left::Reason) -> Vec<Broadcast> {
        if self.users.remove(&session).is_none() {
            return Vec::new();
        }
        vec![Broadcast::Everyone(
            UserLeft {
                session_id: session,
                reason: reason as i32,
            }
            .into(),
        )]
    }

    pub fn join_channel(&mut self, session: SessionId, target: ChannelId) -> Vec<Broadcast> {
        let Some(channel) = self.channels.get(&target).map(|c| &c.wire) else {
            // 频道不存在：忽略。客户端的频道树可能刚好过时了一点，
            // 为这个断开连接太粗暴。
            return Vec::new();
        };
        let Some(user) = self.users.get(&session) else {
            return Vec::new();
        };
        if user.channel_id == target {
            return Vec::new();
        }
        // 角色不够：忽略。客户端本来就该把进不去的频道画成灰的。
        if (user.role as i32) < channel.min_role {
            return Vec::new();
        }
        if channel.max_users > 0 {
            let occupancy = self.sessions_in_channel(target).len();
            if occupancy >= channel.max_users as usize {
                return Vec::new();
            }
        }

        let user = self.users.get_mut(&session).expect("just checked");
        user.channel_id = target;
        let wire = user.to_wire();
        vec![Broadcast::Everyone(UserState { user: Some(wire) }.into())]
    }

    /// 建一个频道。
    ///
    /// 返回空的广播列表 = 没建成。**不给客户端回错误**，跟 [`Server::join_channel`]
    /// 一个路子：客户端本来就该按自己的角色把界面画对（建不了就别显示那个按钮），
    /// 走到这里还被拒说明要么是客户端有 bug、要么是有人在手搓协议 ——
    /// 这两种都不值得为它设计一条错误消息。
    ///
    /// 唯一的例外是建成了：那会广播一条 `ChannelState` 给**所有人**，
    /// 包括建的人自己。客户端因此不需要本地先插一个再等确认。
    pub fn create_channel(&mut self, session: SessionId, req: CreateChannel) -> Vec<Broadcast> {
        // 只抄走需要的两个值，别一路攥着 &self.users —— 下面要 &mut self。
        let Some((creator_role, creator_key)) =
            self.users.get(&session).map(|u| (u.role, u.public_key))
        else {
            return Vec::new();
        };
        if (creator_role as i32) < (MIN_ROLE_TO_CREATE_CHANNEL as i32) {
            return Vec::new();
        }
        if self.channels.len() >= MAX_CHANNELS {
            return Vec::new();
        }

        let name = req.name.trim();
        if name.is_empty() || name.len() > MAX_CHANNEL_NAME_BYTES {
            return Vec::new();
        }
        let description = req.description.trim();
        if description.len() > MAX_CHANNEL_DESCRIPTION_BYTES {
            return Vec::new();
        }

        // 不认识的父频道一律挂到根上。**不是拒绝** —— 客户端的频道树可能刚好
        // 过时了一点（比如父频道刚被别人删了），为这个让建频道失败没道理。
        let parent_id = if self.channels.contains_key(&req.parent_id) {
            req.parent_id
        } else {
            self.root
        };

        // 建频道的人不能造出一个自己都进不去的频道 —— 那是纯粹的误操作，
        // 而且它会立刻变成一个谁也删不掉、谁也进不去的僵尸（删要么是本人
        // 要么是频道管理，而本人进不去就不会想起来删）。
        let min_role = Role::try_from(req.min_role).unwrap_or(Role::Guest);
        let min_role = if (min_role as i32) > (creator_role as i32) {
            creator_role
        } else {
            min_role
        };

        let id = self.next_channel_id();
        let wire = Channel {
            id,
            parent_id,
            name: name.to_string(),
            description: description.to_string(),
            max_users: req.max_users,
            min_role: min_role as i32,
            created_by: creator_key.0.to_vec(),
        };
        self.channels.insert(
            id,
            ChannelInfo {
                wire: wire.clone(),
                created_by: Some(creator_key),
            },
        );
        self.changes.push(Change::ChannelSaved(wire.clone()));

        vec![Broadcast::Everyone(
            ChannelState {
                channel: Some(wire),
                removed: false,
            }
            .into(),
        )]
    }

    /// 删一个频道。
    ///
    /// 里面的人挪回根频道，子频道上提到根频道 —— **不能把人留在一个不存在的
    /// 频道里**，那样他们的语音会被转发到没人收的地方，而界面上看不出问题。
    ///
    /// 广播顺序是先挪人和子频道、最后才是删除本身，这样客户端在任何一个
    /// 中间状态下都不会引用到一个已经消失的频道 id。
    pub fn delete_channel(&mut self, session: SessionId, target: ChannelId) -> Vec<Broadcast> {
        // 根频道删不掉。它的 created_by 是 None，下面的判断本来也会拦住，
        // 但这条写在最前面是因为理由不一样：根频道是结构性的，不是权限问题。
        if target == self.root {
            return Vec::new();
        }
        let Some(channel) = self.channels.get(&target) else {
            return Vec::new();
        };
        let Some(user) = self.users.get(&session) else {
            return Vec::new();
        };

        let mine = channel.created_by == Some(user.public_key);
        let senior = (user.role as i32) >= (MIN_ROLE_TO_DELETE_OTHERS_CHANNEL as i32);
        if !mine && !senior {
            return Vec::new();
        }

        let mut out = Vec::new();

        // 子频道上提到根。
        let orphans: Vec<ChannelId> = self
            .channels
            .values()
            .filter(|c| c.wire.parent_id == target && c.wire.id != target)
            .map(|c| c.wire.id)
            .collect();
        for id in orphans {
            let info = self.channels.get_mut(&id).expect("just listed");
            info.wire.parent_id = self.root;
            self.changes.push(Change::ChannelSaved(info.wire.clone()));
            out.push(Broadcast::Everyone(
                ChannelState {
                    channel: Some(info.wire.clone()),
                    removed: false,
                }
                .into(),
            ));
        }

        // 里面的人挪回根频道。
        let stranded = self.sessions_in_channel(target);
        for id in stranded {
            let user = self.users.get_mut(&id).expect("just listed");
            user.channel_id = self.root;
            out.push(Broadcast::Everyone(
                UserState {
                    user: Some(user.to_wire()),
                }
                .into(),
            ));
        }

        let removed = self.channels.remove(&target).expect("just checked");
        self.changes.push(Change::ChannelRemoved(target));
        out.push(Broadcast::Everyone(
            ChannelState {
                channel: Some(removed.wire),
                removed: true,
            }
            .into(),
        ));
        out
    }

    /// 改一个频道：名字、说明、父频道。见 `EditChannel` 的协议注释。
    ///
    /// 跟建频道一样，被拒就是什么都不发。名字不合法整条拒掉；父频道不合法
    /// （不存在、是自己、是自己的子孙）只是不挪，名字照改 —— 客户端手里的
    /// 频道树可能刚好过时，不该因为这个连改名都失败。
    pub fn edit_channel(&mut self, session: SessionId, req: EditChannel) -> Vec<Broadcast> {
        let target = req.channel_id;
        let Some(user) = self.users.get(&session) else {
            return Vec::new();
        };
        let Some(channel) = self.channels.get(&target) else {
            return Vec::new();
        };
        let allowed = if target == self.root {
            user.role == Role::Admin
        } else {
            channel.created_by == Some(user.public_key)
                || rank(user.role) >= rank(MIN_ROLE_TO_DELETE_OTHERS_CHANNEL)
        };
        if !allowed {
            return Vec::new();
        }

        let name = req.name.trim();
        if name.is_empty() || name.len() > MAX_CHANNEL_NAME_BYTES {
            return Vec::new();
        }
        let description = req.description.trim();
        if description.len() > MAX_CHANNEL_DESCRIPTION_BYTES {
            return Vec::new();
        }

        let current_parent = channel.wire.parent_id;
        let parent_id = if target == self.root {
            self.root
        } else if req.parent_id != target
            && self.channels.contains_key(&req.parent_id)
            && !self.is_under(req.parent_id, target)
        {
            req.parent_id
        } else {
            current_parent
        };

        let info = self.channels.get_mut(&target).expect("just checked");
        if info.wire.name == name
            && info.wire.description == description
            && info.wire.parent_id == parent_id
        {
            return Vec::new();
        }
        info.wire.name = name.to_string();
        info.wire.description = description.to_string();
        info.wire.parent_id = parent_id;
        let wire = info.wire.clone();
        self.changes.push(Change::ChannelSaved(wire.clone()));
        vec![Broadcast::Everyone(
            ChannelState {
                channel: Some(wire),
                removed: false,
            }
            .into(),
        )]
    }

    /// `candidate` 是不是 `ancestor` 自己或者它的子孙。挪频道时防成环用。
    fn is_under(&self, candidate: ChannelId, ancestor: ChannelId) -> bool {
        let mut current = candidate;
        for _ in 0..=self.channels.len() {
            if current == ancestor {
                return true;
            }
            match self.channels.get(&current) {
                Some(c) if c.wire.parent_id != current => current = c.wire.parent_id,
                _ => return false,
            }
        }
        // 走了这么多步还没到头 = 已经在转圈了。当成「是」，拒掉这次挪动。
        true
    }

    /// 踢人。见 `KickUser` 的协议注释。
    pub fn kick(&mut self, session: SessionId, target: SessionId, reason: &str) -> Vec<Broadcast> {
        let (Some(actor), Some(victim)) = (self.users.get(&session), self.users.get(&target))
        else {
            return Vec::new();
        };
        if session == target
            || rank(actor.role) < rank(MIN_ROLE_TO_KICK)
            || rank(victim.role) >= rank(actor.role)
        {
            return Vec::new();
        }
        let detail = with_reason(format!("{} 把你请出了服务器", actor.name), reason);
        let mut out = vec![Broadcast::Kick {
            session: target,
            reason: goodbye::Reason::Kicked,
            detail,
        }];
        out.extend(self.remove_user(target, user_left::Reason::Kicked));
        out
    }

    /// 封禁。见 `BanUser` 的协议注释。`now_ms` 由连接层给 —— 这里不碰时钟。
    pub fn ban(
        &mut self,
        session: SessionId,
        target: SessionId,
        reason: &str,
        now_ms: i64,
    ) -> Vec<Broadcast> {
        let (Some(actor), Some(victim)) = (self.users.get(&session), self.users.get(&target))
        else {
            return Vec::new();
        };
        if session == target || actor.role != Role::Admin || rank(victim.role) >= rank(actor.role) {
            return Vec::new();
        }
        let mut reason = reason.trim().to_string();
        truncate_utf8(&mut reason, MAX_REASON_BYTES);
        let entry = BanEntry {
            public_key: victim.public_key,
            name: victim.name.clone(),
            banned_at_ms: now_ms,
            banned_by: actor.name.clone(),
            reason: reason.clone(),
        };
        let detail = with_reason(format!("{} 封禁了你", actor.name), &reason);

        self.bans.insert(entry.public_key, entry.clone());
        self.changes.push(Change::BanAdded(entry));

        let mut out = vec![Broadcast::Kick {
            session: target,
            reason: goodbye::Reason::Banned,
            detail,
        }];
        out.extend(self.remove_user(target, user_left::Reason::Banned));
        out.extend(self.ban_list_to_admins());
        out
    }

    /// 解封。只有管理员能解。
    pub fn unban(&mut self, session: SessionId, public_key: &[u8]) -> Vec<Broadcast> {
        let Some(actor) = self.users.get(&session) else {
            return Vec::new();
        };
        if actor.role != Role::Admin {
            return Vec::new();
        }
        let Ok(key) = <[u8; PublicKey::LEN]>::try_from(public_key).map(PublicKey) else {
            return Vec::new();
        };
        if self.bans.remove(&key).is_none() {
            return Vec::new();
        }
        self.changes.push(Change::BanRemoved(key));
        self.ban_list_to_admins()
    }

    /// 改一个在线的人的角色。见 `SetRole` 的协议注释。
    ///
    /// 不能改自己的：最后一个管理员手一滑把自己降成成员，这个服务器就再也
    /// 没有管理员了。也不能改别的管理员的：两个管理员互相降级没有赢家。
    pub fn set_role(
        &mut self,
        session: SessionId,
        target: SessionId,
        role: Role,
    ) -> Vec<Broadcast> {
        let (Some(actor), Some(victim)) = (self.users.get(&session), self.users.get(&target))
        else {
            return Vec::new();
        };
        if session == target
            || actor.role != Role::Admin
            || victim.role == Role::Admin
            || role == Role::Unspecified
            || victim.role == role
        {
            return Vec::new();
        }
        let user = self.users.get_mut(&target).expect("just checked");
        user.role = role;
        let wire = user.to_wire();
        let key = user.public_key;
        self.granted.insert(key, role);
        self.changes.push(Change::RoleSet(key, role));

        let mut out = vec![Broadcast::Everyone(UserState { user: Some(wire) }.into())];
        if role == Role::Admin {
            out.push(Broadcast::One(target, self.ban_list()));
        }
        out
    }

    /// 完整的封禁名单，按封禁时间排。
    fn ban_list(&self) -> ServerMessage {
        let mut entries: Vec<BannedUser> = self.bans.values().map(BanEntry::to_wire).collect();
        entries.sort_by_key(|e| e.banned_at_ms);
        BanList { entries }.into()
    }

    /// 名单变了：发给每个在线的管理员。
    fn ban_list_to_admins(&self) -> Vec<Broadcast> {
        let list = self.ban_list();
        self.users
            .values()
            .filter(|u| u.role == Role::Admin)
            .map(|u| Broadcast::One(u.session_id, list.clone()))
            .collect()
    }

    /// 下一个没被用过的频道 id。
    ///
    /// **不用「最大值 + 1」**：频道会被删，那样算出来的 id 在删掉最后一个之后
    /// 会被重新发出去，而客户端手里可能还攥着旧的那个 id（比如一条刚发出去的
    /// JoinChannel）。从 1 往上找第一个空位也有同样的问题，但配合单调递增的
    /// `next_channel` 游标就没有 —— 游标只增不减。
    fn next_channel_id(&mut self) -> ChannelId {
        loop {
            let id = self.next_channel;
            self.next_channel = self.next_channel.wrapping_add(1).max(1);
            if !self.channels.contains_key(&id) {
                return id;
            }
        }
    }

    pub fn set_self_state(
        &mut self,
        session: SessionId,
        self_muted: bool,
        self_deafened: bool,
    ) -> Vec<Broadcast> {
        let Some(user) = self.users.get_mut(&session) else {
            return Vec::new();
        };
        user.self_muted = self_muted;
        // 关了耳朵就不该还在说话 —— 客户端理应自己处理，但服务端不能指望客户端。
        user.self_deafened = self_deafened;
        if self_deafened {
            user.self_muted = true;
        }
        let wire = user.to_wire();
        vec![Broadcast::Everyone(UserState { user: Some(wire) }.into())]
    }

    /// 转发一条文字消息。
    ///
    /// **发送者和时间戳一律由服务端填**，客户端填的会被覆盖 —— 否则任何人
    /// 都能冒充别人说话。
    pub fn text_message(
        &mut self,
        session: SessionId,
        mut message: TextMessage,
        now_ms: i64,
    ) -> Vec<Broadcast> {
        let Some(user) = self.users.get(&session) else {
            return Vec::new();
        };
        if message.body.is_empty() {
            return Vec::new();
        }
        truncate_utf8(&mut message.body, MAX_TEXT_BYTES);

        // 只能往自己所在的频道发。客户端指定别的频道一律改成自己的 ——
        // 不然就成了往任意频道喊话。
        message.channel_id = user.channel_id;
        message.sender_session_id = session;
        message.timestamp_ms = now_ms;

        vec![Broadcast::Channel(user.channel_id, message.into_server())]
    }
}

/// 清掉昵称里会把界面搞乱的东西，并限长。
/// 「某某把你请出了服务器：理由」。理由空着就不带冒号。
fn with_reason(what: String, reason: &str) -> String {
    let mut reason = reason.trim().to_string();
    truncate_utf8(&mut reason, MAX_REASON_BYTES);
    if reason.is_empty() {
        what
    } else {
        format!("{what}：{reason}")
    }
}

fn sanitize_name(raw: &str) -> String {
    let cleaned: String = raw
        .chars()
        // 控制字符会把终端和界面搞乱；换行会让一个人占好几行
        .filter(|c| !c.is_control())
        .collect();
    let trimmed = cleaned.trim();
    if trimmed.is_empty() {
        return "无名氏".to_string();
    }
    let mut name = trimmed.to_string();
    truncate_utf8(&mut name, MAX_NAME_BYTES);
    name
}

/// 按**字节**截断，但不切在多字节字符中间。
///
/// 直接 `truncate(n)` 在中文上会 panic —— 跟 `protocol::text` 里记的是同一个坑。
fn truncate_utf8(s: &mut String, max_bytes: usize) {
    if s.len() <= max_bytes {
        return;
    }
    let mut end = max_bytes;
    while end > 0 && !s.is_char_boundary(end) {
        end -= 1;
    }
    s.truncate(end);
}

#[cfg(test)]
mod tests {
    use super::*;

    fn key(n: u8) -> PublicKey {
        PublicKey([n; 32])
    }

    fn open_server() -> Server {
        Server::new(Config::default())
    }

    fn saved_channel(id: ChannelId, parent_id: ChannelId, name: &str, by: Option<u8>) -> Channel {
        Channel {
            id,
            parent_id,
            name: name.to_string(),
            min_role: Role::Guest as i32,
            created_by: by.map(|k| key(k).0.to_vec()).unwrap_or_default(),
            ..Default::default()
        }
    }

    fn restored(channels: Vec<Channel>, next_channel_id: ChannelId) -> Server {
        Server::restore(
            Config::default(),
            Saved {
                channels,
                next_channel_id,
                ..Saved::default()
            },
        )
    }

    /// 重启之后：频道还在，建的人还能删。
    #[test]
    fn restored_channels_are_still_owned_by_their_creator() {
        let mut server = restored(vec![saved_channel(5, 1, "开黑", Some(7))], 6);
        let owner = admit(&mut server, 7, "阿狸");
        let stranger = admit(&mut server, 8, "路人");

        assert_eq!(server.channels[&5].wire.name, "开黑");
        assert!(server.delete_channel(stranger, 5).is_empty(), "别人删掉了");
        assert!(
            !server.delete_channel(owner, 5).is_empty(),
            "建的人删不掉了"
        );
        assert!(!server.channels.contains_key(&5));
    }

    /// 发号游标要接着存档往下走 —— 重启之后再建的频道不能撞上旧 id。
    #[test]
    fn new_ids_continue_after_the_saved_ones() {
        // 存档说游标在 20（中间删过一些），最大的 id 是 9
        let mut server = restored(vec![saved_channel(9, 1, "老频道", Some(7))], 20);
        let owner = admit(&mut server, 7, "阿狸");
        create(&mut server, owner, "新频道");
        let new_id = server
            .channels
            .values()
            .find(|c| c.wire.name == "新频道")
            .unwrap()
            .wire
            .id;
        assert_eq!(new_id, 20);

        // 游标没记（旧存档）的话，至少要比最大的 id 大
        let mut server = restored(vec![saved_channel(9, 1, "老频道", Some(7))], 0);
        let owner = admit(&mut server, 7, "阿狸");
        create(&mut server, owner, "新频道");
        assert!(server
            .channels
            .values()
            .all(|c| c.wire.id != 9 || c.wire.name == "老频道"));
    }

    /// 存档是个文件，会坏。坏了也得起来一棵合法的树。
    #[test]
    fn a_broken_save_still_gives_a_valid_tree() {
        let server = restored(
            vec![
                saved_channel(3, 99, "父频道没了", None),
                saved_channel(4, 5, "环A", None),
                saved_channel(5, 4, "环B", None),
                saved_channel(6, 6, "自己是自己的父", None),
                saved_channel(7, 1, "   ", None),
                saved_channel(0, 1, "零号", None),
                saved_channel(8, 3, "挂在修好的频道下", None),
            ],
            0,
        );
        let root = server.root_channel();
        for id in [3, 6] {
            assert_eq!(
                server.channels[&id].wire.parent_id, root,
                "频道 {id} 没挂回根上"
            );
        }
        // 环只拆一处：4 挂回根上之后，5 → 4 → 根 已经合法，5 不用动。
        let cycle = [4, 5].map(|id| server.channels[&id].wire.parent_id);
        assert!(cycle.contains(&root), "环没拆开：{cycle:?}");
        assert_eq!(server.channels[&8].wire.parent_id, 3, "本来就合法的不该动");
        assert!(!server.channels.contains_key(&7), "空名字的该丢掉");
        assert!(!server.channels.contains_key(&0));
        for id in server.channels.keys() {
            assert!(server.reaches_root(*id), "频道 {id} 走不回根");
        }
    }

    /// 根频道只认存档里的名字，别的规矩不能被存档改掉。
    #[test]
    fn the_saved_root_cannot_become_deletable() {
        let mut hacked = saved_channel(1, 3, "改了名的大厅", Some(7));
        hacked.min_role = Role::Admin as i32;
        let mut server = restored(vec![hacked], 0);
        let root = server.root_channel();
        assert_eq!(server.channels[&root].wire.name, "改了名的大厅");
        assert_eq!(server.channels[&root].wire.parent_id, root);
        assert_eq!(server.channels[&root].created_by, None);

        let someone = admit(&mut server, 7, "阿狸");
        assert!(server.delete_channel(someone, root).is_empty());
    }

    #[test]
    fn restoring_respects_the_channel_limit() {
        let many = (2..(MAX_CHANNELS as u32 + 50))
            .map(|id| saved_channel(id, 1, "频道", None))
            .collect();
        let server = restored(many, 0);
        assert_eq!(server.channels.len(), MAX_CHANNELS);
    }

    fn admit(server: &mut Server, k: u8, name: &str) -> SessionId {
        server.admit(key(k), "", name).unwrap().session_id
    }

    /// 开一个设了邀请码的服务器。给错码进来的人就是访客。
    fn open_gated_server() -> Server {
        Server::new(Config {
            invite_code: Some("letmein".to_string()),
            ..Config::default()
        })
    }

    fn create(server: &mut Server, session: SessionId, name: &str) -> Vec<Broadcast> {
        server.create_channel(
            session,
            CreateChannel {
                name: name.to_string(),
                ..Default::default()
            },
        )
    }

    /// 从一串广播里把新建频道的 id 抠出来。
    fn created_id(broadcasts: &[Broadcast]) -> ChannelId {
        use protocol::control::server_message::Payload;
        broadcasts
            .iter()
            .find_map(|b| match b {
                Broadcast::Everyone(ServerMessage {
                    payload: Some(Payload::ChannelState(cs)),
                }) if !cs.removed => cs.channel.as_ref().map(|c| c.id),
                _ => None,
            })
            .expect("广播里没有新建的频道")
    }

    #[test]
    fn root_channel_exists_and_parents_itself() {
        let server = open_server();
        let welcome = server.welcome_for(0);
        assert_eq!(welcome.channels.len(), 1);
        let root = &welcome.channels[0];
        assert_eq!(root.id, root.parent_id, "根频道的 parent 该指向自己");
    }

    #[test]
    fn admitted_user_lands_in_root_and_is_announced() {
        let mut server = open_server();
        let admitted = server.admit(key(1), "", "阿强").unwrap();
        assert_eq!(admitted.welcome.name, "阿强");
        assert_eq!(admitted.welcome.current_channel_id, server.root_channel());
        assert_eq!(admitted.welcome.users.len(), 1);
        assert!(matches!(
            admitted.broadcasts.as_slice(),
            [Broadcast::Others(me, _)] if *me == admitted.session_id
        ));
    }

    #[test]
    fn duplicate_names_get_a_suffix_not_a_rejection() {
        let mut server = open_server();
        admit(&mut server, 1, "阿强");
        let second = server.admit(key(2), "", "阿强").unwrap();
        assert_eq!(second.welcome.name, "阿强2", "重名该加后缀，不该拒绝");
        let third = server.admit(key(3), "", "阿强").unwrap();
        assert_eq!(third.welcome.name, "阿强3");
    }

    /// 同一个公钥再连 = 顶号。断线重连、换机器都会走到这里。
    #[test]
    fn same_key_displaces_the_old_session() {
        let mut server = open_server();
        let first = admit(&mut server, 7, "阿强");
        assert_eq!(server.user_count(), 1);

        let again = server.admit(key(7), "", "阿强").unwrap();
        assert_eq!(again.displaced, Some(first), "旧会话该被顶掉");
        assert_eq!(server.user_count(), 1, "顶号之后不该变成两个人");
        assert_ne!(again.session_id, first, "新会话要有新的 id");
        // 顶号也要广播旧会话的离开，否则别人界面上会留一个幽灵
        assert!(again
            .broadcasts
            .iter()
            .any(|b| matches!(b, Broadcast::Everyone(m)
                if matches!(&m.payload, Some(protocol::control::server_message::Payload::UserLeft(l))
                    if l.session_id == first))));
        // 名字不该因为顶号变成"阿强2" —— 旧的那个已经走了
        assert_eq!(again.welcome.name, "阿强");
    }

    #[test]
    fn invite_code_controls_the_role() {
        let mut server = Server::new(Config {
            invite_code: Some("winter".into()),
            require_invite: false,
            ..Config::default()
        });
        let with = server.admit(key(1), "winter", "甲").unwrap();
        assert_eq!(with.welcome.role, Role::Member as i32);
        let without = server.admit(key(2), "", "乙").unwrap();
        assert_eq!(without.welcome.role, Role::Guest as i32, "没码只能当访客");
        let wrong = server.admit(key(3), "summer", "丙").unwrap();
        assert_eq!(wrong.welcome.role, Role::Guest as i32);
    }

    #[test]
    fn require_invite_actually_blocks() {
        let mut server = Server::new(Config {
            invite_code: Some("winter".into()),
            require_invite: true,
            ..Config::default()
        });
        assert_eq!(server.admit(key(1), "", "甲"), Err(Denied::InviteRequired));
        assert_eq!(
            server.admit(key(2), "summer", "乙"),
            Err(Denied::InviteRequired)
        );
        assert!(server.admit(key(3), "winter", "丙").is_ok());
    }

    /// 配置矛盾（要求邀请码但没设）时一律不放 —— 总比「以为设了限制其实谁都能进」强。
    #[test]
    fn contradictory_config_fails_closed() {
        let mut server = Server::new(Config {
            invite_code: None,
            require_invite: true,
            ..Config::default()
        });
        assert_eq!(server.admit(key(1), "", "甲"), Err(Denied::InviteRequired));
        assert_eq!(
            server.admit(key(1), "随便什么", "甲"),
            Err(Denied::InviteRequired)
        );
    }

    #[test]
    fn server_fills_up() {
        let mut server = Server::new(Config {
            max_users: 2,
            ..Config::default()
        });
        admit(&mut server, 1, "甲");
        admit(&mut server, 2, "乙");
        assert_eq!(server.admit(key(3), "", "丙"), Err(Denied::Full));
    }

    /// 服务器满了的时候正是最需要管理员的时候，不能把他也挡在外面。
    #[test]
    fn admin_gets_in_even_when_full_and_gated() {
        let mut server = Server::new(Config {
            max_users: 1,
            require_invite: true,
            invite_code: Some("winter".into()),
            admin_keys: vec![key(9)],
        });
        // 这台服务器要邀请码，普通人得带上；管理员不用
        server.admit(key(1), "winter", "甲").unwrap();
        let admin = server.admit(key(9), "", "管理员").unwrap();
        assert_eq!(admin.welcome.role, Role::Admin as i32);
    }

    #[test]
    fn disconnect_announces_departure() {
        let mut server = open_server();
        let session = admit(&mut server, 1, "甲");
        let events = server.disconnect(session);
        assert_eq!(events.len(), 1);
        assert_eq!(server.user_count(), 0);
        // 走了的人再断一次不该再广播
        assert!(server.disconnect(session).is_empty());
    }

    #[test]
    fn joining_a_missing_channel_is_ignored_not_fatal() {
        let mut server = open_server();
        let session = admit(&mut server, 1, "甲");
        assert!(server.join_channel(session, 9999).is_empty());
        assert_eq!(server.channel_of(session), Some(server.root_channel()));
    }

    #[test]
    fn text_message_sender_and_timestamp_are_server_stamped() {
        let mut server = open_server();
        let a = admit(&mut server, 1, "甲");
        let b = admit(&mut server, 2, "乙");

        // 甲试图冒充乙，并且指定一个别的频道
        let forged = TextMessage {
            channel_id: 4242,
            sender_session_id: b,
            body: "我是乙".into(),
            timestamp_ms: 1,
        };
        let events = server.text_message(a, forged, 999);
        let Broadcast::Channel(channel, msg) = &events[0] else {
            panic!("文字消息应该只发给一个频道");
        };
        assert_eq!(*channel, server.root_channel(), "不能往别的频道喊话");
        let Some(protocol::control::server_message::Payload::TextMessage(text)) = &msg.payload
        else {
            panic!("载荷不对");
        };
        assert_eq!(text.sender_session_id, a, "发送者必须是服务端填的");
        assert_eq!(text.timestamp_ms, 999, "时间戳必须是服务端填的");
        assert_eq!(text.channel_id, server.root_channel());
    }

    #[test]
    fn empty_text_is_dropped() {
        let mut server = open_server();
        let a = admit(&mut server, 1, "甲");
        let events = server.text_message(
            a,
            TextMessage {
                body: String::new(),
                ..Default::default()
            },
            0,
        );
        assert!(events.is_empty());
    }

    #[test]
    fn overlong_text_is_truncated_on_a_char_boundary() {
        let mut server = open_server();
        let a = admit(&mut server, 1, "甲");
        // 全是三字节汉字，截断点一定落在字符中间
        let body = "中".repeat(MAX_TEXT_BYTES);
        let events = server.text_message(
            a,
            TextMessage {
                body,
                ..Default::default()
            },
            0,
        );
        let Broadcast::Channel(_, msg) = &events[0] else {
            panic!()
        };
        let Some(protocol::control::server_message::Payload::TextMessage(text)) = &msg.payload
        else {
            panic!()
        };
        assert!(text.body.len() <= MAX_TEXT_BYTES);
        assert!(text.body.chars().all(|c| c == '中'), "截断切坏了字符");
    }

    #[test]
    fn deafening_yourself_also_mutes_you() {
        let mut server = open_server();
        let a = admit(&mut server, 1, "甲");
        let events = server.set_self_state(a, false, true);
        let Broadcast::Everyone(msg) = &events[0] else {
            panic!()
        };
        let Some(protocol::control::server_message::Payload::UserState(state)) = &msg.payload
        else {
            panic!()
        };
        let user = state.user.as_ref().unwrap();
        assert!(user.self_deafened);
        assert!(user.self_muted, "关了耳朵就不该还在说话");
    }

    #[test]
    fn names_are_sanitized() {
        assert_eq!(sanitize_name("  阿强  "), "阿强");
        assert_eq!(sanitize_name(""), "无名氏");
        assert_eq!(sanitize_name("   "), "无名氏");
        assert_eq!(sanitize_name("阿\n强\t"), "阿强", "控制字符会把界面搞乱");
        // 超长的中文名截断不能 panic
        let long = sanitize_name(&"中".repeat(100));
        assert!(long.len() <= MAX_NAME_BYTES);
        assert!(long.chars().all(|c| c == '中'));
    }

    #[test]
    fn sessions_in_channel_tracks_moves() {
        let mut server = open_server();
        let a = admit(&mut server, 1, "甲");
        let b = admit(&mut server, 2, "乙");
        let root = server.root_channel();
        let mut in_root = server.sessions_in_channel(root);
        in_root.sort_unstable();
        assert_eq!(in_root, vec![a, b]);

        server.disconnect(b);
        assert_eq!(server.sessions_in_channel(root), vec![a]);
    }

    // ======================================================================
    // 多频道
    // ======================================================================

    #[test]
    fn a_member_can_create_a_channel_and_everyone_hears_about_it() {
        let mut server = open_server();
        let me = admit(&mut server, 1, "阿强");

        let out = create(&mut server, me, "打本");
        let id = created_id(&out);
        assert_ne!(id, server.root_channel(), "新频道不能跟根频道撞 id");

        // 广播给所有人，**包括建的人自己** —— 客户端因此不用本地先插一个。
        assert!(matches!(out.as_slice(), [Broadcast::Everyone(_)]));

        let welcome = server.welcome_for(me);
        assert_eq!(welcome.channels.len(), 2);
        let made = welcome.channels.iter().find(|c| c.id == id).unwrap();
        assert_eq!(made.name, "打本");
        assert_eq!(
            made.parent_id,
            server.root_channel(),
            "没指定父频道就挂根上"
        );
    }

    #[test]
    fn a_guest_cannot_create_a_channel() {
        let mut server = open_gated_server();
        // 码给错了 -> 访客
        let guest = server.admit(key(1), "wrong", "路人").unwrap();
        assert_eq!(guest.welcome.role, Role::Guest as i32);

        assert!(
            create(&mut server, guest.session_id, "捣乱").is_empty(),
            "访客不该能建频道 —— 没有邀请码谁都能进，那等于把防滥用的闸打开"
        );
        assert_eq!(server.welcome_for(0).channels.len(), 1);
    }

    #[test]
    fn an_empty_or_oversized_name_is_refused() {
        let mut server = open_server();
        let me = admit(&mut server, 1, "阿强");

        assert!(create(&mut server, me, "   ").is_empty(), "空名字要拒");
        let long = "啊".repeat(MAX_CHANNEL_NAME_BYTES);
        assert!(create(&mut server, me, &long).is_empty(), "超长名字要拒");
        assert_eq!(server.welcome_for(0).channels.len(), 1);
    }

    #[test]
    fn the_name_is_trimmed() {
        let mut server = open_server();
        let me = admit(&mut server, 1, "阿强");
        let id = created_id(&create(&mut server, me, "  打本  "));
        let welcome = server.welcome_for(me);
        let made = welcome.channels.iter().find(|c| c.id == id).unwrap();
        assert_eq!(made.name, "打本");
    }

    #[test]
    fn channel_count_is_capped() {
        let mut server = open_server();
        let me = admit(&mut server, 1, "阿强");
        // 根频道占掉一个名额。
        for i in 1..MAX_CHANNELS {
            assert!(
                !create(&mut server, me, &format!("频道{i}")).is_empty(),
                "第 {i} 个就建不出来了，上限是 {MAX_CHANNELS}"
            );
        }
        assert!(
            create(&mut server, me, "再来一个").is_empty(),
            "到上限了还能建 —— 成员就能建频道，没有这个闸一个人能刷满内存"
        );
    }

    /// 建频道的人不能造出一个自己都进不去的频道。
    #[test]
    fn you_cannot_lock_yourself_out_of_your_own_channel() {
        let mut server = open_server();
        let me = admit(&mut server, 1, "阿强");
        assert_eq!(server.welcome_for(me).role, Role::Member as i32);

        let out = server.create_channel(
            me,
            CreateChannel {
                name: "管理层".to_string(),
                min_role: Role::Admin as i32,
                ..Default::default()
            },
        );
        let id = created_id(&out);
        let welcome = server.welcome_for(me);
        let made = welcome.channels.iter().find(|c| c.id == id).unwrap();
        assert_eq!(
            made.min_role,
            Role::Member as i32,
            "门槛该被压到建的人自己的角色 —— 否则这是个谁也进不去、本人也不会想起来删的僵尸频道"
        );

        // 而且他真的能进去。
        assert!(!server.join_channel(me, id).is_empty());
        assert_eq!(server.channel_of(me), Some(id));
    }

    #[test]
    fn the_creator_can_delete_their_own_channel() {
        let mut server = open_server();
        let me = admit(&mut server, 1, "阿强");
        let id = created_id(&create(&mut server, me, "打本"));

        let out = server.delete_channel(me, id);
        assert!(!out.is_empty(), "建的人该能删掉自己建的");
        assert_eq!(server.welcome_for(me).channels.len(), 1);
    }

    #[test]
    fn a_member_cannot_delete_someone_elses_channel() {
        let mut server = open_server();
        let mine = admit(&mut server, 1, "阿强");
        let other = admit(&mut server, 2, "阿伟");
        let id = created_id(&create(&mut server, mine, "打本"));

        assert!(
            server.delete_channel(other, id).is_empty(),
            "普通成员不该能删别人建的频道"
        );
        assert_eq!(server.welcome_for(mine).channels.len(), 2);
    }

    #[test]
    fn an_admin_can_delete_anyones_channel() {
        let mut server = Server::new(Config {
            admin_keys: vec![key(9)],
            ..Config::default()
        });
        let member = admit(&mut server, 1, "阿强");
        let admin = admit(&mut server, 9, "管理员");
        assert_eq!(server.welcome_for(admin).role, Role::Admin as i32);

        let id = created_id(&create(&mut server, member, "打本"));
        assert!(!server.delete_channel(admin, id).is_empty());
        assert_eq!(server.welcome_for(admin).channels.len(), 1);
    }

    #[test]
    fn the_root_channel_cannot_be_deleted() {
        let mut server = Server::new(Config {
            admin_keys: vec![key(9)],
            ..Config::default()
        });
        let admin = admit(&mut server, 9, "管理员");
        let root = server.root_channel();
        assert!(
            server.delete_channel(admin, root).is_empty(),
            "连管理员也不该能删根频道 —— 那是结构性的，不是权限问题"
        );
        assert_eq!(server.welcome_for(admin).channels.len(), 1);
    }

    /// **删频道不能把人留在一个不存在的频道里。**
    ///
    /// 留在那儿的话他的语音会被转发到没人收的地方，而界面上看不出任何问题。
    #[test]
    fn deleting_a_channel_moves_the_people_inside_back_to_root() {
        let mut server = open_server();
        let owner = admit(&mut server, 1, "阿强");
        let other = admit(&mut server, 2, "阿伟");

        let id = created_id(&create(&mut server, owner, "打本"));
        server.join_channel(other, id);
        assert_eq!(server.channel_of(other), Some(id));

        server.delete_channel(owner, id);
        assert_eq!(
            server.channel_of(other),
            Some(server.root_channel()),
            "频道没了，里面的人要被挪回根频道"
        );
    }

    #[test]
    fn deleting_a_parent_lifts_its_children_to_root() {
        let mut server = open_server();
        let me = admit(&mut server, 1, "阿强");

        let parent = created_id(&create(&mut server, me, "公会"));
        let child = created_id(&server.create_channel(
            me,
            CreateChannel {
                parent_id: parent,
                name: "打本".to_string(),
                ..Default::default()
            },
        ));

        server.delete_channel(me, parent);
        let welcome = server.welcome_for(me);
        let kid = welcome.channels.iter().find(|c| c.id == child);
        assert!(kid.is_some(), "子频道不该跟着父频道一起消失");
        assert_eq!(
            kid.unwrap().parent_id,
            server.root_channel(),
            "子频道要上提到根，否则它挂在一个不存在的父节点上"
        );
    }

    /// 频道 id 不能被回收 —— 客户端手里可能还攥着刚发出去的旧 id。
    #[test]
    fn channel_ids_are_not_reused_after_deletion() {
        let mut server = open_server();
        let me = admit(&mut server, 1, "阿强");

        let first = created_id(&create(&mut server, me, "第一个"));
        server.delete_channel(me, first);
        let second = created_id(&create(&mut server, me, "第二个"));
        assert_ne!(second, first, "删掉之后 id 不该被重新发出去");
    }

    /// 不认识的父频道挂到根上，**不是拒绝**。
    #[test]
    fn an_unknown_parent_falls_back_to_root_instead_of_failing() {
        let mut server = open_server();
        let me = admit(&mut server, 1, "阿强");
        let out = server.create_channel(
            me,
            CreateChannel {
                parent_id: 9999,
                name: "打本".to_string(),
                ..Default::default()
            },
        );
        let id = created_id(&out);
        let welcome = server.welcome_for(me);
        let made = welcome.channels.iter().find(|c| c.id == id).unwrap();
        assert_eq!(
            made.parent_id,
            server.root_channel(),
            "客户端的频道树可能刚好过时了一点，为这个让建频道失败没道理"
        );
    }

    #[test]
    fn min_role_still_gates_who_can_enter() {
        let mut server = Server::new(Config {
            invite_code: Some("letmein".to_string()),
            admin_keys: vec![key(9)],
            ..Config::default()
        });
        let admin = admit(&mut server, 9, "管理员");
        let guest = server.admit(key(1), "wrong", "路人").unwrap().session_id;

        let id = created_id(&server.create_channel(
            admin,
            CreateChannel {
                name: "成员专用".to_string(),
                min_role: Role::Member as i32,
                ..Default::default()
            },
        ));

        assert!(server.join_channel(guest, id).is_empty(), "访客进不去");
        assert_eq!(server.channel_of(guest), Some(server.root_channel()));
        assert!(!server.join_channel(admin, id).is_empty(), "管理员进得去");
    }

    // ======================================================================
    // 管理员链接、角色、踢人、封禁、改频道
    // ======================================================================

    /// 带着一条还没用掉的管理员链接起来的服务器。
    fn claimable_server() -> Server {
        Server::restore(
            Config::default(),
            Saved {
                admin_claim: Some("admin-code".into()),
                ..Saved::default()
            },
        )
    }

    fn admit_admin(server: &mut Server, k: u8) -> SessionId {
        server
            .admit(key(k), "admin-code", "管理员")
            .unwrap()
            .session_id
    }

    fn role_of(server: &Server, session: SessionId) -> Role {
        server.users[&session].role
    }

    fn kicked(events: &[Broadcast]) -> Option<(SessionId, goodbye::Reason)> {
        events.iter().find_map(|e| match e {
            Broadcast::Kick {
                session, reason, ..
            } => Some((*session, *reason)),
            _ => None,
        })
    }

    fn ban_lists_to(events: &[Broadcast]) -> Vec<SessionId> {
        events
            .iter()
            .filter_map(|e| match e {
                Broadcast::One(to, m)
                    if matches!(
                        m.payload,
                        Some(protocol::control::server_message::Payload::BanList(_))
                    ) =>
                {
                    Some(*to)
                }
                _ => None,
            })
            .collect()
    }

    #[test]
    fn the_admin_link_makes_exactly_one_admin() {
        let mut server = claimable_server();
        assert!(!server.has_admin());

        let admin = admit_admin(&mut server, 1);
        assert_eq!(role_of(&server, admin), Role::Admin);
        assert!(server.has_admin());
        assert_eq!(server.admin_claim(), None, "用过的链接该作废");
        assert_eq!(
            server.take_changes(),
            vec![Change::RoleSet(key(1), Role::Admin), Change::AdminClaimUsed]
        );

        // 第二个人拿同一条链接进来：只是个普通人
        let second = server.admit(key(2), "admin-code", "也想当").unwrap();
        assert_ne!(second.welcome.role, Role::Admin as i32);
    }

    /// 服务器要邀请码时，管理员链接里的码本身就是通行证。
    #[test]
    fn the_admin_link_works_on_a_gated_server() {
        let mut server = Server::restore(
            Config {
                require_invite: true,
                invite_code: Some("letmein".into()),
                ..Config::default()
            },
            Saved {
                admin_claim: Some("admin-code".into()),
                ..Saved::default()
            },
        );
        let admin = admit_admin(&mut server, 1);
        assert_eq!(role_of(&server, admin), Role::Admin);
    }

    /// 管理员一进来就拿到封禁名单；别人拿不到。
    #[test]
    fn only_admins_receive_the_ban_list() {
        let mut server = claimable_server();
        let admitted = server.admit(key(1), "admin-code", "管理员").unwrap();
        assert_eq!(
            ban_lists_to(&admitted.broadcasts),
            vec![admitted.session_id]
        );

        let member = server.admit(key(2), "", "阿狸").unwrap();
        assert!(ban_lists_to(&member.broadcasts).is_empty());
    }

    /// 被管理员定过角色的人，下次进来就是这个角色，不看手里是哪条链接。
    #[test]
    fn a_granted_role_outlives_the_invite_code() {
        let mut server = Server::restore(
            Config {
                require_invite: true,
                invite_code: Some("letmein".into()),
                ..Config::default()
            },
            Saved {
                roles: vec![(key(2), Role::ChannelAdmin)],
                ..Saved::default()
            },
        );
        // 没带码，照样进得来，而且是频道管理
        let admitted = server.admit(key(2), "", "阿狸").unwrap();
        assert_eq!(admitted.welcome.role, Role::ChannelAdmin as i32);
        // 没被定过角色的人照旧要码
        assert_eq!(
            server.admit(key(3), "", "路人").unwrap_err(),
            Denied::InviteRequired
        );
    }

    #[test]
    fn channel_admins_kick_those_below_them() {
        let mut server = claimable_server();
        let admin = admit_admin(&mut server, 1);
        let mod_ = admit(&mut server, 2, "频道管理");
        let member = admit(&mut server, 3, "阿狸");
        let other = admit(&mut server, 4, "波波");
        server.set_role(admin, mod_, Role::ChannelAdmin);

        // 成员踢不了人
        assert!(server.kick(member, other, "").is_empty());
        // 频道管理踢不了管理员，也踢不了自己
        assert!(server.kick(mod_, admin, "").is_empty());
        assert!(server.kick(mod_, mod_, "").is_empty());

        let events = server.kick(mod_, member, "刷屏");
        assert_eq!(
            kicked(&events),
            Some((member, goodbye::Reason::Kicked)),
            "{events:?}"
        );
        assert!(
            matches!(events[0], Broadcast::Kick { .. }),
            "先让他知道被踢了，再告诉别人他走了"
        );
        let Broadcast::Kick { detail, .. } = &events[0] else {
            unreachable!()
        };
        assert!(
            detail.contains("频道管理") && detail.contains("刷屏"),
            "{detail}"
        );
        assert!(!server.users.contains_key(&member));

        // 踢完了能马上回来 —— 这是踢不是封
        assert!(server.admit(key(3), "", "阿狸").is_ok());
    }

    /// 频道管理之间谁也踢不了谁。
    #[test]
    fn equals_cannot_kick_each_other() {
        let mut server = claimable_server();
        let admin = admit_admin(&mut server, 1);
        let a = admit(&mut server, 2, "甲");
        let b = admit(&mut server, 3, "乙");
        server.set_role(admin, a, Role::ChannelAdmin);
        server.set_role(admin, b, Role::ChannelAdmin);
        assert!(server.kick(a, b, "").is_empty());
    }

    #[test]
    fn only_admins_ban_and_the_banned_stay_out() {
        let mut server = claimable_server();
        let admin = admit_admin(&mut server, 1);
        let mod_ = admit(&mut server, 2, "频道管理");
        let troll = admit(&mut server, 3, "捣乱的");
        server.set_role(admin, mod_, Role::ChannelAdmin);
        server.take_changes();

        assert!(server.ban(mod_, troll, "", 0).is_empty(), "频道管理不能封");

        let events = server.ban(admin, troll, "  刷屏  ", 1234);
        assert_eq!(kicked(&events), Some((troll, goodbye::Reason::Banned)));
        assert_eq!(
            ban_lists_to(&events),
            vec![admin],
            "名单变了要告诉在线的管理员"
        );
        let changes = server.take_changes();
        let [Change::BanAdded(entry)] = changes.as_slice() else {
            panic!("{changes:?}");
        };
        assert_eq!(entry.name, "捣乱的");
        assert_eq!(entry.reason, "刷屏");
        assert_eq!(entry.banned_by, "管理员");

        assert_eq!(
            server.admit(key(3), "", "换个名字").unwrap_err(),
            Denied::Banned
        );
    }

    #[test]
    fn unbanning_lets_them_back_in() {
        let mut server = claimable_server();
        let admin = admit_admin(&mut server, 1);
        let member = admit(&mut server, 2, "阿狸");
        let troll = admit(&mut server, 3, "捣乱的");
        server.ban(admin, troll, "", 0);

        assert!(
            server.unban(member, &key(3).0).is_empty(),
            "只有管理员能解封"
        );
        assert!(server.unban(admin, &[1, 2, 3]).is_empty(), "公钥长度不对");

        let events = server.unban(admin, &key(3).0);
        assert_eq!(ban_lists_to(&events), vec![admin]);
        assert!(server.admit(key(3), "", "捣乱的").is_ok());
    }

    #[test]
    fn roles_are_set_by_admins_only_and_never_on_admins() {
        let mut server = claimable_server();
        let admin = admit_admin(&mut server, 1);
        let member = admit(&mut server, 2, "阿狸");
        let other = admit(&mut server, 3, "波波");
        server.take_changes();

        assert!(server
            .set_role(member, other, Role::ChannelAdmin)
            .is_empty());
        assert!(
            server.set_role(admin, admin, Role::Member).is_empty(),
            "不能改自己"
        );
        assert!(server.set_role(admin, member, Role::Unspecified).is_empty());

        let events = server.set_role(admin, member, Role::ChannelAdmin);
        assert_eq!(role_of(&server, member), Role::ChannelAdmin);
        assert!(
            matches!(events[0], Broadcast::Everyone(_)),
            "所有人都该看到"
        );
        assert_eq!(
            server.take_changes(),
            vec![Change::RoleSet(key(2), Role::ChannelAdmin)]
        );

        // 提成管理员：立刻拿到封禁名单；之后别的管理员动不了他
        let events = server.set_role(admin, member, Role::Admin);
        assert_eq!(ban_lists_to(&events), vec![member]);
        assert!(server.set_role(admin, member, Role::Guest).is_empty());
    }

    fn edit(
        server: &mut Server,
        session: SessionId,
        channel_id: ChannelId,
        name: &str,
        parent_id: ChannelId,
    ) -> Vec<Broadcast> {
        server.edit_channel(
            session,
            EditChannel {
                channel_id,
                name: name.into(),
                description: String::new(),
                parent_id,
            },
        )
    }

    #[test]
    fn the_creator_renames_their_own_channel_and_strangers_cannot() {
        let mut server = open_server();
        let owner = admit(&mut server, 1, "阿狸");
        let stranger = admit(&mut server, 2, "路人");
        let id = created_id(&create(&mut server, owner, "开黑"));
        let root = server.root_channel();
        server.take_changes();

        assert!(edit(&mut server, stranger, id, "我的了", root).is_empty());
        let events = edit(&mut server, owner, id, "  吃鸡  ", root);
        assert_eq!(events.len(), 1);
        assert_eq!(server.channels[&id].wire.name, "吃鸡");
        let changes = server.take_changes();
        assert!(
            matches!(&changes[..], [Change::ChannelSaved(c)] if c.name == "吃鸡"),
            "{changes:?}"
        );

        // 没改动就什么都不发，也不写盘
        assert!(edit(&mut server, owner, id, "吃鸡", root).is_empty());
        assert!(server.take_changes().is_empty());
        // 空名字拒掉
        assert!(edit(&mut server, owner, id, "   ", root).is_empty());
    }

    #[test]
    fn channel_admins_can_edit_other_peoples_channels() {
        let mut server = claimable_server();
        let admin = admit_admin(&mut server, 1);
        let owner = admit(&mut server, 2, "阿狸");
        let mod_ = admit(&mut server, 3, "频道管理");
        server.set_role(admin, mod_, Role::ChannelAdmin);
        let id = created_id(&create(&mut server, owner, "开黑"));
        let root = server.root_channel();
        assert!(!edit(&mut server, mod_, id, "改个名", root).is_empty());
    }

    /// 挪到自己的子孙下面会成环：不挪，但名字照改。
    #[test]
    fn a_channel_cannot_be_moved_under_itself() {
        let mut server = open_server();
        let owner = admit(&mut server, 1, "阿狸");
        let parent = created_id(&create(&mut server, owner, "父"));
        let child = created_id(&server.create_channel(
            owner,
            CreateChannel {
                name: "子".into(),
                parent_id: parent,
                ..Default::default()
            },
        ));
        let root = server.root_channel();

        edit(&mut server, owner, parent, "新名字", child);
        assert_eq!(server.channels[&parent].wire.parent_id, root, "成环了");
        assert_eq!(server.channels[&parent].wire.name, "新名字");

        edit(&mut server, owner, parent, "新名字", parent);
        assert_eq!(
            server.channels[&parent].wire.parent_id, root,
            "父频道成了自己"
        );

        // 正常的挪动照样可以
        let other = created_id(&create(&mut server, owner, "另一个"));
        edit(&mut server, owner, child, "子", other);
        assert_eq!(server.channels[&child].wire.parent_id, other);
    }

    /// 根频道只有管理员能改，而且只能改名，挪不了。
    #[test]
    fn only_admins_rename_the_root_and_it_never_moves() {
        let mut server = claimable_server();
        let admin = admit_admin(&mut server, 1);
        let member = admit(&mut server, 2, "阿狸");
        let root = server.root_channel();
        let other = created_id(&create(&mut server, member, "别处"));

        assert!(edit(&mut server, member, root, "我的大厅", root).is_empty());
        assert!(!edit(&mut server, admin, root, "公会大厅", other).is_empty());
        assert_eq!(server.channels[&root].wire.name, "公会大厅");
        assert_eq!(server.channels[&root].wire.parent_id, root);
    }

    /// 频道的建、改、删都要记成变化，存档才知道要写什么。
    #[test]
    fn channel_changes_are_recorded_for_the_store() {
        let mut server = open_server();
        let owner = admit(&mut server, 1, "阿狸");
        assert!(server.take_changes().is_empty(), "进来一个人不用写盘");

        let id = created_id(&create(&mut server, owner, "开黑"));
        assert!(matches!(&server.take_changes()[..], [Change::ChannelSaved(c)] if c.id == id));

        server.delete_channel(owner, id);
        assert_eq!(server.take_changes(), vec![Change::ChannelRemoved(id)]);
    }
}
