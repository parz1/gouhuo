// SPDX-License-Identifier: GPL-3.0-or-later
//! Reproducible call integration: real TLS authentication/control, Hub UDP
//! routing, Opus pipelines and the call projection, with no physical devices.
//!
//! Run with `cargo test -p client-runtime --release --test call_session --
//! --nocapture`. The stage evidence records facts, not GUI frame rates or a
//! hardware latency claim. Administrator mute is explicitly a constraint
//! injection: the current control protocol has no moderator-mute command.

use std::collections::BTreeMap;
use std::io;
use std::net::{TcpListener, UdpSocket};
use std::sync::atomic::{AtomicBool, AtomicUsize, Ordering};
use std::sync::{mpsc, Arc, Mutex};
use std::thread::ThreadId;
use std::time::{Duration, Instant};

use client_core::{Client, Event, Options};
use client_runtime::call::{CallCommand, CallController, CallState, CallViewModel, CommandResult};
use client_runtime::self_state::{ConnectionState, SelfStatus};
use client_runtime::{
    AudioBackend, Devices, OpenedCapture, RuntimeHandle, RuntimeStage, StartVoice, VoiceRuntime,
};
use protocol::{Invite, PublicKey};
use server::conn::Hub;
use server::state::{Config, Server};
use transport::{server_config, ServerCert};
use voice_core::audio::{Capture, Render};
use voice_core::identity::Identity;
use voice_core::pipeline::TransmitMode;

const DEADLINE: Duration = Duration::from_secs(5);

/// One capture destructor can be held while commands and a new authenticated
/// session are submitted. This represents a slow driver teardown, not a sleep
/// used to guess whether the lifecycle worker has reached a particular line.
struct RetireGate {
    entered: mpsc::Sender<()>,
    release: mpsc::Receiver<()>,
}

#[derive(Default)]
struct ControlledAudio {
    silent: AtomicBool,
    opens: Mutex<Vec<Option<String>>>,
    drops: AtomicUsize,
    audible_frames: AtomicUsize,
    retire_gate: Mutex<Option<RetireGate>>,
}

impl ControlledAudio {
    fn hold_next_retirement(&self) -> (mpsc::Receiver<()>, mpsc::Sender<()>) {
        let (entered, seen) = mpsc::channel();
        let (release, released) = mpsc::channel();
        assert!(self.retire_gate.lock().unwrap().is_none());
        *self.retire_gate.lock().unwrap() = Some(RetireGate {
            entered,
            release: released,
        });
        (seen, release)
    }
}

struct ToneCapture {
    audio: Arc<ControlledAudio>,
    owner: ThreadId,
    sample: usize,
}

impl Capture for ToneCapture {
    fn read(&mut self, output: &mut [f32]) -> io::Result<bool> {
        assert_eq!(self.owner, std::thread::current().id());
        std::thread::sleep(Duration::from_millis(10));
        let gain = if self.audio.silent.load(Ordering::SeqCst) {
            0.0
        } else {
            0.2
        };
        for sample in output {
            *sample = (self.sample as f32 * 0.13).sin() * gain;
            self.sample += 1;
        }
        Ok(true)
    }
}

impl Drop for ToneCapture {
    fn drop(&mut self) {
        assert_eq!(self.owner, std::thread::current().id());
        // Take and release the metadata lock before waiting, as a real backend
        // must not make fast diagnostic reads depend on driver teardown.
        let gate = self.audio.retire_gate.lock().unwrap().take();
        if let Some(gate) = gate {
            let _ = gate.entered.send(());
            let _ = gate.release.recv_timeout(DEADLINE);
        }
        self.audio.drops.fetch_add(1, Ordering::SeqCst);
    }
}

struct ObservedRender {
    audio: Arc<ControlledAudio>,
    owner: ThreadId,
}

impl Render for ObservedRender {
    fn write(&mut self, frame: &[f32]) -> io::Result<()> {
        assert_eq!(self.owner, std::thread::current().id());
        if frame.iter().any(|sample| sample.abs() > 0.001) {
            self.audio.audible_frames.fetch_add(1, Ordering::SeqCst);
        }
        std::thread::sleep(Duration::from_millis(10));
        Ok(())
    }
}

struct Backend(Arc<ControlledAudio>);

impl AudioBackend for Backend {
    fn capture(&self, device: Option<&str>) -> io::Result<OpenedCapture> {
        self.0.opens.lock().unwrap().push(device.map(str::to_owned));
        Ok(OpenedCapture::new(Box::new(ToneCapture {
            audio: Arc::clone(&self.0),
            owner: std::thread::current().id(),
            sample: 0,
        })))
    }

    fn render(&self, _: Option<&str>) -> io::Result<Box<dyn Render>> {
        Ok(Box::new(ObservedRender {
            audio: Arc::clone(&self.0),
            owner: std::thread::current().id(),
        }))
    }
}

struct LocalServer {
    invite: Invite,
    hub: Arc<Hub>,
}

impl LocalServer {
    fn start(admin: PublicKey) -> Self {
        let cert = ServerCert::generate().unwrap();
        let listener = TcpListener::bind("127.0.0.1:0").unwrap();
        let address = listener.local_addr().unwrap();
        let invite = Invite {
            host: address.ip().to_string(),
            port: address.port(),
            cert: cert.fingerprint(),
            code: None,
        };
        let hub = Arc::new(Hub::new(
            Server::new(Config {
                admin_keys: vec![admin],
                ..Default::default()
            }),
            UdpSocket::bind("127.0.0.1:0").unwrap(),
        ));
        let tls = Arc::new(server_config(&cert).unwrap());
        let control = Arc::clone(&hub);
        std::thread::spawn(move || server::accept_loop(listener, tls, control));
        let voice = Arc::clone(&hub);
        std::thread::spawn(move || voice.run_voice());
        Self { invite, hub }
    }
}

fn options() -> Options {
    Options {
        heartbeat: Duration::from_millis(100),
        liveness_timeout: Duration::from_secs(3),
        reconnect_first: Duration::from_millis(150),
        reconnect_max: Duration::from_millis(500),
    }
}

struct Endpoint {
    identity: Identity,
    client: Client,
    events: mpsc::Receiver<Event>,
    runtime: VoiceRuntime,
    audio: Arc<ControlledAudio>,
    call: CallState,
    devices: Devices,
}

impl Endpoint {
    fn connect(server: &LocalServer, identity: Identity, name: &str) -> Self {
        let (client, events) =
            Client::connect_with(&server.invite.to_url().unwrap(), &identity, name, options())
                .unwrap();
        let audio = Arc::new(ControlledAudio::default());
        let runtime = VoiceRuntime::new(Arc::new(Backend(Arc::clone(&audio)))).unwrap();
        let mut call = CallState::default();
        call.connected();
        Self {
            identity,
            client,
            events,
            runtime,
            audio,
            call,
            devices: Devices::default(),
        }
    }

    fn handle(&self) -> RuntimeHandle {
        self.runtime.handle()
    }

    fn start(&self) -> u64 {
        let (session_id, server, keys) = self.client.voice_endpoint_session();
        let handle = self.handle();
        // Match the host start path: mode changes clear an old PTT press;
        // current local mute/deafen intent is never replaced by a roster echo.
        handle.set_mode(handle.mode());
        handle.set_server_muted(
            self.client
                .roster()
                .my_user()
                .is_some_and(|u| u.server_muted),
        );
        handle.start_voice(StartVoice {
            host: server.ip().to_string(),
            udp_port: server.port(),
            session_id,
            sequences: Arc::clone(&keys.sequences),
            upstream_key: *keys.upstream.as_bytes(),
            downstream_key: *keys.downstream.as_bytes(),
            devices: self.devices.clone(),
        })
    }

    fn view(&self) -> CallViewModel {
        let snapshot = self.handle().snapshot();
        let roster = self.client.roster();
        CallViewModel::project(&self.call, &snapshot, true, Some(&roster), &BTreeMap::new())
    }

    fn command(&self, command: CallCommand) -> CommandResult {
        CallController::dispatch(command, &self.client, &self.handle())
    }

    fn ptt(&self, down: bool) {
        assert_eq!(
            CallController::set_ptt(&self.handle(), self.call.connection(), down),
            CommandResult::Applied,
        );
    }

    fn sent(&self) -> u64 {
        self.handle().snapshot().voice.unwrap().packets_sent
    }
}

impl Drop for Endpoint {
    fn drop(&mut self) {
        self.client.disconnect();
        self.handle().shutdown();
        // This test is an event-loop-independent shutdown coordinator.
        let _ = self.runtime.wait_stopped(DEADLINE);
    }
}

fn wait(description: &str, mut condition: impl FnMut() -> bool) {
    let until = Instant::now() + DEADLINE;
    while !condition() {
        assert!(Instant::now() < until, "timed out: {description}");
        std::thread::sleep(Duration::from_millis(5));
    }
}

fn ready(endpoint: &Endpoint) -> bool {
    endpoint
        .handle()
        .snapshot()
        .voice
        .is_some_and(|s| s.input_available && s.render_available && s.udp_ok && s.error.is_none())
}

/// Assert the DTOs consume this actual pipeline snapshot without changing its
/// sending meaning. Receivers additionally require real authenticated speech.
fn assert_projection(endpoint: &Endpoint, expected_sending: bool) {
    let view = endpoint.view();
    assert_eq!(view.audio.self_state.transmitting, expected_sending);
    let me = view.members.iter().find(|m| m.is_me).unwrap();
    let row = view.rows.iter().find(|r| !r.is_channel && r.is_me).unwrap();
    assert_eq!(me.speaking, expected_sending);
    assert_eq!(row.speaking, expected_sending);
    assert_eq!(me.muted, view.audio.self_state.muted);
    assert_eq!(row.muted, view.audio.self_state.muted);
}

fn received_speech(receiver: &Endpoint, sender: u32) -> bool {
    let view = receiver.view();
    view.audio.speaking.contains(&sender)
        && view.members.iter().any(|m| m.id == sender && m.speaking)
        && view
            .rows
            .iter()
            .any(|r| !r.is_channel && r.id == sender && r.speaking)
}

fn quiet(sender: &Endpoint, receiver: &Endpoint) {
    wait("sending stopped and remote terminator consumed", || {
        !sender
            .handle()
            .snapshot()
            .voice
            .is_some_and(|s| s.transmitting)
            && !received_speech(receiver, sender.client.session_id())
    });
    // Allow the single capture/encoder frame already in progress to finish;
    // packets_sent counts actual speech submissions, not keepalive replies.
    std::thread::sleep(Duration::from_millis(30));
    let sent = sender.sent();
    std::thread::sleep(Duration::from_millis(100));
    assert_eq!(sender.sent(), sent, "audio escaped a closed sending gate");
    assert_projection(sender, false);
}

fn evidence(stage: &str, start: Instant, a: &Endpoint, b: &Endpoint) {
    let facts = |side: &Endpoint| {
        let snapshot = side.handle().snapshot();
        let view = side.view();
        let stats = snapshot.voice.as_ref();
        format!(
            "session={} request={} stage={:?} status={:?} tx={} sent={} received={} audible_frames={}",
            side.client.session_id(), snapshot.request_id, snapshot.stage,
            view.audio.self_state.status, view.audio.self_state.transmitting,
            stats.map_or(0, |s| s.packets_sent), stats.map_or(0, |s| s.packets_received),
            side.audio.audible_frames.load(Ordering::SeqCst),
        )
    };
    eprintln!(
        "[call-session +{}ms] {stage}\n  admin: {}\n  member: {}",
        start.elapsed().as_millis(),
        facts(a),
        facts(b)
    );
}

#[test]
fn real_tls_udp_call_lifecycle_and_projection() {
    let start = Instant::now();
    let admin_identity = Identity::generate().unwrap();
    let server = LocalServer::start(admin_identity.public_key());
    let admin = Endpoint::connect(&server, admin_identity, "admin");
    let mut member = Endpoint::connect(&server, Identity::generate().unwrap(), "member");
    wait("two real authenticated members in one channel", || {
        admin.client.roster().users.len() == 2 && member.client.roster().users.len() == 2
    });
    assert!(admin.client.roster().is_admin());
    assert_eq!(
        admin.client.roster().my_channel(),
        member.client.roster().my_channel()
    );
    admin.handle().set_mode(TransmitMode::Always);
    member.handle().set_mode(TransmitMode::PushToTalk);
    admin.start();
    member.start();
    wait(
        "both pipelines, authenticated UDP probe and decoded playback",
        || {
            ready(&admin)
                && ready(&member)
                && received_speech(&member, admin.client.session_id())
                && member.audio.audible_frames.load(Ordering::SeqCst) > 0
        },
    );
    assert_projection(&admin, true);
    assert_projection(&member, false);
    assert_eq!(
        member.view().audio.self_state.status,
        SelfStatus::PushToTalkWaiting
    );
    evidence(
        "TLS / UDP probe / same-channel Opus playback",
        start,
        &admin,
        &member,
    );

    member.ptt(true);
    wait(
        "PTT produces authenticated speech on the other client",
        || {
            member.sent() > 3
                && received_speech(&admin, member.client.session_id())
                && admin.audio.audible_frames.load(Ordering::SeqCst) > 0
        },
    );
    assert_projection(&member, true);
    member.ptt(false);
    assert!(!member.handle().snapshot().voice.unwrap().transmitting);
    quiet(&member, &admin);
    evidence(
        "PTT press / immediate release / remote terminator",
        start,
        &admin,
        &member,
    );

    member.ptt(true);
    wait("PTT resumed before mute", || {
        received_speech(&admin, member.client.session_id())
    });
    assert!(matches!(
        member.command(CallCommand::ToggleMute),
        CommandResult::SelfStateChanged {
            muted: true,
            deafened: false
        }
    ));
    quiet(&member, &admin);
    wait("TLS mute echo reached both rosters", || {
        member.client.roster().my_user().unwrap().self_muted
            && admin.client.roster().users[&member.client.session_id()].self_muted
    });
    assert_eq!(member.view().audio.self_state.status, SelfStatus::Muted);
    assert!(member.view().audio.self_state.input_available);
    member.command(CallCommand::ToggleMute);
    wait("unmute resumes pressed PTT", || {
        received_speech(&admin, member.client.session_id())
    });

    member.command(CallCommand::ToggleDeafen);
    quiet(&member, &admin);
    wait("deafen echo and local projection", || {
        member.client.roster().my_user().unwrap().self_deafened
            && member.view().audio.self_state.status == SelfStatus::Deafened
    });
    std::thread::sleep(Duration::from_millis(30));
    let heard = member.audio.audible_frames.load(Ordering::SeqCst);
    let admin_sent = admin.sent();
    std::thread::sleep(Duration::from_millis(120));
    assert_eq!(
        member.audio.audible_frames.load(Ordering::SeqCst),
        heard,
        "deafened playback was not silent"
    );
    assert!(
        admin.sent() > admin_sent,
        "peer stopped transmitting instead of local deafen"
    );
    member.command(CallCommand::ToggleDeafen);
    assert!(
        member.handle().snapshot().intent.muted,
        "opening ears reopened the microphone"
    );
    member.command(CallCommand::ToggleMute);
    wait("opening ears and explicitly unmuting resumes voice", || {
        received_speech(&admin, member.client.session_id())
            && member.audio.audible_frames.load(Ordering::SeqCst) > heard
    });
    evidence(
        "local mute / TLS echoes / deafen silences render",
        start,
        &admin,
        &member,
    );

    member.handle().set_mode(TransmitMode::VoiceActivity {
        threshold_db: -45.0,
    });
    wait("VAD speech", || {
        member.handle().snapshot().voice.unwrap().transmitting
    });
    member.audio.silent.store(true, Ordering::SeqCst);
    wait("silent input is still within the real VAD tail", || {
        let stats = member.handle().snapshot().voice.unwrap();
        stats.input_db < -80.0 && stats.transmitting
    });
    let tail_start = member.sent();
    wait("silent tail submits more real UDP audio", || {
        member.sent() > tail_start
    });
    assert_projection(&member, true);
    quiet(&member, &admin);
    assert_eq!(
        member.view().audio.self_state.status,
        SelfStatus::VoiceActivityWaiting
    );
    member.audio.silent.store(false, Ordering::SeqCst);
    wait("VAD resumes on loud input", || {
        received_speech(&admin, member.client.session_id())
    });
    evidence(
        "VAD real silent hangover / eventual terminator",
        start,
        &admin,
        &member,
    );

    // The protocol currently has no administrator-mute operation. Exercise the
    // established runtime constraint boundary against real UDP instead of
    // inventing a control command or claiming a moderator protocol round trip.
    member.handle().set_server_muted(true);
    quiet(&member, &admin);
    let forced = member.view();
    assert_eq!(forced.audio.self_state.status, SelfStatus::ServerMuted);
    assert!(
        !forced.audio.self_muted,
        "moderator constraint overwrote local toggle intent"
    );
    assert!(forced.members.iter().find(|m| m.is_me).unwrap().muted);
    member.handle().set_server_muted(false);
    wait("clearing injected moderator constraint resumes VAD", || {
        received_speech(&admin, member.client.session_id())
    });
    evidence(
        "moderator constraint injection / real UDP gated (no moderator protocol)",
        start,
        &admin,
        &member,
    );

    member.handle().set_mode(TransmitMode::PushToTalk);
    member.command(CallCommand::ToggleDeafen);
    quiet(&member, &admin);
    wait(
        "closed mic / ears acknowledged before TCP replacement",
        || {
            member
                .client
                .roster()
                .my_user()
                .is_some_and(|u| u.self_muted && u.self_deafened)
        },
    );
    let old_session = member.client.session_id();
    let old_keys = member.client.voice_keys();
    member.client.reconnect_transport();
    let mut saw_reconnecting = false;
    wait(
        "TCP reconnect events and fresh authenticated session",
        || {
            while let Ok(event) = member.events.try_recv() {
                match event {
                    Event::Reconnecting {
                        attempt, reason, ..
                    } => {
                        saw_reconnecting = true;
                        member.call.reconnecting(attempt, reason);
                        member.handle().quiesce();
                        let stale = member.view();
                        assert_eq!(stale.audio.connection, ConnectionState::Reconnecting);
                        assert!(!stale.audio.self_state.input_available);
                        assert!(!stale.audio.self_state.transmitting);
                        assert!(stale.members.iter().all(|m| !m.speaking));
                        assert!(stale.rows.iter().all(|r| !r.speaking));
                    }
                    Event::Reconnected { .. } => {
                        assert!(saw_reconnecting, "reconnected before interruption event");
                        member.call.connected();
                        member.handle().clear_volumes();
                        member.start();
                        return true;
                    }
                    Event::Disconnected(why) => panic!("reconnect ended unexpectedly: {why:?}"),
                    _ => {}
                }
            }
            false
        },
    );
    let fresh_session = member.client.session_id();
    assert_ne!(fresh_session, old_session);
    let fresh_keys = member.client.voice_keys();
    assert!(!Arc::ptr_eq(&old_keys, &fresh_keys));
    assert!(
        old_keys.upstream.as_bytes() != fresh_keys.upstream.as_bytes(),
        "TLS reconnect reused upstream key material"
    );
    assert!(
        old_keys.downstream.as_bytes() != fresh_keys.downstream.as_bytes(),
        "TLS reconnect reused downstream key material"
    );
    wait(
        "fresh runtime ready, intent restored by TLS and obsolete roster session gone",
        || {
            ready(&member)
                && member
                    .client
                    .roster()
                    .my_user()
                    .is_some_and(|u| u.self_muted && u.self_deafened)
                && !admin.client.roster().users.contains_key(&old_session)
                && admin.client.roster().users.contains_key(&fresh_session)
        },
    );
    let snapshot = member.handle().snapshot();
    assert_eq!(snapshot.session_id, Some(fresh_session));
    assert!(snapshot.intent.muted && snapshot.intent.deafened);
    assert_projection(&member, false);
    assert!(!admin.view().members.iter().any(|m| m.id == old_session));
    member.command(CallCommand::ToggleDeafen);
    member.command(CallCommand::ToggleMute);
    member.ptt(true);
    wait("fresh-session voice reaches existing peer", || {
        received_speech(&admin, fresh_session)
    });
    evidence(
        "TCP reconnect / fresh keys-session / retained mute-deafen / new-session voice",
        start,
        &admin,
        &member,
    );

    let opens = member.audio.opens.lock().unwrap().len();
    let (retiring, release) = member.audio.hold_next_retirement();
    member.devices.capture = Some("superseded-device".into());
    member.start();
    retiring
        .recv_timeout(DEADLINE)
        .expect("old capture teardown did not enter gate");
    // A newer device choice supersedes queued preparation while old Drop waits.
    member.devices.capture = Some("latest-device".into());
    let latest_request = member.start();
    member.ptt(true);
    assert_eq!(member.handle().snapshot().request_id, latest_request);
    assert!(member.handle().snapshot().intent.transmitting);
    assert_eq!(
        member.audio.opens.lock().unwrap().len(),
        opens,
        "candidate opened before old device teardown completed"
    );
    let received_before_replacement = admin.handle().snapshot().voice.unwrap().packets_received;
    release.send(()).unwrap();
    wait("latest device candidate and real peer speech", || {
        ready(&member)
            && member.handle().snapshot().request_id == latest_request
            && member.sent() > 3
            && admin.handle().snapshot().voice.unwrap().packets_received
                > received_before_replacement + 3
            && received_speech(&admin, fresh_session)
    });
    let opened = member.audio.opens.lock().unwrap();
    assert_eq!(opened.len(), opens + 1);
    assert_eq!(opened.last().unwrap().as_deref(), Some("latest-device"));
    assert!(!opened
        .iter()
        .any(|device| device.as_deref() == Some("superseded-device")));
    drop(opened);
    assert_projection(&member, true);
    evidence(
        "device replacement / pending PTT / superseded candidate never opens",
        start,
        &admin,
        &member,
    );

    let leaving_session = member.client.session_id();
    let old_client = member.client.clone();
    let (retiring, release) = member.audio.hold_next_retirement();
    member.handle().stop_after(Duration::from_millis(80));
    assert_eq!(member.command(CallCommand::Leave), CommandResult::Left);
    member.call.offline();
    assert!(member.view().members.iter().all(|m| !m.speaking));
    retiring
        .recv_timeout(DEADLINE)
        .expect("leaving capture did not retire");
    member.call.begin_join();
    let (client, events) = Client::connect_with(
        &server.invite.to_url().unwrap(),
        &member.identity,
        "member",
        options(),
    )
    .unwrap();
    let abandoned_events = std::mem::replace(&mut member.events, events);
    member.client = client;
    member.call.connected();
    assert!(
        !old_client.is_same(&member.client),
        "old event source aliases new client"
    );
    member.handle().set_self_state(false, false);
    member.handle().clear_volumes();
    member.devices.capture = Some("rejoined-device".into());
    let rejoined_request = member.start();
    member.ptt(true);
    let rejoined_session = member.client.session_id();
    assert_ne!(rejoined_session, leaving_session);
    release.send(()).unwrap();
    wait(
        "immediate rejoin survives old Stop and delayed retirement",
        || {
            ready(&member)
                && member.handle().snapshot().request_id == rejoined_request
                && member.sent() > 3
                && received_speech(&admin, rejoined_session)
                && !admin.client.roster().users.contains_key(&leaving_session)
        },
    );
    // Wait past the deliberately queued old deadline: acceptance must be
    // generation-safe, not merely a momentary new-session success.
    std::thread::sleep(Duration::from_millis(160));
    assert_eq!(member.handle().snapshot().request_id, rejoined_request);
    assert_eq!(
        member.handle().snapshot().session_id,
        Some(rejoined_session)
    );
    assert_eq!(member.handle().snapshot().stage, RuntimeStage::Ready);
    assert_projection(&member, true);
    assert!(!admin.view().members.iter().any(|m| m.id == leaving_session));
    // Late events from the abandoned Client can be read, but identity filtering
    // must reject them instead of mutating the new connection's CallState.
    let _late_events: Vec<_> = abandoned_events.try_iter().collect();
    assert!(!old_client.is_same(&member.client));
    assert_eq!(member.call.connection(), ConnectionState::Connected);
    evidence(
        "leave / immediate real TLS rejoin / late Stop deadline cannot retire new call",
        start,
        &admin,
        &member,
    );

    member.client.disconnect();
    admin.client.disconnect();
    member.handle().shutdown();
    admin.handle().shutdown();
    assert!(member.runtime.wait_stopped(DEADLINE));
    assert!(admin.runtime.wait_stopped(DEADLINE));
    wait("server has no authenticated users after shutdown", || {
        server.hub.user_count() == 0
    });
    assert_eq!(
        member.audio.drops.load(Ordering::SeqCst),
        member.audio.opens.lock().unwrap().len()
    );
    assert_eq!(
        admin.audio.drops.load(Ordering::SeqCst),
        admin.audio.opens.lock().unwrap().len()
    );
    eprintln!(
        "[call-session +{}ms] voice workers drained; all controlled captures dropped on their owners",
        start.elapsed().as_millis()
    );
}
