// SPDX-License-Identifier: GPL-3.0-or-later

//! 去加入页那儿取「这个域名上的篝火在哪儿」。
//!
//! 取的是 `https://<域名>/.well-known/gouhuo`，格式见 `protocol::Discovery`。
//! 拿到的地址和证书指纹之所以可信，**全靠这一次 HTTPS 请求验过了域名的证书** ——
//! 所以这里走系统自带的 WinHTTP：证书库、吊销检查、代理设置都跟着系统走，
//! 不多带一套 TLS 和根证书（跟检查更新是一个做法，见 `update.rs`）。
//!
//! 请求里什么都不带：一个不带参数的 GET。用户的加入码（`#code=…`）在认地址的
//! 时候就已经摘出去了，到不了这里。

use client_core::address::JoinPage;
use protocol::{Discovery, DiscoveryError, DISCOVERY_PATH};

/// 说明最长多少字节。正常的只有一百多字节；再长就不是我们的东西了，别往内存里灌。
const MAX_BODY: usize = 8 * 1024;

/// 解析、连接、发、收各等多久（毫秒）。用户正盯着「正在查找」这几个字。
const TIMEOUT_MS: i32 = 5000;

#[derive(Debug)]
pub enum FetchError {
    /// 连不上、证书不对、超时 —— 系统没告诉我们更多。
    Unreachable,
    /// 连上了，但那个路径上没有东西（不是 200）。
    NoPage(u32),
    /// 有东西，但不是篝火的说明。
    Invalid(DiscoveryError),
}

/// 取加入页上的说明。**会阻塞**，别在界面线程上调。
pub fn fetch(page: &JoinPage) -> Result<Discovery, FetchError> {
    let body = get(page.secure, &page.host, page.port, DISCOVERY_PATH)?;
    let text = String::from_utf8_lossy(&body);
    Discovery::parse(&text).map_err(FetchError::Invalid)
}

#[cfg(windows)]
fn get(secure: bool, host: &str, port: u16, path: &str) -> Result<Vec<u8>, FetchError> {
    use windows_sys::Win32::Networking::WinHttp::*;

    struct Handle(*mut core::ffi::c_void);
    impl Drop for Handle {
        fn drop(&mut self) {
            // SAFETY: 只包 WinHttp* 返回的非空句柄，只关一次。
            unsafe { WinHttpCloseHandle(self.0) };
        }
    }
    fn handle(raw: *mut core::ffi::c_void) -> Result<Handle, FetchError> {
        if raw.is_null() {
            Err(FetchError::Unreachable)
        } else {
            Ok(Handle(raw))
        }
    }
    let wide = |s: &str| s.encode_utf16().chain(Some(0)).collect::<Vec<u16>>();

    let agent = wide(&format!("gouhuo/{}", env!("CARGO_PKG_VERSION")));
    // IPv6 字面量要带方括号，WinHTTP 才认它是地址。
    let host = wide(&client_core::address::bracketed(host));
    let verb = wide("GET");
    let path = wide(path);

    // SAFETY: 下面每个调用传的字符串都是以 0 结尾的 UTF-16，活到调用结束；
    // 句柄都由 Handle 管着，按相反顺序关掉；读数据时给的缓冲区和长度对得上。
    unsafe {
        let session = handle(WinHttpOpen(
            agent.as_ptr(),
            WINHTTP_ACCESS_TYPE_AUTOMATIC_PROXY,
            std::ptr::null(),
            std::ptr::null(),
            0,
        ))?;
        WinHttpSetTimeouts(session.0, TIMEOUT_MS, TIMEOUT_MS, TIMEOUT_MS, TIMEOUT_MS);
        let connection = handle(WinHttpConnect(session.0, host.as_ptr(), port, 0))?;
        let request = handle(WinHttpOpenRequest(
            connection.0,
            verb.as_ptr(),
            path.as_ptr(),
            std::ptr::null(),
            std::ptr::null(),
            std::ptr::null(),
            if secure { WINHTTP_FLAG_SECURE } else { 0 },
        ))?;
        // 跳转不跟：说明就该在用户输的那个域名上。跟着跳的话，担保这份说明的
        // 就变成了跳转目标的证书，而用户从没见过那个域名。
        let disable: u32 = WINHTTP_DISABLE_REDIRECTS;
        WinHttpSetOption(
            request.0,
            WINHTTP_OPTION_DISABLE_FEATURE,
            (&disable as *const u32).cast(),
            std::mem::size_of::<u32>() as u32,
        );
        if WinHttpSendRequest(request.0, std::ptr::null(), 0, std::ptr::null(), 0, 0, 0) == 0
            || WinHttpReceiveResponse(request.0, std::ptr::null_mut()) == 0
        {
            return Err(FetchError::Unreachable);
        }

        let mut status: u32 = 0;
        let mut len = std::mem::size_of::<u32>() as u32;
        if WinHttpQueryHeaders(
            request.0,
            WINHTTP_QUERY_STATUS_CODE | WINHTTP_QUERY_FLAG_NUMBER,
            std::ptr::null(),
            (&mut status as *mut u32).cast(),
            &mut len,
            std::ptr::null_mut(),
        ) == 0
        {
            return Err(FetchError::Unreachable);
        }
        if status != 200 {
            return Err(FetchError::NoPage(status));
        }

        let mut body = Vec::new();
        let mut chunk = [0u8; 2048];
        loop {
            let mut read: u32 = 0;
            if WinHttpReadData(
                request.0,
                chunk.as_mut_ptr().cast(),
                chunk.len() as u32,
                &mut read,
            ) == 0
            {
                return Err(FetchError::Unreachable);
            }
            if read == 0 {
                break;
            }
            body.extend_from_slice(&chunk[..read as usize]);
            if body.len() > MAX_BODY {
                return Err(FetchError::Invalid(DiscoveryError::NotGouhuo));
            }
        }
        Ok(body)
    }
}

#[cfg(not(windows))]
fn get(_secure: bool, _host: &str, _port: u16, _path: &str) -> Result<Vec<u8>, FetchError> {
    Err(FetchError::Unreachable)
}
