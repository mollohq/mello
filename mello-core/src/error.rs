//! Error types

use thiserror::Error;

#[derive(Debug, Error)]
pub enum Error {
    #[error("Not connected")]
    NotConnected,
    
    #[error("Authentication failed: {0}")]
    AuthFailed(String),
    
    #[error("Network error: {0}")]
    Network(String),
    
    #[error("Internal error: {0}")]
    Internal(String),
}

pub type Result<T> = std::result::Result<T, Error>;
