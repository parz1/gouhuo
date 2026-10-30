// SPDX-License-Identifier: GPL-3.0-or-later
#![cfg_attr(not(debug_assertions), windows_subsystem = "windows")]

//! 篝火客户端。
//!
//! 桌面组合根：连接事件更新 CallState，按钮提交 CallCommand，
//! CallViewModel 投影状态，SlintAdapter 同步控件。窗口、设备选择、
//! 身份和设置存储仍在桌面端；通话规则在 client-core / client-runtime。
//!
//! # 界面线程不碰网络
//!
//! `client-core` 的读线程收到消息之后往 channel 里塞事件，这里用
//! `upgrade_in_event_loop` 把更新搬回界面线程。界面线程从不阻塞在 socket 上，
//! 所以网络卡住了界面照样能动 —— 这在语音软件里是必须的，
//! 用户第一件想做的事就是点「离开」。

use std::cell::RefCell;
use std::rc::Rc;
use std::sync::mpsc::Receiver;
use std::sync::{Arc, Mutex};

use call::SlintAdapter;
use client_core::{Client, Ended, Event};
use client_process::RuntimeHandle;
use client_runtime::call::{
    AudioViewModel, CallCommand, CallController, CallState, CallViewModel, CommandResult,
};
use client_runtime::ipc::{Cue as Chime, StartVoice};
use client_runtime::self_state::ConnectionState;
use client_runtime::{recovery, Devices, RuntimeStage};
use slint::{ComponentHandle, Model, ModelRc, SharedString, VecModel};
use voice_core::identity::Identity;
use voice_types::TransmitMode;

mod call;
mod campfire;
mod clock;
mod diagnostic_log;
mod discover;
mod engine_update;
mod history_view;
mod home;
mod join;
mod platform;
mod scene;
mod session;
mod settings;
mod settings_page;
mod single_instance;
mod tray;
mod ui_timing;
mod update;
mod voice;

use clock::*;
use home::*;
use platform::*;
use scene::*;
use session::*;
use settings_page::*;
use tray::*;
use voice::*;

use settings::{
    db_to_level, level_to_db, snap_volume, CloseAction, Settings, SettingsWriter,
    SettingsWriterHandle, TalkMode,
};
use voice_core::hotkey::{Hotkeys, Key};

slint::include_modules!();

/// 窗口起不来时，换成软件渲染重开一次用的后端名。
const SOFTWARE_BACKEND: &str = "winit-software";

/// 标在重开的那个进程上：已经是退路了，再失败就别再重开，免得无限循环。
const FALLBACK_MARKER: &str = "GOUHUO_SOFTWARE_FALLBACK";

/// 哪一步失败的。只有窗口起不来值得换渲染器重试。
enum Failure {
    /// 建窗口、显示窗口失败 —— 多半是 OpenGL 用不了。
    Window(slint::PlatformError),
    /// 窗口已经起来过了，事件循环中途出错。重开没有意义。
    EventLoop(slint::PlatformError),
}

fn main() {
    // 已经有一个在跑了：把这次的链接交给它，自己退出。见 single_instance.rs。
    // 名额要攥到进程结束。
    let key = single_instance::key();
    let (_instance, already_running) = single_instance::claim(&key);
    if already_running {
        // 那边可能也刚启动、还没开始收（窗口建好才开始）：连着点两下图标就是这样。
        // 多等一会儿，别急着自己也开起来、把那边顶掉。
        let link = link_from_args().unwrap_or_default();
        for _ in 0..20 {
            if single_instance::forward(&key, &link) {
                return;
            }
            std::thread::sleep(std::time::Duration::from_millis(100));
        }
    }
    let error = match run(&key) {
        Ok(()) => return,
        Err(Failure::EventLoop(e)) => e,
        Err(Failure::Window(e)) => {
            // 界面默认用 OpenGL 画（Slint 的 FemtoVG）。没装显卡驱动的虚拟机、
            // 远程桌面、很老的核显上，OpenGL 上下文建不起来 —— 而 Slint 自己的
            // 兜底只管「渲染器对象建不出来」，建上下文是在显示窗口那一刻，兜不住。
            // 所以换成软件渲染，整个重开一次。慢一点，但能用。
            if std::env::var_os(FALLBACK_MARKER).is_none() && relaunch_with_software_renderer() {
                return;
            }
            e
        }
    };
    // 这是个窗口程序，没有控制台：只 eprintln 的话用户看到的就是
    // 「双击没反应」。必须弹出来。
    show_fatal(&format!(
        "篝火的窗口打不开：{error}\n\n\
         多半是显卡驱动的问题。装一下显卡驱动再试；\
         远程桌面里的话，换成在本机上打开。"
    ));
}

/// 带着同样的参数（可能有一条邀请链接）重开自己，这次用软件渲染。
/// 开起来了返回 `true`，这个进程就可以退了。
fn relaunch_with_software_renderer() -> bool {
    let Ok(exe) = std::env::current_exe() else {
        return false;
    };
    std::process::Command::new(exe)
        .args(std::env::args_os().skip(1))
        .env("SLINT_BACKEND", SOFTWARE_BACKEND)
        .env(FALLBACK_MARKER, "1")
        .spawn()
        .is_ok()
}

/// 只给测退路用：设了它，第一次建窗口就当失败处理，走跟 OpenGL 起不来一样的路。
/// 没有这个的话，那条路只有在一台没显卡驱动的机器上才测得到。
const SIMULATE_WINDOW_FAILURE: &str = "GOUHUO_SIMULATE_WINDOW_FAILURE";

fn run(instance_key: &str) -> Result<(), Failure> {
    ui_timing::mark_frame(ui_timing::FramePhase::Startup);
    if std::env::var_os(SIMULATE_WINDOW_FAILURE).is_some()
        && std::env::var_os(FALLBACK_MARKER).is_none()
    {
        return Err(Failure::Window(slint::PlatformError::Other(
            "模拟的窗口失败".into(),
        )));
    }
    let app = ui_timing::measure("App::new", App::new).map_err(Failure::Window)?;
    ui_timing::install_frame_probe(app.window());

    // 身份是本地的一对密钥，没有账号密码。第一次跑会生成一个。
    let (identity, first_run) = match load_identity() {
        Ok((identity, first_run)) => (Rc::new(identity), first_run),
        Err(e) => {
            app.set_error_headline("打不开你的身份文件".into());
            app.set_error_advice(
                format!("{e}\n检查一下 %APPDATA%\\gouhuo 这个目录是不是只读的。").into(),
            );
            app.run().map_err(Failure::Window)?;
            return Ok(());
        }
    };
    app.set_my_fingerprint(identity.public_key().fingerprint().to_grouped_hex().into());

    // 全局热键。起不来不该让客户端打不开 —— 语音激活那条路不需要它。
    let hotkeys = match Hotkeys::start() {
        Ok(hotkeys) => Some(Rc::new(hotkeys)),
        Err(e) => {
            eprintln!("全局热键起不来，按住说话用不了：{e}");
            None
        }
    };

    let stored = Settings::load();
    app.set_nick(if stored.nick.is_empty() {
        default_nick().into()
    } else {
        stored.nick.clone().into()
    });
    app.set_ptt_label(
        stored
            .ptt_key
            .map(|key| key.label())
            .unwrap_or_default()
            .into(),
    );
    app.set_vad_level(db_to_level(stored.vad_threshold_db));
    app.set_cue_sounds(stored.cue_sounds);
    app.set_announce_names(stored.announce_names);
    app.set_cue_volume(stored.cue_volume as f32 / 100.0);
    app.set_close_action(stored.close_action.index());
    app.set_check_updates(stored.check_updates);
    if let Some(hotkeys) = &hotkeys {
        hotkeys.set_ptt(stored.ptt_key);
    }

    // 用 Arc<Mutex<..>> 而不是 Rc<RefCell<..>>：连接结果要从后台线程
    // 搬回界面线程，那个闭包必须是 Send 的。
    let (runtime, engine_updates) = match engine_update::runtime(stored.check_updates) {
        Ok(runtime) => runtime,
        Err(error) => {
            show_fatal(&format!("语音运行线程无法启动：{error}"));
            return Ok(());
        }
    };
    runtime.handle().set_mode(transmit_mode(&stored));
    let settings_writer = match SettingsWriter::start() {
        Ok(writer) => writer,
        Err(error) => {
            show_fatal(&format!("设置保存线程无法启动：{error}"));
            return Ok(());
        }
    };
    let history_writer = Settings::path()
        .ok()
        .and_then(|path| path.parent().map(|parent| parent.to_owned()))
        .and_then(
            |directory| match client_process::history::Writer::start(directory) {
                Ok(writer) => Some(writer),
                Err(_) => {
                    eprintln!("连接历史后台线程无法启动；通话仍可继续。");
                    None
                }
            },
        );
    let state = Arc::new(Mutex::new(State {
        history: history_writer.as_ref().map(|writer| writer.handle()),
        runtime: Some(runtime.handle()),
        engine_updates: Some(engine_updates),
        settings_writer: Some(settings_writer.handle()),
        settings: stored,
        ..State::default()
    }));

    sync_audio(&app, &state);
    wire_home(&app, &identity, &state);
    refresh_home(&app, &state);
    wire_actions(&app, &state);
    wire_settings(&app, &state, hotkeys.clone());
    wire_scan(&app, &state);
    wire_close(&app, &state);
    wire_update(&app, &state);
    let tray = wire_tray(&app, &state);
    TRAY.with(|slot| *slot.borrow_mut() = tray);
    load_devices(&app, &state);
    spawn_status_poll(app.as_weak(), Arc::clone(&state));
    spawn_fire(app.as_weak());
    listen_for_other_instances(&app, &state, instance_key);
    if let Some(hotkeys) = hotkeys {
        spawn_ptt_poll(app.as_weak(), Arc::clone(&state), hotkeys);
    }

    // **先显示窗口，再自动连。** 窗口起不来的话整个进程会换软件渲染重开
    // （见 main）；要是先连上了，重开的那个再连一次，就把这边顶下去了。
    // 窗口要先创建出来才有句柄可以改标题栏。
    app.show().map_err(Failure::Window)?;
    dark_titlebar(&app);

    // 火的定时器平时由状态同步那边按需开（最慢半秒看一次）。刚打开窗口时
    // 首页就在眼前，别让火先愣半秒。
    FIRE_TIMER.with(|slot| {
        if let Some(timer) = slot.borrow().as_ref() {
            timer.restart();
        }
    });

    // **普通启动停在首页，不自动连。** 首页上最显眼的就是「回到上次的篝火」，
    // 回车就进；但要不要现在进、进哪一个，是用户说了算 —— 一打开就替他
    // 把麦克风接进昨天那个频道，不是每次都合适。
    //
    // 命令行上给了邀请链接就不一样了：Windows 把 `gouhuo://` 的协议处理器就是这么
    // 调起来的，点链接这个动作本身就是「我要进去」。老用户直接连，这才叫一键加入。
    //
    // **第一次跑的人不自动连**：那时昵称还是 Windows 用户名，而且身份刚生成，
    // 该让他先看一眼再进去，否则他会顶着 "admin" 出现在一屋子人面前。
    // 链接替他填好，他改完昵称点「加入」就行。
    if let Some(link) = link_from_args() {
        if first_run {
            app.set_address_input(link.as_str().into());
            app.set_home_mode(1);
            app.invoke_address_edited(link.into());
        } else {
            app.invoke_join_link(link.into());
        }
    }

    // **不能用 run_event_loop**：它在最后一个窗口隐藏时就返回，收到托盘等于退出。
    // 真正的退出走 quit_app。
    let result = slint::run_event_loop_until_quit();
    tear_down();
    // Also persist drafts from a slider whose release event was interrupted
    // by closing the window. Scheduling still performs no filesystem I/O.
    state.lock().expect("state poisoned").persist_settings();
    runtime.handle().shutdown();
    settings_writer.shutdown();
    // The event loop has ended; device cleanup can now be drained without
    // freezing a visible window. RuntimeHandle itself never joins threads.
    if !runtime.wait_stopped(std::time::Duration::from_secs(3)) {
        eprintln!("音频设备仍在回收，退出等待已结束。");
    }
    if !settings_writer.wait_stopped(std::time::Duration::from_secs(3)) {
        eprintln!("设置写入仍未完成，退出等待已结束。");
    }
    state.lock().expect("state poisoned").history.take();
    if let Some(writer) = history_writer {
        if !writer.finish(std::time::Duration::from_secs(2)) {
            eprintln!("连接历史写入未结束，退出等待已结束。");
        }
    }
    result.map_err(Failure::EventLoop)?;
    Ok(())
}

/// 检查更新：开关、「去下载」，以及启动时查一次。
fn wire_update(app: &App, state: &Arc<Mutex<State>>) {
    app.set_current_version(env!("CARGO_PKG_VERSION").into());
    {
        let state = Arc::clone(state);
        let weak = app.as_weak();
        app.on_set_check_updates(move |on| {
            let mut locked = state.lock().expect("state poisoned");
            locked.settings.check_updates = on;
            if let Some(updates) = &locked.engine_updates {
                updates.set_enabled(on);
            }
            locked.persist_settings();
            if let Some(app) = weak.upgrade() {
                app.set_check_updates(on);
            }
        });
    }
    // 检查结果提供的发布页与安装包地址；官方清单要求它们与更新源同源。
    let url = Arc::new(Mutex::new((String::new(), String::new())));
    {
        let url = Arc::clone(&url);
        app.on_open_update(move || open_in_browser(&url.lock().expect("url poisoned").0));
    }
    {
        let url = Arc::clone(&url);
        app.on_download_update(move || open_in_browser(&url.lock().expect("url poisoned").1));
    }
    let start: Rc<dyn Fn(bool)> = {
        let weak = app.as_weak();
        Rc::new(move |manual| {
            let Some(app) = weak.upgrade() else { return };
            if app.get_update_status() == 1 {
                return;
            }
            app.set_update_status(1);
            app.set_update_error("".into());
            let weak = weak.clone();
            let url = url.clone();
            update::check_in_background(manual, move |result| {
                let _ = weak.upgrade_in_event_loop(move |app| match result {
                    update::CheckResult::Available(available) => {
                        app.set_update_has_download(!available.download_url.is_empty());
                        app.set_update_notes(available.notes.into());
                        app.set_update_source(available.source.into());
                        *url.lock().expect("url poisoned") =
                            (available.url, available.download_url);
                        app.set_update_version(available.version.clone().into());
                        app.set_update_reminder_version(available.version.into());
                        app.set_update_status(3);
                        app.set_update_checked(true);
                        if manual {
                            app.set_show_update_dialog(true);
                        }
                    }
                    update::CheckResult::Current => {
                        app.set_update_status(2);
                        app.set_update_checked(true);
                        app.set_update_version("".into());
                        app.set_update_reminder_version("".into());
                        app.set_show_update_dialog(false);
                        *url.lock().expect("url poisoned") = (String::new(), String::new());
                        app.set_update_has_download(false);
                        app.set_update_notes("".into());
                    }
                    update::CheckResult::Failed => {
                        app.set_update_status(if app.get_update_version().is_empty() {
                            4
                        } else {
                            3
                        });
                        app.set_update_error("暂时无法连接发布服务，请检查网络后重试。".into());
                    }
                });
            });
        })
    };
    {
        let start = start.clone();
        app.on_check_update_now(move || start(true));
    }
    if state.lock().expect("state poisoned").settings.check_updates
        && update::automatic_check_enabled()
    {
        start(false);
    }
}

/// 收别的 `gouhuo.exe` 转交过来的链接（用户又点了一条邀请链接，或者又双击了图标）。
fn listen_for_other_instances(app: &App, state: &Arc<Mutex<State>>, key: &str) {
    let weak = app.as_weak();
    let state = Arc::clone(state);
    let served = single_instance::serve(key, move |message| {
        let state = Arc::clone(&state);
        let _ = weak.upgrade_in_event_loop(move |app| on_forwarded(&app, &state, &message));
    });
    if let Err(e) = served {
        // 没它也能用，只是点链接会再开一个、把这边顶下去（跟没做单实例时一样）。
        eprintln!("收不了别的实例转交的链接：{e}");
    }
}

fn on_forwarded(app: &App, state: &Arc<Mutex<State>>, message: &str) {
    // 不管带没带链接，先把窗口叫出来：收在托盘里的时候再点一次图标，
    // 用户要的就是看到窗口。
    let _ = app.show();
    app.window().set_minimized(false);
    bring_to_front(app);

    // 只认邀请链接，跟命令行参数一个规矩。
    let link = message.trim();
    if !link.starts_with(protocol::URL_PREFIX)
        || call_connection(state) == ConnectionState::Connecting
    {
        return;
    }
    if matches!(
        call_connection(state),
        ConnectionState::Connected | ConnectionState::Reconnecting
    ) {
        let current = state
            .lock()
            .expect("state poisoned")
            .settings
            .last_invite
            .clone();
        // 按解析出来的内容比，不按字面比：同一条链接大小写、有没有空白都可能不一样。
        let here = protocol::Invite::parse(&current);
        if here.is_ok() && here == protocol::Invite::parse(link) {
            return;
        }
        // 点了另一个服务器的链接：离开这边，去那边。点链接这个动作本身就是
        // 「我要去那儿」，再问一句只是多一步。
        app.invoke_leave();
    }
    app.invoke_join_link(link.into());
}

#[derive(Default)]
struct State {
    history: Option<client_process::history::Handle>,
    history_journey: Option<client_process::history::Journey>,
    history_generation: u64,
    history_attempt: u32,
    history_operation: Option<std::sync::mpsc::Receiver<client_process::history::OperationResult>>,
    history_view_version: Option<(u64, u64, u64, bool)>,
    diagnostic_log: diagnostic_log::Recorder,
    client: Option<Client>,
    call: CallState,
    recovery: client_runtime::health::CallHealth,
    recovery_epoch: Option<std::time::Instant>,
    recovery_history: std::collections::VecDeque<String>,
    /// Commands/snapshots only. The portable runtime owns and retires audio.
    runtime: Option<RuntimeHandle>,
    engine_updates: Option<engine_update::Controller>,
    settings: Settings,
    settings_writer: Option<SettingsWriterHandle>,
    /// 下拉框里第 n 项对应哪个设备 id。第 0 项是「系统默认」，所以是 None。
    capture_ids: Vec<Option<String>>,
    render_ids: Vec<Option<String>>,
    device_generation: u64,
    scan_generation: u64,
    scan_cancel: Option<Arc<std::sync::atomic::AtomicBool>>,
    /// 篝火上谁坐哪块石头。跨刷新保留 —— 坐下了就不挪。
    seats: campfire::SeatMap,
    /// 第几次「加入」。每开始一次、每取消一次都加一。
    ///
    /// 连接是在后台线程上阻塞着做的，取消不了它，只能不理它：结果回来时
    /// 这个数对不上，就说明用户已经不要了（或者又点了别的），那份结果直接丢掉。
    join_generation: u64,
    /// 停下来等用户的那个服务器：等他核对指纹，或者等他填加入码。
    pending: Option<join::Known>,
    /// 上一次要加入的是什么。「重试」就是把它原样再来一遍。
    last_request: Option<join::Request>,
    /// 首页上面那一大块现在说的是谁。`None` 就是最近用过的那个。
    ///
    /// 正在加入、或者刚失败的那个服务器不一定是最近用过的，也可能根本没存过 ——
    /// 这时候得把它摆在上面，不然「正在加入」「没能加入」说的是谁都不知道。
    hero: Option<Hero>,
    /// 第几次网络体检；同 `join_generation` 的用法：过期的结果直接丢。
    check_generation: u64,
    check_cancel: Option<Arc<std::sync::atomic::AtomicBool>>,
    /// 体检要的服务器指纹没人担保，正停在核对那一步；点了「指纹一致」之后
    /// 接着体检，而不是加入。
    check_after_trust: bool,
    /// 最近一次体检的报告原文，「保存为文件」存的就是它。
    check_report: Option<String>,
}

impl State {
    fn record_connection(
        &mut self,
        phase: client_process::history::Phase,
        cause: Option<protocol::connection::ConnectionCause>,
        session: Option<u32>,
        retry_in: Option<std::time::Duration>,
    ) {
        if let (Some(handle), Some(journey)) = (&self.history, &mut self.history_journey) {
            journey.record(
                handle,
                client_process::history::Transition {
                    phase,
                    cause,
                    session,
                    retry_in,
                    generation: self.history_generation,
                    attempt: self.history_attempt,
                },
            );
        }
        if phase == client_process::history::Phase::Ended {
            self.history_journey = None;
        }
    }

    /// Queue a complete preference snapshot; disk I/O belongs to the writer.
    fn persist_settings(&self) {
        if let Some(writer) = &self.settings_writer {
            writer.schedule(&self.settings);
        }
    }
}

thread_local! {
    /// 定时器丢掉就停了，所以要让它活到窗口关掉为止。
    static VOICE_TIMER: RefCell<Option<slint::Timer>> = const { RefCell::new(None) };
    static PTT_TIMER: RefCell<Option<slint::Timer>> = const { RefCell::new(None) };
    /// 等用户按键设置热键时，接收那个键的通道。
    static REBIND: RefCell<Option<std::sync::mpsc::Receiver<Key>>> =
        const { RefCell::new(None) };
    /// 托盘图标。丢掉它图标就没了，所以跟定时器一样放在这儿活到最后。
    /// `None` = 建不起来（系统不支持之类）—— 那样点 × 就只能退出，不能收起来。
    static TRAY: RefCell<Option<Tray>> = const { RefCell::new(None) };
    /// 篝火：画面多大、哪个频道的柴堆、火烧到哪儿了。
    /// 大小是界面报上来的，频道是名单刷新时定的；任何一个变了就重画底图。
    static CAMPFIRE: RefCell<campfire::Stage> = RefCell::new(campfire::Stage::default());
    /// 首页上那堆火：近景，没有人围着。跟频道里那堆各烧各的 ——
    /// 两边画面大小不一样，共用一个的话每次切换都要重新点火。
    static HOME_FIRE: RefCell<campfire::Stage> = RefCell::new(campfire::Stage::close_up());
    /// Width and height notifications are merged into one event-loop update.
    static SCENE_RESIZE: RefCell<Option<(f32, f32)>> = const { RefCell::new(None) };
    /// 让火动起来的定时器。只在篝火真的看得见时跑，见 `fire_pace`。
    static FIRE_TIMER: RefCell<Option<slint::Timer>> = const { RefCell::new(None) };
}

/// 每个麦克风试多久。
///
/// 太短的话用户还没来得及说出一个字就轮到下一个了；太长的话五个设备要等
/// 半分钟。1.2 秒够说一句「喂喂」，五个设备一共 6 秒。
const SCAN_PER_DEVICE: std::time::Duration = std::time::Duration::from_millis(1200);

/// Translate view callbacks into portable commands; local focus remains in Slint.
fn wire_actions(app: &App, state: &Arc<Mutex<State>>) {
    {
        let weak = app.as_weak();
        let state = Arc::clone(state);
        app.on_open_member(move |id| {
            let (Some(app), Some(client)) = (weak.upgrade(), current(&state)) else {
                return;
            };
            if !client.roster().users.contains_key(&(id as u32)) {
                return;
            }
            let rows = app.get_rows();
            let row = (0..rows.row_count())
                .filter_map(|i| rows.row_data(i))
                .find(|row| !row.is_channel && row.id == id);
            if let Some(row) = row {
                app.set_member_data(row);
                app.set_member_id(id);
            }
        });
    }
    // 每个回调都要拿到当前连接。写成一个小闭包而不是宏 ——
    // 回调的签名各不相同（有的带参数有的不带），宏反而要绕。

    {
        let weak = app.as_weak();
        app.on_campfire_resized(move |width, height| {
            let scheduled = SCENE_RESIZE
                .with(|pending| pending.borrow_mut().replace((width, height)).is_some());
            if scheduled {
                return;
            }
            let weak = weak.clone();
            let _ = slint::invoke_from_event_loop(move || {
                let size = SCENE_RESIZE.with(|pending| pending.borrow_mut().take());
                if let (Some(app), Some((width, height))) = (weak.upgrade(), size) {
                    resize_campfire(&app, width, height);
                }
            });
        });
    }

    {
        let state = Arc::clone(state);
        app.on_join_channel(move |id| {
            dispatch_call(&state, CallCommand::JoinChannel { channel: id as u32 });
        });
    }

    {
        let state = Arc::clone(state);
        app.on_create_channel(move |name| {
            dispatch_call(
                &state,
                CallCommand::CreateChannel {
                    name: name.to_string(),
                    parent: 0,
                },
            );
        });
    }

    {
        let state = Arc::clone(state);
        let weak = app.as_weak();
        app.on_set_user_volume(move |session, raw| {
            let Some(app) = weak.upgrade() else { return };
            let Some(CommandResult::VolumeChanged {
                public_key,
                percent,
                ..
            }) = dispatch_call(
                &state,
                CallCommand::SetVolume {
                    session: session as u32,
                    percent: snap_volume(raw),
                },
            )
            else {
                return;
            };
            state
                .lock()
                .expect("state poisoned")
                .settings
                .set_user_volume(&volume_key(&public_key), percent);
            if let Some(client) = current(&state) {
                refresh(&app, &state, &client);
            }
        });
    }

    {
        let state = Arc::clone(state);
        app.on_save_user_volumes(move || {
            state.lock().expect("state poisoned").persist_settings();
        });
    }

    {
        let state = Arc::clone(state);
        app.on_rename_channel(move |id, name| {
            dispatch_call(
                &state,
                CallCommand::RenameChannel {
                    channel: id as u32,
                    name: name.to_string(),
                },
            );
        });
    }

    {
        let state = Arc::clone(state);
        app.on_kick_user(move |session| {
            dispatch_call(
                &state,
                CallCommand::Kick {
                    session: session as u32,
                    reason: String::new(),
                },
            );
        });
    }

    {
        let state = Arc::clone(state);
        app.on_ban_user(move |session| {
            dispatch_call(
                &state,
                CallCommand::Ban {
                    session: session as u32,
                    reason: String::new(),
                },
            );
        });
    }

    {
        let state = Arc::clone(state);
        app.on_set_role(move |session, index| {
            // 下拉框的序号 0..=3 对应协议里的 1..=4（0 是 Unspecified）。
            let role = protocol::control::Role::try_from(index + 1)
                .unwrap_or(protocol::control::Role::Unspecified);
            dispatch_call(
                &state,
                CallCommand::SetRole {
                    session: session as u32,
                    role,
                },
            );
        });
    }

    {
        let state = Arc::clone(state);
        app.on_unban(move |key| {
            let Ok(key) = protocol::base32::decode(&key) else {
                return;
            };
            dispatch_call(&state, CallCommand::Unban { public_key: key });
        });
    }

    {
        let state = Arc::clone(state);
        app.on_delete_channel(move |id| {
            dispatch_call(&state, CallCommand::DeleteChannel { channel: id as u32 });
        });
    }

    {
        let state = Arc::clone(state);
        let weak = app.as_weak();
        app.on_send_text(move || {
            let Some(app) = weak.upgrade() else { return };
            if matches!(
                dispatch_call(
                    &state,
                    CallCommand::SendText {
                        body: app.get_draft().to_string()
                    }
                ),
                Some(
                    CommandResult::Applied
                        | CommandResult::Ignored(client_runtime::call::IgnoreReason::EmptyText)
                )
            ) {
                app.set_draft(SharedString::new());
            }
        });
    }

    {
        let state = Arc::clone(state);
        let weak = app.as_weak();
        app.on_toggle_mute(move || {
            let Some(app) = weak.upgrade() else { return };
            dispatch_call(&state, CallCommand::ToggleMute);
            sync_audio(&app, &state);
        });
    }

    {
        let state = Arc::clone(state);
        let weak = app.as_weak();
        app.on_toggle_deafen(move || {
            let Some(app) = weak.upgrade() else { return };
            dispatch_call(&state, CallCommand::ToggleDeafen);
            sync_audio(&app, &state);
        });
    }

    {
        let state = Arc::clone(state);
        let weak = app.as_weak();
        app.on_leave(move || {
            {
                let mut locked = state.lock().expect("state poisoned");
                let reason = if locked.call.connection() == ConnectionState::Reconnecting {
                    protocol::connection::ConnectionReason::ReconnectCancelled
                } else {
                    protocol::connection::ConnectionReason::UserLeft
                };
                locked.record_connection(
                    client_process::history::Phase::Ended,
                    Some(protocol::connection::ConnectionCause {
                        reason,
                        source: protocol::connection::EvidenceSource::UserAction,
                    }),
                    None,
                    None,
                );
            }
            cancel_scan(&state);
            dispatch_call(&state, CallCommand::Leave);
            {
                let mut locked = state.lock().expect("state poisoned");
                locked.client = None;
                locked.call.offline();
            }
            if let Some(app) = weak.upgrade() {
                app.invoke_reset_call_view();
                sync_audio(&app, &state);
                clear_join_error(&app);
                refresh_home(&app, &state);
            }
        });
    }
}

/// 事件循环停了之后、`main` 返回之前，把线程局部变量里带后台线程的东西收干净。
///
/// **不能留给进程退出时自动析构。** Windows 上 `main` 返回之后，系统先把别的线程
/// 全杀掉，然后才析构线程局部变量。按住说话的定时器里攥着全局热键（`Hotkeys`），
/// 它析构时要 join 自己的 Raw Input 线程 —— 那个线程已经被杀了，join 就 panic，
/// 析构里的 panic 直接 abort。调试版的 abort 会把进程卡在系统的错误报告上，
/// 表现为「点了退出，进程却一直挂在后台」。是不是撞上全看时序，所以时有时无。
fn tear_down() {
    PTT_TIMER.with(|slot| slot.borrow_mut().take());
    VOICE_TIMER.with(|slot| slot.borrow_mut().take());
    FIRE_TIMER.with(|slot| slot.borrow_mut().take());
    REBIND.with(|slot| slot.borrow_mut().take());
    TRAY.with(|slot| slot.borrow_mut().take());
}

/// 点窗口的 × 怎么办、托盘里点「退出」怎么办。
fn wire_close(app: &App, state: &Arc<Mutex<State>>) {
    {
        let weak = app.as_weak();
        let state = Arc::clone(state);
        app.window().on_close_requested(move || {
            use slint::CloseRequestResponse::{HideWindow, KeepWindowShown};
            let Some(app) = weak.upgrade() else {
                return HideWindow;
            };
            let has_tray = TRAY.with(|slot| slot.borrow().is_some());
            let (client, action) = {
                let locked = state.lock().expect("state poisoned");
                (locked.client.clone(), locked.settings.close_action)
            };
            // 没连着就没什么可「继续」的；没有托盘就没地方收。都直接退出。
            let Some(client) = client.filter(|_| {
                matches!(
                    call_connection(&state),
                    ConnectionState::Connected | ConnectionState::Reconnecting
                ) && has_tray
            }) else {
                quit_app(&state);
                return HideWindow;
            };
            match action {
                CloseAction::Tray => HideWindow,
                CloseAction::Quit => {
                    quit_app(&state);
                    HideWindow
                }
                CloseAction::Ask => {
                    app.set_close_prompt_channel(current_channel_name(&client).into());
                    app.set_close_prompt(true);
                    KeepWindowShown
                }
            }
        });
    }

    {
        let weak = app.as_weak();
        let state = Arc::clone(state);
        app.on_close_choice(move |to_tray, remember| {
            let Some(app) = weak.upgrade() else { return };
            app.set_close_prompt(false);
            if remember {
                let action = if to_tray {
                    CloseAction::Tray
                } else {
                    CloseAction::Quit
                };
                app.set_close_action(action.index());
                let mut locked = state.lock().expect("state poisoned");
                locked.settings.close_action = action;
                locked.persist_settings();
            }
            if to_tray {
                let _ = app.hide();
            } else {
                quit_app(&state);
            }
        });
    }

    {
        let weak = app.as_weak();
        let state = Arc::clone(state);
        app.on_set_close_action(move |index| {
            let action = CloseAction::from_index(index);
            if let Some(app) = weak.upgrade() {
                app.set_close_action(action.index());
            }
            let mut locked = state.lock().expect("state poisoned");
            locked.settings.close_action = action;
            locked.persist_settings();
        });
    }
}

/// 真的退出：先离开频道（服务端和别人那边马上看到你走了，而不是等 30 秒超时），
/// 再停掉事件循环。
fn quit_app(state: &Arc<Mutex<State>>) {
    state.lock().expect("state poisoned").record_connection(
        client_process::history::Phase::Ended,
        Some(protocol::connection::ConnectionCause {
            reason: protocol::connection::ConnectionReason::ApplicationExit,
            source: protocol::connection::EvidenceSource::UserAction,
        }),
        None,
        None,
    );
    cancel_scan(state);
    let client = {
        let locked = state.lock().expect("state poisoned");
        if let Some(runtime) = &locked.runtime {
            runtime.stop();
        }
        locked.client.clone()
    };
    if let Some(client) = client {
        client.disconnect();
    }
    let _ = slint::quit_event_loop();
}

fn current_channel_name(client: &Client) -> String {
    let roster = client.roster();
    roster
        .channels
        .get(&roster.my_channel())
        .map(|c| c.name.clone())
        .unwrap_or_default()
}

fn call_connection(state: &Arc<Mutex<State>>) -> ConnectionState {
    state.lock().expect("state poisoned").call.connection()
}

fn sync_audio(app: &App, state: &Arc<Mutex<State>>) {
    let (call, runtime, ptt_bound) = {
        let locked = state.lock().expect("state poisoned");
        (
            locked.call.clone(),
            locked.runtime.clone(),
            locked.settings.ptt_key.is_some(),
        )
    };
    if let Some(runtime) = runtime {
        let view = CallViewModel::project_audio(&call, &runtime.snapshot(), ptt_bound);
        SlintAdapter::apply_audio(app, &view);
        SlintAdapter::apply_activity(app, &view);
    }
}

fn dispatch_call(state: &Arc<Mutex<State>>, command: CallCommand) -> Option<CommandResult> {
    let (client, runtime) = {
        let locked = state.lock().expect("state poisoned");
        (locked.client.clone()?, locked.runtime.clone()?)
    };
    Some(CallController::dispatch(command, &client, &runtime))
}

/// 返回 `(身份, 是不是这次新建的)`。
fn current_runtime(state: &Arc<Mutex<State>>) -> Option<RuntimeHandle> {
    state.lock().expect("state poisoned").runtime.clone()
}

fn current(state: &Arc<Mutex<State>>) -> Option<Client> {
    state.lock().expect("state poisoned").client.clone()
}

fn current_voice(state: &Arc<Mutex<State>>) -> Option<RuntimeHandle> {
    let locked = state.lock().expect("state poisoned");
    locked.client.as_ref()?;
    locked.runtime.clone()
}

fn load_identity() -> std::io::Result<(Identity, bool)> {
    let path = Identity::default_path()?;
    Identity::load_or_create(&path)
}

/// 从命令行里挑出邀请链接。
///
/// 只认 `gouhuo://` 开头的那个参数，别的一概不管 —— 协议处理器被调起来时，
/// 参数里可能还夹着别的东西，而把任意一个参数当链接用是个很好的注入入口。
fn link_from_args() -> Option<String> {
    std::env::args()
        .skip(1)
        .find(|arg| arg.starts_with(protocol::URL_PREFIX))
}

/// 默认昵称用 Windows 的用户名 —— 比让用户对着空框发呆强。
fn default_nick() -> String {
    std::env::var("USERNAME")
        .or_else(|_| std::env::var("USER"))
        .map(|name| name.trim().to_string())
        .ok()
        .filter(|name| !name.is_empty())
        .unwrap_or_else(|| "玩家".to_string())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn default_nick_is_never_empty() {
        assert!(!default_nick().is_empty());
    }
}
