# Method

This describes release 1.26, the Final submission. The history of each change, with measurements,
is in [RELEASES.md](RELEASES.md) and [history/](history/).

## 1. Task and score

ABIDES is a Python discrete-event simulator of a limit-order-book market: one exchange, noise,
value, momentum and market-making agents, a mean-reverting fundamental with jumps, and stochastic
message latency. Track 3 asks for a faster simulator with the same output. The trace and the
message ledger must match the baseline in every row.

```
rate       = verified rows ÷ (FinishedAt − StartedAt)     Docker's own timestamps
unit score = median of 4 timed runs, after 1 warm-up      all 5 byte-identical
Final score = arithmetic mean of unit scores
```

The runner reads the row count from the Parquet footer and ignores self-reported speed. Containers
run under runc with `--network=none --cpus=4 --memory=16g` and a GPU attached, on a Xeon Platinum
8570 VM with an NVIDIA B200. One wrong row fails the unit, and repeats that disagree get the entry
refused.

The organizers' calibration
([#24](https://github.com/Agenthon-2026/track3-simulation-public/issues/24),
[#34](https://github.com/Agenthon-2026/track3-simulation-public/issues/34)) measured a static binary
in an empty image that only copies the reference outputs: a median window of 170 ms, range 142 to
204 ms, 50 runs. Docker stamps `StartedAt` just before the task is created, so runc's GPU hooks are
inside that window. A `python:3.13-slim` image doing the same copy took 343 ms.

fastsim's whole process takes 4.2 ms on a median public unit and 47 ms on the largest (1.17M rows)
on an i7-10700K. The floor and these times were measured apart, so read them as scale. Rates are
averaged, so large units dominate: on the public set at this floor, 5 of 71 units hold 39% of the
score. 3.25M rows/s on this floor needs at least 0.55M rows per Final unit, five times the public
mean, so the sealed units are larger than the public ones.

## 2. Exactness

Each item below was checked against the reference outputs and the NumPy 1.26.4 source.

**Random numbers**
- `src/rng/` reproduces NumPy's legacy `RandomState` (MT19937) bit for bit. Legacy `pareto` is
  `exp(E/a) - 1`, not `expm1`; the two differ by up to about 28,000 ULP.
- The seed chain is reproduced in full: `np.random.seed(seed)`, then one `u32` per generator in
  order (oracle, exchange, each agent, latency model, kernel). The oracle's constructor draws one
  `exponential` from the global stream between the oracle's and the exchange's seeds.
- NumPy's arm64 wheels use fused multiply-add, so golden data is generated on linux/amd64. The
  image links glibc 2.41 statically, the libm the reference outputs were generated with.

**Event order**
- Queue entries sort by `(delivery time, sender, receiver, message id)`, and ids come from one
  global counter that wakeups also use.
- A message reads the compute delay before it is handled and a wakeup after, and the exchange
  changes its delay while handling a message.
- The market-close broadcast is one message with one id, sent in subscription order, with one
  latency draw per recipient.
- Order-book reports go out in ABIDES's order, because each send takes a message id and a latency
  sample.

**Numbers and files**
- The first megashock time is a float and later ones mix int and float; latencies round half to
  even after clipping.
- Without `latency_config`, the baseline falls back to fixed pairwise latencies, and so does
  fastsim.
- Parquet files carry the pandas metadata block. Without it, nullable `t_send_ns` reads as float64
  and loses precision at 1.6e18 ns.

**The bug the public scenarios could not show.** ABIDES holds megashock times as `np.float64`, and
NumPy compares an int with a double by rounding the int to a double first. At 1.6e18 ns, doubles
are 256 ns apart. So a trader observing 90 ns after a megashock still sees the old fundamental.
fastsim compared exactly, applied the jump and sold at the bid, where the ABIDES trader did
nothing. Expected hits over all 95 public scenarios: 0.0002. Differential fuzzing found it in 1 of
600 runs, more than two weeks after the first 95/95. Release 1.26 compares as NumPy does. See
[history/PERFORMANCE_V27.md](history/PERFORMANCE_V27.md).

## 3. The event loop

The loop is single-threaded, because it defines the order.

- **Sort key.** Time, sender, receiver and message id fit one `u128`, so a compare has no branches.
  Agent ids over 2^16 or message ids over 2^32 fall back to a binary heap of tuples.
- **Wakeup lane.** Traders with one wakeup interval form a class, and each class arrives in key
  order, so a FIFO per class replaces a heap. Out-of-order wakeups go to a small heap.
- **Message lane.** It takes one of three shapes, chosen from the run's own data, so the choice is
  deterministic: a sorted array for constant latency, a 4-ary heap by default, or a ring once 16 or
  more messages are pending on average over the first 4,096 pops. The ring has 512 buckets of up to
  8 entries and an overflow heap. It retunes its bucket width from the buckets it pops, with a guard
  that keeps it spanning nine in ten pending messages.
- **Busy receivers.** Messages for one busy agent share a deadline and requeue as a group, through
  an indexed heap with one entry per agent.
- **Messages.** 64 bytes with one type tag, read once at delivery. Wakeups take no message slot.
- **Order book.** Levels keep the best price at the end of an array. Search looks at the last 8
  levels, then bisects. Reports are sent during matching, not buffered.

The next event is the minimum over the three lanes, selected with conditional moves.

## 4. Threads and determinism

With a budget of 4 CPUs, the main thread runs the loop and helpers do the rest.

| Helper | Work | When |
|---|---|---|
| Latency look-ahead | Draws log-normal and Pareto latency samples ahead, in stream order | Started before the run; the loop stops waiting for samples |
| Agent draws | Computes noise and value traders' draws by wakeup-interval group | A burst at the start of a run |
| Trace replay | Turns raw events into final trace rows, as the baseline's `trace.py` does | At least 4,096 expected rows |
| Encoders | Encode row groups into Parquet column chunks | The remaining CPUs |

Small runs use no helpers at all. Runs of 100,000 rows or more poll every 10 to 20 µs instead of
sleeping, which took the main thread's `FUTEX_WAKE` calls on the largest unit from 592 to about 30.

Determinism does not depend on timing. Helpers either precompute values from fixed streams, which
changes who computes a value but not the order it is drawn in, or fill numbered slots. Row groups
are cut by counts of recorded rows. Output bytes are the same for any thread count.

## 5. Output

A hand-written Parquet writer (`src/pq.rs`) supports exactly what the task needs: flat `INT32`,
`INT64` and UTF-8 columns, `DELTA_BINARY_PACKED` integers, dictionary strings, and Snappy framing
with literal runs only, which every reader decodes.

Files are written while the simulation runs and hashed with SHA-256 as they are written. The
`msg_type` column chunks go at the end of the file, because a later fill can relabel an earlier
row; those labels are fixed in place in the already encoded chunk. Row groups shrink near the end of
the run, so little work is left when the simulation stops.

## 6. Process and image

- **Binary.** Static, at a fixed address (no self-relocation), x86-64-v3, fat LTO, PGO with ext-TSP
  block placement. `#![no_main]` skips Rust's runtime start, taking start-up system calls from 42 to
  24. `malloc` is tuned to avoid repeated heap growth, transparent huge pages are off, and the first
  row group is prefaulted.
- **PGO builds are draws.** Thread timing in training makes two builds of one source differ by 1 to
  2%. Release 1.26 is the fastest of 12 builds, measured on 71 units. Training under Docker Desktop's
  emulation cost 2 to 4% on the big units, so the release was built natively on x86.
- **Empty root.** The GPU hook mounts drivers and runs `ldconfig` in the container's root, so the
  image is `FROM scratch`. On the lab, the smallest unit's window went from 162 to 124 ms.
- **No copy-up on the clock.** An image directory that gets a new entry during the start is copied
  up by overlayfs. The image has no `/usr`, so injection creates it fresh, and a `WorkingDir` set in
  the image config only makes Docker copy up `/etc` at create time, before the clock starts. On the
  lab, 110 to 102 ms.

## 7. Verification

| Check | Result at release 1.26 |
|---|---|
| Public scenarios against the reference | 95/95 identical in rows, columns and dtypes |
| Differential fuzzing, cached ABIDES runs re-checked | 2,274 identical |
| Boundary campaign on the megashock compare | 600 identical, 557 boundary hits |
| Container validation, two input layouts | 142/142 runs pass gates g0 to g3 |
| Output compared with release 1.25 | 374 Parquet files byte-identical |
| Unit tests | 45 Rust tests; mutation checks on the queue |

Fuzzing draws random scenarios across every parameter the baseline reads, runs them through the
real ABIDES and through fastsim, and demands identical rows. Scenarios where ABIDES itself raises
`KeyError: 'order_id'` have no reference and are not counted.

## 8. Measurement

The Development board could not guide this work. One binary scored 208.7k to 221.1k there, and each
unit moved by about ±30 ms between uploads. Decisions came from local runs instead:

- whole-process time of released binaries, with versions interleaved and the order reversed in a
  second batch, on all 71 units;
- `perf` cycle and instruction counts;
- Final-style container runs with a GPU, read from Docker's timestamps
  ([evidence/measurement/](../evidence/measurement/));
- a score proxy, mean(rows ÷ (F + process time)), reported at several values of the fixed overhead F.

Against ABIDES, on one i7-10700K with the Final binary, the median speedup over 65 public scenarios
is ×880, from ×66 on the smallest to ×1,765. The largest unit takes 87.6 s in ABIDES and 50.3 ms in
fastsim. ABIDES is timed on its simulation loop only and fastsim on its whole process, which favours
ABIDES. Its times on that machine are 0.96× the official reference times at the median, so the
comparison sits close to the organizers' own numbers. The data is in
[evidence/same-machine/](../evidence/same-machine/).

On the lab, three image changes gained +35% and 13 program releases +12%, though the program gains
were mostly measured with the proxy at about 100 ms of overhead. The per-release data is in
[evidence/final-timing/release_gains.csv](../evidence/final-timing/release_gains.csv).
