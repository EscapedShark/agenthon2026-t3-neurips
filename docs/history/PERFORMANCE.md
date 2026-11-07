This records the first optimization pass, committed as `55d2f33`.
The subsequent queue, message-layout and sampling changes are measured separately
in [PERFORMANCE_HOTPATHS.md](PERFORMANCE_HOTPATHS.md).
The latest changes to chunk scheduling, busy recipients, exact latency lookahead
and PGO are measured against v1 in [PERFORMANCE_PIPELINE.md](PERFORMANCE_PIPELINE.md).
Release 1.2 ("v3") redefines `wall_clock_sec` as the baseline's `abides.run` span and moves
trace finishing off the event loop; see [PERFORMANCE_V3.md](PERFORMANCE_V3.md).
Release 1.3 ("v4") populates the memory a run writes before it starts, encodes Parquet about
40% faster and schedules batch subs by size; see [PERFORMANCE_V4.md](PERFORMANCE_V4.md).
Release 1.4 ("v5") shortens the time outside the simulation window (zero-copy output, parallel
hashing and `msg_type` encoding, smaller single-scenario row groups, early exit); see
[PERFORMANCE_V5.md](PERFORMANCE_V5.md).
Release 1.5 ("v6") prepares batch outputs on a helper thread, starts batch subs sooner, reads
the card with a line scan and adds sorted and bucketed message-queue forms; see
[PERFORMANCE_V6.md](PERFORMANCE_V6.md).

That first pass completes four representative public single-scenario runs
2.03–2.14 times faster on an Apple M2 Max. All 95 public scenarios still reproduce
the reference trace and message ledger, including row order and pandas dtypes,
on both native macOS ARM64 and Linux x86-64.

Measurements from 2026-09-25 compare that implementation with commit
`796dae5c0f7d77646d905c565a8e33f3b9c94ee8`, using Rust 1.98.1 release builds.
Each binary receives one warm-up and seven measured repeats, with alternating
execution order and no concurrent benchmark processes. Times are medians of the
complete process, including startup, simulation, encoding, writes, hashes and
teardown. The host has 12 logical CPUs; these are local measurements, not official
benchmark-hardware results.

| Public unit | Before (ms) | After (ms) | Speedup | Median peak RSS before / after (MiB) |
|---|---:|---:|---:|---:|
| t3-as06-throughput-fast | 46.5 | 21.7 | 2.14x | 31.0 / 24.5 |
| t3-gb-highfreq-40hz-60s | 275.2 | 128.9 | 2.13x | 163.5 / 88.9 |
| t3-gb-mega-throughput | 591.8 | 281.0 | 2.11x | 356.7 / 172.9 |
| t3-mr-deep-book-state-size | 172.3 | 85.0 | 2.03x | 81.7 / 50.7 |
| t3-gbatch-dense-3 | 8.9 | 7.2 | 1.23x | 14.5 / 13.5 |
| t3-gbatch-homog-8 | 11.1 | 9.5 | 1.16x | 25.3 / 22.8 |

The [raw measurements](benchmarks/2026-09-25-m2-max.json) include every measured
duration, executable hashes, environment information and a separate comparison
against the intermediate sequential encoder. Concurrent encoding adds another
1.23–1.28x on the three tested single scenarios, with a small memory increase.

The implementation changes are:

- Encode high-cardinality timestamps and IDs with lossless delta encoding, retain
  dictionaries for the other columns, and omit unused Parquet page statistics.
- Convert rows in groups of 65,536 with reusable integer, string and null buffers.
  Ledger sequence numbers continue across group boundaries.
- Encode trace and ledger concurrently when a spare CPU is available and the
  output is large enough. Batch workers divide the available CPU budget first.
- Flush pending quotes when the timestamp changes and stably sort only the
  completed timestamp group. Preserve first-side order, last quote values and
  last-execution labelling.
- Precompute exchange links for the default distance latency model in O(n)
  space/time, retaining exact on-demand calculation for other pairs and the
  original random-position draw order.
- Measure actual in-memory totals in `bench`, report warmed medians, and include
  output hashes in the CLI's reported wall time.

Validation completed:

- Release tests on macOS and Linux x86-64, including the strict Linux NumPy RNG
  golden test, quote/lifecycle ordering tests, and comparison of sparse latency
  against the original full-matrix formula for all pairs at several seeds/sizes.
- All 95 public scenarios on each platform: both outputs equal the reference
  row-for-row with matching dtypes; the semantic, message and batch checks invoked
  by `compare_unit.py` pass. Linux execution used an offline amd64 Docker container
  under emulation, limited to 4 CPUs and 8 GiB; it was used for correctness only.
- Differential fuzzing against ABIDES with seed `20260925`: 60/60 valid scenarios
  match exactly. Four additional generated scenarios fail inside ABIDES with
  `KeyError: 'order_id'`, so they provide no reference result.
- Original-binary comparisons for empty output, zero latency, a busy exchange,
  default distance latency, and 4,096 agents without trading all match exactly.
- Sequential and concurrent encoders produce byte-identical files on the mega
  scenario; standalone and single-sub batch outputs also match byte-for-byte.

Parquet file hashes differ from the original encoding, as expected; the CLI hashes
the actual emitted bytes. Decoded values, order, nulls and dtypes remain identical.

To repeat the local comparisons from the `fastsim` directory, preserve a baseline
release executable before modifying the source. This workspace retains the one
used here under the ignored `target/perf-baseline-796dae5/` directory:

```sh
cargo build --release --locked --offline --bins
python3 tools/bench_compare.py --baseline target/perf-baseline-796dae5/fastsim --runs 7
.venv/bin/python tools/compare_unit.py
cargo test --release --locked --offline
```
