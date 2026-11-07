Release 1.14 ("v15") targets the Final's measure: rows divided by each container's whole lifetime.
It trims the fixed cost of every run (start-up and the memory a small run touches) and takes
branches and loads out of the event loop.

Without a card, every scenario still writes its ledger (unchanged). Every trace and ledger holds the
same rows as release 1.13's. Row groups are cut differently (smaller first chunks, 4,096-row
chunks), so most files are not byte-identical to 1.13's.

- **A small run's first chunk is small.** It holds an eighth of the expected rows (at least 2,048),
  and only that much memory is prepared before the run.
- **No Rust runtime start-up.** The C runtime calls `main` directly.
- **Branch-free choices in the event queue.** Each lane reports its next event with an "empty" key
  instead of an `Option`, and the Ring's bucket minimum needs no branch.
- **Lighter sends.** A latency sample is one comparison and one load; the agents' pre-drawn values
  are stored round by round.
- **One raw row for an order's two quotes.**
- **4,096-row chunks** for a single scenario (was 8,192).

All numbers come from an i7-10700K (x86-64, native runs). Each is a whole process run in the Final
layout (scenario inputs only, no card), PGO builds, labels interleaved, warm-up dropped. "Score
proxy" is mean(rows / (F + t)) over the 71 public units, for a fixed per-run container overhead F.

## Result

The release binaries extracted from `fastsim-t3:1.13` and `fastsim-t3:1.14`, twelve paired runs
per unit, in both label orders:

| | Release 1.13 | v15 (order A) | v15 (order B) |
|---|---:|---:|---:|
| Sum of per-unit medians | 523.2 / 519.8 ms | 486.1 ms | 488.5 ms |
| Median per-unit ratio | | 0.921x | 0.917x |
| Units > 1% faster / within 1% / > 1% slower | | 70 / 1 / 0 | 70 / 1 / 0 |

| Score proxy | F = 50 ms | 100 ms | 200 ms | 400 ms |
|---|---:|---:|---:|---:|
| order A | +1.52% | +1.02% | +0.63% | +0.35% |
| order B | +1.09% | +0.70% | +0.42% | +0.23% |

Order A's 1.13 runs of gb-pop-horizon-scale caught a host burst (47 ms instead of 41), so its
proxy reads high; the gain at F = 100 ms is about +0.8%.

| Unit | Release 1.13 | v15 |
|---|---:|---:|
| gb-mega-throughput | 64.5 ms | -5.0% |
| gb-pop-horizon-scale | 41-47 ms | -4.9% |
| gb-horizon-240s, gb-highfreq-40hz-60s, gb-pop-128-agents | 18.8-31.3 ms | -4.1% to -5.1% |
| mr-deep-book-state-size | 22.2 ms | 0.0% |
| small units (eq-*, as*, ca-*, ra-*) | 2.1-3.6 ms | -6% to -20% |

Development layout (card visible, reported simulation window, capped at 1e7 per unit): unchanged.
Every unit is at the cap in both releases except s001-price-time-priority, whose 70 µs window reads
2-12M in both from one run to the next.

## Fixed cost per run

### A small run's first chunk is small

At exit a process gives back every page it touched, about 0.27 µs each on this host, after paying
for faulting them in. A small unit touched about 900 pages, half of them for its first trace and
ledger chunks: 8,192 rows each, populated before the run, of which a unit like s019 uses a few
hundred.

- **First chunk.** Each stream's first chunk now holds an eighth of the rows the scenario is
  expected to produce, at least 2,048 (`Taper::first`; the same share and floor as the chunks near
  the close). Only those rows are populated. Spare buffers keep the full capacity, populating only
  as much.
- **Effect.** Pages touched: s019 811 to 361, eq-lognormal-lowsigma 842 to 396, as01 890 to 562.
  Whole process: s019 -17%, eq-lognormal-lowsigma -10%, ca-thin-book-depth -8%; mid and large
  units unchanged.

### No Rust runtime start-up

`#![no_main]`: the C runtime calls `main`. Rust's own start-up reads `/proc/self/maps` and installs
a stack-overflow handler on an alternate signal stack: 18 of the 42 system calls of a run's start.
`main` does the two things the program needs from it: SIGPIPE ignored, and standard descriptors
reopened on `/dev/null` if closed. Test builds keep the harness's `main`.

Start-up of the usage path: 277 to 242 µs (60 interleaved runs).

**Not kept: a non-PIE binary.** Without relocations to apply (2,417 in the PIE binary) the
usage path starts another 16-18 µs faster in plain builds. In PGO builds over all units it measured
+26 µs and 0 µs per small unit in the two label orders, with no score change, so release builds stay
PIE.

## The event loop

On gb-mega the queue (pop, peek, push, deferrals) is about 30% of the main thread, the exchange
(book, reports) 25%, the traders 15%. The profile is flat: no instruction above 2%. These changes
each measured positive in four runs (normal and JCC-padded PGO builds, both label orders):

| Change | Score proxy F = 100 ms | Notes |
|---|---:|---|
| Branch-free bucket minimum | +0.06% to +0.09% | gb-mega -0.4% to -1.3% |
| Lanes report "empty" with a key | +0.10% to +0.23% | gb units -0.3% to -2.6% |
| Latency block in the sampler; draws round by round | +0.05% to +0.15% | median unit -0.4% |
| One raw row for both quotes | +0.06% to +0.14% | median unit -1.6% to -2.0% |
| 4,096-row chunks | +0.04% to +0.12% | mid units faster; mr-deep +1% to +2.5% |

- **Branch-free bucket minimum.** Ring buckets pad their unused slots with the largest key, so the
  smaller of the first two slots is chosen without a branch. A bucket rarely holds more than two
  messages.
- **"Empty" as a key.** The wakeup lane, each message form and the overflow heap report their next
  event as `(key, slot)`, with `u128::MAX` for none. The queue then picks the next event with a
  conditional move, instead of matching three `Option` tuples. Packed message ids stop at
  2^32 - 2, so no real key is `u128::MAX` (the ledger already reserves id 2^32 - 1 for its null).
- **Latency samples.** The sampler holds the producer's current block and its index itself: one
  comparison and one load per sample, instead of a chain through `Option<Prefetch>`.
- **Agents' pre-drawn values.** They are stored round-major (`round * agents + agent`). The agents
  of one interval act in the same bursts, each taking its next round, so one burst's draws share
  cache lines, and the helper writes them in order.
- **Quotes.** An accepted order's best bid and best ask go into one `TraceType::Quotes` raw row: bid
  price and size in `order_id` and `price`, ask price in `size`, ask size in `agent_id`. When the
  ask size needs more than 32 bits, it falls back to two rows. The replay makes the same two `quote`
  calls in the same order, so the trace is identical; gb-mega logs 224,000 fewer raw rows.
- **Chunks.** Smaller full chunks (4,096 rows, 160 kB of ledger rows) shorten what the replay thread
  and the encoders have left when a run stops.

Also kept, with no measurable effect: a trader's market-hours check and its action on a spread reply
now sit in their message's match arm.

### Measured and not kept

| Attempt | Result |
|---|---|
| Ring buckets as chains of pooled nodes (2 kB of heads, first-level cache resident) | gb-mega +8.9%; score -0.69% |
| Ring buckets kept sorted | gb-mega +7.7% to +9.4%; score -0.76% to -0.97% (every push wrote a second, cold line) |
| Prefetch the bucket 24 ahead of the ring's start | +0.02% to +0.08% (noise) |
| Prefetch the next occupied bucket on each pop | -0.06% to -0.09% |
| Bucket minimum by conditional move of both entries | -0.09% to -0.27% |
| Cache the two smallest Ring entries | gb-mega +11.5% to +13.2%; score -1.1% |
| 16,384-row chunks | gb units +0.8% to +3.5%; score -0.25% to -0.32% |

The loop is bound by latency and branches, not cache capacity. Removing branches and loads from the
dispatch helps; any extra work or cold line in a push hurts a lot.

## Correctness

- The tests pass. New or changed tests:
  - `full_width_keys_and_broadcast_ties_keep_tuple_order` now also checks that id 2^32 - 1 falls
    back to tuple order.
  - Mutation checks confirmed the queue tests catch an unsorted chain insert, a missed second-key
    update and a stale moved position.
- All 71 units, both layouts, from the measured build:
  - every trace and ledger frame equals the reference row for row;
  - `events.json` agrees;
  - the official developer verifier admits each unit with no label;
  - the two layouts produce byte-identical files.
- The release 1.14 binary (sha256 `cbc6674d…`) writes the same 374 parquet files, byte for byte,
  as the build that was checked.
- Container validation: 71/71 units in both input layouts.
