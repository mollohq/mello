//! The deterministic reference clip: what the reference source
//! (libmello/src/video/capture_reference.cpp) plays.
//!
//! The repository holds this generator and its seed, not the clip. The
//! harness renders the clip on first use into `benchmarks/clips/`
//! (git-ignored) and checks its FNV-1a 64 hash against the value frozen in
//! `benchmarks/baselines/stream/profiles.json`.
//!
//! Content, at 60 fps, 1200 frames (20 s). Positions are in 1080p units and
//! scale with the frame width, so the 720p cut shows the same scene.
//!
//! | Frames | Content |
//! |---|---|
//! | 0-599 | Scene A: a textured background that pans right, six textured sprites that bounce at different speeds, two text lines that scroll left |
//! | 600 | Scene cut |
//! | 600-839 | Scene B: other textures, colours and pan direction, the same kinds of motion |
//! | 840-1019 | Static: the picture of frame 840, only the burned-in code changes |
//! | 1020-1199 | Scene B resumes from where it stopped |
//!
//! Every frame carries the burned-in code (`code.rs`) in its top-left
//! corner: the frame index and the clip clock in milliseconds.
//!
//! Determinism. The generator uses integer arithmetic and `f64` add,
//! multiply and divide only (no trigonometry, no fused multiply-add), so the
//! bytes are the same on every platform. The seeded generator is
//! `voice_gate::rng`, whose sequence is frozen by a test.
//!
//! Format: I420 (Y, then U, then V), BT.709 studio range, frames back to
//! back, no header.

use crate::code::{self, FrameCode};
use voice_gate::rng::Rng;

/// Generator version. A change of the rendering changes the hash; bump it
/// and re-take the baselines.
pub const CLIP_VERSION: u32 = 1;
pub const SCENE_CUT: u32 = 600;
pub const STATIC_START: u32 = 840;
pub const STATIC_FRAMES: u32 = 180;

/// What to render.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct ClipSpec {
    pub width: u32,
    pub height: u32,
    pub fps: u32,
    pub frames: u32,
    pub seed: u64,
}

impl ClipSpec {
    pub fn frame_bytes(&self) -> usize {
        (self.width as usize) * (self.height as usize) * 3 / 2
    }

    /// The clip clock of frame `index`, in milliseconds.
    pub fn clock_ms(&self, index: u32) -> u32 {
        ((u64::from(index) * 1000 + u64::from(self.fps) / 2) / u64::from(self.fps)) as u32
    }

    /// File name in `benchmarks/clips/`.
    pub fn file_name(&self) -> String {
        format!(
            "stream-gate-v{}-{}x{}p{}-{}f-seed{}.yuv",
            CLIP_VERSION, self.width, self.height, self.fps, self.frames, self.seed
        )
    }
}

/// The content time of a frame: the static section holds the picture.
pub fn content_frame(index: u32) -> u32 {
    if index < STATIC_START {
        index
    } else if index < STATIC_START + STATIC_FRAMES {
        STATIC_START
    } else {
        index - STATIC_FRAMES
    }
}

const TILE: usize = 512;

/// A 512 x 512 luma texture: value noise at two scales plus fine grain.
fn noise_tile(rng: &mut Rng, lo: i32, hi: i32) -> Vec<u8> {
    let coarse = grid(rng, 8); // 64 px cells
    let mid = grid(rng, 32); // 16 px cells
    let mut out = vec![0u8; TILE * TILE];
    for y in 0..TILE {
        for x in 0..TILE {
            let a = sample_grid(&coarse, 8, x, y);
            let b = sample_grid(&mid, 32, x, y);
            let grain = (rng.next_u64() >> 59) as i32 - 16; // -16..15
            let v = (a * 5 + b * 3) / 8; // 0..255
            let v = lo + (v * (hi - lo)) / 255 + grain;
            out[y * TILE + x] = v.clamp(16, 235) as u8;
        }
    }
    out
}

fn grid(rng: &mut Rng, n: usize) -> Vec<i32> {
    (0..n * n).map(|_| (rng.next_u64() >> 56) as i32).collect()
}

/// Bilinear sample of an n x n periodic grid over the tile.
fn sample_grid(g: &[i32], n: usize, x: usize, y: usize) -> i32 {
    let cell = TILE / n;
    let (gx, gy) = (x / cell, y / cell);
    let (fx, fy) = ((x % cell) as i32, (y % cell) as i32);
    let c = cell as i32;
    let at = |i: usize, j: usize| g[(j % n) * n + (i % n)];
    let top = at(gx, gy) * (c - fx) + at(gx + 1, gy) * fx;
    let bot = at(gx, gy + 1) * (c - fx) + at(gx + 1, gy + 1) * fx;
    (top * (c - fy) + bot * fy) / (c * c)
}

/// A sprite: a textured rectangle that bounces inside the frame.
#[derive(Clone, Debug)]
struct Sprite {
    w: i64,
    h: i64,
    x0: i64,
    y0: i64,
    vx: i64,
    vy: i64,
    kind: u8,
    period: i64,
    luma_a: u8,
    luma_b: u8,
    u: u8,
    v: u8,
}

struct Scene {
    tile_y: Vec<u8>,
    tile_c: Vec<u8>,
    pan_x: i64,
    pan_y: i64,
    tint_u: i32,
    tint_v: i32,
    sprites: Vec<Sprite>,
}

fn make_scene(rng: &mut Rng, b: bool) -> Scene {
    let (lo, hi) = if b { (60, 220) } else { (30, 190) };
    let tile_y = noise_tile(rng, lo, hi);
    let tile_c = noise_tile(rng, 16, 235);
    let mut sprites = Vec::new();
    for i in 0..6 {
        let pick =
            |rng: &mut Rng, lo: i64, hi: i64| lo + (rng.next_u64() % (hi - lo + 1) as u64) as i64;
        let speed = if i < 2 {
            18
        } else if i < 4 {
            9
        } else {
            4
        };
        sprites.push(Sprite {
            w: pick(rng, 160, 420),
            h: pick(rng, 120, 320),
            x0: pick(rng, 0, 1500),
            y0: pick(rng, 60, 700),
            vx: pick(rng, speed / 2, speed) * if rng.next_u64() & 1 == 0 { 1 } else { -1 },
            vy: pick(rng, speed / 3, speed) * if rng.next_u64() & 1 == 0 { 1 } else { -1 },
            kind: (i % 3) as u8,
            period: pick(rng, 8, 28),
            luma_a: pick(rng, 20, 80) as u8,
            luma_b: pick(rng, 160, 235) as u8,
            u: pick(rng, 40, 216) as u8,
            v: pick(rng, 40, 216) as u8,
        });
    }
    Scene {
        tile_y,
        tile_c,
        pan_x: if b { -5 } else { 3 },
        pan_y: if b { 2 } else { 0 },
        tint_u: if b { 20 } else { -10 },
        tint_v: if b { -15 } else { 12 },
        sprites,
    }
}

/// Position on a bouncing path: a triangle wave over [0, span].
fn bounce(start: i64, v: i64, t: i64, span: i64) -> i64 {
    if span <= 0 {
        return 0;
    }
    let p = (start + v * t).rem_euclid(2 * span);
    if p > span {
        2 * span - p
    } else {
        p
    }
}

// 5 x 7 bitmap font: digits, upper-case letters and space.
fn glyph(ch: u8) -> [u8; 7] {
    match ch {
        b'0' => [0x0E, 0x11, 0x13, 0x15, 0x19, 0x11, 0x0E],
        b'1' => [0x04, 0x0C, 0x04, 0x04, 0x04, 0x04, 0x0E],
        b'2' => [0x0E, 0x11, 0x01, 0x02, 0x04, 0x08, 0x1F],
        b'3' => [0x1F, 0x02, 0x04, 0x02, 0x01, 0x11, 0x0E],
        b'4' => [0x02, 0x06, 0x0A, 0x12, 0x1F, 0x02, 0x02],
        b'5' => [0x1F, 0x10, 0x1E, 0x01, 0x01, 0x11, 0x0E],
        b'6' => [0x06, 0x08, 0x10, 0x1E, 0x11, 0x11, 0x0E],
        b'7' => [0x1F, 0x01, 0x02, 0x04, 0x08, 0x08, 0x08],
        b'8' => [0x0E, 0x11, 0x11, 0x0E, 0x11, 0x11, 0x0E],
        b'9' => [0x0E, 0x11, 0x11, 0x0F, 0x01, 0x02, 0x0C],
        b'A' => [0x0E, 0x11, 0x11, 0x1F, 0x11, 0x11, 0x11],
        b'B' => [0x1E, 0x11, 0x11, 0x1E, 0x11, 0x11, 0x1E],
        b'C' => [0x0E, 0x11, 0x10, 0x10, 0x10, 0x11, 0x0E],
        b'D' => [0x1C, 0x12, 0x11, 0x11, 0x11, 0x12, 0x1C],
        b'E' => [0x1F, 0x10, 0x10, 0x1E, 0x10, 0x10, 0x1F],
        b'F' => [0x1F, 0x10, 0x10, 0x1E, 0x10, 0x10, 0x10],
        b'G' => [0x0E, 0x11, 0x10, 0x17, 0x11, 0x11, 0x0F],
        b'H' => [0x11, 0x11, 0x11, 0x1F, 0x11, 0x11, 0x11],
        b'I' => [0x0E, 0x04, 0x04, 0x04, 0x04, 0x04, 0x0E],
        b'J' => [0x07, 0x02, 0x02, 0x02, 0x02, 0x12, 0x0C],
        b'K' => [0x11, 0x12, 0x14, 0x18, 0x14, 0x12, 0x11],
        b'L' => [0x10, 0x10, 0x10, 0x10, 0x10, 0x10, 0x1F],
        b'M' => [0x11, 0x1B, 0x15, 0x15, 0x11, 0x11, 0x11],
        b'N' => [0x11, 0x11, 0x19, 0x15, 0x13, 0x11, 0x11],
        b'O' => [0x0E, 0x11, 0x11, 0x11, 0x11, 0x11, 0x0E],
        b'P' => [0x1E, 0x11, 0x11, 0x1E, 0x10, 0x10, 0x10],
        b'Q' => [0x0E, 0x11, 0x11, 0x11, 0x15, 0x12, 0x0D],
        b'R' => [0x1E, 0x11, 0x11, 0x1E, 0x14, 0x12, 0x11],
        b'S' => [0x0F, 0x10, 0x10, 0x0E, 0x01, 0x01, 0x1E],
        b'T' => [0x1F, 0x04, 0x04, 0x04, 0x04, 0x04, 0x04],
        b'U' => [0x11, 0x11, 0x11, 0x11, 0x11, 0x11, 0x0E],
        b'V' => [0x11, 0x11, 0x11, 0x11, 0x11, 0x0A, 0x04],
        b'W' => [0x11, 0x11, 0x11, 0x15, 0x15, 0x15, 0x0A],
        b'X' => [0x11, 0x11, 0x0A, 0x04, 0x0A, 0x11, 0x11],
        b'Y' => [0x11, 0x11, 0x11, 0x0A, 0x04, 0x04, 0x04],
        b'Z' => [0x1F, 0x01, 0x02, 0x04, 0x08, 0x10, 0x1F],
        _ => [0; 7],
    }
}

const TEXT_BIG: &[u8] =
    b"MELLO STREAM QUALITY GATE 0123456789 THE QUICK BROWN FOX JUMPS OVER THE LAZY DOG   ";
const TEXT_SMALL: &[u8] =
    b"PACK MY BOX WITH FIVE DOZEN LIQUOR JUGS 9876543210 SPHINX OF BLACK QUARTZ JUDGE MY VOW   ";

/// Is the text pixel at (x, y) of a line lit? `scale` pixels per font dot,
/// one dot of spacing per character, the line scrolled left by `scroll`.
fn text_lit(text: &[u8], scale: i64, scroll: i64, x: i64, y: i64) -> bool {
    let cw = 6 * scale;
    let total = cw * text.len() as i64;
    let xx = (x + scroll).rem_euclid(total);
    let ci = (xx / cw) as usize;
    let gx = (xx % cw) / scale;
    let gy = y / scale;
    if !(0..5).contains(&gx) || !(0..7).contains(&gy) {
        return false;
    }
    let row = glyph(text[ci])[gy as usize];
    (row >> (4 - gx)) & 1 == 1
}

/// Renders frames of one clip. Holds the two scenes.
pub struct Renderer {
    spec: ClipSpec,
    scenes: [Scene; 2],
}

impl Renderer {
    pub fn new(spec: ClipSpec) -> Self {
        let mut rng = Rng::stream(spec.seed, "clip");
        let a = make_scene(&mut rng, false);
        let b = make_scene(&mut rng, true);
        Self {
            spec,
            scenes: [a, b],
        }
    }

    pub fn spec(&self) -> &ClipSpec {
        &self.spec
    }

    /// Render frame `index` as I420 into `out` (`frame_bytes()` long).
    pub fn render(&self, index: u32, out: &mut [u8]) {
        let w = self.spec.width as usize;
        let h = self.spec.height as usize;
        let (yp, rest) = out.split_at_mut(w * h);
        let (up, vp) = rest.split_at_mut(w * h / 4);
        let t = i64::from(content_frame(index));
        let scene = &self.scenes[usize::from(content_frame(index) >= SCENE_CUT)];
        let t_scene = if t >= i64::from(SCENE_CUT) {
            t - i64::from(SCENE_CUT)
        } else {
            t
        };
        // 1080p units to pixels: x * w / 1920.
        let su = |v: i64| v * w as i64 / 1920;
        let (cx, cy) = (scene.pan_x * t_scene, scene.pan_y * t_scene);
        let band_h = su(150);
        let band_top = h as i64 - band_h;

        for y in 0..h {
            let yy = y as i64;
            let row = &mut yp[y * w..(y + 1) * w];
            // Background (tile coordinates in 1080p units).
            let ty = ((yy * 1920 / w as i64 + cy).rem_euclid(TILE as i64)) as usize;
            for (x, px) in row.iter_mut().enumerate() {
                let tx = ((x as i64 * 1920 / w as i64 + cx).rem_euclid(TILE as i64)) as usize;
                *px = scene.tile_y[ty * TILE + tx];
            }
            // Sprites.
            for s in &scene.sprites {
                let sx = bounce(s.x0, s.vx, t_scene, 1920 - s.w);
                let sy = bounce(s.y0, s.vy, t_scene, 930 - s.h);
                let (x0, x1, y0, y1) = (su(sx), su(sx + s.w), su(sy), su(sy + s.h));
                if yy < y0 || yy >= y1 {
                    continue;
                }
                let ly = (yy - y0) * 1920 / w as i64;
                for x in x0.max(0)..x1.min(w as i64) {
                    let lx = (x - x0) * 1920 / w as i64;
                    let p = s.period;
                    let on = match s.kind {
                        0 => ((lx / p) + (ly / p)) % 2 == 0,
                        1 => ((lx + ly + 3 * t_scene) / p) % 2 == 0,
                        _ => {
                            scene.tile_y
                                [((ly * 3) as usize % TILE) * TILE + (lx * 3) as usize % TILE]
                                > 128
                        }
                    };
                    row[x as usize] = if on { s.luma_b } else { s.luma_a };
                }
            }
            // Text band: two lines, big and small, scrolling left.
            if yy >= band_top {
                let by = yy - band_top;
                let big_scale = su(9).max(2);
                let small_scale = su(4).max(1);
                let big_top = su(12);
                let small_top = big_top + 8 * big_scale;
                for (x, px) in row.iter_mut().enumerate() {
                    let xu = x as i64;
                    let lit = (by >= big_top
                        && text_lit(TEXT_BIG, big_scale, su(7) * t, xu, by - big_top))
                        || (by >= small_top
                            && text_lit(TEXT_SMALL, small_scale, su(3) * t, xu, by - small_top));
                    *px = if lit { 230 } else { 24 };
                }
            }
        }

        // Chroma at half resolution.
        let (cw, ch) = (w / 2, h / 2);
        for y in 0..ch {
            let yy = (y * 2) as i64;
            let ty = ((yy * 1920 / w as i64 + cy) / 2).rem_euclid(TILE as i64) as usize;
            for x in 0..cw {
                let tx =
                    (((x * 2) as i64 * 1920 / w as i64 + cx) / 2).rem_euclid(TILE as i64) as usize;
                let n = i32::from(scene.tile_c[ty * TILE + tx]) - 128;
                up[y * cw + x] = (128 + n / 3 + scene.tint_u).clamp(16, 240) as u8;
                vp[y * cw + x] = (128 - n / 4 + scene.tint_v).clamp(16, 240) as u8;
            }
            for s in &scene.sprites {
                let sx = bounce(s.x0, s.vx, t_scene, 1920 - s.w);
                let sy = bounce(s.y0, s.vy, t_scene, 930 - s.h);
                let (x0, x1, y0, y1) = (su(sx) / 2, su(sx + s.w) / 2, su(sy) / 2, su(sy + s.h) / 2);
                let yh = y as i64;
                if yh < y0 || yh >= y1 {
                    continue;
                }
                for x in x0.max(0)..x1.min(cw as i64) {
                    up[y * cw + x as usize] = s.u;
                    vp[y * cw + x as usize] = s.v;
                }
            }
            if yy >= band_top {
                up[y * cw..(y + 1) * cw].fill(128);
                vp[y * cw..(y + 1) * cw].fill(128);
            }
        }

        code::paint(
            FrameCode {
                index,
                clock_ms: self.spec.clock_ms(index),
            },
            yp,
            w,
            up,
            vp,
            cw,
        );
    }
}

/// FNV-1a 64, continued from `h` (start with `FNV_INIT`).
pub const FNV_INIT: u64 = 0xcbf2_9ce4_8422_2325;
pub fn fnv1a(mut h: u64, bytes: &[u8]) -> u64 {
    for &b in bytes {
        h ^= u64::from(b);
        h = h.wrapping_mul(0x0000_0100_0000_01B3);
    }
    h
}

/// Render the whole clip into `path`, using every core: frames render in
/// parallel batches and are written and hashed in order. Returns the hash.
pub fn write_clip(spec: &ClipSpec, path: &std::path::Path) -> Result<String, String> {
    use std::io::Write;
    let renderer = Renderer::new(spec.clone());
    if let Some(dir) = path.parent() {
        std::fs::create_dir_all(dir).map_err(|e| format!("{}: {e}", dir.display()))?;
    }
    let tmp = path.with_extension("yuv.part");
    let file = std::fs::File::create(&tmp).map_err(|e| format!("{}: {e}", tmp.display()))?;
    let mut out = std::io::BufWriter::with_capacity(8 << 20, file);
    let threads = std::thread::available_parallelism().map_or(4, |n| n.get());
    let fb = spec.frame_bytes();
    let mut hash = FNV_INIT;
    let mut next = 0u32;
    while next < spec.frames {
        let batch = (spec.frames - next).min(threads as u32 * 2);
        let mut bufs: Vec<Vec<u8>> = (0..batch).map(|_| vec![0u8; fb]).collect();
        std::thread::scope(|s| {
            for (chunk_i, chunk) in bufs
                .chunks_mut(batch.div_ceil(threads as u32) as usize)
                .enumerate()
            {
                let r = &renderer;
                let first = next + (chunk_i * batch.div_ceil(threads as u32) as usize) as u32;
                s.spawn(move || {
                    for (k, buf) in chunk.iter_mut().enumerate() {
                        r.render(first + k as u32, buf);
                    }
                });
            }
        });
        for b in &bufs {
            hash = fnv1a(hash, b);
            out.write_all(b).map_err(|e| e.to_string())?;
        }
        next += batch;
    }
    out.flush().map_err(|e| e.to_string())?;
    drop(out);
    std::fs::rename(&tmp, path).map_err(|e| format!("{}: {e}", path.display()))?;
    Ok(format!("{hash:016x}"))
}

/// Hash an existing clip file.
pub fn hash_file(path: &std::path::Path) -> Result<String, String> {
    use std::io::Read;
    let mut f = std::fs::File::open(path).map_err(|e| format!("{}: {e}", path.display()))?;
    let mut buf = vec![0u8; 8 << 20];
    let mut h = FNV_INIT;
    loop {
        let n = f.read(&mut buf).map_err(|e| e.to_string())?;
        if n == 0 {
            break;
        }
        h = fnv1a(h, &buf[..n]);
    }
    Ok(format!("{h:016x}"))
}

#[cfg(test)]
mod tests {
    use super::*;

    fn spec() -> ClipSpec {
        ClipSpec {
            width: 640,
            height: 360,
            fps: 60,
            frames: 1200,
            seed: 20261021,
        }
    }

    #[test]
    fn content_time_holds_through_the_static_section() {
        assert_eq!(content_frame(839), 839);
        assert_eq!(content_frame(840), 840);
        assert_eq!(content_frame(1019), 840);
        assert_eq!(content_frame(1020), 840);
        assert_eq!(content_frame(1021), 841);
    }

    #[test]
    fn rendering_is_deterministic_and_carries_the_code() {
        let r = Renderer::new(spec());
        let mut a = vec![0u8; spec().frame_bytes()];
        let mut b = vec![0u8; spec().frame_bytes()];
        for index in [0u32, 599, 600, 900, 1199] {
            r.render(index, &mut a);
            Renderer::new(spec()).render(index, &mut b);
            assert_eq!(a, b, "frame {index}");
            let got = code::read_luma(&a, 640).expect("code reads");
            assert_eq!(got.index, index);
            assert_eq!(got.clock_ms, spec().clock_ms(index));
        }
    }

    #[test]
    fn frames_move_cut_and_hold() {
        let r = Renderer::new(spec());
        let fb = spec().frame_bytes();
        let f = |i: u32| {
            let mut v = vec![0u8; fb];
            r.render(i, &mut v);
            v
        };
        let diff = |a: &[u8], b: &[u8]| {
            // Outside the code region (first 48 rows).
            a[640 * 48..640 * 360]
                .iter()
                .zip(&b[640 * 48..640 * 360])
                .filter(|(x, y)| x != y)
                .count()
        };
        let (f10, f11) = (f(10), f(11));
        assert!(diff(&f10, &f11) > 10_000, "motion");
        let (f599, f600) = (f(599), f(600));
        assert!(diff(&f599, &f600) > 150_000, "scene cut");
        let (f850, f900) = (f(850), f(900));
        assert_eq!(diff(&f850, &f900), 0, "static section");
    }

    #[test]
    fn the_hash_of_a_frame_is_frozen() {
        // The baseline depends on these bytes. A change here moves every
        // score: bump CLIP_VERSION and re-take the baselines.
        let r = Renderer::new(spec());
        let mut a = vec![0u8; spec().frame_bytes()];
        r.render(123, &mut a);
        assert_eq!(format!("{:016x}", fnv1a(FNV_INIT, &a)), FROZEN_FRAME_123);
    }

    const FROZEN_FRAME_123: &str = "0e49a68b08051412";
}
