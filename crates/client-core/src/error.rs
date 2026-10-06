// SPDX-License-Identifier: MPL-2.0

//! 连不上的时候到底是为什么。
//!
//! 这个文件的每一条错误都会**原样显示给用户**，所以它们不是给程序员看的。
//! 痛点之一是「装完还要读文档」—— 连不上又只给一句 `Connection refused`，
//! 用户唯一能做的就是去问发链接给他的那个人，而那个人也不知道。
//!
//! 每条错误都要回答两件事：**发生了什么**，和**现在该做什么**。

use std::fmt;

#[derive(Debug)]
pub enum ConnectError {
    /// 邀请链接本身就不对。
    BadInvite(protocol::InviteError),
    /// TCP 都没连上。
    Unreachable {
        host: String,
        port: u16,
        source: std::io::Error,
    },
    /// 证书指纹对不上 —— 要么服务器换了证书，要么有人在中间。
    WrongCertificate(String),
    /// TLS 本身失败了（不是指纹的问题）。
    Tls(String),
    /// 服务端明确拒绝了。
    Rejected {
        reason: protocol::control::rejected::Reason,
        detail: String,
    },
    /// 连上了，但对面说的话我们听不懂 —— 大概率不是篝火服务端。
    NotAGouhuoServer,
    /// 连接中途断了。
    Io(std::io::Error),
}

impl ConnectError {
    /// Metadata for history/export; contains no address, invitation or remote detail.
    pub fn connection_cause(&self) -> protocol::connection::ConnectionCause {
        use protocol::connection::{ConnectionCause as Cause, ConnectionReason as Reason};
        use protocol::control::rejected::Reason as Rejection;
        let reason = match self {
            Self::BadInvite(_) => Reason::InvalidInvite,
            Self::Unreachable { source, .. } => match source.kind() {
                std::io::ErrorKind::TimedOut => Reason::ConnectionTimeout,
                std::io::ErrorKind::ConnectionRefused => Reason::ConnectionRefused,
                _ => Reason::NetworkError,
            },
            Self::WrongCertificate(_) => Reason::CertificateMismatch,
            Self::Tls(_) => Reason::TlsError,
            Self::NotAGouhuoServer => Reason::ProtocolError,
            Self::Io(source) => match source.kind() {
                std::io::ErrorKind::TimedOut => Reason::ConnectionTimeout,
                std::io::ErrorKind::UnexpectedEof => Reason::ReadError,
                _ => Reason::NetworkError,
            },
            Self::Rejected { reason, .. } => {
                return Cause::server(match reason {
                    Rejection::InviteRequired => Reason::InviteRequired,
                    Rejection::Full => Reason::ServerFull,
                    Rejection::Banned => Reason::Banned,
                    Rejection::VersionMismatch => Reason::VersionMismatch,
                    Rejection::BadSignature => Reason::AuthenticationFailed,
                    Rejection::Internal => Reason::ServerInternal,
                    Rejection::Unspecified => Reason::Unknown,
                })
            }
        };
        Cause::local(reason)
    }

    /// 一句话说清楚出了什么事。
    pub fn headline(&self) -> String {
        use protocol::control::rejected::Reason;
        match self {
            ConnectError::BadInvite(_) => "这条邀请链接不对".into(),
            ConnectError::Unreachable { host, port, .. } => {
                format!("连不上 {host}:{port}")
            }
            ConnectError::WrongCertificate(_) => "服务器的身份对不上".into(),
            ConnectError::Tls(_) => "加密连接建不起来".into(),
            ConnectError::Rejected { reason, detail } => match reason {
                Reason::InviteRequired => "这个服务器要邀请码".into(),
                Reason::Full => "服务器满了".into(),
                Reason::Banned => "你被这个服务器封了".into(),
                Reason::VersionMismatch => "版本对不上".into(),
                Reason::BadSignature => "身份验证没通过".into(),
                // 服务端自己说了原因就用它的，别自作主张翻译
                _ if !detail.is_empty() => detail.clone(),
                _ => "服务器拒绝了这次连接".into(),
            },
            ConnectError::NotAGouhuoServer => "对面不是篝火服务器".into(),
            ConnectError::Io(_) => "连接断了".into(),
        }
    }

    /// 现在该做什么。界面上显示在 [`Self::headline`] 下面一行。
    pub fn advice(&self) -> String {
        use protocol::control::rejected::Reason;
        match self {
            ConnectError::BadInvite(_) => {
                "确认整条链接都复制全了 —— 中间断行或者少几个字符都会这样。\
                 如果是别人转发给你的，让他重新发一次原始链接。"
                    .into()
            }
            ConnectError::Unreachable { .. } => "服务器可能没开，或者端口没转发出来。\
                 如果你们在同一个局域网，让对方确认防火墙放行了；\
                 如果是走公网，确认路由器上 TCP 和 UDP 两个方向都转发了同一个端口。"
                .into(),
            ConnectError::WrongCertificate(detail) => {
                format!(
                    "{detail}\n\
                     最常见的原因是服务器重装过、或者数据目录丢了 —— \
                     那样它会生成新证书，老邀请链接和以前记下的指纹都失效。\
                     先问管理员是不是换过：换过就重新验证，或者让他重发一条邀请链接；\
                     他说没换过，就别连了。"
                )
            }
            ConnectError::Tls(detail) => {
                format!("{detail}\n对面可能不是篝火服务器，或者版本差太远。")
            }
            ConnectError::Rejected { reason, detail } => match reason {
                Reason::InviteRequired => {
                    "找管理员要加入码，或者一条带邀请码的链接。光有地址是进不来的。".into()
                }
                Reason::Full => "等会儿再试。人数上限是服务器自己设的。".into(),
                Reason::VersionMismatch => {
                    format!("{detail}\n升级客户端，或者让管理员升级服务端。")
                }
                Reason::BadSignature => "你的身份文件可能坏了。可以重新生成一个 —— \
                     但那等于换了个人，服务器上给你的权限要重新配。"
                    .into(),
                _ => detail.clone(),
            },
            ConnectError::NotAGouhuoServer => {
                "这个地址和端口上跑的是别的东西。确认一下链接有没有搞错。".into()
            }
            ConnectError::Io(_) => "网络断了或者服务器关了。过一会儿重连试试。".into(),
        }
    }
}

impl ConnectError {
    /// 断线重连时碰到这个错误，**还值不值得再试**。
    ///
    /// 判断标准是「过一会儿会不会自己好」：网络不通、服务器还没起来、满员，
    /// 都会自己好；链接不对、证书变了、被封了、版本不对，再试一万次也一样，
    /// 反复重试只会把真正的原因藏在一句「正在重连」后面。
    pub fn is_retryable(&self) -> bool {
        use protocol::control::rejected::Reason;
        match self {
            ConnectError::Unreachable { .. } | ConnectError::Io(_) => true,
            // 握手中途被掐断（网络抖了）也会落到这里。证书不对单独有一支，不在这儿。
            ConnectError::Tls(_) => true,
            ConnectError::Rejected { reason, .. } => {
                matches!(
                    reason,
                    Reason::Full | Reason::Internal | Reason::Unspecified
                )
            }
            ConnectError::BadInvite(_)
            | ConnectError::WrongCertificate(_)
            | ConnectError::NotAGouhuoServer => false,
        }
    }
}

/// Preserve server evidence independently of the localized farewell text.
pub fn farewell_cause(
    reason: protocol::control::goodbye::Reason,
) -> protocol::connection::ConnectionCause {
    use protocol::connection::{ConnectionCause, ConnectionReason};
    use protocol::control::goodbye::Reason;
    ConnectionCause::server(match reason {
        Reason::Displaced => ConnectionReason::Displaced,
        Reason::Kicked => ConnectionReason::Kicked,
        Reason::Banned => ConnectionReason::Banned,
        Reason::Unspecified => ConnectionReason::Unknown,
    })
}

/// 服务端说了 `Goodbye` 之后，给用户看的两行字：`(发生了什么, 现在该做什么)`。
///
/// 跟 [`ConnectError`] 同一个规矩：两件事都要说到。
pub fn farewell(reason: protocol::control::goodbye::Reason, detail: &str) -> (String, String) {
    use protocol::control::goodbye::Reason;
    match reason {
        Reason::Displaced => (
            "你在别处登录了".into(),
            "同一个身份在另一台电脑或另一个窗口连进了这个服务器，这边就被顶下去了。             想回到这边，重新点加入就行 —— 那边会被顶下去。"
                .into(),
        ),
        Reason::Kicked => (
            "你被请出了服务器".into(),
            if detail.is_empty() {
                "管理员把你踢了出去。可以重新加入 —— 除非接着被封了。".into()
            } else {
                format!("{detail}\n可以重新加入 —— 除非接着被封了。")
            },
        ),
        Reason::Banned => (
            "你被这个服务器封了".into(),
            if detail.is_empty() {
                "这个身份进不来了。有疑问去问管理员。".into()
            } else {
                format!("{detail}\n这个身份进不来了。有疑问去问管理员。")
            },
        ),
        Reason::Unspecified => (
            "服务器断开了连接".into(),
            if detail.is_empty() {
                "服务器没说原因。可以重新加入试试。".into()
            } else {
                detail.to_string()
            },
        ),
    }
}

impl fmt::Display for ConnectError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "{}\n{}", self.headline(), self.advice())
    }
}

impl std::error::Error for ConnectError {}

impl From<std::io::Error> for ConnectError {
    fn from(e: std::io::Error) -> Self {
        ConnectError::Io(e)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use protocol::control::rejected::Reason;

    #[test]
    fn local_failures_do_not_claim_a_remote_diagnosis() {
        use protocol::connection::{ConnectionReason as R, EvidenceSource as S};
        for (kind, expected) in [
            (std::io::ErrorKind::TimedOut, R::ConnectionTimeout),
            (std::io::ErrorKind::ConnectionRefused, R::ConnectionRefused),
            (std::io::ErrorKind::ConnectionReset, R::NetworkError),
        ] {
            let error = ConnectError::Unreachable {
                host: "private-host.invalid".into(),
                port: 20800,
                source: kind.into(),
            };
            let cause = error.connection_cause();
            assert_eq!(cause.reason, expected);
            assert_eq!(cause.source, S::LocalObservation);
            assert!(!format!("{cause:?}").contains("private-host"));
        }
        let cause = ConnectError::Io(std::io::ErrorKind::UnexpectedEof.into()).connection_cause();
        assert_eq!(cause.reason, R::ReadError);
    }

    #[test]
    fn arbitrary_server_detail_does_not_change_or_leak_the_cause() {
        use protocol::connection::{ConnectionReason as R, EvidenceSource as S};
        let error = ConnectError::Rejected {
            reason: Reason::Unspecified,
            detail: "被踢 banned certificate gouhuo://secret".into(),
        };
        let cause = error.connection_cause();
        assert_eq!(cause.reason, R::Unknown);
        assert_eq!(cause.source, S::ServerConfirmed);
        assert!(!format!("{cause:?}").contains("secret"));
        assert_eq!(
            farewell_cause(protocol::control::goodbye::Reason::Unspecified).reason,
            R::Unknown
        );
    }

    /// 每一条错误都必须**既说发生了什么，又说该做什么**。
    ///
    /// 这条测试存在的意义是：将来加新的错误分支时，别只写一半。
    #[test]
    fn every_error_says_what_happened_and_what_to_do() {
        let cases = vec![
            ConnectError::BadInvite(protocol::InviteError::Truncated),
            ConnectError::Unreachable {
                host: "example.com".into(),
                port: 20800,
                source: std::io::Error::other("x"),
            },
            ConnectError::WrongCertificate("指纹对不上".into()),
            ConnectError::Tls("握手失败".into()),
            ConnectError::NotAGouhuoServer,
            ConnectError::Io(std::io::Error::other("x")),
        ];
        let rejections = [
            Reason::InviteRequired,
            Reason::Full,
            Reason::Banned,
            Reason::VersionMismatch,
            Reason::BadSignature,
            Reason::Internal,
        ];

        let all = cases
            .into_iter()
            .chain(rejections.into_iter().map(|reason| ConnectError::Rejected {
                reason,
                detail: "服务端给的说明".into(),
            }));

        for error in all {
            let headline = error.headline();
            let advice = error.advice();
            assert!(!headline.is_empty(), "{error:?} 没说发生了什么");
            assert!(!advice.is_empty(), "{error:?} 没说该怎么办");
            // 一句话就要说清楚，别把整段解释塞进标题
            assert!(
                headline.chars().count() < 40,
                "{error:?} 的标题太长了：{headline}"
            );
        }
    }

    /// 能自己好的才重试。再试也不会变的，要立刻停下来把原因给用户看。
    #[test]
    fn only_transient_errors_are_retried() {
        let transient = [
            ConnectError::Unreachable {
                host: "example.com".into(),
                port: 20800,
                source: std::io::Error::other("x"),
            },
            ConnectError::Io(std::io::Error::other("x")),
            ConnectError::Rejected {
                reason: Reason::Full,
                detail: String::new(),
            },
        ];
        for e in transient {
            assert!(e.is_retryable(), "{e:?} 过一会儿会自己好，该重试");
        }

        let permanent = [
            ConnectError::BadInvite(protocol::InviteError::Truncated),
            ConnectError::WrongCertificate("指纹对不上".into()),
            ConnectError::NotAGouhuoServer,
            ConnectError::Rejected {
                reason: Reason::Banned,
                detail: String::new(),
            },
            ConnectError::Rejected {
                reason: Reason::InviteRequired,
                detail: String::new(),
            },
            ConnectError::Rejected {
                reason: Reason::VersionMismatch,
                detail: String::new(),
            },
            ConnectError::Rejected {
                reason: Reason::BadSignature,
                detail: String::new(),
            },
        ];
        for e in permanent {
            assert!(!e.is_retryable(), "{e:?} 再试也一样，不该重试");
        }
    }

    #[test]
    fn every_farewell_says_what_happened_and_what_to_do() {
        use protocol::control::goodbye::Reason as Bye;
        for reason in [Bye::Unspecified, Bye::Displaced, Bye::Kicked, Bye::Banned] {
            for detail in ["", "服务端给的说明"] {
                let (headline, advice) = farewell(reason, detail);
                assert!(!headline.is_empty(), "{reason:?} 没说发生了什么");
                assert!(!advice.is_empty(), "{reason:?} 没说该怎么办");
                assert!(headline.chars().count() < 40, "{reason:?} 的标题太长了");
            }
        }
    }

    /// 报错里不能出现只有程序员看得懂的东西。
    #[test]
    fn errors_do_not_leak_jargon() {
        let error = ConnectError::Unreachable {
            host: "127.0.0.1".into(),
            port: 20800,
            source: std::io::Error::from(std::io::ErrorKind::ConnectionRefused),
        };
        let text = error.to_string();
        for jargon in ["ConnectionRefused", "os error", "Err(", "rustls"] {
            assert!(!text.contains(jargon), "报错里漏出了 `{jargon}`：{text}");
        }
    }
}
