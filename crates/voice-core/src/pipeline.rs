// SPDX-License-Identifier: MPL-2.0

//! 整条语音链路。
//!
//! ```text
//! 采集 ──► APM ──► Opus ──► 加密 ──► UDP ─┐
//!                                          │  服务端转发
//! 播放 ◄── 混音 ◄── Opus ◄── 抖动缓冲 ◄────┘
//! ```
//!
//! # 三个线程，各自的时钟来自设备
//!
//! - **发送线程**：阻塞在 [`Capture::read`] 上。采集设备的节奏就是它的节拍
//! - **接收线程**：阻塞在 `recv_from` 上
//! - **播放线程**：阻塞在 [`Render::write`] 上。播放设备的节奏就是它的节拍
//!
//! **没有任何一个线程用 sleep 凑节拍。** 音频设备有自己的晶振，跟系统时钟
//! 差个几十 ppm 是常态；自己数着时间喂数据，几分钟之后就会积累出一次
//! 溢出或欠载，听感上是周期性的爆音。让设备当时钟就没有这个问题。
//!
//! # 不说话的时候不发包
//!
//! 「常驻几小时」的场景里，一屋子人绝大部分时间都不说话。所以松开按键
//! （或者 VAD 判定没人声）之后：
//!
//! 1. 发最后一包，带上 [`FLAG_TERMINATOR`] —— 对面的抖动缓冲据此立刻收尾，
//!    而不是等欠载了才发现这边不说了
//! 2. 然后**一个包都不发**，只留每两秒一次的保活
//!
//! 静默带宽因此是 57 字节 / 2 秒 ≈ 0.23 kbps。
//!
//! # 抖动缓冲是自适应的
//!
//! 每个说话人一个 [`Playback`]：深度跟着网络走，卡完之后靠加速播（删基音周期）
//! 把延迟还回去。见 `jitter` 和 `playout` 两个模块的文档。
//! 固定深度的那个还在（[`JitterConfig::fixed`]），测量工具拿它当基线。

use std::collections::{BTreeMap, VecDeque};
use std::io;
use std::net::{SocketAddr, UdpSocket};
use std::sync::atomic::{AtomicBool, AtomicI32, AtomicU32, AtomicU64, Ordering};
use std::sync::{Arc, Mutex};
use std::thread::JoinHandle;
use std::time::{Duration, Instant};

use protocol::{
    VoiceCipher, VoiceHeader, FLAG_KEEPALIVE, FLAG_TERMINATOR, MAX_DATAGRAM, VOICE_HEADER_LEN,
};

use crate::audio::{Capture, Render, FRAME_MS, FRAME_SAMPLES, SAMPLE_RATE};
use crate::codec::{VoiceDecoder, VoiceEncoder};
use crate::cue::CueQueue;
use crate::jitter::JitterConfig;
use crate::playout::Playback;

/// 固定抖动缓冲的深度（帧）。**语音链路已经不用它了**，留着给测量工具当基线：
/// M1 量过，2 帧（20 ms）在「同城」和「跨省」两档网络下都不欠载，
/// 是能跑进延迟预算的最浅的一档。
pub const DEFAULT_JITTER_FRAMES: usize = 2;

/// 语音链路默认用的抖动缓冲：自适应的。
pub fn default_jitter() -> JitterConfig {
    JitterConfig::adaptive(FRAME_MS as f64)
}

/// 多久发一次保活包。
///
/// 要短于常见 NAT 的 UDP 映射存活时间（很多家用路由器是 30 秒），
/// 又不能密到白烧带宽。2 秒在两者之间，而且它同时是「UDP 还通不通」的探测。
pub const KEEPALIVE_INTERVAL: Duration = Duration::from_secs(2);

/// 多久没收到对方的包就认为他不在说话了。
const SPEAKING_TIMEOUT: Duration = Duration::from_millis(200);

/// 多久没收到某个人的任何包就把他的解码器扔掉。
const SPEAKER_IDLE: Duration = Duration::from_secs(30);

/// 单人音量的上限：400%。
///
/// 有人麦克风离得远、增益又低，100% 听不清，要能拉上去。再高就没有意义了 ——
/// 混音那头有 tanh 软限幅，放大到这个程度原本的动态已经被压平了。
pub const MAX_VOLUME: f32 = 4.0;

/// 多久收不到保活回包就认为 UDP 不通。
const UDP_DEAD_AFTER: Duration = Duration::from_secs(8);

/// 试听队列最深攒几帧。
///
/// 采集和播放是**两个设备、两个晶振**，就算都标称 10 ms 也会慢慢漂开
/// （几十 ppm 是常态，大约每一百秒差一帧）。攒满了就丢最老的，空了就补静音 ——
/// 一百秒掉一帧在试听里完全听不出来，而攒着不丢会让延迟越来越大。
const MONITOR_MAX_FRAMES: usize = 3;

/// 采集之后、编码之前要做的处理。
///
/// 抽成 trait 是为了让 APM 成为**可选**的：服务端不要它（只转发 Opus 包），
/// 而没有 APM 的链路照样能通，只是没有回声消除和降噪，戴耳机用没问题。
///
/// 注意 `Apm` 内部有一把 `Mutex`（sonora 的方法是 `&mut self`，而这个 trait
/// 是 `&self` + `Arc`），所以采集和渲染会互相挡一下。量级见 `apm` 的模块文档。
pub trait AudioProcessor: Send + Sync {
    /// 处理麦克风采到的一帧（回声消除、降噪、增益）。
    fn process_capture(&self, frame: &mut [f32]);
    /// 把要播出去的一帧告诉它，当回声消除的参考信号。
    ///
    /// **必须在真正播出去之前调**，而且每一帧都要调 —— AEC 靠的是把
    /// 「播出去的」和「采回来的」对齐，漏掉几帧就会让它算错延迟，
    /// 然后回声就消不掉了。
    fn analyze_render(&self, frame: &mut [f32]);
}

#[cfg(feature = "apm")]
impl AudioProcessor for crate::apm::Apm {
    fn process_capture(&self, frame: &mut [f32]) {
        // 处理失败就用原始帧。宁可有点回声，也不能让语音断掉。
        let _ = crate::apm::Apm::process_capture(self, frame);
    }
    fn analyze_render(&self, frame: &mut [f32]) {
        let _ = crate::apm::Apm::analyze_render(self, frame);
    }
}

/// 什么时候往外发。
///
/// 能在链路跑着的时候改（[`Pipeline::set_mode`]）—— 改设置不该让声音断一下。
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

impl TransmitMode {
    /// 塞进一个 u32 里，好放进原子变量。
    ///
    /// 阈值用定点存（0.01 dB 一档）：分贝值在 -120..0 之间，精度远远够用，
    /// 而原子浮点数在稳定版 Rust 里没有。
    fn encode(self) -> u32 {
        match self {
            TransmitMode::PushToTalk => 0,
            TransmitMode::Always => 1,
            TransmitMode::VoiceActivity { threshold_db } => {
                let centi = (threshold_db.clamp(-120.0, 0.0) * -100.0) as u32;
                2 | (centi << 8)
            }
        }
    }

    fn decode(raw: u32) -> Self {
        match raw & 0xFF {
            0 => TransmitMode::PushToTalk,
            1 => TransmitMode::Always,
            _ => TransmitMode::VoiceActivity {
                threshold_db: -((raw >> 8) as f32) / 100.0,
            },
        }
    }
}

pub struct PipelineConfig {
    /// 必须与 upstream_key 的生命周期一致，换设备时继续共享。
    pub sequences: Arc<protocol::VoiceSequences>,
    pub session_id: u32,
    pub server: SocketAddr,
    pub upstream_key: [u8; 32],
    pub downstream_key: [u8; 32],
    /// 抖动缓冲。正常用 [`default_jitter`]。
    pub jitter: JitterConfig,
    pub mode: TransmitMode,
}

/// 界面要显示的东西。
#[derive(Debug, Clone, Default, PartialEq)]
pub struct VoiceStats {
    pub sequences_exhausted: bool,
    pub udp_failed: bool,
    pub error: Option<String>,
    /// UDP 那条路通不通（保活有没有回来）。不通就该退回 TCP 传语音。
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

struct Speaker {
    playback: Playback<VoiceDecoder>,
    last_packet: Instant,
    last_voice: Instant,
    /// 每个人的音量，界面上可以单独调。1.0 是原样。
    volume: f32,
    /// 上一个包头里的采样时间戳，和它解开回绕之后的值。见 [`Speaker::sent_ms`]。
    last_timestamp: Option<(u32, i64)>,
    /// 已经报给 `Shared::underruns` 的次数，只报增量。
    reported_stalls: u64,
    /// 收到过 terminator，这一段说完了。说话指示据此立刻熄灭，不用等超时。
    ended: bool,
}

impl Speaker {
    fn new(jitter: JitterConfig) -> Result<Self, opus::Error> {
        let now = Instant::now();
        Ok(Self {
            playback: Playback::new(jitter, VoiceDecoder::new()?, FRAME_SAMPLES),
            last_packet: now,
            last_voice: now - SPEAKING_TIMEOUT,
            volume: 1.0,
            last_timestamp: None,
            reported_stalls: 0,
            ended: false,
        })
    }

    /// 包头里的采样时间戳 → 发送端采到这一帧的时刻（毫秒，发送端自己的时钟）。
    ///
    /// 时间戳是 u32 个采样点，48 kHz 下 24.8 小时绕一圈 —— 一挂就是几小时的场景里
    /// 真会碰上。所以按跟上一个包的差值累加，而不是直接除。差值当成有符号的：
    /// 乱序到的包时间戳比上一个小，那是往回的一小步，不是往前绕了一整圈。
    fn sent_ms(&mut self, timestamp: u32) -> f64 {
        let unwrapped = match self.last_timestamp {
            None => timestamp as i64,
            Some((last, base)) => base + timestamp.wrapping_sub(last) as i32 as i64,
        };
        self.last_timestamp = Some((timestamp, unwrapped));
        unwrapped as f64 * 1000.0 / SAMPLE_RATE as f64
    }
}

/// 一条跑着的语音链路。丢掉它就会把所有线程停下来。
pub struct Pipeline {
    sequences: Arc<protocol::VoiceSequences>,
    stop: Arc<AtomicBool>,
    transmitting: Arc<AtomicBool>,
    muted: Arc<AtomicBool>,
    deafened: Arc<AtomicBool>,
    mode: Arc<AtomicU32>,
    monitoring: Arc<AtomicBool>,
    socket: Arc<UdpSocket>,
    shared: Arc<Shared>,
    threads: Vec<JoinHandle<()>>,
}

struct Shared {
    error: Mutex<Option<String>>,
    speakers: Mutex<BTreeMap<u32, Speaker>>,
    /// 每个人的音量，按会话 id。**跟 [`Speaker`] 分开存**：
    ///
    /// - `Speaker` 是收到第一个包才建的，而用户可能在对方开口之前就调好了
    /// - 安静超过 [`SPEAKER_IDLE`] 的 `Speaker` 会被清掉，下次开口重建 ——
    ///   音量要是只存在它身上，歇一会儿就悄悄回到 100% 了
    ///
    /// `Speaker::volume` 是这里的缓存，播放线程每帧读它就不用多拿一把锁。
    /// 锁的顺序永远是先 `speakers` 后 `volumes`，[`Pipeline::set_volume`]
    /// 两把分开拿、不嵌套。
    volumes: Mutex<BTreeMap<u32, f32>>,
    /// 提示音和念名字，播放线程每帧混一点进去。见 `cue` 模块。
    cues: Arc<CueQueue>,
    packets_sent: AtomicU64,
    packets_received: AtomicU64,
    underruns: AtomicU64,
    /// 最近一次保活回来的时刻（Instant 不能放原子里，存成毫秒差）
    last_keepalive_ms: AtomicU64,
    rtt_us: AtomicU32,
    /// 麦克风电平，存成 0.01 dB 一档的定点 —— 稳定版 Rust 没有原子浮点。
    input_centi_db: AtomicI32,
    /// 试听：采集线程往里放，播放线程取走。
    monitor: Mutex<VecDeque<Vec<f32>>>,
    started: Instant,
}

impl Shared {
    fn now_ms(&self) -> u64 {
        self.started.elapsed().as_millis() as u64
    }

    /// 同一个原点，带小数的毫秒。抖动缓冲要亚毫秒的精度：
    /// 本机回环上一跳才几十微秒，按整毫秒算全是 0。
    fn now_ms_f64(&self) -> f64 {
        self.started.elapsed().as_secs_f64() * 1000.0
    }

    /// 从链路起来算的微秒数。u32 大约 71 分钟回绕 —— 只拿来算往返时间，
    /// 用回绕减法就行。
    fn now_us(&self) -> u32 {
        self.started.elapsed().as_micros() as u32
    }
}

impl Pipeline {
    /// 起链路。**立刻返回**，三个线程在后台跑。
    pub fn start(
        cfg: PipelineConfig,
        mut capture: Box<dyn Capture>,
        mut render: Box<dyn Render>,
        processor: Option<Box<dyn AudioProcessor>>,
    ) -> io::Result<Self> {
        let socket = UdpSocket::bind(match cfg.server {
            SocketAddr::V4(_) => "0.0.0.0:0",
            SocketAddr::V6(_) => "[::]:0",
        })?;
        // **故意不 connect。**
        //
        // connect 过的 UDP socket，内核只收那个地址来的包 —— 听起来是好事，
        // 但它同时挡掉了「给自己发一个包把阻塞的 recv 叫醒」这个手法
        // （见 Drop），而那是关掉链路时唯一能叫醒接收线程的办法。
        //
        // 不 connect 的代价是要自己比对来源。那是两行，而且伪造包本来就
        // 过不了 AEAD 那一关 —— 内核过滤只是省点 CPU，不是安全边界。
        crate::net::set_recv_buffer(&socket, crate::net::DEFAULT_RECV_BUFFER)?;
        let socket = Arc::new(socket);

        let stop = Arc::new(AtomicBool::new(false));
        let transmitting = Arc::new(AtomicBool::new(false));
        let muted = Arc::new(AtomicBool::new(false));
        let deafened = Arc::new(AtomicBool::new(false));
        let mode = Arc::new(AtomicU32::new(cfg.mode.encode()));
        let monitoring = Arc::new(AtomicBool::new(false));
        let shared = Arc::new(Shared {
            error: Mutex::new(None),
            speakers: Mutex::new(BTreeMap::new()),
            volumes: Mutex::new(BTreeMap::new()),
            cues: Arc::new(CueQueue::new()),
            packets_sent: AtomicU64::new(0),
            packets_received: AtomicU64::new(0),
            underruns: AtomicU64::new(0),
            last_keepalive_ms: AtomicU64::new(0),
            rtt_us: AtomicU32::new(0),
            input_centi_db: AtomicI32::new(SILENT_DB_CENTI),
            monitor: Mutex::new(VecDeque::new()),
            started: Instant::now(),
        });

        let jitter = cfg.jitter;
        let processor: Option<Arc<dyn AudioProcessor>> = processor.map(Arc::from);
        let mut threads = Vec::new();

        // ---- 发送 ----
        {
            let socket = Arc::clone(&socket);
            let stop = Arc::clone(&stop);
            let transmitting = Arc::clone(&transmitting);
            let muted = Arc::clone(&muted);
            let monitoring = Arc::clone(&monitoring);
            let shared = Arc::clone(&shared);
            let processor = processor.clone();
            let session_id = cfg.session_id;
            let key = cfg.upstream_key;
            let server = cfg.server;
            let mode = Arc::clone(&mode);
            let sequences = Arc::clone(&cfg.sequences);
            threads.push(spawn("gouhuo-voice-send", move || {
                send_loop(
                    &mut *capture,
                    processor,
                    socket,
                    &stop,
                    &transmitting,
                    &muted,
                    &monitoring,
                    &shared,
                    session_id,
                    key,
                    server,
                    mode,
                    sequences,
                );
            })?);
        }

        // ---- 接收 ----
        {
            let socket = Arc::clone(&socket);
            let stop = Arc::clone(&stop);
            let shared = Arc::clone(&shared);
            let key = cfg.downstream_key;
            let server = cfg.server;
            threads.push(spawn("gouhuo-voice-recv", move || {
                recv_loop(socket, &stop, &shared, key, server, jitter);
            })?);
        }

        // ---- 播放 ----
        {
            let stop = Arc::clone(&stop);
            let deafened = Arc::clone(&deafened);
            let shared = Arc::clone(&shared);
            let processor = processor.clone();
            threads.push(spawn("gouhuo-voice-play", move || {
                play_loop(&mut *render, processor, &stop, &deafened, &shared);
            })?);
        }

        // ---- 保活 ----
        {
            let socket = Arc::clone(&socket);
            let stop = Arc::clone(&stop);
            let session_id = cfg.session_id;
            let key = cfg.upstream_key;
            let server = cfg.server;
            let shared = Arc::clone(&shared);
            let sequences = Arc::clone(&cfg.sequences);
            threads.push(spawn("gouhuo-voice-keepalive", move || {
                keepalive_loop(socket, &stop, &shared, session_id, key, server, sequences);
            })?);
        }

        Ok(Self {
            sequences: cfg.sequences,
            stop,
            transmitting,
            muted,
            deafened,
            mode,
            monitoring,
            socket,
            shared,
            threads,
        })
    }

    /// 按下/松开说话键。
    pub fn set_transmitting(&self, on: bool) {
        self.transmitting.store(on, Ordering::Relaxed);
    }

    /// 闭麦。压过说话键和 VAD。
    pub fn set_muted(&self, muted: bool) {
        self.muted.store(muted, Ordering::Relaxed);
    }

    /// 关耳朵。链路照常转，只是播出去的是静音 —— 见 [`play_loop`]。
    pub fn set_deafened(&self, deafened: bool) {
        self.deafened.store(deafened, Ordering::Relaxed);
    }

    /// 试听自己的麦克风。
    ///
    /// **用扬声器开这个会啸叫** —— 麦克风会把扬声器的声音收回去再放出来。
    /// 界面上要写清楚「戴耳机」。开了 APM 的回声消除之后能好很多，
    /// 但那是可选的，不能指望它。
    pub fn set_monitoring(&self, on: bool) {
        self.monitoring.store(on, Ordering::Relaxed);
        if !on {
            // 关掉的时候把攒着的帧倒干净，否则再打开会先播出一小段旧声音。
            self.shared
                .monitor
                .lock()
                .expect("monitor poisoned")
                .clear();
        }
    }

    /// 往播放里插提示音、念名字的口子。见 `cue` 模块。
    pub fn cues(&self) -> Arc<CueQueue> {
        Arc::clone(&self.shared.cues)
    }

    pub fn is_monitoring(&self) -> bool {
        self.monitoring.load(Ordering::Relaxed)
    }

    /// 换发送方式。链路不断，下一帧就生效。
    pub fn set_mode(&self, mode: TransmitMode) {
        self.mode.store(mode.encode(), Ordering::Relaxed);
        // 从语音激活切到按住说话时，如果不清一下，可能会卡在「一直在发」
        self.transmitting.store(false, Ordering::Relaxed);
    }

    pub fn mode(&self) -> TransmitMode {
        TransmitMode::decode(self.mode.load(Ordering::Relaxed))
    }

    /// 单独调某个人的音量。0.0 是静音，1.0 是原样，最大 [`MAX_VOLUME`]。
    ///
    /// 对方还没开口也可以先调，开口时就是这个音量；安静一阵之后再开口也还是。
    pub fn set_volume(&self, session: u32, volume: f32) {
        let volume = if volume.is_finite() {
            volume.clamp(0.0, MAX_VOLUME)
        } else {
            1.0
        };
        if let Ok(mut volumes) = self.shared.volumes.lock() {
            if volume == 1.0 {
                volumes.remove(&session);
            } else {
                volumes.insert(session, volume);
            }
        }
        if let Ok(mut speakers) = self.shared.speakers.lock() {
            if let Some(speaker) = speakers.get_mut(&session) {
                speaker.volume = volume;
            }
        }
    }

    pub fn stats(&self) -> VoiceStats {
        let now = Instant::now();
        let speaking = self
            .shared
            .speakers
            .lock()
            .map(|speakers| {
                speakers
                    .iter()
                    .filter(|(_, s)| {
                        !s.ended && now.duration_since(s.last_voice) < SPEAKING_TIMEOUT
                    })
                    .map(|(id, _)| *id)
                    .collect()
            })
            .unwrap_or_default();

        let last = self.shared.last_keepalive_ms.load(Ordering::Relaxed);
        let udp_ok = last > 0
            && self.shared.now_ms().saturating_sub(last) < UDP_DEAD_AFTER.as_millis() as u64;

        VoiceStats {
            sequences_exhausted: self.sequences.exhausted(),
            udp_failed: !udp_ok && self.shared.started.elapsed() >= UDP_DEAD_AFTER,
            error: self
                .shared
                .error
                .lock()
                .expect("voice error poisoned")
                .clone(),
            udp_ok,
            rtt_ms: self.shared.rtt_us.load(Ordering::Relaxed) as f64 / 1000.0,
            packets_sent: self.shared.packets_sent.load(Ordering::Relaxed),
            packets_received: self.shared.packets_received.load(Ordering::Relaxed),
            underruns: self.shared.underruns.load(Ordering::Relaxed),
            speaking,
            input_db: self.shared.input_centi_db.load(Ordering::Relaxed) as f32 / 100.0,
        }
    }
}

impl Drop for Pipeline {
    fn drop(&mut self) {
        self.stop.store(true, Ordering::Relaxed);
        // 接收线程阻塞在 recv 上，光置个标志叫不醒它。给**自己**发一个包 ——
        // 这个手法在 M1 就用过（见 net::send_wake），那次是为了绕开
        // SO_RCVTIMEO 会吃数据的坑。
        //
        // 注意要发到 127.0.0.1:我们的端口，不是 socket 绑的 0.0.0.0：
        // 往 0.0.0.0 发包是没有意义的。
        if let Ok(local) = self.socket.local_addr() {
            let loopback = SocketAddr::new(
                match local {
                    SocketAddr::V4(_) => std::net::Ipv4Addr::LOCALHOST.into(),
                    SocketAddr::V6(_) => std::net::Ipv6Addr::LOCALHOST.into(),
                },
                local.port(),
            );
            let _ = crate::net::send_wake(loopback);
        }
        for thread in self.threads.drain(..) {
            let _ = thread.join();
        }
    }
}

fn spawn(name: &str, f: impl FnOnce() + Send + 'static) -> io::Result<JoinHandle<()>> {
    std::thread::Builder::new().name(name.into()).spawn(f)
}

/// 音节内的短暂停顿和低电平尾音不能按 10 ms 逐帧切掉。
/// 只延后 VAD 的关闭；闭麦、松开 PTT 和切换模式仍立即生效。
#[derive(Default)]
struct TransmitGate {
    remaining: u32,
}

impl TransmitGate {
    fn update(&mut self, mode: TransmitMode, level: f32, pressed: bool, muted: bool) -> bool {
        if muted {
            self.remaining = 0;
            return false;
        }
        match mode {
            TransmitMode::VoiceActivity { threshold_db } => {
                if pressed || level > threshold_db {
                    self.remaining = 200 / FRAME_MS;
                    true
                } else if self.remaining > 0 {
                    self.remaining -= 1;
                    true
                } else {
                    false
                }
            }
            TransmitMode::PushToTalk => {
                self.remaining = 0;
                pressed
            }
            TransmitMode::Always => {
                self.remaining = 0;
                true
            }
        }
    }
}

#[allow(clippy::too_many_arguments)]
fn send_loop(
    capture: &mut dyn Capture,
    processor: Option<Arc<dyn AudioProcessor>>,
    socket: Arc<UdpSocket>,
    stop: &AtomicBool,
    transmitting: &AtomicBool,
    muted: &AtomicBool,
    monitoring: &AtomicBool,
    shared: &Shared,
    session_id: u32,
    key: [u8; 32],
    server: SocketAddr,
    mode: Arc<AtomicU32>,
    sequences: Arc<protocol::VoiceSequences>,
) {
    // 音频线程要优先于游戏线程被调度，否则一次掉帧就是一次爆音。
    crate::clock::boost_current_thread();

    let cipher = VoiceCipher::new(&key);
    let Ok(mut encoder) = VoiceEncoder::new() else {
        return;
    };

    let mut frame = vec![0.0f32; FRAME_SAMPLES];
    let mut wire = Vec::with_capacity(MAX_DATAGRAM);
    // 采样时钟：**每采一帧都加**，不管发没发。这样对面能从时间戳上看出
    // 中间静默了多久，而不是以为包丢了。
    let mut was_sending = false;
    let mut gate = TransmitGate::default();

    while !stop.load(Ordering::Relaxed) {
        match capture.read(&mut frame) {
            Ok(true) => {}
            Ok(false) => return,
            Err(error) => {
                *shared.error.lock().expect("voice error poisoned") = Some(format!(
                    "麦克风无法继续录音：{error}。请检查设备后点重试语音，或在设置里换一个麦克风。"
                ));
                return;
            }
        }
        let timestamp = sequences.advance_samples(FRAME_SAMPLES as u32);

        if let Some(processor) = &processor {
            processor.process_capture(&mut frame);
        }

        // 电平和试听都取 **APM 之后**的帧 —— 那才是真正会被发出去的东西。
        // 取处理之前的话，用户看到的电平里还带着回声和底噪，
        // 而那正是 APM 要消掉的，会让他以为自己的麦克风有问题。
        let level = frame_db(&frame);
        shared
            .input_centi_db
            .store((level * 100.0) as i32, Ordering::Relaxed);

        if monitoring.load(Ordering::Relaxed) {
            let mut queue = shared.monitor.lock().expect("monitor poisoned");
            if queue.len() >= MONITOR_MAX_FRAMES {
                queue.pop_front();
            }
            queue.push_back(frame.clone());
        }

        // 闭麦压过一切。按着说话键也不行 —— 用户点了闭麦就是不想出声，
        // 这时候还漏出去一声是很糟糕的那种 bug。
        let sending = gate.update(
            TransmitMode::decode(mode.load(Ordering::Relaxed)),
            level,
            transmitting.load(Ordering::Relaxed),
            muted.load(Ordering::Relaxed),
        );

        if !sending {
            if was_sending {
                // 说完了。补一个 terminator，对面立刻收尾而不是等欠载。
                let Some(seq) = sequences.next(false) else {
                    return;
                };
                let header = VoiceHeader {
                    session: session_id,
                    seq,
                    timestamp,
                    flags: FLAG_TERMINATOR,
                };
                if cipher.seal(header, &[], &mut wire).is_ok() {
                    let _ = socket.send_to(&wire, server);
                }
                was_sending = false;
            }
            continue;
        }

        let Ok(packet) = encoder.encode(&frame) else {
            continue;
        };
        let Some(seq) = sequences.next(false) else {
            return;
        };
        let header = VoiceHeader {
            session: session_id,
            seq,
            timestamp,
            flags: 0,
        };
        if cipher.seal(header, packet, &mut wire).is_ok() && socket.send_to(&wire, server).is_ok() {
            shared.packets_sent.fetch_add(1, Ordering::Relaxed);
        }
        was_sending = true;
    }
}

fn recv_loop(
    socket: Arc<UdpSocket>,
    stop: &AtomicBool,
    shared: &Shared,
    key: [u8; 32],
    server: SocketAddr,
    jitter: JitterConfig,
) {
    crate::clock::boost_current_thread();
    let cipher = VoiceCipher::new(&key);
    let mut buf = [0u8; 2048];
    let mut payload = Vec::with_capacity(MAX_DATAGRAM);

    while !stop.load(Ordering::Relaxed) {
        let Ok((n, from)) = socket.recv_from(&mut buf) else {
            // 收不到不代表完蛋：Windows 上给一个关掉的端口发过包之后，
            // 下一次 recv 会报 WSAECONNRESET。照着它退出就是「有人退游戏，
            // 全频道哑了」。
            continue;
        };
        // **先看停止标志再看包内容。** 关链路时会给自己发一个包把这里叫醒，
        // 所以「收到任何东西 + 已经在停了」就该走人。
        //
        // 反过来写（认 WAKE_MAGIC 这个魔数就退出）的话，任何知道我们地址的人
        // 发一个已知常量就能把别人的语音线程关掉 —— 那是个不用密钥的 DoS。
        if stop.load(Ordering::Relaxed) {
            return;
        }
        if from != server {
            continue;
        }
        let Ok(header) = cipher.open(&buf[..n], &mut payload) else {
            continue;
        };
        shared.packets_received.fetch_add(1, Ordering::Relaxed);

        if header.is_keepalive() {
            // 保活回来了：UDP 两个方向都通。时间戳里装的是我们发出去的时刻。
            shared
                .last_keepalive_ms
                .store(shared.now_ms().max(1), Ordering::Relaxed);
            // 时间戳装的是发出去那一刻的微秒数，跟 shared 同一个原点。
            // **用微秒不是毫秒**：本机回环的往返是几十微秒，按毫秒算一律是 0，
            // 界面上就永远显示 0.0 ms，看着像坏了。
            let rtt = shared.now_us().wrapping_sub(header.timestamp);
            shared.rtt_us.store(rtt, Ordering::Relaxed);
            continue;
        }

        let mut speakers = shared.speakers.lock().expect("speakers poisoned");
        let speaker = match speakers.entry(header.session) {
            std::collections::btree_map::Entry::Occupied(e) => e.into_mut(),
            std::collections::btree_map::Entry::Vacant(e) => {
                let Ok(mut speaker) = Speaker::new(jitter) else {
                    continue;
                };
                if let Some(&volume) = shared
                    .volumes
                    .lock()
                    .expect("volumes poisoned")
                    .get(&header.session)
                {
                    speaker.volume = volume;
                }
                e.insert(speaker)
            }
        };
        speaker.last_packet = Instant::now();

        if header.is_terminator() {
            speaker.playback.end_at(header.seq);
            speaker.ended = true;
            continue;
        }
        speaker.ended = false;
        speaker.last_voice = Instant::now();
        let sent_ms = speaker.sent_ms(header.timestamp);
        speaker
            .playback
            .push(header.seq, sent_ms, payload.clone(), shared.now_ms_f64());
    }
}

fn play_loop(
    render: &mut dyn Render,
    processor: Option<Arc<dyn AudioProcessor>>,
    stop: &AtomicBool,
    deafened: &AtomicBool,
    shared: &Shared,
) {
    crate::clock::boost_current_thread();

    let mut mix = vec![0.0f32; FRAME_SAMPLES];
    let mut decoded = vec![0.0f32; FRAME_SAMPLES];

    while !stop.load(Ordering::Relaxed) {
        mix.fill(0.0);
        {
            let mut speakers = shared.speakers.lock().expect("speakers poisoned");
            let now = Instant::now();
            // 走掉的人要清掉，否则解码器会一直攒着
            speakers.retain(|_, s| now.duration_since(s.last_packet) < SPEAKER_IDLE);

            let now_ms = shared.now_ms_f64();
            for speaker in speakers.values_mut() {
                // 丢包、迟到都在里面处理了：让解码器自己编一帧（PLC），
                // 攒多了就加速播。见 playout 模块。
                let got = speaker.playback.pull(now_ms, &mut decoded);
                // 「停着等」只会在说话中途发生（说完了有 terminator），
                // 所以它正好就是能听出来的那种卡。
                let stalls = speaker.playback.jitter().stats.stalls;
                if stalls > speaker.reported_stalls {
                    shared
                        .underruns
                        .fetch_add(stalls - speaker.reported_stalls, Ordering::Relaxed);
                    speaker.reported_stalls = stalls;
                }
                if !got {
                    continue;
                }
                let volume = speaker.volume;
                for (out, sample) in mix.iter_mut().zip(&decoded) {
                    *out += sample * volume;
                }
            }
        }

        // 试听：把自己的麦克风混进来。
        //
        // **走的是直连，不过编解码**。过一遍 Opus 能听到「别人听你是什么效果」，
        // 但要多等抖动缓冲那 20 ms，加起来就六十多毫秒 —— 延迟听自己说话
        // 超过 50 ms 会明显干扰发音（延迟听觉反馈），用户会以为是软件有回声。
        // 试听要回答的是「麦克风好不好使、增益够不够、有没有噪音」，直连够了。
        if let Some(mine) = shared.monitor.lock().expect("monitor poisoned").pop_front() {
            for (out, sample) in mix.iter_mut().zip(&mine) {
                *out += sample;
            }
        }

        // 提示音跟别人的声音混在一起，**在 APM 的参考信号之前** ——
        // 这样外放的人那边，麦克风收进去的提示音会被当成回声消掉，
        // 不会原样发给全频道。关了耳朵的时候跟别的声音一起静掉。
        shared.cues.mix_into(&mut mix);

        // 关了耳朵就播静音。
        //
        // **照样要把这一帧走完**：解码器的状态要跟着推进（跳帧会让恢复时
        // 第一帧解错），APM 的参考信号也不能断（断了它会算错回声延迟）。
        if deafened.load(Ordering::Relaxed) {
            mix.fill(0.0);
        }

        // 多路叠加会超出 ±1。低电平不变，拐点处幅度和斜率都连续。
        for sample in mix.iter_mut() {
            *sample = crate::limiter::soft_limit(*sample);
        }

        // **播之前**告诉 APM 我们要播什么，它才能把回声消掉。
        if let Some(processor) = &processor {
            processor.analyze_render(&mut mix);
        }
        if let Err(error) = render.write(&mix) {
            *shared.error.lock().expect("voice error poisoned") = Some(format!("耳机或扬声器无法继续播放：{error}。请检查设备后点重试语音，或在设置里换一个输出设备。"));
            return;
        }
    }
}

fn keepalive_loop(
    socket: Arc<UdpSocket>,
    stop: &AtomicBool,
    shared: &Shared,
    session_id: u32,
    key: [u8; 32],
    server: SocketAddr,
    sequences: Arc<protocol::VoiceSequences>,
) {
    let cipher = VoiceCipher::new(&key);
    let mut wire = Vec::with_capacity(VOICE_HEADER_LEN + 32);
    // 保活序号从连接的共享计数器取，只有换密钥才从 0 开始。服务端留了单独防重放窗口，
    // 所以它跟语音的序号互不干扰 —— 两边都得这么认。
    //
    // 第一版让它从 u32::MAX 倒着走，结果是：语音每秒把窗口推进 100，
    // 两秒后倒着走的保活序号已经落在窗口外，被当成重放丢掉。
    // 表现是「UDP 时通时不通」，而语音本身看着一切正常。
    // 一上来立刻发一个：服务端要靠它学到我们的地址，不然只听不说的人
    // 会完全听不见声音。
    loop {
        let Some(seq) = sequences.next(true) else {
            return;
        };
        let header = VoiceHeader {
            session: session_id,
            seq,
            // 时间戳这里装的是「发出去的时刻」，回来就是一次 RTT
            timestamp: shared.now_us(),
            flags: FLAG_KEEPALIVE,
        };
        if cipher.seal(header, &[], &mut wire).is_ok() {
            let _ = socket.send_to(&wire, server);
        }

        // 分成小段睡，这样停的时候不用等满一个周期
        let deadline = Instant::now() + KEEPALIVE_INTERVAL;
        while Instant::now() < deadline {
            if stop.load(Ordering::Relaxed) {
                return;
            }
            std::thread::sleep(Duration::from_millis(50));
        }
    }
}

/// 「完全没有声音」对应的分贝值，定点存法。
const SILENT_DB_CENTI: i32 = -12_000;

/// 一帧的能量，分贝（满刻度 0 dB）。
///
/// `miccheck` 也用它 —— 电平表在两个地方必须是同一套算法，
/// 不然用户会发现「试麦时条子动，进了频道就不动了」。
pub(crate) fn frame_db(frame: &[f32]) -> f32 {
    if frame.is_empty() {
        return -120.0;
    }
    let sum: f32 = frame.iter().map(|s| s * s).sum();
    let rms = (sum / frame.len() as f32).sqrt();
    if rms <= 1e-9 {
        -120.0
    } else {
        20.0 * rms.log10()
    }
}

/// 这条链路理论上的固有延迟，毫秒。不含网络往返。
///
/// 三段：采集攒够一帧、编码器前瞻、抖动缓冲。**不含设备本身的延迟** ——
/// 那个是声卡和驱动决定的，M2 实测 30.4 ms。
pub fn intrinsic_latency_ms(jitter_frames: usize) -> f64 {
    let mut encoder = match VoiceEncoder::new() {
        Ok(e) => e,
        Err(_) => return f64::NAN,
    };
    let lookahead_ms =
        crate::codec::encoder_lookahead(&mut encoder) as f64 * 1000.0 / SAMPLE_RATE as f64;
    FRAME_MS as f64 + lookahead_ms + (jitter_frames * FRAME_MS as usize) as f64
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn vad_preserves_quiet_tails_but_mute_and_ptt_release_are_immediate() {
        let vad = TransmitMode::VoiceActivity {
            threshold_db: -45.0,
        };
        let mut gate = TransmitGate::default();
        assert!(!gate.update(vad, -60.0, false, false));
        assert!(gate.update(vad, -25.0, false, false));
        for _ in 0..200 / FRAME_MS {
            assert!(gate.update(vad, -60.0, false, false));
        }
        assert!(!gate.update(vad, -60.0, false, false));
        assert!(gate.update(vad, -25.0, false, false));
        assert!(!gate.update(vad, -25.0, true, true));
        assert!(!gate.update(vad, -60.0, false, false));
        assert!(gate.update(vad, -25.0, false, false));
        assert!(!gate.update(TransmitMode::PushToTalk, -25.0, false, false));
        assert!(!gate.update(vad, -60.0, false, false));
    }

    #[test]
    fn silence_is_far_below_the_voice_threshold() {
        let silence = vec![0.0f32; FRAME_SAMPLES];
        assert!(frame_db(&silence) < -100.0);
    }

    #[test]
    fn a_loud_frame_is_near_full_scale() {
        let loud = vec![0.5f32; FRAME_SAMPLES];
        let db = frame_db(&loud);
        assert!(
            (-7.0..-5.0).contains(&db),
            "0.5 满幅该在 -6 dB 附近，实际 {db}"
        );
    }

    /// 默认 VAD 阈值必须落在「安静房间的底噪」和「正常说话」之间。
    #[test]
    fn the_default_vad_threshold_separates_speech_from_silence() {
        let threshold = -45.0;
        // 很轻的底噪
        let noise: Vec<f32> = (0..FRAME_SAMPLES)
            .map(|i| 0.0005 * ((i * 7919) % 17) as f32)
            .collect();
        // 正常说话大概在 -20 dB 上下
        let speech: Vec<f32> = (0..FRAME_SAMPLES)
            .map(|i| 0.1 * (i as f32 * 0.05).sin())
            .collect();
        assert!(frame_db(&noise) < threshold, "底噪 {}", frame_db(&noise));
        assert!(frame_db(&speech) > threshold, "说话 {}", frame_db(&speech));
    }

    /// 固有延迟必须算上编码器前瞻 —— M1 第一版漏了这一项，数字偏小。
    #[test]
    fn intrinsic_latency_includes_the_encoder_lookahead() {
        let naive = FRAME_MS as f64 + (DEFAULT_JITTER_FRAMES * FRAME_MS as usize) as f64;
        let real = intrinsic_latency_ms(DEFAULT_JITTER_FRAMES);
        assert!(real > naive, "没算前瞻：{real} vs {naive}");
        assert!(real < naive + 10.0, "前瞻不该有这么大：{real}");
    }

    /// 发送方式塞进原子变量再取出来，必须还是原来那个。
    #[test]
    fn transmit_modes_round_trip_through_the_atomic_encoding() {
        for mode in [
            TransmitMode::PushToTalk,
            TransmitMode::Always,
            TransmitMode::VoiceActivity {
                threshold_db: -45.0,
            },
            TransmitMode::VoiceActivity {
                threshold_db: -60.5,
            },
            TransmitMode::VoiceActivity { threshold_db: 0.0 },
        ] {
            assert_eq!(TransmitMode::decode(mode.encode()), mode, "{mode:?}");
        }
    }

    /// 阈值越界不能变成别的模式 —— 那会让用户一个字都发不出去，
    /// 而界面上显示的还是「语音激活」。
    #[test]
    fn an_out_of_range_threshold_is_clamped_not_wrapped() {
        for threshold_db in [-500.0, 100.0, f32::NAN] {
            let decoded =
                TransmitMode::decode(TransmitMode::VoiceActivity { threshold_db }.encode());
            assert!(
                matches!(decoded, TransmitMode::VoiceActivity { .. }),
                "{threshold_db} 变成了 {decoded:?}"
            );
        }
    }

    #[test]
    fn keepalive_beats_common_nat_timeouts() {
        // 很多家用路由器的 UDP 映射是 30 秒
        assert!(KEEPALIVE_INTERVAL.as_secs() * 4 < 30);
    }
}
