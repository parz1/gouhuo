// SPDX-License-Identifier: GPL-3.0-or-later

//! 每个人的网络状况怎么变成界面上那几个字。
//!
//! 数据来自三处，含义不同，界面上要分开说：
//!
//! - **上行丢包**：服务端按序号缺口量的，是「他说的话有多少没送到服务器」。
//!   旧客户端也有，因为不靠他配合。
//! - **RTT、下行丢包**：那个人在 Ping 里自报的。
//! - **我收到他的丢包**：本机播放链路量的，只对正在说话的人有。
//!
//! 一个人「网络差」的判定取这几项里最差的。规则在 [`level_of`]，不散在界面里。

use client_core::{Roster, VoiceReport};
use voice_types::VoiceStats;

use crate::RuntimeSnapshot;

/// 0 不知道，1 好，2 有波动，3 差。界面按这个上色。
pub const NET_UNKNOWN: i32 = 0;
pub const NET_GOOD: i32 = 1;
pub const NET_DEGRADED: i32 = 2;
pub const NET_POOR: i32 = 3;

/// 丢包到这个千分比算「有波动」/「差」。
pub const DEGRADED_LOSS_PERMILLE: u32 = 20;
pub const POOR_LOSS_PERMILLE: u32 = 100;
/// 往返到这个毫秒数算「有波动」/「差」。
pub const DEGRADED_RTT_MS: u32 = 150;
pub const POOR_RTT_MS: u32 = 300;
/// 接收统计至少要有这么多帧才拿出来说。
pub const MIN_HEARD_FRAMES: u64 = 20;

#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct MemberNetView {
    pub level: i32,
    pub rtt_ms: Option<u32>,
    /// 服务端量的：他到服务器。
    pub uplink_loss_permille: Option<u32>,
    /// 他自报的：服务器到他。
    pub downlink_loss_permille: Option<u32>,
    /// 本机量的：他的声音到我这里。
    pub heard_loss_permille: Option<u32>,
    pub tcp_voice: bool,
    /// 服务端没给报告，这是本地凭 Pong 估的。
    pub estimated: bool,
}

pub fn level_of(rtt_ms: Option<u32>, loss_permille: Option<u32>) -> i32 {
    if rtt_ms.is_none() && loss_permille.is_none() {
        return NET_UNKNOWN;
    }
    let loss = loss_permille.unwrap_or(0);
    let rtt = rtt_ms.unwrap_or(0);
    if loss >= POOR_LOSS_PERMILLE || rtt >= POOR_RTT_MS {
        NET_POOR
    } else if loss >= DEGRADED_LOSS_PERMILLE || rtt >= DEGRADED_RTT_MS {
        NET_DEGRADED
    } else {
        NET_GOOD
    }
}

impl MemberNetView {
    /// 这个人最差的那个丢包数。自己看的是上行和下行；看别人是他的上行和我收到的。
    pub fn worst_loss(&self, is_me: bool) -> Option<u32> {
        let candidates = if is_me {
            [self.uplink_loss_permille, self.downlink_loss_permille]
        } else {
            [self.uplink_loss_permille, self.heard_loss_permille]
        };
        candidates.into_iter().flatten().max()
    }

    /// 名单那一行后面的几个字。没数据就是空串，界面什么都不画。
    pub fn summary(&self, is_me: bool) -> String {
        let mut parts = Vec::new();
        if let Some(rtt) = self.rtt_ms {
            parts.push(format!("{rtt} ms"));
        }
        if is_me {
            // 自己的两个方向分开写：收听丢包多半是**别人**的上行差，
            // 混成一个「丢 X%」会让人以为是自己的线路。
            if let Some(loss) = self.uplink_loss_permille {
                parts.push(format!("上行丢{}", percent(loss)));
            }
            if let Some(loss) = self.downlink_loss_permille {
                parts.push(format!("收听丢{}", percent(loss)));
            }
        } else if let Some(loss) = self.worst_loss(is_me) {
            parts.push(format!("丢{}", percent(loss)));
        }
        if self.tcp_voice {
            parts.push("兼容连接".into());
        }
        parts.join(" · ")
    }

    /// 成员菜单里的几行。
    pub fn detail(&self, is_me: bool) -> String {
        let who = if is_me { "我" } else { "对方" };
        let mut lines = Vec::new();
        if let Some(rtt) = self.rtt_ms {
            lines.push(format!("往返 {rtt} ms（{who}到服务器）"));
        }
        if let Some(loss) = self.uplink_loss_permille {
            let how = if self.estimated {
                "本机估算"
            } else {
                "服务端测"
            };
            lines.push(format!("上行丢包 {}（{how}）", percent(loss)));
        }
        if let Some(loss) = self.downlink_loss_permille {
            if is_me {
                lines.push(format!("收听丢包 {}（本机测）", percent(loss)));
            } else {
                lines.push(format!("对方收听丢包 {}（对方自报）", percent(loss)));
            }
        }
        if let Some(loss) = self.heard_loss_permille {
            lines.push(format!("我收到对方的丢包 {}（本机测）", percent(loss)));
        }
        if self.tcp_voice {
            lines.push(format!("{who}走 TLS 兼容连接，延迟可能增加"));
        }
        if lines.is_empty() {
            return "暂无网络数据：对方还没说话，或者服务端版本太旧不发报告。".into();
        }
        lines.join("\n")
    }
}

fn percent(permille: u32) -> String {
    if permille % 10 == 0 {
        format!("{}%", permille / 10)
    } else {
        format!("{:.1}%", permille as f64 / 10.0)
    }
}

/// 把名单里的报告和本机的接收统计拼成一个人的视图。
pub fn project_network(
    session: u32,
    is_me: bool,
    roster: &Roster,
    snapshot: &RuntimeSnapshot,
) -> MemberNetView {
    let mut view = MemberNetView::default();
    if let Some(reported) = roster.network_of(session) {
        view.rtt_ms = reported.rtt_known.then_some(reported.rtt_ms);
        view.uplink_loss_permille = reported
            .uplink_known
            .then_some(reported.uplink_loss_permille.min(1000));
        view.downlink_loss_permille = reported
            .downlink_known
            .then_some(reported.downlink_loss_permille.min(1000));
        view.tcp_voice = reported.tcp_voice;
        view.estimated = !roster.network_from_server;
    }
    if !is_me {
        view.heard_loss_permille = snapshot
            .voice
            .as_ref()
            .and_then(|stats| heard_loss(stats, session));
    }
    view.level = level_of(view.rtt_ms, view.worst_loss(is_me));
    view
}

fn heard_loss(stats: &VoiceStats, session: u32) -> Option<u32> {
    let speaker = stats
        .receive_quality
        .iter()
        .find(|s| s.session == session)?;
    let q = &speaker.quality;
    let total = q.played + q.missing;
    if total < MIN_HEARD_FRAMES {
        return None;
    }
    Some(((q.missing * 1000) / total).min(1000) as u32)
}

/// 心跳里要报给服务端的那几个数，从语音快照里取。
///
/// 下行丢包把所有说话人合在一起算；没人说话就是不知道，不报零。
pub fn voice_report(stats: Option<&VoiceStats>) -> VoiceReport {
    let Some(stats) = stats else {
        return VoiceReport::default();
    };
    let (played, missing) = stats.receive_quality.iter().fold((0u64, 0u64), |acc, s| {
        (acc.0 + s.quality.played, acc.1 + s.quality.missing)
    });
    let total = played + missing;
    VoiceReport {
        packets_sent: stats.packets_sent,
        rtt_ms: (stats.udp_ok && !stats.tcp_active && stats.rtt_ms > 0.0)
            .then(|| stats.rtt_ms.round().clamp(0.0, u32::MAX as f64) as u32),
        downlink_loss_permille: (total >= MIN_HEARD_FRAMES)
            .then(|| ((missing * 1000) / total).min(1000) as u32),
        tcp_voice: stats.tcp_active,
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use voice_types::{ReceiveQuality, SpeakerQuality};

    #[test]
    fn levels_follow_the_worst_number() {
        assert_eq!(level_of(None, None), NET_UNKNOWN);
        assert_eq!(level_of(Some(40), Some(0)), NET_GOOD);
        assert_eq!(level_of(Some(40), Some(25)), NET_DEGRADED);
        assert_eq!(level_of(Some(160), None), NET_DEGRADED);
        assert_eq!(level_of(Some(40), Some(120)), NET_POOR);
        assert_eq!(level_of(Some(320), Some(0)), NET_POOR);
    }

    #[test]
    fn summary_and_detail_say_where_each_number_comes_from() {
        let view = MemberNetView {
            level: NET_POOR,
            rtt_ms: Some(180),
            uplink_loss_permille: Some(123),
            downlink_loss_permille: Some(30),
            heard_loss_permille: Some(80),
            tcp_voice: false,
            estimated: false,
        };
        assert_eq!(view.summary(false), "180 ms · 丢12.3%");
        assert_eq!(view.summary(true), "180 ms · 上行丢12.3% · 收听丢3%");
        let detail = view.detail(false);
        assert!(detail.contains("上行丢包 12.3%（服务端测）"), "{detail}");
        assert!(detail.contains("我收到对方的丢包 8%（本机测）"), "{detail}");
        assert!(detail.contains("对方收听丢包 3%（对方自报）"), "{detail}");
        assert_eq!(MemberNetView::default().summary(true), "");
        assert!(MemberNetView::default().detail(false).contains("暂无"));
        let tcp = MemberNetView {
            tcp_voice: true,
            ..MemberNetView::default()
        };
        assert_eq!(tcp.summary(false), "兼容连接");
    }

    #[test]
    fn voice_report_aggregates_speakers_and_withholds_unknowns() {
        assert_eq!(voice_report(None), VoiceReport::default());
        let quality = |played, missing| ReceiveQuality {
            played,
            missing,
            ..ReceiveQuality::default()
        };
        let stats = VoiceStats {
            udp_ok: true,
            rtt_ms: 47.6,
            packets_sent: 900,
            receive_quality: vec![
                SpeakerQuality {
                    session: 2,
                    quality: quality(90, 10),
                },
                SpeakerQuality {
                    session: 3,
                    quality: quality(95, 5),
                },
            ],
            ..VoiceStats::default()
        };
        let report = voice_report(Some(&stats));
        assert_eq!(report.packets_sent, 900);
        assert_eq!(report.rtt_ms, Some(48));
        assert_eq!(report.downlink_loss_permille, Some(75));
        assert!(!report.tcp_voice);

        let silent = VoiceStats {
            udp_ok: false,
            receive_quality: vec![SpeakerQuality {
                session: 2,
                quality: quality(5, 1),
            }],
            ..VoiceStats::default()
        };
        let report = voice_report(Some(&silent));
        assert_eq!(report.rtt_ms, None, "UDP 没通就没有 RTT");
        assert_eq!(report.downlink_loss_permille, None, "样本不够不报");
    }
}
