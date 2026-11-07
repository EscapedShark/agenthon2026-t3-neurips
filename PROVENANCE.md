# Provenance

## What the team wrote

Everything in this repository, except the licence texts quoted in `THIRD_PARTY_NOTICES.md` and the
`LICENSE` file. That covers the `fastsim/` crate, the tools in `packaging/` and `scripts/`, the
documents, and the measurement scripts and data in `evidence/`.

## AI tools

The team used two AI coding assistants in developing fastsim: Claude Code (Anthropic) and Codex
(OpenAI). This repository's documentation and the poster were also prepared with Claude Code. The
correctness claims can be checked without either tool: the tests, the row-by-row comparison with
the reference outputs and the fuzzing against ABIDES are in [docs/REPRODUCE.md](docs/REPRODUCE.md).

## What it builds on

| Input | How it is used | Licence | In this repository |
|---|---|---|---|
| ABIDES, `jpmorganchase/abides-jpmc-public` at `f9cbe51`, with the four official Track 3 patches | The behaviour fastsim reproduces; run as the reference in differential fuzzing | BSD-3-Clause | No source files; notices included |
| NumPy 1.26.4 | The random stream `src/rng/` reproduces; generated the golden test data | BSD-3-Clause | Golden data in `fastsim/tests/data/`; notices included |
| Track 3 units, references and scorer, `track3-simulation-public` at `1d7e994` | Scenarios, reference outputs, gates; public scenario inputs train PGO | MIT | No, fetched at the pinned commit |
| Agenthon toolkit, `Agenthon2026-public` at `v2.5.1` | Output checks in validation | See that repository | No, fetched at the pinned tag |
| Rust crates in `fastsim/Cargo.lock` | Compiled into the binary or used by tests | See `THIRD_PARTY_NOTICES.md` | No, fetched by Cargo |
| Rust 1.98.1, `rust:1.98.1-slim-trixie` (pinned by digest in the Dockerfile) | Toolchain; static glibc 2.41 in the image | See `THIRD_PARTY_NOTICES.md` | No |

No market data, vendor data or private data was used. No model weights are involved, and the
simulator makes no use of the GPU. Every scenario's seed is in its `scenario.json`.

## How the Final image relates to this repository

`packaging/tools/check_context.py` proves that this repository holds the 200 build inputs of release
1.26, using the file hashes recorded when the image was built
(`evidence/release-1.26/source.json`, context `00dc0cf743ae7df3…`). The image is
`docker.io/greedyshark/fastsim-t3@sha256:ddea2096b660…`, and its binary has sha256
`9c59e65058b3583d…`.

`packaging/docker/Dockerfile` is byte-identical to the file the image was built from, because it is
one of those 200 inputs. Its comments still say `sub/`, the name of `packaging/` during the
competition.

## What was left out

- The Team Key, team claims and submission archives.
- The tools that packed and uploaded submissions, which are not needed to reproduce the method.
- Raw benchmark logs and lab machine details. The notes in `docs/history/` mention folders such as
  `benchmarks/`, which are not in this repository.
- The official repositories and ABIDES, as listed above.

The private development history is not included. This repository starts from the submitted source.
