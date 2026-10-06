// SPDX-License-Identifier: GPL-3.0-or-later

//! 官方 HTTPS 更新清单优先，失败时回退 GitHub 的正式 Release。
//! 仍只提醒和打开浏览器：不在客户端内下载或执行安装包。
//! Windows 网络请求使用系统 WinHTTP，限制响应大小并拒绝重定向。

/// GitHub 回退源。官方更新清单无法读取时检查它下面的 `/latest`。
pub const REPO: &str = "parz1/gouhuo";

/// 只给测试用：换一个仓库问（比如一个已经发过版本的），好在还没发版的时候验证这条路。
const REPO_OVERRIDE: &str = "GOUHUO_UPDATE_REPO";

/// 有新版本时要显示的东西。
pub struct Available {
    /// 比如 `0.2.0`（去掉了前面的 `v`）。
    pub version: String,
    /// 那个版本的发布页。
    pub url: String,
    pub download_url: String,
    pub notes: String,
    pub source: &'static str,
}

pub enum CheckResult {
    Available(Available),
    Current,
    Failed,
}

/// 启动检查稍后执行；手动检查立即执行，调试构建也可使用。
pub fn check_in_background(manual: bool, done: impl FnOnce(CheckResult) + Send + 'static) {
    let repo = std::env::var(REPO_OVERRIDE).unwrap_or_else(|_| REPO.to_string());
    // 调试版不查：开发时每跑一次就去 GitHub 问一次没有意义。
    if !manual && !automatic_check_enabled() {
        return;
    }
    // 保留回调，以便线程创建失败时也能结束界面上的检查状态。
    let done = std::sync::Arc::new(std::sync::Mutex::new(Some(done)));
    let worker_done = done.clone();
    let spawned = std::thread::Builder::new()
        .name("gouhuo-update-check".into())
        .spawn(move || {
            // 别跟启动抢：窗口画出来、自动连上服务器之后再说。
            if !manual {
                std::thread::sleep(std::time::Duration::from_secs(5));
            }
            let result = check_sources(
                update_base_url().as_deref(),
                &repo,
                env!("CARGO_PKG_VERSION"),
                fetch_manifest,
                latest_tag,
            );
            if let Some(done) = worker_done.lock().expect("update callback poisoned").take() {
                done(result);
            }
        });
    if spawned.is_err() {
        if let Some(done) = done.lock().expect("update callback poisoned").take() {
            done(CheckResult::Failed);
        }
    }
}

fn classify_latest(tag: Option<String>, current: &str, repo: &str) -> CheckResult {
    match tag {
        Some(tag) if triple(&tag).is_some() => {
            if is_newer(&tag, current) {
                CheckResult::Available(Available {
                    version: tag.trim_start_matches(['v', 'V']).to_string(),
                    url: format!("https://github.com/{repo}/releases/tag/{tag}"),
                    download_url: String::new(),
                    notes: "请打开发布页查看本次更新说明。".into(),
                    source: "GitHub",
                })
            } else {
                CheckResult::Current
            }
        }
        _ => CheckResult::Failed,
    }
}

const MAX_MANIFEST_BYTES: usize = 64 * 1024;
const DEFAULT_UPDATE_BASE_URL: &str = "https://downloads.gouhuo.minerei.dev";

fn update_base_url() -> Option<String> {
    std::env::var("GOUHUO_UPDATE_BASE_URL")
        .ok()
        .or_else(|| option_env!("GOUHUO_UPDATE_BASE_URL").map(str::to_owned))
        .or_else(|| Some(DEFAULT_UPDATE_BASE_URL.to_owned()))
        .filter(|value| !value.trim().is_empty())
}

pub fn automatic_check_enabled() -> bool {
    !cfg!(debug_assertions)
        || std::env::var_os(REPO_OVERRIDE).is_some()
        || std::env::var_os("GOUHUO_UPDATE_BASE_URL").is_some()
}

fn base_url(value: &str) -> Option<url::Url> {
    let parsed = url::Url::parse(value.trim()).ok()?;
    (parsed.scheme() == "https"
        && parsed.host_str().is_some()
        && parsed.username().is_empty()
        && parsed.password().is_none()
        && parsed.query().is_none()
        && parsed.fragment().is_none()
        && parsed.path() == "/")
        .then_some(parsed)
}

fn same_origin_url(value: &str, base: &url::Url) -> Option<String> {
    let parsed = url::Url::parse(value).ok()?;
    (parsed.origin() == base.origin()
        && parsed.username().is_empty()
        && parsed.password().is_none()
        && parsed.query().is_none()
        && parsed.fragment().is_none()
        && value.len() <= 2048)
        .then(|| parsed.to_string())
}

fn parse_manifest(body: &[u8], base: &url::Url, current: &str) -> Option<CheckResult> {
    if body.len() > MAX_MANIFEST_BYTES {
        return None;
    }
    let value: serde_json::Value = serde_json::from_slice(body).ok()?;
    if value["schema"].as_u64()? != 1 {
        return None;
    }
    let version = value["version"].as_str()?;
    if version.len() > 64 {
        return None;
    }
    triple(version)?;
    let file = &value["files"]["windows-x64"];
    let download_url = same_origin_url(file["url"].as_str()?, base)?;
    let hash = file["sha256"].as_str()?;
    if hash.len() != 64
        || !hash.bytes().all(|b| b.is_ascii_hexdigit())
        || !(1..=512 * 1024 * 1024).contains(&file["size"].as_u64()?)
    {
        return None;
    }
    let page = same_origin_url(value["release_url"].as_str()?, base)?;
    let notes = value["notes"].as_str()?;
    if notes.len() > 16 * 1024 {
        return None;
    }
    Some(if is_newer(version, current) {
        CheckResult::Available(Available {
            version: version.trim_start_matches(['v', 'V']).into(),
            url: page,
            download_url,
            notes: notes.into(),
            source: "官方更新源",
        })
    } else {
        CheckResult::Current
    })
}

// Only a failed/invalid manifest falls back. A valid current manifest is authoritative.
fn check_sources(
    base: Option<&str>,
    repo: &str,
    current: &str,
    fetch: impl FnOnce(&str) -> Option<Vec<u8>>,
    github: impl FnOnce(&str) -> Option<String>,
) -> CheckResult {
    if let Some(base) = base.and_then(base_url) {
        let endpoint = base.join("stable.json").expect("fixed relative path");
        if let Some(result) =
            fetch(endpoint.as_str()).and_then(|body| parse_manifest(&body, &base, current))
        {
            return result;
        }
    }
    classify_latest(github(repo), current, repo)
}

#[cfg(windows)]
fn fetch_manifest(endpoint: &str) -> Option<Vec<u8>> {
    use windows_sys::Win32::Networking::WinHttp::*;
    let parsed = url::Url::parse(endpoint).ok()?;
    struct Handle(*mut core::ffi::c_void);
    impl Drop for Handle {
        fn drop(&mut self) {
            unsafe { WinHttpCloseHandle(self.0) };
        }
    }
    fn handle(raw: *mut core::ffi::c_void) -> Option<Handle> {
        (!raw.is_null()).then_some(Handle(raw))
    }
    let wide = |s: &str| s.encode_utf16().chain(Some(0)).collect::<Vec<u16>>();
    let host = wide(parsed.host_str()?);
    let path = wide(parsed.path());
    let agent = wide(&format!("gouhuo/{}", env!("CARGO_PKG_VERSION")));
    let verb = wide("GET");
    // SAFETY: NUL-terminated UTF-16 stays alive for each call; handles close exactly once.
    unsafe {
        let session = handle(WinHttpOpen(
            agent.as_ptr(),
            WINHTTP_ACCESS_TYPE_AUTOMATIC_PROXY,
            std::ptr::null(),
            std::ptr::null(),
            0,
        ))?;
        if WinHttpSetTimeouts(session.0, 3000, 3000, 3000, 3000) == 0 {
            return None;
        }
        let connection = handle(WinHttpConnect(
            session.0,
            host.as_ptr(),
            parsed.port_or_known_default()?,
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
        let disable: u32 = WINHTTP_DISABLE_REDIRECTS;
        if WinHttpSetOption(
            request.0,
            WINHTTP_OPTION_DISABLE_FEATURE,
            (&disable as *const u32).cast(),
            std::mem::size_of::<u32>() as u32,
        ) == 0
        {
            return None;
        }
        let headers = wide("Cache-Control: no-cache\r\n");
        if WinHttpSendRequest(
            request.0,
            headers.as_ptr(),
            u32::MAX,
            std::ptr::null(),
            0,
            0,
            0,
        ) == 0
            || WinHttpReceiveResponse(request.0, std::ptr::null_mut()) == 0
        {
            return None;
        }
        let mut status = 0u32;
        let mut len = std::mem::size_of::<u32>() as u32;
        if WinHttpQueryHeaders(
            request.0,
            WINHTTP_QUERY_STATUS_CODE | WINHTTP_QUERY_FLAG_NUMBER,
            std::ptr::null(),
            (&mut status as *mut u32).cast(),
            &mut len,
            std::ptr::null_mut(),
        ) == 0
            || status != 200
        {
            return None;
        }
        let started = std::time::Instant::now();
        let mut body = Vec::new();
        loop {
            let mut chunk = [0u8; 4096];
            let mut read = 0u32;
            if started.elapsed() > std::time::Duration::from_secs(10)
                || WinHttpReadData(
                    request.0,
                    chunk.as_mut_ptr().cast(),
                    chunk.len() as u32,
                    &mut read,
                ) == 0
            {
                return None;
            }
            if read == 0 {
                break;
            }
            if body.len() + read as usize > MAX_MANIFEST_BYTES {
                return None;
            }
            body.extend_from_slice(&chunk[..read as usize]);
        }
        Some(body)
    }
}

#[cfg(not(windows))]
fn fetch_manifest(_endpoint: &str) -> Option<Vec<u8>> {
    None
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

    fn manifest(version: &str) -> Vec<u8> {
        serde_json::to_vec(&serde_json::json!({
            "schema": 1, "version": version,
            "release_url": "https://downloads.gouhuo.minerei.dev/releases/0.4.0/index.html",
            "notes": "改善更新体验。",
            "files": {"windows-x64": {
                "url": "https://downloads.gouhuo.minerei.dev/releases/0.4.0/gouhuo-setup-0.4.0.exe",
                "sha256": "a".repeat(64), "size": 1024
            }}
        }))
        .unwrap()
    }

    #[test]
    fn manifest_provides_official_download_and_notes() {
        let base = base_url(DEFAULT_UPDATE_BASE_URL).unwrap();
        match parse_manifest(&manifest("0.4.0"), &base, "0.3.1").unwrap() {
            CheckResult::Available(release) => {
                assert_eq!(release.source, "官方更新源");
                assert_eq!(release.notes, "改善更新体验。");
                assert!(release.download_url.ends_with("gouhuo-setup-0.4.0.exe"));
            }
            _ => panic!("expected an update"),
        }
    }

    #[test]
    fn rejects_invalid_untrusted_or_oversized_manifests() {
        let base = base_url(DEFAULT_UPDATE_BASE_URL).unwrap();
        assert!(parse_manifest(&vec![b' '; MAX_MANIFEST_BYTES + 1], &base, "0.3.1").is_none());
        assert!(parse_manifest(b"<html>challenge</html>", &base, "0.3.1").is_none());
        for (field, bad) in [
            ("schema", serde_json::json!(2)),
            ("version", serde_json::json!("0.4.0-rc.1")),
            (
                "release_url",
                serde_json::json!("https://evil.example/install"),
            ),
            ("notes", serde_json::json!("a".repeat(16 * 1024 + 1))),
        ] {
            let mut value: serde_json::Value = serde_json::from_slice(&manifest("0.4.0")).unwrap();
            value[field] = bad;
            assert!(
                parse_manifest(&serde_json::to_vec(&value).unwrap(), &base, "0.3.1").is_none(),
                "{field}"
            );
        }
        for (field, bad) in [
            (
                "url",
                serde_json::json!("http://downloads.gouhuo.minerei.dev/install.exe"),
            ),
            ("url", serde_json::json!("https://evil.example/install.exe")),
            (
                "url",
                serde_json::json!("https://user:secret@downloads.gouhuo.minerei.dev/install.exe"),
            ),
            ("sha256", serde_json::json!("z".repeat(64))),
            ("size", serde_json::json!(0)),
        ] {
            let mut value: serde_json::Value = serde_json::from_slice(&manifest("0.4.0")).unwrap();
            value["files"]["windows-x64"][field] = bad;
            assert!(
                parse_manifest(&serde_json::to_vec(&value).unwrap(), &base, "0.3.1").is_none(),
                "{field}"
            );
        }
        for bad in [
            "http://example.com",
            "https://user:pass@example.com",
            "https://example.com/path",
            "https://example.com?key=value",
        ] {
            assert!(base_url(bad).is_none(), "{bad}");
        }
    }

    #[test]
    fn valid_manifest_is_authoritative_and_failure_falls_back() {
        let result = check_sources(
            Some(DEFAULT_UPDATE_BASE_URL),
            REPO,
            "0.4.0",
            |endpoint| {
                assert!(endpoint.ends_with("/stable.json"));
                Some(manifest("0.4.0"))
            },
            |_| panic!("valid current manifest must not consult GitHub"),
        );
        assert!(matches!(result, CheckResult::Current));
        let result = check_sources(
            Some(DEFAULT_UPDATE_BASE_URL),
            REPO,
            "0.3.1",
            |_| None,
            |_| Some("v0.4.0".into()),
        );
        assert!(matches!(result, CheckResult::Available(_)));
        let result = check_sources(
            Some(DEFAULT_UPDATE_BASE_URL),
            REPO,
            "0.3.1",
            |_| Some(b"bad json".to_vec()),
            |_| None,
        );
        assert!(matches!(result, CheckResult::Failed));
    }

    #[test]
    fn failed_checks_are_not_reported_as_current() {
        assert!(matches!(
            classify_latest(None, "0.3.1", REPO),
            CheckResult::Failed
        ));
        assert!(matches!(
            classify_latest(Some("nightly".into()), "0.3.1", REPO),
            CheckResult::Failed
        ));
        assert!(matches!(
            classify_latest(Some("v0.3.1".into()), "0.3.1", REPO),
            CheckResult::Current
        ));
        match classify_latest(Some("V0.4.0".into()), "0.3.1", REPO) {
            CheckResult::Available(available) => {
                assert_eq!(available.version, "0.4.0");
                assert_eq!(
                    available.url,
                    "https://github.com/parz1/gouhuo/releases/tag/V0.4.0"
                );
            }
            _ => panic!("new release must be available"),
        }
    }

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
