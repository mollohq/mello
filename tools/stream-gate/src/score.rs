//! Visual scores: PSNR and SSIM on luma per frame here, VMAF through
//! `scripts/stream-gate-score.py` and the pinned `vmaf` CLI.
//!
//! Every presented frame is compared with the reference frame of the same
//! index (read from the burned-in code), so a late or repeated frame is
//! scored for its own picture; freezes and drops are delivery metrics.
//! Frames whose code does not read are not scored (`frames_unreadable`).

use serde_json::{json, Value};
use std::io::{Read, Seek, SeekFrom, Write};
use std::path::{Path, PathBuf};
use std::process::Command;

/// PSNR of two luma planes; 100 for identical planes.
pub fn psnr(a: &[u8], b: &[u8]) -> f64 {
    let mut se: u64 = 0;
    for (x, y) in a.iter().zip(b) {
        let d = i64::from(*x) - i64::from(*y);
        se += (d * d) as u64;
    }
    if se == 0 {
        return 100.0;
    }
    let mse = se as f64 / a.len() as f64;
    (10.0 * (255.0 * 255.0 / mse).log10()).min(100.0)
}

/// Mean SSIM of two luma planes over 8 x 8 windows on a 4-pixel grid
/// (Wang et al. 2004 constants, unweighted windows).
pub fn ssim(a: &[u8], b: &[u8], w: usize, h: usize) -> f64 {
    const C1: f64 = 6.5025; // (0.01 * 255)^2
    const C2: f64 = 58.5225; // (0.03 * 255)^2
    let mut total = 0.0;
    let mut n = 0u64;
    let mut y = 0;
    while y + 8 <= h {
        let mut x = 0;
        while x + 8 <= w {
            let (mut sa, mut sb, mut saa, mut sbb, mut sab) = (0u64, 0u64, 0u64, 0u64, 0u64);
            for r in 0..8 {
                let row = (y + r) * w + x;
                for c in 0..8 {
                    let p = u64::from(a[row + c]);
                    let q = u64::from(b[row + c]);
                    sa += p;
                    sb += q;
                    saa += p * p;
                    sbb += q * q;
                    sab += p * q;
                }
            }
            let ma = sa as f64 / 64.0;
            let mb = sb as f64 / 64.0;
            let va = saa as f64 / 64.0 - ma * ma;
            let vb = sbb as f64 / 64.0 - mb * mb;
            let cov = sab as f64 / 64.0 - ma * mb;
            total += ((2.0 * ma * mb + C1) * (2.0 * cov + C2))
                / ((ma * ma + mb * mb + C1) * (va + vb + C2));
            n += 1;
            x += 4;
        }
        y += 4;
    }
    if n == 0 {
        1.0
    } else {
        total / n as f64
    }
}

/// One frame to score: the presented luma (slot in the distorted file) and
/// the clip frame it shows.
#[derive(Clone, Copy, Debug)]
pub struct Pair {
    pub dis_slot: u64,
    pub clip_frame: u32,
}

#[derive(Clone, Debug, Default)]
pub struct Visual {
    pub psnr: Vec<f64>,
    pub ssim: Vec<f64>,
}

fn read_plane(f: &mut std::fs::File, offset: u64, buf: &mut [u8]) -> Result<(), String> {
    f.seek(SeekFrom::Start(offset)).map_err(|e| e.to_string())?;
    f.read_exact(buf).map_err(|e| e.to_string())
}

/// PSNR and SSIM of every pair, in parallel. `dis` holds luma planes back to
/// back; `clip` is the I420 reference clip.
pub fn psnr_ssim(
    clip: &Path,
    dis: &Path,
    w: usize,
    h: usize,
    pairs: &[Pair],
) -> Result<Visual, String> {
    let threads = std::thread::available_parallelism().map_or(4, |n| n.get());
    let luma = w * h;
    let frame = luma * 3 / 2;
    let chunk = pairs.len().div_ceil(threads).max(1);
    let results: Vec<Result<Vec<(f64, f64)>, String>> = std::thread::scope(|s| {
        let handles: Vec<_> = pairs
            .chunks(chunk)
            .map(|part| {
                s.spawn(move || {
                    let mut cf = std::fs::File::open(clip)
                        .map_err(|e| format!("{}: {e}", clip.display()))?;
                    let mut df =
                        std::fs::File::open(dis).map_err(|e| format!("{}: {e}", dis.display()))?;
                    let (mut r, mut d) = (vec![0u8; luma], vec![0u8; luma]);
                    let mut out = Vec::with_capacity(part.len());
                    for p in part {
                        read_plane(&mut cf, u64::from(p.clip_frame) * frame as u64, &mut r)?;
                        read_plane(&mut df, p.dis_slot * luma as u64, &mut d)?;
                        out.push((psnr(&r, &d), ssim(&r, &d, w, h)));
                    }
                    Ok(out)
                })
            })
            .collect();
        handles
            .into_iter()
            .map(|h| {
                h.join()
                    .unwrap_or_else(|_| Err("scorer thread panicked".into()))
            })
            .collect()
    });
    let mut v = Visual::default();
    for r in results {
        for (p, s) in r? {
            v.psnr.push(p);
            v.ssim.push(s);
        }
    }
    Ok(v)
}

/// Write the reference and distorted I420 files for VMAF (chroma 128: VMAF
/// reads luma only), every `subsample`-th pair. Returns the frame count.
#[allow(clippy::too_many_arguments)]
pub fn write_vmaf_pair(
    clip: &Path,
    dis: &Path,
    w: usize,
    h: usize,
    pairs: &[Pair],
    subsample: usize,
    ref_out: &Path,
    dis_out: &Path,
) -> Result<usize, String> {
    let luma = w * h;
    let frame = luma * 3 / 2;
    let mut cf = std::fs::File::open(clip).map_err(|e| format!("{}: {e}", clip.display()))?;
    let mut df = std::fs::File::open(dis).map_err(|e| format!("{}: {e}", dis.display()))?;
    let open = |p: &Path| {
        std::fs::File::create(p)
            .map(|f| std::io::BufWriter::with_capacity(8 << 20, f))
            .map_err(|e| format!("{}: {e}", p.display()))
    };
    let (mut ro, mut dout) = (open(ref_out)?, open(dis_out)?);
    let grey = vec![128u8; luma / 2];
    let mut buf = vec![0u8; luma];
    let mut n = 0;
    for p in pairs.iter().step_by(subsample.max(1)) {
        read_plane(&mut cf, u64::from(p.clip_frame) * frame as u64, &mut buf)?;
        ro.write_all(&buf).map_err(|e| e.to_string())?;
        ro.write_all(&grey).map_err(|e| e.to_string())?;
        read_plane(&mut df, p.dis_slot * luma as u64, &mut buf)?;
        dout.write_all(&buf).map_err(|e| e.to_string())?;
        dout.write_all(&grey).map_err(|e| e.to_string())?;
        n += 1;
    }
    ro.flush().map_err(|e| e.to_string())?;
    dout.flush().map_err(|e| e.to_string())?;
    Ok(n)
}

/// One VMAF job for the scorer.
#[derive(Clone, Debug)]
pub struct VmafJob {
    pub id: String,
    pub reference: PathBuf,
    pub distorted: PathBuf,
    pub width: usize,
    pub height: usize,
}

/// Pooled VMAF of one job: (mean, 5th percentile, minimum).
pub type VmafPooled = (f64, f64, f64);

/// The scorer's answer.
#[derive(Clone, Debug)]
pub struct VmafScores {
    pub available: bool,
    pub scorer: String,
    pub detail: Value,
    /// id -> (mean, 5th percentile, minimum)
    pub results: Vec<(String, Option<VmafPooled>)>,
}

/// Run `scripts/stream-gate-score.py` on the jobs. Without Python or the
/// `vmaf` CLI the answer says so and the gate reports VMAF as not scored.
pub fn vmaf(script: &Path, out_dir: &Path, jobs: &[VmafJob]) -> VmafScores {
    let none = |reason: String| VmafScores {
        available: false,
        scorer: "none".into(),
        detail: json!({ "available": false, "reason": reason }),
        results: Vec::new(),
    };
    let Ok(py) = std::env::var("STREAM_GATE_PYTHON") else {
        return none("STREAM_GATE_PYTHON not set (run scripts/stream-gate.sh)".into());
    };
    let manifest = out_dir.join("vmaf-jobs.json");
    let body = json!({
        "jobs": jobs.iter().map(|j| json!({
            "id": j.id,
            "reference": j.reference.to_string_lossy(),
            "distorted": j.distorted.to_string_lossy(),
            "width": j.width,
            "height": j.height,
        })).collect::<Vec<_>>()
    });
    if let Err(e) = std::fs::write(&manifest, body.to_string()) {
        return none(format!("{}: {e}", manifest.display()));
    }
    let out = match Command::new(&py).arg(script).arg(&manifest).output() {
        Ok(o) => o,
        Err(e) => return none(format!("{py}: {e}")),
    };
    let text = String::from_utf8_lossy(&out.stdout);
    let v: Value = match serde_json::from_str(text.trim()) {
        Ok(v) => v,
        Err(e) => {
            return none(format!(
                "scorer output is not JSON ({e}); stderr: {}",
                String::from_utf8_lossy(&out.stderr).trim()
            ))
        }
    };
    if !v.get("available").and_then(Value::as_bool).unwrap_or(false) {
        let reason = v
            .get("reason")
            .and_then(Value::as_str)
            .unwrap_or("unknown")
            .to_owned();
        return none(reason);
    }
    let results = v
        .get("results")
        .and_then(Value::as_array)
        .map(|a| {
            a.iter()
                .map(|r| {
                    let id = r.get("id").and_then(Value::as_str).unwrap_or("").to_owned();
                    let g = |k: &str| r.get(k).and_then(Value::as_f64);
                    let s = match (g("mean"), g("p5"), g("min")) {
                        (Some(m), Some(p), Some(n)) => Some((m, p, n)),
                        _ => None,
                    };
                    (id, s)
                })
                .collect()
        })
        .unwrap_or_default();
    VmafScores {
        available: true,
        scorer: v
            .get("scorer")
            .and_then(Value::as_str)
            .unwrap_or("vmaf")
            .to_owned(),
        detail: v,
        results,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn identical_planes_score_perfect() {
        let a: Vec<u8> = (0..64 * 64).map(|i| (i % 251) as u8).collect();
        assert_eq!(psnr(&a, &a), 100.0);
        assert!((ssim(&a, &a, 64, 64) - 1.0).abs() < 1e-12);
    }

    #[test]
    fn noise_lowers_both_scores() {
        // A smooth gradient: noise of sigma 5 is large against its variance.
        let a: Vec<u8> = (0..64 * 64)
            .map(|i| (100 + (i % 64) / 4 + (i / 64) / 4) as u8)
            .collect();
        let mut rng = voice_gate::rng::Rng::new(5);
        let b: Vec<u8> = a
            .iter()
            .map(|&x| (i32::from(x) + (rng.gaussian() * 5.0) as i32).clamp(0, 255) as u8)
            .collect();
        let p = psnr(&a, &b);
        assert!((32.0..36.0).contains(&p), "{p}");
        let s = ssim(&a, &b, 64, 64);
        assert!(s < 0.9 && s > 0.05, "{s}");
    }
}
