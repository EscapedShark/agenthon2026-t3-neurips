# Releases

Each upload to the Development board, and the Final. Dates are 2026. Two scoring rules apply:

- **Old rule, to 7 October.** Each unit scored its self-reported rate over the simulation window,
  capped at 1e7, so the maximum was 10,000,000.
- **New rule, from 8 October.** Each unit scores verified rows over the container's life, from
  creation to removal. Scores are about 200,000 because that time is about 500 ms per unit.

On 30 September the Final's runtime also changed from gVisor to runc, which ended the work on
gVisor in v7 to v10.

| Upload | Release | Date | Old rule | New rule | Main change |
|---|---|---|---:|---:|---|
| v1 | 1.0.0 | 09-25 | 5,129,649 | | Exact port; 64-byte messages, 4-ary heaps, own Parquet writer |
| v2 | 1.1 | 09-26 | 7,459,501 | | PGO, latency look-ahead (×1.59 on log-normal units), busy-receiver groups |
| v3 | 1.2 | 09-26 | 9,572,389 | | Timing window aligned with the baseline's; two-stage trace |
| v4 | 1.3 | 09-26 | 9,862,229 | | Prefaulting (window faults 440–765 → about 5); 40% faster encoding; batch scheduling |
| v5 | 1.4 | 09-26 | | | Time outside the window: `writev`, parallel hash and write |
| v6 | 1.5 | 09-26 | 10,000,000 | | Batch rework; ring and sorted queue shapes |
| v7 | 1.6 | 09-27 | 9,990,095 | | Fewer system calls for gVisor |
| v8 | 1.7 | 09-27 | 10,000,000 | | Transparent huge pages off |
| v9 | 1.8 | 09-27 | 9,876,135 | | Trace streamed and hashed during the run; `msg_type` last in the file |
| v10 | 1.9 | 09-27 | 10,000,000 | | Batch scheduling: start simulations first, idle threads help |
| v11 | 1.10 | 10-04 | | | Streaming under runc; agent draws look-ahead; pre-started threads (+6.8%) |
| v12 | 1.11 | 10-04 | | | Fill labels settled during replay; cancel tombstones; indexed wait groups (+0.60%) |
| v13 | 1.12 | 10-04 | 9,976,711 | | Ring entries hold key and slot together (+0.36%) |
| v14 | 1.13 | 10-05 | 9,955,983 | | 40-byte ledger rows; small row groups near the close; wakeup classes (+1.02%) |
| v15 | 1.14 | 10-05 | 9,963,821 | | Small first row group; no Rust runtime start; branch-free selection (+0.8%) |
| v16 | 1.15 | 10-07 | 9,998,261 | 189,372 | Event loop: fewer branches and indirect loads (+0.12%) |
| v17 | 1.16 | 10-08 | | 212,671 | `FROM scratch` image (+25.5%) |
| v18 | 1.17 | 10-08 | | 221,088 | `utility` driver capability (+1.4% on the lab; no effect under CDI injection) |
| v19 | 1.18 | 10-08 | | 208,723 | Streaming on Development too |
| v20 | 1.19 | | | | Reports sent during matching; 20-byte ring entries (+0.27%); not uploaded |
| v21 | 1.20 | 10-09 | | 210,622 | x86-64-v3 and new PGO; one message type tag (+0.83%; program time −7.2%) |
| v22 | 1.21 | 10-09 | | | Helpers poll in large runs; `FUTEX_WAKE` 592 → about 30 (+0.4%) |
| v23 | 1.22 | 10-09 | | 211,479 | Image layout: no `/usr`, config-only `WorkingDir` (+6.2%) |
| v24 | 1.23 | 10-11 | | | Grouped agent draws, ext-TSP, PGO trained with streaming (+0.42%) |
| v25 | 1.24 | 10-11 | | | Ring retunes its width (+0.155%; −7.9% on denser variants) |
| v26 | 1.25 | 10-11 | | | Early latency helper, fixed address, native PGO (+0.154%) |
| v27 | 1.26 | 10-11 | | | Megashock times compared as NumPy does (−0.03%) |

**Final:** release 1.26, score 3,249,452.2547, 3rd of 55 teams. The official board is at
<https://www.agenthon.net/#leaderboard>.

Percentages are gains on our x86 lab (i7-10700K, Tesla T10). v17, v18, v21 and v23 come from
Final-style GPU container runs. The others come from a process-time score proxy at about 100 ms of
fixed overhead. Lab overhead differs from the Final machine's, so these show relative size only.

Releases 1.15 to 1.26 write byte-identical outputs on the public units.

v17, v18 and v19 share one binary, so their spread on the board is mostly noise. Later Development
scores are not a guide to speed, and for that reason v24 to v27 were judged by local measurement.

The notes in [history/](history/) record the measurements for each release. There are none for v11
to v13.
