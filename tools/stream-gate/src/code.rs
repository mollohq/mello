//! The burned-in code: the frame index and the clip clock in the top-left
//! corner of every reference frame, as blocks a viewer reads back from the
//! decoded pixels.
//!
//! Layout. Three rows of 32 blocks, each block `BLOCK` x `BLOCK` pixels,
//! from pixel (0, 0): 512 x 48 pixels. The blocks sit on the 16-pixel
//! macroblock grid, so an encoder keeps their edges. Bit order is most
//! significant first, left to right.
//!
//! | Row | Bits |
//! |---|---|
//! | 0 | frame index (u32) |
//! | 1 | clip clock in milliseconds (u32) |
//! | 2 | CRC-16/CCITT-FALSE of the 8 bytes of rows 0 and 1 (big-endian), then its bitwise complement |
//!
//! A 1 bit is luma 235, a 0 bit luma 16, with neutral chroma (128). The
//! reader averages the inner 8 x 8 pixels of a block and thresholds at the
//! middle. A frame whose CRC does not match is unreadable.

/// Block size in pixels.
pub const BLOCK: usize = 16;
/// Blocks per row.
pub const COLS: usize = 32;
/// Rows of blocks.
pub const ROWS: usize = 3;
/// Width and height of the code region in pixels.
pub const REGION_W: usize = BLOCK * COLS;
pub const REGION_H: usize = BLOCK * ROWS;

const LUMA_ONE: u8 = 235;
const LUMA_ZERO: u8 = 16;

/// What one frame carries.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct FrameCode {
    pub index: u32,
    pub clock_ms: u32,
}

/// CRC-16/CCITT-FALSE (poly 0x1021, init 0xFFFF).
pub fn crc16(bytes: &[u8]) -> u16 {
    let mut crc: u16 = 0xFFFF;
    for &b in bytes {
        crc ^= u16::from(b) << 8;
        for _ in 0..8 {
            crc = if crc & 0x8000 != 0 {
                (crc << 1) ^ 0x1021
            } else {
                crc << 1
            };
        }
    }
    crc
}

/// The 96 bits of a code, row by row, most significant first.
pub fn bits(code: FrameCode) -> [bool; COLS * ROWS] {
    let mut payload = [0u8; 8];
    payload[..4].copy_from_slice(&code.index.to_be_bytes());
    payload[4..].copy_from_slice(&code.clock_ms.to_be_bytes());
    let crc = crc16(&payload);
    let row2 = (u32::from(crc) << 16) | u32::from(!crc);
    let words = [code.index, code.clock_ms, row2];
    let mut out = [false; COLS * ROWS];
    for (r, w) in words.iter().enumerate() {
        for c in 0..COLS {
            out[r * COLS + c] = (w >> (31 - c)) & 1 == 1;
        }
    }
    out
}

/// Paint the code into a luma plane (`stride` bytes per row) and set the
/// chroma under it to 128. `u` and `v` are the half-resolution planes of
/// I420 with `cstride` bytes per row.
pub fn paint(
    code: FrameCode,
    y: &mut [u8],
    stride: usize,
    u: &mut [u8],
    v: &mut [u8],
    cstride: usize,
) {
    let b = bits(code);
    for r in 0..ROWS {
        for c in 0..COLS {
            let val = if b[r * COLS + c] { LUMA_ONE } else { LUMA_ZERO };
            for py in 0..BLOCK {
                let row = (r * BLOCK + py) * stride + c * BLOCK;
                y[row..row + BLOCK].fill(val);
            }
        }
    }
    for py in 0..REGION_H / 2 {
        let row = py * cstride;
        u[row..row + REGION_W / 2].fill(128);
        v[row..row + REGION_W / 2].fill(128);
    }
}

/// Read a code from a luma plane. None when the CRC does not match.
pub fn read_luma(y: &[u8], stride: usize) -> Option<FrameCode> {
    read_with(|px, py| y[py * stride + px])
}

/// BT.709 studio-range luma of a full-range RGB pixel, the inverse of the
/// viewer's NV12 to RGBA conversion (staging_texture.cpp).
#[inline]
pub fn luma_709(r: u8, g: u8, b: u8) -> u8 {
    // 16 + 219 * (0.2126 R + 0.7152 G + 0.0722 B) / 255 in 16.16 fixed point.
    let y = 16 * 65536 + 11966 * u32::from(r) + 40254 * u32::from(g) + 4064 * u32::from(b) + 32768;
    (y >> 16).min(255) as u8
}

/// Read a code from an RGBA frame (`width` pixels per row, 4 bytes each).
pub fn read_rgba(rgba: &[u8], width: usize) -> Option<FrameCode> {
    read_with(|px, py| {
        let i = (py * width + px) * 4;
        luma_709(rgba[i], rgba[i + 1], rgba[i + 2])
    })
}

fn read_with(sample: impl Fn(usize, usize) -> u8) -> Option<FrameCode> {
    let mut words = [0u32; ROWS];
    for (r, word) in words.iter_mut().enumerate() {
        for c in 0..COLS {
            let mut sum = 0u32;
            for py in 4..12 {
                for px in 4..12 {
                    sum += u32::from(sample(c * BLOCK + px, r * BLOCK + py));
                }
            }
            let mean = sum / 64;
            let bit = mean >= (u32::from(LUMA_ONE) + u32::from(LUMA_ZERO)) / 2;
            *word = (*word << 1) | u32::from(bit);
        }
    }
    let code = FrameCode {
        index: words[0],
        clock_ms: words[1],
    };
    let mut payload = [0u8; 8];
    payload[..4].copy_from_slice(&code.index.to_be_bytes());
    payload[4..].copy_from_slice(&code.clock_ms.to_be_bytes());
    let crc = crc16(&payload);
    let expect = (u32::from(crc) << 16) | u32::from(!crc);
    (words[2] == expect).then_some(code)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn crc_matches_the_ccitt_false_check_value() {
        assert_eq!(crc16(b"123456789"), 0x29B1);
    }

    #[test]
    fn a_painted_code_reads_back_from_luma_and_rgba() {
        let (w, h) = (640usize, 64usize);
        let mut y = vec![90u8; w * h];
        let mut u = vec![60u8; w * h / 4];
        let mut v = vec![200u8; w * h / 4];
        let code = FrameCode {
            index: 1_234_567,
            clock_ms: 20_576,
        };
        paint(code, &mut y, w, &mut u, &mut v, w / 2);
        assert_eq!(read_luma(&y, w), Some(code));
        // Through the viewer's conversion: studio luma to full-range grey.
        let rgba: Vec<u8> = y
            .iter()
            .flat_map(|&l| {
                let c = ((i32::from(l) - 16) * 298 + 128) >> 8;
                let g = c.clamp(0, 255) as u8;
                [g, g, g, 255]
            })
            .collect();
        assert_eq!(read_rgba(&rgba, w), Some(code));
    }

    #[test]
    fn noise_up_to_a_quarter_of_the_swing_still_reads() {
        let w = 512usize;
        let mut y = vec![0u8; w * REGION_H];
        let mut u = vec![0u8; w * REGION_H / 4];
        let mut v = u.clone();
        let code = FrameCode {
            index: 77,
            clock_ms: 1283,
        };
        paint(code, &mut y, w, &mut u, &mut v, w / 2);
        let mut rng = voice_gate::rng::Rng::new(3);
        for p in y.iter_mut() {
            let n = (rng.uniform() * 100.0) as i32 - 50;
            *p = (i32::from(*p) + n).clamp(0, 255) as u8;
        }
        assert_eq!(read_luma(&y, w), Some(code));
    }

    #[test]
    fn a_damaged_code_is_unreadable() {
        let w = 512usize;
        let mut y = vec![0u8; w * REGION_H];
        let mut u = vec![0u8; w * REGION_H / 4];
        let mut v = u.clone();
        paint(
            FrameCode {
                index: 4,
                clock_ms: 67,
            },
            &mut y,
            w,
            &mut u,
            &mut v,
            w / 2,
        );
        // Set the last index bit (4 becomes 5): the CRC no longer matches.
        for py in 0..BLOCK {
            for px in 0..BLOCK {
                y[py * w + 31 * BLOCK + px] = 235;
            }
        }
        assert_eq!(read_luma(&y, w), None);
    }

    #[test]
    fn luma_709_inverts_the_viewer_conversion_within_one_step() {
        for l in 16u8..=235 {
            let c = ((i32::from(l) - 16) * 298 + 128) >> 8;
            let g = c.clamp(0, 255) as u8;
            let back = luma_709(g, g, g);
            assert!(
                (i32::from(back) - i32::from(l)).abs() <= 1,
                "{l} -> {g} -> {back}"
            );
        }
    }
}
