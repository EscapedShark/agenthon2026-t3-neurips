#!/usr/bin/env python3
"""Build, train, and rebuild fastsim with PGO using public scenario inputs only.

Requires Python 3.11+, Cargo, and llvm-profdata matching rustc's LLVM major version.
The requested target must run on this machine (or in its emulator). Each invocation
uses a fresh work directory; profiles are never mixed across source revisions.
"""
from __future__ import annotations

import argparse
import hashlib
import json
import os
import re
import shlex
import shutil
import subprocess
import tempfile
import time
from pathlib import Path

ROOT = Path(__file__).resolve().parents[1]


def digest(path: Path) -> str:
    return hashlib.sha256(path.read_bytes()).hexdigest()


def run(argv: list[str], *, env: dict | None = None, capture: bool = False) -> str:
    if not capture:
        print(shlex.join(argv), flush=True)
    result = subprocess.run(argv, cwd=ROOT, env=env, check=True, text=True,
                            stdout=subprocess.PIPE if capture else None)
    return result.stdout or ""


def source_hashes() -> dict[str, str]:
    files = [ROOT / name for name in ("Cargo.toml", "Cargo.lock", "tools/pgo_build.py")]
    files.extend(sorted((ROOT / "src").rglob("*.rs")))
    return {str(p.relative_to(ROOT)): digest(p) for p in files}


def main() -> None:
    ap = argparse.ArgumentParser(description=__doc__)
    ap.add_argument("--units", type=Path, default=ROOT.parent / "track3-simulation-public/units")
    ap.add_argument("--target", help="defaults to rustc's host triple")
    ap.add_argument("--llvm-profdata", type=Path)
    ap.add_argument("--work-dir", type=Path, default=ROOT / "target/pgo")
    ap.add_argument("--out", type=Path, help="also copy the optimized executable here")
    ap.add_argument("--rounds", type=int, default=2)
    ap.add_argument("--offline", action="store_true")
    ap.add_argument("--features", help="cargo features of both builds (measurement builds, e.g. marks)")
    args = ap.parse_args()
    if args.rounds < 1:
        ap.error("--rounds must be positive")
    rustc = run(["rustc", "-Vv"], capture=True)
    host = re.search(r"^host: (.+)$", rustc, re.M).group(1)
    llvm_major = re.search(r"^LLVM version: (\d+)", rustc, re.M).group(1)
    target = args.target or host
    sysroot = Path(run(["rustc", "--print", "sysroot"], capture=True).strip())
    bundled = sysroot / "lib/rustlib" / host / "bin/llvm-profdata"
    profdata = args.llvm_profdata or (bundled if bundled.is_file() else shutil.which("llvm-profdata"))
    if not profdata:
        ap.error("install rustup's llvm-tools-preview or pass --llvm-profdata")
    profdata = str(Path(profdata).resolve())
    llvm_version = run([profdata, "--version"], capture=True)
    if not re.search(r"version\s+" + llvm_major + r"\.", llvm_version, re.I):
        ap.error(f"llvm-profdata must match rustc's LLVM {llvm_major}: {llvm_version.strip()}")
    units = sorted(p for p in args.units.resolve().iterdir() if (p / "card.toml").is_file())
    if not units:
        ap.error("--units contains no public unit cards")
    inputs = {}
    for unit in units:
        files = [unit / "card.toml"]
        files += ([unit / "batch.json", *sorted((unit / "scenarios").glob("*.json"))]
                  if (unit / "batch.json").is_file() else [unit / "scenario.json"])
        for path in files:
            inputs[str(path.relative_to(args.units.resolve()))] = digest(path)
    sources = source_hashes()
    work = args.work_dir.resolve()
    work.mkdir(parents=True, exist_ok=False)
    raw = work / "raw"
    raw.mkdir()
    env = dict(os.environ)
    flags = (env["CARGO_ENCODED_RUSTFLAGS"].split("\x1f") if env.get("CARGO_ENCODED_RUSTFLAGS")
             else shlex.split(env.get("RUSTFLAGS", "")))
    if any("profile-generate" in f or "profile-use" in f for f in flags):
        ap.error("base RUSTFLAGS must not already contain a PGO mode")
    env.pop("RUSTFLAGS", None)
    # Different processes never overwrite one another's counters.
    env["LLVM_PROFILE_FILE"] = str(raw / "%m-%p.profraw")
    # The image sets FASTSIM_STREAM=1 (the binary's default too): train the scenarios on that path.
    env["FASTSIM_STREAM"] = "1"
    command = ["cargo", "build", "--release", "--locked", "--bin", "fastsim", "--target", target]
    if args.offline:
        command.append("--offline")
    if args.features:
        command += ["--features", args.features]
    durations = {}

    def build(name: str, mode: list[str]) -> Path:
        build_env = {**env, "CARGO_TARGET_DIR": str(work / name),
                     "CARGO_ENCODED_RUSTFLAGS": "\x1f".join(flags + mode)}
        start = time.monotonic()
        run(command, env=build_env)
        durations[name] = time.monotonic() - start
        return work / name / target / "release/fastsim"

    instrumented = build("instrumented", [f"-Cprofile-generate={raw}"])
    start = time.monotonic()
    with tempfile.TemporaryDirectory(prefix="training-", dir=work) as temporary:
        out = Path(temporary)
        for round_no in range(args.rounds):
            for unit in units:
                dest = out / unit.name
                config = unit / "scenario.json"
                if round_no % 2 == 1 and not (unit / "batch.json").is_file():
                    # Without its card a scenario writes the ledger whatever the unit requires (the
                    # official local harness mounts no card): every other round trains that too.
                    alone = out / "final-layout" / unit.name
                    alone.mkdir(parents=True, exist_ok=True)
                    shutil.copyfile(config, alone / "scenario.json")
                    config = alone / "scenario.json"
                argv = (["simulate-batch", "--batch-dir", str(unit / "scenarios"), "--out-dir", str(dest)]
                        if (unit / "batch.json").is_file() else
                        ["simulate", "--config", str(config), "--out", str(dest / "trace.parquet")])
                event = json.loads(run([str(instrumented), *argv], env=env, capture=True))
                if event.get("total_events", event.get("n_events", 0)) <= 0:
                    raise RuntimeError(f"training produced no events: {unit.name}")
            print(f"Trained {len(units)} units, round {round_no + 1}/{args.rounds}", flush=True)
    durations["training"] = time.monotonic() - start
    profiles = sorted(raw.glob("*.profraw"))
    if not profiles:
        raise RuntimeError("instrumented executions produced no profiles")
    merged = work / "merged.profdata"
    run([profdata, "merge", "-o", str(merged), *map(str, profiles)])
    if source_hashes() != sources:
        raise RuntimeError("source changed during training; rebuild in a fresh work directory")
    optimized = build("optimized", [f"-Cprofile-use={merged}", "-Cllvm-args=-pgo-warn-missing-function"])
    if source_hashes() != sources:
        raise RuntimeError("source changed during the optimized build")
    metadata = {"rustc": rustc, "llvm_profdata": llvm_version, "target": target, "base_flags": flags,
                "rounds": args.rounds, "features": args.features, "units": [p.name for p in units],
                "source_sha256": sources,
                "training_sha256": inputs, "profile_count": len(profiles), "durations_sec": durations,
                "profile_sha256": digest(merged), "binary": str(optimized), "binary_sha256": digest(optimized)}
    if args.out:
        args.out.parent.mkdir(parents=True, exist_ok=True)
        shutil.copy2(optimized, args.out)
        metadata["output"] = str(args.out.resolve())
    (work / "build.json").write_text(json.dumps(metadata, indent=2) + "\n")
    print(f"PGO executable: {args.out.resolve() if args.out else optimized}", flush=True)


if __name__ == "__main__":
    main()
