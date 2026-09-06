use super::NotificationData;

#[derive(Clone, Debug, Eq, PartialEq)]
pub enum Event {
    Connected {
        server_id: String,
        at: i64,
    },
    Disconnected {
        server_id: String,
        at: i64,
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
        key: String,
    },
    Tick {
        at: i64,
    },
}
