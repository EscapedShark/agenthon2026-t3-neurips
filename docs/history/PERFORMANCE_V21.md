Release 1.20 is built and validated. This round optimized the actual source and released binary of
1.19. On the 71 public units, the released binary is faster on every unit on the local x86 machine:
the sum of per-unit median whole-process times fell **7.24%**, and the largest scenario fell
**13.57%**. No image was pushed and nothing was submitted in this round.

The measurement host is an i7-10700K with a Tesla T10, and the Final's host is a Xeon Platinum 8570
with a B200. The numbers below are local results on the public set, not scores on the sealed set or
the official host.

The official Final description was rechecked on 2026-10-09. The runtime is **runc**. Timing takes
Docker's `State.StartedAt` to `State.FinishedAt`, which includes runtime and GPU start-up, and
excludes create, inspect and remove. Each run's rate is verified rows over that window; a unit takes
the median of its repeats, then units are averaged. Repeat and warm-up counts follow the official
plan. The local official repository was at `bffb57227f796f9fa769a23d01fc364bee14a119`. Historical
gVisor data was not used to judge this round.

Two changes were kept.

1. **Compile for x86-64-v3 explicitly, and retrain PGO.** Release 1.19 targeted generic x86-64. The
   new target lets the compiler use AVX2, BMI and similar instructions, and it is a fixed target, so
   it does not depend on the build machine's `native` features. PGO's generate and use stages take
   the same flags, and each candidate gets a fresh profile. Training reads public scenario inputs
   only, and the final run still computes everything, with no cached reference output. The release
   build stays on Rust 1.98.1, Debian 13, glibc 2.41 and static linking.
2. **Keep one message type tag.** The old structure held a `Payload` enum, a message type inside it,
   and `Msg.kind`. The new one keeps a single `MsgKind` in the 40-byte payload and dispatches on it
   directly. Four full i64 fields hold order, price or period values, and a bit marks whether a
   spread is present. `Msg` stays 64 bytes and cache-line aligned. No order field was narrowed, no
   special price stands for empty, and no `unsafe` was added. `None` against `Some(0)` and the i64
   extremes have tests.

Low-level counters support this direction. In a separate PGO experiment, moving gb-mega from the
generic target to v3 cut whole-process instructions by 15.50% and cycles by 12.40%, with branch
misses about the same. The message change took about 1.35% more cycles off on v3. Most of the gain
comes from executing fewer instructions and simpler dispatch, not from removing event-order
dependencies. These are splits from experimental binaries; the release's numbers come from the
actual 1.20 image and its extracted binary.

Native comparison of the released binaries. The process exits after writing the trace and ledger,
with no card and `FASTSIM_STREAM=1` as in the released image. Each version warms up once and is
measured 12 times, with the order rotated and no two versions running at once.

| Unit | 1.19 process ms | 1.20 process ms | Change |
|---|---:|---:|---:|
| gb-mega-throughput | 61.225 | 52.916 | -13.57% |
| gb-horizon-240s | 30.383 | 26.628 | -12.36% |
| gb-highfreq-40hz-60s | 27.158 | 23.946 | -11.83% |
| gb-pop-horizon-scale | 38.533 | 34.668 | -10.03% |
| gb-pop-128-agents | 16.989 | 16.092 | -5.28% |
| mr-cancel-replace-churn | 18.360 | 17.559 | -4.36% |
| mr-deep-book-state-size | 20.738 | 20.475 | -1.27% |
| Sum of medians, all 71 units | 482.310 | 447.369 | -7.24% |

The sum of medians shows program cost; it is not a Final score. The Final-style container comparison
uses the released images: 4-CPU quota, 16 GiB, non-root, read-only root, no network, a GPU request
and `NVIDIA_DRIVER_CAPABILITIES=compute`, still with no card. Each run reads the daemon's timestamps
and compares every stable output's bytes with the validated 1.19 baseline. Each unit's first run is a
warm-up, then 7 runs per version, then the whole thing again with the versions in reverse order.

| GPU container comparison | 1.19 local score | 1.20 local score | Gain |
|---|---:|---:|---:|
| First round, 7 runs per unit | 763,729.77 | 773,994.57 | +1.34% |
| Reversed round, 7 runs per unit | 771,153.42 | 776,950.58 | +0.75% |
| All 14 runs, unit medians recomputed | 769,394.40 | 775,757.67 | **+0.83%** |

The two rounds ran 2,272 containers including warm-ups, and every stable file matched. Both rounds
gained, but start-up noise changes the size. The combined score is not the mean of the two totals.
Resampling the repeats in pairs 2,000 times within each round and unit, over these 71 units, gives a
95% percentile interval of about **+0.23% to +1.54%**. That covers only local timing noise. It is not
the official family-resampled interval, and it says nothing about the sealed set or the target host.

To let the host user clean up outputs, the container script creates batch subdirectories in advance,
so it is a local timing comparison, not the official producer. It ties the rate's numerator to the
verified Parquet bytes and row counts. The formal container validation ran footer row counts and
gates g0 to g3 separately. Do not use the outer `docker run` time or the program time in
`events.json` as the Final's denominator.

The other candidates were built and dropped. The table gives their large-unit time change during
exploration; positive is slower.

| Candidate | gb-mega | gb-pop-horizon | Verdict |
|---|---:|---:|---|
| Store the free-list head in the message slab | -0.03% | +0.48% | No gain |
| Ledger switch as a const generic | +14.63% | +13.92% | Dropped |
| Load the message ahead of the queue pop | +1.32% | +2.88% | Dropped |
| Compare time first, then the full event key | +6.17% | +8.77% | Dropped |
| Ring bucket with 4 slots instead of 8 | -0.74% | +0.79% | No stable gain |
| Ring entry of 16 bytes instead of 20 | +1.37% | +2.13% | Rebuilding keys cancels the capacity gain |

In some two-candidate exploration scripts, rotation and reversal cancelled out and the order was
fixed, so those numbers served only for screening. The final native test has the order fixed up, and
the GPU container test rotates each round and repeats in reverse. All raw measurements were kept,
and no unfavourable unit was removed.

Correctness evidence:

- 35 Rust release tests and 20 packaging tests pass.
- The released `fastsim-t3:1.20` container ran 142 times, on 71 public units in two input layouts.
  All runs pass the official developer verifier g0 to g3, exact row counts and local checks.
- Against 1.19's validation record, all 184 Parquet files in the card layout and all 190 in the
  scenario-only layout are byte-identical.
- Native runs and both GPU container rounds checked stable outputs and row counts on every repeat,
  warm-ups included.
- Of 64 random configurations, 61 match the pinned ABIDES row for row, and 3 are ABIDES's own
  `KeyError: 'order_id'`, which are not counted as passes. Releases 1.19 and 1.20 give byte-identical
  outputs on all 64.
- All 200 file hashes of the release context were checked: the build inputs match the validated
  image's source.

Artifact: `fastsim-t3:1.20`, image digest
`sha256:d3300e248a99999856d423194edc5ba97deb5bf747b1fa309ebd4028e71db1d1`, binary SHA-256
`5a8569887e21e6eba2574f939c27c87bae80e677e6957533e034a02ede21ce91`, 1,881,008 bytes. The minimum ISA
is x86-64-v3. An older CPU needs a generic x86-64 build from the build context, with PGO retrained.

Where to work next, by the Final's denominator and the real hardware:

| Layer | State and next step |
|---|---|
| Compilation and layout | v3 and new PGO are in. Next, compare v3, the matching microarchitecture target and AVX-512 on a Xeon 8570, recording cycles, instructions and cache misses. The i7 cannot validate this, so local results cannot justify a higher ISA. BOLT or more code alignment should also wait for a target-machine profile. |
| Event queue and order book | 1.19's direct report sending, cache-line alignment and ring work stay. This round's six candidates show that fewer fields or branches does not mean faster. Sample the target machine's hottest instructions and their dependencies first. |
| Parallelism and GPU | Events in one market depend on time, message ids, RNG and the order book. Look-ahead and streaming are already parallel, and more threads may add synchronization. Moving work to a GPU would first need proof that batchable work outweighs submission and transfer cost; a B200's rated power does not imply a higher score. |
| Output and ledger | Streaming Parquet, write-while-computing and a scratch image exist. Without a card the ledger is still written, because an unpublished card may require it, and scenario names are no basis for skipping work. |
| Container start | On small scenarios, most of the 120 to 130 ms window is the start-up path. The official launcher overrides the image's GPU capability with `compute`, which no program sidecar can change. Measure the start window on the target machine, and do not count host tuning as an image improvement. |

A hypothetical model from the released binary's native measurements: at a fixed overhead of 125 ms,
making **all of 1.20** 10% faster adds about **1.16%** to the public-set score, and halving it adds
about **6.32%**. These are sensitivity calculations, not container measurements and not official
limits. They explain why a 7.24% faster program gains only about a percent of score.

The raw records were kept in this round's audit folder (not in this repository). To repeat the
comparison on an x86 Linux GPU host, with the same public inputs and nothing else running:

```sh
python3 containers.py --units "$UNITS" --images fastsim-t3:1.19 fastsim-t3:1.20 --repeats 7 --out containers-final.json
python3 containers.py --units "$UNITS" --images fastsim-t3:1.20 fastsim-t3:1.19 --repeats 7 --out containers-confirm.json
python3 bench.py --bins "$BINS" --units "$UNITS" --labels release19 release20 --repeats 12 --out releases-native.json
python3 differential.py --base "$BINS/release19" --candidate "$BINS/release20" --configs random-configs --out releases-random.json
python3 analyze.py
```

`$BINS` holds the two binaries, extracted from their release images and checked against the recorded
SHA-256. The analysis scripts use only the standard library. Packaging and validation use
`packaging/tools/build.py` and `packaging/tools/validate.py`.
