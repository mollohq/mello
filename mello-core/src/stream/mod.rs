use std::sync::mpsc as std_mpsc;

use crate::events::Event;

#[derive(Debug, Clone)]
pub struct StreamError {
    pub message: String,
}

impl std::fmt::Display for StreamError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "{}", self.message)
    }
}

pub struct StreamManager {
    ctx: *mut mello_sys::MelloContext,
    event_tx: std_mpsc::Sender<Event>,
    hosting: bool,
    watching: Option<String>,
}

impl StreamManager {
    pub fn new(ctx: *mut mello_sys::MelloContext, event_tx: std_mpsc::Sender<Event>) -> Self {
        Self {
            ctx,
            event_tx,
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

    pub fn encoder_available(&self) -> bool {
        if self.ctx.is_null() {
            return false;
        }
        unsafe { mello_sys::mello_encoder_available(self.ctx) }
    }

    pub fn start_hosting(&mut self) -> Result<(), StreamError> {
        if self.hosting {
            return Err(StreamError {
                message: "Already hosting a stream.".into(),
            });
        }

        if !self.encoder_available() {
            let msg = "Streaming requires a hardware encoder \
                       (NVIDIA, AMD, or Intel). None was found on this machine.";
            log::error!("{}", msg);
            return Err(StreamError {
                message: msg.into(),
            });
        }

        self.hosting = true;
        log::info!("Stream hosting started");
        Ok(())
    }

    pub fn stop_hosting(&mut self) {
        if !self.hosting {
            return;
        }
        self.hosting = false;
        log::info!("Stream hosting stopped");
    }
}
