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

/// 多久去问一次语音链路的状态。
///
/// 定这个数的是**电平条**：200 ms 的条子看着是一跳一跳的，用户会以为卡了，
/// 而它恰恰是用来判断「麦克风有没有在动」的，跟不上就失去了意义。
/// 说话指示也跟着受益。
///
/// 每秒 20 次读几个原子变量加两次加锁，跟音频线程各自每秒 100 次比
/// 完全不在一个量级上。
const VOICE_POLL: std::time::Duration = std::time::Duration::from_millis(50);

/// 多久看一次按住说话的键有没有被按下。
///
/// 这个数直接决定「按下去到开始发声」的延迟，所以要比语音状态那个快得多。
/// 20 ms 是两帧音频，用户感觉不出来；再快就只是在空转了。
const PTT_POLL: std::time::Duration = std::time::Duration::from_millis(20);

/// 窗口看不见（最小化、收在托盘里）时，界面同步多久一次。
///
/// 这时候电平条、谁在说话都没人看，只剩托盘图标要跟着麦克风状态走，以及发现窗口
/// 被恢复了、切回 [`VOICE_POLL`]。半秒够：恢复窗口后电平条最多晚半秒动起来。
///
/// 为什么要管这个：篝火真正的用法是进游戏之后挂在后台几个小时。实测（#11）改之前
/// 界面线程不管窗口在不在都每秒醒 190 次左右，跟游戏抢的就是这种零碎的调度。
const BACKGROUND_POLL: std::time::Duration = std::time::Duration::from_millis(500);

/// 篝火的火苗多久动一下。10 帧：像素画本来就是一跳一跳的，再快看不出区别，
/// 只是多花 CPU —— 而这个窗口是要挂在游戏旁边一晚上的。
const FIRE_FRAME: std::time::Duration = std::time::Duration::from_millis(100);

/// 窗口不在前台时（比如放在副屏上）火慢下来：4 帧，看得出在烧就够了。
const FIRE_FRAME_IDLE: std::time::Duration = std::time::Duration::from_millis(250);

mod call;
mod campfire;
mod discover;
mod engine_update;
mod join;
mod settings;
mod single_instance;
mod ui_timing;
mod update;

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

#[cfg(windows)]
fn show_fatal(message: &str) {
    use windows_sys::Win32::UI::WindowsAndMessaging::{MessageBoxW, MB_ICONERROR, MB_OK};
    let wide = |s: &str| s.encode_utf16().chain(Some(0)).collect::<Vec<u16>>();
    let (text, caption) = (wide(message), wide("篝火"));
    // SAFETY: 两个指针都指向以 0 结尾的 UTF-16，活到调用结束。
    unsafe {
        MessageBoxW(
            std::ptr::null_mut(),
            text.as_ptr(),
            caption.as_ptr(),
            MB_OK | MB_ICONERROR,
        );
    }
}

#[cfg(not(windows))]
fn show_fatal(message: &str) {
    eprintln!("{message}");
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
    let state = Arc::new(Mutex::new(State {
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

#[cfg(windows)]
fn open_in_browser(url: &str) {
    use windows_sys::Win32::UI::Shell::ShellExecuteW;
    use windows_sys::Win32::UI::WindowsAndMessaging::SW_SHOWNORMAL;
    // 只打开 https 的网址：ShellExecute 什么都能「打开」，包括本地程序。
    if !url.starts_with("https://") {
        return;
    }
    let wide = |s: &str| s.encode_utf16().chain(Some(0)).collect::<Vec<u16>>();
    let (verb, target) = (wide("open"), wide(url));
    // SAFETY: 两个字符串以 0 结尾，活到调用结束；其余参数为空。
    unsafe {
        ShellExecuteW(
            std::ptr::null_mut(),
            verb.as_ptr(),
            target.as_ptr(),
            std::ptr::null(),
            std::ptr::null(),
            SW_SHOWNORMAL,
        );
    }
}

#[cfg(not(windows))]
fn open_in_browser(_url: &str) {}

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

/// 把窗口拉到最前面。转交的那个进程已经用 AllowSetForegroundWindow 把权限让给我们了，
/// 不然 Windows 只会让任务栏按钮闪一下。
#[cfg(windows)]
fn bring_to_front(app: &App) {
    use raw_window_handle::{HasWindowHandle, RawWindowHandle};
    use windows_sys::Win32::UI::WindowsAndMessaging::SetForegroundWindow;

    let handle = app.window().window_handle();
    let Ok(handle) = handle.window_handle() else {
        return;
    };
    let RawWindowHandle::Win32(win32) = handle.as_raw() else {
        return;
    };
    // SAFETY: hwnd 来自窗口系统本身。
    unsafe { SetForegroundWindow(win32.hwnd.get() as *mut core::ffi::c_void) };
}

#[cfg(not(windows))]
fn bring_to_front(_app: &App) {}

/// 让标题栏跟着窗口一起是深色的。
///
/// 深色应用配一条浅色标题栏，是「界面停留在 2010 年」最典型的症状之一：
/// 整个窗口看着像是两个程序拼起来的。Windows 10 1809 起支持这个属性，
/// 更早的版本上这次调用会失败，忽略就行 —— 老系统上只是标题栏还是浅色。
#[cfg(windows)]
fn dark_titlebar(app: &App) {
    use raw_window_handle::{HasWindowHandle, RawWindowHandle};
    use windows_sys::Win32::Graphics::Dwm::DwmSetWindowAttribute;

    /// DWMWA_USE_IMMERSIVE_DARK_MODE
    const DARK_MODE: u32 = 20;

    let handle = app.window().window_handle();
    let Ok(handle) = handle.window_handle() else {
        return;
    };
    let RawWindowHandle::Win32(win32) = handle.as_raw() else {
        return;
    };
    let on: i32 = 1;
    // SAFETY: hwnd 来自窗口系统本身，属性值是一个我们自己的 i32，长度也是它的大小。
    unsafe {
        DwmSetWindowAttribute(
            win32.hwnd.get() as *mut core::ffi::c_void,
            DARK_MODE,
            (&on as *const i32).cast(),
            std::mem::size_of::<i32>() as u32,
        );
    }
}

#[cfg(not(windows))]
fn dark_titlebar(_app: &App) {}

#[derive(Default)]
struct State {
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
}

impl State {
    /// Queue a complete preference snapshot; disk I/O belongs to the writer.
    fn persist_settings(&self) {
        if let Some(writer) = &self.settings_writer {
            writer.schedule(&self.settings);
        }
    }
}

#[derive(Clone)]
struct Hero {
    title: String,
    address: String,
    /// 柴堆的种子。
    seed: u32,
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

/// 首页上的每一个动作：回到存着的服务器、按输的地址加入、填加入码、核对指纹、
/// 取消、重试、移除。
///
/// 它们最后都落到同一个地方（[`begin_join`]）：把「要加入什么」交给后台线程去办，
/// 办到哪一步、卡在哪儿，再由 [`finish_join`] 搬回界面上。怎么办的见 `join.rs`。
fn wire_home(app: &App, identity: &Rc<Identity>, state: &Arc<Mutex<State>>) {
    let begin: Rc<dyn Fn(join::Request)> = {
        let weak = app.as_weak();
        let identity = Rc::clone(identity);
        let state = Arc::clone(state);
        Rc::new(move |request| {
            if let Some(app) = weak.upgrade() {
                begin_join(&app, &identity, &state, request);
            }
        })
    };

    {
        // 点链接启动、别的实例转交过来的链接。
        let begin = Rc::clone(&begin);
        app.on_join_link(move |link| {
            begin(join::Request::Address {
                text: link.to_string(),
                fresh: false,
            });
        });
    }

    {
        let begin = Rc::clone(&begin);
        let state = Arc::clone(state);
        app.on_join_saved(move |id| {
            let saved = state
                .lock()
                .expect("state poisoned")
                .settings
                .servers
                .get(id.max(0) as usize)
                .and_then(join::Known::from_saved);
            if let Some(known) = saved {
                begin(join::Request::Known(known));
            }
        });
    }

    {
        let weak = app.as_weak();
        let state = Arc::clone(state);
        app.on_address_edited(move |text| {
            if let Some(app) = weak.upgrade() {
                // 还在输的时候，空的不算错 —— 不然光标一进框就是一行红字。
                if text.trim().is_empty() {
                    app.set_address_hint("".into());
                    app.set_address_bad(false);
                } else {
                    check_address(&app, &state, &text);
                }
            }
        });
    }

    {
        let begin = Rc::clone(&begin);
        let weak = app.as_weak();
        let state = Arc::clone(state);
        app.on_join_address(move || {
            let Some(app) = weak.upgrade() else { return };
            let text = app.get_address_input().to_string();
            // 认不出来的东西不拿去连：原因就写在框下面，改了再点。
            if check_address(&app, &state, &text) {
                begin(join::Request::Address { text, fresh: false });
            }
        });
    }

    {
        let begin = Rc::clone(&begin);
        let weak = app.as_weak();
        let state = Arc::clone(state);
        app.on_submit_code(move || {
            let Some(app) = weak.upgrade() else { return };
            let code = app.get_join_code().trim().to_string();
            let pending = state.lock().expect("state poisoned").pending.clone();
            let (Some(mut known), false) = (pending, code.is_empty()) else {
                return;
            };
            known.invite.code = Some(code);
            begin(join::Request::Known(known));
        });
    }

    {
        // 用户核对过指纹了：从这一刻起它就是固定下来的，跟邀请链接里带的一样。
        let begin = Rc::clone(&begin);
        let state = Arc::clone(state);
        app.on_trust_server(move || {
            let pending = state.lock().expect("state poisoned").pending.clone();
            if let Some(known) = pending {
                begin(join::Request::Known(known));
            }
        });
    }

    {
        let begin = Rc::clone(&begin);
        let state = Arc::clone(state);
        app.on_retry_join(move || {
            let last = state.lock().expect("state poisoned").last_request.clone();
            if let Some(request) = last {
                begin(request);
            }
        });
    }

    {
        // 证书对不上：重新去取这台服务器的指纹。有加入页就问加入页（CA 证书担保，
        // 不用用户核对）；没有就直接取，取回来照样要用户核对 —— 服务器重装了
        // 和有人在中间，从这边看是一模一样的。
        let begin = Rc::clone(&begin);
        let state = Arc::clone(state);
        app.on_reverify_server(move || {
            let last = state.lock().expect("state poisoned").last_request.clone();
            let text = match last {
                Some(join::Request::Known(known)) => known.page.unwrap_or_else(|| {
                    let host = client_core::address::bracketed(&known.invite.host);
                    if known.invite.port == protocol::DEFAULT_PORT {
                        host
                    } else {
                        format!("{host}:{}", known.invite.port)
                    }
                }),
                Some(join::Request::Address { text, .. }) => text,
                None => return,
            };
            begin(join::Request::Address { text, fresh: true });
        });
    }

    {
        let weak = app.as_weak();
        let state = Arc::clone(state);
        app.on_cancel_join(move || {
            let Some(app) = weak.upgrade() else { return };
            {
                let mut locked = state.lock().expect("state poisoned");
                // 后台那次连接掐不掉，只能不认它的结果，见 State::join_generation。
                locked.join_generation += 1;
                locked.pending = None;
                locked.hero = None;
                locked.call.offline();
            }
            sync_audio(&app, &state);
            app.set_connect_stage("".into());
            app.set_home_mode(0);
            app.set_join_code("".into());
            app.set_code_rejected(false);
            clear_join_error(&app);
            refresh_home(&app, &state);
        });
    }

    {
        let weak = app.as_weak();
        let state = Arc::clone(state);
        app.on_forget_server(move |id| {
            {
                let mut locked = state.lock().expect("state poisoned");
                locked.settings.forget_server(id.max(0) as usize);
                locked.persist_settings();
            }
            if let Some(app) = weak.upgrade() {
                refresh_home(&app, &state);
            }
        });
    }

    {
        let weak = app.as_weak();
        app.on_home_fire_resized(move |width, height| {
            let Some(app) = weak.upgrade() else { return };
            let frame = HOME_FIRE.with(|stage| {
                let mut stage = stage.borrow_mut();
                stage.set_size((width, height)).then(|| stage.still())
            });
            if let Some(frame) = frame {
                app.set_home_fire(frame);
            }
        });
    }
}

/// 认一下地址框里的东西，把结果写到框下面那行字上。认得出来返回 `true`。
fn check_address(app: &App, state: &Arc<Mutex<State>>, text: &str) -> bool {
    let saved = state
        .lock()
        .expect("state poisoned")
        .settings
        .servers
        .clone();
    let described = join::describe(text, &saved);
    app.set_address_bad(described.is_err());
    match described {
        Ok(hint) => {
            app.set_address_hint(hint.into());
            true
        }
        Err(problem) => {
            app.set_address_hint(problem.into());
            false
        }
    }
}

fn clear_join_error(app: &App) {
    app.set_error_headline("".into());
    app.set_error_advice("".into());
    app.set_error_can_reverify(false);
}

/// 柴堆的种子：按服务器的证书指纹算，所以每个服务器门口是不一样的一堆柴。
fn fire_seed(invite: &protocol::Invite) -> u32 {
    let f = invite.cert.0;
    u32::from_le_bytes([f[0], f[1], f[2], f[3]])
}

fn hero_of(known: &join::Known) -> Hero {
    Hero {
        title: known.title(),
        address: known.address(),
        seed: fire_seed(&known.invite),
    }
}

/// 开始加入。连接会阻塞（域名解析、HTTPS、TCP、TLS 握手，最长十几秒），
/// **不能在界面线程上做** —— 否则窗口会白到超时为止，用户以为程序死了。
fn begin_join(
    app: &App,
    identity: &Rc<Identity>,
    state: &Arc<Mutex<State>>,
    request: join::Request,
) {
    if call_connection(state) == ConnectionState::Connecting {
        return;
    }
    let nick = {
        let typed = app.get_nick().to_string();
        let trimmed = typed.trim().to_string();
        if trimmed.is_empty() {
            default_nick()
        } else {
            trimmed
        }
    };
    // 身份只有一份，不能移进后台线程。导出再导入拿一份副本 ——
    // 这条路径本来就要能跑（换机器就是靠它），顺手也验了一次。
    let Ok(identity) = Identity::import(&identity.export()) else {
        return;
    };

    // 上面那一大块先换成「正在加入谁」。输的是地址的话，这时候还不知道名字，
    // 先显示地址；**绝不显示原文** —— 邀请链接和 `#code=` 里都可能带着加入码。
    let hero = match &request {
        join::Request::Known(known) => hero_of(known),
        join::Request::Address { text, .. } => {
            use client_core::address::{display_address, parse, Target};
            let shown = match parse(text, join::allow_loopback_http()) {
                Ok(Target::Invite(invite)) => display_address(&invite.host, invite.port),
                Ok(Target::Page(page)) => page.host,
                Ok(Target::Host(host)) => display_address(&host.host, host.port_or_default()),
                Err(_) => "…".to_string(),
            };
            Hero {
                title: shown.clone(),
                address: shown,
                seed: 0,
            }
        }
    };

    let (generation, saved) = {
        let mut locked = state.lock().expect("state poisoned");
        locked.join_generation += 1;
        locked.pending = None;
        locked.last_request = Some(request.clone());
        locked.hero = Some(hero);
        locked.call.begin_join();
        (locked.join_generation, locked.settings.servers.clone())
    };
    sync_audio(app, state);
    app.set_connect_stage("正在准备…".into());
    app.set_home_mode(0);
    app.set_code_rejected(false);
    clear_join_error(app);
    refresh_home(app, state);

    let weak = app.as_weak();
    let state = Arc::clone(state);
    std::thread::spawn(move || {
        // 「现在在干什么」搬回界面线程。已经被取消了的那一次就别再往界面上写了。
        let progress = {
            let weak = weak.clone();
            let state = Arc::clone(&state);
            move |stage: &str, server: Option<&join::Known>| {
                let stage = stage.to_string();
                let hero = server.map(hero_of);
                let state = Arc::clone(&state);
                let _ = weak.upgrade_in_event_loop(move |app| {
                    {
                        let mut locked = state.lock().expect("state poisoned");
                        if locked.join_generation != generation {
                            return;
                        }
                        if let Some(hero) = hero {
                            locked.hero = Some(hero);
                        }
                    }
                    app.set_connect_stage(stage.into());
                    refresh_home(&app, &state);
                });
            }
        };
        let outcome = join::run(request, &identity, &nick, &saved, &progress);
        let _ = weak.upgrade_in_event_loop(move |app| {
            finish_join(&app, &state, generation, outcome, &nick);
        });
    });
}

/// 后台那次加入有结果了：进去了、要用户核对指纹、要加入码，或者没成。
fn finish_join(
    app: &App,
    state: &Arc<Mutex<State>>,
    generation: u64,
    outcome: join::Outcome,
    nick: &str,
) {
    if state.lock().expect("state poisoned").join_generation != generation {
        // 用户已经取消了。要是偏偏连上了，得把它断掉 ——
        // 不然服务器上会多出一个谁也看不见、还开着麦的「我」。
        if let join::Outcome::Joined { client, .. } = outcome {
            client.disconnect();
        }
        return;
    }
    state.lock().expect("state poisoned").call.offline();
    sync_audio(app, state);
    app.set_connect_stage("".into());

    match outcome {
        join::Outcome::Joined {
            client,
            events,
            server,
        } => on_connected(app, state, client, events, nick, server),
        join::Outcome::ConfirmFingerprint(server) => {
            app.set_trust_fingerprint(server.invite.cert.to_grouped_hex().into());
            app.set_home_mode(3);
            let mut locked = state.lock().expect("state poisoned");
            locked.hero = Some(hero_of(&server));
            locked.pending = Some(server);
        }
        join::Outcome::NeedCode { server, rejected } => {
            app.set_join_code("".into());
            app.set_code_rejected(rejected);
            app.set_home_mode(2);
            let mut locked = state.lock().expect("state poisoned");
            locked.hero = Some(hero_of(&server));
            locked.pending = Some(server);
        }
        join::Outcome::Failed(failure) => {
            app.set_error_headline(failure.headline.into());
            app.set_error_advice(failure.advice.into());
            app.set_error_can_reverify(failure.certificate_changed);
            app.set_home_mode(0);
            let mut locked = state.lock().expect("state poisoned");
            let seed = locked.hero.as_ref().map(|h| h.seed).unwrap_or(0);
            locked.hero = Some(Hero {
                title: failure.title,
                address: failure.address,
                seed,
            });
        }
    }
    refresh_home(app, state);
}

/// 把存着的服务器摆到首页上：最近用的那个在上面那一大块，别的排在下面。
///
/// 每次都整个重建。最多二十行，而且只在加入、离开、移除的时候才调。
fn refresh_home(app: &App, state: &Arc<Mutex<State>>) {
    let (servers, hero) = {
        let locked = state.lock().expect("state poisoned");
        (locked.settings.servers.clone(), locked.hero.clone())
    };
    let now = unix_now();
    // (在存着的服务器里排第几, 怎么连, 上次什么时候来的)
    let known: Vec<(i32, join::Known, u64)> = servers
        .iter()
        .enumerate()
        .filter_map(|(index, saved)| {
            join::Known::from_saved(saved).map(|k| (index as i32, k, saved.last_used))
        })
        .collect();

    let (hero_id, title, address, used, seed) = match (&hero, known.first()) {
        (Some(hero), _) => {
            // 正在加入的要是正好是存着的某一个，下面那一排里就别再出现一次。
            let id = known
                .iter()
                .find(|(_, k, _)| k.address() == hero.address)
                .map(|(id, _, _)| *id)
                .unwrap_or(-1);
            (
                id,
                hero.title.clone(),
                hero.address.clone(),
                String::new(),
                hero.seed,
            )
        }
        (None, Some((id, recent, last_used))) => (
            *id,
            recent.title(),
            recent.address(),
            ago(now, *last_used),
            fire_seed(&recent.invite),
        ),
        (None, None) => (-1, String::new(), String::new(), String::new(), 0),
    };

    app.set_has_hero(hero.is_some() || !known.is_empty());
    app.set_has_servers(!known.is_empty());
    app.set_hero_id(hero_id);
    app.set_hero_name(title.into());
    app.set_hero_address(address.into());
    app.set_hero_used(used.into());

    let others: Vec<ServerRow> = known
        .iter()
        .filter(|(id, _, _)| *id != hero_id)
        .map(|(id, k, last_used)| ServerRow {
            id: *id,
            name: k.name.clone().into(),
            address: k.address().into(),
            used: ago(now, *last_used).into(),
        })
        .collect();
    app.set_other_servers(ModelRc::new(VecModel::from(others)));

    // 换了服务器就换一堆柴。
    let frame = HOME_FIRE.with(|stage| {
        let mut stage = stage.borrow_mut();
        stage.set_channel(seed).then(|| stage.still())
    });
    if let Some(frame) = frame {
        app.set_home_fire(frame);
    }
}

fn unix_now() -> u64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_secs())
        .unwrap_or(0)
}

/// 「上次什么时候来的」：刚刚 / 今天 21:40 / 昨天 21:40 / 3 天前 / 9月27日。
///
/// `then` 是 0 表示不知道（从老版本升上来的那一条），什么都不写。
fn ago(now: u64, then: u64) -> String {
    if then == 0 || then > now + 60 {
        return String::new();
    }
    if now.saturating_sub(then) < 90 {
        return "刚刚".to_string();
    }
    let offset = local_offset_seconds();
    let day = |t: u64| (t as i64 + offset).div_euclid(86_400);
    let days = day(now) - day(then);
    match days {
        0 => format!("今天 {}", clock_time(then as i64 * 1000)),
        1 => format!("昨天 {}", clock_time(then as i64 * 1000)),
        2..=6 => format!("{days} 天前"),
        _ => clock_date(then as i64 * 1000),
    }
}

fn on_connected(
    app: &App,
    state: &Arc<Mutex<State>>,
    client: Client,
    events: Receiver<Event>,
    nick: &str,
    server: join::Known,
) {
    let presented_at = std::time::Instant::now();
    ui_timing::mark_frame(ui_timing::FramePhase::CallEnter);
    app.invoke_reset_call_view();
    // 服务端可能改过昵称（重名会加后缀），以它给的为准。
    let actual = {
        let roster = client.roster();
        roster.name_of(roster.me)
    };
    app.set_nick(if actual.is_empty() {
        nick.into()
    } else {
        actual.into()
    });

    {
        // 连上了才存 —— 存一个连不上的服务器只会让下次打开就看到一个错误。
        // 不管是粘链接、输域名还是输 IP 核对指纹进来的，存下来都是同一种东西
        // （地址 + 固定的指纹 + 加入码），下次从首页一点就进。
        let mut locked = state.lock().expect("state poisoned");
        locked.client = Some(client.clone());
        locked.call.connected();
        locked.call.set_notice("");
        locked.recovery = client_runtime::health::CallHealth::default();
        locked.recovery_epoch = Some(std::time::Instant::now());
        locked.recovery_history.clear();
        locked.settings.nick = app.get_nick().to_string();
        locked.settings.remember_server(
            &server.invite,
            &server.name,
            server.page.as_deref(),
            unix_now(),
        );
        locked.persist_settings();
        locked.hero = None;
        locked.pending = None;
    }
    // 频道树顶上那一行：加入页给的名字，没有就是地址。
    app.set_server_name(server.title().into());
    // 首页收拾干净，离开频道回来时是它平常的样子。
    app.set_home_mode(0);
    app.set_address_input("".into());
    app.set_address_hint("".into());
    app.set_address_bad(false);
    app.set_join_code("".into());
    clear_join_error(app);
    refresh_home(app, state);
    app.set_member_id(-1);
    if let Some(runtime) = current_runtime(state) {
        runtime.set_self_state(false, false);
        runtime.set_transmitting(false);
        runtime.set_monitoring(false);
        runtime.clear_volumes();
    }
    refresh(app, state, &client);

    start_voice(app, state, &client);
    ui_timing::record("join result -> call state", presented_at.elapsed());
    pump_events(app.as_weak(), Arc::clone(state), client, events);
}

/// 起语音链路，并开一个定时器把它的状态搬到界面上。
///
/// 设备打不开不该让人掉线 —— 文字和名单照样能用，只是没声音。所以这里
/// 失败只是把原因显示出来，不动连接。
fn start_voice(app: &App, state: &Arc<Mutex<State>>, client: &Client) {
    // A scan owns a microphone until its background function returns. Starting
    // a call cancels it and its completion resumes the latest session.
    if cancel_scan(state) {
        return;
    }
    let Some(runtime) = current_runtime(state) else {
        return;
    };
    let (session_id, server, keys) = client.voice_endpoint_session();
    let settings = state.lock().expect("state poisoned").settings.clone();
    runtime.set_mode(transmit_mode(&settings));
    // Replacements preserve local intent. Volumes are staged before admission
    // so a candidate cannot send with stale/default controls.
    apply_volumes(state, client);
    runtime.start_voice(StartVoice {
        host: server.ip().to_string(),
        udp_port: server.port(),
        session_id,
        upstream_key: *keys.upstream.as_bytes(),
        downstream_key: *keys.downstream.as_bytes(),
        devices: Devices {
            capture: settings.capture_device,
            render: settings.render_device,
        },
    });
    sync_audio(app, state);
}

/// 把设备列表灌进下拉框，并记下「第 n 项是哪个 id」。
///
/// 第 0 项永远是「系统默认」—— 绝大多数人不该需要管这个，
/// 而且它是唯一在换了耳机之后还能跟着走的选项。
fn load_devices(app: &App, state: &Arc<Mutex<State>>) {
    let generation = {
        let mut locked = state.lock().expect("state poisoned");
        locked.device_generation = locked.device_generation.wrapping_add(1);
        if locked.capture_ids.is_empty() {
            locked.capture_ids.push(None);
            locked.render_ids.push(None);
            app.set_capture_devices(ModelRc::new(VecModel::from(vec!["系统默认".into()])));
            app.set_render_devices(ModelRc::new(VecModel::from(vec!["系统默认".into()])));
        }
        locked.device_generation
    };
    let weak = app.as_weak();
    let state = Arc::clone(state);
    std::thread::spawn(move || {
        let at = std::time::Instant::now();
        let (capture, render) = current_runtime(&state)
            .and_then(|runtime| runtime.devices().ok())
            .unwrap_or_default();
        let lists: Vec<_> = [capture, render]
            .into_iter()
            .map(|endpoints| {
                let mut labels = vec![String::from("系统默认")];
                let mut ids = vec![None];
                for endpoint in endpoints {
                    labels.push(format!(
                        "{}{}",
                        endpoint.name,
                        if endpoint.is_hardware {
                            ""
                        } else {
                            "（虚拟）"
                        }
                    ));
                    ids.push(Some(endpoint.id));
                }
                (labels, ids)
            })
            .collect();
        ui_timing::record("device enumeration (background)", at.elapsed());
        let _ = weak.upgrade_in_event_loop(move |app| {
            let mut locked = state.lock().expect("state poisoned");
            if generation != locked.device_generation {
                return;
            }
            let mut models = Vec::new();
            for (is_capture, (labels, ids)) in [true, false].into_iter().zip(lists) {
                let saved = if is_capture {
                    &locked.settings.capture_device
                } else {
                    &locked.settings.render_device
                };
                let index = ids.iter().position(|id| id == saved).unwrap_or(0) as i32;
                let labels: Vec<SharedString> = labels.into_iter().map(Into::into).collect();
                models.push((is_capture, index, ModelRc::new(VecModel::from(labels))));
                if is_capture {
                    locked.capture_ids = ids;
                } else {
                    locked.render_ids = ids;
                }
            }
            drop(locked);
            for (is_capture, index, model) in models {
                if is_capture {
                    app.set_capture_devices(model);
                    app.set_capture_index(index);
                } else {
                    app.set_render_devices(model);
                    app.set_render_index(index);
                }
            }
        });
    });
}

/// 换设备。链路要重起 —— WASAPI 的流是绑在设备上的，换不了。
///
/// 重起会让声音断一下（几十毫秒）。这是换设备本来就该有的代价，
/// 比为了热切换在音频线程里加一套状态机划算得多。
fn restart_voice(app: &App, state: &Arc<Mutex<State>>) {
    if call_connection(state) == ConnectionState::Reconnecting {
        return;
    }
    if cancel_scan(state) {
        return;
    }
    if let Some(client) = current(state) {
        start_voice(app, state, &client);
    } else {
        start_mic_check(app, state);
    }
}

/// 每个麦克风试多久。
///
/// 太短的话用户还没来得及说出一个字就轮到下一个了；太长的话五个设备要等
/// 半分钟。1.2 秒够说一句「喂喂」，五个设备一共 6 秒。
const SCAN_PER_DEVICE: std::time::Duration = std::time::Duration::from_millis(1200);

/// 挨个试每个麦克风，把结果显示出来。
///
/// 在后台线程上跑：每个设备都要真的打开一次，那几秒里界面不能是卡死的。
fn wire_scan(app: &App, state: &Arc<Mutex<State>>) {
    let state = Arc::clone(state);
    let weak = app.as_weak();
    app.on_scan_microphones(move || {
        let Some(app) = weak.upgrade() else { return };
        // A scan is an offline device operation. It must not compete with a
        // live call; changing a call's microphone uses the runtime instead.
        if app.get_scanning() || current(&state).is_some() {
            return;
        }
        stop_mic_check(&state);
        let draining = current_runtime(&state).map(|runtime| {
            let id = runtime.snapshot().request_id;
            (runtime, id)
        });
        let cancel = Arc::new(std::sync::atomic::AtomicBool::new(false));
        let generation = {
            let mut locked = state.lock().expect("state poisoned");
            locked.scan_generation = locked.scan_generation.wrapping_add(1);
            locked.scan_cancel = Some(Arc::clone(&cancel));
            locked.scan_generation
        };
        app.set_scanning(true);
        app.set_scan_results(ModelRc::new(VecModel::from(Vec::<ScanRow>::new())));
        let weak = app.as_weak();
        let state = Arc::clone(&state);
        std::thread::spawn(move || {
            let ready = draining.is_none_or(|(runtime, id)| {
                let deadline = std::time::Instant::now() + std::time::Duration::from_secs(3);
                loop {
                    if cancel.load(std::sync::atomic::Ordering::Acquire) {
                        return false;
                    }
                    let snapshot = runtime.snapshot();
                    if snapshot.request_id != id {
                        return false;
                    }
                    if snapshot.stage == RuntimeStage::Idle {
                        return true;
                    }
                    if std::time::Instant::now() >= deadline {
                        return false;
                    }
                    std::thread::sleep(std::time::Duration::from_millis(20));
                }
            });
            let results = if ready {
                current_runtime(&state)
                    .and_then(|runtime| runtime.scan(SCAN_PER_DEVICE, Arc::clone(&cancel)).ok())
                    .unwrap_or_default()
            } else {
                Vec::new()
            };
            // The scanner has dropped its capture on its own thread before
            // posting completion. Only now may the newest requested audio start.
            let _ = weak.upgrade_in_event_loop(move |app| {
                {
                    let mut locked = state.lock().expect("state poisoned");
                    if generation != locked.scan_generation {
                        return;
                    }
                    locked.scan_cancel = None;
                }
                let rows = results
                    .iter()
                    .map(|result| ScanRow {
                        name: format!(
                            "{}{}",
                            result.name,
                            if result.is_hardware {
                                ""
                            } else {
                                "（虚拟）"
                            }
                        )
                        .into(),
                        verdict: result.verdict.clone().into(),
                        ok: result.hears_something,
                        id: result.id.clone().into(),
                    })
                    .collect::<Vec<_>>();
                app.set_scan_results(ModelRc::new(VecModel::from(rows)));
                app.set_scanning(false);
                if let Some(client) = current(&state) {
                    if call_connection(&state) == ConnectionState::Connected {
                        start_voice(&app, &state, &client);
                    }
                } else if app.get_show_settings() {
                    start_mic_check(&app, &state);
                }
            });
        });
    });
}

/// 手动生成快照；不包含邀请凭据、身份密钥或聊天内容。
fn diagnostics(app: &App, state: &Arc<Mutex<State>>) -> String {
    let locked = state.lock().expect("state poisoned");
    let output = app
        .get_render_devices()
        .row_data(app.get_render_index().max(0) as usize)
        .unwrap_or_default();
    let mut text = format!(
        "篝火 {} / 协议 {} / {}
连接：{}
麦克风：{}
输出选择：{}
闭麦：{} / 关闭声音：{}
",
        env!("CARGO_PKG_VERSION"),
        protocol::control::PROTOCOL_VERSION,
        std::env::consts::OS,
        if app.get_connected() {
            "已连接"
        } else {
            "未连接"
        },
        app.get_capture_in_use(),
        output,
        app.get_self_muted(),
        app.get_self_deafened()
    );
    if let Some(runtime) = locked.runtime.as_ref() {
        text.push_str(&format!(
            "声音内核：{} / IPC {} / PID {:?}\n",
            runtime.engine_version(),
            client_runtime::ipc::VERSION,
            runtime.process_id()
        ));
    }
    if let Some(stats) = locked.runtime.as_ref().and_then(|r| r.snapshot().voice) {
        text.push_str(&format!(
            "UDP：{} / RTT：{:.1} ms
发送包：{} / 收到包：{} / 播放等待：{}
输入电平：{:.1} dB
",
            if stats.udp_ok {
                "通"
            } else if stats.udp_failed {
                "超时未连通"
            } else {
                "探测中"
            },
            stats.rtt_ms,
            stats.packets_sent,
            stats.packets_received,
            stats.underruns,
            stats.input_db
        ));
    }
    if let Some(client) = &locked.client {
        let (_, endpoint, _) = client.voice_endpoint_session();
        text.push_str(&format!("当前语音目标：{endpoint}\n"));
        text.push_str(&format!(
            "服务端最近报告的 UDP 接收数（含保活）：{}\n",
            client.server_udp_received()
        ));
    }
    text.push_str(&format!(
        "语音提示：{} {}
连接提示：{} {}
",
        app.get_voice_error(),
        app.get_voice_notice(),
        app.get_error_headline(),
        app.get_error_advice()
    ));
    if !locked.recovery_history.is_empty() {
        text.push_str("最近连接恢复记录（本次加入后的秒数）：\n");
        for entry in &locked.recovery_history {
            text.push_str(entry);
            text.push('\n');
        }
    }
    text.push_str("本地 UI 阶段（事件处理耗时，非整帧耗时）：\n");
    for phase in ui_timing::snapshot() {
        text.push_str(&phase);
        text.push('\n');
    }
    if let Some(snapshot) = locked.runtime.as_ref().map(RuntimeHandle::snapshot) {
        text.push_str(&format!("语音后台阶段：{:?}\n", snapshot.timings));
    }
    if let Some(error) = locked
        .settings_writer
        .as_ref()
        .and_then(SettingsWriterHandle::last_error)
    {
        text.push_str(&format!("设置保存失败：{error}\n"));
    }
    text
}

/// 设置面板：切换说话方式、绑按住说话的键、试听麦克风、选设备。
fn wire_settings(app: &App, state: &Arc<Mutex<State>>, hotkeys: Option<Rc<Hotkeys>>) {
    {
        let state = Arc::clone(state);
        let weak = app.as_weak();
        app.on_retry_voice(move || {
            if let Some(app) = weak.upgrade() {
                load_devices(&app, &state);
                restart_voice(&app, &state);
            }
        });
    }
    {
        let state = Arc::clone(state);
        let weak = app.as_weak();
        app.on_show_diagnostics(move || {
            if let Some(app) = weak.upgrade() {
                app.set_diagnostics(diagnostics(&app, &state).into());
                app.set_diagnostics_open(true);
            }
        });
    }
    {
        let state = Arc::clone(state);
        let weak = app.as_weak();
        app.on_set_cue_sounds(move |on| {
            let mut locked = state.lock().expect("state poisoned");
            locked.settings.cue_sounds = on;
            locked.persist_settings();
            if let Some(app) = weak.upgrade() {
                app.set_cue_sounds(on);
            }
        });
    }

    {
        let state = Arc::clone(state);
        let weak = app.as_weak();
        app.on_set_announce_names(move |on| {
            let mut locked = state.lock().expect("state poisoned");
            locked.settings.announce_names = on;
            locked.persist_settings();
            if let Some(app) = weak.upgrade() {
                app.set_announce_names(on);
            }
        });
    }

    {
        let state = Arc::clone(state);
        app.on_set_cue_volume(move |level| {
            state.lock().expect("state poisoned").settings.cue_volume =
                (level.clamp(0.0, 1.0) * 100.0).round() as u32;
        });
    }

    {
        let state = Arc::clone(state);
        app.on_save_cue_volume(move || {
            state.lock().expect("state poisoned").persist_settings();
        });
    }

    {
        let state = Arc::clone(state);
        let weak = app.as_weak();
        app.on_preview_cues(move || {
            let nick = weak
                .upgrade()
                .map(|app| app.get_nick().to_string())
                .unwrap_or_default();
            announce(&state, Chime::CameIn, &nick, true);
        });
    }

    {
        let state = Arc::clone(state);
        let weak = app.as_weak();
        app.on_pick_capture(move |index| {
            let Some(app) = weak.upgrade() else { return };
            {
                let mut locked = state.lock().expect("state poisoned");
                let id = locked.capture_ids.get(index as usize).cloned().flatten();
                locked.settings.capture_device = id;
                locked.persist_settings();
            }
            restart_voice(&app, &state);
        });
    }

    {
        // 从检测结果里点一行：直接按设备 id 选，不经过下拉框的序号。
        let state = Arc::clone(state);
        let weak = app.as_weak();
        app.on_pick_capture_id(move |id| {
            let Some(app) = weak.upgrade() else { return };
            let id = id.to_string();
            {
                let mut locked = state.lock().expect("state poisoned");
                locked.settings.capture_device = Some(id.clone());
                locked.persist_settings();
            }
            // 下拉框跟着走。对不上就保持原样 —— 真正生效的是上面存的 id，
            // 下拉框显示得对不对只是好看不好看的事。
            let index = state
                .lock()
                .expect("state poisoned")
                .capture_ids
                .iter()
                .position(|known| known.as_deref() == Some(id.as_str()));
            if let Some(index) = index {
                app.set_capture_index(index as i32);
            }
            restart_voice(&app, &state);
        });
    }

    {
        let state = Arc::clone(state);
        let weak = app.as_weak();
        app.on_pick_render(move |index| {
            let Some(app) = weak.upgrade() else { return };
            {
                let mut locked = state.lock().expect("state poisoned");
                let id = locked.render_ids.get(index as usize).cloned().flatten();
                locked.settings.render_device = id;
                locked.persist_settings();
            }
            restart_voice(&app, &state);
        });
    }

    {
        let state = Arc::clone(state);
        let weak = app.as_weak();
        app.on_toggle_monitor(move || {
            let Some(app) = weak.upgrade() else { return };
            let Some(runtime) = current_runtime(&state) else {
                return;
            };
            let on = !runtime.is_monitoring();
            runtime.set_monitoring(on);
            sync_audio(&app, &state);
        });
    }

    {
        let state = Arc::clone(state);
        let weak = app.as_weak();
        app.on_set_vad(move |level| {
            let mode = {
                let mut locked = state.lock().expect("state poisoned");
                locked.settings.vad_threshold_db = level_to_db(level);
                locked.persist_settings();
                transmit_mode(&locked.settings)
            };
            if let Some(voice) = current_runtime(&state) {
                voice.set_mode(mode);
            }
            if let Some(app) = weak.upgrade() {
                sync_audio(&app, &state);
            }
        });
    }

    {
        let weak = app.as_weak();
        let state = Arc::clone(state);
        app.on_toggle_settings(move || {
            let Some(app) = weak.upgrade() else { return };
            let opening = !app.get_show_settings();
            app.set_show_settings(opening);
            if opening {
                // 没连服务器也要能看电平、能试听 —— 这正是连不上时最想知道的事。
                start_mic_check(&app, &state);
            } else {
                // 关掉设置就把设备还回去。常驻几小时的软件不该一直占着麦克风，
                // 而且麦克风灯一直亮着会让人不安。
                stop_mic_check(&state);
                sync_audio(&app, &state);
            }
        });
    }

    {
        let state = Arc::clone(state);
        let weak = app.as_weak();
        app.on_set_ptt_mode(move |ptt| {
            let Some(app) = weak.upgrade() else { return };
            let mode = {
                let mut locked = state.lock().expect("state poisoned");
                locked.settings.talk_mode = if ptt {
                    TalkMode::PushToTalk
                } else {
                    TalkMode::VoiceActivity
                };
                locked.persist_settings();
                transmit_mode(&locked.settings)
            };
            // 链路不用重起，下一帧就按新方式走。
            if let Some(voice) = current_runtime(&state) {
                voice.set_mode(mode);
            }
            sync_audio(&app, &state);
        });
    }

    {
        let hotkeys = hotkeys.clone();
        let weak = app.as_weak();
        app.on_rebind_ptt(move || {
            let (Some(app), Some(hotkeys)) = (weak.upgrade(), hotkeys.as_ref()) else {
                return;
            };
            REBIND.with(|slot| *slot.borrow_mut() = Some(hotkeys.capture_next()));
            app.set_rebinding(true);
        });
    }

    {
        let weak = app.as_weak();
        app.on_cancel_rebind(move || {
            let Some(app) = weak.upgrade() else { return };
            if let Some(hotkeys) = &hotkeys {
                hotkeys.cancel_capture();
            }
            REBIND.with(|slot| *slot.borrow_mut() = None);
            app.set_rebinding(false);
        });
    }
}

/// 盯着按住说话的键，也顺便接住「正在设置热键」按下的那个键。
///
/// 轮询而不是回调：热键是在另一个线程上收到的，而改界面必须在界面线程上。
/// 每 20 ms 读一个原子变量，比每次按键都投递一次跨线程消息便宜得多 ——
/// 而按键在游戏里是每秒几十次的事。
fn spawn_ptt_poll(weak: slint::Weak<App>, state: Arc<Mutex<State>>, hotkeys: Rc<Hotkeys>) {
    let timer = slint::Timer::default();
    timer.start(slint::TimerMode::Repeated, PTT_POLL, move || {
        let Some(app) = weak.upgrade() else { return };

        // 用户正在设置热键？看看按下来没有。
        let captured = REBIND.with(|slot| slot.borrow().as_ref().and_then(|rx| rx.try_recv().ok()));
        if let Some(key) = captured {
            REBIND.with(|slot| *slot.borrow_mut() = None);
            hotkeys.set_ptt(Some(key));
            app.set_rebinding(false);
            app.set_ptt_label(key.label().into());
            let mut locked = state.lock().expect("state poisoned");
            locked.settings.ptt_key = Some(key);
            locked.persist_settings();
            return;
        }

        // 按住说话：把键的状态推给链路。
        if let Some(runtime) = current_runtime(&state) {
            CallController::set_ptt(&runtime, call_connection(&state), hotkeys.is_down());
        }
    });
    PTT_TIMER.with(|slot| *slot.borrow_mut() = Some(timer));
}

/// 设置里的说话方式翻译成链路那边的发送方式。
fn transmit_mode(settings: &Settings) -> TransmitMode {
    match settings.talk_mode {
        TalkMode::PushToTalk => TransmitMode::PushToTalk,
        TalkMode::VoiceActivity => TransmitMode::VoiceActivity {
            threshold_db: settings.vad_threshold_db,
        },
    }
}

/// 定时把音频状态搬到界面上。**整个程序只有一个**，连着和没连着都靠它。
///
/// 用 Slint 自己的定时器而不是线程：它就在界面线程上跑，省掉一次跨线程投递，
/// 而这件事每秒要做二十次。
fn spawn_status_poll(weak: slint::Weak<App>, state: Arc<Mutex<State>>) {
    let timer = slint::Timer::default();
    timer.start(slint::TimerMode::Repeated, VOICE_POLL, move || {
        let Some(app) = weak.upgrade() else { return };
        let (runtime, client, ptt_bound, call) = {
            let locked = state.lock().expect("state poisoned");
            (
                locked.runtime.clone(),
                locked.client.clone(),
                locked.settings.ptt_key.is_some(),
                locked.call.clone(),
            )
        };
        let Some(runtime) = runtime else { return };
        let snapshot = runtime.snapshot();
        let reconnecting = call.connection() == ConnectionState::Reconnecting;
        let mut view = CallViewModel::project_audio(&call, &snapshot, ptt_bound);
        SlintAdapter::apply_audio(&app, &view);
        update_tray(&view, client.as_ref());

        let ptt_needed = app.get_rebinding() || (client.is_some() && view.self_state.ptt_mode);
        PTT_TIMER.with(|slot| {
            if let Some(timer) = slot.borrow().as_ref() {
                if ptt_needed && !timer.running() {
                    timer.restart();
                } else if !ptt_needed && timer.running() {
                    timer.stop();
                }
            }
        });
        let hidden = !app.window().is_visible() || app.window().is_minimized();
        if !hidden && fire_pace(&app).is_some() {
            FIRE_TIMER.with(|slot| {
                if let Some(timer) = slot.borrow().as_ref() {
                    if !timer.running() {
                        timer.restart();
                    }
                }
            });
        }
        let idle = snapshot.stage == RuntimeStage::Idle;
        let interval = if hidden || idle {
            BACKGROUND_POLL
        } else {
            VOICE_POLL
        };
        VOICE_TIMER.with(|slot| {
            if let Some(timer) = slot.borrow().as_ref() {
                if timer.interval() != interval {
                    timer.set_interval(interval);
                }
            }
        });
        // Health policy has no Slint/window dependencies and runs in the tray too.
        if client.is_some() {
            let health = {
                let mut locked = state.lock().expect("state poisoned");
                let now = locked
                    .recovery_epoch
                    .get_or_insert_with(std::time::Instant::now)
                    .elapsed();
                locked.recovery.tick(now, &snapshot, reconnecting)
            };
            if let Some(notice) = health.notice {
                let call = {
                    let mut locked = state.lock().expect("state poisoned");
                    locked.call.set_notice(notice);
                    locked.call.clone()
                };
                view = CallViewModel::project_audio(&call, &snapshot, ptt_bound);
                SlintAdapter::apply_audio(&app, &view);
                update_tray(&view, client.as_ref());
            }
            if let Some(action) = &health.action {
                let mut reason = format!(
                    "{action:?}; stage={:?}; preparation={:?}",
                    snapshot.stage, snapshot.error
                );
                if let Some(stats) = &snapshot.voice {
                    reason.push_str(&format!(
                        "; udp_failed={}; capture={:?}; render={:?}; transport={:?}",
                        stats.udp_failed,
                        stats.capture_error,
                        stats.render_error,
                        stats.transport_error
                    ));
                }
                record_recovery(&state, &reason);
            }
            match health.action {
                Some(recovery::Action::Lost) => connection_notice(&state, false),
                Some(recovery::Action::Recovered) => connection_notice(&state, true),
                Some(recovery::Action::RetryVoice) => {
                    if runtime.needs_reauthentication() {
                        if let Some(client) = &client {
                            client.reconnect_transport();
                        }
                    } else {
                        restart_voice(&app, &state);
                    }
                    return;
                }
                Some(recovery::Action::Reconnect) => {
                    if let Some(client) = &client {
                        client.reconnect_transport();
                    }
                    return;
                }
                None => {}
            }
        }
        if !hidden {
            SlintAdapter::apply_activity(&app, &view);
        }
    });
    VOICE_TIMER.with(|slot| *slot.borrow_mut() = Some(timer));
}

/// 开一次独立试麦（没连服务器的时候用）。
fn start_mic_check(app: &App, state: &Arc<Mutex<State>>) {
    if current(state).is_some() {
        return;
    }
    if state.lock().expect("state poisoned").scan_cancel.is_some() {
        return;
    }
    let Some(runtime) = current_runtime(state) else {
        return;
    };
    let settings = state.lock().expect("state poisoned").settings.clone();
    runtime.start_mic_check(Devices {
        capture: settings.capture_device,
        render: settings.render_device,
    });
    sync_audio(app, state);
}

/// 停掉独立试麦。连服务器之前必须停 —— 不然麦克风会被采两遍。
fn stop_mic_check(state: &Arc<Mutex<State>>) {
    cancel_scan(state);
    // Closing Settings during a call keeps the call's runtime alive.
    if current(state).is_none() {
        if let Some(runtime) = current_runtime(state) {
            runtime.stop();
            runtime.set_monitoring(false);
        }
    }
}

fn cancel_scan(state: &Arc<Mutex<State>>) -> bool {
    if let Some(cancel) = &state.lock().expect("state poisoned").scan_cancel {
        cancel.store(true, std::sync::atomic::Ordering::Release);
        true
    } else {
        false
    }
}

fn pump_events(
    weak: slint::Weak<App>,
    state: Arc<Mutex<State>>,
    client: Client,
    events: Receiver<Event>,
) {
    std::thread::spawn(move || {
        // `recv` 在通道另一端被丢掉时返回 Err，循环自然结束 ——
        // 不需要额外的关闭信号。
        while let Ok(event) = events.recv() {
            let client = client.clone();
            let state = Arc::clone(&state);
            let posted = weak.upgrade_in_event_loop(move |app| {
                if !state
                    .lock()
                    .expect("state poisoned")
                    .client
                    .as_ref()
                    .is_some_and(|c| c.is_same(&client))
                {
                    return;
                }
                match event {
                    Event::Reconnecting {
                        attempt, reason, ..
                    } => {
                        ui_timing::mark_frame(ui_timing::FramePhase::Reconnecting);
                        record_recovery(
                            &state,
                            &format!("TCP reconnect attempt={attempt}: {reason}"),
                        );
                        // 旧链路的会话和密钥都作废了，发出去的声音服务端只会丢掉。
                        // 停掉它，别让用户以为自己还在被人听见 —— 麦克风指示灯也跟着灭。
                        let first = {
                            let mut locked = state.lock().expect("state poisoned");
                            let now = locked
                                .recovery_epoch
                                .get_or_insert_with(std::time::Instant::now)
                                .elapsed();
                            locked.call.reconnecting(attempt, reason);
                            locked.recovery.interrupt(now)
                        };
                        if let Some(voice) = current_voice(&state) {
                            voice.quiesce();
                        }
                        if first {
                            connection_notice(&state, false);
                        }
                        sync_audio(&app, &state);
                    }
                    Event::Reconnected => {
                        ui_timing::mark_frame(ui_timing::FramePhase::Reconnected);
                        record_recovery(&state, "TCP connected; waiting for voice probe");
                        {
                            let mut locked = state.lock().expect("state poisoned");
                            locked.call.connected();
                            locked.call.set_notice("服务器已连接，正在验证语音…");
                        }
                        // 服务端可能又给改了名（重名加后缀），以它为准。
                        let actual = {
                            let roster = client.roster();
                            roster.name_of(roster.me)
                        };
                        if !actual.is_empty() {
                            app.set_nick(actual.into());
                        }
                        app.set_member_id(-1);
                        if let Some(runtime) = current_runtime(&state) {
                            runtime.clear_volumes();
                        }
                        refresh(&app, &state, &client);
                        // 会话 id、端口、密钥全换了，语音链路只能按新的重起。
                        start_voice(&app, &state, &client);
                    }
                    Event::Disconnected(ended) => {
                        if matches!(&ended, Ended::Refused { .. }) {
                            connection_notice(&state, false);
                        }
                        let mut locked = state.lock().expect("state poisoned");
                        // 旧连接的收尾可能晚到：用户点了离开、马上又连了别的服务器，
                        // 这时候不能把新连接也一起拆了。
                        if !locked.client.as_ref().is_some_and(|c| c.is_same(&client)) {
                            return;
                        }
                        locked.client = None;
                        locked.call.offline();
                        let runtime = locked.runtime.clone();
                        let play_notice = matches!(&ended, Ended::Refused { .. });
                        drop(locked);
                        if let Some(runtime) = runtime {
                            runtime.quiesce();
                            if play_notice {
                                runtime.stop_after(std::time::Duration::from_millis(400));
                            } else {
                                runtime.stop();
                            }
                        }
                        app.invoke_reset_call_view();
                        sync_audio(&app, &state);
                        // 回到首页。「上次」那几个字要重算 —— 刚离开的这个现在是「刚刚」。
                        refresh_home(&app, &state);
                        match ended {
                            // 自己走的不是错误，别在首页上挂一条报错。
                            Ended::ByUser => clear_join_error(&app),
                            Ended::Refused { headline, advice } => {
                                app.set_error_headline(headline.into());
                                app.set_error_advice(advice.into());
                                app.set_error_can_reverify(false);
                                // 收在托盘里的时候被踢了、被封了、被顶号了：把窗口叫出来，
                                // 不然用户以为自己还在频道里，一直对着空气说话。
                                let _ = app.show();
                                app.window().set_minimized(false);
                            }
                        }
                    }
                    Event::CameIn { name, .. } => announce(&state, Chime::CameIn, &name, false),
                    Event::WentOut { name, .. } => announce(&state, Chime::WentOut, &name, false),
                    // 其余的都只是「画面该变了」。
                    _ => refresh(&app, &state, &client),
                }
            });
            if posted.is_err() {
                // 窗口关了
                return;
            }
        }
    });
}

fn record_recovery(state: &Arc<Mutex<State>>, message: &str) {
    let mut locked = state.lock().expect("state poisoned");
    let elapsed = locked
        .recovery_epoch
        .map(|e| e.elapsed().as_secs())
        .unwrap_or(0);
    if locked.recovery_history.len() == 20 {
        locked.recovery_history.pop_front();
    }
    locked
        .recovery_history
        .push_back(format!("+{elapsed}s {message}"));
}

/// One local sound per outage and verified recovery, using the same AEC-aware mixer.
fn connection_notice(state: &Arc<Mutex<State>>, recovered: bool) {
    let locked = state.lock().expect("state poisoned");
    if locked.settings.cue_sounds {
        if let Some(runtime) = &locked.runtime {
            runtime.notice(
                if recovered {
                    Chime::Recovered
                } else {
                    Chime::Lost
                },
                None,
                true,
                locked.settings.cue_volume as f32 / 100.0,
            );
        }
    }
}

/// Local notices are synthesized and mixed with the AEC reference in the engine.
fn announce(state: &Arc<Mutex<State>>, kind: Chime, name: &str, preview: bool) {
    let locked = state.lock().expect("state poisoned");
    if locked.client.is_none() && !preview {
        return;
    }
    let Some(runtime) = &locked.runtime else {
        return;
    };
    runtime.notice(
        kind,
        (locked.settings.announce_names || preview).then(|| name.to_owned()),
        locked.settings.cue_sounds || preview,
        locked.settings.cue_volume as f32 / 100.0,
    );
}
/// 单人音量在设置里按什么存：公钥的 base32。
fn volume_key(public_key: &[u8]) -> String {
    protocol::base32::encode(public_key)
}

/// 把设置里存的单人音量推给语音链路。
///
/// 语音链路只认会话 id，而会话 id 每次连接都换，所以这件事要在
/// 「名单变了」和「链路重起了」两个时候都做一遍。给每个人都设一次，
/// 包括 100% 的 —— 会话 id 在服务端是会回绕复用的，不能指望新来的人
/// 身上没有旧设置。
fn apply_volumes(state: &Arc<Mutex<State>>, client: &Client) {
    let (voice, settings) = {
        let locked = state.lock().expect("state poisoned");
        let Some(voice) = locked.runtime.clone() else {
            return;
        };
        (voice, locked.settings.clone())
    };
    let roster = client.roster();
    for user in roster.users.values() {
        if user.session_id == roster.me {
            continue;
        }
        let percent = settings.user_volume(&volume_key(&user.public_key));
        voice.set_volume(user.session_id, percent as f32 / 100.0);
    }
}

/// 重画左边的树和右边的聊天。
///
/// 每次事件投影完整业务快照；适配器保持模型身份，只通知变化的行。
fn refresh(app: &App, state: &Arc<Mutex<State>>, client: &Client) {
    // Never nest the State and Roster locks; transport events can take either.
    let (settings, call, runtime) = {
        let locked = state.lock().expect("state poisoned");
        (
            locked.settings.clone(),
            locked.call.clone(),
            locked.runtime.clone(),
        )
    };
    let Some(runtime) = runtime else { return };
    apply_volumes(state, client);
    let view = {
        let roster = client.roster();
        runtime.set_server_muted(roster.my_user().is_some_and(|me| me.server_muted));
        CallViewModel::project(
            &call,
            &runtime.snapshot(),
            settings.ptt_key.is_some(),
            Some(&roster),
            &settings.user_volumes,
        )
    };
    SlintAdapter::apply_call(app, &view);
    let here = SlintAdapter::scene_members(&view);
    refresh_campfire(app, state, view.channel_id, view.me, here);
}

/// 把频道里的人排到篝火的座位上。
///
/// 跟名单不同，**不整个重建模型**，只改变了的那几块石头：整个换掉的话
/// 每次有人改个静音，所有石头都要销毁重建，说话的光晕动画也跟着断。
fn refresh_campfire(app: &App, state: &Arc<Mutex<State>>, channel: u32, me: u32, here: Vec<Seat>) {
    let ids: Vec<u32> = here.iter().map(|s| s.id as u32).collect();
    let (seated, waiting): (Vec<Option<u32>>, Vec<u32>) = {
        let mut locked = state.lock().expect("state poisoned");
        locked.seats.update(channel, me, &ids);
        let seated = (0..campfire::SEATS)
            .map(|n| {
                ids.iter()
                    .copied()
                    .find(|&id| locked.seats.seat_of(id) == Some(n))
            })
            .collect();
        (seated, locked.seats.waiting().to_vec())
    };

    let find = |id: u32| here.iter().find(|seat| seat.id as u32 == id).cloned();
    let size = CAMPFIRE.with(|stage| {
        let mut stage = stage.borrow_mut();
        if stage.set_channel(channel) {
            app.set_campfire_backdrop(ui_timing::measure("scene channel still", || stage.still()));
        }
        stage.size()
    });
    let seats: Vec<Seat> = seated
        .into_iter()
        .enumerate()
        .map(|(n, id)| {
            let mut seat = id.and_then(find).unwrap_or_default();
            if seat.present {
                (seat.stone, seat.stone_talking) =
                    campfire::stone_images(seat.seed as u32, n, size);
            }
            seat
        })
        .collect();
    let waiting: Vec<Seat> = waiting.into_iter().filter_map(find).collect();

    SlintAdapter::apply_scene(app, seats, waiting);
}

/// 篝火画面的大小变了：底图和石头都按新尺寸重画（一格的逻辑大小不变，格数变了）。
fn resize_campfire(app: &App, width: f32, height: f32) {
    if !width.is_finite() || !height.is_finite() || width <= 0.0 || height <= 0.0 {
        return;
    }
    let backdrop = CAMPFIRE.with(|stage| {
        let mut stage = stage.borrow_mut();
        stage.set_size((width, height)).then(|| {
            ui_timing::mark_frame(ui_timing::FramePhase::SceneResize);
            ui_timing::measure("scene resize still", || stage.still())
        })
    });
    let Some(backdrop) = backdrop else {
        return;
    };
    app.set_campfire_backdrop(backdrop);
    let geometry: Vec<SeatGeometry> = campfire::seat_geometry((width, height))
        .into_iter()
        .map(|(x, y, size, up)| SeatGeometry { x, y, size, up })
        .collect();
    app.set_scene_geometry(ModelRc::new(VecModel::from(geometry)));
    let seats = app.get_seats();
    for n in 0..seats.row_count() {
        let Some(mut seat) = seats.row_data(n) else {
            continue;
        };
        if seat.present {
            (seat.stone, seat.stone_talking) =
                campfire::stone_images(seat.seed as u32, n, (width, height));
            seats.set_row_data(n, seat);
        }
    }
}

/// 火现在该多久动一下；`None` 是别动。
///
/// - 篝火不在眼前（收在托盘里、最小化、切到文字聊天、窄窗口时看着频道树）：不动
/// - 系统关了动画：不动
/// - 窗口在前台：[`FIRE_FRAME`]
/// - 不在前台，但前台是个铺满同一块屏幕的全屏程序（就是在打游戏）：不动。
///   这是最常见的情况 —— 篝火开着没最小化、被游戏整个盖住，画了也没人看得见
/// - 不在前台、也没被全屏盖住（比如放在副屏上）：慢下来，[`FIRE_FRAME_IDLE`]
fn fire_pace(app: &App) -> Option<std::time::Duration> {
    // 两堆火：频道里的那堆，和首页上的那堆。哪个都不在眼前就不动。
    if !(app.get_campfire_shown() || app.get_home_shown())
        || !app.window().is_visible()
        || app.window().is_minimized()
        || !system_animations_on()
    {
        return None;
    }
    match foreground(app) {
        Foreground::Us => Some(FIRE_FRAME),
        Foreground::FullscreenOther => None,
        Foreground::Other => Some(FIRE_FRAME_IDLE),
    }
}

enum Foreground {
    Us,
    /// 别的程序，而且铺满了我们所在的那块屏幕。
    FullscreenOther,
    Other,
}

#[cfg(windows)]
fn foreground(app: &App) -> Foreground {
    use raw_window_handle::{HasWindowHandle, RawWindowHandle};
    use windows_sys::Win32::Foundation::RECT;
    use windows_sys::Win32::Graphics::Gdi::{
        GetMonitorInfoW, MonitorFromWindow, MONITORINFO, MONITOR_DEFAULTTONEAREST,
    };
    use windows_sys::Win32::UI::WindowsAndMessaging::{
        GetClassNameW, GetForegroundWindow, GetWindowRect,
    };

    let handle = app.window().window_handle();
    let Ok(handle) = handle.window_handle() else {
        return Foreground::Us;
    };
    let RawWindowHandle::Win32(win32) = handle.as_raw() else {
        return Foreground::Us;
    };
    let ours = win32.hwnd.get() as *mut core::ffi::c_void;
    // SAFETY: 全是只读的查询；句柄来自窗口系统本身，出参都是我们自己的栈变量、大小对得上。
    unsafe {
        let front = GetForegroundWindow();
        if front.is_null() || front == ours {
            return Foreground::Us;
        }
        // 点了桌面的时候前台是桌面本身，它也铺满整块屏幕，但不是在打游戏
        let mut class = [0u16; 16];
        let len = GetClassNameW(front, class.as_mut_ptr(), class.len() as i32) as usize;
        let class = String::from_utf16_lossy(&class[..len]);
        if class == "Progman" || class == "WorkerW" {
            return Foreground::Other;
        }
        let monitor = MonitorFromWindow(ours, MONITOR_DEFAULTTONEAREST);
        if monitor != MonitorFromWindow(front, MONITOR_DEFAULTTONEAREST) {
            return Foreground::Other;
        }
        let mut info: MONITORINFO = std::mem::zeroed();
        info.cbSize = std::mem::size_of::<MONITORINFO>() as u32;
        let mut rect: RECT = std::mem::zeroed();
        if GetMonitorInfoW(monitor, &mut info) == 0 || GetWindowRect(front, &mut rect) == 0 {
            return Foreground::Other;
        }
        let screen = info.rcMonitor;
        let covers = rect.left <= screen.left
            && rect.top <= screen.top
            && rect.right >= screen.right
            && rect.bottom >= screen.bottom;
        if covers {
            Foreground::FullscreenOther
        } else {
            Foreground::Other
        }
    }
}

#[cfg(not(windows))]
fn foreground(_app: &App) -> Foreground {
    Foreground::Us
}

/// Windows 设置里「显示动画」关了没有（辅助功能 → 视觉效果 → 动画效果）。
///
/// 关了的人要么是晃眼、要么是机器吃力，火就不动了，只画一帧。
#[cfg(windows)]
fn system_animations_on() -> bool {
    use windows_sys::Win32::UI::WindowsAndMessaging::{
        SystemParametersInfoW, SPI_GETCLIENTAREAANIMATION,
    };
    let mut on: i32 = 1;
    // SAFETY: 这个查询往 pvParam 写一个 BOOL，给的正是一个 BOOL 大小的变量。
    let ok = unsafe {
        SystemParametersInfoW(
            SPI_GETCLIENTAREAANIMATION,
            0,
            (&mut on as *mut i32).cast(),
            0,
        )
    };
    ok == 0 || on != 0
}

#[cfg(not(windows))]
fn system_animations_on() -> bool {
    true
}

/// 火的定时器。一开始是停着的，由状态同步那边（`spawn_status_poll`）按需开起来；
/// 该停、该变快变慢，它每一帧自己看（`fire_pace`）。
fn spawn_fire(weak: slint::Weak<App>) {
    let timer = slint::Timer::default();
    let mut last = std::time::Instant::now();
    timer.start(slint::TimerMode::Repeated, FIRE_FRAME, move || {
        let Some(app) = weak.upgrade() else { return };
        let now = std::time::Instant::now();
        // 定时器停过一阵再开，别让火一下子「快进」好几秒
        let dt = now
            .duration_since(last)
            .min(FIRE_FRAME_IDLE * 2)
            .as_secs_f32();
        last = now;
        let pace = fire_pace(&app);
        FIRE_TIMER.with(|slot| {
            if let Some(timer) = slot.borrow().as_ref() {
                match pace {
                    None => timer.stop(),
                    Some(pace) if timer.interval() != pace => timer.set_interval(pace),
                    Some(_) => {}
                }
            }
        });
        if pace.is_none() {
            return;
        }
        if app.get_home_shown() {
            let frame = HOME_FIRE.with(|stage| stage.borrow_mut().advance(dt));
            app.set_home_fire(frame);
        } else {
            let frame = CAMPFIRE.with(|stage| stage.borrow_mut().advance(dt));
            app.set_campfire_backdrop(frame);
        }
    });
    timer.stop();
    FIRE_TIMER.with(|slot| *slot.borrow_mut() = Some(timer));
}

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

/// 建托盘图标，接上它的菜单。建不起来返回 `None`，不影响别的。
fn wire_tray(app: &App, state: &Arc<Mutex<State>>) -> Option<Tray> {
    let tray = match Tray::new() {
        Ok(tray) => tray,
        Err(e) => {
            eprintln!("托盘图标建不起来，点 × 只能退出：{e}");
            return None;
        }
    };
    tray.set_status("篝火 · 没连着".into());
    {
        let weak = app.as_weak();
        tray.on_show_window(move || {
            if let Some(app) = weak.upgrade() {
                let _ = app.show();
                app.window().set_minimized(false);
            }
        });
    }
    {
        let weak = app.as_weak();
        tray.on_toggle_mute(move || {
            if let Some(app) = weak.upgrade() {
                app.invoke_toggle_mute();
            }
        });
    }
    {
        let weak = app.as_weak();
        tray.on_toggle_deafen(move || {
            if let Some(app) = weak.upgrade() {
                app.invoke_toggle_deafen();
            }
        });
    }
    {
        let weak = app.as_weak();
        tray.on_leave(move || {
            if let Some(app) = weak.upgrade() {
                app.invoke_leave();
            }
        });
    }
    {
        let state = Arc::clone(state);
        tray.on_quit(move || quit_app(&state));
    }
    Some(tray)
}

/// 把连接和麦克风的状态搬到托盘图标上。跟着状态轮询的定时器一起跑。
fn update_tray(view: &AudioViewModel, client: Option<&Client>) {
    TRAY.with(|slot| {
        let slot = slot.borrow();
        let Some(tray) = slot.as_ref() else { return };
        let connected = matches!(
            view.connection,
            ConnectionState::Connected | ConnectionState::Reconnecting
        ) && client.is_some();
        let muted = view.self_muted;
        let deafened = view.self_state.deafened;
        let icon = match (connected, view.self_state.muted) {
            (false, _) => 0,
            (true, false) => 1,
            (true, true) => 2,
        };
        let status = match client {
            _ if !connected => "篝火 · 没连着".to_string(),
            _ if !view.reconnecting.is_empty() => "篝火 · 正在重连…".to_string(),
            _ if !view.voice_notice.is_empty() => format!("篝火 · {}", view.voice_notice),
            Some(client) => {
                let mic = if view.self_state.server_muted {
                    "被管理员闭麦"
                } else if deafened {
                    "关着耳朵"
                } else if muted {
                    "闭着麦"
                } else {
                    "麦克风开着"
                };
                format!("篝火 · 在「{}」里 · {mic}", current_channel_name(client))
            }
            None => "篝火".to_string(),
        };
        // 值没变就别设：定时器一秒二十次，每次都设会让托盘一直在重建图标。
        if tray.get_state() != icon {
            tray.set_state(icon);
        }
        if tray.get_status().as_str() != status {
            tray.set_status(status.into());
        }
        if tray.get_connected() != connected {
            tray.set_connected(connected);
        }
        if tray.get_muted() != muted {
            tray.set_muted(muted);
        }
        if tray.get_deafened() != deafened {
            tray.set_deafened(deafened);
        }
    });
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

/// Unix 毫秒 → 本地的 `HH:MM`。
///
/// 自己算而不是拉一个日期库进来：这里只要「今天的几点几分」，
/// 而日期库会带着时区数据库一起进安装包。
fn clock_time(timestamp_ms: i64) -> String {
    let Ok(now) = std::time::SystemTime::now().duration_since(std::time::UNIX_EPOCH) else {
        return String::new();
    };
    // 用「本机现在的本地时间」和「本机现在的 UTC 时间」之差当时区偏移。
    // 对聊天时间戳足够了 —— 误差只会出现在跨夏令时切换的那一小时，
    // 而中国不用夏令时。
    let offset = local_offset_seconds();
    let local = timestamp_ms / 1000 + offset;
    let _ = now;
    let seconds_today = local.rem_euclid(86_400);
    format!(
        "{:02}:{:02}",
        seconds_today / 3600,
        (seconds_today % 3600) / 60
    )
}

/// 「9月27日」。封禁名单里用：封了多久比封在几点几分要紧。
fn clock_date(timestamp_ms: i64) -> String {
    let local = timestamp_ms.div_euclid(1000) + local_offset_seconds();
    let (month, day) = month_day(local.div_euclid(86_400));
    format!("{month}月{day}日")
}

/// 1970-01-01 起的第几天 → (月, 日)。公历，Howard Hinnant 的 civil_from_days。
///
/// 不为这一个格式拉进一整个日期库。
fn month_day(days: i64) -> (u32, u32) {
    let z = days + 719_468;
    let era = z.div_euclid(146_097);
    let doe = z - era * 146_097;
    let yoe = (doe - doe / 1460 + doe / 36_524 - doe / 146_096) / 365;
    let doy = doe - (365 * yoe + yoe / 4 - yoe / 100);
    let mp = (5 * doy + 2) / 153;
    let day = (doy - (153 * mp + 2) / 5 + 1) as u32;
    let month = if mp < 10 { mp + 3 } else { mp - 9 } as u32;
    (month, day)
}

#[cfg(windows)]
fn local_offset_seconds() -> i64 {
    // SAFETY: GetTimeZoneInformation 只写它自己的输出结构，没有别的副作用。
    unsafe {
        let mut info =
            std::mem::zeroed::<windows_sys::Win32::System::Time::TIME_ZONE_INFORMATION>();
        let kind = windows_sys::Win32::System::Time::GetTimeZoneInformation(&mut info);
        // Bias 是「本地时间 + Bias = UTC」，单位是分钟，所以要取负
        let bias = match kind {
            2 => info.Bias + info.DaylightBias, // TIME_ZONE_ID_DAYLIGHT
            _ => info.Bias + info.StandardBias,
        };
        -(bias as i64) * 60
    }
}

#[cfg(not(windows))]
fn local_offset_seconds() -> i64 {
    0
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn month_day_matches_known_dates() {
        assert_eq!(month_day(0), (1, 1)); // 1970-01-01
        assert_eq!(month_day(59), (3, 1)); // 1970 不是闰年
        assert_eq!(month_day(11_016), (2, 29)); // 2000-02-29
        assert_eq!(month_day(19_675), (11, 14)); // 2023-11-14
        assert_eq!(month_day(-1), (12, 31)); // 1969-12-31
    }

    #[test]
    fn clock_time_is_hh_mm() {
        let text = clock_time(1_700_000_000_000);
        assert_eq!(text.len(), 5, "{text}");
        assert_eq!(&text[2..3], ":");
        let hour: u32 = text[..2].parse().unwrap();
        let minute: u32 = text[3..].parse().unwrap();
        assert!(hour < 24);
        assert!(minute < 60);
    }

    /// 时间戳是服务端给的，不可信。0 和负数都不能让界面 panic。
    #[test]
    fn nonsense_timestamps_do_not_panic() {
        for stamp in [0, -1, i64::MIN, i64::MAX] {
            let text = clock_time(stamp);
            assert!(text.is_empty() || text.len() == 5, "{stamp} -> {text}");
        }
    }

    #[test]
    fn default_nick_is_never_empty() {
        assert!(!default_nick().is_empty());
    }
}
