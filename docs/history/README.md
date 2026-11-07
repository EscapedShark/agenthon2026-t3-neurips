# Optimization notes

The notes written during the competition, one per round. They hold the measurements, the changes,
what was tried and dropped, and how each release was verified. [../RELEASES.md](../RELEASES.md) is
the index by release.

| File | Round |
|---|---|
| `PERFORMANCE.md`, `PERFORMANCE_HOTPATHS.md`, `PERFORMANCE_OUTPUT.md` | v1 |
| `PERFORMANCE_PIPELINE.md` | v2 |
| `PERFORMANCE_V3.md` to `PERFORMANCE_V10.md` | v3 to v10 |
| `PERFORMANCE_V14.md` to `PERFORMANCE_V27.md` | v14 to v27 |

They are records, so they are kept as written, with two exceptions: V21 was translated into English,
and local paths were removed. Some describe rules that changed later, such as the gVisor runtime of
the early Final. Paths such as `benchmarks/...`, `sub/...` and `releases/...` refer to folders of the
private workspace. `sub/` is now `packaging/`, `benchmarks/` is not included, and the records for the
Final release are in `evidence/release-1.26/`. `<x86-lab>` stands for the x86 measurement machine.

One known error: `PERFORMANCE_V27.md` says release 1.26 passes 46 tests. The source has 45.
