#!/usr/bin/env python3
"""Interleaved whole-process comparison; each run must retain all stable output bytes.

--perf measures hardware counters in a separate pass, not a container timing claim.
Score proxies average median(repeat rates), with an explicitly assumed fixed overhead.
"""
import argparse
import hashlib
import json
import os
from pathlib import Path
import platform
import shutil
import statistics
import subprocess
import tempfile
import time

ap = argparse.ArgumentParser(description=__doc__)
ap.add_argument('--bins', type=Path, required=True)
ap.add_argument('--units', type=Path, required=True)
ap.add_argument('--labels', nargs='+', required=True)
ap.add_argument('--select', nargs='*')
ap.add_argument('--repeats', type=int, default=10)
ap.add_argument('--layout', choices=['final', 'unit'], default='final')
ap.add_argument('--perf', action='store_true')
ap.add_argument('--out', type=Path, required=True)
a = ap.parse_args()
assert a.repeats > 0
records, units = [], []
env = {**os.environ, 'FASTSIM_STREAM': '1'}
exes = {l: (a.bins/l).resolve() for l in a.labels}
source_units = sorted(p for p in a.units.resolve().iterdir() if (p/'card.toml').is_file())
if a.select:
    source_units = [p for p in source_units if p.name in a.select]
    assert len(source_units) == len(a.select)
with tempfile.TemporaryDirectory(prefix='v21-bench-') as td:
    tmp = Path(td)
    for unit in source_units:
        inp = tmp/'input'; inp.mkdir()
        batch = (unit/'batch.json').is_file()
        if a.layout == 'unit':
            shutil.copy2(unit/'card.toml', inp/'card.toml')
        if batch:
            shutil.copytree(unit/'scenarios', inp/'scenarios')
        else:
            shutil.copy2(unit/'scenario.json', inp/'scenario.json')
        expected = None
        for repeat in range(a.repeats+1):
            # Rotate then reverse the order, balancing drift without concurrent runs.
            offset = repeat % len(a.labels)
            order = a.labels[offset:] + a.labels[:offset]
            # Reverse after a full rotation, rather than on every odd repeat: with
            # two labels the latter would undo the rotation and always run A first.
            if (repeat // len(a.labels)) % 2: order = order[::-1]
            for label in order:
                out = tmp/'output'; out.mkdir()
                cmd = [str(exes[label])]
                cmd += (['simulate-batch','--batch-dir',str(inp/'scenarios'),'--out-dir',str(out)] if batch else
                        ['simulate','--config',str(inp/'scenario.json'),'--out',str(out/'trace.parquet')])
                if a.perf:
                    cmd = ['perf','stat','-x,','-e','cycles:u,instructions:u,branch-misses:u,task-clock',
                           '-o',str(tmp/'perf.csv'),'--'] + cmd
                start = time.perf_counter_ns()
                r = subprocess.run(cmd, env=env, stdout=subprocess.DEVNULL, stderr=subprocess.PIPE, timeout=60)
                wall = (time.perf_counter_ns()-start)/1e9
                assert r.returncode == 0, (label, unit.name, r.stderr.decode())
                event = json.loads((out/('batch_events.json' if batch else 'events.json')).read_text())
                n = event.get('n_events', event.get('total_events'))
                stable = {str(p.relative_to(out)):hashlib.sha256(p.read_bytes()).hexdigest()
                          for p in sorted(out.rglob('*.parquet'))}
                if expected is None: expected = (n, stable)
                assert expected == (n, stable), (label, unit.name, repeat, 'stable bytes mismatch')
                if repeat:
                    rec = dict(unit=unit.name, label=label, repeat=repeat, wall_sec=wall, n_events=n,
                               sim_sec=event['wall_clock_sec'])
                    if a.perf:
                        for line in (tmp/'perf.csv').read_text().splitlines():
                            fields = line.split(',')
                            if len(fields)>2 and fields[2] in ('cycles:u','instructions:u','branch-misses:u','task-clock'):
                                rec[fields[2]] = float(fields[0])
                    records.append(rec)
                shutil.rmtree(out)
        summary = dict(unit=unit.name, n_events=expected[0], sha256=expected[1], median={})
        for label in a.labels:
            samples=[r for r in records if r['unit']==unit.name and r['label']==label]
            fields=['wall_sec','sim_sec'] + (['cycles:u','instructions:u','branch-misses:u'] if a.perf else [])
            summary['median'][label]={k:statistics.median(r[k] for r in samples) for k in fields}
        units.append(summary)
        field='cycles:u' if a.perf else 'wall_sec'
        base=summary['median'][a.labels[0]][field]
        print(unit.name, ' '.join(f"{l}={summary['median'][l][field]/base:.4f}" for l in a.labels), flush=True)
        shutil.rmtree(inp)
report = dict(clock='perf whole process counters' if a.perf else 'whole process wall clock',
              layout=a.layout, platform=platform.platform(), repeats=a.repeats, warmup=1,
              order_protocol='rotate each repeat; reverse after each complete label rotation',
              binary_sha256={l:hashlib.sha256(p.read_bytes()).hexdigest() for l,p in exes.items()},
              units=units, runs=records)
if not a.perf:
    report['fixed_overhead_proxy']={}
    for overhead in (0.064,0.125,0.25,0.45):
        scores={l:statistics.mean(statistics.median(r['n_events']/(overhead+r['wall_sec'])
                      for r in records if r['unit']==u.name and r['label']==l) for u in source_units)
                for l in a.labels}
        report['fixed_overhead_proxy'][str(overhead)]=scores
        print('Proxy',overhead,scores, flush=True)
a.out.write_text(json.dumps(report,indent=2)+'\n')
