//! The client side of the game clip upload (`crate::clip_upload`): the
//! worker starts at the first sign-in and uploads only the signed-in user's
//! clips, also those that a restart left in the queue.

use crate::clip_upload::{PendingClip, UploadWorker};

impl super::Client {
    /// Each sign-in. Game clips are Windows only until the macOS work (plan 9.2).
    pub(super) fn start_clip_uploads(&mut self) {
        if !cfg!(windows) {
            return;
        }
        if self.clip_uploads.is_none() {
            self.clip_uploads = UploadWorker::start(
                self.event_tx.clone(),
                self.nakama.config().http_base(),
                self.nakama.shared_token(),
            );
        }
        if let Some(worker) = &self.clip_uploads {
            worker.set_user(self.nakama.current_user_id().map(String::from));
        }
    }

    /// A sign-out: the queue stays for the next sign-in of the same user.
    pub(super) fn stop_clip_uploads(&self) {
        if let Some(worker) = &self.clip_uploads {
            worker.set_user(None);
        }
    }

    pub(super) fn handle_share_game_clip(&mut self, mut clip: PendingClip) {
        let Some(user_id) = self.nakama.current_user_id() else {
            log::warn!("[clip-upload] {} stays local: not signed in", clip.clip_id);
            return;
        };
        clip.user_id = user_id.to_string();
        if self.clip_uploads.is_none() {
            self.start_clip_uploads();
        }
        match &self.clip_uploads {
            Some(worker) => worker.enqueue(clip),
            None => log::warn!(
                "[clip-upload] {} stays local: no upload worker",
                clip.clip_id
            ),
        }
    }
}
