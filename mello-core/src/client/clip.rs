use crate::crew_events::PostClipRequest;
use crate::events::Event;
use std::path::PathBuf;
use std::time::{SystemTime, UNIX_EPOCH};

fn generate_clip_id() -> String {
    let ts = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap_or_default()
        .as_millis();
    format!("clip_{}", ts)
}

impl super::Client {
    pub(super) fn handle_start_clip_buffer(&self) {
        let result = unsafe { mello_sys::mello_clip_buffer_start(self.voice.mello_ctx()) };
        if result != mello_sys::MelloResult_MELLO_OK {
            log::error!("Failed to start clip buffer: {}", result);
            let _ = self.event_tx.send(Event::ClipCaptureFailed {
                reason: "Failed to start clip buffer".into(),
            });
            return;
        }
        log::info!("Clip buffer started");
        let _ = self.event_tx.send(Event::ClipBufferStarted);
    }

    pub(super) fn handle_stop_clip_buffer(&self) {
        let result = unsafe { mello_sys::mello_clip_buffer_stop(self.voice.mello_ctx()) };
        if result != mello_sys::MelloResult_MELLO_OK {
            log::warn!("Failed to stop clip buffer: {}", result);
        }
        log::info!("Clip buffer stopped");
        let _ = self.event_tx.send(Event::ClipBufferStopped);
    }

    pub(super) fn handle_capture_clip(&self, seconds: f32) {
        let clip_id = generate_clip_id();
        let filename = format!("{}.wav", &clip_id);

        let clip_dir = std::env::temp_dir().join("mello_clips");
        if let Err(e) = std::fs::create_dir_all(&clip_dir) {
            log::error!("Failed to create clips dir: {}", e);
            let _ = self.event_tx.send(Event::ClipCaptureFailed {
                reason: format!("Cannot create clips directory: {}", e),
            });
            return;
        }

        let output_path: PathBuf = clip_dir.join(&filename);
        let path_str = output_path.to_string_lossy().to_string();

        let c_path = std::ffi::CString::new(path_str.clone()).unwrap_or_default();
        let result = unsafe {
            mello_sys::mello_clip_capture(self.voice.mello_ctx(), seconds, c_path.as_ptr())
        };

        if result != mello_sys::MelloResult_MELLO_OK {
            log::error!("Clip capture failed: {}", result);
            let _ = self.event_tx.send(Event::ClipCaptureFailed {
                reason: "Clip capture returned error".into(),
            });
            return;
        }

        log::info!(
            "Clip captured: {} ({:.1}s) -> {}",
            clip_id,
            seconds,
            path_str
        );
        let _ = self.event_tx.send(Event::ClipCaptured {
            clip_id,
            path: path_str,
            duration_seconds: seconds,
        });
    }

    pub(super) async fn handle_post_clip(
        &self,
        crew_id: &str,
        clip_id: &str,
        duration_seconds: f64,
        local_path: &str,
    ) {
        let req = PostClipRequest {
            crew_id: crew_id.to_string(),
            clip_id: clip_id.to_string(),
            clip_type: "voice".to_string(),
            duration_seconds,
            participants: Vec::new(),
            game: String::new(),
            local_path: local_path.to_string(),
        };
        match self.nakama.post_clip(&req).await {
            Ok(resp) => {
                log::info!(
                    "Clip posted: clip_id={} event_id={}",
                    resp.clip_id,
                    resp.event_id
                );
                let _ = self.event_tx.send(Event::ClipPosted {
                    clip_id: resp.clip_id,
                    event_id: resp.event_id,
                });
            }
            Err(e) => {
                log::warn!("post_clip failed: {}", e);
            }
        }
    }

    pub(super) async fn handle_load_crew_timeline(&self, crew_id: &str, cursor: Option<&str>) {
        match self.nakama.crew_timeline(crew_id, cursor).await {
            Ok(response) => {
                log::info!(
                    "Timeline loaded for crew {}: {} entries",
                    crew_id,
                    response.entries.len()
                );
                let _ = self.event_tx.send(Event::TimelineLoaded { response });
            }
            Err(e) => {
                log::warn!("crew_timeline failed: {}", e);
            }
        }
    }
}
