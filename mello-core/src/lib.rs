pub mod client;
pub mod command;
pub mod config;
pub mod crew;
pub mod crew_state;
pub mod error;
pub mod events;
pub mod nakama;
pub mod presence;
pub mod session;
pub mod stream;
pub mod voice;

pub use client::Client;
pub use command::Command;
pub use config::Config;
pub use error::{Error, Result};
pub use events::Event;
pub use stream::{Codec, QualityPreset, StreamConfig, StreamError};
pub use voice::AudioDevice;

pub mod prelude {
    pub use crate::{Client, Command, Config, Error, Event, Result};
}
