Release 1.6 ("v7") targets the runtime the Final ranks under: gVisor. It removes most of the
system calls v6 made while a run was under way and writes the ledger while the simulation runs.
Every output file is byte-identical to v6's.

## How it was measured

- **Sandbox.** runsc `release-20260803.0`, the version T3 `AGENTS.md` names (the aarch64 build,
  sha512 checked). It runs inside a privileged `debian:trixie-slim` container in Docker Desktop's
  Linux VM, with `--cpus 4 --memory 6g`, as `runsc --ignore-cgroups --network=none run` with an
  OCI bundle close to the platform's:
  - a read-only root, which is a copy of trixie-slim (with the container's own `/` as the root,
    bind mounts show up empty);
  - uid 65534;
  - `/input` holding only `scenario.json` or `scenarios/`, with no card, as in the sealed tree;
  - `/output` on an ext4 Docker volume;
  - a 64 MiB `/tmp`.
- **Binaries.** Static aarch64 builds of the source (release profile, no PGO).
- **Timing.** A run is timed around `runsc run`. An empty static binary takes about 27 ms
  (sandbox start and teardown), the same for every submission. A unit's "own time" below is its
  median minus that. Whole runs vary by 1-2 ms, so small A/B comparisons used the in-process time
  instead (the `FASTSIM_TIMING` mark after `events.json` is written).
- **Proxy score.** The mean over the 71 public units of each unit's median of per-run rates, each
  rate being rows / (whole `runsc run` time). That is the Final's formula, with this sandbox's
  fixed cost.

## What the measurements showed

**System calls, page faults and writes cost several times more than natively; computation does
not.** Microbenchmarks in the same sandbox, against the same binary run directly in the VM:

| Operation | gVisor | VM, no sandbox |
|---|---:|---:|
| System call (`getppid`) | 2.5 us | 0.14 us |
| `Condvar::notify_all` with no waiter (a futex call) | 2.0 us | 0.14 us |
| `mprotect` | 3.3 us | 0.8 us |
| Thread spawn, cost to the caller | ~130 us | ~30 us |
| Thread spawn + join | 113 us | 45 us |
| First write to a fresh 4 KiB page | 1.3 us | 0.06 us (THP) |
| Create + close a file in `/output` | 45 us | 11 us |
| Writing a large file | 0.44-0.58 ms/MB | 0.15 ms/MB |
| `Instant::now`, first ~100 ms of a sandbox / later | 2 us / 0.02 us | 0.02-0.04 us |
| Random read-modify-write, 1-16 MiB set | 1.9-2.0 ns | 1.9 ns |
| Random read-modify-write, 64 MiB set | 3.9 ns | 3.2 ns |

More on memory:

- `MADV_POPULATE_WRITE` fails with `EINVAL`, so populating memory falls back to touching each page.
  `MAP_POPULATE` is no cheaper (1.0 us per page, then 0.6 us on first use).
- Application memory is a 4 KiB-page memfd: runsc passes `--host-thp-shmem-enabled=never` from the
  host. That is why the 64 MiB random set is slower.
- When a mapping grows contiguously into a new 2 MiB unit, gVisor maps the merged range again, and
  every page already touched in it faults once more (0.33-0.47 us each).

**v6 lost most of its time to its own system calls.**

- **`mprotect` from malloc.** `t3-gb-mega-throughput` made 4,252 system calls, 1,785 of them
  `mprotect` from the encoder and replay threads. glibc grows a thread's arena heap by exactly
  the bytes it needs, one `mprotect` per step.
- **Futex wakes on the simulation thread.** It made 1,174 futex wakes, one per handoff:
  - latency blocks: 586;
  - the encoder pipeline's condvar, notified after every job: 430;
  - replay chunks: 138.

  Each wake is a system call. A probe on the simulation thread measured handoffs at 17 us median
  inside gVisor against 5.5 us natively.
- **Serial writes at the end.** The outputs were written one after the other once the run was
  over. gb-mega's 7.8 MB trace and 14.2 MB ledger took 11.7 ms, against 3.4 ms natively.

| `t3-gb-mega-throughput`, v6, median of 7 | VM | gVisor |
|---|---:|---:|
| `Sim::run` window | 77.4 ms | 111.7 ms |
| Setup before the run + encoding tail | 2.4 ms | 8.2 ms |
| Hash + write outputs | 3.8 ms | 12.8 ms |
| Before `simulate` and after `events.json` (start, exit) | ~2.6 ms | ~10 ms |

A mid-size unit (`sf-01`, 72k rows) paid about 2.9 ms more in gVisor, 1.2-1.5 ms of it in setup:
each helper thread's creation (~0.14 ms each) and the pages populated before the run.

## Changes

- **Arena top pad.** `mallopt(M_TOP_PAD, 64 MiB)`: each thread's arena starts with 64 MiB of
  usable heap (untouched pages cost nothing), so it no longer grows one `mprotect` at a time.
  Together with the next two changes, gb-mega went from 4,252 system calls to 948 and sf-01 from
  415 to 194. On a native kernel with transparent huge pages set to `always`, the pad made the
  simulation 3-7% slower; release 1.7 turns THP off for the process (see PERFORMANCE_V8.md).
- **Larger latency blocks.** The lookahead producer's blocks double from 2,048 samples up to
  16,384. The first block is still ready soon after the run starts. gb-mega hands over about 75
  blocks instead of 586. The samples and their order are unchanged.
- **Notify only a waiting thread.** The encoder pipeline counts idle workers and knows when the
  drainer waits. A submitted job wakes a worker only if one is idle, and a finished job notifies
  only a waiting drainer.
- **The ledger is written while the run continues.** A single scenario creates its three files
  first.
  - When an encoder finishes a ledger row group, the finished groups that follow the
    written ones go to the file in order and their bytes are freed. One thread writes at a
    time, outside the pipeline lock, and a job stays pending until its writes are done.
  - After the run only the last group and the footer remain. The footer is built from each
    chunk's recorded size (`ColumnChunk::take_bytes`).
  - The trace still waits for its `msg_type` column, which is final only at the end.
  - Batch subs are unchanged: their files come from the helper that prepares them.
- **Counted spins.** Batch `wait_for` counts its spins instead of reading the clock, which is a
  system call early in a gVisor sandbox.
- `TraceBuilder::with_sink` no longer builds a 64K-row buffer only to replace it (2.6 MB).

## Measurements

v6 (release 1.5 source) and v7, run alternately with an empty binary in one container, nine
runs each per unit. Own time is the median minus the empty binary's (gVisor 26.95 ms, VM
1.33 ms). Units below about 10 ms are within the whole-run noise.

| Public unit | Rows | v6 gVisor (ms) | v7 gVisor (ms) | v6 VM (ms) | v7 VM (ms) |
|---|---:|---:|---:|---:|---:|
| t3-eq-lognormal-lowsigma | 14,379 | 6.34 | 4.57 | 2.51 | 2.34 |
| t3-sf-01-baseline-regime | 71,932 | 7.96 | 5.35 | 5.37 | 5.22 |
| t3-mp03-mm-heavy-newest | 120,920 | 12.11 | 9.02 | 7.50 | 7.15 |
| t3-mp05-cancel-churn-newest | 240,829 | 20.23 | 17.09 | 14.20 | 13.64 |
| t3-mr-cancel-replace-churn | 322,007 | 33.27 | 27.59 | 20.74 | 20.32 |
| t3-mr-deep-book-state-size | 319,488 | 35.16 | 30.27 | 25.15 | 24.73 |
| t3-gb-pop-128-agents | 303,603 | 37.62 | 29.21 | 24.20 | 23.13 |
| t3-gb-highfreq-40hz-60s | 500,530 | 55.64 | 41.11 | 32.99 | 32.33 |
| t3-gb-horizon-240s | 555,032 | 64.21 | 45.92 | 37.18 | 36.72 |
| t3-gb-pop-horizon-scale | 725,591 | 82.01 | 66.02 | 52.99 | 50.90 |
| t3-gb-mega-throughput | 1,168,360 | 129.66 | 98.72 | 82.99 | 79.80 |
| t3-gbatch-dense-3 | 24,751 | 5.65 | 3.53 | 2.27 | 1.98 |
| t3-gbatch-homog-8 | 65,163 | 9.35 | 6.86 | 3.33 | 3.10 |

Over all 71 units:

| | v6 | v7 |
|---|---:|---:|
| Own time, gVisor | 972 ms | **785 ms (-19%)** |
| Proxy score, gVisor | 2.025M | **2.202M (+8.7%)** |
| Own time, VM | 591.5 ms | 567.0 ms |
| Proxy score, VM | 9.12M | 9.50M (+4.1%) |

- **By change.** The first three changes plus the counted spins made up +6.2% of the gVisor proxy
  score. Streaming the ledger added +1.4%, mostly on the large units: gb-mega -9.6 ms and
  gb-pop-horizon -10.8 ms at that step.
- **In-process time under gVisor** (9 runs), v6 to v7:
  - gb-mega 123.8 to 98.1 ms;
  - gb-pop-horizon 87.2 to 63.3 ms;
  - mr-cancel-replace-churn 30.2 to 25.3 ms;
  - mp05 21.4 to 17.2 ms;
  - sf-01 9.0 to 7.6 ms;
  - as01 6.0 to 4.5 ms.
- **What is left.** The gVisor tax over the VM is about 220 ms over the roster:
  - the window's remaining handoffs and 4 KiB pages;
  - setup (thread creation, populating);
  - the trace's hash and write at the end;
  - exit teardown on the large units.
- **Real hosts.** The proxy's fixed cost is only runsc's. If the Final's timer also covers a
  longer `docker run`, the same savings are a smaller share of each rate.

## Tried and dropped

- **Fewer threads.** With a budget of 3 CPUs gb-mega took 52 ms more, and with 1 CPU 97 ms more:
  the helper threads pay off even at gVisor's spawn cost.
- **`M_MMAP_THRESHOLD`.** 64 KiB, 128 KiB and 1 MiB, to keep growing buffers out of the heap that
  holds the book: all within noise.
- **`M_ARENA_MAX=1`:** within noise.
- **Skipping the population before the run.** When `MADV_POPULATE_WRITE` fails, populating falls
  back to touching each page; leaving that out as well made runs 0.19 ms slower per unit
  (in-process, 12 units).
- **Huge pages as the explanation for the slower window.** Disabling THP for native runs
  (`PR_SET_THP_DISABLE`) changed nothing, and faults inside the window are rare natively. The
  window's gap comes from the handoffs and from gVisor's page handling, not from TLB reach alone.

## Validation

- All Rust tests pass.
- `tools/compare_unit.py`: **95/95 public scenarios identical to the reference** (every row,
  column and dtype) and 95/95 pass the official gates.
- **Byte identity with v6** on all 71 units, 190 Parquet files, with static linux/arm64 builds
  and no card mounted.
- **Byte-level fuzzing against v6.** 300 random scenarios from `tools/fuzz_vs_abides.py`'s
  generator (seed 20260927): every file identical, every run exits 0.
- **Streaming under stress.** Every run below was byte-identical to v6:
  - 64 repeated runs over 8 units at 4 CPUs;
  - 15 runs under a 2-CPU quota;
  - a debug build (assertions on) under CPU affinities of 1 to 4 CPUs, 56 runs over 7 units,
    covering inline encoding, no replay thread and the full pipeline.

## Left for later

- **Threads created at process start.** Create the helper threads at process start, each spawning
  the next while the scenario is parsed, and populate buffers on them. Estimated at about
  0.6 ms per mid-size unit.
- **`msg_type` chunks at the end of the trace file.** The trace could then be hashed and written
  during the run, about 4 ms on gb-mega. It changes the file layout, so it needs checking with
  pyarrow and the official gates first. Done in release 1.8 (see PERFORMANCE_V9.md), which also
  found the hash running after the write, not beside it.
