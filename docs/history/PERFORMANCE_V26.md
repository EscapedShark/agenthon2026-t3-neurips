Release 1.25 ("v26") is release 1.24 with three changes. The event loop no longer waits for its
latency samples. The binary is a static executable at a fixed address. And its PGO profile is
trained on a real x86 machine rather than under the Mac's emulation, which matters more than the
first two. Every trace and ledger file of the public units is byte-identical to release 1.24's.

## The latency lookahead

Log-normal and Pareto latencies are drawn ahead by a helper thread. It produced blocks of 2,048
samples, doubling up to 16,384, through a channel two blocks deep. The run handed it the stream when
the event loop began, with the loop drawing the first 1,024 samples itself meanwhile. Marks builds
on the lab show the loop waiting for it in most runs:

| Unit | Waits per run, 1.24 | 1.25 |
|---|---|---|
| gb-mega | 3-4, 160-230 us | none |
| gb-pop-horizon-scale | 2-4, 160-230 us | none |
| gb-horizon-240s | 3-4, 180-280 us | none |
| gb-highfreq-40hz-60s | 3-4, 200-240 us | none |
| gb-pop-128-agents | 2-4, 160-210 us | none |
| eq-lognormal-lowsigma (1 ms loop) | 2-4, 60-120 us | 0-2, 0-60 us |

The waits came at blocks 1-4, and longest at the fourth. Per sample, the helper is about twice as
fast as the loop uses samples. But each block was twice the size of the one before, so it had to
draw twice as many samples per block and only just kept up.

1.25 makes three changes to the lookahead:

- The helper is started right after the run's plan is made, while the outputs and the simulation
  are set up (0.1-0.3 ms). It continues where the loop's own first 1,024 draws stop. The stream is
  seeded from the scenario's seed chain, and nothing draws from it before the run.
- A polling helper (one prepared before the run) hands over constant blocks of 4,096 samples.
- Its channel holds four blocks.

A sleeping helper (spawned lazily on sustained demand) keeps the doubling, which saves it wake-ups.
The samples, and therefore the outputs, are unchanged.

## A static executable at a fixed address

`-C relocation-model=static` in the Dockerfile's `RUSTFLAGS`. The binary is an ET_EXEC loaded at
0x200000 instead of a static PIE. It no longer relocates itself at start (2,428 relative
relocations), and its code uses absolute addresses and jump tables. The binary is 1,808,016
bytes, 62 KB smaller.

## The lab A/B

Lab-native PGO builds, two per variant, all 71 units in the card layout, whole-process wall time,
10 runs per unit and label, two batches with the label order reversed. Results are against the
mean of two builds of release 1.24's source.

| Variant | Own time | Geomean | gb-mega, pop-horizon, horizon-240, highfreq | Proxy at F = 102 ms |
|---|---:|---:|---:|---:|
| lookahead (2 builds × 2 batches) | -1.7% to -1.9% | -2.1% to -2.2% | -1.8% to -2.5% | +0.21% to +0.24% |
| **lookahead + fixed address** | **-2.1% to -2.5%** | **-2.5% to -2.9%** | **-2.8% to -3.4%** | **+0.28% to +0.33%** |

On the twelve denser variants of v25 the same builds were 1.5-2.0% and 2.7-2.9% faster.

## PGO trained on the Mac costs 2-4%

Four release-pipeline builds of the 1.25 context, made by `build.py` on the Mac, were all 2.3-2.9%
slower than release 1.24 on the four big gb units. That is +0.8% to +1.2% own time and -0.23% to
-0.27% proxy, the opposite of the lab-native builds of the same source.

- **Not the toolchain.** Non-PGO builds of the 1.25 context (marks, wait-instrumented) in the
  release Docker image (glibc 2.41) and natively on the lab (glibc 2.35) run alike, and neither
  waits for latency samples.
- **Not waiting either.** A PGO build in the release image with the same instrumentation doesn't
  wait.
- **The cause is the training.** Docker Desktop runs the amd64 build stage under emulation, where
  the threads' relative speeds differ from real hardware. The profile it records lays the event
  loop out worse. The cycles of all threads barely moved (gb4 +0.3-0.5%), but the loop's thread is
  a third of gb-mega's CPU.
- **Native builds fix it.** Three builds of the same context with the same Dockerfile, made
  natively on the x86 lab (`docker build --no-cache`, its legacy builder), were 0.9-1.35% faster
  than release 1.24, gb4 1.05-1.5% faster, proxy +0.10% to +0.16%, like the lab's own builds.
- **The release is one of them.** 1.25 is the native build `n25c`, moved to the Mac with
  `docker save | docker load` and recorded by `build.py --adopt` with a note on where it was built
  (`sub/releases/1.25/adopted.json`).

## Tried and dropped

- **BOLT** (bolt-22 from Debian trixie; LBR profiles of all 71 units, 3 rounds):
  - On top of PGO and ext-TSP, full BOLT (block and function order, hot/cold splitting) was +0.9%
    to +1.1% own time and -0.06% to -0.09% proxy.
  - Without splitting it was neutral: proxy within ±0.01%, gb4 -0.3%, small units +0.7-1%.
  - The rewritten binary is 6.5 MB and keeps the old text. `-use-old-text` fails on two static
    glibc functions.
- **Polling output workers in mid-sized runs.** A ledger hand-over costs the loop 0.2-0.3 us,
  18 us for mr-deep-book-state-size's 72. There was nothing to gain, and the change was reverted.
- **Hashing the trace's tail sooner.** After the run, gb-mega still writes and hashes 568 KB: the
  `msg_type` chunks, kept to the end because executions relabel earlier rows, and the footer.
  - That takes 1.6 ms on the lab's i7, which has no SHA instructions, but 0.36 ms with them (the
    Mac; the Final's Xeon has SHA-NI, which the `sha2` crate uses).
  - At most +0.02-0.05% on the Final. Not done.
- **Start-up and exit.**
  - A static "hello" takes 160-190 us from spawn to reaped on the lab, and fastsim exiting at its
    first mark 236 us.
  - Self-relocation (10-20 us) is gone with the fixed address.
  - At exit the process holds 2-8 MB and one thread; the binary's own pages are 1.4 MB of that.
  - The main thread takes about 50 page faults per loop; prefaulting covers it, and the helpers
    take the rest.
- **Deterministic PGO.** Two native trainings of one source differ in 173 of 1,715 functions:
  - hot ones by under 0.1% of their counts;
  - the batch's work sharing by up to 19%.
  Those small differences flip layout decisions, so builds still vary by about 1-2%. Builds are
  measured and the best adopted.

## Correctness

- **Tests:** `cargo test --release` passes 44 tests. The new one is
  `early_producer_matches_the_scalar_stream`: an early producer, for both served models and three
  seeds, gives the scalar stream's samples across blocks and self-sends.
- **Reference comparison:** `tools/compare_unit.py` gives 95/95 scenarios identical to the
  reference and 95/95 passing the official gates.
- **Differential fuzzing against ABIDES:** `fuzz_vs_abides.py` has a new `--scale` mode, `--reuse`
  (keep the ABIDES outputs of identical cases) and a `summary.json` per run.
  - `--scale` draws markets like the throughput-scale units: the gb units' four-type ecology,
    scaled along random axes (30-600 noise traders at 5-80 Hz, up to 200 value traders, 12 market
    makers, 30 momentum traders, order sizes up to 100, oracle jumps and scheduled jumps, every
    latency model, mixed rates within a kind).
  - A campaign of 1,000 such scenarios with the 1.24 binary found every finished one identical to
    ABIDES.
  - The release's source rechecked against every reference so far: 999 + 57 + 56 + 6 = 1,118
    cases, all identical, 0 mismatches (`recheck.py`). Of the 999, 737 use the log-normal or
    Pareto latency the lookahead serves.
  - The thousandth case (100 market makers rebalancing 10 levels every 10 ms: 10.1 million
    events, 8.4 million messages, 0.94 s in fastsim) was stopped after 197 CPU minutes in ABIDES.
  - With the release's source directly:
    - 600 ordinary scenarios: 562 identical, 38 where ABIDES itself raises
      `KeyError: 'order_id'`, 0 mismatches.
    - A mixed-interval campaign (`--mixed --min-draws 12000`), stopped after about 100
      scenarios: 97 identical, 3 ABIDES errors, 0 mismatches.
  - `--timeout` now gives up on an ABIDES case after that many seconds (`ABIDES-TIMEOUT`), so one
    case can no longer stall a campaign.
- **Validation:** `validate.py` passes 71/71 units in both input layouts (142 runs, 0 failures).
  All 374 parquet files are byte-identical to release 1.24's validation record.

## Release

- **Image:** `fastsim-t3:1.25`, manifest `sha256:8a5bcb48d8015fdb6e9385770dcfa95a1f7a108718a66c252edabc62fe19db08`,
  binary sha256 `6473a9df…` (1,808,016 bytes, static, ET_EXEC), context `a62cae54…`.
- **Against release 1.24**, three batches (card layout, 12 runs per unit):
  - Own time is 1.47%, 1.46% and 1.41% lower; geomean 1.6-1.7% lower; gb4 1.2-1.4% lower.
  - The proxy gains +0.159%, +0.145% and +0.158% at F = 102 ms, and +0.11% to +0.12% at F = 150 ms.
  - `perf`: instructions -2.71% and cycles -4.37% (geomean); gb4 cycles -0.93%.
- **Denser variants:** -1.0% against 1.24 and -9.1% against 1.23 (-13.7% where the span guard
  binds).
- **The image on the lab's GPU:** CDI and legacy injection, card and staged layouts. Every unit's
  stable outputs are byte-identical to release 1.24's (`finalrace.py --cross-check`). Final-style
  windows against 1.24: -0.76% (CDI, card, warm-up + 4), -0.21%, +0.12% and +1.66%. That is
  container noise.
- **Published:** `docker.io/greedyshark/fastsim-t3@sha256:8a5bcb48d8015fdb6e9385770dcfa95a1f7a108718a66c252edabc62fe19db08`
  (anonymous pull 200).
- **ZIPs:** `sub/submissions/submission-1.25-8a5bcb48-dev.zip` (= `v26.zip`, sha256 `701d9537…`) and
  `submission-1.25-8a5bcb48-final.zip` (sha256 `9e581442…`). `check_zip.py` passed both.

Release records: `sub/releases/1.25/`. Measurements: `benchmarks/v26-latency-nonpie-2026-10-11/`.
