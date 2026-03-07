//! Event types for UI callbacks

#[derive(Debug, Clone)]
pub enum Event {
    Connected,
    Disconnected { reason: String },
    Error { message: String },
    // TODO: Add more events
}
