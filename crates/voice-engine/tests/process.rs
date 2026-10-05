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

/// Invoked by scripts/voice-update-rehearsal.ps1 with separately built engines.
/// Transport fixtures feed the production discovery/staging code; audio is synthetic.
#[cfg(windows)]
#[test]
#[ignore = "requires independently built 0.1.1 engine and an isolated rehearsal directory"]
fn independent_update_rehearsal() {
    use client_process::update::{
        download::{check_allowed, check_and_stage, Fetch, REPO},
        hex, Compatibility, EngineStore, Manifest,
    };
    use ed25519_dalek::{Signer, SigningKey};
    use sha2::{Digest, Sha256};
    use std::{collections::BTreeMap, fs, io, path::PathBuf};

    struct ReleaseFiles(BTreeMap<String, Vec<u8>>);
    impl Fetch for ReleaseFiles {
        fn get(
            &mut self,
            url: &str,
            maximum: usize,
            allowed: &dyn Fn() -> bool,
        ) -> io::Result<Vec<u8>> {
            check_allowed(allowed)?;
            let bytes = self
                .0
                .get(url)
                .ok_or_else(|| io::Error::other("missing rehearsal asset"))?;
            if bytes.len() > maximum {
                return Err(io::Error::other("rehearsal asset exceeds limit"));
            }
            Ok(bytes.clone())
        }
    }
    fn files(version: &str, binary: &[u8], key: &SigningKey) -> ReleaseFiles {
        let manifest = serde_json::to_vec(&Manifest {
            schema: 1,
            version: version.into(),
            target: "windows-x64".into(),
            ipc: ipc::VERSION,
            server_protocol: protocol::control::PROTOCOL_VERSION,
            min_ui: "0.3.1".into(),
            max_ui: None,
            size: binary.len() as u64,
            sha256: hex(&Sha256::digest(binary)),
        })
        .unwrap();
        let signature = hex(&key.sign(&manifest).to_bytes()).into_bytes();
        let name = format!("gouhuo-voice-{version}-windows-x64.exe");
        let base = format!("https://github.com/{REPO}/releases/download/voice-v{version}");
        ReleaseFiles(BTreeMap::from([
            (format!("https://api.github.com/repos/{REPO}/releases?per_page=100"),
             serde_json::to_vec(&serde_json::json!([{
                 "tag_name": format!("voice-v{version}"), "draft":false, "prerelease":false,
                 "assets":[{"name":"manifest.json","size":manifest.len()},
                     {"name":"manifest.sig","size":signature.len()}, {"name":name,"size":binary.len()}]
             }])).unwrap()),
            (format!("{base}/manifest.json"), manifest),
            (format!("{base}/manifest.sig"), signature),
            (format!("{base}/{name}"), binary.to_vec()),
        ]))
    }
    let root = PathBuf::from(std::env::var_os("GOUHUO_REHEARSAL_ROOT").expect("rehearsal root"));
    let ui = PathBuf::from(std::env::var_os("GOUHUO_REHEARSAL_UI").expect("unchanged UI"));
    let updated =
        fs::read(std::env::var_os("GOUHUO_REHEARSAL_ENGINE").expect("0.1.1 engine")).unwrap();
    let ui_before = hex(&Sha256::digest(fs::read(&ui).unwrap()));
    let parent_before = hex(&Sha256::digest(
        fs::read(std::env::current_exe().unwrap()).unwrap(),
    ));
    let key = SigningKey::from_bytes(&[5; 32]); // Public fixture seed, never a release secret.
    let store = Arc::new(
        EngineStore::new(
            root.join("store"),
            key.verifying_key().to_bytes(),
            Compatibility {
                target: "windows-x64".into(),
                ipc: ipc::VERSION,
                server_protocol: protocol::control::PROTOCOL_VERSION,
                ui: "0.3.1".into(),
                bundled: "0.1.0".into(),
            },
        )
        .unwrap(),
    );
    let runtime =
        VoiceRuntime::managed_with_args(ENGINE, vec!["--synthetic".into()], Arc::clone(&store))
            .unwrap();
    let handle = runtime.handle();
    wait(|| handle.engine_version() == "0.1.0");
    let original_pid = handle.process_id().unwrap();
    let invite = server();
    let caller = join(&invite, "update-rehearsal");
    handle.set_mode(TransmitMode::Always);
    handle.start_voice(request(&caller));
    wait(|| {
        handle
            .snapshot()
            .voice
            .is_some_and(|voice| voice.packets_sent >= 3)
    });
    assert_eq!(
        check_and_stage(&mut files("0.1.1", &updated, &key), &store, &|| true)
            .unwrap()
            .as_deref(),
        Some("0.1.1")
    );
    assert!(!handle.activate_pending(), "retired an active call");
    assert_eq!(handle.process_id(), Some(original_pid));
    handle.stop();
    wait(|| handle.snapshot().stage == RuntimeStage::Idle);
    caller.disconnect();
    handle.start_mic_check(Devices::default());
    wait(|| handle.snapshot().mic.is_some());
    assert!(!handle.activate_pending(), "retired an active mic check");
    assert_eq!(handle.process_id(), Some(original_pid));
    handle.stop();
    wait(|| handle.snapshot().stage == RuntimeStage::Idle);
    assert!(handle.activate_pending());
    wait(|| handle.engine_version() == "0.1.1" && handle.snapshot().stage == RuntimeStage::Idle);
    let upgraded_pid = handle.process_id().unwrap();
    assert_ne!(upgraded_pid, original_pid);
    wait(|| {
        fs::read_to_string(root.join("store/state.json")).is_ok_and(|raw| {
            let state: serde_json::Value = serde_json::from_str(&raw).unwrap();
            state["active"] == "0.1.1" && state["booting"].is_null()
        })
    });
    // Both spawn failure and a signed host lying about its version must roll back.
    for (version, binary) in [
        ("0.1.2", b"not a Windows executable".as_slice()),
        ("0.1.3", updated.as_slice()),
    ] {
        assert_eq!(
            check_and_stage(&mut files(version, binary, &key), &store, &|| true)
                .unwrap()
                .as_deref(),
            Some(version)
        );
        let previous_pid = handle.process_id();
        assert!(handle.activate_pending());
        wait(|| {
            handle.process_id().is_some()
                && handle.process_id() != previous_pid
                && handle.engine_version() == "0.1.1"
                && handle.snapshot().stage == RuntimeStage::Idle
        });
        wait(|| {
            fs::read_to_string(root.join("store/state.json")).is_ok_and(|raw| {
                let state: serde_json::Value = serde_json::from_str(&raw).unwrap();
                state["active"] == "0.1.1"
                    && state["booting"].is_null()
                    && state["highest"] == version
            })
        });
    }
    let fresh = join(&invite, "updated-core-call");
    handle.start_voice(request(&fresh));
    wait(|| {
        handle
            .snapshot()
            .voice
            .is_some_and(|voice| voice.packets_sent >= 3)
    });
    handle.stop();
    wait(|| handle.snapshot().stage == RuntimeStage::Idle);
    fresh.disconnect();
    handle.shutdown();
    assert!(runtime.wait_stopped(Duration::from_secs(3)));
    // A new supervisor still selects the verified cached version.
    let restarted =
        VoiceRuntime::managed_with_args(ENGINE, vec!["--synthetic".into()], Arc::clone(&store))
            .unwrap();
    wait(|| restarted.handle().engine_version() == "0.1.1");
    restarted.handle().start_mic_check(Devices::default());
    wait(|| restarted.handle().snapshot().mic.is_some());
    restarted.handle().shutdown();
    assert!(restarted.wait_stopped(Duration::from_secs(3)));
    assert_eq!(hex(&Sha256::digest(fs::read(&ui).unwrap())), ui_before);
    assert_eq!(
        hex(&Sha256::digest(
            fs::read(std::env::current_exe().unwrap()).unwrap()
        )),
        parent_before
    );
    fs::write(
        root.join("report.json"),
        serde_json::to_vec_pretty(&serde_json::json!({
            "bundled":"0.1.0", "updated":"0.1.1", "rollback":"0.1.1",
            "original_pid":original_pid, "upgraded_pid":upgraded_pid,
            "ui_sha256":ui_before, "parent_sha256":parent_before,
            "call_deferred":true, "mic_deferred":true, "restart_verified":true,
            "spawn_failure_rolled_back":true, "version_mismatch_rolled_back":true,
            "transport":"local signed release fixture", "audio":"synthetic"
        }))
        .unwrap(),
    )
    .unwrap();
}

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
    stage_fixture_bytes(store, version, &std::fs::read(ENGINE).unwrap());
}

fn stage_fixture_bytes(store: &client_process::update::EngineStore, version: &str, bytes: &[u8]) {
    use client_process::update::{hex, Manifest};
    use ed25519_dalek::{Signer, SigningKey};
    use sha2::{Digest, Sha256};
    let key = SigningKey::from_bytes(&[5; 32]);
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
        sha256: hex(&Sha256::digest(bytes)),
    };
    let manifest = serde_json::to_vec(&manifest).unwrap();
    store
        .stage_bytes(&manifest, &key.sign(&manifest).to_bytes(), bytes)
        .unwrap();
}

#[test]
fn idle_update_spawn_failure_automatically_restarts_bundled_engine() {
    let (root, store) = signed_store("0.1.1", false);
    let runtime =
        VoiceRuntime::managed_with_args(ENGINE, vec!["--synthetic".into()], Arc::clone(&store))
            .unwrap();
    let handle = runtime.handle();
    wait(|| handle.engine_version() == "0.1.0");
    handle.start_mic_check(Devices::default());
    wait(|| handle.snapshot().mic.is_some());
    handle.stop();
    wait(|| handle.snapshot().stage == RuntimeStage::Idle);
    stage_fixture_bytes(&store, "0.1.1", b"not an executable");
    let old_pid = handle.process_id();
    assert!(handle.activate_pending());
    wait(|| {
        handle.process_id().is_some()
            && handle.process_id() != old_pid
            && handle.engine_version() == "0.1.0"
            && handle.snapshot().stage == RuntimeStage::Idle
    });
    assert!(!store.has_pending().unwrap());
    handle.start_mic_check(Devices::default());
    wait(|| handle.snapshot().mic.is_some());
    handle.shutdown();
    assert!(runtime.wait_stopped(Duration::from_secs(3)));
    std::fs::remove_dir_all(root).unwrap();
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
