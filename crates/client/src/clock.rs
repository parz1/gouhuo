// SPDX-License-Identifier: GPL-3.0-or-later

//! Unix 时间 → 界面上的「今天 21:40」「9月27日」。
//!
//! 自己算而不是拉一个日期库进来：这里只要几种固定格式，而日期库会带着时区数据库进安装包。

pub(crate) fn unix_now() -> u64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_secs())
        .unwrap_or(0)
}

/// 「上次什么时候来的」：刚刚 / 今天 21:40 / 昨天 21:40 / 3 天前 / 9月27日。
///
/// `then` 是 0 表示不知道（从老版本升上来的那一条），什么都不写。
pub(crate) fn ago(now: u64, then: u64) -> String {
    if then == 0 || then > now + 60 {
        return String::new();
    }
    if now.saturating_sub(then) < 90 {
        return "刚刚".to_string();
    }
    let offset = local_offset_seconds();
    let day = |t: u64| (t as i64 + offset).div_euclid(86_400);
    let days = day(now) - day(then);
    match days {
        0 => format!("今天 {}", clock_time(then as i64 * 1000)),
        1 => format!("昨天 {}", clock_time(then as i64 * 1000)),
        2..=6 => format!("{days} 天前"),
        _ => clock_date(then as i64 * 1000),
    }
}

/// Unix 毫秒 → 本地的 `HH:MM`。
///
/// 自己算而不是拉一个日期库进来：这里只要「今天的几点几分」，
/// 而日期库会带着时区数据库一起进安装包。
pub(crate) fn clock_time(timestamp_ms: i64) -> String {
    // 用「本机现在的本地时间」和「本机现在的 UTC 时间」之差当时区偏移。
    // 对聊天时间戳足够了 —— 误差只会出现在跨夏令时切换的那一小时，
    // 而中国不用夏令时。
    let offset = local_offset_seconds();
    let local = timestamp_ms / 1000 + offset;
    let seconds_today = local.rem_euclid(86_400);
    format!(
        "{:02}:{:02}",
        seconds_today / 3600,
        (seconds_today % 3600) / 60
    )
}

/// 「9月27日」。封禁名单里用：封了多久比封在几点几分要紧。
pub(crate) fn clock_date(timestamp_ms: i64) -> String {
    let local = timestamp_ms.div_euclid(1000) + local_offset_seconds();
    let (month, day) = month_day(local.div_euclid(86_400));
    format!("{month}月{day}日")
}

/// 1970-01-01 起的第几天 → (月, 日)。
pub(crate) fn month_day(days: i64) -> (u32, u32) {
    let (_, month, day) = civil_from_days(days);
    (month, day)
}

/// 1970-01-01 起的第几天 → (年, 月, 日)。公历，Howard Hinnant 的 civil_from_days。
///
/// 不为这几个格式拉进一整个日期库。
pub(crate) fn civil_from_days(days: i64) -> (i64, u32, u32) {
    let z = days + 719_468;
    let era = z.div_euclid(146_097);
    let doe = z - era * 146_097;
    let yoe = (doe - doe / 1460 + doe / 36_524 - doe / 146_096) / 365;
    let doy = doe - (365 * yoe + yoe / 4 - yoe / 100);
    let mp = (5 * doy + 2) / 153;
    let day = (doy - (153 * mp + 2) / 5 + 1) as u32;
    let month = if mp < 10 { mp + 3 } else { mp - 9 } as u32;
    let year = yoe + era * 400 + i64::from(month <= 2);
    (year, month, day)
}

#[cfg(windows)]
pub(crate) fn local_offset_seconds() -> i64 {
    // SAFETY: GetTimeZoneInformation 只写它自己的输出结构，没有别的副作用。
    unsafe {
        let mut info =
            std::mem::zeroed::<windows_sys::Win32::System::Time::TIME_ZONE_INFORMATION>();
        let kind = windows_sys::Win32::System::Time::GetTimeZoneInformation(&mut info);
        // Bias 是「本地时间 + Bias = UTC」，单位是分钟，所以要取负
        let bias = match kind {
            2 => info.Bias + info.DaylightBias, // TIME_ZONE_ID_DAYLIGHT
            _ => info.Bias + info.StandardBias,
        };
        -(bias as i64) * 60
    }
}

#[cfg(not(windows))]
pub(crate) fn local_offset_seconds() -> i64 {
    0
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn month_day_matches_known_dates() {
        assert_eq!(month_day(0), (1, 1)); // 1970-01-01
        assert_eq!(month_day(59), (3, 1)); // 1970 不是闰年
        assert_eq!(month_day(11_016), (2, 29)); // 2000-02-29
        assert_eq!(month_day(19_675), (11, 14)); // 2023-11-14
        assert_eq!(month_day(-1), (12, 31)); // 1969-12-31
    }

    #[test]
    fn clock_time_is_hh_mm() {
        let text = clock_time(1_700_000_000_000);
        assert_eq!(text.len(), 5, "{text}");
        assert_eq!(&text[2..3], ":");
        let hour: u32 = text[..2].parse().unwrap();
        let minute: u32 = text[3..].parse().unwrap();
        assert!(hour < 24);
        assert!(minute < 60);
    }

    /// 时间戳是服务端给的，不可信。0 和负数都不能让界面 panic。
    #[test]
    fn nonsense_timestamps_do_not_panic() {
        for stamp in [0, -1, i64::MIN, i64::MAX] {
            let text = clock_time(stamp);
            assert!(text.is_empty() || text.len() == 5, "{stamp} -> {text}");
        }
    }
}
