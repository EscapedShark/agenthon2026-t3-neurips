#!/usr/bin/env python3
"""Compare complete processes, alternating baseline/candidate order after warm-up.

Includes startup, simulation, encoding, writes, hashes and teardown. Nothing runs concurrently.
Usage: bench_compare.py --baseline /path/to/old/fastsim [--candidate PATH] [UNIT_DIR ...]
"""

from __future__ import annotations

import argparse
import hashlib
import json
import platform
import statistics
import subprocess
import tempfile
import time
from pathlib import Path

ROOT = Path(__file__).resolve().parents[1]
UNITS = ROOT.parent / "track3-simulation-public" / "units"
DEFAULT_UNITS = [
    "t3-as06-throughput-fast",
    "t3-gb-highfreq-40hz-60s",
    "t3-gb-mega-throughput",
    "t3-mr-deep-book-state-size",
    "t3-gbatch-dense-3",
    "t3-gbatch-homog-8",
]


def run(binary: Path, unit: Path, out: Path) -> tuple[float, dict]:
    if (unit / "batch.json").exists():
        args = ["simulate-batch", "--batch-dir", str(unit / "scenarios"), "--out-dir", str(out)]
    else:
        config = unit if unit.is_file() else unit / "scenario.json"
        args = ["simulate", "--config", str(config), "--out", str(out / "trace.parquet")]
    start = time.perf_counter()
    proc = subprocess.run([str(binary), *args], capture_output=True, text=True, timeout=1800)
    elapsed = time.perf_counter() - start
    if proc.returncode:
        raise RuntimeError(f"{binary} failed on {unit}: {proc.stderr[-2000:]}")
    return elapsed, json.loads(proc.stdout)


def main() -> None:
    ap = argparse.ArgumentParser(description=__doc__)
    ap.add_argument("--baseline", type=Path, required=True)
    ap.add_argument("--candidate", type=Path, default=ROOT / "target/release/fastsim")
    ap.add_argument("--runs", type=int, default=7, help="measured repeats per binary, after one warm-up")
    ap.add_argument("--json", type=Path, help="save all measured process times and aggregate results")
    ap.add_argument("--all", action="store_true", help="measure the complete 71-unit public roster")
    ap.add_argument("--verify", action="store_true", help="compare decoded Parquet values, order and schema")
    ap.add_argument("units", nargs="*", type=Path)
    args = ap.parse_args()
    if args.runs < 1:
        ap.error("--runs must be positive")
    binaries = {"baseline": args.baseline.resolve(), "candidate": args.candidate.resolve()}
    if args.all and args.units:
        ap.error("--all and explicit units are mutually exclusive")
    units = (sorted(p for p in UNITS.iterdir() if (p / "card.toml").is_file()) if args.all else
             args.units or [UNITS / name for name in DEFAULT_UNITS])
    if args.verify:
        import pyarrow.parquet as pq
    results = []
    with tempfile.TemporaryDirectory(prefix="fastsim-bench-") as temp:
        for index, unit in enumerate(units):
            unit = unit.resolve()
            label = unit.parent.name if unit.is_file() else unit.name
            times: dict[str, list[float]] = {key: [] for key in binaries}
            rss: dict[str, list[int]] = {key: [] for key in binaries}
            reported: dict[str, list[float]] = {key: [] for key in binaries}
            expected_events = None
            for repeat in range(args.runs + 1):
                order = ["baseline", "candidate"] if repeat % 2 == 0 else ["candidate", "baseline"]
                for key in order:
                    out = Path(temp) / str(index) / key
                    elapsed, event = run(binaries[key], unit, out)
                    n_events = event.get("total_events", event.get("n_events"))
                    if expected_events is None:
                        expected_events = n_events
                    elif n_events != expected_events:
                        raise RuntimeError(f"{label}: event count changed: {n_events} != {expected_events}")
                    if repeat:
                        times[key].append(elapsed)
                        rss[key].append(event["peak_memory_bytes"])
                        reported[key].append(event["wall_clock_sec"])
            verified = 0
            if args.verify:
                base, cand = (Path(temp) / str(index) / key for key in ("baseline", "candidate"))
                members = {p.relative_to(base) for p in base.rglob("*.parquet")}
                if members != {p.relative_to(cand) for p in cand.rglob("*.parquet")}:
                    raise RuntimeError(f"{label}: output file set changed")
                for member in sorted(members):
                    a, b = pq.read_table(base / member), pq.read_table(cand / member)
                    if not a.schema.equals(b.schema, check_metadata=True) or not a.equals(b, check_metadata=True):
                        raise RuntimeError(f"{label}/{member}: Parquet values, order or schema changed")
                    verified += 1
            medians = {key: statistics.median(values) for key, values in times.items()}
            reported_medians = {key: statistics.median(values) for key, values in reported.items()}
            result = {
                "unit": label,
                "n_events": expected_events,
                "process_seconds": times,
                "median_seconds": medians,
                "speedup": medians["baseline"] / medians["candidate"],
                "reported_seconds": reported,
                "median_reported_seconds": reported_medians,
                "reported_speedup": reported_medians["baseline"] / reported_medians["candidate"],
                "verified_parquet_files": verified,
                "median_peak_rss_bytes": {key: statistics.median(values) for key, values in rss.items()},
            }
            results.append(result)
            print(
                f"{label:36s} {medians['baseline'] * 1000:8.1f} -> {medians['candidate'] * 1000:8.1f} ms "
                f"{result['speedup']:5.2f}x  "
                f"reported {result['reported_speedup']:.2f}x  "
                f"RSS {result['median_peak_rss_bytes']['baseline'] / 2**20:.1f} -> "
                f"{result['median_peak_rss_bytes']['candidate'] / 2**20:.1f} MiB",
                flush=True,
            )
    mean_rates = {key: statistics.mean(r["n_events"] / r["median_reported_seconds"][key] for r in results)
                  for key in binaries}
    print(f"Mean per-unit reported events/s: {mean_rates['baseline']:.0f} -> {mean_rates['candidate']:.0f} "
          f"({mean_rates['candidate'] / mean_rates['baseline']:.3f}x); "
          f"{sum(r['verified_parquet_files'] for r in results)} Parquet files verified")
    if args.json:
        args.json.write_text(json.dumps({"runs": args.runs, "platform": platform.platform(),
            "binaries": {k: str(v) for k, v in binaries.items()},
            "binary_sha256": {k: hashlib.sha256(v.read_bytes()).hexdigest() for k, v in binaries.items()},
            "mean_reported_events_per_sec": mean_rates, "results": results}, indent=2) + "\n")


if __name__ == "__main__":
    main()
