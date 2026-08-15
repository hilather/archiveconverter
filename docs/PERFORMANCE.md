# Performance

Profiling (1M small files, solid→non-solid) shows wall time is dominated by **7z pack
(~75–80%)**, then extract (~15%), then tree cleanup (~5%). Orchestration is secondary.

Published timings: [`docs/bench/RESULTS.md`](bench/RESULTS.md).

## Implemented optimizations

| # | Idea | Status | Notes |
|---|------|--------|-------|
| 1 | Exclude during extract (`-x!`) | **Done** | Maps common regexes (`\.tmp$`, `^__MACOSX/`) to 7z globs; avoids write-then-delete |
| 2 | Stream single outer member (`-so`) | **Done** | No temp-tree+copy for one member |
| 3 | Extract/convert pipeline overlap | **Done** | Prefetch next outer member while converting current (`--no-pipeline-overlap` to disable) |
| 4 | Auto pack threads | **Done** | Many/tiny files → `-mmt=1`; few large → `-mmt=on`. Override with `--threads N` |
| 5 | Passthrough already non-solid | **Done** | Copy nested if `Solid = -` and no filters (`--no-passthrough-nonsolid` to force recompress) |
| 6 | Solid single-pass outer extract | **Done** | One `7z x` of all needed members; default on (`--no-solid-single-pass`) |
| 7 | Nested concurrency | **Done** | Size-aware: workers + `--nested-size-budget` (default 500 M). Bulk-stages sources when workers > 1 |
| 8 | In-process lib7z | **Skipped** | CLI still correct; spawn cost ≪ pack |
| 9 | Cheaper listing | **Done** | Prefer `7z l -ba` over full `-slt` for member lists |
| 10 | Faster cleanup | **Done** | Parallel file delete for large extract trees |
| 11 | Outer tar / dir (no recompress wrap) | **Done** | `--outer-format tar\|dir` — first-layer members only for `dir` |
| 12 | Native solid→non-solid writer | **Done (Phase 3)** | Custom packer + parallel codec |
| 13–14 | Rename/hardlink, no per-nested verify | **Done** | |
| 15 | `--profile` stage timings | **Done** | Info-level stage logs |
| 16 | Long-lived 7z process | **Skipped** | Marginal |
| 17 | Outer append-store (mutex) | **Done** | Nested converts finish → append Copy packs; no final outer recompress |
| 18 | Single-nest pack threads=1 | **Done** | MT LZMA often slower on dense tiny-file nests |
| 19 | Skip corrupt nested | **Done** | Log + continue; missing from output |
| 20 | Skip unexpected members | **Done** | Passthrough extract/type failures, unsafe/duplicate paths; bulk extract falls back per-member |
| 21 | Rsync filter files/rules | **Done** | First-match, dir prune, `--filter-from` / `--exclude-from`; simple excludes still map to `7z -x!` |
| 22 | Manual bench baselines | **Done** | `bench_nested baseline-manual` stores one-at-a-time 7z times at matching `-mmt` |

## CLI knobs

```bash
# Defaults aim at throughput + safe disk for solid nested work
archiveconverter convert in.7z -o out.7z \
  --threads 4 \              # nest workers + pack MT when nests ≥ 2; single nest forces pack=1
  --nested-size-budget 500M \
  --nested-concurrency 0 \   # 0 = auto from --threads / CPUs
  --profile \                # stage timings
  --outer-format 7z \        # or tar | dir
  --no-solid-single-pass \   # A/B outer extract
  --no-passthrough-nonsolid \
  --no-pipeline-overlap

# First layer only (no outer 7z/tar): default dir = <input-stem>/ next to archive
archiveconverter convert path/game.7z --outer-format dir --level 1
```

## Measured notes

- Pure solid **extract** many members: single-pass ~**20×** vs per-member restart.
- Nested convert vs careful serial 7z (one nest at a time, threads=1): ~**1.0–1.01×** on large loads.
- Multi-nest full matrix (1M files/nest): **~2×** at 4 workers vs serial t=1 for 4–10 nests; single nest prefers t=1.
- Non-solid passthrough: near-zero cost when re-converting already converted trees.
- Auto `-mmt=1` on tiny-file archives avoids MT regressions seen in early serial benches.

See [`docs/bench/RESULTS.md`](bench/RESULTS.md) for full tables.

## Phase 1 native backend (`--backend native`)

Uses **sevenz-rust2** in-process:

- `ArchiveReader::for_each_entries` (solid-order decode once)
- `ArchiveWriter::push_archive_entry` (non-solid per file)
- Filters applied while streaming; skipped solid entries are fully drained
- Nested convert uses streaming when `prefer_streaming` is on (native default)

## Phase 2 native pipeline / codec policy

| Feature | Detail |
|---------|--------|
| **Decode-ahead pipeline** | Decode next entry while encoding current (`--native-pipeline ahead` / `ahead:N`) |
| **Size-aware LZMA2 MT** | Entries ≥ `--native-large-threshold` (default 512 KiB) use `Lzma2Options::from_level_mt` |
| **Parallel pack_dir reads** | Rayon pre-reads files when packing a directory |
| **CLI knobs** | `--native-pipeline`, `--native-large-threshold`, `--threads` (encode threads) |

```bash
# Phase 2 native convert (decode-ahead + sevenz-rust2 writer)
archiveconverter convert-single in.7z -o out.7z \
  --backend native --level 1 --threads 4 \
  --native-pipeline ahead:2 --native-large-threshold 524288

# Bake-off
cargo test --release --test phase2_bakeoff -- --nocapture
cargo test --release --test native_backend -- --nocapture
```

**Note:** Phase 2 pure-Rust encode via `ArchiveWriter` is often still behind `7zz` on dense tiny-file packs. Wins more when members are larger (MT encode) and when avoiding full extract trees.

## Phase 3: windowed parallel codec + streaming packer

| Feature | Detail |
|---------|--------|
| **ParallelCodec pipeline** | Default for native: solid-order decode → windowed **rayon** LZMA2 → stream packs |
| **Codec trait** | `pure-rust` (`lzma-rust2`) or `liblzma` (system raw LZMA2 via `lzma-sys`) |
| **Streaming packer** | `NonsolidLzma2Writer`: append each compressed pack as it finishes; header at end |
| **Peak memory** | ≈ **in-flight window** (encode thread count) of uncompressed files + small reorder of compressed packs — **not** the whole archive. |

```bash
# Phase 3 (defaults when --backend native): windowed parallel + liblzma
archiveconverter convert-single in.7z -o out.7z \
  --backend native --level 1 --threads 4 \
  --native-pipeline parallel --native-codec liblzma

# A/B pure Rust codec
archiveconverter convert-single in.7z -o out.7z \
  --backend native --native-pipeline parallel --native-codec pure-rust

# Bake-off (CLI vs decode-ahead vs parallel pure-rust vs parallel liblzma)
cargo test --release --test phase3_bakeoff -- --nocapture
```

**Design:** Decode never races more than `--threads` (encode workers) files ahead. Compress in parallel; **write packs immediately** (reorder buffer holds compressed blobs only until sequence order is ready).

### Measured (release, 8 000 tiny files, solid→non-solid, exclude `.tmp`)

| Engine | Seconds | vs CLI |
|--------|---------|--------|
| CLI 7zz extract+pack | ~1.8 | 1.00× |
| native decode-ahead + MT(4) | ~2.6 | ~1.4× |
| **windowed parallel pure-rust (4)** | **~0.39** | **~0.21×** (~5× faster) |
| **windowed parallel liblzma (4)** | **~0.19** | **~0.11×** (~9× faster) |

Full host table: [`docs/bench/RESULTS.md`](bench/RESULTS.md#phase-3-single-solidnonsolid-bake-off).

## Nested size-aware concurrency

Default when converting an outer with nested 7z members:

| Knob | Default | Meaning |
|------|---------|---------|
| `--nested-concurrency` | `0` (auto) | Max nests in flight (= `--threads` or CPU count) |
| `--nested-size-budget` | `500M` | Max **sum of packed sizes** of nests converting together |

Schedule: sort nests **smallest first**; start the next only if workers free **and** `running_sum + size ≤ budget`. A single nest larger than the budget still runs alone.

**Single nested archive:** always convert with **1 pack/encode thread** (and 1 nest worker), even if `--threads N` is set. Full-scale benches showed multi-thread LZMA often slower on dense tiny-file nests; multi-thread and multi-nest concurrency apply when there are **2+** nests.

```bash
archiveconverter convert outer.7z -o out.7z --threads 4 --nested-size-budget 500M
archiveconverter convert outer.7z -o out.7z --nested-concurrency 1   # force serial nests
archiveconverter convert outer.7z -o out.7z --nested-size-budget 0   # workers only, no size cap
```

## Outer container formats

| Format | Flag / inference | What is written |
|--------|------------------|-----------------|
| **7z** (default) | `--outer-format 7z` or `-o out.7z` | Non-solid outer; members stored (Copy), no recompress |
| **tar** | `--outer-format tar` or `-o out.tar` | Uncompressed tar of first-layer members |
| **dir** | `--outer-format dir` (or path ending in `/`) | First-layer files only; no re-wrap |

**Dir default path:** if `-o` is omitted with `--outer-format dir`, output is  
`<input-dir>/<archive-stem>/` (e.g. `path/game.7z` → `path/game/`).

Nested members inside remain **non-solid `.7z`** after convert; only the *outer* container changes.

## Outer archive append (store / tar / dir)

Finished members are written under a **mutex** (`SyncedOuterWriter`):

1. Nested convert finishes → append/store that `.7z` (or write into tar/dir)  
2. Passthrough files take the same path  
3. 7z end header (or tar trailer / dir finalize) once all producers finish  

Workers never share a bare file handle for archive formats; only one thread appends at a time. Existing nested packs are not recompressed into the outer.

## Still open (future)

- Map more regex exclude patterns to 7z globs.
- Publish full-scale **manual** baselines alongside tool matrix (store via `baseline-manual`, large wall time).

### Design note

Rsync wildcard excludes (`*.tmp`) are intentionally **not** mapped to `7z -x!` globs: 7z `*` does not reliably cross `/`, and a partial map would skip the post-filter pass. Only literal prefixes and exact names take the fast path.
