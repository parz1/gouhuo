// SPDX-License-Identifier: MPL-2.0

//! 篝火服务端。
//!
//! 分成两层，分界线是「有没有 IO」：
//!
//! - [`state`] —— 纯状态机。频道树、谁在哪儿、谁能进来。不碰 socket，
//!   不碰时钟，不碰密码学。所有规则都在这里，所以所有规则都能用单元测试钉住。
//! - [`conn`] —— TLS、线程、分帧、签名校验。只负责把字节搬进搬出，
//!   然后把「发生了什么」翻译成对 [`state`] 的一次调用。
//! - [`store`] —— 存档。把状态机广播出来的频道变化写进 SQLite，启动时再读回来。
//!   状态机本身不知道有这一层。
//! - [`voice`] —— UDP 转发。热路径，不碰 [`state`] 的规则，只问它
//!   「这个人在哪个频道、那个频道里还有谁」。
//!
//! 这条线划在这里是有代价的（`conn` 里要写一堆转接代码），换来的是：
//! 「满了之后管理员还进不进得来」这种问题，答案在一个 20 行的函数里，
//! 而不是散在一个 tokio 任务的七个 await 点之间。

pub mod conn;
mod control_io;
pub mod state;
pub mod store;
pub mod voice;
#[cfg(feature = "web")]
pub mod web;

use std::io;
use std::net::TcpListener;
use std::sync::Arc;

/// 接连接，一条一个线程，直到 listener 出错为止。**这个函数会阻塞。**
///
/// 单条连接失败绝不能让整个服务端退出 —— 对端在握手前就跑掉是家常便饭。
pub fn accept_loop(
    listener: TcpListener,
    tls_config: Arc<rustls::ServerConfig>,
    hub: Arc<conn::Hub>,
) {
    for incoming in listener.incoming() {
        let sock = match incoming {
            Ok(sock) => sock,
            Err(e) => {
                eprintln!("accept 失败：{e}");
                continue;
            }
        };
        let Some(admission) = hub.reserve_connection() else {
            continue;
        };
        let peer_addr = sock.peer_addr().ok();
        let tls_config = Arc::clone(&tls_config);
        let hub = Arc::clone(&hub);
        let spawned = std::thread::Builder::new()
            .name("gouhuo-conn".into())
            .spawn(move || {
                if let Err(e) = conn::serve_admitted(sock, tls_config, hub, admission) {
                    // 连接出错是日常（网线拔了、客户端崩了），记一行就行。
                    if let Some(addr) = peer_addr {
                        eprintln!("[{addr}] 连接结束：{e}");
                    }
                }
            });
        if let Err(e) = spawned {
            eprintln!("开不出线程，拒掉这条连接：{e}");
        }
    }
}

/// 起看门狗线程。见 [`conn`] 的模块文档：超时不靠读超时，靠这个。
pub fn spawn_watchdog(hub: Arc<conn::Hub>, interval: std::time::Duration) -> io::Result<()> {
    std::thread::Builder::new()
        .name("gouhuo-sweep".into())
        .spawn(move || loop {
            std::thread::sleep(interval);
            hub.sweep_idle();
        })?;
    Ok(())
}
