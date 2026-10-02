// SPDX-License-Identifier: MPL-2.0

//! 端到端语音：一个人说话，另一个人听见，**并且量出来花了多久**。
//!
//! 走的是完整链路 —— 真 TLS 握手派生的密钥、真 Opus、真加密、真 UDP、
//! 真服务端转发、真抖动缓冲。只有两头的声卡是合成的。
//!
//! # 这是第一次真正量端到端
//!
//! M1 量的是协议链路（46.5 ms），M2 量的是设备（30.4 ms）和 APM（15.0 ms），
//! 加起来 91.9 ms。但那三段**从没作为一条链路跑过** —— 加法能对，接线可能错：
//! 少一次拷贝、多一层缓冲、线程调度慢了一拍，加法都看不出来。
//!
//! 这里喂一个啁啾进去，从另一头把它捞出来做互相关，得到的是**实测**的延迟。
//!
//! # 这个数字不包括什么
//!
//! **不含声卡。** 合成设备一到节拍就把整帧交出去，真麦克风要先攒满 10 ms
//! 才有一帧，真扬声器那边还有驱动和硬件的缓冲 —— 这部分 M2 单独量过 30.4 ms。
//!
//! **不含 APM。** 那一段 M2 量过 15.0 ms。
//!
//! **网络是本机回环。** 真网络的往返要另外加。
//!
//! 所以这里量到的是「**我们自己写的那一段**」：分帧、编码器前瞻、加密、
//! 转发、抖动缓冲、解码、混音。它也正是唯一由我们的代码决定的部分。

use std::net::{TcpListener, UdpSocket};
use std::sync::Arc;
use std::time::{Duration, Instant};

use client_core::Client;
use protocol::Invite;
use server::conn::Hub;
use server::state::{Config, Server};
use transport::{server_config, ServerCert};
use voice_core::audio::{
    Capture, CollectingRender, NullRender, Render, SyntheticCapture, FRAME_SAMPLES, SAMPLE_RATE,
};
use voice_core::identity::Identity;
use voice_core::pipeline::{default_jitter, Pipeline, PipelineConfig, PipelineState, TransmitMode};

#[path = "support/measured_audio.rs"]
mod measured_audio;

/// 啁啾前面留多少帧静音。
///
/// 给链路时间把自己撑起来：抖动缓冲要预填，Opus 编码器前几帧在爬坡，
/// 服务端还要先从保活包里学到我们的 UDP 地址。
const LEAD_FRAMES: usize = 60;

/// 啁啾本身多长。
const CHIRP_FRAMES: usize = 40;

/// 啁啾之后再跑多久，好让它整个被推出来。
const TAIL_FRAMES: usize = 60;

struct TestServer {
    invite: Invite,
    #[allow(dead_code)]
    hub: Arc<Hub>,
}

fn start_server() -> TestServer {
    let cert = ServerCert::generate().unwrap();
    let fingerprint = cert.fingerprint();
    let tls = Arc::new(server_config(&cert).unwrap());
    let listener = TcpListener::bind("127.0.0.1:0").unwrap();
    let addr = listener.local_addr().unwrap();
    let voice = UdpSocket::bind("127.0.0.1:0").unwrap();

    let hub = Arc::new(Hub::new(Server::new(Config::default()), voice));
    let voice_hub = Arc::clone(&hub);
    std::thread::spawn(move || voice_hub.run_voice());
    let accept_hub = Arc::clone(&hub);
    std::thread::spawn(move || server::accept_loop(listener, tls, accept_hub));

    TestServer {
        invite: Invite {
            host: addr.ip().to_string(),
            port: addr.port(),
            cert: fingerprint,
            code: None,
        },
        hub,
    }
}

fn join(server: &TestServer, name: &str) -> Client {
    let link = server.invite.to_url().unwrap();
    Client::connect(&link, &Identity::generate().unwrap(), name)
        .expect("连不上")
        .0
}

fn voice_config(client: &Client, server: &TestServer, mode: TransmitMode) -> PipelineConfig {
    let (session_id, udp_port, keys) = client.voice_session();
    let addr = format!("{}:{}", server.invite.host, udp_port)
        .parse()
        .expect("服务端给的语音地址解析不了");
    PipelineConfig {
        session_id,
        server: addr,
        sequences: Arc::clone(&keys.sequences),
        upstream_key: *keys.upstream.as_bytes(),
        downstream_key: *keys.downstream.as_bytes(),
        jitter: default_jitter(),
        mode,
    }
}

/// 啁啾，f32。
fn chirp_f32(samples: usize) -> Vec<f32> {
    voice_core::signal::chirp(samples, SAMPLE_RATE, 300.0, 3400.0, 0.5)
        .into_iter()
        .map(|s| s as f32 / i16::MAX as f32)
        .collect()
}

fn to_i16(samples: &[f32]) -> Vec<i16> {
    samples
        .iter()
        .map(|&s| (s.clamp(-1.0, 1.0) * i16::MAX as f32) as i16)
        .collect()
}

/// 甲说话，乙听见，而且能量对得上。
///
/// 这是「跑通了」的定义。延迟是多少是下一条测试的事。
#[test]
fn a_chirp_makes_it_all_the_way_through() {
    let server = start_server();
    let alice = join(&server, "阿狸");
    let bob = join(&server, "波波");

    let mut source = vec![0.0f32; FRAME_SAMPLES * LEAD_FRAMES];
    source.extend_from_slice(&chirp_f32(FRAME_SAMPLES * CHIRP_FRAMES));
    source.extend(std::iter::repeat(0.0).take(FRAME_SAMPLES * TAIL_FRAMES));

    let capture = SyntheticCapture::new(source.clone()).then_silence();
    let alice_voice = Pipeline::start(
        voice_config(&alice, &server, TransmitMode::Always),
        Box::new(capture),
        Box::new(NullRender::default()),
        None,
    )
    .unwrap();

    let (render, played) = CollectingRender::new();
    // 波波只听不说。他必须照样能听见 —— 服务端是从保活包里学到他的地址的。
    let silent = SyntheticCapture::new(Vec::new()).then_silence();
    let bob_voice = Pipeline::start(
        voice_config(&bob, &server, TransmitMode::PushToTalk),
        Box::new(silent),
        Box::new(render),
        None,
    )
    .unwrap();

    let total_frames = LEAD_FRAMES + CHIRP_FRAMES + TAIL_FRAMES;
    std::thread::sleep(Duration::from_millis((total_frames * 10 + 500) as u64));

    let stats = bob_voice.stats();
    assert!(stats.udp_ok, "波波那边 UDP 没通 —— 保活没回来");
    assert!(stats.packets_received > 100, "波波几乎没收到包：{stats:?}");
    assert!(
        alice_voice.stats().packets_sent > 100,
        "阿狸几乎没发出去包：{:?}",
        alice_voice.stats()
    );

    let played = played.lock().unwrap().clone();
    let energy: f32 = played.iter().map(|s| s * s).sum();
    assert!(energy > 0.0, "波波那边一点声音都没有");

    // 啁啾的能量应该集中在中段，而不是均匀铺满 —— 后者说明我们捞到的是噪声
    let third = played.len() / 3;
    let mid: f32 = played[third..2 * third].iter().map(|s| s * s).sum();
    assert!(
        mid > energy * 0.5,
        "能量没集中在中段，捞到的可能不是那个啁啾"
    );
}

/// 对方还没开口就把他调成 0，开口时就该听不见。
///
/// 回归：原来音量存在「说话人」对象身上，而那个对象收到第一个包才建 ——
/// 开口前调的音量直接被丢掉，而且一个人安静 30 秒被清掉、再开口时也会
/// 悄悄回到 100%。界面上的音量是按人存的，进频道时就设好，两种情况都会踩到。
#[test]
fn a_volume_set_before_anyone_speaks_still_applies() {
    let server = start_server();
    let alice = join(&server, "阿狸");
    let bob = join(&server, "波波");

    let mut source = vec![0.0f32; FRAME_SAMPLES * LEAD_FRAMES];
    source.extend_from_slice(&chirp_f32(FRAME_SAMPLES * CHIRP_FRAMES));
    source.extend(std::iter::repeat(0.0).take(FRAME_SAMPLES * TAIL_FRAMES));

    let (render, played) = CollectingRender::new();
    let silent = SyntheticCapture::new(Vec::new()).then_silence();
    let bob_voice = Pipeline::start(
        voice_config(&bob, &server, TransmitMode::PushToTalk),
        Box::new(silent),
        Box::new(render),
        None,
    )
    .unwrap();
    // 阿狸一个包都还没发，波波就先把她静音了
    bob_voice.set_volume(alice.session_id(), 0.0);

    let _alice_voice = Pipeline::start(
        voice_config(&alice, &server, TransmitMode::Always),
        Box::new(SyntheticCapture::new(source).then_silence()),
        Box::new(NullRender::default()),
        None,
    )
    .unwrap();

    let total_frames = LEAD_FRAMES + CHIRP_FRAMES + TAIL_FRAMES;
    std::thread::sleep(Duration::from_millis((total_frames * 10 + 500) as u64));

    let stats = bob_voice.stats();
    assert!(
        stats.packets_received > 100,
        "波波没收到包，这条测试就什么都没证明：{stats:?}"
    );
    let played = played.lock().unwrap().clone();
    let energy: f32 = played.iter().map(|s| s * s).sum();
    assert_eq!(energy, 0.0, "调成 0 的人还是听得见");
}

/// 提示音从播放那一路出来 —— 跟别人的声音混在一起，这样 APM 才拿得到它
/// 当参考信号（见 `voice_core::cue` 的文档）。
#[test]
fn a_cue_comes_out_of_the_speakers() {
    use voice_core::cue::{chime, Chime};

    let server = start_server();
    let bob = join(&server, "波波");
    let (render, played) = CollectingRender::new();
    let bob_voice = Pipeline::start(
        voice_config(&bob, &server, TransmitMode::PushToTalk),
        Box::new(SyntheticCapture::new(Vec::new()).then_silence()),
        Box::new(render),
        None,
    )
    .unwrap();

    let sound = chime(Chime::CameIn);
    assert!(bob_voice.cues().push(&sound, 1.0));
    std::thread::sleep(Duration::from_millis(600));

    let played = played.lock().unwrap().clone();
    let expected: f32 = sound.iter().map(|s| s * s).sum();
    let energy: f32 = played.iter().map(|s| s * s).sum();
    assert!(
        (energy - expected).abs() < expected * 0.01,
        "放出来的能量 {energy}，提示音本身 {expected}"
    );
    assert_eq!(bob_voice.cues().pending(), 0, "放完了队列该是空的");
}

/// **量端到端延迟。** 见模块文档：这个数字不含声卡和 APM。
#[test]
fn end_to_end_latency_is_measured_not_added_up() {
    use measured_audio::{MeasuredCapture, MeasuredRender};

    let server = start_server();
    let alice = join(&server, "阿狸");
    let bob = join(&server, "波波");

    let mut source = vec![0.0f32; FRAME_SAMPLES * LEAD_FRAMES];
    source.extend_from_slice(&chirp_f32(FRAME_SAMPLES * CHIRP_FRAMES));
    source.extend(std::iter::repeat(0.0).take(FRAME_SAMPLES * TAIL_FRAMES));

    let (render, rendered) = MeasuredRender::new();
    let silent = SyntheticCapture::new(Vec::new()).then_silence();
    let bob_voice = Pipeline::start(
        voice_config(&bob, &server, TransmitMode::PushToTalk),
        Box::new(silent),
        Box::new(render),
        None,
    )
    .unwrap();

    // 互相关只搜索非负采样滞后，因此接收端的采样原点必须先于发送端。
    // 先等到真实第一帧播放完成，避免并行测试下线程启动顺序反转原点。
    let render_deadline = Instant::now() + Duration::from_secs(5);
    while rendered.lock().unwrap().frames.is_empty() {
        assert!(
            Instant::now() < render_deadline,
            "接收端播放未在期限内就绪：{:?}",
            bob_voice.stats()
        );
        std::thread::sleep(Duration::from_millis(5));
    }

    let (capture, captured) = MeasuredCapture::new(source.clone());
    let alice_voice = Pipeline::start(
        voice_config(&alice, &server, TransmitMode::Always),
        Box::new(capture),
        Box::new(NullRender::default()),
        None,
    )
    .unwrap();

    // 等实际源帧与播放尾巴到齐，不能用墙钟 sleep 推定线程按时跑完了。
    let source_deadline = Instant::now() + Duration::from_secs(5);
    loop {
        let source_end = captured.lock().unwrap().sample_time(source.len() - 1);
        let played_until = rendered.lock().unwrap().frames.last().map(|f| f.at);
        if source_end.is_some_and(|end| {
            played_until.is_some_and(|at| at >= end + Duration::from_millis(500))
        }) {
            break;
        }
        if Instant::now() >= source_deadline {
            let captured = captured.lock().unwrap().clone();
            let rendered = rendered.lock().unwrap().clone();
            let origin = captured
                .frames
                .iter()
                .chain(&rendered.frames)
                .map(|f| f.at)
                .min()
                .unwrap_or_else(Instant::now);
            panic!(
                "信号未在期限内完成；发送端 {:?}，接收端 {:?}\n采集 {}\n播放 {}",
                alice_voice.stats(),
                bob_voice.stats(),
                captured.timing_report(origin),
                rendered.timing_report(origin)
            );
        }
        std::thread::sleep(Duration::from_millis(5));
    }

    let alice_stats = alice_voice.stats();
    let bob_stats = bob_voice.stats();
    // 重的相关计算发生在测量结束之后，不与仍在跑的音频线程抢 CPU。
    drop(alice_voice);
    drop(bob_voice);
    let captured = captured.lock().unwrap().clone();
    let rendered = rendered.lock().unwrap().clone();
    let cap_t0 = captured.frames.first().expect("采集没起来").at;
    let play_t0 = rendered.frames.first().expect("播放没起来").at;
    let origin = cap_t0.min(play_t0);
    let diagnostic = || {
        format!(
            "初始 muted=false/deafened=false；发送 Always，接收 PTT/released；\
             发送端 {alice_stats:?}，接收端 {bob_stats:?}\n采集 {}\n播放 {}",
            captured.timing_report(origin),
            rendered.timing_report(origin)
        )
    };
    assert!(
        play_t0 <= cap_t0,
        "接收端采样原点必须先于发送端；{}",
        diagnostic()
    );
    assert!(
        alice_stats.udp_ok
            && bob_stats.udp_ok
            && alice_stats.packets_sent > 100
            && bob_stats.packets_received > 100,
        "真实 UDP 链路没有完成信号传输；{}",
        diagnostic()
    );

    // ---- 方法一：互相关 ----
    //
    // 拿啁啾中段当参考（避开起播和收尾），在播出来的采样点里找它。
    let reference = to_i16(&source);
    let observed = to_i16(&rendered.samples);
    let ref_start = FRAME_SAMPLES * (LEAD_FRAMES + CHIRP_FRAMES / 4);
    let ref_len = FRAME_SAMPLES * (CHIRP_FRAMES / 2);
    // 搜索的仍是物理 0..300 ms；跳拍后两端样本序号不能当作同一把时钟。
    let in_at = captured
        .sample_time(ref_start)
        .unwrap_or_else(|| panic!("参考帧没有采集时间；{}", diagnostic()));
    let candidates = rendered
        .candidate_window(in_at, Duration::from_millis(300), ref_len)
        .unwrap_or_else(|| panic!("没有完整的物理延迟搜索窗口；{}", diagnostic()));
    let observed_start = *candidates.start();
    let max_lag = *candidates.end() - observed_start;
    let estimate = voice_core::signal::best_lag(
        &reference[ref_start..ref_start + ref_len],
        &observed[observed_start..],
        0,
        ref_len,
        max_lag,
    )
    .unwrap_or_else(|| panic!("互相关没跑起来：播出来的采样点不够长；{}", diagnostic()));
    assert!(
        estimate.peak > 0.3,
        "相关峰值只有 {:.2}，捞到的多半不是那个啁啾；参考位置 {ref_start}，\
         搜索位置 {candidates:?}，{}",
        estimate.peak,
        diagnostic()
    );
    assert!(
        !estimate.at_boundary,
        "峰值落在搜索范围边界上，结果不可信；{}",
        diagnostic()
    );

    let observed_sample = observed_start + estimate.lag;
    let out_at = rendered
        .sample_time(observed_sample)
        .unwrap_or_else(|| panic!("匹配帧没有播放时间；{}", diagnostic()));
    assert!(
        out_at >= in_at,
        "匹配声音早于采集，测量不可信；{}",
        diagnostic()
    );
    let measured_ms = out_at.duration_since(in_at).as_secs_f64() * 1000.0;

    // ---- 方法二：理论下限 ----
    //
    // 分帧 10 ms + 编码器前瞻 + 抖动缓冲。实测必须**大于等于**它 ——
    // 小于就说明有一段根本没生效（比如抖动缓冲被绕过去了）。
    // 自适应缓冲最浅是一帧（本机回环上没有抖动，它就停在最浅）。
    let floor = voice_core::pipeline::intrinsic_latency_ms(1);

    println!("端到端（不含声卡和 APM）：{measured_ms:.1} ms");
    println!("理论下限：{floor:.1} ms");
    println!("相关峰值：{:.3}", estimate.peak);
    println!(
        "匹配样本：capture[{ref_start}] -> render[{observed_sample}]；\
         采集 late/skip={}/{}，播放 late/skip={}/{}",
        captured.frames.last().unwrap().late_ticks,
        captured.frames.last().unwrap().skipped_ticks,
        rendered.frames.last().unwrap().late_ticks,
        rendered.frames.last().unwrap().skipped_ticks,
    );

    assert!(
        measured_ms >= floor - 2.0,
        "实测 {measured_ms:.1} ms 比理论下限 {floor:.1} ms 还小 —— \
         多半是某一段没真的生效；{}",
        diagnostic()
    );
    // 上限放得很宽：这是「跑通」的第一版，固定抖动缓冲，而且测试机器上
    // 还跑着服务端和两条链路。收紧要等自适应缓冲那一步。
    assert!(
        measured_ms < 150.0,
        "实测 {measured_ms:.1} ms，比理论下限 {floor:.1} ms 大太多了；{}",
        diagnostic()
    );
}

/// 松开说话键之后就不该再发包了。
///
/// 「常驻几小时不打扰人」里，静默带宽是实打实的一条 —— 一屋子人里
/// 绝大多数时间没人说话。
#[test]
fn silence_costs_almost_nothing() {
    let server = start_server();
    let alice = join(&server, "阿狸");

    // 有信号，但按键没按下 —— PTT 模式下一个语音包都不该发
    let source = chirp_f32(FRAME_SAMPLES * 100);
    let capture = SyntheticCapture::new(source).then_silence();
    let voice = Pipeline::start(
        voice_config(&alice, &server, TransmitMode::PushToTalk),
        Box::new(capture),
        Box::new(NullRender::default()),
        None,
    )
    .unwrap();

    std::thread::sleep(Duration::from_millis(1200));
    let quiet = voice.stats();
    assert_eq!(quiet.packets_sent, 0, "没按键却在发语音：{quiet:?}");
    assert!(quiet.udp_ok, "保活没通 —— 那服务端根本不知道往哪儿发给我们");

    // 按下去就该有了
    voice.set_transmitting(true);
    std::thread::sleep(Duration::from_millis(500));
    let talking = voice.stats();
    assert!(talking.packets_sent > 20, "按了键却没发包：{talking:?}");
}

/// VAD：有人声就发，静下来就停。
#[test]
fn voice_activity_follows_the_signal() {
    let server = start_server();
    let alice = join(&server, "阿狸");

    // 前 50 帧静音，中间 50 帧有声音，后面又静音
    let mut source = vec![0.0f32; FRAME_SAMPLES * 50];
    source.extend_from_slice(&chirp_f32(FRAME_SAMPLES * 50));
    source.extend(std::iter::repeat(0.0).take(FRAME_SAMPLES * 50));

    let capture = SyntheticCapture::new(source).then_silence();
    let voice = Pipeline::start(
        voice_config(
            &alice,
            &server,
            TransmitMode::VoiceActivity {
                threshold_db: -45.0,
            },
        ),
        Box::new(capture),
        Box::new(NullRender::default()),
        None,
    )
    .unwrap();

    // 静音段
    std::thread::sleep(Duration::from_millis(450));
    let during_silence = voice.stats().packets_sent;

    // 有声段
    std::thread::sleep(Duration::from_millis(550));
    let during_speech = voice.stats().packets_sent;

    // 又静下来：允许 200 ms 的尾音保护，然后必须停发。
    std::thread::sleep(Duration::from_millis(600));
    let after = voice.stats().packets_sent;

    assert!(
        during_silence < 5,
        "静音时不该发包，却发了 {during_silence} 个"
    );
    assert!(
        during_speech > during_silence + 20,
        "有声音时该开始发包：{during_silence} -> {during_speech}"
    );
    assert!(
        after <= during_speech + 25,
        "静下来之后该停：{during_speech} -> {after}"
    );
    std::thread::sleep(Duration::from_millis(300));
    assert_eq!(voice.stats().packets_sent, after, "尾音保护结束后仍在发包");
}

/// 丢掉 Pipeline 就该把线程收干净，而且**不能挂住**。
///
/// 接收线程阻塞在 recv 上，光置个停止标志叫不醒它 —— 这条测试盯的就是
/// 那个叫醒机制真的有效。
#[test]
fn dropping_the_pipeline_stops_cleanly() {
    let server = start_server();
    let alice = join(&server, "阿狸");

    let capture = SyntheticCapture::new(vec![0.0; FRAME_SAMPLES * 10]).then_silence();
    let voice = Pipeline::start(
        voice_config(&alice, &server, TransmitMode::Always),
        Box::new(capture),
        Box::new(NullRender::default()),
        None,
    )
    .unwrap();
    std::thread::sleep(Duration::from_millis(300));

    let start = Instant::now();
    drop(voice);
    let took = start.elapsed();
    assert!(
        took < Duration::from_secs(3),
        "关链路花了 {took:?} —— 有线程没被叫醒"
    );
}

/// 合成设备要满足 trait 的约定：阻塞到下一帧。
/// 不满足的话上面所有延迟数字都不作数。
#[test]
fn synthetic_devices_pace_themselves() {
    let mut capture = SyntheticCapture::new(vec![0.0; FRAME_SAMPLES * 10]);
    let mut frame = vec![0.0; FRAME_SAMPLES];
    let start = Instant::now();
    while capture.read(&mut frame).unwrap() {}
    assert!(start.elapsed() > Duration::from_millis(80));

    let (mut render, _) = CollectingRender::new();
    let start = Instant::now();
    for _ in 0..10 {
        render.write(&frame).unwrap();
    }
    assert!(start.elapsed() > Duration::from_millis(80));
}

/// 拿**真声卡**跑一遍。默认跳过 —— CI 上没有音频设备。
///
/// ```bash
/// cargo test -p client-core --test voice_pipeline -- --ignored --nocapture
/// ```
///
/// 它只验「设备打得开、UDP 通得了」，不验听感：频道里只有一个人，
/// 播出去的全是静音，所以不会有啸叫。
#[test]
#[ignore = "要真声卡"]
#[cfg(windows)]
fn real_devices_open_and_udp_comes_up() {
    let server = start_server();
    let alice = join(&server, "阿狸");

    let capture = voice_core::wasapi::WasapiCapture::new(None);
    let diagnostics = capture.diagnostics();
    let voice = Pipeline::start(
        voice_config(&alice, &server, TransmitMode::Always),
        Box::new(capture),
        Box::new(voice_core::wasapi::WasapiRender::new(None)),
        None,
    )
    .expect("起不了语音链路");

    // 保活每 2 秒一次，给它两轮。顺便报一下麦克风电平 ——
    // 「电平条不动」到底是麦克风没收到音还是我们算错了，就看这个。
    voice.set_monitoring(false);
    let mut peak = f32::NEG_INFINITY;
    for _ in 0..50 {
        std::thread::sleep(Duration::from_millis(100));
        peak = peak.max(voice.stats().input_db);
    }
    let stats = voice.stats();
    println!("真设备：{stats:?}");
    println!("这 5 秒里麦克风的峰值电平：{peak:.1} dB（默认语音激活阈值是 -45 dB）");
    println!("采集设备：{}", diagnostics.device_name());
    println!(
        "设备标成静音的采样点占比：{:.1}%（接近 100% 就是设备那边真的没收到音）",
        diagnostics.silent_ratio() * 100.0
    );

    assert!(stats.udp_ok, "UDP 没通 —— 保活没回来：{stats:?}");
    assert!(
        stats.packets_sent > 100,
        "麦克风没出数据（5 秒该有约 500 帧）：{stats:?}"
    );
    assert_eq!(
        stats.underruns, 0,
        "播放欠载了 —— 设备节拍跟不上：{stats:?}"
    );
}

/// 试听：把自己的麦克风混进播放，**但不发给任何人**。
///
/// 这是「找人试」之前唯一能自己回答的问题：麦克风到底有没有在收音。
#[test]
fn monitoring_plays_your_own_mic_without_sending_it() {
    let server = start_server();
    let alice = join(&server, "阿狸");

    let mut source = vec![0.0f32; FRAME_SAMPLES * 20];
    source.extend_from_slice(&chirp_f32(FRAME_SAMPLES * 60));
    source.extend(std::iter::repeat(0.0).take(FRAME_SAMPLES * 20));

    let capture = SyntheticCapture::new(source.clone()).then_silence();
    let (render, played) = CollectingRender::new();
    // 按住说话，但**不按** —— 所以一个语音包都不该发出去
    let voice = Pipeline::start(
        voice_config(&alice, &server, TransmitMode::PushToTalk),
        Box::new(capture),
        Box::new(render),
        None,
    )
    .unwrap();
    voice.set_monitoring(true);

    std::thread::sleep(Duration::from_millis(1200));

    let stats = voice.stats();
    assert_eq!(
        stats.packets_sent, 0,
        "试听把声音发出去了 —— 别人会听到你在试麦：{stats:?}"
    );

    let played = played.lock().unwrap().clone();
    let energy: f32 = played.iter().map(|s| s * s).sum();
    assert!(energy > 0.0, "开了试听却一点声音都没有");

    // 真的是麦克风那个信号，不是别的什么
    let estimate = voice_core::signal::best_lag(
        &to_i16(&source),
        &to_i16(&played),
        FRAME_SAMPLES * 30,
        FRAME_SAMPLES * 20,
        (SAMPLE_RATE as usize * 200) / 1000,
    )
    .expect("互相关跑不起来");
    assert!(
        estimate.peak > 0.5,
        "播出来的跟麦克风进去的对不上，相关峰值只有 {:.2}",
        estimate.peak
    );
}

/// 关掉试听就该彻底安静 —— 而且再打开时不能先播一段旧的。
#[test]
fn monitoring_can_be_turned_off_cleanly() {
    let server = start_server();
    let alice = join(&server, "阿狸");

    let capture = SyntheticCapture::new(chirp_f32(FRAME_SAMPLES * 200)).then_silence();
    let (render, played) = CollectingRender::new();
    let voice = Pipeline::start(
        voice_config(&alice, &server, TransmitMode::PushToTalk),
        Box::new(capture),
        Box::new(render),
        None,
    )
    .unwrap();

    // 一开始没开，应该是静音
    std::thread::sleep(Duration::from_millis(400));
    let quiet: f32 = played.lock().unwrap().iter().map(|s| s * s).sum();
    assert_eq!(quiet, 0.0, "没开试听却有声音");

    voice.set_monitoring(true);
    std::thread::sleep(Duration::from_millis(400));
    let loud: f32 = played.lock().unwrap().iter().map(|s| s * s).sum();
    assert!(loud > 0.0, "开了试听还是没声音");

    voice.set_monitoring(false);
    std::thread::sleep(Duration::from_millis(100));
    let before = played.lock().unwrap().len();
    std::thread::sleep(Duration::from_millis(400));
    let after: f32 = played.lock().unwrap()[before..].iter().map(|s| s * s).sum();
    assert_eq!(after, 0.0, "关了试听还在播");
}

/// 电平表要跟着麦克风动。
///
/// 它回答的是用户最常问的那个问题：「我说话了，为什么语音激活没触发？」——
/// 看一眼电平条就知道是麦克风没收到音，还是收到了但没过阈值。
#[test]
fn the_input_level_follows_the_microphone() {
    let server = start_server();
    let alice = join(&server, "阿狸");

    // 前面静音，后面有声音
    let mut source = vec![0.0f32; FRAME_SAMPLES * 60];
    source.extend_from_slice(&chirp_f32(FRAME_SAMPLES * 100));

    let capture = SyntheticCapture::new(source).then_silence();
    let voice = Pipeline::start(
        voice_config(&alice, &server, TransmitMode::PushToTalk),
        Box::new(capture),
        Box::new(NullRender::default()),
        None,
    )
    .unwrap();

    std::thread::sleep(Duration::from_millis(400));
    let silent = voice.stats().input_db;
    std::thread::sleep(Duration::from_millis(600));
    let speaking = voice.stats().input_db;

    assert!(silent < -90.0, "静音时电平该贴底，实际 {silent:.1} dB");
    assert!(
        speaking > silent + 40.0,
        "有声音时电平该明显抬起来：{silent:.1} -> {speaking:.1} dB"
    );
    // 默认 VAD 阈值是 -45 dB，正常说话必须能过
    assert!(
        speaking > -45.0,
        "这个信号连默认阈值都过不去，电平算错了：{speaking:.1} dB"
    );
}

/// 换设备重建两端 Pipeline，沿用 TLS 会话；新语音和保活必须立即通过旧防重放窗口。
#[test]
fn rebuilding_devices_preserves_sequences_and_bidirectional_voice() {
    let server = start_server();
    let alice = join(&server, "alice");
    let bob = join(&server, "bob");
    let make_voice = |client: &Client| {
        let source = chirp_f32(FRAME_SAMPLES * 200);
        let (render, played) = CollectingRender::new();
        let voice = Pipeline::start(
            voice_config(client, &server, TransmitMode::PushToTalk),
            Box::new(SyntheticCapture::new(source).then_silence()),
            Box::new(render),
            None,
        )
        .unwrap();
        (voice, played)
    };
    let (a, _) = make_voice(&alice);
    let (b, _) = make_voice(&bob);
    a.set_transmitting(true);
    b.set_transmitting(true);
    let wait_for = |condition: &dyn Fn() -> bool| {
        let deadline = Instant::now() + Duration::from_secs(3);
        while !condition() {
            assert!(Instant::now() < deadline, "重建后的链路没恢复");
            std::thread::sleep(Duration::from_millis(10));
        }
    };
    wait_for(&|| {
        a.stats().packets_sent > 40
            && b.stats().packets_sent > 40
            && a.stats().udp_ok
            && b.stats().udp_ok
    });
    drop(a);
    drop(b);
    let a_received = server.hub.voice.packets_received(alice.session_id());
    let b_received = server.hub.voice.packets_received(bob.session_id());
    let (a, a_played) = make_voice(&alice);
    let (b, b_played) = make_voice(&bob);
    assert!(Arc::ptr_eq(
        &alice.voice_keys().sequences,
        &voice_config(&alice, &server, TransmitMode::Always).sequences
    ));
    // 重建默认 PTT，闭麦仍然优先；保活必须已经恢复。
    a.set_muted(true);
    a.set_transmitting(true);
    wait_for(&|| a.stats().udp_ok && b.stats().udp_ok);
    assert_eq!(a.stats().packets_sent, 0);
    a.set_muted(false);
    b.set_transmitting(true);
    wait_for(&|| {
        server.hub.voice.packets_received(alice.session_id()) > a_received + 20
            && server.hub.voice.packets_received(bob.session_id()) > b_received + 20
    });
    wait_for(&|| {
        a_played.lock().unwrap().iter().any(|v| v.abs() > 0.05)
            && b_played.lock().unwrap().iter().any(|v| v.abs() > 0.05)
    });
    a.set_transmitting(false);
    b.set_transmitting(false);
}

#[test]
fn a_capture_failure_is_reported_while_receive_remains_available() {
    struct FailedCapture;
    impl Capture for FailedCapture {
        fn read(&mut self, _: &mut [f32]) -> std::io::Result<bool> {
            Err(std::io::Error::other("设备已拔出"))
        }
    }
    let server = start_server();
    let alice = join(&server, "alice");
    let voice = Pipeline::start(
        voice_config(&alice, &server, TransmitMode::Always),
        Box::new(FailedCapture),
        Box::new(NullRender::default()),
        None,
    )
    .unwrap();
    let deadline = Instant::now() + Duration::from_secs(2);
    loop {
        let stats = voice.stats();
        if let Some(error) = &stats.error {
            assert!(error.contains("麦克风"));
            assert!(error.contains("重试语音"));
            assert_eq!(stats.capture_error.as_ref(), Some(error));
            assert!(!stats.input_available);
            assert!(!stats.transmitting);
            assert!(stats.render_error.is_none());
            if stats.udp_ok {
                break;
            }
        }
        assert!(Instant::now() < deadline);
        std::thread::sleep(Duration::from_millis(10));
    }
}

#[test]
fn a_playback_failure_does_not_stop_sending_voice() {
    struct FailedRender;
    impl Render for FailedRender {
        fn write(&mut self, _: &[f32]) -> std::io::Result<()> {
            Err(std::io::Error::other("设备已拔出"))
        }
    }
    let server = start_server();
    let alice = join(&server, "alice");
    let voice = Pipeline::start(
        voice_config(&alice, &server, TransmitMode::Always),
        Box::new(SyntheticCapture::new(vec![0.; FRAME_SAMPLES]).then_silence()),
        Box::new(FailedRender),
        None,
    )
    .unwrap();
    let deadline = Instant::now() + Duration::from_secs(2);
    loop {
        let stats = voice.stats();
        if let Some(error) = &stats.error {
            assert!(error.contains("播放"));
            assert!(error.contains("重试语音"));
            assert_eq!(stats.render_error.as_ref(), Some(error));
            assert!(!stats.render_available);
            assert!(stats.capture_error.is_none());
            if stats.packets_sent > 10 {
                assert!(stats.transmitting, "输出失败时仍在发送，状态不能被错误抹掉");
                assert!(stats.input_available);
                break;
            }
        }
        assert!(Instant::now() < deadline);
        std::thread::sleep(Duration::from_millis(10));
    }
}

/// UDP 受阻时区分探测中和已超时，网络恢复后提示自动清除。
#[test]
fn udp_failure_is_visible_and_clears_after_probes_return() {
    use protocol::{VoiceCipher, VoiceHeader};
    let socket = UdpSocket::bind("127.0.0.1:0").unwrap();
    socket
        .set_read_timeout(Some(Duration::from_secs(4)))
        .unwrap();
    let voice = Pipeline::start(
        PipelineConfig {
            session_id: 1,
            server: socket.local_addr().unwrap(),
            sequences: Arc::new(protocol::VoiceSequences::default()),
            upstream_key: [1; 32],
            downstream_key: [2; 32],
            jitter: default_jitter(),
            mode: TransmitMode::PushToTalk,
        },
        Box::new(SyntheticCapture::new(Vec::new()).then_silence()),
        Box::new(NullRender::default()),
        None,
    )
    .unwrap();
    assert!(!voice.stats().udp_failed);
    let deadline = Instant::now() + Duration::from_secs(10);
    while !voice.stats().udp_failed {
        assert!(Instant::now() < deadline);
        std::thread::sleep(Duration::from_millis(20));
    }
    assert!(!voice.stats().udp_ok);
    // 先丢掉阻塞期间旧探测；用刚发出的探测验证恢复，避免复用旧时间戳。
    socket.set_nonblocking(true).unwrap();
    let mut received = [0; 2048];
    while socket.recv_from(&mut received).is_ok() {}
    socket.set_nonblocking(false).unwrap();
    let (n, from) = socket.recv_from(&mut received).unwrap();
    let mut payload = Vec::new();
    let header: VoiceHeader = VoiceCipher::new(&[1; 32])
        .open(&received[..n], &mut payload)
        .unwrap();
    assert!(header.is_keepalive());
    let mut reply = Vec::new();
    VoiceCipher::new(&[2; 32])
        .seal(header, &[], &mut reply)
        .unwrap();
    socket.send_to(&reply, from).unwrap();
    let deadline = Instant::now() + Duration::from_secs(2);
    while !voice.stats().udp_ok {
        assert!(Instant::now() < deadline);
        std::thread::sleep(Duration::from_millis(10));
    }
    assert!(!voice.stats().udp_failed);
}

#[test]
fn ended_capture_is_reported_even_when_udp_is_healthy() {
    let server = start_server();
    let client = join(&server, "ended");
    let voice = Pipeline::start(
        voice_config(&client, &server, TransmitMode::Always),
        Box::new(SyntheticCapture::new(vec![0.; FRAME_SAMPLES])),
        Box::new(NullRender::default()),
        None,
    )
    .unwrap();
    let deadline = Instant::now() + Duration::from_secs(3);
    while !(voice.stats().udp_ok && voice.stats().error.is_some()) {
        assert!(Instant::now() < deadline);
        std::thread::sleep(Duration::from_millis(10));
    }
}

#[test]
fn quiescing_stops_transport_but_plays_the_local_disconnect_notice() {
    let server = start_server();
    let client = join(&server, "notice");
    let (render, collected) = CollectingRender::new();
    let voice = Pipeline::start(
        voice_config(&client, &server, TransmitMode::Always),
        Box::new(SyntheticCapture::new(vec![0.; FRAME_SAMPLES]).then_silence()),
        Box::new(render),
        None,
    )
    .unwrap();
    let deadline = Instant::now() + Duration::from_secs(3);
    while !voice.stats().udp_ok {
        assert!(Instant::now() < deadline);
        std::thread::sleep(Duration::from_millis(10));
    }
    voice.quiesce();
    assert!(!voice.stats().transmitting);
    assert!(!voice.stats().input_available);
    std::thread::sleep(Duration::from_millis(100));
    let sent = voice.stats().packets_sent;
    collected.lock().unwrap().clear();
    voice
        .cues()
        .push(&voice_core::cue::connection_chime(false), 1.0);
    std::thread::sleep(Duration::from_millis(400));
    assert_eq!(voice.stats().packets_sent, sent);
    assert!(
        voice.stats().error.is_none(),
        "intentional capture shutdown is not a device failure"
    );
    assert!(collected.lock().unwrap().iter().any(|s| s.abs() > 0.05));
    assert!(voice.stats().render_available);
}

/// 每次只让采集返回一帧；下一次 read 开始后，上一帧的编码和状态提交已经完成。
/// 验证 VAD 尾音和即时控制时不依赖设备调度与固定 sleep。
enum CaptureStep {
    Frame(f32),
    Fail,
}

struct SteppedCapture {
    steps: std::sync::mpsc::Receiver<CaptureStep>,
    reads: Arc<std::sync::atomic::AtomicUsize>,
}

impl Capture for SteppedCapture {
    fn read(&mut self, frame: &mut [f32]) -> std::io::Result<bool> {
        self.reads.fetch_add(1, std::sync::atomic::Ordering::SeqCst);
        match self.steps.recv() {
            Ok(CaptureStep::Frame(value)) => {
                frame.fill(value);
                Ok(true)
            }
            Ok(CaptureStep::Fail) => Err(std::io::Error::other("受控采集故障")),
            Err(_) => Ok(false),
        }
    }
}

struct SteppedVoice {
    voice: Option<Pipeline>,
    steps: Option<std::sync::mpsc::Sender<CaptureStep>>,
    reads: Arc<std::sync::atomic::AtomicUsize>,
    completed: usize,
}

impl SteppedVoice {
    fn start(config: PipelineConfig, initial: PipelineState) -> Self {
        let (steps, input) = std::sync::mpsc::channel();
        let reads = Arc::new(std::sync::atomic::AtomicUsize::new(0));
        let voice = Pipeline::start_with_state(
            config,
            Box::new(SteppedCapture {
                steps: input,
                reads: reads.clone(),
            }),
            Box::new(NullRender::default()),
            None,
            initial,
        )
        .unwrap();
        Self {
            voice: Some(voice),
            steps: Some(steps),
            reads,
            completed: 0,
        }
    }

    fn voice(&self) -> &Pipeline {
        self.voice.as_ref().unwrap()
    }

    fn frame(&mut self, value: f32) {
        self.steps
            .as_ref()
            .unwrap()
            .send(CaptureStep::Frame(value))
            .unwrap();
        self.completed += 1;
        wait_until(|| self.reads.load(std::sync::atomic::Ordering::SeqCst) > self.completed);
    }
}

impl Drop for SteppedVoice {
    fn drop(&mut self) {
        // 即使断言 panic，也先解除受控 Capture 的阻塞，然后才 join Pipeline。
        self.steps.take();
        self.voice.take();
    }
}

fn wait_until(mut condition: impl FnMut() -> bool) {
    let deadline = Instant::now() + Duration::from_secs(3);
    while !condition() {
        assert!(Instant::now() < deadline, "受控语音链路未达到预期状态");
        std::thread::sleep(Duration::from_millis(1));
    }
}

#[test]
fn actual_transmission_preserves_vad_tail_and_clears_on_mute_or_capture_failure() {
    let server = start_server();
    let client = join(&server, "controlled-vad");
    let mut source = SteppedVoice::start(
        voice_config(
            &client,
            &server,
            TransmitMode::VoiceActivity {
                threshold_db: -45.0,
            },
        ),
        PipelineState::default(),
    );
    source.frame(0.0);
    assert!(source.voice().stats().input_available);
    assert!(!source.voice().stats().transmitting);
    source.frame(0.2);
    assert!(source.voice().stats().transmitting);
    for _ in 0..200 / voice_core::audio::FRAME_MS {
        source.frame(0.0);
        assert!(source.voice().stats().transmitting, "VAD 尾音包还在发");
    }
    source.frame(0.0);
    assert!(!source.voice().stats().transmitting);

    source.frame(0.2);
    source.voice().set_muted(true);
    assert!(
        !source.voice().stats().transmitting,
        "闭麦不能等待下一帧才更新"
    );
    assert!(source.voice().stats().input_available, "闭麦不关闭电平采集");
    let before = source.voice().stats().packets_sent;
    source.frame(0.2);
    assert_eq!(source.voice().stats().packets_sent, before);
    source.voice().set_muted(false);
    source.frame(0.2);
    assert!(source.voice().stats().transmitting);
    source
        .steps
        .as_ref()
        .unwrap()
        .send(CaptureStep::Fail)
        .unwrap();
    wait_until(|| source.voice().stats().capture_error.is_some());
    wait_until(|| !source.voice().stats().input_available);
    assert!(!source.voice().stats().transmitting);
    assert!(source.voice().stats().render_error.is_none());
}

#[test]
fn ptt_release_and_permission_revocation_hide_transmission_without_another_frame() {
    let server = start_server();
    let client = join(&server, "controlled-ptt");
    let mut source = SteppedVoice::start(
        voice_config(&client, &server, TransmitMode::PushToTalk),
        PipelineState {
            transmitting: true,
            ..PipelineState::default()
        },
    );
    source.frame(0.2);
    assert!(source.voice().stats().transmitting);
    source.voice().set_transmitting(false);
    assert!(!source.voice().stats().transmitting);
    source.voice().set_transmitting(true);
    assert!(
        !source.voice().stats().transmitting,
        "新按键不伪装成已提交音频"
    );
    source.frame(0.2);
    assert!(source.voice().stats().transmitting);
    source.voice().set_send_enabled(false);
    assert!(!source.voice().stats().transmitting);
    let sent = source.voice().stats().packets_sent;
    source.frame(0.2);
    assert_eq!(source.voice().stats().packets_sent, sent);
    source.voice().quiesce();
    assert!(!source.voice().stats().input_available);
    assert!(!source.voice().stats().udp_failed, "主动退场不是网络故障");
}

#[test]
fn initial_permission_blocks_voice_and_keepalive_until_the_candidate_is_accepted() {
    let sink = UdpSocket::bind("127.0.0.1:0").unwrap();
    sink.set_nonblocking(true).unwrap();
    let mut source = SteppedVoice::start(
        PipelineConfig {
            sequences: Arc::new(protocol::VoiceSequences::default()),
            session_id: 7,
            server: sink.local_addr().unwrap(),
            upstream_key: [1; 32],
            downstream_key: [2; 32],
            jitter: default_jitter(),
            mode: TransmitMode::Always,
        },
        PipelineState {
            send_enabled: false,
            transmitting: true,
            ..PipelineState::default()
        },
    );
    for _ in 0..4 {
        source.frame(0.2);
    }
    wait_until(|| source.voice().stats().render_available);
    assert!(source.voice().stats().input_available);
    assert_eq!(source.voice().stats().packets_sent, 0);
    assert!(!source.voice().stats().transmitting);
    let mut wire = [0; 2048];
    assert_eq!(
        sink.recv_from(&mut wire).unwrap_err().kind(),
        std::io::ErrorKind::WouldBlock
    );
    source.voice().set_muted(true);
    source.voice().set_send_enabled(true);
    source.frame(0.2);
    assert_eq!(
        source.voice().stats().packets_sent,
        0,
        "先注入闭麦再接纳不能漏音"
    );
    wait_until(|| sink.recv_from(&mut wire).is_ok());
    source.voice().set_muted(false);
    source.frame(0.2);
    assert!(source.voice().stats().transmitting);
}

#[test]
fn initial_muted_state_is_applied_before_the_first_capture_frame() {
    let server = start_server();
    let client = join(&server, "initial-mute");
    let mut source = SteppedVoice::start(
        voice_config(&client, &server, TransmitMode::Always),
        PipelineState {
            muted: true,
            deafened: true,
            transmitting: true,
            monitoring: true,
            ..PipelineState::default()
        },
    );
    assert!(source.voice().is_monitoring());
    source.frame(0.2);
    assert!(source.voice().stats().input_available);
    assert_eq!(source.voice().stats().packets_sent, 0);
    assert!(!source.voice().stats().transmitting);
    source.voice().set_muted(false);
    source.frame(0.2);
    assert_eq!(source.voice().stats().packets_sent, 1);
    assert!(source.voice().stats().transmitting);
}

#[test]
fn dropping_a_control_handle_neither_joins_nor_stops_the_owned_pipeline() {
    fn assert_send_sync<T: Send + Sync>() {}
    assert_send_sync::<voice_core::pipeline::PipelineControl>();
    let server = start_server();
    let client = join(&server, "control-handle");
    let mut source = SteppedVoice::start(
        voice_config(&client, &server, TransmitMode::Always),
        PipelineState::default(),
    );
    source.frame(0.2);
    let control = source.voice().control();
    let (finished, done) = std::sync::mpsc::channel();
    let worker = std::thread::spawn(move || {
        drop(control);
        finished.send(()).unwrap();
    });
    // 采集此时阻塞，若控制句柄持有线程并 join，这里就不会返回。
    done.recv_timeout(Duration::from_secs(1))
        .expect("控制句柄释放等待了音频线程");
    worker.join().unwrap();
    source.frame(0.2);
    assert_eq!(source.voice().stats().packets_sent, 2);
    let survivor = source.voice().control();
    source.steps.take();
    source.voice.take();
    assert!(!survivor.stats().transmitting);
    assert!(!survivor.stats().input_available);
    assert!(!survivor.stats().render_available);
}

/// A real UDP relay independently blocks each direction while TLS stays connected.
#[test]
fn one_way_udp_loss_is_detected_even_if_other_people_are_still_audible() {
    use std::sync::atomic::{AtomicBool, AtomicU8, Ordering};
    let server = start_server();
    let alice = join(&server, "one-way");
    let bob = join(&server, "speaker");
    let mut config = voice_config(&alice, &server, TransmitMode::Always);
    let upstream = config.server;
    let socket = UdpSocket::bind("127.0.0.1:0").unwrap();
    socket
        .set_read_timeout(Some(Duration::from_millis(50)))
        .unwrap();
    config.server = socket.local_addr().unwrap();
    let mode = Arc::new(AtomicU8::new(0));
    let stop = Arc::new(AtomicBool::new(false));
    let worker_mode = mode.clone();
    let worker_stop = stop.clone();
    let relay = std::thread::spawn(move || {
        let mut client = None;
        let mut buf = [0; 2048];
        while !worker_stop.load(Ordering::Relaxed) {
            let Ok((n, from)) = socket.recv_from(&mut buf) else {
                continue;
            };
            if from == upstream {
                if worker_mode.load(Ordering::Relaxed) != 2 {
                    if let Some(to) = client {
                        let _ = socket.send_to(&buf[..n], to);
                    }
                }
            } else {
                client = Some(from);
                if worker_mode.load(Ordering::Relaxed) != 1 {
                    let _ = socket.send_to(&buf[..n], upstream);
                }
            }
        }
    });
    let voice = Pipeline::start(
        config,
        Box::new(SyntheticCapture::new(vec![0.; FRAME_SAMPLES]).then_silence()),
        Box::new(NullRender::default()),
        None,
    )
    .unwrap();
    let _speaker = Pipeline::start(
        voice_config(&bob, &server, TransmitMode::Always),
        Box::new(SyntheticCapture::new(vec![0.; FRAME_SAMPLES]).then_silence()),
        Box::new(NullRender::default()),
        None,
    )
    .unwrap();
    let wait = |condition: &dyn Fn() -> bool| {
        let deadline = Instant::now() + Duration::from_secs(12);
        while !condition() {
            assert!(
                Instant::now() < deadline,
                "UDP relay did not reach expected state"
            );
            std::thread::sleep(Duration::from_millis(20));
        }
    };
    wait(&|| voice.stats().udp_ok);
    let session = alice.session_id();
    for direction in [1, 2] {
        let before = voice.stats().packets_received;
        mode.store(direction, Ordering::Relaxed);
        wait(&|| voice.stats().udp_failed);
        assert_eq!(alice.session_id(), session, "TLS session remains connected");
        if direction == 1 {
            assert!(
                voice.stats().packets_received > before + 100,
                "downstream voice still arrives"
            );
        }
        mode.store(0, Ordering::Relaxed);
        wait(&|| voice.stats().udp_ok);
        assert!(!voice.stats().udp_failed);
    }
    stop.store(true, Ordering::Relaxed);
    relay.join().unwrap();
}
