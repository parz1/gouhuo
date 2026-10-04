// SPDX-License-Identifier: GPL-3.0-or-later
//! Compile-time trust pin; no unsigned configuration can replace this key.

use client_process::{
    update::{unhex, Compatibility, EngineStore},
    VoiceRuntime,
};
use std::{io, sync::Arc};

pub fn runtime() -> io::Result<VoiceRuntime> {
    let binary = std::env::current_exe()?.with_file_name(if cfg!(windows) {
        "gouhuo-voice.exe"
    } else {
        "gouhuo-voice"
    });
    let Some(key) = option_env!("GOUHUO_VOICE_PUBLIC_KEY") else {
        return VoiceRuntime::new(binary);
    };
    let root = voice_core::identity::Identity::default_path()?
        .parent()
        .ok_or_else(|| io::Error::other("invalid application directory"))?
        .join("voice-engines");
    let store = EngineStore::new(
        root,
        unhex::<32>(key)?,
        Compatibility {
            target: if cfg!(windows) {
                "windows-x64"
            } else {
                "linux-x64"
            }
            .into(),
            ipc: client_runtime::ipc::VERSION,
            server_protocol: protocol::control::PROTOCOL_VERSION,
            ui: env!("CARGO_PKG_VERSION").into(),
            bundled: option_env!("GOUHUO_BUNDLED_VOICE_VERSION")
                .unwrap_or("0.1.0")
                .into(),
        },
    )?;
    let store = Arc::new(store);
    let runtime = VoiceRuntime::managed(binary, Arc::clone(&store))?;
    let handle = runtime.handle();
    // Staging is offline/background work. This observer does no IO on the GUI
    // thread and cannot activate during a call, mic check or pending scan.
    std::thread::Builder::new()
        .name("voice-update-activation".into())
        .spawn(move || {
            while !handle.is_closed() {
                if store.has_pending().unwrap_or(false) {
                    handle.activate_pending();
                }
                std::thread::sleep(std::time::Duration::from_secs(30));
            }
        })?;
    Ok(runtime)
}
