Release 1.17 ("v18") is release 1.16 with one more line in the image: it asks the NVIDIA hook for
the `utility` driver capability only. The binary is the same file (sha256 `72d87b2b…`; the build
reused 1.16's compile stage), and every trace and ledger file is byte-identical to release 1.15's,
in both input layouts.

## The setting

Every Track 3 unit runs with `--gpus all`. The hook reads `NVIDIA_DRIVER_CAPABILITIES` from the
container's environment and mounts that set of driver files into the root, then runs `ldconfig`
there. Without the variable it mounts the default set, `utility,compute`.

- The simulator computes nothing on the GPU, so the image sets
  `NVIDIA_DRIVER_CAPABILITIES=utility`. On x86 with a Tesla T10 (driver 535.309.01, NVIDIA
  Container Toolkit 1.20.1) the hook then mounts 5 libraries and 3 tools instead of 20 and 5.
  The device itself is granted as before.
- The variable must name listed capabilities. `none` fails the container's start ("unsupported
  capabilities found in 'none'"), and an empty value means the default set.
- `NVIDIA_VISIBLE_DEVICES` in the image changes nothing: `--gpus all` sets the devices.

## Measurements

All 71 units in the Final layout, run as the organizers' runner does (`--network=none --cpus=4
--memory=16g`, host user) plus `--gpus all`, five runs per image after a warm-up, in both label
orders:

| | Release 1.16 | v18 (order A) | v18 (order B) |
|---|---:|---:|---:|
| Mean of rows / median window (the Final's measure) | 763,635 / 762,102 | 771,516 (+1.03%) | 775,565 (+1.77%) |
| The same over the whole `docker run` | 456,635 / 453,859 | 458,958 (+0.51%) | 461,009 (+1.58%) |
| Median unit's window | 127.8 / 127.0 ms | 126.2 ms | 126.0 ms |
| Units faster | | 48 / 71 | 44 / 71 |

About 1 ms per unit (median change -1.0 and -1.3 ms), against run-to-run differences of up to
14 ms either way on single units. An earlier pair of runs of the same image built on top of 1.16
measured +0.71% and +1.60%.

## Correctness

- Container validation (Development limits): 71/71 units in both input layouts.
- With `--gpus all` on the T10: 71 units in both layouts, no failure, and the 374 parquet files are
  byte-identical to release 1.15's.
