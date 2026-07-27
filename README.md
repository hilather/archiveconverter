# archiveconverter

Convert **nested solid 7z archives** to **non-solid** (`-ms=off`) form, one embedded archive at a time, with regex-based exclude and rename controls.

## Why

Solid 7z archives (`-ms=on`, the default) pack many files into one compressed stream. Nested solid archives make selective or streaming access expensive. This tool rebuilds:

- each **nested** `.7z` as non-solid (with optional member exclusion), and
- the **outer** archive as non-solid,

while keeping peak temp disk ≈ **one nested conversion at a time**.

## Requirements

- Rust 1.70+ (edition 2021)
- [7-Zip](https://www.7-zip.org/) CLI: `7zz`, `7z`, or `7za` on `PATH` (or `~/.local/bin/7zz`)

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

# Convert (nested one-at-a-time)
archiveconverter convert outer.7z -o out.7z \
  --exclude-outer '^skip_me\.7z$' \
  --exclude-inner '(?i)\.tmp$' \
  --rename '_old\.7z$=.7z' \
  --verify \
  --level 5

# Single archive only (no nesting)
archiveconverter convert-single solid.7z -o nonsolid.7z --exclude '\.tmp$' --verify
```

### Flags

| Flag | Meaning |
|------|---------|
| `--exclude-inner REGEX` | Drop matching paths **inside** each nested 7z |
| `--exclude-outer REGEX` | Drop matching members of the **outer** archive |
| `--rename PATTERN=REPL` | Rewrite outer member names (ordered; supports `$1` / `$name`) |
| `--basename-match` | Match excludes against basename only |
| `--dry-run` | Print plan only |
| `--verify` | `7z t` + entry count check |
| `--temp-dir` / `--keep-temp` | Control temp workspace |
| `--level` / `--threads` | Compression knobs passed to 7z |

Path matching uses normalized `/` separators (Rust `regex` crate).

## Architecture

Extensible converter registry:

- **v1:** `7z-solid-to-nonsolid` — extract → filter → pack `-ms=off`
- **stub:** `zip-stub` — reserved for future ZIP conversion

Pipeline: list outer → plan (skip / passthrough / convert nested) → process **one nested 7z at a time** → pack non-solid outer.

## Tests

```bash
export PATH="$HOME/.local/bin:$PATH"
cargo test
```

Integration tests build solid nested fixtures with the 7z CLI.

## Benchmarks

```bash
cargo build --release --bin archiveconverter --bin bench_nested
./target/release/bench_nested scales            # list sizes
```

| Scale | Files / nested | Content | Nested outers | Intent |
|-------|----------------|---------|---------------|--------|
| `tiny` | 200 | 1 MiB | 1, 2 | seconds |
| `small` | 2 000 | 4 MiB | 1, 2, 4 | default iteration |
| `quick` | 10 000 | 16 MiB | 1, 2, 4 | few minutes |
| `full` | 1 000 000 | 300 MiB | 1, 2, 4, 10 | hours |

```bash
# Fast local loop (default scale is small)
./target/release/bench_nested all --scale tiny --threads 1,2,4
./target/release/bench_nested all --scale small

# Heavier
./target/release/bench_nested generate --scale quick
./target/release/bench_nested run --scale quick --threads 1,2,4

# Production-shaped (slow)
./target/release/bench_nested generate --scale full
./target/release/bench_nested run --scale full --threads 1,2,3,4
```

Fixtures and results: `benchdata/<scale>/` (gitignored). Full-run notes:
`benchdata/full/results/RESULTS.md`.

**Finding on million-tiny-file workloads:** `--threads 1` is often *faster* than
2–4; multi-threaded LZMA does not help when per-file work is tiny and nested
archives convert serially.

## License

MIT
