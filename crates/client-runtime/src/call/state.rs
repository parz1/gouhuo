// SPDX-License-Identifier: GPL-3.0-or-later
use crate::self_state::ConnectionState;

/// Control-connection lifecycle, independent of window properties and audio
/// preparation. Connected means authenticated; voice health is a separate fact.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub enum ConnectionPhase {
    #[default]
    Offline,
    Connecting,
    Connected,
    Reconnecting {
        attempt: u32,
        reason: String,
    },
}

/// Authoritative call-level state. Local audio intent belongs to RuntimeHandle;
/// server users, channels and permissions belong to client_core::Roster.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct CallState {
    pub phase: ConnectionPhase,
    pub voice_notice: String,
}

impl CallState {
    pub fn begin_join(&mut self) {
        self.phase = ConnectionPhase::Connecting;
        self.voice_notice.clear();
    }

    pub fn connected(&mut self) {
        self.phase = ConnectionPhase::Connected;
    }

    pub fn reconnecting(&mut self, attempt: u32, reason: impl Into<String>) {
        self.phase = ConnectionPhase::Reconnecting {
            attempt,
            reason: reason.into(),
        };
    }

    pub fn offline(&mut self) {
        self.phase = ConnectionPhase::Offline;
        self.voice_notice.clear();
    }

    pub fn set_notice(&mut self, notice: impl Into<String>) {
        self.voice_notice = notice.into();
    }

    pub fn connection(&self) -> ConnectionState {
        match self.phase {
            ConnectionPhase::Offline => ConnectionState::Offline,
            ConnectionPhase::Connecting => ConnectionState::Connecting,
            ConnectionPhase::Connected => ConnectionState::Connected,
            ConnectionPhase::Reconnecting { .. } => ConnectionState::Reconnecting,
        }
    }
}
