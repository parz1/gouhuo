// SPDX-License-Identifier: GPL-3.0-or-later

//! 设置页的接线：偏好、热键、按住说话轮询和连接历史。存储格式在 `settings.rs`。

use super::*;

/// 多久看一次按住说话的键有没有被按下。
///
/// 这个数直接决定「按下去到开始发声」的延迟，所以要比语音状态那个快得多。
/// 20 ms 是两帧音频，用户感觉不出来；再快就只是在空转了。
pub(crate) const PTT_POLL: std::time::Duration = std::time::Duration::from_millis(20);

/// 设置面板：切换说话方式、绑按住说话的键、试听麦克风、选设备。
pub(crate) fn wire_settings(app: &App, state: &Arc<Mutex<State>>, hotkeys: Option<Rc<Hotkeys>>) {
    {
        let state = Arc::clone(state);
        let weak = app.as_weak();
        app.on_set_diagnostic_recording(move |enabled| {
            state
                .lock()
                .expect("state poisoned")
                .diagnostic_log
                .set_enabled(enabled);
            if let Some(app) = weak.upgrade() {
                app.set_diagnostic_recording(enabled);
                app.set_diagnostic_export_status(
                    if enabled {
                        "时间线正在记录；可直接导出最近的故障经过。"
                    } else {
                        "时间线已暂停，已有记录保留，可以导出。"
                    }
                    .into(),
                );
            }
        });
    }
    {
        let state = Arc::clone(state);
        let weak = app.as_weak();
        app.on_export_diagnostics(move || {
            let report = state
                .lock()
                .expect("state poisoned")
                .diagnostic_log
                .report();
            let weak = weak.clone();
            std::thread::spawn(move || {
                let status = match diagnostic_log::export(&report) {
                    Ok(path) => format!("已导出：{}", path.display()),
                    Err(error) => format!("导出失败：{error}"),
                };
                let _ = weak.upgrade_in_event_loop(move |app| {
                    app.set_diagnostic_export_status(status.into())
                });
            });
        });
    }
    {
        let state = Arc::clone(state);
        let weak = app.as_weak();
        app.on_retry_voice(move || {
            if let Some(app) = weak.upgrade() {
                load_devices(&app, &state);
                restart_voice(&app, &state);
            }
        });
    }
    {
        let state = Arc::clone(state);
        let weak = app.as_weak();
        app.on_show_diagnostics(move || {
            if let Some(app) = weak.upgrade() {
                app.set_diagnostics(diagnostics(&app, &state).into());
                app.set_diagnostics_open(true);
            }
        });
    }
    {
        let state = Arc::clone(state);
        let weak = app.as_weak();
        app.on_set_cue_sounds(move |on| {
            let mut locked = state.lock().expect("state poisoned");
            locked.settings.cue_sounds = on;
            locked.persist_settings();
            if let Some(app) = weak.upgrade() {
                app.set_cue_sounds(on);
            }
        });
    }

    {
        let state = Arc::clone(state);
        let weak = app.as_weak();
        app.on_set_announce_names(move |on| {
            let mut locked = state.lock().expect("state poisoned");
            locked.settings.announce_names = on;
            locked.persist_settings();
            if let Some(app) = weak.upgrade() {
                app.set_announce_names(on);
            }
        });
    }

    {
        let state = Arc::clone(state);
        app.on_set_cue_volume(move |level| {
            state.lock().expect("state poisoned").settings.cue_volume =
                (level.clamp(0.0, 1.0) * 100.0).round() as u32;
        });
    }

    {
        let state = Arc::clone(state);
        app.on_save_cue_volume(move || {
            state.lock().expect("state poisoned").persist_settings();
        });
    }

    {
        let state = Arc::clone(state);
        let weak = app.as_weak();
        app.on_preview_cues(move || {
            let nick = weak
                .upgrade()
                .map(|app| app.get_nick().to_string())
                .unwrap_or_default();
            announce(&state, Chime::CameIn, &nick, true);
        });
    }

    {
        let state = Arc::clone(state);
        let weak = app.as_weak();
        app.on_pick_capture(move |index| {
            let Some(app) = weak.upgrade() else { return };
            {
                let mut locked = state.lock().expect("state poisoned");
                let Some(id) = locked.capture_ids.get(index as usize).cloned() else {
                    return;
                };
                locked.settings.capture_device = id;
                locked.persist_settings();
            }
            restart_voice(&app, &state);
        });
    }

    {
        // 从检测结果里点一行：直接按设备 id 选，不经过下拉框的序号。
        let state = Arc::clone(state);
        let weak = app.as_weak();
        app.on_pick_capture_id(move |id| {
            let Some(app) = weak.upgrade() else { return };
            let id = id.to_string();
            {
                let mut locked = state.lock().expect("state poisoned");
                locked.settings.capture_device = Some(id.clone());
                locked.persist_settings();
            }
            // 下拉框跟着走。对不上就保持原样 —— 真正生效的是上面存的 id，
            // 下拉框显示得对不对只是好看不好看的事。
            let index = state
                .lock()
                .expect("state poisoned")
                .capture_ids
                .iter()
                .position(|known| known.as_deref() == Some(id.as_str()));
            if let Some(index) = index {
                app.set_capture_index(index as i32);
            }
            restart_voice(&app, &state);
        });
    }

    {
        let state = Arc::clone(state);
        let weak = app.as_weak();
        app.on_pick_render(move |index| {
            let Some(app) = weak.upgrade() else { return };
            {
                let mut locked = state.lock().expect("state poisoned");
                let Some(id) = locked.render_ids.get(index as usize).cloned() else {
                    return;
                };
                locked.settings.render_device = id;
                locked.persist_settings();
            }
            restart_voice(&app, &state);
        });
    }

    {
        let state = Arc::clone(state);
        let weak = app.as_weak();
        app.on_toggle_monitor(move || {
            let Some(app) = weak.upgrade() else { return };
            let Some(runtime) = current_runtime(&state) else {
                return;
            };
            let on = !runtime.is_monitoring();
            runtime.set_monitoring(on);
            sync_audio(&app, &state);
        });
    }

    {
        let state = Arc::clone(state);
        let weak = app.as_weak();
        app.on_set_vad(move |level| {
            let mode = {
                let mut locked = state.lock().expect("state poisoned");
                locked.settings.vad_threshold_db = level_to_db(level);
                locked.persist_settings();
                transmit_mode(&locked.settings)
            };
            if let Some(voice) = current_runtime(&state) {
                voice.set_mode(mode);
            }
            if let Some(app) = weak.upgrade() {
                sync_audio(&app, &state);
            }
        });
    }

    {
        let weak = app.as_weak();
        let state = Arc::clone(state);
        app.on_refresh_history(move || {
            if let Some(app) = weak.upgrade() {
                refresh_history(&app, &state);
            }
        });
    }
    for clear in [false, true] {
        let weak = app.as_weak();
        let state = Arc::clone(state);
        let action = move || {
            if let Some(app) = weak.upgrade() {
                begin_history_operation(&app, &state, clear);
            }
        };
        if clear {
            app.on_clear_history(action);
        } else {
            app.on_export_history(action);
        }
    }
    {
        let weak = app.as_weak();
        let state = Arc::clone(state);
        app.on_settings_category_changed(move |category| {
            let Some(app) = weak.upgrade() else { return };
            if category == 0 {
                start_mic_check(&app, &state);
            } else {
                stop_mic_check(&state);
                sync_audio(&app, &state);
            }
            if category == 4 {
                refresh_history(&app, &state);
            }
        });
    }

    {
        let weak = app.as_weak();
        let state = Arc::clone(state);
        app.on_toggle_settings(move || {
            let Some(app) = weak.upgrade() else { return };
            let opening = !app.get_show_settings();
            app.set_show_settings(opening);
            if opening && app.get_settings_tab() == 0 {
                // 没连服务器也要能看电平、能试听 —— 这正是连不上时最想知道的事。
                start_mic_check(&app, &state);
            } else {
                // 关掉设置就把设备还回去。常驻几小时的软件不该一直占着麦克风，
                // 而且麦克风灯一直亮着会让人不安。
                stop_mic_check(&state);
                sync_audio(&app, &state);
            }
        });
    }

    {
        let state = Arc::clone(state);
        let weak = app.as_weak();
        app.on_set_ptt_mode(move |ptt| {
            let Some(app) = weak.upgrade() else { return };
            let mode = {
                let mut locked = state.lock().expect("state poisoned");
                locked.settings.talk_mode = if ptt {
                    TalkMode::PushToTalk
                } else {
                    TalkMode::VoiceActivity
                };
                locked.persist_settings();
                transmit_mode(&locked.settings)
            };
            // 链路不用重起，下一帧就按新方式走。
            if let Some(voice) = current_runtime(&state) {
                voice.set_mode(mode);
            }
            sync_audio(&app, &state);
        });
    }

    {
        let hotkeys = hotkeys.clone();
        let weak = app.as_weak();
        app.on_rebind_ptt(move || {
            let (Some(app), Some(hotkeys)) = (weak.upgrade(), hotkeys.as_ref()) else {
                return;
            };
            REBIND.with(|slot| *slot.borrow_mut() = Some(hotkeys.capture_next()));
            app.set_rebinding(true);
        });
    }

    {
        let weak = app.as_weak();
        app.on_cancel_rebind(move || {
            let Some(app) = weak.upgrade() else { return };
            if let Some(hotkeys) = &hotkeys {
                hotkeys.cancel_capture();
            }
            REBIND.with(|slot| *slot.borrow_mut() = None);
            app.set_rebinding(false);
        });
    }
}

/// 盯着按住说话的键，也顺便接住「正在设置热键」按下的那个键。
///
/// 轮询而不是回调：热键是在另一个线程上收到的，而改界面必须在界面线程上。
/// 每 20 ms 读一个原子变量，比每次按键都投递一次跨线程消息便宜得多 ——
/// 而按键在游戏里是每秒几十次的事。
pub(crate) fn spawn_ptt_poll(
    weak: slint::Weak<App>,
    state: Arc<Mutex<State>>,
    hotkeys: Rc<Hotkeys>,
) {
    let timer = slint::Timer::default();
    timer.start(slint::TimerMode::Repeated, PTT_POLL, move || {
        let Some(app) = weak.upgrade() else { return };

        // 用户正在设置热键？看看按下来没有。
        let captured = REBIND.with(|slot| slot.borrow().as_ref().and_then(|rx| rx.try_recv().ok()));
        if let Some(key) = captured {
            REBIND.with(|slot| *slot.borrow_mut() = None);
            hotkeys.set_ptt(Some(key));
            app.set_rebinding(false);
            app.set_ptt_label(key.label().into());
            let mut locked = state.lock().expect("state poisoned");
            locked.settings.ptt_key = Some(key);
            locked.persist_settings();
            return;
        }

        // 按住说话：把键的状态推给链路。
        if let Some(runtime) = current_runtime(&state) {
            CallController::set_ptt(&runtime, call_connection(&state), hotkeys.is_down());
        }
    });
    PTT_TIMER.with(|slot| *slot.borrow_mut() = Some(timer));
}

/// 设置里的说话方式翻译成链路那边的发送方式。
pub(crate) fn transmit_mode(settings: &Settings) -> TransmitMode {
    match settings.talk_mode {
        TalkMode::PushToTalk => TransmitMode::PushToTalk,
        TalkMode::VoiceActivity => TransmitMode::VoiceActivity {
            threshold_db: settings.vad_threshold_db,
        },
    }
}

pub(crate) fn refresh_history(app: &App, state: &Mutex<State>) {
    let handle = state.lock().expect("state poisoned").history.clone();
    let Some(handle) = handle else {
        app.set_history_available(false);
        app.set_history_summary("连接历史暂不可用".into());
        app.set_history_report("无法启动历史记录线程，通话仍可继续。".into());
        return;
    };
    let version = handle.view_version();
    let view = history_view::project(&handle.snapshot());
    app.set_history_available(true);
    app.set_history_summary(view.summary.into());
    app.set_history_report(view.report.into());
    app.set_history_warning(view.warning.into());
    state.lock().expect("state poisoned").history_view_version = Some(version);
}

pub(crate) fn begin_history_operation(app: &App, state: &Mutex<State>, clear: bool) {
    let mut locked = state.lock().expect("state poisoned");
    if locked.history_operation.is_some() {
        return;
    }
    let reply = locked.history.as_ref().and_then(|handle| {
        if clear {
            handle.request_clear()
        } else {
            handle.request_export()
        }
    });
    if let Some(reply) = reply {
        locked.history_operation = Some(reply);
        app.set_history_busy(true);
        app.set_history_status(
            if clear {
                "正在清除…"
            } else {
                "正在导出…"
            }
            .into(),
        );
    } else {
        app.set_history_status("操作未排入队列，请稍后再试。".into());
    }
}

pub(crate) fn poll_history(app: &App, state: &Mutex<State>) {
    use client_process::history::OperationResult;
    use std::sync::mpsc::TryRecvError;
    let (result, changed) = {
        let mut locked = state.lock().expect("state poisoned");
        let result =
            locked
                .history_operation
                .as_ref()
                .and_then(|receiver| match receiver.try_recv() {
                    Ok(result) => Some(Ok(result)),
                    Err(TryRecvError::Empty) => None,
                    Err(TryRecvError::Disconnected) => Some(Err(())),
                });
        if result.is_some() {
            locked.history_operation = None;
        }
        let changed = app.get_show_settings()
            && app.get_settings_tab() == 4
            && locked
                .history
                .as_ref()
                .is_some_and(|handle| Some(handle.view_version()) != locked.history_view_version);
        (result, changed)
    };
    let completed = result.is_some();
    if let Some(result) = result {
        app.set_history_busy(false);
        app.set_history_status(match result {
            Ok(OperationResult::Cleared) => "本机连接历史已清除。".into(),
            Ok(OperationResult::Exported(path)) => format!("已导出到 {}", path.display()).into(),
            Ok(OperationResult::Failed) => "操作未完成，请检查保存位置是否可写。".into(),
            Err(()) => "操作结果未确认，请重新查看历史。".into(),
        });
    }
    if completed || changed {
        refresh_history(app, state);
    }
}
