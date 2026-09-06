use std::collections::{BTreeMap, HashMap};

use serde::{Deserialize, Serialize};

#[derive(Clone, Copy, Debug, Deserialize, Eq, Ord, PartialEq, PartialOrd, Serialize)]
#[serde(rename_all = "lowercase")]
pub enum IncidentState {
    Ok,
    Warning,
    Error,
}

#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct Incident {
    pub key: String,
    pub state: IncidentState,
    #[serde(default)]
    pub details: String,
    pub incident_started_at: String,
    #[serde(default)]
    pub stale: bool,
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct NotificationData {
    pub total: Vec<Incident>,
    pub added: Vec<Incident>,
    pub removed: Vec<Incident>,
    pub updated: Vec<Incident>,
    pub num_items_ok: usize,
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct ServerStatus {
    pub id: String,
    pub connected: bool,
    pub initialized: bool,
    pub connection_changed_at: Option<i64>,
    pub last_heartbeat_at: Option<i64>,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum OverallState {
    Unknown,
    Ok,
    InternalError,
    Warning,
    Error,
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct SnoozedIncident {
    pub incident: Incident,
    pub until: i64,
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct UiSnapshot {
    pub servers: Vec<ServerStatus>,
    pub active: Vec<Incident>,
    pub snoozed: Vec<SnoozedIncident>,
    pub alerting_state: OverallState,
    pub snoozed_state: Option<OverallState>,
    pub unknown_server_count: usize,
}

#[derive(Clone, Debug)]
pub struct AppState {
    pub(crate) server_order: Vec<String>,
    pub(crate) servers: HashMap<String, ServerStatus>,
    pub(crate) incidents: HashMap<String, Vec<Incident>>,
    pub(crate) internal_incidents: BTreeMap<String, Incident>,
    pub(crate) snoozed: BTreeMap<String, i64>,
}

impl AppState {
    pub fn new(server_ids: Vec<String>, snoozed: BTreeMap<String, i64>) -> Self {
        let servers = server_ids
            .iter()
            .map(|id| {
                (
                    id.clone(),
                    ServerStatus {
                        id: id.clone(),
                        connected: false,
                        initialized: false,
                        connection_changed_at: None,
                        last_heartbeat_at: None,
                    },
                )
            })
            .collect();
        Self {
            server_order: server_ids,
            servers,
            incidents: HashMap::new(),
            internal_incidents: BTreeMap::new(),
            snoozed,
        }
    }

    pub fn snoozes(&self) -> &BTreeMap<String, i64> {
        &self.snoozed
    }

    pub fn snapshot(&self, now: i64) -> UiSnapshot {
        let mut incidents: Vec<_> = self.internal_incidents.values().cloned().collect();
        for id in &self.server_order {
            if let Some(server_incidents) = self.incidents.get(id) {
                incidents.extend(server_incidents.iter().cloned());
            }
        }

        let mut active = Vec::new();
        let mut snoozed = Vec::new();
        for incident in incidents {
            if let Some(until) = self
                .snoozed
                .get(&incident.key)
                .copied()
                .filter(|until| *until > now)
            {
                snoozed.push(SnoozedIncident { incident, until });
            } else {
                active.push(incident);
            }
        }
        let servers: Vec<_> = self
            .server_order
            .iter()
            .filter_map(|id| self.servers.get(id).cloned())
            .collect();
        let unknown_server_count = servers.iter().filter(|server| !server.initialized).count();
        let mut alerting_state = overall_state(&active);
        if alerting_state == OverallState::Ok && unknown_server_count > 0 {
            alerting_state = OverallState::Unknown;
        }
        let snoozed_state = (!snoozed.is_empty()).then(|| {
            overall_state(
                &snoozed
                    .iter()
                    .map(|item| item.incident.clone())
                    .collect::<Vec<_>>(),
            )
        });
        UiSnapshot {
            servers,
            active,
            snoozed,
            alerting_state,
            snoozed_state,
            unknown_server_count,
        }
    }
}

fn overall_state(items: &[Incident]) -> OverallState {
    items.iter().fold(OverallState::Ok, |overall, item| {
        let current = if item.key.starts_with("internal.") && item.state != IncidentState::Ok {
            OverallState::InternalError
        } else {
            match item.state {
                IncidentState::Ok => OverallState::Ok,
                IncidentState::Warning => OverallState::Warning,
                IncidentState::Error => OverallState::Error,
            }
        };
        if rank(current) > rank(overall) {
            current
        } else {
            overall
        }
    })
}

fn rank(state: OverallState) -> u8 {
    match state {
        OverallState::Unknown => 0,
        OverallState::Ok => 1,
        OverallState::InternalError => 2,
        OverallState::Warning => 3,
        OverallState::Error => 4,
    }
}
