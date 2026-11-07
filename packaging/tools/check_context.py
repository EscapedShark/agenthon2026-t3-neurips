#!/usr/bin/env python3
"""Check that this repository holds the build inputs of the Final image, release 1.26.

Stages the Docker build context exactly as build.py does, without running Docker, and compares it
with evidence/release-1.26/source.json. The context is the crate, the PGO builder, the Dockerfile
and the public scenario inputs used for PGO training, so the official units must be checked out at
the pinned commit first (scripts/setup_workspace.sh).

    python3 packaging/tools/check_context.py
"""
from __future__ import annotations

import json
import sys
import tempfile
from pathlib import Path

sys.dont_write_bytecode = True
from build import stage_context  # noqa: E402
from common import WORKSPACE, tree_digest  # noqa: E402

RECORD = WORKSPACE / "evidence" / "release-1.26" / "source.json"


def main() -> int:
    expected = json.loads(RECORD.read_text(encoding="utf-8"))
    with tempfile.TemporaryDirectory(prefix="fastsim-context-") as tmp:
        files = stage_context(Path(tmp) / "context")
    digest = tree_digest(files)
    wanted = expected["files"]
    missing = sorted(set(wanted) - set(files))
    extra = sorted(set(files) - set(wanted))
    changed = sorted(name for name in set(files) & set(wanted) if files[name] != wanted[name])
    print(f"staged {len(files)} files, context {digest}")
    print(f"record {len(wanted)} files, context {expected['context_sha256']}")
    for label, names in (("missing", missing), ("extra", extra), ("changed", changed)):
        for name in names:
            print(f"{label}: {name}")
    if digest != expected["context_sha256"]:
        print("MISMATCH: these inputs are not release 1.26's")
        return 1
    print("OK: identical to the build context of release 1.26")
    return 0


if __name__ == "__main__":
    raise SystemExit(main())
