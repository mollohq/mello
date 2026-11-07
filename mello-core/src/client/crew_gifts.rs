//! The crew quests gifts (plans/crew-quests.md section 4): the banner, the
//! custom invite name and crew emoji, through the RPCs of `crew_gifts.go`.
//! An upload asks for a slot on the media bucket and PUTs the bytes there,
//! as a chat image does. After a change, crew state and the feed load again,
//! so the crew header, the quest card and the chat see it.

use crate::events::Event;

/// The largest media image `FetchMediaImage` takes: a banner is under
/// 400 KB and an emoji under 256 KB on the server.
const MAX_MEDIA_BYTES: usize = 2 << 20;

#[derive(serde::Deserialize, Default)]
struct UploadSlot {
    #[serde(default)]
    upload_url: String,
    #[serde(default)]
    key: String,
}

/// The message of a Nakama RPC error, for the UI: "name taken" from
/// `{"code":6,"message":"name taken"}`. Anything else is one plain sentence.
fn server_reason(e: &crate::Error) -> String {
    if let crate::Error::Server(text) = e {
        if let Ok(v) = serde_json::from_str::<serde_json::Value>(text) {
            if let Some(m) = v.get("message").and_then(|m| m.as_str()) {
                if !m.is_empty() {
                    let mut c = m.chars();
                    let first = c
                        .next()
                        .map(|f| f.to_uppercase().collect::<String>())
                        .unwrap_or_default();
                    return format!("{first}{}.", c.as_str().trim_end_matches('.'));
                }
            }
        }
    }
    "The server did not answer.".into()
}

impl super::Client {
    fn gift_failed(&self, crew_id: &str, gift: &str, reason: String) {
        log::warn!("[gifts] {gift} for crew {crew_id} failed: {reason}");
        let _ = self.event_tx.send(Event::CrewGiftFailed {
            crew_id: crew_id.to_string(),
            gift: gift.to_string(),
            reason,
        });
    }

    /// A gift changed: say so, then load crew state and the feed again.
    async fn gift_changed(&self, crew_id: &str, gift: &str) {
        log::info!("[gifts] {gift} changed for crew {crew_id}");
        let _ = self.event_tx.send(Event::CrewGiftChanged {
            crew_id: crew_id.to_string(),
            gift: gift.to_string(),
        });
        if self.nakama.active_crew_id() == Some(crew_id) {
            self.handle_set_active_crew(crew_id).await;
            self.handle_load_crew_feed(crew_id).await;
        }
    }

    /// Ask `rpc` for a slot and PUT the bytes there. The key, or the reason.
    async fn upload_media(
        &self,
        rpc: &str,
        crew_id: &str,
        mime: &str,
        bytes: Vec<u8>,
    ) -> Result<String, String> {
        let payload = serde_json::json!({ "crew_id": crew_id, "mime": mime, "bytes": bytes.len() });
        let slot: UploadSlot = match self.nakama.rpc(rpc, &payload).await {
            Ok(text) => serde_json::from_str(&text).unwrap_or_default(),
            Err(e) => return Err(server_reason(&e)),
        };
        if slot.upload_url.is_empty() || slot.key.is_empty() {
            return Err("Image upload is off on this server.".into());
        }
        match reqwest::Client::new()
            .put(&slot.upload_url)
            .header("Content-Type", mime)
            .header("Cache-Control", "public, max-age=31536000, immutable")
            .body(bytes)
            .send()
            .await
        {
            Ok(resp) if resp.status().is_success() => Ok(slot.key),
            Ok(resp) => {
                log::warn!("[gifts] media PUT answered HTTP {}", resp.status());
                Err("The upload was refused.".into())
            }
            Err(e) => {
                log::warn!("[gifts] media PUT failed: {e}");
                Err("The upload did not complete.".into())
            }
        }
    }

    pub(super) async fn handle_set_crew_banner_game(&self, crew_id: &str, game_id: &str) {
        let payload = serde_json::json!({ "crew_id": crew_id, "kind": "game", "game_id": game_id });
        match self.nakama.rpc("set_crew_banner", &payload).await {
            Ok(_) => self.gift_changed(crew_id, "banner").await,
            Err(e) => self.gift_failed(crew_id, "banner", server_reason(&e)),
        }
    }

    pub(super) async fn handle_upload_crew_banner(&self, crew_id: &str, jpeg: Vec<u8>) {
        let key = match self
            .upload_media("crew_banner_upload_url", crew_id, "image/jpeg", jpeg)
            .await
        {
            Ok(k) => k,
            Err(reason) => return self.gift_failed(crew_id, "banner", reason),
        };
        let payload = serde_json::json!({ "crew_id": crew_id, "kind": "upload", "key": key });
        match self.nakama.rpc("set_crew_banner", &payload).await {
            Ok(_) => self.gift_changed(crew_id, "banner").await,
            Err(e) => self.gift_failed(crew_id, "banner", server_reason(&e)),
        }
    }

    pub(super) async fn handle_clear_crew_banner(&self, crew_id: &str) {
        let payload = serde_json::json!({ "crew_id": crew_id });
        match self.nakama.rpc("clear_crew_banner", &payload).await {
            Ok(_) => self.gift_changed(crew_id, "banner").await,
            Err(e) => self.gift_failed(crew_id, "banner", server_reason(&e)),
        }
    }

    pub(super) async fn handle_check_invite_name(&self, crew_id: &str, name: &str) {
        let payload = serde_json::json!({ "crew_id": crew_id, "name": name });
        let state = match self.nakama.rpc("check_invite_name", &payload).await {
            Ok(text) => serde_json::from_str::<serde_json::Value>(&text)
                .ok()
                .and_then(|v| v.get("state").and_then(|s| s.as_str()).map(str::to_string))
                .unwrap_or_default(),
            Err(e) => {
                log::warn!("[gifts] check_invite_name failed: {e}");
                String::new()
            }
        };
        let _ = self.event_tx.send(Event::InviteNameChecked {
            crew_id: crew_id.to_string(),
            name: name.to_string(),
            state,
        });
    }

    pub(super) async fn handle_set_invite_name(&self, crew_id: &str, name: &str) {
        let payload = serde_json::json!({ "crew_id": crew_id, "name": name });
        match self.nakama.rpc("set_invite_name", &payload).await {
            Ok(_) => self.gift_changed(crew_id, "link").await,
            Err(e) => self.gift_failed(crew_id, "link", server_reason(&e)),
        }
    }

    pub(super) async fn handle_upload_crew_emoji(
        &self,
        crew_id: &str,
        name: &str,
        mime: &str,
        bytes: Vec<u8>,
    ) {
        let key = match self
            .upload_media("crew_emoji_upload_url", crew_id, mime, bytes)
            .await
        {
            Ok(k) => k,
            Err(reason) => return self.gift_failed(crew_id, "emoji", reason),
        };
        let payload = serde_json::json!({ "crew_id": crew_id, "name": name, "key": key });
        match self.nakama.rpc("crew_emoji_add", &payload).await {
            Ok(_) => self.gift_changed(crew_id, "emoji").await,
            Err(e) => self.gift_failed(crew_id, "emoji", server_reason(&e)),
        }
    }

    pub(super) async fn handle_remove_crew_emoji(&self, crew_id: &str, name: &str) {
        let payload = serde_json::json!({ "crew_id": crew_id, "name": name });
        match self.nakama.rpc("crew_emoji_remove", &payload).await {
            Ok(_) => self.gift_changed(crew_id, "emoji").await,
            Err(e) => self.gift_failed(crew_id, "emoji", server_reason(&e)),
        }
    }

    /// Fire and forget: the funnel mark never holds the command loop.
    pub(super) fn handle_report_invite_copied(&self, crew_id: &str) {
        let payload = serde_json::json!({ "crew_id": crew_id });
        match self.nakama.rpc_detached("report_invite_copied", &payload) {
            Ok(fut) => {
                tokio::spawn(async move {
                    if let Err(e) = fut.await {
                        log::debug!("[gifts] report_invite_copied failed: {e}");
                    }
                });
            }
            Err(e) => log::debug!("[gifts] report_invite_copied not sent: {e}"),
        }
    }

    /// Fetch a media image off the command loop. Empty bytes on any failure,
    /// so the UI stops asking.
    pub(super) fn handle_fetch_media_image(&self, url: String) {
        let tx = self.event_tx.clone();
        tokio::spawn(async move {
            let bytes = match reqwest::get(&url).await {
                Ok(resp) if resp.status().is_success() => match resp.bytes().await {
                    Ok(b) if b.len() <= MAX_MEDIA_BYTES => b.to_vec(),
                    Ok(b) => {
                        log::warn!("[gifts] media image of {} bytes refused: {url}", b.len());
                        Vec::new()
                    }
                    Err(e) => {
                        log::warn!("[gifts] media image body failed: {e}");
                        Vec::new()
                    }
                },
                Ok(resp) => {
                    log::warn!("[gifts] media image answered HTTP {}: {url}", resp.status());
                    Vec::new()
                }
                Err(e) => {
                    log::warn!("[gifts] media image fetch failed: {e}");
                    Vec::new()
                }
            };
            let _ = tx.send(Event::MediaImageLoaded { url, bytes });
        });
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_server_error_reads_as_one_sentence() {
        let e = crate::Error::Server(
            r#"{"code":6,"message":"name taken","error":"name taken"}"#.into(),
        );
        assert_eq!(server_reason(&e), "Name taken.");
        let e = crate::Error::Server("<html>bad gateway</html>".into());
        assert_eq!(server_reason(&e), "The server did not answer.");
    }
}
