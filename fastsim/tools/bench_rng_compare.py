#!/usr/bin/env python3
"""Benchmark old/current lognormal sampling in separately compiled copies of one harness."""

from __future__ import annotations

import argparse
import hashlib
import json
import statistics
import subprocess
import tempfile
from pathlib import Path

ROOT = Path(__file__).resolve().parents[1]
HARNESS = r'''
#![allow(dead_code)]
mod rng {
    #[cfg(baseline)] #[path=OLD_MT] pub mod mt19937;
    #[cfg(not(baseline))] #[path=NEW_MT] pub mod mt19937;
    #[cfg(baseline)] #[path=OLD_RS] pub mod random_state;
    #[cfg(not(baseline))] #[path=NEW_RS] pub mod random_state;
}
use std::hint::black_box;
use std::time::Instant;
#[inline(never)]
fn measure() -> (f64, u64, usize) {
    let mut rs = rng::random_state::RandomState::new(42);
    let start = Instant::now();
    let mut last = 0.0;
    for _ in 0..5_000_000 {
        last = black_box(rs.lognormal(8.517193191416238, 0.75));
    }
    (start.elapsed().as_secs_f64() * 1000.0, last.to_bits(), rs.state().1)
}
fn main() {
    for i in 0..8 {
        let (ms, bits, pos) = measure();
        println!("{i} {ms} {bits} {pos}");
    }
}
'''


def main() -> None:
    ap = argparse.ArgumentParser(description=__doc__)
    ap.add_argument("--baseline", required=True, help="git commit containing the original RNG")
    ap.add_argument("--json", type=Path)
    args = ap.parse_args()
    commit = subprocess.check_output(["git", "rev-parse", "--verify", args.baseline], cwd=ROOT, text=True).strip()
    samples: dict[str, list[float]] = {key: [] for key in ("baseline", "candidate")}
    outputs: dict[str, list[str]] = {key: [] for key in samples}
    checks = set()
    hashes = {}
    with tempfile.TemporaryDirectory(prefix="fastsim-rng-bench-") as temp:
        folder = Path(temp)
        source = HARNESS
        for label, key in [("mt19937.rs", "MT"), ("random_state.rs", "RS")]:
            old = folder / f"old-{label}"
            old.write_bytes(subprocess.check_output(["git", "show", f"{commit}:src/rng/{label}"], cwd=ROOT))
            new = ROOT / "src/rng" / label
            source = source.replace(f"OLD_{key}", json.dumps(str(old), ensure_ascii=False))
            source = source.replace(f"NEW_{key}", json.dumps(str(new), ensure_ascii=False))
            hashes[label] = {"baseline": hashlib.sha256(old.read_bytes()).hexdigest(),
                             "candidate": hashlib.sha256(new.read_bytes()).hexdigest()}
        harness = folder / "rng_harness.rs"
        harness.write_text(source)
        for key in samples:
            cmd = ["rustc", "--edition=2024", "-C", "opt-level=3", "-C", "lto=fat",
                   "-C", "codegen-units=1", "-C", "panic=abort", str(harness), "-o", str(folder / key)]
            if key == "baseline":
                cmd.extend(["--cfg", "baseline"])
            subprocess.run(cmd, check=True)
        for round_index in range(3):
            order = ["baseline", "candidate"] if round_index % 2 == 0 else ["candidate", "baseline"]
            for key in order:
                output = subprocess.check_output([str(folder / key)], text=True)
                outputs[key].append(output)
                for line in output.splitlines():
                    run, ms, bits, pos = line.split()
                    checks.add((bits, pos))
                    if int(run):
                        samples[key].append(float(ms))
    if len(checks) != 1:
        raise RuntimeError("RNG benchmark result/state differs; run the full correctness tests")
    medians = {key: statistics.median(values) for key, values in samples.items()}
    result = {"baseline_commit": commit, "source_sha256": hashes,
              "distribution": "lognormal(mu=8.517193191416238, sigma=0.75)", "seed": 42,
              "draws_per_run": 5_000_000, "rounds": 3, "warmups_per_process": 1,
              "runs_per_process": 7, "times_ms": samples, "median_ms": medians,
              "speedup": medians["baseline"] / medians["candidate"],
              "last_draw_bits": next(iter(checks))[0], "final_mt_position": next(iter(checks))[1],
              "raw_output": outputs}
    print(f"5M lognormal draws: {medians['baseline']:.2f} -> {medians['candidate']:.2f} ms "
          f"({result['speedup']:.3f}x)")
    if args.json:
        args.json.write_text(json.dumps(result, indent=2) + "\n")


if __name__ == "__main__":
    main()
