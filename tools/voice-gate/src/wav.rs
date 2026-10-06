//! Minimal mono 16-bit PCM WAV reader and writer.

use std::fs;
use std::io::Write;
use std::path::Path;

/// Mono 16-bit PCM audio.
pub struct Wav {
    pub sample_rate: u32,
    pub samples: Vec<i16>,
}

fn u16_at(b: &[u8], at: usize) -> u16 {
    u16::from_le_bytes([b[at], b[at + 1]])
}

fn u32_at(b: &[u8], at: usize) -> u32 {
    u32::from_le_bytes([b[at], b[at + 1], b[at + 2], b[at + 3]])
}

/// Read a mono 16-bit PCM WAV. Other formats are an error.
pub fn read(path: &Path) -> Result<Wav, String> {
    let b = fs::read(path).map_err(|e| format!("{}: {e}", path.display()))?;
    if b.len() < 12 || &b[0..4] != b"RIFF" || &b[8..12] != b"WAVE" {
        return Err(format!("{}: not a RIFF/WAVE file", path.display()));
    }
    let mut pos = 12;
    let mut fmt: Option<(u16, u16, u32, u16)> = None;
    while pos + 8 <= b.len() {
        let id = &b[pos..pos + 4];
        let len = u32_at(&b, pos + 4) as usize;
        let body = pos + 8;
        if body + len > b.len() {
            return Err(format!("{}: truncated chunk", path.display()));
        }
        if id == b"fmt " && len >= 16 {
            fmt = Some((
                u16_at(&b, body),
                u16_at(&b, body + 2),
                u32_at(&b, body + 4),
                u16_at(&b, body + 14),
            ));
        } else if id == b"data" {
            let (format, channels, rate, bits) =
                fmt.ok_or_else(|| format!("{}: data before fmt", path.display()))?;
            if format != 1 || channels != 1 || bits != 16 {
                return Err(format!(
                    "{}: need mono 16-bit PCM (format={format} channels={channels} bits={bits})",
                    path.display()
                ));
            }
            let samples = b[body..body + len]
                .chunks_exact(2)
                .map(|c| i16::from_le_bytes([c[0], c[1]]))
                .collect();
            return Ok(Wav {
                sample_rate: rate,
                samples,
            });
        }
        pos = body + len + (len & 1);
    }
    Err(format!("{}: no data chunk", path.display()))
}

/// Write mono 16-bit PCM.
pub fn write(path: &Path, sample_rate: u32, samples: &[i16]) -> Result<(), String> {
    let data_len = (samples.len() * 2) as u32;
    let mut out = Vec::with_capacity(44 + samples.len() * 2);
    out.extend_from_slice(b"RIFF");
    out.extend_from_slice(&(36 + data_len).to_le_bytes());
    out.extend_from_slice(b"WAVEfmt ");
    out.extend_from_slice(&16u32.to_le_bytes());
    out.extend_from_slice(&1u16.to_le_bytes()); // PCM
    out.extend_from_slice(&1u16.to_le_bytes()); // mono
    out.extend_from_slice(&sample_rate.to_le_bytes());
    out.extend_from_slice(&(sample_rate * 2).to_le_bytes());
    out.extend_from_slice(&2u16.to_le_bytes());
    out.extend_from_slice(&16u16.to_le_bytes());
    out.extend_from_slice(b"data");
    out.extend_from_slice(&data_len.to_le_bytes());
    for s in samples {
        out.extend_from_slice(&s.to_le_bytes());
    }
    let mut f = fs::File::create(path).map_err(|e| format!("{}: {e}", path.display()))?;
    f.write_all(&out)
        .map_err(|e| format!("{}: {e}", path.display()))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn round_trip() {
        let dir = std::env::temp_dir().join("voice-gate-wav-test");
        std::fs::create_dir_all(&dir).expect("temp dir");
        let p = dir.join("rt.wav");
        let samples: Vec<i16> = (0..1000).map(|i| (i * 37 % 6000 - 3000) as i16).collect();
        write(&p, 16000, &samples).expect("write");
        let w = read(&p).expect("read");
        assert_eq!(w.sample_rate, 16000);
        assert_eq!(w.samples, samples);
    }
}
