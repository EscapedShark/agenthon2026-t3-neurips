//! MT19937 exactly as NumPy implements it (`numpy/random/src/mt19937/mt19937.{h,c}`).
//!
//! Only the parts `RandomState` touches: `init_genrand`-style integer seeding, `init_by_array`
//! seeding, the 32-bit tempered output, and NumPy's 53-bit double construction.

pub const N: usize = 624;
const M: usize = 397;
const MATRIX_A: u32 = 0x9908_b0df;
const UPPER_MASK: u32 = 0x8000_0000;
const LOWER_MASK: u32 = 0x7fff_ffff;

#[derive(Clone)]
pub struct Mt19937 {
    key: [u32; N],
    pos: usize,
}

impl Mt19937 {
    /// `mt19937_seed`: what `RandomState(seed)` does for an integer seed in `[0, 2**32)`.
    pub fn new(seed: u32) -> Self {
        let mut key = [0u32; N];
        let mut s = seed;
        for (pos, k) in key.iter_mut().enumerate() {
            *k = s;
            s = 1_812_433_253u32
                .wrapping_mul(s ^ (s >> 30))
                .wrapping_add(pos as u32 + 1);
        }
        Self { key, pos: N }
    }

    /// [`Mt19937::new`] for every seed of `seeds`, written in place: `state(i)` is where seed
    /// `i`'s generator goes. One seed's recurrence is a chain of 624 dependent multiplies; eight
    /// independent chains side by side keep the multiplier busy instead of waiting on each result.
    ///
    /// # Safety
    /// `state(i)` must be valid for writes of an `Mt19937` for every `i < seeds.len()`, and the
    /// places must not overlap; they may be uninitialized (they are fully written).
    pub unsafe fn seed_many(seeds: &[u32], state: impl Fn(usize) -> *mut Mt19937) {
        const LANES: usize = 8;
        let full = seeds.len() / LANES * LANES;
        for first in (0..full).step_by(LANES) {
            let mut s: [u32; LANES] = std::array::from_fn(|lane| seeds[first + lane]);
            // SAFETY: the caller's places, each written once per position.
            let keys: [*mut u32; LANES] = std::array::from_fn(|lane| unsafe { (&raw mut (*state(first + lane)).key).cast::<u32>() });
            for (pos, column) in (0..N).zip(1u32..) {
                for lane in 0..LANES {
                    unsafe { keys[lane].add(pos).write(s[lane]) };
                    s[lane] = 1_812_433_253u32.wrapping_mul(s[lane] ^ (s[lane] >> 30)).wrapping_add(column);
                }
            }
        }
        for (i, &seed) in seeds.iter().enumerate().skip(full) {
            // SAFETY: as above.
            unsafe { state(i).write(Mt19937::new(seed)) };
        }
        for i in 0..full {
            unsafe { (&raw mut (*state(i)).pos).write(N) };
        }
    }

    /// `mt19937_init_by_array`: what `RandomState(seed)` does for a 1-d integer array seed.
    pub fn from_key(init_key: &[u32]) -> Self {
        assert!(!init_key.is_empty(), "seed array must be non-empty");
        let mut st = Self::new(19_650_218);
        let mt = &mut st.key;
        let key_len = init_key.len();
        let (mut i, mut j) = (1usize, 0usize);
        for _ in 0..N.max(key_len) {
            mt[i] = (mt[i] ^ (mt[i - 1] ^ (mt[i - 1] >> 30)).wrapping_mul(1_664_525))
                .wrapping_add(init_key[j])
                .wrapping_add(j as u32);
            i += 1;
            j += 1;
            if i >= N {
                mt[0] = mt[N - 1];
                i = 1;
            }
            if j >= key_len {
                j = 0;
            }
        }
        for _ in 0..N - 1 {
            mt[i] = (mt[i] ^ (mt[i - 1] ^ (mt[i - 1] >> 30)).wrapping_mul(1_566_083_941))
                .wrapping_sub(i as u32);
            i += 1;
            if i >= N {
                mt[0] = mt[N - 1];
                i = 1;
            }
        }
        mt[0] = 0x8000_0000;
        st
    }

    #[cold]
    #[inline(never)]
    fn generate(&mut self) {
        let key = &mut self.key;
        #[inline(always)]
        fn twist(a: u32, b: u32, far: u32) -> u32 {
            let y = (a & UPPER_MASK) | (b & LOWER_MASK);
            far ^ (y >> 1) ^ ((y & 1).wrapping_neg() & MATRIX_A)
        }
        for i in 0..N - M {
            key[i] = twist(key[i], key[i + 1], key[i + M]);
        }
        for i in N - M..N - 1 {
            key[i] = twist(key[i], key[i + 1], key[i + M - N]);
        }
        key[N - 1] = twist(key[N - 1], key[0], key[M - 1]);
        self.pos = 0;
    }

    /// `mt19937_next`: one tempered 32-bit output.
    #[inline(always)]
    pub fn next_u32(&mut self) -> u32 {
        if self.pos == N {
            self.generate();
        }
        let y = self.key[self.pos];
        self.pos += 1;
        Self::temper(y)
    }

    #[inline(always)]
    fn temper(mut y: u32) -> u32 {
        y ^= y >> 11;
        y ^= (y << 7) & 0x9d2c_5680;
        y ^= (y << 15) & 0xefc6_0000;
        y ^ (y >> 18)
    }

    /// Consecutive words share a state-boundary check and position update. The fixed-size
    /// tempering operations can also be vectorized, without changing MT's raw state or draws.
    #[inline(always)]
    fn next_u32_pair(&mut self) -> [u32; 2] {
        if self.pos < N - 1 {
            let words = std::array::from_fn(|i| Self::temper(self.key[self.pos + i]));
            self.pos += 2;
            words
        } else {
            // A draw may straddle a twist, including after an odd number of integer draws.
            std::array::from_fn(|_| self.next_u32())
        }
    }

    /// `mt19937_next64`: the first 32-bit draw is the HIGH word (verified against NumPy by the
    /// golden tests; the C source leaves the operand order to the compiler).
    #[inline(always)]
    pub fn next_u64(&mut self) -> u64 {
        let [hi, lo] = self.next_u32_pair();
        ((hi as u64) << 32) | lo as u64
    }

    /// `mt19937_next_double`: 27 + 26 bits -> a double in `[0, 1)`.
    #[inline(always)]
    pub fn next_f64(&mut self) -> f64 {
        let [a, b] = self.next_u32_pair();
        Self::double(a, b)
    }

    #[inline(always)]
    fn double(a: u32, b: u32) -> f64 {
        // All 53 bits fit exactly in f64: combine before converting, saving one conversion
        // and the floating-point multiply/add in NumPy's equivalent scalar expression.
        let bits = ((a >> 5) as u64) << 26 | (b >> 6) as u64;
        bits as f64 / 9_007_199_254_740_992.0
    }

    /// The raw state as `RandomState.get_state()` reports it: `(key, pos)`.
    pub fn state(&self) -> (&[u32; N], usize) {
        (&self.key, self.pos)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn scalar_double(rng: &mut Mt19937) -> f64 {
        let a = (rng.next_u32() >> 5) as f64;
        let b = (rng.next_u32() >> 6) as f64;
        (a * 67_108_864.0 + b) / 9_007_199_254_740_992.0
    }

    #[test]
    fn seeding_many_matches_seeding_each() {
        let seeds: Vec<u32> = (0..19u32).map(|i| i.wrapping_mul(2_654_435_761) ^ (i << 7)).chain([0, u32::MAX]).collect();
        for n in [0, 1, 7, 8, 9, 16, seeds.len()] {
            let mut many: Vec<std::mem::MaybeUninit<Mt19937>> = (0..n).map(|_| std::mem::MaybeUninit::uninit()).collect();
            let base = many.as_mut_ptr();
            // SAFETY: one distinct element per seed.
            unsafe { Mt19937::seed_many(&seeds[..n], |i| base.add(i).cast()) };
            for (state, &seed) in many.iter_mut().zip(&seeds) {
                // SAFETY: seeded above.
                let state = unsafe { state.assume_init_mut() };
                let mut one = Mt19937::new(seed);
                assert_eq!(state.key, one.key);
                assert_eq!(state.pos, one.pos);
                for _ in 0..700 {
                    assert_eq!(state.next_u32(), one.next_u32());
                }
            }
        }
    }

    #[test]
    fn paired_draws_match_scalar_at_every_twist_offset() {
        for seed in [0, 17, u32::MAX] {
            let mut start = Mt19937::new(seed);
            for offset in 0..=N {
                let mut grouped = start.clone();
                let mut scalar = start.clone();
                for _ in 0..33 {
                    assert_eq!(grouped.next_u32(), scalar.next_u32());
                    assert_eq!(
                        grouped.next_f64().to_bits(),
                        scalar_double(&mut scalar).to_bits()
                    );
                    assert_eq!(
                        grouped.next_f64().to_bits(),
                        scalar_double(&mut scalar).to_bits()
                    );
                    let hi = scalar.next_u32() as u64;
                    let lo = scalar.next_u32() as u64;
                    assert_eq!(grouped.next_u64(), (hi << 32) | lo);
                }
                assert_eq!(
                    grouped.state(),
                    scalar.state(),
                    "seed {seed}, offset {offset}"
                );
                start.next_u32();
            }
        }
    }
}
