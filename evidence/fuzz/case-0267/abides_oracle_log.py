"""Run the pinned ABIDES baseline on one scenario with the oracle instrumented.

Logs every megashock (time, value, the time it was drawn from) and every observe_price call
(time, agent's r_t, observation) to a JSON-lines file."""
import json, sys
import numpy as np
from abides_markets.oracles import sparse_mean_reverting_oracle as smro
from abides_fork import simulate as sim

cfg, out, log = sys.argv[1], sys.argv[2], sys.argv[3]
fh = open(log, "w")
O = smro.SparseMeanRevertingOracle
orig_compute = O.compute_fundamental_at_timestamp
orig_observe = O.observe_price

def compute(self, ts, v_adj, symbol, pt, pv):
    v = orig_compute(self, ts, v_adj, symbol, pt, pv)
    fh.write(json.dumps({"k": "c", "ts": repr(ts), "tst": type(ts).__name__, "pt": repr(pt), "ptt": type(pt).__name__, "pv": repr(pv), "adj": repr(v_adj), "v": repr(v)}) + "\n")
    return v

def observe(self, symbol, current_time, random_state, sigma_n=1000):
    obs = orig_observe(self, symbol, current_time, random_state, sigma_n)
    ms = self.megashocks[symbol][-1]
    fh.write(json.dumps({"k": "o", "t": repr(current_time), "r": repr(self.r[symbol][1]), "obs": repr(obs), "sn": repr(sigma_n),
                         "next_ms": repr(ms["MegashockTime"]), "next_msv": repr(ms["MegashockValue"])}) + "\n")
    return obs

O.compute_fundamental_at_timestamp = compute
O.observe_price = observe
sim.simulate(cfg, out)
fh.close()
