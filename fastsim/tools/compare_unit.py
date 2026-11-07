#!/usr/bin/env python3
"""Run fastsim on public units and compare with the reference outputs.

Two verdicts per scenario:
* EXACT: trace.parquet and message_trace.parquet equal the reference row for row (the bar we hold
  ourselves to: identical output means every gate passes by construction);
* GATES: the official scorer's own checks — `semantic_regression_pass` with the card's tier and
  tolerances, the stylized-fact gate for family 5, and the message-ledger gates the card requires.

Usage: compare_unit.py [--bin PATH] [UNIT_DIR ...]   (default: every public unit)
"""

from __future__ import annotations

import argparse
import json
import subprocess
import sys
import tempfile
import time
from pathlib import Path

import pandas as pd
from qfbench2_common.scoring import stylized_facts
from qfbench2_track_simulation import batch as tbatch
from qfbench2_track_simulation import semantics
from qfbench2_track_simulation.scoring import _CardPolicy

ROOT = Path(__file__).resolve().parents[1]
UNITS = ROOT.parent / "track3-simulation-public" / "units"


def first_diff(cand: pd.DataFrame, ref: pd.DataFrame) -> str | None:
    if list(cand.columns) != list(ref.columns):
        return f"columns {list(cand.columns)} != {list(ref.columns)}"
    bad_types = {c: (str(cand[c].dtype), str(ref[c].dtype)) for c in ref.columns if cand[c].dtype != ref[c].dtype}
    if len(cand) != len(ref):
        n = min(len(cand), len(ref))
    else:
        n = len(ref)
    for col in ref.columns:
        a, b = cand[col].iloc[:n].reset_index(drop=True), ref[col].iloc[:n].reset_index(drop=True)
        neq = ~((a == b).fillna(False) | (a.isna() & b.isna()))
        if neq.any():
            i = int(neq.to_numpy().nonzero()[0][0])
            lo, hi = max(0, i - 3), i + 4
            return (
                f"first difference at row {i} col {col!r}"
                f"\n--- candidate\n{cand.iloc[lo:hi].to_string()}\n--- reference\n{ref.iloc[lo:hi].to_string()}"
            )
    if len(cand) != len(ref):
        return f"row count {len(cand)} != {len(ref)} (common prefix identical)"
    if bad_types:
        return f"dtypes differ: {bad_types}"
    return None


def gates(unit: Path, cand: pd.DataFrame, ref: pd.DataFrame, cmsg: pd.DataFrame | None, rmsg: pd.DataFrame | None) -> list[str]:
    policy = _CardPolicy(unit)
    ok, breaches = semantics.semantic_regression_pass(
        cand,
        ref,
        family=policy.family,
        tier=policy.tier,
        ceilings=policy.ceilings,
        spread_bps_tolerance=policy.spread_bps_tolerance,
        timestamp_tolerance_ns=policy.timestamp_tolerance_ns,
        kendall_tau_floor=policy.kendall_tau_floor,
    )
    out = [] if ok else [f"semantic: {breaches}"]
    if policy.family == 5:
        report = stylized_facts.stylized_fact_report(
            semantics.mid_price_series(cand).to_numpy(dtype=float),
            semantics.mid_price_series(ref).to_numpy(dtype=float),
            cand_depth_hist=semantics.depth_histogram(cand),
            ref_depth_hist=semantics.depth_histogram(ref),
        )
        adm, sf = stylized_facts.admissible(report, policy.ceilings)
        if not adm:
            out.append(f"stylized: {sf}")
    if policy.requires_message_ledger and rmsg is not None:
        if cmsg is None:
            return out + ["message ledger required but not written"]
        for name, (ok, br) in [
            ("msg_semantics", semantics.check_message_semantics(cmsg)),
            ("msg_reference", semantics.check_message_reference(cmsg, rmsg)),
        ] + ([("protocol", semantics.check_protocol_fidelity(cmsg, rmsg))] if policy.family == 7 else []):
            if not ok:
                out.append(f"{name}: {br}")
    return out


def run(cmd: list[str]) -> tuple[bool, float, str]:
    t0 = time.perf_counter()
    proc = subprocess.run(cmd, capture_output=True, text=True)
    return proc.returncode == 0, time.perf_counter() - t0, proc.stderr.strip()[-500:]


def compare(label: str, unit: Path, out_dir: Path, ref_dir: Path, dt: float, verbose: bool,
            batch: bool = False) -> tuple[bool, bool]:
    cand = pd.read_parquet(out_dir / "trace.parquet")
    ref = pd.read_parquet(ref_dir / "trace.parquet")
    rmsg_path = ref_dir / "message_trace.parquet"
    rmsg = pd.read_parquet(rmsg_path) if rmsg_path.exists() else None
    cmsg_path = out_dir / "message_trace.parquet"
    d_trace = first_diff(cand, ref)
    if cmsg_path.exists():
        cmsg = pd.read_parquet(cmsg_path)
        d_msg = first_diff(cmsg, rmsg) if rmsg is not None else None
        msgs = f"{len(cmsg):8d}/{len(rmsg) if rmsg is not None else -1:<8d}"
    else:
        # Omitting the ledger is only correct where the scorer does not require it.
        cmsg = None
        required = batch or _CardPolicy(unit).requires_message_ledger
        d_msg = "ledger missing but required" if required else None
        msgs = f"{'omitted':>8s}/{'n/a':<8s}"
    g = gates(unit, cand, ref, cmsg, rmsg)
    exact = d_trace is None and d_msg is None
    print(
        f"{'EXACT' if exact else 'DIFF '} {'GATES-OK' if not g else 'GATES-FAIL'} {label:48s} "
        f"rows {len(cand):8d}/{len(ref):<8d} msgs {msgs} {dt*1000:8.1f} ms"
    )
    if verbose or not exact:
        if d_trace:
            print("   trace:", d_trace.replace("\n", "\n   "))
        if d_msg:
            print("   ledger:", d_msg.replace("\n", "\n   "))
    for line in g:
        print("   gate:", line[:600])
    return exact, not g


def main() -> int:
    ap = argparse.ArgumentParser()
    ap.add_argument("--bin", default=str(ROOT / "target" / "release" / "fastsim"))
    ap.add_argument("--keep", default=None, help="write outputs under this directory")
    ap.add_argument("--no-run", action="store_true", help="compare outputs already under --keep")
    ap.add_argument("--verbose", action="store_true")
    ap.add_argument("--ledger", default=None, choices=["auto", "always", "never"], help="passed to simulate")
    ap.add_argument("units", nargs="*")
    args = ap.parse_args()
    units = [Path(u) for u in args.units] or sorted(p for p in UNITS.iterdir() if p.is_dir())

    n_exact = n_gates = n = 0
    base = Path(args.keep) if args.keep else Path(tempfile.mkdtemp(prefix="fastsim-cmp-"))
    for unit in units:
        out_unit = base / unit.name
        out_unit.mkdir(parents=True, exist_ok=True)
        if (unit / "batch.json").exists():
            subs = json.loads((unit / "batch.json").read_text())["subs"]
            ok, dt = True, 0.0
            if not args.no_run:
                ok, dt, err = run([args.bin, "simulate-batch", "--batch-dir", str(unit / "scenarios"), "--out-dir", str(out_unit)])
                if not ok:
                    print(f"CRASH {unit.name}: {err}")
                    n += len(subs)
                    continue
            policy = _CardPolicy(unit)
            agg_ok, agg = tbatch.check_aggregate(out_unit, tbatch.load_subs(unit), tol=0.05)
            iso_ok, iso = tbatch.score_isolation(
                unit, out_unit, policy.family, policy.tier, policy.ceilings,
                timestamp_tolerance_ns=policy.timestamp_tolerance_ns, kendall_tau_floor=policy.kendall_tau_floor,
            )
            print(f"BATCH {unit.name}: aggregate {'OK' if agg_ok else agg} isolation {'OK' if iso_ok else iso[:2]} "
                  f"{len(subs)} subs in {dt*1000:.1f} ms")
            for sub in subs:
                n += 1
                e, g = compare(f"{unit.name}/{sub['sub']}", unit, out_unit / sub["sub"],
                               unit / "checks" / "reference_data" / sub["sub"], 0.0, args.verbose, batch=True)
                n_exact += e
                n_gates += g and agg_ok and iso_ok
            continue
        n += 1
        dt = 0.0
        if not args.no_run:
            ok, dt, err = run([args.bin, "simulate", "--config", str(unit / "scenario.json"), "--out", str(out_unit / "trace.parquet")]
                              + (["--ledger", args.ledger] if args.ledger else []))
            if not ok:
                print(f"CRASH {unit.name}: {err}")
                continue
        e, g = compare(unit.name, unit, out_unit, unit, dt, args.verbose)
        n_exact += e
        n_gates += g
    print(f"\n{n_exact}/{n} scenarios identical to the reference (every row, every column, dtypes); "
          f"{n_gates}/{n} pass the official gates")
    return 0 if n_exact == n else 1


if __name__ == "__main__":
    raise SystemExit(main())
