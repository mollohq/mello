use rand::seq::SliceRandom;
use std::time::Duration;

const AVATAR_WORKER_URL: &str = "https://avatar.m3llo.app";
const APPROVED_STYLES: &[&str] = &[
    "adventurer-neutral",
    "avataaars-neutral",
    "fun-emoji",
    "pixel-art",
    "thumbs",
];
pub const RENDER_SIZE: u32 = 280; // 140px @ 2x for HiDPI

pub const EASTER_WORDS: &[&str] = &[
    "hey", "hey", "hey", "there,", "there,", "there,", "we", "we", "we", "love", "love", "love",
    "you", "you", "you", "for", "for", "for", "being", "being", "being", "here", "here", "here",
    "\u{2764}", "\u{2764}", "\u{2764}", "\u{2764}",
];

pub fn is_heart(word: &str) -> bool {
    word == "\u{2764}"
}

pub struct AvatarSlot {
    pub svg_data: Option<String>,
    pub style: String,
    pub seed: String,
}

pub struct AvatarGridState {
    pub session_seed: String,
    pub roll_counter: u32,
    pub shuffle_counter: u32,
    pub slots: [AvatarSlot; 7],
    pub selected_slot: Option<usize>,
    pub upload_data: Option<Vec<u8>>,
    pub flipping: [bool; 7],
}

impl AvatarGridState {
    pub fn new() -> Self {
        let session_seed = uuid::Uuid::new_v4().to_string();
        Self {
            session_seed,
            roll_counter: 0,
            shuffle_counter: 0,
            slots: std::array::from_fn(|_| AvatarSlot {
                svg_data: None,
                style: String::new(),
                seed: String::new(),
            }),
            selected_slot: None,
            upload_data: None,
            flipping: [false; 7],
        }
    }

    pub fn make_seed(&self, slot_index: usize) -> String {
        format!(
            "{}_r{}_{}",
            self.session_seed, self.roll_counter, slot_index
        )
    }

    pub fn make_shuffle_seed(&mut self, slot_index: usize) -> String {
        self.shuffle_counter += 1;
        format!(
            "{}_s{}_{}",
            self.session_seed, self.shuffle_counter, slot_index
        )
    }

    pub fn pick_random_style() -> &'static str {
        let mut rng = rand::thread_rng();
        APPROVED_STYLES.choose(&mut rng).unwrap()
    }

    pub fn pick_unselected_non_flipping_slot(&self) -> Option<usize> {
        let mut rng = rand::thread_rng();
        let candidates: Vec<usize> = (0..7)
            .filter(|&i| Some(i) != self.selected_slot && !self.flipping[i])
            .collect();
        candidates.choose(&mut rng).copied()
    }
}

/// Fetch an avatar SVG from the CF Worker and rasterize to raw RGBA bytes.
/// Returns (svg_string, rgba_bytes) on success — both are `Send`.
pub async fn fetch_and_rasterize(
    http: &reqwest::Client,
    style: &str,
    seed: &str,
) -> Option<(String, Vec<u8>)> {
    let url = format!("{}/{}/svg?seed={}", AVATAR_WORKER_URL, style, seed);
    log::debug!("[avatar] fetching {}", url);

    let resp = match http.get(&url).timeout(Duration::from_secs(3)).send().await {
        Ok(r) => r,
        Err(e) => {
            log::warn!("[avatar] fetch failed for {}/{}: {}", style, seed, e);
            return None;
        }
    };

    if !resp.status().is_success() {
        log::warn!(
            "[avatar] worker returned {} for {}/{}",
            resp.status(),
            style,
            seed
        );
        return None;
    }

    let svg_data = match resp.text().await {
        Ok(t) => t,
        Err(e) => {
            log::warn!("[avatar] body read failed for {}/{}: {}", style, seed, e);
            return None;
        }
    };
    log::debug!(
        "[avatar] got {} bytes SVG for {}/{}",
        svg_data.len(),
        style,
        seed
    );

    let rgba = match rasterize_svg(&svg_data) {
        Some(r) => r,
        None => {
            log::warn!(
                "[avatar] rasterize failed for {}/{} (svg len={})",
                style,
                seed,
                svg_data.len()
            );
            return None;
        }
    };
    log::debug!(
        "[avatar] rasterized {}/{} -> {} bytes RGBA",
        style,
        seed,
        rgba.len()
    );
    Some((svg_data, rgba))
}

/// Rasterize an SVG string into raw RGBA bytes at RENDER_SIZE.
pub fn rasterize_svg(svg_data: &str) -> Option<Vec<u8>> {
    let opt = resvg::usvg::Options::default();
    let tree = match resvg::usvg::Tree::from_str(svg_data, &opt) {
        Ok(t) => t,
        Err(e) => {
            log::warn!("[avatar] SVG parse error: {}", e);
            return None;
        }
    };

    let size = RENDER_SIZE;
    let pixmap = match resvg::tiny_skia::Pixmap::new(size, size) {
        Some(p) => p,
        None => {
            log::warn!("[avatar] pixmap alloc failed for {}x{}", size, size);
            return None;
        }
    };
    let mut pixmap = pixmap;

    let svg_size = tree.size();
    log::debug!(
        "[avatar] SVG size: {}x{}, target: {}x{}",
        svg_size.width(),
        svg_size.height(),
        size,
        size
    );
    let sx = size as f32 / svg_size.width();
    let sy = size as f32 / svg_size.height();
    let scale = sx.min(sy);
    let tx = (size as f32 - svg_size.width() * scale) / 2.0;
    let ty = (size as f32 - svg_size.height() * scale) / 2.0;
    let transform = resvg::tiny_skia::Transform::from_scale(scale, scale).post_translate(tx, ty);

    resvg::render(&tree, transform, &mut pixmap.as_mut());

    Some(pixmap.data().to_vec())
}

/// Construct a `slint::Image` from raw RGBA bytes. Must be called on the UI thread.
pub fn rgba_to_image(rgba: &[u8], w: u32, h: u32) -> slint::Image {
    let buffer = slint::SharedPixelBuffer::<slint::Rgba8Pixel>::clone_from_slice(rgba, w, h);
    slint::Image::from_rgba8(buffer)
}
