Release 1.9 ("v10") keeps release 1.8's gains inside gVisor, stops writing during the run
natively, and shortens batches whose subs do not divide evenly between the CPUs. Every output file
is byte-identical to 1.8's.

- **Streaming only inside gVisor.** A scenario on its own writes and hashes its outputs while it
  runs only inside gVisor. Natively it writes them after the run, as release 1.5 did.
- **Batch: simulations first.** A thread whose sub has finished simulating hands that sub's
  outputs to a thread with nothing left to start, and starts the next sub itself.
- **Batch: idle threads help.** Threads with no sub left to start take encoding jobs from the
  subs still running.

## Streaming only inside gVisor

Release 1.8 wrote and hashed the trace on helper threads during the simulation. Inside gVisor
that pays: writing costs several times what it does natively there. Natively it buys nothing, since
the reported clock covers the simulation alone. It also costs something when a helper thread shares
a physical core with the simulation. The SHA-256 then takes cycles from the simulation's core.

- **Detection.** `main` already calls `prctl(PR_SET_THP_DISABLE)`. gVisor rejects that call with
  `EINVAL`. When the call succeeds, the kernel's release string is checked too: gVisor's is
  `4.19.0-gvisor`, which covers a later gVisor that accepts the call. Both checks were verified
  natively and under runsc `release-20260803.0`.
- **Override.** `FASTSIM_STREAM=1` or `=0` forces either path (testing).
- **Same bytes.** Streamed and non-streamed outputs are byte-identical: the layout is the same.

Native windows on an i7-10700K, no PGO, seven runs per unit, against 1.8's source:

| Placement | All 71 units | mid (60-130k rows) | large (>130k) |
|---|---:|---:|---:|
| 4 threads on 2 cores (SMT siblings) | **0.906x** | 0.841x | 0.832x |
| 4 separate cores | 0.992x | 0.995x | 0.996x |

Inside gVisor, 1.9 detects the sandbox and streams. Its in-process times equal 1.8's (gb-mega
104.6-106.2 ms for both).

## Batch scheduling

A batch sub's clock covers every step. For an 8,000-row sub on the i7:

| Step | Time |
|---|---:|
| Simulation | 1.0 ms |
| Replay | 0.13 ms |
| Encoding (trace 0.175, ledger 0.25) | 0.43 ms |
| Hash + write | 0.24 ms |

Two batch shapes left CPUs waiting.

- **More subs than CPUs** (many-6: 6 subs, 4 CPUs). Round two started only after round one's
  outputs were written, then ran on two threads while the other two exited.
- **Uneven subs** (varsize: 5k to 12k rows). The small subs' threads finished early and waited
  while the largest sub encoded alone.

### Changes

- **`output::start` and `Running::finish`.** `output::start` runs setup and the simulation, and
  returns a `Running`. `Running::finish` replays, encodes and assembles the files, on any thread.
  `simulate_with` is `start(..).finish()`.
- **Handing outputs over.** A thread whose simulation ends hands the sub's outputs over when both
  hold:
  - subs remain to start;
  - fewer remain than the batch has threads.

  It then starts the next sub. With 8 subs on 4 threads nothing is handed over: every thread has a
  second sub anyway, and handing over would only keep more memory live.
- **`output::Help`.** A thread with no sub left to start does two things:
  - it finishes the subs handed to it;
  - it takes queued encoding jobs from subs that have no workers of their own.

  A sub queues a job only while such a thread exists. It runs whatever is left itself when it
  drains, so nothing waits on a helper.
  - **Sleeping.** Helpers spin briefly, then sleep until work arrives. An earlier version spun and
    yielded instead. Under a 4-CPU affinity that made hetero-mix 27% slower, because the spinning
    threads took cycles from the busy ones.
- **Ledger first.** When another thread can encode the ledger's last row group, it is queued
  before the trace's replay, which it does not depend on. Otherwise it stays after the trace
  encoding, as before. Doing it first on the same thread cost homog-8 5%: the ledger encoding
  pushed the just-logged trace out of cache before the replay.

### Measurements

Batch clocks on the i7 without CPU pinning (the official runner uses a `--cpus` quota, not
pinning), median of 61 runs:

| Unit | 1.8 + streaming change | 1.9 |
|---|---:|---:|
| gbatch-many-6 | 3.43 ms | **2.95 ms (-14%)** |
| gbatch-varsize | 2.79 ms | **2.50 ms (-10.5%)** |
| gbatch-dense-3 | 2.14 | 2.11 |
| gbatch-homog-4 | 1.94 | 1.94 |
| gbatch-homog-8 | 3.44 | 3.47 |
| gbatch-hetero-mix | 4.17 | 4.18 |

In the Linux VM (the Development proxy, seven runs), against release 1.5:

- many-6 went from 3.23 to 2.67 ms, and varsize from 2.42 to 2.07 ms.
- Every batch now needs at most 0.70x of the time the rate cap allows. Release 1.5's worst was
  0.71x, on varsize.

## Against release 1.5 (v6, the best Development score so far)

Both built from source without PGO.

**Development window (Linux VM, card visible, seven runs of all 71 units).**
- Every category was 0.95x to 0.96x of 1.5's window; batches were 0.93x.
- The capped mean stays at 10.000M for both.
- The uncapped mean rose from 17.33M to 18.17M.

**Final proxy (runsc, no card, five runs after a warm-up, all 71 units).**

| | 1.5 | 1.9 |
|---|---:|---:|
| Linux VM (arm64): mean rate | 1.981M | **2.198M (+10.9%)** |
| Linux VM: units' own time | 983 ms | 759 ms |
| i7 (x86-64, rootless runsc): mean rate | 1.244M | **1.387M (+11.4%)** |
| i7: units' own time | 1,244 ms | 884 ms |

The official timer wraps the whole `docker run`, so its fixed cost is larger than runsc's alone
(28 ms in the VM, 51 ms on the i7). The same own times give smaller gains as that cost grows:

| Fixed cost per run | 100 ms | 200 ms | 400 ms | 800 ms |
|---|---:|---:|---:|---:|
| Gain of 1.9 over 1.5 (VM / i7) | +6.2% / +8.0% | +4.1% / +5.2% | +2.4% / +3.1% | +1.4% / +1.7% |

## Tried and dropped

- **Populating each batch thread's heap inside the clock.** Neither `MADV_POPULATE_WRITE` nor
  touching 1.5-3 MiB made it cheaper: four threads populating at once were 0.3-1.5 ms slower than
  faulting on demand. The faults are real, though: a sub on a thread whose heap is already in use
  simulates in 0.78 ms instead of 0.97.
- **Warming the heap before the clock.** Not done: it would only move work out of the reported
  clock.
- **Transparent huge pages for batch heaps.** 30-40% slower: every first touch of a 2 MiB region
  clears all of it.

## Validation

- All Rust tests pass. `cargo clippy` adds no new warnings.
- **`tools/compare_unit.py`.** 95/95 public scenarios identical to the reference and 95/95 pass
  the official gates, both with and without streaming.
- **Byte identity with release 1.8** (x86-64, all 71 units): all 190 Parquet files are identical,
  and every `trace_sha256` equals its file's SHA-256.
- **Streamed and non-streamed output are identical** (all 65 single units, trace and ledger).
- **Repeats and CPU sets** (8 units, including all 6 batches), under affinities of 1, 2, 3, 4 and
  8 CPUs and SMT pairs:
  - 672 runs of the release build, all identical;
  - 320 runs of a build with debug assertions and overflow checks on, all identical and identical
    to the release build.
- **Under runsc.** 60 runs over 10 units (all batch shapes, the largest units, s001) are identical
  to native runs.
