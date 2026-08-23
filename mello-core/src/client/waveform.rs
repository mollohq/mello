//! Peak waveform extraction from captured clip WAV files.

use base64::Engine as _;
use std::path::Path;

const BUCKET_COUNT: usize = 64;

/// Compute a 64-bucket peak waveform from a 16-bit PCM WAV file and return it
/// base64-encoded (~88 chars). Returns `None` on parse/IO failure without
/// propagating errors — waveform extraction must never block capture.
pub fn compute_clip_waveform_b64(path: &Path) -> Option<String> {
    let bytes = match std::fs::read(path) {
        Ok(b) => b,
        Err(e) => {
            log::warn!("waveform: failed to read {}: {e}", path.display());
            return None;
        }
    };

    let pcm = parse_wav_pcm(&bytes)?;

    let peaks = compute_peaks(&pcm);
    Some(base64::engine::general_purpose::STANDARD.encode(peaks))
}

/// Decode a base64 clip waveform into normalized peak values in `0.0..=1.0`.
pub fn decode_clip_waveform(b64: &str) -> Vec<f32> {
    if b64.is_empty() {
        return Vec::new();
    }
    let bytes = match base64::engine::general_purpose::STANDARD.decode(b64) {
        Ok(b) => b,
        Err(_) => return Vec::new(),
    };
    bytes.iter().map(|&b| f32::from(b) / 255.0).collect()
}

struct WavPcm {
    samples: Vec<i16>,
    channels: u16,
}

fn parse_wav_pcm(data: &[u8]) -> Option<WavPcm> {
    if data.len() < 12 {
        log::warn!("waveform: WAV too short ({} bytes)", data.len());
        return None;
    }
    if &data[0..4] != b"RIFF" || &data[8..12] != b"WAVE" {
        log::warn!("waveform: not a RIFF/WAVE file");
        return None;
    }

    let mut pos = 12usize;
    let mut channels: Option<u16> = None;
    let mut bits_per_sample: Option<u16> = None;
    let mut audio_format: Option<u16> = None;
    let mut pcm_data: Option<&[u8]> = None;

    while pos + 8 <= data.len() {
        let chunk_id = &data[pos..pos + 4];
        let chunk_size = u32::from_le_bytes(data[pos + 4..pos + 8].try_into().ok()?);
        pos += 8;
        let chunk_end = pos.checked_add(chunk_size as usize)?;
        if chunk_end > data.len() {
            log::warn!("waveform: chunk extends past end of file");
            return None;
        }
        let chunk_body = &data[pos..chunk_end];

        match chunk_id {
            b"fmt " => {
                if chunk_body.len() < 16 {
                    log::warn!("waveform: fmt chunk too short");
                    return None;
                }
                audio_format = Some(u16::from_le_bytes([chunk_body[0], chunk_body[1]]));
                channels = Some(u16::from_le_bytes([chunk_body[2], chunk_body[3]]));
                bits_per_sample = Some(u16::from_le_bytes([chunk_body[14], chunk_body[15]]));
            }
            b"data" => pcm_data = Some(chunk_body),
            _ => {}
        }

        // RIFF chunks are padded to an even byte boundary.
        pos = chunk_end + (chunk_size as usize % 2);
    }

    let channels = channels?;
    let bits_per_sample = bits_per_sample?;
    let audio_format = audio_format.unwrap_or(0);
    let pcm_data = pcm_data?;

    if audio_format != 1 {
        log::warn!("waveform: unsupported audio format {audio_format} (expected PCM)");
        return None;
    }
    if bits_per_sample != 16 {
        log::warn!("waveform: unsupported bits per sample {bits_per_sample} (expected 16)");
        return None;
    }
    if channels == 0 {
        log::warn!("waveform: zero channels");
        return None;
    }

    let sample_count = pcm_data.len() / 2;
    if sample_count == 0 {
        log::warn!("waveform: empty PCM data");
        return None;
    }
    if pcm_data.len() % 2 != 0 {
        log::warn!("waveform: odd PCM byte count");
        return None;
    }

    let samples: Vec<i16> = pcm_data
        .chunks_exact(2)
        .map(|c| i16::from_le_bytes([c[0], c[1]]))
        .collect();

    Some(WavPcm { samples, channels })
}

fn compute_peaks(pcm: &WavPcm) -> [u8; BUCKET_COUNT] {
    let channels = pcm.channels as usize;
    let frame_count = pcm.samples.len() / channels;
    if frame_count == 0 {
        return [0u8; BUCKET_COUNT];
    }

    let mut raw_peaks = [0u16; BUCKET_COUNT];
    for (bucket, peak) in raw_peaks.iter_mut().enumerate() {
        let start = bucket * frame_count / BUCKET_COUNT;
        let end = ((bucket + 1) * frame_count / BUCKET_COUNT).max(start);
        let mut max_abs = 0u16;
        for frame in start..end {
            for ch in 0..channels {
                let sample = pcm.samples[frame * channels + ch];
                max_abs = max_abs.max(sample.unsigned_abs());
            }
        }
        *peak = max_abs;
    }

    let global_max = raw_peaks.iter().copied().max().unwrap_or(0);
    if global_max == 0 {
        return [0u8; BUCKET_COUNT];
    }

    let mut peaks = [0u8; BUCKET_COUNT];
    for (i, &peak) in raw_peaks.iter().enumerate() {
        let normalized = peak as f32 / global_max as f32;
        peaks[i] = (normalized * 255.0).round() as u8;
    }
    peaks
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::io::Write;

    fn write_wav(path: &Path, channels: u16, samples: &[i16]) {
        let bits_per_sample: u16 = 16;
        let byte_rate = 44100u32 * channels as u32 * bits_per_sample as u32 / 8;
        let block_align = channels * bits_per_sample / 8;
        let data_size = (samples.len() * 2) as u32;
        let fmt_size = 16u32;
        let riff_size = 4 + 8 + fmt_size + 8 + data_size;

        let mut file = std::fs::File::create(path).expect("create wav");
        file.write_all(b"RIFF").unwrap();
        file.write_all(&riff_size.to_le_bytes()).unwrap();
        file.write_all(b"WAVE").unwrap();
        file.write_all(b"fmt ").unwrap();
        file.write_all(&fmt_size.to_le_bytes()).unwrap();
        file.write_all(&1u16.to_le_bytes()).unwrap(); // PCM
        file.write_all(&channels.to_le_bytes()).unwrap();
        file.write_all(&44100u32.to_le_bytes()).unwrap();
        file.write_all(&byte_rate.to_le_bytes()).unwrap();
        file.write_all(&block_align.to_le_bytes()).unwrap();
        file.write_all(&bits_per_sample.to_le_bytes()).unwrap();
        file.write_all(b"data").unwrap();
        file.write_all(&data_size.to_le_bytes()).unwrap();
        for &s in samples {
            file.write_all(&s.to_le_bytes()).unwrap();
        }
    }

    #[test]
    fn silence_produces_all_zeros() {
        let dir = std::env::temp_dir().join("mello_waveform_test");
        let _ = std::fs::create_dir_all(&dir);
        let path = dir.join("silence.wav");
        let samples = vec![0i16; 44100];
        write_wav(&path, 1, &samples);

        let b64 = compute_clip_waveform_b64(&path).expect("waveform");
        let decoded = decode_clip_waveform(&b64);
        assert_eq!(decoded.len(), 64);
        assert!(decoded.iter().all(|&v| v == 0.0));
    }

    #[test]
    fn full_scale_square_wave_produces_255s() {
        let dir = std::env::temp_dir().join("mello_waveform_test");
        let _ = std::fs::create_dir_all(&dir);
        let path = dir.join("square.wav");
        let samples: Vec<i16> = (0..8192)
            .map(|i| if i % 2 == 0 { i16::MAX } else { i16::MIN })
            .collect();
        write_wav(&path, 1, &samples);

        let b64 = compute_clip_waveform_b64(&path).expect("waveform");
        let bytes = base64::engine::general_purpose::STANDARD
            .decode(&b64)
            .expect("decode b64");
        assert_eq!(bytes.len(), 64);
        assert!(bytes.iter().all(|&b| b == 255));
    }

    #[test]
    fn ramp_is_monotonically_increasing() {
        let dir = std::env::temp_dir().join("mello_waveform_test");
        let _ = std::fs::create_dir_all(&dir);
        let path = dir.join("ramp.wav");
        let frame_count = 64 * 100;
        let samples: Vec<i16> = (0..frame_count)
            .map(|i| (i as f32 / frame_count as f32 * i16::MAX as f32) as i16)
            .collect();
        write_wav(&path, 1, &samples);

        let b64 = compute_clip_waveform_b64(&path).expect("waveform");
        let decoded = decode_clip_waveform(&b64);
        for window in decoded.windows(2) {
            assert!(
                window[1] + 0.001 >= window[0],
                "not monotonic: {:?}",
                decoded
            );
        }
        assert!(*decoded.last().expect("last") > 0.9);
    }

    #[test]
    fn stereo_uses_both_channels() {
        let dir = std::env::temp_dir().join("mello_waveform_test");
        let _ = std::fs::create_dir_all(&dir);
        let path = dir.join("stereo.wav");
        let frames = 4096usize;
        let mut samples = Vec::with_capacity(frames * 2);
        for i in 0..frames {
            let left = 0i16;
            let right = if i < frames / 2 { i16::MAX } else { 0 };
            samples.push(left);
            samples.push(right);
        }
        write_wav(&path, 2, &samples);

        let b64 = compute_clip_waveform_b64(&path).expect("waveform");
        let decoded = decode_clip_waveform(&b64);
        let first_half_max = decoded[..32].iter().copied().fold(0.0f32, f32::max);
        let second_half_max = decoded[32..].iter().copied().fold(0.0f32, f32::max);
        assert!(
            first_half_max > 0.9,
            "first half should be loud: {first_half_max}"
        );
        assert!(
            second_half_max < 0.1,
            "second half should be quiet: {second_half_max}"
        );
    }

    #[test]
    fn garbage_file_returns_none() {
        let dir = std::env::temp_dir().join("mello_waveform_test");
        let _ = std::fs::create_dir_all(&dir);
        let path = dir.join("garbage.wav");
        std::fs::write(&path, b"not a wav file at all").unwrap();
        assert!(compute_clip_waveform_b64(&path).is_none());
    }

    #[test]
    fn truncated_wav_returns_none() {
        let dir = std::env::temp_dir().join("mello_waveform_test");
        let _ = std::fs::create_dir_all(&dir);
        let path = dir.join("truncated.wav");
        std::fs::write(&path, b"RIFF\x00\x00\x00\x00WAVE").unwrap();
        assert!(compute_clip_waveform_b64(&path).is_none());
    }

    #[test]
    fn encode_decode_roundtrip() {
        let original: [u8; 64] = core::array::from_fn(|i| (i * 4) as u8);
        let b64 = base64::engine::general_purpose::STANDARD.encode(original);
        let decoded = decode_clip_waveform(&b64);
        assert_eq!(decoded.len(), 64);
        for (i, &v) in decoded.iter().enumerate() {
            let expected = original[i] as f32 / 255.0;
            assert!((v - expected).abs() < 1e-6, "bucket {i}: {v} vs {expected}");
        }
    }

    #[test]
    fn decode_empty_or_invalid_returns_empty() {
        assert!(decode_clip_waveform("").is_empty());
        assert!(decode_clip_waveform("not!!!valid").is_empty());
    }
}
