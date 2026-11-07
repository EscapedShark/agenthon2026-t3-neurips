#!/usr/bin/env python3
"""Step 3: run every public unit in platform-limited linux/amd64 containers and check the outputs.

    packaging/.venv/bin/python -u packaging/tools/validate.py [--version V] [--workers 3] [--units NAME ...]

Each unit runs once per input layout the official material describes:

  unit    the whole unit directory at /input (SUBMISSION_CLI.md, DEVELOPMENT-RUNTIME.md), so
          card.toml is visible and the message ledger follows the card;
  staged  only scenario.json or scenarios/ at /input (throughput/run_unit.py,
          regression_suite/run_regression.py), so the ledger is always written.

Container settings are the Development limits: uid 65534, read-only root, 64 MiB noexec /tmp,
256 PIDs, 1,024 files, 64 MiB per file, the card's CPUs and memory, no swap, no network.

Every run must exit 0; pass the official output sanitizer with the Track 3 allowlist; write exactly
the expected files within 64 MiB; produce a trace (and ledger) equal to the unit's reference row
for row, dtypes included; write events.json that agrees with the trace; and be admitted by the
official developer verifier. The two layouts must produce byte-identical stable outputs, which is
the Final's repeat rule. A full pass writes validation.json and release-manifest.json.
"""
from __future__ import annotations

import argparse
import importlib.metadata as metadata
import shutil
import sys
import tempfile
import threading
import time
import tomllib
from concurrent.futures import ThreadPoolExecutor
from pathlib import Path
from typing import Any

sys.dont_write_bytecode = True
import pandas as pd  # noqa: E402
import pyarrow.parquet as pq  # noqa: E402
from qfbench2_common import manifest as unit_manifest  # noqa: E402
from qfbench2_common import taskcard  # noqa: E402
from qfbench2_common.sanitize import materialize_tree, verify_destination  # noqa: E402
from qfbench2_track_simulation.limits import allowed_paths_for, requires_message_ledger  # noqa: E402
from qfbench2_track_simulation.scoring import build_developer_verifier  # noqa: E402

from common import (  # noqa: E402
    SUB, TOOLKIT_VERSION, TRACK_REPO, ToolError, git_state, image_identity, load_release, now,
    read_json, run, sha256_file, units, write_json,
)

LIMIT = 64 << 20
LAYOUTS = ("unit", "staged")
STABLE = ("trace.parquet", "message_trace.parquet")
RESOURCES = {
    "platform": "linux/amd64", "user": "65534:65534", "root": "read-only", "capabilities": "none",
    "no_new_privileges": True, "tmp": "tmpfs 64 MiB rw,noexec,nosuid,nodev", "pids": 256,
    "nofile": 1024, "nproc": 256, "fsize_bytes": LIMIT, "cpus_memory": "from each card, swap off",
    "network": "none", "env": {"QFBENCH_SEED": "0", "QFBENCH_NETWORK": "none"},
}
_verifier_lock = threading.Lock()
_compare_lock = threading.Lock()
_compare_cache: dict[tuple[str, str], str | None] = {}


def container_command(tag: str, card: dict, in_dir: Path, out_dir: Path, batch: bool) -> list[str]:
    env = card.get("environment", {})
    cpus = str(env.get("cpus", 4))
    memory = str(env.get("memory", "16g")).lower().replace("gi", "g")
    verb = (["simulate-batch", "--batch-dir", "/input/scenarios", "--out-dir", "/output"] if batch
            else ["simulate", "--config", "/input/scenario.json", "--out", "/output/trace.parquet"])
    return ["docker", "run", "--rm", "--platform", "linux/amd64", "--pull", "never",
            "--read-only", "--user", "65534:65534", "--cap-drop=ALL", "--security-opt", "no-new-privileges",
            "--tmpfs", "/tmp:rw,noexec,nosuid,nodev,size=64m",
            "--pids-limit", "256", "--ulimit", "nofile=1024:1024", "--ulimit", "nproc=256:256",
            "--ulimit", f"fsize={LIMIT}:{LIMIT}",
            "--cpus", cpus, "--memory", memory, "--memory-swap", memory, "--network=none",
            "-v", f"{in_dir}:/input:ro", "-v", f"{out_dir}:/output:rw",
            "-e", "QFBENCH_SEED=0", "-e", "QFBENCH_NETWORK=none", tag, *verb]


def rows(path: Path) -> int:
    return int(pq.ParquetFile(path).metadata.num_rows)


def frame_mismatch(candidate: Path, reference: Path) -> str | None:
    """None when the two parquet files hold the same frame: columns, dtypes, every value."""
    key = (sha256_file(candidate), str(reference))
    with _compare_lock:
        if key in _compare_cache:
            return _compare_cache[key]
    try:
        pd.testing.assert_frame_equal(pd.read_parquet(candidate), pd.read_parquet(reference), check_exact=True)
        verdict = None
    except AssertionError as error:
        verdict = " ".join(str(error).split())[:500]
    with _compare_lock:
        _compare_cache[key] = verdict
    return verdict


def check_events(doc: dict, scenario: dict, trace: Path, ledger: Path | None, fields: list[str]) -> list[str]:
    problems = [f"events.json lacks {name}" for name in fields if name not in doc]
    n = rows(trace)
    if doc.get("n_events") != n:
        problems.append(f"n_events {doc.get('n_events')} != trace rows {n}")
    for name in ("scenario_id", "seed"):
        if doc.get(name) != scenario.get(name):
            problems.append(f"events.json {name} {doc.get(name)!r} != scenario {scenario.get(name)!r}")
    problems += rate_problems(doc, n, "wall_clock_sec", "events_per_sec")
    if doc.get("trace_sha256") != sha256_file(trace):
        problems.append("trace_sha256 does not match trace.parquet")
    if ledger is not None:
        if doc.get("n_messages") != rows(ledger):
            problems.append("n_messages does not match message_trace.parquet")
        # Optional (the scorer never reads it): checked only when a release reports it.
        if "message_trace_sha256" in doc and doc["message_trace_sha256"] != sha256_file(ledger):
            problems.append("message_trace_sha256 does not match message_trace.parquet")
    return problems


def rate_problems(doc: dict, n: int, wall_key: str, rate_key: str) -> list[str]:
    wall, rate = doc.get(wall_key), doc.get(rate_key)
    if not isinstance(wall, (int, float)) or wall <= 0 or not isinstance(rate, (int, float)):
        return [f"{wall_key}/{rate_key} missing or not positive"]
    if abs(rate - n / wall) > 0.05 * (n / wall):  # the Development board's self-report check
        return [f"{rate_key} {rate:.1f} is not n/{wall_key} ({n / wall:.1f}) within 5%"]
    return []


def check_batch_events(doc: dict, accepted: Path, subs: list[str]) -> list[str]:
    counts = {sub: rows(accepted / sub / "trace.parquet") for sub in subs}
    problems = []
    if doc.get("n_scenarios") != len(subs):
        problems.append("batch_events n_scenarios != declared subs")
    if doc.get("total_events") != sum(counts.values()):
        problems.append(f"batch_events total_events {doc.get('total_events')} != trace rows {sum(counts.values())}")
    listed = {entry.get("sub"): entry for entry in doc.get("per_scenario", [])}
    for sub in subs:
        entry = listed.get(sub, {})
        if entry.get("n_events") != counts[sub] or entry.get("trace_sha256") != sha256_file(accepted / sub / "trace.parquet"):
            problems.append(f"batch_events per_scenario disagrees with {sub}")
    return problems + rate_problems(doc, sum(counts.values()), "wall_clock_sec", "events_per_sec")


def run_layout(tag: str, unit: Path, card: dict, card_valid: dict, layout: str, work: Path) -> dict[str, Any]:
    batch = (unit / "batch.json").exists()
    roster = read_json(unit / "batch.json")["subs"] if batch else []
    subs = [str(entry["sub"]) for entry in roster]
    base = work / layout
    base.mkdir()
    if layout == "unit":
        in_dir = unit
    else:
        in_dir = base / "input"
        in_dir.mkdir()
        if batch:
            shutil.copytree(unit / "scenarios", in_dir / "scenarios")
        else:
            (in_dir / "scenario.json").write_bytes((unit / "scenario.json").read_bytes())
    out_dir = base / "output"
    out_dir.mkdir()
    out_dir.chmod(0o777)
    command = container_command(tag, card, in_dir, out_dir, batch)
    started = time.monotonic()
    try:
        result = run(command, timeout=1800, check=False)
    except ToolError as error:
        return {"exit": None, "problems": [str(error)]}
    wall = time.monotonic() - started
    record: dict[str, Any] = {"exit": result.returncode, "host_wall_sec": round(wall, 3), "problems": []}
    problems = record["problems"]
    if result.returncode != 0:
        problems.append(f"exit {result.returncode}: {(result.stderr or '')[-800:]}")
        return record

    accepted = base / "accepted"
    materialized = materialize_tree(out_dir, accepted, allowed_paths=allowed_paths_for(unit))
    if materialized.rejections or materialized.unsafe_modes:
        problems.append(f"sanitizer refused the tree: {dict((k.value, v) for k, v in materialized.rejections.items())}, unsafe modes {materialized.unsafe_modes}")
        return record
    problems += verify_destination(accepted, materialized.files)
    files = {f.path: f for f in materialized.files}
    record["files"] = {path: {"sha256": f.sha256, "bytes": f.size_bytes} for path, f in sorted(files.items())}
    record["total_bytes"] = materialized.total_bytes
    if materialized.total_bytes > LIMIT or any(f.size_bytes > LIMIT for f in files.values()):
        problems.append("output exceeds 64 MiB")
    ledger_expected = batch or layout == "staged" or requires_message_ledger(card)
    record["ledger_written"] = any(path.endswith("message_trace.parquet") for path in files)
    if batch:
        expected = {"batch_events.json"} | {f"{sub}/{name}" for sub in subs
                                             for name in ("trace.parquet", "events.json", "message_trace.parquet")}
    else:
        expected = {"trace.parquet", "events.json"} | ({"message_trace.parquet"} if ledger_expected else set())
    if set(files) != expected:
        problems.append(f"files {sorted(files)} != expected {sorted(expected)}")
        return record

    fields = list(card.get("io", {}).get("output_events_fields", []))
    markets = [(str(e["sub"]), accepted / str(e["sub"]), unit / "checks" / "reference_data" / str(e["sub"]),
                read_json(unit / e["scenario_file"])) for e in roster] if batch else [
        (None, accepted, unit, read_json(unit / "scenario.json"))]
    n_events = 0
    for sub, produced, reference, scenario in markets:
        label = f"{sub}/" if sub else ""
        trace = produced / "trace.parquet"
        ledger = produced / "message_trace.parquet"
        ledger = ledger if ledger.exists() else None
        n_events += rows(trace)
        for name, path in (("trace.parquet", trace), ("message_trace.parquet", ledger)):
            if path is not None:
                mismatch = frame_mismatch(path, reference / name)
                if mismatch:
                    problems.append(f"{label}{name} differs from the reference: {mismatch}")
        problems += [f"{label}{p}" for p in check_events(read_json(produced / "events.json"), scenario, trace, ledger, fields)]
    if batch:
        problems += check_batch_events(read_json(accepted / "batch_events.json"), accepted, subs)
    record["n_events"] = n_events
    record["host_events_per_sec"] = round(n_events / wall, 1)

    ctx = {"unit_dir": unit, "output_dir": accepted, "card": card_valid, "split": card_valid["task"]["split"]}
    with _verifier_lock:
        verdict = build_developer_verifier(ctx).run(ctx)
    record["verifier"] = {"admissible": verdict.admissible, "labels": [label.value for label in verdict.labels],
                          "gates": {name: gate.passed for name, gate in verdict.gate_results.items()}}
    if not verdict.admissible or verdict.labels:
        problems.append(f"developer verifier: admissible={verdict.admissible} labels={record['verifier']['labels']}")
    return record


def validate_unit(tag: str, unit: Path) -> dict[str, Any]:
    card = tomllib.loads((unit / "card.toml").read_text(encoding="utf-8"))
    row: dict[str, Any] = {"unit": unit.name, "verb": "simulate-batch" if (unit / "batch.json").exists() else "simulate",
                           "ledger_required": bool((unit / "batch.json").exists() or requires_message_ledger(card)),
                           "layouts": {}, "problems": []}
    card_valid, card_errors = taskcard.load_and_validate(unit)
    manifest_errors = unit_manifest.verify_manifest(unit)
    if card_errors or manifest_errors:
        row["problems"].append(f"unit card/manifest invalid: {card_errors + manifest_errors}")
        row["ok"] = False
        return row
    with tempfile.TemporaryDirectory(prefix=f"t3v-{unit.name}-") as tmp:
        for layout in LAYOUTS:
            row["layouts"][layout] = run_layout(tag, unit, card, card_valid, layout, Path(tmp))
    runs = [row["layouts"][layout] for layout in LAYOUTS]
    if all("files" in r for r in runs):
        shared = [p for p in runs[0]["files"] if p.rsplit("/", 1)[-1] in STABLE and p in runs[1]["files"]]
        row["repeat_identical_files"] = len(shared)
        if not shared or any(runs[0]["files"][p]["sha256"] != runs[1]["files"][p]["sha256"] for p in shared):
            row["problems"].append("stable outputs differ between the two runs")
    row["ok"] = not row["problems"] and all(not r["problems"] for r in runs)
    return row


def main() -> int:
    parser = argparse.ArgumentParser(description=__doc__, formatter_class=argparse.RawDescriptionHelpFormatter)
    parser.add_argument("--version", help="release version (default: release.toml)")
    parser.add_argument("--workers", type=int, default=3)
    parser.add_argument("--units", nargs="*", help="only these unit names (writes validation-smoke.json)")
    args = parser.parse_args()
    try:
        release = load_release(args.version)
        directory = release.dir
        image = read_json(directory / "image.json")
        subset = bool(args.units)
        if not subset and (directory / "published.json").exists():
            raise ToolError("this release is published; its validation record is final")
        if metadata.version("qfbench2-common") != TOOLKIT_VERSION:
            raise ToolError(f"packaging/.venv must have qfbench2-common {TOOLKIT_VERSION}; run packaging/tools/setup_env.sh")
        if image_identity(release.local_tag)["id"] != image["image_id"]:
            raise ToolError(f"{release.local_tag} is not the image recorded in releases/{release.version}/image.json")
        roster = units()
        if subset:
            roster = [u for u in roster if u.name in set(args.units)]
            if len(roster) != len(set(args.units)):
                raise ToolError("unknown unit name in --units")
        if not subset:
            (directory / "release-manifest.json").unlink(missing_ok=True)
    except ToolError as error:
        print(f"Validation stopped: {error}", file=sys.stderr)
        return 1

    print(f"Validating {release.local_tag} ({image['manifest_digest'][:19]}…) on {len(roster)} units × {len(LAYOUTS)} layouts, "
          f"{args.workers} at a time", flush=True)
    started, begun = time.monotonic(), now()
    results = []
    with ThreadPoolExecutor(max_workers=args.workers) as pool:
        for i, row in enumerate(pool.map(lambda u: validate_unit(release.local_tag, u), roster), 1):
            results.append(row)
            if not row["ok"]:
                problems = row["problems"] + [f"[{k}] {p}" for k, r in row["layouts"].items() for p in r["problems"]]
                print(f"FAIL {row['unit']}: " + " | ".join(problems)[:1500], flush=True)
            if i % 10 == 0 or i == len(roster):
                print(f"{i}/{len(roster)} units checked, {sum(not r['ok'] for r in results)} failed", flush=True)

    failures = [row["unit"] for row in results if not row["ok"]]
    same_image = image_identity(release.local_tag)["id"] == image["image_id"]
    report = {
        "status": "passed" if not failures and same_image else "failed",
        "version": release.version,
        "image": release.local_tag,
        "image_id": image["image_id"],
        "manifest_digest": image["manifest_digest"],
        "binary_sha256": image["binary"]["sha256"],
        "units": len(results),
        "runs": len(results) * len(LAYOUTS),
        "failures": failures,
        "image_unchanged_during_validation": same_image,
        "unit_roster": {"repo": TRACK_REPO.name, **git_state(TRACK_REPO)},
        "toolkit": {name: metadata.version(name) for name in ("qfbench2-common", "qfbench2-track-simulation", "pandas", "pyarrow")},
        "resources": RESOURCES,
        "caveats": [
            "Local host is Apple silicon: linux/amd64 runs under Docker Desktop emulation, so host timings are not platform timings.",
            "--gpus all (NVIDIA hook) cannot be exercised on this host.",
        ],
        "started": begun,
        "finished": now(),
        "elapsed_seconds": round(time.monotonic() - started, 1),
        "results": results,
    }
    name = "validation-smoke.json" if subset else "validation.json"
    write_json(directory / name, report)
    if not subset and report["status"] == "passed":
        source = read_json(directory / "source.json")
        write_json(directory / "release-manifest.json", {
            "schema_version": "1.0",
            "version": release.version,
            "local_tag": release.local_tag,
            "image_id": image["image_id"],
            "manifest_digest": image["manifest_digest"],
            "binary_sha256": image["binary"]["sha256"],
            "context_sha256": source["context_sha256"],
            "fastsim_git": source["fastsim_git"],
            "validation_sha256": sha256_file(directory / "validation.json"),
            "units": len(results),
            "status": "passed",
            "written": now(),
        })
    print(f"{report['status'].upper()}: {len(results) - len(failures)}/{len(results)} units, "
          f"{report['elapsed_seconds']} s; record: {(directory / name).relative_to(SUB)}", flush=True)
    return 0 if report["status"] == "passed" else 1


if __name__ == "__main__":
    raise SystemExit(main())
