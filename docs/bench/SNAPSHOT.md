# Benchmark index

Canonical published numbers: **[`RESULTS.md`](RESULTS.md)**  
Full nested matrix CSV: **[`full-results.csv`](full-results.csv)**

## Host (results host)

| | |
|--|--|
| CPUs | 12 logical |
| Model | Intel Core i7-8750H @ 2.20 GHz |
| Storage | NVMe, Linux |

## What’s measured

| Suite | What | Where |
|-------|------|--------|
| Full nested matrix | 1M tiny files/nest × 1/2/4/10 nests × threads 1/2/4 | [RESULTS.md § Full](RESULTS.md#full-nested-matrix-latest) |
| Large tool vs manual | 280k×3 nests, one-at-a-time, threads=1 | [RESULTS.md § Large](RESULTS.md#large-tool-vs-manual-7z-one-at-a-time) |
| Phase 3 bake-off | 8k tiny solid→non-solid codecs | [RESULTS.md § Phase 3](RESULTS.md#phase-3-single-solidnonsolid-bake-off) |
| Tiny + manual baseline | smoke + concurrent-nest advantage | [RESULTS.md § Tiny](RESULTS.md#tiny-scale-smoke--manual-baseline-side-by-side) |

## Local (gitignored) artifacts

```text
benchdata/<scale>/                 fixtures + logs
benchdata/<scale>/results/         raw CSV/MD, output archives
benchdata/<scale>/results/manual_baseline.json   optional stored manual 7z times
```

Do not commit fixtures or multi‑hundred‑MB `out-*.7z` files. Re-run benches and update `RESULTS.md` / `full-results.csv` when numbers change meaningfully.

## Related docs

- [`../PERFORMANCE.md`](../PERFORMANCE.md) — knobs and implementation status  
- [`../../README.md`](../../README.md) — usage + summary table  
