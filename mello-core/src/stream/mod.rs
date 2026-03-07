//! Stream (video) management

pub struct StreamManager {
    hosting: bool,
    watching: Option<String>,
}

impl StreamManager {
    pub fn new() -> Self {
        Self {
            hosting: false,
            watching: None,
        }
    }
    
    pub fn start_hosting(&mut self) {
        self.hosting = true;
        // TODO: Call libmello
    }
    
    pub fn stop_hosting(&mut self) {
        self.hosting = false;
        // TODO: Call libmello
    }
}
