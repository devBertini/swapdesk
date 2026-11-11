//! Dates, durations and device status lines, shared by the shells so both read alike.

use crate::api::{DesktopKind, DeviceStatus, DeviceView, Language, Msg, Os};
use crate::strings::{MsgKey, text};

/// "Now" as the shell knows it: milliseconds since the Unix epoch and the offset of local time from UTC.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub struct Clock {
    pub now_ms: u64,
    pub utc_offset_min: i32,
}

/// A local calendar moment.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct LocalTime {
    /// Days since 1970-01-01 in local time.
    pub day: i64,
    pub month: u32,
    pub day_of_month: u32,
    pub hour: u32,
    pub minute: u32,
}

/// Howard Hinnant's `civil_from_days`: (month, day) of a day count since the epoch.
fn month_day(days: i64) -> (u32, u32) {
    let z = days + 719_468;
    let era = z.div_euclid(146_097);
    let doe = z.rem_euclid(146_097);
    let yoe = (doe - doe / 1460 + doe / 36_524 - doe / 146_096) / 365;
    let doy = doe - (365 * yoe + yoe / 4 - yoe / 100);
    let mp = (5 * doy + 2) / 153;
    let d = (doy - (153 * mp + 2) / 5 + 1) as u32;
    let m = if mp < 10 { mp + 3 } else { mp - 9 } as u32;
    let _ = era;
    (m, d)
}

pub fn local_time(at_ms: u64, utc_offset_min: i32) -> LocalTime {
    let secs = (at_ms / 1000) as i64 + i64::from(utc_offset_min) * 60;
    let day = secs.div_euclid(86_400);
    let in_day = secs.rem_euclid(86_400);
    let (month, day_of_month) = month_day(day);
    LocalTime { day, month, day_of_month, hour: (in_day / 3600) as u32, minute: ((in_day % 3600) / 60) as u32 }
}

fn hhmm(t: LocalTime) -> String {
    format!("{:02}:{:02}", t.hour, t.minute)
}

fn date(lang: Language, t: LocalTime) -> String {
    match lang {
        Language::PtBr => format!("{:02}/{:02}", t.day_of_month, t.month),
        Language::En => format!("{:02}/{:02}", t.month, t.day_of_month),
    }
}

/// How many local days before today `at_ms` falls (0 today, 1 yesterday).
fn days_ago(at: LocalTime, clock: Clock) -> i64 {
    local_time(clock.now_ms, clock.utc_offset_min).day - at.day
}

/// "Hoje às 15:25", "Ontem às 23:40", "05/11 às 10:30".
pub fn when_phrase(lang: Language, at_ms: u64, clock: Clock) -> String {
    let t = local_time(at_ms, clock.utc_offset_min);
    let msg = match days_ago(t, clock) {
        0 => Msg::new(MsgKey::WhenToday).with(hhmm(t)),
        1 => Msg::new(MsgKey::WhenYesterday).with(hhmm(t)),
        _ => Msg::new(MsgKey::WhenDate).with(date(lang, t)).with(hhmm(t)),
    };
    text(lang, &msg)
}

/// "Hoje, 15:25", "Ontem, 23:40", "05/11, 10:30".
pub fn table_when(lang: Language, at_ms: u64, clock: Clock) -> String {
    let t = local_time(at_ms, clock.utc_offset_min);
    let msg = match days_ago(t, clock) {
        0 => Msg::new(MsgKey::TableToday).with(hhmm(t)),
        1 => Msg::new(MsgKey::TableYesterday).with(hhmm(t)),
        _ => Msg::new(MsgKey::TableDate).with(date(lang, t)).with(hhmm(t)),
    };
    text(lang, &msg)
}

/// "6 minutos", "1 minuto", "menos de um minuto".
pub fn minutes_phrase(lang: Language, secs: u32) -> String {
    let minutes = secs / 60;
    let msg = match minutes {
        0 => Msg::new(MsgKey::MinutesLess),
        1 => Msg::new(MsgKey::MinutesOne),
        n => Msg::new(MsgKey::MinutesMany).with(n.to_string()),
    };
    text(lang, &msg)
}

/// "17 min" (at least 1 min).
pub fn short_minutes(lang: Language, secs: u32) -> String {
    text(lang, &Msg::new(MsgKey::DurMinutes).with((secs / 60).max(1).to_string()))
}

/// "1:42" for a countdown.
pub fn countdown(secs: u16) -> String {
    format!("{}:{:02}", secs / 60, secs % 60)
}

pub fn os_label(lang: Language, os: Os, version: &str) -> String {
    let name = match os {
        Os::Windows => "Windows",
        Os::MacOs => "macOS",
    };
    text(lang, &Msg::new(MsgKey::OsLabel).with(name).with(version)).trim().to_owned()
}

/// The second line of a sidebar row: "Online, macOS 15", "Dormindo, Windows 11", "Não pareado".
pub fn status_line(lang: Language, d: &DeviceView) -> String {
    let os = os_label(lang, d.os, &d.os_version);
    let msg = match &d.status {
        DeviceStatus::Online { .. } => Msg::new(MsgKey::StatusOnline).with(os),
        DeviceStatus::Asleep => Msg::new(MsgKey::StatusAsleep).with(os),
        DeviceStatus::Offline => Msg::new(MsgKey::StatusOffline).with(os),
        DeviceStatus::OnNetworkUnpaired { pairing_open: true } => Msg::new(MsgKey::StatusUnpairedOpen),
        DeviceStatus::OnNetworkUnpaired { pairing_open: false } => Msg::new(MsgKey::StatusUnpaired),
    };
    text(lang, &msg)
}

/// The line under the title of a device: "Online em 192.168.0.21, resposta em 3 ms".
pub fn detail_line(lang: Language, d: &DeviceView) -> String {
    let msg = match &d.status {
        DeviceStatus::Online { address, rtt_ms } => {
            Msg::new(MsgKey::DetailOnline).with(address.clone()).with(rtt_ms.to_string())
        }
        DeviceStatus::Asleep if d.wakeable => Msg::new(MsgKey::DetailAsleep),
        DeviceStatus::Asleep => Msg::new(MsgKey::DetailAsleepNoWake),
        DeviceStatus::Offline => Msg::new(MsgKey::DetailOffline),
        DeviceStatus::OnNetworkUnpaired { .. } => Msg::new(MsgKey::DetailUnpaired),
    };
    text(lang, &msg)
}

pub fn where_phrase(lang: Language, desktop: DesktopKind) -> String {
    let key = match desktop {
        DesktopKind::Default => MsgKey::WhereDefault,
        DesktopKind::Secure => MsgKey::WhereSecure,
        DesktopKind::Locked | DesktopKind::AccountAway => MsgKey::WhereLocked,
        DesktopKind::LoginWindow => MsgKey::WhereLogin,
    };
    text(lang, &Msg::new(key))
}

#[cfg(test)]
mod tests {
    use super::*;

    /// 2026-11-08 15:30 UTC.
    const NOW: u64 = 1_794_151_800_000;

    fn clock() -> Clock {
        Clock { now_ms: NOW, utc_offset_min: 0 }
    }

    #[test]
    fn local_time_of_a_known_instant() {
        let t = local_time(NOW, 0);
        assert_eq!((t.month, t.day_of_month, t.hour, t.minute), (11, 8, 15, 30));
        let t = local_time(NOW, -180);
        assert_eq!((t.month, t.day_of_month, t.hour, t.minute), (11, 8, 12, 30));
    }

    #[test]
    fn today_yesterday_and_older() {
        assert_eq!(table_when(Language::PtBr, NOW - 5 * 60_000, clock()), "Hoje, 15:25");
        assert_eq!(table_when(Language::PtBr, NOW - 16 * 3_600_000, clock()), "Ontem, 23:30");
        assert_eq!(table_when(Language::PtBr, NOW - 3 * 86_400_000, clock()), "05/11, 15:30");
        assert_eq!(table_when(Language::En, NOW - 3 * 86_400_000, clock()), "11/05, 15:30");
        assert_eq!(when_phrase(Language::PtBr, NOW - 16 * 3_600_000, clock()), "Ontem às 23:30");
    }

    #[test]
    fn durations_and_countdowns() {
        assert_eq!(minutes_phrase(Language::PtBr, 360), "6 minutos");
        assert_eq!(minutes_phrase(Language::PtBr, 70), "1 minuto");
        assert_eq!(minutes_phrase(Language::En, 20), "less than a minute");
        assert_eq!(short_minutes(Language::PtBr, 17 * 60), "17 min");
        assert_eq!(countdown(102), "1:42");
        assert_eq!(countdown(9), "0:09");
    }
}
