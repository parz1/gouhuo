// SPDX-License-Identifier: MPL-2.0

//! Loopback-only real TLS/UDP server for native UI fault validation.
//! Run with <evidence-directory> [port]. Pair with scripts/udp_fault_relay.py.
//! UDP binds 127.0.0.2 at the advertised port; the relay binds 127.0.0.1.
//! This preserves the real Welcome message and all production authentication,
//! encryption, routing, and control handling. No production fault switches.
use protocol::Invite;
use server::{
    conn::Hub,
    state::{Config, Server},
};
use std::{
    io,
    net::{TcpListener, UdpSocket},
    path::PathBuf,
    sync::Arc,
    time::Duration,
};
use transport::{server_config, ServerCert};

fn main() -> Result<(), Box<dyn std::error::Error>> {
    let mut args = std::env::args().skip(1);
    let directory = PathBuf::from(args.next().ok_or("expected evidence directory")?);
    let port: u16 = args.next().unwrap_or_else(|| "20898".into()).parse()?;
    if port == 0 || args.next().is_some() {
        return Err("expected a nonzero port and no extra arguments".into());
    }
    std::fs::create_dir_all(&directory)?;
    let (cert, _) = ServerCert::load_or_create(&directory)?;
    let tls = Arc::new(server_config(&cert)?);
    let listener = TcpListener::bind(("127.0.0.1", port))?;
    let socket = UdpSocket::bind(("127.0.0.2", port))?;
    let config = Config {
        require_invite: true,
        invite_code: Some("local-ui-validation".into()),
        ..Config::default()
    };
    let hub = Arc::new(Hub::new(Server::new(config), socket));
    let invite = Invite {
        host: "127.0.0.1".into(),
        port,
        cert: cert.fingerprint(),
        code: Some("local-ui-validation".into()),
    };
    std::fs::write(directory.join("invite.txt"), invite.to_url()?)?;
    let voice = Arc::clone(&hub);
    std::thread::Builder::new()
        .name("validation-voice".into())
        .spawn(move || voice.run_voice())?;
    server::spawn_watchdog(Arc::clone(&hub), Duration::from_secs(3))?;
    eprintln!("Loopback TLS ready; UDP backend 127.0.0.2:{port}; invite saved locally. Start relay, GUI first, then exactly one talker.");
    server::accept_loop(listener, tls, hub);
    Err(io::Error::other("accept loop ended").into())
}
