// SPDX-License-Identifier: GPL-3.0-or-later
//! The client voice lifecycle, without a GUI toolkit or platform device API.
//!
//! [`VoiceRuntime`] owns the lifecycle worker; [`RuntimeHandle`] only shares fast
//! controls and snapshots. Dropping a GUI handle never joins audio threads.
//! An audio backend opens, uses and drops streams on the audio thread itself.

pub mod call;
pub mod health;
pub mod recovery;
mod runtime;
pub mod self_state;

pub use runtime::*;
