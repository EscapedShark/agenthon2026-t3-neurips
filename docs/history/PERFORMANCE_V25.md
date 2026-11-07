Release 1.24 ("v25") is release 1.23 with one change: the message ring re-chooses its bucket width
from the buckets it pops, never so narrow that it stops spanning most pending messages. Every trace
and ledger file of the public units is byte-identical to release 1.23's. The released binary is the
fastest of three fresh PGO builds of the release's source: the first build was slower than release
1.23's binary.

## The ring's width

A message lane that holds 16 or more messages on average over its first 4,096 pops becomes a ring:
512 buckets of up to 8 entries with an overflow heap behind them. Its width is a 32nd of the median
distance ahead of the messages pending at that moment, rounded up to a power of two. That moment is
a rough guide. Besides the message popped, the popped buckets hold on average 0.86 more messages in
gb-mega (8 ns buckets), 1.53 in gb-pop-128-agents and 1.18 in gb-pop-horizon-scale (16 ns), 0.36
in gb-horizon-240s (16 ns) and 0.16 in gb-highfreq-40hz-60s (4 ns).

### Retuning

Once the ring exists, it measures the mean length of the buckets it pops over 16,384 pops. It then
moves every message (its own and the overflow heap's) to a ring of a new width. The target is 0.45
messages beyond the one popped. The excess is taken to scale with the width, so the shift changes by
log2(0.45 / excess), rounded (an excess under 0.01 counts as 0.01). The ring measures and retunes
once more over the next 65,536 pops. A ring pops in key order whatever its width, so the outputs
cannot change. The cost is a counter while a window runs, a flag test per ring pop, and two moves
of the pending messages (tens to hundreds on the public units).

### The span guard

The new width never gets so narrow that the ring stops spanning nine in ten of the pending messages
(the 90th percentile of their distance ahead at the retune; `span_shift`). A message beyond the
span goes to the overflow heap, where each push and pop pays a heap's levels. Without the guard, a
market denser than any public unit loses most of the gain or gets slower (below). On the public
units the guard never binds. The narrowest rings, gb-mega's and pop-128's at 4 ns, span 2 µs
against 90th percentiles of 0.7 to 0.8 µs.

## Choosing the target

The candidates were lab-native PGO builds, two per variant. Measurement: all 71 units in the card
layout, whole-process wall time without `perf`, 12 runs per unit and label, labels interleaved, two
batches with the label order reversed. The base is two builds of release 1.23's source; the proxy
is mean(rows / (F + own time)).

| Target excess | Widths: gb-mega, pop-horizon, horizon-240, highfreq | Proxy at F = 102 ms, 2 builds × 2 batches |
|---|---|---:|
| none (release 1.23) | 8, 16, 16, 4 ns | ±0.015% |
| 0.30 | 2, 4, 16, 8 ns | -0.39% to -0.42% |
| **0.45** | **4, 8, 16, 8 ns** | **+0.06% to +0.15% (mean +0.11%)** |
| 0.60 | 4, 8, 32, 16 ns | +0.08% to +0.12% (mean +0.09%) |
| 0.80 | 8, 8, 32, 16 ns | +0.01% to +0.07% (mean +0.04%) |

At 0.45:

- pop-horizon is 2.5% to 3.7% faster, gb-pop-128-agents 3.4% to 4.2%, gb-mega 0% to 1.2%.
- highfreq changes by -1.0% to +0.1%.
- horizon-240 keeps its width.

0.30 makes gb-mega's buckets too narrow (+8%). 0.60 and 0.80 widen horizon-240 (+0.6% to +1.6%).

## The public units

The counters build of 1.23 and of the release source shows:

- 23 of the 71 units switch to the ring, and all 23 retune.
- 15 end at another width than release 1.23's.
- Overflow pushes after the retune stay small: at most 2.8% (ca-fast-mean-reversion, 5.1% before).
  gb-mega goes to 0.26% (from 0.05%), pop-128 to 0.30% (from 0.68%), pop-horizon to 0.01% (from
  0.21%), highfreq to 0.01% (from 0.25%).
- The release source and the variant measured above (no guard) make the same decisions, with the
  same counters, on all 23.

## Denser markets

The sealed units may push the throughput axes further than the public ones. Twelve denser variants
of the gb units were run (`benchmarks/v25-ring-retune-2026-10-11/dense/units/`):

- gb-mega with 2 to 8 times its noise traders, some with wider log-normal latencies (1-5 µs mean,
  σ 1-1.5) or faster noise traders.
- pop-128 and pop-horizon with 2 and 4 times the agents.
- highfreq with twice and horizon-240 with three times the agents.

Release 1.23's first width is too wide on ten of them: popped buckets hold 1.5 to 3.1 messages
beyond the one popped, and the retune narrows it. (highfreq × 2 and horizon-240 × 3 sit near the
target and keep their widths.) On six of the variants the guard binds. The table gives the geomean of
wall time against release 1.23's binary: 12 runs per unit, two batches for the lab builds, the
release in its confirmation batch.

| Build | The 6 where the guard binds | The other 6 |
|---|---:|---:|
| 1.24 without the guard (2 lab builds) | -4.3% to -4.7% | -2.1% to -2.3% |
| 1.24 (2 lab builds, the first release build) | -10.9% to -13.0% | -2.3% to +0.4% |
| **released 1.24** | **-13.2%** | **-2.4%** |

The guard matters most on two of them:

- pop-128 × 4: -17% to -20% with the guard, +0.6% to +1.9% without.
- gb-mega with 400 noise traders: -2.7% to -5.2% with the guard, +6.4% to +7.3% without.

Without the guard, the narrow ring sends most of their messages to the overflow heap.

## Builds vary

PGO trains on runs whose threads' timing varies, so two builds of one source give different
binaries. Each build below is a fresh release-pipeline draw (`sub/tools/build.py`'s `docker build`
plus `--no-cache`). Columns: wall time and proxy against release 1.23's binary, two batches.

| Build | Own time, 71 units | Proxy at F = 102 ms |
|---|---:|---:|
| 1.23, second draw | +1.46% / +1.35% | -0.27% / -0.27% |
| 1.23, third draw | -0.05% / -0.34% | +0.01% / +0.04% |
| 1.24, first build | +0.97% / +0.81% | -0.14% / -0.12% |
| 1.24, draw b | -0.81% / -0.76% | +0.19% / +0.14% |
| **1.24, draw c (released)** | **-0.87% / -0.87%** | **+0.19% / +0.16%** |
| 1.24, draw d | -0.25% / -0.38% | +0.06% / +0.05% |

- Release 1.23's binary is a typical draw of its source.
- 1.24's first build is an unlucky one. It also executes more instructions: +0.68% against release
  1.23 (geomean), where draws b-d run +0.23% to +0.27% (the retune's counting). Part of its
  slowness is in what it executes, not only in its layout.
- The draws were chosen on the lab's i7. The Final's Xeon may rank layouts differently, but not
  instruction counts.
- `build.py --rebuild --adopt TAG --adopt-context SHA --adopt-log LOG` records such a candidate as
  the release instead of building again. The freshly staged context must have the digest the
  candidate was built from, and the candidate's build log becomes the release's.

## Correctness

- `cargo test --release`: 43 tests pass. Six of them are new in `queue.rs`:
  - `retuned_width_aims_at_the_target_bucket_length`
  - `span_shift_is_the_narrowest_width_spanning_a_message`
  - `retuning_rings_change_width_and_keep_the_single_queue_order`
  - `ring_rebuilt_at_any_width_keeps_key_order`: rebuilds at 1 ns to 2^40 ns at random moments,
    against a reference heap.
  - `ring_from_a_heap_retunes_its_width_twice`
  - `retuned_ring_keeps_spanning_most_pending_messages`

  Every differential test also runs two retuning forms: windows of 3 pops ×4, nine windows,
  starting at 1 ns and at 2^24 ns.
- Mutation checks:
  - Dropping the overflow heap's messages in a rebuild fails 9 tests.
  - Moving messages with a wrong slot fails 12.
  - Removing the guard fails the guard's test.
- `tools/compare_unit.py`: 95/95 scenarios identical to the reference, and 95/95 pass the official
  gates.
- Differential fuzzing against the pinned ABIDES: 60 ordinary scenarios (`--min-draws 12000`) and 60
  with mixed intervals.
  - 113 identical, 7 where ABIDES itself raises `KeyError: 'order_id'`, 0 mismatches.
  - 73 ran the ring. 54 retuned (all compared), and 35 of those changed its width.
- The six denser variants where the guard binds are identical to ABIDES (trace and ledger, 215,296
  to 1,184,340 rows each).
- On every variant, the stable outputs are byte-identical across release 1.23, 1.24, and the builds
  without the guard.

## Release

- **Image:** `fastsim-t3:1.24`, manifest `sha256:20772248fb6e111fdefb317bc780fde07ec8fe16b42e0bfb9ce9aa61f9d4a2d0`,
  binary sha256 `b3dfb3fc…` (1,869,808 bytes). It is draw c of context `1580ad48…`, adopted with
  `build.py --adopt` (`sub/releases/1.24/adopted.json`). The first build's records were replaced;
  its measurements are under `release-draws/first_*`.
- **Validation:** `validate.py` passed 71/71 units in both input layouts (142 runs) with 0 failures.
  All 374 parquet files are byte-identical to release 1.23's validation record.
- **The image on the lab's GPU:** CDI and legacy injection, card and staged layouts. Every unit's
  stable outputs are byte-identical to release 1.23's. Final-style windows against 1.23: +0.34%
  (CDI, card, warm-up + 4), -1.30%, -1.01% and -0.69% (warm-up + 1). That is the windows' noise.
- **The release binaries:** card layout, whole-process wall time, 12 runs per unit.
  - Three batches: own time -0.61%, -0.65%, -0.60%; proxy +0.135%, +0.170%, +0.159% at F = 102 ms
    and +0.10% to +0.13% at F = 150 ms.
  - The four big gb units are 1.4% to 1.9% faster: pop-horizon -3.1% to -4.2%, pop-128 -3.9% to
    -4.8%.
  - A fourth batch (+0.16%, -0.03%) had gb-highfreq 12% slower, and is left out. In 100 alternating
    runs, every binary (release 1.23 included) stalled about 6 ms in 4% to 12% of highfreq runs
    (the lab's POLL windows, below). 1.24's median was 1.1% to 1.3% faster.
  - `perf`: instructions +0.25% and cycles 0.00% (geomean over 71 units).
  - Denser variants: -7.9% geomean (-13.2% where the guard binds).
- **Published:** `docker.io/greedyshark/fastsim-t3@sha256:20772248fb6e111fdefb317bc780fde07ec8fe16b42e0bfb9ce9aa61f9d4a2d0`
  (anonymous pull 200).
- **ZIPs:** `sub/submissions/submission-1.24-20772248-dev.zip` (= `v25.zip`, sha256 `42792d81…`) and
  `submission-1.24-20772248-final.zip` (sha256 `2c4ef25b…`). `check_zip.py` passed both.

## The lab's stalls

Every binary, release 1.23 included, sometimes runs 6-10 ms slower on the lab. On gb-highfreq this
hit 4% to 12% of runs, on pop-horizon 1% to 19%. The cause is the lab host, not fastsim
(`benchmarks/v25-ring-retune-2026-10-11/lab-stall/`):

- **The slow runs come in windows.** Each window lasts 0.3 to 0.7 s and recurs every 5 to 20 s.
  The windows hit every binary at the same moments and also occur on an idle machine.
- **Only the event loop slows.** Inside a window it is 44% longer (34.2 to 49.4 ms in a marks
  build) at the same 4.70 GHz.
- **Nothing else changes.** The main thread is never off its CPU and never migrates, and its L2
  and L3 misses are unchanged. Its IPC falls from 2.79 to 1.93.
- **Inside a window, every idle CPU polls.** Each spends 88% to 99% of its idle time in the
  kernel's POLL state (a PAUSE loop) instead of C3. That includes the main thread's SMT sibling,
  which then takes a share of the core.
- **PAUSE spinners reproduce it exactly.** fastsim pinned to CPUs 0-7, with PAUSE spinners on the
  sibling hyperthreads 8-15, runs its loop in 49.1 ms at IPC 1.94 and 4.70 GHz, against 34.1 ms at
  IPC 2.79 without them.
- **The windows line up with i915 work.** The lab's Intel iGPU has four display ports, all
  disconnected, under a GDM login screen. Its driver's work items (`i915-unordered`, `events`)
  run during the windows and almost never outside them.
- **The likely mechanism** is display probing that holds a CPU latency QoS request of 0, which
  allows only the POLL state. Without root this could not be traced further.

A server without an Intel iGPU should not do this, but any CPU latency request of 0 on an SMT host
would cost the event loop the same. Labels interleaved per unit see the windows equally, so the
A/B comparisons above stay unbiased; a batch can still be hit (the left-out fourth batch).

**Fixed on the lab (2026-10-11).** GDM was stopped and connector polling turned off
(`drm_kms_helper.poll=0`), both at run time, so a reboot brings them back.

- 120 s idle and 69 s under load show no POLL window. Idle time is 99.5% in C3.
- Slow pop-horizon runs fell from 19/100 to 4/200, and gb-highfreq has 0/200.

**Two smaller sources remain:**

- **Firmware SMIs every 10.0 s.** `perf stat -e msr/smi/` per run (`smiruns.py`) shows each burst
  freezing every CPU for about 5-7 ms, invisible to the kernel. The main thread's user cycles stay
  unchanged while its time on the CPU grows by the freeze. All 6 runs in 1,500 that met an SMI
  burst were slow (+11 to +21 ms of process CPU). This is the board's BIOS: it hits about 0.3%
  of 30 ms runs.
- **Minor slowdowns.** About 1.3% of runs are 2-3 ms slower, with 6-8% more cycles for the same
  instructions: small contention, not traced further.

## Open

- To keep the lab quiet after a reboot: `systemctl set-default multi-user.target` and
  `drm_kms_helper.poll=0` on the kernel command line (root).
- PGO draws differ by about 1% in own time. Training whose counts do not depend on thread timing
  could remove the need to draw.

Release records: `sub/releases/1.24/`. Measurements: `benchmarks/v25-ring-retune-2026-10-11/`.
