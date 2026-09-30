// SPDX-License-Identifier: MIT OR Apache-2.0

//! 「这个域名上的篝火在哪儿」—— 加入页旁边那份给客户端读的说明。
//!
//! # 为什么要有它
//!
//! 邀请链接把地址、端口、证书指纹都编在自己身上，所以很长。部署了域名的服务器
//! 其实不需要让人传这么一长串：域名的 HTTPS 证书已经能证明「这是 voice.example.com
//! 说的话」，那就让它自己把语音服务的地址、端口和指纹说出来。用户只输域名，
//! 客户端去 `https://<域名>/.well-known/gouhuo` 取这份说明。
//!
//! 信任链多了一跳，但每一跳都有人担保：
//!
//! ```text
//! 用户输的域名  ->  CA 签的网页证书  ->  这份说明里的指纹  ->  语音的 TLS 连接
//! ```
//!
//! # 里面**没有**加入码
//!
//! 这份说明谁都能取，所以只放公开的东西。[`Discovery::invite`] 永远不带邀请码 ——
//! 写的时候不写，读的时候读到了也丢掉。私人服务器只多一个 `private=1`，
//! 告诉客户端「待会儿要向用户要加入码」。
//!
//! # 格式
//!
//! 一行一个 `键=值`，跟客户端的设置文件一个路数：
//!
//! ```text
//! gouhuo=1
//! invite=gouhuo://j/…
//! name=周末开黑
//! private=1
//! ```
//!
//! 第一行必须是 `gouhuo=<版本>`。域名上跑的可能是任何东西，别把一张
//! 恰好返回 200 的网页当成说明来解析。版本不认识就拒绝，理由同邀请链接。

use crate::invite::{Invite, InviteError};

/// 客户端去哪个路径取这份说明。
pub const DISCOVERY_PATH: &str = "/.well-known/gouhuo";

/// 当前版本。
pub const DISCOVERY_VERSION: u32 = 1;

/// 名字最长多少个字符。它会原样显示在客户端首页上，别让一个服务器把界面撑爆。
const MAX_NAME_CHARS: usize = 40;

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Discovery {
    /// 地址、端口、证书指纹。**不带邀请码**。
    pub invite: Invite,
    /// 给人看的服务器名。可能是空的。
    pub name: String,
    /// 进去要不要加入码。只是提示 —— 说了算的是语音服务自己。
    pub private: bool,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum DiscoveryError {
    /// 不是篝火的说明（那个地址上跑的是别的东西）。
    NotGouhuo,
    /// 是更新的服务端写的，这个客户端看不懂。
    UnsupportedVersion(u32),
    /// 说明里的地址和指纹坏了。
    BadInvite(InviteError),
}

impl core::fmt::Display for DiscoveryError {
    fn fmt(&self, f: &mut core::fmt::Formatter<'_>) -> core::fmt::Result {
        match self {
            DiscoveryError::NotGouhuo => f.write_str("这个地址上没有篝火的加入页"),
            DiscoveryError::UnsupportedVersion(v) => write!(
                f,
                "加入页是版本 {v} 的，当前客户端只认到版本 {DISCOVERY_VERSION}，升级一下"
            ),
            DiscoveryError::BadInvite(e) => write!(f, "加入页给的服务器信息有误：{e}"),
        }
    }
}

impl std::error::Error for DiscoveryError {}

impl Discovery {
    /// 服务端用：从自己的邀请（可能带邀请码）生成公开的说明。
    pub fn public(invite: &Invite, name: &str) -> Self {
        Self {
            invite: Invite {
                code: None,
                ..invite.clone()
            },
            name: clean_name(name),
            private: invite.code.is_some(),
        }
    }

    pub fn to_text(&self) -> Result<String, InviteError> {
        let public = Invite {
            code: None,
            ..self.invite.clone()
        };
        Ok(format!(
            "gouhuo={DISCOVERY_VERSION}\ninvite={}\nname={}\nprivate={}\n",
            public.to_url()?,
            clean_name(&self.name),
            u8::from(self.private),
        ))
    }

    pub fn parse(text: &str) -> Result<Self, DiscoveryError> {
        let mut lines = text.lines().map(str::trim).filter(|l| !l.is_empty());
        let version = lines
            .next()
            .and_then(|line| line.strip_prefix("gouhuo="))
            .and_then(|v| v.trim().parse::<u32>().ok())
            .ok_or(DiscoveryError::NotGouhuo)?;
        if version != DISCOVERY_VERSION {
            return Err(DiscoveryError::UnsupportedVersion(version));
        }

        let (mut invite, mut name, mut private) = (None, String::new(), false);
        for line in lines {
            let Some((key, value)) = line.split_once('=') else {
                continue;
            };
            match key.trim() {
                "invite" => {
                    invite = Some(Invite::parse(value).map_err(DiscoveryError::BadInvite)?);
                }
                "name" => name = clean_name(value),
                "private" => private = value.trim() == "1",
                // 认不出来的键跳过：同一个版本里加可选字段不该让老客户端连不上。
                _ => {}
            }
        }
        let invite = invite.ok_or(DiscoveryError::NotGouhuo)?;
        Ok(Self {
            // 公开的说明里不该有邀请码。真有也不用它：加入码只从用户手里来。
            invite: Invite {
                code: None,
                ..invite
            },
            name,
            private,
        })
    }
}

/// 名字只留一行、不留控制字符、不超长。
fn clean_name(name: &str) -> String {
    name.chars()
        .filter(|c| !c.is_control())
        .collect::<String>()
        .trim()
        .chars()
        .take(MAX_NAME_CHARS)
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::identity::Fingerprint;

    fn invite(code: Option<&str>) -> Invite {
        Invite {
            host: "voice.example.com".into(),
            port: 20800,
            cert: Fingerprint([7; 16]),
            code: code.map(str::to_string),
        }
    }

    #[test]
    fn round_trips() {
        let original = Discovery::public(&invite(None), "周末开黑");
        let parsed = Discovery::parse(&original.to_text().unwrap()).unwrap();
        assert_eq!(parsed, original);
        assert!(!parsed.private);
    }

    /// 最要紧的一条：加入码不出现在公开的说明里。
    #[test]
    fn the_join_code_never_leaves_the_server() {
        let discovery = Discovery::public(&invite(Some("private-secret")), "周末开黑");
        assert!(discovery.private);
        assert_eq!(discovery.invite.code, None);
        let text = discovery.to_text().unwrap();
        assert!(!text.contains("private-secret"));
        assert_eq!(Discovery::parse(&text).unwrap().invite.code, None);
    }

    /// 就算有个写错了的服务端把带码的邀请放了出来，客户端也不拿它当加入码用。
    #[test]
    fn a_leaked_code_is_dropped_when_reading() {
        let leaky = format!(
            "gouhuo=1\ninvite={}\n",
            invite(Some("oops")).to_url().unwrap()
        );
        assert_eq!(Discovery::parse(&leaky).unwrap().invite.code, None);
    }

    #[test]
    fn other_web_pages_are_not_mistaken_for_it() {
        for text in [
            "",
            "<!doctype html><html><body>404</body></html>",
            "invite=gouhuo://j/abc\n",
            "gouhuo=1\nname=没有地址\n",
            "gouhuo=很新\n",
        ] {
            assert_eq!(
                Discovery::parse(text),
                Err(DiscoveryError::NotGouhuo),
                "{text:?}"
            );
        }
    }

    #[test]
    fn future_versions_are_rejected_not_guessed() {
        let text = Discovery::public(&invite(None), "x")
            .to_text()
            .unwrap()
            .replace("gouhuo=1", "gouhuo=2");
        assert_eq!(
            Discovery::parse(&text),
            Err(DiscoveryError::UnsupportedVersion(2))
        );
    }

    #[test]
    fn a_damaged_invite_is_an_error_not_a_guess() {
        let text = "gouhuo=1\ninvite=gouhuo://j/0400e1r7\n";
        assert!(matches!(
            Discovery::parse(text),
            Err(DiscoveryError::BadInvite(_))
        ));
    }

    /// 名字是部署的人随手填的，会原样显示在别人的首页上。
    #[test]
    fn names_are_kept_to_one_short_line() {
        let discovery = Discovery::public(&invite(None), "  第一行\n第二行\tinvite=x  ");
        assert_eq!(discovery.name, "第一行第二行invite=x");
        let text = discovery.to_text().unwrap();
        assert_eq!(text.lines().count(), 4, "{text}");
        assert_eq!(
            Discovery::public(&invite(None), &"长".repeat(200))
                .name
                .chars()
                .count(),
            MAX_NAME_CHARS
        );
    }

    #[test]
    fn unknown_keys_and_blank_lines_are_skipped() {
        let mut text = Discovery::public(&invite(None), "x").to_text().unwrap();
        text.push_str("\n将来的字段=某个值\n这行没有等号\n");
        assert!(Discovery::parse(&text).is_ok());
    }
}
