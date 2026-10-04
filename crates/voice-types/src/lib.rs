// SPDX-License-Identifier: MPL-2.0
//! Voice data shared by an engine and its frontend. These are Rust API types,
//! not a wire format or a stable ABI; IPC encoding is a separate boundary.

/// 单人音量的上限：400%。
///
/// 有人麦克风离得远、增益又低，100% 听不清，要能拉上去。再高就没有意义了 ——
/// 混音那头有 tanh 软限幅，放大到这个程度原本的动态已经被压平了。
pub const MAX_VOLUME: f32 = 4.0;

#[cfg_attr(feature = "serde", derive(serde::Serialize, serde::Deserialize))]
#[derive(Debug, Clone, Copy, PartialEq)]
pub enum TransmitMode {
    /// 按住才发。全局热键控制 —— 游戏里最常用的方式。
    PushToTalk,
    /// 有人声就发。
    ///
    /// 阈值是**帧能量的分贝**（满刻度为 0 dB）。-45 dB 在安静房间里够用；
    /// 机械键盘和风扇会把它顶起来，那正是 APM 的降噪要解决的事。
    VoiceActivity { threshold_db: f32 },
    /// 一直发。测试用。
    Always,
}

/// 与 GUI 无关的语音事实快照。
#[cfg_attr(feature = "serde", derive(serde::Serialize, serde::Deserialize))]
#[derive(Debug, Clone, Default, PartialEq)]
pub struct VoiceStats {
    pub sequences_exhausted: bool,
    pub udp_failed: bool,
    pub error: Option<String>,
    pub capture_error: Option<String>,
    pub render_error: Option<String>,
    pub transport_error: Option<String>,
    /// 成功提交了语音包且当前控制许可仍有效；包括 VAD 尾音。
    pub transmitting: bool,
    /// 最近有采集帧，且采集线程尚未退出。
    pub input_available: bool,
    /// 输出已成功写入，且播放线程尚未退出。
    pub render_available: bool,
    /// UDP 双向保活是否正常；不通时由客户端执行恢复策略。
    pub udp_ok: bool,
    /// 最近一次保活的往返时间，毫秒。
    pub rtt_ms: f64,
    pub packets_sent: u64,
    pub packets_received: u64,
    /// 该播的帧没到、只好用 PLC 顶上的次数（说话中途，不算句间停顿）——
    /// 这个数涨就是能听出来的卡顿。
    pub underruns: u64,
    /// 现在谁在说话。
    pub speaking: Vec<u32>,
    /// 麦克风当前的电平，分贝（满刻度 0 dB）。
    ///
    /// **是 APM 处理之后的值** —— 那才是真正会被发出去的东西。
    /// 界面上拿它画电平条，用户据此判断「麦克风到底有没有在收音」。
    pub input_db: f32,
}
