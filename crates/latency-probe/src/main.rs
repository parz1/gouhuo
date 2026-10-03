// SPDX-License-Identifier: GPL-3.0-or-later

//! M1 go/no-go 探针。
//!
//! 一句话：**先别碰 UI，先把一个数字测出来。**
//!
//! 走完整的「编码 → UDP → 服务端转发 → 抖动缓冲 → 解码」链路，
//! 喂合成信号打时间戳，测纯协议延迟。不接音频设备 —— 拿到的是不掺设备延迟的
//! 干净数字，也就是这套架构的物理下限。
//!
//! 如果下限就跑不进预算，先解决这个，不要往下做任何 UI 或功能。

mod bandwidth;
mod cpucost;
use latency_probe::netem;
mod relay;
mod report;
mod run;
mod sim;

use std::process::ExitCode;

use netem::NetemConfig;
use run::{RunCfg, RunResult};
use voice_core::clock::TimerResolutionGuard;

struct Profile {
    name: &'static str,
    owd_ms: f64,
    jitter_ms: f64,
    loss: f64,
    desc: &'static str,
}

/// 网络档位。数字是规划用的典型值，不是从某次抓包来的 ——
/// 真实分布是重尾的，等有了服务端和真用户再用实测替换。
const PROFILES: &[Profile] = &[
    Profile {
        name: "floor",
        owd_ms: 0.0,
        jitter_ms: 0.0,
        loss: 0.0,
        desc: "完美网络，架构物理下限",
    },
    Profile {
        name: "lan",
        owd_ms: 1.0,
        jitter_ms: 0.3,
        loss: 0.0,
        desc: "同一局域网",
    },
    Profile {
        name: "city",
        owd_ms: 12.0,
        jitter_ms: 4.0,
        loss: 0.005,
        desc: "同城，经服务端两跳",
    },
    Profile {
        name: "bad",
        owd_ms: 20.0,
        jitter_ms: 15.0,
        loss: 0.03,
        desc: "同城但网差 / 家用 WiFi",
    },
    Profile {
        name: "region",
        owd_ms: 35.0,
        jitter_ms: 10.0,
        loss: 0.01,
        desc: "跨省",
    },
];

/// 默认推荐配置，也是 verdict 判定用的那一行。
const DEFAULT_PROFILE: &str = "city";
const DEFAULT_FRAME_MS: u32 = 20;
const DEFAULT_TARGET: usize = 2;

struct Args {
    seconds: f64,
    frame_ms: u32,
    target: usize,
    profile: String,
    bitrate: i32,
    complexity: i32,
    seed: u64,
    fec: bool,
    dtx: bool,
    crypto: bool,
    relay: bool,
    single: bool,
    json: bool,
    assert_p95_ms: Option<f64>,
    adaptive: bool,
}

impl Default for Args {
    fn default() -> Self {
        Self {
            seconds: 6.0,
            frame_ms: DEFAULT_FRAME_MS,
            target: DEFAULT_TARGET,
            profile: DEFAULT_PROFILE.to_string(),
            bitrate: 24_000,
            complexity: 5,
            seed: 0x1234_5678,
            fec: true,
            dtx: true,
            crypto: true,
            relay: true,
            single: false,
            json: false,
            assert_p95_ms: None,
            adaptive: false,
        }
    }
}

fn main() -> ExitCode {
    // `latency-probe sim [秒数]`：固定 vs 自适应抖动缓冲，离线跑，见 sim.rs。
    let mut raw = std::env::args().skip(1);
    if raw.next().as_deref() == Some("sim") {
        let seconds = raw.next().and_then(|s| s.parse().ok());
        sim::main(seconds);
        return ExitCode::SUCCESS;
    }

    let args = match parse_args() {
        Ok(Some(a)) => a,
        Ok(None) => return ExitCode::SUCCESS, // --help
        Err(e) => {
            eprintln!("参数错误：{e}");
            eprintln!("用 --help 看用法。");
            return ExitCode::from(2);
        }
    };

    // 整个进程持有 1 ms 定时器精度。没有它，20 ms 的节拍在 Windows 上会变成
    // 31 ms，测出来的全是调度器的锅，跟架构无关。
    let timer = TimerResolutionGuard::acquire();
    if cfg!(windows) && !timer.is_active() {
        eprintln!("警告：timeBeginPeriod(1) 没拿到，节拍会不准，这次的数字别当真。");
    }

    let configs = if args.single {
        vec![single_cfg(&args)]
    } else {
        matrix_cfgs(&args)
    };

    let mut runs: Vec<RunResult> = Vec::with_capacity(configs.len());
    if !args.json && !args.single {
        report::matrix_header();
    }
    for (i, cfg) in configs.iter().enumerate() {
        if args.json {
            eprintln!(
                "[{}/{}] {} {} ms {} 帧…",
                i + 1,
                configs.len(),
                cfg.profile,
                cfg.frame_ms,
                cfg.target_frames
            );
        }
        match run::run(cfg) {
            Ok(r) => {
                if !args.json && !args.single {
                    report::matrix_row(&r);
                }
                runs.push(r);
            }
            Err(e) => {
                eprintln!("运行失败（{} {} ms）：{e}", cfg.profile, cfg.frame_ms);
                return ExitCode::FAILURE;
            }
        }
    }
    if !args.json && !args.single {
        report::matrix_legend();
    }

    let bw_rows = measure_bandwidth(&args);
    let cpu_rows = measure_cpu_cost(&args);

    // 详细分解给哪一行：优先给判定选出来的那个配置；没有能用的配置时，
    // 退回默认配置，好让人看见「差在哪」。
    let detail_target = report::pick_best(&runs)
        .or_else(|| {
            runs.iter().find(|r| {
                r.cfg.profile == DEFAULT_PROFILE
                    && r.cfg.frame_ms == DEFAULT_FRAME_MS
                    && r.cfg.target_frames == DEFAULT_TARGET
            })
        })
        .or_else(|| runs.last())
        .expect("at least one run");

    if args.json {
        print!("{}", report::json(&runs, &bw_rows, &cpu_rows));
    } else {
        report::detail(detail_target);
        report::bandwidth_table(&bw_rows);
        report::cpu_table(&cpu_rows);
        // --single 只跑一个配置，凑不齐判定需要的那张矩阵，给结论是误导。
        if !args.single {
            report::verdict(&runs, &bw_rows, &cpu_rows);
        }
    }

    // CI 门禁。判据是「判定档里**存在**一个音质过关、且协议延迟在 limit 以内的配置」——
    // 不是「所有配置都达标」。矩阵里本来就故意含了明显不该用的配置（比如缓冲 3 帧
    // 的 20 ms），拿最差那行当门禁只会逼着把矩阵改窄，那就失去测的意义了。
    if let Some(limit) = args.assert_p95_ms {
        match report::pick_best(&runs).filter(|r| r.mouth_to_ear_p95_ms <= limit) {
            Some(r) => eprintln!(
                "红线通过：{} 档下 {} ms 帧 / 缓冲 {} 帧 达标，m2e-p95 = {:.1} ms <= {:.1} ms",
                report::VERDICT_PROFILE,
                r.cfg.frame_ms,
                r.cfg.target_frames,
                r.mouth_to_ear_p95_ms,
                limit
            ),
            None => {
                let closest = runs
                    .iter()
                    .filter(|r| r.cfg.profile == report::VERDICT_PROFILE && report::quality_ok(r))
                    .map(|r| r.mouth_to_ear_p95_ms)
                    .fold(f64::INFINITY, f64::min);
                eprintln!(
                    "红线失败：{} 档下没有音质过关且 m2e-p95 <= {:.1} ms 的配置（最好的一个是 {:.1} ms）",
                    report::VERDICT_PROFILE, limit, closest
                );
                return ExitCode::FAILURE;
            }
        }
    }

    ExitCode::SUCCESS
}

fn single_cfg(args: &Args) -> RunCfg {
    let p = PROFILES
        .iter()
        .find(|p| p.name == args.profile)
        .unwrap_or_else(|| panic!("unknown profile {}", args.profile));
    RunCfg {
        profile: p.name.to_string(),
        frame_ms: args.frame_ms,
        seconds: args.seconds,
        bitrate: args.bitrate,
        complexity: args.complexity,
        fec: args.fec,
        dtx: args.dtx,
        crypto: args.crypto,
        relay: args.relay,
        target_frames: args.target,
        adaptive: args.adaptive,
        net: NetemConfig {
            owd_ms: p.owd_ms,
            jitter_ms: p.jitter_ms,
            loss: p.loss,
            seed: args.seed,
        },
    }
}

/// 默认矩阵：四档网络 × 两种帧长 × 三种缓冲深度。
///
/// 为什么要整个矩阵而不是一个数：单看一个配置没法回答「跑不进红线的时候
/// 该动哪个旋钮」。这张表把三个旋钮各自值多少毫秒直接摆出来。
fn matrix_cfgs(args: &Args) -> Vec<RunCfg> {
    let mut out = Vec::new();
    for p in PROFILES.iter().filter(|p| p.name != "region") {
        for &frame_ms in &[10u32, 20] {
            for &target in &[1usize, 2, 3] {
                out.push(RunCfg {
                    profile: p.name.to_string(),
                    frame_ms,
                    seconds: args.seconds,
                    bitrate: args.bitrate,
                    complexity: args.complexity,
                    fec: args.fec,
                    dtx: args.dtx,
                    crypto: args.crypto,
                    relay: args.relay,
                    target_frames: target,
                    adaptive: false,
                    net: NetemConfig {
                        owd_ms: p.owd_ms,
                        jitter_ms: p.jitter_ms,
                        loss: p.loss,
                        seed: args.seed,
                    },
                });
            }
        }
    }
    out
}

/// complexity 是 CPU 红线上最大的那个旋钮，所以把它扫出来而不是只测一个点。
fn measure_cpu_cost(args: &Args) -> Vec<cpucost::CpuCostRow> {
    let mut rows = Vec::new();
    for &frame_ms in &[10u32, 20] {
        for &complexity in &[0i32, 3, 5, 8, 10] {
            rows.push(cpucost::measure(
                frame_ms,
                args.bitrate,
                complexity,
                args.fec,
            ));
        }
    }
    rows
}

fn measure_bandwidth(args: &Args) -> Vec<bandwidth::BandwidthRow> {
    let mut rows = Vec::new();
    for &frame_ms in &[10u32, 20] {
        for &bitrate in &[16_000i32, 24_000, 32_000] {
            rows.push(bandwidth::measure(
                frame_ms,
                bitrate,
                args.complexity,
                args.fec,
                args.crypto,
            ));
        }
    }
    rows
}

fn parse_args() -> Result<Option<Args>, String> {
    let mut a = Args::default();
    let mut it = std::env::args().skip(1);
    while let Some(arg) = it.next() {
        let mut next = |name: &str| -> Result<String, String> {
            it.next().ok_or_else(|| format!("{name} 后面少了值"))
        };
        match arg.as_str() {
            "--help" | "-h" => {
                print_help();
                return Ok(None);
            }
            "--seconds" => {
                a.seconds = next("--seconds")?
                    .parse()
                    .map_err(|_| "--seconds 要是数字")?
            }
            "--frame-ms" => {
                a.frame_ms = next("--frame-ms")?
                    .parse()
                    .map_err(|_| "--frame-ms 要是整数")?
            }
            "--target" => a.target = next("--target")?.parse().map_err(|_| "--target 要是整数")?,
            "--profile" => a.profile = next("--profile")?,
            "--bitrate" => {
                a.bitrate = next("--bitrate")?
                    .parse()
                    .map_err(|_| "--bitrate 要是整数")?
            }
            "--complexity" => {
                a.complexity = next("--complexity")?
                    .parse()
                    .map_err(|_| "--complexity 要是整数")?
            }
            "--seed" => a.seed = next("--seed")?.parse().map_err(|_| "--seed 要是整数")?,
            // 用 redline.rs 里的闸，不在命令行上重复一遍数字 ——
            // 两处各写一份迟早会漂移，而漂移的那天 CI 是绿的。
            "--assert-gate" => a.assert_p95_ms = Some(voice_core::redline::GATE_PROTOCOL_MS),
            "--assert-p95-ms" => {
                a.assert_p95_ms = Some(
                    next("--assert-p95-ms")?
                        .parse()
                        .map_err(|_| "--assert-p95-ms 要是数字")?,
                )
            }
            "--no-fec" => a.fec = false,
            "--no-dtx" => a.dtx = false,
            "--no-crypto" => a.crypto = false,
            "--no-relay" => a.relay = false,
            "--single" => a.single = true,
            "--json" => a.json = true,
            "--adaptive" => a.adaptive = true,
            other => return Err(format!("不认识的参数 {other}")),
        }
    }
    if !PROFILES.iter().any(|p| p.name == a.profile) {
        return Err(format!("不认识的 profile {}", a.profile));
    }
    if a.frame_ms == 0 || 1000 % a.frame_ms != 0 {
        return Err("--frame-ms 要能整除 1000（Opus 支持 2.5/5/10/20/40/60）".into());
    }
    if a.seconds < 1.0 {
        return Err("--seconds 太短，测不出分位数".into());
    }
    Ok(Some(a))
}

fn print_help() {
    println!("M1 延迟探针 —— 测「编码 → UDP → 转发 → 抖动缓冲 → 解码」的纯协议延迟");
    println!();
    println!("默认跑完整矩阵（四档网络 × 10/20 ms 帧 × 缓冲 1/2/3 帧）再给结论。");
    println!();
    println!("  --single              只跑一个配置，出详细分解");
    println!("  --profile NAME        网络档位，默认 {DEFAULT_PROFILE}");
    println!("  --frame-ms N          帧长，默认 {DEFAULT_FRAME_MS}");
    println!("  --target N            抖动缓冲目标深度（帧），默认 {DEFAULT_TARGET}");
    println!("  --seconds F           每个配置跑多久，默认 6");
    println!("  --bitrate N           Opus 码率 bps，默认 24000");
    println!("  --complexity N        Opus complexity 0-10，默认 5");
    println!("  --seed N              网络仿真随机种子，用来精确复现");
    println!("  --no-fec / --no-dtx   关掉 in-band FEC / DTX");
    println!("  --no-crypto           不加密（看 AEAD 的开销有多大）");
    println!("  --no-relay            客户端直连，不走服务端转发那一跳");
    println!("  --json                输出 JSON，给 CI 用");
    println!("  --adaptive            用自适应抖动缓冲（线上那一份），配合 --single 用");
    println!();
    println!("  sim [秒数]            固定 vs 自适应缓冲，离线模拟一小时（或给定秒数），几秒跑完");
    println!(
        "  --assert-gate         按 redline::GATE_PROTOCOL_MS（当前 {:.0} ms）卡防回归闸，CI 用这个",
        voice_core::redline::GATE_PROTOCOL_MS
    );
    println!("  --assert-p95-ms F     同上但自己指定毫秒数");
    println!();
    println!("网络档位：");
    for p in PROFILES {
        println!(
            "  {:<8} 单程 {:>4.1} ms, 抖动 σ={:>4.1} ms, 丢包 {:>4.1}%   {}",
            p.name,
            p.owd_ms,
            p.jitter_ms,
            p.loss * 100.0,
            p.desc
        );
    }
}
