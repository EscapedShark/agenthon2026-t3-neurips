//! Dev-only timing: medians after one warm-up, in-process (no file writes or process startup).
//!   bench <scenario.json> [runs] [workers] [ledger: 0|1]
//! Reports row collection and the streaming path separately. Their difference is not an encoding
//! tail: buffer reuse, threading and latency lookahead change the simulation's own cost.

use std::time::Instant;

use fastsim::output;
use fastsim::scenario::Scenario;
use fastsim::sim::{engine::Sim, trace::{LedgerBuilder, RawTrace}};

fn median(samples: &mut [f64]) -> f64 {
    samples.sort_unstable_by(f64::total_cmp);
    let mid = samples.len() / 2;
    if samples.len() % 2 == 0 { (samples[mid - 1] + samples[mid]) / 2.0 } else { samples[mid] }
}

fn main() {
    let args: Vec<String> = std::env::args().collect();
    let path = args.get(1).expect("usage: bench <scenario.json> [runs] [workers] [ledger]");
    let runs: usize = args.get(2).map_or(5, |r| r.parse().expect("runs"));
    let workers: usize = args.get(3).map_or(3, |w| w.parse().expect("workers"));
    let ledger = args.get(4).is_none_or(|l| l != "0");
    assert!(runs > 0, "runs must be positive");
    let sc = Scenario::from_json(&std::fs::read_to_string(path).expect("read"));
    let (mut sim, mut full) = (Vec::new(), Vec::new());
    let mut rows = (0, 0, 0);
    for run in 0..=runs {
        let t = Instant::now();
        let engine = Sim::new(&sc, RawTrace::new(), LedgerBuilder::new(ledger));
        let mut engine = if workers >= 2 { engine.with_latency_prefetch(true) } else { engine };
        let out = engine.run();
        drop(engine);
        let s = t.elapsed().as_secs_f64();
        std::hint::black_box(&out);
        drop(out);
        let t = Instant::now();
        let o = output::simulate(&sc, ledger, workers);
        let f = t.elapsed().as_secs_f64();
        rows = (o.trace_rows, o.ledger_rows, o.trace.len() + o.ledger.as_ref().map_or(0, |f| f.len()));
        std::hint::black_box(o);
        if run > 0 {
            sim.push(s);
            full.push(f);
        }
    }
    let (sim, full) = (median(&mut sim), median(&mut full));
    println!(
        "{path}: {} rows, {} msgs, {:.1} MB parquet | medians of {runs} | sim only {:.1} ms | \
         simulate+encode ({workers} spare CPUs, ledger {ledger}) {:.1} ms => {:.2}M events/s",
        rows.0, rows.1, rows.2 as f64 / 1e6, sim * 1e3, full * 1e3, rows.0 as f64 / full / 1e6
    );
}
