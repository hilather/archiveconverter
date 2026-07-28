# archiveconverter

Convert **nested solid 7z archives** to **non-solid** form, with regex exclude/rename, size-aware nested concurrency, and optional outer **7z / tar / directory** output.

## Why

Solid 7z archives (`-ms=on`, the default) pack many files into one compressed stream. Nested solid archives make selective or streaming access expensive. This tool rebuilds:

- each **nested** `.7z` as non-solid (with optional member exclusion), and
- the **outer** as non-solid **7z**, uncompressed **tar**, or a plain **directory** of first-layer members,

while controlling peak work via a **size-aware nested concurrency budget** (default 500 MiB of packed nests in flight).

## Requirements

- Rust 1.70+ (edition 2021)
- [7-Zip](https://www.7-zip.org/) CLI: `7zz`, `7z`, or `7za` on `PATH` (or `~/.local/bin/7zz`) for the default CLI backend

```bash
# Linux example (user-local static binary)
mkdir -p ~/.local/bin
curl -fsSL -o /tmp/7z.tar.xz https://www.7-zip.org/a/7z2501-linux-x64.tar.xz
tar -xOf /tmp/7z.tar.xz 7zz > ~/.local/bin/7zz
chmod +x ~/.local/bin/7zz
export PATH="$HOME/.local/bin:$PATH"
```

## Build

```bash
cargo build --release
```

## Backends

| Backend | Flag | Engine |
|---------|------|--------|
| **CLI** (default) | `--backend cli` | Official `7zz`/`7z` subprocesses |
| **Native** (Phase 1–3) | `--backend native` | [`sevenz-rust2`](https://crates.io/crates/sevenz-rust2) + optional **liblzma** |

Native path **streams solid → non-solid** without unpacking a full file tree:

| Phase | Pipeline (`--native-pipeline`) | Notes |
|-------|--------------------------------|--------|
| 1 | `sequential` | Decode then encode each entry |
| 2 | `ahead` / `ahead:N` | Decode-ahead queue + size-aware MT LZMA2 |
| 3 | `parallel` (default) | Windowed parallel LZMA2 → **stream** packs (not whole archive in RAM) |

Codec for Phase 3: `--native-codec liblzma` (default) or `pure-rust`.

```bash
archiveconverter convert-single solid.7z -o out.7z --backend native --threads 4 --level 1
archiveconverter convert-single solid.7z -o out.7z --backend native \
  --native-pipeline parallel --native-codec liblzma
archiveconverter convert outer.7z -o out.7z --backend native \
  --native-pipeline ahead:2 --native-large-threshold 524288
archiveconverter backend
```

## CI & releases

GitHub Actions (`.github/workflows/`):

- **CI** on push/PR: `cargo test`, release build, CLI smoke, tiny nested bench, and
  `tests/compare_7z_cli.rs` (correctness + timing vs a manual 7z CLI pipeline).
- **Release** on tags `v*`: multi-target binaries uploaded to a GitHub Release.

```bash
git tag v0.1.0
git push origin v0.1.0
```

## Usage

```bash
# Inspect backend
archiveconverter backend
archiveconverter list-converters

# Dry-run plan
archiveconverter convert outer.7z -o out.7z \
  --exclude-outer '^skip_me\.7z$' \
  --exclude-inner '(?i)\.tmp$' \
  --exclude-inner '^__MACOSX/' \
  --rename '_old\.7z$=.7z' \
  --dry-run

# Convert → non-solid outer 7z (default)
archiveconverter convert outer.7z -o out.7z \
  --exclude-outer '^skip_me\.7z$' \
  --exclude-inner '(?i)\.tmp$' \
  --rename '_old\.7z$=.7z' \
  --verify \
  --level 5 \
  --threads 4

# Outer as uncompressed tar (nested members stay non-solid .7z inside)
archiveconverter convert outer.7z -o out.tar --outer-format tar --verify --level 1
# or infer from extension:
archiveconverter convert outer.7z -o out.tar --verify --level 1

# First layer only: write members into a directory (no re-wrap)
# Default dir = archive stem next to input: path/game.7z → path/game/
archiveconverter convert path/game.7z --outer-format dir --verify --level 1
archiveconverter convert outer.7z -o /tmp/unpacked --outer-format dir --level 1

# Single archive only (no nesting)
archiveconverter convert-single solid.7z -o nonsolid.7z --exclude '\.tmp$' --verify
```

### Flags

| Flag | Meaning |
|------|---------|
| `--outer-format 7z\|tar\|dir` | Outer container (`7z` default; `tar` = uncompressed; `dir` = first-layer files only). If omitted: `.tar` → tar; path ending in `/` → dir |
| `-o PATH` | Output file (7z/tar) or directory (`dir`). For `dir`, defaults to `<input-dir>/<archive-stem>/` |
| `--exclude-inner REGEX` | Drop matching paths **inside** each nested 7z |
| `--exclude-outer REGEX` | Drop matching members of the **outer** archive |
| `--rename PATTERN=REPL` | Rewrite outer member names (ordered; supports `$1` / `$name`) |
| `--basename-match` | Match excludes against basename only |
| `--dry-run` | Print plan only |
| `--verify` | Entry-count check (7z / tar / directory as appropriate) |
| `--temp-dir` / `--keep-temp` | Control temp workspace |
| `--level` / `--threads` | Nested pack level / nest workers + pack MT (single nest forces pack threads=1) |
| `--nested-concurrency N` | Max nests in flight (`0` = auto from threads/CPUs) |
| `--nested-size-budget SIZE` | Max packed size of nests converting together (default `500M`; `0` = no cap) |
| `--backend cli\|native` | 7z engine |
| `--native-pipeline` / `--native-codec` | Native Phase 2/3 knobs |
| `--profile` | Stage timings at info level |

Path matching uses normalized `/` separators (Rust `regex` crate).

## Architecture

Extensible converter registry:

- **v1:** `7z-solid-to-nonsolid` — extract → filter → pack `-ms=off` (or native stream)
- **stub:** `zip-stub` — reserved for future ZIP conversion

Pipeline:

1. List outer → plan (skip / passthrough / convert nested)  
2. Optional solid single-pass bulk extract of needed members  
3. Convert nests with **size-aware concurrency** (smallest first; budget + worker caps)  
4. Append finished members into outer **7z store / tar / directory** under a mutex (no final outer recompress)

Corrupt nested archives are **skipped** (logged); they do not abort the whole job.

## Tests

```bash
export PATH="$HOME/.local/bin:$PATH"
cargo test
```

Integration tests build solid nested fixtures with the 7z CLI.

## Benchmarks

Published numbers (tables + CSV): **[`docs/bench/RESULTS.md`](docs/bench/RESULTS.md)**  
Index: [`docs/bench/SNAPSHOT.md`](docs/bench/SNAPSHOT.md) · knobs: [`docs/PERFORMANCE.md`](docs/PERFORMANCE.md)

### Full nested matrix (summary)

Host: 12-thread i7-8750H, NVMe. ~1M tiny files per nest, `--level 1`, CLI backend, size-aware nested concurrency.

| nested | t=1 | t=2 | t=4 | t4 vs t1 |
|-------:|----:|----:|----:|---------:|
| 1 | 226 s | 244 s | 247 s | **0.92×** |
| 2 | 460 s | 293 s | 301 s | **1.53×** |
| 4 | 896 s | 568 s | 400 s | **2.24×** |
| 10 | 2351 s | 2756 s\* | 1147 s | **2.05×** |

\*n=10 t=2 looks like an outlier. Single nest prefers 1 pack thread (tool forces this). Multi-nest benefits from concurrent workers.

Other published results: large tool≈manual (~1.01× at threads=1), Phase 3 native liblzma ~**9×** vs CLI on 8k tiny files — see RESULTS.md.

### How to run benches

```bash
cargo build --release --bin archiveconverter --bin bench_nested
./target/release/bench_nested scales            # list sizes
```

| Scale | Files / nested | Content | Nested outers | Intent |
|-------|----------------|---------|---------------|--------|
| `tiny` | 200 | 1 MiB | 1, 2 | seconds |
| `small` | 2 000 | 4 MiB | 1, 2, 4 | default iteration |
| `quick` | 10 000 | 16 MiB | 1, 2, 4 | few minutes |
| `full` | 1 000 000 | 300 MiB | 1, 2, 4, 10 | hours |

```bash
# Fast local loop
./target/release/bench_nested all --scale tiny --threads 1,2,4
./target/release/bench_nested all --scale small

# Production-shaped (slow)
./target/release/bench_nested generate --scale full
./target/release/bench_nested run --scale full --threads 1,2,4 --level 1

# Stored manual 7z baselines (same -mmt=N as --threads N; one nest at a time)
./target/release/bench_nested baseline-manual --scale tiny --threads 1,2,4
./target/release/bench_nested run --scale tiny --threads 1,2,4   # side-by-side tool vs manual
```

Local fixtures/artifacts: `benchdata/<scale>/` (**gitignored**). Only result *tables* are committed under `docs/bench/`.

### Tool vs manual 7z (fair one-at-a-time)

```bash
# Fast correctness + short timing (CI)
cargo test --test compare_7z_cli -- --nocapture

# Multi-minute load (both paths one nested at a time; ignored by default)
cargo test --release --test compare_7z_cli large_tool_vs_manual \
  -- --ignored --nocapture
# Optional: LARGE_BENCH_FILES=280000 LARGE_BENCH_NESTED=3 LARGE_BENCH_MIN_SECS=120
```

### Performance features (defaults on where safe)

| Feature | Flag / default |
|---------|----------------|
| Solid outer single-pass extract | default; `--no-solid-single-pass` |
| Passthrough already-non-solid nested | default; `--no-passthrough-nonsolid` |
| Auto pack threads (tiny files → 1) | omit `--threads` |
| Extract/convert overlap prefetch | default; `--no-pipeline-overlap` |
| Size-aware nested parallel | default: up to `--threads` workers, `--nested-size-budget 500M` |
| Single nest → pack threads=1 | always |
| Outer append store / tar / dir | `--outer-format 7z\|tar\|dir` |
| Cap nested workers | `--nested-concurrency N` (`0` = auto) |
| Exclude via 7z `-x!` when regex maps | automatic for `\.ext$`, `^prefix/` |
| Stage timings | `--profile` |
| Native parallel codec (Phase 3) | `--backend native` + `--native-pipeline parallel` |
| Native LZMA2 engine | `--native-codec liblzma` \| `pure-rust` |
| Skip corrupt nested | always (log + continue) |

Details: [`docs/PERFORMANCE.md`](docs/PERFORMANCE.md).

## License

MIT
