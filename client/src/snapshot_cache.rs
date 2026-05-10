use std::collections::HashMap;
use std::fs;
use std::path::PathBuf;
use std::sync::Mutex;
use std::time::{Duration, SystemTime};

const CACHE_DIR: &str = "mello_snapshots";
const MAX_CACHE_BYTES: usize = 50 * 1024 * 1024;

static CACHE: std::sync::OnceLock<Mutex<SnapshotCache>> = std::sync::OnceLock::new();

fn cache_global() -> &'static Mutex<SnapshotCache> {
    CACHE.get_or_init(|| Mutex::new(SnapshotCache::new()))
}

fn cache_dir() -> PathBuf {
    std::env::temp_dir().join(CACHE_DIR)
}

pub struct SnapshotCache {
    disk_usage: usize,
    entries: HashMap<String, CacheEntry>,
}

struct CacheEntry {
    disk_size: usize,
    last_accessed: SystemTime,
    path: PathBuf,
}

impl SnapshotCache {
    fn new() -> Self {
        let dir = cache_dir();
        let _ = fs::create_dir_all(&dir);
        Self {
            disk_usage: 0,
            entries: HashMap::new(),
        }
    }

    fn url_hash(url: &str) -> String {
        use std::collections::hash_map::DefaultHasher;
        use std::hash::{Hash, Hasher};
        let mut s = DefaultHasher::new();
        url.hash(&mut s);
        format!("{:016x}", s.finish())
    }

    fn evict_until_below(&mut self, target_bytes: usize) {
        let mut by_age: Vec<_> = self
            .entries
            .iter()
            .map(|(k, e)| (k.clone(), e.last_accessed))
            .collect();
        by_age.sort_by(|a, b| a.1.cmp(&b.1));

        while self.disk_usage > target_bytes {
            if let Some((key, _)) = by_age.pop() {
                if let Some(entry) = self.entries.remove(&key) {
                    if entry.path.exists() {
                        let _ = fs::remove_file(&entry.path);
                    }
                    self.disk_usage = self.disk_usage.saturating_sub(entry.disk_size);
                }
            } else {
                break;
            }
        }
    }

    fn ensure_cache_dir_size(&mut self) {
        if self.disk_usage > MAX_CACHE_BYTES {
            self.evict_until_below(MAX_CACHE_BYTES / 2);
        }
    }

    fn get_or_fetch(&mut self, url: &str) -> Option<PathBuf> {
        let key = Self::url_hash(url);

        if let Some(entry) = self.entries.get_mut(&key) {
            entry.last_accessed = SystemTime::now();
            if entry.path.exists() {
                return Some(entry.path.clone());
            }
            let disk_size = entry.disk_size;
            self.disk_usage = self.disk_usage.saturating_sub(disk_size);
            self.entries.remove(&key);
            return self.get_or_fetch(url);
        }

        self.ensure_cache_dir_size();

        let path = cache_dir().join(format!("{}.jpg", key));
        let bytes = fetch_jpeg_sync(url)?;
        log::debug!(
            "[snapshot] fetched {} bytes for {}",
            bytes.len(),
            &url[url.len().saturating_sub(40)..]
        );

        let disk_size = bytes.len();
        if let Err(e) = fs::write(&path, &bytes) {
            log::warn!("[snapshot] failed to write cache file: {}", e);
            return None;
        }

        self.entries.insert(
            key,
            CacheEntry {
                disk_size,
                last_accessed: SystemTime::now(),
                path: path.clone(),
            },
        );
        self.disk_usage += disk_size;
        Some(path)
    }
}

fn fetch_jpeg_sync(url: &str) -> Option<Vec<u8>> {
    let rt = tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .build()
        .ok()?;
    rt.block_on(async {
        let client = reqwest::Client::new();
        let bytes = client
            .get(url)
            .timeout(Duration::from_secs(5))
            .send()
            .await
            .ok()?
            .bytes()
            .await
            .ok()?;
        Some(bytes.to_vec())
    })
}

fn decode_jpeg_to_rgba(path: &PathBuf) -> Option<(Vec<u8>, u32, u32)> {
    let dyn_img = image::ImageReader::open(path).ok()?.decode().ok()?;
    let rgba = dyn_img.to_rgba8();
    let (w, h) = rgba.dimensions();
    Some((rgba.into_raw(), w, h))
}

pub fn rgba_to_image(rgba: &[u8], w: u32, h: u32) -> slint::Image {
    let buffer = slint::SharedPixelBuffer::<slint::Rgba8Pixel>::clone_from_slice(rgba, w, h);
    slint::Image::from_rgba8(buffer)
}

/// Decode a single snapshot from disk cache to slint::Image.
/// Falls back to fetching if not yet cached.
pub fn decode_snapshot(url: &str) -> Option<slint::Image> {
    let path = {
        let mut cache = match cache_global().lock() {
            Ok(cache) => cache,
            Err(poisoned) => {
                log::warn!("[snapshot] cache lock poisoned, recovering");
                poisoned.into_inner()
            }
        };
        cache.get_or_fetch(url)?
    };

    let (rgba, w, h) = decode_jpeg_to_rgba(&path)?;
    log::trace!("[snapshot] decoded {}x{} from disk", w, h);
    Some(rgba_to_image(&rgba, w, h))
}

/// Pre-fetch all URLs to disk cache without decoding.
pub fn prefetch_all(urls: &[String]) {
    log::info!("[snapshot] prefetch_all: {} URLs", urls.len());
    let mut cache = match cache_global().lock() {
        Ok(cache) => cache,
        Err(poisoned) => {
            log::warn!("[snapshot] cache lock poisoned, recovering");
            poisoned.into_inner()
        }
    };
    let mut fetched = 0usize;
    let mut cached = 0usize;
    for url in urls {
        let key = SnapshotCache::url_hash(url);
        let was_cached = cache.entries.contains_key(&key);
        if cache.get_or_fetch(url).is_some() {
            if was_cached {
                cached += 1;
            } else {
                fetched += 1;
            }
        }
    }
    log::info!(
        "[snapshot] prefetch done: {} fetched, {} already cached, {} total",
        fetched,
        cached,
        urls.len()
    );
}

#[cfg(test)]
mod tests {
    use super::*;
    use image::{codecs::jpeg::JpegEncoder, Rgb, RgbImage};
    use std::io::Write;

    #[test]
    fn decode_jpeg_to_rgba_handles_rgb_jpeg() {
        let mut img = RgbImage::new(4, 2);
        for y in 0..2 {
            for x in 0..4 {
                img.put_pixel(x, y, Rgb([200, 100, 50]));
            }
        }

        let mut jpeg_bytes = Vec::new();
        {
            let mut enc = JpegEncoder::new_with_quality(&mut jpeg_bytes, 85);
            enc.encode_image(&img).expect("jpeg encode should succeed");
        }

        let mut path = std::env::temp_dir();
        path.push(format!(
            "mello-snapshot-cache-test-{}.jpg",
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .expect("clock should be valid")
                .as_nanos()
        ));

        let mut file = fs::File::create(&path).expect("temp file should be created");
        file.write_all(&jpeg_bytes)
            .expect("jpeg bytes should be written");
        drop(file);

        let decoded = decode_jpeg_to_rgba(&path);
        let _ = fs::remove_file(&path);

        let (rgba, w, h) = decoded.expect("jpeg decode should succeed");
        assert_eq!(w, 4);
        assert_eq!(h, 2);
        assert_eq!(rgba.len(), (w * h * 4) as usize);
    }

    #[test]
    fn fetch_jpeg_sync_does_not_panic_without_tokio_context() {
        let result =
            std::panic::catch_unwind(|| fetch_jpeg_sync("http://127.0.0.1:9/not-found.jpg"));
        assert!(result.is_ok(), "fetch_jpeg_sync should not panic");
    }
}
