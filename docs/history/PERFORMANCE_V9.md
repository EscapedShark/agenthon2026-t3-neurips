Release 1.8 ("v9") shortens the end of a run inside gVisor.

- **Parallel hash.** The trace's hash now runs beside its write, not after it.
- **Streamed trace.** A scenario run on its own writes and hashes its trace while it runs, as it
  already did the ledger. The trace's `msg_type` column chunks now come last in the file.
- **What changed in the files.** Every trace row is unchanged, but the trace's bytes differ from
  v8's. The ledger is byte-identical.

## What the measurements showed

**The trace's hash ran after its write, not beside it.** For a trace of 256 KiB or more, the main
thread starts a thread to hash it and immediately writes it with one `writev`. Inside gVisor, a new
thread's first system calls wait while a large write copies out of the address space: the
alternate signal stack's `mmap` and the guard page's `mprotect`. The hash thread then started only
when the write was done.

Timed around that step (median of 12 runs, runsc `release-20260803.0`, see PERFORMANCE_V7.md):

| Unit | Hash and write, v8 | Writing after the hash thread runs |
|---|---:|---:|
| t3-gb-mega-throughput (7.8 MB trace) | 8.04 ms | 3.88 ms |
| t3-gb-pop-horizon-scale | 4.80 ms | 2.40 ms |
| t3-mr-cancel-replace-churn | 2.20 ms | 1.25 ms |

An instrumented build that printed a line between starting the thread and writing had shown the
two overlapping. The print gave the new thread time to start, which hid the problem.

**What remained was the whole trace, hashed and written at the end.** v7 streamed the ledger
because its row groups are final when encoded. The trace's `msg_type` column is not: execution
labels are settled only when the run ends. Its six other columns are final when a row group is
encoded, but the file's bytes had to be hashed in order, and each row group's `msg_type` chunk sat
in the middle of it.

## Changes

- **The hash thread starts first.** The writer spins until the hash thread is running. Traces
  that are not streamed (batch subs, library callers) still take this path.
- **`msg_type` chunks go last.** `pq::assemble_pieces_deferred` lays out every other column chunk
  in row-group order, then all `msg_type` chunks, then the footer:
  - each column chunk's offsets in the footer point to where it is;
  - a row group's `file_offset` is its first byte in the file.

  Readers locate column chunks by these offsets, so the file reads as before. Batch subs and
  `output::trace_parquet` use the same layout.
- **The trace is written while the run continues.** The pipeline streams both files the same way:
  each completed row group follows the written ones in order, one thread writes a file at a time
  outside the pipeline lock, and the trace's SHA-256 is updated as its pieces are written. After the
  run, only the `msg_type` chunks (about 0.6 MB for gb-mega) and the footer are written and added to
  the digest. A scenario run on its own creates its three files before simulating, as in v7.

## Measurements

In-process time under gVisor, from the start of `simulate` until `events.json` is written; median
of 11 runs:

| Unit | v8 | + hash first | v9 (+ streamed trace) |
|---|---:|---:|---:|
| t3-gb-mega-throughput | 100.67 ms | 96.44 | **93.45** |
| t3-gb-pop-horizon-scale | 65.76 | 63.40 | **59.17** |
| t3-gb-horizon-240s | 46.12 | 43.93 | **42.64** |
| t3-mr-cancel-replace-churn | 26.47 | 25.45 | **24.39** |
| t3-mp05-cancel-churn-newest | 18.78 | 18.07 | 18.45 |
| t3-mp03-mm-heavy-newest | 10.95 | 10.88 | **10.30** |
| t3-sf-01-baseline-regime | 7.99 | 7.99 | **7.58** |
| t3-as01-base-mix | 4.93 | 5.07 | **4.67** |

Over all 71 units, each run timed around `runsc run` (seven runs):

- the proxy score rose from 2.102M to 2.145M (+2.0%);
- the units' own time fell from 816.8 to 770.8 ms;
- v9 was faster on 47 of 71 units.

The Development window is unchanged. In the Linux VM (card visible, nine runs of all 71 units),
v9's window was 1.002x v8's, with every category between 0.996x and 1.011x, and the capped mean
stayed at 10.000M. The writing and hashing run on the helper threads, not the simulation's. Release
1.9 streams only inside gVisor: with a helper thread on the simulation's core (SMT), the
in-window hash made mid-size units' windows 1.19x as long (see PERFORMANCE_V10.md).

## Validation

- **Rust tests.** All tests pass. parquet-rs reads the new layout back row for row, including a
  trace with two row groups whose `msg_type` chunks are both at the end.
- **pyarrow.** The exported round-trip fixtures (`tools/check_parquet_fixtures.py`, pyarrow
  25.0.1, the scorer's reader) read back exactly: 16 of 16.
- **`tools/compare_unit.py`.** 95/95 public scenarios identical to the reference (every row,
  column and dtype), and 95/95 pass the official gates.
- **Against v8** (all 71 units, static linux/arm64, no card mounted):
  - every one of the 95 traces has `trace_sha256` equal to the file's SHA-256;
  - each trace reads (pyarrow) into a table equal to v8's, schema included;
  - each footer's row count equals `n_events`;
  - all 95 ledgers are byte-identical.
- **Repeats and thread counts.** Every run below was byte-identical:
  - 64 repeated runs over 8 units;
  - 15 runs under a 2-CPU quota;
  - a debug build (assertions on) under CPU affinities of 1 to 4 CPUs, 64 runs over 8 units.
