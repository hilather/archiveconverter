# archiveconverter

**Nested solid 7z → non-solid**, with filters, renames, size-aware concurrent nest conversion, and outer output as **7z**, **tar**, or a plain **directory**.

| | |
|--|--|
| **Status** | Active; CLI + library (`archiveconverter` crate) |
| **License** | MIT |
| **Repo** | [hilather/archiveconverter](https://github.com/hilather/archiveconverter) |
| **Default engine** | Official 7-Zip CLI (`7zz` / `7z` / `7za`) |
| **Optional engine** | Native `sevenz-rust2` + Phase 3 windowed parallel **liblzma** codec |

Published benchmark tables: **[`docs/bench/RESULTS.md`](docs/bench/RESULTS.md)** · design notes: [`docs/PERFORMANCE.md`](docs/PERFORMANCE.md) · agent rules: [`AGENTS.md`](AGENTS.md)

---

## What it does

Solid 7z (`-ms=on`) packs many files into one compressed stream. Nested solid archives make random access and remounting expensive.

Given an **outer** archive that embeds one or more solid `.7z` members, this tool:

1. **Plans** outer members (skip / passthrough / convert nested)  
2. **Converts** each nested solid 7z → non-solid (`-ms=off`), with optional excludes  
3. **Writes** the first layer into an outer **non-solid 7z**, uncompressed **tar**, or **directory**  
4. Keeps peak work bounded via **size-aware nested concurrency** (default 500 MiB packed nests in flight)

Nested *content* is still compressed 7z; only the **outer container** and **solidity** of nested archives change.

---

## Feature overview (current)

| Area | Capability |
|------|------------|
| **Outer formats** | `7z` (default, store/Copy append) · `tar` (uncompressed) · `dir` (no re-wrap) |
| **Dir default name** | `--outer-format dir` without `-o` → `<input-dir>/<archive-stem>/` |
| **Nested concurrency** | Smallest-first; workers from `--threads`; cap with `--nested-size-budget` / `--nested-concurrency` |
| **Single nest** | Pack/encode threads forced to **1** (MT often slower on dense tiny files) |
| **Outer writer** | Streaming append under a mutex — **no final recompress** of the outer |
| **Solid outer extract** | Single-pass bulk extract of needed members (default) |
| **Passthrough** | Already non-solid nested archives can be copied when filters are empty |
| **Filters** | Regex exclude/include **and** rsync filter files/rules (first-match; dir prune) |
| **7z excludes** | Common regexes / simple rsync excludes mapped to `7z -x!` globs when possible |
| **Corrupt / unexpected members** | Skipped with a warning; rest of the outer still completes |
| **Backends** | `cli` (default) or `native` (streaming Phase 1–3 pipelines) |
| **Native Phase 3** | Windowed parallel LZMA2 (`liblzma` or pure-rust); packs stream out; bounded RAM |
| **Headers** | Custom non-solid writers aligned for 7zz / sevenz-rust2 / common mounters |
| **File metadata** | Member **Modified** times and Windows attributes preserved through solid→non-solid and outer 7z store append (native + CLI) |
| **Bench harness** | `bench_nested` scales + **stored manual 7z baselines** at matching `-mmt=N` |

---

## Requirements

- **Rust** 1.70+ (edition 2021)
- **7-Zip CLI** on `PATH` for the default backend: `7zz`, `7z`, or `7za` (or `~/.local/bin/7zz`)
- Native backend: no 7z required for pure in-process convert paths (CI still installs 7z for fixtures/tests)

```bash
# Linux: official 7zz (user-local)
mkdir -p ~/.local/bin
curl -fsSL -o /tmp/7z.tar.xz https://www.7-zip.org/a/7z2501-linux-x64.tar.xz
tar -xOf /tmp/7z.tar.xz 7zz > ~/.local/bin/7zz
chmod +x ~/.local/bin/7zz
export PATH="$HOME/.local/bin:$PATH"
```

## Build

```bash
cargo build --release
# binaries: target/release/archiveconverter  target/release/bench_nested
```

---

## Quick start

```bash
# Inspect engines
archiveconverter backend
archiveconverter list-converters

# Plan only
archiveconverter convert outer.7z -o out.7z \
  --exclude-outer '^skip_me\.7z$' \
  --exclude-inner '(?i)\.tmp$' \
  --exclude-inner '^__MACOSX/' \
  --rename '_old\.7z$=.7z' \
  --dry-run

# Nested convert → non-solid outer 7z
archiveconverter convert outer.7z -o out.7z \
  --exclude-outer '^skip_me\.7z$' \
  --exclude-inner '(?i)\.tmp$' \
  --rename '_old\.7z$=.7z' \
  --threads 4 --level 1 --verify

# Same filters as rsync files/rules (first-match; dir prune)
archiveconverter convert outer.7z -o out.7z \
  --exclude-from-outer skip.excludes \
  --filter-inner '- *.tmp' \
  --filter-inner '- __MACOSX/'

# Outer as uncompressed tar
archiveconverter convert outer.7z -o out.tar --outer-format tar --level 1 --verify
# extension alone also selects tar:
archiveconverter convert outer.7z -o out.tar --level 1

# First layer only (no outer archive). Default dir = archive stem next to input.
#   path/game.7z  →  path/game/
archiveconverter convert path/game.7z --outer-format dir --level 1 --verify
archiveconverter convert outer.7z -o /tmp/unpacked --outer-format dir --level 1

# Single archive (no nesting)
archiveconverter convert-single solid.7z -o nonsolid.7z --exclude '\.tmp$' --verify

# Native Phase 3 (fast on tiny-file solid→non-solid)
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
| `convert-single` | One 7z solid→non-solid (no outer nesting logic) |
| `backend` | Print CLI / native backend info |
| `list-converters` | Registered converters |

### `convert` options

| Flag | Default | Meaning |
|------|---------|---------|
| `-o`, `--output` | required for 7z/tar; optional for `dir` | Output file or directory |
| `--outer-format` | inferred | `7z` \| `tar` \| `dir`. Omit: `.tar` → tar; path ends with `/` → dir; else 7z |
| `--exclude-inner` / `--exclude-outer` | — | Regex exclude (repeatable); appended after rsync rules |
| `--include-inner` / `--include-outer` | — | Regex include (repeatable); first-match with excludes |
| `--filter-inner` / `--filter-outer` | — | Rsync rule: `+ pat`, `- pat`, or bare exclude |
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
| `--native-codec` | `liblzma` | `liblzma` \| `pure-rust` (Phase 3) |
| `--native-large-threshold` | 512 KiB | Size for MT LZMA2 on native encode |
| `--verify` | off | Count/test output (7z / tar / dir) |
| `--dry-run` | off | Print plan only |
| `--temp-dir` / `--keep-temp` | system temp | Control workspace |
| `--profile` | off | Stage timings at info |
| `--no-solid-single-pass` | off | Disable bulk outer extract |
| `--no-passthrough-nonsolid` | off | Always recompress non-solid nests |
| `--no-pipeline-overlap` | off | Disable extract/convert prefetch |
| `-v` / `-vv` | info | Debug / trace logging |

Path matching uses `/`-normalized paths. **Regex** flags use Rust `regex`. **Rsync** flags follow `rsync(1)` include/exclude rules (see below).

### Rsync filter rules

Rules are checked **in order**; the first match wins. Unmatched paths are **kept** (rsync default). An include-only list is not a whitelist — pair `+ *.txt` with `- *` to keep only text files.

| Pattern | Meaning |
|---------|---------|
| `*.tmp` (no `/`) | Match the **basename** (any directory) |
| `nested/skip.7z` | Match that full path |
| `/skip.7z` | Match `skip.7z` at the archive root only |
| `secret/` | Directories named `secret` only; children are pruned (rsync would not recurse) |
| `secret/***` | `secret` and everything under it |
| `*` / `?` / `[abc]` | Non-`/` wildcards; `**` also matches `/` |

Filter files accept `#` / `;` comments, `+`/`-`/`include`/`exclude`, `merge` / `.` (inlined), and `clear` / `!`. `dir-merge` / `:` is treated as `merge` (archives have no live per-directory walk).

CLI assembly order for each side (inner / outer): `--filter-from` → `--filter` → `--include-from` → `--exclude-from` → regex `--include-*` → regex `--exclude-*`. Put mixed include/exclude sequences in a filter file when order matters.

`convert-single` has the same rsync flags without the `-inner`/`-outer` suffix (`--filter`, `--filter-from`, `--include-from`, `--exclude-from`).

### Outer formats

| Format | How to select | Result |
|--------|---------------|--------|
| **7z** | default / `-o out.7z` | Non-solid outer; members stored (Copy), no recompress |
| **tar** | `--outer-format tar` or `-o out.tar` | Uncompressed tar of first-layer members |
| **dir** | `--outer-format dir` or `-o path/` | First-layer files only; nested still non-solid `.7z` |

Dir default without `-o`: same directory as the input archive, **name = input file stem** (suffix stripped), e.g. `/data/game.7z` → `/data/game/`.

---

## Architecture

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
| `convert` | Registry; `7z-solid-to-nonsolid` (+ zip stub) |
| `archive` | CLI + native backends |
| `codec` | Store/tar/dir outer writers, Phase 3 LZMA2, headers |
| `filter` | Regex + rsync include/exclude + rename |
| `bin/bench_nested` | Fixture scales + timing + manual baselines |

**Disk model:** nested converts are concurrent only within the size budget; each nest’s unpack tree is scrubbed when done. Outer packs are appended, not rebuilt.

**Failure model:** a corrupt nested archive, a passthrough that will not extract, an unsafe/duplicate member path, or a bulk-extract failure (falls back to per-member) is **skipped** (stderr warning + log). Other members still land in the output. The job fails only if **nothing** usable remains to write.

---

## Backends

| Backend | Flag | Notes |
|---------|------|--------|
| **CLI** | `--backend cli` (default) | Official 7zz/7z; production-safe parity with manual scripts |
| **Native** | `--backend native` | In-process solid-order decode; no full tree for streaming paths |

Native pipelines (`--native-pipeline`):

| Value | Behavior |
|-------|----------|
| `parallel` (default) | Phase 3: windowed parallel LZMA2 → stream packs |
| `ahead` / `ahead:N` | Phase 2: decode-ahead queue |
| `sequential` | Phase 1: one entry at a time |

Codec (`--native-codec`): `liblzma` (default, usually fastest) or `pure-rust`.

On ~8k tiny-file solid→non-solid, Phase 3 liblzma is about **9×** faster than CLI extract+pack on the results host — see [RESULTS](docs/bench/RESULTS.md#phase-3-single-solidnonsolid-bake-off).

---

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

\*n=10 t=2 looks like an outlier. **Single nest → prefer 1 pack thread** (tool enforces this). **Multi-nest → workers help.**

### Other published numbers

| Suite | Result |
|-------|--------|
| Large tool vs manual 7z (280k×3, threads=1, one-at-a-time) | tool ≈ **1.01×** manual |
| Phase 3 parallel liblzma vs CLI (8k files) | ~**0.11×** wall (~9× faster) |

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

# Manual 7z baselines at matching -mmt=N (one nest at a time), then side-by-side
./target/release/bench_nested baseline-manual --scale tiny --threads 1,2,4
./target/release/bench_nested run --scale tiny --threads 1,2,4
```

- Local fixtures/outputs: `benchdata/` (**gitignored**)  
- Committed numbers only: `docs/bench/RESULTS.md`, `docs/bench/full-results.csv`  
- Agents: update those docs when performance-relevant code changes — see [`AGENTS.md`](AGENTS.md)

### Tool vs manual (tests)

```bash
cargo test --test compare_7z_cli -- --nocapture
cargo test --release --test compare_7z_cli large_tool_vs_manual -- --ignored --nocapture
```

---

## Tests & CI

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
  codec/        # outer 7z/tar/dir, Phase 3 codecs, headers
  filter/       # regex + rsync filters, rename
  util/         # threads, size parse, temp, cleanup
  bin/bench_nested.rs
tests/          # e2e, cli smoke, compare_7z_cli, phase bakeoffs
docs/
  PERFORMANCE.md
  bench/RESULTS.md, full-results.csv, SNAPSHOT.md
AGENTS.md       # instructions for coding agents
.grok/skills/   # Grok project skills
```

---

## Documentation map

| Doc | Audience |
|-----|----------|
| **This README** | Users + contributors; feature surface |
| [`docs/PERFORMANCE.md`](docs/PERFORMANCE.md) | Optimization inventory + knobs |
| [`docs/bench/RESULTS.md`](docs/bench/RESULTS.md) | **Published** timings |
| [`docs/bench/SNAPSHOT.md`](docs/bench/SNAPSHOT.md) | Bench index |
| [`AGENTS.md`](AGENTS.md) | Agents: keep docs/results in sync every commit |
| `.grok/skills/keep-docs-current/` | Auto skill for the same policy |

---

## License

MIT
