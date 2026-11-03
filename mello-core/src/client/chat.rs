use crate::events::Event;

impl super::Client {
    pub(super) async fn handle_send_message(
        &self,
        content: &str,
        reply_to: Option<&str>,
        mentions: &[crate::chat::MentionRef],
    ) {
        let body = crate::chat::encode_mentions(content, mentions);
        let envelope = crate::chat::MessageEnvelope::text(&body, reply_to.map(String::from));
        let json = match serde_json::to_string(&envelope) {
            Ok(j) => j,
            Err(e) => {
                log::error!("Failed to serialize message envelope: {}", e);
                return;
            }
        };
        if let Err(e) = self.nakama.send_raw_chat_message(&json).await {
            log::error!("Failed to send message: {}", e);
        }
    }

    pub(super) async fn handle_send_gif(&self, gif: crate::chat::GifData, body: &str) {
        let envelope = crate::chat::MessageEnvelope::gif(gif, body);
        let json = match serde_json::to_string(&envelope) {
            Ok(j) => j,
            Err(e) => {
                log::error!("Failed to serialize GIF envelope: {}", e);
                return;
            }
        };
        if let Err(e) = self.nakama.send_raw_chat_message(&json).await {
            log::error!("Failed to send GIF message: {}", e);
        }
    }

    /// Ask the server for a slot on the chat bucket, PUT the bytes there, and
    /// tell the UI the image is ready to send. Every failure reaches the UI
    /// as `ChatImageUploadFailed` with one sentence.
    pub(super) async fn handle_upload_chat_image(
        &self,
        attachment_id: String,
        mime: String,
        width: u32,
        height: u32,
        bytes: Vec<u8>,
    ) {
        let fail = |reason: &str| {
            log::warn!("[chat] image {} upload failed: {}", attachment_id, reason);
            let _ = self.event_tx.send(Event::ChatImageUploadFailed {
                attachment_id: attachment_id.clone(),
                reason: reason.to_string(),
            });
        };
        let Some(crew_id) = self.nakama.active_crew_id().map(str::to_string) else {
            fail("No crew is open.");
            return;
        };
        let req = crate::crew_events::ChatUploadURLRequest {
            crew_id,
            mime: mime.clone(),
            bytes: bytes.len() as u64,
        };
        let slot = match self.nakama.chat_upload_url(&req).await {
            Ok(r) => r,
            Err(e) => {
                log::warn!("[chat] chat_upload_url failed: {}", e);
                fail("The server did not give an upload slot.");
                return;
            }
        };
        if slot.upload_url.is_empty() || slot.media_url.is_empty() {
            fail("Image upload is off on this server.");
            return;
        }
        let http = reqwest::Client::new();
        match http
            .put(&slot.upload_url)
            .header("Content-Type", &mime)
            .body(bytes.clone())
            .send()
            .await
        {
            Ok(resp) if resp.status().is_success() => {}
            Ok(resp) => {
                log::warn!("[chat] image PUT answered HTTP {}", resp.status());
                fail("The upload was refused.");
                return;
            }
            Err(e) => {
                log::warn!("[chat] image PUT failed: {}", e);
                fail("The upload did not complete.");
                return;
            }
        }
        log::info!(
            "[chat] image {} uploaded: {} bytes, {}x{}",
            attachment_id,
            bytes.len(),
            width,
            height
        );
        let _ = self.event_tx.send(Event::ChatImageUploaded {
            attachment_id,
            image: crate::chat::ImageData {
                url: slot.media_url,
                width,
                height,
                bytes: bytes.len() as u64,
                mime,
            },
        });
    }

    pub(super) async fn handle_send_images(
        &self,
        images: Vec<crate::chat::ImageData>,
        content: &str,
        reply_to: Option<&str>,
        mentions: &[crate::chat::MentionRef],
    ) {
        if images.is_empty() {
            log::warn!("[chat] SendImages with no images, dropped");
            return;
        }
        let body = crate::chat::encode_mentions(content, mentions);
        let envelope =
            crate::chat::MessageEnvelope::image(images, &body, reply_to.map(String::from));
        let json = match serde_json::to_string(&envelope) {
            Ok(j) => j,
            Err(e) => {
                log::error!("Failed to serialize image envelope: {}", e);
                return;
            }
        };
        if let Err(e) = self.nakama.send_raw_chat_message(&json).await {
            log::error!("Failed to send image message: {}", e);
        }
    }

    /// The bucket keys of `urls`: the path under the public host. The server
    /// accepts only the caller's own keys, so a wrong URL is refused there.
    pub(super) async fn handle_delete_chat_images(&self, urls: Vec<String>) {
        let keys: Vec<String> = urls
            .iter()
            .filter_map(|u| crate::chat::image_key_from_url(u))
            .collect();
        if keys.is_empty() {
            return;
        }
        let req = crate::crew_events::ChatDeleteImagesRequest { keys };
        match self.nakama.chat_delete_images(&req).await {
            Ok(()) => log::info!("[chat] removed {} image objects", req.keys.len()),
            Err(e) => log::warn!("[chat] chat_delete_images failed: {}", e),
        }
    }

    pub(super) async fn handle_edit_message(
        &self,
        message_id: &str,
        new_body: &str,
        mentions: &[crate::chat::MentionRef],
    ) {
        let body = crate::chat::encode_mentions(new_body, mentions);
        let envelope = crate::chat::MessageEnvelope::text(&body, None);
        let json = match serde_json::to_string(&envelope) {
            Ok(j) => j,
            Err(e) => {
                log::error!("Failed to serialize edit envelope: {}", e);
                return;
            }
        };
        if let Err(e) = self.nakama.update_chat_message(message_id, &json).await {
            log::error!("Failed to edit message: {}", e);
        }
    }

    pub(super) async fn handle_delete_message(&self, message_id: &str) {
        log::info!("Deleting chat message {}", message_id);
        if let Err(e) = self.nakama.remove_chat_message(message_id).await {
            log::error!("Failed to delete message {}: {}", message_id, e);
        }
    }

    pub(super) async fn handle_search_gifs(&self, query: &str) {
        match self.giphy.search(query, 20).await {
            Ok(results) => {
                let gifs: Vec<_> = results.iter().filter_map(|r| r.to_gif_data()).collect();
                let _ = self.event_tx.send(Event::GifsLoaded { gifs });
            }
            Err(e) => {
                log::error!("GIF search failed: {}", e);
                let _ = self.event_tx.send(Event::GifsFailed { error: e });
            }
        }
    }

    pub(super) async fn handle_trending_gifs(&self) {
        match self.giphy.trending(20).await {
            Ok(results) => {
                let gifs: Vec<_> = results.iter().filter_map(|r| r.to_gif_data()).collect();
                let _ = self.event_tx.send(Event::GifsLoaded { gifs });
            }
            Err(e) => {
                log::error!("Trending GIFs failed: {}", e);
                let _ = self.event_tx.send(Event::GifsFailed { error: e });
            }
        }
    }

    pub(super) async fn handle_mark_chat_read(&self, crew_id: &str, message_id: &str, time: &str) {
        let cursor = crate::chat::ChatReadCursor {
            message_id: message_id.to_string(),
            time: time.to_string(),
        };
        match self.nakama.write_chat_cursor(crew_id, &cursor).await {
            Ok(()) => log::debug!("[chat] read cursor for {} at {}", crew_id, message_id),
            Err(e) => log::warn!("[chat] write read cursor for {} failed: {}", crew_id, e),
        }
    }

    pub(super) async fn handle_load_history(&mut self, cursor: Option<&str>) {
        let effective_cursor = cursor.or(self.history_cursor.as_deref());
        if effective_cursor.is_none() {
            log::debug!("No history cursor, nothing more to load");
            return;
        }

        let channel_id = match self.nakama.channel_id().await {
            Some(id) => id,
            None => return,
        };

        match self
            .nakama
            .list_display_chat_messages(&channel_id, 50, effective_cursor)
            .await
        {
            Ok((messages, next_cursor)) => {
                self.history_cursor = next_cursor.clone();
                let _ = self.event_tx.send(Event::HistoryLoaded {
                    messages,
                    cursor: next_cursor,
                });
            }
            Err(e) => {
                log::error!("Failed to load history: {}", e);
                let _ = self.event_tx.send(Event::HistoryLoadFailed);
            }
        }
    }
}
