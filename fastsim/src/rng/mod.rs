//! Bit-exact reproduction of NumPy's legacy random stream.

pub mod mt19937;
pub mod random_state;

pub use mt19937::Mt19937;
pub use random_state::RandomState;

/// Python's `round()` on a float: round half to even (banker's rounding).
#[inline(always)]
pub fn py_round(x: f64) -> f64 {
    x.round_ties_even()
}
