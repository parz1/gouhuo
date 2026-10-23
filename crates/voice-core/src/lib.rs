// SPDX-License-Identifier: MPL-2.0

//! 客户端语音内核。完全独立于 UI —— 如果实测内存不达标，把它拆成独立进程
//! 加托盘常驻是纯工程重构，不需要重写任何逻辑。
//!
//! M1 含：抖动缓冲、实时时钟、socket 调优、度量。
//! M2 起加 WASAPI 采集/渲染。
//! 后续补：WASAPI 采集/渲染、APM(AEC3/NS/AGC)、Opus 编解码封装、传输、全局热键。

#[cfg(feature = "apm")]
pub mod apm;

#[cfg(feature = "codec")]
pub mod audio;
pub mod clock;
#[cfg(feature = "codec")]
pub mod codec;
#[cfg(feature = "pipeline")]
pub mod cue;
pub mod hotkey;
pub mod identity;
pub mod jitter;
pub mod limiter;
pub mod metrics;
#[cfg(feature = "pipeline")]
pub mod miccheck;
pub mod net;
pub mod netsim;
#[cfg(feature = "pipeline")]
pub mod pipeline;
pub mod playout;
pub mod quality;
pub mod redline;
pub mod signal;
pub mod timescale;
#[cfg(feature = "pipeline")]
pub mod tts;
pub mod tunnel;

#[cfg(windows)]
pub mod sysstat;

#[cfg(windows)]
pub mod wasapi;
