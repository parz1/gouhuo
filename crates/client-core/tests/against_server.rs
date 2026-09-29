// SPDX-License-Identifier: MPL-2.0

//! 对着**真服务端**跑。没有 mock，没有假握手。
//!
//! 起一个真的篝火服务端，生成一条真的邀请链接，然后走用户实际走的那条路：
//! 粘链接 → 连上 → 看到名单 → 说话 → 走人。
//!
//! 这个文件回答的是「界面之下的那一层到底能不能用」。等界面接上去的时候，
//! 它只需要把这里已经验证过的东西画出来。

use std::net::{TcpListener, UdpSocket};
use std::sync::mpsc::{Receiver, RecvTimeoutError};
use std::sync::Arc;
use std::time::Duration;

use client_core::{Client, ConnectError, Ended, Event};
use protocol::control::rejected::Reason;
use protocol::Invite;
use server::conn::Hub;
use server::state::{Config, Server};
use transport::{server_config, ServerCert};
use voice_core::identity::Identity;

const WAIT: Duration = Duration::from_secs(5);

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

fn open_server() -> TestServer {
    start(Config::default())
}

fn join(server: &TestServer, name: &str) -> (Client, Receiver<Event>) {
    let link = server.invite.to_url().unwrap();
    Client::connect(&link, &Identity::generate().unwrap(), name).expect("连不上")
}

/// 等一个满足条件的事件，中间别的事件丢掉。
fn wait_for(events: &Receiver<Event>, mut matches: impl FnMut(&Event) -> bool) -> Event {
    let deadline = std::time::Instant::now() + WAIT;
    loop {
        let left = deadline.saturating_duration_since(std::time::Instant::now());
        match events.recv_timeout(left) {
            Ok(event) if matches(&event) => return event,
            Ok(_) => continue,
            Err(RecvTimeoutError::Timeout) => panic!("等的事件一直没来"),
            Err(RecvTimeoutError::Disconnected) => panic!("事件通道断了"),
        }
    }
}

/// 用户实际经历的全部过程：粘一条链接进来，然后就在频道里了。
#[test]
fn an_invite_link_is_all_you_need() {
    let server = open_server();
    let (client, _events) = join(&server, "阿狸");

    let roster = client.roster();
    assert_eq!(roster.me, client.session_id());
    assert_eq!(roster.name_of(roster.me), "阿狸");
    assert_eq!(roster.users.len(), 1);

    // 频道树已经在了，界面可以直接画
    let tree = roster.tree();
    assert_eq!(tree.len(), 1, "至少该有个根频道");
    assert_eq!(tree[0].depth, 0);
    assert_eq!(roster.my_channel(), tree[0].channel.id);

    // 语音那半边要的东西也齐了
    assert_ne!(client.udp_port(), 0, "没拿到语音端口");
    assert_ne!(
        client.voice_keys().upstream.as_bytes(),
        client.voice_keys().downstream.as_bytes(),
        "上下行密钥不该是同一把"
    );
}

/// 第二个人进来，第一个人要收到「有人进来了」，而且带着名字 ——
/// 界面要靠它播提示音和念 TTS。
#[test]
fn a_newcomer_shows_up_with_a_name() {
    let server = open_server();
    let (alice, alice_events) = join(&server, "阿狸");
    let (bob, _bob_events) = join(&server, "波波");

    let event = wait_for(&alice_events, |e| matches!(e, Event::Joined { .. }));
    let Event::Joined { session, name } = event else {
        unreachable!()
    };
    assert_eq!(name, "波波", "提示音要念名字，事件里就得带着");
    assert_eq!(session, bob.session_id());

    // 名单也跟上了
    wait_for(&alice_events, |e| *e == Event::RosterChanged);
    assert_eq!(alice.roster().users.len(), 2);
}

/// 自己登录不该被当成「有人进来了」—— 否则一进频道就先给自己播一声。
#[test]
fn my_own_login_is_not_announced() {
    let server = open_server();
    let (_client, events) = join(&server, "阿狸");

    // 服务端会把自己的 UserState 也广播回来，这里应该只看到 RosterChanged
    match events.recv_timeout(Duration::from_millis(500)) {
        Ok(Event::Joined { name, .. }) => panic!("给自己播了一声进场：{name}"),
        Ok(_) | Err(RecvTimeoutError::Timeout) => {}
        Err(e) => panic!("{e}"),
    }
}

#[test]
fn text_goes_around_and_is_attributed() {
    let server = open_server();
    let (alice, alice_events) = join(&server, "阿狸");
    let (bob, _bob_events) = join(&server, "波波");
    wait_for(&alice_events, |e| matches!(e, Event::Joined { .. }));

    bob.send_text("  在哪  ");

    let event = wait_for(&alice_events, |e| matches!(e, Event::Text(_)));
    let Event::Text(line) = event else {
        unreachable!()
    };
    assert_eq!(line.body, "在哪", "前后空白该在客户端就去掉");
    assert_eq!(line.sender_session, bob.session_id());
    assert_eq!(line.sender_name, "波波");
    assert!(line.timestamp_ms > 0, "时间戳该是服务端盖的");

    // 也进了本地的聊天记录，界面直接读这个
    assert_eq!(alice.roster().chat.back().unwrap().body, "在哪");
}

/// 空消息不该发出去 —— 用户手滑按了回车不该在别人屏幕上留一行空白。
#[test]
fn empty_text_is_not_sent() {
    let server = open_server();
    let (alice, alice_events) = join(&server, "阿狸");
    let (bob, _bob_events) = join(&server, "波波");
    wait_for(&alice_events, |e| matches!(e, Event::Joined { .. }));

    bob.send_text("   ");
    bob.send_text("");
    bob.send_text("真的一条");

    let event = wait_for(&alice_events, |e| matches!(e, Event::Text(_)));
    let Event::Text(line) = event else {
        unreachable!()
    };
    assert_eq!(line.body, "真的一条", "空消息被发出去了");
    assert_eq!(alice.roster().chat.len(), 1);
}

#[test]
fn self_state_propagates_to_everyone() {
    let server = open_server();
    let (alice, alice_events) = join(&server, "阿狸");
    let (bob, _bob_events) = join(&server, "波波");
    wait_for(&alice_events, |e| matches!(e, Event::Joined { .. }));

    bob.set_self_state(false, true);

    let deadline = std::time::Instant::now() + WAIT;
    loop {
        wait_for(&alice_events, |e| *e == Event::RosterChanged);
        let roster = alice.roster();
        let bob_user = roster.users.get(&bob.session_id()).unwrap();
        if bob_user.self_deafened {
            assert!(bob_user.self_muted, "关了耳朵就该同时闭麦");
            break;
        }
        drop(roster);
        assert!(std::time::Instant::now() < deadline, "状态一直没同步过来");
    }
}

/// 有人走了，剩下的人要知道是谁走的 —— 名字得在事件里，
/// 因为那时候他已经从名单里删掉了。
#[test]
fn leaving_is_announced_with_the_name() {
    let server = open_server();
    let (alice, alice_events) = join(&server, "阿狸");
    let (bob, bob_events) = join(&server, "波波");
    wait_for(&alice_events, |e| matches!(e, Event::Joined { .. }));
    let bob_session = bob.session_id();

    bob.disconnect();
    // 断开的人自己也要收到通知，界面才知道该切回未连接状态。
    // 而且要标明是自己走的 —— 不能重连，界面也不该报错。
    let ended = wait_for(&bob_events, |e| matches!(e, Event::Disconnected(_)));
    assert_eq!(ended, Event::Disconnected(Ended::ByUser));

    let event = wait_for(&alice_events, |e| matches!(e, Event::Left { .. }));
    let Event::Left { session, name } = event else {
        unreachable!()
    };
    assert_eq!(session, bob_session);
    assert_eq!(name, "波波");
    assert_eq!(alice.roster().users.len(), 1);
}

/// 邀请码不对的时候，报错要告诉用户该去做什么。
#[test]
fn a_wrong_invite_code_tells_you_what_to_do() {
    let server = start(Config {
        require_invite: true,
        invite_code: Some("winter2026".into()),
        ..Config::default()
    });

    let mut wrong = server.invite.clone();
    wrong.code = Some("猜的".into());
    let error = Client::connect_to(&wrong, &Identity::generate().unwrap(), "路人")
        .err()
        .expect("邀请码不对却连上了");

    assert!(
        matches!(
            error,
            ConnectError::Rejected {
                reason: Reason::InviteRequired,
                ..
            }
        ),
        "{error:?}"
    );
    assert!(error.advice().contains("管理员"), "{}", error.advice());
    assert_eq!(server.hub.user_count(), 0);

    // 正确的那条照样能进
    let (_client, _events) = join(&server, "自己人");
    assert_eq!(server.hub.user_count(), 1);
}

/// 指纹对不上必须连不上，而且要说清楚这可能意味着什么。
#[test]
fn a_wrong_fingerprint_is_explained_not_just_refused() {
    let server = open_server();
    let mut tampered = server.invite.clone();
    tampered.cert = ServerCert::generate().unwrap().fingerprint();

    let error = Client::connect_to(&tampered, &Identity::generate().unwrap(), "阿狸")
        .err()
        .expect("指纹不对却连上了");
    assert!(
        matches!(error, ConnectError::WrongCertificate(_)),
        "{error:?}"
    );
    let advice = error.advice();
    assert!(advice.contains("重装"), "要说清楚最常见的原因：{advice}");
    assert!(advice.contains("别连"), "也要说什么时候该收手：{advice}");
}

/// 改过的链接在碰网络之前就该被拦下。
#[test]
fn a_tampered_link_never_reaches_the_network() {
    let server = open_server();
    let link = server.invite.to_url().unwrap();
    let mut chars: Vec<char> = link.chars().collect();
    let mid = chars.len() - 4;
    chars[mid] = if chars[mid] == 'a' { 'b' } else { 'a' };
    let tampered: String = chars.into_iter().collect();

    let error = Client::connect(&tampered, &Identity::generate().unwrap(), "阿狸")
        .err()
        .expect("改过的链接却连上了");
    assert!(matches!(error, ConnectError::BadInvite(_)), "{error:?}");
    assert!(error.advice().contains("复制"), "{}", error.advice());
    assert_eq!(server.hub.user_count(), 0);
}

/// 服务器没开的时候，别只说一句「连接被拒绝」。
#[test]
fn an_unreachable_server_says_what_to_check() {
    // 绑一个端口再立刻放掉，拿到一个几乎肯定没人听的端口号
    let spare = TcpListener::bind("127.0.0.1:0").unwrap();
    let port = spare.local_addr().unwrap().port();
    drop(spare);

    let invite = Invite {
        host: "127.0.0.1".into(),
        port,
        cert: ServerCert::generate().unwrap().fingerprint(),
        code: None,
    };
    let error = Client::connect_to(&invite, &Identity::generate().unwrap(), "阿狸")
        .err()
        .expect("没人听的端口却连上了");

    assert!(
        matches!(error, ConnectError::Unreachable { .. }),
        "{error:?}"
    );
    let advice = error.advice();
    assert!(advice.contains("防火墙"), "{advice}");
    assert!(advice.contains("转发"), "{advice}");
}

/// 同一个身份连第二次会顶掉第一次 —— 换机器、客户端崩了重开都会走到这儿。
/// 旧的那条要收到 Disconnected，界面才知道该说「你在别处登录了」。
#[test]
fn logging_in_again_disconnects_the_old_client() {
    let server = open_server();
    let identity = Identity::generate().unwrap();
    let link = server.invite.to_url().unwrap();

    let (_first, first_events) = Client::connect(&link, &identity, "阿狸").unwrap();
    let second_identity = Identity::import(&identity.export()).unwrap();
    let (second, _second_events) = Client::connect(&link, &second_identity, "阿狸").unwrap();

    let ended = wait_for(&first_events, |e| matches!(e, Event::Disconnected(_)));
    let Event::Disconnected(Ended::Refused { headline, .. }) = ended else {
        panic!("被顶号要说明原因，不能当成自己走的：{ended:?}");
    };
    assert!(headline.contains("别处"), "{headline}");
    assert_eq!(server.hub.user_count(), 1, "顶号顶成了两个人");
    assert_eq!(second.roster().users.len(), 1);
}

// ==========================================================================
// 多频道
//
// 状态机那层的规则在 server::state 的单元测试里盖满了。这里只管一件事：
// **一个人建的频道，另一个人真的能看见、能进去** —— 那要求协议、广播、
// 名单三段都对得上，单元测试一段都碰不到。
// ==========================================================================

/// 等到名单里出现一个叫这个名字的频道，返回它的 id。
fn wait_for_channel(client: &Client, events: &Receiver<Event>, name: &str) -> u32 {
    let deadline = std::time::Instant::now() + WAIT;
    loop {
        if let Some(id) = client
            .roster()
            .channels
            .values()
            .find(|c| c.name == name)
            .map(|c| c.id)
        {
            return id;
        }
        if std::time::Instant::now() >= deadline {
            panic!("等「{name}」这个频道一直没等到");
        }
        let _ = events.recv_timeout(Duration::from_millis(100));
    }
}

#[test]
fn a_channel_one_person_makes_shows_up_for_everyone() {
    let server = open_server();
    let (maker, maker_events) = join(&server, "阿强");
    let (watcher, watcher_events) = join(&server, "阿伟");
    // 在先到的那个身上等 —— 自己刚连上不会给自己发事件，名单是从 Welcome 建的。
    wait_for(&maker_events, |e| matches!(e, Event::Joined { .. }));

    maker.create_channel("打本", 0);

    // 建的人自己也是从服务端的广播里知道的 —— 不本地先插。
    let mine = wait_for_channel(&maker, &maker_events, "打本");
    let theirs = wait_for_channel(&watcher, &watcher_events, "打本");
    assert_eq!(mine, theirs, "两边看到的该是同一个频道 id");
}

#[test]
fn you_can_actually_walk_into_a_channel_someone_else_made() {
    let server = open_server();
    let (maker, maker_events) = join(&server, "阿强");
    let (walker, walker_events) = join(&server, "阿伟");
    wait_for(&maker_events, |e| matches!(e, Event::Joined { .. }));

    maker.create_channel("打本", 0);
    let id = wait_for_channel(&walker, &walker_events, "打本");

    walker.join_channel(id);

    // 两边都要等。**两个客户端各有各的读线程**，一边看到了不代表另一边也看到了 ——
    // 只等一边的话这个测试会随机红，而那种红比不测还糟。
    let deadline = std::time::Instant::now() + WAIT;
    loop {
        // 别把锁攥到断言里：断言失败时 guard 还活着，会把锁毒掉，
        // 然后读线程跟着 panic，真正的失败原因就被埋了。
        let walker_in = walker.roster().my_channel() == id;
        let maker_sees = maker.roster().users_in(id).iter().any(|u| u.name == "阿伟");
        if walker_in && maker_sees {
            break;
        }
        assert!(
            std::time::Instant::now() < deadline,
            "进频道没同步上：自己那边 {walker_in}，对面那边 {maker_sees}"
        );
        let _ = walker_events.recv_timeout(Duration::from_millis(50));
        let _ = maker_events.recv_timeout(Duration::from_millis(50));
    }
}

/// 有人正在建频道的时候进来的人，最后也得看到**全部**频道。
///
/// 新人拿到的 Welcome 是登录那一刻的快照，之后的变化只靠广播。服务端曾经是
/// 先发 Welcome、再把人登记进广播名单，中间那一小段里建的频道新人就永远
/// 看不到了 —— CI 上偶发「等不到别人建的频道」就是它。窗口很窄，这里靠
/// 一边狂建一边连几个人把它撞出来。
#[test]
fn people_who_arrive_mid_change_still_see_everything() {
    const COUNT: usize = 60;
    let server = open_server();
    let (maker, maker_events) = join(&server, "阿强");

    let builder = {
        let maker = maker.clone();
        std::thread::spawn(move || {
            for i in 0..COUNT {
                maker.create_channel(&format!("频道{i}"), 0);
                std::thread::sleep(Duration::from_millis(1));
            }
        })
    };
    let newcomers: Vec<_> = (0..4).map(|i| join(&server, &format!("路人{i}"))).collect();
    builder.join().unwrap();

    // 建的人自己看到最后一个，说明服务端全建完、广播也全发出去了。
    wait_for_channel(&maker, &maker_events, &format!("频道{}", COUNT - 1));

    for (client, events) in &newcomers {
        let deadline = std::time::Instant::now() + WAIT;
        loop {
            let missing: Vec<_> = {
                let roster = client.roster();
                (0..COUNT)
                    .map(|i| format!("频道{i}"))
                    .filter(|name| !roster.channels.values().any(|c| &c.name == name))
                    .collect()
            };
            if missing.is_empty() {
                break;
            }
            assert!(
                std::time::Instant::now() < deadline,
                "{} 漏了这些频道：{missing:?}",
                client.roster().name_of(client.session_id())
            );
            let _ = events.recv_timeout(Duration::from_millis(100));
        }
    }
}

/// 删掉一个有人在里面的频道，**那个人要被挪回根频道**。
///
/// 留在一个不存在的频道里的话，他的语音会被转发到没人收的地方，
/// 而界面上完全看不出问题 —— 这是最难查的一类。
#[test]
fn deleting_a_channel_does_not_strand_the_people_inside() {
    let server = open_server();
    let (maker, maker_events) = join(&server, "阿强");
    let (walker, walker_events) = join(&server, "阿伟");
    wait_for(&maker_events, |e| matches!(e, Event::Joined { .. }));

    let root = walker.roster().root().expect("没有根频道");

    maker.create_channel("打本", 0);
    let id = wait_for_channel(&walker, &walker_events, "打本");
    walker.join_channel(id);

    let deadline = std::time::Instant::now() + WAIT;
    while walker.roster().my_channel() != id {
        assert!(std::time::Instant::now() < deadline, "没进去");
        let _ = walker_events.recv_timeout(Duration::from_millis(100));
    }

    maker.delete_channel(id);

    let deadline = std::time::Instant::now() + WAIT;
    loop {
        let roster = walker.roster();
        if !roster.channels.contains_key(&id) && roster.my_channel() == root {
            break;
        }
        drop(roster);
        assert!(
            std::time::Instant::now() < deadline,
            "频道删了，但人没被挪回根频道"
        );
        let _ = walker_events.recv_timeout(Duration::from_millis(100));
    }
    let _ = maker_events;
}

/// 服务端拒了就是什么都不会发生 —— 客户端不该留下幽灵。
#[test]
fn a_refused_creation_leaves_no_ghost_channel() {
    let server = start(Config {
        invite_code: Some("letmein".to_string()),
        ..Config::default()
    });
    // 把邀请链接里的码抹掉再连 = 访客，建不了频道。
    let guest_invite = Invite {
        code: None,
        ..server.invite.clone()
    };
    let (guest, _events) = Client::connect(
        &guest_invite.to_url().unwrap(),
        &Identity::generate().unwrap(),
        "路人",
    )
    .expect("连不上");

    // connect 返回时名单已经从 Welcome 建好了，不用等。
    let before = guest.roster().channels.len();
    guest.create_channel("捣乱", 0);

    // 给服务端足够的时间把它忽略掉。
    std::thread::sleep(Duration::from_millis(300));
    assert_eq!(
        guest.roster().channels.len(),
        before,
        "被拒之后不该在本地留下一个只有自己看得见的频道"
    );
}

/// 提示音要的是「进出**我这个**频道」，对着真服务端走一遍：
/// 连进来、挪走、挪回来、断线。
#[test]
fn comings_and_goings_in_my_channel() {
    let server = open_server();
    let (_alice, alice_events) = join(&server, "阿狸");

    let (bob, _bob_events) = join(&server, "波波");
    let event = wait_for(&alice_events, |e| {
        matches!(e, Event::CameIn { .. } | Event::WentOut { .. })
    });
    assert!(
        matches!(&event, Event::CameIn { name, .. } if name == "波波"),
        "新来的人落在我的频道，该是「进来了」：{event:?}"
    );

    bob.create_channel("隔壁", 0);
    let room = {
        let deadline = std::time::Instant::now() + WAIT;
        loop {
            if let Some(id) = bob
                .roster()
                .channels
                .values()
                .find(|c| c.name == "隔壁")
                .map(|c| c.id)
            {
                break id;
            }
            assert!(std::time::Instant::now() < deadline, "频道没建出来");
            std::thread::sleep(Duration::from_millis(20));
        }
    };
    bob.join_channel(room);
    let event = wait_for(&alice_events, |e| {
        matches!(e, Event::CameIn { .. } | Event::WentOut { .. })
    });
    assert!(
        matches!(&event, Event::WentOut { name, .. } if name == "波波"),
        "{event:?}"
    );

    let root = bob.roster().root().unwrap();
    bob.join_channel(root);
    let event = wait_for(&alice_events, |e| {
        matches!(e, Event::CameIn { .. } | Event::WentOut { .. })
    });
    assert!(
        matches!(&event, Event::CameIn { name, .. } if name == "波波"),
        "{event:?}"
    );

    bob.disconnect();
    let event = wait_for(&alice_events, |e| {
        matches!(e, Event::CameIn { .. } | Event::WentOut { .. })
    });
    assert!(
        matches!(&event, Event::WentOut { name, .. } if name == "波波"),
        "{event:?}"
    );
}
