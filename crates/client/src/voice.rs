// SPDX-License-Identifier: GPL-3.0-or-later

//! 声音内核的启动、设备列表、扫描、试麦和每人音量。音频本身在 gouhuo-voice 进程里。

use super::*;

/// 起语音链路，并开一个定时器把它的状态搬到界面上。
///
/// 设备打不开不该让人掉线 —— 文字和名单照样能用，只是没声音。所以这里
/// 失败只是把原因显示出来，不动连接。
pub(crate) fn start_voice(app: &App, state: &Arc<Mutex<State>>, client: &Client) {
    // A scan owns a microphone until its background function returns. Starting
    // a call cancels it and its completion resumes the latest session.
    if cancel_scan(state) {
        return;
    }
    let Some(runtime) = current_runtime(state) else {
        return;
    };
    let (session_id, server, keys) = client.voice_endpoint_session();
    let settings = state.lock().expect("state poisoned").settings.clone();
    runtime.set_mode(transmit_mode(&settings));
    // Replacements preserve local intent. Volumes are staged before admission
    // so a candidate cannot send with stale/default controls.
    apply_volumes(state, client);
    runtime.start_voice_with_tunnel(
        StartVoice {
            tcp_voice: false,
            tcp_active: false,
            host: server.ip().to_string(),
            udp_port: server.port(),
            session_id,
            upstream_key: *keys.upstream.as_bytes(),
            downstream_key: *keys.downstream.as_bytes(),
            devices: Devices {
                capture: settings.capture_device,
                render: settings.render_device,
            },
        },
        client.voice_tunnel(session_id),
    );
    sync_audio(app, state);
}

/// 把设备列表灌进下拉框，并记下「第 n 项是哪个 id」。
///
/// 第 0 项永远是「系统默认」—— 绝大多数人不该需要管这个，
/// 而且它是唯一在换了耳机之后还能跟着走的选项。
pub(crate) fn load_devices(app: &App, state: &Arc<Mutex<State>>) {
    let generation = {
        let mut locked = state.lock().expect("state poisoned");
        locked.device_generation = locked.device_generation.wrapping_add(1);
        if locked.capture_ids.is_empty() {
            locked.capture_ids.push(None);
            locked.render_ids.push(None);
            app.set_capture_devices(ModelRc::new(VecModel::from(vec!["系统默认".into()])));
            app.set_render_devices(ModelRc::new(VecModel::from(vec!["系统默认".into()])));
        }
        locked.device_generation
    };
    let weak = app.as_weak();
    let state = Arc::clone(state);
    std::thread::spawn(move || {
        let at = std::time::Instant::now();
        let (capture, render) = current_runtime(&state)
            .and_then(|runtime| runtime.devices().ok())
            .unwrap_or_default();
        let lists: Vec<_> = [capture, render]
            .into_iter()
            .map(|endpoints| {
                let mut labels = vec![String::from("系统默认")];
                let mut ids = vec![None];
                for endpoint in endpoints {
                    labels.push(format!(
                        "{}{}",
                        endpoint.name,
                        if endpoint.is_hardware {
                            ""
                        } else {
                            "（虚拟）"
                        }
                    ));
                    ids.push(Some(endpoint.id));
                }
                (labels, ids)
            })
            .collect();
        ui_timing::record("device enumeration (background)", at.elapsed());
        let _ = weak.upgrade_in_event_loop(move |app| {
            let mut locked = state.lock().expect("state poisoned");
            if generation != locked.device_generation {
                return;
            }
            let mut models = Vec::new();
            for (is_capture, (mut labels, mut ids)) in [true, false].into_iter().zip(lists) {
                let saved = if is_capture {
                    &locked.settings.capture_device
                } else {
                    &locked.settings.render_device
                };
                let index = selected_device_index(&mut labels, &mut ids, saved) as i32;
                let labels: Vec<SharedString> = labels.into_iter().map(Into::into).collect();
                models.push((is_capture, index, ModelRc::new(VecModel::from(labels))));
                if is_capture {
                    locked.capture_ids = ids;
                } else {
                    locked.render_ids = ids;
                }
            }
            drop(locked);
            for (is_capture, index, model) in models {
                if is_capture {
                    app.set_capture_devices(model);
                    app.set_capture_index(index);
                } else {
                    app.set_render_devices(model);
                    app.set_render_index(index);
                }
            }
        });
    });
}

/// 换设备。链路要重起 —— WASAPI 的流是绑在设备上的，换不了。
///
/// 重起会让声音断一下（几十毫秒）。这是换设备本来就该有的代价，
/// 比为了热切换在音频线程里加一套状态机划算得多。
pub(crate) fn restart_voice(app: &App, state: &Arc<Mutex<State>>) {
    if call_connection(state) == ConnectionState::Reconnecting {
        return;
    }
    if cancel_scan(state) {
        return;
    }
    if let Some(client) = current(state) {
        start_voice(app, state, &client);
    } else {
        start_mic_check(app, state);
    }
}

/// 挨个试每个麦克风，把结果显示出来。
///
/// 在后台线程上跑：每个设备都要真的打开一次，那几秒里界面不能是卡死的。
pub(crate) fn wire_scan(app: &App, state: &Arc<Mutex<State>>) {
    let state = Arc::clone(state);
    let weak = app.as_weak();
    app.on_scan_microphones(move || {
        let Some(app) = weak.upgrade() else { return };
        // A scan is an offline device operation. It must not compete with a
        // live call; changing a call's microphone uses the runtime instead.
        if app.get_scanning() || current(&state).is_some() {
            return;
        }
        stop_mic_check(&state);
        let draining = current_runtime(&state).map(|runtime| {
            let id = runtime.snapshot().request_id;
            (runtime, id)
        });
        let cancel = Arc::new(std::sync::atomic::AtomicBool::new(false));
        let generation = {
            let mut locked = state.lock().expect("state poisoned");
            locked.scan_generation = locked.scan_generation.wrapping_add(1);
            locked.scan_cancel = Some(Arc::clone(&cancel));
            locked.scan_generation
        };
        app.set_scanning(true);
        app.set_scan_results(ModelRc::new(VecModel::from(Vec::<ScanRow>::new())));
        let weak = app.as_weak();
        let state = Arc::clone(&state);
        std::thread::spawn(move || {
            let ready = draining.is_none_or(|(runtime, id)| {
                let deadline = std::time::Instant::now() + std::time::Duration::from_secs(3);
                loop {
                    if cancel.load(std::sync::atomic::Ordering::Acquire) {
                        return false;
                    }
                    let snapshot = runtime.snapshot();
                    if snapshot.request_id != id {
                        return false;
                    }
                    if snapshot.stage == RuntimeStage::Idle {
                        return true;
                    }
                    if std::time::Instant::now() >= deadline {
                        return false;
                    }
                    std::thread::sleep(std::time::Duration::from_millis(20));
                }
            });
            let results = if ready {
                current_runtime(&state)
                    .and_then(|runtime| runtime.scan(SCAN_PER_DEVICE, Arc::clone(&cancel)).ok())
                    .unwrap_or_default()
            } else {
                Vec::new()
            };
            // The scanner has dropped its capture on its own thread before
            // posting completion. Only now may the newest requested audio start.
            let _ = weak.upgrade_in_event_loop(move |app| {
                {
                    let mut locked = state.lock().expect("state poisoned");
                    if generation != locked.scan_generation {
                        return;
                    }
                    locked.scan_cancel = None;
                }
                let rows = results
                    .iter()
                    .map(|result| ScanRow {
                        name: format!(
                            "{}{}",
                            result.name,
                            if result.is_hardware {
                                ""
                            } else {
                                "（虚拟）"
                            }
                        )
                        .into(),
                        verdict: result.verdict.clone().into(),
                        ok: result.hears_something,
                        id: result.id.clone().into(),
                    })
                    .collect::<Vec<_>>();
                app.set_scan_results(ModelRc::new(VecModel::from(rows)));
                app.set_scanning(false);
                if let Some(client) = current(&state) {
                    if call_connection(&state) == ConnectionState::Connected {
                        start_voice(&app, &state, &client);
                    }
                } else if app.get_show_settings() {
                    start_mic_check(&app, &state);
                }
            });
        });
    });
}

/// 开一次独立试麦（没连服务器的时候用）。
pub(crate) fn start_mic_check(app: &App, state: &Arc<Mutex<State>>) {
    if current(state).is_some() {
        return;
    }
    if state.lock().expect("state poisoned").scan_cancel.is_some() {
        return;
    }
    let Some(runtime) = current_runtime(state) else {
        return;
    };
    let settings = state.lock().expect("state poisoned").settings.clone();
    runtime.start_mic_check(Devices {
        capture: settings.capture_device,
        render: settings.render_device,
    });
    sync_audio(app, state);
}

/// 停掉独立试麦。连服务器之前必须停 —— 不然麦克风会被采两遍。
pub(crate) fn stop_mic_check(state: &Arc<Mutex<State>>) {
    cancel_scan(state);
    // Closing Settings during a call keeps the call's runtime alive.
    if current(state).is_none() {
        if let Some(runtime) = current_runtime(state) {
            runtime.stop();
            runtime.set_monitoring(false);
        }
    }
}

pub(crate) fn cancel_scan(state: &Arc<Mutex<State>>) -> bool {
    if let Some(cancel) = &state.lock().expect("state poisoned").scan_cancel {
        cancel.store(true, std::sync::atomic::Ordering::Release);
        true
    } else {
        false
    }
}

/// 单人音量在设置里按什么存：公钥的 base32。
pub(crate) fn volume_key(public_key: &[u8]) -> String {
    protocol::base32::encode(public_key)
}

/// 把设置里存的单人音量推给语音链路。
///
/// 语音链路只认会话 id，而会话 id 每次连接都换，所以这件事要在
/// 「名单变了」和「链路重起了」两个时候都做一遍。给每个人都设一次，
/// 包括 100% 的 —— 会话 id 在服务端是会回绕复用的，不能指望新来的人
/// 身上没有旧设置。
pub(crate) fn apply_volumes(state: &Arc<Mutex<State>>, client: &Client) {
    let (voice, settings) = {
        let locked = state.lock().expect("state poisoned");
        let Some(voice) = locked.runtime.clone() else {
            return;
        };
        (voice, locked.settings.clone())
    };
    let roster = client.roster();
    for user in roster.users.values() {
        if user.session_id == roster.me {
            continue;
        }
        let percent = settings.user_volume(&volume_key(&user.public_key));
        voice.set_volume(user.session_id, percent as f32 / 100.0);
    }
}

/// Keep a missing saved selection visible; displaying "system default" would
/// misrepresent the fixed microphone policy and make re-selection ambiguous.
pub fn selected_device_index(
    labels: &mut Vec<String>,
    ids: &mut Vec<Option<String>>,
    selected: &Option<String>,
) -> usize {
    if let Some(index) = ids.iter().position(|id| id == selected) {
        return index;
    }
    labels.push("已选设备（当前不可用）".into());
    ids.push(selected.clone());
    ids.len() - 1
}

#[cfg(test)]
mod device_selection_tests {
    use super::*;

    #[test]
    fn missing_fixed_selection_stays_visible_and_does_not_select_default() {
        let mut labels = vec!["系统默认".into(), "麦克风".into()];
        let mut ids = vec![None, Some("present".into())];
        let missing = Some("unplugged".into());
        let index = selected_device_index(&mut labels, &mut ids, &missing);
        assert_ne!(index, 0);
        assert_eq!(ids[index], missing);
        assert!(labels[index].contains("不可用"));
        assert_eq!(
            selected_device_index(&mut labels, &mut ids, &missing),
            index
        );
        assert_eq!(ids.len(), 3);
        assert_eq!(selected_device_index(&mut labels, &mut ids, &None), 0);
        assert_eq!(
            selected_device_index(&mut labels, &mut ids, &Some("present".into())),
            1
        );
    }
}
