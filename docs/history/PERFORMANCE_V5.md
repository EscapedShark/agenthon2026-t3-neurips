Release 1.4 ("v5") shortens everything outside the simulation window: preparation, finishing,
encoding, hashing, writing and exit. The Final ranks the host's clock around the whole run without
a cap, so that time counts there in full; the Development window changes little. Decoded outputs
are identical to v4's, batch outputs byte for byte.

## What the v4 result showed

The v4 primary was **9,862,229** (v3: 9,572,389). With the per-unit cap of 10,000,000
(see [PERFORMANCE_V4.md](PERFORMANCE_V4.md)), the remaining deficit came from four units:

| Unit | v3 | v4 | Below the cap by |
|---|---:|---:|---:|
| gbatch-dense-3 | 8.89M | 5.35M | 4.65M |
| s001-price-time-priority | 7.38M | 7.34M | 2.66M |
| gbatch-varsize | 9.36M | 7.96M | 2.04M |
| eq-deterministic-baseline | 6.33M | 9.57M | 0.43M |

- dense-3 and varsize ran slower than in v3 although the Linux VM measures v4 faster, and in the
  same submission homog-4 (the same sub configuration as dense-3) improved from 11.2M to 12.5M.
  A single measurement per unit on a shared queue is the likely cause.
- s001's window did not change on the platform (81.9 us, then 82.3 us) while it fell 15-25%
  locally and in the VM: with 604 events, about 30 us of it is a fixed cold-start cost there.
- eq-pareto-heavytail and eq-lognormal-highsigma, 42% slower than their siblings in v3, ran at
  their siblings' speed in v4, confirming that v3 measurement was disturbed.

## Where a run's time goes outside the window

In the Linux VM (binaries and inputs copied into the container, since Docker Desktop's shared
folders slow down exec and reads), a 72k-event unit took about 6.5 ms from exec to exit, of which
3.8 ms was the simulation window:

| Phase | sf-01 |
|---|---:|
| exec and static initialization | ~0.15 ms |
| reading the scenario, JSON and card | ~0.1 ms |
| preparation (populating ~700 pages, creating threads) | 0.6-0.8 ms |
| simulation window | ~3.8 ms |
| finishing: replay thread, `msg_type` encoding, last chunks | ~0.8 ms |
| hashing and writing | ~0.5 ms |
| freeing memory and exit | ~0.3 ms |

For the 72k-120k-event units that make up most of the roster, time outside the window was
36-49% of the whole run; for the largest units 10-16%.

## Changes

- **Zero-copy Parquet output.** `pq::ParquetFile` keeps a file as its pieces (magic, column
  chunks, footer). Offsets for the footer come from the piece lengths, and the file is written
  with `writev` without first concatenating a copy of it into one buffer.
- **Hashing.** A trace of at least 256 KiB is hashed on a second thread while the main thread
  writes both files; a smaller one is hashed piece by piece as it is written.
- **`msg_type` in parallel.** Once execution labels are final, each row group's `msg_type`
  column is queued as a job, so the encoder threads and the finishing thread encode them together
  (gb-mega: 72 row groups).
- **Exit.** After writing its outputs and printing its record, the process exits without freeing
  the encoded files piece by piece; the kernel reclaims the memory at once.
- **Row groups by mode.** `output::Mode::Single` streams 8,192-row chunks, so the last chunks
  encoded after the run are half as large; `Mode::BatchSub` keeps 16,384 rows, since a batch clock
  covers the encoding anyway and larger groups cost less per row. A mode's bytes do not depend on
  the thread count, so repeats stay identical.
- **One spare populated chunk** instead of two per log: finished chunks come back for reuse before
  the spare is full, and the window's page faults stay at about five.
- **Smaller changes inside the window:** a requeued event sifts down from the root and stops as
  soon as it fits (instead of descending to a leaf and sifting up), and `Sim::run` hands back its
  logs through placeholders that allocate nothing (it used to reserve 2.6 MB there).

## Measurements

v4 (release 1.3 source) and v5 built natively for linux/arm64, run alternately in one container
(`--cpus 4 --memory 6g`), seven runs each per unit. "Process" is exec to exit measured around the
command, including about 0.55 ms of shell fork and exec that is the same for both.

| Public unit | v4 window (ms) | v5 window (ms) | v4 process (ms) | v5 process (ms) | Process speedup |
|---|---:|---:|---:|---:|---:|
| t3-s001-price-time-priority | 0.050 | 0.046 | 1.81 | 1.68 | 1.08x |
| t3-eq-deterministic-baseline | 1.050 | 1.041 | 3.96 | 3.48 | 1.14x |
| t3-eq-lognormal-lowsigma | 1.047 | 0.991 | 3.96 | 3.44 | 1.15x |
| t3-as01-base-mix | 1.778 | 1.695 | 5.79 | 4.37 | 1.32x |
| t3-sf-01-baseline-regime | 3.862 | 3.857 | 8.18 | 6.79 | 1.20x |
| t3-mp03-mm-heavy-newest | 6.551 | 6.455 | 11.78 | 9.72 | 1.21x |
| t3-mr-deep-book-state-size | 25.559 | 25.622 | 32.87 | 29.88 | 1.10x |
| t3-gb-mega-throughput | 84.113 | 82.495 | 94.39 | 90.83 | 1.04x |
| t3-gbatch-dense-3 | 3.312 | 3.134 | 4.63 | 4.53 | 1.02x |
| t3-gbatch-hetero-mix | 4.438 | 4.251 | 5.59 | 5.46 | 1.02x |
| t3-gbatch-homog-8 | 6.624 | 6.380 | 8.41 | 8.08 | 1.04x |

Over all 71 units:

| Mean over units | v4 | v5 |
|---|---:|---:|
| Window, capped at 1e7 (the Development score) | 9.920M | 9.916M |
| Window, uncapped | 15.24M | 15.38M |
| Process, uncapped (the Final's kind of number) | 7.500M | **8.649M (+15.3%)** |

The median unit's whole run is 17.6% faster. Two units looked slower in this seven-run pass
(s019 and mr-long-horizon-replay); 21 alternating runs of the row-group change alone measure them
7.4% and 1.4% faster end to end, so that was noise. Smaller row groups add one or two chunk
handoffs on the simulation thread, which costs 0-2% of a single scenario's window.

## Tried and dropped

- Recording batch subs straight into the trace builder instead of logging and replaying: no
  consistent difference in 25 alternating native runs per batch.
- Moving trace encoding from the replay thread back to the encoders: neutral for single
  scenarios.
- 8,192-row groups in batches as well: batches 3-6% slower.

## Validation

- All 20 Rust tests pass; the pipeline determinism test now compares 0-3 worker threads in
  `Mode::Single` with 8,192-row groups, byte for byte.
- `tools/compare_unit.py`: **95/95 public scenarios identical to the reference** (every row,
  column and dtype) and 95/95 pass the official gates.
- Against v4 on all 71 units: the six batch units are byte-identical; 64 single scenarios differ in
  bytes only because of the smaller row groups (s001 has one row group either way).
- **Decoded-equality differential fuzzing against v4** (seed 5150, 40% of cases scaled 3-30x, plus
  random batches): 242 cases, 158M events, every table equal including schema and pandas
  metadata. Before the row-group change, byte-level fuzzing against v4 matched 220/220 cases
  (seed 31337, 262M events).
- Differential fuzzing against the real ABIDES baseline (seed 20260927): 152 of 160 random
  scenarios identical; the other 8 fail inside ABIDES (`KeyError: 'order_id'`, no orders), so they
  cannot be sealed units.
- `trace_sha256` equals the SHA-256 of the written trace for all 95 public traces, 44 of them
  hashed on the second thread.
- **Release 1.4 (linux/amd64 PGO image, manifest `sha256:e0fb3e10...`):** 71/71 units x 2 input
  layouts pass `sub/tools/validate.py` in containers with the card's limits, read-only root and no
  network. Published as `docker.io/greedyshark/fastsim-t3:1.4` (anonymously pullable) and packed
  as `sub/submissions/v5.zip`, which `check_zip.py` accepts.
