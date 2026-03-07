//! Main client struct

use crate::{Config, Event, Result};

pub struct Client {
    config: Config,
    // TODO: Add Nakama client, voice manager, stream manager
}

impl Client {
    pub async fn new(config: Config) -> Result<Self> {
        log::info!("Initializing Mello client...");
        
        // TODO: Initialize subsystems
        
        Ok(Self { config })
    }
    
    pub fn poll_event(&mut self) -> Option<Event> {
        // TODO: Poll event queue
        None
    }
    
    pub async fn tick(&mut self) {
        // TODO: Tick subsystems
    }
}
