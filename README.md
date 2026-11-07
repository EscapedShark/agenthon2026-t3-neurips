# fastsim

A row-exact Rust rewrite of the ABIDES market simulator, built for Track 3 (Accelerated Market
Simulation) of [Agenthon 2026](https://www.agenthon.net/), a NeurIPS 2026 competition.
Team abc123 (Peidong Liu, Yiping Yin, Xing Zheng) placed **3rd of 55 teams**.

Given the same `scenario.json`, fastsim writes the same trace and message ledger as the Python
baseline, in every row, column and dtype. It does so about a thousand times faster.

| | |
|---|---|
| Final score | **3,249,452** verified rows per second of container time, 0.004% behind 2nd place |
| Speed | median **×880** over ABIDES, ×66 to ×1,765 across 65 public scenarios, with the Final binary on one i7-10700K, ABIDES timed on its simulation loop only and fastsim on its whole process |
| Exactness at release | 95/95 public scenarios identical; 2,274 fuzzed ABIDES runs and 600 boundary runs with 0 mismatches; 142/142 container runs pass the official gates |
| Final image | 1.8 MB: one static binary on an empty root |

The team's poster at the Agenthon workshop is *The Clock Starts Before Your Code Does*.

## The main finding

The Final divides verified rows by Docker's `StartedAt` to `FinishedAt` window, and that window
opens before the program does. On the Final machine, a static binary that only copies its outputs
already takes about 170 ms. fastsim's whole process takes 4.2 ms on a median public unit and 47 ms
on the largest, measured on our lab machine. So about 98% of the scored time on a median public
unit is outside the program, and the work had three parts:

1. **Match every row.** One wrong row fails a unit, and every repeat must be byte-identical.
2. **Clear the event loop.** Event order cannot run in parallel, so one thread owns it and
   everything else moves to helper threads, without changing a byte of output.
3. **Empty the image.** GPU hooks run on the clock, so the image holds one binary and nothing that
   they would have to touch.

[docs/METHOD.md](docs/METHOD.md) explains each part.

## Contents

```
fastsim/      the simulator, a Rust crate; src/ is byte-identical to the Final release
  src/        event loop, queue, order book, RNG, output pipeline, Parquet writer
  tests/      RNG golden data, Parquet read-back, determinism, trace semantics
  tools/      comparison with the reference, differential fuzzing, PGO build, benchmarks
packaging/    Dockerfile, image build, container validation, source identity check
scripts/      setup_workspace.sh fetches the official inputs at pinned commits
docs/         METHOD.md, REPRODUCE.md, RELEASES.md, and the optimization notes in history/
evidence/     release records, fuzz summaries, timing data, measurement scripts
```

## Quick start

You need Rust 1.98 or newer.

```bash
cd fastsim
cargo build --release
./target/release/fastsim simulate --config path/to/scenario.json --out out/trace.parquet
./target/release/fastsim simulate-batch --batch-dir path/to/scenarios --out-dir out
```

`simulate` writes `trace.parquet`, `message_trace.parquet` (when required) and `events.json`.
`simulate-batch` writes one directory per scenario and a `batch_events.json` summary.

## Reproducing the result

You need Git LFS, Rust 1.98 or newer, and Python 3.13 or newer.

```bash
bash scripts/setup_workspace.sh
python3 packaging/tools/check_context.py
(cd fastsim && cargo test --release)
packaging/.venv/bin/python fastsim/tools/compare_unit.py --bin fastsim/target/release/fastsim
```

These commands fetch the official units and scorer at the pinned commits, check that this
repository holds the exact build inputs of the Final image, run the tests, and compare every public
scenario with the reference row by row. The expected result is 95/95 identical and 95/95 passing
the official gates. [docs/REPRODUCE.md](docs/REPRODUCE.md) covers the expected output of each step,
differential fuzzing against ABIDES, rebuilding the image, and what cannot be reproduced.

The Final image stays public and can be pulled without an account:

```
docker.io/greedyshark/fastsim-t3@sha256:ddea2096b66000e6856f7b8aede5ebe25c89bd53858828f973a45cfc9f0a7320
```

## Licence

fastsim's code is licensed under [Apache-2.0](LICENSE). It reimplements the behaviour of ABIDES and
of NumPy's legacy random stream, both BSD-3-Clause, whose notices are in
[THIRD_PARTY_NOTICES.md](THIRD_PARTY_NOTICES.md). The official Agenthon units and tools are not
included here and stay under their own terms. [PROVENANCE.md](PROVENANCE.md) lists every input,
what was left out, and the AI tools used.

## Acknowledgements

Thanks to the Agenthon 2026 organizers (the Society of Quantitative Analysts and CEWIT at Stony
Brook University) for the track, and for publishing their timing calibration of the Final machine.
