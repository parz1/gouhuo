// SPDX-License-Identifier: GPL-3.0-or-later
use std::collections::BTreeMap;

use client_core::Roster;
use protocol::control::User;

use super::{CallState, ConnectionPhase};
use crate::self_state::{ConnectionState, SelfStateView};
use crate::{CaptureInfo, RuntimeSnapshot};

/// Audio presentation for the frequent poll path. No roster walk, date format,
/// image generation or GUI model access is needed to produce this value.
#[derive(Debug, Clone, PartialEq)]
pub struct AudioViewModel {
    pub connection: ConnectionState,
    pub self_state: SelfStateView,
    /// The user's mute toggle, distinct from effective/administrator mute.
    pub self_muted: bool,
    pub render_available: bool,
    pub udp_ok: bool,
    pub udp_failed: bool,
    pub voice_error: String,
    pub reconnecting: String,
    pub voice_notice: String,
    pub recovery_message: String,
    pub capture: CaptureInfo,
    /// Live remote speaker sessions. Empty unless the call is connected.
    /// Local speaking always comes from self_state.transmitting instead.
    pub speaking: Vec<u32>,
}

impl AudioViewModel {
    /// Shared rule for member rows and scene seats, including frequent updates.
    pub fn member_speaking(&self, session: u32, is_me: bool) -> bool {
        if is_me {
            self.self_state.transmitting
        } else {
            self.speaking.contains(&session)
        }
    }
}

/// Ordinary Rust equivalent of a flattened channel/member list row. Session
/// and channel identifiers keep their unsigned protocol type until adaptation.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct RowView {
    pub is_channel: bool,
    pub id: u32,
    pub name: String,
    pub depth: i32,
    pub muted: bool,
    pub deafened: bool,
    pub speaking: bool,
    pub is_me: bool,
    pub is_current: bool,
    pub count: i32,
    pub can_delete: bool,
    pub volume: i32,
    pub role: i32,
    pub can_kick: bool,
    pub can_ban: bool,
    pub can_set_role: bool,
    pub can_edit: bool,
}

/// Member data without images, seat assignment or a GUI string/model type.
/// public_key is the stable input for scene seeds and saved user preferences.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct MemberView {
    pub id: u32,
    pub public_key: Vec<u8>,
    pub name: String,
    pub muted: bool,
    pub deafened: bool,
    pub speaking: bool,
    pub is_me: bool,
    pub volume: i32,
    pub role: i32,
    pub can_kick: bool,
    pub can_ban: bool,
    pub can_set_role: bool,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ChatView {
    pub sender: String,
    pub body: String,
    /// Keep the original instant; the GUI adapter chooses locale/time format.
    pub timestamp_ms: i64,
    pub is_me: bool,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct BanView {
    pub name: String,
    pub key: String,
    pub banned_at_ms: i64,
    pub banned_by: String,
    pub reason: String,
}

/// One call projection used for structural refreshes. Audio state is computed
/// from the same snapshot as rows/members, so a rename or roster update cannot
/// temporarily extinguish a real speaker or revive a stale GUI indication.
#[derive(Debug, Clone, PartialEq)]
pub struct CallViewModel {
    pub audio: AudioViewModel,
    pub rows: Vec<RowView>,
    /// Current-channel members, in the same stable name order as the roster.
    pub members: Vec<MemberView>,
    pub chat: Vec<ChatView>,
    pub bans: Vec<BanView>,
    pub channel_id: u32,
    pub channel_name: String,
    pub channel_count: u32,
    pub me: u32,
    pub can_create_channel: bool,
    pub is_admin: bool,
}

impl CallViewModel {
    pub fn project_audio(
        state: &CallState,
        snapshot: &RuntimeSnapshot,
        ptt_bound: bool,
    ) -> AudioViewModel {
        let connection = state.connection();
        let stats = snapshot.voice.as_ref();
        let mut self_state = SelfStateView::project(snapshot, connection, ptt_bound);
        if connection == ConnectionState::Reconnecting {
            // The old session's device readings cannot describe the new call.
            self_state.input_available = false;
            self_state.input_level = 0.0;
        }
        let voice_error = snapshot
            .error
            .as_ref()
            .or_else(|| stats.and_then(|s| s.error.as_ref()))
            .or_else(|| snapshot.mic.as_ref().and_then(|s| s.error.as_ref()))
            .cloned()
            .unwrap_or_default();
        let reconnecting = match &state.phase {
            ConnectionPhase::Reconnecting { attempt, reason } => {
                format!("连接断了，正在自动重连（第 {attempt} 次）。\n{reason}")
            }
            _ => String::new(),
        };
        let connected = connection == ConnectionState::Connected;
        let udp_failed = connected && stats.is_some_and(|s| s.udp_failed);
        // Preserve the call page's existing priority in one GUI-free location.
        let recovery_message = if !reconnecting.is_empty() {
            reconnecting.clone()
        } else if !voice_error.is_empty() {
            voice_error.clone()
        } else if !state.voice_notice.is_empty() {
            state.voice_notice.clone()
        } else if udp_failed {
            "语音通路未连通，正在自动恢复".into()
        } else {
            String::new()
        };
        AudioViewModel {
            connection,
            self_state,
            self_muted: snapshot.intent.muted,
            render_available: stats.is_some_and(|s| s.render_available),
            udp_ok: connected && stats.is_some_and(|s| s.udp_ok),
            udp_failed,
            voice_error,
            reconnecting,
            voice_notice: state.voice_notice.clone(),
            recovery_message,
            capture: snapshot.capture.clone(),
            speaking: if connected {
                stats.map(|s| s.speaking.clone()).unwrap_or_default()
            } else {
                Vec::new()
            },
        }
    }

    pub fn project(
        state: &CallState,
        snapshot: &RuntimeSnapshot,
        ptt_bound: bool,
        roster: Option<&Roster>,
        user_volumes: &BTreeMap<String, u32>,
    ) -> Self {
        let audio = Self::project_audio(state, snapshot, ptt_bound);
        let Some(roster) = roster else {
            return Self {
                audio,
                rows: Vec::new(),
                members: Vec::new(),
                chat: Vec::new(),
                bans: Vec::new(),
                channel_id: 0,
                channel_name: String::new(),
                channel_count: 0,
                me: 0,
                can_create_channel: false,
                is_admin: false,
            };
        };
        let channel_id = roster.my_channel();
        let mut rows = Vec::new();
        for node in roster.tree() {
            let users = roster.users_in(node.channel.id);
            rows.push(RowView {
                is_channel: true,
                id: node.channel.id,
                name: node.channel.name,
                depth: to_int(node.depth),
                muted: false,
                deafened: false,
                speaking: false,
                is_me: false,
                is_current: node.channel.id == channel_id,
                count: to_int(users.len()),
                can_delete: roster.can_delete_channel(node.channel.id),
                volume: 100,
                role: 0,
                can_kick: false,
                can_ban: false,
                can_set_role: false,
                can_edit: roster.can_edit_channel(node.channel.id),
            });
            for user in users {
                let member = project_member(user, roster, &audio, user_volumes);
                rows.push(RowView {
                    is_channel: false,
                    id: member.id,
                    name: member.name,
                    depth: to_int(node.depth.saturating_add(1)),
                    muted: member.muted,
                    deafened: member.deafened,
                    speaking: member.speaking,
                    is_me: member.is_me,
                    is_current: false,
                    count: 0,
                    can_delete: false,
                    volume: member.volume,
                    role: member.role,
                    can_kick: member.can_kick,
                    can_ban: member.can_ban,
                    can_set_role: member.can_set_role,
                    can_edit: false,
                });
            }
        }
        let members: Vec<_> = roster
            .users_in(channel_id)
            .into_iter()
            .map(|user| project_member(user, roster, &audio, user_volumes))
            .collect();
        let chat = roster
            .chat
            .iter()
            .map(|line| ChatView {
                sender: line.sender_name.clone(),
                body: line.body.clone(),
                timestamp_ms: line.timestamp_ms,
                is_me: line.sender_session == roster.me,
            })
            .collect();
        let bans = roster
            .bans
            .iter()
            .map(|ban| BanView {
                name: ban.name.clone(),
                key: protocol::base32::encode(&ban.public_key),
                banned_at_ms: ban.banned_at_ms,
                banned_by: ban.banned_by.clone(),
                reason: ban.reason.clone(),
            })
            .collect();
        Self {
            audio,
            rows,
            channel_count: members.len().min(u32::MAX as usize) as u32,
            members,
            chat,
            bans,
            channel_id,
            channel_name: roster
                .channels
                .get(&channel_id)
                .map(|c| c.name.clone())
                .unwrap_or_default(),
            me: roster.me,
            can_create_channel: roster.can_create_channel(),
            is_admin: roster.is_admin(),
        }
    }
}

fn project_member(
    user: &User,
    roster: &Roster,
    audio: &AudioViewModel,
    user_volumes: &BTreeMap<String, u32>,
) -> MemberView {
    let is_me = user.session_id == roster.me;
    MemberView {
        id: user.session_id,
        public_key: user.public_key.clone(),
        name: user.name.clone(),
        // Protocol self-state echoes have no revision, so they must never undo
        // a newer local click. Administrator mute is a separate runtime intent.
        muted: if is_me {
            audio.self_state.muted
        } else {
            user.self_muted || user.server_muted
        },
        deafened: if is_me {
            audio.self_state.deafened
        } else {
            user.self_deafened
        },
        speaking: audio.member_speaking(user.session_id, is_me),
        is_me,
        volume: user_volumes
            .get(&protocol::base32::encode(&user.public_key))
            .copied()
            .unwrap_or(100)
            .min(400) as i32,
        role: user.role,
        can_kick: roster.can_kick(user.session_id),
        can_ban: roster.can_ban(user.session_id),
        can_set_role: roster.can_set_role(user.session_id),
    }
}

fn to_int(value: usize) -> i32 {
    value.min(i32::MAX as usize) as i32
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::self_state::SelfStatus;
    use crate::{MicSnapshot, RuntimeStage};
    use client_core::ChatLine;
    use protocol::control::{BannedUser, Channel, Role};
    use voice_types::{TransmitMode, VoiceStats};

    fn connected_state() -> CallState {
        let mut state = CallState::default();
        state.connected();
        state
    }

    fn live_snapshot() -> RuntimeSnapshot {
        RuntimeSnapshot {
            stage: RuntimeStage::Ready,
            session_id: Some(5),
            voice: Some(VoiceStats {
                input_available: true,
                input_db: -20.0,
                render_available: true,
                udp_ok: true,
                transmitting: true,
                speaking: vec![8],
                ..Default::default()
            }),
            ..Default::default()
        }
    }

    fn roster() -> Roster {
        Roster {
            me: 5,
            channels: [(
                1,
                Channel {
                    id: 1,
                    parent_id: 1,
                    name: "篝火".into(),
                    ..Default::default()
                },
            )]
            .into(),
            users: [
                (
                    5,
                    User {
                        session_id: 5,
                        public_key: vec![5; 32],
                        name: "我".into(),
                        channel_id: 1,
                        role: Role::Admin as i32,
                        ..Default::default()
                    },
                ),
                (
                    8,
                    User {
                        session_id: 8,
                        public_key: vec![8; 32],
                        name: "伙伴".into(),
                        channel_id: 1,
                        role: Role::Member as i32,
                        ..Default::default()
                    },
                ),
            ]
            .into(),
            ..Default::default()
        }
    }

    fn full(state: &CallState, snapshot: &RuntimeSnapshot, roster: &Roster) -> CallViewModel {
        CallViewModel::project(state, snapshot, true, Some(roster), &BTreeMap::new())
    }

    #[test]
    fn connection_transitions_need_no_gui_or_client_presence() {
        let mut state = CallState::default();
        let snapshot = live_snapshot();
        assert_eq!(
            CallViewModel::project_audio(&state, &snapshot, true).connection,
            ConnectionState::Offline
        );
        state.set_notice("上次的提示");
        state.begin_join();
        assert!(state.voice_notice.is_empty());
        let preparing = CallViewModel::project_audio(&state, &snapshot, true);
        assert_eq!(preparing.self_state.status, SelfStatus::Preparing);
        assert!(!preparing.self_state.transmitting);
        assert!(!preparing.udp_ok);
        state.connected();
        assert!(
            CallViewModel::project_audio(&state, &snapshot, true)
                .self_state
                .transmitting
        );
        state.reconnecting(3, "证书通道关闭");
        let reconnecting = CallViewModel::project_audio(&state, &snapshot, true);
        assert_eq!(reconnecting.connection, ConnectionState::Reconnecting);
        assert!(reconnecting.reconnecting.contains("第 3 次"));
        assert!(reconnecting.reconnecting.ends_with("证书通道关闭"));
        state.set_notice("正在恢复");
        state.offline();
        assert!(state.voice_notice.is_empty());
        assert_eq!(state.connection(), ConnectionState::Offline);
    }

    #[test]
    fn old_roster_self_echo_cannot_undo_local_intent() {
        let state = connected_state();
        let mut snapshot = live_snapshot();
        let mut roster = roster();
        // An old muted echo follows a newer unmute click.
        let me = roster.users.get_mut(&5).unwrap();
        me.self_muted = true;
        me.self_deafened = true;
        let view = full(&state, &snapshot, &roster);
        let row = view.rows.iter().find(|r| !r.is_channel && r.is_me).unwrap();
        let member = view.members.iter().find(|m| m.is_me).unwrap();
        assert!(!view.audio.self_muted && !row.muted && !member.muted);
        assert!(!row.deafened && !member.deafened);
        assert!(row.speaking && member.speaking && view.audio.self_state.transmitting);
        // The reverse ordering must also keep a newer mute, without a round trip.
        roster.users.get_mut(&5).unwrap().self_muted = false;
        snapshot.intent.muted = true;
        let view = full(&state, &snapshot, &roster);
        assert!(view.audio.self_muted);
        assert!(view.rows.iter().find(|r| r.is_me).unwrap().muted);
        assert!(view.members.iter().find(|m| m.is_me).unwrap().muted);
        assert!(!view.audio.self_state.transmitting);
        // Administrator mute constrains the projection without changing the toggle.
        snapshot.intent.muted = false;
        snapshot.intent.server_muted = true;
        let view = full(&state, &snapshot, &roster);
        assert!(!view.audio.self_muted);
        assert_eq!(view.audio.self_state.status, SelfStatus::ServerMuted);
        assert!(view.members.iter().find(|m| m.is_me).unwrap().muted);
    }

    #[test]
    fn output_failure_keeps_actual_sending_in_every_presentation() {
        let state = connected_state();
        let mut snapshot = live_snapshot();
        snapshot.stage = RuntimeStage::Failed;
        let stats = snapshot.voice.as_mut().unwrap();
        stats.render_available = false;
        stats.render_error = Some("耳机已拔出".into());
        stats.error = stats.render_error.clone();
        let view = full(&state, &snapshot, &roster());
        assert_eq!(view.audio.self_state.status, SelfStatus::OutputFailed);
        assert_eq!(view.audio.recovery_message, "耳机已拔出");
        assert!(!view.audio.render_available);
        assert!(view.audio.self_state.transmitting);
        assert!(view.rows.iter().find(|r| r.is_me).unwrap().speaking);
        assert!(view.members.iter().find(|m| m.is_me).unwrap().speaking);
    }

    #[test]
    fn reconnection_retires_stale_audio_for_rows_and_members() {
        let mut state = connected_state();
        let mut snapshot = live_snapshot();
        snapshot.voice.as_mut().unwrap().udp_failed = true;
        state.reconnecting(1, "连接重置");
        let view = full(&state, &snapshot, &roster());
        assert!(view.audio.speaking.is_empty());
        assert!(!view.audio.self_state.transmitting);
        assert!(!view.audio.self_state.input_available);
        assert_eq!(view.audio.self_state.input_level, 0.0);
        assert!(!view.audio.udp_ok && !view.audio.udp_failed);
        assert!(view.rows.iter().all(|r| !r.speaking));
        assert!(view.members.iter().all(|m| !m.speaking));
    }

    #[test]
    fn roster_refresh_uses_current_speech_not_previous_ui_or_empty_defaults() {
        let state = connected_state();
        let mut snapshot = live_snapshot();
        let mut roster = roster();
        let old = full(&state, &snapshot, &roster);
        assert!(old.members.iter().find(|m| m.id == 8).unwrap().speaking);
        roster.users.get_mut(&8).unwrap().name = "新昵称".into();
        let renamed = full(&state, &snapshot, &roster);
        let remote = renamed.members.iter().find(|m| m.id == 8).unwrap();
        assert_eq!(remote.name, "新昵称");
        assert!(remote.speaking);
        assert!(
            renamed
                .rows
                .iter()
                .find(|r| !r.is_channel && r.id == 8)
                .unwrap()
                .speaking
        );
        snapshot.voice.as_mut().unwrap().speaking.clear();
        snapshot.voice.as_mut().unwrap().transmitting = false;
        let stopped = full(&state, &snapshot, &roster);
        assert!(stopped.members.iter().all(|m| !m.speaking));
        assert!(stopped.rows.iter().all(|r| !r.speaking));
    }

    #[test]
    fn offline_mic_check_keeps_input_and_mode_comes_from_runtime() {
        let mut snapshot = RuntimeSnapshot {
            mic: Some(MicSnapshot {
                input_db: -24.0,
                input_available: true,
                error: None,
            }),
            ..Default::default()
        };
        snapshot.intent.mode = TransmitMode::VoiceActivity {
            threshold_db: -45.0,
        };
        let view = CallViewModel::project(
            &CallState::default(),
            &snapshot,
            false,
            None,
            &BTreeMap::new(),
        );
        assert!(view.audio.self_state.input_available);
        assert!(view.audio.self_state.input_level > 0.0);
        assert!(!view.audio.self_state.ptt_mode);
        assert!(!view.audio.self_state.transmitting);
        assert!(view.rows.is_empty() && view.members.is_empty());
        assert_eq!(view.channel_count, 0);
        assert!(!view.can_create_channel && !view.is_admin);
    }

    #[test]
    fn errors_and_recovery_keep_the_existing_priority_and_clear_absent_facts() {
        let mut state = connected_state();
        state.set_notice("正在恢复语音");
        let mut snapshot = live_snapshot();
        snapshot.voice.as_mut().unwrap().udp_failed = true;
        snapshot.voice.as_mut().unwrap().error = Some("输出故障".into());
        snapshot.error = Some("启动失败".into());
        snapshot.mic = Some(MicSnapshot {
            error: Some("试麦失败".into()),
            ..Default::default()
        });
        assert_eq!(
            CallViewModel::project_audio(&state, &snapshot, true).recovery_message,
            "启动失败"
        );
        state.reconnecting(2, "TCP 重置");
        let reconnecting = CallViewModel::project_audio(&state, &snapshot, true);
        assert_eq!(reconnecting.recovery_message, reconnecting.reconnecting);
        state.connected();
        snapshot.error = None;
        assert_eq!(
            CallViewModel::project_audio(&state, &snapshot, true).voice_error,
            "输出故障"
        );
        snapshot.voice = None;
        let no_voice = CallViewModel::project_audio(&state, &snapshot, true);
        assert_eq!(no_voice.voice_error, "试麦失败");
        assert!(!no_voice.udp_ok && !no_voice.udp_failed && !no_voice.render_available);
        snapshot.mic = None;
        assert_eq!(
            CallViewModel::project_audio(&state, &snapshot, true).recovery_message,
            "正在恢复语音"
        );
        state.set_notice("");
        snapshot = live_snapshot();
        snapshot.voice.as_mut().unwrap().udp_failed = true;
        assert_eq!(
            CallViewModel::project_audio(&state, &snapshot, true).recovery_message,
            "语音通路未连通，正在自动恢复"
        );
    }

    #[test]
    fn structural_dtos_preserve_identity_permissions_volume_and_message_instants() {
        let state = connected_state();
        let snapshot = live_snapshot();
        let mut roster = roster();
        roster.chat.push_back(ChatLine {
            sender_session: 8,
            sender_name: "旧昵称".into(),
            body: "链接".into(),
            timestamp_ms: 12345,
        });
        roster.bans.push(BannedUser {
            public_key: vec![9; 32],
            name: "离开的人".into(),
            banned_at_ms: 45678,
            banned_by: "管理员".into(),
            reason: "原因".into(),
        });
        let key = protocol::base32::encode(&roster.users[&8].public_key);
        let volumes = [(key, 275)].into();
        let view = CallViewModel::project(&state, &snapshot, true, Some(&roster), &volumes);
        assert_eq!(view.channel_id, 1);
        assert_eq!(view.channel_name, "篝火");
        assert_eq!(view.channel_count, 2);
        assert_eq!(view.me, 5);
        assert!(view.is_admin && view.can_create_channel);
        let remote = view.members.iter().find(|m| m.id == 8).unwrap();
        assert_eq!(remote.public_key, vec![8; 32]);
        assert_eq!(remote.volume, 275);
        assert!(remote.can_kick && remote.can_ban && remote.can_set_role);
        assert_eq!(
            view.rows
                .iter()
                .find(|r| !r.is_channel && r.id == 8)
                .unwrap()
                .volume,
            275
        );
        assert_eq!(view.chat[0].sender, "旧昵称");
        assert_eq!(view.chat[0].timestamp_ms, 12345);
        assert!(!view.chat[0].is_me);
        assert_eq!(view.bans[0].banned_at_ms, 45678);
        assert_eq!(view.bans[0].key, protocol::base32::encode(&[9; 32]));
    }
}
