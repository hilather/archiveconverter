---
name: keep-docs-current
description: >
  Keep archiveconverter README, PERFORMANCE.md, and published bench results
  (docs/bench/) synchronized with code and measurements on every commit.
  Use when committing, finishing a PR, changing CLI flags, pipeline/outer-format
  behavior, concurrency, native codecs, or re-running benches; also when the user
  says "update docs", "sync results", "docs drift", or runs /keep-docs-current.
  Complements AGENTS.md mandatory policy in this repo.
---

# Keep docs & published results current

## When to run

Invoke **before every commit** (and before push) if the session touched:

- CLI (`src/cli.rs`, convert flags/defaults)
- Pipeline, outer formats, concurrency, backends, codecs
- Bench harness or published timings
- Anything user-visible that README or PERFORMANCE still describes differently

Also run when the user asks to document, release, or “make sure README is up to date.”

## Policy (summary)

Canonical agent policy: repo-root **`AGENTS.md`**. This skill operationalizes it.

| Commit to git | Do not commit |
|---------------|---------------|
| `README.md` | `benchdata/**` |
| `docs/PERFORMANCE.md` | `target/**` |
| `docs/bench/RESULTS.md` | Multi-MB archives |
| `docs/bench/full-results.csv` | |
| `docs/bench/SNAPSHOT.md` | |
| `AGENTS.md` / this skill if policy changes | |

## Procedure

### 1. Diff the change set

```bash
git status
git diff --stat
```

List which of: **CLI**, **pipeline/outer**, **perf behavior**, **benches** actually changed.

### 2. Reconcile prose with code

1. Read `src/cli.rs` (and `convert --help` if needed) as flag source of truth.  
2. Update **`README.md`**: feature table, flags, architecture, quick start, defaults.  
3. Update **`docs/PERFORMANCE.md`**: implemented table rows, knobs, measured notes.  
4. Grep for removed flags or outdated claims:

```bash
rg -n 'one at a time|serially|mmt=on only|outer-format|nested-size-budget|pack threads' README.md docs/
```

Fix stale “serial only nested” language unless describing the **manual baseline** path.

### 3. Published numbers

**If you re-ran benches and have new numbers:**

1. Update `docs/bench/RESULTS.md` (date, host if known, tables, takeaways).  
2. If full matrix CSV cells changed, rewrite `docs/bench/full-results.csv`.  
3. Refresh README “Performance highlights” summary table.  
4. Touch `docs/bench/SNAPSHOT.md` only if index/host/links need it.

**If you did not re-run benches:**

- Do **not** invent timings.  
- Still fix behavioral docs.  
- Optionally note “numbers predate &lt;change&gt;; re-run full matrix to refresh.”

**Never** commit local `benchdata/` fixtures or `out-*.7z`.

### 4. Defaults that must match code

Verify docs still say:

- Default outer: **7z** append-store  
- `--nested-size-budget` default **500M**  
- **Single nest** → pack threads **1**  
- **Dir** default path = input **stem** beside input  
- Corrupt / unexpected members → **skip**, continue  
- Manual baseline = one nest at a time; tool may concurrent multi-nest  

### 5. Commit hygiene

Prefer **one commit** that includes code + doc/result updates. If docs were forgotten, add a follow-up commit *before push* titled like:

`docs: sync README and RESULTS with <feature>`

### 6. Done criteria

- [ ] README reflects current flags and feature set  
- [ ] PERFORMANCE checklist matches implementation  
- [ ] Published RESULTS (and CSV if needed) not knowingly wrong  
- [ ] No `benchdata/` in the commit  
- [ ] `git status` clean of intended docs after add  

## Commands (reference)

```bash
cargo build --release --bins
./target/release/archiveconverter convert --help
./target/release/bench_nested scales

# After a full matrix run (hours):
# copy key tables into docs/bench/RESULTS.md and full-results.csv
# update README summary
```

## Anti-patterns

- Shipping `--outer-format dir` / tar / concurrency changes without README  
- Leaving RESULTS as old serial-only matrix after concurrent pipeline lands  
- Committing multi-hundred-MB `benchdata/full/results/out-*.7z`  
- Duplicating full RESULTS tables into three places without updating all of them  

When unsure, update **RESULTS.md + README summary + PERFORMANCE** in that order of priority.
