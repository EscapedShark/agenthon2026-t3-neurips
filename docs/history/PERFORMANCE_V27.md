Release 1.26 ("v27") is release 1.25 with one correctness fix. The oracle now compares a megashock's
time with an observation's time the way NumPy does, not exactly. Every trace and ledger file of the
public units is unchanged.

The binary is the best of twelve native PGO draws:

- 0.15% more own time than release 1.25, whose draw was an unusually fast one;
- faster than two fresh draws of 1.25's own source, made the same day;
- the same instruction counts as 1.25.

## The mismatch

The mixed-interval campaign on release 1.25 ran 600 scenarios (`fuzz_vs_abides.py 600 --mixed
--min-draws 12000 --seed 20261013`):

- 577 were identical to ABIDES;
- in 22, ABIDES itself raised `KeyError: 'order_id'`;
- 1 differed: case 0267.

Releases 1.22 to 1.25 give byte-identical outputs on case 0267, so the bug is older than all of them.

Case 0267 is a 2-second market:

- 64 value traders at 100 Hz and 8 at 5 Hz;
- 96 noise traders at 5 Hz;
- 2 megashocks per second;
- no `latency_config`, so the fixed pairwise latencies apply.

The outputs agree up to row 11,488 of 11,615, at 1.993 s. There, value trader 4 receives its spread
quote. fastsim's trader sells 12 at the bid; ABIDES's trader does nothing.

## The cause

A megashock is due at 1612517401993112832 ns. Trader 4 observes the fundamental 90 ns later, at
...112922.

- **ABIDES.** `mkt_open` is an `np.int64`, because `str_to_ns` returns NumPy integers. So the
  megashock times, `mkt_open + exponential` and then `float + int(...)`, are `np.float64`.
  - NumPy compares an `np.float64` with an int (NumPy's or Python's) by converting the int to a
    double first. At 1.6e18 a double is a multiple of 256 ns, and ...112922 rounds to ...112832.
  - So `mst < current_time` is false: trader 4 sees the old fundamental, 50,920, and observes
    50,900. The book has no asks to buy from, and the bid (50,042) is not above the observation,
    so it does not trade.
  - The jump (-1,212.5) is applied at the next observation, agent 8's at ...228506. Its OU step is
    taken from ...112922, as a double ...112832, to the megashock time, so `d = 0`. The new
    fundamental is 49,707.
- **fastsim.** It compared exactly, as Python compares a Python `float` with an `int`. It applied
  the jump at trader 4's observation, so trader 4 saw 49,707 and sold at the bid.

## The fix

`src/sim/oracle.rs` makes its three comparisons of a megashock time with an integer time round the
integer to a double first, as NumPy does:

- the megashock time `<` the observation time (`while mst < current_time`);
- the observation time `<=` the previous time (never a float in practice);
- the megashock time `>=` the scheduled jump's time (`ts >= jump["time_ns"]` inside the megashock's
  step).

The test `megashock_times_compare_as_numpy_doubles` checks fundamentals from ABIDES's own oracle
(NumPy 1.26.4, `oracle_golden.py`). They are observed 90, 128 and 300 ns after each of the first
four megashocks of three oracles:

- at +90 ns the megashock is not yet applied;
- +128 ns is a tie, and it is applied only when the double rounds up;
- the third oracle has a scheduled jump 64 ns after its first megashock, with strong reversion, so
  the jump's position relative to the OU step changes the value.

The old code fails the test at the first +90 ns observation. With only the scheduled-jump
comparison left exact, it fails at the third oracle.

## How often it happens

A hit needs an observation within 128 ns after a megashock. Per megashock, that is about 128 ns ×
the value traders' observation rate, when their observations are spread out.

- **Public units.** 13 of the 95 scenarios have megashocks, but only 12-48 value-trader
  observations per second and 1-18 megashocks per run. That gives 0.0002 expected hits over all 95,
  and no public output changes.
- **Case 0267.** 6,400 observations per second.
- **A targeted campaign** (`jumpfuzz.py`). 600 scenarios with 16-384 value traders at 50-400 Hz,
  50-1,000 megashocks per second, every latency model and both exchange modes. ABIDES runs
  instrumented, and its oracle log counts the hits.
  - The 600 runs had 396,333 megashocks. 189 scenarios hit the boundary, 557 times in all.
  - Release 1.26 is identical to ABIDES in all 600.
  - Release 1.25 differs in 120, all of them scenarios with hits. In the other 69, the early jump
    changed no trader's decision.
  - The 411 scenarios without a hit are identical with both releases.

## Correctness

- **Tests:** `cargo test --release` passes 46 tests, including the new one.
- **Reference comparison:** `tools/compare_unit.py` gives 95/95 scenarios identical to the
  reference and 95/95 passing the official gates.
- **Differential fuzzing.** The fixed source rechecked against every cached ABIDES reference:
  - 578 + 562 cases from the 1.25 campaigns (mixed, now including case 0267; ordinary);
  - 57 + 56 from the 1.24 campaigns;
  - 999 + 16 from the scale campaign and its calibration;
  - the 6 dense variants.

  That is 2,274 cases, all identical (`recheck.py`).
- **The release binary on the boundary scenarios.** On the lab, the release binary's outputs for
  the 600 boundary scenarios are byte-identical to the fixed Mac build's, so identical to ABIDES
  (`hashrun.py`). Release 1.25's match in 480; its 120 others are the campaign's mismatches.
- **Validation:** `validate.py` passes 71/71 units in both input layouts (142 runs, 0 failures).
  All 374 parquet files are byte-identical to release 1.25's validation record.

## The release's PGO draw

The 1.26 context differs from 1.25's in `oracle.rs` only. The lab (i7-10700K) built it natively 12
times (`n26a`-`n26l`), and the 1.25 context twice more (`n25d`, `n25e`). Each was measured against
release 1.25 (`rel25`): 71 units in the card layout, whole-process wall time, 10 runs per unit and
label, in batches with the label order reversed.

| Draw | Source | Batches | Own time | gb4 | Proxy at F = 102 ms |
|---|---|---:|---:|---:|---:|
| **`n26j` (released)** | 1.26 | 4 | **+0.15%** (+0.11 to +0.20) | +0.29% | **-0.032%** |
| `n26g` | 1.26 | 4 | +0.28% | +0.04% | -0.025% |
| `n26a` | 1.26 | 8 | +0.31% | +0.36% | -0.046% |
| the other nine | 1.26 | 2 each | +0.44% to +0.74% | -0.11% to +0.65% | -0.040% to -0.080% |
| `n25e` | 1.25 | 2 | +0.52% | +0.96% | -0.101% |
| `n25d` | 1.25 | 2 | +0.72% | +0.01% | -0.059% |

- **The gap is the draw, not the fix.** The two fresh draws of 1.25's own source are slower than
  `n26j`. Release 1.25 was the best of three native draws of its context; the other two were 0.0%
  and 0.4% slower (`benchmarks/v26-latency-nonpie-2026-10-11/release-draws/nat_*.json`).
- **Instruction counts match.** `perf` (3 runs per unit) puts instructions at -0.01% for `n26j`
  and `n25d` alike, and -0.05% to +0.00% for the other 1.26 draws. Cycles: `n26j` +0.03%, `n25d`
  +0.14%, both geomean.
- **The cost.** -0.03% proxy is about 70 points of a 213,000 score, under every noise the scores
  carry.

The released image on the lab's GPU: CDI injection, card layout, warm-up + 4 counted runs per unit.
Every unit's stable outputs are byte-identical to release 1.25's (`finalrace.py --cross-check`).
Final-style windows: -0.23% against 1.25, and -0.01% with the image order reversed. Dev-style
(create to remove): -0.26% and +0.46%. 1.25's races against 1.24 ranged from -0.76% to +1.66%.

## Release

- **Image:** `fastsim-t3:1.26`, manifest
  `sha256:ddea2096b66000e6856f7b8aede5ebe25c89bd53858828f973a45cfc9f0a7320`, binary sha256
  `9c59e650…` (1,806,320 bytes, static, ET_EXEC), context `00dc0cf7…`.
  - Native draw `n26j`, adopted with `build.py --adopt` (`sub/releases/1.26/adopted.json`).
- **Published:** `docker.io/greedyshark/fastsim-t3@sha256:ddea2096b66000e6856f7b8aede5ebe25c89bd53858828f973a45cfc9f0a7320`.
  - The anonymous manifest request returns 200, and the lab pulled it anonymously.
- **ZIPs:** both pass `check_zip.py`.
  - `sub/submissions/submission-1.26-ddea2096-dev.zip` (= `v27.zip`, sha256 `66851737…`);
  - `submission-1.26-ddea2096-final.zip` (sha256 `f7b166ca…`).

Release records: `sub/releases/1.26/`. Measurements: `benchmarks/v27-megashock-compare-2026-10-12/`.
