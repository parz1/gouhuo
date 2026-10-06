// SPDX-License-Identifier: MIT OR Apache-2.0

//! 客户端与服务端共享的**唯一一份**协议定义。
//!
//! 这个 crate 里不放任何业务逻辑，只放「线上长什么样」。两端都依赖它，
//! 改一个字段两边同时编译报错 —— 这就是把它独立出来的全部理由。
//!
//! 分层（抄 Mumble，二十年验证过）：
//! - 控制面：TCP + TLS，登录 / 频道树 / 成员状态 / 文字消息（`control`）
//! - 语音面：UDP，Opus 帧 + 序号 + 时间戳，ChaCha20-Poly1305（`voice`）
//! - UDP 不通时语音包塞进 TCP 回退（M3）
//!
//! 除了线上格式，这里还放两样「客户端和服务端必须理解得一模一样」的东西：
//! - `identity`：身份（Ed25519 公钥）和指纹
//! - `invite`：邀请链接 —— 它是协议的一部分，第三方客户端也得能解析
#![forbid(unsafe_code)]

pub mod base32;
pub mod connection;
pub mod control;
#[cfg(feature = "crypto")]
mod crypto;
mod discovery;
mod identity;
mod invite;
pub mod text;
mod voice;

#[cfg(feature = "crypto")]
pub use crypto::*;
pub use discovery::*;
pub use identity::*;
pub use invite::*;
pub use voice::*;

/// 全链路固定 48 kHz —— Opus 的原生采样率，任何重采样都是白送的延迟和 CPU。
pub const SAMPLE_RATE: u32 = 48_000;

/// 单个 UDP 负载上限。给 IPv6 和常见隧道留足余量，永不分片。
pub const MAX_DATAGRAM: usize = 1200;

/// IPv4 + UDP 头开销。算带宽红线时必须算进去，否则是自欺欺人。
pub const IPV4_UDP_OVERHEAD: usize = 28;
/// IPv6 + UDP 头开销。
pub const IPV6_UDP_OVERHEAD: usize = 48;
