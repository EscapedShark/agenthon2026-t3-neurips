# Evidence

The records behind the numbers in the README, the method and the poster.

| Path | Content |
|---|---|
| `release-1.26/source.json` | SHA-256 of all 200 build inputs of the Final image |
| `release-1.26/image.json`, `published.json`, `adopted.json` | The image, its digest, and the PGO build chosen as the release |
| `release-1.26/validation.json`, `release-manifest.json` | 71 units in two layouts, 142 runs, with the hash of every output |
| `fuzz/mixed_B_summary_1.25.json` | The campaign that found the bug: 577 identical, 22 ABIDES errors, 1 mismatch |
| `fuzz/recheck.py`, `recheck_1.26.log` | The fixed source against every cached ABIDES reference: 2,274 identical |
| `fuzz/boundary/` | The boundary campaign: 600 scenarios, 557 hits, 0 mismatches at release 1.26 |
| `fuzz/case-0267/` | The failing scenario, and ABIDES's oracle instrumented to show why |
| `same-machine/` | ABIDES against the Final binary on one i7-10700K, 65 scenarios, with the script |
| `final-timing/lab_units_1.26.csv` | Rows and whole-process time per unit, release 1.26, i7-10700K |
| `final-timing/release_gains.csv` | Lab gain per release, with how each was measured |
| `measurement/` | The scripts for native and Final-style container comparisons |

The organizers' floor calibration, with the raw timestamps of 100 runs, is public on
[track3-simulation-public#24](https://github.com/Agenthon-2026/track3-simulation-public/issues/24).

Some scripts here were written for the private workspace and take paths as arguments or from the
environment. In the logs, `<scratch>` stands for a temporary directory.
