// SPDX-License-Identifier: MPL-2.0

//! 网络状况：体检会话、服务端的 NetworkReport、旧服务端下的 Pong 估算。
//! 对着真服务端跑，没有 mock。

use std::net::{TcpListener, UdpSocket};
use std::sync::atomic::AtomicBool;
use std::sync::mpsc::{Receiver, RecvTimeoutError};
use std::sync::Arc;
use std::time::{Duration, Instant};

use client_core::netcheck::{self, CheckPlan, PhasePlan, Stage, Verdict};
use client_core::{Client, ConnectError, Event, VoiceReport};
use protocol::control::rejected::Reason;
use protocol::Invite;
use server::conn::Hub;
use server::state::{Config, Server};
use transport::{server_config, ServerCert};
use voice_core::identity::Identity;

const WAIT: Duration = Duration::from_secs(8);

struct TestServer {
    invite: Invite,
    hub: Arc<Hub>,
}

fn start(config: Config) -> TestServer {
    let cert = ServerCert::generate().unwrap();
    let fingerprint = cert.fingerprint();
    let tls = Arc::new(server_config(&cert).unwrap());
    let listener = TcpListener::bind("127.0.0.1:0").unwrap();
    let addr = listener.local_addr().unwrap();
    let voice = UdpSocket::bind("127.0.0.1:0").unwrap();
    let invite_code = config.invite_code.clone();
    let hub = Arc::new(Hub::new(Server::new(config), voice));
    let voice_hub = Arc::clone(&hub);
    std::thread::spawn(move || voice_hub.run_voice());
    let accept_hub = Arc::clone(&hub);
    std::thread::spawn(move || server::accept_loop(listener, tls, accept_hub));
    TestServer {
        invite: Invite {
            host: addr.ip().to_string(),
            port: addr.port(),
            cert: fingerprint,
            code: invite_code,
        },
        hub,
    }
}

fn quick_plan() -> CheckPlan {
    CheckPlan {
        tls_pings: 3,
        slow: PhasePlan {
            rate_per_sec: 20,
            seconds: 0.5,
            payload: 0,
        },
        fast: PhasePlan {
            rate_per_sec: 100,
            seconds: 0.5,
            payload: 80,
        },
        grace: Duration::from_millis(200),
    }
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

/// 体检走完整条真实路径，服务端认得体检模式：名单里看不到它，报告有数字。
#[test]
fn a_check_measures_udp_without_joining() {
    let server = start(Config::default());
    let (watcher, events) =
        Client::connect_to(&server.invite, &Identity::generate().unwrap(), "围观").unwrap();

    let stages = std::sync::Mutex::new(Vec::new());
    let report = netcheck::run(
        &server.invite,
        &Identity::generate().unwrap(),
        &quick_plan(),
        &|stage| stages.lock().unwrap().push(stage),
        &AtomicBool::new(false),
    )
    .expect("体检该成功");

    assert!(report.server_probe_aware, "新服务端要确认体检模式");
    assert!(report.server_tcp_voice);
    assert_eq!(report.tls_rtt_ms.len(), 3, "三次 TLS Ping 都该回来");
    let slow = report.slow.as_ref().unwrap();
    let fast = report.fast.as_ref().unwrap();
    assert_eq!(slow.sent, 10);
    assert_eq!(fast.sent, 50);
    assert!(slow.received >= 9, "回环上基本不该丢：{slow:?}");
    assert!(fast.received >= 45, "回环上基本不该丢：{fast:?}");
    assert!(fast.rtt_median_ms < 200.0);
    assert!(matches!(
        report.verdict,
        Verdict::Good | Verdict::Fluctuating
    ));
    assert_eq!(stages.lock().unwrap().last(), Some(&Stage::Done));
    let text = report.to_text();
    assert!(text.contains("UDP 说话节奏"), "{text}");
    assert!(!text.contains("旧版本"), "{text}");

    // 围观者从头到尾没见过第二个人。
    std::thread::sleep(Duration::from_millis(200));
    while let Ok(event) = events.try_recv() {
        assert!(
            !matches!(event, Event::Joined { .. } | Event::Left { .. }),
            "体检会话不该出现在名单里：{event:?}"
        );
    }
    assert_eq!(watcher.roster().users.len(), 1);
    assert_eq!(server.hub.user_count(), 1);
}

/// 体检照常验邀请码：码不对，连 UDP 都测不到。
#[test]
fn a_check_still_needs_the_invite_code() {
    let server = start(Config {
        invite_code: Some("secret".into()),
        require_invite: true,
        ..Config::default()
    });
    let mut wrong = server.invite.clone();
    wrong.code = Some("nope".into());
    let result = netcheck::run(
        &wrong,
        &Identity::generate().unwrap(),
        &quick_plan(),
        &|_| {},
        &AtomicBool::new(false),
    );
    assert!(
        matches!(
            result,
            Err(netcheck::CheckError::Connect(ConnectError::Rejected {
                reason: Reason::InviteRequired,
                ..
            }))
        ),
        "错码不该通过"
    );
}

/// 取消立刻生效，服务端那边也不留下会话。
#[test]
fn a_check_can_be_cancelled() {
    let server = start(Config::default());
    let cancel = Arc::new(AtomicBool::new(false));
    let flag = Arc::clone(&cancel);
    let result = netcheck::run(
        &server.invite,
        &Identity::generate().unwrap(),
        &CheckPlan {
            slow: PhasePlan {
                rate_per_sec: 10,
                seconds: 30.0,
                payload: 0,
            },
            ..quick_plan()
        },
        &move |stage| {
            if matches!(stage, Stage::Udp { .. }) {
                flag.store(true, std::sync::atomic::Ordering::Relaxed);
            }
        },
        &cancel,
    );
    assert!(matches!(result, Err(netcheck::CheckError::Cancelled)));
}

/// 服务端每隔几秒把所有人的网络状况发给所有人：上行丢包是服务端量的，
/// RTT 和下行丢包是各自在 Ping 里报的。
#[test]
fn the_server_reports_everyones_network() {
    let server = start(Config::default());
    let (alice, alice_events) =
        Client::connect_to(&server.invite, &Identity::generate().unwrap(), "alice").unwrap();
    let (bob, _bob_events) =
        Client::connect_to(&server.invite, &Identity::generate().unwrap(), "bob").unwrap();
    bob.set_voice_report(VoiceReport {
        packets_sent: 0,
        rtt_ms: Some(180),
        downlink_loss_permille: Some(35),
        tcp_voice: false,
    });
    // bob 的下一次心跳要等 5 秒；先让服务端收到它，再广播报告。
    std::thread::sleep(client_core::HEARTBEAT + Duration::from_millis(500));
    server.hub.broadcast_network_report();
    wait_for(&alice_events, |e| {
        matches!(e, Event::RosterChanged) && alice.roster().network_from_server
    });
    let roster = alice.roster();
    let bobs = roster.network_of(bob.session_id()).expect("报告里该有 bob");
    assert!(bobs.rtt_known && bobs.rtt_ms == 180, "{bobs:?}");
    assert!(
        bobs.downlink_known && bobs.downlink_loss_permille == 35,
        "{bobs:?}"
    );
    assert!(
        !bobs.uplink_known,
        "bob 一个包都没发，服务端不该凭空给出上行丢包：{bobs:?}"
    );
    assert!(roster.network_of(alice.session_id()).is_some());
    drop(roster);

    // 服务端给过报告之后，本地的 Pong 估算不再覆盖。
    alice.set_voice_report(VoiceReport {
        packets_sent: 5000,
        rtt_ms: Some(20),
        downlink_loss_permille: None,
        tcp_voice: false,
    });
    std::thread::sleep(client_core::HEARTBEAT * 2 + Duration::from_millis(500));
    let roster = alice.roster();
    assert!(roster.network_from_server);
    let mine = roster.network_of(alice.session_id()).unwrap();
    assert!(
        !mine.rtt_known,
        "服务端那份说了算，本地 Pong 估算不该把 RTT 20 写进去：{mine:?}"
    );
}
