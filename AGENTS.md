# Agent instructions — archiveconverter

You are working in **archiveconverter**, a Rust tool that converts nested solid 7z archives to non-solid form (outer 7z / tar / dir), with size-aware nest concurrency and optional native Phase 3 codecs.

This file is **mandatory policy** for every coding agent session in this repo (Grok, Claude, Codex, Cursor, etc.).

---

## Non-negotiable: docs & results stay current

**Every commit that changes user-visible behavior, performance, CLI, or published numbers must update documentation in the same change** (or an immediately following commit in the same PR/session before push).

Do **not** leave README / PERFORMANCE / RESULTS stale after shipping a feature or re-running benches.

### What to update (checklist)

| Change type | Update these |
|-------------|----------------|
| New/changed CLI flag or default | `README.md` (flags + quick start), `docs/PERFORMANCE.md` if it is a perf knob |
| Pipeline / concurrency / outer format behavior | `README.md` (feature table + architecture), `docs/PERFORMANCE.md` |
| New module / public API | `README.md` project layout if structure changed; rustdoc on public items |
| Meaningful bench re-run or new published number | `docs/bench/RESULTS.md`, `docs/bench/full-results.csv` when full matrix CSV changes, `docs/bench/SNAPSHOT.md` if index/host changes, **README summary table** if full-matrix headline numbers move |
| Perf optimization status | `docs/PERFORMANCE.md` implemented table |
| Commit that only refactors with no user-visible effect | No doc churn required; still fix anything you *know* is wrong |

### Published vs local data

| Path | Commit? |
|------|---------|
| `docs/bench/RESULTS.md` | **Yes** — tables, interpretation |
| `docs/bench/full-results.csv` | **Yes** — full matrix only |
| `docs/bench/SNAPSHOT.md` | **Yes** — index/host links |
| `README.md` bench summary | **Yes** — keep aligned with RESULTS |
| `benchdata/**` | **No** — gitignored fixtures, logs, multi‑MB `out-*.7z` |
| `target/**`, `fixtures/generated/**` | **No** |

When you re-run full benches locally, **copy numbers into `docs/bench/`** — do not rely on `benchdata/` for readers of the GitHub repo.

### After changing performance-sensitive code

1. Note whether existing published numbers may be invalid.  
2. If you re-ran benches: refresh `docs/bench/RESULTS.md` (+ CSV) and the README summary.  
3. If you did **not** re-run: leave numbers but fix prose if behavior/knobs changed; do not invent timings.  
4. Prefer honest “as of DATE / host” labels over silent drift.

### Commit messages

When docs are part of the work, include them in the **same** commit as the code when practical. Message should mention doc/results if that is a material part of the change.

---

## Project map (short)

| Path | Role |
|------|------|
| `src/pipeline/` | Nested convert orchestration, size budget, outer finalize |
| `src/convert/` | Converter registry (`7z-solid-to-nonsolid`) |
| `src/archive/` | CLI (`7zz`) + native backends |
| `src/codec/` | Outer 7z store / tar / dir writers; Phase 3 codecs; headers |
| `src/cli.rs` | Clap surface — source of truth for flags |
| `src/bin/bench_nested.rs` | Scales, tool runs, `baseline-manual` |
| `tests/` | e2e, cli smoke, compare_7z_cli, phase bakeoffs |
| `docs/PERFORMANCE.md` | Optimization inventory |
| `docs/bench/RESULTS.md` | **Canonical published timings** |

Defaults that must stay accurate in docs:

- Outer default: **7z** store append (not recompressed outer pack)  
- `--nested-size-budget` default **500M**  
- **Single nest** → pack threads **1**  
- Dir mode default path: **input stem** next to input archive  
- Corrupt nests: **skip**, do not fail the whole job  

---

## Build / test expectations

```bash
export PATH="$HOME/.local/bin:$PATH"   # 7zz if installed there
cargo test
cargo build --release --bins
# Optional: cargo test --release --test phase3_bakeoff -- --nocapture
```

Prefer `7zz` for parity with local benches. Do not commit `benchdata/` or release binaries under `target/`.

---

## Doc quality bar

- Prefer **current behavior** over historical design narrative.  
- Link to `docs/bench/RESULTS.md` instead of duplicating huge tables everywhere; README may keep a **short** summary table.  
- When removing or renaming a flag, grep README + docs + AGENTS and fix all hits.  
- CLI help text in `src/cli.rs` should match README flag tables.

---

## Skill

Project Grok skill (auto-invoked on commit/doc/bench work):

`.grok/skills/keep-docs-current/SKILL.md`

Use `/keep-docs-current` or follow that skill whenever finishing a commit that might leave docs behind.
