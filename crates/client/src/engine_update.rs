// SPDX-License-Identifier: GPL-3.0-or-later
//! Compile-time trust pin; no unsigned configuration can replace this key.

use client_process::{
    update::{
        download::{check_and_stage, https::Https},
        unhex, Compatibility, EngineStore,
    },
    VoiceRuntime,
};
use std::{
    io,
    sync::{
        atomic::{AtomicBool, AtomicU64, Ordering},
        Arc,
    },
    time::{Duration, Instant},
};

#[derive(Clone)]
pub struct Controller {
    preference: Arc<Preference>,
    store: Option<Arc<EngineStore>>,
}
struct Preference {
    enabled: AtomicBool,
    epoch: AtomicU64,
}
impl Controller {
    pub fn set_enabled(&self, on: bool) {
        if let Some(store) = &self.store {
            store.set_activation_allowed(on);
        }
        if self.preference.enabled.swap(on, Ordering::AcqRel) != on {
            self.preference.epoch.fetch_add(1, Ordering::AcqRel);
        }
    }
}

fn retry_delay(failures: u32) -> Duration {
    Duration::from_secs(match failures {
        0 => 6 * 3600,
        1 => 60,
        2 => 300,
        3 => 1800,
        _ => 6 * 3600,
    })
}

pub fn runtime(enabled: bool) -> io::Result<(VoiceRuntime, Controller)> {
    let preference = Arc::new(Preference {
        enabled: AtomicBool::new(enabled),
        epoch: AtomicU64::new(0),
    });
    let binary = std::env::current_exe()?.with_file_name(if cfg!(windows) {
        "gouhuo-voice.exe"
    } else {
        "gouhuo-voice"
    });
    let Some(key) = option_env!("GOUHUO_VOICE_PUBLIC_KEY") else {
        return Ok((
            VoiceRuntime::new(binary)?,
            Controller {
                preference,
                store: None,
            },
        ));
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
    store.set_activation_allowed(enabled);
    let runtime = VoiceRuntime::managed(binary, Arc::clone(&store))?;
    let controller = Controller {
        preference: Arc::clone(&preference),
        store: Some(Arc::clone(&store)),
    };
    let handle = runtime.handle();
    // Discovery and staging run in the background, with no IO on the GUI
    // thread and cannot activate during a call, mic check or pending scan.
    std::thread::Builder::new()
        .name("voice-update-activation".into())
        .spawn(move || {
            let mut epoch = preference.epoch.load(Ordering::Acquire);
            let mut next_check = Instant::now() + Duration::from_secs(5);
            let mut next_activation = Instant::now();
            let mut failures = 0u32;
            while !handle.is_closed() {
                let current_epoch = preference.epoch.load(Ordering::Acquire);
                if epoch != current_epoch {
                    epoch = current_epoch;
                    failures = 0;
                    next_check = Instant::now() + Duration::from_secs(5);
                }
                let allowed = || {
                    !handle.is_closed()
                        && preference.enabled.load(Ordering::Acquire)
                        && preference.epoch.load(Ordering::Acquire) == epoch
                };
                if allowed() && Instant::now() >= next_check {
                    let result = check_and_stage(&mut Https, &store, &allowed);
                    failures = if result.is_ok() {
                        0
                    } else {
                        failures.saturating_add(1)
                    };
                    next_check = Instant::now() + retry_delay(failures);
                }
                if allowed()
                    && Instant::now() >= next_activation
                    && store.has_pending().unwrap_or(false)
                {
                    handle.activate_pending();
                }
                if Instant::now() >= next_activation {
                    next_activation = Instant::now() + Duration::from_secs(30);
                }
                std::thread::sleep(Duration::from_secs(1));
            }
        })?;
    Ok((runtime, controller))
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn preference_changes_invalidate_inflight_work_and_retries_back_off() {
        let preference = Arc::new(Preference {
            enabled: AtomicBool::new(true),
            epoch: AtomicU64::new(0),
        });
        let controller = Controller {
            preference: Arc::clone(&preference),
            store: None,
        };
        controller.set_enabled(false);
        controller.set_enabled(true);
        assert_eq!(preference.epoch.load(Ordering::Acquire), 2);
        assert!(preference.enabled.load(Ordering::Acquire));
        controller.set_enabled(true);
        assert_eq!(preference.epoch.load(Ordering::Acquire), 2);
        assert_eq!(retry_delay(1).as_secs(), 60);
        assert!(retry_delay(2) > retry_delay(1));
        assert_eq!(retry_delay(u32::MAX), retry_delay(4));
    }
}
