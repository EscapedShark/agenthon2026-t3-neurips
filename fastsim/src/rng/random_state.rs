//! NumPy's legacy `np.random.RandomState`, bit-for-bit.
//!
//! Every method mirrors the scalar (`size=None`) path of `numpy/random/mtrand.pyx` and the C in
//! `numpy/random/src/legacy/legacy-distributions.c` / `distributions.c`. The legacy stream is frozen
//! by NumPy's compatibility policy, so it is the same for every NumPy version.
//!
//! Floating-point rules that matter for exactness:
//! * no fused multiply-add anywhere (NumPy's x86-64 build has none; Rust never contracts);
//! * `ln`/`exp`/`exp_m1` resolve to the platform libm, which is glibc in the scoring image — the
//!   same library NumPy's C calls into.

use super::mt19937::Mt19937;

#[derive(Clone)]
pub struct RandomState {
    mt: Mt19937,
    has_gauss: bool,
    gauss: f64,
}

/// `gen_mask`: smallest all-ones mask covering `max`.
#[inline(always)]
fn gen_mask(max: u64) -> u64 {
    let mut mask = max;
    mask |= mask >> 1;
    mask |= mask >> 2;
    mask |= mask >> 4;
    mask |= mask >> 8;
    mask |= mask >> 16;
    mask | (mask >> 32)
}

impl RandomState {
    /// `np.random.RandomState(seed)` for an integer seed in `[0, 2**32)`.
    pub fn new(seed: u32) -> Self {
        Self::from_mt(Mt19937::new(seed))
    }

    /// `np.random.RandomState(seed_array)` for a 1-d array of values in `[0, 2**32)`. A length-1
    /// array squeezes to a scalar in NumPy's `_legacy_seeding`, so it seeds like the integer.
    pub fn from_key(key: &[u32]) -> Self {
        match key {
            [single] => Self::new(*single),
            _ => Self::from_mt(Mt19937::from_key(key)),
        }
    }

    /// [`RandomState::new`] for each of `seeds` (see [`Mt19937::seed_many`]).
    pub fn new_many(seeds: &[u32]) -> Vec<RandomState> {
        let mut states: Vec<RandomState> = Vec::with_capacity(seeds.len());
        let base = states.as_mut_ptr();
        // SAFETY: element `i` of the spare capacity is written whole (its generator here, the
        // rest below) before the length covers it.
        unsafe {
            Mt19937::seed_many(seeds, |i| &raw mut (*base.add(i)).mt);
            for i in 0..seeds.len() {
                (&raw mut (*base.add(i)).has_gauss).write(false);
                (&raw mut (*base.add(i)).gauss).write(0.0);
            }
            states.set_len(seeds.len());
        }
        states
    }

    fn from_mt(mt: Mt19937) -> Self {
        Self {
            mt,
            has_gauss: false,
            gauss: 0.0,
        }
    }

    /// `RandomState.seed(seed)`.
    pub fn seed(&mut self, seed: u32) {
        *self = Self::new(seed);
    }

    /// `(key, pos, has_gauss, cached_gaussian)` as `get_state()` reports them.
    pub fn state(&self) -> (&[u32; super::mt19937::N], usize, bool, f64) {
        let (key, pos) = self.mt.state();
        (key, pos, self.has_gauss, self.gauss)
    }

    #[inline(always)]
    pub fn next_u32(&mut self) -> u32 {
        self.mt.next_u32()
    }

    /// `random_sample()` / `rand()`: a double in `[0, 1)`.
    #[inline(always)]
    pub fn random_sample(&mut self) -> f64 {
        self.mt.next_f64()
    }

    /// `random_bounded_uint64_fill(off, rng, 1, use_masked=True)`: uniform on `[off, off + rng]`.
    #[inline]
    fn bounded_u64(&mut self, off: u64, rng: u64) -> u64 {
        if rng == 0 {
            off
        } else if rng <= 0xFFFF_FFFF {
            if rng == 0xFFFF_FFFF {
                return off.wrapping_add(self.mt.next_u32() as u64);
            }
            let rng32 = rng as u32;
            let mask = gen_mask(rng) as u32;
            loop {
                let val = self.mt.next_u32() & mask;
                if val <= rng32 {
                    return off.wrapping_add(val as u64);
                }
            }
        } else if rng == u64::MAX {
            off.wrapping_add(self.mt.next_u64())
        } else {
            let mask = gen_mask(rng);
            loop {
                let val = self.mt.next_u64() & mask;
                if val <= rng {
                    return off.wrapping_add(val);
                }
            }
        }
    }

    /// `randint(low, high)` with the default (int64) dtype: uniform on `[low, high)`.
    #[inline]
    pub fn randint(&mut self, low: i64, high: i64) -> i64 {
        assert!(low < high, "randint: low >= high");
        let rng = (high as i128 - 1 - low as i128) as u64;
        self.bounded_u64(low as u64, rng) as i64
    }

    /// `randint(low, high, dtype=np.uint64)`: uniform on `[low, high)`.
    #[inline]
    pub fn randint_u64(&mut self, low: u64, high: u64) -> u64 {
        assert!(low < high, "randint: low >= high");
        self.bounded_u64(low, high - 1 - low)
    }

    /// `standard_normal()` = `legacy_gauss`: Marsaglia polar method, second value cached.
    #[inline]
    pub fn standard_normal(&mut self) -> f64 {
        if self.has_gauss {
            let g = self.gauss;
            self.has_gauss = false;
            self.gauss = 0.0;
            return g;
        }
        loop {
            // Draw first, then transform the pair so the independent arithmetic can be
            // scheduled together. The stream and the polar method's rejection order stay exact.
            let u1 = self.mt.next_f64();
            let u2 = self.mt.next_f64();
            let x1 = 2.0 * u1 - 1.0;
            let x2 = 2.0 * u2 - 1.0;
            let r2 = x1 * x1 + x2 * x2;
            if r2 < 1.0 && r2 != 0.0 {
                let f = (-2.0 * r2.ln() / r2).sqrt();
                self.gauss = f * x1;
                self.has_gauss = true;
                return f * x2;
            }
        }
    }

    /// `normal(loc, scale)` = `loc + scale * gauss`. Consumes a draw even when `scale == 0`.
    #[inline]
    pub fn normal(&mut self, loc: f64, scale: f64) -> f64 {
        loc + scale * self.standard_normal()
    }

    /// `lognormal(mean, sigma)` = `exp(normal(mean, sigma))`.
    #[inline]
    pub fn lognormal(&mut self, mean: f64, sigma: f64) -> f64 {
        self.normal(mean, sigma).exp()
    }

    /// `uniform(low, high)` = `low + (high - low) * U`. Does not touch the gaussian cache.
    #[inline]
    pub fn uniform(&mut self, low: f64, high: f64) -> f64 {
        let range = high - low;
        low + range * self.mt.next_f64()
    }

    /// `standard_exponential()` = `-log(1 - U)`.
    #[inline]
    pub fn standard_exponential(&mut self) -> f64 {
        -(1.0 - self.mt.next_f64()).ln()
    }

    /// `exponential(scale)` = `scale * standard_exponential()`.
    #[inline]
    pub fn exponential(&mut self, scale: f64) -> f64 {
        scale * self.standard_exponential()
    }

    /// `pareto(a)` = `exp(standard_exponential() / a) - 1` (Lomax / Pareto II). The legacy stream
    /// keeps `exp(x) - 1`; only the new `Generator` switched to `expm1`, and the two differ by up to
    /// tens of thousands of ULP for small `x`.
    #[inline]
    pub fn pareto(&mut self, a: f64) -> f64 {
        (self.standard_exponential() / a).exp() - 1.0
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn states_seeded_together_draw_as_states_seeded_alone() {
        let seeds: Vec<u32> = (0..21u32).map(|i| i.wrapping_mul(0x9e37_79b9) ^ 0x5bd1_e995).collect();
        for (mut many, &seed) in RandomState::new_many(&seeds).into_iter().zip(&seeds) {
            let mut one = RandomState::new(seed);
            for _ in 0..400 {
                assert_eq!(many.standard_normal().to_bits(), one.standard_normal().to_bits());
                assert_eq!(many.randint(0, 7), one.randint(0, 7));
            }
        }
    }
}
