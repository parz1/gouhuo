// SPDX-License-Identifier: GPL-3.0-or-later
//! Real child process, TLS authentication, encrypted UDP and no physical audio.

use std::net::{TcpListener, UdpSocket};
use std::process::{Child, Command, Stdio};
use std::sync::Arc;
use std::time::{Duration, Instant};

use client_core::{Client, Options};
use client_process::VoiceRuntime;
use client_runtime::ipc::{self, Hello, Request, StartVoice};
use client_runtime::{Devices, RuntimeStage, VoiceIntent};
use protocol::{Invite, VoiceCipher, VoiceHeader, FLAG_KEEPALIVE, FLAG_TERMINATOR};
use server::conn::Hub;
use server::state::{Config, Server};
use transport::{server_config, ServerCert};
use voice_core::identity::Identity;
use voice_types::TransmitMode;

const ENGINE: &str = env!("CARGO_BIN_EXE_gouhuo-voice");

fn signed_store(
    version: &str,
    staged: bool,
) -> (std::path::PathBuf, Arc<client_process::update::EngineStore>) {
    use client_process::update::{Compatibility, EngineStore};
    use ed25519_dalek::SigningKey;
    static ID: std::sync::atomic::AtomicU64 = std::sync::atomic::AtomicU64::new(0);
    let root = std::env::temp_dir().join(format!(
        "gouhuo-engine-activation-{}-{}",
        std::process::id(),
        ID.fetch_add(1, std::sync::atomic::Ordering::Relaxed)
    ));
    let key = SigningKey::from_bytes(&[5; 32]); // Test-only signing seed.
    let store = Arc::new(
        EngineStore::new(
            root.clone(),
            key.verifying_key().to_bytes(),
            Compatibility {
                target: if cfg!(windows) {
                    "windows-x64"
                } else {
                    "linux-x64"
                }
                .into(),
                ipc: ipc::VERSION,
                server_protocol: protocol::control::PROTOCOL_VERSION,
                ui: "0.3.1".into(),
                bundled: "0.0.0".into(),
            },
        )
        .unwrap(),
    );
    if staged {
        stage_fixture_engine(&store, version);
    }
    (root, store)
}

fn stage_fixture_engine(store: &client_process::update::EngineStore, version: &str) {
    use client_process::update::{hex, Manifest};
    use ed25519_dalek::{Signer, SigningKey};
    use sha2::{Digest, Sha256};
    let key = SigningKey::from_bytes(&[5; 32]);
    let bytes = std::fs::read(ENGINE).unwrap();
    let manifest = Manifest {
        schema: 1,
        version: version.into(),
        target: if cfg!(windows) {
            "windows-x64"
        } else {
            "linux-x64"
        }
        .into(),
        ipc: ipc::VERSION,
        server_protocol: protocol::control::PROTOCOL_VERSION,
        min_ui: "0.3.1".into(),
        max_ui: None,
        size: bytes.len() as u64,
        sha256: hex(&Sha256::digest(&bytes)),
    };
    let manifest = serde_json::to_vec(&manifest).unwrap();
    store
        .stage(
            &manifest,
            &key.sign(&manifest).to_bytes(),
            std::path::Path::new(ENGINE),
        )
        .unwrap();
}

#[test]
fn signed_engine_activates_only_after_mic_check_retires() {
    let (root, store) = signed_store("0.1.0", false);
    let runtime =
        VoiceRuntime::managed_with_args(ENGINE, vec!["--synthetic".into()], Arc::clone(&store))
            .unwrap();
    let handle = runtime.handle();
    wait(|| handle.engine_version() == "0.1.0");
    handle.start_mic_check(Devices::default());
    wait(|| handle.snapshot().mic.is_some());
    stage_fixture_engine(&store, "0.1.0");
    assert!(store.has_pending().unwrap());
    assert!(
        !handle.activate_pending(),
        "update retired an active mic check"
    );
    handle.stop();
    wait(|| handle.snapshot().stage == RuntimeStage::Idle);
    let old_pid = handle.process_id();
    assert!(handle.activate_pending());
    wait(|| {
        handle.process_id().is_some()
            && handle.process_id() != old_pid
            && handle.engine_version() == "0.1.0"
            && handle.snapshot().stage == RuntimeStage::Idle
    });
    handle.shutdown();
    assert!(runtime.wait_stopped(Duration::from_secs(3)));
    std::fs::remove_dir_all(root).unwrap();
}

#[test]
fn signed_manifest_version_mismatch_rolls_back_to_bundled_engine() {
    // A valid signature does not excuse a host advertising a different version.
    let (root, store) = signed_store("0.1.1", true);
    let runtime =
        VoiceRuntime::managed_with_args(ENGINE, vec!["--synthetic".into()], Arc::clone(&store))
            .unwrap();
    let handle = runtime.handle();
    wait(|| handle.engine_version() == "0.1.0");
    assert!(store.begin_start().unwrap().is_none());
    handle.start_mic_check(Devices::default());
    wait(|| handle.snapshot().mic.is_some());
    handle.shutdown();
    assert!(runtime.wait_stopped(Duration::from_secs(3)));
    std::fs::remove_dir_all(root).unwrap();
}

fn wait(mut predicate: impl FnMut() -> bool) {
    let until = Instant::now() + Duration::from_secs(5);
    while !predicate() {
        assert!(Instant::now() < until, "process condition timed out");
        std::thread::sleep(Duration::from_millis(10));
    }
}

fn server() -> String {
    let cert = ServerCert::generate().unwrap();
    let listener = TcpListener::bind("127.0.0.1:0").unwrap();
    let tcp = listener.local_addr().unwrap();
    let invite = Invite {
        host: tcp.ip().to_string(),
        port: tcp.port(),
        cert: cert.fingerprint(),
        code: None,
    };
    let tls = Arc::new(server_config(&cert).unwrap());
    let hub = Arc::new(Hub::new(
        Server::new(Config::default()),
        UdpSocket::bind("127.0.0.1:0").unwrap(),
    ));
    let voice = Arc::clone(&hub);
    std::thread::spawn(move || voice.run_voice());
    std::thread::spawn(move || server::accept_loop(listener, tls, hub));
    invite.to_url().unwrap()
}

fn join(invite: &str, name: &str) -> Client {
    Client::connect_with(
        invite,
        &Identity::generate().unwrap(),
        name,
        Options::default(),
    )
    .unwrap()
    .0
}

fn request(client: &Client) -> StartVoice {
    let (session_id, endpoint, keys) = client.voice_endpoint_session();
    StartVoice {
        host: endpoint.ip().to_string(),
        udp_port: endpoint.port(),
        session_id,
        upstream_key: *keys.upstream.as_bytes(),
        downstream_key: *keys.downstream.as_bytes(),
        devices: Devices::default(),
    }
}

fn sequence(socket: &UdpSocket, cipher: &VoiceCipher, session: u32) -> u32 {
    let until = Instant::now() + Duration::from_secs(3);
    let mut packet = [0; 2048];
    let mut plain = Vec::new();
    loop {
        assert!(Instant::now() < until, "no authenticated voice packet");
        if let Ok(n) = socket.recv(&mut packet) {
            let header = cipher.open(&packet[..n], &mut plain).unwrap();
            if header.session == session
                && !header.is_keepalive()
                && header.flags & FLAG_TERMINATOR == 0
            {
                return header.seq;
            }
        }
    }
}

#[test]
fn child_preserves_mute_ptt_and_nonce_sequences_across_device_replacement() {
    let invite = server();
    let speaker = join(&invite, "engine-speaker");
    let listener = join(&invite, "raw-listener");
    let (listener_session, address, listener_keys) = listener.voice_endpoint_session();
    let socket = UdpSocket::bind("127.0.0.1:0").unwrap();
    socket.connect(address).unwrap();
    socket
        .set_read_timeout(Some(Duration::from_millis(50)))
        .unwrap();
    let mut packet = Vec::new();
    VoiceCipher::new(listener_keys.upstream.as_bytes())
        .seal(
            VoiceHeader {
                session: listener_session,
                seq: 0,
                timestamp: 0,
                flags: FLAG_KEEPALIVE,
            },
            &[],
            &mut packet,
        )
        .unwrap();
    socket.send(&packet).unwrap();
    let cipher = VoiceCipher::new(listener_keys.downstream.as_bytes());
    let runtime = VoiceRuntime::with_args(ENGINE, vec!["--synthetic".into()]).unwrap();
    let handle = runtime.handle();
    handle.set_mode(TransmitMode::PushToTalk);
    handle.set_muted(true);
    let start = request(&speaker);
    handle.start_voice(start.clone());
    wait(|| handle.snapshot().stage == RuntimeStage::Ready);
    assert_eq!(handle.snapshot().voice.unwrap().packets_sent, 0);
    assert_eq!(handle.engine_version(), "0.1.0");
    handle.set_muted(false);
    handle.set_transmitting(true);
    wait(|| {
        handle
            .snapshot()
            .voice
            .is_some_and(|s| s.packets_sent >= 12)
    });
    handle.set_transmitting(false);
    assert!(!handle.snapshot().voice.unwrap().transmitting);
    std::thread::sleep(Duration::from_millis(100));
    let mut largest = sequence(&socket, &cipher, start.session_id);
    let mut bytes = [0; 2048];
    let mut plain = Vec::new();
    while let Ok(n) = socket.recv(&mut bytes) {
        let header = cipher.open(&bytes[..n], &mut plain).unwrap();
        if header.session == start.session_id && !header.is_keepalive() {
            largest = largest.max(header.seq);
        }
    }
    handle.start_voice(StartVoice {
        devices: Devices {
            capture: Some("replacement".into()),
            render: None,
        },
        ..start.clone()
    });
    wait(|| handle.snapshot().stage == RuntimeStage::Ready);
    assert_eq!(handle.snapshot().voice.unwrap().packets_sent, 0);
    handle.set_transmitting(true);
    let next = sequence(&socket, &cipher, start.session_id);
    assert!(
        next > largest,
        "nonce sequence reset across device replacement: {next} <= {largest}"
    );
    handle.set_server_muted(true);
    assert!(!handle.snapshot().voice.unwrap().transmitting);
    handle.set_muted(false);
    assert!(handle.snapshot().intent.server_muted);
    runtime.handle().shutdown();
    assert!(runtime.wait_stopped(Duration::from_secs(3)));
    assert!(handle.process_id().is_none());
    speaker.disconnect();
    listener.disconnect();
}

#[test]
fn child_rejects_one_reused_direction_key_in_a_new_session() {
    let invite = server();
    let client = join(&invite, "key-reuse");
    let runtime = VoiceRuntime::with_args(ENGINE, vec!["--synthetic".into()]).unwrap();
    let handle = runtime.handle();
    handle.set_muted(true);
    let old = request(&client);
    handle.start_voice(old.clone());
    wait(|| handle.snapshot().stage == RuntimeStage::Ready);
    let original_pid = handle.process_id();
    // One directional key being new does not permit resetting the other one.
    handle.start_voice(StartVoice {
        session_id: old.session_id.wrapping_add(1),
        downstream_key: [42; 32],
        ..old
    });
    wait(|| handle.needs_reauthentication());
    assert_eq!(handle.snapshot().stage, RuntimeStage::Failed);
    assert!(handle.snapshot().voice.is_none());
    assert_eq!(handle.process_id(), original_pid);
    handle.shutdown();
    assert!(runtime.wait_stopped(Duration::from_secs(3)));
    client.disconnect();
}

#[test]
fn retired_process_rejects_old_keys_and_recovers_with_fresh_authentication() {
    let invite = server();
    let client = join(&invite, "old-session");
    let runtime = VoiceRuntime::with_args(ENGINE, vec!["--synthetic".into()]).unwrap();
    let handle = runtime.handle();
    handle.set_mode(TransmitMode::Always);
    let old = request(&client);
    handle.start_voice(old.clone());
    wait(|| handle.snapshot().stage == RuntimeStage::Ready);
    handle.retire_engine();
    wait(|| handle.process_id().is_none());
    assert!(handle.needs_reauthentication());
    handle.start_voice(old);
    assert_eq!(handle.snapshot().stage, RuntimeStage::Failed);
    assert!(handle.needs_reauthentication());
    assert!(handle.process_id().is_none());
    client.disconnect();
    let fresh = join(&invite, "fresh-session");
    handle.start_voice(request(&fresh));
    wait(|| handle.snapshot().stage == RuntimeStage::Ready);
    assert!(!handle.needs_reauthentication());
    assert!(handle.snapshot().voice.unwrap().udp_ok);
    runtime.handle().shutdown();
    assert!(runtime.wait_stopped(Duration::from_secs(3)));
    fresh.disconnect();
}

struct ChildGuard(Child);
impl Drop for ChildGuard {
    fn drop(&mut self) {
        let _ = self.0.kill();
        let _ = self.0.wait();
    }
}

fn child() -> ChildGuard {
    let mut command = Command::new(ENGINE);
    command
        .arg("--synthetic")
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::null());
    #[cfg(windows)]
    {
        use std::os::windows::process::CommandExt;
        command.creation_flags(0x08000000);
    }
    ChildGuard(command.spawn().unwrap())
}

#[test]
fn parent_pipe_eof_stops_an_active_child() {
    let mut child = child();
    let mut input = child.0.stdin.take().unwrap();
    let mut output = child.0.stdout.take().unwrap();
    ipc::write_frame(
        &mut input,
        &Hello {
            protocol: ipc::VERSION,
            engine_version: String::new(),
        },
    )
    .unwrap();
    assert_eq!(
        ipc::read_frame::<Hello>(&mut output).unwrap().protocol,
        ipc::VERSION
    );
    ipc::write_frame(
        &mut input,
        &Request {
            id: 1,
            revision: 0,
            intent: VoiceIntent::default(),
            volumes: Default::default(),
            command: ipc::Command::StartMic(Devices::default()),
        },
    )
    .unwrap();
    loop {
        if let ipc::Reply::Snapshot { snapshot, .. } = ipc::read_frame(&mut output).unwrap() {
            if snapshot.stage == RuntimeStage::Ready {
                break;
            }
        }
    }
    drop(input);
    wait(|| child.0.try_wait().unwrap().is_some());
}

#[test]
fn incompatible_handshake_exits_without_opening_devices() {
    let mut child = child();
    ipc::write_frame(
        child.0.stdin.as_mut().unwrap(),
        &Hello {
            protocol: ipc::VERSION + 1,
            engine_version: String::new(),
        },
    )
    .unwrap();
    wait(|| child.0.try_wait().unwrap().is_some());
    assert!(!child.0.wait().unwrap().success());
}
