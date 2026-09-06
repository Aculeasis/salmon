use std::rc::Rc;

use slint::{ModelRc, VecModel};

use super::{IncidentView, MainWindow, SalmonTray, ServerView};
use crate::domain::{Incident, IncidentState, OverallState as DomainOverallState, UiSnapshot};
use crate::runtime::unix_now;
use crate::tray::{OverallState, TrayIcons, TrayState};

pub fn apply_snapshot(
    window: &MainWindow,
    tray: &SalmonTray,
    icons: &TrayIcons,
    snapshot: UiSnapshot,
) {
    let now = unix_now();
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
            last_heartbeat: relative_time(server.last_heartbeat_at, now).into(),
            connected: server.connected,
        })
        .collect();
    window.set_servers(ModelRc::from(Rc::new(VecModel::from(servers))));
    let active: Vec<_> = snapshot
        .active
        .iter()
        .map(|incident| incident_view(incident, None))
        .collect();
    window.set_active_incidents(ModelRc::from(Rc::new(VecModel::from(active))));
    let snoozed: Vec<_> = snapshot
        .snoozed
        .iter()
        .map(|item| incident_view(&item.incident, Some(item.until)))
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

fn incident_view(incident: &Incident, snoozed_until: Option<i64>) -> IncidentView {
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
        started_at: display_wire_time(&incident.incident_started_at).into(),
        snoozed_until: snoozed_until
            .map(|until| relative_time(Some(until), unix_now()))
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

fn display_wire_time(value: &str) -> String {
    if let Ok(timestamp) =
        time::OffsetDateTime::parse(value, &time::format_description::well_known::Rfc3339)
    {
        format!(
            "{value} ({})",
            relative_time(Some(timestamp.unix_timestamp()), unix_now())
        )
    } else if let Ok(timestamp) = value.parse::<i64>() {
        relative_time(Some(timestamp), unix_now())
    } else {
        value.to_owned()
    }
}

fn relative_time(value: Option<i64>, now: i64) -> String {
    let Some(value) = value else {
        return "—".into();
    };
    let delta = now - value;
    if delta >= 0 {
        format!("{} ago", duration(delta))
    } else {
        format!("in {}", duration(-delta))
    }
}

fn duration(seconds: i64) -> String {
    if seconds < 60 {
        format!("{seconds}s")
    } else if seconds < 3600 {
        format!("{}m", seconds / 60)
    } else if seconds < 86400 {
        format!("{}h {}m", seconds / 3600, seconds % 3600 / 60)
    } else {
        format!("{}d {}h", seconds / 86400, seconds % 86400 / 3600)
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
