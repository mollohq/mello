pub struct VoiceManager {
    muted: bool,
    deafened: bool,
}

impl VoiceManager {
    pub fn new() -> Self {
        Self {
            muted: false,
            deafened: false,
        }
    }

    pub fn set_mute(&mut self, muted: bool) {
        self.muted = muted;
    }

    pub fn set_deafen(&mut self, deafened: bool) {
        self.deafened = deafened;
    }

    pub fn is_muted(&self) -> bool {
        self.muted
    }

    pub fn is_deafened(&self) -> bool {
        self.deafened
    }
}
