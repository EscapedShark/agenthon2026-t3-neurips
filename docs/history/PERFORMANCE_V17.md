Release 1.16 ("v17") changes the image, not the simulator: the static binary now runs from an
empty root (`FROM scratch`) instead of Debian 13 slim. Every trace and ledger file is
byte-identical to release 1.15's, in both input layouts.

Since 8 October 2026 the Development board, like the Final, divides each unit's events by the
container's measured time. That time is mostly not the simulation. On the board, v16's units took
478-632 ms (median 561) from creating the container to removing it, of which its own process was
1-61 ms: even a simulation taking no time at all would have scored only 3.8% more. The rest is the
container runtime's own work, and part of it depends on the image.

## The image

Every Track 3 unit runs with `--gpus all`. On each start the NVIDIA hook mounts the driver's
libraries and tools into the container's root and runs `ldconfig` there. The more the root holds,
the longer that takes.

- **Layout.** `/usr/local/bin/simulate` (the static binary), `simulate-batch` linking to it, an
  empty `/etc` and a `/tmp` with mode 1777. `PATH` and `HOME=/tmp` are set in the image, the user is
  65534, and the interface label is unchanged. No shell, no libraries, no `VOLUME`.
- **Why `/etc`.** The hook writes `/etc/ld.so.cache`. Toolkit 1.20.1 creates `/etc` when it is
  missing; the empty directory keeps that working with toolkits that do not. It costs nothing
  measurable.
- **Nothing else runs in the image.** The organizers' runner runs a participant image only for the
  verb (its `run_unit.py` says it must never run it again, for instance to change the output's
  owner), so the image needs no shell.

## Measurements

x86 (i7-10700K) with a Tesla T10, driver 535.309.01 and NVIDIA Container Toolkit 1.20.1.

The smallest unit (s001), 30 runs per image, labels interleaved, with Development's container
limits and `--gpus all`:

| Image | Creation to removal | StartedAt to FinishedAt |
|---|---:|---:|
| Release 1.15 (Debian 13 slim) | 304.7 ms | 162.1 ms |
| Empty root, binary only | 259.0 ms | 123.9 ms |
| Empty root with an empty `/etc` (release 1.16) | 262.8 ms | 123.7 ms |
| Empty root with a Debian-like skeleton (`/etc` files, lib dirs, merged-`/usr` links) | 290.7-307.0 ms | 151.2-156.1 ms |

- Without `--gpus all` the images differ by about 1 ms in the window, 4 ms from creation to
  removal.
- Of the skeleton, the library directories and merged-`/usr` links cost 19 ms of window; each of
  `debian_version`, `ld.so.conf` and `passwd`/`group` about 2 ms. Asking only for the `utility`
  driver capability (5 libraries mounted instead of 20) gained nothing.

All 71 units in the Final layout, run as the organizers' runner does (`--network=none --cpus=4
--memory=16g`, host user) plus `--gpus all`, five runs per image after a warm-up, in both label
orders:

| | Release 1.15 | v17 (order A) | v17 (order B) |
|---|---:|---:|---:|
| Mean of rows / median window (the Final's measure) | 611,035 / 607,251 | 764,863 (+25.2%) | 764,010 (+25.8%) |
| The same over the whole `docker run` | 391,057 / 389,579 | 455,774 (+16.6%) | 455,452 (+16.9%) |
| Median unit's window | 164.2 / 165.1 ms | 128.0 ms | 128.0 ms |
| Units faster | | 71 / 71 | 71 / 71 |

Each unit's window shrank by 14-72 ms (median 36 ms). How much of it the B200 hosts of the
Development board and the Final save depends on their driver and toolkit; the mechanism is the
same.

## Correctness

- The release binary is a new PGO build of release 1.15's source (sha256 `72d87b2b…`).
- Container validation (Development limits): 71/71 units in both input layouts, with the scoring
  package of the 8 October rules (exact event counts, 256 MiB outputs).
- With `--gpus all` on the T10: 71 units in both layouts, no failure, and the 374 parquet files are
  byte-identical to release 1.15's.
