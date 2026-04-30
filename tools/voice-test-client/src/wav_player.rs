use std::fs::File;
use std::io::{Read, Seek, SeekFrom};
use std::path::Path;

pub const FRAME_SAMPLES: usize = 960;

pub fn read_wav_mono_48k_i16(path: &str) -> Result<Vec<i16>, String> {
    let p = Path::new(path);
    let mut file = File::open(p).map_err(|e| format!("failed to open WAV '{}': {}", path, e))?;

    let mut riff = [0u8; 12];
    file.read_exact(&mut riff)
        .map_err(|e| format!("failed to read WAV header '{}': {}", path, e))?;
    if &riff[0..4] != b"RIFF" || &riff[8..12] != b"WAVE" {
        return Err(format!("'{}' is not a RIFF/WAVE file", path));
    }

    let mut fmt_found = false;
    let mut data_found = false;
    let mut channels = 0u16;
    let mut sample_rate = 0u32;
    let mut bits_per_sample = 0u16;
    let mut data_bytes: Vec<u8> = Vec::new();

    loop {
        let mut chunk_header = [0u8; 8];
        match file.read_exact(&mut chunk_header) {
            Ok(()) => {}
            Err(_) => break,
        }

        let chunk_id = &chunk_header[0..4];
        let chunk_size = u32::from_le_bytes([
            chunk_header[4],
            chunk_header[5],
            chunk_header[6],
            chunk_header[7],
        ]) as usize;

        if chunk_id == b"fmt " {
            if chunk_size < 16 {
                return Err(format!("'{}' has invalid fmt chunk", path));
            }
            let mut fmt = vec![0u8; chunk_size];
            file.read_exact(&mut fmt)
                .map_err(|e| format!("failed to read fmt chunk '{}': {}", path, e))?;

            let audio_format = u16::from_le_bytes([fmt[0], fmt[1]]);
            channels = u16::from_le_bytes([fmt[2], fmt[3]]);
            sample_rate = u32::from_le_bytes([fmt[4], fmt[5], fmt[6], fmt[7]]);
            bits_per_sample = u16::from_le_bytes([fmt[14], fmt[15]]);

            if audio_format != 1 {
                return Err(format!(
                    "'{}' must be PCM (format=1), got {}",
                    path, audio_format
                ));
            }
            fmt_found = true;
        } else if chunk_id == b"data" {
            data_bytes.resize(chunk_size, 0);
            file.read_exact(&mut data_bytes)
                .map_err(|e| format!("failed to read data chunk '{}': {}", path, e))?;
            data_found = true;
        } else {
            file.seek(SeekFrom::Current(chunk_size as i64))
                .map_err(|e| format!("failed to skip chunk in '{}': {}", path, e))?;
        }

        if (chunk_size & 1) != 0 {
            let mut pad = [0u8; 1];
            let _ = file.read_exact(&mut pad);
        }
    }

    if !fmt_found || !data_found {
        return Err(format!("'{}' is missing fmt or data chunk", path));
    }
    if channels != 1 || sample_rate != 48_000 || bits_per_sample != 16 {
        return Err(format!(
            "'{}' must be mono/48k/16-bit PCM (got ch={}, rate={}, bits={})",
            path, channels, sample_rate, bits_per_sample
        ));
    }

    if (data_bytes.len() & 1) != 0 {
        return Err(format!("'{}' data chunk size is not aligned to i16", path));
    }

    let mut out = Vec::with_capacity(data_bytes.len() / 2);
    for chunk in data_bytes.chunks_exact(2) {
        out.push(i16::from_le_bytes([chunk[0], chunk[1]]));
    }
    Ok(out)
}

pub struct FrameMixer {
    clean: Vec<i16>,
    noise: Option<Vec<i16>>,
    clean_pos: usize,
    noise_pos: usize,
    noise_gain: f32,
    looping: bool,
}

impl FrameMixer {
    pub fn new(clean: Vec<i16>, noise: Option<Vec<i16>>, noise_gain: f32, looping: bool) -> Self {
        Self {
            clean,
            noise,
            clean_pos: 0,
            noise_pos: 0,
            noise_gain,
            looping,
        }
    }

    pub fn next_frame(&mut self) -> Option<Vec<i16>> {
        if self.clean.is_empty() {
            return None;
        }

        if self.clean_pos >= self.clean.len() {
            if self.looping {
                self.clean_pos = 0;
            } else {
                return None;
            }
        }

        let mut frame = Vec::with_capacity(FRAME_SAMPLES);
        for _ in 0..FRAME_SAMPLES {
            if self.clean_pos >= self.clean.len() {
                if self.looping {
                    self.clean_pos = 0;
                } else {
                    frame.push(0);
                    continue;
                }
            }

            let clean = self.clean[self.clean_pos] as f32;
            self.clean_pos += 1;

            let noise = match self.noise.as_ref() {
                Some(noise) if !noise.is_empty() => {
                    if self.noise_pos >= noise.len() {
                        if self.looping {
                            self.noise_pos = 0;
                        }
                    }
                    if self.noise_pos < noise.len() {
                        let v = noise[self.noise_pos] as f32;
                        self.noise_pos += 1;
                        v
                    } else {
                        0.0
                    }
                }
                _ => 0.0,
            };

            let mixed = clean + noise * self.noise_gain;
            let clamped = mixed.clamp(i16::MIN as f32, i16::MAX as f32) as i16;
            frame.push(clamped);
        }

        Some(frame)
    }
}
