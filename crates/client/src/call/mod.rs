// SPDX-License-Identifier: GPL-3.0-or-later
//! Slint adaptation of the portable call state and commands.

mod adapter;

pub use adapter::SlintAdapter;

#[cfg(test)]
mod integration_tests;
