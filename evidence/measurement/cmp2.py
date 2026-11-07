#!/usr/bin/env python3
"""cmp2.py DIR BASE NEW wallfile... [--perf perffile]: own time, gb4, proxy, cycles/instructions of NEW vs BASE."""
import json, math, statistics as st, sys
N, base, new = sys.argv[1], sys.argv[2], sys.argv[3]
files = sys.argv[4:]
gb = ['t3-gb-mega-throughput', 't3-gb-pop-horizon-scale', 't3-gb-horizon-240s', 't3-gb-highfreq-40hz-60s']
for f in files:
    d = json.load(open(f'{N}/{f}')); units = d['units']; rows = {u['unit']: u['n_events'] for u in units}
    if 'cycles:u' in units[0]['median'][base]:
        gi = math.exp(st.mean(math.log(u['median'][new]['instructions:u'] / u['median'][base]['instructions:u']) for u in units))
        gc = math.exp(st.mean(math.log(u['median'][new]['cycles:u'] / u['median'][base]['cycles:u']) for u in units))
        g4 = sum(u['median'][new]['cycles:u'] for u in units if u['unit'] in gb) / sum(u['median'][base]['cycles:u'] for u in units if u['unit'] in gb)
        print(f"{f:16} perf: instructions {100*(gi-1):+.2f}%  cycles {100*(gc-1):+.2f}% (geomean)  gb4 cycles {100*(g4-1):+.2f}%")
        continue
    med = {l: {u['unit']: u['median'][l]['wall_sec'] * 1000 for u in units} for l in (base, new)}
    p = lambda F: 100 * (st.mean(rows[u] / (F + med[new][u]) for u in rows) / st.mean(rows[u] / (F + med[base][u]) for u in rows) - 1)
    g = math.exp(st.mean(math.log(med[new][u] / med[base][u]) for u in rows))
    print(f"{f:16} own {100*(sum(med[new].values())/sum(med[base].values())-1):+.2f}%  geomean {100*(g-1):+.2f}%  gb4 {100*(sum(med[new][u] for u in gb)/sum(med[base][u] for u in gb)-1):+.2f}% ["
          + ' '.join(f"{100*(med[new][u]/med[base][u]-1):+.1f}" for u in gb) + f"]  proxy F=102 {p(102):+.3f}%  F=150 {p(150):+.3f}%")
