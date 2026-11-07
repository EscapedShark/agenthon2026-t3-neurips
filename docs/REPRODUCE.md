# Reproducing the result

All commands run from the repository root.

## Requirements

| Step | Needs |
|---|---|
| 1 to 4 | Git with [Git LFS](https://git-lfs.com/), Rust 1.98 or newer, Python 3.13 or newer |
| 5 | Python 3.11 and the ABIDES baseline |
| 6 and 7 | Docker; a linux/amd64 host for timing |

## 1. Fetch the official inputs

```bash
bash scripts/setup_workspace.sh
```

This clones two repositories next to `fastsim/`, at the commits release 1.26 was validated against,
pulls the reference outputs from Git LFS (about 290 MB), and creates `packaging/.venv` with the
official toolkit and scoring package. Git ignores all of it.

| Input | Pin | Licence |
|---|---|---|
| [track3-simulation-public](https://github.com/Agenthon-2026/track3-simulation-public): units, references, scorer | `1d7e994761f2e9256dd0c572c3592422c8fd0914` | MIT |
| [Agenthon2026-public](https://github.com/Agenthon-2026/Agenthon2026-public): toolkit | tag `v2.5.1` | see that repository |

## 2. Check the source identity

```bash
python3 packaging/tools/check_context.py
```

This stages the Docker build context without running Docker: the crate, the PGO builder, the
Dockerfile and the public scenario inputs. It compares all 200 files with the record of the Final
image. Expected last line:

```
OK: identical to the build context of release 1.26
```

The context digest is `00dc0cf743ae7df345ac9fc0e0ef157a0e19ce17eab339e2cc68d660185dcf8c`.

## 3. Run the tests

```bash
cd fastsim && cargo test --release && cd ..
```

Expected: 45 tests pass.

## 4. Compare with the reference

```bash
packaging/.venv/bin/python fastsim/tools/compare_unit.py --bin fastsim/target/release/fastsim
```

This runs every public scenario and compares the outputs with the reference files, then runs the
official scorer's own checks. Expected: 95/95 identical and 95/95 passing the gates.

macOS works for this step. Its libm can differ from glibc by 1 or 2 ULP, which does not reach any
integer output.

## 5. Fuzz against ABIDES (optional)

Install the pinned baseline, as `baselines/README.md` in `track3-simulation-public` describes. It
needs Python 3.11, because ABIDES needs pandas 1.x:

```bash
python3.11 -m venv .venv-abides
.venv-abides/bin/python -m pip install numpy==1.26.4 pandas==1.5.3 scipy==1.17.1 pyarrow==15.0.2 coloredlogs==15.0.1
git clone https://github.com/jpmorganchase/abides-jpmc-public abides-ref
git -C abides-ref checkout f9cbe51342b7dedd9587e4e069040d68a5c6477f
for patch in order_size_model.pomegranate-free kernel_message_ledger exchange_protocol_stp oracle_scheduled_jump; do
  git -C abides-ref apply "$PWD/track3-simulation-public/baselines/patches/$patch.patch"
done
.venv-abides/bin/python -m pip install --no-deps abides-ref/abides-core abides-ref/abides-markets
```

Then run:

```bash
.venv-abides/bin/python fastsim/tools/fuzz_vs_abides.py 100
```

The last line counts the verdicts. `OK` means identical rows, and there must be no `MISMATCH`.
`ABIDES-ERROR` means ABIDES itself crashed, usually with `KeyError: 'order_id'`, so that scenario
has no reference. The `--mixed`, `--scale` and `--min-draws` options select the campaigns described
in the history notes.
[evidence/fuzz/](../evidence/fuzz/) has the summaries and the boundary campaign.

## 6. Use the Final image

```bash
docker pull docker.io/greedyshark/fastsim-t3@sha256:ddea2096b66000e6856f7b8aede5ebe25c89bd53858828f973a45cfc9f0a7320
```

The image needs an x86-64-v3 CPU. It has both verbs on `PATH` and no entrypoint:

```bash
docker run --rm --platform linux/amd64 --network=none --cpus=4 --memory=16g \
  -v "$PWD/track3-simulation-public/units/t3-s001-price-time-priority:/input:ro" \
  -v "$PWD/out:/output" --user "$(id -u):$(id -g)" \
  docker.io/greedyshark/fastsim-t3@sha256:ddea2096b66000e6856f7b8aede5ebe25c89bd53858828f973a45cfc9f0a7320 \
  simulate --config /input/scenario.json --out /output/trace.parquet
```

Create `out/` first.

## 7. Rebuild and validate the image (optional)

```bash
packaging/.venv/bin/python packaging/tools/build.py
packaging/.venv/bin/python -u packaging/tools/validate.py --workers 3
```

`build.py` stages the context, runs the PGO build in the pinned Rust image, sets `WorkingDir` in the
image config, and refuses any layer that holds more than the expected five entries. `validate.py`
runs all 71 units in both input layouts under the platform's limits and checks every output against
the reference.

A rebuilt image will not have the Final image's digest. PGO training depends on thread timing, so
two builds of one source give different binaries with identical outputs. The Final binary is the
fastest of 12 native builds (sha256 `9c59e65058b3583d…`), which is why the published digest is the
reference. `evidence/release-1.26/` holds the build and validation records.

## What cannot be reproduced here

- **The Final score.** It came from sealed units on the organizers' machine. Neither is public.
- **Exact timings.** They depend on the machine. Program times and speedups in this repository come
  from an i7-10700K running Linux. To repeat the speedups, install the baseline as in step 5, extract
  the Final binary from the image, and run
  [evidence/same-machine/measure_same_machine.py](../evidence/same-machine/measure_same_machine.py)
  with `--bin` and `--python`. ABIDES ran at 0.96× its official reference time on that machine.
- **The container floor.** It depends on the host's GPU stack. The organizers published theirs on
  [#24](https://github.com/Agenthon-2026/track3-simulation-public/issues/24).
