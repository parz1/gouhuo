// SPDX-License-Identifier: GPL-3.0-or-later
use std::collections::BTreeMap;
use std::io;
use std::net::ToSocketAddrs;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{mpsc, Arc, Mutex};
use std::time::{Duration, Instant};

use crate::{
    CaptureInfo, Devices, MicSnapshot, RuntimeSnapshot, RuntimeStage, RuntimeTimings, VoiceIntent,
};
use voice_types::TransmitMode;

use voice_core::audio::{Capture, Render};
use voice_core::cue::CueQueue;
use voice_core::miccheck::{MicCheck, MicCheckControl};
use voice_core::pipeline::{
    default_jitter, AudioProcessor, Pipeline, PipelineConfig, PipelineControl, PipelineState,
};

pub trait CaptureDiagnostics: Send + Sync {
    fn snapshot(&self) -> CaptureInfo;
}

pub struct OpenedCapture {
    pub stream: Box<dyn Capture>,
    pub diagnostics: Option<Arc<dyn CaptureDiagnostics>>,
}

impl OpenedCapture {
    pub fn new(stream: Box<dyn Capture>) -> Self {
        Self {
            stream,
            diagnostics: None,
        }
    }
}

/// Factories run on the thread that will use the returned stream. A platform
/// implementation may therefore open a thread-affine device here safely.
pub trait AudioBackend: Send + Sync + 'static {
    fn capture(&self, device: Option<&str>) -> io::Result<OpenedCapture>;
    fn render(&self, device: Option<&str>) -> io::Result<Box<dyn Render>>;
    /// Runs on the lifecycle worker, before either audio stream is opened.
    fn processor(&self) -> Option<Box<dyn AudioProcessor>> {
        None
    }
}

/// A connection's immutable voice material. Replacements using the same keys
/// must reuse `sequences`; a new authenticated session supplies fresh material.
pub struct StartVoice {
    pub host: String,
    pub udp_port: u16,
    pub session_id: u32,
    pub sequences: Arc<protocol::VoiceSequences>,
    pub upstream_key: [u8; 32],
    pub downstream_key: [u8; 32],
    pub devices: Devices,
}

struct State {
    requested_at: Instant,
    snapshot: RuntimeSnapshot,
    control: Option<PipelineControl>,
    mic_control: Option<MicCheckControl>,
    cues: Option<Arc<CueQueue>>,
    volumes: BTreeMap<u32, f32>,
}

struct Shared {
    request_id: AtomicU64,
    state: Mutex<State>,
}

enum Command {
    Voice(u64, StartVoice),
    Mic(u64, Devices),
    Stop(u64),
    StopAfter(u64, Instant),
    Shutdown(u64),
}

/// A lightweight GUI-agnostic command and snapshot handle. It never owns a
/// `Pipeline`/`MicCheck`, and its destruction cannot wait for audio threads.
#[derive(Clone)]
pub struct RuntimeHandle {
    shared: Arc<Shared>,
    commands: mpsc::Sender<Command>,
}

/// The detached lifecycle worker exits when the last handle is dropped, or
/// after `shutdown`. Use `wait_stopped` only from a non-GUI shutdown coordinator.
pub struct VoiceRuntime {
    handle: RuntimeHandle,
    stopped: mpsc::Receiver<()>,
}

impl VoiceRuntime {
    pub fn new(backend: Arc<dyn AudioBackend>) -> io::Result<Self> {
        let shared = Arc::new(Shared {
            request_id: AtomicU64::new(0),
            state: Mutex::new(State {
                requested_at: Instant::now(),
                snapshot: RuntimeSnapshot::default(),
                control: None,
                mic_control: None,
                cues: None,
                volumes: BTreeMap::new(),
            }),
        });
        let (commands, receiver) = mpsc::channel();
        let (done, stopped) = mpsc::channel();
        let worker_shared = Arc::clone(&shared);
        std::thread::Builder::new()
            .name("gouhuo-voice-runtime".into())
            .spawn(move || {
                run(backend, worker_shared, receiver);
                let _ = done.send(());
            })?;
        Ok(Self {
            handle: RuntimeHandle { shared, commands },
            stopped,
        })
    }

    pub fn handle(&self) -> RuntimeHandle {
        self.handle.clone()
    }

    /// Explicit drain for application exit/tests. Audio device periods remain
    /// the bound on device teardown; a blocked backend may exceed the timeout.
    pub fn wait_stopped(&self, timeout: Duration) -> bool {
        self.stopped.recv_timeout(timeout).is_ok()
    }
}

impl RuntimeHandle {
    fn replace(&self, stage: RuntimeStage, session_id: Option<u32>) -> u64 {
        let mut state = self.shared.state.lock().expect("runtime state poisoned");
        let id = self.shared.request_id.fetch_add(1, Ordering::AcqRel) + 1;
        if let Some(control) = state.control.take() {
            control.quiesce();
            control.shutdown();
        }
        if let Some(control) = state.mic_control.take() {
            control.shutdown();
        }
        state.cues = None;
        state.requested_at = Instant::now();
        state.snapshot = RuntimeSnapshot {
            request_id: id,
            stage,
            session_id,
            intent: state.snapshot.intent.clone(),
            ..Default::default()
        };
        id
    }

    pub fn start_voice(&self, request: StartVoice) -> u64 {
        let id = self.replace(RuntimeStage::Preparing, Some(request.session_id));
        let _ = self.commands.send(Command::Voice(id, request));
        id
    }

    pub fn start_mic_check(&self, devices: Devices) -> u64 {
        let id = self.replace(RuntimeStage::Preparing, None);
        let _ = self.commands.send(Command::Mic(id, devices));
        id
    }

    /// Invalidates in-flight preparation and immediately disables the current
    /// voice transport. Blocking device teardown runs only on the worker.
    pub fn stop(&self) {
        let id = self.replace(RuntimeStage::Stopping, None);
        let _ = self.commands.send(Command::Stop(id));
    }

    pub fn shutdown(&self) {
        let id = self.replace(RuntimeStage::Stopping, None);
        let _ = self.commands.send(Command::Shutdown(id));
    }

    /// Transport interruption preserves receive-side/local cue playback while
    /// retiring the authenticated voice material. A subsequent start replaces it.
    pub fn quiesce(&self) {
        let mut state = self.shared.state.lock().expect("runtime state poisoned");
        state.snapshot.request_id = self.shared.request_id.fetch_add(1, Ordering::AcqRel) + 1;
        if let Some(control) = &state.control {
            control.quiesce();
        }
        state.snapshot.stage = RuntimeStage::Stopping;
        if let Some(stats) = &mut state.snapshot.voice {
            stats.transmitting = false;
            stats.udp_ok = false;
            stats.speaking.clear();
        }
    }

    /// Keep local cue playback briefly after quiescing the voice transport. A
    /// new start invalidates this request, so its deadline cannot stop a newer
    /// authenticated session or microphone check.
    pub fn stop_after(&self, delay: Duration) {
        let mut state = self.shared.state.lock().expect("runtime state poisoned");
        let id = self.shared.request_id.fetch_add(1, Ordering::AcqRel) + 1;
        if let Some(control) = &state.control {
            control.quiesce();
        }
        state.snapshot.request_id = id;
        state.snapshot.stage = RuntimeStage::Stopping;
        if let Some(stats) = &mut state.snapshot.voice {
            stats.transmitting = false;
            stats.udp_ok = false;
            stats.speaking.clear();
        }
        let _ = self
            .commands
            .send(Command::StopAfter(id, Instant::now() + delay));
    }

    fn intent(
        &self,
        change: impl FnOnce(&mut VoiceIntent),
        apply: impl FnOnce(&PipelineControl, &VoiceIntent),
    ) {
        let mut state = self.shared.state.lock().expect("runtime state poisoned");
        change(&mut state.snapshot.intent);
        if let Some(control) = &state.control {
            apply(control, &state.snapshot.intent);
        }
        if let Some(control) = &state.mic_control {
            control.set_monitoring(state.snapshot.intent.monitoring);
        }
        // No lifecycle message is needed: current controls apply immediately,
        // and an in-flight preparation reads the newest intent before commit.
        // In particular, global PTT polling cannot flood a blocked DNS/APM queue.
    }

    pub fn set_muted(&self, on: bool) {
        self.intent(
            |s| s.muted = on || s.deafened,
            |c, s| c.set_muted(s.muted || s.server_muted),
        );
    }

    pub fn set_server_muted(&self, on: bool) {
        self.intent(
            |s| s.server_muted = on,
            |c, s| c.set_muted(s.muted || s.deafened || s.server_muted),
        );
    }

    /// Apply the initial local intent atomically before submitting start_voice.
    /// Later self-state echoes should acknowledge rather than overwrite intent.
    pub fn set_self_state(&self, muted: bool, deafened: bool) {
        self.intent(
            |s| {
                s.muted = muted || deafened;
                s.deafened = deafened;
            },
            |c, s| {
                if s.deafened {
                    c.set_muted(true);
                }
                c.set_deafened(s.deafened);
                c.set_muted(s.muted || s.deafened || s.server_muted);
            },
        );
    }

    /// Closing sound also closes the microphone; reopening sound leaves it
    /// muted until the user explicitly opens it again.
    pub fn set_deafened(&self, on: bool) {
        self.intent(
            |s| {
                s.deafened = on;
                if on {
                    s.muted = true;
                }
            },
            |c, s| {
                if s.deafened {
                    c.set_muted(true);
                }
                c.set_deafened(s.deafened);
                c.set_muted(s.muted || s.server_muted);
            },
        );
    }

    pub fn set_transmitting(&self, on: bool) {
        self.intent(
            |s| s.transmitting = on,
            |c, s| c.set_transmitting(s.transmitting),
        );
    }

    pub fn set_monitoring(&self, on: bool) {
        self.intent(|s| s.monitoring = on, |c, s| c.set_monitoring(s.monitoring));
    }

    pub fn is_monitoring(&self) -> bool {
        self.snapshot().intent.monitoring
    }

    pub fn set_mode(&self, mode: TransmitMode) {
        self.intent(
            |s| {
                s.mode = mode;
                s.transmitting = false;
            },
            |c, s| c.set_mode(s.mode),
        );
    }

    pub fn mode(&self) -> TransmitMode {
        self.snapshot().intent.mode
    }

    pub fn set_volume(&self, member: u32, volume: f32) {
        let volume = if volume.is_finite() {
            volume.clamp(0.0, voice_core::pipeline::MAX_VOLUME)
        } else {
            1.0
        };
        let mut state = self.shared.state.lock().expect("runtime state poisoned");
        state.volumes.insert(member, volume);
        if let Some(control) = &state.control {
            control.set_volume(member, volume);
        }
    }

    /// Drop stale session volume identities before applying the new roster.
    pub fn clear_volumes(&self) {
        self.shared
            .state
            .lock()
            .expect("runtime state poisoned")
            .volumes
            .clear();
    }

    pub fn cues(&self) -> Option<Arc<CueQueue>> {
        self.shared
            .state
            .lock()
            .expect("runtime state poisoned")
            .cues
            .clone()
    }

    pub fn snapshot(&self) -> RuntimeSnapshot {
        let state = self.shared.state.lock().expect("runtime state poisoned");
        let mut snapshot = state.snapshot.clone();
        if let Some(control) = &state.control {
            snapshot.voice = Some(control.stats());
        }
        if let Some(control) = &state.mic_control {
            snapshot.mic = Some(MicSnapshot {
                input_db: control.input_db(),
                input_available: control.is_running(),
                error: control.error(),
            });
        }
        snapshot
    }
}

impl crate::VoiceControl for RuntimeHandle {
    fn snapshot(&self) -> RuntimeSnapshot {
        RuntimeHandle::snapshot(self)
    }

    fn set_muted(&self, on: bool) {
        RuntimeHandle::set_muted(self, on);
    }

    fn set_deafened(&self, on: bool) {
        RuntimeHandle::set_deafened(self, on);
    }

    fn set_transmitting(&self, on: bool) {
        RuntimeHandle::set_transmitting(self, on);
    }

    fn set_volume(&self, member: u32, volume: f32) {
        RuntimeHandle::set_volume(self, member, volume);
    }

    fn stop(&self) {
        RuntimeHandle::stop(self);
    }
}

enum Active {
    Voice {
        owner: Pipeline,
        id: u64,
        diagnostics: DiagnosticsSlot,
        progress: Arc<DeviceProgress>,
    },
    Mic {
        owner: MicCheck,
        id: u64,
        diagnostics: DiagnosticsSlot,
        progress: Arc<DeviceProgress>,
    },
}

type DiagnosticsSlot = Arc<Mutex<Option<Arc<dyn CaptureDiagnostics>>>>;

#[derive(Default)]
struct DeviceProgress {
    capture_at: Mutex<Option<Instant>>,
    render_at: Mutex<Option<Instant>>,
}

// Deferred factories keep creation, use and final destruction of COM/device
// streams on the capture/render thread, even when the backend opens eagerly.
struct DeferredCapture {
    backend: Arc<dyn AudioBackend>,
    device: Option<String>,
    stream: Option<Box<dyn Capture>>,
    diagnostics: DiagnosticsSlot,
    progress: Arc<DeviceProgress>,
    first_recorded: bool,
}

impl Capture for DeferredCapture {
    fn read(&mut self, out: &mut [f32]) -> io::Result<bool> {
        if self.stream.is_none() {
            let opened = self.backend.capture(self.device.as_deref())?;
            *self.diagnostics.lock().expect("diagnostics poisoned") = opened.diagnostics;
            self.stream = Some(opened.stream);
        }
        let read = self.stream.as_mut().expect("opened capture").read(out)?;
        if read && !self.first_recorded {
            self.progress
                .capture_at
                .lock()
                .expect("capture progress poisoned")
                .get_or_insert_with(Instant::now);
            self.first_recorded = true;
        }
        Ok(read)
    }
}

struct DeferredRender {
    backend: Arc<dyn AudioBackend>,
    device: Option<String>,
    stream: Option<Box<dyn Render>>,
    progress: Arc<DeviceProgress>,
    first_recorded: bool,
}

impl Render for DeferredRender {
    fn write(&mut self, frame: &[f32]) -> io::Result<()> {
        if self.stream.is_none() {
            self.stream = Some(self.backend.render(self.device.as_deref())?);
        }
        self.stream.as_mut().expect("opened render").write(frame)?;
        if !self.first_recorded {
            self.progress
                .render_at
                .lock()
                .expect("render progress poisoned")
                .get_or_insert_with(Instant::now);
            self.first_recorded = true;
        }
        Ok(())
    }
}

fn devices(backend: &Arc<dyn AudioBackend>, devices: Devices) -> AudioStreams {
    let diagnostics = Arc::new(Mutex::new(None));
    let progress = Arc::new(DeviceProgress::default());
    (
        Box::new(DeferredCapture {
            backend: Arc::clone(backend),
            device: devices.capture,
            stream: None,
            diagnostics: Arc::clone(&diagnostics),
            progress: Arc::clone(&progress),
            first_recorded: false,
        }),
        Box::new(DeferredRender {
            backend: Arc::clone(backend),
            device: devices.render,
            stream: None,
            progress: Arc::clone(&progress),
            first_recorded: false,
        }),
        diagnostics,
        progress,
    )
}

type AudioStreams = (
    Box<dyn Capture>,
    Box<dyn Render>,
    DiagnosticsSlot,
    Arc<DeviceProgress>,
);

fn measured(shared: &Shared, id: u64, set: impl FnOnce(&mut RuntimeTimings)) {
    let mut state = shared.state.lock().expect("runtime state poisoned");
    if current(shared, id) {
        set(&mut state.snapshot.timings);
    }
}

fn ms(duration: Duration) -> f64 {
    duration.as_secs_f64() * 1000.0
}

fn current(shared: &Shared, id: u64) -> bool {
    shared.request_id.load(Ordering::Acquire) == id
}

fn retire(shared: &Shared, active: &mut Option<Active>) {
    {
        let mut state = shared.state.lock().expect("runtime state poisoned");
        if let Some(control) = state.control.take() {
            control.shutdown();
        }
        if let Some(control) = state.mic_control.take() {
            control.shutdown();
        }
        state.cues = None;
    }
    // No GUI/shared lock is held during join.
    drop(active.take());
}

fn fail(shared: &Shared, id: u64, error: io::Error) {
    let mut state = shared.state.lock().expect("runtime state poisoned");
    if current(shared, id) {
        state.snapshot.stage = RuntimeStage::Failed;
        state.snapshot.error = Some(error.to_string());
    }
}

fn run(backend: Arc<dyn AudioBackend>, shared: Arc<Shared>, receiver: mpsc::Receiver<Command>) {
    let mut active = None;
    let mut stop_at = None;
    loop {
        match receiver.recv_timeout(Duration::from_millis(20)) {
            Ok(Command::Shutdown(_id)) => {
                retire(&shared, &mut active);
                break;
            }
            Ok(Command::Stop(id)) => {
                if !current(&shared, id) {
                    continue;
                }
                let at = Instant::now();
                retire(&shared, &mut active);
                measured(&shared, id, |t| t.retire_ms = Some(ms(at.elapsed())));
                let mut state = shared.state.lock().expect("runtime state poisoned");
                if current(&shared, id) {
                    state.snapshot.stage = RuntimeStage::Idle;
                }
            }
            Ok(Command::StopAfter(id, deadline)) => {
                if current(&shared, id) {
                    stop_at = Some((id, deadline));
                }
            }
            Ok(Command::Voice(id, request)) => {
                if !current(&shared, id) {
                    continue;
                }
                let queued = shared
                    .state
                    .lock()
                    .expect("runtime state poisoned")
                    .requested_at
                    .elapsed();
                measured(&shared, id, |t| t.queued_ms = Some(ms(queued)));
                let retiring_at = Instant::now();
                retire(&shared, &mut active);
                measured(&shared, id, |t| {
                    t.retire_ms = Some(ms(retiring_at.elapsed()))
                });
                let prepared = (|| -> io::Result<Option<Active>> {
                    if !current(&shared, id) {
                        return Ok(None);
                    }
                    if request.udp_port == 0 {
                        return Err(io::Error::other("服务器没给出语音端口"));
                    }
                    let at = Instant::now();
                    let resolution = (request.host.as_str(), request.udp_port).to_socket_addrs();
                    measured(&shared, id, |t| t.dns_ms = Some(ms(at.elapsed())));
                    let server = resolution?
                        .next()
                        .ok_or_else(|| io::Error::other("无法解析服务器语音地址"))?;
                    if !current(&shared, id) {
                        return Ok(None);
                    }
                    let at = Instant::now();
                    let processor = backend.processor();
                    measured(&shared, id, |t| t.processor_ms = Some(ms(at.elapsed())));
                    if !current(&shared, id) {
                        return Ok(None);
                    }
                    let (capture, render, diagnostics, progress) =
                        devices(&backend, request.devices);
                    let (mode, initial) = {
                        let state = shared.state.lock().expect("runtime state poisoned");
                        let intent = &state.snapshot.intent;
                        (
                            intent.mode,
                            PipelineState {
                                muted: intent.muted || intent.deafened || intent.server_muted,
                                deafened: intent.deafened,
                                transmitting: intent.transmitting,
                                monitoring: intent.monitoring,
                                send_enabled: false,
                            },
                        )
                    };
                    let at = Instant::now();
                    let started = Pipeline::start_with_state(
                        PipelineConfig {
                            server,
                            session_id: request.session_id,
                            sequences: request.sequences,
                            upstream_key: request.upstream_key,
                            downstream_key: request.downstream_key,
                            jitter: default_jitter(),
                            mode,
                        },
                        capture,
                        render,
                        processor,
                        initial,
                    );
                    measured(&shared, id, |t| t.start_ms = Some(ms(at.elapsed())));
                    let owner = started?;
                    let control = owner.control();
                    let mut state = shared.state.lock().expect("runtime state poisoned");
                    if !current(&shared, id) {
                        control.shutdown();
                        drop(state);
                        drop(owner);
                        return Ok(None);
                    }
                    let intent = &state.snapshot.intent;
                    // Reapply intentions that changed while Pipeline started. No UDP
                    // datagram (including keepalive) has been allowed before this.
                    control.set_muted(intent.muted || intent.deafened || intent.server_muted);
                    control.set_deafened(intent.deafened);
                    control.set_mode(intent.mode);
                    control.set_transmitting(intent.transmitting);
                    control.set_monitoring(intent.monitoring);
                    for (&member, &volume) in &state.volumes {
                        control.set_volume(member, volume);
                    }
                    state.cues = Some(control.cues());
                    state.control = Some(control.clone());
                    state.snapshot.stage = RuntimeStage::Starting;
                    control.set_send_enabled(true);
                    Ok(Some(Active::Voice {
                        owner,
                        id,
                        diagnostics,
                        progress,
                    }))
                })();
                match prepared {
                    Ok(next) => active = next,
                    Err(error) => fail(&shared, id, error),
                }
            }
            Ok(Command::Mic(id, device_ids)) => {
                if !current(&shared, id) {
                    continue;
                }
                let queued = shared
                    .state
                    .lock()
                    .expect("runtime state poisoned")
                    .requested_at
                    .elapsed();
                measured(&shared, id, |t| t.queued_ms = Some(ms(queued)));
                let at = Instant::now();
                retire(&shared, &mut active);
                measured(&shared, id, |t| t.retire_ms = Some(ms(at.elapsed())));
                if !current(&shared, id) {
                    continue;
                }
                let at = Instant::now();
                let processor = backend.processor();
                measured(&shared, id, |t| t.processor_ms = Some(ms(at.elapsed())));
                if !current(&shared, id) {
                    continue;
                }
                let (capture, render, diagnostics, progress) = devices(&backend, device_ids);
                let at = Instant::now();
                let started = MicCheck::start(capture, render, processor);
                measured(&shared, id, |t| t.start_ms = Some(ms(at.elapsed())));
                match started {
                    Ok(owner) => {
                        let mut state = shared.state.lock().expect("runtime state poisoned");
                        if !current(&shared, id) {
                            drop(state);
                            drop(owner);
                            continue;
                        }
                        owner.set_monitoring(state.snapshot.intent.monitoring);
                        state.mic_control = Some(owner.control());
                        state.cues = Some(owner.cues());
                        state.snapshot.stage = RuntimeStage::Starting;
                        active = Some(Active::Mic {
                            owner,
                            id,
                            diagnostics,
                            progress,
                        });
                    }
                    Err(error) => fail(&shared, id, error),
                }
            }
            Err(mpsc::RecvTimeoutError::Timeout) => {}
            Err(mpsc::RecvTimeoutError::Disconnected) => {
                retire(&shared, &mut active);
                break;
            }
        }
        if let Some((id, deadline)) = stop_at {
            if !current(&shared, id) {
                stop_at = None;
            } else if Instant::now() >= deadline {
                let at = Instant::now();
                retire(&shared, &mut active);
                measured(&shared, id, |t| t.retire_ms = Some(ms(at.elapsed())));
                let mut state = shared.state.lock().expect("runtime state poisoned");
                if current(&shared, id) {
                    state.snapshot.stage = RuntimeStage::Idle;
                    state.snapshot.voice = None;
                    state.snapshot.mic = None;
                    state.snapshot.capture = CaptureInfo::default();
                }
                stop_at = None;
            }
        }
        sync(&shared, active.as_ref());
    }
    let mut state = shared.state.lock().expect("runtime state poisoned");
    state.snapshot.stage = RuntimeStage::Idle;
    state.snapshot.voice = None;
    state.snapshot.mic = None;
}

fn sync(shared: &Shared, active: Option<&Active>) {
    let Some(active) = active else {
        return;
    };
    let mut state = shared.state.lock().expect("runtime state poisoned");
    let (id, diagnostics, progress) = match active {
        Active::Voice {
            id,
            diagnostics,
            progress,
            ..
        }
        | Active::Mic {
            id,
            diagnostics,
            progress,
            ..
        } => (*id, diagnostics, progress),
    };
    if !current(shared, id) {
        return;
    }
    let provider = diagnostics.lock().expect("diagnostics poisoned").clone();
    if let Some(provider) = provider {
        state.snapshot.capture = provider.snapshot();
    }
    let since = state.requested_at;
    if let Some(at) = *progress
        .capture_at
        .lock()
        .expect("capture progress poisoned")
    {
        state
            .snapshot
            .timings
            .first_capture_ms
            .get_or_insert(ms(at.saturating_duration_since(since)));
    }
    if let Some(at) = *progress.render_at.lock().expect("render progress poisoned") {
        state
            .snapshot
            .timings
            .first_render_ms
            .get_or_insert(ms(at.saturating_duration_since(since)));
    }
    match active {
        Active::Voice { owner, .. } => {
            let stats = owner.stats();
            if stats.udp_ok {
                state
                    .snapshot
                    .timings
                    .first_udp_ms
                    .get_or_insert(ms(since.elapsed()));
            }
            state.snapshot.stage =
                if stats.error.is_some() || stats.sequences_exhausted || stats.udp_failed {
                    RuntimeStage::Failed
                } else if stats.input_available && stats.render_available && stats.udp_ok {
                    RuntimeStage::Ready
                } else {
                    RuntimeStage::Starting
                };
            state.snapshot.voice = Some(stats);
        }
        Active::Mic { owner, .. } => {
            owner.set_monitoring(state.snapshot.intent.monitoring);
            let error = owner.error();
            state.snapshot.stage = if error.is_some() {
                RuntimeStage::Failed
            } else if owner.is_running() {
                RuntimeStage::Ready
            } else {
                RuntimeStage::Starting
            };
            state.snapshot.mic = Some(MicSnapshot {
                input_db: owner.input_db(),
                input_available: owner.is_running(),
                error,
            });
        }
    }
}

#[cfg(test)]
mod ordering_tests {
    use super::*;
    use voice_core::audio::{NullRender, SyntheticCapture};

    struct Backend;
    impl AudioBackend for Backend {
        fn capture(&self, _device: Option<&str>) -> io::Result<OpenedCapture> {
            Ok(OpenedCapture::new(Box::new(
                SyntheticCapture::new(Vec::new()).then_silence(),
            )))
        }
        fn render(&self, _device: Option<&str>) -> io::Result<Box<dyn Render>> {
            Ok(Box::new(NullRender::default()))
        }
    }

    #[test]
    fn a_stop_sender_descheduled_after_replace_cannot_retire_a_new_owner() {
        let runtime = VoiceRuntime::new(Arc::new(Backend)).unwrap();
        let handle = runtime.handle();
        // Simulate one sender pausing after its shared request update, while
        // another sender submits and installs a newer microphone request.
        let stale_id = handle.replace(RuntimeStage::Stopping, None);
        let live_id = handle.start_mic_check(Devices::default());
        let deadline = Instant::now() + Duration::from_secs(2);
        while !handle.snapshot().mic.is_some_and(|s| s.input_available) {
            assert!(Instant::now() < deadline);
            std::thread::sleep(Duration::from_millis(5));
        }
        handle.commands.send(Command::Stop(stale_id)).unwrap();
        std::thread::sleep(Duration::from_millis(60));
        let snapshot = handle.snapshot();
        assert_eq!(snapshot.request_id, live_id);
        assert_eq!(snapshot.stage, RuntimeStage::Ready);
        assert!(snapshot.mic.unwrap().input_available);
        handle.shutdown();
        assert!(runtime.wait_stopped(Duration::from_secs(2)));
    }

    #[test]
    fn changing_mode_clears_a_pending_ptt_press_before_replacement() {
        let runtime = VoiceRuntime::new(Arc::new(Backend)).unwrap();
        let handle = runtime.handle();
        handle.set_transmitting(true);
        handle.set_mode(TransmitMode::VoiceActivity {
            threshold_db: -45.0,
        });
        assert!(!handle.snapshot().intent.transmitting);
        handle.set_mode(TransmitMode::PushToTalk);
        assert!(!handle.snapshot().intent.transmitting);
        handle.shutdown();
        assert!(runtime.wait_stopped(Duration::from_secs(2)));
    }
}
