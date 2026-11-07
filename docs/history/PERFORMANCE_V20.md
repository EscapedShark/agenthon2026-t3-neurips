Release 1.19 ("v20") changes the event loop of release 1.15 (the binary of releases 1.16 to
1.18); the image is release 1.18's. Every trace and ledger file is byte-identical to release
1.15's, in both input layouts.

## The changes

Profiled on gb-mega (x86, Final layout): 1.48M loop iterations for 1.17M trace rows, 419
instructions and 169 cycles per iteration, 1.22 branch misses per iteration; the queue pop is a
quarter of the instructions, the exchange's limit-order handling (book plus reports) a fifth.

- **Reports sent as the book emits them.** `Book::handle_limit_order` and `Book::cancel` hand
  each report to a closure (`Reports::send` in `engine.rs`) instead of appending it to a `Vec`
  the kernel drained afterwards. The kernel's queue, slab, latency sampler and message counter
  are borrowed apart from the book, so nothing the book does waits on a send and the order of
  sends (message ids and latency draws) is unchanged. The round trip through the buffer was
  the costliest part: whole-process cycles -0.8..-2.7% on the big units.
- **Ring entries of 20 bytes.** A ring bucket is now a 64-byte-aligned 192-byte block of eight
  packed `(key, slot)` entries, so its first three entries share one cache line, and the bucket's
  smallest key is found among those three without a branch (slots past the bucket's length hold
  the empty key). The `for i in 2..len` loop that followed was the loop's worst predicted
  branch; gb units lose 7-15% of their branch misses.
- **Ring span anchored at the current time.** The ring no longer records the bucket of the last
  message taken: a message is let in when it lies within the ring's span from the bucket of the
  queue's current time, which is the latest time an event was taken at and never decreases
  (`EventQueue::peek`), so nothing in the ring ever falls out of the span. Saves the bookkeeping
  on every pop.
- **One `busy_until` store per event**, `now + delay` taken before a message's handler (the
  delay in force before it, as ABIDES: the exchange resets its own inside the handler for the
  next message) and after a wakeup's.

Tried and rejected this round: the event loop instantiated per queue form (no gain, and the
heap-to-ring switch mid-event broke exactness), a message slab indexed by message id (+10%
cycles: streaming through the slab defeats the free list's reuse of cache-hot slots).

## Measurements

x86 (i7-10700K), native, Final layout (scenario inputs only, outputs written while running).

Whole-process cycles (`perf stat`), 8 runs per label, labels interleaved, median per unit, two
independent PGO builds of each source (normal and with branches aligned to 32-byte boundaries):

| Unit | normal builds | aligned builds |
|---|---:|---:|
| gb-mega-throughput | -1.3..-2.0% | -1.9% |
| gb-pop-horizon-scale | -1.8..-2.3% | -2.1% |
| gb-pop-128-agents | -1.6..-2.1% | -1.8% |
| gb-horizon-240s | -1.3..-1.6% | -1.0% |
| gb-highfreq-40hz-60s | -0.9..-1.2% | -1.0% |
| mr-deep-book-state-size | -0.7..-2.9% | -2.5% |
| mr-cancel-replace-churn | -1.0..-1.4% | -1.8% |
| s012-partial-fill-cancel-race | -1.2..-1.4% | -1.8% |
| sf-01-baseline-regime | -1.1..-1.4% | -1.6% |
| eq-deterministic-baseline | -1.1% | -1.4% |

All 71 units, whole-process wall clock, 10 runs per label in each of the two label orders:

| | order A | order B |
|---|---:|---:|
| Sum of the units' median own time | -2.4% | -1.7% |
| Units faster | 65 / 71 | 66 / 71 |
| Final proxy (mean of rows / (F + own time)) at F = 125 ms | +0.38% | +0.15% |
| at F = 250 ms | +0.24% | +0.08% |
| at F = 450 ms | +0.15% | +0.05% |

The gain is small because the container's own time dominates every unit's clock (see
PERFORMANCE_V17.md): the simulator is ~10% of a unit's Development time and, on x86 with a GPU,
~30% of gb-mega's Final window.

## Correctness

- `cargo test --release`: all tests pass.
- Native x86 builds: 190 parquet files of the Final layout and 184 of the Development layout
  (card mounted) byte-identical to release 1.15's outputs (families `aabe9400…` / `99a03925…`).
- Release image `fastsim-t3:1.19` (manifest `sha256:d77c3d3b…`, binary sha256 `3e3a6317…`, 1,845,488
  bytes, static PIE): container validation on 71 units in both input layouts, and on x86 with a
  Tesla T10 (`--gpus all`) 71 units in the Development layout byte-identical to release 1.18's
  outputs; the extracted release binary's outputs byte-identical to release 1.15's in both
  layouts (see `sub/releases/1.19/`).
