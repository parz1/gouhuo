// SPDX-License-Identifier: GPL-3.0-or-later

//! Offline UI fixture used only by the two preview examples. It owns no session,
//! audio device, identity, settings store or single-instance lock.

use std::{cell::RefCell, rc::Rc};

use slint::{ComponentHandle, Model, ModelRc, VecModel};

use crate::{campfire, App, ChatRow, Row, Seat, SeatGeometry};

pub const CASES: &[&str] = &[
    "vad-waiting",
    "vad-sending",
    "muted",
    "deafened",
    "ptt-unbound",
    "ptt-waiting",
    "ptt-sending",
    "rebinding",
    "reconnecting",
    "capture-failed",
    "render-failed",
    "udp-failed",
    "long-nickname-key",
    "alone",
    "eight-members",
    "twelve-members",
    "member-menu",
    "chat",
    "diagnostics",
    "navigation",
];

pub struct Fixture {
    app: slint::Weak<App>,
    rows: Rc<VecModel<Row>>,
    seats: Rc<VecModel<Seat>>,
    waiting: Rc<VecModel<Seat>>,
    stage: RefCell<campfire::Stage>,
    timings: RefCell<Vec<(f32, f32, u128, u128)>>,
}

impl Fixture {
    pub fn install(app: &App) -> Rc<Self> {
        let rows = Rc::new(VecModel::default());
        let seats = Rc::new(VecModel::default());
        let waiting = Rc::new(VecModel::default());
        app.set_rows(ModelRc::from(rows.clone()));
        app.set_seats(ModelRc::from(seats.clone()));
        app.set_waiting(ModelRc::from(waiting.clone()));
        app.set_server_name("篝火 · 深夜小队".into());
        app.set_channel_name("游戏闲聊".into());
        app.set_nick("小鱼".into());
        app.set_my_fingerprint("离线预览身份 · 不使用真实账号".into());
        app.set_vad_level(0.56);
        app.set_preview_enabled(true);
        app.set_can_create_channel(true);
        app.set_capture_devices(ModelRc::from(Rc::new(VecModel::from(vec![
            "系统默认（预览）".into(),
        ]))));
        app.set_render_devices(ModelRc::from(Rc::new(VecModel::from(vec![
            "系统默认（预览）".into(),
        ]))));
        app.set_diagnostics("离线布局预览\n会话：模拟 · 不联网\n麦克风：模拟输入\n扬声器：模拟输出\n语音通路：模拟\n\nF1：760×520\nF2：400×360\nF3：切换状态\nF4：打开成员菜单\nTab / Shift+Tab / Esc / Space：实际 Slint 键盘路径".into());
        app.set_chat(ModelRc::from(Rc::new(VecModel::from(vec![
            ChatRow {
                sender: "阿杰".into(),
                time: "22:41".into(),
                body: "今晚就在这儿聊，准备好再开一局。".into(),
                is_me: false,
            },
            ChatRow {
                sender: "小鱼".into(),
                time: "22:42".into(),
                body: "收到。文字、导航、诊断切换时底栏都留在原位。".into(),
                is_me: true,
            },
        ]))));
        let fixture = Rc::new(Self {
            app: app.as_weak(),
            rows,
            seats,
            waiting,
            stage: RefCell::new(campfire::Stage::default()),
            timings: RefCell::new(Vec::new()),
        });
        fixture.stage.borrow_mut().set_channel(7);
        let f = fixture.clone();
        app.on_campfire_resized(move |w, h| f.resize(w, h));
        let f = fixture.clone();
        app.on_open_member(move |id| f.open_member(id));
        let f = fixture.clone();
        app.on_set_user_volume(move |id, value| {
            if let Some(app) = f.app.upgrade() {
                let mut member = app.get_member_data();
                if member.id == id {
                    member.volume = value.round() as i32;
                    app.set_member_data(member);
                }
                for index in 0..f.rows.row_count() {
                    let mut row = f.rows.row_data(index).unwrap();
                    if !row.is_channel && row.id == id {
                        row.volume = value.round() as i32;
                        f.rows.set_row_data(index, row);
                    }
                }
            }
        });
        let weak = app.as_weak();
        app.on_toggle_mute(move || {
            if let Some(app) = weak.upgrade() {
                if !app.get_self_deafened() {
                    app.set_self_muted(!app.get_self_muted());
                    app.set_transmitting(false);
                    Self::refresh_presentation(&app);
                }
            }
        });
        let weak = app.as_weak();
        app.on_toggle_deafen(move || {
            if let Some(app) = weak.upgrade() {
                app.set_self_deafened(!app.get_self_deafened());
                if app.get_self_deafened() {
                    app.set_self_muted(true);
                    app.set_transmitting(false);
                }
                Self::refresh_presentation(&app);
            }
        });
        let weak = app.as_weak();
        app.on_toggle_settings(move || {
            if let Some(app) = weak.upgrade() {
                app.set_show_settings(!app.get_show_settings());
            }
        });
        let weak = app.as_weak();
        app.on_set_ptt_mode(move |value| {
            if let Some(app) = weak.upgrade() {
                app.set_ptt_mode(value);
                app.set_transmitting(false);
                Self::refresh_presentation(&app);
            }
        });
        let weak = app.as_weak();
        app.on_rebind_ptt(move || {
            if let Some(app) = weak.upgrade() {
                app.set_ptt_mode(true);
                app.set_rebinding(true);
                Self::refresh_presentation(&app);
            }
        });
        let weak = app.as_weak();
        app.on_cancel_rebind(move || {
            if let Some(app) = weak.upgrade() {
                app.set_rebinding(false);
                Self::refresh_presentation(&app);
            }
        });
        let weak = app.as_weak();
        app.on_show_diagnostics(move || {
            if let Some(app) = weak.upgrade() {
                app.set_diagnostics_open(true);
            }
        });
        let weak = app.as_weak();
        app.on_retry_voice(move || {
            if let Some(app) = weak.upgrade() {
                app.set_voice_error("".into());
                app.set_voice_notice("正在探测语音通路，请稍候".into());
                Self::refresh_presentation(&app);
            }
        });
        let f = fixture.clone();
        app.on_join_channel(move |id| {
            if let Some(app) = f.app.upgrade() {
                if let Some(row) = f.rows.iter().find(|row| row.is_channel && row.id == id) {
                    app.set_channel_name(row.name);
                }
            }
        });
        let weak = app.as_weak();
        app.on_leave(move || {
            if let Some(app) = weak.upgrade() {
                app.set_reconnecting("".into());
                app.set_voice_notice("预览中的离开动作；此进程没有真实会话".into());
                Self::refresh_presentation(&app);
            }
        });
        fixture.apply("vad-waiting");
        fixture
    }

    // Simulated view-model output for these preview fixtures. There is no
    // presentation policy in CallPage: every command updates the supplied view.
    fn refresh_presentation(app: &App) {
        let reconnecting = app.get_reconnecting();
        let voice_error = app.get_voice_error();
        let voice_notice = app.get_voice_notice();
        let status = if !reconnecting.is_empty() {
            "正在重连"
        } else if app.get_udp_failed() {
            "语音通路异常"
        } else if !app.get_input_available() && !voice_error.is_empty() {
            "麦克风输入不可用"
        } else if app.get_self_deafened() {
            "已关闭声音 · 麦克风已关"
        } else if app.get_self_muted() {
            "麦克风已关闭"
        } else if !app.get_input_available() {
            "输入未就绪"
        } else if app.get_transmitting() && !app.get_render_available() {
            "正在发送 · 播放不可用"
        } else if app.get_transmitting() {
            "正在发送"
        } else if !app.get_render_available() {
            "播放不可用"
        } else if app.get_ptt_mode() && app.get_ptt_label().is_empty() {
            "未绑定说话键"
        } else if app.get_ptt_mode() {
            "等待按键"
        } else {
            "等待人声"
        };
        app.set_self_status(status.into());
        app.set_recovery_message(if !reconnecting.is_empty() {
            reconnecting
        } else if !voice_error.is_empty() {
            voice_error
        } else if !voice_notice.is_empty() {
            voice_notice
        } else if app.get_udp_failed() {
            "语音通路未连通，正在自动恢复".into()
        } else {
            "".into()
        });
    }

    pub fn open_member(&self, id: i32) {
        if let Some(app) = self.app.upgrade() {
            if let Some(row) = self
                .rows
                .iter()
                .find(|row| !row.is_channel && !row.is_me && row.id == id)
            {
                app.set_member_data(row);
                app.set_member_id(id);
            }
        }
    }

    fn populate(&self, count: usize) {
        let app = self.app.upgrade().unwrap();
        let names = [
            app.get_nick().to_string(),
            "阿杰".into(),
            "老张".into(),
            "超级长昵称的队友 · Lighthouse 2026".into(),
            "阿狸".into(),
            "小雨".into(),
            "橘子".into(),
            "Alex".into(),
            "第九位成员".into(),
            "第十位成员".into(),
            "Luna".into(),
            "Mika".into(),
        ];
        let mut rows = vec![Row {
            is_channel: true,
            id: 7,
            name: "游戏闲聊".into(),
            is_current: true,
            count: count as i32,
            can_edit: true,
            ..Default::default()
        }];
        let ids: Vec<_> = (1..=count as u32).collect();
        let mut map = campfire::SeatMap::default();
        map.update(7, 1, &ids);
        let mut seats = vec![Seat::default(); 8];
        let mut waiting = Vec::new();
        for (index, name) in names.iter().enumerate().take(count) {
            let id = index as u32 + 1;
            let seed = campfire::stone_seed(&id.to_le_bytes());
            let name = name.clone();
            rows.push(Row {
                id: id as i32,
                name: name.clone().into(),
                depth: 1,
                is_me: index == 0,
                muted: index == 2 || index == 0 && app.get_self_muted(),
                deafened: index == 0 && app.get_self_deafened(),
                speaking: index == 1,
                volume: 100,
                role: if index == 1 { 4 } else { 2 },
                can_kick: index != 0,
                can_ban: index != 0,
                can_set_role: index != 0,
                ..Default::default()
            });
            let seat = Seat {
                present: true,
                id: id as i32,
                name: name.clone().into(),
                glyph: campfire::glyph(&name).into(),
                seed: seed as i32,
                muted: rows.last().unwrap().muted,
                deafened: rows.last().unwrap().deafened,
                speaking: index == 1,
                is_me: index == 0,
                volume: 100,
                role: rows.last().unwrap().role,
                can_kick: index != 0,
                can_ban: index != 0,
                can_set_role: index != 0,
                ..Default::default()
            };
            if let Some(position) = map.seat_of(id) {
                seats[position] = seat;
            } else {
                waiting.push(seat);
            }
        }
        rows.push(Row {
            is_channel: true,
            id: 8,
            name: "安静休息".into(),
            count: 0,
            can_edit: true,
            can_delete: true,
            ..Default::default()
        });
        self.rows.set_vec(rows);
        self.seats.set_vec(seats);
        self.waiting.set_vec(waiting);
        app.set_channel_count(count as i32);
        let size = self.stage.borrow().size();
        self.resize(size.0, size.1);
    }

    pub fn resize(&self, w: f32, h: f32) {
        if w <= 0.0 || h <= 0.0 {
            return;
        }
        let Some(app) = self.app.upgrade() else {
            return;
        };
        let start = std::time::Instant::now();
        {
            let mut stage = self.stage.borrow_mut();
            stage.set_size((w, h));
            app.set_campfire_backdrop(stage.still());
        }
        let stage_us = start.elapsed().as_micros();
        let stones_start = std::time::Instant::now();
        app.set_scene_geometry(ModelRc::from(Rc::new(VecModel::from(
            campfire::seat_geometry((w, h))
                .into_iter()
                .map(|(x, y, size, up)| SeatGeometry { x, y, size, up })
                .collect::<Vec<_>>(),
        ))));
        for index in 0..self.seats.row_count() {
            let mut seat = self.seats.row_data(index).unwrap();
            if seat.present {
                let images = campfire::stone_images(seat.seed as u32, index, (w, h));
                seat.stone = images.0;
                seat.stone_talking = images.1;
                self.seats.set_row_data(index, seat);
            }
        }
        self.timings
            .borrow_mut()
            .push((w, h, stage_us, stones_start.elapsed().as_micros()));
    }

    pub fn write_timings(&self, path: &std::path::Path) -> std::io::Result<()> {
        let mut text = String::from(
            "# Offline fixture only: CPU stage render and stone generation; excludes native UI first frame/DNS/audio/network\nwidth,height,stage_us,geometry_and_stones_us\n",
        );
        for (w, h, stage, stones) in self.timings.borrow().iter() {
            text.push_str(&format!("{w},{h},{stage},{stones}\n"));
        }
        std::fs::write(path, text)
    }

    pub fn apply(&self, case: &str) {
        let app = self.app.upgrade().unwrap();
        app.set_member_id(-1);
        app.set_show_settings(false);
        app.set_tree_open(false);
        app.set_chat_open(false);
        app.set_diagnostics_open(false);
        app.set_nick("小鱼".into());
        app.set_reconnecting("".into());
        app.set_voice_error("".into());
        app.set_voice_notice("".into());
        app.set_self_status("等待人声".into());
        app.set_recovery_message("".into());
        app.set_self_muted(false);
        app.set_self_deafened(false);
        app.set_ptt_mode(false);
        app.set_ptt_label("鼠标侧键".into());
        app.set_rebinding(false);
        app.set_input_available(true);
        app.set_render_available(true);
        app.set_input_level(0.34);
        app.set_udp_ok(true);
        app.set_udp_failed(false);
        app.set_transmitting(false);
        match case {
            "vad-sending" => {
                app.set_transmitting(true);
                app.set_input_level(0.76);
            }
            "muted" => app.set_self_muted(true),
            "deafened" => {
                app.set_self_muted(true);
                app.set_self_deafened(true);
            }
            "ptt-unbound" => {
                app.set_ptt_mode(true);
                app.set_ptt_label("".into());
            }
            "ptt-waiting" => app.set_ptt_mode(true),
            "ptt-sending" => {
                app.set_ptt_mode(true);
                app.set_transmitting(true);
                app.set_input_level(0.76);
            }
            "rebinding" => {
                app.set_ptt_mode(true);
                app.set_rebinding(true);
            }
            "reconnecting" => {
                app.set_reconnecting("连接中断，正在重连，频道和成员暂时保留".into());
                app.set_input_available(false);
            }
            "capture-failed" => {
                app.set_input_available(false);
                app.set_voice_error("麦克风不可用：设备已移除，请重新选择设备".into());
                app.set_self_status("麦克风输入不可用".into());
            }
            "render-failed" => {
                app.set_render_available(false);
                app.set_voice_error("播放不可用：扬声器已移除；麦克风仍在发送".into());
                app.set_transmitting(true);
                app.set_input_level(0.76);
                app.set_self_status("正在发送 · 播放不可用".into());
            }
            "udp-failed" => {
                app.set_udp_failed(true);
                app.set_udp_ok(false);
            }
            "long-nickname-key" => {
                app.set_nick("非常非常长的自我昵称 · Lighthouse 2026".into());
                app.set_ptt_mode(true);
                app.set_ptt_label("Left Control + Mouse Extra Button 2".into());
            }
            "chat" => app.set_chat_open(true),
            "diagnostics" => app.set_diagnostics_open(true),
            "navigation" => app.set_tree_open(true),
            _ => {}
        }
        self.populate(match case {
            "alone" => 1,
            "eight-members" => 8,
            "twelve-members" => 12,
            _ => 4,
        });
        app.set_connected(true);
        Self::refresh_presentation(&app);
        if case == "member-menu" {
            self.open_member(4);
        }
        app.set_preview_case(case.into());
    }
}
