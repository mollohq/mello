//! Giphy search and trending, called from the client as Giphy's API rules
//! require (no proxy, no cache of responses or media URLs).

use serde::{Deserialize, Serialize};

use crate::chat::GifData;

const GIPHY_BASE: &str = "https://api.giphy.com/v1/gifs";

/// Build-time Giphy API key from `GIPHY_API_KEY`. A production build fails to
/// compile without it (`client/src/lib.rs`). A dev build without it gets
/// `GifError::NoKey` on every request instead of a 401 from Giphy.
const GIPHY_KEY: Option<&str> = option_env!("GIPHY_API_KEY");

/// Why a Giphy request produced no GIFs. Shown in the picker, so each variant
/// maps to one sentence the user can act on.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub enum GifError {
    /// This build has no `GIPHY_API_KEY`.
    NoKey,
    /// Giphy answered 401 or 403: the baked-in key is wrong or revoked.
    Unauthorized,
    /// Giphy answered 429: the key's hourly quota is used up.
    RateLimited,
    /// Any other non-2xx answer.
    Status(u16),
    /// The request did not complete.
    Network(String),
    /// The answer was not the JSON we expect.
    Parse(String),
}

impl std::fmt::Display for GifError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            GifError::NoKey => write!(f, "no GIPHY_API_KEY in this build"),
            GifError::Unauthorized => write!(f, "Giphy rejected the API key"),
            GifError::RateLimited => write!(f, "Giphy rate limit reached"),
            GifError::Status(code) => write!(f, "Giphy answered HTTP {code}"),
            GifError::Network(e) => write!(f, "Giphy request failed: {e}"),
            GifError::Parse(e) => write!(f, "Giphy answer unreadable: {e}"),
        }
    }
}

impl std::error::Error for GifError {}

#[derive(Debug, Clone, Deserialize)]
pub struct GiphyGif {
    pub id: String,
    #[serde(default)]
    pub title: String,
    pub images: GiphyImages,
}

#[derive(Debug, Clone, Deserialize)]
pub struct GiphyImages {
    pub fixed_width: Option<GiphyRendition>,
    pub fixed_width_small: Option<GiphyRendition>,
    pub preview_gif: Option<GiphyRendition>,
    pub original: Option<GiphyRendition>,
}

#[derive(Debug, Clone, Deserialize)]
pub struct GiphyRendition {
    pub url: String,
    #[serde(default, deserialize_with = "de_str_u32")]
    pub width: u32,
    #[serde(default, deserialize_with = "de_str_u32")]
    pub height: u32,
}

/// Giphy returns dimensions as strings ("200"), not integers. A value that
/// does not parse becomes 0, so one odd rendition cannot fail the whole page.
fn de_str_u32<'de, D: serde::Deserializer<'de>>(d: D) -> std::result::Result<u32, D::Error> {
    #[derive(Deserialize)]
    #[serde(untagged)]
    enum Dim {
        Str(String),
        Num(u32),
    }
    Ok(match Dim::deserialize(d)? {
        Dim::Str(s) => s.parse().unwrap_or(0),
        Dim::Num(n) => n,
    })
}

#[derive(Debug, Deserialize)]
struct GiphySearchResponse {
    data: Vec<GiphyGif>,
}

impl GiphyGif {
    /// `url` is the `fixed_width` rendition (200px wide), what the chat shows.
    /// `preview` is `fixed_width_small` (100px wide), what the picker tiles show.
    pub fn to_gif_data(&self) -> Option<GifData> {
        let display = self
            .images
            .fixed_width
            .as_ref()
            .or(self.images.original.as_ref())?;
        let preview = self
            .images
            .fixed_width_small
            .as_ref()
            .or(self.images.preview_gif.as_ref())
            .unwrap_or(display);
        Some(GifData {
            id: self.id.clone(),
            url: display.url.clone(),
            preview: preview.url.clone(),
            width: display.width,
            height: display.height,
            alt: self.title.clone(),
        })
    }
}

pub struct GiphyClient {
    http: reqwest::Client,
}

impl Default for GiphyClient {
    fn default() -> Self {
        Self::new()
    }
}

impl GiphyClient {
    pub fn new() -> Self {
        Self {
            http: reqwest::Client::new(),
        }
    }

    pub async fn search(&self, query: &str, limit: u32) -> Result<Vec<GiphyGif>, GifError> {
        let key = GIPHY_KEY.ok_or(GifError::NoKey)?;
        let url = format!(
            "{GIPHY_BASE}/search?q={}&api_key={key}&limit={limit}&rating=g",
            urlencoding::encode(query),
        );
        self.fetch(&url).await
    }

    pub async fn trending(&self, limit: u32) -> Result<Vec<GiphyGif>, GifError> {
        let key = GIPHY_KEY.ok_or(GifError::NoKey)?;
        let url = format!("{GIPHY_BASE}/trending?api_key={key}&limit={limit}&rating=g");
        self.fetch(&url).await
    }

    async fn fetch(&self, url: &str) -> Result<Vec<GiphyGif>, GifError> {
        let resp = self
            .http
            .get(url)
            .send()
            .await
            .map_err(|e| GifError::Network(e.to_string()))?;
        let status = resp.status();
        if !status.is_success() {
            return Err(status_error(status.as_u16()));
        }
        let page: GiphySearchResponse = resp
            .json()
            .await
            .map_err(|e| GifError::Parse(e.to_string()))?;
        Ok(page.data)
    }
}

fn status_error(code: u16) -> GifError {
    match code {
        401 | 403 => GifError::Unauthorized,
        429 => GifError::RateLimited,
        other => GifError::Status(other),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn status_codes_map_to_actionable_errors() {
        assert_eq!(status_error(401), GifError::Unauthorized);
        assert_eq!(status_error(403), GifError::Unauthorized);
        assert_eq!(status_error(429), GifError::RateLimited);
        assert_eq!(status_error(503), GifError::Status(503));
    }

    #[test]
    fn odd_dimension_does_not_fail_the_page() {
        let json = r#"{"data":[{"id":"a","title":"t","images":{
            "fixed_width":{"url":"u","width":"200","height":"x"},
            "fixed_width_small":{"url":"p","width":100,"height":75}}}]}"#;
        let page: GiphySearchResponse = serde_json::from_str(json).expect("parses");
        let gif = page.data[0].to_gif_data().expect("gif data");
        assert_eq!(gif.url, "u");
        assert_eq!(gif.preview, "p");
        assert_eq!((gif.width, gif.height), (200, 0));
    }
}
