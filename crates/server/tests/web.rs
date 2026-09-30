// SPDX-License-Identifier: MPL-2.0

use std::io::{Read, Write};
use std::net::{TcpListener, TcpStream};
use std::process::{Child, Command, Stdio};
use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};

struct RunningServer {
    child: Child,
    data: std::path::PathBuf,
}

impl Drop for RunningServer {
    fn drop(&mut self) {
        let _ = self.child.kill();
        let _ = self.child.wait();
        let _ = std::fs::remove_dir_all(&self.data);
    }
}

fn request(address: &str, method: &str, path: &str) -> Vec<u8> {
    let mut socket = TcpStream::connect(address).unwrap();
    socket
        .set_read_timeout(Some(Duration::from_secs(3)))
        .unwrap();
    write!(socket, "{method} {path} HTTP/1.1\r\nHost: localhost\r\nConnection: close\r\nContent-Length: 0\r\n\r\n").unwrap();
    let mut response = Vec::new();
    socket.read_to_end(&mut response).unwrap();
    response
}

#[test]
fn join_http_routes_protect_credentials_and_serve_the_browser_assets() {
    let reservation = TcpListener::bind("127.0.0.1:0").unwrap();
    let address = reservation.local_addr().unwrap().to_string();
    drop(reservation);
    let unique = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap()
        .as_nanos();
    let data =
        std::env::temp_dir().join(format!("gouhuo-web-test-{}-{unique}", std::process::id()));
    let child = Command::new(env!("CARGO_BIN_EXE_gouhuo-server"))
        .env("GOUHUO_PORT", "0")
        .env("GOUHUO_DATA", &data)
        .env("GOUHUO_HOST", "voice.example.com")
        .env("GOUHUO_NAME", "<unsafe>&服务器")
        .env("GOUHUO_WEB_LISTEN", &address)
        .env("GOUHUO_JOIN_URL", "https://voice.example.com/")
        .env("GOUHUO_INVITE", "secret-that-must-never-be-public")
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .spawn()
        .unwrap();
    let mut server = RunningServer { child, data };
    let deadline = Instant::now() + Duration::from_secs(5);
    while TcpStream::connect(&address).is_err() {
        assert!(
            server.child.try_wait().unwrap().is_none(),
            "server exited during startup"
        );
        assert!(Instant::now() < deadline, "HTTP listener did not start");
        std::thread::sleep(Duration::from_millis(20));
    }
    let response = String::from_utf8(request(&address, "GET", "/?code=untrusted-query")).unwrap();
    assert!(response.starts_with("HTTP/1.1 200"));
    assert!(response.contains("&lt;unsafe&gt;&amp;服务器"));
    assert!(response
        .to_ascii_lowercase()
        .contains("content-security-policy:"));
    assert!(response
        .to_ascii_lowercase()
        .contains("referrer-policy: no-referrer"));
    assert!(response
        .to_ascii_lowercase()
        .contains("cache-control: no-store"));
    assert!(!response.contains("secret-that-must-never-be-public"));
    assert!(!response.contains("untrusted-query"));
    for path in [
        "/join/",
        "/join.css",
        "/join.mjs",
        "/fire.mjs",
        "/flame.svg",
    ] {
        assert!(
            request(&address, "GET", path).starts_with(b"HTTP/1.1 200"),
            "{path}"
        );
    }
    // 客户端读的那份说明：能解析出地址和指纹，但同样不含加入码。
    let discovery = String::from_utf8(request(&address, "GET", "/.well-known/gouhuo")).unwrap();
    assert!(discovery.starts_with("HTTP/1.1 200"));
    assert!(!discovery.contains("secret-that-must-never-be-public"));
    let body = discovery.split("\r\n\r\n").nth(1).unwrap();
    let parsed = protocol::Discovery::parse(body).unwrap();
    assert_eq!(parsed.invite.host, "voice.example.com");
    assert_eq!(parsed.invite.code, None);
    assert!(parsed.private);
    assert!(request(&address, "GET", "/../invite-code.txt").starts_with(b"HTTP/1.1 404"));
    assert!(request(&address, "POST", "/").starts_with(b"HTTP/1.1 405"));
    let head = request(&address, "HEAD", "/");
    let split = head.windows(4).position(|w| w == b"\r\n\r\n").unwrap();
    assert_eq!(
        head.len(),
        split + 4,
        "HEAD must not send the document body"
    );
}
