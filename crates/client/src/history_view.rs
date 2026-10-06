// SPDX-License-Identifier: GPL-3.0-or-later

//! Human-readable projection of metadata-only history; no filesystem operations.
use client_process::history::{Phase, Snapshot};
use protocol::connection::{ConnectionReason as Reason, EvidenceSource};

pub struct View {
    pub summary: String,
    pub report: String,
    pub warning: String,
}

pub fn project(snapshot: &Snapshot) -> View {
    let summary = if snapshot.loading {
        "正在读取历史…".into()
    } else if let Some(latest) = snapshot.records.back() {
        format!(
            "最近状态：{} · {}",
            phase(latest.phase),
            date(latest.utc_unix_ms)
        )
    } else {
        "还没有连接记录".into()
    };
    let warning = if snapshot.dropped_queue_records > 0
        || snapshot.rejected_commands > 0
        || snapshot.disk_failures > 0
        || snapshot.invalid_disk_documents > 0
        || snapshot.clear_failed
    {
        format!(
            "部分记录或操作未完成：队列丢弃 {} 条，操作拒绝 {} 次，磁盘故障 {} 次，损坏文件 {} 份。{}",
            snapshot.dropped_queue_records, snapshot.rejected_commands,
            snapshot.disk_failures, snapshot.invalid_disk_documents,
            if snapshot.clear_failed { "历史清除失败，原记录可能仍然保留。" } else { "" }
        )
    } else {
        String::new()
    };
    let mut report = String::new();
    for record in snapshot.records.iter().rev() {
        let id: String = record.journey[..4]
            .iter()
            .map(|b| format!("{b:02x}"))
            .collect();
        report.push_str(&format!(
            "{}\n{} · 本机连接 {} · 第 {} 代\n",
            date(record.utc_unix_ms),
            phase(record.phase),
            id,
            record.generation
        ));
        if record.attempt > 0 {
            report.push_str(&format!("重连第 {} 次", record.attempt));
            if let Some(wait) = record.retry_in_ms {
                report.push_str(&format!("，等待 {:.1} 秒", wait as f64 / 1000.0));
            }
            report.push('\n');
        }
        if let Some(cause) = record.cause {
            report.push_str(&format!(
                "{} · {}\n",
                reason(cause.reason),
                match cause.source {
                    EvidenceSource::LocalObservation => "本机观测",
                    EvidenceSource::ServerConfirmed => "服务端明确告知",
                    EvidenceSource::UserAction => "用户操作",
                }
            ));
        }
        report.push('\n');
    }
    if report.is_empty() {
        report = "加入服务器后，连接状态会自动记录在这里。".into();
    }
    View {
        summary,
        report,
        warning,
    }
}

fn phase(value: Phase) -> &'static str {
    match value {
        Phase::Preparing => "准备连接",
        Phase::AwaitingTrust => "等待核对服务器指纹",
        Phase::AwaitingCode => "等待加入码",
        Phase::Connected => "服务器已连接",
        Phase::ReconnectWaiting => "等待重连",
        Phase::Restored => "服务器连接已恢复，语音待确认",
        Phase::Ended => "连接结束",
    }
}

fn reason(value: Reason) -> &'static str {
    match value {
        Reason::Unknown => "原因未确定",
        Reason::UserLeft => "主动离开",
        Reason::JoinCancelled => "取消加入",
        Reason::ApplicationExit => "退出应用",
        Reason::ReconnectCancelled => "取消重连",
        Reason::Kicked => "被请出服务器",
        Reason::Banned => "被服务器封禁",
        Reason::Displaced => "同一身份在另一处登录",
        Reason::InvalidInvite => "地址或邀请无法使用",
        Reason::InviteRequired => "需要有效的加入码",
        Reason::AuthenticationFailed => "身份验证未通过",
        Reason::VersionMismatch => "协议版本不兼容",
        Reason::ServerFull => "服务器已满",
        Reason::ServerInternal => "服务器报告内部错误",
        Reason::ConnectionTimeout => "建立连接超时",
        Reason::ConnectionRefused => "连接请求被拒绝",
        Reason::NetworkError => "网络连接出错",
        Reason::HeartbeatTimeout => "长时间未收到服务器响应",
        Reason::RemoteClosed => "远端关闭连接",
        Reason::ReadError => "读取连接时出错",
        Reason::WriteError => "发送控制消息失败",
        Reason::TlsError => "加密连接建立失败",
        Reason::CertificateMismatch => "服务器证书不匹配",
        Reason::ProtocolError => "收到的内容不符合协议",
        Reason::ControlBackpressure => "控制消息积压",
        Reason::ControlQueueUnavailable => "控制消息队列不可用",
        Reason::TransportRestartRequested => "本机请求重新建立连接",
    }
}

/// UTC Gregorian date, extending the existing client's civil_from_days formula.
fn date(timestamp_ms: u64) -> String {
    if timestamp_ms > 253_402_300_799_999 {
        return "时间不可用".into();
    }
    let seconds = (timestamp_ms / 1000) as i64;
    let z = seconds / 86_400 + 719_468;
    let era = z / 146_097;
    let doe = z - era * 146_097;
    let yoe = (doe - doe / 1460 + doe / 36_524 - doe / 146_096) / 365;
    let doy = doe - (365 * yoe + yoe / 4 - yoe / 100);
    let mp = (5 * doy + 2) / 153;
    let day = doy - (153 * mp + 2) / 5 + 1;
    let month = if mp < 10 { mp + 3 } else { mp - 9 };
    let year = yoe + era * 400 + i64::from(month <= 2);
    let time = seconds % 86_400;
    format!(
        "{year:04}-{month:02}-{day:02} {:02}:{:02}:{:02} UTC",
        time / 3600,
        time / 60 % 60,
        time % 60
    )
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn dates_include_year_seconds_and_explicit_utc() {
        assert_eq!(date(0), "1970-01-01 00:00:00 UTC");
        assert_eq!(date(951_782_400_000), "2000-02-29 00:00:00 UTC");
        assert_eq!(date(1_704_067_200_000), "2024-01-01 00:00:00 UTC");
        assert_eq!(date(253_402_300_799_999), "9999-12-31 23:59:59 UTC");
        assert_eq!(date(u64::MAX), "时间不可用");
    }
    #[test]
    fn incomplete_history_and_unknown_causes_remain_explicit() {
        let view = project(&Snapshot {
            dropped_queue_records: 2,
            ..Snapshot::default()
        });
        assert!(view.warning.contains("丢弃 2 条"));
        assert_eq!(reason(Reason::Unknown), "原因未确定");
        assert!(phase(Phase::Restored).contains("语音待确认"));
    }

    #[test]
    fn rejected_operations_and_failed_clear_are_visible_without_record_loss() {
        let rejected = project(&Snapshot {
            rejected_commands: 1,
            ..Snapshot::default()
        });
        assert!(rejected.warning.contains("操作拒绝 1 次"));
        let failed_clear = project(&Snapshot {
            clear_failed: true,
            ..Snapshot::default()
        });
        assert!(failed_clear.warning.contains("历史清除失败"));
        assert!(project(&Snapshot::default()).warning.is_empty());
    }
}
