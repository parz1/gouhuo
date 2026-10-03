// SPDX-License-Identifier: MPL-2.0

//! 控制面的读写两半。
//!
//! 跟 `server::conn` 里那套是同一个形状，原因也一样：rustls 的连接状态只有
//! 一份，读和写都要 `&mut`，所以
//!
//! - **读**：通过 `Arc<TcpStream>` 共享同一 OS 句柄，阻塞读原始字节，**不持锁**
//! - **写**：`Mutex<Wire>` 里装着 TLS 状态和写用的 socket 句柄
//!
//! 没有把它抽到 `transport` 里共用，是因为两边真正共享的只有这三十行机械代码：
//! 服务端那边还要管握手超时，客户端这边将来要管重连。硬凑一个泛型，
//! 换来的是两边都得迁就的参数。

use std::io::{self, Read, Write};
use std::net::TcpStream;
use std::sync::{Arc, Mutex};

use prost::Message;
use protocol::control::{decode_frame, encode_frame, MAX_FRAME_BODY};

pub struct Wire {
    pub conn: rustls::ClientConnection,
    pub sock: Arc<TcpStream>,
}

impl Wire {
    fn flush_tls(&mut self) -> io::Result<()> {
        while self.conn.wants_write() {
            self.conn.write_tls(&mut self.sock.as_ref())?;
        }
        self.sock.as_ref().flush()
    }

    pub fn send<M: Message>(&mut self, message: &M) -> io::Result<()> {
        let mut frame = Vec::new();
        encode_frame(message, &mut frame).map_err(io::Error::other)?;
        self.conn.writer().write_all(&frame)?;
        self.flush_tls()
    }
}

/// 从 TLS 流里一条一条读消息。
pub struct Reader {
    sock: Arc<TcpStream>,
    wire: Arc<Mutex<Wire>>,
    buffered: Vec<u8>,
}

impl Reader {
    pub fn new(sock: Arc<TcpStream>, wire: Arc<Mutex<Wire>>) -> Self {
        Self {
            sock,
            wire,
            buffered: Vec::with_capacity(4096),
        }
    }

    /// 撤掉认证期的读超时，所有读写与取消共享同一 OS 句柄。
    pub fn clear_read_timeout(&self) -> io::Result<()> {
        self.sock.set_read_timeout(None)
    }

    /// 取下一条消息。返回 `Ok(None)` 表示对面关了。
    pub fn next<M: Message + Default>(&mut self) -> io::Result<Option<M>> {
        loop {
            match decode_frame::<M>(&self.buffered) {
                Ok(Some((message, used))) => {
                    self.buffered.drain(..used);
                    return Ok(Some(message));
                }
                Ok(None) => {}
                Err(e) => return Err(io::Error::other(e)),
            }
            if !self.fill()? {
                return Ok(None);
            }
        }
    }

    fn fill(&mut self) -> io::Result<bool> {
        // 握手那一步可能已经顺手把服务端紧跟着发来的数据读进 TLS 层了，
        // 先取干净再去碰 socket。
        if self.drain_plaintext()? > 0 {
            return Ok(true);
        }

        let mut chunk = [0u8; 8192];
        let n = self.sock.as_ref().read(&mut chunk)?;
        if n == 0 {
            return Ok(false);
        }

        let mut rest = &chunk[..n];
        while !rest.is_empty() {
            let consumed = {
                let mut wire = self.wire.lock().expect("wire poisoned");
                let consumed = wire.conn.read_tls(&mut rest)?;
                wire.conn.process_new_packets().map_err(io::Error::other)?;
                if wire.conn.wants_write() {
                    wire.flush_tls()?;
                }
                consumed
            };
            if consumed == 0 {
                return Err(io::Error::other("TLS 层卡住了：既不收字节也不出明文"));
            }
            self.drain_plaintext()?;
        }
        Ok(true)
    }

    fn drain_plaintext(&mut self) -> io::Result<usize> {
        let mut wire = self.wire.lock().expect("wire poisoned");
        let available = wire.conn.process_new_packets().map_err(io::Error::other)?;
        let pending = available.plaintext_bytes_to_read();
        if pending == 0 {
            return Ok(0);
        }
        if self.buffered.len() + pending > MAX_FRAME_BODY * 2 {
            return Err(io::Error::other("服务端积压了太多没解析的控制消息"));
        }
        let start = self.buffered.len();
        self.buffered.resize(start + pending, 0);
        wire.conn.reader().read_exact(&mut self.buffered[start..])?;
        Ok(pending)
    }
}
