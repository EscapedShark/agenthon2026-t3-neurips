Release 1.18 ("v19") is release 1.17 with one more variable in the image: `FASTSIM_STREAM=1`. The
binary is the same file (sha256 `72d87b2b…`) and so is the image's only layer. Every trace and
ledger file is byte-identical to release 1.15's, in both input layouts.

## The setting

A single scenario either writes its outputs while it simulates or writes them after the run.
While it simulates, the trace and ledger chunks go to their files as they are encoded, and the
trace is hashed as it goes. The binary chooses by whether it sees the unit's card (`streams` in
`src/bin/fastsim.rs`):

- **Without a card** (the Final's inputs), it writes while it runs.
- **With a card** (Development mounts the unit's directory), it writes after the run. That was
  right while Development scored the simulation's own reported window, where writing beside the
  simulation could only slow it.

Since 8 October, Development divides each unit's events by the container's whole time, from its
creation to its removal. Writing after the run now only adds to that time.

The image therefore sets `FASTSIM_STREAM=1`, so every single scenario writes while it runs,
card or no card. Batch subs are unchanged: they always write after their sub.

## Measurements

x86 (i7-10700K) with a Tesla T10, set up as the Development board runs a unit:

- the unit's directory, card included, mounted at /input;
- Development's container limits;
- `--gpus all -e NVIDIA_DRIVER_CAPABILITIES=compute`, which is what the official launchers pass.

71 units, ten runs per image after a warm-up, labels interleaved and their order rotated:

| | Release 1.17 | v19 |
|---|---:|---:|
| Mean of rows / median creation-to-removal time (Development's measure) | 392,488 | 402,716 (+2.61%) |
| Median unit's change | | -0.5 ms |

| Unit | Release 1.17 | v19 |
|---|---:|---:|
| gb-mega-throughput | 335.9 ms | 304.2 ms |
| gb-pop-horizon-scale | 300.7 ms | 284.5 ms |
| mr-cancel-replace-churn | 280.4 ms | 267.6 ms |
| gb-horizon-240s | 292.4 ms | 279.8 ms |
| gb-highfreq-40hz-60s | 283.8 ms | 278.8 ms |

Release 1.16 scored 212,671.46 on Development, with units taking 444-565 ms on the board.
Applying each unit's local saving to its time on the board gives 216,083 (+1.60%).

In the Final's layout (no card) the binary already wrote while it ran, so v19 runs the same code
there. Measured the Final's way (scenario inputs only, the runner's flags plus `--gpus all`, 71
units), the mean of rows over the median window moved by noise around no change:

- -0.85%, with four runs per image (the variable added to release 1.17's image);
- +0.18%, with six runs per image in the opposite order (release 1.18 itself).

## Checked and left as they are

- **CPU quota.** Under `--cpus=4`, the container's `cpu.stat` shows no throttled period in any
  run, writing during the run or after it. gb-mega uses 155-160 ms of CPU in all; the quota
  allows 400 ms per 100 ms period.
- **Cancels.** Counted on all 65 single units:
  - A cancel scans 1.0-3.5 orders of its level on average, in queues of 1.5-5.7.
  - Removing the order moves under one entry on average.
  - Where the level empties, 1-15 levels move.
  - On the gb units cancels are 0.1% of the event loop's iterations. On the cancel-heavy mr/mp
    units they are 13-23%, and the order is nearly always the first in its queue.

  An order-id index would save no work.
- **Driver capabilities.** The official launchers pass `-e NVIDIA_DRIVER_CAPABILITIES=compute`
  (`throughput/timer.py`, `gpu_docker_args`, which `run_unit.py` also uses). That overrides the
  image's `utility`, which takes effect only where a launcher sets nothing.

## Correctness

- Container validation (Development limits): 71/71 units in both input layouts.
- With `--gpus all` on the T10: 71 units in both layouts with no failure. The 374 parquet files
  are byte-identical to release 1.17's, and so to release 1.15's.
- In the Development race above, every unit gave one output digest across all 22 runs of both
  images.
