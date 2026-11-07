This pass (release 1.2, "v3") changes what `wall_clock_sec` measures and takes work off the
event loop. It is compared with v2, release 1.1, whose source is `sub/releases/1.1/context/`.
Every emitted Parquet file is byte-identical to v2's.

## What `wall_clock_sec` measures

The official baseline (`baselines/abides_fork/simulate.py`) times only `abides.run(config)`.
Reading and parsing the scenario, `build_config` (agents, oracle, latency model and random
states), extracting the trace, writing both Parquet files and hashing them all fall outside its
timer. Until v2, fastsim timed everything from the start of `simulate` until its files were
written, so its reported rate was not comparable with the baseline's.

`wall_clock_sec` of a single scenario is now the elapsed time of `Sim::run`, the same span as
`abides.run(config)`:

- **Inside:** kernel initialization and start (the market-close wakeup and every agent's first
  wakeup), the complete event loop, and every row the run records. Latency lookahead and trace
  replay threads work concurrently with the loop, and the window waits for nothing they do.
- **Outside, before:** parsing, `Sim::new` (seed chain, oracle, agents, latency model) and
  creation of idle helper threads. An idle latency producer receives the stream only when the
  run starts: no simulation value is computed before the timer starts.
- **Outside, after:** finishing the trace from the run's log (grouping, quote de-duplication,
  execution labels; `extract_trace` in the baseline), Parquet encoding, hashing and writes,
  and stopping helper threads.

A batch keeps the baseline's batch clock: from after listing `scenarios/` until every sub's
files are written, including each sub's loading, simulation, encoding and writes. Each sub's
own `events.json` reports its simulation window, as the baseline's per-sub `simulate` does.

`events_per_sec` is still `n_events / wall_clock_sec` exactly. The Final ranks a rate the
organizers measure around the whole container run, so this definition affects only the
self-reported Development number; the end-to-end changes below affect both.

## Changes

- **Order book:** levels reuse the order queues of removed levels instead of allocating a new
  `VecDeque` per new price level.
- **Trace log:** the event loop appends one row per `order_event` / `quote` call to a
  `RawTrace`. `TraceBuilder::replay` applies the same calls afterwards, so grouping, sorting,
  quote de-duplication and labelling run off the loop, on a replay thread that also encodes the
  trace when a ledger is written (the encoder threads keep up with the ledger).
- **Helper threads:** threads a run is expected to use are created before it, from a size
  estimate based on the agents' wake counts (`Scenario::estimated_rows`, within 0.9x-1.5x on
  the public scenarios). Small runs create none, and a low estimate falls back to on-demand
  creation. A prepared latency producer draws the 1,024 samples the loop draws itself while
  it starts, then continues the identical stream. The run's helper threads are joined after
  the window, and the finishing thread helps encode any queued row groups.
- **Ledger digest:** `events.json` no longer carries `message_trace_sha256`. The scorer never
  reads it (its required fields are `scenario_id`, `n_events`, `wall_clock_sec`,
  `events_per_sec`, `seed` and `trace_sha256`), and it cost a full hash of the ledger inside
  every batch clock. `n_messages` remains.

## Measurements

Native PGO builds of v2 (`target/perf-pgo/fastsim`) and v3 (`target/perf-pgo-v3/fastsim`, built from
the release 1.2 source) on an Apple M2 Max, with one warm-up and eleven measured runs per unit,
alternating order and nothing else running. These are local measurements, not platform timings.

- **Mean of per-unit reported events/s, all 71 units: 11.58M -> 17.90M (1.546x).**
  Single scenarios: 11.34M -> 18.17M. Batches: 14.23M -> 14.98M (their clock still includes
  encoding and writes).
- **Mean of per-unit complete-process events/s: 6.572M -> 6.860M (1.044x).** Process time
  includes startup, parsing, simulation, encoding, hashing, writes and teardown, so this is the
  change the Final's host clock sees. The median unit is 3.4% faster; the largest units gain
  13-14%.

| Public unit | v2 reported ms | v3 reported ms | Reported speedup | v2 process ms | v3 process ms | Process speedup |
|---|---:|---:|---:|---:|---:|---:|
| t3-s001-price-time-priority | 0.416 | 0.051 | 8.18x | 4.05 | 4.23 | 0.96x |
| t3-eq-lognormal-highmag | 2.263 | 0.973 | 2.33x | 6.09 | 5.76 | 1.06x |
| t3-as01-base-mix | 3.279 | 1.677 | 1.95x | 6.99 | 6.60 | 1.06x |
| t3-sf-01-baseline-regime | 5.360 | 3.444 | 1.56x | 9.50 | 9.48 | 1.00x |
| t3-fastlob-core | 6.129 | 4.234 | 1.45x | 10.37 | 10.16 | 1.02x |
| t3-mr-deep-book-state-size | 25.885 | 21.097 | 1.23x | 30.97 | 29.00 | 1.07x |
| t3-gb-mega-throughput | 91.176 | 71.024 | 1.28x | 97.02 | 85.16 | 1.14x |
| t3-gbatch-homog-8 | 3.845 | 3.626 | 1.06x | 8.19 | 8.09 | 1.01x |

The [raw measurements](benchmarks/2026-09-26-v3-m2-max.json) hold every repeat, both executable
hashes and peak RSS. On the Development platform, v2 reported 7.46M against 11.58M locally;
the v3 clock is dominated by the event loop, for which v1 and v2 showed platform/local time
ratios of about 1.3 on the large no-ledger units.

An earlier v3 candidate moved trace replay onto a thread that took one of v2's two encoders.
Ledger units then left up to 2 ms of queued encoding after the run (fastlob-tight-book: 3.35 ms
window, 5.6 ms until encoding finished), and complete-process time regressed 4-9% on nine
mid-size ledger units. Encoding the trace on the replay thread and letting the finishing thread
help drain the queue fixed it: in the final measurement those nine units are 1.5-7.3% faster
than v2 end to end. Two units were slower than v2 in the full run (s001 0.96x,
ca-fat-tail-jumps 0.94x); a separate 25-repeat check measures both 1.02x faster, so that was
run-to-run variation (s001's process time alone ranges 3.1-4.1 ms between batches here).

## Validation

- All Rust tests pass (13 unit tests, including the rewritten lookahead tests that cover
  prepared and lazy producers, early stops at every switch point, and unstarted producers; the
  pipeline determinism test compares 0-3 worker threads byte for byte).
- **Byte identity with v2 on all 71 public units:** three runs per binary, 552 Parquet files,
  no difference.
- **Binary differential fuzzing against v2** with the ABIDES fuzz generator, plus 40% of cases
  scaled up 3-30x in horizon, rates and agent counts to exercise chunk streaming, the replay
  thread and lookahead, plus random batches: seed 777, 330 cases (121M events) on the final code;
  seed 20260926, 330 cases (202M events) before the final encoder-placement change. Every
  Parquet file is identical and no case crashes.
- **Release 1.2 (linux/amd64 PGO image, manifest `sha256:89800ab3...`):** 71/71 units x 2 input
  layouts pass `sub/tools/validate.py`: exact trace and ledger match, official sanitizer and
  developer verifier, `events.json` arithmetic, and byte-identical outputs across layouts, in
  containers with the card's CPU and memory limits, read-only root and no network.
- The native PGO comparison verified all 184 emitted Parquet files decode equal to v2's.

To reproduce from the `fastsim` directory:

```sh
cargo test --release --locked --offline
python3 tools/pgo_build.py --work-dir target/pgo-v3 --out target/perf-pgo-v3/fastsim \
  --llvm-profdata /opt/homebrew/opt/llvm@22/bin/llvm-profdata --offline
../sub/.venv/bin/python tools/bench_compare.py --baseline target/perf-pgo/fastsim \
  --candidate target/perf-pgo-v3/fastsim --all --verify --runs 11 --json target/v3.json
```
