// SPDX-License-Identifier: GPL-3.0-or-later

//! 启动时看一眼有没有新版本。有就在界面上提一句，**不自动下载、不自动装**。
//!
//! 问的是 `github.com/<仓库>/releases/latest`：GitHub 会把它 302 到最新那个版本的
//! 页面（`…/releases/tag/v0.2.0`），只看跳转地址就知道版本号，不用读页面，也不用
//! API —— API 对没登录的请求按 IP 限流（每小时 60 次），网吧、校园网一个出口 IP
//! 后面几百个人，很快就撞上。
//!
//! 走系统自带的 WinHTTP：用系统的证书库、系统的代理设置，不多带一套 TLS 和根证书。
//!
//! 什么都不发：一个不带任何参数的 GET，服务器看到的只有 IP 和 User-Agent 里的版本号。
//! 设置里能关。连不上、超时、格式不对，一律当作没有新版本，不打扰用户。

/// 发布页。检查更新问的是它下面的 `/latest`；提示里的「去下载」打开的也是它。
pub const REPO: &str = "parz1/gouhuo";

/// 只给测试用：换一个仓库问（比如一个已经发过版本的），好在还没发版的时候验证这条路。
const REPO_OVERRIDE: &str = "GOUHUO_UPDATE_REPO";

/// 有新版本时要显示的东西。
pub struct Available {
    /// 比如 `0.2.0`（去掉了前面的 `v`）。
    pub version: String,
    /// 那个版本的发布页。
    pub url: String,
}

/// 在后台线程上检查，查到比自己新的版本就调一次 `found`。查不到什么都不做。
pub fn check_in_background(found: impl FnOnce(Available) + Send + 'static) {
    let repo = std::env::var(REPO_OVERRIDE).unwrap_or_else(|_| REPO.to_string());
    // 调试版不查：开发时每跑一次就去 GitHub 问一次没有意义。
    if cfg!(debug_assertions) && std::env::var_os(REPO_OVERRIDE).is_none() {
        return;
    }
    let _ = std::thread::Builder::new()
        .name("gouhuo-update-check".into())
        .spawn(move || {
            // 别跟启动抢：窗口画出来、自动连上服务器之后再说。
            std::thread::sleep(std::time::Duration::from_secs(5));
            let Some(tag) = latest_tag(&repo) else { return };
            if is_newer(&tag, env!("CARGO_PKG_VERSION")) {
                found(Available {
                    version: tag.trim_start_matches('v').to_string(),
                    url: format!("https://github.com/{repo}/releases/tag/{tag}"),
                });
            }
        });
}

/// `v0.2.0` 比 `0.1.3` 新吗？只比前三段数字；认不出来的一律当「不新」。
fn is_newer(tag: &str, current: &str) -> bool {
    match (triple(tag), triple(current)) {
        (Some(latest), Some(current)) => latest > current,
        _ => false,
    }
}

fn triple(version: &str) -> Option<(u64, u64, u64)> {
    let version = version.trim().trim_start_matches(['v', 'V']);
    // 预发布版（0.2.0-rc.1）不提示：/latest 本来也不会指向它，这里再保险一次。
    if version.contains('-') {
        return None;
    }
    let version = version.split('+').next()?;
    let mut parts = version.split('.').map(|p| p.parse::<u64>().ok());
    let triple = (parts.next()??, parts.next()??, parts.next()??);
    if parts.next().is_some() {
        return None;
    }
    Some(triple)
}

/// 从 `https://github.com/<repo>/releases/tag/<tag>` 里取出 `<tag>`。
/// 仓库还没发过版本时 GitHub 会跳到 `…/releases`，那就是 `None`。
fn tag_from_location(location: &str, repo: &str) -> Option<String> {
    let prefix = format!("https://github.com/{repo}/releases/tag/");
    let tag = location.trim().strip_prefix(&prefix)?;
    // 版本号里只该有这些字符。别的一概不认，免得把奇怪的东西拼进要打开的网址里。
    let ok = !tag.is_empty()
        && tag.len() <= 64
        && tag
            .chars()
            .all(|c| c.is_ascii_alphanumeric() || matches!(c, '.' | '-' | '+' | '_'));
    ok.then(|| tag.to_string())
}

#[cfg(windows)]
fn latest_tag(repo: &str) -> Option<String> {
    use windows_sys::Win32::Networking::WinHttp::*;

    struct Handle(*mut core::ffi::c_void);
    impl Drop for Handle {
        fn drop(&mut self) {
            // SAFETY: 只包 WinHttp* 返回的非空句柄，只关一次。
            unsafe { WinHttpCloseHandle(self.0) };
        }
    }
    fn handle(raw: *mut core::ffi::c_void) -> Option<Handle> {
        (!raw.is_null()).then_some(Handle(raw))
    }
    let wide = |s: &str| s.encode_utf16().chain(Some(0)).collect::<Vec<u16>>();

    let agent = wide(&format!("gouhuo/{}", env!("CARGO_PKG_VERSION")));
    let host = wide("github.com");
    let verb = wide("GET");
    let path = wide(&format!("/{repo}/releases/latest"));

    // SAFETY: 下面每个调用传的字符串都是以 0 结尾的 UTF-16，活到调用结束；
    // 句柄都由 Handle 管着，按相反顺序关掉。
    unsafe {
        let session = handle(WinHttpOpen(
            agent.as_ptr(),
            WINHTTP_ACCESS_TYPE_AUTOMATIC_PROXY,
            std::ptr::null(),
            std::ptr::null(),
            0,
        ))?;
        // 解析、连接、发、收各 5 秒。国内连 GitHub 时好时坏，宁可这次不查。
        WinHttpSetTimeouts(session.0, 5000, 5000, 5000, 5000);
        let connection = handle(WinHttpConnect(
            session.0,
            host.as_ptr(),
            INTERNET_DEFAULT_HTTPS_PORT,
            0,
        ))?;
        let request = handle(WinHttpOpenRequest(
            connection.0,
            verb.as_ptr(),
            path.as_ptr(),
            std::ptr::null(),
            std::ptr::null(),
            std::ptr::null(),
            WINHTTP_FLAG_SECURE,
        ))?;
        // 要的就是跳转地址本身，别替我们跟过去。
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
            return None;
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
            || !(300..400).contains(&status)
        {
            return None;
        }

        let mut buf = [0u16; 1024];
        let mut len = std::mem::size_of_val(&buf) as u32;
        if WinHttpQueryHeaders(
            request.0,
            WINHTTP_QUERY_LOCATION,
            std::ptr::null(),
            buf.as_mut_ptr().cast(),
            &mut len,
            std::ptr::null_mut(),
        ) == 0
        {
            return None;
        }
        let location = String::from_utf16(&buf[..len as usize / 2]).ok()?;
        tag_from_location(&location, repo)
    }
}

#[cfg(not(windows))]
fn latest_tag(_repo: &str) -> Option<String> {
    None
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn compares_versions() {
        assert!(is_newer("v0.2.2", "0.2.1"));
        assert!(!is_newer("v0.2.2", "0.2.2"));
        assert!(is_newer("v0.2.0", "0.1.9"));
        assert!(is_newer("v0.1.10", "0.1.9"), "按数字比，不按字符串比");
        assert!(is_newer("1.0.0", "0.99.99"));
        assert!(!is_newer("v0.1.0", "0.1.0"), "一样的不算新");
        assert!(!is_newer("v0.0.9", "0.1.0"));
        assert!(!is_newer("v0.3.0-rc.1", "0.2.0"), "预发布不提示");
        assert!(!is_newer("nightly", "0.1.0"), "认不出来的不提示");
        assert!(!is_newer("v0.2", "0.1.0"), "少一段的不认");
    }

    /// 真去问 GitHub。要联网，所以默认不跑：`cargo test -p client -- --ignored`
    #[test]
    #[ignore]
    fn asks_github() {
        // 发过版本的仓库：拿得到版本号
        let tag = latest_tag("slint-ui/slint").expect("slint 应该有发布");
        assert!(triple(&tag).is_some(), "{tag}");
        // 不存在的仓库：GitHub 回 404，不是跳转
        assert_eq!(latest_tag("parz1/definitely-not-a-repo-7f3a"), None);
    }

    #[test]
    #[ignore = "需要联网访问实际发布仓库"]
    fn asks_our_release_repository() {
        let tag = latest_tag(REPO).expect("实际发布仓库应返回正式版本标签");
        assert!(triple(&tag).is_some(), "{tag}");
    }

    #[test]
    fn reads_the_tag_from_the_redirect() {
        let repo = "parz1/gouhuo";
        assert_eq!(
            tag_from_location("https://github.com/parz1/gouhuo/releases/tag/v0.2.0", repo)
                .as_deref(),
            Some("v0.2.0")
        );
        // 还没发过版本：跳到的是发布列表
        assert_eq!(
            tag_from_location("https://github.com/parz1/gouhuo/releases", repo),
            None
        );
        // 别的仓库、别的网站、奇怪的字符，都不认
        assert_eq!(
            tag_from_location("https://github.com/evil/gouhuo/releases/tag/v9.9.9", repo),
            None
        );
        assert_eq!(
            tag_from_location("https://example.com/parz1/gouhuo/releases/tag/v9.9.9", repo),
            None
        );
        assert_eq!(
            tag_from_location(
                "https://github.com/parz1/gouhuo/releases/tag/v1.0.0?x=\"a b\"",
                repo
            ),
            None
        );
    }
}
