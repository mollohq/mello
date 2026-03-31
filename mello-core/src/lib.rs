pub mod auth_discord;
pub mod auth_google;
pub mod chat;
pub mod client;
pub mod command;
pub mod config;
pub mod crew;
pub mod crew_events;
pub mod crew_state;
pub mod emoji;
pub mod error;
pub mod events;
pub mod giphy;
pub mod nakama;
pub mod oauth;
pub mod presence;
pub mod session;
pub mod stream;
pub mod transport;
pub mod voice;

pub use client::{Client, FrameSlot};
pub use command::Command;
pub use config::Config;
pub use error::{Error, Result};
pub use events::Event;
pub use stream::{Codec, QualityPreset, StreamConfig, StreamError};
pub use voice::AudioDevice;

/// Protocol version this build speaks. Bump on breaking client↔server changes.
pub const PROTOCOL_VERSION: u32 = 1;
/// Oldest server protocol this client can tolerate.
pub const MIN_SERVER_PROTOCOL: u32 = 1;

pub mod prelude {
    pub use crate::{Client, Command, Config, Error, Event, Result};
}
