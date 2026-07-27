#!/usr/bin/env bash
# Build sample solid nested 7z archives for manual experiments.
set -euo pipefail
ROOT="$(cd "$(dirname "$0")/.." && pwd)"
OUT="${1:-$ROOT/fixtures/generated}"
mkdir -p "$OUT"
export PATH="${HOME}/.local/bin:${PATH}"

if ! command -v 7zz >/dev/null 2>&1 && ! command -v 7z >/dev/null 2>&1; then
  echo "error: 7zz/7z not found on PATH" >&2
  exit 1
fi
SEVENZ="$(command -v 7zz || command -v 7z)"

pack_solid() {
  local src=$1 dest=$2
  rm -f "$dest"
  (cd "$src" && "$SEVENZ" a -t7z -mx=5 -ms=on -y "$dest" . >/dev/null)
}

make_inner() {
  local name=$1
  local tree="$OUT/tree-$name"
  rm -rf "$tree"
  mkdir -p "$tree/data" "$tree/notes" "$tree/__MACOSX"
  echo "hello from $name" >"$tree/data/hello.txt"
  echo "binary-keep" >"$tree/data/keep.bin"
  echo "temporary" >"$tree/data/drop.tmp"
  echo "junk" >"$tree/__MACOSX/._junk"
  echo "readme" >"$tree/notes/readme.txt"
  pack_solid "$tree" "$OUT/$name"
}

make_inner "alpha_old.7z"
make_inner "beta.7z"
make_inner "skip_me.7z"

STAGE="$OUT/outer-stage"
rm -rf "$STAGE"
mkdir -p "$STAGE"
cp "$OUT/alpha_old.7z" "$OUT/beta.7z" "$OUT/skip_me.7z" "$STAGE/"
echo "outer readme" >"$STAGE/readme.txt"
pack_solid "$STAGE" "$OUT/outer.7z"

echo "Fixtures written under $OUT"
ls -la "$OUT"/*.7z
