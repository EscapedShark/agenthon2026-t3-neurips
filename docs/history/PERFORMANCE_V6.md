Release 1.5 ("v6") reworks how a batch runs, adds two message-queue forms to the event loop, and
reads the card without the TOML parser. Every output file is byte-identical to v5's.

## What the measurements showed

**A batch spent up to a millisecond creating directories.** A timeline of `t3-gbatch-dense-3` in
the Linux VM (Docker Desktop, `--cpus 4`) showed each sub finishing its simulation and encoding
in about 1.6 ms and then waiting 0.4-1.4 ms to write its files; with the output on a tmpfs the
same step took 60-80 us. The Development runtime binds a host directory at `/output`
(`-v "$PWD/out":/output`, DEVELOPMENT-RUNTIME.md), so writes go to a real file system. On the
VM's ext4 volume, `mkdir` of a sub's directory took about 210 us on its own, and concurrent
`mkdir`s in one parent directory run one after another:

| Threads creating a directory, file and 60 KB write | Slowest thread, ext4 | Slowest thread, tmpfs |
|---:|---:|---:|
| 1 | 0.21 ms | 0.004 ms |
| 3 | 1.33-1.54 ms | 0.005 ms |
| 8 | 4.5-4.9 ms | 0.005 ms |

Creating and writing the files themselves took 5-30 us each. On a host where a new directory's
metadata needs a disk read, the wait is longer still. The platform's batch clocks varied between
submissions far more than their simulations could explain (dense-3: 2.79 ms in v3, 4.63 ms in
v4; hetero-mix: 7.72 ms, then 4.87 ms), which fits a slow, contended metadata step at their end.

**The batch clock started with 150-260 us of setup.** Before the first sub started: reading and
parsing the three sub files (45 us), the card through the `toml` crate (40-55 us),
`available_parallelism` (13-20 us, it reads the cgroup files) and creating the threads one by
one (65-100 us).

**Burst traffic made the message heap the event loop's largest cost.** In `t3-gb-mega-throughput`
(144 agents) about 115 messages were pending on average when one was taken, although each is
delivered within 8 us: agents act at the same instants and their orders and the exchange's
replies are in flight together. Taking the next message from the 4-ary heap cost about 15 ns, and
the message lane took roughly 40% of the loop (macOS `sample`). In the typical 72k-event unit
13-20 messages are pending.

**Page-fault costs in the VM.** First-touch 4 KiB faults cost 0.8-1.3 us per page and
`MADV_POPULATE_WRITE` 0.6-0.8 us, so populating memory ahead only pays for pages that are then
used (populating whole spare chunks in batch subs was slower in v4 for that reason). The VM
kernel backs large anonymous regions with transparent huge pages (0.05 us per 4 KiB equivalent);
the platform's setting is unknown, so nothing here depends on it.

## Changes

- **Batch outputs prepared ahead.** A helper thread creates every sub's directory and its three
  output files (and `batch_events.json`) while the subs load and simulate; a sub takes its open
  files when it is ready to write, or creates them itself if it gets there first. A sub's write
  is now its `writev` calls.
- **Batch start.** The card is read and the worker threads are created before the directory is
  listed, idle until the sub list is published; the clock still starts after the listing, as the
  baseline's does, and nothing is read from the subs before it. Every thread (the main one
  included) loads and parses subs in parallel, then all derive the same size-ordered plan. The
  first sub now starts 34-72 us after the clock starts instead of 150-260 us.
- **Card and CPU count.** The card's three values are read by a line scan that accepts only plain
  one-line keys and tables and hands anything else (multi-line or inline values, dotted or quoted
  keys, repeated tables, non-decimal numbers) to the `toml` parser. It agrees with the parser on
  all 71 public cards and a set of edge cases. The CPU count is the affinity mask
  (`sched_getaffinity`), capped by the card as before.
- **Memory reuse.** `mallopt` keeps allocations up to 32 MiB on the heap and freed memory mapped,
  so memory one phase frees (the trace log after replay, or a previous sub on the same batch
  thread) is reused without faulting again. Batch subs free their encoded outputs for that reason;
  single scenarios still exit without freeing.
- **Message queue forms.** The message lane stays a 4-ary heap unless the run shows another form
  pays:
  - with a constant latency, messages are kept in a sorted array (they arrive almost in send
    order: appending and taking the front are O(1));
  - when at least 16 messages are pending on average over the first 4,096 taken, the heap turns
    into a ring of 512 time buckets (a 32nd of the median time ahead, as a power of two) plus an
    overflow heap for messages beyond the ring or in a full bucket. The next message is the
    smaller of the first occupied bucket's smallest key and the overflow heap's head, so the
    order is exactly the heap's.
- **Heap padding.** The heaps keep `u128::MAX` after their last entry, so choosing the smallest of
  four children is branch-free also on the partly filled bottom level.

## Measurements

v5 (release 1.4 source) and v6 built natively for linux/arm64 and run alternately in one
container (`--cpus 4 --memory 6g`), outputs written to an ext4 Docker volume (the closest the VM
gets to a host bind mount), seven runs each per unit. "Process" is exec to exit measured around
the command, including about 0.55 ms of shell fork and exec that is the same for both.

| Public unit | v5 window (ms) | v6 window (ms) | v5 process (ms) | v6 process (ms) | Process speedup |
|---|---:|---:|---:|---:|---:|
| t3-s001-price-time-priority | 0.046 | 0.042 | 1.41 | 1.40 | 1.01x |
| t3-eq-deterministic-baseline | 1.008 | 0.861 | 3.36 | 3.09 | 1.09x |
| t3-eq-lognormal-lowsigma | 0.980 | 0.914 | 3.34 | 3.14 | 1.06x |
| t3-as01-base-mix | 1.672 | 1.581 | 4.14 | 3.98 | 1.04x |
| t3-mp03-mm-heavy-newest | 6.357 | 5.424 | 9.27 | 8.28 | 1.12x |
| t3-mr-deep-book-state-size | 24.638 | 22.623 | 28.15 | 26.02 | 1.08x |
| t3-gb-mega-throughput | 80.871 | 74.225 | 88.77 | 81.83 | 1.08x |
| t3-gbatch-dense-3 | 2.858 | 1.448 | 4.01 | 2.88 | 1.39x |
| t3-gbatch-varsize | 2.791 | 2.050 | 4.00 | 3.62 | 1.11x |
| t3-gbatch-homog-4 | 3.546 | 1.461 | 4.68 | 2.96 | 1.58x |
| t3-gbatch-hetero-mix | 4.069 | 2.981 | 5.14 | 4.13 | 1.24x |
| t3-gbatch-homog-8 | 5.600 | 2.628 | 7.05 | 4.24 | 1.66x |
| t3-gbatch-many-6 | 5.136 | 2.610 | 6.44 | 4.25 | 1.52x |

Over all 71 units:

| Mean over units | v5 | v6 |
|---|---:|---:|
| Window, capped at 1e7 (the Development score) | 9.963M | **10.000M** |
| Window, uncapped | 15.86M | 17.63M |
| Process, uncapped (the Final's kind of number) | 9.218M | **9.902M (+7.4%)** |

Every unit's window is above 1e7 in the VM. The platform adds about 30 us of cold start to
`t3-s001-price-time-priority` (82 us there against 47 us in the VM in v4), which keeps it below
the cap; the other single scenarios run within -6% to +13% of their VM times.

Separately, 21 alternating runs of the heap padding alone: `sf-01` window 1.064x,
`sf-04-thin-book-depth` 1.066x, `mp01-stp-newest-baseline` 1.030x. Native simulation-only medians
(`bench`, M2 Max) with and without the ring: `gb-mega` 88.7 -> 76.3 ms, `gb-highfreq-40hz-60s`
35.9 -> 30.8 ms, `gb-horizon-240s` 37.9 -> 34.0 ms, `mr-deep-book-state-size` 24.8 -> 22.4 ms,
`mp03-mm-heavy-newest` 6.4 -> 5.5 ms, `ca-baseline-calm` 2.3 -> 2.1 ms; units that stay below the
threshold are unchanged.

## Tried and dropped

- A lower ring threshold (8 or 12 pending messages): 6-11% faster on some units (`mp01`,
  `as06`, `fastlob-core`) and 4-7% slower on units with many busy requeues (`st01`, `s012`,
  `mr-cancel-replace-churn`).
- Taking a heap's root lazily so that the next push into the same lane fills it (one sift from the
  top instead of a pop and a push): no measurable difference.
- Populating batch subs' memory: populating costs 60-75% of faulting per page in the VM, so it
  only pays for pages that are used, and a sub's size is known only after it is read.

## Validation

- All 21 Rust tests pass, including queue differential tests against a single `BinaryHeap` of
  the original tuples for every lane form (heap, sorted, ring with 1 ns to 16 ms buckets, and a
  heap turning into a ring mid-run), with pushes into the past, far ahead, busy requeues and ids
  up to 2^32; and the card scan against the `toml` parser.
- `tools/compare_unit.py`: **95/95 public scenarios identical to the reference** (every row,
  column and dtype) and 95/95 pass the official gates.
- **Byte identity with v5** on all 71 units (three runs per binary, 552 Parquet files).
- **Byte-level differential fuzzing against v5**: seed 7070, 440 cases (random batches included,
  153M events), and seed 8181 with agent counts scaled 8-32x, 165 cases; every file identical. An
  instrumented build switched to the ring in 145 of those runs, with bucket widths of 1-128 ns.
- Batch error paths: a missing or empty batch directory and an unparsable sub exit with status 1
  and the error.
