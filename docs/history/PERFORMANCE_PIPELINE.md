This pass implements earlier chunk encoding, grouped busy-recipient scheduling,
exact latency lookahead, thread overhead reductions and profile-guided builds.
It compares with v1, commit `7686931c1096484918a3031ab6a717c0b953b286`.

On the Apple M2 Max, the arithmetic mean of per-unit throughput across all 71
public units increases from **8.415M to 12.259M events/s (+45.7%)**, using each
unit's median reported wall time. The corresponding mean calculated from complete
process wall times improves **32.1%**. These are native macOS measurements, not a
prediction of the Linux leaderboard score. The host has 12 logical CPUs; each
simulation uses a budget of at most four active worker CPUs, without CPU pinning.

Both executables received one warm-up and eleven measured runs per unit, with
alternating order and no concurrent builds or tests. Reported time includes input
loading, simulation, encoding, hashes and Parquet writes, as in v1. Complete
process time also includes startup, the final JSON output and teardown. No timer
boundaries or scoring fields were changed. All per-unit reported medians improved
in this run; the smallest gains are within normal timing noise.

| Unit | v1 reported ms | Optimized reported ms | Reported speedup | Complete process speedup |
|---|---:|---:|---:|---:|
| t3-as01-base-mix | 4.006 | 2.873 | 1.394x | 1.250x |
| t3-as03-mm-liquidity-heavy | 7.779 | 4.981 | 1.562x | 1.316x |
| t3-fastlob-core | 10.617 | 5.789 | 1.834x | 1.537x |
| t3-mr-deep-book-state-size | 42.277 | 24.561 | 1.721x | 1.629x |
| t3-gb-mega-throughput | 116.649 | 85.802 | 1.360x | 1.344x |
| t3-s001-price-time-priority | 0.482 | 0.423 | 1.140x | 1.046x |
| t3-gbatch-homog-8 | 3.790 | 3.729 | 1.016x | 1.002x |

The [full measurements](benchmarks/2026-09-26-pipeline-m2-max.json) include every
repeat, both executable hashes and peak RSS. Deep-book RSS falls from 28.2 MiB to
17.0 MiB; mega falls from 36.2 MiB to 30.9 MiB. Smaller row groups have a compression
and metadata cost, and a few batch cases use more memory; homog-8 rises from
10.4 MiB to 12.5 MiB. Every output still meets the platform's size limits.

The implementation is as follows:

- **Earlier encoding:** reduce streamed row groups from 65,536 to 16,384 rows.
  Keep timestamp-group sorting and execution labels intact. Row values, ordering,
  nulls and dtypes are identical to v1; different row-group boundaries change file
  bytes and hashes. Repeated runs of the new version produce identical bytes.
- **Busy recipients:** after eight busy deferrals to the same deadline, park that
  recipient's encountered events in a private heap and represent its head with
  one global marker. Moving the deadline then updates the group once. Delivery
  still uses the complete `(time, sender, recipient, message_id)` order. Events
  that the kernel has not encountered retain their original deadlines, preserving
  the first-event-past-stop rule, including backward timestamps. Sparse collisions
  and oversized recipient IDs retain the existing queue path. In the dense
  256-message test, scheduler steps fall from over 30,000 to below 1,024 with the
  same deliveries. The isolated change improves the public deep-book unit by
  about 28%; its roster-wide average is approximately neutral.
- **Exact latency lookahead:** for lognormal and Pareto models, start a bounded
  producer after 4,096 non-self samples and generate blocks of 2,048 values. It
  continues from the exact private NumPy RNG state, including the cached Gaussian,
  using the same arithmetic, clamping and rounding. Self-sends consume no draw;
  agent and oracle streams are untouched. Unused lookahead is discarded and the
  producer is joined on shutdown. Single scenarios share their CPU budget between
  simulation, this producer and encoders; batch workers continue sampling inline.
  The incremental roster-wide mean improves about 20.0% on this host.
- **Thread overhead:** encode outputs below 4,096 total rows inline, allocate
  inline scratch only on demand, skip the extra hash thread for small ledgers and
  batch workers, create the output directory once, and format SHA-256 without 32
  small allocations. The isolated small-unit gain is about 9%; the whole-roster
  mean is approximately neutral.
- **PGO:** `tools/pgo_build.py` builds an instrumented executable, trains twice on
  all 71 public units, merges profiles using a matching LLVM major version, then
  rebuilds with the same base flags and PGO. Every run requires a fresh work
  directory and records source, input, profile and executable hashes. On this
  host PGO adds 7.9% to the mean after the other changes. The Dockerfile now runs
  this process for Linux x86-64 with static glibc and the baseline x86-64 ISA.
  Only configuration inputs enter training; the final runtime excludes training
  files, Python, compiler and profiles. See the
  [Rust PGO workflow](https://doc.rust-lang.org/rustc/profile-guided-optimization.html).

The [incremental measurements](benchmarks/2026-09-26-pipeline-incremental.json)
record each isolated step with seven repeats. Their aggregate gains should not be
multiplied to infer the total: different units contribute different amounts to
the arithmetic mean. The eleven-repeat v1 comparison above measures the final
combination directly. The `bench` utility also now respects its ledger switch and
reports collection and streaming times separately; their difference is not
mislabelled as an encoding tail.

Validation completed on 2026-09-26:

- All **19 Rust tests** pass on macOS and Linux x86-64, including strict Linux
  NumPy golden samples, full-width queue keys, busy-group ordering with backward
  sends and stop boundaries, exact prefetch samples and producer shutdown.
  A new integration test compares byte-identical trace/ledger outputs with zero,
  one, two and three background workers, across multiple row groups and RNG blocks.
- Native PGO: **95/95 public scenarios** reproduce the official reference rows,
  order and dtypes and pass all invoked official gates. The timing comparison also
  verifies all **184 emitted Parquet files** against v1.
- Linux PGO: **71/71 units, 142 container runs** across the complete-unit and
  config-only layouts pass the existing submission validator. This covers all 95
  scenarios per layout, official sanitizer and developer verifier, actual output
  hashes, limits, and byte identity between layouts. Containers use the card CPU
  and memory limits, read-only roots, restricted users and no network. The
  [validation record](benchmarks/2026-09-26-pipeline-linux-validation.json) includes
  each verdict. Linux runs use Docker Desktop emulation for correctness; they are
  not Linux performance measurements. The NVIDIA runtime hook is unavailable here.
- Differential fuzzing with seed `20260926`: **91/91 valid generated scenarios**
  equal ABIDES in trace and ledger. Five further inputs fail inside the reference
  with `KeyError: 'order_id'` and do not count as successful checks. The
  [log](benchmarks/2026-09-26-fuzz.log) records those exclusions. The fuzz tool now
  accepts an explicit executable, resolves relative output paths correctly and
  rejects runs with zero valid reference cases.
- **16 additional kernel edge cases** match v1, including zero and negative
  latency, negative computation delay, a busy exchange, early stop, default
  distance latency and no traders, each with seeds 0 and 2^32−1. See the
  [inputs and verdicts](benchmarks/2026-09-26-pipeline-edge-validation.json).
- The build-context whitelist test and the existing 20 packaging tests pass.

Native and Linux PGO builds use the same source hashes. Their build records are
[native](benchmarks/2026-09-26-pgo-native-build.json) and
[Linux](benchmarks/2026-09-26-pgo-linux-build.json). The local candidate image is
`fastsim-perf:pgo-dev`, with manifest digest
`sha256:6bac3d7585081b5935e1d8dfdadc6ede99b8329c63dcee3ad70fcb4cfc273d74`.
Its static x86-64 executable SHA-256 is
`188186c6af5d2bc21856071555cb9acb1cb6bc6db5745830e60e98a66c316c14`.
The published v1 image, release snapshot and `v1.zip` remain unchanged.

To reproduce from the `fastsim` directory, use the scorer's Python environment
for Parquet comparisons and a fresh PGO work directory:

```sh
cargo test --release --locked --offline
python3 tools/pgo_build.py --work-dir target/pgo-new --out target/perf-pgo-new/fastsim
# With Homebrew Rust, also pass --llvm-profdata /path/to/the/matching/llvm-profdata.
../sub/.venv/bin/python tools/bench_compare.py \
  --baseline target/perf-baseline-v1/fastsim --candidate target/perf-pgo-new/fastsim \
  --all --verify --runs 11 --json target/comparison.json
../sub/.venv/bin/python tools/compare_unit.py --bin target/perf-pgo-new/fastsim
../sub/.venv/bin/python tools/fuzz_vs_abides.py 96 --seed 20260926 --jobs 3 \
  --bin target/perf-pgo-new/fastsim --out target/fuzz-new
```

For the next submission, the existing `sub/tools/build.py` stages the PGO builder
and whitelisted public inputs automatically. Select a new release version before
building: the published v1 release is immutable. A direct Docker build can disable
PGO with `--build-arg PGO=0` for comparison. Actual leaderboard improvement requires
submitting and measuring that newly built Linux image on the official hardware.
