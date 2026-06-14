use crate::crew_events::DiagnosticLogUploadURLRequest;
use crate::events::Event;

impl super::Client {
    /// Upload a sliced diagnostic log file to private storage via a presigned
    /// PUT URL (mirrors the clip upload path). Emits `DiagnosticCaptureState`
    /// so the debug panel can show uploading/done/failed. The local slice is
    /// removed afterwards regardless of outcome.
    pub(super) async fn handle_upload_diagnostic_log(&self, local_path: &str, capture_id: &str) {
        let emit = |phase: &str, message: String| {
            let _ = self.event_tx.send(Event::DiagnosticCaptureState {
                phase: phase.to_string(),
                message,
            });
        };

        let bytes = match std::fs::read(local_path) {
            Ok(b) => b,
            Err(e) => {
                log::warn!("diagnostic upload: cannot read {}: {}", local_path, e);
                emit("failed", format!("cannot read capture file: {}", e));
                return;
            }
        };

        let req = DiagnosticLogUploadURLRequest {
            capture_id: capture_id.to_string(),
        };
        let url_resp = match self.nakama.diagnostic_log_upload_url(&req).await {
            Ok(r) => r,
            Err(e) => {
                log::warn!("diagnostic_log_upload_url failed: {}", e);
                emit("failed", format!("could not get upload URL: {}", e));
                self.cleanup_capture_file(local_path);
                return;
            }
        };

        if url_resp.upload_url.is_empty() {
            log::info!("diagnostic upload: storage not configured, skipping");
            emit("skipped", "diagnostic storage not configured".to_string());
            self.cleanup_capture_file(local_path);
            return;
        }

        let http = reqwest::Client::new();
        let result = http
            .put(&url_resp.upload_url)
            .header("Content-Type", "text/plain")
            .body(bytes)
            .send()
            .await;

        match result {
            Ok(resp) if resp.status().is_success() => {
                log::info!(
                    "diagnostic log uploaded: capture_id={} key={}",
                    capture_id,
                    url_resp.key
                );
                emit("done", url_resp.key);
            }
            Ok(resp) => {
                log::warn!(
                    "diagnostic upload HTTP {}: capture_id={}",
                    resp.status(),
                    capture_id
                );
                emit("failed", format!("upload HTTP {}", resp.status()));
            }
            Err(e) => {
                log::warn!("diagnostic upload failed: {} capture_id={}", e, capture_id);
                emit("failed", format!("upload error: {}", e));
            }
        }

        self.cleanup_capture_file(local_path);
    }

    fn cleanup_capture_file(&self, local_path: &str) {
        if let Err(e) = std::fs::remove_file(local_path) {
            log::debug!("could not remove capture slice {}: {}", local_path, e);
        }
    }
}
