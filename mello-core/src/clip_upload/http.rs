//! The blocking HTTP side of the upload worker: the Nakama RPCs with the
//! client's session token, and the PUTs to the presigned URLs. The worker
//! thread is not a tokio thread, so it uses `reqwest::blocking`.

use std::fs::File;
use std::io::{Read, Seek, SeekFrom};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::Arc;
use std::time::Duration;

use serde::de::DeserializeOwned;
use serde::Serialize;

use crate::crew_events::{
    ClipUploadCompleteResponse, GameClipUploadCompleteRequest, GameClipUploadUrlRequest,
    GameClipUploadUrlResponse, PostClipRequest, PostClipResponse,
};
use crate::nakama::SharedToken;

use super::pace::{PacedReader, RateSource};
use super::worker::{Backend, Failure, Put, Storage};

const RPC_TIMEOUT: Duration = Duration::from_secs(15);
const CONNECT_TIMEOUT: Duration = Duration::from_secs(10);
/// A 5 MiB part at 500 kbps takes 84 s. A pause ends a part after 10 s.
const PUT_TIMEOUT: Duration = Duration::from_secs(15 * 60);

/// The RPCs, with the token that the client loop keeps fresh.
pub(crate) struct HttpBackend {
    base: String,
    token: SharedToken,
    http: Option<reqwest::blocking::Client>,
}

impl HttpBackend {
    pub(crate) fn new(base: String, token: SharedToken) -> Self {
        let http = reqwest::blocking::Client::builder()
            .connect_timeout(CONNECT_TIMEOUT)
            .timeout(RPC_TIMEOUT)
            .build()
            .map_err(|e| log::error!("[clip-upload] no HTTP client for the RPCs: {e}"))
            .ok();
        Self { base, token, http }
    }

    fn rpc<T: Serialize, R: DeserializeOwned>(&self, id: &str, req: &T) -> Result<R, Failure> {
        let http = self
            .http
            .as_ref()
            .ok_or_else(|| Failure::Retry("no HTTP client".into()))?;
        let token = self
            .token
            .read()
            .unwrap_or_else(|e| e.into_inner())
            .clone()
            .ok_or_else(|| Failure::Retry("not signed in".into()))?;
        let payload =
            serde_json::to_string(req).map_err(|e| Failure::Refused(format!("{id}: {e}")))?;
        let resp = http
            .post(format!("{}/v2/rpc/{}", self.base, id))
            .bearer_auth(token)
            .json(&serde_json::Value::String(payload))
            .send()
            .map_err(|e| Failure::Retry(format!("{id}: {e}")))?;
        let status = resp.status().as_u16();
        let text = resp
            .text()
            .map_err(|e| Failure::Retry(format!("{id}: {e}")))?;
        if !(200..300).contains(&status) {
            return Err(rpc_failure(id, status, &text));
        }
        let body: serde_json::Value = serde_json::from_str(&text)
            .map_err(|e| Failure::Retry(format!("{id}: bad answer: {e}")))?;
        let payload = body.get("payload").and_then(|p| p.as_str()).unwrap_or("{}");
        serde_json::from_str(payload)
            .map_err(|e| Failure::Refused(format!("{id}: bad answer: {e}")))
    }
}

/// Nakama's status for an RPC error: 401 is a token that the client loop
/// refreshes, 5xx a server fault; both try again. Any other 4xx is a
/// refusal: an invalid clip, not a member, or the daily clip limit (429).
fn rpc_failure(id: &str, status: u16, text: &str) -> Failure {
    let message = serde_json::from_str::<serde_json::Value>(text)
        .ok()
        .and_then(|v| v.get("message").and_then(|m| m.as_str()).map(String::from))
        .unwrap_or_else(|| text.to_string());
    let why = format!("{id}: HTTP {status}: {message}");
    if status == 401 || status == 408 || status >= 500 {
        Failure::Retry(why)
    } else {
        Failure::Refused(why)
    }
}

impl Backend for HttpBackend {
    fn post_clip(&self, req: &PostClipRequest) -> Result<PostClipResponse, Failure> {
        self.rpc("post_clip", req)
    }

    fn upload_urls(
        &self,
        req: &GameClipUploadUrlRequest,
    ) -> Result<GameClipUploadUrlResponse, Failure> {
        self.rpc("clip_upload_url", req)
    }

    fn complete(
        &self,
        req: &GameClipUploadCompleteRequest,
    ) -> Result<ClipUploadCompleteResponse, Failure> {
        self.rpc("clip_upload_complete", req)
    }
}

/// The PUTs: one connection, the body read from disk at the pace.
pub(crate) struct HttpStorage {
    http: Option<reqwest::blocking::Client>,
    rate: RateSource,
    stop: Arc<AtomicBool>,
}

impl HttpStorage {
    pub(crate) fn new(rate: RateSource, stop: Arc<AtomicBool>) -> Self {
        let http = reqwest::blocking::Client::builder()
            .connect_timeout(CONNECT_TIMEOUT)
            .timeout(PUT_TIMEOUT)
            .pool_max_idle_per_host(1)
            .build()
            .map_err(|e| log::error!("[clip-upload] no HTTP client for the PUTs: {e}"))
            .ok();
        Self { http, rate, stop }
    }
}

impl Storage for HttpStorage {
    fn put(&self, put: &Put<'_>) -> Result<String, Failure> {
        let http = self
            .http
            .as_ref()
            .ok_or_else(|| Failure::Retry("no HTTP client".into()))?;
        let mut file = File::open(put.path)
            .map_err(|e| Failure::Refused(format!("{}: {e}", put.path.display())))?;
        file.seek(SeekFrom::Start(put.offset))
            .map_err(|e| Failure::Refused(format!("{}: {e}", put.path.display())))?;
        let reader = PacedReader::new(
            file.take(put.len),
            self.rate.clone(),
            put.floor_bps,
            self.stop.clone(),
        );
        // A sized body sends the signed Content-Length.
        let mut req = http
            .put(put.url)
            .body(reqwest::blocking::Body::sized(reader, put.len));
        if let Some(content_type) = put.content_type {
            req = req.header(reqwest::header::CONTENT_TYPE, content_type);
        }
        if let Some(cache_control) = put.cache_control {
            req = req.header(reqwest::header::CACHE_CONTROL, cache_control);
        }
        let resp = match req.send() {
            Ok(resp) => resp,
            Err(e) => {
                if self.stop.load(Ordering::Relaxed) {
                    return Err(Failure::Stopped);
                }
                if (self.rate)() == Some(0) {
                    return Err(Failure::Paused);
                }
                return Err(Failure::Retry(format!("PUT: {e}")));
            }
        };
        let status = resp.status().as_u16();
        if (200..300).contains(&status) {
            let etag = resp
                .headers()
                .get(reqwest::header::ETAG)
                .and_then(|v| v.to_str().ok())
                .unwrap_or_default()
                .to_string();
            return Ok(etag);
        }
        let text = resp.text().unwrap_or_default();
        Err(put_failure(status, &text))
    }
}

/// S3's status for a PUT: 403 is an expired URL, 404 an upload that is gone.
fn put_failure(status: u16, text: &str) -> Failure {
    match status {
        403 => Failure::Expired,
        404 => Failure::NoSuchUpload,
        408 | 429 => Failure::Retry(format!("PUT: HTTP {status}")),
        s if s >= 500 => Failure::Retry(format!("PUT: HTTP {s}")),
        s => Failure::Refused(format!("PUT: HTTP {s}: {}", first_line(text))),
    }
}

fn first_line(text: &str) -> &str {
    text.lines().next().unwrap_or_default()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn rpc_errors_split_into_retry_and_refused() {
        let limit = r#"{"code":8,"message":"clip rate limit exceeded"}"#;
        assert_eq!(
            rpc_failure("post_clip", 429, limit),
            Failure::Refused("post_clip: HTTP 429: clip rate limit exceeded".into())
        );
        assert!(matches!(
            rpc_failure("post_clip", 401, "{}"),
            Failure::Retry(_)
        ));
        assert!(matches!(
            rpc_failure("post_clip", 500, "oops"),
            Failure::Retry(_)
        ));
        assert!(matches!(
            rpc_failure("clip_upload_url", 400, "{}"),
            Failure::Refused(_)
        ));
    }

    #[test]
    fn put_errors_map_to_the_upload_steps() {
        assert_eq!(put_failure(403, ""), Failure::Expired);
        assert_eq!(put_failure(404, ""), Failure::NoSuchUpload);
        assert!(matches!(put_failure(503, ""), Failure::Retry(_)));
        assert!(matches!(
            put_failure(400, "<Error>BadDigest</Error>\nmore"),
            Failure::Refused(why) if why == "PUT: HTTP 400: <Error>BadDigest</Error>"
        ));
    }
}
