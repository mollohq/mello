use image::AnimationDecoder;

const MAX_GIF_FRAMES: usize = 120;

/// Raw decoded GIF frame data (Send-safe, created on async runtime).
pub struct GifFrameData {
    pub frames: Vec<(Vec<u8>, u32, u32)>, // (rgba_bytes, width, height)
    pub delays: Vec<u32>,                 // ms per frame
}

/// Fetch a GIF and decode ALL frames. Pushes result to `inbox` (drained by GifAnimator timer).
pub fn spawn_gif_fetch(
    url: String,
    rt: &tokio::runtime::Handle,
    inbox: &std::sync::Arc<std::sync::Mutex<Vec<(String, GifFrameData)>>>,
) {
    let inbox = inbox.clone();
    rt.spawn(async move {
        let Some(data) = fetch_gif_frames(&url).await else {
            log::debug!("[gif] failed to fetch frames from {}", url);
            return;
        };
        inbox.lock().unwrap().push((url, data));
    });
}

async fn fetch_gif_frames(url: &str) -> Option<GifFrameData> {
    let http = reqwest::Client::new();
    let bytes = http.get(url).send().await.ok()?.bytes().await.ok()?;

    let cursor = std::io::Cursor::new(bytes.as_ref());
    let decoder = image::codecs::gif::GifDecoder::new(cursor).ok()?;
    let raw_frames = decoder.into_frames().collect_frames().ok()?;

    if raw_frames.is_empty() {
        return None;
    }

    let cap = raw_frames.len().min(MAX_GIF_FRAMES);
    let mut frames = Vec::with_capacity(cap);
    let mut delays = Vec::with_capacity(cap);

    for frame in raw_frames.into_iter().take(MAX_GIF_FRAMES) {
        let (num, den) = frame.delay().numer_denom_ms();
        let delay_ms = if den == 0 { 100 } else { num / den };
        delays.push(delay_ms.max(20)); // floor at 20ms to avoid busy-spinning

        let buf = frame.into_buffer();
        let (w, h) = (buf.width(), buf.height());
        frames.push((buf.into_raw(), w, h));
    }

    Some(GifFrameData { frames, delays })
}
