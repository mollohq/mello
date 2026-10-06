//! Signal helpers: FFT, low-pass decimation, levels.

use std::f64::consts::PI;

/// In-place iterative radix-2 complex FFT. `re.len()` must be a power of two.
/// `inverse` computes the unscaled inverse transform.
pub fn fft(re: &mut [f64], im: &mut [f64], inverse: bool) {
    let n = re.len();
    assert!(n.is_power_of_two() && im.len() == n);
    // Bit-reversal permutation.
    let mut j = 0usize;
    for i in 1..n {
        let mut bit = n >> 1;
        while j & bit != 0 {
            j ^= bit;
            bit >>= 1;
        }
        j |= bit;
        if i < j {
            re.swap(i, j);
            im.swap(i, j);
        }
    }
    let sign = if inverse { 1.0 } else { -1.0 };
    let mut len = 2;
    while len <= n {
        let ang = sign * 2.0 * PI / len as f64;
        let (wr, wi) = (ang.cos(), ang.sin());
        let half = len / 2;
        let mut start = 0;
        while start < n {
            let (mut cr, mut ci) = (1.0f64, 0.0f64);
            for k in 0..half {
                let a = start + k;
                let b = a + half;
                let tr = re[b] * cr - im[b] * ci;
                let ti = re[b] * ci + im[b] * cr;
                re[b] = re[a] - tr;
                im[b] = im[a] - ti;
                re[a] += tr;
                im[a] += ti;
                let ncr = cr * wr - ci * wi;
                ci = cr * wi + ci * wr;
                cr = ncr;
            }
            start += len;
        }
        len <<= 1;
    }
}

/// Windowed-sinc (Blackman) low-pass FIR. `cutoff` is a fraction of the
/// sample rate (0 < cutoff < 0.5). Unity gain at DC.
pub fn lowpass_taps(cutoff: f64, taps: usize) -> Vec<f64> {
    let m = (taps - 1) as f64;
    let mut h: Vec<f64> = (0..taps)
        .map(|i| {
            let x = i as f64 - m / 2.0;
            let sinc = if x == 0.0 {
                2.0 * cutoff
            } else {
                (2.0 * PI * cutoff * x).sin() / (PI * x)
            };
            let w = 0.42 - 0.5 * (2.0 * PI * i as f64 / m).cos()
                + 0.08 * (4.0 * PI * i as f64 / m).cos();
            sinc * w
        })
        .collect();
    let sum: f64 = h.iter().sum();
    for v in &mut h {
        *v /= sum;
    }
    h
}

/// Low-pass filter and keep every `factor`-th sample. The output sample `j`
/// is centred on input sample `j * factor` (the filter delay is removed).
pub fn decimate(x: &[i16], factor: usize, taps: &[f64]) -> Vec<f64> {
    let half = taps.len() / 2;
    let n_out = x.len() / factor;
    let mut out = Vec::with_capacity(n_out);
    for j in 0..n_out {
        let centre = j * factor;
        let mut acc = 0.0;
        for (k, &h) in taps.iter().enumerate() {
            let idx = centre as isize + k as isize - half as isize;
            if idx >= 0 && (idx as usize) < x.len() {
                acc += h * f64::from(x[idx as usize]);
            }
        }
        out.push(acc);
    }
    out
}

/// RMS of a slice of int16 samples, as a fraction of full scale.
pub fn rms_i16(x: &[i16]) -> f64 {
    if x.is_empty() {
        return 0.0;
    }
    let s: f64 = x.iter().map(|&v| f64::from(v) * f64::from(v)).sum();
    (s / x.len() as f64).sqrt() / 32768.0
}

/// Level in dBFS of an RMS fraction.
pub fn dbfs(rms: f64) -> f64 {
    20.0 * (rms + 1e-12).log10()
}

/// Percentile (0..=100) of the values, linear interpolation. None when empty.
pub fn percentile(values: &[f64], p: f64) -> Option<f64> {
    if values.is_empty() {
        return None;
    }
    let mut v = values.to_vec();
    v.sort_by(|a, b| a.partial_cmp(b).unwrap_or(std::cmp::Ordering::Equal));
    let pos = (p / 100.0) * (v.len() - 1) as f64;
    let lo = pos.floor() as usize;
    let hi = pos.ceil() as usize;
    let frac = pos - lo as f64;
    Some(v[lo] * (1.0 - frac) + v[hi] * frac)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn fft_round_trip_and_peak() {
        let n = 64;
        let mut re: Vec<f64> = (0..n)
            .map(|i| (2.0 * PI * 5.0 * i as f64 / n as f64).cos())
            .collect();
        let mut im = vec![0.0; n];
        let orig = re.clone();
        fft(&mut re, &mut im, false);
        let mag: Vec<f64> = re
            .iter()
            .zip(&im)
            .map(|(a, b)| (a * a + b * b).sqrt())
            .collect();
        let peak = mag[..n / 2]
            .iter()
            .enumerate()
            .max_by(|a, b| a.1.partial_cmp(b.1).expect("finite"))
            .map(|(i, _)| i);
        assert_eq!(peak, Some(5));
        fft(&mut re, &mut im, true);
        for (a, b) in re.iter().zip(&orig) {
            assert!((a / n as f64 - b).abs() < 1e-9);
        }
    }

    #[test]
    fn decimate_keeps_dc_and_alignment() {
        let taps = lowpass_taps(0.04, 97);
        let mut x = vec![0i16; 4800];
        x[2400] = 30000; // impulse
        let y = decimate(&x, 12, &taps);
        let peak = y
            .iter()
            .enumerate()
            .max_by(|a, b| a.1.partial_cmp(b.1).expect("finite"))
            .map(|(i, _)| i);
        assert_eq!(peak, Some(200));
        let dc = decimate(&[1000i16; 4800], 12, &taps);
        assert!((dc[200] - 1000.0).abs() < 1.0);
    }

    #[test]
    fn percentile_basic() {
        let v = [1.0, 2.0, 3.0, 4.0, 5.0];
        assert_eq!(percentile(&v, 50.0), Some(3.0));
        assert_eq!(percentile(&v, 100.0), Some(5.0));
        assert_eq!(percentile(&[], 50.0), None);
    }
}
