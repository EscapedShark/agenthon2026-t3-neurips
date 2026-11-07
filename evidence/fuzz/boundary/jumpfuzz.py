"""Targeted campaign for the megashock-time boundary: many high-frequency value traders and
frequent megashocks, so that observations land within 128 ns after a megashock.

ABIDES runs instrumented (abides_oracle_log.py) and the oracle log gives each case's boundary
hits: megashocks applied at a time before the previous observation, i.e. an observation that came
after the megashock but compared as not after it. Every case is then compared with each binary.

Usage: jumpfuzz.py N OUT_DIR --seed S --jobs J --bin LABEL=PATH [--bin LABEL=PATH ...]
"""
import argparse, json, os, random, subprocess, sys
from concurrent.futures import ProcessPoolExecutor
from pathlib import Path

import pandas as pd

WS = Path(__file__).resolve().parents[5]  # the workspace: fastsim/benchmarks/<this folder>/fuzz/boundary/
sys.path.insert(0, str(WS / "fastsim" / "tools"))
import fuzz_vs_abides as F  # noqa: E402

ABIDES_PY = WS / ".venv-abides" / "bin" / "python"
BASELINES = WS / "track3-simulation-public" / "baselines"
LOGGER = Path(__file__).resolve().parents[1] / "case-0267" / "abides_oracle_log.py"


def scenario(rng: random.Random, i: int) -> dict:
    horizon = rng.choice([1, 2]) * 1_000_000_000
    agents, mix = [], {"NoiseTrader": 0, "MarketMaker": 0, "ValueTrader": 0, "MomentumTrader": 0}
    budget = 120_000  # wakeups, so ABIDES stays near 10 s a case
    for _ in range(rng.choice([1, 2, 3])):
        rate = rng.choice([50.0, 100.0, 200.0, 400.0])
        count = max(1, min(rng.choice([16, 32, 64, 96, 128]), int(budget / (rate * horizon / 1e9) / 2)))
        agents.append({"agent_type": "ValueTrader", "count": count,
                       "params": {"arrival_rate_hz": rate, "order_size_mean": rng.choice([1, 5, 12.5, 25]),
                                  "threshold_ticks": rng.choice([1, 2, 5, 10])}})
    nt = rng.choice([16, 32, 64, 96])
    agents.append({"agent_type": "NoiseTrader", "count": nt,
                   "params": {"arrival_rate_hz": rng.choice([5.0, 20.0, 50.0]), "order_size_mean": rng.choice([1, 5, 10]),
                              "order_size_std": rng.choice([1, 2, 5]), "price_offset_ticks": rng.choice([1, 2, 5])}})
    if rng.random() < 0.5:
        agents.append({"agent_type": "MarketMaker", "count": rng.choice([1, 2, 4]),
                       "params": {"rebalance_interval_ns": rng.choice([10_000_000, 50_000_000]), "spread_ticks": rng.choice([2, 4]),
                                  "depth_levels": rng.choice([1, 3]), "size_per_level": rng.choice([10, 50])}})
    for a in agents:
        mix[a["agent_type"]] += a["count"]
    params = {"initial_price": rng.choice([100_000, 1_000_000]), "kappa": rng.choice([0.0, 0.05, 1.0]),
              "sigma": rng.choice([5e-5, 5e-4]), "jump_intensity": rng.choice([50.0, 200.0, 500.0, 1000.0]),
              "jump_sigma": rng.choice([0.0, 200.0, 1000.0])}
    if rng.random() < 0.25:
        params["scheduled_jump"] = {"time_ns": rng.randrange(horizon), "magnitude": rng.choice([-500, 300])}
    sc = {"scenario_id": f"jump-{i:04d}", "seed": rng.randrange(2**32), "horizon_ns": horizon, "agent_mix": mix,
          "exchange_config": F.rand_exchange(rng), "oracle_config": {"type": "mean_reverting", "params": params},
          "agent_configs": agents}
    lat = F.rand_latency(rng)
    if lat:
        sc["latency_config"] = lat
    return sc


def hits(log: Path) -> tuple[int, int]:
    """(megashocks applied, of those applied at a time before the previous observation)."""
    n = h = 0
    for line in log.open():
        r = json.loads(line)
        if r["k"] == "c" and r["tst"] == "float64":
            n += 1
            if r["ptt"] != "float64" and int(float(r["ts"])) < int(r["pt"]):
                h += 1
    return n, h


def one(args):
    i, sc, base, bins = args
    d = Path(base) / f"{i:04d}"
    (d / "ref").mkdir(parents=True, exist_ok=True)
    cfg = d / "scenario.json"
    cfg.write_text(json.dumps(sc, indent=1))
    env = {**os.environ, "PYTHONPATH": str(BASELINES)}
    r = subprocess.run([str(ABIDES_PY), str(LOGGER), str(cfg), str(d / "ref" / "trace.parquet"), str(d / "oracle.jsonl")],
                       capture_output=True, text=True, env=env, cwd=d, timeout=1800)
    if r.returncode != 0:
        return i, "ABIDES-ERROR", (0, 0), {}
    n, h = hits(d / "oracle.jsonl")
    (d / "oracle.jsonl").unlink()
    verdicts = {}
    for label, path in bins:
        out = d / label
        out.mkdir(exist_ok=True)
        o = subprocess.run([path, "simulate", "--config", str(cfg), "--out", str(out / "trace.parquet")], capture_output=True, text=True)
        if o.returncode != 0:
            verdicts[label] = "CRASH"
            continue
        same = all(pd.read_parquet(out / f).equals(pd.read_parquet(d / "ref" / f)) for f in ("trace.parquet", "message_trace.parquet"))
        verdicts[label] = "OK" if same else "MISMATCH"
        if same:
            for f in out.iterdir():
                f.unlink()
            out.rmdir()
    return i, "RAN", (n, h), verdicts


def main():
    ap = argparse.ArgumentParser()
    ap.add_argument("n", type=int)
    ap.add_argument("out")
    ap.add_argument("--seed", type=int, default=1)
    ap.add_argument("--jobs", type=int, default=8)
    ap.add_argument("--bin", action="append", required=True)
    a = ap.parse_args()
    bins = [(b.split("=", 1)[0], str(Path(b.split("=", 1)[1]).resolve())) for b in a.bin]
    rng = random.Random(a.seed)
    cases = [(i, scenario(rng, i), a.out, bins) for i in range(a.n)]
    summary = {"cases": a.n, "abides_errors": 0, "megashocks": 0, "boundary_hits": 0, "cases_with_hits": 0,
               "verdicts": {lab: {} for lab, _ in bins}, "by_hits": {}}
    done = 0
    with ProcessPoolExecutor(max_workers=a.jobs) as ex:
        for i, status, (n, h), verdicts in ex.map(one, cases):
            done += 1
            if status != "RAN":
                summary["abides_errors"] += 1
                continue
            summary["megashocks"] += n
            summary["boundary_hits"] += h
            summary["cases_with_hits"] += h > 0
            key = "hit" if h else "no-hit"
            for lab, v in verdicts.items():
                summary["verdicts"][lab][v] = summary["verdicts"][lab].get(v, 0) + 1
                k2 = f"{lab}/{key}/{v}"
                summary["by_hits"][k2] = summary["by_hits"].get(k2, 0) + 1
            if any(v != "OK" for v in verdicts.values()) or h:
                print(f"#{i:04d} megashocks {n} hits {h} " + " ".join(f"{k}={v}" for k, v in verdicts.items()), flush=True)
            if done % 50 == 0:
                print(f"-- {done}/{a.n}", flush=True)
    (Path(a.out) / "summary.json").write_text(json.dumps(summary, indent=1) + "\n")
    print(json.dumps(summary, indent=1))


if __name__ == "__main__":
    main()
