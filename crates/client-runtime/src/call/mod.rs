// SPDX-License-Identifier: GPL-3.0-or-later
//! Portable call state, commands and presentation projections.
//!
//! A GUI adapts these ordinary Rust values to its models; it does not supply
//! connection flags back to the call state or decide whether audio is sending.

pub mod controller;
pub mod state;
pub mod viewmodel;

pub use controller::{CallCommand, CallController, CommandResult, IgnoreReason};
pub use state::{CallState, ConnectionPhase};
pub use viewmodel::{AudioViewModel, BanView, CallViewModel, ChatView, MemberView, RowView};
