use anyhow::{Context, Result};
use notify_rust::Notification;
use std::collections::HashMap;
use std::sync::mpsc::{self, Receiver, SyncSender, TrySendError};
use std::thread::{self, JoinHandle};
use std::time::Duration;
use time::OffsetDateTime;
use tokio::time::Instant;

/// Maximum number of notifications waiting behind the one currently being shown.
const NOTIFICATION_QUEUE_CAPACITY: usize = 32;
/// Maximum shutdown delay when the platform notification service is stuck.
const NOTIFICATION_SHUTDOWN_TIMEOUT: Duration = Duration::from_secs(2);

/// Injectable boundary around the desktop notification service.
pub trait NotificationSink: Send + 'static {
    fn push(&self, title: &str, body: &str) -> Result<()>;
}

/// Production sink backed by the platform notification daemon.
#[derive(Clone, Copy, Debug, Default)]
pub struct DesktopNotificationSink;

impl NotificationSink for DesktopNotificationSink {
    fn push(&self, title: &str, body: &str) -> Result<()> {
        desktop_notification(title, body)
            .show()
            .context("desktop notification delivery failed")?;
        Ok(())
    }
}

/// One notification owned by the delivery worker.
struct QueuedNotification {
    title: String,
    body: String,
}

/// Result of a nonblocking attempt to enqueue a notification.
#[derive(Debug, Eq, PartialEq)]
enum EnqueueOutcome {
    Queued,
    Dropped,
    Unavailable,
}

/// Serializes desktop notifications without blocking the async event pump.
///
/// The bounded queue deliberately drops newly generated notifications when full:
/// stale incident transitions are less useful than keeping current network and UI
/// state responsive. Accepted notifications are delivered in reducer order.
pub struct NotificationDispatcher {
    /// Bounded producer endpoint; dropping it tells the worker to drain and stop.
    sender: Option<SyncSender<QueuedNotification>>,
    /// Completion signal used to put a deadline on shutdown.
    stopped: Receiver<()>,
    /// Owned while joinable and deliberately discarded if shutdown times out.
    worker: Option<JoinHandle<()>>,
}

impl NotificationDispatcher {
    /// Starts the production desktop-notification worker.
    pub fn start() -> Result<Self> {
        #[cfg(windows)]
        register_app_id();

        Self::start_with_sink(DesktopNotificationSink, NOTIFICATION_QUEUE_CAPACITY)
    }

    /// Queues a notification, or drops it immediately if all queue slots are busy.
    pub fn enqueue(&mut self, title: String, body: String) {
        let _ = self.enqueue_inner(title, body);
    }

    /// Drains accepted notifications and waits briefly for the worker to finish.
    ///
    /// A notification daemon call may block inside platform code. In that case the
    /// worker thread is detached after the timeout so application shutdown can
    /// still complete; process exit then terminates the detached thread.
    pub fn shutdown(mut self) {
        let _ = self.shutdown_with_timeout(NOTIFICATION_SHUTDOWN_TIMEOUT);
    }

    /// Starts a dispatcher with an injectable sink and queue size for tests.
    fn start_with_sink<S>(sink: S, capacity: usize) -> Result<Self>
    where
        S: NotificationSink,
    {
        let (sender, receiver) = mpsc::sync_channel::<QueuedNotification>(capacity);
        let (stopped_sender, stopped) = mpsc::channel();
        let worker = thread::Builder::new()
            .name("salmon-watch-notifications".into())
            .spawn(move || {
                while let Ok(notification) = receiver.recv() {
                    if let Err(error) = sink.push(&notification.title, &notification.body) {
                        log::error!("{error:#}");
                    }
                }
                let _ = stopped_sender.send(());
            })
            .context("failed to start desktop notification worker")?;

        Ok(Self {
            sender: Some(sender),
            stopped,
            worker: Some(worker),
        })
    }

    /// Implements enqueueing and exposes the state transition to unit tests.
    fn enqueue_inner(&mut self, title: String, body: String) -> EnqueueOutcome {
        let notification = QueuedNotification { title, body };
        let result = match self.sender.as_ref() {
            Some(sender) => sender.try_send(notification),
            None => return EnqueueOutcome::Unavailable,
        };

        match result {
            Ok(()) => EnqueueOutcome::Queued,
            Err(TrySendError::Full(_)) => {
                log::warn!("desktop notification queue is full; dropping notification");
                EnqueueOutcome::Dropped
            }
            Err(TrySendError::Disconnected(_)) => {
                self.sender.take();
                log::error!("desktop notification worker stopped unexpectedly");
                EnqueueOutcome::Unavailable
            }
        }
    }

    /// Closes the queue and joins the worker unless delivery exceeds `timeout`.
    fn shutdown_with_timeout(&mut self, timeout: Duration) -> ShutdownOutcome {
        self.sender.take();

        match self.stopped.recv_timeout(timeout) {
            Ok(()) | Err(mpsc::RecvTimeoutError::Disconnected) => {
                if self
                    .worker
                    .take()
                    .is_some_and(|worker| worker.join().is_err())
                {
                    log::error!("desktop notification worker panicked");
                } else {
                    log::info!("desktop notification worker stopped");
                }
                ShutdownOutcome::Stopped
            }
            Err(mpsc::RecvTimeoutError::Timeout) => {
                self.worker.take();
                log::warn!(
                    "desktop notification worker did not stop within {}s; detaching it",
                    timeout.as_secs_f64()
                );
                ShutdownOutcome::Detached
            }
        }
    }
}

impl Drop for NotificationDispatcher {
    fn drop(&mut self) {
        // Closing the last sender lets the worker finish even if an early return
        // bypasses the explicit, bounded shutdown path.
        self.sender.take();
    }
}

/// Whether an explicit shutdown joined or detached the worker.
#[derive(Debug, Eq, PartialEq)]
enum ShutdownOutcome {
    Stopped,
    Detached,
}

/// Constructs notification metadata separately so it can be tested without delivery.
fn desktop_notification(title: &str, body: &str) -> Notification {
    let mut notification = Notification::new();
    notification
        .appname("Salmon Watch")
        .summary(title)
        .body(body);
    #[cfg(windows)]
    notification.app_id("Salmon Watch");
    notification
}

/// Ensures the Windows AppUserModelID is registered for desktop toast notifications.
#[cfg(windows)]
pub fn register_app_id() {
    use winreg::RegKey;
    use winreg::enums::*;

    let hkcu = RegKey::predef(HKEY_CURRENT_USER);
    if let Ok((key, _)) = hkcu.create_subkey(r"Software\Classes\AppUserModelId\Salmon Watch") {
        let _ = key.set_value("DisplayName", &"Salmon Watch");

        let icon_path = dirs::data_local_dir().map(|dir| dir.join("SalmonWatch").join("icon.png"));
        if let Some(ref path) = icon_path {
            if let Some(parent) = path.parent() {
                let _ = std::fs::create_dir_all(parent);
            }
            const APP_ICON: &[u8] = include_bytes!("../assets/app-icon.png");
            let _ = std::fs::write(path, APP_ICON);
            let _ = key.set_value("IconUri", &path.to_string_lossy().as_ref());
        }
    }
}

/// Action to take for a processed notification.
#[derive(Debug, Eq, PartialEq)]
pub enum FilterAction {
    /// Deliver this notification immediately.
    Send { title: String, body: String },
    /// Deliver multiple notifications in order.
    SendBatch {
        notifications: Vec<(String, String)>,
    },
    /// Suppress / delay this notification.
    Suppress,
}

#[derive(Debug, Eq, PartialEq)]
enum ServerConnectionState {
    /// No ongoing connection error for this server.
    Idle,
    /// A connection error occurred and is being delayed until `deadline`.
    Delaying {
        deadline: Instant,
        deadline_at: OffsetDateTime,
        title: String,
        body: String,
    },
    /// The delay expired, the connection had not recovered, and the error was notified.
    Notified,
}

enum ConnectionEvent<'a> {
    Error { server_id: &'a str },
    Ok { server_id: &'a str },
}

fn parse_connection_event(title: &str) -> Option<ConnectionEvent<'_>> {
    if let Some(id) = title.strip_prefix("error: internal.connection.") {
        Some(ConnectionEvent::Error { server_id: id })
    } else if let Some(id) = title.strip_prefix("error: internal.tunnel.") {
        Some(ConnectionEvent::Error { server_id: id })
    } else if let Some(id) = title.strip_prefix("OK: internal.connection.") {
        Some(ConnectionEvent::Ok { server_id: id })
    } else {
        title
            .strip_prefix("OK: internal.tunnel.")
            .map(|id| ConnectionEvent::Ok { server_id: id })
    }
}

/// Filters and debounces connection error notifications.
///
/// Transient drops (such as SSH tunnel reconnects) that recover within `delay`
/// are completely suppressed to avoid noisy, useless desktop toasts.
/// Non-connection notifications (e.g. server incident alerts) always pass through immediately.
#[derive(Debug)]
pub struct ConnectionNotificationFilter {
    delay: Duration,
    servers: HashMap<String, ServerConnectionState>,
}

impl ConnectionNotificationFilter {
    pub fn new(delay: Duration) -> Self {
        Self {
            delay,
            servers: HashMap::new(),
        }
    }

    /// Evaluates an incoming notification effect against the delay and server state.
    pub fn handle_notification_at(
        &mut self,
        title: String,
        body: String,
        occurred_at: OffsetDateTime,
        now: Instant,
        wall_now: OffsetDateTime,
    ) -> FilterAction {
        let event = match parse_connection_event(&title) {
            Some(event) => event,
            None => return FilterAction::Send { title, body },
        };

        if self.delay.is_zero() {
            match event {
                ConnectionEvent::Error { server_id } => {
                    self.servers
                        .insert(server_id.to_string(), ServerConnectionState::Notified);
                }
                ConnectionEvent::Ok { server_id } => {
                    self.servers
                        .insert(server_id.to_string(), ServerConnectionState::Idle);
                }
            }
            return FilterAction::Send { title, body };
        }

        match event {
            ConnectionEvent::Error { server_id } => {
                let state = self
                    .servers
                    .entry(server_id.to_string())
                    .or_insert(ServerConnectionState::Idle);
                match state {
                    ServerConnectionState::Idle => {
                        let deadline_at = occurred_at
                            + time::Duration::try_from(self.delay)
                                .expect("notification delay fits time::Duration");
                        let remaining = std::time::Duration::try_from(deadline_at - wall_now)
                            .unwrap_or(Duration::ZERO);
                        *state = ServerConnectionState::Delaying {
                            deadline: now + remaining,
                            deadline_at,
                            title,
                            body,
                        };
                        FilterAction::Suppress
                    }
                    ServerConnectionState::Delaying {
                        title: existing_title,
                        body: existing_body,
                        deadline_at,
                        ..
                    } => {
                        if occurred_at >= *deadline_at {
                            *state = ServerConnectionState::Notified;
                            return FilterAction::Send { title, body };
                        }
                        // While the timer is ticking, suppress subsequent errors and retain
                        // the most recent error details while preserving the original deadline.
                        *existing_title = title;
                        *existing_body = body;
                        FilterAction::Suppress
                    }
                    ServerConnectionState::Notified => {
                        // Error was already notified to desktop; do not re-emit duplicate toasts
                        // on retry failures before recovery.
                        FilterAction::Suppress
                    }
                }
            }
            ConnectionEvent::Ok { server_id } => {
                let state = self
                    .servers
                    .entry(server_id.to_string())
                    .or_insert(ServerConnectionState::Idle);
                match state {
                    ServerConnectionState::Idle => {
                        // Spurious or untracked OK; suppress.
                        FilterAction::Suppress
                    }
                    ServerConnectionState::Delaying { deadline_at, .. }
                        if occurred_at < *deadline_at =>
                    {
                        // Connection recovered before delay expired; clear the error and suppress OK.
                        *state = ServerConnectionState::Idle;
                        FilterAction::Suppress
                    }
                    ServerConnectionState::Delaying { .. } => {
                        // Runtime may have been busy while both the deadline and recovery passed.
                        // Preserve ordering by delivering the delayed error before its recovery.
                        let old_state = std::mem::replace(state, ServerConnectionState::Idle);
                        let ServerConnectionState::Delaying {
                            title: error_title,
                            body: error_body,
                            ..
                        } = old_state
                        else {
                            unreachable!("matched delaying state")
                        };
                        FilterAction::SendBatch {
                            notifications: vec![(error_title, error_body), (title, body)],
                        }
                    }
                    ServerConnectionState::Notified => {
                        // Error was notified previously; notify recovery and reset to Idle.
                        *state = ServerConnectionState::Idle;
                        FilterAction::Send { title, body }
                    }
                }
            }
        }
    }

    #[cfg(test)]
    fn handle_notification(&mut self, title: String, body: String, now: Instant) -> FilterAction {
        self.handle_notification_at(
            title,
            body,
            OffsetDateTime::UNIX_EPOCH,
            now,
            OffsetDateTime::UNIX_EPOCH,
        )
    }

    /// Earliest deadline among all currently delaying servers, if any.
    pub fn next_deadline(&self) -> Option<Instant> {
        self.servers
            .values()
            .filter_map(|state| match state {
                ServerConnectionState::Delaying { deadline, .. } => Some(*deadline),
                _ => None,
            })
            .min()
    }

    /// Clears notification-only state after the corresponding domain incident
    /// recovers without producing an `OK` notification (for example, while it
    /// is snoozed).
    pub fn reset_server(&mut self, server_id: &str) {
        self.servers.remove(server_id);
    }

    /// Drains and transitions all notifications whose delay deadline has passed.
    pub fn drain_due(&mut self, now: Instant) -> Vec<(String, String)> {
        let mut due = Vec::new();
        for state in self.servers.values_mut() {
            if let ServerConnectionState::Delaying { deadline, .. } = state
                && now >= *deadline
            {
                let old_state = std::mem::replace(state, ServerConnectionState::Notified);
                if let ServerConnectionState::Delaying { title, body, .. } = old_state {
                    due.push((title, body));
                }
            }
        }
        due
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::atomic::{AtomicBool, Ordering};
    use std::sync::{Arc, Mutex};

    /// Sink that records every delivered notification.
    struct RecordingSink(Arc<Mutex<Vec<(String, String)>>>);

    impl NotificationSink for RecordingSink {
        fn push(&self, title: &str, body: &str) -> Result<()> {
            self.0.lock().unwrap().push((title.into(), body.into()));
            Ok(())
        }
    }

    /// Sink that blocks its first delivery until the test releases it.
    struct BlockingFirstSink {
        first: AtomicBool,
        delivered: mpsc::Sender<String>,
        release: Mutex<mpsc::Receiver<()>>,
    }

    impl NotificationSink for BlockingFirstSink {
        fn push(&self, title: &str, _body: &str) -> Result<()> {
            self.delivered.send(title.into()).unwrap();
            if self.first.swap(false, Ordering::SeqCst) {
                self.release.lock().unwrap().recv().unwrap();
            }
            Ok(())
        }
    }

    #[test]
    fn desktop_notification_has_application_and_message() {
        let notification = desktop_notification("Example title", "Example body");

        assert_eq!(notification.appname, "Salmon Watch");
        assert_eq!(notification.summary, "Example title");
        assert_eq!(notification.body, "Example body");
    }

    #[test]
    #[cfg(windows)]
    fn register_app_id_creates_registry_entry() {
        register_app_id();
        use winreg::RegKey;
        use winreg::enums::*;

        let hkcu = RegKey::predef(HKEY_CURRENT_USER);
        let key = hkcu
            .open_subkey(r"Software\Classes\AppUserModelId\Salmon Watch")
            .expect("open AppUserModelId key");
        let display_name: String = key.get_value("DisplayName").expect("get DisplayName");
        assert_eq!(display_name, "Salmon Watch");
    }

    #[test]
    fn dispatcher_delivers_notifications_in_enqueue_order() {
        let delivered = Arc::new(Mutex::new(Vec::new()));
        let mut dispatcher =
            NotificationDispatcher::start_with_sink(RecordingSink(delivered.clone()), 4).unwrap();

        dispatcher.enqueue("first".into(), "one".into());
        dispatcher.enqueue("second".into(), "two".into());
        dispatcher.enqueue("third".into(), "three".into());
        dispatcher.shutdown();

        assert_eq!(
            *delivered.lock().unwrap(),
            vec![
                ("first".into(), "one".into()),
                ("second".into(), "two".into()),
                ("third".into(), "three".into()),
            ]
        );
    }

    #[test]
    fn dispatcher_drops_each_notification_when_full_and_resumes_afterward() {
        let (delivered_sender, delivered) = mpsc::channel();
        let (release, release_receiver) = mpsc::channel();
        let sink = BlockingFirstSink {
            first: AtomicBool::new(true),
            delivered: delivered_sender,
            release: Mutex::new(release_receiver),
        };
        let mut dispatcher = NotificationDispatcher::start_with_sink(sink, 2).unwrap();

        assert_eq!(
            dispatcher.enqueue_inner("first".into(), String::new()),
            EnqueueOutcome::Queued
        );
        assert_eq!(delivered.recv().unwrap(), "first");
        assert_eq!(
            dispatcher.enqueue_inner("second".into(), String::new()),
            EnqueueOutcome::Queued
        );
        assert_eq!(
            dispatcher.enqueue_inner("third".into(), String::new()),
            EnqueueOutcome::Queued
        );
        assert_eq!(
            dispatcher.enqueue_inner("dropped one".into(), String::new()),
            EnqueueOutcome::Dropped
        );
        assert_eq!(
            dispatcher.enqueue_inner("dropped two".into(), String::new()),
            EnqueueOutcome::Dropped
        );

        release.send(()).unwrap();
        assert_eq!(delivered.recv().unwrap(), "second");
        assert_eq!(
            dispatcher.enqueue_inner("after recovery".into(), String::new()),
            EnqueueOutcome::Queued
        );
        dispatcher.shutdown();

        let remaining: Vec<_> = delivered.try_iter().collect();
        assert_eq!(remaining, vec!["third", "after recovery"]);
    }

    #[test]
    fn shutdown_detaches_a_stuck_notification_worker_after_timeout() {
        let (delivered_sender, delivered) = mpsc::channel();
        let (release, release_receiver) = mpsc::channel();
        let sink = BlockingFirstSink {
            first: AtomicBool::new(true),
            delivered: delivered_sender,
            release: Mutex::new(release_receiver),
        };
        let mut dispatcher = NotificationDispatcher::start_with_sink(sink, 1).unwrap();
        dispatcher.enqueue("blocked".into(), String::new());
        assert_eq!(delivered.recv().unwrap(), "blocked");

        let started = Instant::now();
        assert_eq!(
            dispatcher.shutdown_with_timeout(Duration::from_millis(20)),
            ShutdownOutcome::Detached
        );
        assert!(started.elapsed() < Duration::from_secs(1));

        // Let the detached test worker finish rather than leaking it into later tests.
        release.send(()).unwrap();
    }

    #[test]
    fn filter_sends_immediately_when_delay_is_zero() {
        let mut filter = ConnectionNotificationFilter::new(Duration::ZERO);
        let now = Instant::now();

        let action = filter.handle_notification(
            "error: internal.tunnel.srv1".into(),
            "ssh error".into(),
            now,
        );
        assert_eq!(
            action,
            FilterAction::Send {
                title: "error: internal.tunnel.srv1".into(),
                body: "ssh error".into(),
            }
        );
        assert_eq!(filter.next_deadline(), None);

        let action =
            filter.handle_notification("OK: internal.tunnel.srv1".into(), String::new(), now);
        assert_eq!(
            action,
            FilterAction::Send {
                title: "OK: internal.tunnel.srv1".into(),
                body: String::new(),
            }
        );
    }

    #[test]
    fn filter_never_delays_service_incidents() {
        let mut filter = ConnectionNotificationFilter::new(Duration::from_secs(10));
        let now = Instant::now();

        let action =
            filter.handle_notification("error: srv1.disk".into(), "disk is full".into(), now);
        assert_eq!(
            action,
            FilterAction::Send {
                title: "error: srv1.disk".into(),
                body: "disk is full".into(),
            }
        );
        assert_eq!(filter.next_deadline(), None);

        let action = filter.handle_notification("OK: srv1.disk".into(), String::new(), now);
        assert_eq!(
            action,
            FilterAction::Send {
                title: "OK: srv1.disk".into(),
                body: String::new(),
            }
        );
    }

    #[test]
    fn filter_suppresses_transient_connection_error_and_recovery() {
        let mut filter = ConnectionNotificationFilter::new(Duration::from_secs(10));
        let t0 = Instant::now();

        // 1. Error arrives -> suppressed, deadline set to t0 + 10s
        let action =
            filter.handle_notification("error: internal.tunnel.srv1".into(), "ssh down".into(), t0);
        assert_eq!(action, FilterAction::Suppress);
        assert_eq!(filter.next_deadline(), Some(t0 + Duration::from_secs(10)));

        // 2. Recovery arrives at t0 + 2s -> suppressed, error cancelled
        let action = filter.handle_notification(
            "OK: internal.tunnel.srv1".into(),
            String::new(),
            t0 + Duration::from_secs(2),
        );
        assert_eq!(action, FilterAction::Suppress);
        assert_eq!(filter.next_deadline(), None);

        // 3. At t0 + 10s, nothing is due
        assert!(filter.drain_due(t0 + Duration::from_secs(10)).is_empty());
    }

    #[test]
    fn filter_uses_event_time_when_recovery_waited_in_runtime_queue() {
        let mut filter = ConnectionNotificationFilter::new(Duration::from_secs(10));
        let t0 = Instant::now();
        let wall_t0 = OffsetDateTime::UNIX_EPOCH;

        assert_eq!(
            filter.handle_notification_at(
                "error: internal.connection.srv1".into(),
                "socket closed".into(),
                wall_t0,
                t0 + Duration::from_secs(15),
                wall_t0 + time::Duration::seconds(15),
            ),
            FilterAction::Suppress
        );

        // The recovery occurred after five seconds, but runtime did not process it
        // until fifteen seconds after the error.
        assert_eq!(
            filter.handle_notification_at(
                "OK: internal.connection.srv1".into(),
                String::new(),
                wall_t0 + time::Duration::seconds(5),
                t0 + Duration::from_secs(15),
                wall_t0 + time::Duration::seconds(15),
            ),
            FilterAction::Suppress
        );
        assert_eq!(filter.next_deadline(), None);
    }

    #[test]
    fn filter_preserves_error_then_ok_order_when_late_recovery_waited_in_queue() {
        let mut filter = ConnectionNotificationFilter::new(Duration::from_secs(10));
        let t0 = Instant::now();
        let wall_t0 = OffsetDateTime::UNIX_EPOCH;

        filter.handle_notification_at(
            "error: internal.connection.srv1".into(),
            "socket closed".into(),
            wall_t0,
            t0 + Duration::from_secs(15),
            wall_t0 + time::Duration::seconds(15),
        );

        assert_eq!(
            filter.handle_notification_at(
                "OK: internal.connection.srv1".into(),
                String::new(),
                wall_t0 + time::Duration::seconds(12),
                t0 + Duration::from_secs(15),
                wall_t0 + time::Duration::seconds(15),
            ),
            FilterAction::SendBatch {
                notifications: vec![
                    (
                        "error: internal.connection.srv1".into(),
                        "socket closed".into(),
                    ),
                    ("OK: internal.connection.srv1".into(), String::new()),
                ],
            }
        );
        assert_eq!(filter.next_deadline(), None);
    }

    #[test]
    fn filter_suppresses_repeated_errors_and_preserves_original_deadline() {
        let mut filter = ConnectionNotificationFilter::new(Duration::from_secs(10));
        let t0 = Instant::now();

        filter.handle_notification(
            "error: internal.tunnel.srv1".into(),
            "ssh attempt 1".into(),
            t0,
        );

        // Subsequent error at t0 + 3s updates body but preserves t0 + 10s deadline
        let action = filter.handle_notification(
            "error: internal.connection.srv1".into(),
            "socket closed".into(),
            t0 + Duration::from_secs(3),
        );
        assert_eq!(action, FilterAction::Suppress);
        assert_eq!(filter.next_deadline(), Some(t0 + Duration::from_secs(10)));

        // At t0 + 9s, still not due
        assert!(filter.drain_due(t0 + Duration::from_secs(9)).is_empty());

        // At t0 + 10s, due and delivers latest error
        let due = filter.drain_due(t0 + Duration::from_secs(10));
        assert_eq!(
            due,
            vec![(
                "error: internal.connection.srv1".into(),
                "socket closed".into()
            )]
        );
        assert_eq!(filter.next_deadline(), None);
    }

    #[test]
    fn filter_delivers_persistent_error_after_delay_and_notifies_recovery() {
        let mut filter = ConnectionNotificationFilter::new(Duration::from_secs(10));
        let t0 = Instant::now();

        filter.handle_notification("error: internal.tunnel.srv1".into(), "ssh down".into(), t0);

        // Delay expires
        let due = filter.drain_due(t0 + Duration::from_secs(10));
        assert_eq!(
            due,
            vec![("error: internal.tunnel.srv1".into(), "ssh down".into())]
        );

        // When connection recovers, OK is sent because error was notified
        let action = filter.handle_notification(
            "OK: internal.tunnel.srv1".into(),
            String::new(),
            t0 + Duration::from_secs(20),
        );
        assert_eq!(
            action,
            FilterAction::Send {
                title: "OK: internal.tunnel.srv1".into(),
                body: String::new(),
            }
        );
    }

    #[test]
    fn filter_tracks_multiple_servers_independently() {
        let mut filter = ConnectionNotificationFilter::new(Duration::from_secs(10));
        let t0 = Instant::now();

        // Server A fails at t0
        filter.handle_notification("error: internal.tunnel.srvA".into(), "down A".into(), t0);

        // Server B fails at t0 + 3s
        filter.handle_notification(
            "error: internal.tunnel.srvB".into(),
            "down B".into(),
            t0 + Duration::from_secs(3),
        );

        // Server A recovers at t0 + 5s (within delay)
        filter.handle_notification(
            "OK: internal.tunnel.srvA".into(),
            String::new(),
            t0 + Duration::from_secs(5),
        );

        // Earliest deadline is now Server B at t0 + 13s
        assert_eq!(filter.next_deadline(), Some(t0 + Duration::from_secs(13)));

        // At t0 + 10s (when Server A would have expired), nothing is due
        assert!(filter.drain_due(t0 + Duration::from_secs(10)).is_empty());

        // At t0 + 13s, Server B is due
        let due = filter.drain_due(t0 + Duration::from_secs(13));
        assert_eq!(
            due,
            vec![("error: internal.tunnel.srvB".into(), "down B".into())]
        );
    }

    #[test]
    fn filter_accepts_a_new_error_after_a_silent_recovery() {
        let mut filter = ConnectionNotificationFilter::new(Duration::from_secs(10));
        let t0 = Instant::now();

        filter.handle_notification(
            "error: internal.connection.srv1".into(),
            "first outage".into(),
            t0,
        );
        assert_eq!(
            filter.drain_due(t0 + Duration::from_secs(10)),
            vec![(
                "error: internal.connection.srv1".into(),
                "first outage".into()
            )]
        );

        // A snoozed recovery does not produce an OK notification, so runtime
        // must reset the filter explicitly from the domain transition.
        filter.reset_server("srv1");

        assert_eq!(
            filter.handle_notification(
                "error: internal.connection.srv1".into(),
                "second outage".into(),
                t0 + Duration::from_secs(20),
            ),
            FilterAction::Suppress
        );
        assert_eq!(filter.next_deadline(), Some(t0 + Duration::from_secs(30)));
    }
}
