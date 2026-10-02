// SPDX-License-Identifier: GPL-3.0-or-later

//! 存在磁盘上的偏好设置。
//!
//! # 为什么不用 JSON / TOML
//!
//! 就这么几个值，拉一个序列化库进来不划算 —— 而且这个文件**用户会手改**：
//! 自部署这群人遇到问题的第一反应就是打开配置文件看看。一行一个
//! `键=值`，记事本打开就懂，改错了也只是那一行被忽略。
//!
//! # 坏文件不能让程序起不来
//!
//! 这个文件跟身份文件放在一起。身份文件坏了是大事（等于换了个人），
//! 偏好设置坏了不是 —— 认不出来的行直接跳过，缺的值用默认。
//! 绝不能因为有人手滑多打了个字就打不开客户端。

use std::collections::BTreeMap;
use std::io;
use std::path::{Path, PathBuf};
use std::sync::{mpsc, Arc, Condvar, Mutex};
use std::time::Duration;

use protocol::Invite;
use voice_core::hotkey::Key;

/// 点窗口的 × 时怎么办。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum CloseAction {
    /// 每次问：收到托盘还是退出。**默认** —— 两种习惯的人都照顾到，
    /// 而且没人会在不知情的情况下挂在频道里。
    Ask,
    /// 收到托盘，继续在频道里。
    Tray,
    /// 退出。
    Quit,
}

impl CloseAction {
    /// 界面上那一排按钮的序号。
    pub fn index(self) -> i32 {
        match self {
            CloseAction::Ask => 0,
            CloseAction::Tray => 1,
            CloseAction::Quit => 2,
        }
    }

    pub fn from_index(index: i32) -> Self {
        match index {
            1 => CloseAction::Tray,
            2 => CloseAction::Quit,
            _ => CloseAction::Ask,
        }
    }
}

/// 按住说话还是语音激活。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum TalkMode {
    PushToTalk,
    VoiceActivity,
}

/// 加入过的一个服务器。首页上「回到上次的篝火」和下面那一排都是它。
#[derive(Debug, Clone, PartialEq)]
pub struct SavedServer {
    /// 完整的邀请链接：地址、端口、证书指纹，私人服务器还带着加入码。
    ///
    /// 不管当初是怎么加入的（粘链接、输域名、输 IP 核对指纹），存下来都是这一种 ——
    /// 再次进入时就跟点邀请链接一模一样，指纹是固定住的。
    pub link: String,
    /// 给人看的名字。加入页告诉我们的；没有就是空的，界面上显示地址。
    pub name: String,
    /// 上次加入的时刻，Unix 秒。
    pub last_used: u64,
    /// 加入页地址（不含加入码）。服务器换了证书时，可以从这儿重新取指纹。
    pub page: Option<String>,
}

impl SavedServer {
    pub fn invite(&self) -> Option<Invite> {
        Invite::parse(&self.link).ok()
    }
}

/// 最多记多少个服务器。再多首页就成了通讯录，而且这个文件是要能手改的。
pub const MAX_SERVERS: usize = 20;

#[derive(Debug, Clone, PartialEq)]
pub struct Settings {
    pub nick: String,
    /// 上次连的邀请链接，跟 `servers` 里第一个是同一条。
    ///
    /// 留着它是为了老版本：换回旧客户端时，它还认这一行。
    pub last_invite: String,
    /// 加入过的服务器，最近用的在前。
    pub servers: Vec<SavedServer>,
    pub talk_mode: TalkMode,
    /// 按住说话绑的键。`None` 表示还没绑。
    pub ptt_key: Option<Key>,
    /// 语音激活的阈值，分贝。
    pub vad_threshold_db: f32,
    /// 用哪个麦克风。`None` 是系统默认。
    ///
    /// 存的是 WASAPI 的设备 id，重启之后还有效。设备没了会退回默认 ——
    /// 拔个耳机不该让人从此说不了话。
    pub capture_device: Option<String>,
    /// 用哪个扬声器/耳机。
    pub render_device: Option<String>,
    /// 单独给某个人调的音量，百分比。键是那个人公钥的 base32。
    ///
    /// **按公钥存，不按会话 id，也不按昵称**：会话 id 一断线就换，昵称会改、
    /// 会重名。只有公钥跨会话、跨服务器都认得出是同一个人 —— 在这个服务器上
    /// 把谁调小了，在另一个服务器上碰到他也还是小的。
    ///
    /// 100% 的不存。只存调过的，文件里就只有「这几个人我调过」。
    pub user_volumes: BTreeMap<String, u32>,
    /// 有人进出我所在的频道时响一声。
    pub cue_sounds: bool,
    /// 顺便把名字念出来（Windows 自带的语音合成）。
    ///
    /// **默认关**：一屋子人进进出出，每次都念一句会很吵，而且第一次听到
    /// 电脑突然开口说话会吓一跳。想要的人自己开。
    pub announce_names: bool,
    /// 提示音和念名字的音量，百分比。
    pub cue_volume: u32,
    /// 点窗口的 × 时怎么办。
    pub close_action: CloseAction,
    /// 启动时去 GitHub 看一眼有没有新版本。见 update.rs。
    pub check_updates: bool,
}

/// 提示音音量的默认值。提示音本身已经比人声轻了，再打个六折：
/// 挂几个小时，每次有人进出都响，宁可轻一点。
pub const DEFAULT_CUE_VOLUME: u32 = 60;

/// 单人音量的范围，百分比。上限跟语音链路那边的 `MAX_VOLUME` 对齐。
pub const MAX_USER_VOLUME: u32 = 400;
pub const DEFAULT_USER_VOLUME: u32 = 100;

/// 滑条上的原始值 → 存下来的百分比。
///
/// 取到 5% 一档：没人分得出 67% 和 68%，而文件里出现 67.3829 只会让手改的人困惑。
/// 100% 附近吸住 —— 拖过头想调回原样的时候，不该要人对准一个像素。
pub fn snap_volume(raw: f32) -> u32 {
    if !raw.is_finite() {
        return DEFAULT_USER_VOLUME;
    }
    let clamped = raw.clamp(0.0, MAX_USER_VOLUME as f32);
    if (clamped - DEFAULT_USER_VOLUME as f32).abs() <= 7.0 {
        return DEFAULT_USER_VOLUME;
    }
    ((clamped / 5.0).round() * 5.0) as u32
}

/// 设置文件里单人音量那几行的前缀：`volume.<公钥>=<百分比>`。
const VOLUME_PREFIX: &str = "volume.";

/// 存下来的服务器那几行的前缀：`server.<序号>.<字段>=<值>`。
const SERVER_PREFIX: &str = "server.";

/// 电平条和滑块用的分贝范围。
///
/// 下限 -60 dB：再往下是本底噪声，画出来也只是一条贴着左边的线。
/// 上限 0 dB 是满刻度，超过就是削波了。
pub const MIN_DB: f32 = -60.0;
pub const MAX_DB: f32 = 0.0;

/// 分贝 → 0–1。界面上电平条和阈值刻线共用这把尺子。
pub fn db_to_level(db: f32) -> f32 {
    ((db - MIN_DB) / (MAX_DB - MIN_DB)).clamp(0.0, 1.0)
}

/// 0–1 → 分贝。
pub fn level_to_db(level: f32) -> f32 {
    MIN_DB + level.clamp(0.0, 1.0) * (MAX_DB - MIN_DB)
}

impl Default for Settings {
    fn default() -> Self {
        Self {
            nick: String::new(),
            last_invite: String::new(),
            servers: Vec::new(),
            // 默认语音激活。默认按住说话的话，没绑键的新用户会发现
            // 怎么说都没人听见，而且完全不知道为什么。
            talk_mode: TalkMode::VoiceActivity,
            ptt_key: None,
            vad_threshold_db: DEFAULT_VAD_THRESHOLD_DB,
            capture_device: None,
            render_device: None,
            user_volumes: BTreeMap::new(),
            cue_sounds: true,
            announce_names: false,
            cue_volume: DEFAULT_CUE_VOLUME,
            close_action: CloseAction::Ask,
            check_updates: true,
        }
    }
}

/// 语音激活的默认阈值。安静房间里够用；机械键盘和风扇会把它顶起来，
/// 那正是 APM 的降噪要解决的事。
pub const DEFAULT_VAD_THRESHOLD_DB: f32 = -45.0;

impl Settings {
    /// 这个人（按公钥的 base32）的音量，百分比。没调过就是 100。
    pub fn user_volume(&self, key: &str) -> u32 {
        self.user_volumes
            .get(key)
            .copied()
            .unwrap_or(DEFAULT_USER_VOLUME)
    }

    pub fn set_user_volume(&mut self, key: &str, percent: u32) {
        let percent = percent.min(MAX_USER_VOLUME);
        if percent == DEFAULT_USER_VOLUME {
            self.user_volumes.remove(key);
        } else {
            self.user_volumes.insert(key.to_string(), percent);
        }
    }

    /// 加入成功了：记下这个服务器，排到最前面。
    ///
    /// **按地址认同一个服务器（不分大小写），不按指纹**：服务器重装换了证书，用户重新核对之后
    /// 该是把旧的那条换掉，而不是首页上多出一个同名同地址、点了必定连不上的。
    pub fn remember_server(&mut self, invite: &Invite, name: &str, page: Option<&str>, now: u64) {
        let Ok(link) = invite.to_url() else { return };
        let existing = self
            .servers
            .iter()
            .position(|server| {
                server
                    .invite()
                    .is_some_and(|old| same_address(&old, &invite.host, invite.port))
            })
            .map(|index| self.servers.remove(index));
        let name = one_line(name);
        self.servers.insert(
            0,
            SavedServer {
                link: link.clone(),
                // 这次没拿到名字（比如点邀请链接进来的）就沿用以前的。
                name: if name.is_empty() {
                    existing
                        .as_ref()
                        .map(|s| s.name.clone())
                        .unwrap_or_default()
                } else {
                    name
                },
                last_used: now,
                page: page
                    .map(one_line)
                    .filter(|p| !p.is_empty())
                    .or(existing.and_then(|s| s.page)),
            },
        );
        self.servers.truncate(MAX_SERVERS);
        self.last_invite = link;
    }

    pub fn forget_server(&mut self, index: usize) {
        if index < self.servers.len() {
            self.servers.remove(index);
        }
        self.last_invite = self
            .servers
            .first()
            .map(|s| s.link.clone())
            .unwrap_or_default();
    }

    pub fn path() -> io::Result<PathBuf> {
        // 跟身份文件放在一起，搬机器的时候一起走。
        let identity = voice_core::identity::Identity::default_path()?;
        Ok(identity
            .parent()
            .unwrap_or(Path::new("."))
            .join("settings.txt"))
    }

    /// 读。读不出来就用默认 —— 第一次跑本来就没有这个文件。
    pub fn load() -> Self {
        Self::path()
            .and_then(std::fs::read_to_string)
            .map(|text| Self::parse(&text))
            .unwrap_or_default()
    }

    pub fn parse(text: &str) -> Self {
        let mut settings = Self::default();
        // 序号 → 那个服务器的几行。序号只用来把同一个服务器的几行凑到一起。
        let mut servers: BTreeMap<u32, SavedServer> = BTreeMap::new();
        for line in text.lines() {
            let line = line.trim();
            if line.is_empty() || line.starts_with('#') {
                continue;
            }
            let Some((key, value)) = line.split_once('=') else {
                continue;
            };
            let value = value.trim();
            let key = key.trim();
            if let Some(who) = key.strip_prefix(VOLUME_PREFIX) {
                // 不认识的值跳过，不夹到范围里：手改成 4000 多半是手滑，
                // 当成 400% 放出来会吓人一跳。
                if !who.is_empty() && !who.contains(char::is_whitespace) {
                    if let Ok(percent) = value.parse::<u32>() {
                        if percent <= MAX_USER_VOLUME {
                            settings.set_user_volume(who, percent);
                        }
                    }
                }
                continue;
            }
            if let Some(rest) = key.strip_prefix(SERVER_PREFIX) {
                let Some((index, field)) = rest.split_once('.') else {
                    continue;
                };
                let Ok(index) = index.parse::<u32>() else {
                    continue;
                };
                let server = servers.entry(index).or_insert_with(|| SavedServer {
                    link: String::new(),
                    name: String::new(),
                    last_used: 0,
                    page: None,
                });
                match field {
                    "link" => server.link = value.to_string(),
                    "name" => server.name = value.to_string(),
                    "used" => server.last_used = value.parse().unwrap_or(0),
                    "page" => server.page = non_empty(value),
                    _ => {}
                }
                continue;
            }
            match key {
                "nick" => settings.nick = value.to_string(),
                "last_invite" => settings.last_invite = value.to_string(),
                "talk_mode" => {
                    settings.talk_mode = match value {
                        "ptt" => TalkMode::PushToTalk,
                        _ => TalkMode::VoiceActivity,
                    }
                }
                "ptt_key" => settings.ptt_key = value.parse().ok().and_then(Key::decode),
                "close" => {
                    settings.close_action = match value {
                        "tray" => CloseAction::Tray,
                        "quit" => CloseAction::Quit,
                        // 认不出来的回到「每次问」：比悄悄替用户选一个好。
                        _ => CloseAction::Ask,
                    }
                }
                "cue_sounds" => settings.cue_sounds = value != "off",
                "check_updates" => settings.check_updates = value != "off",
                "announce_names" => settings.announce_names = value == "on",
                "cue_volume" => {
                    if let Ok(percent) = value.parse::<u32>() {
                        if percent <= 100 {
                            settings.cue_volume = percent;
                        }
                    }
                }
                "capture_device" => settings.capture_device = non_empty(value),
                "render_device" => settings.render_device = non_empty(value),
                "vad_threshold_db" => {
                    // 解析不出来或者离谱的值一律退回默认 —— 一个手滑打成
                    // 正数的阈值会让用户一个字都发不出去。
                    if let Ok(db) = value.parse::<f32>() {
                        if db.is_finite() && (MIN_DB..=MAX_DB).contains(&db) {
                            settings.vad_threshold_db = db;
                        }
                    }
                }
                // 认不出来的键跳过。将来加了新设置，老版本读到也不会炸。
                _ => {}
            }
        }

        // 链接解析不了的那条丢掉：留着它，首页上就有一个点了必定报错的服务器。
        settings.servers = servers
            .into_values()
            .filter(|server| server.invite().is_some())
            .collect();
        // 最近用的在前。时间一样的保持文件里的先后（这个排序是稳定的）。
        settings
            .servers
            .sort_by_key(|server| std::cmp::Reverse(server.last_used));
        settings.servers.truncate(MAX_SERVERS);
        // 从只记一条链接的老版本升上来：那一条就是第一个存下来的服务器。
        if settings.servers.is_empty() && Invite::parse(&settings.last_invite).is_ok() {
            settings.servers.push(SavedServer {
                link: settings.last_invite.clone(),
                name: String::new(),
                last_used: 0,
                page: None,
            });
        }
        settings
    }

    /// Synchronous persistence for non-GUI callers. The GUI schedules complete
    /// snapshots through `SettingsWriter` instead of waiting for the filesystem.
    pub fn save(&self) -> io::Result<()> {
        let path = Self::path()?;
        if let Some(dir) = path.parent() {
            std::fs::create_dir_all(dir)?;
        }
        std::fs::write(&path, self.serialize())
    }

    fn serialize(&self) -> String {
        let mode = match self.talk_mode {
            TalkMode::PushToTalk => "ptt",
            TalkMode::VoiceActivity => "vad",
        };
        format!(
            "# 篝火的偏好设置。可以手改，改坏了那一行会被忽略。\n\
             nick={}\n\
             last_invite={}\n\
             talk_mode={}\n\
             ptt_key={}\n\
             vad_threshold_db={:.1}\n\
             capture_device={}\n\
             render_device={}\n\
             cue_sounds={}\n\
             announce_names={}\n\
             cue_volume={}\n\
             close={}\n\
             check_updates={}\n",
            // 值里有换行的话会把文件切坏，所以过滤掉。
            // 昵称里的换行是粘贴时最容易带进来的东西。
            one_line(&self.nick),
            one_line(&self.last_invite),
            mode,
            self.ptt_key.map(Key::encode).unwrap_or(0),
            self.vad_threshold_db,
            one_line(self.capture_device.as_deref().unwrap_or("")),
            one_line(self.render_device.as_deref().unwrap_or("")),
            on_off(self.cue_sounds),
            on_off(self.announce_names),
            self.cue_volume,
            match self.close_action {
                CloseAction::Ask => "ask",
                CloseAction::Tray => "tray",
                CloseAction::Quit => "quit",
            },
            on_off(self.check_updates),
        ) + &self.serialize_servers()
            + &self.serialize_volumes()
    }

    fn serialize_servers(&self) -> String {
        if self.servers.is_empty() {
            return String::new();
        }
        let mut out = String::from(
            "# 加入过的服务器，最近用的在前。删掉某个序号的几行就是忘掉那个服务器。\n\
             # link 里可能带着私人服务器的加入码 —— 别把这个文件发给别人。\n",
        );
        for (index, server) in self.servers.iter().enumerate() {
            let n = index + 1;
            out.push_str(&format!(
                "{SERVER_PREFIX}{n}.link={}\n{SERVER_PREFIX}{n}.name={}\n{SERVER_PREFIX}{n}.used={}\n",
                one_line(&server.link),
                one_line(&server.name),
                server.last_used,
            ));
            if let Some(page) = &server.page {
                out.push_str(&format!("{SERVER_PREFIX}{n}.page={}\n", one_line(page)));
            }
        }
        out
    }

    fn serialize_volumes(&self) -> String {
        if self.user_volumes.is_empty() {
            return String::new();
        }
        let mut out = String::from(
            "# 单独调过音量的人：volume.<公钥>=<百分比>，删掉那行就回到 100%。
",
        );
        for (who, percent) in &self.user_volumes {
            out.push_str(&format!(
                "{VOLUME_PREFIX}{}={percent}
",
                one_line(who)
            ));
        }
        out
    }
}

fn same_address(invite: &Invite, host: &str, port: u16) -> bool {
    invite.port == port && invite.host.eq_ignore_ascii_case(host)
}

fn on_off(value: bool) -> &'static str {
    if value {
        "on"
    } else {
        "off"
    }
}

fn non_empty(value: &str) -> Option<String> {
    (!value.is_empty()).then(|| value.to_string())
}

fn one_line(value: &str) -> String {
    value.replace(['\n', '\r'], " ").trim().to_string()
}

struct SettingsWriteState {
    /// At most one complete snapshot waits behind the write in progress.
    pending: Option<Settings>,
    closing: bool,
    error: Option<String>,
}

struct SettingsWriteShared {
    state: Mutex<SettingsWriteState>,
    ready: Condvar,
}

/// GUI-safe settings persistence. Handles never own a thread-joining object.
#[derive(Clone)]
pub struct SettingsWriterHandle {
    shared: Arc<SettingsWriteShared>,
}

impl SettingsWriterHandle {
    /// Replace the single pending snapshot. Serialization and all disk I/O run
    /// on the worker, outside this mutex. `false` means shutdown has begun.
    pub fn schedule(&self, settings: &Settings) -> bool {
        let snapshot = settings.clone();
        let mut state = self.shared.state.lock().expect("settings writer poisoned");
        if state.closing {
            return false;
        }
        state.pending = Some(snapshot);
        self.shared.ready.notify_one();
        true
    }

    /// The most recent failed write; a later successful write clears it.
    #[allow(dead_code)]
    pub fn last_error(&self) -> Option<String> {
        self.shared
            .state
            .lock()
            .expect("settings writer poisoned")
            .error
            .clone()
    }

    /// Accept no further writes; flush the latest already accepted snapshot.
    /// This only signals the worker and never waits for a write in progress.
    pub fn shutdown(&self) {
        let mut state = self.shared.state.lock().expect("settings writer poisoned");
        state.closing = true;
        self.shared.ready.notify_one();
    }
}

/// One ordered writer avoids a thread per edit and old snapshots overwriting
/// new preferences. Drain it after the GUI event loop has ended.
pub struct SettingsWriter {
    handle: SettingsWriterHandle,
    stopped: mpsc::Receiver<()>,
}

impl SettingsWriter {
    pub fn start() -> io::Result<Self> {
        Self::spawn(Settings::save)
    }

    fn spawn(
        mut write: impl FnMut(&Settings) -> io::Result<()> + Send + 'static,
    ) -> io::Result<Self> {
        let shared = Arc::new(SettingsWriteShared {
            state: Mutex::new(SettingsWriteState {
                pending: None,
                closing: false,
                error: None,
            }),
            ready: Condvar::new(),
        });
        let worker_shared = Arc::clone(&shared);
        let (done, stopped) = mpsc::channel();
        std::thread::Builder::new()
            .name("gouhuo-settings-writer".into())
            .spawn(move || {
                loop {
                    let snapshot = {
                        let mut state = worker_shared
                            .state
                            .lock()
                            .expect("settings writer poisoned");
                        while state.pending.is_none() && !state.closing {
                            state = worker_shared
                                .ready
                                .wait(state)
                                .expect("settings writer poisoned");
                        }
                        match state.pending.take() {
                            Some(snapshot) => snapshot,
                            None => break,
                        }
                    };
                    // Never hold the pending-state mutex while serializing or
                    // waiting on filesystem/antivirus/device operations.
                    let error = write(&snapshot).err().map(|error| error.to_string());
                    if let Some(error) = &error {
                        eprintln!("保存设置失败：{error}");
                    }
                    worker_shared
                        .state
                        .lock()
                        .expect("settings writer poisoned")
                        .error = error;
                }
                let _ = done.send(());
            })?;
        Ok(Self {
            handle: SettingsWriterHandle { shared },
            stopped,
        })
    }

    pub fn handle(&self) -> SettingsWriterHandle {
        self.handle.clone()
    }

    pub fn shutdown(&self) {
        self.handle.shutdown();
    }

    /// Blocking drain only for an exit coordinator outside the GUI event loop.
    /// Returns false if disk I/O is still blocked after the supplied bound.
    pub fn wait_stopped(&self, timeout: Duration) -> bool {
        self.stopped.recv_timeout(timeout).is_ok()
    }
}

impl Drop for SettingsWriter {
    fn drop(&mut self) {
        // Covers startup/early-return paths too. The detached worker flushes
        // accepted data; dropping the owner or a GUI handle never joins it.
        self.shutdown();
    }
}

#[cfg(test)]
mod settings_writer_tests {
    use super::*;
    use std::time::Instant;

    fn wait(condition: impl Fn() -> bool) {
        let until = Instant::now() + Duration::from_secs(2);
        while !condition() {
            assert!(
                Instant::now() < until,
                "timed out waiting for writer result"
            );
            std::thread::sleep(Duration::from_millis(1));
        }
    }

    #[test]
    fn blocked_disk_does_not_block_schedule_and_shutdown_flushes_latest_in_order() {
        let (entered, started) = mpsc::channel();
        let (release, released) = mpsc::channel();
        let writes = Arc::new(Mutex::new(Vec::new()));
        let committed = Arc::clone(&writes);
        let mut first = true;
        let writer = SettingsWriter::spawn(move |settings| {
            if first {
                first = false;
                entered.send(()).unwrap();
                released.recv().unwrap();
            }
            committed.lock().unwrap().push(settings.clone());
            Ok(())
        })
        .unwrap();
        let handle = writer.handle();
        let initial = Settings {
            nick: "first".into(),
            ..Settings::default()
        };
        assert!(handle.schedule(&initial));
        started.recv_timeout(Duration::from_secs(1)).unwrap();

        // Keep the disk write blocked. A different frontend thread must still
        // finish scheduling complete newer preferences before it is released.
        let (scheduled, completion) = mpsc::channel();
        let frontend = handle.clone();
        let latest = Settings {
            nick: "latest".into(),
            cue_volume: 37,
            talk_mode: TalkMode::VoiceActivity,
            ..Settings::default()
        };
        let expected = latest.clone();
        let submitter = std::thread::spawn(move || {
            let skipped = Settings {
                nick: "superseded".into(),
                ..Settings::default()
            };
            assert!(frontend.schedule(&skipped));
            assert!(frontend.schedule(&latest));
            scheduled.send(()).unwrap();
        });
        completion
            .recv_timeout(Duration::from_secs(1))
            .expect("schedule waited for disk I/O");
        submitter.join().unwrap();
        writer.shutdown();
        assert!(!handle.schedule(&Settings {
            nick: "too late".into(),
            ..Settings::default()
        }));
        release.send(()).unwrap();
        assert!(writer.wait_stopped(Duration::from_secs(2)));
        let stored = writes.lock().unwrap();
        assert_eq!(
            *stored,
            vec![initial, expected],
            "pending snapshots were not coalesced or were written out of order"
        );
    }

    #[test]
    fn releasing_a_gui_handle_never_joins_a_blocked_write() {
        let (entered, started) = mpsc::channel();
        let (release, released) = mpsc::channel();
        let writer = SettingsWriter::spawn(move |_| {
            entered.send(()).unwrap();
            released.recv().unwrap();
            Ok(())
        })
        .unwrap();
        let handle = writer.handle();
        assert!(handle.schedule(&Settings::default()));
        started.recv_timeout(Duration::from_secs(1)).unwrap();
        let (done, completion) = mpsc::channel();
        let gui = std::thread::spawn(move || {
            drop(handle);
            done.send(()).unwrap();
        });
        completion
            .recv_timeout(Duration::from_secs(1))
            .expect("GUI handle drop joined the writer");
        gui.join().unwrap();
        writer.shutdown();
        release.send(()).unwrap();
        assert!(writer.wait_stopped(Duration::from_secs(2)));
    }

    #[test]
    fn failed_persistence_is_reported_and_a_later_success_clears_it() {
        let (written, committed) = mpsc::channel();
        let writer = SettingsWriter::spawn(move |settings| {
            written.send(settings.nick.clone()).unwrap();
            if settings.nick == "fail" {
                Err(io::Error::new(
                    io::ErrorKind::PermissionDenied,
                    "controlled permission failure",
                ))
            } else {
                Ok(())
            }
        })
        .unwrap();
        let handle = writer.handle();
        handle.schedule(&Settings {
            nick: "fail".into(),
            ..Settings::default()
        });
        assert_eq!(
            committed.recv_timeout(Duration::from_secs(1)).unwrap(),
            "fail"
        );
        wait(|| handle.last_error().is_some());
        assert!(handle.last_error().unwrap().contains("permission failure"));
        handle.schedule(&Settings {
            nick: "success".into(),
            ..Settings::default()
        });
        assert_eq!(
            committed.recv_timeout(Duration::from_secs(1)).unwrap(),
            "success"
        );
        wait(|| handle.last_error().is_none());
        writer.shutdown();
        assert!(writer.wait_stopped(Duration::from_secs(2)));
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn invite(host: &str, code: Option<&str>) -> Invite {
        Invite {
            host: host.into(),
            port: 20800,
            cert: protocol::Fingerprint([0x5a; 16]),
            code: code.map(str::to_string),
        }
    }

    fn sample() -> Settings {
        Settings {
            nick: "阿狸".into(),
            last_invite: "gouhuo://j/abc".into(),
            servers: vec![
                SavedServer {
                    link: invite("voice.example.com", Some("winter"))
                        .to_url()
                        .unwrap(),
                    name: "周末开黑".into(),
                    last_used: 1_790_000_000,
                    page: Some("https://voice.example.com/".into()),
                },
                SavedServer {
                    link: invite("203.0.113.7", None).to_url().unwrap(),
                    name: String::new(),
                    last_used: 1_780_000_000,
                    page: None,
                },
            ],
            talk_mode: TalkMode::PushToTalk,
            ptt_key: Some(Key::Keyboard(0x20)),
            vad_threshold_db: -38.5,
            capture_device: Some("{0.0.1.00000000}.{abc}".into()),
            render_device: None,
            user_volumes: BTreeMap::from([("AAAA".to_string(), 40), ("BBBB".to_string(), 250)]),
            cue_sounds: false,
            announce_names: true,
            cue_volume: 35,
            close_action: CloseAction::Tray,
            check_updates: false,
        }
    }

    #[test]
    fn round_trips() {
        let original = sample();
        assert_eq!(Settings::parse(&original.serialize()), original);
    }

    #[test]
    fn defaults_are_usable_without_a_file() {
        let settings = Settings::parse("");
        assert_eq!(settings.talk_mode, TalkMode::VoiceActivity);
        assert_eq!(settings.ptt_key, None);
    }

    /// 默认必须是语音激活。默认按住说话的话，没绑键的新用户会发现
    /// 怎么说都没人听见，而且完全不知道为什么。
    #[test]
    fn the_default_mode_works_without_any_binding() {
        assert_eq!(Settings::default().talk_mode, TalkMode::VoiceActivity);
    }

    /// 文件坏了不能让程序起不来。
    #[test]
    fn garbage_is_skipped_not_fatal() {
        let settings = Settings::parse(
            "这是一行乱写的\n\
             nick=阿狸\n\
             = 没有键\n\
             ptt_key=不是数字\n\
             未来的设置=某个值\n\
             talk_mode=谁知道呢\n",
        );
        assert_eq!(settings.nick, "阿狸");
        assert_eq!(settings.ptt_key, None);
        // 认不出来的模式退回默认，而不是让用户发不出声
        assert_eq!(settings.talk_mode, TalkMode::VoiceActivity);
    }

    #[test]
    fn comments_and_blank_lines_are_ignored() {
        let settings = Settings::parse("# 注释\n\n  \nnick=波波\n");
        assert_eq!(settings.nick, "波波");
    }

    /// 昵称里粘进换行的话不能把文件切坏。
    #[test]
    fn newlines_in_values_do_not_corrupt_the_file() {
        let settings = Settings {
            nick: "阿狸\nptt_key=999".into(),
            ..sample()
        };
        let parsed = Settings::parse(&settings.serialize());
        assert!(!parsed.nick.contains('\n'));
        assert_eq!(
            parsed.ptt_key,
            Some(Key::Keyboard(0x20)),
            "换行把别的设置覆盖掉了"
        );
    }

    #[test]
    fn mouse_buttons_survive_the_round_trip() {
        let settings = Settings {
            ptt_key: Some(Key::Mouse(5)),
            ..sample()
        };
        assert_eq!(
            Settings::parse(&settings.serialize()).ptt_key,
            Some(Key::Mouse(5))
        );
    }

    /// 没设设备的时候要是 None，不能是空字符串 —— 空字符串会被当成
    /// 一个不存在的设备 id 去打开。
    #[test]
    fn an_unset_device_is_none_not_empty() {
        let settings = Settings::parse("capture_device=\nrender_device=\n");
        assert_eq!(settings.capture_device, None);
        assert_eq!(settings.render_device, None);
    }

    /// 离谱的阈值不能让用户一个字都发不出去。
    #[test]
    fn an_absurd_threshold_falls_back_to_the_default() {
        for value in ["999", "-999", "abc", "NaN", "inf"] {
            let settings = Settings::parse(&format!("vad_threshold_db={value}\n"));
            assert_eq!(
                settings.vad_threshold_db, DEFAULT_VAD_THRESHOLD_DB,
                "阈值 `{value}` 该退回默认"
            );
        }
    }

    /// 电平条和滑块共用一把尺子，两边换算必须对得上。
    #[test]
    fn the_decibel_scale_round_trips() {
        for db in [MIN_DB, -45.0, -20.0, MAX_DB] {
            assert!((level_to_db(db_to_level(db)) - db).abs() < 0.01, "{db}");
        }
        // 超出范围的要夹住，不能跑出条子外面
        assert_eq!(db_to_level(-200.0), 0.0);
        assert_eq!(db_to_level(50.0), 1.0);
    }

    #[test]
    fn user_volumes_default_to_full_and_forget_resets() {
        let mut settings = Settings::default();
        assert_eq!(settings.user_volume("AAAA"), 100);
        settings.set_user_volume("AAAA", 30);
        assert_eq!(settings.user_volume("AAAA"), 30);
        // 调回 100 就不再记着这个人
        settings.set_user_volume("AAAA", 100);
        assert!(settings.user_volumes.is_empty());
        // 超上限的夹住
        settings.set_user_volume("BBBB", 9999);
        assert_eq!(settings.user_volume("BBBB"), MAX_USER_VOLUME);
    }

    #[test]
    fn the_slider_snaps_to_steps_and_to_full_volume() {
        assert_eq!(snap_volume(0.0), 0);
        assert_eq!(snap_volume(2.4), 0);
        assert_eq!(snap_volume(67.3829), 65);
        assert_eq!(snap_volume(94.0), 100);
        assert_eq!(snap_volume(106.9), 100);
        assert_eq!(snap_volume(250.0), 250);
        assert_eq!(snap_volume(999.0), MAX_USER_VOLUME);
        assert_eq!(snap_volume(-3.0), 0);
        assert_eq!(snap_volume(f32::NAN), DEFAULT_USER_VOLUME);
    }

    /// 静音某个人（0%）也要存下来，下次进来还是静音的。
    #[test]
    fn a_muted_person_stays_muted() {
        let mut settings = Settings::default();
        settings.set_user_volume("AAAA", 0);
        assert_eq!(
            Settings::parse(&settings.serialize()).user_volume("AAAA"),
            0
        );
    }

    #[test]
    fn absurd_volumes_are_skipped() {
        let settings = Settings::parse(
            "volume.AAAA=4000
             volume.BBBB=-5
             volume.CCCC=很大
             volume.=50
             volume.DD DD=50
             volume.EEEE=60
",
        );
        assert_eq!(
            settings.user_volumes,
            BTreeMap::from([("EEEE".to_string(), 60)])
        );
    }

    /// 从只记一条链接的老版本升上来，那一条要出现在首页上。
    #[test]
    fn the_old_single_invite_becomes_the_first_saved_server() {
        let link = invite("voice.example.com", None).to_url().unwrap();
        let settings = Settings::parse(&format!("nick=阿狸\nlast_invite={link}\n"));
        assert_eq!(settings.servers.len(), 1);
        assert_eq!(settings.servers[0].link, link);
        // 一条坏掉的旧链接不该变成一个点了必定报错的服务器
        assert!(Settings::parse("last_invite=gouhuo://j/abc\n")
            .servers
            .is_empty());
    }

    #[test]
    fn joining_again_moves_the_server_to_the_front_without_duplicating() {
        let mut settings = Settings::default();
        settings.remember_server(&invite("a.example.com", None), "甲", None, 100);
        settings.remember_server(&invite("b.example.com", None), "乙", None, 200);
        assert_eq!(settings.servers[0].name, "乙");

        // 这次是点邀请链接进来的，没有名字，还带上了加入码
        let with_code = invite("A.Example.com", Some("winter"));
        settings.remember_server(&with_code, "", Some("https://a.example.com/"), 300);
        assert_eq!(settings.servers.len(), 2, "同一个地址不该存两份");
        assert_eq!(settings.servers[0].name, "甲", "没拿到新名字就沿用旧的");
        assert_eq!(settings.servers[0].last_used, 300);
        assert_eq!(
            settings.servers[0].invite().unwrap().code.as_deref(),
            Some("winter")
        );
        assert_eq!(settings.last_invite, with_code.to_url().unwrap());

        // 再从别的路子进来，加入页地址还记着
        settings.remember_server(&invite("a.example.com", Some("winter")), "", None, 400);
        assert_eq!(
            settings.servers[0].page.as_deref(),
            Some("https://a.example.com/")
        );
    }

    /// 服务器换了证书、用户重新核对之后：旧的那条被换掉，而不是多出一条。
    #[test]
    fn a_new_fingerprint_replaces_the_old_entry() {
        let mut settings = Settings::default();
        settings.remember_server(&invite("a.example.com", None), "甲", None, 100);
        let renewed = Invite {
            cert: protocol::Fingerprint([0x11; 16]),
            ..invite("a.example.com", None)
        };
        settings.remember_server(&renewed, "", None, 200);
        assert_eq!(settings.servers.len(), 1);
        assert_eq!(settings.servers[0].invite().unwrap().cert, renewed.cert);
    }

    #[test]
    fn forgetting_and_the_cap() {
        let mut settings = Settings::default();
        for n in 0..(MAX_SERVERS as u64 + 5) {
            settings.remember_server(&invite(&format!("s{n}.example.com"), None), "", None, n);
        }
        assert_eq!(settings.servers.len(), MAX_SERVERS);
        let second = settings.servers[1].link.clone();
        settings.forget_server(0);
        assert_eq!(settings.last_invite, second);
        settings.forget_server(999);
        while !settings.servers.is_empty() {
            settings.forget_server(0);
        }
        assert_eq!(settings.last_invite, "");
    }

    /// 手改坏了的那几行不该让别的服务器也丢了。
    #[test]
    fn damaged_server_lines_are_skipped() {
        let good = invite("a.example.com", None).to_url().unwrap();
        let settings = Settings::parse(&format!(
            "server.1.link=gouhuo://j/坏的\n\
             server.1.name=坏的\n\
             server.x.link={good}\n\
             server.3.link={good}\n\
             server.3.used=很久以前\n\
             server.3.未来的字段=1\n\
             server.4.name=只有名字\n"
        ));
        assert_eq!(settings.servers.len(), 1);
        assert_eq!(settings.servers[0].link, good);
        assert_eq!(settings.servers[0].last_used, 0);
    }

    /// 服务器名是别人的加入页给的，里面有换行也不能把文件切坏。
    #[test]
    fn a_hostile_server_name_cannot_inject_settings() {
        let mut settings = Settings::default();
        settings.remember_server(
            &invite("a.example.com", None),
            "好名字\nnick=被改了\nserver.9.link=x",
            None,
            1,
        );
        let parsed = Settings::parse(&settings.serialize());
        assert_eq!(parsed.nick, "");
        assert_eq!(parsed.servers.len(), 1);
    }

    /// 提示音默认开、念名字默认关；写坏了也退回这两个默认。
    #[test]
    fn cues_default_to_a_chime_without_speech() {
        let settings = Settings::parse("cue_volume=999\ncue_volume=-1\n");
        assert!(settings.cue_sounds);
        assert!(!settings.announce_names);
        assert_eq!(settings.cue_volume, DEFAULT_CUE_VOLUME);

        let garbled = Settings::parse("cue_sounds=也许\nannounce_names=也许\n");
        assert!(garbled.cue_sounds, "认不出来的值不该把提示音关掉");
        assert!(!garbled.announce_names, "认不出来的值不该让电脑突然开口");
    }

    /// 默认每次问；写坏了也回到每次问，不替用户悄悄选一个。
    #[test]
    fn closing_asks_by_default_and_when_garbled() {
        assert_eq!(Settings::default().close_action, CloseAction::Ask);
        assert_eq!(
            Settings::parse("close=也许\n").close_action,
            CloseAction::Ask
        );
        assert_eq!(
            Settings::parse("close=quit\n").close_action,
            CloseAction::Quit
        );
        for action in [CloseAction::Ask, CloseAction::Tray, CloseAction::Quit] {
            assert_eq!(CloseAction::from_index(action.index()), action);
        }
    }

    /// 设置文件跟身份文件放在一起，搬机器的时候一起走。
    #[test]
    fn lives_next_to_the_identity_file() {
        let (Ok(settings), Ok(identity)) = (
            Settings::path(),
            voice_core::identity::Identity::default_path(),
        ) else {
            return; // 没有 %APPDATA% 的环境，跳过
        };
        assert_eq!(settings.parent(), identity.parent());
    }
}
