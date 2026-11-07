Release 1.13 ("v14") targets the Final's measure: rows divided by each container's whole lifetime.
It makes the message ledger cheaper, shortens what is left to do when a simulation stops, and
takes work out of the event loop of the largest units.

Without a card, every scenario still writes its ledger (unchanged). Every trace and ledger holds
the same rows as release 1.12's. Row groups of single scenarios are cut differently near the
close (see "Chunks taper near the close"), so those files are not byte-identical to 1.12's.

- **Compact ledger rows.** Rows are 40 bytes instead of one 64-byte cache line.
- **Chunks taper near the close.** The work left when the simulation stops is a small chunk, not
  up to 8,192 rows per stream.
- **Wakeups take no message slot.** A wakeup is the one event an agent addresses to itself, and
  its key holds everything about it.
- **A FIFO per wakeup class.** Traders sharing a wakeup interval re-arm in key order, so their
  wakeups need no heap.
- **The book searches from the best price.** The last 8 levels are probed before a binary search.

All numbers come from an i7-10700K (x86-64, runc-like native runs). Each is a whole process run
in the Final layout (scenario inputs only, no card), PGO builds, labels interleaved, warm-up
dropped. "Score proxy" is mean(rows / (F + t)) over the 71 public units, for a fixed per-run
container overhead F.

## Result

Against release 1.12, ten paired runs per unit:

| | Release 1.12 | v14 |
|---|---:|---:|
| Sum of per-unit medians | 594.2 ms | 557.4 ms |
| Mean per-unit ratio | | 0.937x |
| Units > 1% faster / within 1% / > 1% slower | | 66 / 5 / 0 |

| Score proxy | F = 50 ms | 100 ms | 200 ms | 400 ms |
|---|---:|---:|---:|---:|
| v14 vs 1.12 | +1.46% | +1.02% | +0.65% | +0.37% |

| Unit | Release 1.12 | v14 |
|---|---:|---:|
| gb-mega-throughput | 70.2 ms | -8.5% |
| gb-pop-horizon-scale | 44.5 ms | -7.9% |
| gb-highfreq-40hz-60s | 30.2 ms | -7.9% |
| gb-pop-128-agents | 20.6 ms | -8.9% |
| as01-base-mix, as05, eq-* | 2.2-3.0 ms | -8% to -15% |

Development layout (card visible, reported simulation window, capped at 1e7 per unit): unchanged
within noise (9.81M vs 9.80M on this host). A few small units report 2-3% lower in-window rates,
because their last chunks are now handed over inside the window.

## Compact ledger rows

The ledger cost about 6% of each large unit's process (gb-mega: 4.2 ms of 70). Phase marks put
4.4 ms of it inside the event loop: one 64-byte row per delivered message, 1.42 M rows. The tail
added only about 0.3 ms.

- **Row layout.** `LedgerRow` is now 40 bytes: `t_recv`, `t_send`, and five 32-bit fields
  (message id, order id, causal parent, source, destination) plus the kind.
- **Derived latency.** `latency_ns` is computed by the encoder (`t_recv - t_send`, 0 for a
  wakeup).
- **Ids.** A run within the 64 MiB output limit has far fewer than 2^32 messages or orders.
  `u32::MAX` marks a null id.
- **Smaller setup.** The first ledger chunk now takes 80 pages to prepare instead of 128.

The bytes are identical to 1.12's: 190 + 190 parquet files, in both input layouts. The score proxy
gains +0.26% at F = 100 ms, and gb-mega gains 1.0 ms.

## Chunks taper near the close

When a simulation stops, the helpers still had to replay up to 8,192 logged calls and encode up to
8,192 rows of each output. On a 3 ms unit that was half a millisecond.

- **The taper.** Each stream now sizes its next chunk from what remains: about an eighth of the
  rows still expected, between 2,048 rows and the full chunk.
  - "Expected" extrapolates the rows so far over the simulated time left until the close.
  - The streams are the raw trace log, the trace row groups and the ledger row groups.
- **Determinism.** The sizes depend on the logged rows only, so every run of a scenario cuts the
  same row groups. Batch subs keep fixed chunks.
- **Tuning.** The constants were tuned with `FASTSIM_LAB_TAPER=min,share` on a marks build,
  across two sweeps of all single units.
  - 2,048 and 1/8 came out about 0.1% better than 1,024 and 1/4.
  - Smaller minimums (512) were worse.

| Phase (as01, µs) | 1.12 | taper |
|---|---:|---:|
| replay thread done after the run | 523 | 172 |
| encoders drained | 17-283 | 45-117 |

The rows are the same, but the row groups differ. All 71 units pass in both layouts:
- every trace and ledger frame equals the reference row for row;
- `events.json` agrees;
- the official developer verifier admits each unit with no label;
- the two layouts produce byte-identical files.

A mutation check confirmed the checker fails on a swapped ledger and on a missing file.

## Wakeups take no message slot

A wakeup's message was a 64-byte slab entry written at scheduling and read at delivery. Its
contents are all in the event key: the agent, the time and the message id.

- Agents message only the exchange, and the exchange only them. So `sender == recipient` marks a
  wakeup.
- The loop skips the slab for wakeups, and `send_with_id` debug-asserts that no message is
  self-addressed.
- Outputs are byte-identical.
- The large units gain 1-2% (gb-mega 1.0 ms), and the score proxy +0.19% at F = 100 ms.

## A FIFO per wakeup class

On gb-mega the 4-ary wakeup heap took about 9% of the event loop (220 k wakeups, each pop walking
four levels). A trader re-arms its wakeup `interval_ns` after the one it handles. Wakeups are
handled in key order, so the wakeups of the traders sharing an interval arrive in key order.

- **Structure.** `Wakeups` keeps one FIFO per interval class (up to 8) and a heap for anything that
  does not follow its class's last entry: the first wakeups at the open, requeues, and the
  exchange.
- **Order.** The next wakeup is the smallest of the FIFO fronts and the heap root. The order is
  exact by construction: a FIFO only ever accepts a key larger than its last.
- **Tests.** Periodic agents with requeues, and random traffic with random classes, are checked
  against a single reference heap. Both tests fail when the FIFO accepts an out-of-order key.

Outputs are byte-identical. Because small code changes move the PGO layout on this CPU by about
±1%, the effect was confirmed twice:

| | gb-mega | other gb units | score proxy F = 100 ms |
|---|---:|---:|---:|
| normal build | -3.0% | -1.1% to -1.6% | +0.16% |
| JCC-padded build | -4.6% | -2.3% to -2.8% | +0.37% |

## The book searches from the best price

Levels are stored with the best price last. Searches land near it: on gb-mega 43% at depth 0 (a
new best price), 92% within three levels and 99.4% within eight.

On mr-deep-book-state-size, two market makers quote eight levels each, and searches spread evenly
over depths 1-8.

| Probe | gb units | book-heavy mid units | subset score (normal / JCC) |
|---|---:|---:|---:|
| none (binary search) | | | |
| last 8 levels | -0.4% to -1.7% | +0.4% to +2.3% | +0.16% / +0.17% |
| last 2 levels | -0.7% to -1.5% | -1.2% to +0.5% | +0.21% / -1.56% |

The two-level probe's JCC build was 6-8% slower on every unit, book or not: a code layout effect.
The eight-level probe is the one that measured the same in both builds.

## Measured and not kept

- **Join-free end plus a bounded spin.** At the end of the run, nothing waited for the helpers to
  exit, and the drainer watched the state for a few milliseconds before sleeping. Score proxy
  -0.12% (noise).
- **Replay thread watching near the close.** It spun for its last chunks instead of sleeping.
  Score proxy -0.02% (noise).
- **Non-PIE static binary** (`-C relocation-model=static`). Starting the binary is 16 µs faster
  (248 vs 264 µs; a trivial static C binary takes 190 µs). The all-units score proxy moved
  +0.02%, within noise.
