// SPDX-License-Identifier: GPL-3.0-or-later
//! 一次完整的「编码 → UDP → 转发 → 抖动缓冲 → 解码 → 播放」测量。
//!
//! 不接任何音频设备。这么做是刻意的：设备那一段（WASAPI 共享 vs 独占）
//! 是另一笔账，混在一起就永远说不清「架构本身到底能做到多快」。
//! 这里出来的是**物理下限**，M2 再把设备那段加上去。
//!
//! 线程布局，跟真客户端一一对应：
//! - 采集线程（这里是调用方线程）：节拍出帧 → Opus 编码 → 封包加密 → 发 UDP
//! - netem 线程：仿真网络（真客户端里没有，它是网络本身）
//! - relay 线程：服务端转发一跳
//! - 接收线程：收 UDP → 解密 → 塞抖动缓冲
//! - 播放线程：节拍取帧 → 解码 / PLC → 输出
//!
//! 采集和播放各自跑独立节拍器，相位是随机的 —— 这跟真机上两个设备时钟
//! 互不相干是一回事，那 0 到一帧的随机相位差是真实存在的延迟，不该抹掉。

use std::io;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use opus::{Application, Bitrate, Channels, Decoder, Encoder, Signal};
use protocol::{encode_voice, VoiceCipher, VoiceHeader, IPV4_UDP_OVERHEAD, SAMPLE_RATE};
use voice_core::clock::{boost_current_thread, Ticker};
use voice_core::jitter::{JitterBuffer, JitterConfig, JitterStats, Playout};
use voice_core::metrics::{Histogram, Summary};
use voice_core::playout::{FrameDecoder, Playback};

use crate::netem::{self, NetemConfig, NetemStats};
use crate::relay;
use voice_core::signal;

/// 加密用的固定密钥。M1 只关心 AEAD 的**开销**，密钥协商是 M3 的事
/// （Noise 框架，或者从控制面 TLS 派生 —— 不自研密码学）。
const PROBE_KEY: [u8; 32] = [0x5a; 32];

#[derive(Debug, Clone)]
pub struct RunCfg {
    pub profile: String,
    pub frame_ms: u32,
    pub seconds: f64,
    pub bitrate: i32,
    pub complexity: i32,
    pub fec: bool,
    pub dtx: bool,
    pub crypto: bool,
    pub relay: bool,
    pub target_frames: usize,
    /// 用自适应抖动缓冲（线上那一份播放器：缓冲 + 解码 + 加速）。
    /// 这时 `target_frames` 不起作用。
    pub adaptive: bool,
    pub net: NetemConfig,
}

impl RunCfg {
    pub fn frame_samples(&self) -> usize {
        SAMPLE_RATE as usize * self.frame_ms as usize / 1000
    }

    pub fn frames(&self) -> usize {
        (self.seconds * 1000.0 / self.frame_ms as f64) as usize
    }
}

#[derive(Debug, Clone)]
pub struct RunResult {
    pub cfg: RunCfg,
    /// 墙钟延迟：播放时刻 − 采集时刻。**不含**帧本身的时长和 Opus 前瞻。
    pub wall_ms: Summary,
    /// 嘴到耳的协议部分 = 墙钟 + 一帧（渲染侧要把整帧摊开播完）+ Opus 前瞻。
    /// 前瞻必须算：它把重建出来的波形整体后移，耳朵是真听得到的。
    pub mouth_to_ear_p95_ms: f64,
    /// 互相关测出来的真实波形延迟。这是最诚实的数字，含 Opus 自己的算法延迟。
    pub signal_delay_ms: Option<f64>,
    pub signal_peak: Option<f64>,
    /// Opus 编码器自报的前瞻，换算成毫秒。用来跟上面两个数字对账。
    pub opus_lookahead_ms: f64,
    pub encode_us: Summary,
    pub decode_us: Summary,
    pub relay_hold_us: Option<Summary>,
    pub jitter: JitterStats,
    pub netem: NetemStats,
    pub tx_packets: u64,
    pub tx_payload_bytes: u64,
    pub tx_wire_bytes: u64,
    pub rx_packets: u64,
    pub rx_rejected: u64,
    pub relay_forwarded: u64,
    pub lost_seqs: Vec<u32>,
    /// 编解码本身占一个核的比例 %。这才是架构该背的 CPU，
    /// 不含探针自己的自旋等待和网络仿真线程。
    pub codec_cpu_pct: f64,
    pub speaking_kbps: f64,
    /// 进程资源采样目前只在 Windows 上可用；其他平台用 0 表示不可用。
    pub cpu_pct: f64,
    /// 非 Windows 上用 0 表示不可用，不是实测内存。
    pub peak_rss_bytes: u64,
    /// 节拍器没能准时醒的次数。不为 0 的话这一次测量的数字都要打折扣。
    pub capture_late_ticks: u64,
    pub playout_late_ticks: u64,
    /// 自适应模式结束时的状态：目标深度、停着等了几次、加速还回去多少。
    pub adaptive_note: Option<String>,
}

struct PlayoutOutcome {
    /// 丢掉的序号，最多记前 32 个。丢在开头、中间还是结尾，
    /// 结论完全不同，只报一个总数是查不出问题的。
    lost_seqs: Vec<u32>,
    latency_ms: Histogram,
    decode_us: Histogram,
    pcm: Vec<i16>,
    play_t0: Instant,
    late_ticks: u64,
    adaptive_note: Option<String>,
}

pub fn run(cfg: &RunCfg) -> io::Result<RunResult> {
    let n = cfg.frame_samples();
    let frames = cfg.frames();
    assert!(frames > 10, "run is too short to mean anything");
    let frame_dur = Duration::from_micros(cfg.frame_ms as u64 * 1000);

    // 参考信号多留两帧，免得收尾越界。
    let reference = Arc::new(signal::chirp(
        (frames + 2) * n,
        SAMPLE_RATE,
        180.0,
        5200.0,
        0.45,
    ));

    // ---- 链路：采集 → netem → (relay) → sink ----
    // 用调好接收缓冲的 socket。默认的 8 KB 在完全没有网络损伤的回环上
    // 都能实测出约 0.5% 丢包 —— 见 voice_core::net。
    let sink = voice_core::net::bind_voice_socket("127.0.0.1:0")?;
    let sink_addr = sink.local_addr()?;

    let relay = if cfg.relay {
        Some(relay::spawn(sink_addr)?)
    } else {
        None
    };
    let first_hop = relay.as_ref().map(|r| r.addr()).unwrap_or(sink_addr);
    let netem = netem::spawn(first_hop, cfg.net)?;

    // seq -> 采集时刻（距 t0 的纳秒）。预分配 + 原子写，采集线程上没有锁。
    let capture_ns: Arc<Vec<AtomicU64>> =
        Arc::new((0..frames).map(|_| AtomicU64::new(u64::MAX)).collect());

    let jb = Arc::new(Mutex::new(JitterBuffer::new(JitterConfig::fixed(
        cfg.target_frames,
    ))));
    // 自适应模式：线上那一份播放器。固定模式还是上面那个裸缓冲，逻辑一行没动 ——
    // CI 卡的协议延迟就是用它量的。
    let player: Arc<Mutex<Option<Playback<OpusFloat>>>> = Arc::new(Mutex::new(if cfg.adaptive {
        let mut p = Playback::new(
            JitterConfig::adaptive(cfg.frame_ms as f64),
            OpusFloat::new()?,
            n,
        );
        p.record_delays();
        Some(p)
    } else {
        None
    }));
    let t0 = Instant::now();

    // ---- 接收线程 ----
    let rx_thread = {
        let jb = Arc::clone(&jb);
        let player = Arc::clone(&player);
        let capture_ns = Arc::clone(&capture_ns);
        let crypto = cfg.crypto;
        std::thread::Builder::new()
            .name("rx".into())
            .spawn(move || {
                boost_current_thread();
                let cipher = VoiceCipher::new(&PROBE_KEY);
                let mut buf = [0u8; protocol::MAX_DATAGRAM];
                let mut payload: Vec<u8> = Vec::with_capacity(512);
                let (mut packets, mut rejected) = (0u64, 0u64);
                // 一直阻塞在 recv_from 上。**不设读超时** —— 读超时跟到包间隔同频时
                // Winsock 会把数据报整个吃掉，见 voice_core::net 的模块文档。
                while let Ok((len, _)) = sink.recv_from(&mut buf) {
                    if voice_core::net::is_wake(&buf[..len]) {
                        break;
                    }
                    let parsed = if crypto {
                        cipher.open(&buf[..len], &mut payload)
                    } else {
                        VoiceHeader::parse(&buf[..len]).map(|(h, p)| {
                            payload.clear();
                            payload.extend_from_slice(p);
                            h
                        })
                    };
                    match parsed {
                        Ok(hdr) => {
                            packets += 1;
                            // 这次 clone 是探针的偷懒；真客户端要走缓冲池，
                            // 50 包/秒 × N 人的分配是能在 profile 里看见的。
                            let mut player = player.lock().expect("player poisoned");
                            if let Some(p) = player.as_mut() {
                                // 发送时刻就用采集时刻：两边是同一个 t0，延迟直接可比。
                                let captured = capture_ns
                                    .get(hdr.seq as usize)
                                    .map_or(u64::MAX, |c| c.load(Ordering::Relaxed));
                                let sent_ms = if captured == u64::MAX {
                                    f64::NAN
                                } else {
                                    captured as f64 / 1e6
                                };
                                let now_ms = t0.elapsed().as_secs_f64() * 1000.0;
                                p.push(hdr.seq, sent_ms, payload.clone(), now_ms);
                            } else {
                                drop(player);
                                jb.lock()
                                    .expect("jitter buffer poisoned")
                                    .push(hdr.seq, payload.clone());
                            }
                        }
                        Err(_) => rejected += 1,
                    }
                }
                (packets, rejected)
            })?
    };

    // ---- 播放线程 ----
    // 先起播放，让它的 t0 早于采集的 t0，互相关的滞后就一定是正的。
    let play_thread = {
        let jb = Arc::clone(&jb);
        let player = Arc::clone(&player);
        let adaptive = cfg.adaptive;
        let capture_ns = Arc::clone(&capture_ns);
        let net = cfg.net;
        let frame_ms = cfg.frame_ms as f64;
        let target = cfg.target_frames;
        std::thread::Builder::new()
            .name("playout".into())
            .spawn(move || -> PlayoutOutcome {
                boost_current_thread();
                let mut dec = Decoder::new(SAMPLE_RATE, Channels::Mono).expect("opus decoder");
                let mut scratch = vec![0i16; n];
                // 排空余量：网络尾巴 + 缓冲深度 + 一点富余。
                let drain =
                    target + ((net.owd_ms + 5.0 * net.jitter_ms) / frame_ms).ceil() as usize + 25;
                let max_ticks = frames + drain;

                let mut out = PlayoutOutcome {
                    lost_seqs: Vec::new(),
                    latency_ms: Histogram::with_capacity(frames),
                    decode_us: Histogram::with_capacity(frames),
                    pcm: Vec::with_capacity((frames + drain) * n),
                    play_t0: Instant::now(),
                    late_ticks: 0,
                    adaptive_note: None,
                };
                let (mut ticker, play_t0) = Ticker::start(frame_dur);
                out.play_t0 = play_t0;

                if adaptive {
                    // 每拍从播放器取一帧。延迟的定义跟下面固定模式一样：
                    // 这一拍的时刻 − 采集时刻（播放器把它自己 FIFO 里排着的也算上了）。
                    let mut frame = vec![0.0f32; n];
                    for _ in 0..max_ticks {
                        let now = ticker.tick();
                        let now_ms = now.duration_since(t0).as_secs_f64() * 1000.0;
                        let mut guard = player.lock().expect("player poisoned");
                        let p = guard.as_mut().expect("adaptive 就一定有播放器");
                        let got = p.pull(now_ms, &mut frame);
                        for delay in p.take_delays() {
                            out.latency_ms.push(delay);
                        }
                        let stats = p.jitter().stats;
                        drop(guard);
                        if got {
                            out.pcm.extend(
                                frame
                                    .iter()
                                    .map(|s| (s.clamp(-1.0, 1.0) * i16::MAX as f32) as i16),
                            );
                        } else {
                            out.pcm.resize(out.pcm.len() + n, 0);
                        }
                        if stats.played + stats.lost >= frames as u64 {
                            break;
                        }
                    }
                    let guard = player.lock().expect("player poisoned");
                    if let Some(p) = guard.as_ref() {
                        out.adaptive_note = Some(format!(
                            "结束时目标 {} 帧（{:.1} ms），停着等 {} 次，加速 {} 次共还回去 {:.0} ms",
                            p.jitter().target_frames(),
                            p.jitter().target_ms(),
                            p.jitter().stats.stalls,
                            p.stats.accelerations,
                            p.stats.removed_ms
                        ));
                        for &us in &p.decoder().timings_us {
                            out.decode_us.push(us);
                        }
                    }
                    drop(guard);
                    out.late_ticks = ticker.late_ticks;
                    return out;
                }

                let mut accounted = 0usize;
                for _ in 0..max_ticks {
                    let now = ticker.tick();
                    let slot = jb.lock().expect("jitter buffer poisoned").pop();

                    let (input, seq): (&[u8], Option<u32>) = match &slot {
                        Playout::Prebuffering => {
                            // 还没起播，输出静音。时间轴必须连续，否则互相关就废了。
                            out.pcm.extend(std::iter::repeat(0i16).take(n));
                            continue;
                        }
                        Playout::Frame { seq, payload, .. } => (payload.as_slice(), Some(*seq)),
                        Playout::Lost { seq } if out.lost_seqs.len() < 32 => {
                            out.lost_seqs.push(*seq);
                            (&[][..], None)
                        }
                        // 空输入 = 让 Opus 做丢包隐藏。
                        Playout::Lost { .. } | Playout::Stall | Playout::Underrun => {
                            (&[][..], None)
                        }
                    };

                    let t = Instant::now();
                    let got = dec.decode(input, &mut scratch, false).unwrap_or(0);
                    out.decode_us.push(t.elapsed().as_secs_f64() * 1e6);
                    if got < n {
                        scratch[got..].fill(0);
                    }
                    out.pcm.extend_from_slice(&scratch);

                    if let Some(seq) = seq {
                        let captured = capture_ns[seq as usize].load(Ordering::Relaxed);
                        if captured != u64::MAX {
                            let played = now.duration_since(t0).as_nanos() as u64;
                            out.latency_ms
                                .push(played.saturating_sub(captured) as f64 / 1e6);
                        }
                    }
                    if matches!(slot, Playout::Frame { .. } | Playout::Lost { .. }) {
                        accounted += 1;
                        if accounted >= frames {
                            break;
                        }
                    }
                }
                out.late_ticks = ticker.late_ticks;
                out
            })?
    };

    // ---- 采集（调用方线程）----
    boost_current_thread();
    let mut enc = Encoder::new(SAMPLE_RATE, Channels::Mono, Application::Voip)
        .map_err(|e| io::Error::other(format!("opus encoder: {e}")))?;
    configure_encoder(&mut enc, cfg).map_err(|e| io::Error::other(format!("opus setup: {e}")))?;
    let opus_lookahead_ms = enc.get_lookahead().unwrap_or(0) as f64 * 1000.0 / SAMPLE_RATE as f64;

    let cipher = VoiceCipher::new(&PROBE_KEY);
    let mut encode_us = Histogram::with_capacity(frames);
    let mut payload = vec![0u8; protocol::MAX_DATAGRAM];
    let mut wire: Vec<u8> = Vec::with_capacity(protocol::MAX_DATAGRAM);
    let (mut tx_packets, mut tx_payload_bytes, mut tx_wire_bytes) = (0u64, 0u64, 0u64);

    #[cfg(windows)]
    let mut cpu = voice_core::sysstat::CpuSampler::start();
    let (mut ticker, cap_t0) = Ticker::start(frame_dur);

    for k in 0..frames {
        // 第 k 次唤醒对应参考信号第 k 块的**最后一个采样点**刚刚产生。
        // 换句话说，这一拍之前的音频才算「已经采到」，这正是打包延迟的来源。
        let now = ticker.tick();
        capture_ns[k].store(now.duration_since(t0).as_nanos() as u64, Ordering::Relaxed);

        let block = &reference[k * n..(k + 1) * n];
        let t = Instant::now();
        let len = enc
            .encode(block, &mut payload)
            .map_err(|e| io::Error::other(format!("opus encode: {e}")))?;
        encode_us.push(t.elapsed().as_secs_f64() * 1e6);

        let hdr = VoiceHeader {
            session: 1,
            seq: k as u32,
            timestamp: (k * n) as u32,
            flags: 0,
        };
        if cfg.crypto {
            cipher.seal(hdr, &payload[..len], &mut wire)
        } else {
            encode_voice(hdr, &payload[..len], &mut wire)
        }
        .map_err(|e| io::Error::other(format!("packetize: {e}")))?;

        tx_packets += 1;
        tx_payload_bytes += len as u64;
        tx_wire_bytes += (wire.len() + IPV4_UDP_OVERHEAD) as u64;
        netem.send(wire.clone());
    }
    let capture_late_ticks = ticker.late_ticks;

    // ---- 收尾 ----
    let mut playout = play_thread.join().expect("playout thread panicked");
    #[cfg(windows)]
    let cpu_pct = cpu.sample();
    #[cfg(not(windows))]
    let cpu_pct = 0.0;
    let netem_stats = netem.shutdown();
    let relay_stats = relay.map(|r| r.shutdown());
    let relay_forwarded = relay_stats.as_ref().map(|s| s.forwarded).unwrap_or(0);
    // 转发器已经停了，管线里不会再有新包；这时候才叫醒收包线程，
    // 保证在途的包全都收完了。
    voice_core::net::send_wake(sink_addr)?;
    let (rx_packets, rx_rejected) = rx_thread.join().expect("rx thread panicked");

    let jitter = match player.lock().expect("player poisoned").as_ref() {
        Some(p) => p.jitter().stats,
        None => jb.lock().expect("jitter buffer poisoned").stats,
    };

    // ---- 信号域延迟 ----
    let (signal_delay_ms, signal_peak) = measure_signal_delay(
        &reference,
        &playout.pcm,
        cap_t0,
        playout.play_t0,
        frame_dur,
        n,
        frames,
    );

    let wall_ms = playout.latency_ms.summary();
    let speaking_kbps = tx_wire_bytes as f64 * 8.0 / cfg.seconds / 1000.0;
    let encode_summary = encode_us.summary();
    let decode_summary = playout.decode_us.summary();
    let codec_cpu_pct =
        (encode_summary.mean + decode_summary.mean) / (cfg.frame_ms as f64 * 1000.0) * 100.0;
    #[cfg(windows)]
    let peak_rss_bytes = voice_core::sysstat::mem_info()
        .map(|m| m.peak_working_set)
        .unwrap_or(0);
    #[cfg(not(windows))]
    let peak_rss_bytes = 0;

    Ok(RunResult {
        cfg: cfg.clone(),
        mouth_to_ear_p95_ms: wall_ms.p95 + cfg.frame_ms as f64 + opus_lookahead_ms,
        wall_ms,
        signal_delay_ms,
        signal_peak,
        opus_lookahead_ms,
        encode_us: encode_summary,
        decode_us: decode_summary,
        relay_hold_us: relay_stats.map(|mut s| s.hold_us.summary()),
        jitter,
        netem: netem_stats,
        tx_packets,
        tx_payload_bytes,
        tx_wire_bytes,
        rx_packets,
        rx_rejected,
        relay_forwarded,
        lost_seqs: std::mem::take(&mut playout.lost_seqs),
        codec_cpu_pct,
        speaking_kbps,
        cpu_pct,
        peak_rss_bytes,
        capture_late_ticks,
        playout_late_ticks: playout.late_ticks,
        adaptive_note: playout.adaptive_note.take(),
    })
}

fn configure_encoder(enc: &mut Encoder, cfg: &RunCfg) -> opus::Result<()> {
    enc.set_bitrate(Bitrate::Bits(cfg.bitrate))?;
    enc.set_complexity(cfg.complexity)?;
    enc.set_signal(Signal::Voice)?;
    enc.set_inband_fec(cfg.fec)?;
    // FEC 只有在编码器知道网络在丢包时才真的往包里塞冗余。
    // 真客户端要拿接收端反馈的丢包率来喂这个值，M1 先给个典型值。
    enc.set_packet_loss_perc(if cfg.fec { 10 } else { 0 })?;
    enc.set_dtx(cfg.dtx)?;
    Ok(())
}

fn measure_signal_delay(
    reference: &[i16],
    played: &[i16],
    cap_t0: Instant,
    play_t0: Instant,
    frame_dur: Duration,
    frame_samples: usize,
    frames: usize,
) -> (Option<f64>, Option<f64>) {
    let fs = SAMPLE_RATE as f64;
    // 取参考信号中段：避开起播静音和收尾残帧。
    let ref_start = frames * frame_samples / 2;
    let ref_len = (frame_samples * 12).clamp(2_400, 9_600);
    // 搜索 0..300 ms，任何超过这个的延迟都已经不用测了，直接是 no-go。
    let max_lag = (0.300 * fs) as usize;

    let Some(est) = signal::best_lag(reference, played, ref_start, ref_len, max_lag) else {
        return (None, None);
    };
    if est.at_boundary || est.peak < 0.5 {
        // 峰贴边或者根本没对上 —— 宁可不报，也不要报一个错的数字。
        return (None, Some(est.peak));
    }

    // 参考信号第 i 个采样点产生于 cap_t0 + (i+1)/fs。
    //
    // 播放流第 m 个采样点播出于 play_t0 + period + m/fs：Ticker 的第一拍在
    // t0 + period，那一拍取到的一帧是给**接下来**一个周期播的。这个 period
    // 很容易漏掉 —— 漏了的话互相关会比墙钟推算少整整一帧，两边对不上账。
    //
    // played[m] ≈ reference[m − lag]，于是
    // 延迟 = (play_t0 − cap_t0) + period + (lag − 1)/fs。
    let skew_ms = if play_t0 <= cap_t0 {
        -(cap_t0.duration_since(play_t0).as_secs_f64() * 1000.0)
    } else {
        play_t0.duration_since(cap_t0).as_secs_f64() * 1000.0
    };
    let delay_ms =
        skew_ms + frame_dur.as_secs_f64() * 1000.0 + (est.lag as f64 - 1.0) * 1000.0 / fs;
    (Some(delay_ms), Some(est.peak))
}

/// 自适应模式下给播放器用的 Opus 解码器：出 f32，顺手记下每次解码花了多久。
struct OpusFloat {
    decoder: Decoder,
    timings_us: Vec<f64>,
}

impl OpusFloat {
    fn new() -> io::Result<Self> {
        Ok(Self {
            decoder: Decoder::new(SAMPLE_RATE, Channels::Mono)
                .map_err(|e| io::Error::other(format!("opus decoder: {e}")))?,
            timings_us: Vec::new(),
        })
    }
}

impl FrameDecoder for OpusFloat {
    fn decode(&mut self, payload: &[u8], out: &mut [f32]) -> bool {
        let t = Instant::now();
        let ok = self.decoder.decode_float(payload, out, false).is_ok();
        self.timings_us.push(t.elapsed().as_secs_f64() * 1e6);
        ok
    }

    fn conceal(&mut self, out: &mut [f32]) -> bool {
        // 空输入 = 让 Opus 做丢包隐藏。
        self.decoder.decode_float(&[], out, false).is_ok()
    }
}
