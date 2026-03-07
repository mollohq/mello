//! Voice chat management

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
        // TODO: Call libmello
    }
    
    pub fn set_deafen(&mut self, deafened: bool) {
        self.deafened = deafened;
        // TODO: Call libmello
    }
}
