// SPDX-License-Identifier: GPL-3.0-or-later
//! 报表输出。
//!
//! 表格一律用 ASCII 表头，中文只出现在表格外面 —— 中日韩字符在不同终端里
//! 宽度不一致，混进表格列就永远对不齐。

use crate::bandwidth::BandwidthRow;
use crate::cpucost::CpuCostRow;
use crate::run::RunResult;
use voice_core::redline;

/// 判定档位。红线说的是「同城」，所以结论只在这一档上下。
pub const VERDICT_PROFILE: &str = "city";

pub fn matrix_header() {
    println!();
    println!("延迟矩阵（协议链路，不含音频设备）");
    println!(
        "{:<8} {:>5} {:>6} {:>7} {:>7} {:>7} {:>8} {:>7} {:>6} {:>6} {:>6}",
        "profile",
        "ms/f",
        "target",
        "p50",
        "p95",
        "p99",
        "m2e-p95",
        "signal",
        "loss%",
        "under",
        "late"
    );
    println!("{}", "-".repeat(84));
}

pub fn matrix_row(r: &RunResult) {
    let played = r.jitter.played.max(1) as f64;
    let loss_pct = (r.jitter.lost + r.jitter.late) as f64 / played * 100.0;
    let signal = match r.signal_delay_ms {
        Some(v) => format!("{v:.1}"),
        None => "  n/a".to_string(),
    };
    println!(
        "{:<8} {:>5} {:>6} {:>7.1} {:>7.1} {:>7.1} {:>8.1} {:>7} {:>6.2} {:>6} {:>6}",
        r.cfg.profile,
        r.cfg.frame_ms,
        r.cfg.target_frames,
        r.wall_ms.p50,
        r.wall_ms.p95,
        r.wall_ms.p99,
        r.mouth_to_ear_p95_ms,
        signal,
        loss_pct,
        r.jitter.underruns,
        r.capture_late_ticks + r.playout_late_ticks,
    );
}

pub fn matrix_legend() {
    println!("{}", "-".repeat(84));
    println!("  p50/p95/p99  墙钟延迟：播放时刻 − 采集时刻（毫秒）");
    println!("  m2e-p95      协议部分的嘴到耳 = p95 + 一帧（渲染侧摊开）+ Opus 前瞻");
    println!("  signal       互相关实测的波形延迟。跟 m2e 是两套独立测量，对得上才可信");
    println!("  loss%        听感上丢掉的帧（真丢包 + 迟到被丢）占已播帧的比例");
    println!("  under        抖动缓冲欠载次数。固定缓冲每欠载一次，延迟就阶梯式上一级");
    println!("  late         节拍器没准时醒的次数。不是 0 的话这一行的数字要打折扣");
    println!();
    println!("  固定缓冲的稳态延迟 ≈ 网络单程 + (target−1)×帧长 + 采集/播放两个时钟的相位差，");
    println!("  相位差在真机上是 0 到一帧之间的随机值（两个设备时钟互不相干）。");
    println!("  这里为了让互相关的滞后一定为正，播放线程先于采集线程启动，相位差被钉在");
    println!("  最坏的那一端 —— 所以上表比真机的平均值**偏高约半帧**。这是刻意的保守。");
}

pub fn detail(r: &RunResult) {
    let c = &r.cfg;
    println!();
    let buffer = if c.adaptive {
        "自适应".to_string()
    } else {
        format!("{} 帧", c.target_frames)
    };
    println!(
        "=== 详细分解：{} / {} ms 帧 / 缓冲 {} ===",
        c.profile, c.frame_ms, buffer
    );
    println!(
        "  Opus: {} kbps, complexity {}, FEC {}, DTX {} | 加密 {} | 服务端转发 {}",
        c.bitrate / 1000,
        c.complexity,
        onoff(c.fec),
        onoff(c.dtx),
        onoff(c.crypto),
        onoff(c.relay),
    );
    println!(
        "  网络仿真: 单程 {:.1} ms, 抖动 σ={:.1} ms, 丢包 {:.2}%",
        c.net.owd_ms,
        c.net.jitter_ms,
        c.net.loss * 100.0
    );
    println!();

    // 下面这张表是**各项的量级**，不是一个求和式。网络传输和抖动缓冲攒帧是
    // 同时发生的 —— 包在飞的时候缓冲也在填 —— 所以把这些数加起来会严重高估。
    // 真正成立的关系写在表尾。
    println!("  延迟各项量级（毫秒，注意：不是加法，见表尾）");
    budget_line("打包（一帧攒满才能编）", fmt(c.frame_ms as f64));
    budget_line("Opus 编码器前瞻（自报）", fmt(r.opus_lookahead_ms));
    budget_line("编码 p95", fmt(r.encode_us.p95 / 1000.0));
    budget_line("网络（仿真单程）", fmt(c.net.owd_ms));
    match &r.relay_hold_us {
        Some(s) => budget_line("服务端转发 p95", fmt(s.p95 / 1000.0)),
        None => budget_line("服务端转发", "未接入".to_string()),
    }
    budget_line(
        "抖动缓冲（目标深度）",
        if c.adaptive {
            "自适应".to_string()
        } else {
            fmt(c.target_frames as f64 * c.frame_ms as f64)
        },
    );
    budget_line("解码 p95", fmt(r.decode_us.p95 / 1000.0));
    budget_line("渲染侧摊开一帧", fmt(c.frame_ms as f64));
    println!("    {}", "-".repeat(44));
    println!(
        "    成立的关系：墙钟 ≈ 网络单程 {:.1} + (target−1)×帧长 {:.1} + 时钟相位差(0~{:.0})",
        c.net.owd_ms,
        (c.target_frames as f64 - 1.0) * c.frame_ms as f64,
        c.frame_ms
    );
    println!("                嘴到耳 = 墙钟 + 一帧（渲染侧摊开）+ Opus 前瞻");
    budget_line("墙钟实测 p95", fmt(r.wall_ms.p95));
    budget_line("协议嘴到耳 p95（墙钟+帧+前瞻）", fmt(r.mouth_to_ear_p95_ms));
    match (r.signal_delay_ms, r.signal_peak) {
        (Some(v), Some(p)) => {
            println!(
                "    {:<30} {:>8}   (相关峰 {:.2})",
                "互相关实测波形延迟",
                fmt(v),
                p
            );
            // 对账：互相关量到的波形延迟应该 = 墙钟均值 + 一帧 + Opus 前瞻。
            // 对不上就说明两套测量里有一套的时间基准错了，数字一律不能信。
            let expected = r.wall_ms.mean + c.frame_ms as f64 + r.opus_lookahead_ms;
            let gap = v - expected;
            println!(
                "    对账：墙钟均值 {:.1} + 一帧 {:.1} + 前瞻 {:.1} = {:.1}，互相关 {:.1}，差 {:+.1} ms {}",
                r.wall_ms.mean,
                c.frame_ms as f64,
                r.opus_lookahead_ms,
                expected,
                v,
                gap,
                if gap.abs() < 1.5 { "（对上了）" } else { "（对不上，别信这一行）" }
            );
        }
        _ => budget_line("互相关实测波形延迟", "对不上".to_string()),
    }
    println!();

    println!("  管线计数");
    println!(
        "    发出 {} / netem 收 {} 丢 {} 发 {} / 转发器转 {} / 收到 {} / 解密失败 {}",
        r.tx_packets,
        r.netem.accepted,
        r.netem.dropped,
        r.netem.delivered,
        r.relay_forwarded,
        r.rx_packets,
        r.rx_rejected
    );
    println!(
        "    每包平均：Opus 负载 {:.0} B，含头含 tag 上线 {:.0} B（再加 28 B 的 IP/UDP 头）",
        r.tx_payload_bytes as f64 / r.tx_packets.max(1) as f64,
        r.tx_wire_bytes as f64 / r.tx_packets.max(1) as f64 - protocol::IPV4_UDP_OVERHEAD as f64
    );
    println!(
        "    抖动缓冲: 入 {} / 播出 {} / 丢包 PLC {} / 迟到丢弃 {} / 欠载 {} / 重复 {} / 预缓冲拍 {} / 峰值深度 {}",
        r.jitter.pushed,
        r.jitter.played,
        r.jitter.lost,
        r.jitter.late,
        r.jitter.underruns,
        r.jitter.duplicate,
        r.jitter.prebuffer_ticks,
        r.jitter.max_depth
    );
    if let Some(note) = &r.adaptive_note {
        println!("    自适应: {note}");
    }
    if !r.lost_seqs.is_empty() {
        println!(
            "    丢掉的序号（共 {} 帧，最多列 32 个）: {:?}",
            r.jitter.lost, r.lost_seqs
        );
    }
    println!(
        "    编码 p50/p95/max: {:.0}/{:.0}/{:.0} µs   解码 p50/p95/max: {:.0}/{:.0}/{:.0} µs",
        r.encode_us.p50,
        r.encode_us.p95,
        r.encode_us.max,
        r.decode_us.p50,
        r.decode_us.p95,
        r.decode_us.max
    );
    if let Some(s) = &r.relay_hold_us {
        println!(
            "    转发器停留 p50/p95/max: {:.0}/{:.0}/{:.0} µs",
            s.p50, s.p95, s.max
        );
    }
    println!();
    println!("  资源");
    println!(
        "    编解码 CPU {:.2}% 单核（红线 {:.0}%）  <- 这才是架构该背的那部分",
        r.codec_cpu_pct,
        redline::CPU_PCT
    );
    if cfg!(windows) {
        println!(
            "    进程 CPU   {:.2}% 单核  <- 含探针自己的自旋等待和网络仿真线程，不可比",
            r.cpu_pct
        );
        println!(
            "    峰值工作集 {:.1} MB（红线 {:.0} MB，但要等 voice-core 独立进程后才算数）",
            r.peak_rss_bytes as f64 / 1_048_576.0,
            redline::VOICE_CORE_RSS_MB
        );
    } else {
        println!("    进程 CPU / 峰值工作集：不可用（仅 Windows 支持；JSON 中以 0 表示）");
    }
    println!(
        "    说话时单条上行 {:.1} kbps（红线 {:.0}）",
        r.speaking_kbps,
        redline::SPEAKING_KBPS
    );
}

pub fn bandwidth_table(rows: &[BandwidthRow]) {
    println!();
    println!("带宽（含 IP/UDP 28B + 包头 13B + AEAD tag 16B）");
    println!(
        "{:<5} {:>7} {:>5} {:>9} {:>8} {:>11} {:>11} {:>12} {:>8}",
        "ms/f",
        "opus",
        "pps",
        "fixed-B",
        "opus-B",
        "fixed-kbps",
        "talk-kbps",
        "silent-drop",
        "dtx-hit"
    );
    println!("{}", "-".repeat(86));
    for r in rows {
        println!(
            "{:<5} {:>7} {:>5.0} {:>9} {:>8} {:>11.1} {:>11.1} {:>12.1} {:>7.0}%",
            r.frame_ms,
            format!("{}k", r.bitrate / 1000),
            r.pps,
            r.fixed_overhead_bytes,
            r.median_speaking_payload,
            r.overhead_kbps,
            r.speaking_kbps,
            r.silent_drop_dtx_kbps,
            r.dtx_hit_rate * 100.0
        );
    }
    println!("{}", "-".repeat(86));
    println!("  opus-B       Opus 负载中位数字节。跟 fixed-B 一比就知道开销占了多少");
    println!("  fixed-kbps   跟 Opus 码率无关的固定开销。10 ms 帧会让它翻倍");
    println!("  silent-drop  静音时 DTX 帧根本不发的带宽 —— 这才是能进 5 kbps 红线的做法");
    println!(
        "               （静音仍逐帧发的话是 {:.1} kbps，DTX 只缩小负载，不会替你停发）",
        rows.iter()
            .map(|r| r.silent_keep_sending_kbps)
            .fold(f64::INFINITY, f64::min)
    );
    println!(
        "  加密 {} —— tag 16 字节已经算在 fixed-B 里",
        if rows.first().map(|r| r.crypto).unwrap_or(false) {
            "开"
        } else {
            "关"
        }
    );
}

pub fn cpu_table(rows: &[CpuCostRow]) {
    println!();
    println!("编解码 CPU 成本（单核占比）");
    println!(
        "{:<5} {:>5} {:>10} {:>10} {:>10} {:>9} {:>10}",
        "ms/f", "cplx", "enc-p50-us", "enc-p95-us", "dec-p50-us", "1-stream", "3-streams"
    );
    println!("{}", "-".repeat(66));
    for r in rows {
        println!(
            "{:<5} {:>5} {:>10.0} {:>10.0} {:>10.0} {:>8.2}% {:>9.2}%",
            r.frame_ms,
            r.complexity,
            r.encode_us_p50,
            r.encode_us_p95,
            r.decode_us_p50,
            r.cpu_pct_1_stream,
            r.cpu_pct_3_streams
        );
    }
    println!("{}", "-".repeat(66));
    println!(
        "  1-stream    一路编码 + 一路解码（红线 {:.0}%）",
        redline::CPU_PCT
    );
    println!("  3-streams   一路编码 + 三路解码（频道里三个人同时说话）");
    println!("  还没算进去：APM（AEC3 + 降噪 + AGC）。AEC3 是这条链路上最贵的一块，M2 才知道");
}

/// 一次运行的音质是否可接受。延迟再低，欠载和迟到丢包一多就是没法用的。
///
/// 判据：
/// - 抖动缓冲一次都不欠载（欠载 = 重新预缓冲 = 延迟阶梯式上涨，还回不来）
/// - 听感丢帧不超过网络本身的丢包率再加 1 个百分点
/// - 节拍器一次都没迟到（迟到说明测量本身不可信，不是架构的锅）
pub fn quality_ok(r: &RunResult) -> bool {
    let played = r.jitter.played.max(1) as f64;
    let heard_loss = (r.jitter.lost + r.jitter.late) as f64 / played;
    r.jitter.underruns == 0
        && heard_loss <= r.cfg.net.loss + 0.01
        && r.capture_late_ticks + r.playout_late_ticks == 0
}

/// 在判定档里挑出既进预算、音质又过关的配置；延迟相同时优先大帧长
/// （大帧长的固定带宽开销只有一半）。
pub fn pick_best(runs: &[RunResult]) -> Option<&RunResult> {
    runs.iter()
        .filter(|r| r.cfg.profile == VERDICT_PROFILE)
        .filter(|r| quality_ok(r) && r.mouth_to_ear_p95_ms <= redline::GATE_PROTOCOL_MS)
        .min_by(|a, b| {
            a.mouth_to_ear_p95_ms
                .partial_cmp(&b.mouth_to_ear_p95_ms)
                .expect("no NaN in latency")
                .then(b.cfg.frame_ms.cmp(&a.cfg.frame_ms))
        })
}

pub fn verdict(runs: &[RunResult], bw: &[BandwidthRow], cpu: &[CpuCostRow]) {
    let budget = redline::GATE_PROTOCOL_MS;
    println!();
    println!("{}", "=".repeat(84));
    println!("M1 结论");
    println!("{}", "=".repeat(84));
    println!(
        "  协议链路的防回归闸：{budget:.0} ms（实测 {:.1}，留了余量）",
        redline::MEASURED_PROTOCOL_MS
    );
    println!(
        "  端到端的账：协议 {:.1} + 设备 {:.1} + APM {:.1} = {:.1} ms，产品线 {:.0} ms",
        redline::MEASURED_PROTOCOL_MS,
        redline::DEVICE_BUDGET_MS,
        redline::APM_BUDGET_MS,
        redline::MEASURED_E2E_MS,
        redline::E2E_MS
    );
    println!("  这里只卡协议这一段 —— 它是唯一只跟我们的代码有关的部分。");
    println!();

    println!("  {VERDICT_PROFILE} 档各配置：");
    println!(
        "  {:<6} {:>7} {:>9} {:>7} {:>7} {:>7}  判定",
        "ms/f", "target", "m2e-p95", "under", "loss%", "budget"
    );
    for r in runs.iter().filter(|r| r.cfg.profile == VERDICT_PROFILE) {
        let played = r.jitter.played.max(1) as f64;
        let heard_loss = (r.jitter.lost + r.jitter.late) as f64 / played * 100.0;
        let fits = r.mouth_to_ear_p95_ms <= budget;
        let ok = quality_ok(r);
        println!(
            "  {:<6} {:>7} {:>9.1} {:>7} {:>7.2} {:>7}  {}",
            r.cfg.frame_ms,
            r.cfg.target_frames,
            r.mouth_to_ear_p95_ms,
            r.jitter.underruns,
            heard_loss,
            if fits { "in" } else { "over" },
            match (fits, ok) {
                (true, true) => "可用",
                (true, false) => "延迟够但音质不行",
                (false, true) => "音质够但超预算",
                (false, false) => "都不行",
            }
        );
    }
    println!();

    match pick_best(runs) {
        Some(best) => {
            println!(
                "  >>> GO：{} 档下 {} ms 帧 + 缓冲 {} 帧 可用，协议链路嘴到耳 p95 = {:.1} ms",
                VERDICT_PROFILE,
                best.cfg.frame_ms,
                best.cfg.target_frames,
                best.mouth_to_ear_p95_ms
            );
            if let Some(v) = best.signal_delay_ms {
                println!("      互相关实测波形延迟 {v:.1} ms（两套独立测量对得上）");
            }
            println!(
                "      加上设备 {:.1} + APM {:.1}，端到端 {:.1} ms（产品线 {:.0}）",
                redline::DEVICE_BUDGET_MS,
                redline::APM_BUDGET_MS,
                best.mouth_to_ear_p95_ms + redline::DEVICE_BUDGET_MS + redline::APM_BUDGET_MS,
                redline::E2E_MS
            );
            if let Some(worst) = runs.iter().find(|r| {
                r.cfg.profile == "bad"
                    && r.cfg.frame_ms == best.cfg.frame_ms
                    && r.cfg.target_frames == best.cfg.target_frames
            }) {
                println!(
                    "      同配置在 bad 档（抖动 σ=15 ms）：m2e-p95 {:.1} ms，欠载 {} 次 —— {}",
                    worst.mouth_to_ear_p95_ms,
                    worst.jitter.underruns,
                    if quality_ok(worst) {
                        "还扛得住"
                    } else {
                        "固定缓冲扛不住；语音链路用的是自适应缓冲，对比见 `latency-probe sim`"
                    }
                );
            }
            println!();
            println!("      设备和 APM 那两段 M2 已经测完了，见 docs/m2-baseline.txt。");
        }
        None => {
            let closest = runs
                .iter()
                .filter(|r| r.cfg.profile == VERDICT_PROFILE && quality_ok(r))
                .min_by(|a, b| {
                    a.mouth_to_ear_p95_ms
                        .partial_cmp(&b.mouth_to_ear_p95_ms)
                        .expect("no NaN")
                });
            match closest {
                Some(c) => println!(
                    "  >>> NO-GO：{} 档下音质过关的最低延迟是 {:.1} ms（{} ms 帧 / 缓冲 {} 帧），超预算 {:.1} ms。",
                    VERDICT_PROFILE,
                    c.mouth_to_ear_p95_ms,
                    c.cfg.frame_ms,
                    c.cfg.target_frames,
                    c.mouth_to_ear_p95_ms - budget
                ),
                None => {
                    println!("  >>> NO-GO：{VERDICT_PROFILE} 档下没有任何配置的音质是可接受的。")
                }
            }
            println!("      先解决这个，不要往下做任何 UI 或功能。可动的旋钮按性价比排序：");
            println!(
                "      1) 抖动缓冲深度 —— 每减一帧省一整帧，但欠载会变多（M4 自适应缓冲的主战场）"
            );
            println!("      2) 帧长 20 → 10 ms —— 省打包和缓冲，但固定带宽开销翻倍，先看带宽表");
            println!(
                "      3) 设备那 {:.1} ms 里渲染队列占 20 —— 压到 10 能省 10 ms，代价是偶尔咔哒",
                redline::DEVICE_BUDGET_MS
            );
            println!("      4) 检查矩阵的 late 列 —— 节拍器不准的话，这不是架构的锅");
        }
    }

    println!();
    println!("  其余红线对照：");
    let best_frame = pick_best(runs).map(|r| r.cfg.frame_ms).unwrap_or(20);
    let talk: Vec<&BandwidthRow> = bw.iter().filter(|r| r.frame_ms == best_frame).collect();
    match talk.iter().filter(|r| r.speaking_kbps <= redline::SPEAKING_KBPS).max_by_key(|r| r.bitrate)
    {
        Some(r) => println!(
            "    说话带宽 {:.0} kbps：{} ms 帧下 Opus 最高只能开到 {} kbps（实测 {:.1} kbps）",
            redline::SPEAKING_KBPS,
            r.frame_ms,
            r.bitrate / 1000,
            r.speaking_kbps
        ),
        None => println!(
            "    说话带宽 {:.0} kbps：破线 —— {} ms 帧下连最低的 Opus 码率都超了（固定开销就有 {:.1} kbps）",
            redline::SPEAKING_KBPS,
            best_frame,
            talk.first().map(|r| r.overhead_kbps).unwrap_or(0.0)
        ),
    }
    let silent_best = bw
        .iter()
        .map(|r| r.silent_drop_dtx_kbps)
        .fold(f64::INFINITY, f64::min);
    println!(
        "    静音带宽 {:.0} kbps：{}（实测 {:.1} kbps，前提是 DTX 帧根本不发）",
        redline::SILENT_KBPS,
        if silent_best <= redline::SILENT_KBPS {
            "达标"
        } else {
            "破线"
        },
        silent_best
    );
    if let Some(c) = cpu
        .iter()
        .find(|c| c.frame_ms == best_frame && c.complexity == 5)
    {
        println!(
            "    CPU {:.0}%：编解码单路 {:.2}%，三路 {:.2}%（complexity 5，还没算 APM）",
            redline::CPU_PCT,
            c.cpu_pct_1_stream,
            c.cpu_pct_3_streams
        );
    }
    println!(
        "    内存 {:.0} MB / 安装包 {:.0} MB：M1 测不了，要等 voice-core 独立进程和 Tauri 打包",
        redline::VOICE_CORE_RSS_MB,
        redline::INSTALLER_MB
    );

    println!();
    println!("  尚未测量、不能算数的部分：");
    println!("    - WASAPI 采集 + 渲染延迟（M2，这是 80 ms 里最后一块拼图）");
    println!("    - APM（AEC3/降噪/AGC）的延迟与 CPU（M2）");
    println!("    - 真实公网的抖动分布 —— 这里用的是高斯，真网络是重尾的");
    println!("    - 多人同时说话时服务端转发的扇出开销");
    println!("    - 下行带宽：K 个人同时说话就是 K 倍，红线只管住了单条上行");
}

pub fn json(runs: &[RunResult], bw: &[BandwidthRow], cpu: &[CpuCostRow]) -> String {
    let mut s = String::from("{\n  \"runs\": [\n");
    for (i, r) in runs.iter().enumerate() {
        s.push_str(&format!(
            "    {{\"profile\":\"{}\",\"frame_ms\":{},\"target_frames\":{},\"bitrate\":{},\
\"wall_p50_ms\":{:.3},\"wall_p95_ms\":{:.3},\"wall_p99_ms\":{:.3},\
\"mouth_to_ear_p95_ms\":{:.3},\"signal_delay_ms\":{},\"opus_lookahead_ms\":{:.3},\
\"encode_p95_us\":{:.1},\"decode_p95_us\":{:.1},\"played\":{},\"lost\":{},\"late\":{},\
\"underruns\":{},\"speaking_kbps\":{:.2},\"codec_cpu_pct\":{:.3},\"peak_rss_bytes\":{},\
\"tick_misses\":{},\"quality_ok\":{}}}{}\n",
            r.cfg.profile,
            r.cfg.frame_ms,
            r.cfg.target_frames,
            r.cfg.bitrate,
            r.wall_ms.p50,
            r.wall_ms.p95,
            r.wall_ms.p99,
            r.mouth_to_ear_p95_ms,
            r.signal_delay_ms
                .map(|v| format!("{v:.3}"))
                .unwrap_or_else(|| "null".into()),
            r.opus_lookahead_ms,
            r.encode_us.p95,
            r.decode_us.p95,
            r.jitter.played,
            r.jitter.lost,
            r.jitter.late,
            r.jitter.underruns,
            r.speaking_kbps,
            r.codec_cpu_pct,
            r.peak_rss_bytes,
            r.capture_late_ticks + r.playout_late_ticks,
            quality_ok(r),
            if i + 1 == runs.len() { "" } else { "," }
        ));
    }
    s.push_str("  ],\n  \"bandwidth\": [\n");
    for (i, r) in bw.iter().enumerate() {
        s.push_str(&format!(
            "    {{\"frame_ms\":{},\"bitrate\":{},\"pps\":{:.1},\"fixed_overhead_bytes\":{},\
\"overhead_kbps\":{:.2},\"speaking_kbps\":{:.2},\"silent_keep_sending_kbps\":{:.2},\
\"silent_drop_dtx_kbps\":{:.2},\"dtx_hit_rate\":{:.3}}}{}\n",
            r.frame_ms,
            r.bitrate,
            r.pps,
            r.fixed_overhead_bytes,
            r.overhead_kbps,
            r.speaking_kbps,
            r.silent_keep_sending_kbps,
            r.silent_drop_dtx_kbps,
            r.dtx_hit_rate,
            if i + 1 == bw.len() { "" } else { "," }
        ));
    }
    s.push_str("  ],\n  \"cpu\": [\n");
    for (i, r) in cpu.iter().enumerate() {
        s.push_str(&format!(
            "    {{\"frame_ms\":{},\"complexity\":{},\"encode_us_p50\":{:.1},\"encode_us_p95\":{:.1},\
\"decode_us_p50\":{:.1},\"cpu_pct_1_stream\":{:.3},\"cpu_pct_3_streams\":{:.3}}}{}\n",
            r.frame_ms,
            r.complexity,
            r.encode_us_p50,
            r.encode_us_p95,
            r.decode_us_p50,
            r.cpu_pct_1_stream,
            r.cpu_pct_3_streams,
            if i + 1 == cpu.len() { "" } else { "," }
        ));
    }
    s.push_str("  ]\n}\n");
    s
}

fn budget_line(label: &str, value: String) {
    println!("    {label:<30} {value:>8}");
}

fn fmt(v: f64) -> String {
    format!("{v:.2}")
}

fn onoff(v: bool) -> &'static str {
    if v {
        "开"
    } else {
        "关"
    }
}
