// SPDX-License-Identifier: GPL-3.0-or-later
//! Call behavior through a proxy-shaped control implementation, without a
//! VoiceRuntime, Pipeline, audio backend or device lifecycle in the frontend.

use std::sync::Mutex;

use client_runtime::call::{CallController, CommandResult, IgnoreReason};
use client_runtime::self_state::{ConnectionState, SelfStateView, SelfStatus};
use client_runtime::{RuntimeSnapshot, RuntimeStage, VoiceControl};
use voice_types::{TransmitMode, VoiceStats};

#[derive(Default)]
struct Proxy {
    cached: Mutex<RuntimeSnapshot>,
    presses: Mutex<Vec<bool>>,
}

impl VoiceControl for Proxy {
    fn snapshot(&self) -> RuntimeSnapshot {
        self.cached.lock().unwrap().clone()
    }

    fn set_transmitting(&self, on: bool) {
        self.cached.lock().unwrap().intent.transmitting = on;
        self.presses.lock().unwrap().push(on);
    }

    fn set_muted(&self, _: bool) {
        panic!("unexpected mute command");
    }

    fn set_deafened(&self, _: bool) {
        panic!("unexpected deafen command");
    }

    fn set_volume(&self, _: u32, _: f32) {
        panic!("unexpected volume command");
    }

    fn stop(&self) {
        panic!("unexpected stop command");
    }
}

#[test]
fn ptt_releases_a_proxy_on_reconnect_and_in_vad_mode() {
    let proxy = Proxy::default();
    let control: &dyn VoiceControl = &proxy;
    assert_eq!(
        CallController::set_ptt(control, ConnectionState::Connected, true),
        CommandResult::Applied
    );
    assert!(control.snapshot().intent.transmitting);
    assert_eq!(
        CallController::set_ptt(control, ConnectionState::Reconnecting, true),
        CommandResult::Ignored(IgnoreReason::NotConnected)
    );
    assert!(!control.snapshot().intent.transmitting);
    proxy.cached.lock().unwrap().intent.mode = TransmitMode::VoiceActivity {
        threshold_db: -45.0,
    };
    assert_eq!(
        CallController::set_ptt(control, ConnectionState::Connected, true),
        CommandResult::Ignored(IgnoreReason::NotPushToTalk)
    );
    assert_eq!(
        CallController::set_ptt(control, ConnectionState::Offline, false),
        CommandResult::Applied
    );
    assert_eq!(*proxy.presses.lock().unwrap(), [true, false, false, false]);
}

#[test]
fn a_proxy_press_is_not_projected_as_successful_voice_transmission() {
    let proxy = Proxy::default();
    {
        let mut cached = proxy.cached.lock().unwrap();
        cached.stage = RuntimeStage::Ready;
        cached.voice = Some(VoiceStats {
            input_available: true,
            render_available: true,
            udp_ok: true,
            ..Default::default()
        });
    }
    let control: &dyn VoiceControl = &proxy;
    CallController::set_ptt(control, ConnectionState::Connected, true);
    let view = SelfStateView::project(&control.snapshot(), ConnectionState::Connected, true);
    assert!(
        !view.transmitting,
        "intent cannot impersonate an engine fact"
    );
    proxy
        .cached
        .lock()
        .unwrap()
        .voice
        .as_mut()
        .unwrap()
        .transmitting = true;
    let view = SelfStateView::project(&control.snapshot(), ConnectionState::Connected, true);
    assert!(view.transmitting);
    let view = SelfStateView::project(&control.snapshot(), ConnectionState::Reconnecting, true);
    assert!(!view.transmitting);
    assert_eq!(view.status, SelfStatus::Reconnecting);
}
