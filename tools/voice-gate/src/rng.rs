//! Seeded random numbers for the impairment shim.
//!
//! Own implementation (xoshiro256** seeded by SplitMix64) so the sequence for
//! a seed never changes with a dependency upgrade. The baseline depends on it.

/// xoshiro256** generator.
pub struct Rng {
    s: [u64; 4],
}

fn splitmix64(state: &mut u64) -> u64 {
    *state = state.wrapping_add(0x9E37_79B9_7F4A_7C15);
    let mut z = *state;
    z = (z ^ (z >> 30)).wrapping_mul(0xBF58_476D_1CE4_E5B9);
    z = (z ^ (z >> 27)).wrapping_mul(0x94D0_49BB_1331_11EB);
    z ^ (z >> 31)
}

impl Rng {
    /// A generator for `seed`. Equal seeds give equal sequences.
    pub fn new(seed: u64) -> Self {
        let mut st = seed;
        let s = [
            splitmix64(&mut st),
            splitmix64(&mut st),
            splitmix64(&mut st),
            splitmix64(&mut st),
        ];
        Self { s }
    }

    /// A generator for one named stream of a profile seed, so that a change
    /// to one impairment does not shift the random numbers of another.
    pub fn stream(seed: u64, stream: &str) -> Self {
        // FNV-1a of the stream name, mixed into the seed.
        let mut h: u64 = 0xcbf2_9ce4_8422_2325;
        for b in stream.bytes() {
            h ^= u64::from(b);
            h = h.wrapping_mul(0x0000_0100_0000_01B3);
        }
        Self::new(seed ^ h)
    }

    pub fn next_u64(&mut self) -> u64 {
        let result = self.s[1].wrapping_mul(5).rotate_left(7).wrapping_mul(9);
        let t = self.s[1] << 17;
        self.s[2] ^= self.s[0];
        self.s[3] ^= self.s[1];
        self.s[1] ^= self.s[2];
        self.s[0] ^= self.s[3];
        self.s[2] ^= t;
        self.s[3] = self.s[3].rotate_left(45);
        result
    }

    /// Uniform in [0, 1).
    pub fn uniform(&mut self) -> f64 {
        (self.next_u64() >> 11) as f64 * (1.0 / 9_007_199_254_740_992.0)
    }

    /// True with probability `p`.
    pub fn chance(&mut self, p: f64) -> bool {
        p > 0.0 && self.uniform() < p
    }

    /// Standard normal (Box-Muller, one value per call).
    pub fn gaussian(&mut self) -> f64 {
        let u1 = self.uniform().max(1e-300);
        let u2 = self.uniform();
        (-2.0 * u1.ln()).sqrt() * (2.0 * std::f64::consts::PI * u2).cos()
    }

    /// Exponential with mean `mean`.
    pub fn exponential(&mut self, mean: f64) -> f64 {
        -mean * (1.0 - self.uniform()).max(1e-300).ln()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn same_seed_same_sequence() {
        let mut a = Rng::new(42);
        let mut b = Rng::new(42);
        for _ in 0..100 {
            assert_eq!(a.next_u64(), b.next_u64());
        }
    }

    #[test]
    fn sequence_is_frozen() {
        // The baseline depends on these exact values. If this test fails, the
        // generator changed and every profile result moved with it.
        let mut r = Rng::new(1);
        assert_eq!(r.next_u64(), 0xB3F2_AF6D_0FC7_10C5);
    }

    #[test]
    fn uniform_and_gaussian_are_sane() {
        let mut r = Rng::new(7);
        let n = 20_000;
        let mean_u: f64 = (0..n).map(|_| r.uniform()).sum::<f64>() / n as f64;
        assert!((mean_u - 0.5).abs() < 0.02, "mean {mean_u}");
        let g: Vec<f64> = (0..n).map(|_| r.gaussian()).collect();
        let m = g.iter().sum::<f64>() / n as f64;
        let v = g.iter().map(|x| (x - m) * (x - m)).sum::<f64>() / n as f64;
        assert!(m.abs() < 0.05 && (v - 1.0).abs() < 0.05, "mean {m} var {v}");
    }
}
