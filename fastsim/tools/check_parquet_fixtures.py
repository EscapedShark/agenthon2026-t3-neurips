#!/usr/bin/env python3
"""Verify tests/parquet_roundtrip.rs fixtures with pyarrow/pandas, the scorer's reader.

    FASTSIM_PARQUET_FIXTURES=DIR cargo test --release --test parquet_roundtrip
    .venv/bin/python tools/check_parquet_fixtures.py DIR
"""
import json
import sys
from pathlib import Path

import pandas as pd
import pyarrow.parquet as pq

INT64_NULLABLE = {"t_send_ns", "order_id", "causal_parent"}
bad = 0
files = sorted(Path(sys.argv[1]).glob("*.parquet"))
for f in files:
    want = json.loads(f.with_suffix(".json").read_text())
    df = pd.read_parquet(f)
    pf = pq.ParquetFile(f)
    problems = []
    if pf.metadata.num_rows != len(df):
        problems.append("footer row count")
    for col, values in want.items():
        got = df[col]
        if f.name.startswith("ledger") and col in INT64_NULLABLE and str(got.dtype) != "Int64":
            problems.append(f"{col} dtype {got.dtype}")
        got_list = [None if pd.isna(v) else (int(v) if not isinstance(v, str) else v) for v in got.tolist()]
        if got_list != values:
            i = next(i for i, (a, b) in enumerate(zip(got_list, values)) if a != b) if len(got_list) == len(values) else -1
            problems.append(f"{col} differs (first at {i})")
    if "seq" in df.columns and df["seq"].tolist() != list(range(len(df))):
        problems.append("seq")
    bad += bool(problems)
    print(f"{'OK  ' if not problems else 'FAIL'} {f.name:48s} rows {len(df):6d} groups {pf.metadata.num_row_groups} {problems}")
print(f"{len(files) - bad}/{len(files)} fixture files read back exactly by pyarrow {__import__('pyarrow').__version__}")
sys.exit(1 if bad or not files else 0)
