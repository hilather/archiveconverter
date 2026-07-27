# Benchmark snapshot 2026-07-27T21:29Z

## Host
12
CPU(s):                                  12
Model name:                              Intel(R) Core(TM) i7-8750H CPU @ 2.20GHz
NUMA node0 CPU(s):                       0-11

## Phase 3 single solid→non-solid (8000 tiny files)
    Finished `release` profile [optimized] target(s) in 0.10s
     Running tests/phase3_bakeoff.rs (target/release/deps/phase3_bakeoff-f54b9987a8b8401b)

running 1 test

=== Phase 3 bake-off (8000 files, solid→non-solid, exclude .tmp) ===
engine                                seconds      vs CLI
CLI 7zz extract+pack                    1.823      1.00x
native decode-ahead + MT(4)             2.596       1.42x
parallel-codec pure-rust (4)            0.391       0.21x
parallel-codec liblzma (4)              0.193       0.11x
================================================================

test bakeoff_phase3_codecs_vs_cli ... ok

test result: ok. 1 passed; 0 failed; 0 ignored; 0 measured; 0 filtered out; finished in 6.74s


## Large nested tool vs manual (280k files × 3 nests, CLI backend, threads=1)
tool: 188.0s  manual: 185.4s  ratio: 1.01x  (from prior run same session)

## Full nested matrix status
2026-07-27T21:29:41.835365Z  INFO archiveconverter::pipeline: bulk extract of needed outer members (single-pass / pre-stage for concurrency) members=2 solid=true max_workers=2 size_budget=524288000
2026-07-27T21:29:41.858105Z  INFO archiveconverter::pipeline: passthrough outer member path=README.txt dest=README.txt
2026-07-27T21:29:41.858118Z  INFO archiveconverter::pipeline: nested convert schedule (smallest-first, size-aware) nested=1 max_workers=1 size_budget=524288000 smallest=11370875 largest=11370875
2026-07-27T21:29:41.858121Z  INFO archiveconverter::pipeline: converting nested 7z path=nested-00.7z dest=nested-00.7z index=0 size=11370875
2026-07-27T21:29:46.156313Z  INFO archiveconverter::convert::sevenz_nonsolid: converting 7z to non-solid input=/home/mbrewer/projects/archiveconverter/benchdata/full/tmp/archiveconverter-M7YIoN/outer-solid-pass/nested-00.7z solid=true use_7z_excludes=false
    ELAPSED CMD
      04:03 ./target/release/bench_nested run --scale full --threads 1,2,4 --level 1
