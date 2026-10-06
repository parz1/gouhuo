// SPDX-License-Identifier: MIT OR Apache-2.0

//! Stable, metadata-only connection causes shared by both endpoints.
//! A local timeout describes an observation, never a diagnosis of the remote host.

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
#[cfg_attr(
    feature = "connection-serde",
    derive(serde::Serialize, serde::Deserialize)
)]
#[cfg_attr(feature = "connection-serde", serde(rename_all = "snake_case"))]
pub enum EvidenceSource {
    LocalObservation,
    ServerConfirmed,
    UserAction,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
#[cfg_attr(
    feature = "connection-serde",
    derive(serde::Serialize, serde::Deserialize)
)]
#[cfg_attr(feature = "connection-serde", serde(rename_all = "snake_case"))]
pub enum ConnectionReason {
    Unknown,
    UserLeft,
    JoinCancelled,
    ApplicationExit,
    ReconnectCancelled,
    Kicked,
    Banned,
    Displaced,
    InvalidInvite,
    InviteRequired,
    AuthenticationFailed,
    VersionMismatch,
    ServerFull,
    ServerInternal,
    ConnectionTimeout,
    ConnectionRefused,
    NetworkError,
    HeartbeatTimeout,
    RemoteClosed,
    ReadError,
    WriteError,
    TlsError,
    CertificateMismatch,
    ProtocolError,
    ControlBackpressure,
    ControlQueueUnavailable,
    TransportRestartRequested,
}

impl ConnectionReason {
    /// Stable export code. Never derive this from translated UI strings.
    pub const fn code(self) -> &'static str {
        match self {
            Self::Unknown => "unknown",
            Self::UserLeft => "user_left",
            Self::JoinCancelled => "join_cancelled",
            Self::ApplicationExit => "application_exit",
            Self::ReconnectCancelled => "reconnect_cancelled",
            Self::Kicked => "kicked",
            Self::Banned => "banned",
            Self::Displaced => "displaced",
            Self::InvalidInvite => "invalid_invite",
            Self::InviteRequired => "invite_required",
            Self::AuthenticationFailed => "authentication_failed",
            Self::VersionMismatch => "version_mismatch",
            Self::ServerFull => "server_full",
            Self::ServerInternal => "server_internal",
            Self::ConnectionTimeout => "connection_timeout",
            Self::ConnectionRefused => "connection_refused",
            Self::NetworkError => "network_error",
            Self::HeartbeatTimeout => "heartbeat_timeout",
            Self::RemoteClosed => "remote_closed",
            Self::ReadError => "read_error",
            Self::WriteError => "write_error",
            Self::TlsError => "tls_error",
            Self::CertificateMismatch => "certificate_mismatch",
            Self::ProtocolError => "protocol_error",
            Self::ControlBackpressure => "control_backpressure",
            Self::ControlQueueUnavailable => "control_queue_unavailable",
            Self::TransportRestartRequested => "transport_restart_requested",
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
#[cfg_attr(
    feature = "connection-serde",
    derive(serde::Serialize, serde::Deserialize)
)]
#[cfg_attr(feature = "connection-serde", serde(rename_all = "snake_case"))]
#[cfg_attr(feature = "connection-serde", serde(deny_unknown_fields))]
pub struct ConnectionCause {
    pub reason: ConnectionReason,
    pub source: EvidenceSource,
}

impl ConnectionCause {
    pub const fn local(reason: ConnectionReason) -> Self {
        Self {
            reason,
            source: EvidenceSource::LocalObservation,
        }
    }

    pub const fn server(reason: ConnectionReason) -> Self {
        Self {
            reason,
            source: EvidenceSource::ServerConfirmed,
        }
    }
}
