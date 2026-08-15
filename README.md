# archiveconverter

> **Nested solid 7z → non-solid.** Extract each nested solid archive and repack it without a solid block, so members are individually addressable — while keeping the outer bundle as `7z`, `tar`, or a plain directory.

Solid 7z (`-ms=on`) packs many files into one compressed stream. Nested solid archives make random access and remounting expensive: open one file and the whole nest has to decode. archiveconverter walks an outer archive, converts each nested solid `.7z` to **non-solid** (`-ms=off`), and writes the first layer back out. Bad or unexpected members are skipped with a warning so the rest of the archive still completes.

```bash
archiveconverter convert outer.7z -o out.7z \
  --exclude-outer '^skip_me\.7z$' \
  --exclude-inner '(?i)\.tmp$' \
  --rename '_old\.7z$=.7z' \
  --threads 4 --level 1 --verify
```

| | |
|--|--|
| **Formats** | Outer: `7z` (default) · `tar` · `dir` |
| **Engine** | Official 7-Zip CLI (`7zz`), or an in-process native backend |
| **Filters** | Regex **and** rsync rules (`--filter`, `--exclude-from`, first-match, dir prune) |
| **Concurrency** | Size-aware nested workers (default 500 MiB in flight) |
| **Metadata** | Member mtimes and Windows attributes preserved |
| **License** | MIT |

Full benchmarks: [`docs/bench/RESULTS.md`](docs/bench/RESULTS.md) · performance design: [`docs/PERFORMANCE.md`](docs/PERFORMANCE.md)

---

## Install

You need **Rust** and a **7-Zip CLI** for the default backend.

```bash
# Linux: official 7zz (user-local)
mkdir -p ~/.local/bin
curl -fsSL -o /tmp/7z.tar.xz https://www.7-zip.org/a/7z2501-linux-x64.tar.xz
tar -xOf /tmp/7z.tar.xz 7zz > ~/.local/bin/7zz
chmod +x ~/.local/bin/7zz
export PATH="$HOME/.local/bin:$PATH"

# Build
cargo build --release
# binaries: target/release/archiveconverter  target/release/bench_nested
```

## Quick start

```bash
# Inspect backends
archiveconverter backend
archiveconverter list-converters

# Preview the plan without writing
archiveconverter convert outer.7z -o out.7z --dry-run

# Convert nested archives to non-solid, outer stays 7z
archiveconverter convert outer.7z -o out.7z --threads 4 --level 1 --verify

# Same filters as rsync files / rules (first match wins, dirs prune children)
archiveconverter convert outer.7z -o out.7z \
  --exclude-from-outer skip.excludes \
  --filter-inner 'exclude *.tmp' \
  --filter-inner 'exclude __MACOSX/'

# Outer as uncompressed tar (extension alone also selects tar)
archiveconverter convert outer.7z -o out.tar --level 1

# First layer only, no outer archive; default dir = archive stem
#   path/game.7z  →  path/game/
archiveconverter convert path/game.7z --outer-format dir --level 1 --verify

# Single archive (no nesting)
archiveconverter convert-single solid.7z -o nonsolid.7z --exclude '\.tmp$' --verify

# Native engine (fast on dense tiny-file nests)
archiveconverter convert-single solid.7z -o out.7z \
  --backend native --threads 4 --level 1 \
  --native-pipeline parallel --native-codec liblzma
```

---

## CLI reference

### Commands

| Command | Purpose |
|---------|---------|
| `convert` | Outer archive with nested 7z members |
| `convert-single` | One 7z solid→non-solid (no outer nesting) |
| `backend` | Print CLI / native backend info |
| `list-converters` | Registered converters |

### `convert` options

| Flag | Default | Meaning |
|------|---------|---------|
| `-o`, `--output` | required for 7z/tar; optional for `dir` | Output file or directory |
| `--outer-format` | inferred | `7z` \| `tar` \| `dir`. Omit: `.tar` → tar; path ends with `/` → dir; else 7z |
| `--exclude-inner` / `--exclude-outer` | — | Regex exclude (repeatable); appended after rsync rules |
| `--include-inner` / `--include-outer` | — | Regex include (repeatable); first-match with excludes |
| `--filter-inner` / `--filter-outer` | — | Rsync rule: `+ pat`, `exclude pat`, or bare exclude. A leading `-` needs `--filter-inner='- *.tmp'` |
| `--filter-from-inner` / `--filter-from-outer` | — | Rsync filter file (`#` comments, `merge`, `clear`) |
| `--include-from-*` / `--exclude-from-*` | — | Rsync include-from / exclude-from (one pattern per line) |
| `--rename` | — | `PATTERN=REPL` on outer names (ordered; `$1` / `$name`) |
| `--basename-match` | off | Regex excludes/includes match basename only (rsync keeps its own `/` rules) |
| `--level` | `5` | Nested pack level 0–9 |
| `--threads` | auto | Nest workers + pack MT when nests ≥ 2; single nest forces pack = 1 |
| `--nested-concurrency` | `0` (auto) | Max nests converting at once |
| `--nested-size-budget` | `500M` | Max sum of packed nest sizes in flight (`0` = no size cap) |
| `--backend` | `cli` | `cli` \| `native` |
| `--native-pipeline` | `parallel` | `parallel` \| `ahead[:N]` \| `sequential` |
| `--native-codec` | `liblzma` | `liblzma` \| `pure-rust` (native) |
| `--native-large-threshold` | 512 KiB | Size for MT LZMA2 on native encode |
| `--verify` | off | Count/test output (7z / tar / dir) |
| `--dry-run` | off | Print plan only |
| `--temp-dir` / `--keep-temp` | system temp | Control workspace |
| `--profile` | off | Stage timings at info |
| `--no-solid-single-pass` | off | Disable bulk outer extract |
| `--no-passthrough-nonsolid` | off | Always recompress non-solid nests |
| `--no-pipeline-overlap` | off | Disable extract/convert prefetch |
| `-v` / `-vv` | info | Debug / trace logging |

Path matching uses `/`-normalized paths. Regex flags use Rust `regex`. Rsync flags follow `rsync(1)` include/exclude rules.

### Rsync filter rules

Rules are checked **in order**; the first match wins. Unmatched paths are **kept** (rsync default). An include-only list is not a whitelist — pair `+ *.txt` with `- *` to keep only text files.

| Pattern | Meaning |
|---------|---------|
| `*.tmp` (no `/`) | Match the **basename** (any directory) |
| `nested/skip.7z` | Match that full path |
| `/skip.7z` | Match `skip.7z` at the archive root only |
| `secret/` | Directories named `secret` only; children are pruned |
| `secret/***` | `secret` and everything under it |
| `*` / `?` / `[abc]` | Non-`/` wildcards; `**` also matches `/` |

Filter files accept `#` / `;` comments, `+`/`-`/`include`/`exclude`, `merge` / `.` (inlined), and `clear` / `!`. `dir-merge` / `:` is treated as `merge`.

CLI assembly order for each side (inner / outer): `--filter-from` → `--filter` → `--include-from` → `--exclude-from` → regex `--include-*` → regex `--exclude-*`. Put mixed include/exclude sequences in a filter file when order matters.

`convert-single` has the same rsync flags without the `-inner`/`-outer` suffix (`--filter`, `--filter-from`, `--include-from`, `--exclude-from`).

### Outer formats

| Format | How to select | Result |
|--------|---------------|--------|
| **7z** | default / `-o out.7z` | Non-solid outer; members stored (Copy), no recompress |
| **tar** | `--outer-format tar` or `-o out.tar` | Uncompressed tar of first-layer members |
| **dir** | `--outer-format dir` or `-o path/` | First-layer files only; nested still non-solid `.7z` |

Dir default without `-o`: same directory as the input archive, **name = input file stem**, e.g. `/data/game.7z` → `/data/game/`.

---

## What happens to a bad member

A corrupt nested archive, a passthrough that will not extract, an unsafe or duplicate path, or a failed bulk extract (falls back to per-member) is **skipped** with a warning on stderr. The rest of the members still land in the output. The job fails only if **nothing** usable remains to write.

---

## How it works

```text
input outer.7z
    │
    ├─ list + plan (exclude / rename / convert-nested / passthrough / skip)
    │
    ├─ solid single-pass extract of needed members (when useful)
    │
    ├─ size-aware nest workers ──► each nest: solid→non-solid convert
    │         (mutex)                      │
    │                                      ▼
    └─ SyncedOuterWriter ──────► outer 7z store | tar | directory
```

| Module | Role |
|--------|------|
| `pipeline` | Orchestration, nest scheduling, outer finalize |
| `convert` | Converter registry (`7z-solid-to-nonsolid`) |
| `archive` | CLI + native backends |
| `codec` | Store/tar/dir outer writers, LZMA2 codecs, headers |
| `filter` | Regex + rsync include/exclude + rename |
| `bin/bench_nested` | Fixture scales + timing + manual baselines |

Nested converts are concurrent only within the size budget; each nest’s unpack tree is scrubbed when done. Outer packs are appended, not rebuilt.

## Backends

| Backend | Flag | Notes |
|---------|------|--------|
| **CLI** | `--backend cli` (default) | Official 7zz/7z; production-safe parity with manual scripts |
| **Native** | `--backend native` | In-process solid-order decode; no full tree for streaming paths |

Native pipelines (`--native-pipeline`):

| Value | Behavior |
|-------|----------|
| `parallel` (default) | Windowed parallel LZMA2 → stream packs |
| `ahead` / `ahead:N` | Decode-ahead queue |
| `sequential` | One entry at a time |

Codec (`--native-codec`): `liblzma` (default, usually fastest) or `pure-rust`.

On ~8k tiny-file solid→non-solid, native parallel liblzma is about **9×** faster than CLI extract+pack on the results host — see [RESULTS](docs/bench/RESULTS.md#phase-3-single-solidnonsolid-bake-off).

## Performance highlights

Full tables: [`docs/bench/RESULTS.md`](docs/bench/RESULTS.md) · knobs & implementation checklist: [`docs/PERFORMANCE.md`](docs/PERFORMANCE.md)

### Full nested matrix (CLI backend, level 1)

~1M tiny files per nest · size-aware concurrency · append-store outer · 12-thread laptop (2026-07-27)

| nested | t=1 | t=2 | t=4 | t4 vs t1 |
|-------:|----:|----:|----:|---------:|
| 1 | 226 s | 244 s | 247 s | **0.92×** |
| 2 | 460 s | 293 s | 301 s | **1.53×** |
| 4 | 896 s | 568 s | 400 s | **2.24×** |
| 10 | 2351 s | 2756 s\* | 1147 s | **2.05×** |

\*n=10 t=2 looks like an outlier. **Single nest → prefer 1 pack thread** (the tool enforces this). **Multi-nest → workers help.**

### Other published numbers

| Suite | Result |
|-------|--------|
| Large tool vs manual 7z (280k×3, threads=1, one-at-a-time) | tool ≈ **1.01×** manual |
| Native parallel liblzma vs CLI (8k files) | ~**0.11×** wall (~9× faster) |

### Defaults that matter

| Behavior | Default |
|----------|---------|
| Nested size budget | `500M` packed |
| Nested workers | auto from `--threads` / CPUs |
| Single nest pack threads | **1** always |
| Outer container | non-solid **7z** store append |
| Solid outer extract | single-pass on |
| Non-solid nest passthrough | on when filters empty |

---

## Benchmarks (how to run)

```bash
cargo build --release --bin archiveconverter --bin bench_nested
./target/release/bench_nested scales
```

| Scale | Files / nest | Content | Outers | Intent |
|-------|--------------|---------|--------|--------|
| `tiny` | 200 | 1 MiB | 1, 2 | seconds / CI-ish |
| `small` | 2 000 | 4 MiB | 1, 2, 4 | iteration |
| `quick` | 10 000 | 16 MiB | 1, 2, 4 | minutes |
| `full` | 1 000 000 | 300 MiB | 1, 2, 4, 10 | hours |

```bash
./target/release/bench_nested all --scale tiny --threads 1,2,4
./target/release/bench_nested generate --scale full
./target/release/bench_nested run --scale full --threads 1,2,4 --level 1

# Manual 7z baselines at matching -mmt=N, then side-by-side
./target/release/bench_nested baseline-manual --scale tiny --threads 1,2,4
./target/release/bench_nested run --scale tiny --threads 1,2,4
```

- Local fixtures/outputs: `benchdata/` (gitignored)
- Committed numbers only: `docs/bench/RESULTS.md`, `docs/bench/full-results.csv`

### Tool vs manual (tests)

```bash
cargo test --test compare_7z_cli -- --nocapture
cargo test --release --test compare_7z_cli large_tool_vs_manual -- --ignored --nocapture
```

---

## Tests

```bash
export PATH="$HOME/.local/bin:$PATH"
cargo test
cargo test --release --test phase3_bakeoff -- --nocapture
```

| CI | Trigger | What |
|----|---------|------|
| `.github/workflows/ci.yml` | push/PR to `main` | check, test, release build, CLI smoke, compare bench job |
| `.github/workflows/release.yml` | tags `v*` | multi-target `archiveconverter` + `bench_nested` binaries |

```bash
git tag v0.1.0 && git push origin v0.1.0
```

---

## Project layout

```text
src/
  main.rs, cli.rs, lib.rs
  pipeline/     # nested orchestration
  convert/      # converters
  archive/      # cli + native backends
  codec/        # outer 7z/tar/dir, LZMA2 codecs, headers
  filter/       # regex + rsync filters, rename
  util/         # threads, size parse, temp, cleanup
  bin/bench_nested.rs
tests/          # e2e, cli smoke, compare_7z_cli, phase bakeoffs
docs/
  PERFORMANCE.md
  bench/RESULTS.md, full-results.csv, SNAPSHOT.md
```

## Documentation

| Doc | Audience |
|-----|----------|
| **This README** | Users and contributors; feature surface |
| [`docs/PERFORMANCE.md`](docs/PERFORMANCE.md) | Optimization inventory + knobs |
| [`docs/bench/RESULTS.md`](docs/bench/RESULTS.md) | Published timings |
| [`docs/bench/SNAPSHOT.md`](docs/bench/SNAPSHOT.md) | Bench index |

---

## License

[MIT](LICENSE)
