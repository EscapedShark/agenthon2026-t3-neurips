//! Latency parameters compiled once per simulation. Keep NumPy's arithmetic, clamping and
//! ties-to-even rounding, and consume the same random draws even for degenerate distributions.

use crate::rng::{RandomState, py_round};
use crate::scenario::{Latency, LatencyModel};

#[derive(Clone)]
pub(super) enum LatencyImpl {
    LogNormal {
        mu: f64,
        sigma: f64,
        min_ns: f64,
        max_ns: f64,
    },
    Uniform {
        min_ns: f64,
        range: f64,
        max_ns: f64,
    },
    Pareto {
        base: f64,
        alpha: f64,
        min_ns: f64,
        max_ns: f64,
    },
    Constant(i64),
    Pairwise {
        positions: Vec<f64>,
        exchange_latencies: Vec<i64>,
    },
}

impl LatencyImpl {
    pub fn scenario(lat: &Latency) -> Self {
        let (min_ns, max_ns) = (lat.min_ns, lat.max_ns);
        match lat.model {
            LatencyModel::LogNormal { mu, sigma } => Self::LogNormal {
                mu,
                sigma,
                min_ns,
                max_ns,
            },
            LatencyModel::Uniform => Self::Uniform {
                min_ns,
                range: max_ns - min_ns,
                max_ns,
            },
            LatencyModel::Pareto { base, alpha } => Self::Pareto {
                base,
                alpha,
                min_ns,
                max_ns,
            },
            LatencyModel::Constant(value) => {
                Self::Constant(Self::nanoseconds(value, min_ns, max_ns))
            }
        }
    }

    /// ABIDES's default: uniform positions on a Seattle–NYC line and fixed light-speed links.
    pub fn pairwise(n: usize, seed: u32) -> Self {
        let mut rs = RandomState::new(seed);
        let positions: Vec<f64> = (0..n).map(|_| rs.uniform(0.0, 3_866_660.0)).collect();
        let exchange_latencies = positions
            .iter()
            .map(|&x| Self::distance(positions[0], x))
            .collect();
        Self::Pairwise {
            positions,
            exchange_latencies,
        }
    }

    #[inline]
    fn distance(a: f64, b: f64) -> i64 {
        ((a - b).abs() / 299_792_458e-9).trunc() as i64
    }

    #[inline]
    fn nanoseconds(value: f64, min_ns: f64, max_ns: f64) -> i64 {
        py_round(value.max(min_ns).min(max_ns)) as i64
    }

    #[inline]
    pub fn draw(&self, sender: u32, recipient: u32, rng: &mut RandomState) -> i64 {
        if sender == recipient {
            return 0;
        }
        match *self {
            Self::LogNormal {
                mu,
                sigma,
                min_ns,
                max_ns,
            } => Self::nanoseconds(rng.lognormal(mu, sigma), min_ns, max_ns),
            Self::Uniform {
                min_ns,
                range,
                max_ns,
            } => Self::nanoseconds(min_ns + range * rng.random_sample(), min_ns, max_ns),
            Self::Pareto {
                base,
                alpha,
                min_ns,
                max_ns,
            } => {
                // Keep exp(x)-1 followed by 1+..., even though algebra could cancel them:
                // floating-point cancellation differs and can change the rounded nanosecond.
                Self::nanoseconds(base * (1.0 + rng.pareto(alpha)), min_ns, max_ns)
            }
            Self::Constant(value) => value,
            Self::Pairwise {
                ref positions,
                ref exchange_latencies,
            } => {
                if sender == 0 {
                    exchange_latencies[recipient as usize]
                } else if recipient == 0 {
                    exchange_latencies[sender as usize]
                } else {
                    Self::distance(positions[sender as usize], positions[recipient as usize])
                }
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn compiled_distributions_preserve_samples_and_complete_rng_state() {
        let models = [
            LatencyModel::LogNormal {
                mu: 8.517193191416238,
                sigma: 0.75,
            },
            LatencyModel::LogNormal {
                mu: 0.0,
                sigma: 0.0,
            },
            LatencyModel::LogNormal {
                mu: 1e3,
                sigma: 12.0,
            },
            LatencyModel::Uniform,
            LatencyModel::Pareto {
                base: 100.0,
                alpha: 1.5,
            },
            LatencyModel::Pareto {
                base: 1.0,
                alpha: 1e10,
            },
            LatencyModel::Constant(-1.5),
            LatencyModel::Constant(0.0),
            LatencyModel::Constant(0.5),
            LatencyModel::Constant(1.5),
            LatencyModel::Constant(2.5),
            LatencyModel::Constant(f64::INFINITY),
            LatencyModel::Constant(f64::NAN),
        ];
        for model in models {
            for (min_ns, max_ns) in [
                (0.0, 1e12),
                (500.25, 501.5),
                (5.5, 5.5),
                (-1e12, 1e12),
                (1e3, 0.0),
            ] {
                let lat = Latency {
                    model: model.clone(),
                    min_ns,
                    max_ns,
                };
                let compiled = LatencyImpl::scenario(&lat);
                for seed in [0, 17, u32::MAX] {
                    let mut rng = RandomState::new(seed);
                    let mut reference = rng.clone();
                    for i in 0..4097 {
                        let recipient = if i % 7 == 0 { 0 } else { 1 };
                        let expected = if recipient == 0 {
                            0
                        } else {
                            lat.draw(&mut reference)
                        };
                        assert_eq!(
                            compiled.draw(0, recipient, &mut rng),
                            expected,
                            "{lat:?}, seed {seed}, draw {i}"
                        );
                    }
                    let (key, pos, has_gauss, gauss) = rng.state();
                    let (ref_key, ref_pos, ref_has_gauss, ref_gauss) = reference.state();
                    assert_eq!(key, ref_key);
                    assert_eq!(pos, ref_pos);
                    assert_eq!(has_gauss, ref_has_gauss);
                    assert_eq!(gauss.to_bits(), ref_gauss.to_bits());
                }
            }
        }
    }

    #[test]
    fn sparse_latency_matches_the_full_distance_matrix_without_consuming_draws() {
        for seed in [0, 17, u32::MAX] {
            for n in [1, 2, 17, 257] {
                let model = LatencyImpl::pairwise(n, seed);
                let mut positions_rng = RandomState::new(seed);
                let positions: Vec<_> = (0..n)
                    .map(|_| positions_rng.uniform(0.0, 3_866_660.0))
                    .collect();
                let mut send_rng = RandomState::new(123);
                let expected_next = send_rng.clone().next_u32();
                for i in 0..n {
                    for j in 0..n {
                        let expected =
                            ((positions[i] - positions[j]).abs() / 299_792_458e-9).trunc() as i64;
                        assert_eq!(model.draw(i as u32, j as u32, &mut send_rng), expected);
                    }
                }
                assert_eq!(send_rng.next_u32(), expected_next);
            }
        }
    }
}
