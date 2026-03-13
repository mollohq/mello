use global_hotkey::{
    hotkey::HotKey, GlobalHotKeyEvent, GlobalHotKeyManager,
};

pub struct HotkeyManager {
    manager: GlobalHotKeyManager,
    ptt_id: Option<u32>,
}

impl HotkeyManager {
    pub fn new() -> Result<Self, Box<dyn std::error::Error>> {
        Ok(Self {
            manager: GlobalHotKeyManager::new()?,
            ptt_id: None,
        })
    }

    pub fn register_ptt(&mut self, hotkey: HotKey) -> Result<(), Box<dyn std::error::Error>> {
        // Unregister previous if set
        if let Some(id) = self.ptt_id.take() {
            let prev = HotKey::new(None, global_hotkey::hotkey::Code::Unidentified);
            // We can't reconstruct the old hotkey, so just try unregister
            let _ = self.manager.unregister(prev);
            let _ = id; // consumed
        }
        let id = hotkey.id();
        self.manager.register(hotkey)?;
        self.ptt_id = Some(id);
        Ok(())
    }

    pub fn ptt_id(&self) -> Option<u32> {
        self.ptt_id
    }

    /// Poll for hotkey events — call from the event loop.
    pub fn poll() -> Option<GlobalHotKeyEvent> {
        GlobalHotKeyEvent::receiver().try_recv().ok()
    }
}
