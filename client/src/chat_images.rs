//! Chat image attachments: the composer's pending chips, and the resize and
//! re-encode every picked image goes through before it leaves the machine.
//!
//! The limits are the plan's: at most four images per message, 1600px on the
//! long side, 8 MB after encode. Re-encoding through the `image` crate also
//! drops EXIF, so no location or camera data leaves the machine; the EXIF
//! orientation is applied first so a phone photo is not sideways.

use std::io::Cursor;
use std::sync::{Arc, Mutex};

use image::ImageDecoder as _;

/// Images per message. The server enforces the same number.
pub const MAX_ATTACHMENTS: usize = mello_core::chat::MAX_IMAGES_PER_MESSAGE;
/// Long side after resize.
pub const MAX_SIDE: u32 = 1600;
/// Encoded size cap. The server refuses more.
pub const MAX_BYTES: usize = 8 << 20;
/// A picked file bigger than this is not even decoded.
pub const MAX_SOURCE_BYTES: usize = 64 << 20;
/// JPEG quality for photos.
const JPEG_QUALITY: u8 = 85;
/// Thumbnail side for the chip in the composer.
pub const THUMB_SIDE: u32 = 128;

/// An image ready to upload: the bytes as they will be stored.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct PreparedImage {
    pub bytes: Vec<u8>,
    pub mime: &'static str,
    pub width: u32,
    pub height: u32,
    /// A small RGBA thumbnail for the chip.
    pub thumb: (Vec<u8>, u32, u32),
}

/// Why an image could not be prepared. One sentence, shown on the chip.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct PrepareError(pub String);

impl std::fmt::Display for PrepareError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(&self.0)
    }
}

/// Decode, orient, resize to `MAX_SIDE` and re-encode. A GIF keeps its bytes
/// (its frames would be lost in a re-encode) when it fits the cap. A PNG with
/// transparency stays PNG; everything else becomes JPEG.
pub fn prepare_image(source: &[u8]) -> Result<PreparedImage, PrepareError> {
    if source.len() > MAX_SOURCE_BYTES {
        return Err(PrepareError("That file is over 64 MB.".into()));
    }
    let format = image::guess_format(source)
        .map_err(|_| PrepareError("That file is not an image.".into()))?;

    if format == image::ImageFormat::Gif {
        if source.len() > MAX_BYTES {
            return Err(PrepareError("That GIF is over 8 MB.".into()));
        }
        let first = image::load_from_memory_with_format(source, format)
            .map_err(|_| PrepareError("That GIF cannot be read.".into()))?;
        let (width, height) = (first.width(), first.height());
        return Ok(PreparedImage {
            bytes: source.to_vec(),
            mime: "image/gif",
            width,
            height,
            thumb: thumbnail(&first),
        });
    }

    let mut decoder = image::ImageReader::new(Cursor::new(source))
        .with_guessed_format()
        .map_err(|_| PrepareError("That file is not an image.".into()))?
        .into_decoder()
        .map_err(|_| PrepareError("That image cannot be read.".into()))?;
    let orientation = decoder
        .orientation()
        .unwrap_or(image::metadata::Orientation::NoTransforms);
    let mut img = image::DynamicImage::from_decoder(decoder)
        .map_err(|_| PrepareError("That image cannot be read.".into()))?;
    img.apply_orientation(orientation);

    if img.width().max(img.height()) > MAX_SIDE {
        img = img.resize(MAX_SIDE, MAX_SIDE, image::imageops::FilterType::Lanczos3);
    }
    let (width, height) = (img.width(), img.height());
    let thumb = thumbnail(&img);

    let keep_png = format == image::ImageFormat::Png && has_transparency(&img);
    let (bytes, mime) = if keep_png {
        let mut out = Cursor::new(Vec::new());
        img.write_to(&mut out, image::ImageFormat::Png)
            .map_err(|_| PrepareError("That image cannot be encoded.".into()))?;
        (out.into_inner(), "image/png")
    } else {
        let rgb = img.to_rgb8();
        let mut out = Vec::new();
        let mut enc = image::codecs::jpeg::JpegEncoder::new_with_quality(&mut out, JPEG_QUALITY);
        enc.encode_image(&rgb)
            .map_err(|_| PrepareError("That image cannot be encoded.".into()))?;
        (out, "image/jpeg")
    };
    if bytes.len() > MAX_BYTES {
        return Err(PrepareError("That image is over 8 MB after resize.".into()));
    }
    Ok(PreparedImage {
        bytes,
        mime,
        width,
        height,
        thumb,
    })
}

fn has_transparency(img: &image::DynamicImage) -> bool {
    match img {
        image::DynamicImage::ImageRgba8(buf) => buf.pixels().any(|p| p.0[3] != 255),
        image::DynamicImage::ImageLumaA8(buf) => buf.pixels().any(|p| p.0[1] != 255),
        image::DynamicImage::ImageRgba16(buf) => buf.pixels().any(|p| p.0[3] != u16::MAX),
        image::DynamicImage::ImageLumaA16(buf) => buf.pixels().any(|p| p.0[1] != u16::MAX),
        image::DynamicImage::ImageRgba32F(buf) => buf.pixels().any(|p| p.0[3] < 1.0),
        _ => false,
    }
}

fn thumbnail(img: &image::DynamicImage) -> (Vec<u8>, u32, u32) {
    let t = img
        .resize_to_fill(
            THUMB_SIDE,
            THUMB_SIDE,
            image::imageops::FilterType::Triangle,
        )
        .to_rgba8();
    let (w, h) = (t.width(), t.height());
    (t.into_raw(), w, h)
}

/// A readable size: "860 KB", "1.2 MB".
pub fn size_label(bytes: u64) -> String {
    if bytes >= 1 << 20 {
        format!("{:.1} MB", bytes as f64 / (1 << 20) as f64)
    } else {
        format!("{} KB", bytes.div_ceil(1024).max(1))
    }
}

/// Where a pending chip is in its life.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum AttachmentState {
    Preparing,
    Uploading,
    Ready(mello_core::chat::ImageData),
    Failed(String),
}

/// One chip in the composer. The thumbnail is raw RGBA so the state can
/// cross threads; the UI makes a `slint::Image` of it when it draws.
#[derive(Debug, Clone)]
pub struct Attachment {
    pub id: String,
    pub state: AttachmentState,
    pub thumb: Option<(Vec<u8>, u32, u32)>,
    pub bytes: u64,
}

impl Attachment {
    /// The word the chip shows under the thumbnail.
    pub fn label(&self) -> String {
        match &self.state {
            AttachmentState::Preparing => "preparing".into(),
            AttachmentState::Uploading => "uploading".into(),
            AttachmentState::Ready(_) => size_label(self.bytes),
            AttachmentState::Failed(reason) => reason.clone(),
        }
    }

    pub fn state_name(&self) -> &'static str {
        match self.state {
            AttachmentState::Preparing => "preparing",
            AttachmentState::Uploading => "uploading",
            AttachmentState::Ready(_) => "ready",
            AttachmentState::Failed(_) => "failed",
        }
    }
}

/// A send the user asked for while an upload was still running. It goes
/// out when the last chip settles.
#[derive(Debug, Clone)]
pub struct PendingSend {
    pub text: String,
    pub reply_to: Option<String>,
    pub mentions: Vec<mello_core::chat::MentionRef>,
}

/// The composer's chips and the send that waits on them.
#[derive(Debug, Default)]
pub struct Composer {
    pub attachments: Vec<Attachment>,
    pub pending_send: Option<PendingSend>,
}

/// Shared between callbacks, handlers and the prepare task.
pub type Attachments = Arc<Mutex<Composer>>;

/// Lock the composer; a poisoned lock still holds usable state.
pub fn lock(composer: &Attachments) -> std::sync::MutexGuard<'_, Composer> {
    composer.lock().unwrap_or_else(|e| e.into_inner())
}

/// The uploaded images of the pending chips, in order, when every chip that
/// is not failed is ready. `None` while any upload is still running.
pub fn ready_images(list: &[Attachment]) -> Option<Vec<mello_core::chat::ImageData>> {
    let mut out = Vec::new();
    for a in list {
        match &a.state {
            AttachmentState::Ready(img) => out.push(img.clone()),
            AttachmentState::Failed(_) => {}
            AttachmentState::Preparing | AttachmentState::Uploading => return None,
        }
    }
    Some(out)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn png(w: u32, h: u32, alpha: u8) -> Vec<u8> {
        let img = image::RgbaImage::from_fn(w, h, |x, y| {
            image::Rgba([(x % 256) as u8, (y % 256) as u8, 90, alpha])
        });
        let mut out = Cursor::new(Vec::new());
        image::DynamicImage::ImageRgba8(img)
            .write_to(&mut out, image::ImageFormat::Png)
            .expect("png");
        out.into_inner()
    }

    #[test]
    fn a_big_photo_shrinks_to_1600_and_becomes_jpeg() {
        let p = prepare_image(&png(3000, 2000, 255)).expect("prepared");
        assert_eq!((p.width, p.height), (1600, 1067));
        assert_eq!(p.mime, "image/jpeg");
        assert!(p.bytes.len() < MAX_BYTES);
        assert_eq!(
            image::guess_format(&p.bytes).expect("fmt"),
            image::ImageFormat::Jpeg
        );
        assert_eq!((p.thumb.1, p.thumb.2), (THUMB_SIDE, THUMB_SIDE));
    }

    #[test]
    fn a_small_png_with_transparency_stays_png() {
        let p = prepare_image(&png(300, 200, 128)).expect("prepared");
        assert_eq!((p.width, p.height), (300, 200));
        assert_eq!(p.mime, "image/png");
        assert_eq!(
            image::guess_format(&p.bytes).expect("fmt"),
            image::ImageFormat::Png
        );
    }

    #[test]
    fn a_gif_keeps_its_bytes() {
        let img = image::RgbaImage::from_pixel(40, 30, image::Rgba([1, 2, 3, 255]));
        let mut out = Cursor::new(Vec::new());
        image::DynamicImage::ImageRgba8(img)
            .write_to(&mut out, image::ImageFormat::Gif)
            .expect("gif");
        let gif = out.into_inner();
        let p = prepare_image(&gif).expect("prepared");
        assert_eq!(p.bytes, gif);
        assert_eq!(p.mime, "image/gif");
        assert_eq!((p.width, p.height), (40, 30));
    }

    #[test]
    fn not_an_image_is_one_sentence() {
        let err = prepare_image(b"hello, not an image").expect_err("refused");
        assert_eq!(err.0, "That file is not an image.");
    }

    #[test]
    fn size_labels_read_like_a_file_browser() {
        assert_eq!(size_label(880_640), "860 KB");
        assert_eq!(size_label(1_258_291), "1.2 MB");
        assert_eq!(size_label(10), "1 KB");
    }

    #[test]
    fn ready_images_waits_for_every_upload_and_skips_failed_ones() {
        let img = mello_core::chat::ImageData {
            url: "u".into(),
            width: 1,
            height: 1,
            bytes: 1,
            mime: "image/png".into(),
        };
        let a = |state| Attachment {
            id: "x".into(),
            state,
            thumb: None,
            bytes: 1,
        };
        assert_eq!(
            ready_images(&[
                a(AttachmentState::Ready(img.clone())),
                a(AttachmentState::Failed("no".into()))
            ]),
            Some(vec![img.clone()])
        );
        assert_eq!(
            ready_images(&[
                a(AttachmentState::Ready(img)),
                a(AttachmentState::Uploading)
            ]),
            None
        );
        assert_eq!(ready_images(&[]), Some(vec![]));
    }
}
