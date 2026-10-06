// SPDX-License-Identifier: GPL-3.0-or-later
#![cfg_attr(not(debug_assertions), windows_subsystem = "windows")]

use std::sync::Arc;

fn main() {
    let synthetic = std::env::args().skip(1).any(|arg| arg == "--synthetic");
    let backend: Arc<dyn client_runtime::AudioBackend> = if synthetic {
        Arc::new(voice_engine::SyntheticAudio)
    } else {
        match voice_engine::desktop_audio() {
            Ok(backend) => backend,
            Err(_) => std::process::exit(1),
        }
    };
    if voice_engine::serve(std::io::stdin(), std::io::stdout(), backend).is_err() {
        // IPC parse errors may contain session data. Never print raw payloads.
        std::process::exit(1);
    }
}
