#!/usr/bin/env bash
# Fetch the third-party inputs next to fastsim/, at the commits release 1.26 was validated against,
# including the reference outputs in Git LFS (about 290 MB), and create packaging/.venv with the
# official toolkit and scoring package. Nothing fetched here is part of this repository: all of it
# is ignored by git and stays under its own licence.
#
#   bash scripts/setup_workspace.sh [python]     # python >= 3.13, default python3
set -euo pipefail
ROOT="$(cd -- "$(dirname -- "$0")/.." && pwd)"
TRACK_COMMIT=1d7e994761f2e9256dd0c572c3592422c8fd0914     # official units and scorer
TOOLKIT_TAG=v2.5.1                                        # official toolkit

fetch() {  # url dir ref
    if [ ! -d "$ROOT/$2/.git" ]; then
        git clone --quiet --filter=blob:none "$1" "$ROOT/$2"
    fi
    git -C "$ROOT/$2" fetch --quiet --tags origin
    git -C "$ROOT/$2" -c advice.detachedHead=false checkout --quiet "$3"
    echo "$2 at $(git -C "$ROOT/$2" rev-parse --short HEAD)"
}

git lfs version >/dev/null 2>&1 || { echo "Git LFS is required: the official reference outputs are stored with it" >&2; exit 1; }
fetch https://github.com/Agenthon-2026/track3-simulation-public track3-simulation-public "$TRACK_COMMIT"
git -C "$ROOT/track3-simulation-public" lfs install --local >/dev/null
git -C "$ROOT/track3-simulation-public" lfs pull
fetch https://github.com/Agenthon-2026/Agenthon2026-public Agenthon2026-public "$TOOLKIT_TAG"
bash "$ROOT/packaging/tools/setup_env.sh" "${1:-python3}"
