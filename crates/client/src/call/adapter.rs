// SPDX-License-Identifier: GPL-3.0-or-later
//! Translate plain call projections into Slint properties and stable models.
//! Models are presentation caches; their values never decide call behavior.

use client_runtime::call::{AudioViewModel, CallViewModel, RowView};
use client_runtime::self_state::ConnectionState;
use slint::{Model, ModelRc, VecModel};

use crate::{App, BanRow, ChatRow, Row, Seat};

pub struct SlintAdapter;

impl SlintAdapter {
    pub fn apply_audio(app: &App, view: &AudioViewModel) {
        let state = &view.self_state;
        app.set_connected(matches!(
            view.connection,
            ConnectionState::Connected | ConnectionState::Reconnecting
        ));
        app.set_connecting(view.connection == ConnectionState::Connecting);
        app.set_reconnecting(view.reconnecting.clone().into());
        app.set_self_muted(view.self_muted);
        app.set_self_deafened(state.deafened);
        app.set_self_status(state.status_label().into());
        app.set_input_available(state.input_available);
        app.set_render_available(view.render_available);
        app.set_input_level(state.input_level);
        app.set_monitoring(state.monitoring);
        app.set_transmitting(state.transmitting);
        app.set_ptt_mode(state.ptt_mode);
        app.set_voice_error(view.voice_error.clone().into());
        app.set_voice_notice(view.voice_notice.clone().into());
        app.set_recovery_message(view.recovery_message.clone().into());
        app.set_udp_ok(view.udp_ok);
        app.set_udp_failed(view.udp_failed);
        app.set_capture_in_use(view.capture.name.clone().into());
        app.set_capture_is_virtual(view.capture.is_virtual);
    }

    pub fn apply_call(app: &App, view: &CallViewModel) {
        Self::apply_audio(app, &view.audio);
        let rows: Vec<_> = view.rows.iter().map(row).collect();
        app.set_rows(reconcile(app.get_rows(), rows, |row| {
            (row.is_channel, row.id)
        }));
        app.set_chat(reconcile(
            app.get_chat(),
            view.chat
                .iter()
                .map(|line| ChatRow {
                    sender: line.sender.clone().into(),
                    body: line.body.clone().into(),
                    time: crate::clock_time(line.timestamp_ms).into(),
                    is_me: line.is_me,
                })
                .collect(),
            |line| {
                (
                    line.sender.clone(),
                    line.body.clone(),
                    line.time.clone(),
                    line.is_me,
                )
            },
        ));
        app.set_bans(reconcile(
            app.get_bans(),
            view.bans
                .iter()
                .map(|ban| BanRow {
                    name: ban.name.clone().into(),
                    key: ban.key.clone().into(),
                    detail: format!(
                        "{} 被 {} 封禁{}",
                        crate::clock_date(ban.banned_at_ms),
                        ban.banned_by,
                        if ban.reason.is_empty() {
                            String::new()
                        } else {
                            format!("：{}", ban.reason)
                        }
                    )
                    .into(),
                })
                .collect(),
            |ban| ban.key.clone(),
        ));
        app.set_channel_name(view.channel_name.clone().into());
        app.set_channel_count(view.channel_count as i32);
        app.set_can_create_channel(view.can_create_channel);
        app.set_is_admin(view.is_admin);
        Self::refresh_selection(app);
    }

    /// Business fields come from the same projection as the navigator. The
    /// official scene adds only deterministic visual identity here.
    pub fn scene_members(view: &CallViewModel) -> Vec<Seat> {
        view.members
            .iter()
            .map(|member| Seat {
                present: true,
                id: member.id as i32,
                name: member.name.clone().into(),
                glyph: crate::campfire::glyph(&member.name).into(),
                seed: crate::campfire::stone_seed(&member.public_key) as i32,
                stone: Default::default(),
                stone_talking: Default::default(),
                muted: member.muted,
                deafened: member.deafened,
                speaking: member.speaking,
                is_me: member.is_me,
                volume: member.volume,
                role: member.role,
                can_kick: member.can_kick,
                can_ban: member.can_ban,
                can_set_role: member.can_set_role,
            })
            .collect()
    }

    /// Only changed rows emit notifications during the frequent audio poll.
    pub fn apply_activity(app: &App, view: &AudioViewModel) {
        let rows = app.get_rows();
        for i in 0..rows.row_count() {
            let Some(mut row) = rows.row_data(i) else {
                continue;
            };
            if row.is_channel {
                continue;
            }
            let old = row.clone();
            row.speaking = view.member_speaking(row.id as u32, row.is_me);
            if row.is_me {
                row.muted = view.self_state.muted;
                row.deafened = view.self_state.deafened;
            }
            if old != row {
                rows.set_row_data(i, row);
            }
        }
        let mut waiting_talking = false;
        for (model, waiting) in [(app.get_seats(), false), (app.get_waiting(), true)] {
            for i in 0..model.row_count() {
                let Some(mut seat) = model.row_data(i) else {
                    continue;
                };
                let old = seat.clone();
                seat.speaking = seat.present && view.member_speaking(seat.id as u32, seat.is_me);
                if seat.present && seat.is_me {
                    seat.muted = view.self_state.muted;
                    seat.deafened = view.self_state.deafened;
                }
                waiting_talking |= waiting && seat.speaking;
                if old != seat {
                    model.set_row_data(i, seat);
                }
            }
        }
        app.set_waiting_talking(waiting_talking);
        Self::refresh_selection(app);
    }

    pub fn apply_scene(app: &App, seats: Vec<Seat>, waiting: Vec<Seat>) {
        // Physical slots retain identity even when an occupant changes.
        app.set_waiting_talking(waiting.iter().any(|seat| seat.speaking));
        let current = app.get_seats();
        if current.row_count() == seats.len() {
            for (i, seat) in seats.into_iter().enumerate() {
                // Default empty Images can have distinct handles. An empty
                // physical slot has no rendered member data to invalidate.
                if !seat.present && current.row_data(i).is_some_and(|old| !old.present) {
                    continue;
                }
                if current.row_data(i).as_ref() != Some(&seat) {
                    current.set_row_data(i, seat);
                }
            }
        } else {
            app.set_seats(ModelRc::new(VecModel::from(seats)));
        }
        app.set_waiting(reconcile(app.get_waiting(), waiting, |seat| seat.id));
    }

    fn refresh_selection(app: &App) {
        let id = app.get_member_id();
        if id < 0 {
            return;
        }
        let rows = app.get_rows();
        if let Some(row) = (0..rows.row_count())
            .filter_map(|i| rows.row_data(i))
            .find(|row| !row.is_channel && row.id == id)
        {
            if app.get_member_data() != row {
                app.set_member_data(row);
            }
        } else {
            app.set_member_id(-1);
        }
    }
}

fn row(view: &RowView) -> Row {
    Row {
        id: view.id as i32,
        is_channel: view.is_channel,
        name: view.name.clone().into(),
        depth: view.depth,
        muted: view.muted,
        deafened: view.deafened,
        speaking: view.speaking,
        is_me: view.is_me,
        is_current: view.is_current,
        count: view.count,
        can_delete: view.can_delete,
        volume: view.volume,
        role: view.role,
        can_kick: view.can_kick,
        can_ban: view.can_ban,
        can_set_role: view.can_set_role,
        can_edit: view.can_edit,
    }
}

/// Preserve ModelRc identity and unchanged delegates across structural events.
/// Duplicate keys (e.g. identical chat messages) are matched by occurrence.
fn reconcile<T: Clone + PartialEq + 'static, K: PartialEq>(
    current: ModelRc<T>,
    rows: Vec<T>,
    key: impl Fn(&T) -> K,
) -> ModelRc<T> {
    let Some(model) = current.as_any().downcast_ref::<VecModel<T>>() else {
        return ModelRc::new(VecModel::from(rows));
    };
    let count = rows.len();
    for (i, row) in rows.into_iter().enumerate() {
        let matches = model.row_data(i).is_some_and(|old| key(&old) == key(&row));
        if matches {
            if model.row_data(i).as_ref() != Some(&row) {
                model.set_row_data(i, row);
            }
        } else {
            if let Some(found) = (i + 1..model.row_count())
                .find(|&j| model.row_data(j).is_some_and(|old| key(&old) == key(&row)))
            {
                model.remove(found);
            }
            model.insert(i, row);
        }
    }
    while model.row_count() > count {
        model.remove(count);
    }
    current
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn structural_updates_keep_the_existing_model_and_apply_reorders() {
        let model = ModelRc::new(VecModel::from(vec![(1, "old"), (2, "two"), (3, "three")]));
        let updated = reconcile(
            model.clone(),
            vec![(3, "three"), (1, "new"), (4, "four")],
            |row| row.0,
        );
        assert_eq!(updated, model);
        assert_eq!(
            model.iter().collect::<Vec<_>>(),
            vec![(3, "three"), (1, "new"), (4, "four")]
        );
        reconcile(updated, Vec::new(), |row| row.0);
        assert_eq!(model.row_count(), 0);
    }

    #[test]
    fn channel_and_session_ids_do_not_collide() {
        let model = ModelRc::new(VecModel::from(vec![
            (true, 7, "channel"),
            (false, 7, "user"),
        ]));
        reconcile(
            model.clone(),
            vec![(false, 7, "renamed"), (true, 7, "channel")],
            |row| (row.0, row.1),
        );
        assert_eq!(
            model.iter().collect::<Vec<_>>(),
            vec![(false, 7, "renamed"), (true, 7, "channel")]
        );
    }
}
