# Image pipeline

| File | Purpose |
|---|---|
| `docker/Dockerfile` | The image: a PGO build in the pinned Rust image, then `FROM scratch` with one binary |
| `tools/setup_env.sh` | Creates `.venv` with the official toolkit and scorer |
| `tools/check_context.py` | Checks this repository against the Final image's build inputs, without Docker |
| `tools/build.py` | Stages the context under `releases/`, builds, sets `WorkingDir` in the image config |
| `tools/validate.py` | Runs all units in platform-limited containers and checks every output |
| `tools/test_build_context.py` | Tests context staging and the config patch |

Usage is in [../docs/REPRODUCE.md](../docs/REPRODUCE.md). The Dockerfile is one of the build inputs
that `check_context.py` hashes, so it is left exactly as it was built. Its comments say `sub/`, the
name of this folder during the competition.
