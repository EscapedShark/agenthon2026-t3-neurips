This round removes the output tail and the ledger where it is not required, and replaces the
event queue. It follows [PERFORMANCE_HOTPATHS.md](PERFORMANCE_HOTPATHS.md); the radix queue that
document describes has been replaced (see below).

Changes:

- **Ledger only when required.** `simulate` reads the unit's `card.toml` next to the scenario and
  applies the scorer's own rule (`limits.requires_message_ledger`): an explicit boolean
  `[scoring.params].requires_message_ledger`, else required unless the family is
  `throughput-scale`. Without a readable card the ledger is written. The 6 largest public units
  declare it not required; batch sub-scenarios always write it. When skipped, no ledger row is
  recorded at all, and `events.json` omits `n_messages` / `message_trace_sha256`.
  `--ledger always|never` overrides (testing).
- **Hand-written Parquet writer** ([src/pq.rs](src/pq.rs)): one Snappy v1 data page per column
  chunk, `DELTA_BINARY_PACKED` integers, bit-packed dictionary strings, RLE / bit-packed
  definition levels, Thrift compact footer with the `pandas` metadata. The `parquet` crate is now
  only a dev-dependency, used as an independent reader in tests.
- **Streaming encode.** Trace and ledger builders hand each 65,536-row chunk (one row group) to up
  to `cpus - 1` background encoders while the simulation runs; row buffers are recycled, so memory
  stays bounded. Workers start only when the first chunk exists. The trace's `msg_type` column is
  encoded last, when execution labels (`ORDER_FILLED` vs `PARTIAL_FILL`) are known.
- **Event queue.** Wakeups and messages in two 4-ary heaps whose keys pack
  `(time, sender, recipient, message_id)` into one `u128`; the next event is the smaller head.
  Measured on `gb-mega`: both heaps hold 100+ events (agents wake in lockstep at open-aligned
  intervals), and the radix queue spent ~35% of the simulation redistributing buckets. A timing
  wheel for messages was also tried and measured no faster; it was not kept.
- CPU budget = `min(available_parallelism, card [environment].cpus or 4)`, since inside gVisor the
  host core count may be all that is visible. `FASTSIM_TIMING=1` prints phase times to stderr.

Complete-process medians (7 of 8 runs), Apple M2 Max, local measurements:

| Public unit | 796dae5 (ms) | 55d2f33 (ms) | now (ms) |
|---|---:|---:|---:|
| t3-gb-mega-throughput | 582.6 | 276.0 | 126.4 |
| t3-gb-pop-horizon-scale | 371.9 | 173.5 | 77.0 |
| t3-gb-highfreq-40hz-60s | 266.6 | 122.7 | 55.1 |
| t3-mr-cancel-replace-churn | 150.2 | 62.8 | 33.9 |
| t3-mr-deep-book-state-size | 171.7 | 85.8 | 47.4 |
| t3-as06-throughput-fast | 46.5 | 21.4 | 11.9 |
| t3-s001-price-time-priority | 4.0 | 4.0 | 2.9 |

On `gb-mega` the encoding tail after the last event is under 1 ms, peak RSS fell from 173 MB to
39 MB, and hashing + writing the outputs takes ~4.5 ms.

Validation:

- 14 release tests on macOS and Linux x86-64 (strict NumPy RNG golden on Linux), including new
  ones: queue differential tests against one `BinaryHeap` of original tuples (dense ties,
  broadcasts, requeues, ids crossing 2^32 and the tuple fallback), the card rule, and a Parquet
  round trip through parquet-rs over edge cases (0/1/7/8/9 rows, row-group boundaries, all-null
  row groups, extreme and wrapping i64/i32 values).
- The same edge-case files read back exactly with pyarrow 25.0.1 and 15.0.2
  (`tools/check_parquet_fixtures.py`).
- All 95 public scenarios on macOS and Linux x86-64: identical rows, columns and dtypes, and every
  official gate passes — with the ledger per card, and with `--ledger always`. All 190 files also
  compare identical under pandas 1.5.3 / pyarrow 15.0.2.
- Differential fuzzing against ABIDES, seed 4242: 276 of 276 valid scenarios identical (24 fail
  inside ABIDES with `KeyError: 'order_id'`).
- Output bytes are identical across repeats and across encoder thread counts (repeats must be
  byte-stable in the Final phase).

Reproduce from the fastsim directory:

```sh
cargo test --release
.venv/bin/python tools/compare_unit.py
.venv/bin/python tools/compare_unit.py --ledger always
FASTSIM_PARQUET_FIXTURES=/tmp/fx cargo test --release --test parquet_roundtrip && .venv/bin/python tools/check_parquet_fixtures.py /tmp/fx
.venv/bin/python tools/fuzz_vs_abides.py 300 --seed 4242
```
