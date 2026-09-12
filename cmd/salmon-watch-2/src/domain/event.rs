use super::NotificationData;

/// Every external input accepted by the domain reducer.
///
/// All `at` and `until` values are Unix seconds. Network producers and UI
/// commands share this stream, so the reducer is the single authority for
/// state transitions and their side effects.
#[derive(Clone, Debug, Eq, PartialEq)]
pub enum Event {
    Connected {
        server_id: String,
        at: i64,
    },
    Disconnected {
        server_id: String,
        at: i64,
        /// Empty when the owning tunnel process already reported the root cause.
        error: String,
    },
    TunnelReady {
        server_id: String,
        at: i64,
    },
    TunnelFailed {
        server_id: String,
        at: i64,
        error: String,
    },
    Heartbeat {
        server_id: String,
        at: i64,
    },
    Notification {
        server_id: String,
        data: NotificationData,
        /// Local receipt time used to evaluate snooze deadlines.
        at: i64,
    },
    Snooze {
        key: String,
        until: i64,
    },
    Unsnooze {
        key: String,
    },
    ForgetStale {
        /// Fully qualified key; forgetting never implies remote recovery.
        key: String,
    },
    Tick {
        at: i64,
    },
}
