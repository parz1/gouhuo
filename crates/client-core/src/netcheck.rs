// SPDX-License-Identifier: MPL-2.0

//! 加入前的网络体检：这台电脑到这台服务器之间，TCP、TLS 和 UDP 各是什么样。
//!
//! # 为什么要有它
//!
//! 「有的朋友会断」这句话里没有任何可以动手的信息。是 UDP 被运营商拦了？
//! 是跨境线路晚高峰丢包？是按包速限流（低速探测一切正常，一说话就丢）？
//! 还是服务器本身不可达？这几种在通话页上长得一模一样，解法却完全不同。
//! 体检把它们分开，并给出一段**可以直接复制给服主的文字**。
//!
//! # 怎么测
//!
//! 走真实的连接路径：TCP → TLS（固定指纹）→ 认证 → 派生语音密钥 → UDP 保活。
//! 跟正常加入唯一的区别是 `Hello.probe_only`：新服务端据此不把我们放进名单，
//! 旧服务端不认这个字段，会把我们当成普通加入 —— 那样体检期间会在根频道里
//! 短暂出现一个不出声的「我」，报告里会写明。
//!
//! UDP 分两段：先按**保活的节奏**（每秒十个）测五秒，再按**说话的节奏**
//! （每秒一百个、语音大小的包）测五秒。两段差很多，就是按包速限流的迹象 ——
//! 这正是跨境 UDP 最常见的一种毛病，而它在普通的 ping 里完全看不出来。
//!
//! 保活包本来就是给服务端量 RTT 用的（见 `protocol::FLAG_KEEPALIVE`），
//! 服务端原样回一个，我们按序号配对。负载服务端一律丢掉，所以可以塞满
//! 语音那么多字节而不会被转发给任何人。
//!
//! # 它不能证明什么
//!
//! 十秒的样本只代表这十秒。晚高峰的问题要在晚高峰测；UDP 限流有时要持续
//! 几十秒才开始。结论是**提示**，不是诊断书 —— 所以报告里数字和结论分开写。

use std::collections::BTreeMap;
use std::io;
use std::net::{Ipv4Addr, Ipv6Addr, SocketAddr, UdpSocket};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};

use protocol::control::{server_message, Ping, ServerMessage};
use protocol::{Invite, VoiceCipher, VoiceHeader, VoiceSequences, FLAG_KEEPALIVE, MAX_DATAGRAM};
use rustls::pki_types::ServerName;
use transport::{client_config, derive_voice_key, DOWNSTREAM, UPSTREAM};
use voice_core::identity::Identity;

use crate::client::{authenticate, connect_tcp_peer, CONNECT_TIMEOUT};
use crate::error::ConnectError;
use crate::wire::{Reader, Wire};

/// 一段 UDP 探测的参数。
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct PhasePlan {
    /// 每秒几个包。
    pub rate_per_sec: u32,
    pub seconds: f64,
    /// 每个包塞多少字节负载。0 = 跟真实保活一样；80 ≈ 一帧 64 kbps 语音。
    pub payload: usize,
}

impl PhasePlan {
    pub fn count(&self) -> usize {
        ((self.rate_per_sec as f64) * self.seconds).round().max(1.0) as usize
    }
}

/// 整个体检的节奏。默认十来秒；测试用短的。
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct CheckPlan {
    pub tls_pings: usize,
    pub slow: PhasePlan,
    pub fast: PhasePlan,
    /// 两段发完之后再等多久收迟到的回包。
    pub grace: Duration,
}

impl Default for CheckPlan {
    fn default() -> Self {
        Self {
            tls_pings: 5,
            slow: PhasePlan {
                rate_per_sec: 10,
                seconds: 5.0,
                payload: 0,
            },
            fast: PhasePlan {
                rate_per_sec: 100,
                seconds: 5.0,
                payload: VOICE_SIZED_PAYLOAD,
            },
            grace: Duration::from_millis(1000),
        }
    }
}

/// 一帧 64 kbps / 10 ms 的 Opus 大约这么多字节。
pub const VOICE_SIZED_PAYLOAD: usize = 80;

/// 体检进行到哪一步了，给进度条用。
#[derive(Debug, Clone, PartialEq)]
pub enum Stage {
    Connecting,
    Handshake,
    Authenticating,
    TlsPing,
    /// `percent` 是整段 UDP 测试的进度（0–100）。
    Udp {
        fast: bool,
        percent: u32,
    },
    Done,
}

impl Stage {
    pub fn label(&self) -> String {
        match self {
            Stage::Connecting => "正在连接服务器…".into(),
            Stage::Handshake => "正在建立加密连接…".into(),
            Stage::Authenticating => "正在验证身份…".into(),
            Stage::TlsPing => "正在测 TCP 往返…".into(),
            Stage::Udp {
                fast: false,
                percent,
            } => format!("正在测 UDP（保活节奏）… {percent}%"),
            Stage::Udp {
                fast: true,
                percent,
            } => format!("正在测 UDP（说话节奏）… {percent}%"),
            Stage::Done => "体检完成".into(),
        }
    }
}

#[derive(Debug)]
pub enum CheckError {
    Cancelled,
    Connect(ConnectError),
}

impl From<ConnectError> for CheckError {
    fn from(e: ConnectError) -> Self {
        CheckError::Connect(e)
    }
}

impl From<io::Error> for CheckError {
    fn from(e: io::Error) -> Self {
        CheckError::Connect(ConnectError::Io(e))
    }
}

/// 一段 UDP 探测的结果。
#[derive(Debug, Clone, PartialEq)]
pub struct PhaseResult {
    pub plan: PhasePlan,
    pub sent: usize,
    pub received: usize,
    pub loss_permille: u32,
    pub rtt_min_ms: f64,
    pub rtt_median_ms: f64,
    pub rtt_p95_ms: f64,
    pub rtt_max_ms: f64,
    /// 相邻两次往返之差的平均绝对值。
    pub jitter_ms: f64,
    pub reordered: usize,
    /// 最长的一段连续没回来的包。
    pub longest_gap: usize,
}

impl PhaseResult {
    pub fn loss_percent(&self) -> f64 {
        self.loss_permille as f64 / 10.0
    }
}

/// 体检的结论。规则见 [`CheckReport::judge`]。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Verdict {
    Good,
    /// 延迟高但稳。
    HighLatency,
    /// 有一点丢包或抖动，通话大体可用。
    Fluctuating,
    /// 两段都丢得厉害。
    Congested,
    /// 慢的那段还好，快的那段丢得多：像是按包速限流。
    RateLimited,
    /// TCP 通、UDP 一个回包都没有。
    UdpBlocked,
    /// 没测完。
    Incomplete,
}

impl Verdict {
    pub fn headline(self) -> &'static str {
        match self {
            Verdict::Good => "到这台服务器的线路良好",
            Verdict::HighLatency => "线路通畅，但延迟偏高",
            Verdict::Fluctuating => "线路有波动，通话大体可用",
            Verdict::Congested => "线路丢包严重，通话会断断续续",
            Verdict::RateLimited => "UDP 像是被按包速限流了",
            Verdict::UdpBlocked => "UDP 不通，只能走兼容连接",
            Verdict::Incomplete => "体检没有做完",
        }
    }

    pub fn advice(self) -> &'static str {
        match self {
            Verdict::Good => "现在这条线没问题。如果通话还是会断，多半是别的时段或者别的人的线路。",
            Verdict::HighLatency => {
                "说话会有明显的来回延迟，但不会卡。要降延迟只能换离你更近的服务器或线路。"
            }
            Verdict::Fluctuating => {
                "偶尔会听到卡一下。晚高峰可能更差；把这份报告发给服主，几个人一起对比最有用。"
            }
            Verdict::Congested => {
                "这不是软件能修的：你到服务器之间的线路本身在丢包，常见于跨境链路的晚高峰。\
                 换时段再测一次；持续如此的话，服主需要换线路或加一个离你近的入口。"
            }
            Verdict::RateLimited => {
                "慢速探测正常、说话节奏就大量丢包，是运营商对持续 UDP 流限速的典型表现。\
                 把报告发给服主：服务端可以通过降低包速（更长的帧）或换端口、换线路来绕开。"
            }
            Verdict::UdpBlocked => {
                "TCP 能连上，但 UDP 一个回包都没有。可能是服务器安全组没放 UDP，\
                 也可能是你这边的网络拦了。新版服务端会自动切到 TLS 兼容连接，延迟会高一些。"
            }
            Verdict::Incomplete => "中途被取消或者连接断了。再试一次。",
        }
    }
}

/// 一次体检的全部结果。`to_text` 给出能直接粘贴发人的版本。
#[derive(Debug, Clone, PartialEq)]
pub struct CheckReport {
    pub host: String,
    pub port: u16,
    pub started_unix_ms: u64,
    pub duration: Duration,
    pub client_version: String,
    pub tcp_connect: Duration,
    pub tls_handshake: Duration,
    /// 服务端认得 `probe_only`，体检期间没有进名单。
    pub server_probe_aware: bool,
    pub server_tcp_voice: bool,
    /// TLS 上的 Ping/Pong 往返，按发出顺序。空 = 一次都没回来。
    pub tls_rtt_ms: Vec<f64>,
    pub slow: Option<PhaseResult>,
    pub fast: Option<PhaseResult>,
    pub verdict: Verdict,
}

impl CheckReport {
    pub fn udp_reachable(&self) -> bool {
        self.slow.as_ref().is_some_and(|p| p.received > 0)
            || self.fast.as_ref().is_some_and(|p| p.received > 0)
    }

    pub fn tls_rtt_median_ms(&self) -> Option<f64> {
        median(&self.tls_rtt_ms)
    }

    /// 规则写在这里，而不是散在界面里，是为了能测。
    pub fn judge(slow: Option<&PhaseResult>, fast: Option<&PhaseResult>) -> Verdict {
        let (Some(slow), Some(fast)) = (slow, fast) else {
            return Verdict::Incomplete;
        };
        if slow.received == 0 && fast.received == 0 {
            return Verdict::UdpBlocked;
        }
        // 快的比慢的多丢五个点以上、而且快的丢到了 8% 以上：限流迹象。
        if fast.loss_permille >= 80 && fast.loss_permille >= slow.loss_permille + 50 {
            return Verdict::RateLimited;
        }
        let worst_loss = slow.loss_permille.max(fast.loss_permille);
        if worst_loss >= 100 {
            return Verdict::Congested;
        }
        let worst_jitter = slow.jitter_ms.max(fast.jitter_ms);
        if worst_loss >= 20 || worst_jitter >= 30.0 {
            return Verdict::Fluctuating;
        }
        let rtt = slow.rtt_median_ms.min(fast.rtt_median_ms);
        if rtt >= 200.0 {
            return Verdict::HighLatency;
        }
        Verdict::Good
    }

    /// 纯文本，发给服主用。不含加入码、身份和设备名。
    pub fn to_text(&self) -> String {
        let mut out = String::new();
        out.push_str(&format!("篝火网络体检 · {}\n", self.client_version));
        out.push_str(&format!(
            "服务器 {}:{} · {}\n",
            self.host,
            self.port,
            format_unix(self.started_unix_ms)
        ));
        out.push_str(&format!("结论：{}\n", self.verdict.headline()));
        out.push('\n');
        out.push_str(&format!(
            "TCP 连接 {} ms · TLS 握手 {} ms\n",
            self.tcp_connect.as_millis(),
            self.tls_handshake.as_millis()
        ));
        match self.tls_rtt_median_ms() {
            Some(rtt) => out.push_str(&format!(
                "TCP 往返（TLS Ping）：中位 {rtt:.0} ms，{} 次全部回来\n",
                self.tls_rtt_ms.len()
            )),
            None => out.push_str("TCP 往返（TLS Ping）：没有回包\n"),
        }
        if !self.server_probe_aware {
            out.push_str("服务端是旧版本：体检期间短暂出现在频道里，网络报告功能也不可用\n");
        }
        if let Some(slow) = &self.slow {
            out.push_str(&phase_text("UDP 保活节奏", slow));
        }
        if let Some(fast) = &self.fast {
            out.push_str(&phase_text("UDP 说话节奏", fast));
        }
        out.push('\n');
        out.push_str(self.verdict.advice());
        out.push('\n');
        out
    }
}

fn phase_text(label: &str, p: &PhaseResult) -> String {
    if p.received == 0 {
        return format!(
            "{label}（每秒 {} 包 × {:.0} 秒，每包 {} 字节负载）：发 {} 收 0，全部丢失\n",
            p.plan.rate_per_sec, p.plan.seconds, p.plan.payload, p.sent
        );
    }
    format!(
        "{label}（每秒 {} 包 × {:.0} 秒，每包 {} 字节负载）：发 {} 收 {}，丢包 {:.1}%\n  \
         RTT 最小 {:.0} / 中位 {:.0} / p95 {:.0} / 最大 {:.0} ms · 抖动 {:.1} ms · 乱序 {} · 最长连续丢 {}\n",
        p.plan.rate_per_sec,
        p.plan.seconds,
        p.plan.payload,
        p.sent,
        p.received,
        p.loss_percent(),
        p.rtt_min_ms,
        p.rtt_median_ms,
        p.rtt_p95_ms,
        p.rtt_max_ms,
        p.jitter_ms,
        p.reordered,
        p.longest_gap
    )
}

/// 跑一次体检。**会阻塞十几秒**，在后台线程上调。
///
/// `progress` 每一步调一次；`cancel` 置位后在下一个检查点返回 [`CheckError::Cancelled`]。
pub fn run(
    invite: &Invite,
    identity: &Identity,
    plan: &CheckPlan,
    progress: &dyn Fn(Stage),
    cancel: &AtomicBool,
) -> Result<CheckReport, CheckError> {
    let started = Instant::now();
    let started_unix_ms = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_millis() as u64)
        .unwrap_or(0);
    let cancelled = || cancel.load(Ordering::Relaxed);

    // ---- TCP ----
    progress(Stage::Connecting);
    let t = Instant::now();
    let (sock, mut voice_addr) = connect_tcp_peer(&invite.host, invite.port)?;
    let tcp_connect = t.elapsed();
    if cancelled() {
        return Err(CheckError::Cancelled);
    }
    let sock = Arc::new(sock);
    sock.set_nodelay(true)?;
    sock.set_write_timeout(Some(CONNECT_TIMEOUT))?;
    sock.set_read_timeout(Some(CONNECT_TIMEOUT))?;

    // ---- TLS ----
    progress(Stage::Handshake);
    let t = Instant::now();
    let config =
        Arc::new(client_config(invite.cert).map_err(|e| ConnectError::Tls(e.to_string()))?);
    let name = ServerName::try_from("gouhuo").expect("常量，不会失败");
    let mut conn = rustls::ClientConnection::new(config, name)
        .map_err(|e| ConnectError::Tls(e.to_string()))?;
    {
        let mut handshake_sock = sock.as_ref();
        if let Err(e) = conn.complete_io(&mut handshake_sock) {
            let text = e.to_string();
            return Err(if text.contains("指纹对不上") {
                let detail = text.rsplit("unexpected error: ").next().unwrap_or(&text);
                ConnectError::WrongCertificate(detail.to_string())
            } else {
                ConnectError::Tls(text)
            }
            .into());
        }
    }
    let tls_handshake = t.elapsed();
    let upstream =
        derive_voice_key(&conn, UPSTREAM).map_err(|e| ConnectError::Tls(e.to_string()))?;
    let downstream =
        derive_voice_key(&conn, DOWNSTREAM).map_err(|e| ConnectError::Tls(e.to_string()))?;

    let wire = Arc::new(Mutex::new(Wire {
        conn,
        sock: Arc::clone(&sock),
    }));
    let mut reader = Reader::new(Arc::clone(&sock), Arc::clone(&wire));

    // ---- 认证（体检模式）----
    progress(Stage::Authenticating);
    let welcome = authenticate(
        &wire,
        &mut reader,
        identity,
        invite.code.as_deref().unwrap_or(""),
        "网络体检",
        true,
    )?;
    if cancelled() {
        goodbye(&wire);
        return Err(CheckError::Cancelled);
    }
    voice_addr.set_port(welcome.udp_port as u16);
    let session = welcome.session_id;

    // ---- TLS 往返 ----
    progress(Stage::TlsPing);
    sock.set_read_timeout(Some(Duration::from_secs(3)))?;
    let mut tls_rtt_ms = Vec::with_capacity(plan.tls_pings);
    for i in 0..plan.tls_pings {
        if cancelled() {
            goodbye(&wire);
            return Err(CheckError::Cancelled);
        }
        let stamp = 1_000 + i as i64;
        let sent = Instant::now();
        let ok = wire
            .lock()
            .expect("wire poisoned")
            .send(&protocol::control::ClientMessage::from(Ping {
                timestamp: stamp,
                ..Default::default()
            }))
            .is_ok();
        if !ok {
            break;
        }
        // 旧服务端会把我们当成普通加入，中间可能夹着 UserState 之类的广播，跳过。
        let mut got = false;
        loop {
            match reader.next::<ServerMessage>() {
                Ok(Some(ServerMessage {
                    payload: Some(server_message::Payload::Pong(pong)),
                })) if pong.timestamp == stamp => {
                    tls_rtt_ms.push(sent.elapsed().as_secs_f64() * 1000.0);
                    got = true;
                    break;
                }
                Ok(Some(_)) => continue,
                // 对面关了，或者超时（超时之后这条 TLS 流就不可靠了，下面不再读它）。
                Ok(None) | Err(_) => break,
            }
        }
        if !got {
            break;
        }
    }

    // ---- UDP ----
    let total = plan.slow.count() + plan.fast.count();
    let socket = Arc::new(bind_like(voice_addr)?);
    socket.set_read_timeout(Some(Duration::from_millis(50)))?;
    let log: Arc<Mutex<ProbeLog>> = Arc::new(Mutex::new(ProbeLog::default()));
    let stop = Arc::new(AtomicBool::new(false));
    let receiver = {
        let socket = Arc::clone(&socket);
        let log = Arc::clone(&log);
        let stop = Arc::clone(&stop);
        let cipher = VoiceCipher::new(downstream.as_bytes());
        std::thread::Builder::new()
            .name("gouhuo-netcheck-recv".into())
            .spawn(move || receive_loop(&socket, &cipher, session, &log, &stop))?
    };
    let cipher = VoiceCipher::new(upstream.as_bytes());
    let sequences = VoiceSequences::default();
    let mut done = 0usize;
    let mut phases = Vec::with_capacity(2);
    for (fast, phase) in [(false, plan.slow), (true, plan.fast)] {
        let mut sent_seqs = Vec::with_capacity(phase.count());
        let interval = Duration::from_secs_f64(1.0 / phase.rate_per_sec.max(1) as f64);
        let phase_start = Instant::now();
        for i in 0..phase.count() {
            if cancelled() {
                stop.store(true, Ordering::Relaxed);
                let _ = receiver.join();
                goodbye(&wire);
                return Err(CheckError::Cancelled);
            }
            let due = phase_start + interval * i as u32;
            let now = Instant::now();
            if due > now {
                std::thread::sleep(due - now);
            }
            let Some(seq) = sequences.next(true) else {
                break;
            };
            let header = VoiceHeader {
                session,
                seq,
                timestamp: micros(phase_start.elapsed()),
                flags: FLAG_KEEPALIVE,
            };
            let payload = filler(phase.payload, seq);
            let mut wire_bytes = Vec::with_capacity(MAX_DATAGRAM);
            if cipher.seal(header, &payload, &mut wire_bytes).is_err() {
                break;
            }
            log.lock()
                .expect("probe log poisoned")
                .sent(seq, Instant::now());
            let _ = socket.send_to(&wire_bytes, voice_addr);
            sent_seqs.push(seq);
            done += 1;
            if i % (phase.rate_per_sec.max(1) as usize / 2).max(1) == 0 {
                progress(Stage::Udp {
                    fast,
                    percent: (done * 100 / total.max(1)) as u32,
                });
            }
        }
        phases.push((phase, sent_seqs));
    }
    std::thread::sleep(plan.grace);
    stop.store(true, Ordering::Relaxed);
    let _ = receiver.join();
    goodbye(&wire);

    let log = log.lock().expect("probe log poisoned");
    let mut results = phases
        .into_iter()
        .map(|(phase, seqs)| log.summarize(phase, &seqs));
    let slow = results.next();
    let fast = results.next();
    let verdict = CheckReport::judge(slow.as_ref(), fast.as_ref());
    progress(Stage::Done);
    Ok(CheckReport {
        host: invite.host.clone(),
        port: invite.port,
        started_unix_ms,
        duration: started.elapsed(),
        client_version: crate::client_version(),
        tcp_connect,
        tls_handshake,
        server_probe_aware: welcome.probe_only,
        server_tcp_voice: welcome.tcp_voice,
        tls_rtt_ms,
        slow,
        fast,
        verdict,
    })
}

/// 礼貌地关掉 TLS。失败无所谓 —— 服务端的空闲超时会收拾。
fn goodbye(wire: &Arc<Mutex<Wire>>) {
    if let Ok(mut w) = wire.lock() {
        let Wire { conn, sock } = &mut *w;
        conn.send_close_notify();
        let _ = conn.write_tls(&mut sock.as_ref());
        let _ = sock.shutdown(std::net::Shutdown::Both);
    }
}

fn bind_like(addr: SocketAddr) -> io::Result<UdpSocket> {
    if addr.is_ipv4() {
        UdpSocket::bind((Ipv4Addr::UNSPECIFIED, 0))
    } else {
        UdpSocket::bind((Ipv6Addr::UNSPECIFIED, 0))
    }
}

fn micros(d: Duration) -> u32 {
    (d.as_micros() % u128::from(u32::MAX)) as u32
}

/// 负载内容无所谓（反正加密了），只要长度对。
fn filler(len: usize, seq: u32) -> Vec<u8> {
    (0..len)
        .map(|i| (seq as usize).wrapping_mul(31).wrapping_add(i) as u8)
        .collect()
}

fn receive_loop(
    socket: &UdpSocket,
    cipher: &VoiceCipher,
    session: u32,
    log: &Mutex<ProbeLog>,
    stop: &AtomicBool,
) {
    let mut buf = [0u8; 2048];
    let mut payload = Vec::with_capacity(MAX_DATAGRAM);
    while !stop.load(Ordering::Relaxed) {
        let n = match socket.recv(&mut buf) {
            Ok(n) => n,
            Err(_) => continue,
        };
        let Ok(header) = cipher.open(&buf[..n], &mut payload) else {
            continue;
        };
        if header.session != session || !header.is_keepalive() {
            continue;
        }
        log.lock()
            .expect("probe log poisoned")
            .received(header.seq, Instant::now());
    }
}

#[derive(Default)]
struct ProbeLog {
    sent: BTreeMap<u32, Instant>,
    /// 按到达顺序。
    received: Vec<(u32, Instant)>,
}

impl ProbeLog {
    fn sent(&mut self, seq: u32, at: Instant) {
        self.sent.insert(seq, at);
    }

    fn received(&mut self, seq: u32, at: Instant) {
        // 重复的回包（不该有，但 UDP 什么都可能）只算一次。
        if self.received.iter().any(|(s, _)| *s == seq) {
            return;
        }
        self.received.push((seq, at));
    }

    fn summarize(&self, plan: PhasePlan, seqs: &[u32]) -> PhaseResult {
        let set: std::collections::BTreeSet<u32> = seqs.iter().copied().collect();
        let mut rtts: Vec<(u32, f64)> = Vec::new();
        let mut reordered = 0usize;
        let mut last_seq: Option<u32> = None;
        for (seq, at) in &self.received {
            if !set.contains(seq) {
                continue;
            }
            if let Some(sent) = self.sent.get(seq) {
                rtts.push((
                    *seq,
                    at.saturating_duration_since(*sent).as_secs_f64() * 1000.0,
                ));
            }
            if last_seq.is_some_and(|last| *seq < last) {
                reordered += 1;
            }
            last_seq = Some(*seq);
        }
        let got: std::collections::BTreeSet<u32> = rtts.iter().map(|(s, _)| *s).collect();
        let mut longest_gap = 0usize;
        let mut gap = 0usize;
        for seq in seqs {
            if got.contains(seq) {
                gap = 0;
            } else {
                gap += 1;
                longest_gap = longest_gap.max(gap);
            }
        }
        let sent = seqs.len();
        let received = rtts.len();
        let loss_permille = ((sent - received) * 1000).checked_div(sent).unwrap_or(0) as u32;
        let mut sorted: Vec<f64> = rtts.iter().map(|(_, r)| *r).collect();
        sorted.sort_by(|a, b| a.partial_cmp(b).unwrap_or(std::cmp::Ordering::Equal));
        let jitter_ms = if rtts.len() >= 2 {
            rtts.windows(2)
                .map(|w| (w[1].1 - w[0].1).abs())
                .sum::<f64>()
                / (rtts.len() - 1) as f64
        } else {
            0.0
        };
        PhaseResult {
            plan,
            sent,
            received,
            loss_permille,
            rtt_min_ms: sorted.first().copied().unwrap_or(0.0),
            rtt_median_ms: median(&sorted).unwrap_or(0.0),
            rtt_p95_ms: percentile(&sorted, 0.95).unwrap_or(0.0),
            rtt_max_ms: sorted.last().copied().unwrap_or(0.0),
            jitter_ms,
            reordered,
            longest_gap,
        }
    }
}

fn median(values: &[f64]) -> Option<f64> {
    percentile(values, 0.5)
}

/// `values` 必须已排序。
fn percentile(values: &[f64], p: f64) -> Option<f64> {
    if values.is_empty() {
        return None;
    }
    let mut sorted = values.to_vec();
    sorted.sort_by(|a, b| a.partial_cmp(b).unwrap_or(std::cmp::Ordering::Equal));
    let rank = ((sorted.len() - 1) as f64 * p).round() as usize;
    sorted.get(rank).copied()
}

fn format_unix(ms: u64) -> String {
    // 只给人看的大致时间（UTC），不引日期库。
    let secs = ms / 1000;
    let days = secs / 86_400;
    let (y, m, d) = civil_from_days(days as i64);
    let rem = secs % 86_400;
    format!(
        "{y:04}-{m:02}-{d:02} {:02}:{:02} UTC",
        rem / 3600,
        (rem % 3600) / 60
    )
}

fn civil_from_days(z: i64) -> (i64, u32, u32) {
    let z = z + 719_468;
    let era = z.div_euclid(146_097);
    let doe = z.rem_euclid(146_097);
    let yoe = (doe - doe / 1460 + doe / 36_524 - doe / 146_096) / 365;
    let y = yoe + era * 400;
    let doy = doe - (365 * yoe + yoe / 4 - yoe / 100);
    let mp = (5 * doy + 2) / 153;
    let d = (doy - (153 * mp + 2) / 5 + 1) as u32;
    let m = if mp < 10 { mp + 3 } else { mp - 9 } as u32;
    (if m <= 2 { y + 1 } else { y }, m, d)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn phase(loss_permille: u32, rtt: f64, jitter: f64, received: usize) -> PhaseResult {
        PhaseResult {
            plan: PhasePlan {
                rate_per_sec: 10,
                seconds: 1.0,
                payload: 0,
            },
            sent: 100,
            received,
            loss_permille,
            rtt_min_ms: rtt,
            rtt_median_ms: rtt,
            rtt_p95_ms: rtt,
            rtt_max_ms: rtt,
            jitter_ms: jitter,
            reordered: 0,
            longest_gap: 0,
        }
    }

    #[test]
    fn verdict_rules() {
        let good = phase(0, 40.0, 2.0, 100);
        assert_eq!(CheckReport::judge(Some(&good), Some(&good)), Verdict::Good);
        let far = phase(0, 250.0, 2.0, 100);
        assert_eq!(
            CheckReport::judge(Some(&far), Some(&far)),
            Verdict::HighLatency
        );
        let wobbly = phase(30, 60.0, 5.0, 97);
        assert_eq!(
            CheckReport::judge(Some(&good), Some(&wobbly)),
            Verdict::Fluctuating
        );
        let jittery = phase(0, 60.0, 45.0, 100);
        assert_eq!(
            CheckReport::judge(Some(&jittery), Some(&good)),
            Verdict::Fluctuating
        );
        let bad = phase(180, 90.0, 20.0, 82);
        assert_eq!(
            CheckReport::judge(Some(&bad), Some(&bad)),
            Verdict::Congested
        );
        let limited = phase(200, 90.0, 20.0, 80);
        assert_eq!(
            CheckReport::judge(Some(&good), Some(&limited)),
            Verdict::RateLimited
        );
        let dead = phase(1000, 0.0, 0.0, 0);
        assert_eq!(
            CheckReport::judge(Some(&dead), Some(&dead)),
            Verdict::UdpBlocked
        );
        assert_eq!(CheckReport::judge(None, Some(&good)), Verdict::Incomplete);
    }

    #[test]
    fn summary_counts_loss_reordering_and_gaps() {
        let mut log = ProbeLog::default();
        let t0 = Instant::now();
        let seqs: Vec<u32> = (0..10).collect();
        for s in &seqs {
            log.sent(*s, t0 + Duration::from_millis(u64::from(*s) * 10));
        }
        // 3、4、5 没回来；7 比 6 先到；9 的回包重复一次。
        for (s, at_ms) in [
            (0, 20),
            (1, 32),
            (2, 41),
            (7, 95),
            (6, 96),
            (8, 110),
            (9, 121),
            (9, 122),
        ] {
            log.received(s, t0 + Duration::from_millis(at_ms));
        }
        let r = log.summarize(
            PhasePlan {
                rate_per_sec: 100,
                seconds: 0.1,
                payload: 0,
            },
            &seqs,
        );
        assert_eq!((r.sent, r.received), (10, 7));
        assert_eq!(r.loss_permille, 300);
        assert_eq!(r.reordered, 1);
        assert_eq!(r.longest_gap, 3);
        assert!((r.rtt_min_ms - 20.0).abs() < 0.5, "{}", r.rtt_min_ms);
        assert!(r.rtt_max_ms > r.rtt_min_ms);
    }

    #[test]
    fn report_text_has_no_code_and_names_the_verdict() {
        let report = CheckReport {
            host: "voice.example.com".into(),
            port: 20800,
            started_unix_ms: 1_700_000_000_000,
            duration: Duration::from_secs(12),
            client_version: "gouhuo 0.3.3".into(),
            tcp_connect: Duration::from_millis(48),
            tls_handshake: Duration::from_millis(97),
            server_probe_aware: true,
            server_tcp_voice: true,
            tls_rtt_ms: vec![50.0, 52.0, 49.0],
            slow: Some(phase(0, 48.0, 1.5, 50)),
            fast: Some(phase(150, 60.0, 12.0, 425)),
            verdict: Verdict::RateLimited,
        };
        let text = report.to_text();
        assert!(text.contains("限流"));
        assert!(text.contains("voice.example.com:20800"));
        assert!(text.contains("丢包 15.0%"));
        assert!(text.contains("2023-11-14"));
        assert!(!text.contains("旧版本"));
    }

    #[test]
    fn plan_counts() {
        assert_eq!(CheckPlan::default().slow.count(), 50);
        assert_eq!(CheckPlan::default().fast.count(), 500);
    }
}
