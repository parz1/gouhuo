// SPDX-License-Identifier: MPL-2.0

//! 服务端状态在本地的镜像：频道树、谁在线、最近说了什么。
//!
//! 界面直接读这个结构画画面，不自己攒状态 —— 两份状态早晚会不一致，
//! 而不一致在语音软件里的表现是「明明有人在说话，列表里却没这个人」。
//!
//! # 认人认公钥，不认昵称
//!
//! 昵称会变、会重名（服务端会自动加后缀）、会被改。跨会话稳定的只有公钥。
//! 所以「这是不是上次那个人」一律比 [`User::public_key`]。

use std::collections::BTreeMap;
use std::collections::VecDeque;

use protocol::control::{BannedUser, Channel, Role, User, Welcome};

/// 聊天记录最多留多少条。
///
/// 设计上**明确不做长期消息历史** —— 这是语音软件不是聊天软件，
/// 文字框是用来发个链接、报个坐标的。留一屏够翻就行，关掉就没了。
pub const MAX_CHAT_LINES: usize = 200;

/// 频道树扁平化之后的一行，界面可以直接照着画。
#[derive(Debug, Clone, PartialEq)]
pub struct ChannelNode {
    pub channel: Channel,
    /// 缩进层级，根频道是 0。
    pub depth: usize,
}

#[derive(Debug, Clone, PartialEq)]
pub struct ChatLine {
    pub sender_session: u32,
    /// 发的时候那个人叫什么。**存下来而不是每次去查** ——
    /// 人走了之后他说过的话还得能显示出是谁说的。
    pub sender_name: String,
    pub body: String,
    pub timestamp_ms: i64,
}

#[derive(Debug, Default, Clone)]
pub struct Roster {
    /// 自己的会话 id。
    pub me: u32,
    pub channels: BTreeMap<u32, Channel>,
    pub users: BTreeMap<u32, User>,
    pub chat: VecDeque<ChatLine>,
    /// 封禁名单。**只有管理员有**：服务端只把它发给管理员，别人这里永远是空的。
    pub bans: Vec<BannedUser>,
}

impl Roster {
    pub fn from_welcome(welcome: &Welcome) -> Self {
        let mut roster = Self {
            me: welcome.session_id,
            ..Default::default()
        };
        for channel in &welcome.channels {
            roster.channels.insert(channel.id, channel.clone());
        }
        for user in &welcome.users {
            roster.users.insert(user.session_id, user.clone());
        }
        roster
    }

    pub fn my_user(&self) -> Option<&User> {
        self.users.get(&self.me)
    }

    pub fn my_channel(&self) -> u32 {
        self.my_user().map(|u| u.channel_id).unwrap_or_default()
    }

    pub fn my_role(&self) -> Role {
        self.my_user()
            .and_then(|u| Role::try_from(u.role).ok())
            .unwrap_or(Role::Unspecified)
    }

    /// 我能不能建频道。界面拿它决定显不显示「新建频道」。
    ///
    /// **这只是画界面用的，不是权限检查。** 真正说了算的是服务端 ——
    /// 这里判断错了最多是显示一个点了没反应的按钮，不会有安全问题。
    pub fn can_create_channel(&self) -> bool {
        (self.my_role() as i32) >= (Role::Member as i32)
    }

    pub fn is_admin(&self) -> bool {
        self.my_role() == Role::Admin
    }

    fn role_of(&self, session: u32) -> Option<Role> {
        self.users
            .get(&session)
            .map(|u| Role::try_from(u.role).unwrap_or(Role::Unspecified))
    }

    /// 我能不能踢这个人。跟服务端一样的规则：频道管理及以上，只能踢比自己低的。
    ///
    /// 下面这几个 `can_*` 都**只是画界面用的**，说了算的是服务端。
    pub fn can_kick(&self, session: u32) -> bool {
        let Some(target) = self.role_of(session) else {
            return false;
        };
        let me = self.my_role();
        session != self.me
            && (me as i32) >= (Role::ChannelAdmin as i32)
            && (target as i32) < (me as i32)
    }

    /// 我能不能封这个人：只有管理员能封，只能封比自己低的。
    pub fn can_ban(&self, session: u32) -> bool {
        let Some(target) = self.role_of(session) else {
            return false;
        };
        session != self.me && self.is_admin() && target != Role::Admin
    }

    /// 我能不能改这个人的角色：管理员改别人的，改不了自己的和别的管理员的。
    pub fn can_set_role(&self, session: u32) -> bool {
        self.can_ban(session)
    }

    /// 我能不能改这个频道（改名、挪）。根频道只有管理员能改。
    pub fn can_edit_channel(&self, channel_id: u32) -> bool {
        let Some(channel) = self.channels.get(&channel_id) else {
            return false;
        };
        if channel.parent_id == channel.id {
            return self.is_admin();
        }
        if (self.my_role() as i32) >= (Role::ChannelAdmin as i32) {
            return true;
        }
        match self.my_user() {
            Some(me) if !channel.created_by.is_empty() => channel.created_by == me.public_key,
            _ => false,
        }
    }

    /// 我能不能删这个频道。同上，只是画界面用的。
    ///
    /// 规则跟服务端对齐：**根频道谁都删不掉**；除此之外，要么是我建的，
    /// 要么我是频道管理及以上。「是不是我建的」靠比公钥 —— 不比 session id，
    /// 那个一断线就变了，而「我建的频道」应该在重连之后还成立。
    pub fn can_delete_channel(&self, channel_id: u32) -> bool {
        let Some(channel) = self.channels.get(&channel_id) else {
            return false;
        };
        if channel.parent_id == channel.id {
            return false;
        }
        if (self.my_role() as i32) >= (Role::ChannelAdmin as i32) {
            return true;
        }
        match self.my_user() {
            // created_by 为空 = 服务器自带的频道，不是任何人建的。
            Some(me) if !channel.created_by.is_empty() => channel.created_by == me.public_key,
            _ => false,
        }
    }

    pub fn name_of(&self, session: u32) -> String {
        self.users
            .get(&session)
            .map(|u| u.name.clone())
            // 服务端保证转发前会盖章，所以查不到只可能是那个人刚走
            .unwrap_or_else(|| "（已离开）".to_string())
    }

    /// 一个频道里的人，按 [`sort_key`] 排。
    pub fn users_in(&self, channel: u32) -> Vec<&User> {
        let mut found: Vec<&User> = self
            .users
            .values()
            .filter(|u| u.channel_id == channel)
            .collect();
        found.sort_by_cached_key(|u| sort_key(&u.name, u.session_id));
        found
    }

    /// 找根频道：`parent_id` 指向自己的那个。
    pub fn root(&self) -> Option<u32> {
        self.channels
            .values()
            .find(|c| c.parent_id == c.id)
            .map(|c| c.id)
    }

    /// 把频道树压成界面能直接渲染的一串行。
    ///
    /// **必须能处理坏掉的树。** 频道树是服务端发来的，理论上永远是棵树，
    /// 但「理论上」在解析不可信输入时不算数：环（A 的父是 B，B 的父是 A）
    /// 会让朴素的递归无限下去，直接把界面线程挂死。所以这里记着访问过谁，
    /// 环里的频道当孤儿处理 —— 显示得难看，总比卡死强。
    pub fn tree(&self) -> Vec<ChannelNode> {
        let mut out = Vec::with_capacity(self.channels.len());
        let mut visited = std::collections::BTreeSet::new();

        if let Some(root) = self.root() {
            self.walk(root, 0, &mut visited, &mut out);
        }

        // 挂不到根上的（环里的、父频道不存在的）也要显示出来，
        // 否则用户会看到「有人在一个我看不见的频道里」。
        for id in self.channels.keys() {
            if !visited.contains(id) {
                self.walk(*id, 0, &mut visited, &mut out);
            }
        }
        out
    }

    fn walk(
        &self,
        id: u32,
        depth: usize,
        visited: &mut std::collections::BTreeSet<u32>,
        out: &mut Vec<ChannelNode>,
    ) {
        if !visited.insert(id) {
            return;
        }
        let Some(channel) = self.channels.get(&id) else {
            return;
        };
        out.push(ChannelNode {
            channel: channel.clone(),
            depth,
        });

        let mut children: Vec<&Channel> = self
            .channels
            .values()
            .filter(|c| c.parent_id == id && c.id != id)
            .collect();
        children.sort_by(|a, b| a.name.cmp(&b.name).then(a.id.cmp(&b.id)));
        for child in children {
            self.walk(child.id, depth + 1, visited, out);
        }
    }

    pub fn push_chat(&mut self, line: ChatLine) {
        if self.chat.len() >= MAX_CHAT_LINES {
            self.chat.pop_front();
        }
        self.chat.push_back(line);
    }
}

/// 名单的排序键。
///
/// # 为什么不按拼音排
///
/// 中文按 UTF-8 字节序就是按码点序，「波波」会排在「阿狸」前面 ——
/// 对中文用户来说这看着是乱的。
///
/// 但拼音排序要一张汉字到拼音的表，还要处理多音字，实际做法是拖进 ICU
/// 那个量级的依赖。**为了一个最多 20 人的列表，这不值得**：安装包 60 MB
/// 的红线是实打实的，而 3–20 人的名单一眼就能扫完，排序对不对几乎不影响使用。
///
/// 这里只做两件不要钱的事：
///
/// - **ASCII 不分大小写**：`alice` 和 `Bob` 不该因为大小写被拆到两头，
///   这个是真会烦到人的。
/// - **同名用 session id 兜底**：保证顺序**稳定**。名单里的人跳来跳去
///   比排得不对更烦 —— 正要点某个人的时候他挪位置了。
fn sort_key(name: &str, session: u32) -> (String, u32) {
    (name.to_lowercase(), session)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn channel(id: u32, parent: u32, name: &str) -> Channel {
        Channel {
            id,
            parent_id: parent,
            name: name.into(),
            description: String::new(),
            max_users: 0,
            min_role: Role::Guest as i32,
            created_by: Vec::new(),
        }
    }

    /// 带「谁建的」的频道。公钥跟 [`user`] 里的规则一致：session 号重复 32 次。
    fn channel_made_by(id: u32, parent: u32, name: &str, creator_session: u32) -> Channel {
        Channel {
            created_by: vec![creator_session as u8; 32],
            ..channel(id, parent, name)
        }
    }

    fn user(session: u32, channel: u32, name: &str) -> User {
        User {
            session_id: session,
            public_key: vec![session as u8; 32],
            name: name.into(),
            channel_id: channel,
            role: Role::Member as i32,
            self_muted: false,
            self_deafened: false,
            server_muted: false,
        }
    }

    fn roster_with(channels: Vec<Channel>) -> Roster {
        let mut roster = Roster::default();
        for c in channels {
            roster.channels.insert(c.id, c);
        }
        roster
    }

    #[test]
    fn tree_nests_and_indents() {
        let roster = roster_with(vec![
            channel(1, 1, "大厅"),
            channel(2, 1, "开黑一队"),
            channel(3, 2, "小房间"),
            channel(4, 1, "挂机"),
        ]);
        let tree = roster.tree();
        let shape: Vec<(u32, usize)> = tree.iter().map(|n| (n.channel.id, n.depth)).collect();
        assert_eq!(shape, vec![(1, 0), (2, 1), (3, 2), (4, 1)]);
    }

    /// 频道树是服务端发来的不可信输入。环必须不能把界面挂死。
    #[test]
    fn a_cycle_does_not_hang_the_tree() {
        let roster = roster_with(vec![
            channel(1, 1, "大厅"),
            // 2 和 3 互相当对方的父频道
            channel(2, 3, "环 A"),
            channel(3, 2, "环 B"),
        ]);
        let tree = roster.tree();
        assert_eq!(tree.len(), 3, "环里的频道也要显示出来，不能吞掉");
        let ids: Vec<u32> = tree.iter().map(|n| n.channel.id).collect();
        assert!(ids.contains(&2) && ids.contains(&3));
    }

    /// 父频道不存在的频道不能消失 —— 否则用户会看到「有人在一个看不见的频道里」。
    #[test]
    fn orphans_are_still_shown() {
        let roster = roster_with(vec![channel(1, 1, "大厅"), channel(9, 77, "孤儿")]);
        let ids: Vec<u32> = roster.tree().iter().map(|n| n.channel.id).collect();
        assert_eq!(ids, vec![1, 9]);
    }

    #[test]
    fn no_root_still_renders_everything() {
        // 一棵完全没有根的树（服务端不该这样，但界面不能因此白屏）
        let roster = roster_with(vec![channel(5, 6, "甲"), channel(6, 5, "乙")]);
        assert_eq!(roster.tree().len(), 2);
    }

    #[test]
    fn users_in_only_lists_that_channel() {
        let mut roster = roster_with(vec![channel(1, 1, "大厅")]);
        for (s, n) in [(1, "阿狸"), (2, "波波"), (3, "阿呆")] {
            roster.users.insert(s, user(s, 1, n));
        }
        roster.users.insert(4, user(4, 2, "别的频道"));

        let names: Vec<&str> = roster.users_in(1).iter().map(|u| u.name.as_str()).collect();
        assert_eq!(names.len(), 3);
        assert!(!names.contains(&"别的频道"));
    }

    /// 大小写不该把名字拆到两头 —— 这个是真会烦到人的。
    #[test]
    fn ascii_names_ignore_case() {
        let mut roster = roster_with(vec![channel(1, 1, "大厅")]);
        for (s, n) in [(1, "bob"), (2, "Alice"), (3, "carol")] {
            roster.users.insert(s, user(s, 1, n));
        }
        let names: Vec<&str> = roster.users_in(1).iter().map(|u| u.name.as_str()).collect();
        assert_eq!(names, vec!["Alice", "bob", "carol"]);
    }

    /// 顺序必须**稳定**：正要点某个人的时候他挪了位置，比排得不对更烦。
    #[test]
    fn identical_names_keep_a_stable_order() {
        let mut roster = roster_with(vec![channel(1, 1, "大厅")]);
        // 服务端其实会给重名加后缀，但客户端不能指望它
        for s in [7, 3, 5] {
            roster.users.insert(s, user(s, 1, "阿狸"));
        }
        let order: Vec<u32> = roster.users_in(1).iter().map(|u| u.session_id).collect();
        assert_eq!(order, vec![3, 5, 7]);
        // 再排一次结果必须一样
        let again: Vec<u32> = roster.users_in(1).iter().map(|u| u.session_id).collect();
        assert_eq!(order, again);
    }

    #[test]
    fn chat_is_capped() {
        let mut roster = Roster::default();
        for i in 0..MAX_CHAT_LINES + 50 {
            roster.push_chat(ChatLine {
                sender_session: 1,
                sender_name: "阿狸".into(),
                body: format!("第 {i} 条"),
                timestamp_ms: i as i64,
            });
        }
        assert_eq!(roster.chat.len(), MAX_CHAT_LINES);
        // 留下的是最新的那些
        assert_eq!(
            roster.chat.back().unwrap().body,
            format!("第 {} 条", MAX_CHAT_LINES + 49)
        );
    }

    /// 人走了之后，他说过的话还得显示出是谁说的。
    #[test]
    fn chat_keeps_the_name_even_after_the_speaker_leaves() {
        let mut roster = roster_with(vec![channel(1, 1, "大厅")]);
        roster.users.insert(7, user(7, 1, "波波"));
        roster.push_chat(ChatLine {
            sender_session: 7,
            sender_name: roster.name_of(7),
            body: "走了".into(),
            timestamp_ms: 0,
        });
        roster.users.remove(&7);

        assert_eq!(roster.chat.back().unwrap().sender_name, "波波");
        assert_eq!(roster.name_of(7), "（已离开）");
    }

    #[test]
    fn welcome_populates_everything() {
        let welcome = Welcome {
            tcp_voice: false,
            session_id: 3,
            name: "阿狸".into(),
            role: Role::Member as i32,
            udp_port: 20800,
            channels: vec![channel(1, 1, "大厅")],
            users: vec![user(3, 1, "阿狸")],
            current_channel_id: 1,
        };
        let roster = Roster::from_welcome(&welcome);
        assert_eq!(roster.me, 3);
        assert_eq!(roster.my_channel(), 1);
        assert_eq!(roster.my_role(), Role::Member);
        assert_eq!(roster.name_of(3), "阿狸");
    }

    // ======================================================================
    // 能不能建 / 能不能删
    //
    // 这两个只决定界面画什么，**不是权限检查** —— 真正说了算的是服务端。
    // 但画错了用户会看到一个点了没反应的按钮，那种 bug 最伤信任。
    // ======================================================================

    /// 带上「我是谁、我什么角色」的名单。
    fn roster_as(me: u32, my_role: Role, channels: Vec<Channel>) -> Roster {
        let mut roster = Roster {
            me,
            ..Default::default()
        };
        for c in channels {
            roster.channels.insert(c.id, c);
        }
        let mut u = user(me, 1, "我");
        u.role = my_role as i32;
        roster.users.insert(me, u);
        roster
    }

    #[test]
    fn a_guest_is_not_offered_the_new_channel_button() {
        let roster = roster_as(1, Role::Guest, vec![channel(1, 1, "大厅")]);
        assert!(!roster.can_create_channel());
    }

    #[test]
    fn a_member_is_offered_the_new_channel_button() {
        let roster = roster_as(1, Role::Member, vec![channel(1, 1, "大厅")]);
        assert!(roster.can_create_channel());
    }

    #[test]
    fn nobody_is_offered_delete_on_the_root_channel() {
        // 连管理员也不行 —— 根频道是结构性的，不是权限问题。
        for role in [Role::Member, Role::ChannelAdmin, Role::Admin] {
            let roster = roster_as(1, role, vec![channel(1, 1, "大厅")]);
            assert!(!roster.can_delete_channel(1), "{role:?} 不该能删根频道");
        }
    }

    #[test]
    fn i_can_delete_the_channel_i_made() {
        let roster = roster_as(
            7,
            Role::Member,
            vec![channel(1, 1, "大厅"), channel_made_by(2, 1, "打本", 7)],
        );
        assert!(roster.can_delete_channel(2));
    }

    #[test]
    fn a_member_cannot_delete_someone_elses_channel() {
        let roster = roster_as(
            7,
            Role::Member,
            vec![channel(1, 1, "大厅"), channel_made_by(2, 1, "打本", 9)],
        );
        assert!(!roster.can_delete_channel(2));
    }

    #[test]
    fn a_channel_admin_can_delete_anyones_channel() {
        let roster = roster_as(
            7,
            Role::ChannelAdmin,
            vec![channel(1, 1, "大厅"), channel_made_by(2, 1, "打本", 9)],
        );
        assert!(roster.can_delete_channel(2));
    }

    /// 服务器自带的频道（`created_by` 是空的）不该被当成"我建的"。
    ///
    /// 会踩到这里是因为空 Vec 跟空 Vec 是相等的 —— 如果我的公钥也碰巧
    /// 读不出来，两边都是空就会判成"我建的"。
    #[test]
    fn a_built_in_channel_is_not_mine_even_if_my_key_is_missing() {
        let mut roster = roster_as(7, Role::Member, vec![channel(1, 1, "大厅")]);
        roster.channels.insert(2, channel(2, 1, "服务器自带的"));
        roster.users.clear(); // 我的 User 还没到
        assert!(!roster.can_delete_channel(2));
    }

    #[test]
    fn an_unknown_channel_is_not_deletable() {
        let roster = roster_as(1, Role::Admin, vec![channel(1, 1, "大厅")]);
        assert!(!roster.can_delete_channel(9999));
    }
}
