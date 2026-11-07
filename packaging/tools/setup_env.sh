#!/usr/bin/env bash
# Create packaging/.venv: Python >= 3.13 with the official toolkit pinned at its release tag
# (installed from the local clone's tag, not its working tree) and the Track 3 scoring package.
# Rerun after pulling either official repository.
#
#   bash packaging/tools/setup_env.sh [python]
set -euo pipefail
SUB="$(cd -- "$(dirname -- "$0")/.." && pwd)"
WORKSPACE="$(dirname -- "$SUB")"
PYTHON="${1:-python3}"
TOOLKIT_TAG="v2.5.1"

"$PYTHON" -c 'import sys; assert sys.version_info >= (3, 13), sys.version'
rm -rf "$SUB/.venv"
"$PYTHON" -m venv "$SUB/.venv"
STAGE="$(mktemp -d)"
trap 'rm -rf "$STAGE"' EXIT
git -C "$WORKSPACE/Agenthon2026-public" archive "$TOOLKIT_TAG" common | tar -x -C "$STAGE"
mkdir "$STAGE/track3"
git -C "$WORKSPACE/track3-simulation-public" archive HEAD pyproject.toml qfbench2_track_simulation \
  | tar -x -C "$STAGE/track3"
"$SUB/.venv/bin/python" -m pip install --quiet --upgrade pip
# pandas and pyarrow at the versions release 1.26 was validated with
"$SUB/.venv/bin/python" -m pip install --quiet "$STAGE/common" "$STAGE/track3" pandas==3.0.6 pyarrow==25.0.1
"$SUB/.venv/bin/python" - <<'EOF'
import importlib.metadata as m, json, pathlib, qfbench2_common
schema = json.loads((pathlib.Path(qfbench2_common.__file__).parent / "schemas/submission.schema.json").read_text())
assert "image" in schema["properties"] and schema.get("additionalProperties") is False and len(schema["required"]) == 12
assert m.version("qfbench2-common") == "2.5.1", m.version("qfbench2-common")
print("packaging/.venv ready:", {p: m.version(p) for p in ("qfbench2-common", "qfbench2-track-simulation", "pandas", "pyarrow")})
EOF
