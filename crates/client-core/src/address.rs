// SPDX-License-Identifier: MPL-2.0

//! 用户往「加入」那个框里输的东西，到底是什么。
//!
//! 邀请链接很长，是因为它把地址、端口、证书指纹全带着。自己架服务器的人通常
//! 有个域名或者固定 IP，让朋友输 `voice.example.com` 比传一长串链接顺手得多。
//! 所以同一个框要认四样东西：
//!
//! | 输入 | 是什么 | 指纹从哪来 |
//! |---|---|---|
//! | `gouhuo://j/…`（或光秃秃的那串码） | 邀请链接 | 链接自己带着 |
//! | `https://voice.example.com/` | HTTPS 加入页 | 加入页说的，CA 证书担保 |
//! | `voice.example.com` | 域名 | 先找加入页；没有就当裸地址 |
//! | `203.0.113.7`、`host:20800` | 裸地址 | 没人担保，取回来让用户核对 |
//!
//! 这个模块**只认输入、不碰网络**。认出来之后怎么连，是上层的事。
//!
//! # 加入码
//!
//! 服务端日志里打的「浏览器邀请」形如 `https://voice.example.com/#code=…`。
//! 加入码放在 `#` 后面，浏览器不会把它发给网页服务器；这里同样只把它从字符串里
//! 取出来带在结果上，取加入页的时候不会把它拼进请求。

use std::net::IpAddr;

use protocol::text::strip_prefix_ignore_ascii_case;
use protocol::{Invite, InviteError};

/// 认出来的目标。
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Target {
    /// 邀请链接：连上去需要的一切都在里面。
    Invite(Invite),
    /// HTTPS 加入页。去它那儿取地址和指纹。
    Page(JoinPage),
    /// 域名或 IP，可能带端口。
    Host(HostAddress),
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct JoinPage {
    /// 走 HTTPS。`false` 只在调用方明确允许、而且是本机回环地址时才会出现（本地调试）。
    pub secure: bool,
    pub host: String,
    /// 网页的端口，不是语音的端口。
    pub port: u16,
    /// `#code=…` 里的加入码。
    pub code: Option<String>,
}

impl JoinPage {
    /// 加入页的地址，不带加入码。存下来、显示出来的都是它。
    pub fn url(&self) -> String {
        let scheme = if self.secure { "https" } else { "http" };
        let default_port = if self.secure { 443 } else { 80 };
        let host = bracketed(&self.host);
        if self.port == default_port {
            format!("{scheme}://{host}/")
        } else {
            format!("{scheme}://{host}:{}/", self.port)
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct HostAddress {
    /// 域名（小写）或 IP 字面量。IPv6 不带方括号。
    pub host: String,
    /// 用户写了端口才有。没写的话，语音端口用默认值，而且域名可以先去找加入页。
    pub port: Option<u16>,
    pub code: Option<String>,
}

impl HostAddress {
    pub fn is_ip(&self) -> bool {
        self.host.parse::<IpAddr>().is_ok()
    }

    pub fn port_or_default(&self) -> u16 {
        self.port.unwrap_or(protocol::DEFAULT_PORT)
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum AddressError {
    Empty,
    /// 看着是邀请链接，但解析不了。
    Invite(InviteError),
    /// `http://` 开头。加入页必须走 HTTPS —— 不然谁都能在半路把指纹换掉。
    NotHttps,
    /// 网址里带了用户名密码之类的东西。
    Unsupported,
    /// 端口不是 1–65535 的数字。
    BadPort,
    /// 认不出来是什么。
    Unrecognized,
}

impl core::fmt::Display for AddressError {
    fn fmt(&self, f: &mut core::fmt::Formatter<'_>) -> core::fmt::Result {
        match self {
            AddressError::Empty => f.write_str("先填服务器地址或邀请链接"),
            AddressError::Invite(e) => write!(f, "{e}"),
            AddressError::NotHttps => {
                f.write_str("加入页地址要以 https:// 开头。只有域名或 IP 的话，直接填它就行")
            }
            AddressError::Unsupported => f.write_str("这种网址用不了。填域名、IP 或邀请链接"),
            AddressError::BadPort => f.write_str("端口不对，应该是 1 到 65535 之间的数字"),
            AddressError::Unrecognized => {
                f.write_str("认不出来。填域名、IP、https:// 加入页地址或 gouhuo:// 邀请链接")
            }
        }
    }
}

impl std::error::Error for AddressError {}

/// 认一下用户输的是什么。
///
/// `allow_loopback_http`：是否接受指向本机回环地址的 `http://` 加入页。只给本地
/// 调试用（本机起的服务端没有 CA 证书）；正式版传 `false`。
pub fn parse(text: &str, allow_loopback_http: bool) -> Result<Target, AddressError> {
    let text = text.trim();
    if text.is_empty() {
        return Err(AddressError::Empty);
    }
    if strip_prefix_ignore_ascii_case(text, "gouhuo://").is_some() {
        return Invite::parse(text)
            .map(Target::Invite)
            .map_err(AddressError::Invite);
    }
    for (scheme, secure) in [("https://", true), ("http://", false)] {
        if let Some(rest) = strip_prefix_ignore_ascii_case(text, scheme) {
            let (authority, code) = split_authority(rest)?;
            let (host, port) = split_host_port(authority)?;
            let allowed = secure || (allow_loopback_http && is_loopback(&host));
            if !allowed {
                return Err(AddressError::NotHttps);
            }
            return Ok(Target::Page(JoinPage {
                secure,
                host,
                port: port.unwrap_or(if secure { 443 } else { 80 }),
                code,
            }));
        }
    }
    if text.contains("://") {
        return Err(AddressError::Unsupported);
    }

    // 到这儿要么是裸地址，要么是没带前缀的那串邀请码。邀请码里没有点、没有冒号，
    // 地址里几乎一定有（域名有点，带端口有冒号，IPv6 全是冒号）。
    let looks_like_address =
        text.contains(['.', ':', '[', '/']) || text.eq_ignore_ascii_case("localhost");
    if !looks_like_address {
        return match Invite::parse(text) {
            Ok(invite) => Ok(Target::Invite(invite)),
            // 一串像模像样的码但校验不过，说清楚是码的问题；别的一律「认不出来」。
            Err(e @ (InviteError::BadChecksum | InviteError::UnsupportedVersion(_))) => {
                Err(AddressError::Invite(e))
            }
            Err(_) => Err(AddressError::Unrecognized),
        };
    }
    let (authority, code) = split_authority(text)?;
    let (host, port) = split_host_port(authority)?;
    Ok(Target::Host(HostAddress { host, port, code }))
}

/// 把 `host[:port]/路径?查询#片段` 切成前面那段和片段里的加入码。
fn split_authority(rest: &str) -> Result<(&str, Option<String>), AddressError> {
    let (before_fragment, fragment) = match rest.split_once('#') {
        Some((before, fragment)) => (before, Some(fragment)),
        None => (rest, None),
    };
    let authority = before_fragment
        .split(['/', '?'])
        .next()
        .unwrap_or(before_fragment);
    if authority.contains('@') {
        return Err(AddressError::Unsupported);
    }
    let code = fragment.and_then(|fragment| {
        fragment
            .split('&')
            .find_map(|pair| pair.strip_prefix("code="))
            .map(percent_decode)
            .filter(|code| !code.is_empty() && code.len() <= u8::MAX as usize)
    });
    Ok((authority, code))
}

fn split_host_port(authority: &str) -> Result<(String, Option<u16>), AddressError> {
    let parse_port = |text: &str| match text.parse::<u16>() {
        Ok(port) if port != 0 => Ok(port),
        _ => Err(AddressError::BadPort),
    };

    // [IPv6] 或 [IPv6]:端口
    if let Some(rest) = authority.strip_prefix('[') {
        let (inside, after) = rest.split_once(']').ok_or(AddressError::Unrecognized)?;
        let ip: std::net::Ipv6Addr = inside.parse().map_err(|_| AddressError::Unrecognized)?;
        let port = match after.strip_prefix(':') {
            Some(port) => Some(parse_port(port)?),
            None if after.is_empty() => None,
            None => return Err(AddressError::Unrecognized),
        };
        return Ok((ip.to_string(), port));
    }
    // 不带方括号的 IPv6：冒号不止一个，没法带端口。
    if authority.matches(':').count() > 1 {
        let ip: std::net::Ipv6Addr = authority.parse().map_err(|_| AddressError::Unrecognized)?;
        return Ok((ip.to_string(), None));
    }

    let (host, port) = match authority.split_once(':') {
        Some((host, port)) => (host, Some(parse_port(port)?)),
        None => (authority, None),
    };
    if !is_hostname(host) {
        return Err(AddressError::Unrecognized);
    }
    Ok((host.to_ascii_lowercase(), port))
}

/// 域名或 IPv4：一段一段的字母数字和连字符，用点隔开。
///
/// 故意收得很紧 —— 这个字符串之后会被拿去解析域名、拼进网址，
/// 中文、空格、引号之类的一概不放过去。
fn is_hostname(host: &str) -> bool {
    !host.is_empty()
        && host.len() <= 253
        && host.split('.').all(|label| {
            !label.is_empty()
                && label.len() <= 63
                && !label.starts_with('-')
                && !label.ends_with('-')
                && label
                    .bytes()
                    .all(|b| b.is_ascii_alphanumeric() || b == b'-')
        })
}

fn is_loopback(host: &str) -> bool {
    host == "localhost" || host.parse::<IpAddr>().is_ok_and(|ip| ip.is_loopback())
}

/// IPv6 写进网址或者跟端口写在一起时要带方括号。
pub fn bracketed(host: &str) -> String {
    if host.contains(':') {
        format!("[{host}]")
    } else {
        host.to_string()
    }
}

/// 给人看的地址：默认端口不写。
pub fn display_address(host: &str, port: u16) -> String {
    if port == protocol::DEFAULT_PORT {
        host.to_string()
    } else {
        format!("{}:{port}", bracketed(host))
    }
}

/// `%E4%B8%AD` → `中`。解不出合法 UTF-8 的就原样留着。
fn percent_decode(text: &str) -> String {
    let bytes = text.as_bytes();
    let mut out = Vec::with_capacity(bytes.len());
    let mut i = 0;
    while i < bytes.len() {
        let hex = |b: u8| (b as char).to_digit(16).map(|d| d as u8);
        match (bytes[i], bytes.get(i + 1), bytes.get(i + 2)) {
            (b'%', Some(&hi), Some(&lo)) if hex(hi).is_some() && hex(lo).is_some() => {
                out.push(hex(hi).unwrap_or(0) << 4 | hex(lo).unwrap_or(0));
                i += 3;
            }
            (b, _, _) => {
                out.push(b);
                i += 1;
            }
        }
    }
    String::from_utf8(out).unwrap_or_else(|_| text.to_string())
}

#[cfg(test)]
mod tests {
    use super::*;
    use protocol::Fingerprint;

    fn invite() -> Invite {
        Invite {
            host: "voice.example.com".into(),
            port: 20800,
            cert: Fingerprint([0x5a; 16]),
            code: Some("winter".into()),
        }
    }

    fn host(text: &str) -> HostAddress {
        match parse(text, false) {
            Ok(Target::Host(host)) => host,
            other => panic!("{text:?} 该是个裸地址，实际是 {other:?}"),
        }
    }

    fn page(text: &str) -> JoinPage {
        match parse(text, false) {
            Ok(Target::Page(page)) => page,
            other => panic!("{text:?} 该是个加入页，实际是 {other:?}"),
        }
    }

    #[test]
    fn invite_links_still_work_with_or_without_the_prefix() {
        let url = invite().to_url().unwrap();
        assert_eq!(parse(&url, false), Ok(Target::Invite(invite())));
        assert_eq!(
            parse(&url.to_uppercase(), false),
            Ok(Target::Invite(invite()))
        );
        let bare = invite().to_code().unwrap();
        assert_eq!(parse(&bare, false), Ok(Target::Invite(invite())));
        assert_eq!(
            parse(&format!("  {bare}\n"), false),
            Ok(Target::Invite(invite()))
        );
    }

    #[test]
    fn a_damaged_invite_is_reported_as_such() {
        let mut url = invite().to_url().unwrap();
        url.pop();
        assert!(matches!(parse(&url, false), Err(AddressError::Invite(_))));
        assert!(matches!(
            parse("gouhuo://j/", false),
            Err(AddressError::Invite(InviteError::Empty))
        ));
    }

    #[test]
    fn domains_and_ips() {
        let h = host("voice.example.com");
        assert_eq!(
            (h.host.as_str(), h.port, h.is_ip()),
            ("voice.example.com", None, false)
        );
        assert_eq!(h.port_or_default(), 20800);

        let h = host("Voice.Example.COM:20900");
        assert_eq!(
            (h.host.as_str(), h.port),
            ("voice.example.com", Some(20900))
        );

        let h = host("203.0.113.7");
        assert!(h.is_ip());
        assert_eq!(host("203.0.113.7:1234").port, Some(1234));

        // 复制网址时带上的尾巴不碍事
        assert_eq!(host("voice.example.com/").host, "voice.example.com");
        assert_eq!(host("voice.example.com/join?x=1").host, "voice.example.com");
        assert_eq!(host("localhost").host, "localhost");
        assert_eq!(host("nas:20800").host, "nas");
    }

    #[test]
    fn ipv6_with_and_without_brackets() {
        let h = host("[2001:db8::1]:20800");
        assert_eq!((h.host.as_str(), h.port), ("2001:db8::1", Some(20800)));
        assert!(h.is_ip());
        assert_eq!(host("[::1]").port, None);
        assert_eq!(host("2001:db8::1").host, "2001:db8::1");
        assert_eq!(bracketed("2001:db8::1"), "[2001:db8::1]");
        assert_eq!(display_address("2001:db8::1", 9), "[2001:db8::1]:9");
        assert_eq!(display_address("2001:db8::1", 20800), "2001:db8::1");
    }

    #[test]
    fn https_join_pages() {
        let p = page("https://voice.example.com/");
        assert_eq!(
            (p.secure, p.host.as_str(), p.port, p.code.clone()),
            (true, "voice.example.com", 443, None)
        );
        assert_eq!(p.url(), "https://voice.example.com/");
        assert_eq!(
            page("HTTPS://Voice.Example.com:8443/join/").url(),
            "https://voice.example.com:8443/"
        );
    }

    /// 服务端日志里那条「浏览器邀请」直接粘进来就能用。
    #[test]
    fn the_join_code_rides_in_the_fragment_and_stays_out_of_the_url() {
        let p = page("https://voice.example.com/#code=a%2B%E4%B8%AD%20%26");
        assert_eq!(p.code.as_deref(), Some("a+中 &"));
        assert!(!p.url().contains("code"), "加入码不能出现在要请求的网址里");

        assert_eq!(
            host("voice.example.com/#code=winter").code.as_deref(),
            Some("winter")
        );
        assert_eq!(page("https://voice.example.com/#").code, None);
        assert_eq!(page("https://voice.example.com/#code=").code, None);
        // 查询参数里的不算：那会被发给网页服务器，不是我们的约定
        assert_eq!(page("https://voice.example.com/?code=x").code, None);
    }

    /// 明文 HTTP 的加入页不能信：半路上谁都能把指纹换成自己的。
    #[test]
    fn plain_http_is_refused_except_for_local_debugging() {
        assert_eq!(
            parse("http://voice.example.com/", false),
            Err(AddressError::NotHttps)
        );
        assert_eq!(
            parse("http://voice.example.com/", true),
            Err(AddressError::NotHttps)
        );
        assert_eq!(
            parse("http://127.0.0.1:20801/", false),
            Err(AddressError::NotHttps)
        );
        let Ok(Target::Page(p)) = parse("http://127.0.0.1:20801/#code=x", true) else {
            panic!("本地调试该放行");
        };
        assert_eq!(
            (p.secure, p.port, p.code.as_deref()),
            (false, 20801, Some("x"))
        );
        assert_eq!(p.url(), "http://127.0.0.1:20801/");
        assert!(matches!(
            parse("http://localhost/", true),
            Ok(Target::Page(_))
        ));
    }

    #[test]
    fn rejects_what_it_cannot_safely_use() {
        assert_eq!(parse("", false), Err(AddressError::Empty));
        assert_eq!(parse("   ", false), Err(AddressError::Empty));
        assert_eq!(
            parse("https://user:pw@voice.example.com/", false),
            Err(AddressError::Unsupported)
        );
        assert_eq!(
            parse("ftp://voice.example.com/", false),
            Err(AddressError::Unsupported)
        );
        assert_eq!(
            parse("voice.example.com:0", false),
            Err(AddressError::BadPort)
        );
        assert_eq!(
            parse("voice.example.com:99999", false),
            Err(AddressError::BadPort)
        );
        assert_eq!(
            parse("voice.example.com:abc", false),
            Err(AddressError::BadPort)
        );
        for text in [
            "你发我的那个码呢？",
            "🎮开黑",
            "voice..example.com",
            "-bad-.example.com",
            "voice example.com",
            "[not-ipv6]",
            "https://",
            "a\"b.com",
        ] {
            assert_eq!(
                parse(text, false),
                Err(AddressError::Unrecognized),
                "{text:?}"
            );
        }
    }

    /// 任意输入都不该 panic —— 这个框是用户随手粘东西的地方。
    #[test]
    fn never_panics_on_arbitrary_input() {
        let pieces = [
            "https://",
            "http://",
            "gouhuo://",
            "[",
            "]",
            ":",
            "#code=",
            "%",
            "%E4",
            "中",
            "a.b",
            "/",
            "?",
            "@",
            " ",
            "1",
            "::",
        ];
        let mut state = 0x243f_6a88u32;
        for _ in 0..4000 {
            let mut text = String::new();
            for _ in 0..6 {
                state = state.wrapping_mul(1_664_525).wrapping_add(1_013_904_223);
                text.push_str(pieces[(state >> 24) as usize % pieces.len()]);
            }
            let _ = parse(&text, true);
        }
    }

    #[test]
    fn every_error_reads_like_a_sentence() {
        for error in [
            AddressError::Empty,
            AddressError::Invite(InviteError::Truncated),
            AddressError::NotHttps,
            AddressError::Unsupported,
            AddressError::BadPort,
            AddressError::Unrecognized,
        ] {
            assert!(!error.to_string().is_empty());
        }
    }
}
