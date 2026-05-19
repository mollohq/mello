use std::collections::hash_map::DefaultHasher;
use std::collections::HashMap;
use std::fs;
use std::hash::{Hash, Hasher};
use std::path::{Path, PathBuf};
use std::sync::Mutex;
use std::time::{Duration, SystemTime};

use image::imageops::FilterType;
use image::GenericImageView;

const CACHE_DIR: &str = "mello_snapshots";
const MAX_CACHE_BYTES: usize = 50 * 1024 * 1024;
pub const THUMB_MAX_WIDTH: u32 = 480;

static CACHE: std::sync::OnceLock<Mutex<SnapshotCache>> = std::sync::OnceLock::new();

fn cache_global() -> &'static Mutex<SnapshotCache> {
    CACHE.get_or_init(|| Mutex::new(SnapshotCache::new()))
}

pub fn cache_dir() -> PathBuf {
    std::env::temp_dir().join(CACHE_DIR)
}

pub fn url_hash(url: &str) -> String {
    let mut s = DefaultHasher::new();
    url.hash(&mut s);
    format!("{:016x}", s.finish())
}

pub fn raw_path_for_url(url: &str) -> PathBuf {
    cache_dir().join(format!("{}.jpg", url_hash(url)))
}

pub fn thumb_path_for_url(url: &str) -> PathBuf {
    cache_dir().join(format!("{}_thumb.jpg", url_hash(url)))
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
                    let thumb = thumb_path_for_key(&key);
                    if thumb.exists() {
                        let _ = fs::remove_file(&thumb);
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

    fn touch(&mut self, url: &str, path: &Path, disk_size: usize) {
        let key = url_hash(url);
        self.entries.insert(
            key,
            CacheEntry {
                disk_size,
                last_accessed: SystemTime::now(),
                path: path.to_path_buf(),
            },
        );
    }

    fn register_file(&mut self, url: &str, path: &Path) {
        let key = url_hash(url);
        if let Ok(meta) = fs::metadata(path) {
            let disk_size = meta.len() as usize;
            if let Some(old) = self.entries.remove(&key) {
                self.disk_usage = self.disk_usage.saturating_sub(old.disk_size);
            }
            self.ensure_cache_dir_size();
            self.touch(url, path, disk_size);
            self.disk_usage += disk_size;
        }
    }
}

fn thumb_path_for_key(key: &str) -> PathBuf {
    cache_dir().join(format!("{key}_thumb.jpg"))
}

/// Returns path to raw JPEG on disk, fetching from CDN when missing.
pub async fn ensure_raw_on_disk(client: &reqwest::Client, url: &str) -> Option<PathBuf> {
    let path = raw_path_for_url(url);
    if path.exists() {
        if let Ok(mut cache) = cache_global().lock() {
            cache.register_file(url, &path);
        }
        return Some(path);
    }

    let bytes = client
        .get(url)
        .timeout(Duration::from_secs(5))
        .send()
        .await
        .ok()?
        .bytes()
        .await
        .ok()?;

    let dir = cache_dir();
    let _ = fs::create_dir_all(&dir);

    if let Ok(mut cache) = cache_global().lock() {
        cache.ensure_cache_dir_size();
    }

    if fs::write(&path, &bytes).is_err() {
        log::warn!("[snapshot] failed to write cache file for {}", url);
        return None;
    }

    log::debug!(
        "[snapshot] fetched {} bytes for {}",
        bytes.len(),
        &url[url.len().saturating_sub(40)..]
    );

    if let Ok(mut cache) = cache_global().lock() {
        cache.register_file(url, &path);
    }

    Some(path)
}

/// Disk-only prefetch (no decode).
pub async fn prefetch_raw(client: &reqwest::Client, url: &str) -> bool {
    ensure_raw_on_disk(client, url).await.is_some()
}

fn write_thumb_jpeg(raw_path: &Path, thumb_path: &Path) -> bool {
    let dyn_img = match image::ImageReader::open(raw_path) {
        Ok(r) => match r.decode() {
            Ok(img) => img,
            Err(e) => {
                log::warn!("[snapshot] decode failed for thumb: {}", e);
                return false;
            }
        },
        Err(e) => {
            log::warn!("[snapshot] open failed for thumb: {}", e);
            return false;
        }
    };

    let (w, h) = dyn_img.dimensions();
    let thumb = if w <= THUMB_MAX_WIDTH {
        dyn_img
    } else {
        let nh = (h as f64 * THUMB_MAX_WIDTH as f64 / w as f64).round() as u32;
        dyn_img.resize(THUMB_MAX_WIDTH, nh.max(1), FilterType::Triangle)
    };

    let rgb = thumb.to_rgb8();
    let file = match fs::File::create(thumb_path) {
        Ok(f) => f,
        Err(e) => {
            log::warn!("[snapshot] failed to create thumb file: {}", e);
            return false;
        }
    };
    let mut enc = image::codecs::jpeg::JpegEncoder::new_with_quality(file, 85);
    enc.encode_image(&rgb).is_ok()
}

/// Decode a thumbnail RGBA buffer from disk (raw or cached thumb JPEG).
pub fn decode_thumb_rgba(raw_path: &Path, url: &str) -> Option<(Vec<u8>, u32, u32)> {
    let thumb_path = thumb_path_for_url(url);
    if !thumb_path.exists() && !write_thumb_jpeg(raw_path, &thumb_path) {
        return decode_rgba_bytes(raw_path);
    }
    decode_rgba_bytes(&thumb_path)
}

fn decode_rgba_bytes(path: &Path) -> Option<(Vec<u8>, u32, u32)> {
    let dyn_img = image::ImageReader::open(path).ok()?.decode().ok()?;
    let rgba = dyn_img.to_rgba8();
    let (w, h) = rgba.dimensions();
    Some((rgba.into_raw(), w, h))
}

pub fn rgba_bytes_to_image(rgba: Vec<u8>, w: u32, h: u32) -> slint::Image {
    rgba_to_image(&rgba, w, h)
}

pub fn rgba_to_image(rgba: &[u8], w: u32, h: u32) -> slint::Image {
    let buffer = slint::SharedPixelBuffer::<slint::Rgba8Pixel>::clone_from_slice(rgba, w, h);
    slint::Image::from_rgba8(buffer)
}

#[cfg(test)]
mod tests {
    use super::*;
    use image::{codecs::jpeg::JpegEncoder, Rgb, RgbImage};
    use std::io::Write;

    #[test]
    fn decode_rgba_from_path_handles_rgb_jpeg() {
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

        let decoded = decode_rgba_bytes(&path);
        let _ = fs::remove_file(&path);

        let (rgba, w, h) = decoded.expect("jpeg decode should succeed");
        assert_eq!(rgba.len(), (w * h * 4) as usize);
    }
}
