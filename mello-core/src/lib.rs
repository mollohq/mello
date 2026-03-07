pub mod client;
pub mod command;
pub mod config;
pub mod crew;
pub mod error;
pub mod events;
pub mod nakama;
pub mod session;
pub mod stream;
pub mod voice;

pub use client::Client;
pub use command::Command;
pub use config::Config;
pub use error::{Error, Result};
pub use events::Event;

pub mod prelude {
    pub use crate::{Client, Command, Config, Error, Event, Result};
}
