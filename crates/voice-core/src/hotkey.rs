// SPDX-License-Identifier: MPL-2.0

//! 全局热键：游戏全屏时也能按住说话。
//!
//! # 为什么不用 RegisterHotKey
//!
//! `RegisterHotKey` 是最省事的那条路，但它有两个致命问题：
//!
//! 1. **它把按键吃掉。** 注册了 `V` 之后，游戏就收不到 `V` 了 ——
//!    而按住说话的那个键，玩家经常同时绑着游戏里的功能。
//! 2. **全屏独占下不可靠。** 它走的是焦点窗口那条路径，很多全屏游戏下
//!    根本不触发。
//!
//! # 为什么不用低级键盘钩子
//!
//! `SetWindowsHookEx(WH_KEYBOARD_LL)` 能收到全部按键，也能选择不吃掉。
//! 但它是**系统级注入**：每一次按键都要经过我们的回调，我们慢一下全系统的
//! 输入就卡一下。而且它是反作弊最敏感的那类行为之一 —— 一个开黑软件
//! 不该让人担心自己会不会因此被封号。
//!
//! # Raw Input
//!
//! `RIDEV_INPUTSINK` 让我们在**没有焦点**的时候也能收到原始输入，
//! 同时：
//!
//! - **不吃掉按键** —— 游戏照样收得到
//! - **不注入任何东西** —— 只是订阅一个广播
//! - 不在别人的输入路径上，我们慢了也不影响别人
//!
//! 代价是要有一个窗口和一个消息循环。用 `HWND_MESSAGE` 建一个纯消息窗口，
//! 不可见、不进任务栏、不参与 Z 序。
//!
//! # 鼠标侧键
//!
//! 按住说话绑鼠标侧键（前进/后退那两个）在玩家里极其常见，所以一起收。
//! Raw Input 里这只是多注册一个用途页的事。

use std::sync::atomic::{AtomicBool, AtomicU32, Ordering};
use std::sync::mpsc::{self, Receiver, Sender};
use std::sync::{Arc, Mutex};

/// 一个能当热键用的键。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Key {
    /// 键盘，值是 Windows 的虚拟键码。
    Keyboard(u16),
    /// 鼠标侧键。4 是「后退」，5 是「前进」。
    ///
    /// 左右中键不收：那三个在游戏里全都有用，绑上去等于废掉一个操作。
    Mouse(u8),
}

impl Key {
    /// 编码成一个 u32，好放进原子变量里。0 表示「没设」。
    pub fn encode(self) -> u32 {
        match self {
            Key::Keyboard(vk) => vk as u32,
            Key::Mouse(button) => 0x1_0000 | button as u32,
        }
    }

    pub fn decode(raw: u32) -> Option<Self> {
        match raw {
            0 => None,
            v if v & 0x1_0000 != 0 => Some(Key::Mouse((v & 0xFF) as u8)),
            v => Some(Key::Keyboard(v as u16)),
        }
    }

    /// 给人看的名字。
    pub fn label(self) -> String {
        match self {
            Key::Mouse(button) => format!("鼠标侧键 {button}"),
            Key::Keyboard(vk) => keyboard_label(vk),
        }
    }
}

/// 热键监听器。丢掉它就会把消息线程停掉。
pub struct Hotkeys {
    shared: Arc<Shared>,
    thread: Option<std::thread::JoinHandle<()>>,
}

struct Shared {
    /// 当前绑的按住说话键，[`Key::encode`] 过的。
    ptt: AtomicU32,
    /// 那个键现在按着没有。
    down: AtomicBool,
    /// 在「按一个键来设置」状态：下一个按下的键会被报出去而不是当热键用。
    capturing: AtomicBool,
    /// 捕获到的键往这儿送。
    captured: Mutex<Option<Sender<Key>>>,
    stop: AtomicBool,
    /// 消息线程的 id，用来叫醒它。
    thread_id: AtomicU32,
}

impl Hotkeys {
    /// 起监听。**立刻返回**，消息循环在后台线程上跑。
    pub fn start() -> std::io::Result<Self> {
        let shared = Arc::new(Shared {
            ptt: AtomicU32::new(0),
            down: AtomicBool::new(false),
            capturing: AtomicBool::new(false),
            captured: Mutex::new(None),
            stop: AtomicBool::new(false),
            thread_id: AtomicU32::new(0),
        });

        let (ready_tx, ready_rx) = mpsc::channel();
        let thread_shared = Arc::clone(&shared);
        let thread = std::thread::Builder::new()
            .name("gouhuo-hotkeys".into())
            .spawn(move || platform::run(thread_shared, ready_tx))?;

        // 等线程真的把窗口建起来 —— 建不起来要在这里就报错，
        // 而不是让用户按了半天键才发现没反应。
        match ready_rx.recv() {
            Ok(Ok(())) => Ok(Self {
                shared,
                thread: Some(thread),
            }),
            Ok(Err(e)) => Err(e),
            Err(_) => Err(std::io::Error::other("热键线程没起来")),
        }
    }

    /// 换按住说话的键。`None` 表示不绑。
    pub fn set_ptt(&self, key: Option<Key>) {
        self.shared
            .ptt
            .store(key.map(Key::encode).unwrap_or(0), Ordering::Relaxed);
        // 换键的时候把按下状态清掉，否则会卡在「一直按着」
        self.shared.down.store(false, Ordering::Relaxed);
    }

    pub fn ptt(&self) -> Option<Key> {
        Key::decode(self.shared.ptt.load(Ordering::Relaxed))
    }

    /// 按住说话的键现在按着没有。
    pub fn is_down(&self) -> bool {
        self.shared.down.load(Ordering::Relaxed)
    }

    /// 进入「按一个键来设置」。下一个按下的键会从返回的通道里出来，
    /// 而且**不会**被当成按住说话触发。
    pub fn capture_next(&self) -> Receiver<Key> {
        let (tx, rx) = mpsc::channel();
        *self.shared.captured.lock().expect("captured poisoned") = Some(tx);
        self.shared.capturing.store(true, Ordering::Relaxed);
        rx
    }

    /// 取消「按一个键来设置」。
    pub fn cancel_capture(&self) {
        self.shared.capturing.store(false, Ordering::Relaxed);
        *self.shared.captured.lock().expect("captured poisoned") = None;
    }
}

impl Drop for Hotkeys {
    fn drop(&mut self) {
        self.shared.stop.store(true, Ordering::Relaxed);
        platform::wake(self.shared.thread_id.load(Ordering::Relaxed));
        if let Some(thread) = self.thread.take() {
            let _ = thread.join();
        }
    }
}

/// 处理一次原始输入事件。放在平台代码外面，好单独测。
#[cfg(any(windows, test))]
fn handle(shared: &Shared, key: Key, pressed: bool) {
    if shared.capturing.load(Ordering::Relaxed) {
        // 只在**按下**的时候捕获。按松开的话，用户抬手那一下会把
        // 刚设好的键立刻又设一遍。
        if !pressed {
            return;
        }
        shared.capturing.store(false, Ordering::Relaxed);
        if let Some(tx) = shared.captured.lock().expect("captured poisoned").take() {
            let _ = tx.send(key);
        }
        return;
    }

    let bound = shared.ptt.load(Ordering::Relaxed);
    if bound != 0 && bound == key.encode() {
        shared.down.store(pressed, Ordering::Relaxed);
    }
}

fn keyboard_label(vk: u16) -> String {
    // 自己给名字的几类键，都是 `GetKeyNameTextW` 处理得不好的：
    //
    // - **左右不分**：它对左右 Ctrl/Shift/Alt 给的是同一个名字，
    //   而按住说话绑「右 Alt」和绑「左 Alt」是完全不同的选择
    // - **F13–F24**：正常键盘上没有这些键，所以拿不到名字。但它们恰恰是
    //   按住说话的**最佳选择** —— 不跟任何东西冲突，用改键软件或者
    //   键盘宏就能映射出来
    // - **小键盘**：它给的名字跟主键盘那排数字一样，分不出来
    if let Some(name) = match vk {
        0x20 => Some("空格".to_string()),
        0x09 => Some("Tab".to_string()),
        0x14 => Some("Caps Lock".to_string()),
        0xA0 => Some("左 Shift".to_string()),
        0xA1 => Some("右 Shift".to_string()),
        0xA2 => Some("左 Ctrl".to_string()),
        0xA3 => Some("右 Ctrl".to_string()),
        0xA4 => Some("左 Alt".to_string()),
        0xA5 => Some("右 Alt".to_string()),
        0x5B => Some("左 Win".to_string()),
        0x5C => Some("右 Win".to_string()),
        // F1–F24
        0x70..=0x87 => Some(format!("F{}", vk - 0x70 + 1)),
        // 小键盘 0–9
        0x60..=0x69 => Some(format!("小键盘 {}", vk - 0x60)),
        0x6A => Some("小键盘 *".to_string()),
        0x6B => Some("小键盘 +".to_string()),
        0x6D => Some("小键盘 -".to_string()),
        0x6E => Some("小键盘 .".to_string()),
        0x6F => Some("小键盘 /".to_string()),
        _ => None,
    } {
        return name;
    }
    platform::key_name(vk).unwrap_or_else(|| format!("键 {vk}"))
}

// ===========================================================================

#[cfg(windows)]
mod platform {
    use super::{handle, Key, Shared};
    use std::sync::atomic::Ordering;
    use std::sync::mpsc::Sender;
    use std::sync::Arc;

    use windows::core::PCWSTR;
    use windows::Win32::Foundation::{HWND, LPARAM, LRESULT, WPARAM};
    use windows::Win32::System::LibraryLoader::GetModuleHandleW;
    use windows::Win32::UI::Input::KeyboardAndMouse::{
        GetKeyNameTextW, MapVirtualKeyW, MAPVK_VK_TO_VSC,
    };
    use windows::Win32::UI::Input::{
        GetRawInputData, RegisterRawInputDevices, HRAWINPUT, RAWINPUT, RAWINPUTDEVICE,
        RAWINPUTHEADER, RIDEV_INPUTSINK, RID_INPUT, RIM_TYPEKEYBOARD, RIM_TYPEMOUSE,
    };
    use windows::Win32::UI::WindowsAndMessaging::{
        CreateWindowExW, DefWindowProcW, DestroyWindow, DispatchMessageW, GetMessageW,
        PostThreadMessageW, RegisterClassW, TranslateMessage, HWND_MESSAGE, MSG, WINDOW_EX_STYLE,
        WINDOW_STYLE, WM_INPUT, WM_QUIT, WNDCLASSW,
    };

    /// 键盘的 HID 用途页/用途。
    const USAGE_PAGE_GENERIC: u16 = 0x01;
    const USAGE_KEYBOARD: u16 = 0x06;
    const USAGE_MOUSE: u16 = 0x02;

    /// RAWKEYBOARD.Flags：这一位是 1 表示松开。
    const RI_KEY_BREAK: u16 = 0x01;

    const RI_MOUSE_BUTTON_4_DOWN: u16 = 0x0040;
    const RI_MOUSE_BUTTON_4_UP: u16 = 0x0080;
    const RI_MOUSE_BUTTON_5_DOWN: u16 = 0x0100;
    const RI_MOUSE_BUTTON_5_UP: u16 = 0x0200;

    thread_local! {
        /// WndProc 拿不到额外参数，只能走线程局部变量。
        /// 反正窗口和消息循环都在同一个线程上。
        static STATE: std::cell::RefCell<Option<Arc<Shared>>> =
            const { std::cell::RefCell::new(None) };
    }

    pub(super) fn run(shared: Arc<Shared>, ready: Sender<std::io::Result<()>>) {
        shared.thread_id.store(
            unsafe { windows::Win32::System::Threading::GetCurrentThreadId() },
            Ordering::Relaxed,
        );
        STATE.with(|slot| *slot.borrow_mut() = Some(Arc::clone(&shared)));

        let hwnd = match create_window() {
            Ok(hwnd) => hwnd,
            Err(e) => {
                let _ = ready.send(Err(e));
                return;
            }
        };
        if let Err(e) = register(hwnd) {
            let _ = ready.send(Err(e));
            unsafe {
                let _ = DestroyWindow(hwnd);
            };
            return;
        }
        let _ = ready.send(Ok(()));

        let mut msg = MSG::default();
        // GetMessageW 返回 0 表示收到 WM_QUIT，返回 -1 表示出错。
        while unsafe { GetMessageW(&mut msg, None, 0, 0) }.0 > 0 {
            if shared.stop.load(Ordering::Relaxed) {
                break;
            }
            unsafe {
                let _ = TranslateMessage(&msg);
                DispatchMessageW(&msg);
            }
        }
        unsafe {
            let _ = DestroyWindow(hwnd);
        };
        STATE.with(|slot| *slot.borrow_mut() = None);
    }

    /// 给消息线程发一条消息，把阻塞在 GetMessageW 上的它叫醒。
    pub(super) fn wake(thread_id: u32) {
        if thread_id != 0 {
            unsafe {
                let _ = PostThreadMessageW(thread_id, WM_QUIT, WPARAM(0), LPARAM(0));
            }
        }
    }

    fn create_window() -> std::io::Result<HWND> {
        let class_name: Vec<u16> = "gouhuo_hotkeys\0".encode_utf16().collect();
        let instance = unsafe { GetModuleHandleW(None) }
            .map_err(|e| std::io::Error::other(format!("拿不到模块句柄：{e}")))?;

        let class = WNDCLASSW {
            lpfnWndProc: Some(wndproc),
            hInstance: instance.into(),
            lpszClassName: PCWSTR(class_name.as_ptr()),
            ..Default::default()
        };
        // 注册失败最常见的原因是「已经注册过了」，那没关系 —— 直接往下建窗口。
        unsafe { RegisterClassW(&class) };

        // HWND_MESSAGE：纯消息窗口。不可见、不进任务栏、不参与 Z 序，
        // 唯一的作用就是有个地方收 WM_INPUT。
        let hwnd = unsafe {
            CreateWindowExW(
                WINDOW_EX_STYLE(0),
                PCWSTR(class_name.as_ptr()),
                PCWSTR::null(),
                WINDOW_STYLE(0),
                0,
                0,
                0,
                0,
                Some(HWND_MESSAGE),
                None,
                Some(instance.into()),
                None,
            )
        }
        .map_err(|e| std::io::Error::other(format!("建不了消息窗口：{e}")))?;
        Ok(hwnd)
    }

    fn register(hwnd: HWND) -> std::io::Result<()> {
        let devices = [
            RAWINPUTDEVICE {
                usUsagePage: USAGE_PAGE_GENERIC,
                usUsage: USAGE_KEYBOARD,
                // INPUTSINK：没有焦点也收。这就是「全屏游戏里也能按住说话」的全部秘密。
                dwFlags: RIDEV_INPUTSINK,
                hwndTarget: hwnd,
            },
            RAWINPUTDEVICE {
                usUsagePage: USAGE_PAGE_GENERIC,
                usUsage: USAGE_MOUSE,
                dwFlags: RIDEV_INPUTSINK,
                hwndTarget: hwnd,
            },
        ];
        unsafe { RegisterRawInputDevices(&devices, std::mem::size_of::<RAWINPUTDEVICE>() as u32) }
            .map_err(|e| std::io::Error::other(format!("订阅不了原始输入：{e}")))
    }

    unsafe extern "system" fn wndproc(
        hwnd: HWND,
        message: u32,
        wparam: WPARAM,
        lparam: LPARAM,
    ) -> LRESULT {
        if message == WM_INPUT {
            on_raw_input(HRAWINPUT(lparam.0 as *mut core::ffi::c_void));
        }
        unsafe { DefWindowProcW(hwnd, message, wparam, lparam) }
    }

    fn on_raw_input(handle_raw: HRAWINPUT) {
        let mut size = 0u32;
        let header_size = std::mem::size_of::<RAWINPUTHEADER>() as u32;
        // 先问要多大。给 None 时它只填 size。
        unsafe {
            GetRawInputData(handle_raw, RID_INPUT, None, &mut size, header_size);
        }
        if size == 0 || size as usize > std::mem::size_of::<RAWINPUT>() * 2 {
            return;
        }

        let mut data = RAWINPUT::default();
        let got = unsafe {
            GetRawInputData(
                handle_raw,
                RID_INPUT,
                Some(&mut data as *mut _ as *mut core::ffi::c_void),
                &mut size,
                header_size,
            )
        };
        if got == u32::MAX || got == 0 {
            return;
        }

        let event = match data.header.dwType {
            t if t == RIM_TYPEKEYBOARD.0 => {
                let keyboard = unsafe { data.data.keyboard };
                // VKey 为 0xFF 是「假键」，驱动用来占位的，忽略。
                if keyboard.VKey == 0xFF {
                    None
                } else {
                    Some((
                        Key::Keyboard(keyboard.VKey),
                        keyboard.Flags & RI_KEY_BREAK == 0,
                    ))
                }
            }
            t if t == RIM_TYPEMOUSE.0 => {
                let flags = unsafe { data.data.mouse.Anonymous.Anonymous.usButtonFlags };
                if flags & RI_MOUSE_BUTTON_4_DOWN != 0 {
                    Some((Key::Mouse(4), true))
                } else if flags & RI_MOUSE_BUTTON_4_UP != 0 {
                    Some((Key::Mouse(4), false))
                } else if flags & RI_MOUSE_BUTTON_5_DOWN != 0 {
                    Some((Key::Mouse(5), true))
                } else if flags & RI_MOUSE_BUTTON_5_UP != 0 {
                    Some((Key::Mouse(5), false))
                } else {
                    None
                }
            }
            _ => None,
        };

        let Some((key, pressed)) = event else { return };
        STATE.with(|slot| {
            if let Some(shared) = slot.borrow().as_ref() {
                handle(shared, key, pressed);
            }
        });
    }

    pub(super) fn key_name(vk: u16) -> Option<String> {
        let scan = unsafe { MapVirtualKeyW(vk as u32, MAPVK_VK_TO_VSC) };
        if scan == 0 {
            return None;
        }
        // GetKeyNameTextW 要的是 lParam 的格式：扫描码在 16–23 位。
        let lparam = (scan << 16) as i32;
        let mut buf = [0u16; 64];
        let len = unsafe { GetKeyNameTextW(lparam, &mut buf) };
        if len <= 0 {
            return None;
        }
        Some(String::from_utf16_lossy(&buf[..len as usize]))
    }
}

#[cfg(not(windows))]
mod platform {
    use super::Shared;
    use std::sync::mpsc::Sender;
    use std::sync::Arc;

    pub(super) fn run(_shared: Arc<Shared>, ready: Sender<std::io::Result<()>>) {
        let _ = ready.send(Err(std::io::Error::other("全局热键只支持 Windows")));
    }
    pub(super) fn wake(_thread_id: u32) {}
    pub(super) fn key_name(_vk: u16) -> Option<String> {
        None
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn shared() -> Shared {
        Shared {
            ptt: AtomicU32::new(0),
            down: AtomicBool::new(false),
            capturing: AtomicBool::new(false),
            captured: Mutex::new(None),
            stop: AtomicBool::new(false),
            thread_id: AtomicU32::new(0),
        }
    }

    #[test]
    fn keys_round_trip_through_the_atomic_encoding() {
        for key in [
            Key::Keyboard(0x20),
            Key::Keyboard(0xA2),
            Key::Mouse(4),
            Key::Mouse(5),
        ] {
            assert_eq!(Key::decode(key.encode()), Some(key), "{key:?}");
        }
        assert_eq!(Key::decode(0), None, "0 要表示「没设」");
    }

    /// 键盘和鼠标的编码不能撞。撞了的话绑鼠标侧键 4 会被虚拟键码 4 触发。
    #[test]
    fn keyboard_and_mouse_encodings_never_collide() {
        let mut seen = std::collections::BTreeSet::new();
        for vk in 0..=0xFFu16 {
            assert!(seen.insert(Key::Keyboard(vk).encode()));
        }
        for button in 4..=5u8 {
            assert!(
                seen.insert(Key::Mouse(button).encode()),
                "鼠标键 {button} 跟某个虚拟键码撞了"
            );
        }
    }

    #[test]
    fn press_and_release_track_the_bound_key() {
        let shared = shared();
        shared
            .ptt
            .store(Key::Keyboard(0x20).encode(), Ordering::Relaxed);

        handle(&shared, Key::Keyboard(0x20), true);
        assert!(shared.down.load(Ordering::Relaxed));
        handle(&shared, Key::Keyboard(0x20), false);
        assert!(!shared.down.load(Ordering::Relaxed));
    }

    /// 别的键不能影响按住说话 —— 否则打字就会一直在发声。
    #[test]
    fn other_keys_do_not_trigger_talking() {
        let shared = shared();
        shared
            .ptt
            .store(Key::Keyboard(0x20).encode(), Ordering::Relaxed);

        handle(&shared, Key::Keyboard(0x41), true);
        assert!(!shared.down.load(Ordering::Relaxed), "按 A 也开始说话了");

        handle(&shared, Key::Keyboard(0x20), true);
        handle(&shared, Key::Keyboard(0x41), false);
        assert!(
            shared.down.load(Ordering::Relaxed),
            "松开别的键把说话也停了"
        );
    }

    /// 没绑键的时候，任何键都不该触发。
    #[test]
    fn nothing_triggers_when_unbound() {
        let shared = shared();
        handle(&shared, Key::Keyboard(0x20), true);
        handle(&shared, Key::Mouse(4), true);
        assert!(!shared.down.load(Ordering::Relaxed));
    }

    #[test]
    fn capture_takes_the_next_press() {
        let shared = shared();
        let (tx, rx) = mpsc::channel();
        *shared.captured.lock().unwrap() = Some(tx);
        shared.capturing.store(true, Ordering::Relaxed);

        handle(&shared, Key::Mouse(5), true);
        assert_eq!(rx.try_recv().unwrap(), Key::Mouse(5));
        assert!(
            !shared.capturing.load(Ordering::Relaxed),
            "捕获完该自动退出"
        );
    }

    /// 捕获只认按下，不认松开。
    ///
    /// 认松开的话，用户抬手那一下会把刚设好的键立刻又设一遍 ——
    /// 而且如果他是用鼠标点的「设置」按钮，抬起的那一下就会把左键设成热键。
    #[test]
    fn capture_ignores_key_release() {
        let shared = shared();
        let (tx, rx) = mpsc::channel();
        *shared.captured.lock().unwrap() = Some(tx);
        shared.capturing.store(true, Ordering::Relaxed);

        handle(&shared, Key::Keyboard(0x20), false);
        assert!(rx.try_recv().is_err(), "松开也被当成设置了");
        assert!(shared.capturing.load(Ordering::Relaxed), "该还在等按下");

        handle(&shared, Key::Keyboard(0x20), true);
        assert_eq!(rx.try_recv().unwrap(), Key::Keyboard(0x20));
    }

    /// 正在设置热键的时候，按下去的那个键**不能**同时把麦克风打开。
    #[test]
    fn capturing_does_not_also_start_talking() {
        let shared = shared();
        shared
            .ptt
            .store(Key::Keyboard(0x20).encode(), Ordering::Relaxed);
        let (tx, _rx) = mpsc::channel();
        *shared.captured.lock().unwrap() = Some(tx);
        shared.capturing.store(true, Ordering::Relaxed);

        handle(&shared, Key::Keyboard(0x20), true);
        assert!(
            !shared.down.load(Ordering::Relaxed),
            "设置热键时把麦克风也开了"
        );
    }

    /// 真的把 Raw Input 那条路跑一遍：起监听，合成一次按键，看收不收得到。
    ///
    /// 默认跳过 —— 它会往系统里注入一次按键，跑测试的时候正在别处打字就会串进去。
    ///
    /// ```bash
    /// cargo test -p voice-core --lib hotkey -- --ignored --nocapture
    /// ```
    #[test]
    #[ignore = "会往系统里注入一次按键"]
    #[cfg(windows)]
    fn raw_input_really_sees_a_synthetic_key() {
        use windows::Win32::UI::Input::KeyboardAndMouse::{
            SendInput, INPUT, INPUT_0, INPUT_KEYBOARD, KEYBDINPUT, KEYEVENTF_KEYUP, VIRTUAL_KEY,
        };

        // F13。正常键盘上没有这个键，所以不会跟别的东西撞。
        const VK_F13: u16 = 0x7C;
        let hotkeys = Hotkeys::start().expect("热键监听起不来");
        hotkeys.set_ptt(Some(Key::Keyboard(VK_F13)));
        assert!(!hotkeys.is_down());

        let key = |up: bool| INPUT {
            r#type: INPUT_KEYBOARD,
            Anonymous: INPUT_0 {
                ki: KEYBDINPUT {
                    wVk: VIRTUAL_KEY(VK_F13),
                    wScan: 0,
                    dwFlags: if up {
                        KEYEVENTF_KEYUP
                    } else {
                        Default::default()
                    },
                    time: 0,
                    dwExtraInfo: 0,
                },
            },
        };

        unsafe { SendInput(&[key(false)], std::mem::size_of::<INPUT>() as i32) };
        assert!(
            wait_for(|| hotkeys.is_down()),
            "按下去了但 Raw Input 没收到 —— 全屏游戏里也就不会有反应"
        );

        unsafe { SendInput(&[key(true)], std::mem::size_of::<INPUT>() as i32) };
        assert!(wait_for(|| !hotkeys.is_down()), "松开了但状态没回去");
    }

    #[cfg(windows)]
    fn wait_for(mut condition: impl FnMut() -> bool) -> bool {
        let deadline = std::time::Instant::now() + std::time::Duration::from_secs(2);
        while std::time::Instant::now() < deadline {
            if condition() {
                return true;
            }
            std::thread::sleep(std::time::Duration::from_millis(10));
        }
        false
    }

    #[test]
    fn labels_are_human_readable() {
        assert_eq!(Key::Keyboard(0x20).label(), "空格");
        assert_eq!(Key::Keyboard(0xA2).label(), "左 Ctrl");
        assert_eq!(Key::Mouse(4).label(), "鼠标侧键 4");
        // 认不出来的键也要给个能显示的东西，不能是空字符串
        assert!(!Key::Keyboard(0x07).label().is_empty());
    }

    /// F13–F24 是按住说话的最佳选择（不跟任何东西冲突），而 Windows
    /// 给不出它们的名字 —— 正常键盘上没有这些键。
    #[test]
    fn function_keys_are_named_even_the_ones_no_keyboard_has() {
        assert_eq!(Key::Keyboard(0x70).label(), "F1");
        assert_eq!(Key::Keyboard(0x7B).label(), "F12");
        assert_eq!(Key::Keyboard(0x7C).label(), "F13");
        assert_eq!(Key::Keyboard(0x87).label(), "F24");
    }

    /// 左右必须分得开：绑「右 Alt」和绑「左 Alt」是完全不同的选择，
    /// 而 GetKeyNameTextW 对这两个给的是同一个名字。
    #[test]
    fn left_and_right_modifiers_are_distinguishable() {
        let pairs = [(0xA0, 0xA1), (0xA2, 0xA3), (0xA4, 0xA5), (0x5B, 0x5C)];
        for (left, right) in pairs {
            assert_ne!(
                Key::Keyboard(left).label(),
                Key::Keyboard(right).label(),
                "虚拟键码 {left:#x} 和 {right:#x} 的名字一样"
            );
        }
    }

    /// 小键盘的数字跟主键盘那排要分得开。
    #[test]
    fn the_numpad_is_distinguishable_from_the_number_row() {
        assert_eq!(Key::Keyboard(0x60).label(), "小键盘 0");
        assert_ne!(Key::Keyboard(0x60).label(), Key::Keyboard(0x30).label());
    }
}
