#!/usr/bin/env bash

set -euo pipefail

here="$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)"
script="$here/package-webview.sh"

pin() { sed -n "s/^$1=\"\(.*\)\"$/\1/p" "$script"; }
pin_archive="$(pin PIN_ARCHIVE)"
pin_sha1="$(pin PIN_SHA1)"
pin_sha256="$(pin PIN_SHA256)"
if [ -z "$pin_archive" ] || [ -z "$pin_sha1" ] || [ -z "$pin_sha256" ]; then
    echo "FAIL: cannot read the pins from $script" >&2
    exit 1
fi

scratch="$(mktemp -d)"
trap 'rm -rf "$scratch"' EXIT

stubs="$scratch/stubs"
mkdir -p "$stubs"
cat > "$stubs/sha1sum" <<EOF
#!/usr/bin/env bash
echo "$pin_sha1  \$1"
EOF
cat > "$stubs/sha256sum" <<EOF
#!/usr/bin/env bash
echo "$pin_sha256  \$1"
EOF
cat > "$stubs/tar" <<'EOF'
#!/usr/bin/env bash
dest=""
members=()
while [ $# -gt 0 ]; do
    case "$1" in
        -C) dest="$2"; shift 2 ;;
        -xjf) shift 2 ;;
        -*) shift ;;
        *) members+=("$1"); shift ;;
    esac
done
for m in "${members[@]}"; do
    rel="${m#*/}"
    rel="${rel/\*/}"
    mkdir -p "$dest/$(dirname "$rel")"
    echo stub > "$dest/$rel"
done
EOF
cat > "$stubs/cargo" <<'EOF'
#!/usr/bin/env bash
mkdir -p target/release/deps
case "$PWD" in
    */crates/eclipse-webview)
        printf '#!/bin/sh\nexit 2\n' > target/release/eclipse-webview
        cp "$CEF_PATH/libcef.so" target/release/libcef.so
        ;;
    *) echo stub > target/release/eclipse ;;
esac
EOF
chmod +x "$stubs"/*

make_repo() {
    local repo="$1"
    mkdir -p "$repo/tools/webview-dist" "$repo/crates/eclipse-webview" "$repo/cef/linux-x86_64"
    cp "$script" "$repo/tools/webview-dist/package-webview.sh"
    printf '{"name": "%s", "sha1": "%s"}\n' "$pin_archive" "$pin_sha1" \
        > "$repo/cef/linux-x86_64/archive.json"
    echo stub > "$repo/cef/linux-x86_64/libcef.so"
    : > "$repo/cef/$pin_archive"
}

run_packager() {
    local repo="$1"
    local out="$2"
    local status=0
    CEF_DIST="$repo/cef/linux-x86_64" OUT="$out" \
        SHA1SUM="$stubs/sha1sum" SHA256SUM="$stubs/sha256sum" \
        TAR="$stubs/tar" CARGO="$stubs/cargo" \
        "$repo/tools/webview-dist/package-webview.sh" > "$repo/stdout.log" 2> "$repo/stderr.log" \
        || status=$?
    echo "$status"
}

fail() { echo "FAIL: $*" >&2; exit 1; }

repo="$scratch/unstamped-install"
make_repo "$repo"
out="$repo/install"
mkdir -p "$out/logs"
echo stub > "$out/eclipse-webview"
echo stub > "$out/libcef.so"
echo keep > "$out/config.json"
echo keep > "$out/logs/run.log"
status="$(run_packager "$repo" "$out")"
[ "$status" != 0 ] || fail "an unstamped install directory was accepted as OUT"
grep -q "refusing to wipe" "$repo/stderr.log" || fail "no refusal message for an unstamped install directory"
if [ ! -f "$out/config.json" ] || [ ! -f "$out/logs/run.log" ]; then
    fail "files in an unstamped install directory were deleted"
fi
echo "ok: an unstamped directory holding eclipse-webview and libcef.so is never wiped"

repo="$scratch/cargo-target"
make_repo "$repo"
out="$repo/crates/eclipse-webview/target/release"
mkdir -p "$out/deps"
echo stub > "$out/eclipse-webview"
echo stub > "$out/libcef.so"
echo keep > "$out/deps/cache"
status="$(run_packager "$repo" "$out")"
[ "$status" != 0 ] || fail "the helper's cargo target directory was accepted as OUT"
grep -q "refusing to wipe" "$repo/stderr.log" || fail "no refusal message for the cargo target directory"
[ -f "$out/deps/cache" ] || fail "the helper's cargo target directory was deleted"
echo "ok: the helper's cargo target directory is never wiped"

repo="$scratch/stamped-payload"
make_repo "$repo"
out="$repo/dist"
mkdir -p "$out"
echo stamp > "$out/.eclipse-webview-payload"
echo stale > "$out/stale-file"
run_packager "$repo" "$out" > /dev/null
if grep -q "refusing to wipe" "$repo/stderr.log"; then
    fail "a stamped payload from a previous run was refused"
fi
[ ! -e "$out/stale-file" ] || fail "a stamped payload from a previous run was not replaced"
echo "ok: a stamped payload from a previous run is replaced"
