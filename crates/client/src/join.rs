// SPDX-License-Identifier: GPL-3.0-or-later

//! 点了「加入」之后、坐到篝火边之前的那一段。
//!
//! 用户给的可能是一条邀请链接、一个域名、一个 IP，或者首页上存着的某个服务器。
//! 不管是哪一种，最后都要变成同一样东西 —— 一份带着**地址、端口、证书指纹**的
//! [`Invite`] —— 才能连。这个文件做的就是把它们各自补齐：
//!
//! ```text
//! 邀请链接 ───────────────────────────────┐
//! 存着的服务器 ────────────────────────────┤
//! https:// 加入页 ── 取说明（CA 证书担保）──┼─► 固定指纹连接 ─► 进去了
//! 域名 ── 有加入页？── 有 ─────────────────┤        │
//!            └ 没有 ┐                       │        └ 服务端要加入码 ─► 问用户 ─┐
//! IP、地址:端口 ────┴ 取指纹 ─► 用户核对 ───┘                                   │
//!                                          ▲───────────────────────────────────┘
//! ```
//!
//! # 两处要停下来问用户
//!
//! - **核对指纹**（[`Outcome::ConfirmFingerprint`]）：裸地址没有人替它担保，
//!   指纹是刚从对面取回来的。用户点头之前，身份、昵称、加入码都不发。
//! - **加入码**（[`Outcome::NeedCode`]）：私人服务器。加入码不在加入页上、
//!   也不在任何公开的地方，只能是用户手里的。它只在固定了指纹的连接里发给服务端。
//!
//! # 这里的函数都会阻塞
//!
//! 域名解析、HTTPS、TCP、TLS 握手，加起来最长十几秒。[`run`] 是给后台线程调的，
//! 它用 `progress` 把「现在在干什么」报出去，界面那边自己搬回界面线程。

use std::sync::mpsc::Receiver;

use client_core::address::{self, display_address, HostAddress, JoinPage, Target};
use client_core::{Client, ConnectError, Event};
use protocol::control::rejected::Reason;
use protocol::{Discovery, DiscoveryError, Invite};
use voice_core::identity::Identity;

use crate::discover::{self, FetchError};
use crate::settings::SavedServer;

/// 一个已经知道怎么连的服务器：地址、端口、指纹都有了。
#[derive(Debug, Clone, PartialEq)]
pub struct Known {
    pub invite: Invite,
    /// 给人看的名字。可能是空的，那就显示地址。
    pub name: String,
    /// 加入页地址（不含加入码）。
    pub page: Option<String>,
}

impl Known {
    pub fn address(&self) -> String {
        display_address(&self.invite.host, self.invite.port)
    }

    /// 首页上那行大字：有名字用名字，没有就用地址。
    pub fn title(&self) -> String {
        if self.name.is_empty() {
            self.address()
        } else {
            self.name.clone()
        }
    }

    pub fn from_saved(saved: &SavedServer) -> Option<Self> {
        Some(Self {
            invite: saved.invite()?,
            name: saved.name.clone(),
            page: saved.page.clone(),
        })
    }
}

/// 用户要加入什么。留着它，「重试」就是原样再来一遍。
#[derive(Debug, Clone, PartialEq)]
pub enum Request {
    /// 框里输的东西。
    ///
    /// `fresh`：不用存着的指纹，重新去取。服务器换了证书、用户点了「重新验证」时用。
    Address { text: String, fresh: bool },
    /// 已经知道怎么连的：存着的服务器，或者用户刚核对过指纹、刚补了加入码的那个。
    Known(Known),
}

pub enum Outcome {
    Joined {
        client: Client,
        events: Receiver<Event>,
        server: Known,
    },
    /// 指纹是刚取回来的，没人担保。要用户核对。
    ConfirmFingerprint(Known),
    /// 服务端要加入码。`rejected`：刚才已经给过一个，不对。
    NeedCode {
        server: Known,
        rejected: bool,
    },
    Failed(Failure),
}

/// 没加入成。两行字的规矩跟 `ConnectError` 一样：发生了什么，现在该做什么。
#[derive(Debug, Clone, PartialEq)]
pub struct Failure {
    /// 是哪个服务器（名字或地址）。连地址都没认出来的话是用户输的原文。
    pub title: String,
    pub address: String,
    pub headline: String,
    pub advice: String,
    /// 是证书对不上。这种情况值得给一个「重新验证」的出路。
    pub certificate_changed: bool,
}

/// 调试版和测试里允许 `http://127.0.0.1…` 的加入页：本机起的服务端没有 CA 证书。
/// 正式版不放行 —— 明文取回来的指纹谁都能在半路换掉。
pub fn allow_loopback_http() -> bool {
    cfg!(debug_assertions) || cfg!(test)
}

/// 用户还在输的时候，告诉他这被认成了什么、接下来会怎么连。
/// `Err` 是认不出来的原因。
pub fn describe(text: &str, saved: &[SavedServer]) -> Result<String, String> {
    let target = address::parse(text, allow_loopback_http()).map_err(|e| e.to_string())?;
    Ok(match &target {
        Target::Invite(invite) => {
            format!("邀请链接 · {}", display_address(&invite.host, invite.port))
        }
        Target::Page(page) => format!(
            "{}{}",
            if page.secure {
                "HTTPS 加入页 · 由网站证书担保"
            } else {
                // 只有调试版认这个，见 allow_loopback_http。
                "本机的加入页 · 没加密，只给调试用"
            },
            if page.code.is_some() {
                "，已带加入码"
            } else {
                ""
            }
        ),
        Target::Host(host) => match find_saved(saved, &host.host, host.port_or_default()) {
            Some(known) => format!("存着的服务器 · {}", known.title()),
            None if host.port.is_none() && !host.is_ip() => {
                "域名 · 先找它的 HTTPS 加入页，没有就直连并核对指纹".to_string()
            }
            None => format!(
                "{} · 端口 {}，第一次连接要核对服务器指纹",
                if host.is_ip() { "IP 地址" } else { "地址" },
                host.port_or_default()
            ),
        },
    })
}

fn find_saved(saved: &[SavedServer], host: &str, port: u16) -> Option<Known> {
    saved
        .iter()
        .filter_map(Known::from_saved)
        .find(|known| known.invite.port == port && known.invite.host.eq_ignore_ascii_case(host))
}

/// 从头走到尾。**会阻塞**，在后台线程上调。
///
/// `progress(现在在干什么, 已经知道是哪个服务器了的话是它)`。
pub fn run(
    request: Request,
    identity: &Identity,
    nick: &str,
    saved: &[SavedServer],
    progress: &dyn Fn(&str, Option<&Known>),
) -> Outcome {
    let server = match request {
        Request::Known(server) => server,
        Request::Address { text, fresh } => match resolve(&text, fresh, saved, progress) {
            Ok(Resolved::Vouched(server)) => server,
            Ok(Resolved::Unvouched(server)) => return Outcome::ConfirmFingerprint(server),
            Err(failure) => return Outcome::Failed(failure),
        },
    };

    progress("正在验证服务器身份，加入中…", Some(&server));
    match Client::connect_to(&server.invite, identity, nick) {
        Ok((client, events)) => Outcome::Joined {
            client,
            events,
            server,
        },
        Err(ConnectError::Rejected {
            reason: Reason::InviteRequired,
            ..
        }) => Outcome::NeedCode {
            rejected: server.invite.code.is_some(),
            server,
        },
        Err(e) => Outcome::Failed(Failure {
            title: server.title(),
            address: server.address(),
            headline: e.headline(),
            advice: e.advice(),
            certificate_changed: matches!(e, ConnectError::WrongCertificate(_)),
        }),
    }
}

enum Resolved {
    /// 指纹有人担保：邀请链接、加入页、以前存下的。可以直接连。
    Vouched(Known),
    /// 指纹是刚从对面取回来的。要用户核对。
    Unvouched(Known),
}

fn resolve(
    text: &str,
    fresh: bool,
    saved: &[SavedServer],
    progress: &dyn Fn(&str, Option<&Known>),
) -> Result<Resolved, Failure> {
    let unusable = |advice: String| Failure {
        title: text.trim().chars().take(60).collect(),
        address: String::new(),
        headline: "这个地址用不了".into(),
        advice,
        certificate_changed: false,
    };
    let target =
        address::parse(text, allow_loopback_http()).map_err(|e| unusable(e.to_string()))?;

    match target {
        Target::Invite(invite) => {
            // 链接里没有名字。以前来过的话，沿用那时候的。
            let before = find_saved(saved, &invite.host, invite.port);
            Ok(Resolved::Vouched(Known {
                name: before.as_ref().map(|k| k.name.clone()).unwrap_or_default(),
                page: before.and_then(|k| k.page),
                invite,
            }))
        }
        Target::Page(page) => {
            progress(&format!("正在查找 {} 的加入页…", page.host), None);
            match discover::fetch(&page) {
                Ok(discovery) => Ok(Resolved::Vouched(from_page(&page, discovery, saved))),
                Err(e) => Err(page_failure(&page, e)),
            }
        }
        Target::Host(host) => resolve_host(host, fresh, saved, progress),
    }
}

fn resolve_host(
    host: HostAddress,
    fresh: bool,
    saved: &[SavedServer],
    progress: &dyn Fn(&str, Option<&Known>),
) -> Result<Resolved, Failure> {
    let port = host.port_or_default();
    let before = find_saved(saved, &host.host, port);

    // 以前加入过：指纹是那时候就固定下来的，直接用。
    if !fresh {
        if let Some(mut known) = before.clone() {
            if host.code.is_some() {
                known.invite.code = host.code.clone();
            }
            return Ok(Resolved::Vouched(known));
        }
    }

    // 只给了域名：它可能架着加入页，那样就有 CA 证书替指纹担保，不用麻烦用户核对。
    if host.port.is_none() && !host.is_ip() {
        let page = JoinPage {
            secure: true,
            host: host.host.clone(),
            port: 443,
            code: host.code.clone(),
        };
        progress(&format!("正在查找 {} 的加入页…", host.host), None);
        match discover::fetch(&page) {
            Ok(discovery) => return Ok(Resolved::Vouched(from_page(&page, discovery, saved))),
            // 有加入页，但这个客户端看不懂：别退回去直连，让用户去升级。
            Err(e @ FetchError::Invalid(DiscoveryError::UnsupportedVersion(_))) => {
                return Err(page_failure(&page, e));
            }
            // 没有加入页是常态（很多服务器就只开了语音端口），往下走直连。
            Err(_) => {}
        }
    }

    let shown = display_address(&host.host, port);
    progress(&format!("正在连接 {shown}，读取服务器指纹…"), None);
    match client_core::probe(&host.host, port) {
        Ok(cert) => Ok(Resolved::Unvouched(Known {
            invite: Invite {
                host: host.host,
                port,
                cert,
                // 没带加入码的话，以前存过就沿用 —— 多半是服务器重装了，码没变。
                code: host
                    .code
                    .or_else(|| before.as_ref().and_then(|k| k.invite.code.clone())),
            },
            name: before.map(|k| k.name).unwrap_or_default(),
            page: None,
        })),
        Err(e) => Err(Failure {
            title: shown.clone(),
            address: shown,
            headline: e.headline(),
            advice: e.advice(),
            certificate_changed: false,
        }),
    }
}

/// 加入页给的说明，加上用户手里的加入码，凑成一个能连的服务器。
fn from_page(page: &JoinPage, discovery: Discovery, saved: &[SavedServer]) -> Known {
    let before = find_saved(saved, &discovery.invite.host, discovery.invite.port);
    Known {
        invite: Invite {
            // 加入码只从用户这边来：这次带了就用这次的，没带就用以前存下的。
            code: page
                .code
                .clone()
                .or_else(|| before.as_ref().and_then(|k| k.invite.code.clone())),
            ..discovery.invite
        },
        name: if discovery.name.is_empty() {
            before.map(|k| k.name).unwrap_or_default()
        } else {
            discovery.name
        },
        page: Some(page.url()),
    }
}

fn page_failure(page: &JoinPage, error: FetchError) -> Failure {
    let (headline, advice) = match error {
        FetchError::Unreachable => (
            format!("打不开 {} 的加入页", page.host),
            "连不上这个网址，或者它的 HTTPS 证书有问题。检查一下地址有没有输错；\
             服主没架加入页的话，直接填域名或 IP，或者找他要一条邀请链接。"
                .to_string(),
        ),
        FetchError::NoPage(status) => (
            format!("{} 上没有篝火的加入页", page.host),
            format!(
                "网站是通的，但那个位置上没有东西（回的是 {status}）。\
                 让服主确认加入页开着；或者直接填域名、IP，找他要一条邀请链接也行。"
            ),
        ),
        FetchError::Invalid(e) => (
            format!("{} 的加入页看不懂", page.host),
            format!("{e}。也可以找服主要一条邀请链接。"),
        ),
    };
    Failure {
        title: page.host.clone(),
        address: page.url(),
        headline,
        advice,
        certificate_changed: false,
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::net::{TcpListener, UdpSocket};
    use std::sync::Arc;

    use server::conn::Hub;
    use server::state::{Config, Server};
    use transport::{server_config, ServerCert};

    /// 一个真的篝火服务端，外加（可选）一个真的加入页。
    struct TestServer {
        invite: Invite,
        hub: Arc<Hub>,
        page: Option<String>,
    }

    fn start(code: Option<&str>, with_page: bool) -> TestServer {
        let cert = ServerCert::generate().unwrap();
        let tls = Arc::new(server_config(&cert).unwrap());
        let listener = TcpListener::bind("127.0.0.1:0").unwrap();
        let addr = listener.local_addr().unwrap();
        let voice = UdpSocket::bind("127.0.0.1:0").unwrap();
        let hub = Arc::new(Hub::new(
            Server::new(Config {
                require_invite: code.is_some(),
                invite_code: code.map(str::to_string),
                ..Config::default()
            }),
            voice,
        ));
        {
            let hub = Arc::clone(&hub);
            std::thread::spawn(move || hub.run_voice());
        }
        {
            let hub = Arc::clone(&hub);
            std::thread::spawn(move || server::accept_loop(listener, tls, hub));
        }
        let invite = Invite {
            host: addr.ip().to_string(),
            port: addr.port(),
            cert: cert.fingerprint(),
            code: code.map(str::to_string),
        };
        let page = with_page.then(|| {
            let spare = TcpListener::bind("127.0.0.1:0").unwrap();
            let web = spare.local_addr().unwrap();
            drop(spare);
            server::web::spawn(
                web,
                server::web::JoinPage::new(&invite, "周末开黑").unwrap(),
            )
            .unwrap();
            format!("http://{web}/")
        });
        TestServer { invite, hub, page }
    }

    fn go(request: Request, saved: &[SavedServer]) -> Outcome {
        run(
            request,
            &Identity::generate().unwrap(),
            "阿狸",
            saved,
            &|_, _| {},
        )
    }

    fn typed(text: impl Into<String>) -> Request {
        Request::Address {
            text: text.into(),
            fresh: false,
        }
    }

    fn saved(known: &Known) -> SavedServer {
        SavedServer {
            link: known.invite.to_url().unwrap(),
            name: known.name.clone(),
            last_used: 1,
            page: known.page.clone(),
        }
    }

    /// 只输 IP 和端口：先停下来让用户核对指纹，点头之后才登录。
    #[test]
    fn a_bare_address_stops_for_the_fingerprint_then_joins() {
        let server = start(None, false);
        let text = format!("{}:{}", server.invite.host, server.invite.port);

        let Outcome::ConfirmFingerprint(known) = go(typed(&text), &[]) else {
            panic!("裸地址该先让用户核对指纹");
        };
        assert_eq!(known.invite.cert, server.invite.cert);
        assert_eq!(server.hub.user_count(), 0, "用户还没点头，不该已经登录了");

        let Outcome::Joined { server: joined, .. } = go(Request::Known(known.clone()), &[]) else {
            panic!("核对过指纹之后该能进去");
        };
        assert_eq!(joined, known);
        assert_eq!(server.hub.user_count(), 1);
    }

    /// 存过的服务器再输一遍地址：指纹早就固定了，不再问，也不重新取。
    #[test]
    fn a_saved_address_is_not_asked_about_again() {
        let server = start(None, false);
        let known = Known {
            invite: server.invite.clone(),
            name: "老王的服".into(),
            page: None,
        };
        let text = format!("{}:{}", server.invite.host, server.invite.port);
        let Outcome::Joined { server: joined, .. } = go(typed(&text), &[saved(&known)]) else {
            panic!("存过的服务器该直接进");
        };
        assert_eq!(joined.name, "老王的服");
    }

    /// 存着的指纹跟服务器现在的对不上：必须失败，而且要标出来是证书的问题。
    /// 「重新验证」（fresh）才会重新取指纹，而且取回来还是要用户核对。
    #[test]
    fn a_changed_certificate_fails_until_the_user_reverifies() {
        let server = start(None, false);
        let stale = Known {
            invite: Invite {
                cert: ServerCert::generate().unwrap().fingerprint(),
                ..server.invite.clone()
            },
            name: "老王的服".into(),
            page: None,
        };
        let text = format!("{}:{}", server.invite.host, server.invite.port);
        let store = [saved(&stale)];

        let Outcome::Failed(failure) = go(typed(&text), &store) else {
            panic!("指纹对不上却没失败");
        };
        assert!(failure.certificate_changed);
        assert_eq!(failure.title, "老王的服");

        let fresh = Request::Address { text, fresh: true };
        let Outcome::ConfirmFingerprint(known) = go(fresh, &store) else {
            panic!("重新验证该取回新指纹给用户核对");
        };
        assert_eq!(known.invite.cert, server.invite.cert);
        assert_eq!(known.name, "老王的服", "名字沿用以前的");
    }

    /// 私人服务器：先被要加入码；给错了再要一次并说明不对；给对了才进。
    #[test]
    fn a_private_server_asks_for_the_code() {
        let server = start(Some("winter2026"), false);
        let mut known = Known {
            invite: Invite {
                code: None,
                ..server.invite.clone()
            },
            name: String::new(),
            page: None,
        };
        assert!(matches!(
            go(Request::Known(known.clone()), &[]),
            Outcome::NeedCode {
                rejected: false,
                ..
            }
        ));
        known.invite.code = Some("猜的".into());
        assert!(matches!(
            go(Request::Known(known.clone()), &[]),
            Outcome::NeedCode { rejected: true, .. }
        ));
        assert_eq!(server.hub.user_count(), 0);
        known.invite.code = Some("winter2026".into());
        assert!(matches!(
            go(Request::Known(known), &[]),
            Outcome::Joined { .. }
        ));
    }

    /// 加入页这条路：取说明、不用核对指纹、名字是加入页给的；
    /// 加入码不在说明里，只从用户粘的 `#code=` 里来。
    #[cfg(windows)]
    #[test]
    fn a_join_page_vouches_for_the_server_and_never_supplies_the_code() {
        let server = start(Some("winter2026"), true);
        let page = server.page.clone().unwrap();

        let Outcome::NeedCode {
            server: known,
            rejected,
        } = go(typed(&page), &[])
        else {
            panic!("私人服务器的加入页不该替用户把加入码填上");
        };
        assert!(!rejected);
        assert_eq!(known.invite.cert, server.invite.cert);
        assert_eq!(known.invite.code, None);
        assert_eq!(known.name, "周末开黑");
        assert_eq!(known.page.as_deref(), Some(page.as_str()));

        let Outcome::Joined { server: joined, .. } =
            go(typed(format!("{page}#code=winter2026")), &[])
        else {
            panic!("带着加入码的浏览器邀请该直接进");
        };
        assert_eq!(joined.invite.code.as_deref(), Some("winter2026"));
        assert!(
            !joined.page.unwrap().contains("winter2026"),
            "存下来的加入页地址里不能带加入码"
        );
    }

    /// 那个网址上没有加入页：说清楚，别卡住。
    #[cfg(windows)]
    #[test]
    fn a_missing_join_page_is_explained() {
        let spare = TcpListener::bind("127.0.0.1:0").unwrap();
        let port = spare.local_addr().unwrap().port();
        drop(spare);
        let Outcome::Failed(failure) = go(typed(format!("http://127.0.0.1:{port}/")), &[]) else {
            panic!("没有加入页却没失败");
        };
        assert!(failure.headline.contains("加入页"), "{}", failure.headline);
        assert!(failure.advice.contains("邀请链接"), "{}", failure.advice);
    }

    #[test]
    fn an_invite_link_joins_directly_and_keeps_the_old_name() {
        let server = start(None, false);
        let before = Known {
            invite: server.invite.clone(),
            name: "老王的服".into(),
            page: Some("https://wang.example.com/".into()),
        };
        let link = server.invite.to_url().unwrap();
        let Outcome::Joined { server: joined, .. } = go(typed(link), &[saved(&before)]) else {
            panic!("邀请链接该直接进");
        };
        assert_eq!(joined, before);
    }

    #[test]
    fn nonsense_fails_before_touching_the_network() {
        let Outcome::Failed(failure) = go(typed("你发我的那个码呢？"), &[]) else {
            panic!("乱输的东西却没失败");
        };
        assert_eq!(failure.headline, "这个地址用不了");
        assert!(!failure.advice.is_empty());
    }

    #[test]
    fn hints_say_what_will_happen() {
        let known = Known {
            invite: Invite {
                host: "voice.example.com".into(),
                port: 20800,
                cert: protocol::Fingerprint([1; 16]),
                code: None,
            },
            name: "周末开黑".into(),
            page: None,
        };
        let store = [saved(&known)];
        assert!(describe("voice.example.com", &store)
            .unwrap()
            .contains("周末开黑"));
        assert!(describe("other.example.com", &store)
            .unwrap()
            .contains("加入页"));
        assert!(describe("203.0.113.7", &store).unwrap().contains("指纹"));
        assert!(describe("https://voice.example.com/#code=x", &[])
            .unwrap()
            .contains("已带加入码"));
        assert!(describe(&known.invite.to_url().unwrap(), &[])
            .unwrap()
            .contains("voice.example.com"));
        assert!(describe("http://voice.example.com/", &[]).is_err());
        assert!(describe("乱七八糟", &[]).is_err());
    }
}
