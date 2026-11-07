Release 1.3 ("v4") follows the v3 Development result. It populates the memory a run writes
before the run, encodes Parquet about 40% faster, and schedules batch subs by size. Every
decoded output is identical to v3's; the file bytes differ because pages are no longer
compressed (see below).

## What the v3 result showed

**The Development score is capped per unit.** The primary score equals the mean over units
of `min(events_per_sec, 10,000,000)`, to four decimals, for all three submissions so far:

| Submission | Primary score | mean(min(rate, 1e7)) | Units above 1e7 |
|---|---:|---:|---:|
| v1 | 5,129,649.3040 | 5,129,649.3040 | 2 |
| v2 | 7,459,500.9286 | 7,459,500.9286 | 13 |
| v3 | 9,572,389.4672 | 9,572,389.4672 | 51 |

So the board's maximum is 10,000,000, and only units below the cap move the score. In v3 these
were the small ones (eq-*, s001, s019, as01-05, ca-thin-book, ra03) and three batches
(hetero-mix, dense-3, varsize).

**The platform adds a fixed cost inside the window.** Across v3's single scenarios the
platform window is about 1.06 ms + 1.14 x the local (macOS) window. The slope says the
platform's single-thread speed is close to the M2 Max; the intercept sinks the small units.

**Docker Desktop's Linux VM reproduces it.** Running the same code natively as linux/arm64
in a container (`--cpus 4`, 4 KiB pages) gives windows close to the platform's (platform ≈
0.55 ms + 0.98 x VM across single scenarios; large units within 5%). macOS uses 16 KiB pages,
which hides the effect. In the VM, `getrusage(RUSAGE_THREAD)` counted the simulation thread's
own page faults inside the window: about 440 for the eq-* units, 640-770 for 25k-80k-event
units, mostly first writes to fresh ledger (1 MiB) and trace-log (320 KiB) chunk buffers
before recycling starts. At about 0.8 us each they were a third of an eq-* window.

## Changes

- **Populated memory (`src/mem.rs`).** Before a single scenario's run, the buffers its loop
  will write are populated: the first ledger and trace-log chunks and up to two spare ones in
  the recycling queues (sized from `Scenario::estimated_rows` x 1.25), the message slab and
  the event queues. On Linux one `madvise(MADV_POPULATE_WRITE)` per buffer faults the pages in
  without writing them (kernel 5.14+); elsewhere one byte per page is touched. No simulation
  value is computed before the window starts. Batch subs populate nothing: their clock covers
  every step, and over-populating would only add work.
- **Parquet encoding.** Pages are stored in the Snappy raw format as one literal: the column
  codec stays `SNAPPY` and every reader decodes it, but no time goes into searching for matches
  (Snappy took a third of encoding time to save 1-30% on these already bit-packed pages).
  `DELTA_BINARY_PACKED` miniblocks are packed by one of 64 width-specialized, fully unrolled
  routines, and the delta pass is specialized for INT32/INT64 at compile time. Dictionary
  indices use the same routine in groups of 32.
- **Batch scheduling.** Subs start largest first (by the size estimate). A sub whose estimate
  exceeds 1.5x an even share of the batch (hetero-mix's 24k-event sub) runs with a single
  scenario's helper threads, since it alone decides when the batch ends; the others run
  inline as before. Creating a thread costs 40-120 us in the VM (median 40-55), so smaller subs
  are not given helpers.

## Measurements

**Linux VM (the platform proxy).** v3 and v4 built natively for linux/arm64 in `rust:1-slim`,
run alternately in one `debian:trixie-slim` container with `--cpus 4 --memory 6g`, seven runs
each per unit (first dropped). Batch rows are the whole batch clock; single-scenario faults are
the simulation thread's own, inside the window.

| Public unit | v3 window (ms) | v4 window (ms) | Speedup | Loop-thread faults v3 → v4 |
|---|---:|---:|---:|---:|
| t3-s001-price-time-priority | 0.063 | 0.047 | 1.35x | 18 → 3 |
| t3-eq-deterministic-baseline | 1.358 | 1.008 | 1.35x | 439 → 4 |
| t3-eq-lognormal-lowsigma | 1.362 | 1.017 | 1.34x | 438 → 7 |
| t3-as01-base-mix | 2.305 | 1.747 | 1.32x | 641 → 7 |
| t3-sf-01-baseline-regime | 4.424 | 3.782 | 1.17x | 763 → 6 |
| t3-gb-mega-throughput | 82.516 | 82.247 | 1.00x | 182 → 12 |
| t3-gbatch-dense-3 | 3.769 | 3.659 | 1.03x | 162 → 200 |
| t3-gbatch-hetero-mix | 5.779 | 4.911 | 1.18x | 197 → 197 |
| t3-gbatch-homog-8 | 6.619 | 6.308 | 1.05x | 195 → 197 |

- Capped mean over all 71 units in the VM: 9.845M -> 9.917M. Small and mid-size
  single scenarios gain 22-36%, large ones 0-17%, batches 5-16%.
- Scaling each unit's v3 platform time by its VM speedup predicts a platform primary of about
  **9.91M** (v3: 9.57M). Units still predicted below the cap: eq-deterministic (8.5M),
  hetero-mix (8.6M), dense-3 (9.2M), s001 (9.9M), and eq-pareto-heavytail and
  eq-lognormal-highsigma (8.6-8.7M). The last two ran 42% slower than their siblings on the
  platform in v3 but only 5-8% slower in the VM, so their v3 times may have been disturbed.

**Native PGO builds on the M2 Max** (`target/perf-pgo-v3` vs `target/perf-pgo-v4`, one warm-up
and nine alternating runs per unit; [raw data](benchmarks/2026-09-26-v4-m2-max.json)). macOS's
16 KiB pages hide most of the fault savings, so this mainly shows the encoder and batch changes:

- Mean reported events/s: 18.14M -> 19.68M. Single scenarios 18.43M -> 19.88M,
  batches 15.08M -> 17.55M (hetero-mix 1.37x, homog-8 1.14x, dense-3 1.10x).
- **Mean complete-process events/s: 7.093M -> 7.310M (1.031x)**, the part the Final's host clock sees.
  The median unit is 3.4% faster end to end, hetero-mix 13% and homog-8 11%.
- Output files are larger without Snappy matching: the trace of a batch sub grows 33 KB ->
  52 KB, sf-01's 372 KB -> 491 KB and its ledger 683 KB -> 763 KB; all far below the limits.
  Encoding one batch sub's trace and ledger takes 100 + 158 us instead of 211 + 276 us.

## Tried and reverted

Each was measured in the Linux VM (alternating A/B, 9-11 repeats) and did not pay off:

- Populating memory in batch subs (all subs, or only the critical-path one): batches 5-11%
  slower; populating inside a clock that already covers it only adds the over-populated pages.
- Latency lookahead and an encoder for every sub when a batch has fewer subs than CPUs
  (dense-3): no gain; each sub's new threads cost about what they saved.
- Finishing a run's outputs by queueing the ledger tail first and helping encode while the
  replay thread drains (spin-wait, then a condition variable): batches 3-9% slower in the VM,
  and only one single scenario's end-to-end time improved on macOS.

## Validation

- All 20 Rust tests pass, including the new `mem` test and the Parquet round trips through the
  `parquet` crate, which decode the literal Snappy pages and width-specialized packing.
- `tools/compare_unit.py`: **95/95 public scenarios identical to the reference** (every row,
  column and dtype) and 95/95 pass the official gates.
- **Decoded-equality differential fuzzing against v3** (the ABIDES fuzz generator, 40% of cases
  scaled 3-30x, plus random batches; seed 4242): 275 cases, 116M events, every Parquet table
  equal to v3's including schema and pandas metadata. Byte identity no longer applies because
  the page encoding changed.
- **Release 1.3 (linux/amd64 PGO image, manifest `sha256:e4f0a422...`):** 71/71 units x 2 input
  layouts pass `sub/tools/validate.py` (exact trace and ledger match, official sanitizer and
  developer verifier, `events.json` arithmetic, byte-identical outputs across layouts), in
  containers with the card's limits, read-only root and no network.
- The native PGO comparison decoded all 184 emitted Parquet files equal to v3's.
