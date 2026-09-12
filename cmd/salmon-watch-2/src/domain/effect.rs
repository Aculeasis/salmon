/// Side effects requested by a pure domain transition and executed by runtime code.
#[derive(Clone, Debug, Eq, PartialEq)]
pub enum Effect {
    /// Delivers a desktop notification after the corresponding state mutation.
    Notify {
        /// Short summary suitable for the notification heading.
        title: String,
        /// Optional incident details; empty for resolution notifications.
        body: String,
    },
    /// Writes the reducer's complete snooze map to persistent storage.
    PersistSnoozes,
}

/// Outcome of reducing one event.
#[derive(Clone, Debug, Default, Eq, PartialEq)]
pub struct Transition {
    /// Whether consumers need a fresh [`super::UiSnapshot`].
    pub changed: bool,
    /// Ordered effects to perform after the state mutation is complete.
    pub effects: Vec<Effect>,
}
