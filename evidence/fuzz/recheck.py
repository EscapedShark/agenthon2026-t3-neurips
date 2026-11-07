"""Recheck a binary against the ABIDES outputs a fuzz run left: recheck.py RUN_DIR BIN [JOBS]

Every case with ref/trace.parquet is simulated again into ours2/ and compared row for row (trace
and ledger), as fuzz_vs_abides.one does. Cases without a reference are skipped."""
import json, subprocess, sys
from concurrent.futures import ProcessPoolExecutor
from pathlib import Path
import pandas as pd


def check(args):
    d, binary = args
    d = Path(d)
    out = d / "ours2"
    out.mkdir(exist_ok=True)
    o = subprocess.run([binary, "simulate", "--config", str(d / "scenario.json"), "--out", str(out / "trace.parquet")],
                       capture_output=True, text=True)
    if o.returncode != 0:
        return d.name, "CRASH", o.stderr[-300:]
    bad = []
    for f in ("trace.parquet", "message_trace.parquet"):
        a, b = pd.read_parquet(out / f), pd.read_parquet(d / "ref" / f)
        if not a.equals(b):
            bad.append(f"{f}: rows {len(a)}/{len(b)}")
    for f in out.iterdir():
        f.unlink()
    out.rmdir()
    return d.name, ("OK" if not bad else "MISMATCH"), "; ".join(bad)


def main():
    run, binary = Path(sys.argv[1]), str(Path(sys.argv[2]).resolve())
    jobs = int(sys.argv[3]) if len(sys.argv) > 3 else 6
    cases = sorted(str(d) for d in run.iterdir() if (d / "ref" / "trace.parquet").is_file())
    counts = {}
    with ProcessPoolExecutor(max_workers=jobs) as ex:
        for name, verdict, detail in ex.map(check, [(c, binary) for c in cases]):
            counts[verdict] = counts.get(verdict, 0) + 1
            if verdict != "OK":
                print(verdict, name, detail, flush=True)
    print(f"{counts} over {len(cases)} cases with a reference in {run.name}")


if __name__ == "__main__":
    main()
