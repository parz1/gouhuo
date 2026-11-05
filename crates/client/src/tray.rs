// SPDX-License-Identifier: GPL-3.0-or-later

//! 托盘图标：建起来、跟着麦克风状态换图标和提示。

use super::*;

/// 建托盘图标，接上它的菜单。建不起来返回 `None`，不影响别的。
pub(crate) fn wire_tray(app: &App, state: &Arc<Mutex<State>>) -> Option<Tray> {
    let tray = match Tray::new() {
        Ok(tray) => tray,
        Err(e) => {
            eprintln!("托盘图标建不起来，点 × 只能退出：{e}");
            return None;
        }
    };
    tray.set_status("篝火 · 没连着".into());
    {
        let weak = app.as_weak();
        tray.on_show_window(move || {
            if let Some(app) = weak.upgrade() {
                let _ = app.show();
                app.window().set_minimized(false);
            }
        });
    }
    {
        let weak = app.as_weak();
        tray.on_toggle_mute(move || {
            if let Some(app) = weak.upgrade() {
                app.invoke_toggle_mute();
            }
        });
    }
    {
        let weak = app.as_weak();
        tray.on_toggle_deafen(move || {
            if let Some(app) = weak.upgrade() {
                app.invoke_toggle_deafen();
            }
        });
    }
    {
        let weak = app.as_weak();
        tray.on_leave(move || {
            if let Some(app) = weak.upgrade() {
                app.invoke_leave();
            }
        });
    }
    {
        let state = Arc::clone(state);
        tray.on_quit(move || quit_app(&state));
    }
    Some(tray)
}

/// 把连接和麦克风的状态搬到托盘图标上。跟着状态轮询的定时器一起跑。
pub(crate) fn update_tray(view: &AudioViewModel, client: Option<&Client>) {
    TRAY.with(|slot| {
        let slot = slot.borrow();
        let Some(tray) = slot.as_ref() else { return };
        let connected = matches!(
            view.connection,
            ConnectionState::Connected | ConnectionState::Reconnecting
        ) && client.is_some();
        let muted = view.self_muted;
        let deafened = view.self_state.deafened;
        let icon = match (connected, view.self_state.muted) {
            (false, _) => 0,
            (true, false) => 1,
            (true, true) => 2,
        };
        let status = match client {
            _ if !connected => "篝火 · 没连着".to_string(),
            _ if !view.reconnecting.is_empty() => "篝火 · 正在重连…".to_string(),
            _ if !view.voice_notice.is_empty() => format!("篝火 · {}", view.voice_notice),
            Some(client) => {
                let mic = if view.self_state.server_muted {
                    "被管理员闭麦"
                } else if deafened {
                    "关着耳朵"
                } else if muted {
                    "闭着麦"
                } else {
                    "麦克风开着"
                };
                format!("篝火 · 在「{}」里 · {mic}", current_channel_name(client))
            }
            None => "篝火".to_string(),
        };
        // 值没变就别设：定时器一秒二十次，每次都设会让托盘一直在重建图标。
        if tray.get_state() != icon {
            tray.set_state(icon);
        }
        if tray.get_status().as_str() != status {
            tray.set_status(status.into());
        }
        if tray.get_connected() != connected {
            tray.set_connected(connected);
        }
        if tray.get_muted() != muted {
            tray.set_muted(muted);
        }
        if tray.get_deafened() != deafened {
            tray.set_deafened(deafened);
        }
    });
}
