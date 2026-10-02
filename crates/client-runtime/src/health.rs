// SPDX-License-Identifier: GPL-3.0-or-later
//! Connection recovery coordination is independent of window visibility.
use crate::{
    recovery::{Action, Recovery},
    RuntimeSnapshot, RuntimeStage,
};
use std::time::Duration;

#[derive(Default)]
pub struct CallHealth {
    recovery: Recovery,
}

pub struct HealthUpdate {
    pub action: Option<Action>,
    /// `Some("")` clears a recovered notice; `None` leaves the current notice.
    pub notice: Option<&'static str>,
    pub healthy: bool,
    pub device_error: bool,
}

impl CallHealth {
    pub fn interrupt(&mut self, now: Duration) -> bool {
        self.recovery.interrupt(now)
    }

    pub fn tick(
        &mut self,
        now: Duration,
        snapshot: &RuntimeSnapshot,
        reconnecting: bool,
    ) -> HealthUpdate {
        let stats = snapshot.voice.as_ref();
        let device_error =
            stats.is_some_and(|s| s.capture_error.is_some() || s.render_error.is_some());
        let healthy = !reconnecting
            && stats.is_some_and(|s| {
                s.udp_ok
                    && s.input_available
                    && s.render_available
                    && s.error.is_none()
                    && !s.sequences_exhausted
            });
        let pending = matches!(
            snapshot.stage,
            RuntimeStage::Preparing | RuntimeStage::Starting | RuntimeStage::Stopping
        );
        let failed = !pending
            && (snapshot.error.is_some()
                || device_error
                || stats.is_some_and(|s| {
                    s.udp_failed || s.transport_error.is_some() || s.sequences_exhausted
                }));
        let action = if reconnecting {
            None
        } else {
            self.recovery.tick(now, healthy, failed, device_error)
        };
        let playback_only = stats.is_some_and(|s| {
            s.render_error.is_some()
                && s.capture_error.is_none()
                && s.transport_error.is_none()
                && s.input_available
                && s.udp_ok
                && !s.udp_failed
                && !s.sequences_exhausted
        });
        let notice = match action {
            Some(Action::Lost) if playback_only => {
                Some("播放已中断，正在恢复；麦克风仍可能在发送。")
            }
            Some(Action::Lost) => Some("语音已中断，正在自动恢复；你的话可能无法送达。"),
            Some(Action::Recovered) => Some(""),
            Some(Action::RetryVoice) if playback_only => {
                Some("正在重启语音以恢复播放，麦克风发送也会短暂中断。")
            }
            Some(Action::RetryVoice) => Some("正在重试语音，恢复后会播放提示音。"),
            Some(Action::Reconnect) => Some("语音持续异常，正在重新连接服务器。"),
            None => None,
        };
        HealthUpdate {
            action,
            notice,
            healthy,
            device_error,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use voice_core::pipeline::VoiceStats;

    #[test]
    fn playback_failure_notice_preserves_the_direction_of_the_failure() {
        let mut health = CallHealth::default();
        let mut snapshot = RuntimeSnapshot {
            stage: RuntimeStage::Failed,
            voice: Some(VoiceStats {
                udp_ok: true,
                input_available: true,
                transmitting: true,
                render_error: Some("speaker failed".into()),
                error: Some("speaker failed".into()),
                ..Default::default()
            }),
            ..Default::default()
        };
        let lost = health.tick(Duration::ZERO, &snapshot, false);
        assert_eq!(lost.action, Some(Action::Lost));
        assert!(lost.notice.unwrap().contains("麦克风仍可能在发送"));
        let retry = health.tick(Duration::from_secs(1), &snapshot, false);
        assert_eq!(retry.action, Some(Action::RetryVoice));
        assert!(retry.notice.unwrap().contains("发送也会短暂中断"));
        snapshot.voice.as_mut().unwrap().capture_error = Some("mic failed too".into());
        let mut another = CallHealth::default();
        assert!(another
            .tick(Duration::ZERO, &snapshot, false)
            .notice
            .unwrap()
            .contains("无法送达"));
    }

    #[test]
    fn pending_replacement_is_not_treated_as_another_failed_probe() {
        let mut health = CallHealth::default();
        let mut snapshot = RuntimeSnapshot {
            stage: RuntimeStage::Failed,
            error: Some("cannot resolve host".into()),
            ..Default::default()
        };
        assert_eq!(
            health.tick(Duration::ZERO, &snapshot, false).action,
            Some(Action::Lost)
        );
        snapshot.stage = RuntimeStage::Preparing;
        assert_eq!(
            health
                .tick(Duration::from_secs(10), &snapshot, false)
                .action,
            None
        );
        snapshot.stage = RuntimeStage::Failed;
        assert_eq!(
            health
                .tick(Duration::from_secs(11), &snapshot, false)
                .action,
            Some(Action::RetryVoice)
        );
    }

    #[test]
    fn recovery_waits_for_both_devices_and_the_authenticated_udp_probe() {
        let mut health = CallHealth::default();
        assert!(health.interrupt(Duration::ZERO));
        let mut snapshot = RuntimeSnapshot {
            stage: RuntimeStage::Starting,
            voice: Some(VoiceStats {
                udp_ok: true,
                input_available: true,
                ..Default::default()
            }),
            ..Default::default()
        };
        assert!(
            !health
                .tick(Duration::from_secs(1), &snapshot, false)
                .healthy
        );
        snapshot.voice.as_mut().unwrap().render_available = true;
        assert_eq!(
            health.tick(Duration::from_secs(2), &snapshot, false).action,
            Some(Action::Recovered)
        );
    }
}
