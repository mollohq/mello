use serde::Deserialize;

use crate::chat::GifData;
use crate::error::Result;

const GIPHY_BASE: &str = "https://api.giphy.com/v1/gifs";

/// Build-time Giphy API key. Set via env var `GIPHY_API_KEY` at compile time.
/// Falls back to a placeholder for dev builds.
const GIPHY_KEY: &str = match option_env!("GIPHY_API_KEY") {
    Some(k) => k,
    None => "GIPHY_DEV_KEY",
};

#[derive(Debug, Clone, Deserialize)]
pub struct GiphyGif {
    pub id: String,
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

/// Giphy returns dimensions as strings ("200"), not integers.
fn de_str_u32<'de, D: serde::Deserializer<'de>>(d: D) -> std::result::Result<u32, D::Error> {
    let s = String::deserialize(d)?;
    s.parse().map_err(serde::de::Error::custom)
}

#[derive(Debug, Deserialize)]
struct GiphySearchResponse {
    data: Vec<GiphyGif>,
}

impl GiphyGif {
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

    pub async fn search(&self, query: &str, limit: u32) -> Result<Vec<GiphyGif>> {
        let url = format!(
            "{}/search?q={}&api_key={}&limit={}&rating=g",
            GIPHY_BASE,
            urlencoding::encode(query),
            GIPHY_KEY,
            limit
        );
        let resp: GiphySearchResponse = self.http.get(&url).send().await?.json().await?;
        Ok(resp.data)
    }

    pub async fn trending(&self, limit: u32) -> Result<Vec<GiphyGif>> {
        let url = format!(
            "{}/trending?api_key={}&limit={}&rating=g",
            GIPHY_BASE, GIPHY_KEY, limit
        );
        let resp: GiphySearchResponse = self.http.get(&url).send().await?.json().await?;
        Ok(resp.data)
    }
}
