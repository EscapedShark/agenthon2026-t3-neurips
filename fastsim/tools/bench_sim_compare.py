#!/usr/bin/env python3
"""Compare the simulation timings reported by two bench executables, alternating order.

Each subprocess warms up once, then reports a median. Report the median of those
medians across rounds; encoding is outside the simulation timer in both versions.
Use bench_compare.py separately for complete-process timings.
"""

from __future__ import annotations

import argparse
import hashlib
import json
import platform
import re
import statistics
import subprocess
from pathlib import Path

ROOT = Path(__file__).resolve().parents[1]
UNITS = ROOT.parent / "track3-simulation-public" / "units"
DEFAULT_UNITS = [
    "t3-as06-throughput-fast",
    "t3-gb-highfreq-40hz-60s",
    "t3-gb-mega-throughput",
    "t3-mr-deep-book-state-size",
]


def main() -> None:
    ap = argparse.ArgumentParser(description=__doc__)
    ap.add_argument("--baseline", type=Path, required=True, help="preserved baseline bench executable")
    ap.add_argument("--candidate", type=Path, default=ROOT / "target/release/bench")
    ap.add_argument("--runs", type=int, default=7, help="measured runs per subprocess, after one warm-up")
    ap.add_argument("--rounds", type=int, default=5, help="alternating subprocess pairs per scenario")
    ap.add_argument("--json", type=Path)
    ap.add_argument("units", nargs="*", type=Path)
    args = ap.parse_args()
    if args.runs < 1 or args.rounds < 1:
        ap.error("runs and rounds must be positive")
    binaries = {"baseline": args.baseline.resolve(), "candidate": args.candidate.resolve()}
    results = []
    for unit in args.units or [UNITS / name for name in DEFAULT_UNITS]:
        config = (unit if unit.is_file() else unit / "scenario.json").resolve()
        samples: dict[str, list[float]] = {key: [] for key in binaries}
        outputs: dict[str, list[str]] = {key: [] for key in binaries}
        expected_rows = None
        for round_index in range(args.rounds):
            order = ["baseline", "candidate"] if round_index % 2 == 0 else ["candidate", "baseline"]
            for key in order:
                proc = subprocess.run([str(binaries[key]), str(config), str(args.runs)],
                                      capture_output=True, text=True, check=True, timeout=1800)
                sim = re.search(r"\| sim(?: only)? ([0-9.]+) ms \|", proc.stdout)
                rows = re.search(r": (\d+) rows, (\d+) msgs(?:,[^|]*)? \|", proc.stdout)
                if sim is None or rows is None:
                    raise RuntimeError(f"unrecognized bench output: {proc.stdout}")
                if expected_rows is None:
                    expected_rows = rows.groups()
                elif rows.groups() != expected_rows:
                    raise RuntimeError(f"{config}: trace/message counts changed")
                samples[key].append(float(sim[1]))
                outputs[key].append(proc.stdout.strip())
        medians = {key: statistics.median(values) for key, values in samples.items()}
        speedup = medians["baseline"] / medians["candidate"]
        result = {"unit": config.parent.name, "trace_rows": int(expected_rows[0]),
                  "ledger_rows": int(expected_rows[1]), "round_medians_ms": samples,
                  "median_ms": medians, "speedup": speedup, "outputs": outputs}
        results.append(result)
        print(f"{config.parent.name:36s} sim {medians['baseline']:7.1f} -> "
              f"{medians['candidate']:7.1f} ms  {speedup:.3f}x", flush=True)
    if args.json:
        args.json.write_text(json.dumps({
            "platform": platform.platform(),
            "rustc": subprocess.check_output(["rustc", "--version"], text=True).strip(),
            "runs_per_round": args.runs, "rounds": args.rounds,
            "aggregation": "median of per-subprocess warmed medians; bench prints to 0.1 ms",
            "binaries": {key: {"path": str(path), "sha256": hashlib.sha256(path.read_bytes()).hexdigest()}
                         for key, path in binaries.items()}, "results": results,
        }, indent=2) + "\n")


if __name__ == "__main__":
    main()
