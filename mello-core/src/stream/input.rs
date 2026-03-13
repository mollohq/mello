use super::error::StreamError;

/// Opaque input event — encoding TBD in input passthrough spec.
pub struct InputEvent {
    pub raw: Vec<u8>,
}

/// Input passthrough allows a viewer to send keyboard/mouse events to the host.
/// Full spec deferred; this trait defines the interface so the rest of the
/// system can account for it.
pub trait InputPassthrough: Send + Sync {
    /// Viewer side: send an input event to the host.
    fn send_event(&self, event: InputEvent) -> Result<(), StreamError>;

    /// Host side: register a callback to receive input events from viewers.
    fn on_event(&self, callback: Box<dyn Fn(InputEvent) + Send + Sync>);
}

/// No-op implementation used until the feature is specced and built.
pub struct InputPassthroughStub;

impl InputPassthrough for InputPassthroughStub {
    fn send_event(&self, _: InputEvent) -> Result<(), StreamError> {
        Err(StreamError::NotImplemented)
    }

    fn on_event(&self, _: Box<dyn Fn(InputEvent) + Send + Sync>) {}
}
