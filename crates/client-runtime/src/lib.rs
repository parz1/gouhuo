// SPDX-License-Identifier: GPL-3.0-or-later
//! The client voice lifecycle, without a GUI toolkit or platform device API.
//!
//! With the default `in-process` feature, the runtime owns a lifecycle worker
//! and device objects stay on their audio threads. Without it, only frontend
//! data, call commands, projections and recovery policies are compiled.

mod api;
pub mod call;
pub mod health;
#[cfg(feature = "ipc")]
pub mod ipc;
pub mod recovery;
#[cfg(feature = "in-process")]
mod runtime;
pub mod self_state;

pub use api::*;
#[cfg(feature = "in-process")]
pub use runtime::*;
