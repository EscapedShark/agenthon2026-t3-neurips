//! `abides_markets.oracles.SparseMeanRevertingOracle` (with the Track 3 scheduled-jump patch).
//!
//! The Python mixes ints and floats in ways that change results, so they are modelled explicitly:
//! the first megashock time is `mkt_open + float` (a float), every later one is `float + int` (still
//! a float), so the "previous time" alternates between int and float; `int - int` stays an exact
//! integer while any float operand makes the subtraction a double. `mkt_open` is an `np.int64`, so
//! the megashock times are `np.float64`, and NumPy compares one with an int (NumPy's or Python's) by
//! rounding the int to a double first: a time up to 128 ns after a megashock is not yet past it.
//! `exp`/`sqrt` are `math.exp`/`math.sqrt` (libm), and `theta**2` is libm `pow`.

use crate::rng::{py_round, RandomState};

/// A Python number that is either an `int` or a `float`.
#[derive(Clone, Copy, Debug)]
pub enum PyNum {
    I(i64),
    F(f64),
}

impl PyNum {
    #[inline(always)]
    fn as_f64(self) -> f64 {
        match self {
            PyNum::I(v) => v as f64,
            PyNum::F(v) => v,
        }
    }

    /// `a - b` with Python's int/float promotion.
    #[inline(always)]
    fn sub(self, other: PyNum) -> PyNum {
        match (self, other) {
            (PyNum::I(a), PyNum::I(b)) => PyNum::I(a - b),
            (a, b) => PyNum::F(a.as_f64() - b.as_f64()),
        }
    }
}

/// `f < i` for a megashock time and an int: NumPy rounds the int to a double.
#[inline(always)]
fn f_lt_i(f: f64, i: i64) -> bool {
    f < i as f64
}

/// `i <= n` for an int and a time that may be a megashock's.
#[inline(always)]
fn i_le(i: i64, n: PyNum) -> bool {
    match n {
        PyNum::I(v) => i <= v,
        PyNum::F(f) => i as f64 <= f,
    }
}

/// `n >= i`.
#[inline(always)]
fn ge_i(n: PyNum, i: i64) -> bool {
    match n {
        PyNum::I(v) => v >= i,
        PyNum::F(f) => f >= i as f64,
    }
}

pub struct Oracle {
    r_bar: i64,
    kappa: f64,
    theta_sq: f64,
    megashock_scale: f64,
    megashock_mean: f64,
    megashock_std: f64,
    /// The oracle's own RandomState (`symbols[..]["random_state"]`).
    rng: RandomState,
    /// NumPy's global generator: later megashock inter-arrivals come from `np.random.exponential`.
    global: RandomState,
    pt: PyNum,
    pv: i64,
    mst: f64,
    msv: f64,
    jump: Option<(i64, i64, bool)>,
    mkt_close: i64,
}

pub struct OracleParams {
    pub mkt_open: i64,
    pub mkt_close: i64,
    pub r_bar: i64,
    pub kappa: f64,
    pub fund_vol: f64,
    pub megashock_lambda: f64,
    pub megashock_mean: f64,
    pub megashock_var: f64,
    /// Absolute kernel time and signed magnitude.
    pub scheduled_jump: Option<(i64, i64)>,
}

impl Oracle {
    /// `__init__`, given the oracle's seed, the constructor's global exponential draw and the
    /// global generator as it stands after `build_config`.
    pub fn new(p: &OracleParams, seed: u32, first_megashock_delta: f64, global: RandomState) -> Oracle {
        let mut rng = RandomState::new(seed);
        let megashock_std = p.megashock_var.sqrt();
        let mut msv = rng.normal(p.megashock_mean, megashock_std);
        if rng.randint(0, 2) != 0 {
            msv = -msv;
        }
        // theta**2 goes through libm pow in Python; keep the call opaque so LLVM cannot rewrite it
        // to theta*theta.
        let theta_sq = p.fund_vol.powf(std::hint::black_box(2.0));
        Oracle {
            r_bar: p.r_bar,
            kappa: p.kappa,
            theta_sq,
            megashock_scale: 1.0 / p.megashock_lambda,
            megashock_mean: p.megashock_mean,
            megashock_std,
            rng,
            global,
            pt: PyNum::I(p.mkt_open),
            pv: p.r_bar,
            mst: p.mkt_open as f64 + first_megashock_delta,
            msv,
            jump: p.scheduled_jump.map(|(t, m)| (t, m, false)),
            mkt_close: p.mkt_close,
        }
    }

    /// `compute_fundamental_at_timestamp(ts, v_adj, symbol, pt, pv)`.
    fn compute(&mut self, ts: PyNum, v_adj: f64, pt: PyNum, pv: i64) -> i64 {
        let d = ts.sub(pt).as_f64();
        let gamma = self.kappa;
        let loc = self.r_bar as f64 + (pv - self.r_bar) as f64 * (-gamma * d).exp();
        let scale = ((self.theta_sq / (2.0 * gamma)) * (1.0 - (-2.0 * gamma * d).exp())).sqrt();
        let v = self.rng.normal(loc, scale) + v_adj;
        // max(0, v) then int(round(v))
        let mut v = if v > 0.0 { py_round(v) as i64 } else { 0 };
        if let Some((time_ns, magnitude, consumed)) = self.jump.as_mut() {
            if !*consumed && ge_i(ts, *time_ns) {
                v = (v + *magnitude).max(0);
                *consumed = true;
            }
        }
        self.pt = ts;
        self.pv = v;
        v
    }

    /// `advance_fundamental_value_series(current_time, symbol)`.
    fn advance(&mut self, current_time: i64) -> i64 {
        let (mut pt, mut pv) = (self.pt, self.pv);
        if i_le(current_time, pt) {
            return pv;
        }
        while f_lt_i(self.mst, current_time) {
            let (mst, msv) = (self.mst, self.msv);
            let v = self.compute(PyNum::F(mst), msv, pt, pv);
            pt = PyNum::F(mst);
            pv = v;
            // pt + int(np.random.exponential(scale=1.0 / lambda)): float + int -> float
            let delta = self.global.exponential(self.megashock_scale).trunc();
            self.mst = mst + delta;
            let mut msv = self.rng.normal(self.megashock_mean, self.megashock_std);
            if self.rng.randint(0, 2) != 0 {
                msv = -msv;
            }
            self.msv = msv;
        }
        self.compute(PyNum::I(current_time), 0.0, pt, pv)
    }

    /// `observe_price(symbol, current_time, random_state, sigma_n)`: the fundamental, plus
    /// `random_state.normal(r_t, sqrt(sigma_n))` when `sigma_n` is not zero. `standard_normal`
    /// yields that generator's next `standard_normal()` and is called only then
    /// (`normal(loc, scale)` is `loc + scale * standard_normal()`).
    pub fn observe_price(&mut self, current_time: i64, sigma_n: f64, standard_normal: impl FnOnce() -> f64) -> i64 {
        let r_t = if current_time >= self.mkt_close {
            self.advance(self.mkt_close - 1)
        } else {
            self.advance(current_time)
        };
        if sigma_n == 0.0 {
            r_t
        } else {
            let (loc, scale) = (r_t as f64, sigma_n.sqrt());
            py_round(loc + scale * standard_normal()) as i64
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    const MKT_OPEN: i64 = 1_612_517_400_000_000_000;

    /// The baseline's oracle: the global generator seeded, the first megashock delta drawn from it,
    /// the oracle's own generator.
    fn oracle(global_seed: u32, oracle_seed: u32, lambda: f64, kappa: f64, jump: Option<(i64, i64)>) -> Oracle {
        let mut global = RandomState::new(global_seed);
        let first_megashock_delta = global.exponential(1.0 / lambda);
        let p = OracleParams {
            mkt_open: MKT_OPEN,
            mkt_close: MKT_OPEN + 23_400_000_000_000,
            r_bar: 50_000,
            kappa,
            fund_vol: 5e-4,
            megashock_lambda: lambda,
            megashock_mean: 1000.0,
            megashock_var: 50_000.0,
            scheduled_jump: jump,
        };
        Oracle::new(&p, oracle_seed, first_megashock_delta, global)
    }

    /// Fundamentals from ABIDES's oracle (NumPy 1.26.4), observed 90, 128 and 300 ns after each of
    /// the first four megashocks. The megashock times are `np.float64`, and NumPy rounds an int time
    /// to a double before comparing: 90 ns after a megashock is not yet past it, 128 ns (a tie) only
    /// when the double rounds up. In `scheduled`, the jump time is 64 ns after the first megashock,
    /// so the jump applies at that megashock.
    #[test]
    fn megashock_times_compare_as_numpy_doubles() {
        let cases: [(Oracle, &[(i64, i64)]); 3] = [
            (
                oracle(7, 11, 2e-9, 1.67e-16, None),
                &[(1612517400039688538, 49999), (1612517400039688576, 48608), (1612517400039688748, 48608), (1612517400796567898, 48614), (1612517400796567936, 49541), (1612517400796568108, 49541), (1612517401085058906, 49538), (1612517401085058944, 48582), (1612517401085059116, 48582), (1612517401727768154, 48582), (1612517401727768192, 48582), (1612517401727768364, 49511)],
            ),
            (
                oracle(123, 456, 4e-9, 0.5e-9, None),
                &[(1612517400298068058, 49996), (1612517400298068096, 49996), (1612517400298068268, 50847), (1612517400382334810, 50809), (1612517400382334848, 49914), (1612517400382335020, 49914), (1612517400446655834, 49921), (1612517400446655872, 48968), (1612517400446656044, 48968), (1612517400647014234, 49066), (1612517400647014272, 49644), (1612517400647014444, 49644)],
            ),
            (
                oracle(7, 11, 2e-9, 1e-2, Some((1612517400039688512, -300))),
                &[(1612517400039688748, 49869), (1612517400796567898, 50000), (1612517400796567936, 50073), (1612517400796568108, 50013), (1612517401085058906, 50000), (1612517401085058944, 50085), (1612517401085059116, 50015), (1612517401727768154, 50000), (1612517401727768192, 50000), (1612517401727768364, 50087)],
            ),
        ];
        for (n, (mut oracle, expected)) in cases.into_iter().enumerate() {
            for (k, &(t, v)) in expected.iter().enumerate() {
                assert_eq!(oracle.observe_price(t, 0.0, || unreachable!()), v, "case {n}, observation {k} at {t}");
            }
        }
    }
}
