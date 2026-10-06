// SPDX-License-Identifier: MPL-2.0

//! 管理员链接、角色、踢人、封禁、改频道 —— 对着**真服务端**走一遍。
//!
//! 规则本身（谁能踢谁、谁能改哪个频道）在 `server::state` 的单元测试里盖满了。
//! 这里只回答一件事：这些规则从客户端点下去，真的能生效，被踢的人真的会停下来。

use std::net::{TcpListener, UdpSocket};
use std::sync::mpsc::{Receiver, RecvTimeoutError};
use std::sync::Arc;
use std::time::{Duration, Instant};

use client_core::{Client, ConnectError, Ended, Event, Options};
use protocol::control::rejected::Reason;
use protocol::control::Role;
use protocol::Invite;
use server::conn::Hub;
use server::state::{Config, Saved, Server};
use transport::{server_config, ServerCert};
use voice_core::identity::Identity;

const WAIT: Duration = Duration::from_secs(5);
const ADMIN_CODE: &str = "admin-code-for-tests";

struct TestServer {
    invite: Invite,
}

impl TestServer {
    fn link(&self) -> String {
        self.invite.to_url().unwrap()
    }

    fn admin_link(&self) -> String {
        Invite {
            code: Some(ADMIN_CODE.into()),
            ..self.invite.clone()
        }
        .to_url()
        .unwrap()
    }
}

/// 一个还没有管理员、手里攥着一条管理员链接的服务器。
fn open_server() -> TestServer {
    let cert = ServerCert::generate().unwrap();
    let fingerprint = cert.fingerprint();
    let tls = Arc::new(server_config(&cert).unwrap());
    let listener = TcpListener::bind("127.0.0.1:0").unwrap();
    let addr = listener.local_addr().unwrap();
    let voice = UdpSocket::bind("127.0.0.1:0").unwrap();

    let server = Server::restore(
        Config::default(),
        Saved {
            admin_claim: Some(ADMIN_CODE.into()),
            ..Saved::default()
        },
    );
    let hub = Arc::new(Hub::new(server, voice));
    let accept_hub = Arc::clone(&hub);
    std::thread::spawn(move || server::accept_loop(listener, tls, accept_hub));

    TestServer {
        invite: Invite {
            host: addr.ip().to_string(),
            port: addr.port(),
            cert: fingerprint,
            code: None,
        },
    }
}

/// 重连参数调快：要确认的恰恰是「被踢之后**不**重连」，不能靠退避慢来蒙混过关。
fn fast() -> Options {
    Options {
        heartbeat: Duration::from_millis(50),
        liveness_timeout: Duration::from_millis(400),
        reconnect_first: Duration::from_millis(50),
        reconnect_max: Duration::from_millis(200),
    }
}

fn join(link: &str, identity: &Identity, name: &str) -> (Client, Receiver<Event>) {
    Client::connect_with(link, identity, name, fast()).expect("连不上")
}

fn wait_for(events: &Receiver<Event>, mut matches: impl FnMut(&Event) -> bool) -> Event {
    let deadline = Instant::now() + WAIT;
    loop {
        let left = deadline.saturating_duration_since(Instant::now());
        match events.recv_timeout(left) {
            Ok(event) if matches(&event) => return event,
            Ok(_) => continue,
            Err(RecvTimeoutError::Timeout) => panic!("等的事件一直没来"),
            Err(RecvTimeoutError::Disconnected) => panic!("事件通道断了"),
        }
    }
}

fn eventually(what: &str, mut ok: impl FnMut() -> bool) {
    let deadline = Instant::now() + WAIT;
    while Instant::now() < deadline {
        if ok() {
            return;
        }
        std::thread::sleep(Duration::from_millis(20));
    }
    panic!("一直没等到：{what}");
}

/// 结束的方式要是「被拒之门外」，而且之后不许偷偷重连。
fn assert_refused_for_good(
    events: &Receiver<Event>,
    headline_has: &str,
    expected: protocol::connection::ConnectionReason,
) {
    let ended = wait_for(events, |e| {
        matches!(e, Event::Disconnected(_) | Event::Reconnecting { .. })
    });
    let Event::Disconnected(Ended::Refused {
        headline, cause, ..
    }) = &ended
    else {
        panic!("被请出去之后不该重连，也不该当成自己走的：{ended:?}");
    };
    assert!(headline.contains(headline_has), "{headline}");
    assert_eq!(
        cause.source,
        protocol::connection::EvidenceSource::ServerConfirmed
    );
    assert_eq!(cause.reason, expected);
    std::thread::sleep(Duration::from_millis(400));
    assert!(
        events
            .try_iter()
            .all(|e| !matches!(e, Event::Reconnecting { .. } | Event::Reconnected)),
        "被请出去的人在背后重连"
    );
}

#[test]
fn the_admin_link_makes_you_admin_and_admins_get_the_ban_list() {
    let server = open_server();
    let (admin, _events) = join(
        &server.admin_link(),
        &Identity::generate().unwrap(),
        "管理员",
    );
    assert!(admin.roster().is_admin(), "用管理员链接进来不是管理员");

    // 链接用过就作废：第二个人拿它进来只是个普通成员
    let (second, _events) = join(
        &server.admin_link(),
        &Identity::generate().unwrap(),
        "也想当",
    );
    assert!(!second.roster().is_admin());
}

#[test]
fn kick_ban_and_unban_from_the_client() {
    let server = open_server();
    let (admin, _admin_events) = join(
        &server.admin_link(),
        &Identity::generate().unwrap(),
        "管理员",
    );
    let (alice, _alice_events) = join(&server.link(), &Identity::generate().unwrap(), "阿狸");
    let bob_identity = Identity::generate().unwrap();
    let (bob, bob_events) = join(&server.link(), &bob_identity, "波波");

    // 管理员把阿狸提成频道管理，她自己那边也看得到
    admin.set_role(alice.session_id(), Role::ChannelAdmin);
    eventually("阿狸成了频道管理", || {
        alice.roster().my_role() == Role::ChannelAdmin
    });
    let bob_session = bob.session_id();
    assert!(alice.roster().can_kick(bob_session));
    assert!(!alice.roster().can_ban(bob_session), "频道管理不该能封人");
    assert!(!bob.roster().can_kick(alice.session_id()));

    // 踢：波波停下来，不自动重连；但他自己再连是可以的
    alice.kick(bob_session, "去隔壁吵");
    assert_refused_for_good(
        &bob_events,
        "请出",
        protocol::connection::ConnectionReason::Kicked,
    );
    let (bob, bob_events) = join(&server.link(), &bob_identity, "波波");

    // 封：停下来，而且再也进不来
    let bob_session = bob.session_id();
    eventually("管理员那边看到波波回来了", || {
        admin.roster().users.contains_key(&bob_session)
    });
    admin.ban(bob_session, "刷屏");
    assert_refused_for_good(
        &bob_events,
        "封",
        protocol::connection::ConnectionReason::Banned,
    );
    eventually("封禁名单里有波波", || {
        admin
            .roster()
            .bans
            .iter()
            .any(|b| b.name == "波波" && b.reason == "刷屏")
    });
    assert!(alice.roster().bans.is_empty(), "封禁名单漏给了非管理员");
    match Client::connect_with(&server.link(), &bob_identity, "换个名字", fast()) {
        Err(ConnectError::Rejected {
            reason: Reason::Banned,
            ..
        }) => {}
        other => panic!("被封的人又进来了：{:?}", other.map(|_| ())),
    }

    // 解封：名单清空，又进得来了
    let key = admin.roster().bans[0].public_key.clone();
    admin.unban(&key);
    eventually("封禁名单清空", || admin.roster().bans.is_empty());
    assert!(Client::connect_with(&server.link(), &bob_identity, "波波", fast()).is_ok());
}

#[test]
fn renaming_a_channel_reaches_everyone() {
    let server = open_server();
    let (alice, _events) = join(&server.link(), &Identity::generate().unwrap(), "阿狸");
    let (bob, _events) = join(&server.link(), &Identity::generate().unwrap(), "波波");

    alice.create_channel("开黑", 0);
    let find = |client: &Client, name: &str| {
        client
            .roster()
            .channels
            .values()
            .find(|c| c.name == name)
            .map(|c| c.id)
    };
    eventually("频道建好", || find(&bob, "开黑").is_some());
    let id = find(&alice, "开黑").unwrap();
    assert!(alice.roster().can_edit_channel(id));
    assert!(!bob.roster().can_edit_channel(id), "别人建的频道不该能改");

    // 别人改不动
    bob.rename_channel(id, "我的了");
    alice.rename_channel(id, "吃鸡");
    eventually("波波那边看到改了名", || {
        find(&bob, "吃鸡") == Some(id)
    });
    assert_eq!(find(&alice, "我的了"), None);
}
