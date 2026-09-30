// SPDX-License-Identifier: MPL-2.0

//! 客户端逻辑，**不含任何界面**。
//!
//! 连服务器、走认证、维护一份服务端状态的镜像、收发控制面消息。
//! 界面层只做两件事：读 [`Roster`]，和调 [`Client`] 上的方法。
//!
//! # 为什么跟界面分开
//!
//! 跟 `voice-core` 是同一个理由：换界面框架、或者将来把界面拆成独立进程，
//! 这里一行都不用动。附带的好处是**它能直接对着真服务端跑集成测试** ——
//! 「点了切换频道，别人那边看到了吗」这种问题，不需要打开界面就能回答。
//!
//! # 线程
//!
//! 一个 [`Client`] 两个线程，**活得比底下的 TCP 连接久**（断了会自己重连，
//! 见 `client` 模块的文档）：
//!
//! - **读线程**：阻塞读 TLS，解出消息，更新 [`Roster`]，往 channel 里发 [`Event`]；
//!   连接断了就由它按退避重连
//! - **心跳线程**：定期发 Ping（服务端 30 秒不收东西就踢人），顺带看服务端
//!   多久没动静了 —— 太久就掐掉连接，让读线程去重连
//!
//! 界面线程既不读也不写 socket，它只是 channel 的另一端。所以界面卡住了
//! 不会导致掉线，网络卡住了也不会冻住界面 —— 这两件事在语音软件里都会发生。

pub mod address;
pub mod error;
pub mod roster;

mod client;
mod probe;
mod wire;

pub use client::{
    Client, Ended, Event, Options, CONNECT_TIMEOUT, HEARTBEAT, LIVENESS_TIMEOUT, RECONNECT_FIRST,
    RECONNECT_MAX,
};
pub use error::ConnectError;
pub use probe::probe;
pub use roster::{ChannelNode, ChatLine, Roster, MAX_CHAT_LINES};

/// 报给服务端的客户端版本。只用来排查问题，不参与任何判断。
pub fn client_version() -> String {
    format!("gouhuo {}", env!("CARGO_PKG_VERSION"))
}
