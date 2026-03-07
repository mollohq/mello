//! Mello Core - Application logic layer

pub mod client;
pub mod config;
pub mod error;
pub mod events;

pub mod nakama;
pub mod crew;
pub mod voice;
pub mod stream;

pub use client::Client;
pub use config::Config;
pub use error::{Error, Result};
pub use events::Event;

/// Prelude for common imports
pub mod prelude {
    pub use crate::{Client, Config, Error, Event, Result};
}
