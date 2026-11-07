#!/usr/bin/env python3
"""Final-style container race in the real Final input layout (card mounted).

Each unit's input folder holds what the official runner mounts (track3#35, 10-09): the scenario
input plus card.toml, never the reference outputs. Containers start as the official runner starts
them (--network=none --cpus=4 --memory=16g, GPU, -e NVIDIA_DRIVER_CAPABILITIES=compute); the GPU is
injected through CDI by default, as Docker 29.7.2 does on the platform.

Every label runs one warm-up and then --repeats counted runs per unit; labels are interleaved and
their order rotates, so host drift falls on all of them. The clock is Docker's State.StartedAt ->
State.FinishedAt, as in the Final; create/start/wait/remove API times are kept as a Development
proxy. Stable output bytes and the event count are compared on every run, warm-up included.
"""
import argparse
import datetime
import hashlib
import http.client
import json
import platform
import shutil
import socket
import statistics
import tempfile
import time
from pathlib import Path


class Docker(http.client.HTTPConnection):
    def __init__(self):
        super().__init__('localhost', timeout=310)

    def connect(self):
        self.sock = socket.socket(socket.AF_UNIX, socket.SOCK_STREAM)
        self.sock.settimeout(self.timeout)
        self.sock.connect('/var/run/docker.sock')

    def api(self, method, path, body=None):
        start = time.perf_counter_ns()
        self.request(method, '/v1.51' + path, body=None if body is None else json.dumps(body).encode(),
                     headers={'Content-Type': 'application/json'})
        r = self.getresponse()
        data = r.read()
        sec = (time.perf_counter_ns() - start) / 1e9
        if r.status >= 300:
            raise RuntimeError((method, path, r.status, data.decode()))
        return (json.loads(data) if data else None), sec


def stamp(s):
    date, _, frac = s.rstrip('Z').partition('.')
    d = datetime.datetime.strptime(date, '%Y-%m-%dT%H:%M:%S').replace(tzinfo=datetime.timezone.utc)
    return int(d.timestamp()) * 10**9 + int(frac.ljust(9, '0')[:9])


GPU = {
    'cdi': [{'Driver': 'cdi', 'DeviceIDs': ['nvidia.com/gpu=all']}],
    'legacy': [{'Driver': '', 'Count': -1, 'Capabilities': [['gpu']]}],
    'none': [],
}

ap = argparse.ArgumentParser(description=__doc__)
ap.add_argument('--units', type=Path, required=True)
ap.add_argument('--images', nargs='+', required=True,
                help='labels IMAGE[@card|@staged]; the layout defaults to --layout')
ap.add_argument('--select', nargs='*')
ap.add_argument('--repeats', type=int, default=4, help='counted runs per label and unit (after one warm-up)')
ap.add_argument('--layout', choices=['card', 'staged'], default='card')
ap.add_argument('--gpu', choices=sorted(GPU), default='cdi')
ap.add_argument('--cross-check', action='store_true',
                help='require byte-identical stable outputs across labels too (same-output candidates)')
ap.add_argument('--out', type=Path, required=True)
a = ap.parse_args()
assert a.repeats > 0
client = Docker()
version, _ = client.api('GET', '/version')
info, _ = client.api('GET', '/info')
def split(label):
    image, _, layout = label.partition('@')
    return image, layout or a.layout


images = {name: client.api('GET', '/images/' + split(name)[0] + '/json')[0]['Id'] for name in a.images}
units = sorted(p for p in a.units.resolve().iterdir() if (p / 'card.toml').is_file())
if a.select:
    units = [p for p in units if p.name in a.select]
    assert len(units) == len(a.select), 'unknown unit in --select'
runs, checks = [], {}
host = None
with tempfile.TemporaryDirectory(prefix='finalrace-') as td:
    tmp = Path(td)
    for unit in units:
        batch = (unit / 'scenarios').is_dir()
        inputs = {}
        for layout in sorted({split(n)[1] for n in a.images}):
            inp = inputs[layout] = tmp / ('input-' + layout)
            inp.mkdir(mode=0o755)
            if batch:
                shutil.copytree(unit / 'scenarios', inp / 'scenarios')
            else:
                shutil.copy2(unit / 'scenario.json', inp / 'scenario.json')
            if layout == 'card':
                shutil.copy2(unit / 'card.toml', inp / 'card.toml')
        for repeat in range(a.repeats + 1):
            k = repeat % len(a.images)
            order = a.images[k:] + a.images[:k]
            if (repeat // len(a.images)) % 2:
                order = order[::-1]
            for name in order:
                inp = inputs[split(name)[1]]
                out = tmp / 'output'
                out.mkdir()
                out.chmod(0o777)
                if batch:
                    # Writable by this host user for cleanup; every label sees the same dirs.
                    for sub in (inp / 'scenarios').glob('*.json'):
                        d = out / sub.stem
                        d.mkdir()
                        d.chmod(0o777)
                verb = (['simulate-batch', '--batch-dir', '/input/scenarios', '--out-dir', '/output'] if batch else
                        ['simulate', '--config', '/input/scenario.json', '--out', '/output/trace.parquet'])
                host = {'NetworkMode': 'none', 'NanoCpus': 4_000_000_000, 'Memory': 16 << 30,
                        'DeviceRequests': GPU[a.gpu],
                        'Binds': [f'{inp}:/input:ro', f'{out}:/output:rw']}
                config = {'Image': images[name], 'Cmd': verb,
                          'Env': ['NVIDIA_DRIVER_CAPABILITIES=compute'], 'HostConfig': host}
                created, create_sec = client.api('POST', '/containers/create', config)
                cid = created['Id']
                try:
                    _, start_sec = client.api('POST', f'/containers/{cid}/start')
                    status, wait_sec = client.api('POST', f'/containers/{cid}/wait?condition=not-running')
                    state, _ = client.api('GET', f'/containers/{cid}/json')
                    assert status['StatusCode'] == 0, (unit.name, name, status)
                finally:
                    _, remove_sec = client.api('DELETE', f'/containers/{cid}?force=1&v=1')
                event = json.loads((out / ('batch_events.json' if batch else 'events.json')).read_text())
                rows = event.get('n_events', event.get('total_events'))
                digest = {str(p.relative_to(out)): hashlib.sha256(p.read_bytes()).hexdigest()
                          for p in sorted(out.rglob('*.parquet'))}
                key = unit.name if a.cross_check else (unit.name, name)
                current = {'n_events': rows, 'sha256': digest}
                checks.setdefault(str(key), current)
                assert checks[str(key)] == current, (unit.name, name, repeat, 'stable output mismatch')
                window = (stamp(state['State']['FinishedAt']) - stamp(state['State']['StartedAt'])) / 1e9
                assert window > 0
                if repeat:
                    runs.append(dict(unit=unit.name, image=name, repeat=repeat, n_events=rows,
                                     window_sec=window,
                                     lifecycle_sec=create_sec + start_sec + wait_sec + remove_sec,
                                     sim_sec=event['wall_clock_sec'], files=sorted(digest)))
                shutil.rmtree(out)
        med = {name: statistics.median(r['window_sec'] for r in runs if r['unit'] == unit.name and r['image'] == name)
               for name in a.images}
        print(unit.name, ' '.join(f'{n}={1000 * t:.2f}ms' for n, t in med.items()), flush=True)
        for inp in inputs.values():
            shutil.rmtree(inp)


def score(field):
    return {name: statistics.mean(statistics.median(r['n_events'] / r[field] for r in runs
                                                    if r['unit'] == u.name and r['image'] == name)
                                  for u in units) for name in a.images}


scores, dev_scores = score('window_sec'), score('lifecycle_sec')
report = {'clock': 'Docker State.StartedAt -> State.FinishedAt (lifecycle_sec: create+start+wait+remove API)',
          'layout': a.layout, 'gpu': a.gpu, 'repeats': a.repeats, 'warmup': 1,
          'platform': platform.platform(), 'images': images, 'docker': version.get('Version'),
          'runtime': info.get('DefaultRuntime'), 'ncpu': info.get('NCPU'), 'host_config': host,
          'checks': checks, 'runs': runs, 'final_style_scores': scores, 'dev_style_scores': dev_scores}
a.out.write_text(json.dumps(report, indent=2) + '\n')
base = a.images[0]
print('Final-style (median of counted windows, mean over units):',
      {n: f'{s:,.0f} ({100 * (s / scores[base] - 1):+.2f}%)' for n, s in scores.items()}, flush=True)
print('Dev-style (create..remove):',
      {n: f'{s:,.0f} ({100 * (s / dev_scores[base] - 1):+.2f}%)' for n, s in dev_scores.items()}, flush=True)
