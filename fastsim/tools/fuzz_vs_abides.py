#!/usr/bin/env python3
"""Differential fuzzing: random scenarios through the real ABIDES baseline and through fastsim.

The public units cover a fraction of the parameter space the sealed set may use. This draws
scenarios across everything the adapter reads (agent mixes and parameters, every latency model
including none, self-trade-prevention policies, ack/compute delays, scheduled jumps, megashocks,
oracle parameters), runs the pinned baseline (abides-ref + .venv-abides) and fastsim on each, and
demands identical trace and ledger rows.

Usage: fuzz_vs_abides.py [N] [--seed S] [--jobs J] [--mixed] [--min-draws D] [--scale] [--reuse]

`--scale` draws markets like the throughput-scale units instead (the gb units' four-type ecology),
scaled along random axes: population, horizon, arrival rates, order sizes, oracle jumps, latency,
mixed rates within a kind. `--reuse` keeps a case's ABIDES output when its directory already holds
the same scenario, so a new binary can be checked against an earlier run's references quickly.
"""

from __future__ import annotations

import argparse
import json
import os
import random
import subprocess
import sys
import tempfile
from concurrent.futures import ProcessPoolExecutor
from pathlib import Path

import pandas as pd

ROOT = Path(__file__).resolve().parents[1]
WS = ROOT.parent
BASELINES = WS / "track3-simulation-public" / "baselines"
ABIDES_PY = WS / ".venv-abides" / "bin" / "python"
FASTSIM = ROOT / "target" / "release" / "fastsim"


def rand_latency(rng: random.Random) -> dict | None:
    """A latency model of any kind the adapter reads, or none."""
    lat_choice = rng.choice(["log_normal", "log_normal", "uniform", "pareto", "deterministic", None])
    lat = None
    if lat_choice == "log_normal":
        lat = {"model": "log_normal", "params": {"mean_ns": rng.choice([100, 500, 5000, 50000]),
                                                 "sigma": rng.choice([0.1, 0.3, 0.75, 1.5]),
                                                 "min_ns": rng.choice([0, 50, 100]), "max_ns": rng.choice([10_000, 1_000_000, 1e12])}}
    elif lat_choice == "uniform":
        lo = rng.choice([0, 100, 1000])
        lat = {"model": "uniform", "params": {"mean_ns": 500, "min_ns": lo, "max_ns": lo + rng.choice([10, 1000, 100000])}}
    elif lat_choice == "pareto":
        lat = {"model": "pareto", "params": {"min_ns": rng.choice([0, 100, 500]), "max_ns": rng.choice([5000, 10_000_000]),
                                             "alpha": rng.choice([1.1, 1.5, 3.0])}}
    elif lat_choice == "deterministic":
        lat = {"model": "deterministic", "params": {"mean_ns": rng.choice([0, 1, 250, 250000]), "min_ns": 0, "max_ns": 1e12}}
    return lat


def rand_exchange(rng: random.Random) -> dict:
    """Exchange settings: protocol enforcement with self-trade prevention and delays, or not."""
    exchange = {"symbol": "SYM", "tick_size": 1}
    if rng.random() < 0.35:
        exchange["protocol_enforcement"] = True
        exchange["stp_policy"] = rng.choice(["cancel_newest", "cancel_oldest", None])
        exchange["ack_delay_ns"] = rng.choice([0, 100, 5000])
        exchange["compute_delay_ns"] = rng.choice([0, 10, 1000])
    elif rng.random() < 0.3:
        exchange["stp_policy"] = "cancel_newest"  # inert without protocol_enforcement
    return exchange


def rand_scenario(rng: random.Random, i: int, mixed: bool = False, min_draws: int = 0) -> dict:
    agents = []
    mix = {
        "NoiseTrader": rng.choice([0, 1, 2, 3, 5, 8]),
        "MarketMaker": rng.choice([0, 0, 1, 1, 2]),
        "ValueTrader": rng.choice([0, 1, 2, 4]),
        "MomentumTrader": rng.choice([0, 0, 1, 2, 3]),
    }
    if sum(mix.values()) == 0:
        mix["NoiseTrader"] = 2
    for kind, count in mix.items():
        if not count:
            continue
        p: dict = {}
        if rng.random() < 0.7:
            p["arrival_rate_hz"] = rng.choice([0.5, 1.0, 2.0, 3.0, 5.0, 10.0, 25.0, 40.0])
        if kind == "NoiseTrader":
            p["order_size_mean"] = rng.choice([1, 5, 10, 20, 7.5])
            p["order_size_std"] = rng.choice([0, 1, 2, 5, 3.3])
            p["price_offset_ticks"] = rng.choice([0, 1, 2, 5, 10])
        elif kind == "MarketMaker":
            p["spread_ticks"] = rng.choice([1, 2, 3, 4, 10])
            p["depth_levels"] = rng.choice([1, 2, 3, 5])
            p["size_per_level"] = rng.choice([1, 5, 10, 50])
            if rng.random() < 0.8:
                p["rebalance_interval_ns"] = rng.choice([100_000_000, 500_000_000, 1_000_000_000, 2_000_000_000])
        elif kind == "ValueTrader":
            p["order_size_mean"] = rng.choice([1, 10, 25, 12.5])
            p["threshold_ticks"] = rng.choice([0, 1, 2, 5])
        else:
            p["order_size_mean"] = rng.choice([1, 15, 20])
            p["threshold_ticks"] = rng.choice([0, 1, 2, 4])
            p["lookback"] = rng.choice([1, 2, 5, 10])
        agents.append({"agent_type": kind, "count": count, "params": p})
    if mixed:
        # More groups of a kind already present, at other intervals (one fast, many slow and the
        # like), anywhere in the list: agents of one kind at mixed intervals.
        for kind in [a["agent_type"] for a in agents]:
            if rng.random() < 0.7:
                p = dict(next(a for a in agents if a["agent_type"] == kind)["params"])
                if kind == "MarketMaker":
                    p["rebalance_interval_ns"] = rng.choice([10_000_000, 50_000_000, 3_000_000_000])
                else:
                    p["arrival_rate_hz"] = rng.choice([0.5, 2.0, 50.0, 100.0, 200.0])
                if kind == "NoiseTrader":
                    p["order_size_mean"] = rng.choice([1, 3, 12])
                    p["price_offset_ticks"] = rng.choice([0, 3, 7])
                agents.insert(rng.randrange(len(agents) + 1),
                              {"agent_type": kind, "count": rng.choice([1, 1, 2, 4]), "params": p})
        mix = {k: sum(a["count"] for a in agents if a["agent_type"] == k) for k in mix}

    lat = rand_latency(rng)
    exchange = rand_exchange(rng)

    oracle = {"initial_price": rng.choice([100_000, 100_000, 50_000, 1_000, 250_000]),
              "kappa": rng.choice([0.0, 0.01, 0.05, 0.5]),
              "sigma": rng.choice([5e-5, 5e-4, 0.01, 1.0]),
              "jump_intensity": rng.choice([0.0, 0.0, 0.5, 2.0, 10.0]),
              "jump_sigma": rng.choice([0.0, 50.0, 500.0])}
    horizon = rng.choice([2, 5, 10] if mixed else [2, 5, 10, 20, 30]) * 1_000_000_000

    def draws() -> float:
        # As `Scenario::estimated_agent_draws`: noise traders and value traders draw at each wakeup.
        def interval(p: dict) -> int:
            return max(1, int(p["rebalance_interval_ns"]) if "rebalance_interval_ns" in p else int(1e9 / p.get("arrival_rate_hz", 1.0)))
        return sum(a["count"] * horizon / interval(a["params"]) for a in agents
                   if a["agent_type"] in ("NoiseTrader", "ValueTrader"))

    # Enough draws for fastsim's agent-draw lookahead (a helper thread from 10,000): grow the
    # drawing groups.
    growable = [a for a in agents if a["agent_type"] in ("NoiseTrader", "ValueTrader")]
    while min_draws and growable and draws() < min_draws:
        a = rng.choice(growable)
        a["count"] = min(a["count"] * 2, 96)
        if all(g["count"] >= 96 for g in growable):
            break
    mix = {k: sum(a["count"] for a in agents if a["agent_type"] == k) for k in mix}
    if rng.random() < 0.25:
        oracle["scheduled_jump"] = {"time_ns": rng.randrange(0, horizon), "magnitude": rng.choice([-2000, -50, 50, 3000])}

    sc = {
        "scenario_id": f"fuzz-{i:04d}",
        "seed": rng.randrange(0, 2**32),
        "horizon_ns": horizon,
        "agent_mix": mix,
        "exchange_config": exchange,
        "oracle_config": {"type": "mean_reverting", "params": oracle},
        "agent_configs": agents,
    }
    if lat is not None:
        sc["latency_config"] = lat
    return sc


def wakeups(agents: list[dict], horizon: int) -> float:
    """Agent wakeups over the horizon (each trader at its arrival rate, each market maker at its
    rebalance interval): about a fifth of the events."""
    def interval(p: dict) -> int:
        return max(1, int(p["rebalance_interval_ns"]) if "rebalance_interval_ns" in p else int(1e9 / p.get("arrival_rate_hz", 1.0)))
    return sum(a["count"] * horizon / interval(a["params"]) for a in agents)


def rand_scale_scenario(rng: random.Random, i: int, max_wakeups: float) -> dict:
    """A market like the throughput-scale units (the gb units' four-type ecology), scaled along
    random axes, its horizon (then its noise traders) cut until it has at most `max_wakeups`."""
    agents = [
        {"agent_type": "NoiseTrader", "count": rng.choice([30, 100, 200, 400, 600]),
         "params": {"order_size_mean": rng.choice([1, 10, 25, 100, 7.5]), "order_size_std": rng.choice([0, 2, 5, 30, 3.3]),
                    "arrival_rate_hz": rng.choice([5.0, 10.0, 20.0, 40.0, 80.0]),
                    "price_offset_ticks": rng.choice([0, 2, 5, 10, 20])}},
        {"agent_type": "MarketMaker", "count": rng.choice([0, 2, 6, 12]),
         "params": {"spread_ticks": rng.choice([1, 2, 4, 10]), "depth_levels": rng.choice([1, 3, 5, 10]),
                    "size_per_level": rng.choice([1, 10, 50, 100]),
                    "rebalance_interval_ns": rng.choice([50_000_000, 200_000_000, 500_000_000, 2_000_000_000])}},
        {"agent_type": "ValueTrader", "count": rng.choice([0, 10, 30, 100, 200]),
         "params": {"fundamental_value_source": "oracle", "order_size_mean": rng.choice([5, 25, 100, 12.5]),
                    "threshold_ticks": rng.choice([0, 2, 5, 10]), "arrival_rate_hz": rng.choice([1.0, 5.0, 20.0])}},
        {"agent_type": "MomentumTrader", "count": rng.choice([0, 2, 8, 30]),
         "params": {"order_size_mean": rng.choice([5, 15, 100]), "threshold_ticks": rng.choice([0, 2, 5]),
                    "lookback": rng.choice([1, 5, 20]), "arrival_rate_hz": rng.choice([1.0, 5.0, 20.0])}},
    ]
    agents = [a for a in agents if a["count"]]
    # Mixed density: another group of a kind at another rate, anywhere in the list.
    for kind in [a["agent_type"] for a in agents]:
        if rng.random() < 0.15:
            p = dict(next(a for a in agents if a["agent_type"] == kind)["params"])
            if kind == "MarketMaker":
                p["rebalance_interval_ns"] = rng.choice([10_000_000, 1_000_000_000])
            else:
                p["arrival_rate_hz"] = rng.choice([0.5, 2.0, 100.0, 200.0])
            agents.insert(rng.randrange(len(agents) + 1),
                          {"agent_type": kind, "count": rng.choice([1, 4, 20, 100]), "params": p})
    lat = rng.choice([{"model": "log_normal", "params": {"mean_ns": 500, "sigma": 0.5, "min_ns": 100, "max_ns": 5_000_000}},
                      rand_latency(rng)])
    exchange = rng.choice([{"symbol": "SYM", "tick_size": 1, "lot_size": 1, "min_price": 1, "max_price": 1_000_000,
                            "stp_policy": "cancel_newest", "order_types_allowed": ["LIMIT", "MARKET", "CANCEL", "REPLACE"]},
                           rand_exchange(rng)])
    oracle = {"initial_price": rng.choice([100_000, 100_000, 50_000, 1_000]),
              "kappa": rng.choice([0.0, 0.01, 0.05, 0.5]),
              "sigma": rng.choice([1e-4, 0.001, 0.01]),
              "dt_ns": 1_000_000, "noise_type": "gaussian",
              "jump_intensity": rng.choice([0.0, 0.0, 0.5, 2.0, 10.0]),
              "jump_sigma": rng.choice([0.0, 50.0, 500.0, 1000.0])}
    horizon = rng.choice([2, 5, 10, 20, 30, 60]) * 1_000_000_000
    while wakeups(agents, horizon) > max_wakeups and horizon > 1_000_000_000:
        horizon //= 2
    while wakeups(agents, horizon) > max_wakeups and agents[0]["count"] > 10:
        agents[0]["count"] //= 2
    if rng.random() < 0.25:
        oracle["scheduled_jump"] = {"time_ns": rng.randrange(0, horizon), "magnitude": rng.choice([-2000, -50, 50, 3000])}
    mix = {k: sum(a["count"] for a in agents if a["agent_type"] == k)
           for k in ("NoiseTrader", "MarketMaker", "ValueTrader", "MomentumTrader")}
    sc = {
        "scenario_id": f"scale-{i:04d}",
        "scenario_family": "throughput-scale",
        "seed": rng.randrange(0, 2**32),
        "horizon_ns": horizon,
        "agent_mix": {k: v for k, v in mix.items() if v},
        "exchange_config": exchange,
        "oracle_config": {"type": "mean_reverting", "params": oracle},
        "agent_configs": agents,
    }
    if lat is not None:
        sc["latency_config"] = lat
    return sc


def one(args: tuple) -> tuple[int, str, str]:
    i, sc, base, binary, *rest = args
    reuse = bool(rest and rest[0])
    timeout = rest[1] if len(rest) > 1 else None
    d = Path(base) / f"{i:04d}"
    (d / "ref").mkdir(parents=True, exist_ok=True)
    (d / "ours").mkdir(parents=True, exist_ok=True)
    cfg = d / "scenario.json"
    text = json.dumps(sc, indent=1)
    cached = reuse and cfg.is_file() and cfg.read_text() == text and (d / "ref" / "trace.parquet").is_file()
    cfg.write_text(text)
    env = {**os.environ, "PYTHONPATH": str(BASELINES)}
    if not cached:
        # ABIDES writes ./log/<timestamp>/summary_log.bz2 relative to its cwd; keep it in the case dir.
        try:
            r = subprocess.run([str(ABIDES_PY), "-m", "abides_fork.simulate", "--config", str(cfg), "--out", str(d / "ref" / "trace.parquet")],
                               capture_output=True, text=True, env=env, cwd=d, timeout=timeout)
        except subprocess.TimeoutExpired:
            (d / "ref" / "trace.parquet").unlink(missing_ok=True)
            return i, "ABIDES-TIMEOUT", f"over {timeout} s"
        if r.returncode != 0:
            (d / "ref" / "trace.parquet").unlink(missing_ok=True)
            return i, "ABIDES-ERROR", r.stderr.strip().splitlines()[-1][:200] if r.stderr.strip() else "?"
    o = subprocess.run([binary, "simulate", "--config", str(cfg), "--out", str(d / "ours" / "trace.parquet")],
                       capture_output=True, text=True)
    if o.returncode != 0:
        return i, "FASTSIM-CRASH", o.stderr.strip()[-300:]
    verdicts = []
    for f in ("trace.parquet", "message_trace.parquet"):
        a, b = pd.read_parquet(d / "ours" / f), pd.read_parquet(d / "ref" / f)
        if not a.equals(b):
            n = min(len(a), len(b))
            diff = next((k for k in range(n) if not a.iloc[k].equals(b.iloc[k])), n)
            verdicts.append(f"{f}: rows {len(a)}/{len(b)} first diff at {diff}")
    return i, ("OK" if not verdicts else "MISMATCH"), f"{len(pd.read_parquet(d / 'ref' / 'trace.parquet'))} rows; " + "; ".join(verdicts)


def main() -> int:
    ap = argparse.ArgumentParser()
    ap.add_argument("n", nargs="?", type=int, default=40)
    ap.add_argument("--seed", type=int, default=1)
    ap.add_argument("--jobs", type=int, default=os.cpu_count() or 4)
    ap.add_argument("--out", default=None)
    ap.add_argument("--bin", type=Path, default=FASTSIM)
    ap.add_argument("--mixed", action="store_true", help="agents of one kind at mixed intervals")
    ap.add_argument("--min-draws", type=int, default=0, help="grow drawing agents to this many draws")
    ap.add_argument("--scale", action="store_true", help="throughput-scale markets (see the module notes)")
    ap.add_argument("--max-wakeups", type=float, default=60_000, help="--scale: agent wakeups per scenario")
    ap.add_argument("--big-share", type=float, default=0.1, help="--scale: share allowed 4x the wakeups")
    ap.add_argument("--reuse", action="store_true", help="keep ABIDES outputs of identical earlier cases")
    ap.add_argument("--timeout", type=float, default=None, help="seconds an ABIDES case may take (then ABIDES-TIMEOUT)")
    args = ap.parse_args()
    rng = random.Random(args.seed)
    base = str(Path(args.out or tempfile.mkdtemp(prefix="fuzz-")).resolve())
    if args.scale:
        scenarios = [rand_scale_scenario(rng, i, args.max_wakeups * (4 if rng.random() < args.big_share else 1))
                     for i in range(args.n)]
    else:
        scenarios = [rand_scenario(rng, i, args.mixed, args.min_draws) for i in range(args.n)]
    work = [(i, sc, base, str(args.bin.resolve()), args.reuse, args.timeout) for i, sc in enumerate(scenarios)]
    counts: dict[str, int] = {}
    verdicts = {}
    with ProcessPoolExecutor(max_workers=args.jobs) as ex:
        for done, (i, verdict, detail) in enumerate(ex.map(one, work), 1):
            counts[verdict] = counts.get(verdict, 0) + 1
            verdicts[f"{i:04d}"] = [verdict, detail]
            if verdict != "OK":
                print(f"{verdict:14s} #{i:04d} {detail}   ({base}/{i:04d})", flush=True)
            if done % 50 == 0:
                print(f"... {done}/{args.n}: {counts}", flush=True)
    (Path(base) / "summary.json").write_text(json.dumps({"args": {k: str(v) for k, v in vars(args).items()},
                                                          "counts": counts, "verdicts": verdicts}, indent=1))
    print(f"{counts} over {args.n} random scenarios (outputs in {base})")
    valid = args.n - counts.get("ABIDES-ERROR", 0) - counts.get("ABIDES-TIMEOUT", 0)
    return 0 if valid > 0 and counts.get("OK", 0) == valid else 1


if __name__ == "__main__":
    raise SystemExit(main())
