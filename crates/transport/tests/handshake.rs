// SPDX-License-Identifier: MPL-2.0

//! 端到端：起一个真的 TLS 服务端和客户端，走完整握手。
//!
//! 单元测试能验「指纹比对的逻辑对不对」，但验不了「这套配置接起来真的能用」。
//! 这个文件里跑的是真 TCP、真 TLS 握手、真密钥派生 —— 信任链上任何一环接错了，
//! 这里会红。

use std::io::{Read, Write};
use std::net::{TcpListener, TcpStream};
use std::sync::mpsc;
use std::sync::Arc;

use protocol::control::{decode_frame, encode_frame, ClientMessage, Hello, PROTOCOL_VERSION};
use protocol::Fingerprint;
use rustls::pki_types::ServerName;
use rustls::{ClientConnection, ServerConnection, StreamOwned};
use transport::{client_config, derive_voice_key, server_config, ServerCert};

/// 服务端那半边：接一条连接，握手，派生密钥，读一条控制消息，回一条。
///
/// 出错走 channel 传回主线程，不在子线程里 panic —— 子线程的 panic 消息会被
/// 测试框架吞掉，只留下一句"recv failed"，查起来全靠猜。
fn serve(listener: TcpListener, cert: ServerCert) -> mpsc::Receiver<Result<ServerOutcome, String>> {
    let (tx, rx) = mpsc::channel();
    std::thread::spawn(move || {
        let outcome = (|| -> Result<ServerOutcome, String> {
            let config = Arc::new(server_config(&cert).map_err(|e| e.to_string())?);
            let (sock, _) = listener.accept().map_err(|e| e.to_string())?;
            let mut conn = ServerConnection::new(config).map_err(|e| e.to_string())?;
            let mut sock = sock;
            conn.complete_io(&mut sock)
                .map_err(|e| format!("握手失败: {e}"))?;

            let key = derive_voice_key(&conn, b"").map_err(|e| e.to_string())?;
            let voice_key = *key.as_bytes();

            let mut stream = StreamOwned::new(conn, sock);
            let mut buf = vec![0u8; 4096];
            let n = stream.read(&mut buf).map_err(|e| e.to_string())?;
            let (msg, _) = decode_frame::<ClientMessage>(&buf[..n])
                .map_err(|e| e.to_string())?
                .ok_or("消息没收全")?;

            // 原样回一条，证明双向都通
            let mut out = Vec::new();
            encode_frame(&msg, &mut out).map_err(|e| e.to_string())?;
            stream.write_all(&out).map_err(|e| e.to_string())?;
            stream.flush().map_err(|e| e.to_string())?;

            Ok(ServerOutcome {
                voice_key,
                received: msg,
            })
        })();
        let _ = tx.send(outcome);
    });
    rx
}

struct ServerOutcome {
    voice_key: [u8; 32],
    received: ClientMessage,
}

fn client_hello() -> ClientMessage {
    Hello {
        protocol_version: PROTOCOL_VERSION,
        client_version: "gouhuo test".into(),
        public_key: vec![9u8; 32],
        probe_only: false,
    }
    .into()
}

/// 整条链：指纹对得上 -> 握手成功 -> 两边派生出同一把 UDP 密钥 -> 控制消息能来回。
#[test]
fn pinned_handshake_succeeds_and_both_sides_agree_on_the_voice_key() {
    let cert = ServerCert::generate().unwrap();
    let fingerprint = cert.fingerprint();
    let listener = TcpListener::bind("127.0.0.1:0").unwrap();
    let addr = listener.local_addr().unwrap();
    let server = serve(listener, cert);

    let config = Arc::new(client_config(fingerprint).unwrap());
    let mut conn = ClientConnection::new(config, ServerName::try_from("gouhuo").unwrap()).unwrap();
    let mut sock = TcpStream::connect(addr).unwrap();
    conn.complete_io(&mut sock)
        .expect("指纹对得上，握手不该失败");

    let client_key = *derive_voice_key(&conn, b"").unwrap().as_bytes();

    let mut stream = StreamOwned::new(conn, sock);
    let hello = client_hello();
    let mut out = Vec::new();
    encode_frame(&hello, &mut out).unwrap();
    stream.write_all(&out).unwrap();
    stream.flush().unwrap();

    let mut buf = vec![0u8; 4096];
    let n = stream.read(&mut buf).unwrap();
    let (echoed, _) = decode_frame::<ClientMessage>(&buf[..n]).unwrap().unwrap();
    assert_eq!(echoed, hello, "回来的控制消息跟发出去的不一样");

    let outcome = server.recv().unwrap().expect("服务端出错");
    assert_eq!(outcome.received, hello, "服务端收到的跟客户端发的不一样");

    // 这一条是整个设计成立的关键：不另起握手，两边照样有同一把密钥。
    assert_eq!(
        client_key, outcome.voice_key,
        "两边派生出的 UDP 密钥不一样 —— 语音就是解不开的"
    );
    assert_ne!(client_key, [0u8; 32], "密钥不能是全零");
}

/// 指纹不对必须握手失败 —— 这是中间人攻击最直接的形态。
#[test]
fn wrong_fingerprint_aborts_the_handshake() {
    let cert = ServerCert::generate().unwrap();
    let listener = TcpListener::bind("127.0.0.1:0").unwrap();
    let addr = listener.local_addr().unwrap();

    // 服务端照常跑；这里不关心它怎么结束
    std::thread::spawn(move || {
        let config = Arc::new(server_config(&cert).unwrap());
        if let Ok((mut sock, _)) = listener.accept() {
            if let Ok(mut conn) = ServerConnection::new(config) {
                let _ = conn.complete_io(&mut sock);
            }
        }
    });

    // 客户端拿着另一张证书的指纹
    let wrong = ServerCert::generate().unwrap().fingerprint();
    let config = Arc::new(client_config(wrong).unwrap());
    let mut conn = ClientConnection::new(config, ServerName::try_from("gouhuo").unwrap()).unwrap();
    let mut sock = TcpStream::connect(addr).unwrap();

    let err = conn
        .complete_io(&mut sock)
        .expect_err("指纹不对却握手成功了");
    let text = err.to_string();
    assert!(
        text.contains("指纹对不上"),
        "报错要说清楚原因，实际是：{text}"
    );
}

/// 不同的 TLS 连接必须派生出不同的密钥。
///
/// 如果两条连接算出同一把，说明派生根本没用上握手的密钥材料 ——
/// 那等于所有人共用一把密钥，语音加密形同虚设。
#[test]
fn different_connections_derive_different_keys() {
    fn one_key(cert: &ServerCert) -> [u8; 32] {
        let listener = TcpListener::bind("127.0.0.1:0").unwrap();
        let addr = listener.local_addr().unwrap();
        let config = Arc::new(server_config(cert).unwrap());
        let handle = std::thread::spawn(move || {
            let (mut sock, _) = listener.accept().unwrap();
            let mut conn = ServerConnection::new(config).unwrap();
            conn.complete_io(&mut sock).unwrap();
            *derive_voice_key(&conn, b"").unwrap().as_bytes()
        });

        let client = Arc::new(client_config(cert.fingerprint()).unwrap());
        let mut conn =
            ClientConnection::new(client, ServerName::try_from("gouhuo").unwrap()).unwrap();
        let mut sock = TcpStream::connect(addr).unwrap();
        conn.complete_io(&mut sock).unwrap();
        let client_key = *derive_voice_key(&conn, b"").unwrap().as_bytes();

        let server_key = handle.join().unwrap();
        assert_eq!(client_key, server_key);
        client_key
    }

    // 同一张服务端证书，两条连接
    let cert = ServerCert::generate().unwrap();
    assert_ne!(one_key(&cert), one_key(&cert), "两条连接派生出了同一把密钥");
}

/// 邀请链接 -> 指纹 -> 握手，整条走一遍。
///
/// 这是用户实际经历的路径：粘一串东西进来，然后就连上了。
#[test]
fn invite_link_drives_the_whole_connection() {
    let cert = ServerCert::generate().unwrap();
    let listener = TcpListener::bind("127.0.0.1:0").unwrap();
    let addr = listener.local_addr().unwrap();

    // 服务端把自己的地址和指纹编成一条邀请链接
    let invite = protocol::Invite {
        host: addr.ip().to_string(),
        port: addr.port(),
        cert: cert.fingerprint(),
        code: Some("winter2026".into()),
    };
    let link = invite.to_url().unwrap();

    let server = serve(listener, cert);

    // 客户端只拿到这条链接，别的什么都不知道
    let parsed = protocol::Invite::parse(&link).unwrap();
    assert_eq!(parsed.code.as_deref(), Some("winter2026"));

    let config = Arc::new(client_config(parsed.cert).unwrap());
    let mut conn = ClientConnection::new(config, ServerName::try_from("gouhuo").unwrap()).unwrap();
    let mut sock =
        TcpStream::connect((parsed.host.as_str(), parsed.port)).expect("按链接里的地址连不上");
    conn.complete_io(&mut sock).expect("按链接里的指纹握手失败");

    let mut stream = StreamOwned::new(conn, sock);
    let hello = client_hello();
    let mut out = Vec::new();
    encode_frame(&hello, &mut out).unwrap();
    stream.write_all(&out).unwrap();
    stream.flush().unwrap();

    let outcome = server.recv().unwrap().expect("服务端出错");
    assert_eq!(outcome.received, hello);
}

/// 被篡改过的邀请链接连指纹都取不出来，根本走不到握手。
#[test]
fn tampered_invite_never_reaches_the_handshake() {
    let invite = protocol::Invite {
        host: "127.0.0.1".into(),
        port: 64738,
        cert: Fingerprint([0x11; 16]),
        code: None,
    };
    let code = invite.to_code().unwrap();
    let mut chars: Vec<char> = code.chars().collect();
    let mid = chars.len() / 2;
    chars[mid] = if chars[mid] == 'a' { 'b' } else { 'a' };
    let tampered: String = chars.into_iter().collect();

    assert!(
        protocol::Invite::parse(&tampered).is_err(),
        "改过的邀请链接必须在解析这一步就被拦下"
    );
}
