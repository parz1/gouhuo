// SPDX-License-Identifier: GPL-3.0-or-later
//! Exercise the real Opus/encrypted UDP pipeline through controlled audio
//! factories. The device boundary is fake; the lifecycle and network are real.
use std::io;
use std::net::UdpSocket;
use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::{mpsc, Arc, Mutex};
use std::thread::ThreadId;
use std::time::{Duration, Instant};

use client_runtime::{
    AudioBackend, Devices, OpenedCapture, RuntimeHandle, RuntimeStage, StartVoice, VoiceRuntime,
};
use protocol::VoiceCipher;
use voice_core::audio::{Capture, Render};
use voice_core::pipeline::{AudioProcessor, TransmitMode};

struct Gate {
    entered: mpsc::Sender<()>,
    release: Mutex<mpsc::Receiver<()>>,
}

struct Backend {
    processor_calls: AtomicUsize,
    gate: Option<Gate>,
    capture_opens: Arc<AtomicUsize>,
    capture_drops: Arc<AtomicUsize>,
    render_drops: Arc<AtomicUsize>,
}

impl Backend {
    fn plain() -> Self {
        Self {
            processor_calls: AtomicUsize::new(0),
            gate: None,
            capture_opens: Arc::new(AtomicUsize::new(0)),
            capture_drops: Arc::new(AtomicUsize::new(0)),
            render_drops: Arc::new(AtomicUsize::new(0)),
        }
    }

    fn gated() -> (Self, mpsc::Receiver<()>, mpsc::Sender<()>) {
        let (entered, wait) = mpsc::channel();
        let (release, released) = mpsc::channel();
        let mut backend = Self::plain();
        backend.gate = Some(Gate {
            entered,
            release: Mutex::new(released),
        });
        (backend, wait, release)
    }
}

struct LoudCapture {
    owner: ThreadId,
    drops: Arc<AtomicUsize>,
}

impl Capture for LoudCapture {
    fn read(&mut self, out: &mut [f32]) -> io::Result<bool> {
        assert_eq!(self.owner, std::thread::current().id());
        std::thread::sleep(Duration::from_millis(10));
        for (i, sample) in out.iter_mut().enumerate() {
            *sample = (i as f32 * 0.17).sin() * 0.2;
        }
        Ok(true)
    }
}

impl Drop for LoudCapture {
    fn drop(&mut self) {
        assert_eq!(
            self.owner,
            std::thread::current().id(),
            "device was moved before destruction"
        );
        self.drops.fetch_add(1, Ordering::SeqCst);
    }
}

struct PacedRender {
    owner: ThreadId,
    drops: Arc<AtomicUsize>,
}

impl Render for PacedRender {
    fn write(&mut self, _frame: &[f32]) -> io::Result<()> {
        assert_eq!(self.owner, std::thread::current().id());
        std::thread::sleep(Duration::from_millis(10));
        Ok(())
    }
}

impl Drop for PacedRender {
    fn drop(&mut self) {
        assert_eq!(
            self.owner,
            std::thread::current().id(),
            "device was moved before destruction"
        );
        self.drops.fetch_add(1, Ordering::SeqCst);
    }
}

impl AudioBackend for Backend {
    fn capture(&self, _device: Option<&str>) -> io::Result<OpenedCapture> {
        self.capture_opens.fetch_add(1, Ordering::SeqCst);
        Ok(OpenedCapture::new(Box::new(LoudCapture {
            owner: std::thread::current().id(),
            drops: Arc::clone(&self.capture_drops),
        })))
    }

    fn render(&self, _device: Option<&str>) -> io::Result<Box<dyn Render>> {
        Ok(Box::new(PacedRender {
            owner: std::thread::current().id(),
            drops: Arc::clone(&self.render_drops),
        }))
    }

    fn processor(&self) -> Option<Box<dyn AudioProcessor>> {
        if self.processor_calls.fetch_add(1, Ordering::SeqCst) == 0 {
            if let Some(gate) = &self.gate {
                gate.entered.send(()).unwrap();
                gate.release.lock().unwrap().recv().unwrap();
            }
        }
        None
    }
}

fn request(socket: &UdpSocket, session_id: u32) -> StartVoice {
    StartVoice {
        host: "127.0.0.1".into(),
        udp_port: socket.local_addr().unwrap().port(),
        session_id,
        sequences: Arc::new(protocol::VoiceSequences::default()),
        upstream_key: [1; 32],
        downstream_key: [2; 32],
        devices: Devices::default(),
    }
}

fn socket() -> UdpSocket {
    let socket = UdpSocket::bind("127.0.0.1:0").unwrap();
    socket
        .set_read_timeout(Some(Duration::from_millis(15)))
        .unwrap();
    socket
}

fn wait(description: &str, condition: impl Fn() -> bool) {
    let deadline = Instant::now() + Duration::from_secs(3);
    while !condition() {
        assert!(
            Instant::now() < deadline,
            "timed out waiting for {description}"
        );
        std::thread::sleep(Duration::from_millis(5));
    }
}

fn received(socket: &UdpSocket, window: Duration) -> Vec<protocol::VoiceHeader> {
    let cipher = VoiceCipher::new(&[1; 32]);
    let until = Instant::now() + window;
    let mut headers = Vec::new();
    let mut packet = [0; protocol::MAX_DATAGRAM];
    while Instant::now() < until {
        match socket.recv(&mut packet) {
            Ok(n) => {
                let mut payload = Vec::new();
                let header = cipher.open(&packet[..n], &mut payload).unwrap();
                headers.push(header);
            }
            Err(error)
                if matches!(
                    error.kind(),
                    io::ErrorKind::WouldBlock | io::ErrorKind::TimedOut
                ) => {}
            Err(error) => panic!("UDP receive failed: {error}"),
        }
    }
    headers
}

fn drain(runtime: &VoiceRuntime, handle: &RuntimeHandle) {
    handle.shutdown();
    assert!(
        runtime.wait_stopped(Duration::from_secs(3)),
        "worker did not drain"
    );
}

#[test]
fn a_cancelled_preparation_never_opens_or_sends_old_voice_material() {
    let (backend, entered, release) = Backend::gated();
    let backend = Arc::new(backend);
    let runtime = VoiceRuntime::new(backend.clone()).unwrap();
    let handle = runtime.handle();
    handle.set_mode(TransmitMode::Always);
    let socket = socket();
    handle.start_voice(request(&socket, 111));
    entered.recv_timeout(Duration::from_secs(3)).unwrap();
    let live_id = handle.start_voice(request(&socket, 222));
    handle.set_muted(true);
    assert_eq!(handle.snapshot().request_id, live_id);
    assert_eq!(backend.capture_opens.load(Ordering::SeqCst), 0);
    release.send(()).unwrap();
    wait("live capture", || {
        handle.snapshot().voice.is_some_and(|s| s.input_available)
    });
    assert_eq!(handle.snapshot().session_id, Some(222));
    assert_eq!(backend.capture_opens.load(Ordering::SeqCst), 1);
    assert!(received(&socket, Duration::from_millis(150))
        .iter()
        .all(|h| h.session == 222 && h.is_keepalive()));
    handle.set_muted(false);
    let headers = received(&socket, Duration::from_millis(120));
    assert!(headers
        .iter()
        .any(|h| h.session == 222 && !h.is_keepalive()));
    assert!(headers.iter().all(|h| h.session != 111));
    drain(&runtime, &handle);
    assert_eq!(backend.capture_drops.load(Ordering::SeqCst), 1);
    assert_eq!(backend.render_drops.load(Ordering::SeqCst), 1);
}

#[test]
fn pending_closed_mic_is_applied_before_the_first_audio_frame() {
    let (backend, entered, release) = Backend::gated();
    let runtime = VoiceRuntime::new(Arc::new(backend)).unwrap();
    let handle = runtime.handle();
    let socket = socket();
    handle.set_mode(TransmitMode::Always);
    handle.start_voice(request(&socket, 1));
    entered.recv_timeout(Duration::from_secs(3)).unwrap();
    // These commands finish while APM initialization is still blocked.
    handle.set_deafened(true);
    handle.set_muted(false); // Closed sound cannot be bypassed by opening mic.
    handle.set_deafened(false); // Reopening sound preserves closed mic.
    assert!(handle.snapshot().intent.muted);
    release.send(()).unwrap();
    wait("input level while muted", || {
        handle
            .snapshot()
            .voice
            .is_some_and(|s| s.input_available && s.input_db > -50.0)
    });
    assert!(!handle.snapshot().voice.unwrap().transmitting);
    assert!(received(&socket, Duration::from_millis(150))
        .iter()
        .all(|h| h.is_keepalive()));
    handle.set_muted(false);
    assert!(received(&socket, Duration::from_millis(120))
        .iter()
        .any(|h| !h.is_keepalive()));
    drain(&runtime, &handle);
}

#[test]
fn cancelling_a_slow_initialization_returns_without_waiting_for_it() {
    let (backend, entered, release) = Backend::gated();
    let backend = Arc::new(backend);
    let runtime = VoiceRuntime::new(backend.clone()).unwrap();
    let handle = runtime.handle();
    let socket = socket();
    handle.start_voice(request(&socket, 3));
    entered.recv_timeout(Duration::from_secs(3)).unwrap();
    let (done, completed) = mpsc::channel();
    let gui = handle.clone();
    std::thread::spawn(move || {
        gui.stop();
        done.send(()).unwrap();
    });
    completed
        .recv_timeout(Duration::from_secs(1))
        .expect("GUI waited for the initializing worker");
    assert_eq!(handle.snapshot().stage, RuntimeStage::Stopping);
    assert!(handle.snapshot().voice.is_none());
    release.send(()).unwrap();
    wait("cancelled state drained", || {
        handle.snapshot().stage == RuntimeStage::Idle
    });
    assert_eq!(backend.capture_opens.load(Ordering::SeqCst), 0);
    assert!(received(&socket, Duration::from_millis(80)).is_empty());
    drain(&runtime, &handle);
}

#[test]
fn ptt_release_and_forced_mute_apply_without_worker_or_gui_ownership() {
    let runtime = VoiceRuntime::new(Arc::new(Backend::plain())).unwrap();
    let handle = runtime.handle();
    let socket = socket();
    handle.set_mode(TransmitMode::PushToTalk);
    handle.set_transmitting(true);
    handle.start_voice(request(&socket, 5));
    wait("PTT sending", || {
        handle.snapshot().voice.is_some_and(|s| s.transmitting)
    });
    handle.set_transmitting(false);
    assert!(
        !handle.snapshot().voice.unwrap().transmitting,
        "PTT release waited for worker synchronization"
    );
    handle.set_transmitting(true);
    wait("PTT resumed", || {
        handle.snapshot().voice.is_some_and(|s| s.transmitting)
    });
    handle.set_server_muted(true);
    assert!(!handle.snapshot().voice.unwrap().transmitting);
    assert!(
        !handle.snapshot().intent.muted,
        "moderator constraint overwrote user intent"
    );
    handle.set_server_muted(false);
    wait("PTT resumes after moderator clears mute", || {
        handle.snapshot().voice.is_some_and(|s| s.transmitting)
    });
    drain(&runtime, &handle);
}

#[test]
fn delayed_retirement_cannot_stop_a_newer_voice_or_microphone_check() {
    let runtime = VoiceRuntime::new(Arc::new(Backend::plain())).unwrap();
    let handle = runtime.handle();
    let socket = socket();
    handle.set_mode(TransmitMode::Always);
    handle.start_voice(request(&socket, 7));
    wait("first live capture", || {
        handle.snapshot().voice.is_some_and(|s| s.input_available)
    });
    handle.stop_after(Duration::from_millis(100));
    let live_id = handle.start_mic_check(Devices::default());
    wait("new microphone check", || {
        handle.snapshot().mic.is_some_and(|s| s.input_available)
    });
    std::thread::sleep(Duration::from_millis(180));
    assert_eq!(handle.snapshot().request_id, live_id);
    assert_eq!(handle.snapshot().stage, RuntimeStage::Ready);
    assert!(handle.snapshot().mic.unwrap().input_available);
    assert!(handle.snapshot().voice.is_none());
    handle.set_monitoring(true);
    assert!(handle.snapshot().intent.monitoring);
    drain(&runtime, &handle);
}

#[test]
fn stage_timings_measure_worker_preparation_and_successful_device_frames() {
    let (backend, entered, release) = Backend::gated();
    let runtime = VoiceRuntime::new(Arc::new(backend)).unwrap();
    let handle = runtime.handle();
    let socket = socket();
    handle.start_voice(request(&socket, 9));
    entered.recv_timeout(Duration::from_secs(3)).unwrap();
    assert!(handle.snapshot().timings.first_capture_ms.is_none());
    std::thread::sleep(Duration::from_millis(40));
    release.send(()).unwrap();
    wait("device first-frame timestamps", || {
        let timings = handle.snapshot().timings;
        timings.first_capture_ms.is_some() && timings.first_render_ms.is_some()
    });
    let timings = handle.snapshot().timings;
    assert!(timings.processor_ms.unwrap() >= 40.0);
    assert!(timings.first_capture_ms.unwrap() >= timings.processor_ms.unwrap());
    assert!(timings.first_render_ms.unwrap() >= timings.processor_ms.unwrap());
    assert!(timings.queued_ms.is_some() && timings.retire_ms.is_some());
    assert!(timings.dns_ms.is_some() && timings.start_ms.is_some());
    // No peer has replied: do not invent successful network recovery timing.
    assert!(timings.first_udp_ms.is_none());
    drain(&runtime, &handle);
}
