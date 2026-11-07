Release 1.7 ("v8") turns transparent huge pages off for the process. The 64 MiB arena top pad
that v7 added for gVisor made the simulation slower on a native kernel with THP set to `always`.
Every output file is byte-identical to v7's and v6's.

## What v7's Development result showed

- **Primary score.** v7 scored 9,990,095 against v6's 10,000,000. The whole difference is
  `t3-s001-price-time-priority`, the only unit below the 1e7 cap. Its 604 events took 65.0 us
  against 57.2 us in v6, and the cap needs 60.4 us or less. Its window varies by about 30% from run
  to run locally (p10 35 us, p90 46 us over 300 runs), with no difference between v6 and v7, so
  s001 lands on either side of the cap by chance.
- **Per-unit noise.** Between the two submissions, per-unit rates moved with a standard deviation
  of about 7%, and the geometric mean was unchanged (0.998).
- **The 26 mid-size single scenarios** (60-130k rows, ledger written) were 2.4% slower on average.
  That is more than their noise explains, and it reproduced locally.

## Cause: the arena top pad with transparent huge pages

Local measurements ran in the Linux VM with `--cpus 4`, the card visible (the Development
layout), 15 alternating runs, on 16 units. The ratios are v7 variants against v6, as a geometric
mean of the `Sim::run` window:

| v7 variant | Window vs v6 |
|---|---:|
| v7 | 0.999 |
| v7 without the 64 MiB top pad | **0.953** |
| v7 without growing latency blocks | 1.042 |
| v7 notifying on every pipeline job | 1.002 |
| v7 with v6's `TraceBuilder` allocation | 0.999 |

- **What is behind the table.** The top pad cost about 4.6% in every unit, and the latency blocks
  gained about 4%, so v7 looked like v6 locally.
- **THP on or off.** The VM kernel sets THP to `always`. With THP turned off for the process
  (`PR_SET_THP_DISABLE` in a wrapper), the top pad's cost disappeared (ratios 1.000-1.013; with
  THP on, 1.027-1.066).
- **Not page faults.** The window took fewer faults with the pad (3 against 4-26), so the cost
  comes from memory backed by 2 MiB pages, not from faults.
- **Why v6 didn't pay it.** Without the pad the heap grows in small steps, rarely covers an aligned
  2 MiB range, and behaves as it does with THP off.
- **The platform.** Its THP setting is unknown. `always` is the default on RHEL-like systems.

## Options measured

- **A:** keep the top pad and call `prctl(PR_SET_THP_DISABLE)` at startup.
- **B:** use the top pad only in `simulate-batch`.

Native windows are nine runs of all 71 units; gVisor is the proxy score from the Final's formula
(runsc `release-20260803.0`, see PERFORMANCE_V7.md), seven runs of all 71 units.

| | Native window vs v6 | gVisor proxy score | gVisor in-process time, 12 units, vs v7 |
|---|---:|---:|---:|
| v6 | 1.000 | 1.928M | |
| v7 | 0.996 | 2.106M | 0 |
| **A** | **0.952** | **2.113M** | **-0.3 ms (noise)** |
| B | 0.951 | 2.054M | +11.3 ms (all 12 slower) |

gVisor rejects `PR_SET_THP_DISABLE` with `EINVAL`, and its application memory never uses the
host's THP, so A is v7 in the Final. B loses the top pad's gVisor gain on single scenarios.

## Change

- `main` calls `prctl(PR_SET_THP_DISABLE, 1)` right after the `mallopt` settings, before any large
  allocation. The flag applies to the process's later faults. The call's result is ignored: it
  fails inside gVisor, where it has nothing to do.

## Measurements

The same comparison as above, with release 1.7's own source (static linux/arm64, seven runs of
all 71 units, card visible):

| Category (units) | v7 window vs v6 | v8 window vs v6 |
|---|---:|---:|
| Tiny, below 20k rows (10) | 1.003 | 0.961 |
| Small, 20-60k (17) | 1.002 | 0.953 |
| Mid, 60-130k (26) | 0.997 | 0.954 |
| Large, above 130k (6) | 0.998 | 0.964 |
| Throughput units without a ledger (6) | 0.968 | 0.951 |
| Batch (6) | 0.921 | 0.955 |
| All 71 | 0.990 | **0.955** |

- **Batch.** Batch subs do not populate memory before their clock, so their first touches happen
  inside it, and huge pages had helped them. v8's batches are 0.7-3.7% slower than v7's (two runs).
  They are all far above the 1e7 cap, so the Development primary is unaffected, and the Final's
  gVisor runs ignore the setting.
- **The Development score.** Its capped mean is 10.000M for every build in the VM. The uncapped
  mean is 17.50M for v6, 17.67M for v7 and 18.32M for v8.

## Validation

- All Rust tests and the 20 packaging tests pass.
- `tools/compare_unit.py`: 95/95 public scenarios identical to the reference, and 95/95 pass the
  official gates.
- **Byte identity with v6** on all 71 units (190 Parquet files, static linux/arm64, no card).
- **The release validation**: 71 units × 2 input layouts passed. All 374 Parquet files it wrote
  are byte-identical to those of releases 1.5 and 1.6.
