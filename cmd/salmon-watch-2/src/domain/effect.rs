#[derive(Clone, Debug, Eq, PartialEq)]
pub enum Effect {
    Notify { title: String, body: String },
    PersistSnoozes,
}

#[derive(Clone, Debug, Default, Eq, PartialEq)]
pub struct Transition {
    pub changed: bool,
    pub effects: Vec<Effect>,
}
