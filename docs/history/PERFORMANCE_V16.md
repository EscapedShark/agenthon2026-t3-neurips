Release 1.15 ("v16") is the last release before the Final. It takes branches and loads out of
the event loop of the large units, which hold most of the Final score. Every trace and ledger
file is byte-identical to release 1.14's, in both input layouts.

- **The message kind is stored at send.** A delivery reads it with one load instead of
  dispatching on the payload's variant.
- **A noise trader picks its side without a branch.** Both prices are worked out, and one `place`
  call takes the chosen one.
- **Queue positions name their structure.** `peek` says exactly where the next event is (the
  wakeup lane, the ring or its overflow heap, the message heap, the sorted lane, a waiting group),
  so a pop or a requeue goes straight to it. A message lane that stays a heap pops inline; the
  probe accounting runs only while the lane is still deciding its form. Sends pack the queue key
  from its parts.
- **The book's last trade price is not computed.** ABIDES keeps the fills' average price for
  `QUERY_LAST_TRADE` and the close price. No scenario agent asks for either and no output records
  them, so each fill no longer pays a float division and a rounding call.

All numbers come from an i7-10700K (x86-64, native runs). Each is a whole process run in the Final
layout (scenario inputs only, no card), PGO builds, labels interleaved, warm-up dropped. "Score
proxy" is mean(rows / (F + t)) over the 71 public units, for a fixed per-run container overhead F
(about 64 ms for runc on this host, without a GPU).

## Result

The release binaries extracted from `fastsim-t3:1.14` and `fastsim-t3:1.15` (sha256 `a50ba2f2…`),
twelve paired runs per unit, in both label orders:

| | Release 1.14 | v16 (order A) | v16 (order B) |
|---|---:|---:|---:|
| Sum of per-unit medians | 497.1 / 486.8 ms | 493.8 ms | 483.0 ms |
| Median per-unit ratio | | 0.994x | 0.995x |
| Units > 1% faster / within 1% / > 1% slower | | 25 / 45 / 1 | 21 / 47 / 3 |

| Score proxy | F = 50 ms | 100 ms | 200 ms | 400 ms |
|---|---:|---:|---:|---:|
| order A | +0.15% | +0.09% | +0.05% | +0.03% |
| order B | +0.21% | +0.14% | +0.08% | +0.04% |

| Unit | Release 1.14 | v16 |
|---|---:|---:|
| gb-mega-throughput | 61.3-61.7 ms | 0.0% to -0.6% |
| gb-pop-horizon-scale | 39.2 ms | -2.1% to -2.2% |
| gb-horizon-240s, gb-highfreq-40hz-60s | 26.6-30.0 ms | +0.4% to -1.9% |
| gb-pop-128-agents | 17.7-17.8 ms | -3.8% to -4.1% |
| mr-deep-book-state-size, mr-cancel-replace-churn | 18.7-22.2 ms | -0.8% to -1.2% |
| mp05-cancel-churn-newest | 12.4-12.5 ms | +0.2% to +0.7% |

The units more than 1% slower differ by order (many-6 +1.1% in A; varsize, ra04 and s001 +1.1% to
+1.4% in B), all of them runs under 3.5 ms: run-to-run noise at that size.

PGO builds of one source are not identical (the training runs' counts vary with thread timing).
A first build of this source (binary `c1a3201e…`, replaced when a test's name changed) measured
+0.14% to +0.19% at F = 100 ms in the same comparisons: builds differ by about 0.05%, and how a
layout fares on this host need not carry over to another CPU.

## Where the Final score is

Each unit's share of the score proxy at F = 64 ms, and what 1 ms less of its process time is
worth (the v16 code before its last change, which measured neutral; 12 runs per unit):

| Unit | Process time | Share | Score per -1 ms |
|---|---:|---:|---:|
| gb-mega-throughput | 61.4 ms | 9.9% | +0.079% |
| gb-pop-horizon-scale | 38.5 ms | 7.5% | +0.073% |
| gb-horizon-240s | 30.0 ms | 6.3% | +0.067% |
| gb-highfreq-40hz-60s | 26.7 ms | 5.9% | +0.065% |
| mr-cancel-replace-churn | 18.6 ms | 4.1% | +0.050% |
| gb-pop-128-agents | 17.2 ms | 4.0% | +0.049% |
| mr-deep-book-state-size | 21.7 ms | 4.0% | +0.046% |
| mp05-cancel-churn-newest | 12.6 ms | 3.3% | +0.044% |

The five gb units hold 34% of the score: each 1% faster process there is worth +0.12%, three
times what 1% of the mr/mp units is worth (+0.04%). The six batch units together hold 4%.

## The event loop

The main thread of gb-mega spends 94% of its process time (57.5 of 61 ms) in the event loop. Per
event (1.48 million loop iterations: 81% ring messages, 15% wakeups, 4% busy requeues) it runs
about 420 instructions in 170 cycles, IPC 2.5. TopDown level 1: retiring 61%, bad speculation 14%,
front end 7%, back end 18%; 1.2 branch misses per event.

- **Instructions.** The queue (peek, pop, push, requeues) is about 35% of them, and the exchange's
  handling of a message (the book and the reports it sends) about a quarter of the cycles. No
  single source line exceeds 5% of either.
- **Branch misses.** The largest sources are the dispatch between the exchange and a trader
  (12%: a delivery's recipient follows no pattern), the scan of a ring bucket holding more than
  two messages (9.5%), a trader's match on the message kind (6%) and the book's matching (8%).
- **After the loop.** The trace's `msg_type` chunks (0.48 MB on gb-mega) and the footers are
  written and hashed when the run ends: the trace hash covers the file in order, and a later fill
  of an order relabels an earlier row as `PARTIAL_FILL` in any row group. That tail is 2.1 ms on
  this host, whose CPU hashes in software; the Final's Xeon has the SHA extensions.
- **Memory.** Peak RSS is 11.4 MB on gb-mega and about 3 MB on a small unit, so the exit (0.27 µs
  per touched page) costs about 0.75 ms and 0.2 ms.

### Measured and not kept

| Attempt | Result |
|---|---|
| 1,024 ring buckets of half the width | score proxy -0.1% |
| One dispatch on the message kind, read from the message | +6% cycles on gb units: the jump waits for the message's load, so its mispredictions resolve late |
| The message kind in the key's slot bits | +2% cycles on gb units |
| Ring entries of 16 bytes (time from a base, the route in 17 bits, id and slot in one `u128`), the minimum of a bucket's first four entries chosen without a branch | event loop: branch misses -20%, instructions +8.5%, cycles -1.7% to +4.9% by unit; score proxy -0.03% to +0.06% |
| Ring buckets aligned to a cache line | event loop cycles within ±1% |
| Development batch fix (no file-preparing thread, helpers only for a sub larger than all others) | Development: 4 dedicated CPUs -10% to -26% per batch; Final (CPU quota on an idle host): batch units +2% to +4%, hetero-mix +27%, score proxy -0.008% at F = 100 ms |

The loop is now bound by its instructions and by mispredictions it cannot avoid (which agent
receives the next message): every remaining change measured within the noise of a run.

## Development batch units

The Development platform scored v15's homog-4 and dense-3 below the 1e7 cap (8.5M and 9.1M; x86:
19M and 14M). Its single units run at this host's speed (window ratio 0.975 over 65 units), and
its batch windows vary 1.0-2.2x between submissions.

- **Cause.** A batch starts as many threads as it has CPUs, plus one that creates the output
  files. With only 4 CPUs, Linux can leave a worker without one for about 2 ms, and its sub then
  runs after another thread's. On x86 with 4 dedicated cores homog-4 takes 2.4 ms (1.75 ms with
  free CPUs), 3.4 ms with one CPU hog beside it; single units are unaffected.
- **Not changed.** The Final runs each container under a CPU quota on an exclusive host, where
  the current design is faster (see the table above). Development scores do not count.

## Correctness

- The tests pass (34). New:
  - `exchange_traffic_keeps_the_single_queue_order`: the engine's traffic (wakeups, messages
    between the exchange and one agent, broadcasts) against a single reference heap, in every
    message lane form.
  - `ring_messages_far_apart_or_in_the_past_keep_key_order`: past times and times far beyond a
    ring's span.
- All 71 units, both layouts: the release binary (sha256 `a50ba2f2…`) writes the same 374
  parquet files, byte for byte, as release 1.14 (and so did the first build, `c1a3201e…`).
- Container validation: 71/71 units in both input layouts.
