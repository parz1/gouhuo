// SPDX-License-Identifier: GPL-3.0-or-later

//! 连上之后：事件泵、状态轮询、断线恢复提示、提示音、名单刷新和诊断文本。

use super::*;

/// 多久去问一次语音链路的状态。
///
/// 定这个数的是**电平条**：200 ms 的条子看着是一跳一跳的，用户会以为卡了，
/// 而它恰恰是用来判断「麦克风有没有在动」的，跟不上就失去了意义。
/// 说话指示也跟着受益。
///
/// 每秒 20 次读几个原子变量加两次加锁，跟音频线程各自每秒 100 次比
/// 完全不在一个量级上。
pub(crate) const VOICE_POLL: std::time::Duration = std::time::Duration::from_millis(50);

/// 窗口看不见（最小化、收在托盘里）时，界面同步多久一次。
///
/// 这时候电平条、谁在说话都没人看，只剩托盘图标要跟着麦克风状态走，以及发现窗口
/// 被恢复了、切回 [`VOICE_POLL`]。半秒够：恢复窗口后电平条最多晚半秒动起来。
///
/// 为什么要管这个：篝火真正的用法是进游戏之后挂在后台几个小时。实测（#11）改之前
/// 界面线程不管窗口在不在都每秒醒 190 次左右，跟游戏抢的就是这种零碎的调度。
pub(crate) const BACKGROUND_POLL: std::time::Duration = std::time::Duration::from_millis(500);

pub(crate) fn on_connected(
    app: &App,
    state: &Arc<Mutex<State>>,
    client: Client,
    events: Receiver<Event>,
    nick: &str,
    server: join::Known,
) {
    let presented_at = std::time::Instant::now();
    ui_timing::mark_frame(ui_timing::FramePhase::CallEnter);
    app.invoke_reset_call_view();
    // 服务端可能改过昵称（重名会加后缀），以它给的为准。
    let actual = {
        let roster = client.roster();
        roster.name_of(roster.me)
    };
    app.set_nick(if actual.is_empty() {
        nick.into()
    } else {
        actual.into()
    });

    {
        // 连上了才存 —— 存一个连不上的服务器只会让下次打开就看到一个错误。
        // 不管是粘链接、输域名还是输 IP 核对指纹进来的，存下来都是同一种东西
        // （地址 + 固定的指纹 + 加入码），下次从首页一点就进。
        let mut locked = state.lock().expect("state poisoned");
        locked.history_generation = 1;
        locked.record_connection(
            client_process::history::Phase::Connected,
            None,
            Some(client.session_id()),
            None,
        );
        locked.client = Some(client.clone());
        locked.call.connected();
        locked.call.set_notice("");
        locked.recovery = client_runtime::health::CallHealth::default();
        locked.recovery_epoch = Some(std::time::Instant::now());
        locked.recovery_history.clear();
        locked.settings.nick = app.get_nick().to_string();
        locked.settings.remember_server(
            &server.invite,
            &server.name,
            server.page.as_deref(),
            unix_now(),
        );
        locked.persist_settings();
        locked.hero = None;
        locked.pending = None;
    }
    // 频道树顶上那一行：加入页给的名字，没有就是地址。
    app.set_server_name(server.title().into());
    // 首页收拾干净，离开频道回来时是它平常的样子。
    app.set_home_mode(0);
    app.set_address_input("".into());
    app.set_address_hint("".into());
    app.set_address_bad(false);
    app.set_join_code("".into());
    clear_join_error(app);
    refresh_home(app, state);
    app.set_member_id(-1);
    if let Some(runtime) = current_runtime(state) {
        runtime.set_self_state(false, false);
        runtime.set_transmitting(false);
        runtime.set_monitoring(false);
        runtime.clear_volumes();
    }
    refresh(app, state, &client);

    start_voice(app, state, &client);
    ui_timing::record("join result -> call state", presented_at.elapsed());
    pump_events(app.as_weak(), Arc::clone(state), client, events);
}

/// 手动生成快照；不包含邀请凭据、身份密钥或聊天内容。
pub(crate) fn diagnostics(app: &App, state: &Arc<Mutex<State>>) -> String {
    let locked = state.lock().expect("state poisoned");
    let output = app
        .get_render_devices()
        .row_data(app.get_render_index().max(0) as usize)
        .unwrap_or_default();
    let mut text = format!(
        "篝火 {} / 协议 {} / {}
连接：{}
麦克风：{}
输出选择：{}
闭麦：{} / 关闭声音：{}
",
        env!("CARGO_PKG_VERSION"),
        protocol::control::PROTOCOL_VERSION,
        std::env::consts::OS,
        if app.get_connected() {
            "已连接"
        } else {
            "未连接"
        },
        app.get_capture_in_use(),
        output,
        app.get_self_muted(),
        app.get_self_deafened()
    );
    if let Some(runtime) = locked.runtime.as_ref() {
        text.push_str(&format!(
            "声音内核：{} / IPC {} / PID {:?}\n",
            runtime.engine_version(),
            client_runtime::ipc::VERSION,
            runtime.process_id()
        ));
    }
    if let Some(snapshot) = locked.runtime.as_ref().map(|r| r.snapshot()) {
        let progress = &snapshot.audio_progress;
        text.push_str(&format!(
            "音频处理：{:?}\n采集帧：{} / 距上次采集：{} ms / 停滞：{}\n播放帧：{} / 距上次播放：{} ms / 停滞：{}\n",
            snapshot.processor, progress.capture_frames, progress.capture_age_ms, progress.capture_stalled,
            progress.render_frames, progress.render_age_ms, progress.render_stalled
        ));
    }
    if let Some(stats) = locked.runtime.as_ref().and_then(|r| r.snapshot().voice) {
        for speaker in &stats.receive_quality {
            let q = &speaker.quality;
            text.push_str(&format!("接收语音会话 {}：{:?}（最近10秒，接收估计）\n播放缺帧：{:.1}% / 最长连续缺帧：{} / 迟到：{} / 乱序：{}\nPLC：{:.1}% / 等待帧：{} / 缓冲溢出丢弃：{} / 过旧帧丢弃：{}\n到达间隔抖动峰值：{:.1} ms / 本地缓冲跨度：{:.1} ms / 目标缓冲：{:.1} ms\n",
                speaker.session, q.level, q.missing_percent, q.max_loss_burst, q.late, q.reordered, q.plc_percent, q.waits, q.overflow, q.stale_dropped, q.jitter_ms, q.buffered_ms, q.target_ms));
        }
        text.push_str(&format!(
            "UDP：{} / RTT：{:.1} ms
发送包：{} / 收到包：{} / 播放等待：{}
输入电平：{:.1} dB
",
            if stats.udp_ok {
                "通"
            } else if stats.udp_failed {
                "超时未连通"
            } else {
                "探测中"
            },
            stats.rtt_ms,
            stats.packets_sent,
            stats.packets_received,
            stats.underruns,
            stats.input_db
        ));
    }
    if let Some(stats) = locked.runtime.as_ref().and_then(|r| r.snapshot().voice) {
        text.push_str(&format!(
            "传输路径：{} / TLS语音探测：{} / 过期或溢出丢帧：{}\n",
            if stats.tcp_active {
                "TLS兼容连接"
            } else {
                "UDP"
            },
            stats.tcp_ok,
            stats.tunnel_dropped
        ));
    }
    if let Some(client) = &locked.client {
        let (_, endpoint, _) = client.voice_endpoint_session();
        text.push_str(&format!("当前语音目标：{endpoint}\n"));
        text.push_str(&format!(
            "服务端最近报告的 UDP 接收数（含保活）：{}\n",
            client.server_udp_received()
        ));
    }
    text.push_str(&format!(
        "语音提示：{} {}
连接提示：{} {}
",
        app.get_voice_error(),
        app.get_voice_notice(),
        app.get_error_headline(),
        app.get_error_advice()
    ));
    if !locked.recovery_history.is_empty() {
        text.push_str("最近连接恢复记录（本次加入后的秒数）：\n");
        for entry in &locked.recovery_history {
            text.push_str(entry);
            text.push('\n');
        }
    }
    if let Some(history) = &locked.history {
        let snapshot = history.snapshot();
        text.push_str(&format!("默认连接历史：{} 条 / UTC Unix 毫秒 / 保留 7 天\n队列丢弃事件 {} / 磁盘故障次数 {} / 损坏文件 {} / 清除失败 {}\n",
            snapshot.records.len(), snapshot.dropped_queue_records, snapshot.disk_failures,
            snapshot.invalid_disk_documents, snapshot.clear_failed));
        for record in snapshot.records.iter().rev().take(20).rev() {
            text.push_str(&format!(
                "{} UTC / {:?} → {:?} / 代次 {} / 重试 {} / 会话 {:?} / 原因 {} / 来源 {:?}\n",
                record.utc_unix_ms,
                record.previous,
                record.phase,
                record.generation,
                record.attempt,
                record.session,
                record.cause.map(|c| c.reason.code()).unwrap_or("none"),
                record.cause.map(|c| c.source)
            ));
        }
    } else {
        text.push_str("默认连接历史不可用；通话仍可继续。\n");
    }
    text.push_str("本地 UI 阶段（事件处理耗时，非整帧耗时）：\n");
    for phase in ui_timing::snapshot() {
        text.push_str(&phase);
        text.push('\n');
    }
    if let Some(snapshot) = locked.runtime.as_ref().map(RuntimeHandle::snapshot) {
        text.push_str(&format!("语音后台阶段：{:?}\n", snapshot.timings));
    }
    if let Some(error) = locked
        .settings_writer
        .as_ref()
        .and_then(SettingsWriterHandle::last_error)
    {
        text.push_str(&format!("设置保存失败：{error}\n"));
    }
    text
}

/// 定时把音频状态搬到界面上。**整个程序只有一个**，连着和没连着都靠它。
///
/// 用 Slint 自己的定时器而不是线程：它就在界面线程上跑，省掉一次跨线程投递，
/// 而这件事每秒要做二十次。
pub(crate) fn spawn_status_poll(weak: slint::Weak<App>, state: Arc<Mutex<State>>) {
    let timer = slint::Timer::default();
    timer.start(slint::TimerMode::Repeated, VOICE_POLL, move || {
        let Some(app) = weak.upgrade() else { return };
        poll_history(&app, &state);
        let (runtime, client, ptt_bound, call) = {
            let locked = state.lock().expect("state poisoned");
            (
                locked.runtime.clone(),
                locked.client.clone(),
                locked.settings.ptt_key.is_some(),
                locked.call.clone(),
            )
        };
        let Some(runtime) = runtime else { return };
        let snapshot = runtime.snapshot();
        {
            let mut locked = state.lock().expect("state poisoned");
            locked.diagnostic_log.set_engine(
                &runtime.engine_version(),
                runtime.process_id(),
                runtime.needs_reauthentication(),
            );
            locked.diagnostic_log.sample(call.connection(), &snapshot);
        }
        let reconnecting = call.connection() == ConnectionState::Reconnecting;
        if let Some(client) = &client {
            client.set_voice_report(client_runtime::call::voice_report(snapshot.voice.as_ref()));
        }
        let mut view = CallViewModel::project_audio(&call, &snapshot, ptt_bound);
        SlintAdapter::apply_audio(&app, &view);
        update_tray(&view, client.as_ref());

        let ptt_needed = app.get_rebinding() || (client.is_some() && view.self_state.ptt_mode);
        PTT_TIMER.with(|slot| {
            if let Some(timer) = slot.borrow().as_ref() {
                if ptt_needed && !timer.running() {
                    timer.restart();
                } else if !ptt_needed && timer.running() {
                    timer.stop();
                }
            }
        });
        let hidden = !app.window().is_visible() || app.window().is_minimized();
        if !hidden && fire_pace(&app).is_some() {
            FIRE_TIMER.with(|slot| {
                if let Some(timer) = slot.borrow().as_ref() {
                    if !timer.running() {
                        timer.restart();
                    }
                }
            });
        }
        let idle = snapshot.stage == RuntimeStage::Idle;
        let interval = if hidden || idle {
            BACKGROUND_POLL
        } else {
            VOICE_POLL
        };
        VOICE_TIMER.with(|slot| {
            if let Some(timer) = slot.borrow().as_ref() {
                if timer.interval() != interval {
                    timer.set_interval(interval);
                }
            }
        });
        // Health policy has no Slint/window dependencies and runs in the tray too.
        if client.is_some() {
            let health = {
                let mut locked = state.lock().expect("state poisoned");
                let now = locked
                    .recovery_epoch
                    .get_or_insert_with(std::time::Instant::now)
                    .elapsed();
                locked.recovery.tick(now, &snapshot, reconnecting)
            };
            if let Some(notice) = health.notice {
                let call = {
                    let mut locked = state.lock().expect("state poisoned");
                    locked.call.set_notice(notice);
                    locked.call.clone()
                };
                view = CallViewModel::project_audio(&call, &snapshot, ptt_bound);
                SlintAdapter::apply_audio(&app, &view);
                update_tray(&view, client.as_ref());
            }
            if let Some(action) = &health.action {
                let mut reason = format!(
                    "{action:?}; stage={:?}; preparation={:?}",
                    snapshot.stage, snapshot.error
                );
                if let Some(stats) = &snapshot.voice {
                    reason.push_str(&format!(
                        "; udp_failed={}; capture={:?}; render={:?}; transport={:?}",
                        stats.udp_failed,
                        stats.capture_error,
                        stats.render_error,
                        stats.transport_error
                    ));
                }
                record_recovery(&state, &reason);
            }
            match health.action {
                Some(recovery::Action::Lost) => connection_notice(&state, false),
                Some(recovery::Action::Recovered) => connection_notice(&state, true),
                Some(recovery::Action::RetryVoice) => {
                    if runtime.needs_reauthentication() {
                        if let Some(client) = &client {
                            client.reconnect_transport();
                        }
                    } else {
                        restart_voice(&app, &state);
                    }
                    return;
                }
                Some(recovery::Action::Reconnect) => {
                    if let Some(client) = &client {
                        client.reconnect_transport();
                    }
                    return;
                }
                None => {}
            }
        }
        if !hidden {
            SlintAdapter::apply_activity(&app, &view);
        }
    });
    VOICE_TIMER.with(|slot| *slot.borrow_mut() = Some(timer));
}

pub(crate) fn pump_events(
    weak: slint::Weak<App>,
    state: Arc<Mutex<State>>,
    client: Client,
    events: Receiver<Event>,
) {
    std::thread::spawn(move || {
        // `recv` 在通道另一端被丢掉时返回 Err，循环自然结束 ——
        // 不需要额外的关闭信号。
        while let Ok(event) = events.recv() {
            let client = client.clone();
            let state = Arc::clone(&state);
            let posted = weak.upgrade_in_event_loop(move |app| {
                if !state
                    .lock()
                    .expect("state poisoned")
                    .client
                    .as_ref()
                    .is_some_and(|c| c.is_same(&client))
                {
                    return;
                }
                match event {
                    Event::Reconnecting {
                        attempt,
                        reason,
                        cause,
                        retry_in,
                        session,
                    } => {
                        {
                            let mut locked = state.lock().expect("state poisoned");
                            locked.history_attempt = attempt;
                            locked.record_connection(
                                client_process::history::Phase::ReconnectWaiting,
                                Some(cause),
                                Some(session),
                                Some(retry_in),
                            );
                        }
                        ui_timing::mark_frame(ui_timing::FramePhase::Reconnecting);
                        record_recovery(
                            &state,
                            &format!("TCP reconnect attempt={attempt}: {reason}"),
                        );
                        // 旧链路的会话和密钥都作废了，发出去的声音服务端只会丢掉。
                        // 停掉它，别让用户以为自己还在被人听见 —— 麦克风指示灯也跟着灭。
                        let first = {
                            let mut locked = state.lock().expect("state poisoned");
                            let now = locked
                                .recovery_epoch
                                .get_or_insert_with(std::time::Instant::now)
                                .elapsed();
                            locked.call.reconnecting(attempt, reason);
                            locked.recovery.interrupt(now)
                        };
                        if let Some(voice) = current_voice(&state) {
                            voice.quiesce();
                        }
                        if first {
                            connection_notice(&state, false);
                        }
                        sync_audio(&app, &state);
                    }
                    Event::Reconnected { session } => {
                        {
                            let mut locked = state.lock().expect("state poisoned");
                            locked.history_generation += 1;
                            locked.record_connection(
                                client_process::history::Phase::Restored,
                                None,
                                Some(session),
                                None,
                            );
                        }
                        ui_timing::mark_frame(ui_timing::FramePhase::Reconnected);
                        record_recovery(&state, "TCP connected; waiting for voice probe");
                        {
                            let mut locked = state.lock().expect("state poisoned");
                            locked.call.connected();
                            locked.call.set_notice("服务器已连接，正在验证语音…");
                        }
                        // 服务端可能又给改了名（重名加后缀），以它为准。
                        let actual = {
                            let roster = client.roster();
                            roster.name_of(roster.me)
                        };
                        if !actual.is_empty() {
                            app.set_nick(actual.into());
                        }
                        app.set_member_id(-1);
                        if let Some(runtime) = current_runtime(&state) {
                            runtime.clear_volumes();
                        }
                        refresh(&app, &state, &client);
                        // 会话 id、端口、密钥全换了，语音链路只能按新的重起。
                        start_voice(&app, &state, &client);
                    }
                    Event::Disconnected(ended) => {
                        let cause = match &ended {
                            Ended::Refused { cause, .. } => *cause,
                            Ended::ByUser => protocol::connection::ConnectionCause {
                                reason: protocol::connection::ConnectionReason::UserLeft,
                                source: protocol::connection::EvidenceSource::UserAction,
                            },
                        };
                        state.lock().expect("state poisoned").record_connection(
                            client_process::history::Phase::Ended,
                            Some(cause),
                            Some(client.session_id()),
                            None,
                        );
                        if matches!(&ended, Ended::Refused { .. }) {
                            connection_notice(&state, false);
                        }
                        let mut locked = state.lock().expect("state poisoned");
                        // 旧连接的收尾可能晚到：用户点了离开、马上又连了别的服务器，
                        // 这时候不能把新连接也一起拆了。
                        if !locked.client.as_ref().is_some_and(|c| c.is_same(&client)) {
                            return;
                        }
                        locked.client = None;
                        locked.call.offline();
                        let runtime = locked.runtime.clone();
                        let play_notice = matches!(&ended, Ended::Refused { .. });
                        drop(locked);
                        if let Some(runtime) = runtime {
                            runtime.quiesce();
                            if play_notice {
                                runtime.stop_after(std::time::Duration::from_millis(400));
                            } else {
                                runtime.stop();
                            }
                        }
                        app.invoke_reset_call_view();
                        sync_audio(&app, &state);
                        // 回到首页。「上次」那几个字要重算 —— 刚离开的这个现在是「刚刚」。
                        refresh_home(&app, &state);
                        match ended {
                            // 自己走的不是错误，别在首页上挂一条报错。
                            Ended::ByUser => clear_join_error(&app),
                            Ended::Refused {
                                headline, advice, ..
                            } => {
                                app.set_error_headline(headline.into());
                                app.set_error_advice(advice.into());
                                app.set_error_can_reverify(false);
                                // 收在托盘里的时候被踢了、被封了、被顶号了：把窗口叫出来，
                                // 不然用户以为自己还在频道里，一直对着空气说话。
                                let _ = app.show();
                                app.window().set_minimized(false);
                            }
                        }
                    }
                    Event::CameIn { name, .. } => announce(&state, Chime::CameIn, &name, false),
                    Event::WentOut { name, .. } => announce(&state, Chime::WentOut, &name, false),
                    // 其余的都只是「画面该变了」。
                    _ => refresh(&app, &state, &client),
                }
            });
            if posted.is_err() {
                // 窗口关了
                return;
            }
        }
    });
}

pub(crate) fn record_recovery(state: &Arc<Mutex<State>>, message: &str) {
    let mut locked = state.lock().expect("state poisoned");
    let elapsed = locked
        .recovery_epoch
        .map(|e| e.elapsed().as_secs())
        .unwrap_or(0);
    if locked.recovery_history.len() == 20 {
        locked.recovery_history.pop_front();
    }
    locked
        .recovery_history
        .push_back(format!("+{elapsed}s {message}"));
}

/// One local sound per outage and verified recovery, using the same AEC-aware mixer.
pub(crate) fn connection_notice(state: &Arc<Mutex<State>>, recovered: bool) {
    let locked = state.lock().expect("state poisoned");
    if locked.settings.cue_sounds {
        if let Some(runtime) = &locked.runtime {
            runtime.notice(
                if recovered {
                    Chime::Recovered
                } else {
                    Chime::Lost
                },
                None,
                true,
                locked.settings.cue_volume as f32 / 100.0,
            );
        }
    }
}

/// Local notices are synthesized and mixed with the AEC reference in the engine.
pub(crate) fn announce(state: &Arc<Mutex<State>>, kind: Chime, name: &str, preview: bool) {
    let locked = state.lock().expect("state poisoned");
    if locked.client.is_none() && !preview {
        return;
    }
    let Some(runtime) = &locked.runtime else {
        return;
    };
    runtime.notice(
        kind,
        (locked.settings.announce_names || preview).then(|| name.to_owned()),
        locked.settings.cue_sounds || preview,
        locked.settings.cue_volume as f32 / 100.0,
    );
}

/// 重画左边的树和右边的聊天。
///
/// 每次事件投影完整业务快照；适配器保持模型身份，只通知变化的行。
pub(crate) fn refresh(app: &App, state: &Arc<Mutex<State>>, client: &Client) {
    // Never nest the State and Roster locks; transport events can take either.
    let (settings, call, runtime) = {
        let locked = state.lock().expect("state poisoned");
        (
            locked.settings.clone(),
            locked.call.clone(),
            locked.runtime.clone(),
        )
    };
    let Some(runtime) = runtime else { return };
    apply_volumes(state, client);
    let view = {
        let roster = client.roster();
        runtime.set_server_muted(roster.my_user().is_some_and(|me| me.server_muted));
        CallViewModel::project(
            &call,
            &runtime.snapshot(),
            settings.ptt_key.is_some(),
            Some(&roster),
            &settings.user_volumes,
        )
    };
    SlintAdapter::apply_call(app, &view);
    let here = SlintAdapter::scene_members(&view);
    refresh_campfire(app, state, view.channel_id, view.me, here);
}
