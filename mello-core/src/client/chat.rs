use crate::events::Event;

impl super::Client {
    pub(super) async fn handle_send_message(&self, content: &str, reply_to: Option<&str>) {
        let envelope = crate::chat::MessageEnvelope::text(content, reply_to.map(String::from));
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

    pub(super) async fn handle_edit_message(&self, message_id: &str, new_body: &str) {
        let envelope = crate::chat::MessageEnvelope::text(new_body, None);
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
        if let Err(e) = self.nakama.remove_chat_message(message_id).await {
            log::error!("Failed to delete message: {}", e);
        }
    }

    pub(super) async fn handle_search_gifs(&self, query: &str) {
        match self.giphy.search(query, 20).await {
            Ok(results) => {
                let gifs: Vec<_> = results.iter().filter_map(|r| r.to_gif_data()).collect();
                let _ = self.event_tx.send(Event::GifsLoaded { gifs });
            }
            Err(e) => log::error!("GIF search failed: {}", e),
        }
    }

    pub(super) async fn handle_trending_gifs(&self) {
        match self.giphy.trending(20).await {
            Ok(results) => {
                let gifs: Vec<_> = results.iter().filter_map(|r| r.to_gif_data()).collect();
                let _ = self.event_tx.send(Event::GifsLoaded { gifs });
            }
            Err(e) => log::error!("Trending GIFs failed: {}", e),
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
            .list_channel_messages_with_cursor(&channel_id, 50, effective_cursor)
            .await
        {
            Ok((mut messages, next_cursor)) => {
                messages.reverse();
                self.history_cursor = next_cursor.clone();
                let _ = self.event_tx.send(Event::HistoryLoaded {
                    messages,
                    cursor: next_cursor,
                });
            }
            Err(e) => log::error!("Failed to load history: {}", e),
        }
    }
}
