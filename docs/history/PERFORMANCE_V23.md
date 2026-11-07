Release 1.22 ("v23") changes where the image keeps its files, not the simulator. The binary is
release 1.21's (the build stage is unchanged and was reused from the build cache), so every output
byte is release 1.21's.

## The finding

The Final times each run from Docker's `State.StartedAt` to `State.FinishedAt`. In Docker 29.7.2,
the platform's version, StartedAt is taken just before containerd creates the task
(`daemon/start.go`: `startupTime := time.Now()` before `ctr.NewTask`). The rootfs mount and the
OCI spec come before it; the shim, `runc create` with the GPU injection and its hooks, `runc start`,
our process and its exit are inside the window.

Two facts about that stack decide what the image costs inside the window:

- **GPU injection is CDI first.** From Docker 29.3, `--gpus` asks for the CDI device
  `nvidia.com/gpu=all` when `nvidia-cdi-hook` is installed and only falls back to the legacy
  prestart hook (`daemon/devices_nvidia_linux.go`); CDI is on by default. NVIDIA Container Toolkit
  1.19.1 (the platform's) enables `nvidia-cdi-refresh`, which writes `/var/run/cdi/nvidia.yaml` at
  install and boot. So the platform most likely mounts every driver file into the root (about 50),
  runs the createContainer hooks `create-symlinks`, `enable-cuda-compat`, `update-ldcache` and
  `disable-device-node-modification`, and ignores `NVIDIA_DRIVER_CAPABILITIES`. The lab's Docker
  29.1.3 still uses the legacy hook for `--gpus`; `--device nvidia.com/gpu=all` reproduces CDI there.
- **Overlayfs fsyncs a directory it copies up.** Since "ovl: fsync after metadata copy-up" (Linux
  6.12; in the 6.10.14 and 6.11.11 stable releases, not in 6.11.1), copying an image directory up
  into the container's writable layer ends with an fsync of it. The platform runs 6.11.0-1016-nvidia.

Release 1.21's image had its binary in `/usr/local/bin`. When a container started, the GPU
injection created `/usr/bin`, `/usr/lib` and `/usr/share`, which copied `/usr` up, and ldconfig and
the hooks wrote `/etc` (Docker's init layer always holds `/etc`), which copied `/etc` up: two
copy-ups with an fsync each, inside the window. On the lab's NVMe an fsync takes about 3 ms, and each
of these copy-ups cost about 3.6 ms. The same count explains the earlier Debian-slim result: 1.15
copied up 11 directories during the start, 1.21 two, and 9 x 3.6 ms is the 31 ms between them.
Directories added to the image to save the injection work make it much worse: the 28 directories
the CDI spec writes into, created in the image, cost +102 ms.

## The change

- The binary is `/bin/simulate`, with `simulate-batch` linking to it. The image holds no `/usr`, so
  the injection creates it fresh in the writable layer. `PATH` still finds both verbs.
- The image's config sets `WorkingDir` to `/etc/fastsim`, a directory the image does not contain.
  Docker creates a missing working directory when it creates the container, which copies `/etc` up
  then, outside the window; when ldconfig and the hooks write `/etc` during the start, it is
  already in the writable layer. A Dockerfile `WORKDIR` would create the directory in the image and
  undo this, so `sub/tools/build.py` sets it after the build in the config alone (`docker save`,
  new config and manifest, `docker load`; the layer is the build's) and refuses a layer holding
  anything but `bin/simulate`, `bin/simulate-batch`, `etc` and `tmp`.
- The Dockerfile's build stage is unchanged, so the build reused the cached PGO binary; a `layout`
  stage copies it to `/bin`.

During a start, 1.21 copied up `/usr` and `/etc`; this layout copies up nothing (`docker diff`
after the container is created and after it ran, CDI and legacy alike).

## Measurements

x86 lab (i7-10700K, Tesla T10 with driver 535.309.01, Docker 29.1.3, containerd snapshotter,
runc 1.3.4, toolkit 1.20.1, ext4 on NVMe), images built from the 1.21 binary.

The smallest unit (s001), StartedAt to FinishedAt, medians, images interleaved:

| GPU injection | 1.21 layout | 1.22 layout | Change |
|---|---:|---:|---:|
| CDI (`--device nvidia.com/gpu=all`), 80 runs each | 108.9 ms | 101.2 ms | -7.7 ms |
| Legacy hook (`--gpus all`), 60 runs each | 122.2 ms | 117.8 ms | -4.4 ms |
| No GPU, 40 runs each | 64.4 ms | 63.6 ms | no change |

Each part alone (CDI, 80 runs each): the binary in `/bin` without the config WorkingDir 105.2 ms
(-3.7), the WorkingDir on top 101.2 ms (-4.0 more).

The release images themselves (s001, CDI, 60 runs each): window 110.2 ms (1.21) and 102.4 ms
(1.22), -7.8 ms; creation to removal 250.8 and 245.0 ms, -5.8 ms.

All 71 units as the organizers' runner starts them (`--network=none --cpus=4 --memory=16g`, host
user, `-e NVIDIA_DRIVER_CAPABILITIES=compute`) with CDI injection, five runs per image after a
warm-up, both label orders:

| | order A | order B |
|---|---:|---:|
| Mean of rows / median window (the Final's measure) | +6.13% | +6.31% |
| Median change of a unit's window | -6.7 ms | -8.5 ms |
| Units faster | 70 / 71 | 70 / 71 |
| Failed runs | 0 | 0 |

**On the platform.** The saving is two copy-ups, each about one fsync of the host's Docker data
disk. Release 1.16 (v17) removed nine copy-ups and saved 51 ms per unit on the Development board
(±20 ms between board runs), about 5.7 ms each, so this layout should save on the order of 11 ms per
unit there (7-16 ms). Development's clock runs from creation to removal and still contains the
earlier `/etc` copy-up, so it sees only the `/usr` part, below its run-to-run noise.

## Correctness

- **Image:** `fastsim-t3:1.22`, manifest `sha256:7b7ff1e47b9ca2d20fc9013eed9f1ebabff784defd83001616c1d02f0f16fc04`,
  one layer holding `bin/simulate`, `bin/simulate-batch -> simulate`, `etc/` and `tmp/` (1777),
  config `WorkingDir=/etc/fastsim`. The build's own manifest before the config change was
  `sha256:28a2b3dc…` (records in `sub/releases/1.22/image.json`).
- **Binary:** sha256 `31921ddd…` (1,858,832 bytes), the same file as release 1.21's: the build stage
  came from the build cache.
- **Validation:** `validate.py` 71/71 units in both input layouts (142 runs) under the Development
  limits (read-only root, uid 65534, noexec /tmp), 0 failures.
- **Copy-ups during the start** (`docker diff` after creation and after the run, lab, CDI and
  legacy, launcher's `-e NVIDIA_DRIVER_CAPABILITIES=compute`): 1.21 copies up `/usr` and `/etc`;
  1.22 copies up `/etc` at creation and nothing during the start.
- **Outputs with GPU injection** (lab, every unit in both input layouts under the Development limits):
  1.21 with CDI, 1.22 with CDI and 1.22 with the legacy hook, 0 failures; all 374 parquet files of
  both 1.22 runs are byte-identical to 1.21's.
- **Published:** `docker.io/greedyshark/fastsim-t3@sha256:7b7ff1e47b9ca2d20fc9013eed9f1ebabff784defd83001616c1d02f0f16fc04`
  (registry digest equal to the validated image's; anonymous pull 200). Development ZIP
  `sub/submissions/submission-1.22-7b7ff1e4-dev.zip` = `v23.zip`, sha256 `08025e97…`, check_zip passed.
- **Tests:** `cargo test --release` passes; `sub/tools/test_build_context.py` gained tests for the
  config change (it keeps the layer, updates config, manifest, index and `manifest.json`, and refuses
  `/usr`, `/lib`, `etc/fastsim`, a wrong `/tmp` mode or link, a missing binary and a Dockerfile
  WORKDIR).
