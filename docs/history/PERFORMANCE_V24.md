Release 1.23 ("v24") is release 1.22 measured again in the Final's real input layout, plus four
changes: the agent-draw lookahead works in groups by wakeup interval, a single scenario streams its
outputs by default, LLVM lays out blocks by the profile's ext-TSP order, and PGO trains with
`FASTSIM_STREAM=1`. Every trace and ledger file of the public units is byte-identical to release
1.22's, in both input layouts.

## The Final mounts the card

On track3#35 (9 October) the organizers stated that the official runner, in Development and in the
Final, mounts each unit's input folder at `/input` as a whole: the scenario input and `card.toml`.
Only the repository's local harness (`throughput/run_unit.py`) copies the scenario alone. Every
earlier "Final layout" measurement here (no card, ledger always written) therefore described a
layout the Final does not use. With the card, release 1.22 already leaves out the ledger exactly
where the card says it is not required (the six single throughput-scale units), and the image's
`FASTSIM_STREAM=1` keeps it streaming.

## Re-baseline (x86 lab, release 1.22, card mounted)

Containers as the official runner starts them (`--network=none --cpus=4 --memory=16g`,
`-e NVIDIA_DRIVER_CAPABILITIES=compute`), the GPU through CDI (`--device nvidia.com/gpu=all`, as
Docker 29.7.2 on the platform does), a warm-up and four counted runs per unit, StartedAt to
FinishedAt (`benchmarks/v24-final-layout-2026-10-11/finalrace.py`):

- The lab's container overhead is about 102 ms per unit (window minus the process's own time).
- One unit's window varies by 10-20 ms between runs, so four counted runs cannot resolve changes
  under about 1%: the comparisons below use whole-process times and `perf` cycles of native runs
  in the same layout, and a score proxy mean(rows / (F + own time)).
- Own time (card mounted, `FASTSIM_STREAM=1`): 439.6 ms over the 71 units, median 4.26 ms. The
  five big gb units are 5.5-8.5% faster than without the card (no ledger).

Score sensitivity at F = 102 ms: 1 ms less on every unit is worth +0.85%; 10% less own time on every
unit +1.28%; gb-mega alone 10% faster +0.39%. The 61 units under 10 ms hold 47% of the score.

Phase marks (65 single units, weighted by that sensitivity): the event loop is worth +13.3% if it
took no time at all; start-up plus exit (about 0.45 ms per unit) +0.43%; the final write and hash
+0.38% (1.6 ms on gb-mega on the lab's i7, which hashes in software; the Xeon has SHA-NI); every
other phase under 0.07%. A main-thread profile of gb-mega (40 runs) is flat: the queue is about a
quarter, and no source line takes more than 3%.

CPU quota: under `--cpus=4`, gb-mega uses 146 ms of CPU in 50.6 ms (2.9 CPUs) and a gb-mega with a
five times longer horizon (5.8 million events) the same 2.9 CPUs; no period was throttled.

## The changes

### Agent-draw lookahead by wakeup interval

The helper thread that computes noise and value traders' draws ahead sized one buffer per kind by
the fastest agent (`rounds = max over agents of ceil(span / interval) + 8`) and filled every agent
for every round, while the event loop waited for round `k` to read any agent's `k`-th draw. All 95
public scenarios have one arrival rate per agent kind, where this is exact sizing; but in a sealed
scenario mixing rates within a kind (say one 1,000 Hz noise trader and 127 at 1 Hz over 60 s) the
buffer would take about 117 MiB and the event loop would wait for the helper to draw 128 agents per
round of the fast one, about 0.6 s of draws for a run of tens of milliseconds.

Agents of one kind and interval now form a group with its own round-major buffer and its own
`done` count, sized to that interval's wakeups. The helper always fills the round needed first
(the smallest `round * interval` over all groups, noise before value on a tie). Each agent still
draws from its own generator in order, and an agent that outlasts its group's rounds continues
from the helper's final generator, so every draw is the same; on the public units, which form one
group per kind, the helper does the same work as before.

The first version found a slot's group, then the group's buffer and count, on every read: 0.5-1.1%
more cycles on the gb units. Each slot now holds everything a read needs (its first draw, stride,
rounds, a cached `done` and a pointer to its group's), which measured as release 1.22 (within
0.2%).

### Streaming by default

`streams()` returned "write after the run" when the card was visible, a default from when
Development scored the simulation window alone. Development and the Final now both time the whole
container and both mount the card, so a single scenario always writes while it runs unless
`FASTSIM_STREAM=0`. The image keeps `FASTSIM_STREAM=1`, so nothing changes on the platform; the
gVisor detection that only served this choice is gone (one `uname` call less at start-up).

### ext-TSP block placement

`-C llvm-args=-enable-ext-tsp-block-placement` in the Dockerfile's `RUSTFLAGS`: LLVM orders each
function's blocks with the profile's ext-TSP model. Layout only; the outputs are the same.

### PGO trained with FASTSIM_STREAM=1

`tools/pgo_build.py` sets `FASTSIM_STREAM=1` for the training runs, as the image does; before,
the training round with the card trained the write-after-the-run path no release runs. The
alternate round without the card is kept (it trains the ledger of the throughput-scale units).

## Measurements

Lab-native PGO builds (same Rust 1.98.1, static, x86-64-v3; the lab's glibc), two builds per
variant, all 71 units in the card layout, `perf` cycles (user), 10 runs per label, labels
interleaved, one batch per table. Build-to-build noise is about 0.2% (b2 against b1).

Variants measured on the way (cycles against b1, release 1.22's source):

| Variant | gb-mega, pop-horizon, horizon-240, highfreq | All 71 units (geomean) |
|---|---:|---:|
| PGO without the no-card round, `FASTSIM_STREAM=1` | +0.8% to +1.75% | +0.5% to +0.8% |
| PGO with `FASTSIM_STREAM=1` only | +0.2% to +0.6% (b2: +0.56%) | +0.1% to +0.2% |
| `-split-machine-functions` | +0.3% to +0.8% | +0.4% to +0.6% |
| ext-TSP block placement | -0.4% to -0.8% | -0.6% to -0.9% |
| grouped lookahead, first version | +0.5% to +1.1% | +0.6% to +0.8% |
| grouped lookahead, per-slot locations | -0.2% to +0.2% | -0.1% to +0.2% |
| streaming default only | +0.0% to +0.3% | -0.1% to +0.1% |

The release candidate (all four changes) against release 1.22's source, one batch:

| Build | The four big gb units | All 71 units (geomean) |
|---|---:|---:|
| b2 (1.22, second build) | +0.09% | -0.04% |
| 1.22 + ext-TSP | -0.38% / -0.54% | -0.71% / -0.74% |
| 1.23 with the old PGO training | -0.40% / -0.34% | -0.71% / -0.60% |
| **1.23** | **-0.52% / -0.65%** | **-0.77% / -0.95%** |

About 0.8% fewer cycles everywhere. The cycles count every thread together; for what the run's
own time gained, see the release binaries' wall time below.

## Correctness

- `cargo test --release`: 37 tests pass, two of them new: `grouped_lookahead_keeps_each_agents_stream`
  (each slot's draws and final generator, over groups of mixed intervals) and
  `mixed_interval_agent_draws_match_draws_in_place` (a scenario with three noise and two value
  intervals, the lookahead on, and its room cut short for the fast groups only). Mutation checks:
  ignoring a slot's place in its group fails three tests; handing the final generators back out of
  slot order fails both new ones.
- `tools/compare_unit.py`: 95/95 scenarios identical to the reference, 95/95 pass the official
  gates.
- Differential fuzzing against the pinned ABIDES (`tools/fuzz_vs_abides.py`, new `--mixed` and
  `--min-draws` options: more groups of a kind at other intervals, grown past the lookahead's
  threshold): 136 + 57 + 59 mixed-interval scenarios and 55 ordinary ones identical, 0 mismatches
  (23 cases where ABIDES itself raises `KeyError: 'order_id'`). 75 of the mixed ones ran the
  lookahead, 69 with several intervals in one kind; the last 59 (39 and 38 of these) ran the
  release's code, the earlier ones its first version, which draws the same.
- Lab builds of 1.23 and 1.22: every stable output byte-identical in both input layouts, 71 units.

## Release

- **Image:** `fastsim-t3:1.23`, manifest `sha256:376ffd23273e477f4d61b8c888a1d2f0671018e38f212d4ce7a0b2300dfb9e35`,
  release 1.22's layout (one layer, binary in `/bin`, `WorkingDir=/etc/fastsim`). Binary sha256
  `6e75bb25…` (1,859,056 bytes, static PIE, x86-64-v3).
- **Validation:** `validate.py` 71/71 units in both input layouts (142 runs), 0 failures; all 374
  parquet files byte-identical to release 1.22's validation record.
- **The image on the lab's GPU:** CDI and legacy (`--gpus`) injection, card and staged layouts,
  every unit's stable outputs byte-identical to release 1.22's (`finalrace.py --cross-check`).
  Final-style windows of 1.23 against 1.22, warm-up + 4 (CDI, card) or warm-up + 1: +1.10% (CDI,
  card; 95% bootstrap interval over the counted runs -0.49% to +2.21%), +0.70% (CDI, staged),
  -0.07% (legacy, card), +0.58% (legacy, staged): the windows' noise is larger than the change.
- **The release binaries** (both built by `sub/tools/build.py`), card layout, 12 runs per unit:
  - `perf` cycles, every thread together: -1.87% on the four big gb units (gb-mega -1.64%,
    pop-horizon -1.80%, horizon-240 -2.49%, highfreq -1.78%), -1.59% geomean over the 71 units, 70
    faster and none slower. That is more than the lab-built pairs above (about -0.8%): PGO builds
    of one source vary, and release 1.22's binary is one draw.
  - Whole-process wall time without `perf`, two batches with the label order reversed: -2.24% and
    -2.13% over the 71 units, -3.8% and -4.0% on the four big gb units. The score proxy
    mean(rows / (F + own time)) gains +0.41% and +0.42% at F = 102 ms (95% bootstrap over the runs
    of the first batch +0.38% to +0.46%), +0.31% and +0.32% at F = 150 ms. The wall time gains more
    than the cycles because the cycles count every thread, while the run's time is the main
    thread's, about a third of gb-mega's CPU time.
  - A first version of this note gave +0.5% from wall times taken around `perf stat`, which adds
    about 12 ms to every run. That figure is withdrawn.
  - These are proxies on the lab, not a prediction of the board: the platform's fixed cost per run
    is unknown, and the container race above cannot resolve the change.
- **Published:** `docker.io/greedyshark/fastsim-t3@sha256:376ffd23273e477f4d61b8c888a1d2f0671018e38f212d4ce7a0b2300dfb9e35`
  (anonymous pull 200). ZIPs `sub/submissions/submission-1.23-376ffd23-dev.zip` (= `v24.zip`,
  sha256 `4ef66cf0…`) and `submission-1.23-376ffd23-final.zip` (sha256 `237a65dc…`); `check_zip.py`
  passed both.

Release records: `sub/releases/1.23/`.
