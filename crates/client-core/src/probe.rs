// SPDX-License-Identifier: MPL-2.0

//! 第一次见面：取回一台服务器的证书指纹，给用户核对。
//!
//! 用户只输了 IP（或者一个没有加入页的域名）时，没有任何人替这台服务器担保。
//! 能做的只有 SSH 第一次连一台机器时做的那件事：把指纹取回来摆在用户面前，
//! 让他跟服主那边的对一下（服务端启动日志里有「证书指纹」那一行）。
//! 他点了头，才用这个指纹去固定证书、正式连接。
//!
//! # 这条连接上什么都不发
//!
//! 握手完读出指纹就关掉。**身份、昵称、加入码一个字节都不往上写** ——
//! 这时候还不知道对面是谁。它们要等 [`crate::Client`] 用固定了指纹的连接去发。
//!
//! 取回来的指纹和之后正式连接用的是两条 TCP 连接，中间人可以在两次之间换证书 ——
//! 但那样第二次就对不上用户确认过的指纹，连接会被拒绝。

use std::sync::Arc;

use protocol::Fingerprint;
use rustls::pki_types::ServerName;

use crate::client::{connect_tcp, CONNECT_TIMEOUT};
use crate::error::ConnectError;

/// 连上去握个手，返回对面证书的指纹。
pub fn probe(host: &str, port: u16) -> Result<Fingerprint, ConnectError> {
    let mut sock = connect_tcp(host, port)?;
    sock.set_nodelay(true)?;
    // 对面接了 TCP 却一声不吭的话（不是篝火的服务），别永远等下去。
    sock.set_read_timeout(Some(CONNECT_TIMEOUT))?;

    let config = Arc::new(transport::probe_config().map_err(|e| ConnectError::Tls(e.to_string()))?);
    let name = ServerName::try_from("gouhuo").expect("常量，不会失败");
    let mut conn = rustls::ClientConnection::new(config, name)
        .map_err(|e| ConnectError::Tls(e.to_string()))?;
    conn.complete_io(&mut sock)
        .map_err(|e| ConnectError::Tls(e.to_string()))?;

    let fingerprint = conn
        .peer_certificates()
        .and_then(|certs| certs.first())
        .map(|cert| Fingerprint::of(cert.as_ref()))
        .ok_or(ConnectError::NotAGouhuoServer)?;

    // 礼貌地告别，让服务端那边立刻收场，而不是等它的握手超时。
    conn.send_close_notify();
    let _ = conn.write_tls(&mut sock);
    let _ = sock.shutdown(std::net::Shutdown::Both);
    Ok(fingerprint)
}
