// SPDX-License-Identifier: MIT OR Apache-2.0

//! 邀请链接。**痛点 #1 的答案：加服务器不用手输 IP 和端口。**
//!
//! # 为什么必须是自包含的
//!
//! 常见的做法是发一个短链，客户端去某个中心服务换取真实地址。这个项目不能这么做 ——
//! 「可自部署」意味着不能有一个所有人都依赖的中心服务。所以邀请链接把
//! **连上去需要的一切**都编在自己身上：地址、端口、证书指纹、可选的邀请码。
//!
//! # 为什么必须带证书指纹
//!
//! 自部署的服务端没有 CA 签的证书，只能自签名。自签名 TLS 如果不固定证书，
//! 等于没有认证 —— 任何人都能中间人。所以邀请链接里必须带指纹，客户端连上去先比对。
//!
//! 这两件事合起来正好：**用户体验上的「一键加入」和安全上的「证书固定」是同一个动作。**
//! 用户粘贴一串东西进去，既省了手输 IP，也完成了信任建立。
//!
//! # 线上格式
//!
//! ```text
//! 偏移  长度  内容
//! 0     1     版本号（当前 1）
//! 1     1     标志位（bit0 = 带邀请码）
//! 2     16    服务端证书指纹
//! 18    2     端口（大端）
//! 20    1     主机名字节数
//! 21    n     主机名（UTF-8，域名或 IP 字面量）
//! ...   1+m   邀请码：1 字节长度 + 内容（仅当 bit0 置位）
//! 末尾  2     校验和 = 前面全部内容的 SHA-256 的前 2 字节
//! ```
//!
//! 整个负载用 Crockford base32 编码。典型长度 64 个字符左右。
//!
//! # 版本怎么演进
//!
//! 版本号不认识就**直接拒绝**，不尝试猜。邀请链接是建立信任的那一步，
//! 在这里「尽力而为地解析」是危险的 —— 宁可让用户看到「你的版本太旧，升级一下」，
//! 也不要猜错了地址或指纹还连上去。

use crate::base32;
use crate::identity::Fingerprint;
use crate::text::strip_prefix_ignore_ascii_case;

/// 当前版本。加字段就 +1；旧客户端会干脆地拒绝而不是猜。
pub const VERSION: u8 = 1;

const FLAG_HAS_CODE: u8 = 1 << 0;
const CHECKSUM_LEN: usize = 2;
/// 版本 + 标志 + 指纹 + 端口 + 主机名长度。
const HEADER_LEN: usize = 1 + 1 + Fingerprint::LEN + 2 + 1;

/// 链接形式的前缀。纯文本形式（不带前缀的那串 base32）也一样能解析。
pub const URL_PREFIX: &str = "gouhuo://j/";

/// 服务端默认监听的端口。用户只输了地址没写端口时，客户端连的就是它。
pub const DEFAULT_PORT: u16 = 20800;

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Invite {
    /// 域名或 IP 字面量。原样保存，不做规范化 —— 规范化是连接时的事。
    pub host: String,
    pub port: u16,
    /// 服务端 TLS 证书的指纹。连上去必须比对，对不上就断开。
    pub cert: Fingerprint,
    /// 可选的邀请码。服务端拿它做授权（次数、有效期、预设角色都由服务端决定）。
    ///
    /// `None` 表示这是一条「公开地址」——能连上，但能不能进去由服务端的策略说了算。
    pub code: Option<String>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum InviteError {
    /// 一个字符都没有。
    Empty,
    /// base32 里有不认识的字符。
    BadCharacter(char),
    /// 长度对不上，八成是粘贴时截断了。
    Truncated,
    /// 校验和不对：粘漏了、粘多了，或者中间被改过。
    BadChecksum,
    /// 版本号不认识 —— 这条链接是更新的客户端生成的。
    UnsupportedVersion(u8),
    /// 主机名不是合法 UTF-8，或者是空的。
    BadHost,
    /// 后面还有没解释的字节。宁可报错也不忽略。
    TrailingBytes,
    /// 主机名或邀请码超过 255 字节，编不出来。
    TooLong,
}

impl core::fmt::Display for InviteError {
    fn fmt(&self, f: &mut core::fmt::Formatter<'_>) -> core::fmt::Result {
        match self {
            InviteError::Empty => f.write_str("邀请码是空的"),
            InviteError::BadCharacter(c) => write!(f, "邀请码里有不认识的字符：{c:?}"),
            InviteError::Truncated => f.write_str("邀请码不完整，八成是复制的时候漏了一截"),
            InviteError::BadChecksum => f.write_str("邀请码校验不过，检查一下是不是复制全了"),
            InviteError::UnsupportedVersion(v) => {
                write!(
                    f,
                    "这条邀请码是版本 {v} 的，当前客户端只认到版本 {VERSION}，升级一下"
                )
            }
            InviteError::BadHost => f.write_str("邀请码里的服务器地址无效"),
            InviteError::TrailingBytes => f.write_str("邀请码末尾有多余内容"),
            InviteError::TooLong => f.write_str("服务器地址或邀请码太长"),
        }
    }
}

impl std::error::Error for InviteError {}

impl Invite {
    /// 编成可粘贴的纯文本（不带 `gouhuo://j/` 前缀）。
    pub fn to_code(&self) -> Result<String, InviteError> {
        Ok(base32::encode(&self.to_bytes()?))
    }

    /// 编成可点击的链接。
    pub fn to_url(&self) -> Result<String, InviteError> {
        Ok(format!("{URL_PREFIX}{}", self.to_code()?))
    }

    /// 两种形式都吃：`gouhuo://j/xxxx` 和光秃秃的 `xxxx`。
    ///
    /// 前后的空白、中间的连字符和换行都会被忽略 —— 从聊天记录里复制什么样的都有。
    pub fn parse(text: &str) -> Result<Self, InviteError> {
        let trimmed = text.trim();
        if trimmed.is_empty() {
            return Err(InviteError::Empty);
        }
        // 前缀大小写不敏感：有些聊天软件会把开头字母自动大写。
        // 用 strip_prefix_ignore_ascii_case 而不是自己切片 —— 直接按字节切
        // 碰上中文输入会 panic，见 protocol::text 的模块文档。
        let body = strip_prefix_ignore_ascii_case(trimmed, URL_PREFIX).unwrap_or(trimmed);
        if body.trim().is_empty() {
            return Err(InviteError::Empty);
        }

        let bytes = base32::decode(body).map_err(|e| InviteError::BadCharacter(e.0))?;
        Self::from_bytes(&bytes)
    }

    fn to_bytes(&self) -> Result<Vec<u8>, InviteError> {
        let host = self.host.as_bytes();
        if host.is_empty() {
            return Err(InviteError::BadHost);
        }
        if host.len() > u8::MAX as usize {
            return Err(InviteError::TooLong);
        }
        if let Some(code) = &self.code {
            if code.len() > u8::MAX as usize {
                return Err(InviteError::TooLong);
            }
        }

        let mut out = Vec::with_capacity(HEADER_LEN + host.len() + CHECKSUM_LEN);
        out.push(VERSION);
        out.push(if self.code.is_some() {
            FLAG_HAS_CODE
        } else {
            0
        });
        out.extend_from_slice(&self.cert.0);
        out.extend_from_slice(&self.port.to_be_bytes());
        out.push(host.len() as u8);
        out.extend_from_slice(host);
        if let Some(code) = &self.code {
            out.push(code.len() as u8);
            out.extend_from_slice(code.as_bytes());
        }
        out.extend_from_slice(&checksum(&out));
        Ok(out)
    }

    fn from_bytes(bytes: &[u8]) -> Result<Self, InviteError> {
        if bytes.len() < HEADER_LEN + CHECKSUM_LEN {
            return Err(InviteError::Truncated);
        }
        let (body, tail) = bytes.split_at(bytes.len() - CHECKSUM_LEN);
        if tail != checksum(body) {
            return Err(InviteError::BadChecksum);
        }

        // 版本先看，别的都往后放 —— 版本不对的话后面的布局根本不成立。
        let version = body[0];
        if version != VERSION {
            return Err(InviteError::UnsupportedVersion(version));
        }
        let flags = body[1];

        let mut cert = [0u8; Fingerprint::LEN];
        cert.copy_from_slice(&body[2..2 + Fingerprint::LEN]);
        let mut cursor = 2 + Fingerprint::LEN;

        let port = u16::from_be_bytes([body[cursor], body[cursor + 1]]);
        cursor += 2;

        let host_len = body[cursor] as usize;
        cursor += 1;
        if host_len == 0 {
            return Err(InviteError::BadHost);
        }
        let host_end = cursor.checked_add(host_len).ok_or(InviteError::Truncated)?;
        if host_end > body.len() {
            return Err(InviteError::Truncated);
        }
        let host = core::str::from_utf8(&body[cursor..host_end])
            .map_err(|_| InviteError::BadHost)?
            .to_string();
        cursor = host_end;

        let code = if flags & FLAG_HAS_CODE != 0 {
            if cursor >= body.len() {
                return Err(InviteError::Truncated);
            }
            let code_len = body[cursor] as usize;
            cursor += 1;
            let code_end = cursor.checked_add(code_len).ok_or(InviteError::Truncated)?;
            if code_end > body.len() {
                return Err(InviteError::Truncated);
            }
            let code = core::str::from_utf8(&body[cursor..code_end])
                .map_err(|_| InviteError::BadHost)?
                .to_string();
            cursor = code_end;
            Some(code)
        } else {
            None
        };

        // base32 解码会在末尾多出最多 4 个补位比特凑成的一个字节，所以允许多 1 字节。
        // 再多就是真的有没解释的内容了 —— 宁可报错，不要默默忽略。
        if body.len() > cursor + 1 {
            return Err(InviteError::TrailingBytes);
        }

        Ok(Invite {
            host,
            port,
            cert: Fingerprint(cert),
            code,
        })
    }
}

fn checksum(body: &[u8]) -> [u8; CHECKSUM_LEN] {
    use sha2::{Digest, Sha256};
    let digest = Sha256::digest(body);
    let mut out = [0u8; CHECKSUM_LEN];
    out.copy_from_slice(&digest[..CHECKSUM_LEN]);
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    fn sample() -> Invite {
        Invite {
            host: "voice.example.com".into(),
            port: 64738,
            cert: Fingerprint([0x5a; 16]),
            code: None,
        }
    }

    #[test]
    fn roundtrips_through_code_and_url() {
        let invite = sample();
        assert_eq!(Invite::parse(&invite.to_code().unwrap()).unwrap(), invite);
        assert_eq!(Invite::parse(&invite.to_url().unwrap()).unwrap(), invite);
    }

    #[test]
    fn roundtrips_with_invite_code() {
        let invite = Invite {
            code: Some("gonghui2026".into()),
            ..sample()
        };
        assert_eq!(Invite::parse(&invite.to_code().unwrap()).unwrap(), invite);
    }

    #[test]
    fn stays_short_enough_to_paste() {
        let url = sample().to_url().unwrap();
        // 目标：能在一行里发出去，不被聊天软件折行成两截
        assert!(url.len() < 110, "链接 {} 个字符，太长了：{url}", url.len());
        assert!(url.starts_with(URL_PREFIX));
    }

    #[test]
    fn survives_being_mangled_by_chat_apps() {
        let invite = sample();
        let code = invite.to_code().unwrap();
        for mangled in [
            format!("  {code}  "),                       // 前后空白
            format!("{}\n{}", &code[..10], &code[10..]), // 折行
            code.to_uppercase(),                         // 自动首字母大写之类
            format!("{}-{}", &code[..8], &code[8..]),    // 用户自己加分隔符
            format!("GOUHUO://J/{code}"),                // 前缀被大写
        ] {
            assert_eq!(
                Invite::parse(&mangled).unwrap(),
                invite,
                "没扛住：{mangled:?}"
            );
        }
    }

    #[test]
    fn truncation_is_caught() {
        let code = sample().to_code().unwrap();
        // 少一个字符一定要报错，不能解析出一个「看起来对」的地址
        let short = &code[..code.len() - 1];
        assert!(Invite::parse(short).is_err(), "截断了却解析成功：{short}");
    }

    /// 最关键的一条：指纹被改掉必须被发现。
    /// 这是中间人攻击最直接的形态 —— 改指纹，让客户端信任攻击者的证书。
    #[test]
    fn tampering_with_the_fingerprint_is_caught() {
        let invite = sample();
        let mut bytes = invite.to_bytes().unwrap();
        bytes[5] ^= 0x01; // 指纹中间的一个比特
        let tampered = base32::encode(&bytes);
        assert_eq!(Invite::parse(&tampered), Err(InviteError::BadChecksum));
    }

    #[test]
    fn tampering_with_the_host_is_caught() {
        let invite = sample();
        let mut bytes = invite.to_bytes().unwrap();
        let host_start = HEADER_LEN;
        bytes[host_start] ^= 0x01;
        assert_eq!(
            Invite::parse(&base32::encode(&bytes)),
            Err(InviteError::BadChecksum)
        );
    }

    #[test]
    fn future_versions_are_rejected_not_guessed() {
        let invite = sample();
        let mut bytes = invite.to_bytes().unwrap();
        bytes[0] = VERSION + 1;
        // 改了版本号校验和就不对了，所以重算 —— 模拟的是「未来版本真的生成了这么一条」
        let len = bytes.len();
        let fixed = checksum(&bytes[..len - CHECKSUM_LEN]);
        bytes[len - CHECKSUM_LEN..].copy_from_slice(&fixed);
        assert_eq!(
            Invite::parse(&base32::encode(&bytes)),
            Err(InviteError::UnsupportedVersion(VERSION + 1))
        );
    }

    /// 用户往邀请码框里粘中文是家常便饭 —— 不能崩。
    #[test]
    fn never_panics_on_pasted_chinese_or_emoji() {
        for text in [
            "随便一串东西",
            "你发我的那个码呢？",
            "🎮开黑",
            "gouhuo://j/中文",
        ] {
            assert!(
                Invite::parse(text).is_err(),
                "{text:?} 不该被当成有效邀请码"
            );
        }
    }

    #[test]
    fn rejects_empty_and_garbage() {
        assert_eq!(Invite::parse(""), Err(InviteError::Empty));
        assert_eq!(Invite::parse("   "), Err(InviteError::Empty));
        assert_eq!(Invite::parse(URL_PREFIX), Err(InviteError::Empty));
        assert!(matches!(
            Invite::parse("!!!"),
            Err(InviteError::BadCharacter('!'))
        ));
        assert_eq!(Invite::parse("abc"), Err(InviteError::Truncated));
    }

    #[test]
    fn rejects_empty_host_when_encoding() {
        let invite = Invite {
            host: String::new(),
            ..sample()
        };
        assert_eq!(invite.to_code(), Err(InviteError::BadHost));
    }

    #[test]
    fn rejects_overlong_fields() {
        let invite = Invite {
            host: "a".repeat(256),
            ..sample()
        };
        assert_eq!(invite.to_code(), Err(InviteError::TooLong));
        let invite = Invite {
            code: Some("x".repeat(256)),
            ..sample()
        };
        assert_eq!(invite.to_code(), Err(InviteError::TooLong));
    }

    #[test]
    fn handles_ip_literals_and_ipv6() {
        for host in ["1.2.3.4", "[2001:db8::1]", "localhost"] {
            let invite = Invite {
                host: host.into(),
                ..sample()
            };
            assert_eq!(
                Invite::parse(&invite.to_code().unwrap()).unwrap().host,
                host
            );
        }
    }

    /// 任意字节都不该让解析器 panic —— 这是解析不可信输入的最低要求。
    #[test]
    fn never_panics_on_arbitrary_input() {
        let mut state = 0x243f_6a88u32;
        for _ in 0..4000 {
            state = state.wrapping_mul(1_664_525).wrapping_add(1_013_904_223);
            let len = (state >> 24) as usize % 80;
            let bytes: Vec<u8> = (0..len)
                .map(|i| (state.wrapping_add(i as u32).wrapping_mul(2_654_435_761) >> 16) as u8)
                .collect();
            let _ = Invite::parse(&base32::encode(&bytes));
        }
    }
}
