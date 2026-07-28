# Performance

Profiling (1M small files, solid→non-solid) shows wall time is dominated by **7z pack
(~75–80%)**, then extract (~15%), then tree cleanup (~5%). Orchestration is secondary.

## Implemented optimizations

| # | Idea | Status | Notes |
|---|------|--------|-------|
| 1 | Exclude during extract (`-x!`) | **Done** | Maps common regexes (`\.tmp$`, `^__MACOSX/`) to 7z globs; avoids write-then-delete |
| 2 | Stream single outer member (`-so`) | **Done** | No temp-tree+copy for one member |
| 3 | Extract/convert pipeline overlap | **Done** | Prefetch next outer member while converting current (`--no-pipeline-overlap` to disable) |
| 4 | Auto pack threads | **Done** | Many/tiny files → `-mmt=1`; few large → `-mmt=on`. Override with `--threads N` |
| 5 | Passthrough already non-solid | **Done** | Copy nested if `Solid = -` and no filters (`--no-passthrough-nonsolid` to force recompress) |
| 6 | Solid single-pass outer extract | **Done** | One `7z x` of all needed members; default on (`--no-solid-single-pass`) |
| 7 | Nested concurrency | **Done** | `--nested-concurrency K` (disk ↑). Bulk-stages sources first when K>1 |
| 8 | In-process lib7z | **Skipped** | CLI still correct; spawn cost << pack |
| 9 | Cheaper listing | **Done** | Prefer `7z l -ba` over full `-slt` for member lists |
| 10 | Faster cleanup | **Done** | Parallel file delete for large extract trees |
| 11 | tar-then-7z mode | **Skipped** | Changes semantics |
| 12 | Native solid→non-solid writer | **Done (Phase 3)** | Custom packer + parallel codec |
| 13–14 | Rename/hardlink, no per-nested verify | **Done** | |
| 15 | `--profile` stage timings | **Done** | Info-level stage logs |
| 16 | Long-lived 7z process | **Skipped** | Marginal |

## CLI knobs

```bash
# Defaults aim at throughput + safe disk for solid nested work
archiveconverter convert in.7z -o out.7z \
  --threads 1 \              # optional pin; omit for auto
  --nested-concurrency 2 \   # parallel nested converts (more disk)
  --profile \                # stage timings
  --no-solid-single-pass \   # A/B outer extract
  --no-passthrough-nonsolid \
  --no-pipeline-overlap
```

## Measured notes

- Pure solid **extract** 40 members: single-pass ~**20×** vs per-member restart.
- Full nested convert: often ~**1.0–1.1×** vs careful serial 7z (pack dominates).
- Non-solid passthrough: near-zero cost when re-converting already converted trees.
- Auto `-mmt=1` on tiny-file archives avoids MT regressions seen in benches.

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
| **Peak memory** | ≈ **in-flight window** (encode thread count) of uncompressed files + small reorder of compressed packs — **not** the whole archive. One huge file = that file (yolo). |

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

**Design:** Decode never races more than `--threads` (encode workers) files ahead. Compress in parallel; **write packs immediately** (reorder buffer holds compressed blobs only until sequence order is ready). Same speed idea as “decode all then par compress,” production-safe RAM.

### Measured (release, 8 000 tiny files, solid→non-solid, exclude `.tmp`)

Windowed pipeline (not full-archive RAM):

| Engine | Seconds | vs CLI |
|--------|---------|--------|
| CLI 7zz extract+pack | 1.60 | 1.00× |
| native decode-ahead + MT(4) | 2.29 | 1.43× |
| **windowed parallel pure-rust (4)** | **0.32** | **0.20×** (~5× faster) |
| **windowed parallel liblzma (4)** | **0.16** | **0.10×** (~10× faster) |

Numbers from `cargo test --release --test phase3_bakeoff -- --nocapture` on this host; absolute times vary by CPU.

## Nested size-aware concurrency

Default when converting an outer with nested 7z members:

| Knob | Default | Meaning |
|------|---------|---------|
| `--nested-concurrency` | `0` (auto) | Max nests in flight (= `--threads` or CPU count) |
| `--nested-size-budget` | `500M` | Max **sum of packed sizes** of nests converting together |

Schedule: sort nests **smallest first**; start the next only if workers free **and** `running_sum + size ≤ budget`. A single nest larger than the budget still runs alone.

**Single nested archive:** always convert with **1 pack/encode thread** (and 1 nest worker), even if `--threads N` is set. Full-scale benches showed multi-thread LZMA often slower on dense tiny-file nests; multi-thread still applies when there are **2+** nests (and to size-aware nest concurrency).

Example: budget `500M`, 5 threads, five ~100 MB nests → all five concurrent.

```bash
archiveconverter convert outer.7z -o out.7z --threads 4 --nested-size-budget 500M
archiveconverter convert outer.7z -o out.7z --nested-concurrency 1   # force serial
archiveconverter convert outer.7z -o out.7z --nested-size-budget 0   # workers only, no size cap
```

## Outer archive append (store, no recompress)

The outer non-solid archive is built by **appending** finished members (Copy method) under a
**mutex** (`SyncedOuterWriter`):

1. Nested convert finishes → stream that `.7z` into the outer pack section  
2. Passthrough files are appended the same way  
3. End header written once when all producers finish  

Workers never share a bare file handle; only one thread appends at a time. Existing packs are
not recompressed. Tests: `codec::store_writer` (roundtrip + concurrent append).
## Still open (future)

- Map more regex exclude patterns to 7z globs.
