//! Replays NumPy-generated golden scripts (tools/gen_rng_golden.py) against the Rust port and
//! demands bit equality: integers exactly, doubles by IEEE-754 bit pattern, MT state exactly.
//!
//! Every `tests/data/rng_golden*.json` is checked, or only `$RNG_GOLDEN` when it is set.
//!
//! The golden data comes from NumPy 1.26.4 on linux/amd64, the stack that generated the reference
//! traces. Doubles that pass through `log`/`exp` depend on the platform libm, so bit equality is
//! demanded only on Linux x86-64 (glibc, as in the scoring image) or when `RNG_GOLDEN_STRICT=1`.
//! Elsewhere a double within 1e-12 relative error counts as libm drift and is only reported;
//! integers and MT state are exact everywhere.

fn strict() -> bool {
    cfg!(all(target_os = "linux", target_arch = "x86_64"))
        || std::env::var("RNG_GOLDEN_STRICT").is_ok_and(|v| v == "1")
}

use std::collections::BTreeMap;
use std::path::PathBuf;

use fastsim::rng::RandomState;
use serde_json::Value;

enum Op {
    RandomSample,
    Randint(i64, i64),
    RandintU64(u64, u64),
    StandardNormal,
    Normal(f64, f64),
    Lognormal(f64, f64),
    Uniform(f64, f64),
    StandardExponential,
    Exponential(f64),
    Pareto(f64),
}

enum Out {
    Int(i128),
    Float(f64),
}

fn parse_op(v: &Value) -> (String, Op) {
    let name = v["name"].as_str().unwrap().to_string();
    let i = |k: usize| v["i"][k].as_i64().unwrap();
    let u = |k: usize| v["i"][k].as_u64().unwrap();
    let f = |k: usize| f64::from_bits(v["f"][k].as_u64().unwrap());
    let op = match name.as_str() {
        "random_sample" => Op::RandomSample,
        "randint" => Op::Randint(i(0), i(1)),
        "randint_u64" => Op::RandintU64(u(0), u(1)),
        "standard_normal" => Op::StandardNormal,
        "normal" => Op::Normal(f(0), f(1)),
        "lognormal" => Op::Lognormal(f(0), f(1)),
        "uniform" => Op::Uniform(f(0), f(1)),
        "standard_exponential" => Op::StandardExponential,
        "exponential" => Op::Exponential(f(0)),
        "pareto" => Op::Pareto(f(0)),
        other => panic!("unknown op {other}"),
    };
    let label = format!("{name}{:?}{:?}", v["i"], v["f"]);
    (label, op)
}

fn apply(rs: &mut RandomState, op: &Op) -> Out {
    match *op {
        Op::RandomSample => Out::Float(rs.random_sample()),
        Op::Randint(lo, hi) => Out::Int(rs.randint(lo, hi) as i128),
        Op::RandintU64(lo, hi) => Out::Int(rs.randint_u64(lo, hi) as i128),
        Op::StandardNormal => Out::Float(rs.standard_normal()),
        Op::Normal(a, b) => Out::Float(rs.normal(a, b)),
        Op::Lognormal(a, b) => Out::Float(rs.lognormal(a, b)),
        Op::Uniform(a, b) => Out::Float(rs.uniform(a, b)),
        Op::StandardExponential => Out::Float(rs.standard_exponential()),
        Op::Exponential(s) => Out::Float(rs.exponential(s)),
        Op::Pareto(a) => Out::Float(rs.pareto(a)),
    }
}

fn golden_files() -> Vec<PathBuf> {
    if let Ok(p) = std::env::var("RNG_GOLDEN") {
        return vec![PathBuf::from(p)];
    }
    let dir = PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("tests/data");
    let mut files: Vec<PathBuf> = std::fs::read_dir(&dir)
        .expect("tests/data missing: run tools/gen_rng_golden.py")
        .filter_map(|e| {
            let p = e.ok()?.path();
            let name = p.file_name()?.to_str()?;
            (name.starts_with("rng_golden") && name.ends_with(".json")).then_some(p)
        })
        .collect();
    files.sort();
    assert!(!files.is_empty(), "no golden files in {}", dir.display());
    files
}

fn check_state(rs: &RandomState, want: &Value, what: &str) -> usize {
    let (key, pos, _, _) = rs.state();
    let want_key: Vec<u32> = want["key"]
        .as_array()
        .unwrap()
        .iter()
        .map(|k| k.as_u64().unwrap() as u32)
        .collect();
    let mut bad = 0;
    if key.as_slice() != want_key.as_slice() {
        eprintln!("  STATE MISMATCH {what}: key differs");
        bad += 1;
    }
    if pos as u64 != want["pos"].as_u64().unwrap() {
        eprintln!("  STATE MISMATCH {what}: pos {pos} != {}", want["pos"]);
        bad += 1;
    }
    bad
}

fn check_file(path: &PathBuf) -> usize {
    let doc: Value = serde_json::from_str(&std::fs::read_to_string(path).unwrap()).unwrap();
    eprintln!(
        "golden {} (numpy {}, {})",
        path.file_name().unwrap().to_string_lossy(),
        doc["numpy_version"],
        doc["platform"]
    );
    let ops: Vec<(String, Op)> = doc["ops"].as_array().unwrap().iter().map(parse_op).collect();
    let mut failures = 0usize;

    for st in doc["seed_states"].as_array().unwrap() {
        let seed = st["seed"].as_u64().unwrap() as u32;
        failures += check_state(&RandomState::new(seed), st, &format!("seed {seed}"));
    }
    for st in doc["array_seed_states"].as_array().unwrap() {
        let key: Vec<u32> = st["seed_array"]
            .as_array()
            .unwrap()
            .iter()
            .map(|k| k.as_u64().unwrap() as u32)
            .collect();
        let what = format!("array seed of len {}", key.len());
        failures += check_state(&RandomState::from_key(&key), st, &what);
    }

    // The ABIDES global chain: seed -> u32 draw -> global exponential -> more u32 draws.
    for ch in doc["chains"].as_array().unwrap() {
        let mut g = RandomState::new(ch["seed"].as_u64().unwrap() as u32);
        let first = g.randint_u64(0, 1 << 32);
        let exp = g.exponential(1.0 / 2.77778e-18);
        let rest: Vec<u64> = (0..8).map(|_| g.randint_u64(0, 1 << 32)).collect();
        let want_rest: Vec<u64> =
            ch["rest"].as_array().unwrap().iter().map(|v| v.as_u64().unwrap()).collect();
        if first != ch["first"].as_u64().unwrap()
            || exp.to_bits() != ch["exp_bits"].as_u64().unwrap()
            || rest != want_rest
        {
            eprintln!("  CHAIN MISMATCH for seed {}", ch["seed"]);
            failures += 1;
        }
    }

    // Per-op tallies: (checked, mismatched, max ulp distance for doubles).
    let mut tally: BTreeMap<&str, (usize, usize, u64)> = BTreeMap::new();
    let mut shown = 0usize;
    let strict = strict();
    let mut drift = 0usize;
    for run in doc["runs"].as_array().unwrap() {
        let seed = run["seed"].as_u64().unwrap() as u32;
        let mut rs = RandomState::new(seed);
        for (n, rec) in run["records"].as_array().unwrap().iter().enumerate() {
            let idx = rec[0].as_u64().unwrap() as usize;
            let (label, op) = &ops[idx];
            let entry = tally.entry(label.as_str()).or_insert((0, 0, 0));
            entry.0 += 1;
            let (ok, ulp) = match apply(&mut rs, op) {
                Out::Int(got) => {
                    let want = rec[1].as_i64().map(|v| v as i128).unwrap_or_else(|| {
                        rec[1].as_u64().unwrap() as i128
                    });
                    (got == want, 0)
                }
                Out::Float(got) => {
                    let want = rec[1].as_u64().unwrap();
                    let d = (got.to_bits() as i128 - want as i128).unsigned_abs() as u64;
                    let w = f64::from_bits(want);
                    if d != 0 && !strict && ((got - w) / w).abs() <= 1e-12 {
                        drift += 1;
                        (true, 0)
                    } else {
                        (d == 0, d)
                    }
                }
            };
            if !ok {
                entry.1 += 1;
                entry.2 = entry.2.max(ulp);
                if shown < 10 {
                    eprintln!("  MISMATCH seed {seed} record {n} op {label}: {rec:?} (ulp {ulp})");
                    shown += 1;
                }
            }
        }
        let (_, pos, has_gauss, gauss) = rs.state();
        let want_gauss = f64::from_bits(run["final_gauss_bits"].as_u64().unwrap());
        let gauss_ok = gauss.to_bits() == want_gauss.to_bits()
            || (!strict && (gauss - want_gauss).abs() <= 1e-12 * want_gauss.abs());
        if pos as u64 != run["final_pos"].as_u64().unwrap()
            || has_gauss != run["final_has_gauss"].as_bool().unwrap()
            || !gauss_ok
        {
            eprintln!("  FINAL STATE MISMATCH seed {seed}");
            failures += 1;
        }
    }
    let mut total = 0;
    for (label, (n, bad, ulp)) in &tally {
        total += n;
        let flag = if *bad > 0 { "  <-- MISMATCH" } else { "" };
        eprintln!("  {n:6} x {label:70} bad={bad} max_ulp={ulp}{flag}");
        failures += bad;
    }
    let mode = if strict { "strict" } else { "libm-tolerant" };
    eprintln!("  {total} records, {failures} failures, {drift} last-bit libm drifts ({mode})");
    failures
}

#[test]
fn numpy_legacy_random_state_is_bit_exact() {
    let failures: usize = golden_files().iter().map(check_file).sum();
    assert_eq!(failures, 0, "RandomState port diverges from NumPy");
}
