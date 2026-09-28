// SPDX-License-Identifier: GPL-3.0-or-later
#![cfg_attr(not(debug_assertions), windows_subsystem = "windows")]

//! 篝火客户端。
//!
//! 这个文件只做一件事：把 `client-core` 的状态搬到界面上，把界面的点击搬回去。
//! **所有规则都不在这里** —— 能不能进、谁在哪个频道、消息怎么归属，
//! 全在 `client-core` 和服务端，那些地方都有测试。
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

use client_core::{Client, ConnectError, Ended, Event};
use slint::{ComponentHandle, Model, ModelRc, SharedString, VecModel};
use voice_core::cue::{chime, Chime};
use voice_core::identity::Identity;
use voice_core::miccheck::{scan_microphones, MicCheck};
use voice_core::pipeline::{default_jitter, Pipeline, PipelineConfig, TransmitMode};
use voice_core::tts::{speakable_name, Announcer};

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

mod campfire;
mod settings;
mod single_instance;
mod update;

use settings::{db_to_level, level_to_db, snap_volume, CloseAction, Settings, TalkMode};
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
    if std::env::var_os(SIMULATE_WINDOW_FAILURE).is_some()
        && std::env::var_os(FALLBACK_MARKER).is_none()
    {
        return Err(Failure::Window(slint::PlatformError::Other(
            "模拟的窗口失败".into(),
        )));
    }
    let app = App::new().map_err(Failure::Window)?;

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
    app.set_invite_link(stored.last_invite.clone().into());
    app.set_ptt_mode(stored.talk_mode == TalkMode::PushToTalk);
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

    // 命令行上给了链接就填进去，盖过上次存的那条。Windows 把 `gouhuo://`
    // 的协议处理器就是这么调起来的 —— 这一个参数同时也是「一键加入」的落点。
    if let Some(link) = link_from_args() {
        app.set_invite_link(link.into());
    }

    // 用 Arc<Mutex<..>> 而不是 Rc<RefCell<..>>：连接结果要从后台线程
    // 搬回界面线程，那个闭包必须是 Send 的。
    let state = Arc::new(Mutex::new(State::default()));
    state.lock().expect("state poisoned").settings = stored;

    wire_join(&app, &identity, &state);
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

    // 点链接进来的老用户直接连，这才叫一键加入。
    //
    // **第一次跑的人不自动连**：那时昵称还是 Windows 用户名，
    // 而且身份刚生成，该让他先看一眼再进去，否则他会顶着 "admin"
    // 出现在一屋子人面前。
    if !first_run && !app.get_invite_link().is_empty() {
        app.invoke_join();
    }

    // **不能用 run_event_loop**：它在最后一个窗口隐藏时就返回，收到托盘等于退出。
    // 真正的退出走 quit_app。
    let result = slint::run_event_loop_until_quit();
    tear_down();
    result.map_err(Failure::EventLoop)?;
    Ok(())
}

/// 检查更新：开关、「去下载」，以及启动时查一次。
fn wire_update(app: &App, state: &Arc<Mutex<State>>) {
    {
        let state = Arc::clone(state);
        let weak = app.as_weak();
        app.on_set_check_updates(move |on| {
            let mut locked = state.lock().expect("state poisoned");
            locked.settings.check_updates = on;
            let _ = locked.settings.save();
            if let Some(app) = weak.upgrade() {
                app.set_check_updates(on);
            }
        });
    }
    // 查到的发布页地址。只有检查更新那条路会写它，而且只写 GitHub 上本仓库的地址。
    let url = Arc::new(Mutex::new(String::new()));
    {
        let url = Arc::clone(&url);
        app.on_open_update(move || open_in_browser(&url.lock().expect("url poisoned")));
    }
    if !state.lock().expect("state poisoned").settings.check_updates {
        return;
    }
    let weak = app.as_weak();
    update::check_in_background(move |available| {
        let _ = weak.upgrade_in_event_loop(move |app| {
            *url.lock().expect("url poisoned") = available.url;
            app.set_update_version(available.version.into());
        });
    });
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
    if !link.starts_with(protocol::URL_PREFIX) || app.get_connecting() {
        return;
    }
    if app.get_connected() {
        let current = state
            .lock()
            .expect("state poisoned")
            .settings
            .last_invite
            .clone();
        if current == link {
            return;
        }
        // 点了另一个服务器的链接：离开这边，去那边。点链接这个动作本身就是
        // 「我要去那儿」，再问一句只是多一步。
        app.invoke_leave();
    }
    app.set_invite_link(link.into());
    app.invoke_join();
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
    /// 语音链路。丢掉它就会把音频线程收干净。
    voice: Option<Arc<Pipeline>>,
    /// 没连服务器时的独立试麦。
    ///
    /// 跟 `voice` **永远不会同时存在** —— 两个都要开同一副耳机，
    /// 虽然共享模式下不会打架，但麦克风会被采两遍，电平也会对不上。
    mic_check: Option<Arc<MicCheck>>,
    settings: Settings,
    /// 采集流的把手：实际打开的设备叫什么。
    capture: Option<voice_core::wasapi::CaptureDiagnostics>,
    /// 下拉框里第 n 项对应哪个设备 id。第 0 项是「系统默认」，所以是 None。
    capture_ids: Vec<Option<String>>,
    render_ids: Vec<Option<String>>,
    /// 念名字的后台线程。**第一次要念的时候才起** —— 大多数人不开这个功能，
    /// 不该为它常驻一个线程和一个语音合成引擎。
    announcer: Option<Announcer>,
    /// 篝火上谁坐哪块石头。跨刷新保留 —— 坐下了就不挪。
    seats: campfire::SeatMap,
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
    /// 让火动起来的定时器。只在篝火真的看得见时跑，见 `fire_pace`。
    static FIRE_TIMER: RefCell<Option<slint::Timer>> = const { RefCell::new(None) };
}

/// 点「加入」之后发生的事。
fn wire_join(app: &App, identity: &Rc<Identity>, state: &Arc<Mutex<State>>) {
    let weak = app.as_weak();
    let identity = Rc::clone(identity);
    let state = Arc::clone(state);

    app.on_join(move || {
        let Some(app) = weak.upgrade() else { return };
        if app.get_connecting() {
            return;
        }
        let link = app.get_invite_link().to_string();
        let nick = {
            let typed = app.get_nick().to_string();
            let trimmed = typed.trim().to_string();
            if trimmed.is_empty() {
                default_nick()
            } else {
                trimmed
            }
        };

        app.set_connecting(true);
        app.set_error_headline("".into());
        app.set_error_advice("".into());

        // 连接会阻塞（DNS、TCP、TLS 握手，最长 8 秒），**不能在界面线程上做** ——
        // 否则窗口会白到超时为止，用户以为程序死了。
        let weak = app.as_weak();
        let state = Arc::clone(&state);
        // 身份只有一份，不能移进后台线程。导出再导入拿一份副本 ——
        // 这条路径本来就要能跑（换机器就是靠它），顺手也验了一次。
        let identity_copy = match Identity::import(&identity.export()) {
            Ok(copy) => copy,
            Err(_) => {
                app.set_connecting(false);
                return;
            }
        };

        std::thread::spawn(move || {
            let outcome = Client::connect(&link, &identity_copy, &nick);
            let _ = weak.upgrade_in_event_loop(move |app| match outcome {
                Ok((client, events)) => {
                    on_connected(&app, &state, client, events, &nick);
                }
                Err(e) => {
                    show_error(&app, &e);
                    app.set_connecting(false);
                }
            });
        });
    });
}

fn show_error(app: &App, e: &ConnectError) {
    app.set_error_headline(e.headline().into());
    app.set_error_advice(e.advice().into());
}

fn on_connected(
    app: &App,
    state: &Arc<Mutex<State>>,
    client: Client,
    events: Receiver<Event>,
    nick: &str,
) {
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
        // 连上了才存 —— 存一条连不上的链接只会让下次打开就看到一个错误。
        let mut locked = state.lock().expect("state poisoned");
        locked.client = Some(client.clone());
        locked.settings.nick = app.get_nick().to_string();
        locked.settings.last_invite = app.get_invite_link().to_string();
        let _ = locked.settings.save();
    }
    app.set_connecting(false);
    app.set_connected(true);
    app.set_self_muted(false);
    app.set_self_deafened(false);
    app.set_voice_error("".into());
    app.set_udp_ok(false);
    refresh(app, state, &client);

    start_voice(app, state, &client);
    pump_events(app.as_weak(), Arc::clone(state), client, events);
}

/// 起语音链路，并开一个定时器把它的状态搬到界面上。
///
/// 设备打不开不该让人掉线 —— 文字和名单照样能用，只是没声音。所以这里
/// 失败只是把原因显示出来，不动连接。
fn start_voice(app: &App, state: &Arc<Mutex<State>>, client: &Client) {
    // 先把独立试麦停掉：两个都开的话麦克风会被采两遍。
    stop_mic_check(state);

    let Some(addr) = resolve_voice_addr(client) else {
        app.set_voice_error("服务器没给出语音端口".into());
        return;
    };

    let mode = transmit_mode(&state.lock().expect("state poisoned").settings);
    let cfg = PipelineConfig {
        session_id: client.session_id(),
        server: addr,
        upstream_key: *client.voice_keys().upstream.as_bytes(),
        downstream_key: *client.voice_keys().downstream.as_bytes(),
        jitter: default_jitter(),
        mode,
    };

    let (capture_id, render_id) = {
        let locked = state.lock().expect("state poisoned");
        (
            locked.settings.capture_device.clone(),
            locked.settings.render_device.clone(),
        )
    };
    let capture = voice_core::wasapi::WasapiCapture::new(capture_id);
    let diagnostics = capture.diagnostics();

    match Pipeline::start(
        cfg,
        Box::new(capture),
        Box::new(voice_core::wasapi::WasapiRender::new(render_id)),
        audio_processor(),
    ) {
        Ok(pipeline) => {
            // 新链路默认开麦开耳朵。界面上闭着的要原样带过去 —— 换设备、断线重连
            // 都会走到这里，闭着麦换了个耳机就变成开麦，是会出事的那种 bug。
            let deafened = app.get_self_deafened();
            pipeline.set_deafened(deafened);
            pipeline.set_muted(app.get_self_muted() || deafened);
            {
                let mut locked = state.lock().expect("state poisoned");
                locked.voice = Some(Arc::new(pipeline));
                locked.capture = Some(diagnostics);
            }
            // 新链路里所有人都是 100%。换设备、断线重连都会走到这里。
            apply_volumes(state, client);
        }
        Err(e) => app.set_voice_error(format!("{e}").into()),
    }
}

/// 把设备列表灌进下拉框，并记下「第 n 项是哪个 id」。
///
/// 第 0 项永远是「系统默认」—— 绝大多数人不该需要管这个，
/// 而且它是唯一在换了耳机之后还能跟着走的选项。
fn load_devices(app: &App, state: &Arc<Mutex<State>>) {
    use voice_core::wasapi::{list_endpoints, Direction};

    for (direction, is_capture) in [(Direction::Capture, true), (Direction::Render, false)] {
        let endpoints = list_endpoints(direction).unwrap_or_default();
        let mut labels: Vec<SharedString> = vec!["系统默认".into()];
        let mut ids: Vec<Option<String>> = vec![None];
        for endpoint in endpoints {
            // 标出虚拟声卡。玩家机器上这类设备极多，而「没声音」十有八九
            // 就是选中了一个没在路由的虚拟麦。
            let suffix = if endpoint.is_hardware {
                ""
            } else {
                "（虚拟）"
            };
            labels.push(format!("{}{suffix}", endpoint.name).into());
            ids.push(Some(endpoint.id));
        }

        let saved = {
            let locked = state.lock().expect("state poisoned");
            if is_capture {
                locked.settings.capture_device.clone()
            } else {
                locked.settings.render_device.clone()
            }
        };
        let index = ids
            .iter()
            .position(|id| *id == saved)
            // 存下来的设备没了（拔了耳机、换了机器）就退回「系统默认」，
            // 跟 open_or_default 的行为一致。
            .unwrap_or(0) as i32;

        let model = ModelRc::new(VecModel::from(labels));
        let mut locked = state.lock().expect("state poisoned");
        if is_capture {
            locked.capture_ids = ids;
            drop(locked);
            app.set_capture_devices(model);
            app.set_capture_index(index);
        } else {
            locked.render_ids = ids;
            drop(locked);
            app.set_render_devices(model);
            app.set_render_index(index);
        }
    }
}

/// 换设备。链路要重起 —— WASAPI 的流是绑在设备上的，换不了。
///
/// 重起会让声音断一下（几十毫秒）。这是换设备本来就该有的代价，
/// 比为了热切换在音频线程里加一套状态机划算得多。
fn restart_voice(app: &App, state: &Arc<Mutex<State>>) {
    let client = state.lock().expect("state poisoned").client.clone();
    let Some(client) = client else {
        // 没连服务器：重起的是独立试麦。
        let was_monitoring = {
            let locked = state.lock().expect("state poisoned");
            locked
                .mic_check
                .as_ref()
                .map(|m| m.is_monitoring())
                .unwrap_or(false)
        };
        stop_mic_check(state);
        app.set_capture_in_use("".into());
        start_mic_check(app, state);
        if was_monitoring {
            if let Some(mic) = state.lock().expect("state poisoned").mic_check.clone() {
                mic.set_monitoring(true);
            }
        }
        return;
    };

    let was_monitoring = current_voice(state)
        .map(|v| v.is_monitoring())
        .unwrap_or(false);
    {
        let mut locked = state.lock().expect("state poisoned");
        // 先丢掉旧的再起新的：同一个设备不能开两路，而且旧链路的线程
        // 还占着那个设备。
        locked.voice = None;
        locked.capture = None;
    }
    app.set_voice_error("".into());
    app.set_udp_ok(false);
    app.set_capture_in_use("".into());
    start_voice(app, state, &client);
    if was_monitoring {
        if let Some(voice) = current_voice(state) {
            voice.set_monitoring(true);
        }
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
        if app.get_scanning() {
            return;
        }
        app.set_scanning(true);
        app.set_scan_results(ModelRc::new(VecModel::from(Vec::<ScanRow>::new())));

        // 检测要挨个打开设备，跟正在跑的试麦抢同一个麦克风。先停掉。
        let was_checking = state.lock().expect("state poisoned").mic_check.is_some();
        stop_mic_check(&state);

        let weak = app.as_weak();
        let state = Arc::clone(&state);
        std::thread::spawn(move || {
            let results = scan_microphones(SCAN_PER_DEVICE);
            let _ = weak.upgrade_in_event_loop(move |app| {
                let rows: Vec<ScanRow> = results
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
                        verdict: result.verdict().into(),
                        ok: result.hears_something(),
                        id: result.id.clone().into(),
                    })
                    .collect();
                app.set_scan_results(ModelRc::new(VecModel::from(rows)));
                app.set_scanning(false);
                if was_checking {
                    start_mic_check(&app, &state);
                }
            });
        });
    });
}

/// 设置面板：切换说话方式、绑按住说话的键、试听麦克风、选设备。
fn wire_settings(app: &App, state: &Arc<Mutex<State>>, hotkeys: Option<Rc<Hotkeys>>) {
    {
        let state = Arc::clone(state);
        let weak = app.as_weak();
        app.on_set_cue_sounds(move |on| {
            let mut locked = state.lock().expect("state poisoned");
            locked.settings.cue_sounds = on;
            let _ = locked.settings.save();
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
            let _ = locked.settings.save();
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
            let _ = state.lock().expect("state poisoned").settings.save();
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
                let _ = locked.settings.save();
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
                let _ = locked.settings.save();
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
                let _ = locked.settings.save();
            }
            restart_voice(&app, &state);
        });
    }

    {
        let state = Arc::clone(state);
        let weak = app.as_weak();
        app.on_toggle_monitor(move || {
            let Some(app) = weak.upgrade() else { return };
            let (voice, mic) = {
                let locked = state.lock().expect("state poisoned");
                (locked.voice.clone(), locked.mic_check.clone())
            };
            let on = match (&voice, &mic) {
                (Some(voice), _) => {
                    let on = !voice.is_monitoring();
                    voice.set_monitoring(on);
                    on
                }
                (None, Some(mic)) => {
                    let on = !mic.is_monitoring();
                    mic.set_monitoring(on);
                    on
                }
                (None, None) => return,
            };
            app.set_monitoring(on);
        });
    }

    {
        let state = Arc::clone(state);
        app.on_set_vad(move |level| {
            let mode = {
                let mut locked = state.lock().expect("state poisoned");
                locked.settings.vad_threshold_db = level_to_db(level);
                let _ = locked.settings.save();
                transmit_mode(&locked.settings)
            };
            if let Some(voice) = current_voice(&state) {
                voice.set_mode(mode);
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
            }
        });
    }

    {
        let state = Arc::clone(state);
        let weak = app.as_weak();
        app.on_set_ptt_mode(move |ptt| {
            let Some(app) = weak.upgrade() else { return };
            app.set_ptt_mode(ptt);
            let mode = {
                let mut locked = state.lock().expect("state poisoned");
                locked.settings.talk_mode = if ptt {
                    TalkMode::PushToTalk
                } else {
                    TalkMode::VoiceActivity
                };
                let _ = locked.settings.save();
                transmit_mode(&locked.settings)
            };
            // 链路不用重起，下一帧就按新方式走。
            if let Some(voice) = current_voice(&state) {
                voice.set_mode(mode);
            }
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
            let _ = locked.settings.save();
            return;
        }

        // 按住说话：把键的状态推给链路。
        let Some(voice) = current_voice(&state) else {
            return;
        };
        if app.get_ptt_mode() {
            let down = hotkeys.is_down();
            voice.set_transmitting(down);
            app.set_transmitting(down);
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

/// 回声消除 / 降噪 / 自动增益。
fn audio_processor() -> Option<Box<dyn voice_core::pipeline::AudioProcessor>> {
    use voice_core::apm::{Apm, ApmConfig};
    // APM 起不来不该让语音也用不了。戴耳机的人根本不需要它。
    match Apm::new(voice_core::audio::SAMPLE_RATE, ApmConfig::default()) {
        Ok(apm) => Some(Box::new(apm)),
        Err(_) => None,
    }
}

fn resolve_voice_addr(client: &Client) -> Option<std::net::SocketAddr> {
    use std::net::ToSocketAddrs;
    if client.udp_port() == 0 {
        return None;
    }
    (client.server_host(), client.udp_port())
        .to_socket_addrs()
        .ok()?
        .next()
}

/// 定时把音频状态搬到界面上。**整个程序只有一个**，连着和没连着都靠它。
///
/// 用 Slint 自己的定时器而不是线程：它就在界面线程上跑，省掉一次跨线程投递，
/// 而这件事每秒要做二十次。
fn spawn_status_poll(weak: slint::Weak<App>, state: Arc<Mutex<State>>) {
    let timer = slint::Timer::default();
    timer.start(slint::TimerMode::Repeated, VOICE_POLL, move || {
        let Some(app) = weak.upgrade() else { return };
        let (voice, mic_check, client, capture) = {
            let locked = state.lock().expect("state poisoned");
            (
                locked.voice.clone(),
                locked.mic_check.clone(),
                locked.client.clone(),
                locked.capture.clone(),
            )
        };
        update_tray(&app, client.as_ref());

        // 按住说话的定时器只在真用得着的时候跑。收在托盘里、用按住说话打游戏时
        // 它照跑 —— 那正是它最要紧的时候。
        let ptt_needed = app.get_rebinding() || (voice.is_some() && app.get_ptt_mode());
        PTT_TIMER.with(|slot| {
            if let Some(timer) = slot.borrow().as_ref() {
                if ptt_needed && !timer.running() {
                    timer.restart();
                } else if !ptt_needed && timer.running() {
                    timer.stop();
                }
            }
        });

        // 窗口看不见就只管托盘，界面同步放慢。窗口开着但没什么在动的时候
        // （没进频道、也没在试麦 —— 登录页）同样放慢：电平条、说话指示都没有。
        let hidden = !app.window().is_visible() || app.window().is_minimized();

        // 篝火该动就把火的定时器开起来（停是它自己停的，见 spawn_fire）。
        // 这里最慢半秒看一次，所以窗口恢复、游戏切走之后，火最多愣半秒才动。
        if !hidden && fire_pace(&app).is_some() {
            FIRE_TIMER.with(|slot| {
                if let Some(timer) = slot.borrow().as_ref() {
                    if !timer.running() {
                        timer.restart();
                    }
                }
            });
        }

        let idle = voice.is_none() && mic_check.is_none();
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
        if hidden {
            return;
        }

        // 实际用的是哪个设备、是不是虚拟声卡。两条路共用同一个把手。
        if let Some(capture) = capture {
            if capture.has_opened() {
                app.set_capture_in_use(capture.device_name().into());
                app.set_capture_is_virtual(capture.is_virtual());
            }
        }

        if let Some(voice) = voice {
            let stats = voice.stats();
            app.set_udp_ok(stats.udp_ok);
            app.set_input_level(db_to_level(stats.input_db));
            app.set_monitoring(voice.is_monitoring());
            app.set_transmitting(transmitting_now(&app, &voice, stats.input_db));
            if let Some(client) = client {
                update_speaking(&app, &client, &stats.speaking);
            }
        } else if let Some(mic) = mic_check {
            app.set_input_level(db_to_level(mic.input_db()));
            app.set_monitoring(mic.is_monitoring());
            // 没连服务器时「在不在发」没有意义，但电平条要靠它变色 ——
            // 用跟语音激活一样的判据，这样调灵敏度时看到的效果是真的。
            let threshold = state
                .lock()
                .expect("state poisoned")
                .settings
                .vad_threshold_db;
            app.set_transmitting(!app.get_ptt_mode() && mic.input_db() > threshold);
            if let Some(error) = mic.error() {
                app.set_voice_error(error.into());
            }
        }
    });
    VOICE_TIMER.with(|slot| *slot.borrow_mut() = Some(timer));
}

/// 开一次独立试麦（没连服务器的时候用）。
fn start_mic_check(app: &App, state: &Arc<Mutex<State>>) {
    if state.lock().expect("state poisoned").voice.is_some() {
        // 已经连上了，语音链路就在跑，用它的电平。
        return;
    }
    let (capture_id, render_id) = {
        let locked = state.lock().expect("state poisoned");
        (
            locked.settings.capture_device.clone(),
            locked.settings.render_device.clone(),
        )
    };
    let capture = voice_core::wasapi::WasapiCapture::new(capture_id);
    let diagnostics = capture.diagnostics();
    app.set_voice_error("".into());

    match MicCheck::start(
        Box::new(capture),
        Box::new(voice_core::wasapi::WasapiRender::new(render_id)),
        audio_processor(),
    ) {
        Ok(check) => {
            let mut locked = state.lock().expect("state poisoned");
            locked.mic_check = Some(Arc::new(check));
            locked.capture = Some(diagnostics);
        }
        Err(e) => app.set_voice_error(format!("{e}").into()),
    }
}

/// 停掉独立试麦。连服务器之前必须停 —— 不然麦克风会被采两遍。
fn stop_mic_check(state: &Arc<Mutex<State>>) {
    let mut locked = state.lock().expect("state poisoned");
    locked.mic_check = None;
}

/// 现在这一刻在不在往外发。
///
/// 自己说话服务端不会转回来，所以这个只能在本地算。它跟链路里那段判断
/// 是同一套规则 —— 两边写法不一样的话，界面显示「正在发送」而实际没发，
/// 是最让人摸不着头脑的那种 bug。
fn transmitting_now(app: &App, voice: &Pipeline, input_db: f32) -> bool {
    match voice.mode() {
        TransmitMode::Always => true,
        TransmitMode::PushToTalk => app.get_transmitting(),
        TransmitMode::VoiceActivity { threshold_db } => input_db > threshold_db,
    }
}

/// 只改「谁在说话」那一列，不整个重建列表。
///
/// 这件事每秒发生五次，而整个重建会让列表的滚动位置跳回顶上。
fn update_speaking(app: &App, client: &Client, speaking: &[u32]) {
    let rows = app.get_rows();
    let me = client.roster().me;
    for i in 0..rows.row_count() {
        let Some(mut row) = rows.row_data(i) else {
            continue;
        };
        if row.is_channel {
            continue;
        }
        // 自己说没说话服务端不会转回来，所以自己那一行永远不亮。
        // 要亮的话得看本地有没有在发 —— 等按键说话做出来再说。
        let now = speaking.contains(&(row.id as u32)) && row.id as u32 != me;
        if row.speaking != now {
            row.speaking = now;
            rows.set_row_data(i, row);
        }
    }

    // 篝火那边同样只改变了的。自己那块石头看的是「在不在发声」，界面自己管。
    let mut waiting_talking = false;
    for (model, is_waiting) in [(app.get_seats(), false), (app.get_waiting(), true)] {
        for i in 0..model.row_count() {
            let Some(mut seat) = model.row_data(i) else {
                continue;
            };
            let now = seat.present && speaking.contains(&(seat.id as u32)) && seat.id as u32 != me;
            waiting_talking |= is_waiting && now;
            if seat.speaking != now {
                seat.speaking = now;
                model.set_row_data(i, seat);
            }
        }
    }
    if app.get_waiting_talking() != waiting_talking {
        app.set_waiting_talking(waiting_talking);
    }
}

/// 把事件从读线程搬到界面线程。
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
            let posted = weak.upgrade_in_event_loop(move |app| match event {
                Event::Reconnecting {
                    attempt, reason, ..
                } => {
                    // 旧链路的会话和密钥都作废了，发出去的声音服务端只会丢掉。
                    // 停掉它，别让用户以为自己还在被人听见 —— 麦克风指示灯也跟着灭。
                    state.lock().expect("state poisoned").voice = None;
                    app.set_udp_ok(false);
                    app.set_transmitting(false);
                    app.set_input_level(0.0);
                    app.set_reconnecting(
                        format!("连接断了，正在自动重连（第 {attempt} 次）。\n{reason}").into(),
                    );
                }
                Event::Reconnected => {
                    app.set_reconnecting("".into());
                    app.set_voice_error("".into());
                    // 服务端可能又给改了名（重名加后缀），以它为准。
                    let actual = {
                        let roster = client.roster();
                        roster.name_of(roster.me)
                    };
                    if !actual.is_empty() {
                        app.set_nick(actual.into());
                    }
                    refresh(&app, &state, &client);
                    // 会话 id、端口、密钥全换了，语音链路只能按新的重起。
                    start_voice(&app, &state, &client);
                }
                Event::Disconnected(ended) => {
                    let mut locked = state.lock().expect("state poisoned");
                    // 旧连接的收尾可能晚到：用户点了离开、马上又连了别的服务器，
                    // 这时候不能把新连接也一起拆了。
                    if !locked.client.as_ref().is_some_and(|c| c.is_same(&client)) {
                        return;
                    }
                    locked.client = None;
                    // 丢掉链路会 join 掉所有音频线程。**必须做** ——
                    // 留着的话麦克风还开着，而用户已经不在频道里了。
                    locked.voice = None;
                    drop(locked);
                    app.set_connected(false);
                    app.set_reconnecting("".into());
                    match ended {
                        // 自己走的不是错误，别在登录页上挂一条报错。
                        Ended::ByUser => {
                            app.set_error_headline("".into());
                            app.set_error_advice("".into());
                        }
                        Ended::Refused { headline, advice } => {
                            app.set_error_headline(headline.into());
                            app.set_error_advice(advice.into());
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
            });
            if posted.is_err() {
                // 窗口关了
                return;
            }
        }
    });
}

/// 有人进出我所在的频道：响一声，按设置再念个名字。
///
/// 声音塞进正在跑的那条链路的播放里（见 `voice_core::cue`）：连着服务器是
/// 语音链路，没连的时候是设置页上的独立试麦 —— 后者只有 `preview` 会用到。
/// 两个都没有就不响：没有地方可以放。
fn announce(state: &Arc<Mutex<State>>, kind: Chime, name: &str, preview: bool) {
    let mut locked = state.lock().expect("state poisoned");
    let sink = match (&locked.voice, &locked.mic_check) {
        (Some(voice), _) => voice.cues(),
        (None, Some(mic)) if preview => mic.cues(),
        _ => return,
    };
    let settings = &locked.settings;
    let gain = settings.cue_volume as f32 / 100.0;
    let (sounds, names) = (settings.cue_sounds, settings.announce_names);
    // 试听的时候两样都响，不管开没开 —— 不然两个都关着的人点了「试听」
    // 什么也听不到，只会以为坏了。
    if sounds || preview {
        sink.push(&chime(kind), gain);
    }
    if names || preview {
        if locked.announcer.is_none() {
            locked.announcer = Announcer::start().ok();
        }
        if let Some(announcer) = &locked.announcer {
            let verb = match kind {
                Chime::CameIn => "进来了",
                Chime::WentOut => "走了",
            };
            announcer.say(&format!("{}{verb}", speakable_name(name)), sink, gain);
        }
    }
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
        let Some(voice) = locked.voice.clone() else {
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
/// 每次事件都整个重建列表，不做增量。20 个人几十条消息，重建的代价
/// 完全测不出来；而增量更新是「名单和实际对不上」这类 bug 的主要来源。
fn refresh(app: &App, state: &Arc<Mutex<State>>, client: &Client) {
    // 先把要用的设置拷出来再去拿名单的锁。**不同时持有两把** ——
    // 别的地方有先拿名单再拿 state 的，两把一起拿迟早死锁。
    let settings = state.lock().expect("state poisoned").settings.clone();
    apply_volumes(state, client);

    let roster = client.roster();

    let mut rows: Vec<Row> = Vec::new();
    for node in roster.tree() {
        let members = roster.users_in(node.channel.id);
        rows.push(Row {
            is_channel: true,
            id: node.channel.id as i32,
            name: node.channel.name.clone().into(),
            depth: node.depth as i32,
            muted: false,
            deafened: false,
            speaking: false,
            is_me: false,
            is_current: node.channel.id == roster.my_channel(),
            count: members.len() as i32,
            can_delete: roster.can_delete_channel(node.channel.id),
            volume: 100,
            role: 0,
            can_kick: false,
            can_ban: false,
            can_set_role: false,
            can_edit: roster.can_edit_channel(node.channel.id),
        });
        for user in members {
            rows.push(Row {
                is_channel: false,
                id: user.session_id as i32,
                name: user.name.clone().into(),
                depth: node.depth as i32 + 1,
                muted: user.self_muted || user.server_muted,
                deafened: user.self_deafened,
                // 说话指示要等 UDP 那半边接上
                speaking: false,
                is_me: user.session_id == roster.me,
                is_current: false,
                count: 0,
                // 只对频道有意义。
                can_delete: false,
                volume: settings.user_volume(&volume_key(&user.public_key)) as i32,
                role: user.role,
                can_kick: roster.can_kick(user.session_id),
                can_ban: roster.can_ban(user.session_id),
                can_set_role: roster.can_set_role(user.session_id),
                can_edit: false,
            });
        }
    }

    let chat: Vec<ChatRow> = roster
        .chat
        .iter()
        .map(|line| ChatRow {
            sender: line.sender_name.clone().into(),
            body: line.body.clone().into(),
            time: clock_time(line.timestamp_ms).into(),
            is_me: line.sender_session == roster.me,
        })
        .collect();

    // 自己的静音状态以服务端为准 —— 别的地方（将来的全局热键）也会改它。
    if let Some(me) = roster.my_user() {
        app.set_self_muted(me.self_muted);
        app.set_self_deafened(me.self_deafened);
    }

    // 访客建不了频道，那一行就别显示。**这只是画界面** ——
    // 真正说了算的是服务端，它会把访客的请求直接忽略掉。
    app.set_can_create_channel(roster.can_create_channel());

    // 封禁名单只有管理员手里有（服务端只发给管理员），别人这里是空的。
    app.set_is_admin(roster.is_admin());
    let bans: Vec<BanRow> = roster
        .bans
        .iter()
        .map(|ban| {
            let mut detail = format!("{} 被 {} 封禁", clock_date(ban.banned_at_ms), ban.banned_by);
            if !ban.reason.is_empty() {
                detail.push('：');
                detail.push_str(&ban.reason);
            }
            BanRow {
                name: ban.name.clone().into(),
                detail: detail.into(),
                key: protocol::base32::encode(&ban.public_key).into(),
            }
        })
        .collect();
    app.set_bans(ModelRc::new(VecModel::from(bans)));

    // 篝火上的人。先把要用的从名单里抄出来，放开名单的锁再去拿 state 的。
    let my_channel = roster.my_channel();
    let me = roster.me;
    let channel_name = roster
        .channels
        .get(&my_channel)
        .map(|c| c.name.clone())
        .unwrap_or_default();
    let here: Vec<Seat> = roster
        .users_in(my_channel)
        .into_iter()
        .map(|user| Seat {
            present: true,
            id: user.session_id as i32,
            name: user.name.clone().into(),
            glyph: campfire::glyph(&user.name).into(),
            seed: campfire::stone_seed(&user.public_key) as i32,
            stone: Default::default(),
            stone_talking: Default::default(),
            muted: user.self_muted || user.server_muted,
            deafened: user.self_deafened,
            speaking: false,
            is_me: user.session_id == me,
            volume: settings.user_volume(&volume_key(&user.public_key)) as i32,
            role: user.role,
            can_kick: roster.can_kick(user.session_id),
            can_ban: roster.can_ban(user.session_id),
            can_set_role: roster.can_set_role(user.session_id),
        })
        .collect();
    drop(roster);

    app.set_rows(ModelRc::new(VecModel::from(rows)));
    app.set_chat(ModelRc::new(VecModel::from(chat)));
    app.set_channel_name(channel_name.into());
    refresh_campfire(app, state, my_channel, me, here);
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

    // 谁在说话要等下一次同步（50 ms 一次）才知道。这之前先沿用模型里原来的，
    // 不然每次刷新石头都会灭一下。
    let was_speaking = |id: i32| {
        [app.get_seats(), app.get_waiting()]
            .iter()
            .any(|m| m.iter().any(|s| s.present && s.id == id && s.speaking))
    };
    let find = |id: u32| {
        here.iter()
            .find(|s| s.id as u32 == id)
            .cloned()
            .map(|mut s| {
                s.speaking = was_speaking(s.id);
                s
            })
    };
    let size = CAMPFIRE.with(|stage| {
        let mut stage = stage.borrow_mut();
        if stage.set_channel(channel) {
            app.set_campfire_backdrop(stage.still());
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

    app.set_waiting_talking(waiting.iter().any(|s| s.speaking));
    let current = app.get_seats();
    if !update_in_place(&current, seats.clone()) {
        app.set_seats(ModelRc::new(VecModel::from(seats)));
    }
    let current = app.get_waiting();
    if !update_in_place(&current, waiting.clone()) {
        app.set_waiting(ModelRc::new(VecModel::from(waiting)));
    }
}

/// 篝火画面的大小变了：底图和石头都按新尺寸重画（一格的逻辑大小不变，格数变了）。
fn resize_campfire(app: &App, width: f32, height: f32) {
    let backdrop = CAMPFIRE.with(|stage| {
        let mut stage = stage.borrow_mut();
        stage.set_size((width, height)).then(|| stage.still())
    });
    let Some(backdrop) = backdrop else {
        return;
    };
    app.set_campfire_backdrop(backdrop);
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
    if !app.get_campfire_shown()
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
        let frame = CAMPFIRE.with(|stage| stage.borrow_mut().advance(dt));
        app.set_campfire_backdrop(frame);
    });
    timer.stop();
    FIRE_TIMER.with(|slot| *slot.borrow_mut() = Some(timer));
}

/// 行数一样就逐行改（只改变了的），返回 true；行数变了返回 false，由调用方整个换掉。
fn update_in_place<T: Clone + PartialEq + 'static>(model: &ModelRc<T>, rows: Vec<T>) -> bool {
    if model.row_count() != rows.len() {
        return false;
    }
    for (i, row) in rows.into_iter().enumerate() {
        if model.row_data(i).as_ref() != Some(&row) {
            model.set_row_data(i, row);
        }
    }
    true
}

fn wire_actions(app: &App, state: &Arc<Mutex<State>>) {
    // 每个回调都要拿到当前连接。写成一个小闭包而不是宏 ——
    // 回调的签名各不相同（有的带参数有的不带），宏反而要绕。
    fn current(state: &Arc<Mutex<State>>) -> Option<Client> {
        state.lock().expect("state poisoned").client.clone()
    }

    {
        let weak = app.as_weak();
        app.on_campfire_resized(move |width, height| {
            if let Some(app) = weak.upgrade() {
                resize_campfire(&app, width, height);
            }
        });
    }

    {
        let state = Arc::clone(state);
        app.on_join_channel(move |id| {
            if let Some(client) = current(&state) {
                client.join_channel(id as u32);
            }
        });
    }

    {
        let state = Arc::clone(state);
        app.on_create_channel(move |name| {
            if let Some(client) = current(&state) {
                // 不在本地先插一个再等确认：建成了服务端会广播给所有人
                // （含自己），名单那边照常处理；被拒就什么都不会发生。
                // 本地先插的话，被拒时界面上会留一个只有自己看得见的幽灵频道。
                client.create_channel(&name, 0);
            }
        });
    }

    {
        let state = Arc::clone(state);
        let weak = app.as_weak();
        app.on_set_user_volume(move |session, raw| {
            let (Some(app), Some(client)) = (weak.upgrade(), current(&state)) else {
                return;
            };
            let session = session as u32;
            let percent = snap_volume(raw);
            let Some(key) = client
                .roster()
                .users
                .get(&session)
                .map(|u| volume_key(&u.public_key))
            else {
                return;
            };
            let voice = {
                let mut locked = state.lock().expect("state poisoned");
                locked.settings.set_user_volume(&key, percent);
                locked.voice.clone()
            };
            if let Some(voice) = voice {
                voice.set_volume(session, percent as f32 / 100.0);
            }
            // 只改这一行，不整个重建名单：拖动时每秒几十次，整个重建会让
            // 正拖着的滑条被销毁重建，手里的那一下就断了。
            let rows = app.get_rows();
            for i in 0..rows.row_count() {
                let Some(mut row) = rows.row_data(i) else {
                    continue;
                };
                if !row.is_channel && row.id == session as i32 {
                    if row.volume != percent as i32 {
                        row.volume = percent as i32;
                        rows.set_row_data(i, row);
                    }
                    break;
                }
            }
            // 篝火上点开的那张卡片也在拖这个音量，同样只改那一块石头。
            for model in [app.get_seats(), app.get_waiting()] {
                for i in 0..model.row_count() {
                    let Some(mut seat) = model.row_data(i) else {
                        continue;
                    };
                    if seat.present && seat.id == session as i32 && seat.volume != percent as i32 {
                        seat.volume = percent as i32;
                        model.set_row_data(i, seat);
                    }
                }
            }
        });
    }

    {
        let state = Arc::clone(state);
        app.on_save_user_volumes(move || {
            let _ = state.lock().expect("state poisoned").settings.save();
        });
    }

    {
        let state = Arc::clone(state);
        app.on_rename_channel(move |id, name| {
            if let Some(client) = current(&state) {
                client.rename_channel(id as u32, &name);
            }
        });
    }

    {
        let state = Arc::clone(state);
        app.on_kick_user(move |session| {
            if let Some(client) = current(&state) {
                client.kick(session as u32, "");
            }
        });
    }

    {
        let state = Arc::clone(state);
        app.on_ban_user(move |session| {
            if let Some(client) = current(&state) {
                client.ban(session as u32, "");
            }
        });
    }

    {
        let state = Arc::clone(state);
        app.on_set_role(move |session, index| {
            // 下拉框的序号 0..=3 对应协议里的 1..=4（0 是 Unspecified）。
            let role = protocol::control::Role::try_from(index + 1)
                .unwrap_or(protocol::control::Role::Unspecified);
            if let Some(client) = current(&state) {
                client.set_role(session as u32, role);
            }
        });
    }

    {
        let state = Arc::clone(state);
        app.on_unban(move |key| {
            let Ok(key) = protocol::base32::decode(&key) else {
                return;
            };
            if let Some(client) = current(&state) {
                client.unban(&key);
            }
        });
    }

    {
        let state = Arc::clone(state);
        app.on_delete_channel(move |id| {
            if let Some(client) = current(&state) {
                client.delete_channel(id as u32);
            }
        });
    }

    {
        let state = Arc::clone(state);
        let weak = app.as_weak();
        app.on_send_text(move || {
            let (Some(app), Some(client)) = (weak.upgrade(), current(&state)) else {
                return;
            };
            client.send_text(&app.get_draft());
            app.set_draft(SharedString::new());
        });
    }

    {
        let state = Arc::clone(state);
        let weak = app.as_weak();
        app.on_toggle_mute(move || {
            let (Some(app), Some(client)) = (weak.upgrade(), current(&state)) else {
                return;
            };
            // 关着耳朵的时候单独开麦没有意义，服务端也会把它改回去。
            let muted = !app.get_self_muted();
            client.set_self_state(muted, app.get_self_deafened());
            if let Some(voice) = current_voice(&state) {
                voice.set_muted(muted);
            }
        });
    }

    {
        let state = Arc::clone(state);
        let weak = app.as_weak();
        app.on_toggle_deafen(move || {
            let (Some(app), Some(client)) = (weak.upgrade(), current(&state)) else {
                return;
            };
            let deafened = !app.get_self_deafened();
            // 关耳朵连带闭麦。服务端也会这么改，这里跟着改是为了按下去立刻有反馈。
            client.set_self_state(app.get_self_muted() || deafened, deafened);
            if let Some(voice) = current_voice(&state) {
                voice.set_deafened(deafened);
                voice.set_muted(app.get_self_muted() || deafened);
            }
        });
    }

    {
        let state = Arc::clone(state);
        let weak = app.as_weak();
        app.on_leave(move || {
            if let Some(client) = current(&state) {
                client.disconnect();
            }
            state.lock().expect("state poisoned").voice = None;
            if let Some(app) = weak.upgrade() {
                // 主动离开不是错误，别把上一次的报错留在登录页上。
                app.set_connected(false);
                app.set_error_headline("".into());
                app.set_error_advice("".into());
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
            let Some(client) = client.filter(|_| app.get_connected() && has_tray) else {
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
                let _ = locked.settings.save();
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
            let _ = locked.settings.save();
        });
    }
}

/// 真的退出：先离开频道（服务端和别人那边马上看到你走了，而不是等 30 秒超时），
/// 再停掉事件循环。
fn quit_app(state: &Arc<Mutex<State>>) {
    let client = {
        let mut locked = state.lock().expect("state poisoned");
        locked.voice = None;
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
fn update_tray(app: &App, client: Option<&Client>) {
    TRAY.with(|slot| {
        let slot = slot.borrow();
        let Some(tray) = slot.as_ref() else { return };
        let connected = app.get_connected() && client.is_some();
        let muted = app.get_self_muted();
        let deafened = app.get_self_deafened();
        let icon = match (connected, muted || deafened) {
            (false, _) => 0,
            (true, false) => 1,
            (true, true) => 2,
        };
        let status = match client {
            _ if !connected => "篝火 · 没连着".to_string(),
            _ if !app.get_reconnecting().is_empty() => "篝火 · 正在重连…".to_string(),
            Some(client) => {
                let mic = if deafened {
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

/// 返回 `(身份, 是不是这次新建的)`。
fn current_voice(state: &Arc<Mutex<State>>) -> Option<Arc<Pipeline>> {
    state.lock().expect("state poisoned").voice.clone()
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
