// SPDX-License-Identifier: GPL-3.0-or-later
//! Cached voice intentions and supervised IPC. No audio code runs in the UI.

use std::collections::{BTreeMap, HashMap, VecDeque};
use std::io;
use std::path::{Path, PathBuf};
use std::process::{Child, Command as ProcessCommand, Stdio};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{mpsc, Arc, Condvar, Mutex};
use std::time::{Duration, Instant};

use client_runtime::ipc::{self, Command, Cue, Hello, Reply, Request, RpcReply, StartVoice};
use client_runtime::{Devices, RuntimeSnapshot, RuntimeStage, VoiceControl, VoiceIntent};
use sha2::{Digest, Sha256};
use voice_types::{TransmitMode, MAX_VOLUME};

const HEARTBEAT_TIMEOUT: Duration = Duration::from_secs(5);
const WRITE_TIMEOUT: Duration = Duration::from_secs(2);
const RPC_LIMIT: usize = 8;

pub mod history;
pub mod update;

struct State {
    snapshot: RuntimeSnapshot,
    volumes: BTreeMap<u32, f32>,
    revision: u64,
    generation: u64,
    pending: Option<Command>,
    dirty: bool,
    cancel_scan: VecDeque<u64>,
    auxiliary: VecDeque<Command>,
    rpc: HashMap<u64, mpsc::SyncSender<RpcReply>>,
    next_rpc: u64,
    ready: bool,
    closed: bool,
    reauthenticate: bool,
    engine_version: String,
    seen_sessions: HashMap<[u8; 32], u64>,
    last_reply: Instant,
    writing_since: Option<Instant>,
    unacked_since: Option<Instant>,
    process_id: Option<u32>,
    io_failure: Option<String>,
    updates_enabled: bool,
    activate_requested: bool,
}

struct Shared {
    state: Mutex<State>,
    wake: Condvar,
}

#[derive(Clone)]
pub struct RuntimeHandle {
    shared: Arc<Shared>,
}

pub struct VoiceRuntime {
    handle: RuntimeHandle,
    stopped: mpsc::Receiver<()>,
}

impl VoiceRuntime {
    pub fn bundled() -> io::Result<Self> {
        let name = if cfg!(windows) {
            "gouhuo-voice.exe"
        } else {
            "gouhuo-voice"
        };
        Self::new(std::env::current_exe()?.with_file_name(name))
    }

    pub fn new(path: impl AsRef<Path>) -> io::Result<Self> {
        Self::with_args(path, Vec::new())
    }

    /// Arguments are for controlled diagnostics/tests, never session material.
    pub fn with_args(path: impl AsRef<Path>, args: Vec<String>) -> io::Result<Self> {
        Self::launch(path.as_ref().to_owned(), args, None)
    }

    pub fn managed(bundled: impl AsRef<Path>, store: Arc<update::EngineStore>) -> io::Result<Self> {
        Self::launch(bundled.as_ref().to_owned(), Vec::new(), Some(store))
    }

    /// Diagnostic arguments never contain authentication material.
    pub fn managed_with_args(
        bundled: impl AsRef<Path>,
        args: Vec<String>,
        store: Arc<update::EngineStore>,
    ) -> io::Result<Self> {
        Self::launch(bundled.as_ref().to_owned(), args, Some(store))
    }

    fn launch(
        path: PathBuf,
        args: Vec<String>,
        store: Option<Arc<update::EngineStore>>,
    ) -> io::Result<Self> {
        let shared = Arc::new(Shared {
            state: Mutex::new(State {
                snapshot: RuntimeSnapshot::default(),
                volumes: BTreeMap::new(),
                revision: 0,
                generation: 0,
                pending: None,
                dirty: false,
                cancel_scan: VecDeque::new(),
                auxiliary: VecDeque::new(),
                rpc: HashMap::new(),
                next_rpc: 0,
                ready: false,
                closed: false,
                reauthenticate: false,
                engine_version: String::new(),
                seen_sessions: HashMap::new(),
                last_reply: Instant::now(),
                writing_since: None,
                unacked_since: None,
                process_id: None,
                io_failure: None,
                updates_enabled: store.is_some(),
                activate_requested: false,
            }),
            wake: Condvar::new(),
        });
        let (done, stopped) = mpsc::channel();
        let worker = Arc::clone(&shared);
        std::thread::Builder::new()
            .name("voice-process-supervisor".into())
            .spawn(move || {
                supervise(path, args, worker, store);
                let _ = done.send(());
            })?;
        Ok(Self {
            handle: RuntimeHandle { shared },
            stopped,
        })
    }

    pub fn handle(&self) -> RuntimeHandle {
        self.handle.clone()
    }
    pub fn wait_stopped(&self, timeout: Duration) -> bool {
        self.stopped.recv_timeout(timeout).is_ok()
    }
}

impl Drop for VoiceRuntime {
    fn drop(&mut self) {
        self.handle.shutdown();
    }
}

impl RuntimeHandle {
    pub fn is_closed(&self) -> bool {
        self.shared
            .state
            .lock()
            .expect("voice proxy poisoned")
            .closed
    }

    /// Only the idle owner can retire a process for an update. Starts racing
    /// retirement remain subject to the normal fresh-key admission checks.
    pub fn activate_pending(&self) -> bool {
        let mut state = self.shared.state.lock().expect("voice proxy poisoned");
        if !state.updates_enabled
            || state.closed
            || !state.ready
            || state.snapshot.stage != RuntimeStage::Idle
            || state.snapshot.session_id.is_some()
            || state.pending.is_some()
            || !state.rpc.is_empty()
        {
            return false;
        }
        state.activate_requested = true;
        state.io_failure = Some("idle engine activation requested".into());
        state.ready = false;
        self.shared.wake.notify_all();
        true
    }
    pub fn snapshot(&self) -> RuntimeSnapshot {
        self.shared
            .state
            .lock()
            .expect("voice proxy poisoned")
            .snapshot
            .clone()
    }

    pub fn engine_version(&self) -> String {
        self.shared
            .state
            .lock()
            .expect("voice proxy poisoned")
            .engine_version
            .clone()
    }

    pub fn needs_reauthentication(&self) -> bool {
        self.shared
            .state
            .lock()
            .expect("voice proxy poisoned")
            .reauthenticate
    }

    pub fn process_id(&self) -> Option<u32> {
        self.shared
            .state
            .lock()
            .expect("voice proxy poisoned")
            .process_id
    }

    /// Retire the current engine; authenticated voice needs fresh keys before
    /// another process is admitted. Used by recovery and future activation.
    pub fn retire_engine(&self) {
        let mut state = self.shared.state.lock().expect("voice proxy poisoned");
        if state.closed {
            return;
        }
        state.io_failure = Some("voice process retirement requested".into());
        state.ready = false;
        state.reauthenticate = state.snapshot.session_id.is_some();
        state.snapshot.stage = RuntimeStage::Failed;
        state.snapshot.voice = None;
        state.snapshot.mic = None;
        state.snapshot.error = Some("语音进程正在回收。".into());
        self.shared.wake.notify_all();
    }

    pub fn start_voice(&self, request: StartVoice) -> u64 {
        let mut state = self.shared.state.lock().expect("voice proxy poisoned");
        if state.closed {
            return state.snapshot.request_id;
        }
        let reused = fingerprints(&request).iter().any(|fingerprint| {
            state
                .seen_sessions
                .get(fingerprint)
                .is_some_and(|generation| !state.ready || *generation != state.generation)
        });
        replace(
            &mut state,
            RuntimeStage::Preparing,
            Some(request.session_id),
        );
        if reused || state.seen_sessions.len() >= 4096 {
            state.reauthenticate = true;
            state.snapshot.stage = RuntimeStage::Failed;
            state.snapshot.error = Some("语音进程已更换，需要重新认证取得新会话。".into());
            state.pending = Some(Command::Stop);
        } else {
            state.reauthenticate = false;
            state.pending = Some(Command::StartVoice(request));
        }
        let id = state.snapshot.request_id;
        self.shared.wake.notify_all();
        id
    }

    pub fn start_mic_check(&self, devices: Devices) -> u64 {
        let mut state = self.shared.state.lock().expect("voice proxy poisoned");
        if state.closed {
            return state.snapshot.request_id;
        }
        replace(&mut state, RuntimeStage::Preparing, None);
        state.pending = Some(Command::StartMic(devices));
        let id = state.snapshot.request_id;
        self.shared.wake.notify_all();
        id
    }

    fn lifecycle(&self, command: Command, preserve_playback: bool) {
        let mut state = self.shared.state.lock().expect("voice proxy poisoned");
        if state.closed {
            return;
        }
        if preserve_playback {
            state.snapshot.request_id += 1;
            state.snapshot.stage = RuntimeStage::Stopping;
            if let Some(stats) = &mut state.snapshot.voice {
                stats.transmitting = false;
                stats.udp_ok = false;
                stats.speaking.clear();
            }
        } else {
            let stage = if state.ready {
                RuntimeStage::Stopping
            } else {
                RuntimeStage::Idle
            };
            replace(&mut state, stage, None);
        }
        state.pending = Some(command);
        self.shared.wake.notify_all();
    }

    pub fn stop(&self) {
        self.lifecycle(Command::Stop, false);
    }
    pub fn quiesce(&self) {
        self.lifecycle(Command::Quiesce, true);
    }
    pub fn stop_after(&self, delay: Duration) {
        self.lifecycle(Command::StopAfter(delay.as_millis().min(5000) as u64), true);
    }

    pub fn shutdown(&self) {
        let mut state = self.shared.state.lock().expect("voice proxy poisoned");
        if state.closed {
            return;
        }
        replace(&mut state, RuntimeStage::Stopping, None);
        state.closed = true;
        self.shared.wake.notify_all();
    }

    fn intent(&self, update: impl FnOnce(&mut VoiceIntent)) {
        let mut state = self.shared.state.lock().expect("voice proxy poisoned");
        if state.closed {
            return;
        }
        let old = state.snapshot.intent.clone();
        update(&mut state.snapshot.intent);
        if state.snapshot.intent == old {
            return;
        }
        let intent = state.snapshot.intent.clone();
        if let Some(stats) = &mut state.snapshot.voice {
            if intent.muted
                || intent.deafened
                || intent.server_muted
                || (matches!(intent.mode, TransmitMode::PushToTalk) && !intent.transmitting)
                || old.mode != intent.mode
            {
                stats.transmitting = false;
            }
        }
        state.revision += 1;
        state.unacked_since.get_or_insert_with(Instant::now);
        state.dirty = true;
        self.shared.wake.notify_all();
    }

    pub fn set_muted(&self, on: bool) {
        self.intent(|s| s.muted = on || s.deafened);
    }
    pub fn set_server_muted(&self, on: bool) {
        self.intent(|s| s.server_muted = on);
    }
    pub fn set_deafened(&self, on: bool) {
        self.intent(|s| {
            s.deafened = on;
            if on {
                s.muted = true;
            }
        });
    }
    pub fn set_self_state(&self, muted: bool, deafened: bool) {
        self.intent(|s| {
            s.muted = muted || deafened;
            s.deafened = deafened;
        });
    }
    pub fn set_transmitting(&self, on: bool) {
        self.intent(|s| s.transmitting = on);
    }
    pub fn set_monitoring(&self, on: bool) {
        self.intent(|s| s.monitoring = on);
    }
    pub fn is_monitoring(&self) -> bool {
        self.snapshot().intent.monitoring
    }
    pub fn set_mode(&self, mode: TransmitMode) {
        self.intent(|s| {
            s.mode = mode;
            s.transmitting = false;
        });
    }
    pub fn mode(&self) -> TransmitMode {
        self.snapshot().intent.mode
    }

    pub fn set_volume(&self, member: u32, volume: f32) {
        let volume = if volume.is_finite() {
            volume.clamp(0.0, MAX_VOLUME)
        } else {
            1.0
        };
        let mut state = self.shared.state.lock().expect("voice proxy poisoned");
        if state.closed
            || (state.volumes.len() >= ipc::MAX_VOLUMES && !state.volumes.contains_key(&member))
        {
            return;
        }
        if state.volumes.get(&member) == Some(&volume) {
            return;
        }
        state.volumes.insert(member, volume);
        state.revision += 1;
        state.unacked_since.get_or_insert_with(Instant::now);
        state.dirty = true;
        self.shared.wake.notify_all();
    }

    pub fn clear_volumes(&self) {
        let mut state = self.shared.state.lock().expect("voice proxy poisoned");
        if state.closed {
            return;
        }
        state.volumes.clear();
        state.revision += 1;
        state.unacked_since.get_or_insert_with(Instant::now);
        state.dirty = true;
        self.shared.wake.notify_all();
    }

    pub fn notice(&self, cue: Cue, name: Option<String>, sound: bool, gain: f32) {
        let mut state = self.shared.state.lock().expect("voice proxy poisoned");
        if state.closed || !state.ready || state.auxiliary.len() >= 16 {
            return;
        }
        state.auxiliary.push_back(Command::Notice {
            cue,
            name: name.map(|s| s.chars().take(12).collect()),
            sound,
            gain: if gain.is_finite() {
                gain.clamp(0.0, 1.0)
            } else {
                0.0
            },
        });
        self.shared.wake.notify_all();
    }

    pub fn devices(&self) -> io::Result<(Vec<ipc::Device>, Vec<ipc::Device>)> {
        match self.rpc(|rpc| Command::Devices { rpc }, Duration::from_secs(5), None)? {
            RpcReply::Devices { capture, render } => Ok((capture, render)),
            _ => Err(io::Error::other("invalid device response")),
        }
    }

    pub fn scan(
        &self,
        per_device: Duration,
        cancel: Arc<AtomicBool>,
    ) -> io::Result<Vec<ipc::ScanResult>> {
        match self.rpc(
            |rpc| Command::Scan {
                rpc,
                per_device_ms: per_device.as_millis().clamp(1, 5000) as u64,
            },
            Duration::from_secs(120),
            Some(cancel),
        )? {
            RpcReply::Scan(results) => Ok(results),
            _ => Err(io::Error::other("invalid scan response")),
        }
    }

    fn rpc(
        &self,
        command: impl FnOnce(u64) -> Command,
        timeout: Duration,
        cancel: Option<Arc<AtomicBool>>,
    ) -> io::Result<RpcReply> {
        let (send, receive) = mpsc::sync_channel(1);
        let rpc = {
            let mut state = self.shared.state.lock().expect("voice proxy poisoned");
            if state.closed || state.rpc.len() >= RPC_LIMIT || state.auxiliary.len() >= 16 {
                return Err(io::Error::other("voice IPC unavailable"));
            }
            state.next_rpc += 1;
            let rpc = state.next_rpc;
            state.rpc.insert(rpc, send);
            state.auxiliary.push_back(command(rpc));
            self.shared.wake.notify_all();
            rpc
        };
        let until = Instant::now() + timeout;
        let mut cancelled = false;
        loop {
            if !cancelled && cancel.as_ref().is_some_and(|c| c.load(Ordering::Acquire)) {
                cancelled = true;
                let mut state = self.shared.state.lock().expect("voice proxy poisoned");
                if let Some(index) = state.auxiliary.iter().position(
                    |command| matches!(command, Command::Scan { rpc: queued, .. } if *queued == rpc),
                ) {
                    state.auxiliary.remove(index);
                    if let Some(reply) = state.rpc.remove(&rpc) {
                        let _ = reply.try_send(RpcReply::Scan(Vec::new()));
                    }
                } else if state.rpc.contains_key(&rpc) && !state.cancel_scan.contains(&rpc) {
                    state.cancel_scan.push_back(rpc);
                }
                self.shared.wake.notify_all();
            }
            match receive.recv_timeout(Duration::from_millis(20)) {
                Ok(RpcReply::Error(error)) => return Err(io::Error::other(error)),
                Ok(reply) => return Ok(reply),
                Err(mpsc::RecvTimeoutError::Disconnected) => {
                    return Err(io::Error::other("voice IPC disconnected"))
                }
                Err(mpsc::RecvTimeoutError::Timeout) if Instant::now() >= until => {
                    let mut state = self.shared.state.lock().expect("voice proxy poisoned");
                    state.rpc.remove(&rpc);
                    state.io_failure = Some("语音设备请求超时。".into());
                    self.shared.wake.notify_all();
                    return Err(io::Error::new(
                        io::ErrorKind::TimedOut,
                        "voice IPC request timed out",
                    ));
                }
                _ => {}
            }
        }
    }
}

impl VoiceControl for RuntimeHandle {
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

fn replace(state: &mut State, stage: RuntimeStage, session_id: Option<u32>) {
    state.auxiliary.retain(|command| {
        if let Command::Scan { rpc, .. } = command {
            if let Some(reply) = state.rpc.remove(rpc) {
                let _ = reply.try_send(RpcReply::Error("语音会话已切换。".into()));
            }
        }
        matches!(command, Command::Devices { .. })
    });
    let id = state.snapshot.request_id + 1;
    state.snapshot = RuntimeSnapshot {
        request_id: id,
        stage,
        session_id,
        intent: state.snapshot.intent.clone(),
        ..Default::default()
    };
}

fn fingerprints(request: &StartVoice) -> [[u8; 32]; 2] {
    [
        Sha256::digest(request.upstream_key).into(),
        Sha256::digest(request.downstream_key).into(),
    ]
}

fn supervise(
    path: PathBuf,
    args: Vec<String>,
    shared: Arc<Shared>,
    store: Option<Arc<update::EngineStore>>,
) {
    loop {
        {
            let mut state = shared.state.lock().expect("voice proxy poisoned");
            if state.closed {
                break;
            }
            state.generation += 1;
            state.io_failure = None;
            state.last_reply = Instant::now();
            state.writing_since = None;
            state.unacked_since = None;
        }
        let selected = store
            .as_ref()
            .and_then(|store| store.begin_start().ok())
            .flatten();
        let selected_path = selected.as_ref().map(|s| s.path.as_path()).unwrap_or(&path);
        let context = selected
            .as_ref()
            .zip(store.as_ref())
            .map(|(s, store)| (Arc::clone(store), s.version.clone()));
        let result = run_child(selected_path, &args, &shared, context);
        let (closed, activating) = {
            let state = shared.state.lock().expect("voice proxy poisoned");
            (state.closed, state.activate_requested)
        };
        if !closed && !activating {
            if let Some((selected, store)) = selected.as_ref().zip(store.as_ref()) {
                let _ = store.reject(&selected.version);
            }
        }
        let mut state = shared.state.lock().expect("voice proxy poisoned");
        state.ready = false;
        // Idle activation seeds Stop before spawning. A spawn/handshake failure
        // must still restart the verified fallback without another user command.
        let fallback_idle = selected.is_some()
            && !activating
            && state.snapshot.session_id.is_none()
            && state.snapshot.mic.is_none()
            && matches!(state.pending, None | Some(Command::Stop));
        for (_, reply) in state.rpc.drain() {
            let _ = reply.try_send(RpcReply::Error("语音进程已停止。".into()));
        }
        state.auxiliary.clear();
        state.cancel_scan.clear();
        let restart = if state.process_id.is_some() {
            match state.pending.take() {
                Some(Command::StartVoice(start))
                    if fingerprints(&start)
                        .iter()
                        .all(|key| !state.seen_sessions.contains_key(key)) =>
                {
                    Some(Command::StartVoice(start))
                }
                Some(Command::StartMic(devices)) => Some(Command::StartMic(devices)),
                _ => None,
            }
        } else {
            None
        };
        state.process_id = None;
        state.pending = restart;
        state.activate_requested = false;
        if state.closed {
            break;
        }
        state.reauthenticate =
            state.snapshot.session_id.is_some() && !state.seen_sessions.is_empty();
        state.snapshot.voice = None;
        state.snapshot.mic = None;
        state.snapshot.stage = RuntimeStage::Failed;
        state.snapshot.error = Some(
            result
                .err()
                .map(|e| format!("语音进程无法运行：{e}"))
                .unwrap_or_else(|| "语音进程已退出。".into()),
        );
        if activating || fallback_idle {
            // Seed the new host with the current lifecycle ID and intentions.
            // An idle replacement otherwise starts at ID 0 and cannot publish
            // snapshots matching a parent that has already retired a mic check.
            state.pending.get_or_insert(Command::Stop);
            state.snapshot.stage = RuntimeStage::Preparing;
            state.snapshot.error = None;
            continue;
        }
        loop {
            if state.closed {
                return;
            }
            if matches!(
                state.pending,
                Some(Command::StartVoice(_)) | Some(Command::StartMic(_))
            ) {
                break;
            }
            if matches!(state.pending, Some(Command::Stop)) {
                state.pending = None;
                state.snapshot.stage = RuntimeStage::Idle;
            }
            state = shared.wake.wait(state).expect("voice proxy poisoned");
        }
    }
}

fn run_child(
    path: &Path,
    args: &[String],
    shared: &Arc<Shared>,
    update: Option<(Arc<update::EngineStore>, String)>,
) -> io::Result<()> {
    let mut command = ProcessCommand::new(path);
    command
        .args(args)
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::null());
    #[cfg(windows)]
    {
        use std::os::windows::process::CommandExt;
        command.creation_flags(0x08000000); // CREATE_NO_WINDOW
    }
    let mut child = command.spawn()?;
    shared
        .state
        .lock()
        .expect("voice proxy poisoned")
        .process_id = Some(child.id());
    let result = communicate(&mut child, shared, update);
    // Always reap before another engine may own a device or use new keys.
    let _ = child.kill();
    let _ = child.wait();
    result
}

fn communicate(
    child: &mut Child,
    shared: &Arc<Shared>,
    update: Option<(Arc<update::EngineStore>, String)>,
) -> io::Result<()> {
    let input = child
        .stdin
        .take()
        .ok_or_else(|| io::Error::other("missing voice stdin"))?;
    let output = child
        .stdout
        .take()
        .ok_or_else(|| io::Error::other("missing voice stdout"))?;
    let ended = Arc::new(AtomicBool::new(false));
    let generation = shared
        .state
        .lock()
        .expect("voice proxy poisoned")
        .generation;
    let reader_shared = Arc::clone(shared);
    let reader_end = Arc::clone(&ended);
    std::thread::spawn(move || {
        let mut output = output;
        let result = (|| -> io::Result<()> {
            let hello: Hello = ipc::read_frame(&mut output)?;
            if hello.protocol != ipc::VERSION
                || hello.engine_version.len() > 64
                || update
                    .as_ref()
                    .is_some_and(|(_, expected)| *expected != hello.engine_version)
            {
                return Err(io::Error::other("incompatible voice IPC version"));
            }
            {
                let mut state = reader_shared.state.lock().expect("voice proxy poisoned");
                if state.generation != generation {
                    return Ok(());
                }
                state.ready = true;
                state.reauthenticate = false;
                state.engine_version = hello.engine_version;
                state.last_reply = Instant::now();
                if state.unacked_since.is_some() {
                    state.unacked_since = Some(Instant::now());
                }
                reader_shared.wake.notify_all();
            }
            let mut confirmed = false;
            while !reader_end.load(Ordering::Acquire) {
                let reply: Reply = ipc::read_frame(&mut output)?;
                let mut state = reader_shared.state.lock().expect("voice proxy poisoned");
                if state.generation != generation {
                    break;
                }
                let mut confirm = false;
                match reply {
                    Reply::Snapshot {
                        revision,
                        requires_auth,
                        mut snapshot,
                    } => {
                        if revision == state.revision
                            && snapshot.request_id == state.snapshot.request_id
                            && (!state.reauthenticate || requires_auth)
                        {
                            state.last_reply = Instant::now();
                            state.unacked_since = None;
                            if requires_auth {
                                state.reauthenticate = true;
                            }
                            snapshot.intent = state.snapshot.intent.clone();
                            state.snapshot = *snapshot;
                            confirm =
                                !requires_auth && state.snapshot.stage != RuntimeStage::Failed;
                        }
                    }
                    Reply::Rpc { rpc, reply } => {
                        if let Some(send) = state.rpc.remove(&rpc) {
                            let _ = send.try_send(reply);
                        }
                    }
                }
                drop(state);
                if confirm && !confirmed {
                    if let Some((store, version)) = &update {
                        store.confirm(version)?;
                    }
                    confirmed = true;
                }
            }
            Ok(())
        })();
        if let Err(error) = result {
            signal_failure(&reader_shared, generation, error);
        }
        reader_end.store(true, Ordering::Release);
        reader_shared.wake.notify_all();
    });
    let writer_shared = Arc::clone(shared);
    let writer_end = Arc::clone(&ended);
    std::thread::spawn(move || {
        let mut input = input;
        let result = (|| -> io::Result<()> {
            ipc::write_frame(
                &mut input,
                &Hello {
                    protocol: ipc::VERSION,
                    engine_version: String::new(),
                },
            )?;
            loop {
                let request = {
                    let mut state = writer_shared.state.lock().expect("voice proxy poisoned");
                    loop {
                        if writer_end.load(Ordering::Acquire)
                            || state.generation != generation
                            || state.io_failure.is_some()
                        {
                            return Ok(());
                        }
                        if state.ready {
                            let command = if state.closed {
                                Some(Command::Shutdown)
                            } else if let Some(command) = state.pending.take() {
                                Some(command)
                            } else if let Some(rpc) = state.cancel_scan.pop_front() {
                                Some(Command::CancelScan { rpc })
                            } else if state.dirty {
                                state.dirty = false;
                                Some(Command::Controls)
                            } else {
                                state.auxiliary.pop_front()
                            };
                            if let Some(command) = command {
                                if let Command::StartVoice(ref start) = command {
                                    for fingerprint in fingerprints(start) {
                                        state.seen_sessions.insert(fingerprint, generation);
                                    }
                                }
                                state.writing_since = Some(Instant::now());
                                break Request {
                                    id: state.snapshot.request_id,
                                    revision: state.revision,
                                    intent: state.snapshot.intent.clone(),
                                    volumes: state.volumes.clone(),
                                    command,
                                };
                            }
                        }
                        state = writer_shared
                            .wake
                            .wait_timeout(state, Duration::from_millis(50))
                            .expect("voice proxy poisoned")
                            .0;
                    }
                };
                ipc::write_frame(&mut input, &request)?;
                {
                    let mut state = writer_shared.state.lock().expect("voice proxy poisoned");
                    if state.generation == generation {
                        state.writing_since = None;
                    }
                }
                if matches!(request.command, Command::Shutdown) {
                    return Ok(());
                }
            }
        })();
        if let Err(error) = result {
            signal_failure(&writer_shared, generation, error);
        }
    });
    let mut shutdown_at = None;
    loop {
        if let Some(status) = child.try_wait()? {
            ended.store(true, Ordering::Release);
            shared.wake.notify_all();
            return if status.success() {
                Ok(())
            } else {
                Err(io::Error::other("voice process exited unsuccessfully"))
            };
        }
        let reason = {
            let state = shared.state.lock().expect("voice proxy poisoned");
            if state.closed {
                shutdown_at.get_or_insert_with(Instant::now);
            }
            if shutdown_at.is_some_and(|at: Instant| at.elapsed() > Duration::from_secs(2)) {
                Some("voice shutdown timeout".into())
            } else if let Some(reason) = &state.io_failure {
                Some(reason.clone())
            } else if state
                .writing_since
                .is_some_and(|at| at.elapsed() > WRITE_TIMEOUT)
            {
                Some("voice IPC write timeout".into())
            } else if state.last_reply.elapsed() > HEARTBEAT_TIMEOUT {
                Some("voice IPC handshake/heartbeat timeout".into())
            } else if state.ready
                && state
                    .unacked_since
                    .is_some_and(|at| at.elapsed() > Duration::from_millis(500))
            {
                Some("voice control acknowledgement timeout".into())
            } else {
                None
            }
        };
        if let Some(reason) = reason {
            ended.store(true, Ordering::Release);
            shared.wake.notify_all();
            return Err(io::Error::other(reason));
        }
        std::thread::sleep(Duration::from_millis(20));
    }
}

fn signal_failure(shared: &Shared, generation: u64, error: io::Error) {
    let mut state = shared.state.lock().expect("voice proxy poisoned");
    if state.generation == generation {
        state.io_failure = Some(format!("voice IPC stopped ({:?})", error.kind()));
        shared.wake.notify_all();
    }
}
