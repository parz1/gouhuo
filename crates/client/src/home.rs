// SPDX-License-Identifier: GPL-3.0-or-later

//! 首页：存着的服务器、输入地址、加入码、核对指纹，以及「正在加入 / 没能加入」。

use super::*;

#[derive(Clone)]
pub(crate) struct Hero {
    title: String,
    address: String,
    /// 柴堆的种子。
    seed: u32,
}

/// 首页上的每一个动作：回到存着的服务器、按输的地址加入、填加入码、核对指纹、
/// 取消、重试、移除。
///
/// 它们最后都落到同一个地方（[`begin_join`]）：把「要加入什么」交给后台线程去办，
/// 办到哪一步、卡在哪儿，再由 [`finish_join`] 搬回界面上。怎么办的见 `join.rs`。
pub(crate) fn wire_home(app: &App, identity: &Rc<Identity>, state: &Arc<Mutex<State>>) {
    let begin: Rc<dyn Fn(join::Request)> = {
        let weak = app.as_weak();
        let identity = Rc::clone(identity);
        let state = Arc::clone(state);
        Rc::new(move |request| {
            if let Some(app) = weak.upgrade() {
                begin_join(&app, &identity, &state, request);
            }
        })
    };
    let check: Rc<dyn Fn(join::Request)> = {
        let weak = app.as_weak();
        let identity = Rc::clone(identity);
        let state = Arc::clone(state);
        Rc::new(move |request| {
            if let Some(app) = weak.upgrade() {
                begin_check(&app, &identity, &state, request);
            }
        })
    };

    {
        // 点链接启动、别的实例转交过来的链接。
        let begin = Rc::clone(&begin);
        app.on_join_link(move |link| {
            begin(join::Request::Address {
                text: link.to_string(),
                fresh: false,
            });
        });
    }

    {
        let begin = Rc::clone(&begin);
        let state = Arc::clone(state);
        app.on_join_saved(move |id| {
            let saved = state
                .lock()
                .expect("state poisoned")
                .settings
                .servers
                .get(id.max(0) as usize)
                .and_then(join::Known::from_saved);
            if let Some(known) = saved {
                begin(join::Request::Known(known));
            }
        });
    }

    {
        let weak = app.as_weak();
        let state = Arc::clone(state);
        app.on_address_edited(move |text| {
            if let Some(app) = weak.upgrade() {
                // 还在输的时候，空的不算错 —— 不然光标一进框就是一行红字。
                if text.trim().is_empty() {
                    app.set_address_hint("".into());
                    app.set_address_bad(false);
                } else {
                    check_address(&app, &state, &text);
                }
            }
        });
    }

    {
        let begin = Rc::clone(&begin);
        let weak = app.as_weak();
        let state = Arc::clone(state);
        app.on_join_address(move || {
            let Some(app) = weak.upgrade() else { return };
            let text = app.get_address_input().to_string();
            // 认不出来的东西不拿去连：原因就写在框下面，改了再点。
            if check_address(&app, &state, &text) {
                begin(join::Request::Address { text, fresh: false });
            }
        });
    }

    {
        let begin = Rc::clone(&begin);
        let weak = app.as_weak();
        let state = Arc::clone(state);
        app.on_submit_code(move || {
            let Some(app) = weak.upgrade() else { return };
            let code = app.get_join_code().trim().to_string();
            let pending = state.lock().expect("state poisoned").pending.clone();
            let (Some(mut known), false) = (pending, code.is_empty()) else {
                return;
            };
            known.invite.code = Some(code);
            begin(join::Request::Known(known));
        });
    }

    {
        // 用户核对过指纹了：从这一刻起它就是固定下来的，跟邀请链接里带的一样。
        let begin = Rc::clone(&begin);
        let check = Rc::clone(&check);
        let state = Arc::clone(state);
        app.on_trust_server(move || {
            let (pending, for_check) = {
                let locked = state.lock().expect("state poisoned");
                (locked.pending.clone(), locked.check_after_trust)
            };
            if let Some(known) = pending {
                if for_check {
                    check(join::Request::Known(known));
                } else {
                    begin(join::Request::Known(known));
                }
            }
        });
    }

    {
        // 网络体检：现在框里有地址就测它；没有就测上面那一大块说的那台。
        let check = Rc::clone(&check);
        let weak = app.as_weak();
        let state = Arc::clone(state);
        app.on_check_network(move || {
            let Some(app) = weak.upgrade() else { return };
            let text = app.get_address_input().to_string();
            let request = if app.get_home_mode() == 1 && !text.trim().is_empty() {
                if !check_address(&app, &state, &text) {
                    return;
                }
                Some(join::Request::Address { text, fresh: false })
            } else {
                let locked = state.lock().expect("state poisoned");
                let hero = app.get_hero_id();
                if hero >= 0 {
                    locked
                        .settings
                        .servers
                        .get(hero as usize)
                        .and_then(join::Known::from_saved)
                        .map(join::Request::Known)
                } else {
                    locked.last_request.clone()
                }
            };
            if let Some(request) = request {
                check(request);
            }
        });
    }

    {
        let weak = app.as_weak();
        let state = Arc::clone(state);
        app.on_cancel_check(move || {
            let Some(app) = weak.upgrade() else { return };
            {
                let mut locked = state.lock().expect("state poisoned");
                locked.check_generation += 1;
                if let Some(cancel) = locked.check_cancel.take() {
                    cancel.store(true, std::sync::atomic::Ordering::Relaxed);
                }
                locked.check_after_trust = false;
            }
            app.set_checking(false);
            app.set_home_mode(0);
            refresh_home(&app, &state);
        });
    }

    {
        let weak = app.as_weak();
        let state = Arc::clone(state);
        app.on_close_check(move || {
            let Some(app) = weak.upgrade() else { return };
            app.set_home_mode(0);
            refresh_home(&app, &state);
        });
    }

    {
        let weak = app.as_weak();
        let state = Arc::clone(state);
        app.on_save_check(move || {
            let Some(app) = weak.upgrade() else { return };
            let Some(report) = state.lock().expect("state poisoned").check_report.clone() else {
                return;
            };
            app.set_check_saved("正在保存…".into());
            let weak = weak.clone();
            std::thread::spawn(move || {
                let status = match diagnostic_log::save_text("netcheck", &report) {
                    Ok(path) => format!("已保存：{}", path.display()),
                    Err(error) => format!("保存失败：{error}"),
                };
                let _ = weak.upgrade_in_event_loop(move |app| app.set_check_saved(status.into()));
            });
        });
    }

    {
        let begin = Rc::clone(&begin);
        let state = Arc::clone(state);
        app.on_retry_join(move || {
            let last = state.lock().expect("state poisoned").last_request.clone();
            if let Some(request) = last {
                begin(request);
            }
        });
    }

    {
        // 证书对不上：重新去取这台服务器的指纹。有加入页就问加入页（CA 证书担保，
        // 不用用户核对）；没有就直接取，取回来照样要用户核对 —— 服务器重装了
        // 和有人在中间，从这边看是一模一样的。
        let begin = Rc::clone(&begin);
        let state = Arc::clone(state);
        app.on_reverify_server(move || {
            let last = state.lock().expect("state poisoned").last_request.clone();
            let text = match last {
                Some(join::Request::Known(known)) => known.page.unwrap_or_else(|| {
                    let host = client_core::address::bracketed(&known.invite.host);
                    if known.invite.port == protocol::DEFAULT_PORT {
                        host
                    } else {
                        format!("{host}:{}", known.invite.port)
                    }
                }),
                Some(join::Request::Address { text, .. }) => text,
                None => return,
            };
            begin(join::Request::Address { text, fresh: true });
        });
    }

    {
        let weak = app.as_weak();
        let state = Arc::clone(state);
        app.on_cancel_join(move || {
            let Some(app) = weak.upgrade() else { return };
            {
                let mut locked = state.lock().expect("state poisoned");
                locked.record_connection(
                    client_process::history::Phase::Ended,
                    Some(protocol::connection::ConnectionCause {
                        reason: protocol::connection::ConnectionReason::JoinCancelled,
                        source: protocol::connection::EvidenceSource::UserAction,
                    }),
                    None,
                    None,
                );
                // 后台那次连接掐不掉，只能不认它的结果，见 State::join_generation。
                locked.join_generation += 1;
                locked.pending = None;
                locked.check_after_trust = false;
                locked.hero = None;
                locked.call.offline();
            }
            sync_audio(&app, &state);
            app.set_connect_stage("".into());
            app.set_home_mode(0);
            app.set_join_code("".into());
            app.set_code_rejected(false);
            clear_join_error(&app);
            refresh_home(&app, &state);
        });
    }

    {
        let weak = app.as_weak();
        let state = Arc::clone(state);
        app.on_forget_server(move |id| {
            {
                let mut locked = state.lock().expect("state poisoned");
                locked.settings.forget_server(id.max(0) as usize);
                locked.persist_settings();
            }
            if let Some(app) = weak.upgrade() {
                refresh_home(&app, &state);
            }
        });
    }

    {
        let weak = app.as_weak();
        app.on_home_fire_resized(move |width, height| {
            let Some(app) = weak.upgrade() else { return };
            let frame = HOME_FIRE.with(|stage| {
                let mut stage = stage.borrow_mut();
                stage.set_size((width, height)).then(|| stage.still())
            });
            if let Some(frame) = frame {
                app.set_home_fire(frame);
            }
        });
    }
}

/// 认一下地址框里的东西，把结果写到框下面那行字上。认得出来返回 `true`。
pub(crate) fn check_address(app: &App, state: &Arc<Mutex<State>>, text: &str) -> bool {
    let saved = state
        .lock()
        .expect("state poisoned")
        .settings
        .servers
        .clone();
    let described = join::describe(text, &saved);
    app.set_address_bad(described.is_err());
    match described {
        Ok(hint) => {
            app.set_address_hint(hint.into());
            true
        }
        Err(problem) => {
            app.set_address_hint(problem.into());
            false
        }
    }
}

pub(crate) fn clear_join_error(app: &App) {
    app.set_error_headline("".into());
    app.set_error_advice("".into());
    app.set_error_can_reverify(false);
}

/// 柴堆的种子：按服务器的证书指纹算，所以每个服务器门口是不一样的一堆柴。
pub(crate) fn fire_seed(invite: &protocol::Invite) -> u32 {
    let f = invite.cert.0;
    u32::from_le_bytes([f[0], f[1], f[2], f[3]])
}

pub(crate) fn hero_of(known: &join::Known) -> Hero {
    Hero {
        title: known.title(),
        address: known.address(),
        seed: fire_seed(&known.invite),
    }
}

/// 开始加入。连接会阻塞（域名解析、HTTPS、TCP、TLS 握手，最长十几秒），
/// **不能在界面线程上做** —— 否则窗口会白到超时为止，用户以为程序死了。
pub(crate) fn begin_join(
    app: &App,
    identity: &Rc<Identity>,
    state: &Arc<Mutex<State>>,
    request: join::Request,
) {
    if call_connection(state) == ConnectionState::Connecting {
        return;
    }
    let nick = {
        let typed = app.get_nick().to_string();
        let trimmed = typed.trim().to_string();
        if trimmed.is_empty() {
            default_nick()
        } else {
            trimmed
        }
    };
    // 身份只有一份，不能移进后台线程。导出再导入拿一份副本 ——
    // 这条路径本来就要能跑（换机器就是靠它），顺手也验了一次。
    let Ok(identity) = Identity::import(&identity.export()) else {
        return;
    };

    // 上面那一大块先换成「正在加入谁」。输的是地址的话，这时候还不知道名字，
    // 先显示地址；**绝不显示原文** —— 邀请链接和 `#code=` 里都可能带着加入码。
    let hero = match &request {
        join::Request::Known(known) => hero_of(known),
        join::Request::Address { text, .. } => {
            use client_core::address::{display_address, parse, Target};
            let shown = match parse(text, join::allow_loopback_http()) {
                Ok(Target::Invite(invite)) => display_address(&invite.host, invite.port),
                Ok(Target::Page(page)) => page.host,
                Ok(Target::Host(host)) => display_address(&host.host, host.port_or_default()),
                Err(_) => "…".to_string(),
            };
            Hero {
                title: shown.clone(),
                address: shown,
                seed: 0,
            }
        }
    };

    let (generation, saved) = {
        let mut locked = state.lock().expect("state poisoned");
        locked.join_generation += 1;
        locked.history_journey = locked
            .history
            .as_ref()
            .and_then(|handle| handle.new_journey().ok());
        locked.history_generation = 0;
        locked.history_attempt = 0;
        locked.record_connection(client_process::history::Phase::Preparing, None, None, None);
        locked.pending = None;
        locked.last_request = Some(request.clone());
        locked.hero = Some(hero);
        locked.call.begin_join();
        (locked.join_generation, locked.settings.servers.clone())
    };
    sync_audio(app, state);
    app.set_connect_stage("正在准备…".into());
    app.set_home_mode(0);
    app.set_code_rejected(false);
    clear_join_error(app);
    refresh_home(app, state);

    let weak = app.as_weak();
    let state = Arc::clone(state);
    std::thread::spawn(move || {
        // 「现在在干什么」搬回界面线程。已经被取消了的那一次就别再往界面上写了。
        let progress = {
            let weak = weak.clone();
            let state = Arc::clone(&state);
            move |stage: &str, server: Option<&join::Known>| {
                let stage = stage.to_string();
                let hero = server.map(hero_of);
                let state = Arc::clone(&state);
                let _ = weak.upgrade_in_event_loop(move |app| {
                    {
                        let mut locked = state.lock().expect("state poisoned");
                        if locked.join_generation != generation {
                            return;
                        }
                        if let Some(hero) = hero {
                            locked.hero = Some(hero);
                        }
                    }
                    app.set_connect_stage(stage.into());
                    refresh_home(&app, &state);
                });
            }
        };
        let outcome = join::run(request, &identity, &nick, &saved, &progress);
        let _ = weak.upgrade_in_event_loop(move |app| {
            finish_join(&app, &state, generation, outcome, &nick);
        });
    });
}

/// 后台那次加入有结果了：进去了、要用户核对指纹、要加入码，或者没成。
pub(crate) fn finish_join(
    app: &App,
    state: &Arc<Mutex<State>>,
    generation: u64,
    outcome: join::Outcome,
    nick: &str,
) {
    if state.lock().expect("state poisoned").join_generation != generation {
        // 用户已经取消了。要是偏偏连上了，得把它断掉 ——
        // 不然服务器上会多出一个谁也看不见、还开着麦的「我」。
        if let join::Outcome::Joined { client, .. } = outcome {
            client.disconnect();
        }
        return;
    }
    state.lock().expect("state poisoned").call.offline();
    sync_audio(app, state);
    app.set_connect_stage("".into());

    match outcome {
        join::Outcome::Joined {
            client,
            events,
            server,
        } => on_connected(app, state, client, events, nick, server),
        join::Outcome::ConfirmFingerprint(server) => {
            state.lock().expect("state poisoned").record_connection(
                client_process::history::Phase::AwaitingTrust,
                None,
                None,
                None,
            );
            app.set_trust_fingerprint(server.invite.cert.to_grouped_hex().into());
            app.set_home_mode(3);
            let mut locked = state.lock().expect("state poisoned");
            locked.hero = Some(hero_of(&server));
            locked.pending = Some(server);
        }
        join::Outcome::NeedCode { server, rejected } => {
            state.lock().expect("state poisoned").record_connection(
                client_process::history::Phase::AwaitingCode,
                Some(protocol::connection::ConnectionCause::server(
                    protocol::connection::ConnectionReason::InviteRequired,
                )),
                None,
                None,
            );
            app.set_join_code("".into());
            app.set_code_rejected(rejected);
            app.set_home_mode(2);
            let mut locked = state.lock().expect("state poisoned");
            locked.hero = Some(hero_of(&server));
            locked.pending = Some(server);
        }
        join::Outcome::Failed(failure) => {
            state.lock().expect("state poisoned").record_connection(
                client_process::history::Phase::Ended,
                Some(failure.cause),
                None,
                None,
            );
            app.set_error_headline(failure.headline.into());
            app.set_error_advice(failure.advice.into());
            app.set_error_can_reverify(failure.certificate_changed);
            app.set_home_mode(0);
            let mut locked = state.lock().expect("state poisoned");
            let seed = locked.hero.as_ref().map(|h| h.seed).unwrap_or(0);
            locked.hero = Some(Hero {
                title: failure.title,
                address: failure.address,
                seed,
            });
        }
    }
    refresh_home(app, state);
}

/// 开始网络体检。跟 [`begin_join`] 一样在后台线程上做，结果由 [`finish_check`] 搬回来。
///
/// 先按加入的规矩认出是哪台服务器（邀请链接、加入页、存过的），指纹没人担保时
/// 同样停下来让用户核对 —— 体检也要把加入码发给对面，不能发给一台没核对过的机器。
pub(crate) fn begin_check(
    app: &App,
    identity: &Rc<Identity>,
    state: &Arc<Mutex<State>>,
    request: join::Request,
) {
    if call_connection(state) == ConnectionState::Connecting {
        return;
    }
    let Ok(identity) = Identity::import(&identity.export()) else {
        return;
    };
    let hero = match &request {
        join::Request::Known(known) => hero_of(known),
        join::Request::Address { text, .. } => {
            use client_core::address::{display_address, parse, Target};
            let shown = match parse(text, join::allow_loopback_http()) {
                Ok(Target::Invite(invite)) => display_address(&invite.host, invite.port),
                Ok(Target::Page(page)) => page.host,
                Ok(Target::Host(host)) => display_address(&host.host, host.port_or_default()),
                Err(_) => "…".to_string(),
            };
            Hero {
                title: shown.clone(),
                address: shown,
                seed: 0,
            }
        }
    };
    let cancel = Arc::new(std::sync::atomic::AtomicBool::new(false));
    let (generation, saved) = {
        let mut locked = state.lock().expect("state poisoned");
        locked.check_generation += 1;
        if let Some(previous) = locked.check_cancel.replace(Arc::clone(&cancel)) {
            previous.store(true, std::sync::atomic::Ordering::Relaxed);
        }
        locked.check_after_trust = false;
        locked.check_report = None;
        locked.pending = None;
        locked.hero = Some(hero);
        (locked.check_generation, locked.settings.servers.clone())
    };
    app.set_checking(true);
    app.set_check_stage("正在准备…".into());
    app.set_check_percent(0);
    app.set_check_headline("".into());
    app.set_check_text("".into());
    app.set_check_saved("".into());
    app.set_check_level(0);
    app.set_home_mode(4);
    clear_join_error(app);
    refresh_home(app, state);

    let weak = app.as_weak();
    let state = Arc::clone(state);
    std::thread::spawn(move || {
        let stage = {
            let weak = weak.clone();
            let state = Arc::clone(&state);
            move |text: String, percent: i32, server: Option<join::Known>| {
                let state = Arc::clone(&state);
                let _ = weak.upgrade_in_event_loop(move |app| {
                    {
                        let mut locked = state.lock().expect("state poisoned");
                        if locked.check_generation != generation {
                            return;
                        }
                        if let Some(server) = &server {
                            locked.hero = Some(hero_of(server));
                        }
                    }
                    app.set_check_stage(text.into());
                    app.set_check_percent(percent);
                    if server.is_some() {
                        refresh_home(&app, &state);
                    }
                });
            }
        };
        let located = join::locate(request, &saved, &|text, server| {
            stage(text.to_string(), 0, server.cloned())
        });
        let known = match located {
            Ok(join::Resolved::Vouched(known)) => known,
            Ok(join::Resolved::Unvouched(known)) => {
                let _ = weak.upgrade_in_event_loop(move |app| {
                    let mut locked = state.lock().expect("state poisoned");
                    if locked.check_generation != generation {
                        return;
                    }
                    locked.check_cancel = None;
                    locked.check_after_trust = true;
                    locked.hero = Some(hero_of(&known));
                    locked.pending = Some(known.clone());
                    drop(locked);
                    app.set_checking(false);
                    app.set_trust_fingerprint(known.invite.cert.to_grouped_hex().into());
                    app.set_home_mode(3);
                    refresh_home(&app, &state);
                });
                return;
            }
            Err(failure) => {
                let _ = weak.upgrade_in_event_loop(move |app| {
                    finish_check(
                        &app,
                        &state,
                        generation,
                        CheckOutcome::Failed {
                            headline: failure.headline,
                            advice: failure.advice,
                        },
                    );
                });
                return;
            }
        };
        stage("正在连接…".to_string(), 0, Some(known.clone()));
        let progress = |step: client_core::netcheck::Stage| {
            let percent = match &step {
                client_core::netcheck::Stage::Udp { percent, .. } => *percent as i32,
                client_core::netcheck::Stage::Done => 100,
                _ => 0,
            };
            stage(step.label(), percent, None);
        };
        let outcome = match client_core::netcheck::run(
            &known.invite,
            &identity,
            &client_core::netcheck::CheckPlan::default(),
            &progress,
            &cancel,
        ) {
            Ok(report) => CheckOutcome::Done(Box::new(report)),
            Err(client_core::netcheck::CheckError::Cancelled) => return,
            Err(client_core::netcheck::CheckError::Connect(error)) => CheckOutcome::Failed {
                headline: error.headline(),
                advice: error.advice(),
            },
        };
        let _ = weak.upgrade_in_event_loop(move |app| {
            finish_check(&app, &state, generation, outcome);
        });
    });
}

pub(crate) enum CheckOutcome {
    Done(Box<client_core::netcheck::CheckReport>),
    Failed { headline: String, advice: String },
}

pub(crate) fn finish_check(
    app: &App,
    state: &Arc<Mutex<State>>,
    generation: u64,
    outcome: CheckOutcome,
) {
    {
        let mut locked = state.lock().expect("state poisoned");
        if locked.check_generation != generation {
            return;
        }
        locked.check_cancel = None;
    }
    let (headline, text, level) = match outcome {
        CheckOutcome::Done(report) => {
            use client_core::netcheck::Verdict;
            let level = match report.verdict {
                Verdict::Good | Verdict::HighLatency => 1,
                Verdict::Fluctuating => 2,
                Verdict::Congested | Verdict::RateLimited | Verdict::UdpBlocked => 3,
                Verdict::Incomplete => 0,
            };
            (
                report.verdict.headline().to_string(),
                report.to_text(),
                level,
            )
        }
        CheckOutcome::Failed { headline, advice } => (
            headline,
            format!(
                "{advice}

连接都没建起来，UDP 没有测。先解决上面这个问题。"
            ),
            3,
        ),
    };
    state.lock().expect("state poisoned").check_report = Some(text.clone());
    app.set_checking(false);
    app.set_check_percent(100);
    app.set_check_headline(headline.into());
    app.set_check_text(text.into());
    app.set_check_level(level);
    app.set_home_mode(4);
    refresh_home(app, state);
}

/// 把存着的服务器摆到首页上：最近用的那个在上面那一大块，别的排在下面。
///
/// 每次都整个重建。最多二十行，而且只在加入、离开、移除的时候才调。
pub(crate) fn refresh_home(app: &App, state: &Arc<Mutex<State>>) {
    let (servers, hero) = {
        let locked = state.lock().expect("state poisoned");
        (locked.settings.servers.clone(), locked.hero.clone())
    };
    let now = unix_now();
    // (在存着的服务器里排第几, 怎么连, 上次什么时候来的)
    let known: Vec<(i32, join::Known, u64)> = servers
        .iter()
        .enumerate()
        .filter_map(|(index, saved)| {
            join::Known::from_saved(saved).map(|k| (index as i32, k, saved.last_used))
        })
        .collect();

    let (hero_id, title, address, used, seed) = match (&hero, known.first()) {
        (Some(hero), _) => {
            // 正在加入的要是正好是存着的某一个，下面那一排里就别再出现一次。
            let id = known
                .iter()
                .find(|(_, k, _)| k.address() == hero.address)
                .map(|(id, _, _)| *id)
                .unwrap_or(-1);
            (
                id,
                hero.title.clone(),
                hero.address.clone(),
                String::new(),
                hero.seed,
            )
        }
        (None, Some((id, recent, last_used))) => (
            *id,
            recent.title(),
            recent.address(),
            ago(now, *last_used),
            fire_seed(&recent.invite),
        ),
        (None, None) => (-1, String::new(), String::new(), String::new(), 0),
    };

    app.set_has_hero(hero.is_some() || !known.is_empty());
    app.set_has_servers(!known.is_empty());
    app.set_hero_id(hero_id);
    app.set_hero_name(title.into());
    app.set_hero_address(address.into());
    app.set_hero_used(used.into());

    let others: Vec<ServerRow> = known
        .iter()
        .filter(|(id, _, _)| *id != hero_id)
        .map(|(id, k, last_used)| ServerRow {
            id: *id,
            name: k.name.clone().into(),
            address: k.address().into(),
            used: ago(now, *last_used).into(),
        })
        .collect();
    app.set_other_servers(ModelRc::new(VecModel::from(others)));

    // 换了服务器就换一堆柴。
    let frame = HOME_FIRE.with(|stage| {
        let mut stage = stage.borrow_mut();
        stage.set_channel(seed).then(|| stage.still())
    });
    if let Some(frame) = frame {
        app.set_home_fire(frame);
    }
}
