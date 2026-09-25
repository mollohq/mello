use thiserror::Error;

#[derive(Debug, Error)]
pub enum Error {
    #[error("Not connected")]
    NotConnected,

    #[error("Authentication failed: {0}")]
    AuthFailed(String),

    #[error("Already in a crew")]
    AlreadyInCrew,

    #[error("Crew not found: {0}")]
    CrewNotFound(String),

    #[error("HTTP error: {0}")]
    Http(#[from] reqwest::Error),

    #[error("WebSocket error: {0}")]
    WebSocket(String),

    #[error("JSON error: {0}")]
    Json(#[from] serde_json::Error),

    #[error("Server error: {0}")]
    Server(String),

    #[error("Internal error: {0}")]
    Internal(String),
}

pub type Result<T> = std::result::Result<T, Error>;

impl Error {
    /// The gRPC status code of a Nakama error response, when this error
    /// carries one. Nakama answers a failed RPC with a JSON body such as
    /// `{"code":5,"message":"..."}`.
    pub fn server_code(&self) -> Option<i64> {
        let Error::Server(body) = self else {
            return None;
        };
        serde_json::from_str::<serde_json::Value>(body)
            .ok()?
            .get("code")?
            .as_i64()
    }
}
