// SPDX-License-Identifier: GPL-3.0-or-later

//! 窗口和系统的胶水：报错框、打开网页、抢前台、深色标题栏、前台是谁、系统动画开关。
//!
//! 每个函数都有 Windows 实现和其他平台的替代实现；非 Windows 只用于开发和 CI 编译。

use super::*;

#[cfg(windows)]
pub(crate) fn show_fatal(message: &str) {
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
pub(crate) fn show_fatal(message: &str) {
    eprintln!("{message}");
}

#[cfg(windows)]
pub(crate) fn open_in_browser(url: &str) {
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
pub(crate) fn open_in_browser(_url: &str) {}

/// 把窗口拉到最前面。转交的那个进程已经用 AllowSetForegroundWindow 把权限让给我们了，
/// 不然 Windows 只会让任务栏按钮闪一下。
#[cfg(windows)]
pub(crate) fn bring_to_front(app: &App) {
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
pub(crate) fn bring_to_front(_app: &App) {}

/// 让标题栏跟着窗口一起是深色的。
///
/// 深色应用配一条浅色标题栏，是「界面停留在 2010 年」最典型的症状之一：
/// 整个窗口看着像是两个程序拼起来的。Windows 10 1809 起支持这个属性，
/// 更早的版本上这次调用会失败，忽略就行 —— 老系统上只是标题栏还是浅色。
#[cfg(windows)]
pub(crate) fn dark_titlebar(app: &App) {
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
pub(crate) fn dark_titlebar(_app: &App) {}

pub(crate) enum Foreground {
    Us,
    /// 别的程序，而且铺满了我们所在的那块屏幕。
    FullscreenOther,
    Other,
}

#[cfg(windows)]
pub(crate) fn foreground(app: &App) -> Foreground {
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
pub(crate) fn foreground(_app: &App) -> Foreground {
    Foreground::Us
}

/// Windows 设置里「显示动画」关了没有（辅助功能 → 视觉效果 → 动画效果）。
///
/// 关了的人要么是晃眼、要么是机器吃力，火就不动了，只画一帧。
#[cfg(windows)]
pub(crate) fn system_animations_on() -> bool {
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
pub(crate) fn system_animations_on() -> bool {
    true
}
