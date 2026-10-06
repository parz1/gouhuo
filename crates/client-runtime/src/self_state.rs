// SPDX-License-Identifier: GPL-3.0-or-later
//! One presentation projection for the dock, local seat and member list.
use crate::{RuntimeSnapshot, RuntimeStage};
use voice_types::TransmitMode;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ConnectionState {
    Offline,
    Connecting,
    Connected,
    Reconnecting,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum SelfStatus {
    Offline,
    Preparing,
    Reconnecting,
    InputFailed,
    OutputFailed,
    TransportFailed,
    ServerMuted,
    Deafened,
    Muted,
    PushToTalkUnbound,
    PushToTalkWaiting,
    VoiceActivityWaiting,
    Sending,
}

impl SelfStatus {
    pub fn label(self) -> &'static str {
        match self {
            Self::Offline => "未连接",
            Self::Preparing => "正在准备语音",
            Self::Reconnecting => "正在恢复连接",
            Self::InputFailed => "麦克风不可用",
            Self::OutputFailed => "听不到声音",
            Self::TransportFailed => "语音无法送达",
            Self::ServerMuted => "被管理员闭麦",
            Self::Deafened => "声音已关闭",
            Self::Muted => "已闭麦",
            Self::PushToTalkUnbound => "尚未绑定按键",
            Self::PushToTalkWaiting => "等待按键",
            Self::VoiceActivityWaiting => "等待说话",
            Self::Sending => "正在发送",
        }
    }
}

#[derive(Debug, Clone, PartialEq)]
pub struct SelfStateView {
    pub muted: bool,
    pub deafened: bool,
    pub server_muted: bool,
    pub input_available: bool,
    pub input_level: f32,
    pub transmitting: bool,
    pub monitoring: bool,
    pub ptt_mode: bool,
    pub ptt_bound: bool,
    pub status: SelfStatus,
}

impl SelfStateView {
    pub fn project(
        snapshot: &RuntimeSnapshot,
        connection: ConnectionState,
        ptt_bound: bool,
    ) -> Self {
        let intent = &snapshot.intent;
        let stats = snapshot.voice.as_ref();
        let ptt_mode = matches!(intent.mode, TransmitMode::PushToTalk);
        let capture_failed = stats.is_some_and(|s| s.capture_error.is_some());
        let transport_failed = stats
            .is_some_and(|s| s.transport_error.is_some() || s.udp_failed || s.sequences_exhausted);
        let render_failed = stats.is_some_and(|s| s.render_error.is_some());
        let input_available = stats
            .map(|s| s.input_available)
            .or_else(|| snapshot.mic.as_ref().map(|s| s.input_available))
            .unwrap_or(false);
        let input_db = stats
            .map(|s| s.input_db)
            .or_else(|| snapshot.mic.as_ref().map(|s| s.input_db))
            .unwrap_or(-120.0);
        let muted = intent.muted || intent.deafened || intent.server_muted;
        // Render failure deliberately does not suppress sending: the microphone
        // and transport may remain operational when only playback fails.
        let transmitting = connection == ConnectionState::Connected
            && !muted
            && !capture_failed
            && !transport_failed
            && stats.is_some_and(|s| s.transmitting);
        let status = match connection {
            ConnectionState::Reconnecting => SelfStatus::Reconnecting,
            ConnectionState::Connecting => SelfStatus::Preparing,
            ConnectionState::Offline => SelfStatus::Offline,
            ConnectionState::Connected if capture_failed => SelfStatus::InputFailed,
            ConnectionState::Connected if transport_failed => SelfStatus::TransportFailed,
            ConnectionState::Connected if intent.server_muted => SelfStatus::ServerMuted,
            ConnectionState::Connected if intent.deafened => SelfStatus::Deafened,
            ConnectionState::Connected if intent.muted => SelfStatus::Muted,
            ConnectionState::Connected if render_failed => SelfStatus::OutputFailed,
            ConnectionState::Connected if snapshot.error.is_some() => SelfStatus::TransportFailed,
            ConnectionState::Connected if transmitting => SelfStatus::Sending,
            ConnectionState::Connected
                if snapshot.stage == RuntimeStage::Starting
                    && (!input_available
                        || stats.is_some_and(|s| !s.udp_ok || !s.render_available)) =>
            {
                SelfStatus::Preparing
            }
            ConnectionState::Connected
                if matches!(
                    snapshot.stage,
                    RuntimeStage::Idle | RuntimeStage::Preparing | RuntimeStage::Stopping
                ) =>
            {
                SelfStatus::Preparing
            }
            ConnectionState::Connected if ptt_mode && !ptt_bound => SelfStatus::PushToTalkUnbound,
            ConnectionState::Connected if ptt_mode => SelfStatus::PushToTalkWaiting,
            ConnectionState::Connected => SelfStatus::VoiceActivityWaiting,
        };
        Self {
            muted,
            deafened: intent.deafened,
            server_muted: intent.server_muted,
            input_available,
            input_level: if input_available && input_db.is_finite() {
                ((input_db + 60.0) / 60.0).clamp(0.0, 1.0)
            } else {
                0.0
            },
            transmitting,
            monitoring: intent.monitoring,
            ptt_mode,
            ptt_bound,
            status,
        }
    }

    pub fn status_label(&self) -> &'static str {
        self.status.label()
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use voice_types::VoiceStats;

    #[test]
    fn a_failed_speaker_does_not_lie_about_sending() {
        let snapshot = RuntimeSnapshot {
            stage: RuntimeStage::Failed,
            voice: Some(VoiceStats {
                input_available: true,
                input_db: -20.0,
                transmitting: true,
                render_error: Some("speaker unplugged".into()),
                error: Some("speaker unplugged".into()),
                ..Default::default()
            }),
            ..Default::default()
        };
        let view = SelfStateView::project(&snapshot, ConnectionState::Connected, true);
        assert_eq!(view.status, SelfStatus::OutputFailed);
        assert!(view.transmitting);
        assert!(view.input_available);
    }

    #[test]
    fn closed_mic_keeps_live_input_and_forced_mute_keeps_user_intent() {
        let mut snapshot = RuntimeSnapshot {
            stage: RuntimeStage::Ready,
            voice: Some(VoiceStats {
                input_available: true,
                input_db: -24.0,
                transmitting: true,
                ..Default::default()
            }),
            ..Default::default()
        };
        snapshot.intent.muted = true;
        let view = SelfStateView::project(&snapshot, ConnectionState::Connected, true);
        assert!(view.input_available && view.input_level > 0.0);
        assert!(!view.transmitting);
        snapshot.intent.muted = false;
        snapshot.intent.server_muted = true;
        let view = SelfStateView::project(&snapshot, ConnectionState::Connected, true);
        assert_eq!(view.status, SelfStatus::ServerMuted);
        assert!(!snapshot.intent.muted);
    }

    #[test]
    fn disconnected_sessions_cannot_reuse_stale_sending() {
        let snapshot = RuntimeSnapshot {
            voice: Some(VoiceStats {
                transmitting: true,
                ..Default::default()
            }),
            ..Default::default()
        };
        for connection in [
            ConnectionState::Offline,
            ConnectionState::Connecting,
            ConnectionState::Reconnecting,
        ] {
            assert!(!SelfStateView::project(&snapshot, connection, true).transmitting);
        }
    }

    #[test]
    fn preparation_waits_for_devices_and_probe_but_keeps_actual_sending_visible() {
        let mut snapshot = RuntimeSnapshot {
            stage: RuntimeStage::Starting,
            voice: Some(VoiceStats::default()),
            ..Default::default()
        };
        assert_eq!(
            SelfStateView::project(&snapshot, ConnectionState::Connected, true).status,
            SelfStatus::Preparing
        );
        snapshot.voice.as_mut().unwrap().input_available = true;
        assert_eq!(
            SelfStateView::project(&snapshot, ConnectionState::Connected, true).status,
            SelfStatus::Preparing
        );
        snapshot.voice.as_mut().unwrap().transmitting = true;
        let view = SelfStateView::project(&snapshot, ConnectionState::Connected, true);
        assert_eq!(view.status, SelfStatus::Sending);
        assert!(view.transmitting);
    }
}
