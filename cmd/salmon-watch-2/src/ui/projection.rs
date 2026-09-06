use std::rc::Rc;

use chrono::{Datelike, Local, NaiveDateTime, TimeZone, Timelike, Utc};
use slint::{ModelRc, VecModel};

use super::{IncidentView, MainWindow, SalmonTray, ServerView};
use crate::domain::{Incident, IncidentState, OverallState as DomainOverallState, UiSnapshot};
use crate::tray::{OverallState, TrayIcons, TrayState};

pub fn apply_snapshot(
    window: &MainWindow,
    tray: &SalmonTray,
    icons: &TrayIcons,
    snapshot: UiSnapshot,
) {
    let now_millis = Utc::now().timestamp_millis();
    let online = snapshot
        .servers
        .iter()
        .filter(|server| server.connected)
        .count();
    window.set_server_summary(
        format!("{online}/{} servers are online", snapshot.servers.len()).into(),
    );
    window.set_online_server_count(online as i32);
    let servers: Vec<_> = snapshot
        .servers
        .iter()
        .map(|server| ServerView {
            id: server.id.clone().into(),
            connection: if server.connected {
                "online"
            } else if server.initialized {
                "offline"
            } else {
                "connecting"
            }
            .into(),
            last_heartbeat: format_unix_timestamp(server.last_heartbeat_at, now_millis).into(),
            connected: server.connected,
        })
        .collect();
    window.set_servers(ModelRc::from(Rc::new(VecModel::from(servers))));
    let active: Vec<_> = snapshot
        .active
        .iter()
        .map(|incident| incident_view(incident, None, now_millis))
        .collect();
    window.set_active_incidents(ModelRc::from(Rc::new(VecModel::from(active))));
    let snoozed: Vec<_> = snapshot
        .snoozed
        .iter()
        .map(|item| incident_view(&item.incident, Some(item.until), now_millis))
        .collect();
    window.set_snoozed_incidents(ModelRc::from(Rc::new(VecModel::from(snoozed))));

    let tray_state = TrayState {
        alerting: overall(snapshot.alerting_state),
        unknown_server_count: snapshot.unknown_server_count,
        server_count: snapshot.servers.len(),
        alerting_count: snapshot.active.len(),
        snoozed: snapshot.snoozed_state.map(overall),
        snoozed_count: snapshot.snoozed.len(),
    };
    if let Ok(icon) = icons.icon(tray_state) {
        window.set_current_tray_icon(icon.clone());
        tray.set_tray_icon(icon);
    }
    tray.set_status_title(tray_state.status_title().into());
    window.set_tray_flashing(tray_state.is_flashing());
}

fn incident_view(incident: &Incident, snoozed_until: Option<i64>, now_millis: i64) -> IncidentView {
    let state = match incident.state {
        IncidentState::Ok => "ok",
        IncidentState::Warning => "warning",
        IncidentState::Error => "error",
    };
    let suffix = if incident.stale {
        " (STALE)"
    } else if snoozed_until.is_some() {
        " (SNOOZED)"
    } else {
        ""
    };
    IncidentView {
        key: incident.key.clone().into(),
        state: format!("{state}{suffix}").into(),
        details: incident.details.clone().into(),
        started_at: display_wire_time(&incident.incident_started_at, now_millis).into(),
        snoozed_until: snoozed_until
            .map(|until| format_unix_timestamp(Some(until), now_millis))
            .unwrap_or_default()
            .into(),
        severity: match incident.state {
            IncidentState::Ok => 1,
            IncidentState::Warning => 2,
            IncidentState::Error => 3,
        },
        stale: incident.stale,
        snoozed: snoozed_until.is_some(),
    }
}

fn display_wire_time(value: &str, now_millis: i64) -> String {
    if value.is_empty() {
        return "never".into();
    }
    if let Ok(timestamp) =
        time::OffsetDateTime::parse(value, &time::format_description::well_known::Rfc3339)
    {
        let millis = timestamp.unix_timestamp_nanos().div_euclid(1_000_000);
        i64::try_from(millis)
            .map(|millis| format_timestamp_millis(millis, now_millis))
            .unwrap_or_else(|_| value.to_owned())
    } else if let Ok(timestamp) = value.parse::<i64>() {
        format_unix_timestamp(Some(timestamp), now_millis)
    } else {
        value.to_owned()
    }
}

fn format_unix_timestamp(value: Option<i64>, now_millis: i64) -> String {
    let Some(value) = value else {
        return "never".into();
    };
    format_timestamp_millis(value.saturating_mul(1_000), now_millis)
}

fn format_timestamp_millis(value_millis: i64, now_millis: i64) -> String {
    let Some(timestamp) = Local.timestamp_millis_opt(value_millis).single() else {
        return value_millis.to_string();
    };
    let Some(current) = Local.timestamp_millis_opt(now_millis).single() else {
        return value_millis.to_string();
    };
    format!(
        "{} ({})",
        format_absolute(timestamp.naive_local(), current.naive_local()),
        format_relative(value_millis, now_millis)
    )
}

fn format_absolute(value: NaiveDateTime, now: NaiveDateTime) -> String {
    let time = format!(
        "{:02}:{:02}:{:02}",
        value.hour(),
        value.minute(),
        value.second()
    );
    if value.date() == now.date() {
        return time;
    }
    let date = format!("{} {}", month_name(value.month()), value.day());
    if value.year() == now.year() {
        format!("{date}, {time}")
    } else {
        format!("{date}, {}, {time}", value.year())
    }
}

fn month_name(month: u32) -> &'static str {
    const MONTHS: [&str; 12] = [
        "Jan", "Feb", "Mar", "Apr", "May", "Jun", "Jul", "Aug", "Sep", "Oct", "Nov", "Dec",
    ];
    MONTHS[(month.saturating_sub(1) as usize).min(MONTHS.len() - 1)]
}

fn format_relative(value_millis: i64, now_millis: i64) -> String {
    let elapsed_millis = i128::from(now_millis) - i128::from(value_millis);
    let future = elapsed_millis < 0;
    let magnitude = elapsed_millis.unsigned_abs();
    let seconds = if future {
        magnitude.div_ceil(1_000)
    } else {
        magnitude / 1_000
    } as i64;
    let duration = duration(seconds);
    if future {
        format!("in {duration}")
    } else {
        format!("{duration} ago")
    }
}

fn duration(seconds: i64) -> String {
    if seconds < 15 {
        "<15s".into()
    } else if seconds < 60 {
        format!("{}s", seconds / 15 * 15)
    } else if seconds < 3600 {
        format!("{}m", seconds / 60)
    } else if seconds < 86400 {
        let total_minutes = seconds / 60;
        let hours = total_minutes / 60;
        let minutes = total_minutes % 60;
        if minutes == 0 {
            format!("{hours}h")
        } else {
            format!("{hours}h {minutes}m")
        }
    } else {
        let total_hours = seconds / 3600;
        let days = total_hours / 24;
        let hours = total_hours % 24;
        if hours == 0 {
            format!("{days}d")
        } else {
            format!("{days}d {hours}h")
        }
    }
}

fn overall(state: DomainOverallState) -> OverallState {
    match state {
        DomainOverallState::Unknown => OverallState::Unknown,
        DomainOverallState::Ok => OverallState::Ok,
        DomainOverallState::InternalError => OverallState::InternalError,
        DomainOverallState::Warning => OverallState::Warning,
        DomainOverallState::Error => OverallState::Error,
    }
}

#[cfg(test)]
mod tests {
    use chrono::NaiveDate;

    use super::*;

    fn date(year: i32, month: u32, day: u32, hour: u32, minute: u32, second: u32) -> NaiveDateTime {
        NaiveDate::from_ymd_opt(year, month, day)
            .unwrap()
            .and_hms_opt(hour, minute, second)
            .unwrap()
    }

    #[test]
    fn absolute_time_is_compact_and_adds_only_the_needed_date_parts() {
        let now = date(2026, 8, 26, 16, 16, 43);
        assert_eq!(format_absolute(date(2026, 8, 26, 1, 2, 3), now), "01:02:03");
        assert_eq!(
            format_absolute(date(2026, 8, 25, 1, 2, 3), now),
            "Aug 25, 01:02:03"
        );
        assert_eq!(
            format_absolute(date(2025, 8, 25, 1, 2, 3), now),
            "Aug 25, 2025, 01:02:03"
        );
    }

    #[test]
    fn relative_time_uses_the_same_buckets_as_the_web_client() {
        const NOW: i64 = 2_000_000_000;
        let cases = [
            (0, "<15s ago"),
            (14_999, "<15s ago"),
            (15_000, "15s ago"),
            (29_999, "15s ago"),
            (30_000, "30s ago"),
            (44_999, "30s ago"),
            (45_000, "45s ago"),
            (59_999, "45s ago"),
            (60_000, "1m ago"),
            (73 * 60_000, "1h 13m ago"),
            ((23 * 60 + 59) * 60_000, "23h 59m ago"),
            ((24 * 60 + 59) * 60_000, "1d ago"),
            (25 * 60 * 60_000, "1d 1h ago"),
        ];
        for (age, expected) in cases {
            assert_eq!(format_relative(NOW - age, NOW), expected);
        }
        assert_eq!(format_relative(NOW + 73 * 60_000, NOW), "in 1h 13m");
        assert_eq!(format_relative(NOW + 14_001, NOW), "in 15s");
    }

    #[test]
    fn missing_and_invalid_wire_timestamps_match_web_fallbacks() {
        assert_eq!(format_unix_timestamp(None, 1_000_000), "never");
        assert_eq!(display_wire_time("", 1_000_000), "never");
        assert_eq!(display_wire_time("not-a-time", 1_000_000), "not-a-time");
    }
}
