// SPDX-License-Identifier: GPL-3.0-or-later
//! Engine host. The pipe reader never waits for devices, scans, DNS or APM.

use std::collections::BTreeMap;
use std::io::{self, Read, Write};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{mpsc, Arc, Mutex};
use std::time::Duration;

use client_runtime::ipc::{self, Command, Cue, Hello, Reply, Request, RpcReply};
use client_runtime::{
    AudioBackend, OpenedCapture, RuntimeHandle, RuntimeSnapshot, RuntimeStage, VoiceRuntime,
};
use voice_core::audio::{Capture, NullRender, Render, SyntheticCapture};
use voice_core::cue::{chime, connection_chime, Chime};
use voice_core::tts::{speakable_name, Announcer};

#[cfg(windows)]
mod platform_audio;

pub fn desktop_audio() -> io::Result<Arc<dyn AudioBackend>> {
    #[cfg(windows)]
    {
        Ok(Arc::new(platform_audio::DesktopAudio))
    }
    #[cfg(not(windows))]
    {
        Err(io::Error::new(
            io::ErrorKind::Unsupported,
            "desktop audio unavailable",
        ))
    }
}

/// Explicit no-hardware diagnostic backend used by process integration tests.
pub struct SyntheticAudio;
impl AudioBackend for SyntheticAudio {
    fn capture(&self, _: Option<&str>) -> io::Result<OpenedCapture> {
        Ok(OpenedCapture::new(Box::new(
            SyntheticCapture::new(
                voice_core::signal::chirp(48_000, 48_000, 300.0, 3400.0, 0.4)
                    .into_iter()
                    .map(|sample| sample as f32 / 32768.0)
                    .collect(),
            )
            .then_silence(),
        ) as Box<dyn Capture>))
    }
    fn render(&self, _: Option<&str>) -> io::Result<Box<dyn Render>> {
        Ok(Box::new(NullRender::default()))
    }
}

struct Session {
    id: u32,
    upstream: [u8; 32],
    downstream: [u8; 32],
    sequences: Arc<protocol::VoiceSequences>,
}

struct State {
    id: u64,
    applied_id: u64,
    revision: u64,
    pending: Option<Command>,
    session: Option<Session>,
    retired_keys: Vec<([u8; 32], [u8; 32])>,
    error: Option<String>,
    scan: Option<(u64, Arc<AtomicBool>)>,
    announcer: Option<Announcer>,
    volumes: BTreeMap<u32, f32>,
}

pub fn serve(
    mut input: impl Read,
    mut output: impl Write + Send + 'static,
    backend: Arc<dyn AudioBackend>,
) -> io::Result<()> {
    let hello: Hello = ipc::read_frame(&mut input)?;
    if hello.protocol != ipc::VERSION {
        return Err(io::Error::other("incompatible voice IPC version"));
    }
    ipc::write_frame(
        &mut output,
        &Hello {
            protocol: ipc::VERSION,
            engine_version: env!("CARGO_PKG_VERSION").into(),
        },
    )?;
    let runtime = VoiceRuntime::new(backend)?;
    let handle = runtime.handle();
    let shared = Arc::new(Mutex::new(State {
        id: 0,
        applied_id: 0,
        revision: 0,
        pending: None,
        session: None,
        retired_keys: Vec::new(),
        error: None,
        scan: None,
        announcer: None,
        volumes: BTreeMap::new(),
    }));
    let stopped = Arc::new(AtomicBool::new(false));
    let (reply, replies) = mpsc::sync_channel::<Reply>(16);
    let writer_state = Arc::clone(&shared);
    let writer_handle = handle.clone();
    let writer_stopped = Arc::clone(&stopped);
    std::thread::spawn(move || {
        while !writer_stopped.load(Ordering::Acquire) {
            let next = {
                let mut state = writer_state.lock().expect("engine state poisoned");
                if state.scan.is_none() {
                    if let Some(command) = state.pending.take() {
                        state.error = apply_lifecycle(command, &writer_handle, &mut state)
                            .err()
                            .map(|e| e.to_string());
                        state.applied_id = state.id;
                    }
                }
                let mut snapshot = if state.applied_id == state.id {
                    writer_handle.snapshot()
                } else {
                    RuntimeSnapshot {
                        stage: RuntimeStage::Preparing,
                        intent: writer_handle.snapshot().intent,
                        ..Default::default()
                    }
                };
                snapshot.request_id = state.id;
                if let Some(error) = &state.error {
                    snapshot.stage = RuntimeStage::Failed;
                    snapshot.voice = None;
                    snapshot.mic = None;
                    snapshot.error = Some(error.clone());
                }
                Reply::Snapshot {
                    revision: state.revision,
                    requires_auth: state.error.is_some(),
                    snapshot: Box::new(snapshot),
                }
            };
            // A bounded reply lane plus one current snapshot; never queue meters.
            for result in replies.try_iter().take(16) {
                if ipc::write_frame(&mut output, &result).is_err() {
                    writer_stopped.store(true, Ordering::Release);
                    writer_handle.shutdown();
                    return;
                }
            }
            if ipc::write_frame(&mut output, &next).is_err() {
                writer_stopped.store(true, Ordering::Release);
                writer_handle.shutdown();
                return;
            }
            std::thread::sleep(Duration::from_millis(20));
        }
    });
    let result = (|| -> io::Result<()> {
        loop {
            let request = match ipc::read_frame::<Request>(&mut input) {
                Ok(request) => request,
                Err(error) if error.kind() == io::ErrorKind::UnexpectedEof => return Ok(()),
                Err(error) => return Err(error),
            };
            request.validate()?;
            if stopped.load(Ordering::Acquire) {
                return Err(io::Error::other("voice output stopped"));
            }
            let mut state = shared.lock().expect("engine state poisoned");
            if request.id < state.id || request.revision < state.revision {
                continue;
            }
            // Intent and volume updates happen on this fast reader, never on
            // the lifecycle queue. Device/APM initialization cannot block mute.
            apply_controls(&request, &handle, &mut state);
            state.revision = request.revision;
            match request.command {
                Command::Shutdown => return Ok(()),
                Command::Controls => {}
                Command::CancelScan { rpc } => {
                    if let Some((active, cancel)) = &state.scan {
                        if *active == rpc {
                            cancel.store(true, Ordering::Release);
                        }
                    }
                }
                Command::Devices { rpc } => {
                    let replies = reply.clone();
                    std::thread::spawn(move || {
                        let _ = replies.try_send(Reply::Rpc {
                            rpc,
                            reply: devices(),
                        });
                    });
                }
                Command::Scan { rpc, per_device_ms } => {
                    if state.scan.is_some()
                        || state.session.is_some() && handle.snapshot().voice.is_some()
                    {
                        let _ = reply.try_send(Reply::Rpc {
                            rpc,
                            reply: RpcReply::Error("麦克风正在使用。".into()),
                        });
                        continue;
                    }
                    handle.stop();
                    let cancel = Arc::new(AtomicBool::new(false));
                    state.scan = Some((rpc, Arc::clone(&cancel)));
                    let scan_state = Arc::clone(&shared);
                    let scan_handle = handle.clone();
                    let scan_reply = reply.clone();
                    std::thread::spawn(move || {
                        // Wait for retirement before a scanner opens capture.
                        let until = std::time::Instant::now() + Duration::from_secs(3);
                        while scan_handle.snapshot().stage != RuntimeStage::Idle
                            && !cancel.load(Ordering::Acquire)
                            && std::time::Instant::now() < until
                        {
                            std::thread::sleep(Duration::from_millis(20));
                        }
                        let results = if scan_handle.snapshot().stage == RuntimeStage::Idle
                            && !cancel.load(Ordering::Acquire)
                        {
                            voice_core::miccheck::scan_microphones_cancellable(
                                Duration::from_millis(per_device_ms),
                                Arc::clone(&cancel),
                            )
                            .into_iter()
                            .map(|r| ipc::ScanResult {
                                verdict: r.verdict(),
                                hears_something: r.hears_something(),
                                id: r.id,
                                name: r.name,
                                is_hardware: r.is_hardware,
                            })
                            .collect()
                        } else {
                            Vec::new()
                        };
                        // Capture has been destroyed on its owning scan thread.
                        let _ = scan_reply.try_send(Reply::Rpc {
                            rpc,
                            reply: RpcReply::Scan(results),
                        });
                        scan_state.lock().expect("engine state poisoned").scan = None;
                    });
                }
                Command::Notice {
                    cue,
                    name,
                    sound,
                    gain,
                } => {
                    if request.id != state.id {
                        continue;
                    }
                    if let Some(sink) = handle.cues() {
                        if sound {
                            let samples = match cue {
                                Cue::CameIn => chime(Chime::CameIn),
                                Cue::WentOut => chime(Chime::WentOut),
                                Cue::Lost => {
                                    sink.clear();
                                    connection_chime(false)
                                }
                                Cue::Recovered => {
                                    sink.clear();
                                    connection_chime(true)
                                }
                            };
                            sink.push(&samples, gain);
                        }
                        if let Some(name) = name {
                            if state.announcer.is_none() {
                                state.announcer = Announcer::start().ok();
                            }
                            if let Some(announcer) = &state.announcer {
                                let verb = if matches!(cue, Cue::CameIn) {
                                    "进来了"
                                } else {
                                    "走了"
                                };
                                announcer.say(
                                    &format!("{}{verb}", speakable_name(&name)),
                                    sink,
                                    gain,
                                );
                            }
                        }
                    }
                }
                command => {
                    if request.id <= state.id {
                        continue;
                    }
                    if let Some((_, cancel)) = &state.scan {
                        cancel.store(true, Ordering::Release);
                    }
                    // Immediately stop the old transport even if scan retirement
                    // delays admission of a new device owner.
                    handle.quiesce();
                    state.id = request.id;
                    state.error = None;
                    state.pending = Some(command);
                }
            }
        }
    })();
    stopped.store(true, Ordering::Release);
    if let Some((_, cancel)) = &shared.lock().expect("engine state poisoned").scan {
        cancel.store(true, Ordering::Release);
    }
    handle.shutdown();
    let _ = runtime.wait_stopped(Duration::from_secs(2));
    result
}

fn apply_controls(request: &Request, handle: &RuntimeHandle, state: &mut State) {
    let intent = &request.intent;
    handle.set_self_state(intent.muted, intent.deafened);
    handle.set_server_muted(intent.server_muted);
    if handle.mode() != intent.mode {
        handle.set_mode(intent.mode);
    }
    handle.set_transmitting(intent.transmitting);
    handle.set_monitoring(intent.monitoring);
    if state.volumes != request.volumes {
        for member in state
            .volumes
            .keys()
            .filter(|id| !request.volumes.contains_key(id))
        {
            handle.set_volume(*member, 1.0);
        }
        handle.clear_volumes();
        for (&member, &volume) in &request.volumes {
            handle.set_volume(member, volume);
        }
        state.volumes = request.volumes.clone();
    }
}

fn apply_lifecycle(command: Command, handle: &RuntimeHandle, state: &mut State) -> io::Result<()> {
    match command {
        Command::StartVoice(start) => {
            let matches = state.session.as_ref().is_some_and(|s| {
                s.upstream == start.upstream_key
                    && s.downstream == start.downstream_key
                    && s.id == start.session_id
            });
            if !matches {
                let key = (start.upstream_key, start.downstream_key);
                if state.session.as_ref().is_some_and(|s| {
                    [s.upstream, s.downstream]
                        .iter()
                        .any(|old| *old == key.0 || *old == key.1)
                }) || state.retired_keys.iter().any(|old| {
                    [old.0, old.1]
                        .iter()
                        .any(|used| *used == key.0 || *used == key.1)
                }) || state.retired_keys.len() >= 4096
                {
                    return Err(io::Error::other("session keys cannot be reused"));
                }
                if let Some(old) = state.session.take() {
                    state.retired_keys.push((old.upstream, old.downstream));
                }
                state.session = Some(Session {
                    id: start.session_id,
                    upstream: start.upstream_key,
                    downstream: start.downstream_key,
                    sequences: Arc::new(protocol::VoiceSequences::default()),
                });
            }
            let session = state.session.as_ref().expect("engine session");
            handle.start_voice(client_runtime::StartVoice {
                host: start.host,
                udp_port: start.udp_port,
                session_id: start.session_id,
                upstream_key: start.upstream_key,
                downstream_key: start.downstream_key,
                sequences: Arc::clone(&session.sequences),
                devices: start.devices,
            });
        }
        Command::StartMic(devices) => {
            handle.start_mic_check(devices);
        }
        Command::Stop => handle.stop(),
        Command::Quiesce => handle.quiesce(),
        Command::StopAfter(ms) => handle.stop_after(Duration::from_millis(ms)),
        _ => return Err(io::Error::other("invalid lifecycle command")),
    }
    Ok(())
}

fn devices() -> RpcReply {
    #[cfg(windows)]
    {
        use voice_core::wasapi::{list_endpoints, Direction};
        let list = |direction| -> io::Result<Vec<ipc::Device>> {
            Ok(list_endpoints(direction)?
                .into_iter()
                .map(|d| ipc::Device {
                    id: d.id,
                    name: d.name,
                    is_hardware: d.is_hardware,
                })
                .collect())
        };
        match (list(Direction::Capture), list(Direction::Render)) {
            (Ok(capture), Ok(render)) => RpcReply::Devices { capture, render },
            _ => RpcReply::Error("音频设备枚举失败。".into()),
        }
    }
    #[cfg(not(windows))]
    {
        RpcReply::Devices {
            capture: Vec::new(),
            render: Vec::new(),
        }
    }
}
