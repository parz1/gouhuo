// SPDX-License-Identifier: GPL-3.0-or-later
//! Frontend data and controls, independent of the in-process audio worker.

use voice_types::{TransmitMode, VoiceStats};

/// Controls used by the call controller, implemented by the current worker or
/// the process proxy. Mutations update local intent before returning so a
/// following snapshot cannot rehydrate an old engine echo. Implementations must
/// keep mute, PTT release and stop independent of slow device preparation.
///
/// This Rust interface is implemented on both sides of the process boundary.
/// IPC startup transfers keys; sequence allocation remains owned by the engine.
pub trait VoiceControl: Send + Sync {
    fn snapshot(&self) -> RuntimeSnapshot;
    fn set_muted(&self, on: bool);
    fn set_deafened(&self, on: bool);
    fn set_transmitting(&self, on: bool);
    fn set_volume(&self, member: u32, volume: f32);
    fn stop(&self);

    fn mode(&self) -> TransmitMode {
        self.snapshot().intent.mode
    }
}

/// Opaque platform device identifiers. `None` follows the system default.
#[cfg_attr(feature = "ipc", derive(serde::Serialize, serde::Deserialize))]
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct Devices {
    pub capture: Option<String>,
    pub render: Option<String>,
}

/// Only portable diagnostic data crosses the platform boundary.
#[cfg_attr(feature = "ipc", derive(serde::Serialize, serde::Deserialize))]
#[derive(Debug, Clone, Default, PartialEq)]
pub struct CaptureInfo {
    pub opened: bool,
    pub name: String,
    pub is_virtual: bool,
    pub silent_ratio: f32,
}

#[cfg_attr(feature = "ipc", derive(serde::Serialize, serde::Deserialize))]
#[derive(Debug, Clone, PartialEq)]
pub struct VoiceIntent {
    pub muted: bool,
    /// Moderator-enforced mute is a constraint, never a change to local intent.
    pub server_muted: bool,
    pub deafened: bool,
    pub transmitting: bool,
    pub monitoring: bool,
    pub mode: TransmitMode,
}

impl Default for VoiceIntent {
    fn default() -> Self {
        Self {
            muted: false,
            server_muted: false,
            deafened: false,
            transmitting: false,
            monitoring: false,
            mode: TransmitMode::PushToTalk,
        }
    }
}

#[cfg_attr(feature = "ipc", derive(serde::Serialize, serde::Deserialize))]
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub enum RuntimeStage {
    #[default]
    Idle,
    Preparing,
    Starting,
    Ready,
    Failed,
    Stopping,
}

#[cfg_attr(feature = "ipc", derive(serde::Serialize, serde::Deserialize))]
#[derive(Debug, Clone, Default, PartialEq)]
pub struct MicSnapshot {
    pub input_db: f32,
    pub input_available: bool,
    pub error: Option<String>,
}

/// Measured stage durations, in milliseconds. First capture/render timings
/// come from successful device I/O on the audio threads. First UDP health is
/// observed by the worker, with up to one polling interval (20 ms) of delay.
#[cfg_attr(feature = "ipc", derive(serde::Serialize, serde::Deserialize))]
#[derive(Debug, Clone, Default, PartialEq)]
pub struct RuntimeTimings {
    pub queued_ms: Option<f64>,
    pub retire_ms: Option<f64>,
    pub dns_ms: Option<f64>,
    pub processor_ms: Option<f64>,
    pub start_ms: Option<f64>,
    /// Time from request submission to first successful input frame.
    pub first_capture_ms: Option<f64>,
    /// Time from request submission to first successful output frame.
    pub first_render_ms: Option<f64>,
    /// Time from request submission to the first observed authenticated probe.
    pub first_udp_ms: Option<f64>,
}

#[cfg_attr(feature = "ipc", derive(serde::Serialize, serde::Deserialize))]
#[derive(Debug, Clone, Default, PartialEq)]
pub struct RuntimeSnapshot {
    pub request_id: u64,
    pub stage: RuntimeStage,
    pub session_id: Option<u32>,
    pub voice: Option<VoiceStats>,
    pub mic: Option<MicSnapshot>,
    pub intent: VoiceIntent,
    pub capture: CaptureInfo,
    /// Preparation failure (DNS, socket/codec/thread initialization), distinct
    /// from the directional live-device errors inside `voice`.
    pub error: Option<String>,
    pub timings: RuntimeTimings,
}
