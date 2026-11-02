use image::AnimationDecoder;

const MAX_GIF_FRAMES: usize = 32;
const MAX_GIF_DIMENSION: u32 = 320;
/// A still image shows at up to 300 logical px; 640 keeps it sharp on a 2x screen.
const MAX_IMAGE_DIMENSION: u32 = 640;

/// Raw decoded GIF frame data (Send-safe, created on async runtime).
pub struct GifFrameData {
    pub frames: Vec<(Vec<u8>, u32, u32)>, // (rgba_bytes, width, height)
    pub delays: Vec<u32>,                 // ms per frame
}

/// Fetch an image and decode it to frames: every frame of a GIF, capped at
/// `MAX_GIF_FRAMES` and `MAX_GIF_DIMENSION`; one frame of anything else,
/// capped at `MAX_IMAGE_DIMENSION`. `None` on any network or decode failure.
pub async fn fetch_gif_frames(url: &str) -> Option<GifFrameData> {
    let http = crate::http::shared();
    let bytes = http.get(url).send().await.ok()?.bytes().await.ok()?;
    decode_media(&bytes)
}

/// The decode half of [`fetch_gif_frames`], for bytes already in hand.
pub fn decode_media(bytes: &[u8]) -> Option<GifFrameData> {
    if image::guess_format(bytes).ok()? != image::ImageFormat::Gif {
        let img = image::load_from_memory(bytes).ok()?;
        let img = if img.width().max(img.height()) > MAX_IMAGE_DIMENSION {
            img.resize(
                MAX_IMAGE_DIMENSION,
                MAX_IMAGE_DIMENSION,
                image::imageops::FilterType::Triangle,
            )
        } else {
            img
        };
        let rgba = img.to_rgba8();
        let (w, h) = (rgba.width(), rgba.height());
        return Some(GifFrameData {
            frames: vec![(rgba.into_raw(), w, h)],
            delays: vec![0],
        });
    }

    let cursor = std::io::Cursor::new(bytes);
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
        let rgba = buf.into_raw();
        let (rgba, w, h) = if w > MAX_GIF_DIMENSION || h > MAX_GIF_DIMENSION {
            let Some(img) = image::RgbaImage::from_raw(w, h, rgba) else {
                continue;
            };
            let (nw, nh) = if w >= h {
                (
                    MAX_GIF_DIMENSION,
                    (h as u64 * MAX_GIF_DIMENSION as u64 / w as u64).max(1) as u32,
                )
            } else {
                (
                    (w as u64 * MAX_GIF_DIMENSION as u64 / h as u64).max(1) as u32,
                    MAX_GIF_DIMENSION,
                )
            };
            let resized =
                image::imageops::resize(&img, nw, nh, image::imageops::FilterType::Triangle);
            (resized.into_raw(), nw, nh)
        } else {
            (rgba, w, h)
        };
        frames.push((rgba, w, h));
    }

    Some(GifFrameData { frames, delays })
}
