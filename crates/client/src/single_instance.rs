// SPDX-License-Identifier: GPL-3.0-or-later

//! 同一个用户（同一份身份）只开一个篝火。
//!
//! 装了安装包之后，点一条 `gouhuo://` 链接，Windows 会再启动一个 `gouhuo.exe`。
//! 两个进程用的是同一个身份 —— 后来的那个连上服务器，就把先开的那个顶下去了
//! （「你在别处登录了」）。用户只是想点个链接进频道，结果被自己踢了。
//!
//! 所以：启动时先看看是不是已经有一个在跑（一个有名字的互斥量）。有的话，把这次的
//! 参数（可能是一条邀请链接）通过命名管道交给它，自己退出；那边把窗口叫到前台、
//! 按链接进频道。转交失败（比如那边是不认识这个的旧版本）就照常启动 —— 不会比
//! 原来更糟。
//!
//! 名字里带着身份所在目录的哈希：不同的 %APPDATA%（测试时常这么干，一台机器上开
//! 好几个身份）互不干扰。

#[cfg(windows)]
mod imp {
    use std::io;
    use windows_sys::Win32::Foundation::{
        CloseHandle, GetLastError, ERROR_ALREADY_EXISTS, ERROR_PIPE_BUSY, ERROR_PIPE_CONNECTED,
        GENERIC_WRITE, HANDLE, INVALID_HANDLE_VALUE,
    };
    use windows_sys::Win32::Storage::FileSystem::{
        CreateFileW, FlushFileBuffers, ReadFile, WriteFile, OPEN_EXISTING, PIPE_ACCESS_INBOUND,
    };
    use windows_sys::Win32::System::Pipes::{
        ConnectNamedPipe, CreateNamedPipeW, DisconnectNamedPipe, WaitNamedPipeW,
        PIPE_READMODE_BYTE, PIPE_REJECT_REMOTE_CLIENTS, PIPE_TYPE_BYTE, PIPE_UNLIMITED_INSTANCES,
        PIPE_WAIT,
    };
    use windows_sys::Win32::System::Threading::CreateMutexW;
    use windows_sys::Win32::UI::WindowsAndMessaging::AllowSetForegroundWindow;

    /// 跟 packaging/windows/gouhuo.iss 里的 AppMutex 一字不差。
    const RUNNING_MUTEX: &str = "GouhuoClientRunning";

    /// 一条消息最长多少字节。邀请链接几十个字符，给足余量，但别让人往里灌。
    const MAX_MESSAGE: usize = 4096;

    /// 管道忙的时候最多等多久。那边建下一个实例是微秒级的事，等满了说明那边卡死了。
    const PIPE_BUSY_WAIT_MS: u32 = 2000;

    /// 占着「我是第一个」这个名额。进程活着就一直占着。
    pub struct Guard(#[allow(dead_code)] HANDLE, #[allow(dead_code)] HANDLE);

    unsafe impl Send for Guard {}

    fn wide(s: &str) -> Vec<u16> {
        s.encode_utf16().chain(Some(0)).collect()
    }

    fn names(key: &str) -> (String, String) {
        (
            format!("Local\\gouhuo-client-{key}"),
            format!("\\\\.\\pipe\\gouhuo-client-{key}"),
        )
    }

    /// 返回 (名额, 是不是已经有一个在跑)。
    pub fn claim(key: &str) -> (Guard, bool) {
        let (mutex, _) = names(key);
        let name = wide(&mutex);
        // SAFETY: 名字是以 0 结尾的 UTF-16，活到调用结束。
        let handle = unsafe { CreateMutexW(std::ptr::null(), 0, name.as_ptr()) };
        let existed = unsafe { GetLastError() } == ERROR_ALREADY_EXISTS;
        // 再挂一个不带哈希的：安装程序（AppMutex）靠它知道篝火还开着，
        // 先让用户关掉再覆盖 exe。它算不出身份目录的哈希，所以名字得是固定的。
        let running = wide(RUNNING_MUTEX);
        // SAFETY: 同上。
        let marker = unsafe { CreateMutexW(std::ptr::null(), 0, running.as_ptr()) };
        (Guard(handle, marker), existed)
    }

    /// 把消息交给已经在跑的那个。成功返回 `true`。
    pub fn forward(key: &str, message: &str) -> bool {
        let (_, pipe) = names(key);
        let name = wide(&pipe);
        // 这边是用户刚点的（链接或者图标），有权把窗口拉到前台；那边是后台进程，
        // 没这个权。先把权限让出去，那边的 SetForegroundWindow 才管用。
        // SAFETY: 纯粹的权限设置，参数是常量。
        unsafe { AllowSetForegroundWindow(u32::MAX) }; // ASFW_ANY
        let handle = loop {
            // SAFETY: 名字以 0 结尾；其余参数是常量或空指针。
            let handle = unsafe {
                CreateFileW(
                    name.as_ptr(),
                    GENERIC_WRITE,
                    0,
                    std::ptr::null(),
                    OPEN_EXISTING,
                    0,
                    std::ptr::null_mut(),
                )
            };
            if handle != INVALID_HANDLE_VALUE {
                break handle;
            }
            // 忙 = 那边在跑，只是空着的实例刚被别人接走、下一个还没建好。等它，
            // 别当成没人在跑。别的错误（多半是管道不存在）才是真没人。
            // SAFETY: 名字以 0 结尾。
            if unsafe { GetLastError() } != ERROR_PIPE_BUSY
                || unsafe { WaitNamedPipeW(name.as_ptr(), PIPE_BUSY_WAIT_MS) } == 0
            {
                return false;
            }
        };
        let bytes = message.as_bytes();
        let mut written = 0u32;
        // SAFETY: handle 刚打开，缓冲区活到调用结束。
        let ok = unsafe {
            WriteFile(
                handle,
                bytes.as_ptr(),
                bytes.len() as u32,
                &mut written,
                std::ptr::null_mut(),
            ) != 0
                && FlushFileBuffers(handle) != 0
        };
        unsafe { CloseHandle(handle) };
        ok && written as usize == bytes.len()
    }

    fn create_pipe(name: &[u16]) -> HANDLE {
        // SAFETY: 名字以 0 结尾；只收本机的连接。
        unsafe {
            CreateNamedPipeW(
                name.as_ptr(),
                PIPE_ACCESS_INBOUND,
                PIPE_TYPE_BYTE | PIPE_READMODE_BYTE | PIPE_WAIT | PIPE_REJECT_REMOTE_CLIENTS,
                PIPE_UNLIMITED_INSTANCES,
                0,
                MAX_MESSAGE as u32,
                0,
                std::ptr::null(),
            )
        }
    }

    /// 起一个线程收别的实例转交过来的消息，每收到一条调一次 `on_message`。
    ///
    /// **一接上就先把下一个实例建好，再读这一个。** 要是读完、关掉、回到循环开头
    /// 才建，中间那一小段管道根本不存在，这时候来的 `forward` 打不开就返回失败 ——
    /// 连着点两条链接，第二个进程以为没人在跑，照常启动，把第一个顶下线。
    pub fn serve(key: &str, on_message: impl Fn(String) + Send + 'static) -> io::Result<()> {
        let (_, pipe) = names(key);
        let name = wide(&pipe);
        let first = create_pipe(&name);
        if first == INVALID_HANDLE_VALUE {
            return Err(io::Error::last_os_error());
        }
        // HANDLE 是裸指针，不能直接搬进线程。
        let first = first as usize;
        std::thread::Builder::new()
            .name("gouhuo-single-instance".into())
            .spawn(move || {
                let mut handle = first as HANDLE;
                loop {
                    // SAFETY: handle 是建好还没连过的管道；阻塞等一个客户端连上来。
                    let connected = unsafe { ConnectNamedPipe(handle, std::ptr::null_mut()) } != 0
                        || unsafe { GetLastError() } == ERROR_PIPE_CONNECTED;
                    let next = create_pipe(&name);
                    if connected {
                        if let Some(message) = read_all(handle) {
                            on_message(message);
                        }
                    }
                    unsafe {
                        DisconnectNamedPipe(handle);
                        CloseHandle(handle);
                    }
                    if next == INVALID_HANDLE_VALUE {
                        return;
                    }
                    handle = next;
                }
            })?;
        Ok(())
    }

    fn read_all(handle: HANDLE) -> Option<String> {
        let mut out = Vec::new();
        let mut buf = [0u8; 512];
        loop {
            let mut read = 0u32;
            // SAFETY: handle 是已连上的管道，buf 活到调用结束。
            let ok = unsafe {
                ReadFile(
                    handle,
                    buf.as_mut_ptr(),
                    buf.len() as u32,
                    &mut read,
                    std::ptr::null_mut(),
                )
            } != 0;
            if read > 0 {
                out.extend_from_slice(&buf[..read as usize]);
                if out.len() > MAX_MESSAGE {
                    return None;
                }
            }
            if !ok || read == 0 {
                break;
            }
        }
        String::from_utf8(out).ok()
    }
}

#[cfg(not(windows))]
mod imp {
    use std::io;
    pub struct Guard;
    pub fn claim(_key: &str) -> (Guard, bool) {
        (Guard, false)
    }
    pub fn forward(_key: &str, _message: &str) -> bool {
        false
    }
    pub fn serve(_key: &str, _on_message: impl Fn(String) + Send + 'static) -> io::Result<()> {
        Ok(())
    }
}

pub use imp::{claim, forward, serve};

/// 这一份身份的钥匙：身份文件所在目录的哈希（FNV-1a）。拿不到目录就用一个固定值。
pub fn key() -> String {
    let dir = voice_core::identity::Identity::default_path()
        .ok()
        .and_then(|p| p.parent().map(|d| d.to_string_lossy().to_lowercase()))
        .unwrap_or_default();
    let mut hash: u64 = 0xcbf2_9ce4_8422_2325;
    for byte in dir.bytes() {
        hash ^= byte as u64;
        hash = hash.wrapping_mul(0x0000_0100_0000_01b3);
    }
    format!("{hash:016x}")
}

#[cfg(all(test, windows))]
mod tests {
    use super::*;
    use std::sync::mpsc;
    use std::time::Duration;

    #[test]
    fn a_second_instance_hands_its_link_to_the_first() {
        let key = format!("test-{}", std::process::id());
        let (_guard, existed) = claim(&key);
        assert!(!existed, "第一个就说已经有人在跑了");
        let (_again, existed) = claim(&key);
        assert!(existed, "第二个没发现第一个");

        let (tx, rx) = mpsc::channel();
        serve(&key, move |message| {
            let _ = tx.send(message);
        })
        .unwrap();
        // serve 返回时管道已经建好了，不用等
        assert!(forward(&key, "gouhuo://j/abc"), "转交失败");
        assert_eq!(
            rx.recv_timeout(Duration::from_secs(2)).unwrap(),
            "gouhuo://j/abc"
        );

        // 空消息（没带链接，只是又点了一下图标）也要送到：那边要把窗口叫出来
        assert!(forward(&key, ""));
        assert_eq!(rx.recv_timeout(Duration::from_secs(2)).unwrap(), "");
    }

    #[test]
    fn links_clicked_back_to_back_all_get_through() {
        let key = format!("burst-{}", std::process::id());
        let (tx, rx) = mpsc::channel();
        serve(&key, move |message| {
            let _ = tx.send(message);
        })
        .unwrap();
        // 一条都不等那边处理完：读完一条到建好下一个管道之间不能有空档
        for i in 0..20 {
            assert!(
                forward(&key, &format!("gouhuo://j/{i}")),
                "第 {i} 条没转交出去"
            );
        }
        for _ in 0..20 {
            rx.recv_timeout(Duration::from_secs(2)).unwrap();
        }
    }

    #[test]
    fn nobody_listening_means_start_normally() {
        let key = format!("nobody-{}", std::process::id());
        assert!(!forward(&key, "gouhuo://j/abc"));
    }

    #[test]
    fn different_profiles_get_different_keys() {
        // key() 本身读的是真实的 %APPDATA%，这里只验它稳定、像个十六进制串
        let a = key();
        assert_eq!(a, key());
        assert_eq!(a.len(), 16);
    }
}
