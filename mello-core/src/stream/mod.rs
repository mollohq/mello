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

    pub fn is_hosting(&self) -> bool {
        self.hosting
    }

    pub fn is_watching(&self) -> bool {
        self.watching.is_some()
    }
}
