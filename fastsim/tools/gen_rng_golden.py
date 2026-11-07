#!/usr/bin/env python3
"""Golden vectors for the Rust `RandomState` port.

Replays a random script of legacy `np.random.RandomState` calls (every method and parameter shape
the ABIDES baseline uses, interleaved so the gaussian cache and the rejection loops are exercised
across MT19937 regenerations) and records each result exactly: integers as ints, doubles as their
IEEE-754 bit patterns. The Rust test `tests/rng_golden.rs` replays the same script.

Usage: python gen_rng_golden.py OUT.json
"""

from __future__ import annotations

import json
import math
import platform
import random
import struct
import sys

import numpy as np


def fbits(x: float) -> int:
    return struct.unpack("<Q", struct.pack("<d", float(x)))[0]


# (name, int args, float args). Float args are shipped as bit patterns so both sides use the
# identical double.
OPS: list[tuple[str, list[int], list[float]]] = [
    ("random_sample", [], []),
    ("randint", [0, 2], []),  # NoiseTrader side / oracle megashock sign
    ("randint", [0, 6], []),  # NoiseTrader offset, price_offset_ticks=5
    ("randint", [0, 11], []),
    ("randint", [0, 4], []),  # power-of-two range: mask == rng
    ("randint", [-5, 5], []),
    ("randint", [0, 1000], []),
    ("randint", [7, 8], []),  # rng == 0: no draw at all
    ("randint", [0, 2**31 + 12345], []),
    ("randint", [0, 2**40], []),  # 64-bit masked path (next_u64 word order)
    ("randint", [10**12, 10**15], []),
    ("randint_u64", [0, 2**32], []),  # ABIDES seed derivation
    ("randint_u64", [5, 2**33], []),
    ("standard_normal", [], []),
    ("normal", [], [10.0, 2.0]),  # NoiseTrader order size
    ("normal", [], [0.0, 0.0]),  # scale 0 still consumes the stream
    ("normal", [], [100000.0, math.sqrt(1000.0)]),  # oracle observation noise
    ("normal", [], [1000.0, math.sqrt(50000.0)]),  # megashock value
    ("lognormal", [], [math.log(1000.0), 0.5]),  # latency, log_normal model
    ("lognormal", [], [math.log(250000.0), 1.2]),
    ("uniform", [], [100.0, 5000.0]),  # latency, uniform model
    ("uniform", [], [-1.0, 1.0]),
    ("standard_exponential", [], []),
    ("exponential", [], [1.0 / 2e-9]),  # megashock inter-arrival, jump_intensity=2/s
    ("exponential", [], [1.0 / 2.77778e-18]),  # megashock inter-arrival, default rate
    ("pareto", [], [1.5]),  # latency, pareto model
    ("pareto", [], [3.0]),
]


def call(rs: np.random.RandomState, name: str, ia: list[int], fa: list[float]) -> int:
    """Run one op; return an int result, or a double's bit pattern."""
    if name == "random_sample":
        return fbits(rs.random_sample())
    if name == "randint":
        return int(rs.randint(ia[0], ia[1]))
    if name == "randint_u64":
        return int(rs.randint(ia[0], ia[1], dtype=np.uint64))
    if name == "standard_normal":
        return fbits(rs.standard_normal())
    if name == "normal":
        return fbits(rs.normal(fa[0], fa[1]))
    if name == "lognormal":
        return fbits(rs.lognormal(fa[0], fa[1]))
    if name == "uniform":
        return fbits(rs.uniform(fa[0], fa[1]))
    if name == "standard_exponential":
        return fbits(rs.standard_exponential())
    if name == "exponential":
        return fbits(rs.exponential(fa[0]))
    if name == "pareto":
        return fbits(rs.pareto(fa[0]))
    raise KeyError(name)


def main() -> int:
    out_path = sys.argv[1]
    picker = random.Random(20260924)
    seeds = [0, 1, 2, 42, 12345, 2**31 - 1, 2**31, 2**32 - 1]
    seeds += [picker.randrange(0, 2**32) for _ in range(32)]
    n_ops = 2500

    runs = []
    for seed in seeds:
        rs = np.random.RandomState(seed)
        records = []
        for _ in range(n_ops):
            idx = picker.randrange(len(OPS))
            name, ia, fa = OPS[idx]
            records.append([idx, call(rs, name, ia, fa)])
        _, key, pos, has_gauss, gauss = rs.get_state()
        runs.append(
            {
                "seed": seed,
                "records": records,
                "final_pos": int(pos),
                "final_has_gauss": bool(has_gauss),
                "final_gauss_bits": fbits(gauss),
            }
        )

    def state_of(seed: object) -> dict[str, object]:
        _, key, pos, _, _ = np.random.RandomState(seed).get_state()
        return {"key": [int(k) for k in key], "pos": int(pos)}

    seed_states = [{"seed": s, **state_of(s)} for s in (0, 42, 2**32 - 1)]
    array_seed_states = [
        {"seed_array": a, **state_of(np.array(a, dtype=np.int64))}
        for a in ([1, 2, 3], [0], [2**32 - 1, 5], list(range(700)))
    ]

    # The global generator exactly as the ABIDES baseline drives it: np.random.seed(s), then
    # `np.random.randint(low=0, high=2**32, dtype="uint64")` per derived RandomState, with the
    # oracle's global `np.random.exponential` interleaved after the first derivation.
    chains = []
    for s in (0, 42, 7, 123456789):
        np.random.seed(s)
        first = int(np.random.randint(low=0, high=2**32, dtype="uint64"))
        exp_bits = fbits(np.random.exponential(scale=1.0 / 2.77778e-18))
        rest = [int(np.random.randint(low=0, high=2**32, dtype="uint64")) for _ in range(8)]
        chains.append({"seed": s, "first": first, "exp_bits": exp_bits, "rest": rest})

    doc = {
        "numpy_version": np.__version__,
        "platform": f"{platform.system()}-{platform.machine()}",
        "ops": [{"name": n, "i": ia, "f": [fbits(x) for x in fa]} for n, ia, fa in OPS],
        "runs": runs,
        "seed_states": seed_states,
        "array_seed_states": array_seed_states,
        "chains": chains,
    }
    with open(out_path, "w") as fh:
        json.dump(doc, fh, separators=(",", ":"))
    n = sum(len(r["records"]) for r in runs)
    print(f"wrote {out_path}: {len(runs)} seeds, {n} records, numpy {np.__version__}, {doc['platform']}")
    return 0


if __name__ == "__main__":
    raise SystemExit(main())
