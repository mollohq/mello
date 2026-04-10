use crate::crew_events::{ClipUploadCompleteRequest, ClipUploadURLRequest, PostClipRequest};
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

    pub(super) async fn handle_upload_clip(&self, crew_id: &str, clip_id: &str, wav_path: &str) {
        // Step 1: Encode WAV -> MP4/AAC
        let mp4_path = wav_path.replace(".wav", ".mp4");
        let c_wav = std::ffi::CString::new(wav_path).unwrap_or_default();
        let c_mp4 = std::ffi::CString::new(mp4_path.clone()).unwrap_or_default();

        let encode_result =
            unsafe { mello_sys::mello_clip_encode(c_wav.as_ptr(), c_mp4.as_ptr(), 64000) };
        if encode_result != mello_sys::MelloResult_MELLO_OK {
            log::warn!("Clip encode failed: {} (wav={})", encode_result, wav_path);
            return;
        }
        log::info!("Clip encoded: {} -> {}", wav_path, mp4_path);

        // Step 2: Get presigned upload URL
        let url_req = ClipUploadURLRequest {
            clip_id: clip_id.to_string(),
            crew_id: crew_id.to_string(),
        };
        let url_resp = match self.nakama.clip_upload_url(&url_req).await {
            Ok(r) => r,
            Err(e) => {
                log::warn!("clip_upload_url failed: {}", e);
                return;
            }
        };

        if url_resp.upload_url.is_empty() {
            log::info!("S3 not configured, skipping upload for clip {}", clip_id);
            return;
        }

        // Step 3: PUT MP4 to presigned URL
        let mp4_bytes = match std::fs::read(&mp4_path) {
            Ok(b) => b,
            Err(e) => {
                log::warn!("Failed to read MP4 file {}: {}", mp4_path, e);
                return;
            }
        };

        let http = reqwest::Client::new();
        match http
            .put(&url_resp.upload_url)
            .header("Content-Type", "audio/mp4")
            .body(mp4_bytes)
            .send()
            .await
        {
            Ok(resp) if resp.status().is_success() => {
                log::info!("Clip uploaded to R2: clip_id={}", clip_id);
            }
            Ok(resp) => {
                log::warn!("Clip upload HTTP {}: clip_id={}", resp.status(), clip_id);
                return;
            }
            Err(e) => {
                log::warn!("Clip upload failed: {} clip_id={}", e, clip_id);
                return;
            }
        }

        // Step 4: Confirm upload with backend
        let complete_req = ClipUploadCompleteRequest {
            clip_id: clip_id.to_string(),
            crew_id: crew_id.to_string(),
        };
        match self.nakama.clip_upload_complete(&complete_req).await {
            Ok(r) => {
                log::info!("Clip upload confirmed: media_url={}", r.media_url);
                let _ = self.event_tx.send(Event::ClipUploaded {
                    clip_id: clip_id.to_string(),
                    media_url: r.media_url,
                });
            }
            Err(e) => {
                log::warn!("clip_upload_complete failed: {}", e);
            }
        }

        // Step 5: Clean up local WAV (keep MP4 as cache)
        if let Err(e) = std::fs::remove_file(wav_path) {
            log::debug!("Could not remove WAV {}: {}", wav_path, e);
        }
    }

    pub(super) async fn handle_play_clip(&self, path: &str) {
        if path.starts_with("http://") || path.starts_with("https://") {
            self.play_clip_from_url(path).await;
        } else if path.ends_with(".mp4") {
            self.play_local_mp4(path);
        } else {
            self.play_local_wav(path);
        }
        self.emit_playback_started(path);
    }

    fn emit_playback_started(&self, path: &str) {
        let ctx = self.voice.mello_ctx();
        let mut total: u64 = 0;
        let mut sr: u32 = 0;
        unsafe {
            mello_sys::mello_clip_playback_progress(ctx, std::ptr::null_mut(), &mut total, &mut sr);
        }
        if sr == 0 || total == 0 {
            return;
        }
        let duration_ms = (total as u64 * 1000 / sr as u64) as u32;
        let _ = self.event_tx.send(crate::Event::ClipPlaybackStarted {
            clip_path: path.to_string(),
            duration_ms,
        });
    }

    fn play_local_wav(&self, path: &str) {
        let c_path = std::ffi::CString::new(path).unwrap_or_default();
        let result = unsafe { mello_sys::mello_clip_play(self.voice.mello_ctx(), c_path.as_ptr()) };
        if result != mello_sys::MelloResult_MELLO_OK {
            log::warn!("play_clip(wav) failed: {} (path={})", result, path);
        } else {
            log::info!("Playing WAV clip: {}", path);
        }
    }

    fn play_local_mp4(&self, path: &str) {
        let c_path = std::ffi::CString::new(path).unwrap_or_default();
        let result =
            unsafe { mello_sys::mello_clip_play_mp4(self.voice.mello_ctx(), c_path.as_ptr()) };
        if result != mello_sys::MelloResult_MELLO_OK {
            log::warn!("play_clip(mp4) failed: {} (path={})", result, path);
        } else {
            log::info!("Playing MP4 clip: {}", path);
        }
    }

    async fn play_clip_from_url(&self, url: &str) {
        log::info!("Downloading clip from {}", url);
        let http = reqwest::Client::new();
        let resp = match http.get(url).send().await {
            Ok(r) if r.status().is_success() => r,
            Ok(r) => {
                log::warn!("Clip download HTTP {}: {}", r.status(), url);
                return;
            }
            Err(e) => {
                log::warn!("Clip download failed: {} url={}", e, url);
                return;
            }
        };

        let bytes = match resp.bytes().await {
            Ok(b) => b,
            Err(e) => {
                log::warn!("Clip download body read failed: {}", e);
                return;
            }
        };

        let temp_path = std::env::temp_dir()
            .join("mello_clips")
            .join("_playback.mp4");
        if let Err(e) = std::fs::create_dir_all(temp_path.parent().unwrap_or(&std::env::temp_dir()))
        {
            log::warn!("Cannot create temp dir: {}", e);
            return;
        }
        if let Err(e) = std::fs::write(&temp_path, &bytes) {
            log::warn!("Cannot write temp MP4: {}", e);
            return;
        }

        let path_str = temp_path.to_string_lossy().to_string();
        log::info!("Downloaded clip to {}, playing", path_str);
        self.play_local_mp4(&path_str);
    }

    pub(super) fn handle_stop_clip_playback(&self) {
        let result = unsafe { mello_sys::mello_clip_stop_playback(self.voice.mello_ctx()) };
        if result != mello_sys::MelloResult_MELLO_OK {
            log::warn!("stop_clip_playback failed: {}", result);
        }
    }

    pub(super) fn handle_pause_clip(&self) {
        unsafe { mello_sys::mello_clip_pause(self.voice.mello_ctx()) };
    }

    pub(super) fn handle_resume_clip(&self) {
        unsafe { mello_sys::mello_clip_resume(self.voice.mello_ctx()) };
    }

    pub(super) fn handle_seek_clip(&self, position_ms: u32) {
        let mut sr: u32 = 0;
        unsafe {
            mello_sys::mello_clip_playback_progress(
                self.voice.mello_ctx(),
                std::ptr::null_mut(),
                std::ptr::null_mut(),
                &mut sr,
            );
        }
        if sr == 0 {
            return;
        }
        let position_samples = (position_ms as u64 * sr as u64) / 1000;
        unsafe { mello_sys::mello_clip_seek(self.voice.mello_ctx(), position_samples) };
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

    /// Polled from voice_tick (~20ms). Sends progress events every 3rd tick (~60ms).
    pub(super) fn clip_playback_tick(&mut self) {
        let ctx = self.voice.mello_ctx();
        let playing = unsafe { mello_sys::mello_clip_is_playing(ctx) };

        if !playing && self.clip_was_playing {
            self.clip_was_playing = false;
            let _ = self.event_tx.send(Event::ClipPlaybackFinished);
            return;
        }

        if !playing {
            return;
        }

        self.clip_was_playing = true;

        // Throttle: emit progress every 3rd tick (~60ms)
        self.clip_tick_counter = self.clip_tick_counter.wrapping_add(1);
        if self.clip_tick_counter % 3 != 0 {
            return;
        }

        let mut pos: u64 = 0;
        let mut total: u64 = 0;
        let mut sr: u32 = 0;
        unsafe {
            mello_sys::mello_clip_playback_progress(ctx, &mut pos, &mut total, &mut sr);
        }
        if sr == 0 || total == 0 {
            return;
        }

        let position_ms = (pos * 1000 / sr as u64) as u32;
        let duration_ms = (total * 1000 / sr as u64) as u32;

        let _ = self.event_tx.send(Event::ClipPlaybackProgress {
            position_ms,
            duration_ms,
        });
    }
}
