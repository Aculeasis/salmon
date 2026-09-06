use super::{AppState, Effect, Event, Incident, IncidentState, Transition};

pub struct Reducer {
    state: AppState,
}

impl Reducer {
    pub fn new(state: AppState) -> Self {
        Self { state }
    }

    pub fn state(&self) -> &AppState {
        &self.state
    }

    pub fn reduce(&mut self, event: Event) -> Transition {
        match event {
            Event::Connected { server_id, at } => self.connected(&server_id, at),
            Event::Disconnected {
                server_id,
                at,
                error,
            } => self.disconnected(&server_id, at, error),
            Event::Heartbeat { server_id, at } => self.heartbeat(&server_id, at),
            Event::Notification {
                server_id,
                data,
                at,
            } => {
                if !self.state.servers.contains_key(&server_id) {
                    return Transition::default();
                }
                let mut effects = Vec::new();
                for item in &data.added {
                    let item = prefixed(item, &server_id);
                    if !self.is_snoozed(&item.key, at) {
                        effects.push(Effect::Notify {
                            title: format!("{}: {}", state_name(item.state), item.key),
                            body: item.details,
                        });
                    }
                }
                for item in &data.removed {
                    let item = prefixed(item, &server_id);
                    if !self.is_snoozed(&item.key, at) {
                        effects.push(Effect::Notify {
                            title: format!("OK: {}", item.key),
                            body: String::new(),
                        });
                    }
                }
                self.state.incidents.insert(
                    server_id.clone(),
                    data.total
                        .iter()
                        .map(|item| prefixed(item, &server_id))
                        .collect(),
                );
                Transition {
                    changed: true,
                    effects,
                }
            }
            Event::Snooze { key, until } => {
                let changed = self.state.snoozed.insert(key, until) != Some(until);
                Transition {
                    changed,
                    effects: changed
                        .then_some(Effect::PersistSnoozes)
                        .into_iter()
                        .collect(),
                }
            }
            Event::Unsnooze { key } => {
                let changed = self.state.snoozed.remove(&key).is_some();
                Transition {
                    changed,
                    effects: changed
                        .then_some(Effect::PersistSnoozes)
                        .into_iter()
                        .collect(),
                }
            }
            Event::ForgetStale { key } => {
                let mut changed = false;
                for incidents in self.state.incidents.values_mut() {
                    let before = incidents.len();
                    incidents.retain(|item| item.key != key || !item.stale);
                    changed |= incidents.len() != before;
                }
                Transition {
                    changed,
                    effects: Vec::new(),
                }
            }
            Event::Tick { at } => {
                let before = self.state.snoozed.len();
                self.state.snoozed.retain(|_, until| *until > at);
                let changed = before != self.state.snoozed.len();
                Transition {
                    changed,
                    effects: changed
                        .then_some(Effect::PersistSnoozes)
                        .into_iter()
                        .collect(),
                }
            }
        }
    }

    fn connected(&mut self, id: &str, at: i64) -> Transition {
        let Some(server) = self.state.servers.get_mut(id) else {
            return Transition::default();
        };
        let changed = !server.initialized || !server.connected;
        server.initialized = true;
        server.connected = true;
        server.connection_changed_at = changed.then_some(at).or(server.connection_changed_at);
        if changed {
            server.last_heartbeat_at = None;
        }
        let removed = self
            .state
            .internal_incidents
            .remove(&format!("internal.connection.{id}"))
            .is_some();
        let effects = removed
            .then(|| Effect::Notify {
                title: format!("OK: internal.connection.{id}"),
                body: String::new(),
            })
            .into_iter()
            .collect();
        Transition {
            changed: changed || removed,
            effects,
        }
    }

    fn disconnected(&mut self, id: &str, at: i64, error: String) -> Transition {
        let Some(server) = self.state.servers.get_mut(id) else {
            return Transition::default();
        };
        let status_changed = !server.initialized || server.connected;
        server.initialized = true;
        server.connected = false;
        server.connection_changed_at = status_changed
            .then_some(at)
            .or(server.connection_changed_at);
        let mut stale_changed = false;
        if let Some(incidents) = self.state.incidents.get_mut(id) {
            for incident in incidents {
                if !incident.stale {
                    incident.stale = true;
                    stale_changed = true;
                }
            }
        }
        let key = format!("internal.connection.{id}");
        let mut effects = Vec::new();
        let internal_changed = if error.is_empty() {
            self.state.internal_incidents.remove(&key).is_some()
        } else {
            match self.state.internal_incidents.get_mut(&key) {
                Some(incident) if incident.details == error => false,
                Some(incident) => {
                    incident.details = error;
                    true
                }
                None => {
                    effects.push(Effect::Notify {
                        title: format!("error: {key}"),
                        body: error.clone(),
                    });
                    self.state.internal_incidents.insert(
                        key.clone(),
                        Incident {
                            key,
                            state: IncidentState::Error,
                            details: error,
                            incident_started_at: at.to_string(),
                            stale: false,
                        },
                    );
                    true
                }
            }
        };
        Transition {
            changed: status_changed || stale_changed || internal_changed,
            effects,
        }
    }

    fn heartbeat(&mut self, id: &str, at: i64) -> Transition {
        let Some(server) = self.state.servers.get_mut(id) else {
            return Transition::default();
        };
        let changed = server.last_heartbeat_at != Some(at);
        server.last_heartbeat_at = Some(at);
        Transition {
            changed,
            effects: Vec::new(),
        }
    }

    fn is_snoozed(&self, key: &str, now: i64) -> bool {
        self.state
            .snoozed
            .get(key)
            .is_some_and(|until| *until > now)
    }
}

fn prefixed(item: &Incident, id: &str) -> Incident {
    let mut item = item.clone();
    item.key = format!("{id}.{}", item.key);
    item
}

fn state_name(state: IncidentState) -> &'static str {
    match state {
        IncidentState::Ok => "ok",
        IncidentState::Warning => "warning",
        IncidentState::Error => "error",
    }
}

#[cfg(test)]
#[path = "reducer_tests.rs"]
mod tests;
