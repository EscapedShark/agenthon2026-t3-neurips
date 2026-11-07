"""hashrun.py BIN CASES_DIR OUT_JSON [JOBS]: run BIN on every CASES_DIR/*/scenario.json into a temp dir
and record the sha256 of trace.parquet and message_trace.parquet per case."""
import hashlib, json, os, subprocess, sys, tempfile
from concurrent.futures import ThreadPoolExecutor
from pathlib import Path

binary, cases, out = sys.argv[1], Path(sys.argv[2]), Path(sys.argv[3])
jobs = int(sys.argv[4]) if len(sys.argv) > 4 else 8

def one(d):
    with tempfile.TemporaryDirectory() as t:
        r = subprocess.run([binary, "simulate", "--config", str(d / "scenario.json"), "--out", f"{t}/trace.parquet"],
                           capture_output=True, text=True)
        if r.returncode != 0:
            return d.name, {"error": r.stderr[-200:]}
        return d.name, {f: hashlib.sha256(Path(t, f).read_bytes()).hexdigest() for f in ("trace.parquet", "message_trace.parquet")}

dirs = sorted(p for p in cases.iterdir() if (p / "scenario.json").is_file())
with ThreadPoolExecutor(jobs) as ex:
    res = dict(ex.map(one, dirs))
out.write_text(json.dumps(res, indent=0, sort_keys=True))
print(len(res), "cases,", sum("error" in v for v in res.values()), "errors")
