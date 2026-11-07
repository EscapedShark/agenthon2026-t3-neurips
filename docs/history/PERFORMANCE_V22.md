Release 1.21 ("v22") changes how the helper threads of a large single scenario wait for work.
The simulation, the image and every output byte are release 1.20's.

## The finding

In the Final layout (scenario alone, ledger written, outputs streamed) the event loop hands a
raw-trace chunk to the replay thread every 4,096 logged calls, a ledger chunk to the encoder every
4,096 messages, and takes a block of latency samples from its producer every 16,384 draws. Each
helper slept between items, so each hand-over to it cost the loop a `futex` wake: `strace -f`
counts 592 `FUTEX_WAKE` calls on the main thread of a gb-mega run (about 600 hand-overs), and the
helpers 1,200 waits. A wake is a system call plus the scheduler's work to run the other thread,
one to a few microseconds each, inside the loop's 49 ms.

Development-layout runs (card mounted) never showed it: gb units write no ledger there, and
releases up to 1.17 wrote after the run.

## The change

Helpers of a single scenario expected to log at least 100,000 trace rows poll instead of sleeping
until woken (`POLL` in `src/output.rs`):

- the replay thread looks at its channel every 10 µs when it is empty, and the channel holds 16
  chunks instead of 4, so a look that comes a little late never has the loop wait for a slot;
- an encoder worker looks at its queue every 20 µs, for up to 256 looks before it sleeps for real;
- the prepared latency producer looks for room in its channel every 20 µs when the channel is
  full (`src/sim/latency_prefetch.rs`);
- helper threads set a timer slack of 1 µs (`threads::precise_timers`), so a 20 µs sleep is a
  20 µs sleep (22.7 µs measured) rather than 70;
- while a chunk is already waiting for the replay thread, which replays and encodes the trace at
  nearly the loop's pace in the largest runs, and the encoder's queue is empty, the replay thread
  leaves that chunk's encoding to the encoder and catches up (`TraceSink::behind`).

The loop's hand-overs then find the helper awake: 31 wakes per gb-mega run instead of 592. Polling
costs the helpers a `nanosleep` each 10-20 µs when idle, a few percent of a core each, within the
container's four CPUs (the loop, the replay thread and one encoder stay below three). Small runs,
batch subs and the lazily started latency producer keep sleeping as before.

Tried and rejected on the way: helpers that spin before sleeping (+45..69% wall on the lab's i7,
where a spinner shares a core with the loop); polling with the 4-chunk channel (the replay thread
fell behind and the loop waited for slots); leaving every chunk's encoding to the encoder while
behind (the single encoder then falls behind and the tail grows 1.7 ms).

## Measurements

x86 (i7-10700K), native, Final layout, release 1.20 versus this source built the same way.

Marks builds, phases of the main thread (6 runs, medians):

| Unit | event loop 1.20 | event loop 1.21 | tail 1.20 | tail 1.21 |
|---|---:|---:|---:|---:|
| gb-mega-throughput | 48.7 ms | 47.2 ms (-3.0%) | 2.41 ms | 2.39 ms |
| gb-pop-horizon-scale | 31.4 ms | 30.4 ms (-3.3%) | 1.68 ms | 1.60 ms |
| gb-horizon-240s | 23.9 ms | 23.1 ms (-3.2%) | 1.25 ms | 1.33 ms |
| mr-deep-book-state-size | 18.7 ms | 18.2 ms (-2.6%) | 0.74 ms | 0.77 ms |

All 71 units, whole-process wall clock, 10 runs per label in each of the two label orders:

| | order A | order B |
|---|---:|---:|
| Sum of the units' median own time | -1.9% | -2.4% |
| gb-pop-horizon-scale / gb-mega / gb-horizon-240s / mr-deep | -5.2 / -4.6 / -4.4 / -2.6% | -6.0 / -4.4 / -3.7 / -4.3% |
| Final proxy (mean of rows / (F + own time)) at F = 125 ms | +0.37% | +0.43% |
| at F = 250 ms | +0.23% | +0.25% |
| at F = 450 ms | +0.14% | +0.15% |

Units below 100,000 expected rows are unchanged (their differences are within noise).

The release binaries themselves, 1.21 against 1.20 on gb-mega: the main thread's `FUTEX_WAKE`
calls fall from 450-540 per run to 9-14, and the process's context switches rise from about 520
to about 1,750 (the helpers' `nanosleep`s). In one interleaved batch of 8 runs the release 1.21
binary was 0.4% faster than 1.20 over eight units while a second PGO build of the same source was
3.7% faster: on this i7 two builds of one source differ by 2-3% through code layout alone (it has
the JCC erratum), which is more than the 1-2% the wakes themselves cost, so the figures above
bracket the gain rather than pin it. The helpers use about 2% more CPU time in all, within the
container's four CPUs.

## Correctness

- `cargo test --release`: all tests pass.
- Native x86 build: 190 parquet files of the Final layout byte-identical to release 1.20's (which
  are release 1.15's). The threads only change when they look for work; what they do is the same.
- Release image `fastsim-t3:1.21` (manifest `sha256:e17abed7…`, binary sha256 `31921ddd…`, 1,858,832
  bytes, static PIE, x86-64-v3): container validation on 71 units in both input layouts (142 runs);
  on x86 with a Tesla T10 (`--gpus all`) 71 units in the Development layout byte-identical to
  release 1.20's outputs; the extracted release binary's outputs byte-identical to release 1.15's
  in both layouts (see `sub/releases/1.21/`).
