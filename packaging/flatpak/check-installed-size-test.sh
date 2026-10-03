#!/usr/bin/env bash

set -euo pipefail

here="$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)"
script="$here/check-installed-size.sh"

fail() { echo "FAIL: $*" >&2; exit 1; }

limit="$(sed -n 's/^LIMIT_BYTES=\([0-9][0-9]*\)$/\1/p' "$script")"
[ -n "$limit" ] || fail "cannot read LIMIT_BYTES from $script"

scratch="$(mktemp -d)"
trap 'rm -rf "$scratch"' EXIT
repo="$scratch/repo"
ostree --repo="$repo" init --mode=archive-z2

commit_tree() {
    ostree --repo="$repo" commit --branch="app/test/x86_64/$1" --tree=dir="$scratch/$1" > /dev/null
}

check_tree() {
    local name="$1"
    local status=0
    "$script" "$repo" "app/test/x86_64/$name" > "$scratch/$name.log" 2>&1 || status=$?
    echo "$status"
}

mkdir -p "$scratch/over/files"
head -c "$((limit + 1))" /dev/urandom > "$scratch/over/files/big"
commit_tree over
[ "$(check_tree over)" = 1 ] || fail "a file of LIMIT_BYTES + 1 bytes passed: $(cat "$scratch/over.log")"
grep -q "above LIMIT_BYTES=$limit " "$scratch/over.log" || fail "the failure does not name the limit: $(cat "$scratch/over.log")"
echo "ok: a file of LIMIT_BYTES + 1 bytes fails and names the limit"

mkdir -p "$scratch/small/files"
head -c 1000000 /dev/urandom > "$scratch/small/files/one-megabyte"
commit_tree small
[ "$(check_tree small)" = 0 ] || fail "a 1 MB file failed: $(cat "$scratch/small.log")"
grep -q "installs 1000448 bytes (1.0 MB)" "$scratch/small.log" || fail "a 1 MB file was not counted in 512-byte blocks: $(cat "$scratch/small.log")"
echo "ok: a 1 MB file passes and counts in 512-byte blocks"

mkdir -p "$scratch/copies/files"
head -c 1000 /dev/urandom > "$scratch/copies/files/first"
cp "$scratch/copies/files/first" "$scratch/copies/files/second"
ln -s first "$scratch/copies/files/link"
commit_tree copies
[ "$(check_tree copies)" = 0 ] || fail "two small files failed: $(cat "$scratch/copies.log")"
grep -q "installs 2048 bytes " "$scratch/copies.log" || fail "identical files and symlinks were not counted as flatpak counts them: $(cat "$scratch/copies.log")"
echo "ok: each copy of an identical file counts and symlinks count nothing, as flatpak counts them"
