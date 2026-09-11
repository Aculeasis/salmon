use std::collections::{BTreeMap, HashSet};
use std::sync::Arc;
use std::thread::{self, JoinHandle};

use anyhow::{Context, Result};
use tokio::sync::{mpsc, watch};
use tokio::task::JoinSet;

use crate::config::Config;
use crate::domain::{AppState, Effect, Event, Incident, IncidentState, Reducer, UiSnapshot};
use crate::network;
use crate::notification::{DesktopNotificationSink, NotificationSink};

type Publisher = Arc<dyn Fn(UiSnapshot) + Send + Sync>;
type SnoozePersister = Arc<dyn Fn(&BTreeMap<String, i64>) -> Result<()> + Send + Sync>;

const MAX_LOG_DETAILS_CHARS: usize = 1_000;

#[derive(Debug, Eq, PartialEq)]
struct EventLogLine {
    level: log::Level,
    message: String,
}

#[derive(Default)]
struct EventLogState {
    awaiting_snapshot: HashSet<String>,
    synchronized: HashSet<String>,
}

impl EventLogState {
    fn lines(&mut self, event: &Event) -> Vec<EventLogLine> {
        match event {
            Event::Connected { server_id, .. } => {
                self.awaiting_snapshot.insert(server_id.clone());
                Vec::new()
            }
            Event::Notification {
                server_id, data, ..
            } => {
                let was_awaiting = self.awaiting_snapshot.remove(server_id);
                let first_snapshot = self.synchronized.insert(server_id.clone());
                let initial = was_awaiting || first_snapshot;
                let mut lines = Vec::new();
                if initial {
                    lines.push(info(format!(
                        "server {server_id} synchronized: {} ongoing incidents",
                        data.total.len()
                    )));
                    lines.extend(
                        data.total
                            .iter()
                            .map(|incident| info(incident_line(server_id, "ongoing", incident))),
                    );
                } else {
                    lines.push(info(format!(
                        "server {server_id} incident update: {} added, {} updated, {} resolved; {} ongoing incidents",
                        data.added.len(),
                        data.updated.len(),
                        data.removed.len(),
                        data.total.len()
                    )));
                    lines.extend(
                        data.added
                            .iter()
                            .map(|incident| info(incident_line(server_id, "added", incident))),
                    );
                    lines.extend(
                        data.updated
                            .iter()
                            .map(|incident| info(incident_line(server_id, "updated", incident))),
                    );
                    lines.extend(data.removed.iter().map(|incident| {
                        info(format!(
                            "server {server_id} resolved incident {}",
                            incident.key
                        ))
                    }));
                }
                lines
            }
            Event::Heartbeat { server_id, .. } => vec![EventLogLine {
                level: log::Level::Debug,
                message: format!("server {server_id} received heartbeat"),
            }],
            Event::Snooze { key, until } => {
                vec![info(format!("snoozed incident {key} until {until}"))]
            }
            Event::Unsnooze { key } => vec![info(format!("unsnoozed incident {key}"))],
            Event::ForgetStale { key } => vec![info(format!("forgot stale incident {key}"))],
            _ => Vec::new(),
        }
    }
}

fn info(message: String) -> EventLogLine {
    EventLogLine {
        level: log::Level::Info,
        message,
    }
}

fn incident_line(server_id: &str, action: &str, incident: &Incident) -> String {
    let state = match incident.state {
        IncidentState::Ok => "ok",
        IncidentState::Warning => "warning",
        IncidentState::Error => "error",
    };
    let mut line = format!(
        "server {server_id} {action} incident {} ({state})",
        incident.key
    );
    if !incident.details.is_empty() {
        line.push_str(": ");
        line.push_str(&log_details(&incident.details));
    }
    line
}

fn log_details(details: &str) -> String {
    let flattened = details
        .chars()
        .map(|character| {
            if character.is_control() || matches!(character, '\u{2028}' | '\u{2029}') {
                ' '
            } else {
                character
            }
        })
        .collect::<String>();
    let mut characters = flattened.chars();
    let mut limited = characters
        .by_ref()
        .take(MAX_LOG_DETAILS_CHARS)
        .collect::<String>();
    if characters.next().is_some() {
        limited.push('…');
    }
    limited
}

#[derive(Debug)]
pub enum Command {
    Snooze { key: String, seconds: i64 },
    Unsnooze { key: String },
    ForgetStale { key: String },
}

pub struct RuntimeHandle {
    commands: mpsc::Sender<Command>,
    shutdown: watch::Sender<bool>,
    thread: Option<JoinHandle<()>>,
}

impl RuntimeHandle {
    pub fn start(
        config: Config,
        snoozes: BTreeMap<String, i64>,
        publish: Publisher,
        persist: SnoozePersister,
    ) -> Result<Self> {
        let (commands, command_rx) = mpsc::channel(32);
        let (shutdown, shutdown_rx) = watch::channel(false);
        let thread = thread::Builder::new()
            .name("salmon-watch-runtime".into())
            .spawn(move || run(config, snoozes, publish, persist, command_rx, shutdown_rx))
            .context("failed to start async runtime thread")?;
        Ok(Self {
            commands,
            shutdown,
            thread: Some(thread),
        })
    }

    pub fn commands(&self) -> mpsc::Sender<Command> {
        self.commands.clone()
    }

    pub fn shutdown(mut self) {
        log::debug!("requesting async runtime shutdown");
        let _ = self.shutdown.send(true);
        if let Some(thread) = self.thread.take() {
            log::debug!("waiting for async runtime thread to stop");
            if thread.join().is_err() {
                log::error!("async runtime thread panicked during shutdown");
            }
        }
    }
}

impl Drop for RuntimeHandle {
    fn drop(&mut self) {
        let _ = self.shutdown.send(true);
    }
}

fn run(
    config: Config,
    snoozes: BTreeMap<String, i64>,
    publish: Publisher,
    persist: SnoozePersister,
    commands: mpsc::Receiver<Command>,
    shutdown: watch::Receiver<bool>,
) {
    log::debug!("async runtime thread started");
    let runtime = match tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .build()
    {
        Ok(runtime) => runtime,
        Err(error) => {
            log::error!("failed to create async runtime: {error}");
            return;
        }
    };
    runtime.block_on(run_async(
        config, snoozes, publish, persist, commands, shutdown,
    ));
    // Network tasks have been joined by run_async. Drop Tokio before reporting
    // the runtime stopped so any remaining auxiliary work is cleaned up too.
    drop(runtime);
    log::info!("async runtime stopped");
}

async fn run_async(
    config: Config,
    snoozes: BTreeMap<String, i64>,
    publish: Publisher,
    persist: SnoozePersister,
    mut commands: mpsc::Receiver<Command>,
    mut shutdown: watch::Receiver<bool>,
) {
    let ids = config
        .ws_client
        .servers
        .iter()
        .map(|server| server.id.clone())
        .collect();
    let mut reducer = Reducer::new(AppState::new(ids, snoozes));
    let mut event_logs = EventLogState::default();
    let (events_tx, mut events_rx) = mpsc::channel(64);
    let mut network_tasks = JoinSet::new();
    for server in config.ws_client.servers {
        network_tasks.spawn(network::run_client(
            server,
            events_tx.clone(),
            shutdown.clone(),
        ));
    }
    (publish)(reducer.state().snapshot(unix_now()));
    let mut ticks = tokio::time::interval(std::time::Duration::from_secs(1));
    loop {
        let event = tokio::select! {
            Some(event) = events_rx.recv() => event,
            Some(command) = commands.recv() => command_event(command),
            _ = ticks.tick() => Event::Tick { at: unix_now() },
            _ = shutdown.changed() => {
                log::info!("stopping network services");
                break;
            },
            else => break,
        };
        let refresh = matches!(event, Event::Tick { .. });
        for line in event_logs.lines(&event) {
            log::log!(line.level, "{}", line.message);
        }
        let transition = reducer.reduce(event);
        for effect in transition.effects {
            match effect {
                Effect::Notify { title, body } => {
                    tokio::task::spawn_blocking(move || {
                        if let Err(error) = DesktopNotificationSink.push(&title, &body) {
                            log::error!("{error:#}");
                        }
                    });
                }
                Effect::PersistSnoozes => {
                    let snoozes = reducer.state().snoozes().clone();
                    let persist = persist.clone();
                    match tokio::task::spawn_blocking(move || persist(&snoozes)).await {
                        Ok(Ok(())) => {}
                        Ok(Err(error)) => {
                            log::error!("failed to persist snoozes: {error:#}")
                        }
                        Err(error) => {
                            log::error!("snooze persistence worker failed: {error}")
                        }
                    }
                }
            }
        }
        if transition.changed || refresh {
            (publish)(reducer.state().snapshot(unix_now()));
        }
    }

    // Keep draining events while clients stop so a client already waiting on a
    // full event channel cannot prevent shutdown. Joining here lets SSH tunnel
    // tasks kill and reap their child processes before Tokio is destroyed.
    drop(events_tx);
    while !network_tasks.is_empty() {
        tokio::select! {
            result = network_tasks.join_next() => {
                if let Some(Err(error)) = result {
                    log::error!("network service task failed during shutdown: {error}");
                }
            }
            _ = events_rx.recv() => {}
        }
    }
    log::info!("network services stopped");
}

fn command_event(command: Command) -> Event {
    match command {
        Command::Snooze { key, seconds } => Event::Snooze {
            key,
            until: unix_now().saturating_add(seconds),
        },
        Command::Unsnooze { key } => Event::Unsnooze { key },
        Command::ForgetStale { key } => Event::ForgetStale { key },
    }
}

pub fn unix_now() -> i64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .unwrap_or_default()
        .as_secs() as i64
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::domain::NotificationData;

    fn incident(key: &str, state: IncidentState, details: &str) -> Incident {
        Incident {
            key: key.into(),
            state,
            details: details.into(),
            incident_started_at: "2026-09-09T00:00:00Z".into(),
            stale: false,
        }
    }

    fn notification(
        server_id: &str,
        total: Vec<Incident>,
        added: Vec<Incident>,
        updated: Vec<Incident>,
        removed: Vec<Incident>,
    ) -> Event {
        Event::Notification {
            server_id: server_id.into(),
            data: NotificationData {
                total,
                added,
                removed,
                updated,
                num_items_ok: 7,
            },
            at: 1,
        }
    }

    #[test]
    fn initial_snapshot_logs_every_ongoing_incident() {
        let mut logs = EventLogState::default();
        logs.lines(&Event::Connected {
            server_id: "home".into(),
            at: 1,
        });
        let lines = logs.lines(&notification(
            "home",
            vec![
                incident("disk", IncidentState::Error, "filesystem\nfull"),
                incident("backup", IncidentState::Warning, "late"),
            ],
            Vec::new(),
            Vec::new(),
            Vec::new(),
        ));
        assert_eq!(
            lines,
            [
                info("server home synchronized: 2 ongoing incidents".into()),
                info("server home ongoing incident disk (error): filesystem full".into()),
                info("server home ongoing incident backup (warning): late".into()),
            ]
        );
    }

    #[test]
    fn subsequent_updates_log_every_delta_without_reprinting_total() {
        let mut logs = EventLogState::default();
        logs.lines(&notification(
            "home",
            vec![incident("existing", IncidentState::Warning, "unchanged")],
            Vec::new(),
            Vec::new(),
            Vec::new(),
        ));
        let lines = logs.lines(&notification(
            "home",
            vec![
                incident("existing", IncidentState::Warning, "unchanged"),
                incident("new", IncidentState::Error, "broken"),
            ],
            vec![incident("new", IncidentState::Error, "broken")],
            vec![incident("existing", IncidentState::Error, "worse")],
            vec![incident("fixed", IncidentState::Warning, "old details")],
        ));
        assert_eq!(
            lines,
            [
                info("server home incident update: 1 added, 1 updated, 1 resolved; 2 ongoing incidents".into()),
                info("server home added incident new (error): broken".into()),
                info("server home updated incident existing (error): worse".into()),
                info("server home resolved incident fixed".into()),
            ]
        );
        assert!(lines.iter().all(|line| !line.message.contains("unchanged")));
    }

    #[test]
    fn reconnect_logs_the_next_complete_snapshot_again() {
        let mut logs = EventLogState::default();
        let snapshot = || {
            notification(
                "home",
                vec![incident("disk", IncidentState::Error, "full")],
                Vec::new(),
                Vec::new(),
                Vec::new(),
            )
        };
        assert_eq!(logs.lines(&snapshot()).len(), 2);
        assert_eq!(logs.lines(&snapshot()).len(), 1);
        logs.lines(&Event::Connected {
            server_id: "home".into(),
            at: 2,
        });
        let reconnected = logs.lines(&snapshot());
        assert_eq!(reconnected.len(), 2);
        assert!(reconnected[1].message.contains("ongoing incident disk"));
    }

    #[test]
    fn heartbeat_is_debug_and_long_details_are_safely_bounded() {
        let mut logs = EventLogState::default();
        assert_eq!(
            logs.lines(&Event::Heartbeat {
                server_id: "home".into(),
                at: 1,
            }),
            [EventLogLine {
                level: log::Level::Debug,
                message: "server home received heartbeat".into(),
            }]
        );

        let details = "x".repeat(MAX_LOG_DETAILS_CHARS + 1);
        let line = incident_line(
            "home",
            "ongoing",
            &incident("huge", IncidentState::Error, &details),
        );
        assert!(line.ends_with('…'));
        assert_eq!(line.matches('x').count(), MAX_LOG_DETAILS_CHARS);
    }
}
