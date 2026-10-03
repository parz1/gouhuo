// SPDX-License-Identifier: GPL-3.0-or-later
use std::net::{TcpListener, UdpSocket};
use std::sync::{atomic::AtomicBool, mpsc, Arc};
use std::time::{Duration, Instant};

use protocol::Invite;
use serde_json::Value;
use server::{conn::Hub, state::Server};
use test_bot::{network::Direction, run_bot, Config};
use transport::{server_config, ServerCert};

fn server() -> (String, Arc<Hub>) {
    let cert = ServerCert::generate().unwrap();
    let tls = Arc::new(server_config(&cert).unwrap());
    let listener = TcpListener::bind("127.0.0.1:0").unwrap();
    let addr = listener.local_addr().unwrap();
    let hub = Arc::new(Hub::new(
        Server::new(Default::default()),
        UdpSocket::bind("127.0.0.1:0").unwrap(),
    ));
    let voice = Arc::clone(&hub);
    std::thread::spawn(move || voice.run_voice());
    let accept = Arc::clone(&hub);
    std::thread::spawn(move || server::accept_loop(listener, tls, accept));
    let invite = Invite {
        host: addr.ip().to_string(),
        port: addr.port(),
        cert: cert.fingerprint(),
        code: None,
    };
    (invite.to_code().unwrap(), hub)
}

fn run(configs: Vec<Config>, hub: &Hub) -> Vec<Value> {
    let (tx, rx) = mpsc::sync_channel(1024);
    let stop = Arc::new(AtomicBool::new(false));
    let workers: Vec<_> = configs
        .into_iter()
        .enumerate()
        .map(|(index, cfg)| {
            let tx = tx.clone();
            let stop = Arc::clone(&stop);
            std::thread::spawn(move || run_bot(cfg, index, tx, stop))
        })
        .collect();
    drop(tx);
    let reports: Vec<_> = rx.iter().collect();
    for worker in workers {
        worker.join().unwrap().unwrap();
    }
    let deadline = Instant::now() + Duration::from_secs(3);
    while hub.user_count() != 0 && Instant::now() < deadline {
        std::thread::sleep(Duration::from_millis(10));
    }
    assert_eq!(hub.user_count(), 0, "bots must disconnect on completion");
    reports
}

fn config(invite: &str, seconds: u64) -> Config {
    let mut cfg = Config::new(invite.into());
    cfg.room = Some("大厅".into());
    cfg.seconds = Duration::from_secs(seconds);
    cfg
}

#[test]
fn multiple_identities_exchange_real_encrypted_audio() {
    let (invite, hub) = server();
    let reports = run(vec![config(&invite, 3); 3], &hub);
    let sessions: std::collections::HashSet<_> = reports
        .iter()
        .filter(|r| r["event"] == "connected")
        .map(|r| r["session"].as_u64().unwrap())
        .collect();
    assert_eq!(sessions.len(), 3);
    let finished: Vec<_> = reports
        .iter()
        .filter(|r| r["event"] == "finished")
        .collect();
    assert_eq!(finished.len(), 3);
    for r in finished {
        assert_eq!(r["udp_ok"], true, "{r}");
        assert!(r["received"].as_u64().unwrap() > 20, "{r}");
        assert!(r["audible_frames"].as_u64().unwrap() > 20, "{r}");
        assert_eq!(r["generation"], 1);
    }
}

#[test]
fn echo_returns_audio_to_the_original_sender() {
    let (invite, hub) = server();
    let mut echo = config(&invite, 3);
    echo.echo = true;
    let reports = run(vec![config(&invite, 3), echo], &hub);
    for r in reports.iter().filter(|r| r["event"] == "finished") {
        assert!(r["received"].as_u64().unwrap() > 10, "{r}");
        assert!(r["audible_frames"].as_u64().unwrap() > 10, "{r}");
        assert_eq!(r["echo_overflow"], 0, "{r}");
    }
}

#[test]
fn upstream_outage_keeps_rendering_then_recovers_without_audio_retry_churn() {
    let (invite, hub) = server();
    let mut receiver = config(&invite, 25);
    receiver.silent = true;
    receiver.impairment.outage_at = Duration::from_secs(2);
    receiver.impairment.outage_for = Duration::from_secs(18);
    receiver.impairment.direction = Direction::Up;
    let reports = run(vec![config(&invite, 25), receiver], &hub);
    let reports: Vec<_> = reports.iter().filter(|r| r["bot"] == 2).collect();
    assert!(reports.iter().any(|r| r["action"] == "Lost"), "{reports:?}");
    assert!(
        !reports.iter().any(|r| r["action"] == "RetryVoice"),
        "{reports:?}"
    );
    assert!(
        reports.iter().any(|r| r["event"] == "tcp_reconnected"),
        "{reports:?}"
    );
    assert!(
        reports.iter().any(|r| r["action"] == "Recovered"),
        "{reports:?}"
    );
    let during: Vec<_> = reports
        .iter()
        .filter(|r| {
            r["event"] == "sample" && (5000..12000).contains(&r["elapsed_ms"].as_u64().unwrap())
        })
        .collect();
    assert!(during.len() >= 3);
    assert!(
        during.last().unwrap()["audible_frames"].as_u64().unwrap()
            > during.first().unwrap()["audible_frames"].as_u64().unwrap() + 100
    );
    let final_report = reports.iter().find(|r| r["event"] == "finished").unwrap();
    assert_eq!(final_report["udp_ok"], true, "{final_report}");
    assert_eq!(final_report["audio_opens"], 2, "{final_report}");
}

#[test]
fn unknown_room_still_disconnects() {
    let (invite, hub) = server();
    let mut cfg = config(&invite, 1);
    cfg.room = Some("does-not-exist".into());
    let (tx, _rx) = mpsc::sync_channel(32);
    assert!(run_bot(cfg, 0, tx, Arc::new(AtomicBool::new(false))).is_err());
    let deadline = Instant::now() + Duration::from_secs(3);
    while hub.user_count() != 0 && Instant::now() < deadline {
        std::thread::sleep(Duration::from_millis(10));
    }
    assert_eq!(hub.user_count(), 0);
}

#[test]
fn cli_loops_wav_and_writes_private_jsonl_without_overwriting() {
    let (invite, hub) = server();
    let dir = std::env::temp_dir().join(format!(
        "gouhuo-bot-test-{}-{}",
        std::process::id(),
        std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .unwrap()
            .as_nanos()
    ));
    std::fs::create_dir(&dir).unwrap();
    let invite_file = dir.join("invite.txt");
    let wav_file = dir.join("voice.wav");
    let log = dir.join("bots.jsonl");
    std::fs::write(&invite_file, &invite).unwrap();
    // Real stereo PCM16 WAV at 8 kHz exercises mixing and 48 kHz resampling.
    let pcm: Vec<u8> = (0..8000)
        .flat_map(|i| {
            let sample =
                (((i as f64 * 440.0 * std::f64::consts::TAU / 8000.0).sin()) * 6000.0) as i16;
            [sample.to_le_bytes(), sample.to_le_bytes()].concat()
        })
        .collect();
    let mut wav = b"RIFF".to_vec();
    wav.extend_from_slice(&(36 + pcm.len() as u32).to_le_bytes());
    wav.extend_from_slice(b"WAVEfmt ");
    wav.extend_from_slice(&16u32.to_le_bytes());
    for n in [1u16, 2] {
        wav.extend_from_slice(&n.to_le_bytes());
    }
    for n in [8000u32, 32000] {
        wav.extend_from_slice(&n.to_le_bytes());
    }
    for n in [4u16, 16] {
        wav.extend_from_slice(&n.to_le_bytes());
    }
    wav.extend_from_slice(b"data");
    wav.extend_from_slice(&(pcm.len() as u32).to_le_bytes());
    wav.extend_from_slice(&pcm);
    std::fs::write(&wav_file, wav).unwrap();
    let decoded = test_bot::audio::load_wav(&wav_file).unwrap();
    assert_eq!(decoded.len(), 48000);
    let command = || {
        let mut cmd = std::process::Command::new(env!("CARGO_BIN_EXE_gouhuo-bot"));
        cmd.arg("--invite-file")
            .arg(&invite_file)
            .args(["--count", "2", "--seconds", "3", "--room", "大厅", "--play"])
            .arg(&wav_file)
            .arg("--log")
            .arg(&log);
        cmd
    };
    let output = command().output().unwrap();
    assert!(
        output.status.success(),
        "{}",
        String::from_utf8_lossy(&output.stderr)
    );
    assert!(output.stdout.is_empty());
    let text = std::fs::read_to_string(&log).unwrap();
    assert!(!text.contains(&invite));
    let rows: Vec<Value> = text
        .lines()
        .map(|s| serde_json::from_str(s).unwrap())
        .collect();
    let finished: Vec<_> = rows.iter().filter(|r| r["event"] == "finished").collect();
    assert_eq!(finished.len(), 2);
    for r in finished {
        assert!(r["audible_frames"].as_u64().unwrap() > 20, "{r}");
    }
    assert!(!command().output().unwrap().status.success());
    assert_eq!(std::fs::read_to_string(&log).unwrap(), text);
    let listener_log = dir.join("listeners.jsonl");
    let output = std::process::Command::new(env!("CARGO_BIN_EXE_gouhuo-bot"))
        .arg("--invite-file")
        .arg(&invite_file)
        .args(["--count", "2", "--speakers", "1", "--seconds", "3"])
        .arg("--log")
        .arg(&listener_log)
        .output()
        .unwrap();
    assert!(
        output.status.success(),
        "{}",
        String::from_utf8_lossy(&output.stderr)
    );
    let rows: Vec<Value> = std::fs::read_to_string(&listener_log)
        .unwrap()
        .lines()
        .map(|s| serde_json::from_str(s).unwrap())
        .collect();
    let receiver = rows
        .iter()
        .find(|r| r["event"] == "finished" && r["bot"] == 2)
        .unwrap();
    assert_eq!(receiver["sent"], 0);
    assert!(
        receiver["audible_frames"].as_u64().unwrap() > 20,
        "{receiver}"
    );
    let deadline = Instant::now() + Duration::from_secs(3);
    while hub.user_count() != 0 && Instant::now() < deadline {
        std::thread::sleep(Duration::from_millis(10));
    }
    assert_eq!(hub.user_count(), 0);
    std::fs::write(&wav_file, b"not a WAV").unwrap();
    assert!(test_bot::audio::load_wav(&wav_file).is_err());
    for path in [&invite_file, &wav_file, &log, &listener_log] {
        std::fs::remove_file(path).unwrap();
    }
    std::fs::remove_dir(dir).unwrap();
}
