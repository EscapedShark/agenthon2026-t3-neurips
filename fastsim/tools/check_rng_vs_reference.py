#!/usr/bin/env python3
"""Cross-check the Rust RNG/seed chain against real reference traces, with no simulator.

Two parts of a reference trace depend on the random streams alone, not on market state:

* each NoiseTrader's sequence of ORDER_SUBMITTED (size, side): one normal() + two randint()
  draws per act from that agent's own RandomState;
* each non-self message's realized latency in the kernel ledger (message_trace.parquet): one draw
  per send from the latency model's RandomState, in send (= message_id) order.

`seedcheck` (Rust) predicts both from the scenario seed; this script reads the reference parquet
and compares. A match proves the global seed chain (including the oracle's interleaved global
exponential draw), per-agent seeding, and the normal/randint/lognormal/uniform/pareto paths.

Usage: check_rng_vs_reference.py [UNIT_DIR ...]   (default: every public unit)
"""

from __future__ import annotations

import json
import subprocess
import sys
from pathlib import Path

import pyarrow.parquet as pq

ROOT = Path(__file__).resolve().parents[1]
UNITS = ROOT.parent / "track3-simulation-public" / "units"
SEEDCHECK = ROOT / "target" / "release" / "seedcheck"


def cases(unit: Path):
    """(label, scenario.json, reference dir) for a single or batch unit."""
    if (unit / "batch.json").exists():
        batch = json.loads((unit / "batch.json").read_text())
        for sub in batch["subs"]:
            yield (
                f"{unit.name}/{sub['sub']}",
                unit / sub["scenario_file"],
                unit / "checks" / "reference_data" / sub["sub"],
            )
    else:
        yield unit.name, unit / "scenario.json", unit


def check(label: str, scenario_path: Path, ref_dir: Path) -> bool:
    scenario = json.loads(scenario_path.read_text())
    kinds = []
    for cfg in scenario["agent_configs"]:
        kinds += [cfg["agent_type"]] * int(cfg["count"])
    noise_ids = [i + 1 for i, k in enumerate(kinds) if k == "NoiseTrader"]

    trace = pq.read_table(ref_dir / "trace.parquet", columns=["agent_id", "msg_type", "side", "size"])
    t = trace.to_pydict()
    subs: dict[int, list[tuple[int, int]]] = {a: [] for a in noise_ids}
    for aid, mt, side, size in zip(t["agent_id"], t["msg_type"], t["side"], t["size"]):
        if mt == "ORDER_SUBMITTED" and aid in subs:
            subs[aid].append((int(size), 1 if side == "BID" else 0))

    msg = pq.read_table(
        ref_dir / "message_trace.parquet", columns=["seq", "src_id", "dst_id", "message_id", "latency_ns", "msg_type"]
    )
    m = msg.to_pydict()
    # A broadcast (the exchange's MarketClosePriceMsg) is ONE message object sent to each
    # subscriber in subscription order = delivery order of the MarketClosePriceRequestMsg.
    requests = sorted(
        (seq, src)
        for seq, src, mt in zip(m["seq"], m["src_id"], m["msg_type"])
        if mt == "MarketClosePriceRequestMsg"
    )
    sub_rank = {src: r for r, (_, src) in enumerate(requests)}
    sent = sorted(
        (mid, sub_rank.get(dst, 0) if mt == "MarketClosePriceMsg" else 0, lat)
        for src, dst, mid, lat, mt in zip(m["src_id"], m["dst_id"], m["message_id"], m["latency_ns"], m["msg_type"])
        if src != dst
    )
    ref_lat = [lat for _, _, lat in sent]

    n_noise = max((len(v) for v in subs.values()), default=0)
    # Undelivered sends (after the stop time) drew a latency but are absent from the ledger.
    n_lat = len(ref_lat) + 1000
    out = subprocess.run(
        [str(SEEDCHECK), str(scenario_path), str(n_noise), str(n_lat)], check=True, capture_output=True, text=True
    ).stdout
    pred = json.loads(out)

    ok = True
    noise_total = noise_match = 0
    first_bad = None
    for aid, seq in subs.items():
        want = [(s, b) for s, b, _ in pred["noise"][str(aid)]]
        noise_total += len(seq)
        for k, got in enumerate(seq):
            if got == want[k]:
                noise_match += 1
            else:
                ok = False
                if first_bad is None:
                    first_bad = f"agent {aid} act {k}: ref {got} pred {want[k]}"
                break

    # Latency: the ledger is the predicted stream minus undelivered tail sends, so walk it as a
    # subsequence and require that skipped draws only happen after the last matched one would allow.
    pl = pred["latency"]
    j = lat_match = 0
    lat_first_bad = None
    for k, lat in enumerate(ref_lat):
        if j < len(pl) and pl[j] == lat:
            lat_match += 1
            j += 1
        else:
            ok = False
            lat_first_bad = f"send #{k} (message_id {sent[k][0]}): ref {lat} pred {pl[j] if j < len(pl) else None}"
            break

    status = "OK  " if ok else "FAIL"
    model = pred["latency_model"]
    print(
        f"{status} {label:48s} noise {noise_match:6d}/{noise_total:<6d} "
        f"latency {lat_match:7d}/{len(ref_lat):<7d} {model}"
    )
    if first_bad:
        print(f"      first noise mismatch: {first_bad}")
    if lat_first_bad:
        print(f"      first latency mismatch: {lat_first_bad}")
    return ok


def main() -> int:
    units = [Path(p) for p in sys.argv[1:]] or sorted(p for p in UNITS.iterdir() if p.is_dir())
    results = [check(*c) for u in units for c in cases(u)]
    print(f"\n{sum(results)}/{len(results)} scenarios match the reference RNG streams exactly")
    return 0 if all(results) else 1


if __name__ == "__main__":
    raise SystemExit(main())
