use thiserror::Error;

#[derive(Debug, Error)]
pub enum StreamError {
    #[error("Viewer limit reached (max {max})")]
    ViewerLimitReached { max: usize },

    #[error("SFU not implemented")]
    SfuNotImplemented,

    #[error("Unknown stream mode: {0}")]
    UnknownMode(String),

    #[error("Feature not implemented")]
    NotImplemented,

    #[error("Encode failed: {0}")]
    EncodeFailed(String),

    #[error("Send failed: {0}")]
    SendFailed(String),

    #[error("Already streaming")]
    AlreadyStreaming,

    #[error("Already watching")]
    AlreadyWatching,

    #[error("Not streaming")]
    NotStreaming,

    #[error("Backend error: {0}")]
    Backend(String),
}
