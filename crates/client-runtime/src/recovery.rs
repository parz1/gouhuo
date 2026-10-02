// SPDX-License-Identifier: GPL-3.0-or-later
//! Recovery policy runs even when the window is hidden. Time is supplied by the caller.
use std::time::Duration;

#[derive(Debug, PartialEq, Eq)]
pub enum Action {
    Lost,
    Recovered,
    RetryVoice,
    Reconnect,
}

#[derive(Default)]
pub struct Recovery {
    interrupted: bool,
    attempts: u32,
    retry_at: Duration,
    reconnect_after: Duration,
}

impl Recovery {
    pub fn interrupt(&mut self, now: Duration) -> bool {
        if self.interrupted {
            return false;
        }
        self.interrupted = true;
        self.retry_at = now + Duration::from_secs(1);
        true
    }

    pub fn tick(
        &mut self,
        now: Duration,
        healthy: bool,
        failed: bool,
        device_error: bool,
    ) -> Option<Action> {
        if healthy {
            let recovered = self.interrupted;
            *self = Self::default();
            return recovered.then_some(Action::Recovered);
        }
        if !failed {
            return None; // A fresh transport must finish probing before another retry.
        }
        if self.interrupt(now) {
            return Some(Action::Lost);
        }
        if now < self.retry_at {
            return None;
        }
        self.attempts = self.attempts.saturating_add(1);
        self.retry_at = now + Duration::from_secs((1u64 << self.attempts.min(5)).min(30));
        if !device_error && self.attempts >= 3 && now >= self.reconnect_after {
            self.reconnect_after = now + Duration::from_secs(30);
            Some(Action::Reconnect)
        } else {
            Some(Action::RetryVoice)
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    fn t(s: u64) -> Duration {
        Duration::from_secs(s)
    }

    #[test]
    fn outage_notifies_once_and_success_waits_for_voice() {
        let mut r = Recovery::default();
        assert_eq!(r.tick(t(8), false, true, false), Some(Action::Lost));
        assert_eq!(r.tick(t(8), false, true, false), None);
        assert_eq!(r.tick(t(9), false, true, false), Some(Action::RetryVoice));
        assert_eq!(r.tick(t(10), false, false, false), None);
        assert!(!r.interrupt(t(11))); // TCP reconnect is part of the same outage.
        assert_eq!(r.tick(t(12), false, false, false), None);
        assert_eq!(r.tick(t(13), true, false, false), Some(Action::Recovered));
        assert_eq!(r.tick(t(14), true, false, false), None);
    }

    #[test]
    fn repeated_udp_failure_escalates_with_a_cooldown() {
        let mut r = Recovery::default();
        assert_eq!(r.tick(t(8), false, true, false), Some(Action::Lost));
        assert_eq!(r.tick(t(9), false, true, false), Some(Action::RetryVoice));
        assert_eq!(r.tick(t(17), false, true, false), Some(Action::RetryVoice));
        assert_eq!(r.tick(t(25), false, true, false), Some(Action::Reconnect));
        assert_eq!(r.tick(t(33), false, true, false), Some(Action::RetryVoice));
        assert_eq!(r.tick(t(40), false, true, false), None);
        assert_eq!(r.tick(t(49), false, true, false), Some(Action::RetryVoice));
        assert_eq!(r.tick(t(79), false, true, false), Some(Action::Reconnect));
    }

    #[test]
    fn device_failure_never_reconnects_the_server() {
        let mut r = Recovery::default();
        assert_eq!(r.tick(t(0), false, true, true), Some(Action::Lost));
        for s in [1, 4, 9, 18, 35, 66] {
            assert_eq!(r.tick(t(s), false, true, true), Some(Action::RetryVoice));
        }
        assert_eq!(r.tick(t(67), true, false, false), Some(Action::Recovered));
    }
}
