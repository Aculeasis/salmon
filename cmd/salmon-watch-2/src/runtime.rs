use std::collections::BTreeMap;
use std::sync::Arc;
use std::thread::{self, JoinHandle};

use anyhow::{Context, Result};
use tokio::sync::{mpsc, watch};

use crate::config::Config;
use crate::domain::{AppState, Effect, Event, Reducer, UiSnapshot};
use crate::network;
use crate::notification::{DesktopNotificationSink, NotificationSink};

type Publisher = Arc<dyn Fn(UiSnapshot) + Send + Sync>;
type SnoozePersister = Arc<dyn Fn(&BTreeMap<String, i64>) -> Result<()> + Send + Sync>;

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
        let _ = self.shutdown.send(true);
        if let Some(thread) = self.thread.take() {
            let _ = thread.join();
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
    let runtime = match tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .build()
    {
        Ok(runtime) => runtime,
        Err(error) => {
            eprintln!("salmon-watch-2: failed to create async runtime: {error}");
            return;
        }
    };
    runtime.block_on(run_async(
        config, snoozes, publish, persist, commands, shutdown,
    ));
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
    let (events_tx, mut events_rx) = mpsc::channel(64);
    for server in config.ws_client.servers {
        tokio::spawn(network::run_client(
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
            _ = shutdown.changed() => break,
            else => break,
        };
        let refresh = matches!(event, Event::Tick { .. });
        let transition = reducer.reduce(event);
        for effect in transition.effects {
            match effect {
                Effect::Notify { title, body } => {
                    tokio::task::spawn_blocking(move || {
                        if let Err(error) = DesktopNotificationSink.push(&title, &body) {
                            eprintln!("salmon-watch-2: {error:#}");
                        }
                    });
                }
                Effect::PersistSnoozes => {
                    let snoozes = reducer.state().snoozes().clone();
                    let persist = persist.clone();
                    match tokio::task::spawn_blocking(move || persist(&snoozes)).await {
                        Ok(Ok(())) => {}
                        Ok(Err(error)) => {
                            eprintln!("salmon-watch-2: failed to persist snoozes: {error:#}")
                        }
                        Err(error) => {
                            eprintln!("salmon-watch-2: snooze persistence worker failed: {error}")
                        }
                    }
                }
            }
        }
        if transition.changed || refresh {
            (publish)(reducer.state().snapshot(unix_now()));
        }
    }
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
