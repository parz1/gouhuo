// SPDX-License-Identifier: GPL-3.0-or-later
//! Exercise the real Opus/encrypted UDP pipeline through controlled audio
//! factories. The device boundary is fake; the lifecycle and network are real.
use std::io;
use std::net::UdpSocket;
use std::sync::atomic::{AtomicBool, AtomicUsize, Ordering};
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
        tunnel: None,
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

fn answer_probe(socket: &UdpSocket) {
    let mut packet = [0; protocol::MAX_DATAGRAM];
    if let Ok((n, peer)) = socket.recv_from(&mut packet) {
        let mut payload = Vec::new();
        let header = VoiceCipher::new(&[1; 32])
            .open(&packet[..n], &mut payload)
            .unwrap();
        if header.is_keepalive() {
            let mut reply = Vec::new();
            VoiceCipher::new(&[2; 32])
                .seal(header, &payload, &mut reply)
                .unwrap();
            socket.send_to(&reply, peer).unwrap();
        }
    }
}

fn drain(runtime: &VoiceRuntime, handle: &RuntimeHandle) {
    handle.shutdown();
    assert!(
        runtime.wait_stopped(Duration::from_secs(3)),
        "worker did not drain"
    );
}

struct PausedCapture {
    inner: Box<dyn Capture>,
    pause: Arc<AtomicBool>,
}
struct PausedRender {
    inner: Box<dyn Render>,
    pause: Arc<AtomicBool>,
}

fn pause_io(pause: &AtomicBool) {
    // Bound the fake driver so assertion failures never leave teardown stuck.
    let deadline = Instant::now() + Duration::from_secs(5);
    while pause.load(Ordering::SeqCst) && Instant::now() < deadline {
        std::thread::sleep(Duration::from_millis(5));
    }
}

impl Capture for PausedCapture {
    fn read(&mut self, out: &mut [f32]) -> io::Result<bool> {
        pause_io(&self.pause);
        self.inner.read(out)
    }
}
impl Render for PausedRender {
    fn write(&mut self, frame: &[f32]) -> io::Result<()> {
        pause_io(&self.pause);
        self.inner.write(frame)
    }
}

struct ResilienceBackend {
    inner: Backend,
    pause_capture: Arc<AtomicBool>,
    pause_render: Arc<AtomicBool>,
    fail_processor: AtomicBool,
    silence: AtomicBool,
    capture_ids: Mutex<Vec<Option<String>>>,
}

impl Default for ResilienceBackend {
    fn default() -> Self {
        Self {
            inner: Backend::plain(),
            pause_capture: Arc::new(AtomicBool::new(false)),
            pause_render: Arc::new(AtomicBool::new(false)),
            fail_processor: AtomicBool::new(false),
            silence: AtomicBool::new(false),
            capture_ids: Mutex::new(Vec::new()),
        }
    }
}

struct Passthrough;
impl AudioProcessor for Passthrough {
    fn process_capture(&self, _frame: &mut [f32]) {}
    fn analyze_render(&self, _frame: &mut [f32]) {}
}

impl AudioBackend for ResilienceBackend {
    fn capture(&self, device: Option<&str>) -> io::Result<OpenedCapture> {
        self.capture_ids
            .lock()
            .unwrap()
            .push(device.map(str::to_owned));
        if device == Some("missing") {
            return Err(io::Error::other("selected input disappeared"));
        }
        Ok(OpenedCapture::new(Box::new(PausedCapture {
            inner: if self.silence.load(Ordering::SeqCst) {
                Box::new(voice_core::audio::SyntheticCapture::new(Vec::new()).then_silence())
            } else {
                self.inner.capture(device)?.stream
            },
            pause: self.pause_capture.clone(),
        })))
    }
    fn render(&self, device: Option<&str>) -> io::Result<Box<dyn Render>> {
        Ok(Box::new(PausedRender {
            inner: self.inner.render(device)?,
            pause: self.pause_render.clone(),
        }))
    }
    fn try_processor(&self) -> io::Result<Option<Box<dyn AudioProcessor>>> {
        if self.fail_processor.load(Ordering::SeqCst) {
            Err(io::Error::other("APM initialization failed"))
        } else {
            Ok(Some(Box::new(Passthrough)))
        }
    }
}

#[test]
fn quiet_muted_mic_check_keeps_progress_and_reports_stalled_playback() {
    let backend = Arc::new(ResilienceBackend::default());
    backend.silence.store(true, Ordering::SeqCst);
    let runtime = VoiceRuntime::new(backend.clone()).unwrap();
    let handle = runtime.handle();
    handle.set_deafened(true);
    handle.start_mic_check(Devices::default());
    wait("more than two seconds of silent capture", || {
        handle.snapshot().audio_progress.capture_frames > 210
    });
    let snapshot = handle.snapshot();
    assert_eq!(snapshot.stage, RuntimeStage::Ready);
    assert!(!snapshot.audio_progress.capture_stalled && !snapshot.audio_progress.render_stalled);
    assert!(snapshot.mic.unwrap().error.is_none());
    backend.pause_render.store(true, Ordering::SeqCst);
    wait("mic-check playback stall", || {
        handle.snapshot().audio_progress.render_stalled
    });
    let snapshot = handle.snapshot();
    assert_eq!(snapshot.stage, RuntimeStage::Failed);
    assert!(snapshot.mic.as_ref().unwrap().input_available);
    assert!(snapshot.mic.unwrap().error.unwrap().contains("播放"));
    backend.pause_render.store(false, Ordering::SeqCst);
    wait("mic-check playback recovers", || {
        handle.snapshot().stage == RuntimeStage::Ready
    });
    assert!(handle.snapshot().intent.muted && handle.snapshot().intent.deafened);
    drain(&runtime, &handle);
}

#[test]
fn live_io_stalls_are_visible_and_clear_when_the_device_advances_again() {
    let backend = Arc::new(ResilienceBackend::default());
    let runtime = VoiceRuntime::new(backend.clone()).unwrap();
    let handle = runtime.handle();
    let socket = socket();
    handle.set_mode(TransmitMode::Always);
    handle.start_voice(request(&socket, 61));
    wait("active devices", || {
        answer_probe(&socket);
        let p = handle.snapshot().audio_progress;
        p.capture_frames > 5 && p.render_frames > 5
    });
    wait("authenticated healthy UDP", || {
        answer_probe(&socket);
        handle.snapshot().voice.is_some_and(|s| s.udp_ok)
    });
    for capture in [true, false] {
        let pause = if capture {
            &backend.pause_capture
        } else {
            &backend.pause_render
        };
        pause.store(true, Ordering::SeqCst);
        wait("stalled I/O", || {
            answer_probe(&socket);
            let s = handle.snapshot();
            if capture {
                s.audio_progress.capture_stalled
            } else {
                s.audio_progress.render_stalled
            }
        });
        let snapshot = handle.snapshot();
        let stats = snapshot.voice.unwrap();
        assert!(stats.udp_ok, "healthy network must not hide stalled audio");
        assert_eq!(snapshot.stage, RuntimeStage::Failed);
        if capture {
            assert!(stats.capture_error.is_some() && !stats.input_available && !stats.transmitting);
            assert!(stats.render_available && stats.render_error.is_none());
        } else {
            assert!(stats.render_error.is_some() && !stats.render_available);
            assert!(stats.input_available && stats.transmitting && stats.capture_error.is_none());
        }
        handle.set_muted(true);
        pause.store(false, Ordering::SeqCst);
        wait("progress recovers", || {
            let s = handle.snapshot();
            !s.audio_progress.capture_stalled
                && !s.audio_progress.render_stalled
                && s.voice
                    .is_some_and(|v| v.error.is_none() && v.input_available && v.render_available)
        });
        assert!(handle.snapshot().intent.muted);
        assert!(!handle.snapshot().voice.unwrap().transmitting);
        handle.set_muted(false);
        wait("sending after explicit unmute", || {
            handle.snapshot().voice.is_some_and(|s| s.transmitting)
        });
    }
    drain(&runtime, &handle);
}

#[test]
fn apm_failure_is_visible_in_call_and_mic_check_and_clears_after_success() {
    use client_runtime::{
        call::{CallState, CallViewModel},
        ProcessorStatus,
    };
    let backend = Arc::new(ResilienceBackend::default());
    backend.fail_processor.store(true, Ordering::SeqCst);
    let runtime = VoiceRuntime::new(backend.clone()).unwrap();
    let handle = runtime.handle();
    let socket = socket();
    handle.start_voice(request(&socket, 62));
    wait("unprocessed voice still captures", || {
        handle.snapshot().voice.is_some_and(|s| s.input_available)
    });
    let snapshot = handle.snapshot();
    assert_eq!(snapshot.processor, ProcessorStatus::Failed);
    assert!(snapshot.error.is_none());
    let mut call = CallState::default();
    call.connected();
    let view = CallViewModel::project_audio(&call, &snapshot, true);
    assert!(!view.processor_warning.is_empty());
    assert!(
        view.recovery_message.is_empty(),
        "APM warning must not claim voice disconnected"
    );
    handle.start_mic_check(Devices::default());
    wait("unprocessed mic check", || {
        handle.snapshot().mic.is_some_and(|s| s.input_available)
    });
    assert_eq!(handle.snapshot().processor, ProcessorStatus::Failed);
    backend.fail_processor.store(false, Ordering::SeqCst);
    handle.start_mic_check(Devices::default());
    wait("working processor", || {
        handle.snapshot().processor == ProcessorStatus::Active
    });
    assert!(
        CallViewModel::project_audio(&call, &handle.snapshot(), true)
            .processor_warning
            .is_empty()
    );
    handle.stop();
    assert!(handle.snapshot().processor.warning().is_empty());
    drain(&runtime, &handle);
}

#[test]
fn missing_selected_input_and_device_replacement_preserve_mute_and_deafen() {
    let backend = Arc::new(ResilienceBackend::default());
    let runtime = VoiceRuntime::new(backend.clone()).unwrap();
    let handle = runtime.handle();
    let socket = socket();
    handle.set_mode(TransmitMode::Always);
    handle.set_deafened(true);
    let mut first = request(&socket, 63);
    let sequences = first.sequences.clone();
    first.devices.capture = Some("missing".into());
    handle.start_voice(first);
    wait("missing selected input", || {
        handle
            .snapshot()
            .voice
            .is_some_and(|s| s.capture_error.is_some())
    });
    assert_eq!(
        *backend.capture_ids.lock().unwrap(),
        vec![Some("missing".into())]
    );
    assert!(handle.snapshot().intent.muted && handle.snapshot().intent.deafened);
    let mut replacement = request(&socket, 63);
    replacement.sequences = sequences;
    replacement.devices.capture = Some("replacement".into());
    handle.start_voice(replacement);
    wait("replacement input", || {
        handle.snapshot().voice.is_some_and(|s| s.input_available)
    });
    assert!(handle.snapshot().intent.muted && handle.snapshot().intent.deafened);
    assert!(!handle.snapshot().voice.unwrap().transmitting);
    handle.set_deafened(false);
    assert!(handle.snapshot().intent.muted);
    assert!(received(&socket, Duration::from_millis(150))
        .iter()
        .all(|h| h.is_keepalive()));
    handle.set_muted(false);
    wait("explicit unmute", || {
        handle.snapshot().voice.is_some_and(|s| s.transmitting)
    });
    assert!(received(&socket, Duration::from_millis(80))
        .iter()
        .any(|h| !h.is_keepalive()));
    drain(&runtime, &handle);
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
