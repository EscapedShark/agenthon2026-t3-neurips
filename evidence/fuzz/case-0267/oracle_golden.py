"""Golden values from ABIDES's SparseMeanRevertingOracle at megashock-time boundaries.

For each case: seed the global generator, build the oracle the way the baseline does (mkt_open an
np.int64, the oracle's own RandomState), then observe (sigma_n = 0) just after each of the first
megashocks: +90 ns (the time rounds down to the megashock's double, so NumPy does not apply it yet),
+128 ns (a tie, rounded to even) and +300 ns (applied). Prints the times and the fundamentals."""
import json
import numpy as np
from abides_markets.oracles.sparse_mean_reverting_oracle import SparseMeanRevertingOracle

MKT_OPEN = np.int64(1612517400000000000)
CLOSE = MKT_OPEN + np.int64(23400 * 10**9)

def case(global_seed, oracle_seed, rate, kappa, jump=None, first=(90, 128, 300)):
    np.random.seed(global_seed)
    sym = {"r_bar": 50000, "kappa": kappa, "sigma_s": 0, "fund_vol": 5e-4,
           "megashock_lambda_a": rate, "megashock_mean": 1000.0, "megashock_var": 50000,
           "random_state": np.random.RandomState(seed=oracle_seed),
           "scheduled_jumps": [] if jump is None else [{"time_ns": jump[0], "magnitude": jump[1], "_consumed": False}]}
    o = SparseMeanRevertingOracle(MKT_OPEN, CLOSE, {"ABM": sym})
    times, values = [], []
    for k in range(4):
        mst = o.megashocks["ABM"][-1]["MegashockTime"]
        for off in (first if k == 0 else (90, 128, 300)):
            t = int(mst) + off
            times.append(t)
            values.append(int(o.observe_price("ABM", t, None, sigma_n=0)))
    return {"times": times, "values": values, "msts": [repr(m["MegashockTime"]) for m in o.megashocks["ABM"]]}

out = {
    "jumps": case(7, 11, 2e-9, 1.67e-16),
    "reverting": case(123, 456, 4e-9, 0.5e-9),
}
# A scheduled jump 64 ns after the first megashock of the first case: NumPy compares the float
# megashock time with the int jump time as doubles, so the jump applies at the megashock.
np.random.seed(7)
mst0 = MKT_OPEN + np.random.exponential(scale=1.0 / 2e-9)
# A strong reversion (kappa 1e-2 per ns) makes the order of jump and OU step visible.
out["scheduled"] = case(7, 11, 2e-9, 1e-2, jump=(int(mst0) + 64, -300), first=(300,))
out["scheduled_mst"] = repr(mst0)
print(json.dumps(out, indent=1))
