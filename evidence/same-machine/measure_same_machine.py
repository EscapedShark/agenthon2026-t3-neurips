#!/usr/bin/env python3
"""Same-machine speed comparison: pinned ABIDES baseline vs fastsim on the 65 single-scenario public units.

The official ABIDES rates in units/*/events.json come from unrecorded hardware, and the organizers
say they are not a same-instance speedup. This script measures both engines on one machine.

  ABIDES : one run per unit. `sim_sec` is the baseline's own wall_clock_sec (the simulation loop
           only, as in the official events.json); `proc_sec` is the whole process.
  fastsim: 11 runs per unit, first dropped, median of whole-process wall time, started with
           posix_spawn and reaped with waitpid, so Python's subprocess cost stays out of the time.

The speedup used on the poster is ABIDES sim_sec / fastsim proc_sec, which favours ABIDES: its
imports, parsing and file writing are left out, while fastsim's are counted.

Usage: measure_same_machine.py WORKSPACE OUT.json [--bin PATH] [--python PATH] [--scratch DIR] [--units a,b,c]
WORKSPACE is the folder holding track3-simulation-public/, and by default .venv-abides and fastsim/.
--bin takes the binary to measure, such as the one extracted from the Final image.
Results are written after every unit, so an interrupted run keeps what it measured.
"""
import argparse, hashlib, json, os, platform, shutil, statistics as st, subprocess, sys, tempfile, time
from pathlib import Path

ap = argparse.ArgumentParser()
ap.add_argument("workspace", type=Path)
ap.add_argument("out", type=Path)
ap.add_argument("--scratch", type=Path, default=None)
ap.add_argument("--units", default="")
ap.add_argument("--repeats", type=int, default=11)
ap.add_argument("--bin", type=Path, default=None, help="fastsim binary (default: WORKSPACE/fastsim/target/release/fastsim)")
ap.add_argument("--python", type=Path, default=None, help="baseline interpreter (default: WORKSPACE/.venv-abides/bin/python)")
a = ap.parse_args()

WS = a.workspace
UNITS = WS / "track3-simulation-public" / "units"
BASELINES = WS / "track3-simulation-public" / "baselines"
PY = a.python or WS / ".venv-abides" / "bin" / "python"
BIN = (a.bin or WS / "fastsim" / "target" / "release" / "fastsim").resolve()


def timed(cmd: list[str], env: dict[str, str]) -> float:
    """Wall time of one run, spawn to reap. Raises if the process fails."""
    actions = [(os.POSIX_SPAWN_OPEN, 1, os.devnull, os.O_WRONLY, 0), (os.POSIX_SPAWN_OPEN, 2, os.devnull, os.O_WRONLY, 0)]
    t0 = time.perf_counter()
    pid = os.posix_spawn(cmd[0], cmd, env, file_actions=actions)
    _, status = os.waitpid(pid, 0)
    wall = time.perf_counter() - t0
    if os.waitstatus_to_exitcode(status):
        raise RuntimeError(f"{cmd[0]} exited {os.waitstatus_to_exitcode(status)}")
    return wall


def cpu_name() -> str:
    if platform.system() == "Linux":
        for line in Path("/proc/cpuinfo").read_text().splitlines():
            if line.startswith("model name"):
                return line.split(":", 1)[1].strip()
    return subprocess.run(["sysctl", "-n", "machdep.cpu.brand_string"], capture_output=True, text=True).stdout.strip()


scratch = Path(tempfile.mkdtemp(prefix="samemachine-", dir=a.scratch))
wanted = [u for u in a.units.split(",") if u]
names = sorted(d.name for d in UNITS.iterdir() if (d / "scenario.json").exists())
if wanted:
    names = [n for n in names if n in wanted]
# Smallest first, so a partial run still covers most units.
ref = {n: json.loads((UNITS / n / "events.json").read_text()) for n in names}
names.sort(key=lambda n: ref[n]["n_events"])

res = {"machine": {"cpu": cpu_name(), "platform": platform.platform(),
                   "fastsim_sha256": hashlib.sha256(BIN.read_bytes()).hexdigest(), "repeats": a.repeats,
                   "started": time.strftime("%Y-%m-%dT%H:%M:%S%z")}, "units": []}
env = {**os.environ, "PYTHONPATH": str(BASELINES)}

for i, n in enumerate(names):
    cfg = UNITS / n / "scenario.json"
    d = scratch / n
    (d / "ab").mkdir(parents=True); (d / "fs").mkdir()
    t0 = time.perf_counter()
    r = subprocess.run([str(PY), "-m", "abides_fork.simulate", "--config", str(cfg), "--out", str(d / "ab" / "trace.parquet")],
                       env=env, capture_output=True, text=True, cwd=d)
    ab_proc = time.perf_counter() - t0
    if r.returncode:
        print("ABIDES failed", n, r.stderr[-300:], flush=True); shutil.rmtree(d); continue
    ab = json.loads((d / "ab" / "events.json").read_text())
    # FASTSIM_STREAM=1 as in the Final image.
    fs_env = {**os.environ, "FASTSIM_STREAM": "1"}
    walls = [timed([str(BIN), "simulate", "--config", str(cfg), "--out", str(d / "fs" / "trace.parquet")], fs_env)
             for _ in range(a.repeats)][1:]
    fs = json.loads((d / "fs" / "events.json").read_text())
    assert fs["n_events"] == ab["n_events"], n
    rec = {"unit": n, "rows": ab["n_events"], "abides_sim_sec": ab["wall_clock_sec"], "abides_proc_sec": ab_proc,
           "fastsim_proc_sec": st.median(walls), "reference_sim_sec": ref[n]["wall_clock_sec"]}
    res["units"].append(rec)
    a.out.write_text(json.dumps(res, indent=1))
    print(f"{i+1}/{len(names)} {n} rows {rec['rows']} abides {rec['abides_sim_sec']:.2f}s fastsim {rec['fastsim_proc_sec']*1000:.2f}ms x{rec['abides_sim_sec']/rec['fastsim_proc_sec']:.0f}", flush=True)
    shutil.rmtree(d)
res["machine"]["finished"] = time.strftime("%Y-%m-%dT%H:%M:%S%z")
a.out.write_text(json.dumps(res, indent=1))
shutil.rmtree(scratch, ignore_errors=True)
print("done", len(res["units"]), flush=True)
