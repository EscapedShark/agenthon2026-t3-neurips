The event queue, message layout and random latency paths have been optimized.
All figures below are incremental to commit `55d2f33f93a505a62725df071f231cb22db58730`,
which already contains the preceding Parquet optimizations.

Implementation changes:

- Use a radix queue for advancing event times. A small heap preserves the exact
  `(time, sender, recipient, message_id, slot)` order within each timestamp.
  Sending into the past switches to a general heap, preserving existing negative-delay
  behavior. Rescheduling retains the original message receive time and causal fields.
- Reduce `Msg` from 88 to 64 bytes (27.3%) and `Payload` from 56 to 40 bytes.
  Store message kinds in payload padding, inline order fields, and retain only prices
  in spread replies. Every order field retains its original width; optional prices
  remain `Option<i64>`, without sentinel values or lossy narrowing.
- Precompute constant latency and uniform ranges and flatten latency dispatch.
  Consecutive MT19937 words share state-boundary checks and position updates.
  Combine the 53-bit integer before conversion to save floating-point operations.
  Draw both uniforms before transforming a normal pair. Preserve distributions,
  rejection sampling, Gaussian caching, draw order, clamping and ties-to-even rounding.

Measurements use Rust 1.98.1 release builds on an Apple M2 Max with 12 logical CPUs
and 32 GiB RAM. Benchmarks ran sequentially without concurrent builds or validation.
These are local measurements, not official scoring-hardware results. The Linux
amd64 container (4 CPUs / 8 GiB, emulated on this host) was used for correctness only.

Simulation timings include initialization and trace finalization, excluding Parquet.
Five alternating baseline/candidate subprocess pairs each perform one warm-up and
seven measured runs. The table reports the median of the five per-process medians;
the existing bench executable reports simulation time to 0.1 ms.

| Public unit | Before sim (ms) | After sim (ms) | Time reduction |
|---|---:|---:|---:|
| t3-as06-throughput-fast | 8.4 | 8.0 | 4.8% |
| t3-gb-highfreq-40hz-60s | 60.9 | 57.1 | 6.2% |
| t3-gb-mega-throughput | 145.8 | 135.0 | 7.4% |
| t3-mr-deep-book-state-size | 53.0 | 43.1 | 18.7% |

Complete-process timings include startup, simulation, concurrent encoding, writes,
hashes and teardown. Each binary receives one warm-up and seven alternating measured
runs. Overall peak RSS remains approximately unchanged.

| Public unit | Before process (ms) | After process (ms) | Speedup |
|---|---:|---:|---:|
| t3-as06-throughput-fast | 21.29 | 20.93 | 1.017x |
| t3-gb-highfreq-40hz-60s | 121.42 | 117.25 | 1.036x |
| t3-gb-mega-throughput | 272.77 | 262.16 | 1.040x |
| t3-mr-deep-book-state-size | 84.82 | 75.72 | 1.120x |
| t3-gbatch-dense-3 | 7.60 | 7.78 | 0.978x |
| t3-gbatch-homog-8 | 9.18 | 9.15 | 1.003x |

Small batches do not show a consistent gain. An additional 31-repeat alternating
comparison measured:

- `t3-gbatch-dense-3`: 8.049 -> 8.362 ms, a change of +0.313 ms (+3.9%).
- `t3-gbatch-homog-8`: 10.071 -> 10.145 ms, a change of +0.075 ms (+0.7%).

An isolated RNG microbenchmark reduces five million lognormal draws from 67.43 ms
to 61.75 ms (8.4% less time, 9.2% more throughput). Both versions compile
the same standalone harness separately, use identical parameters and seed, and run
three alternating subprocess pairs with one warm-up plus seven measurements each.
This measures RNG plus lognormal transformation, excluding latency clamping, queue
operations and output.

Validation of the final implementation:

- Ten release tests pass on each platform, including strict bitwise NumPy RNG golden
  tests on Linux. New tests cover all 625 MT state offsets, full RNG state, differential
  queue operations against the original tuple/BinaryHeap order, same-time inserts,
  backwards insertion, broadcasts and full-width integer keys.
- All 95 public scenarios on each platform match every reference row, column and
  dtype; all official checks invoked by compare_unit.py pass.
- Differential ABIDES fuzzing, seed `20260926`: 61 of 61 valid reference scenarios
  match exactly. Three further cases (#0, #15, #59) fail inside ABIDES with
  `KeyError: order_id` and supply no reference result.
- Ten edge scenarios emit byte-identical outputs to the baseline binary: empty output,
  zero latency, negative latency, negative compute delay, busy exchange, first event
  past stop time, 4096 agents, zero-width uniform, zero-sigma lognormal, default latency.

All samples, binary/source hashes, environment information and edge configurations
are in the [raw measurements](benchmarks/2026-09-25-hotpaths-m2-max.json).

Reproduce from the fastsim directory. The preserved baseline executables are under
the ignored `target/perf-baseline-55d2f33/` directory in this workspace. On a fresh
checkout, build and preserve both fastsim and bench from that commit first.

```sh
cargo test --release --locked --offline
.venv/bin/python tools/compare_unit.py
python3 tools/bench_sim_compare.py --baseline target/perf-baseline-55d2f33/bench --runs 7 --rounds 5
python3 tools/bench_compare.py --baseline target/perf-baseline-55d2f33/fastsim --runs 7
python3 tools/bench_rng_compare.py --baseline 55d2f33
```
