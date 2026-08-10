#!/usr/bin/env bash















set -euo pipefail

here="$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)"
repo="$(cd "$here/../.." && pwd)"








PIN_ARCHIVE="cef_binary_149.0.6+g0d0eeb6+chromium-149.0.7827.201_linux64_minimal.tar.bz2"
PIN_SHA1="d46ec0d5723771bd1c9678c429e1cdb1f1ef0a72"
PIN_SHA256="f90dec4c5c42a7bbd4f2bd80a7a77e0ac6aacfc6627bb43572d803e77f26dfbc"
CEF_CDN="https://cef-builds.spotifycdn.com/"

usage() {
    cat <<EOF
usage: package-webview.sh [--help]

Assembles the shippable Eclipse payload (eclipse + eclipse-webview + the stripped/pruned CEF
runtime + licenses) into one directory (the tier-3 sibling-resolution layout). Every shipped
CEF byte is extracted from the dual-digest-verified pinned tarball.

Environment parameters (all optional; defaults are repo-relative — no machine paths):
  CEF_DIST   the exported CEF binary dist dir — the helper's BUILD input only
             (default: \$repo/vendor/cef/linux-x86_64; fetched via export-cef-dir when
              absent; its libcef.so is digest-checked against the verified tarball before
              the build)
  OUT        the output payload dir (default: \$repo/dist/eclipse-linux-x86_64; wiped per
             run — but ONLY when it is absent, empty, or carries the
             .eclipse-webview-payload stamp of a previous run)
  STRIP/READELF/SHA1SUM/SHA256SUM/TAR/DU/CARGO  tool overrides (default: discovered on PATH)
  EXPORT_CEF_DIR  the export-cef-dir binary (default: discovered; only needed to fetch)

The pinned archive: $PIN_ARCHIVE
EOF
}
if [ "${1:-}" = "--help" ] || [ "${1:-}" = "-h" ]; then
    usage
    exit 0
fi

CEF_DIST="${CEF_DIST:-$repo/vendor/cef/linux-x86_64}"
OUT="${OUT:-$repo/dist/eclipse-linux-x86_64}"

fail() { echo "ERROR: $*" >&2; exit 1; }
STRIP="${STRIP:-$(command -v strip || true)}"
READELF="${READELF:-$(command -v readelf || true)}"
SHA1SUM="${SHA1SUM:-$(command -v sha1sum || true)}"
SHA256SUM="${SHA256SUM:-$(command -v sha256sum || true)}"
TAR="${TAR:-$(command -v tar || true)}"
DU="${DU:-$(command -v du || true)}"
CARGO="${CARGO:-$(command -v cargo || true)}"
EXPORT_CEF_DIR="${EXPORT_CEF_DIR:-$(command -v export-cef-dir || true)}"
[ -n "$STRIP" ] && [ -x "$STRIP" ] || fail "strip not found (install binutils, or set STRIP)"
[ -n "$READELF" ] && [ -x "$READELF" ] || fail "readelf not found (install binutils, or set READELF)"
[ -n "$SHA1SUM" ] && [ -x "$SHA1SUM" ] || fail "sha1sum not found (install coreutils, or set SHA1SUM)"
[ -n "$SHA256SUM" ] && [ -x "$SHA256SUM" ] || fail "sha256sum not found (install coreutils, or set SHA256SUM)"
[ -n "$TAR" ] && [ -x "$TAR" ] || fail "tar not found (install tar, or set TAR)"
[ -n "$DU" ] && [ -x "$DU" ] || fail "du not found (install coreutils, or set DU)"
[ -n "$CARGO" ] && [ -x "$CARGO" ] || fail "cargo not found (install Rust, or set CARGO)"






guard_out() {
    [ -e "$OUT" ] || return 0
    [ -d "$OUT" ] || fail "OUT ($OUT) exists and is not a directory"
    [ -z "$(ls -A "$OUT")" ] && return 0
    [ -f "$OUT/.eclipse-webview-payload" ] && return 0
    { [ -f "$OUT/eclipse-webview" ] && [ -f "$OUT/libcef.so" ]; } && return 0
    fail "OUT ($OUT) exists, is non-empty, and is not a previous run's payload (no .eclipse-webview-payload stamp, no eclipse-webview+libcef.so pair) — refusing to wipe a directory this script did not create (point OUT at a new/empty directory)"
}
guard_out

work="$(mktemp -d)"
trap 'rm -rf "$work"' EXIT


if [ ! -d "$CEF_DIST" ]; then
    [ -n "$EXPORT_CEF_DIR" ] && [ -x "$EXPORT_CEF_DIR" ] \
        || fail "CEF dist not found at $CEF_DIST — 'cargo install export-cef-dir', then re-run (or set CEF_DIST to an existing export)"
    echo "# fetching the pinned CEF dist via export-cef-dir into $CEF_DIST …"
    "$EXPORT_CEF_DIR" --force "$CEF_DIST" || fail "export-cef-dir failed (network / CDN $CEF_CDN)"
fi
[ -f "$CEF_DIST/archive.json" ] || fail "no archive.json in $CEF_DIST — not an export-cef-dir/download-cef export (set CEF_DIST correctly)"
[ -f "$CEF_DIST/libcef.so" ] || fail "no libcef.so in $CEF_DIST — incomplete CEF dist"




dist_name="$(sed -n 's/.*"name"[[:space:]]*:[[:space:]]*"\([^"]*\)".*/\1/p' "$CEF_DIST/archive.json")"
dist_sha1="$(sed -n 's/.*"sha1"[[:space:]]*:[[:space:]]*"\([^"]*\)".*/\1/p' "$CEF_DIST/archive.json")"
[ "$dist_name" = "$PIN_ARCHIVE" ] || fail "CEF dist archive name mismatch: got '$dist_name', pinned '$PIN_ARCHIVE' (wrong/stale CEF_DIST — the pin and the helper's cef crate version move together)"
[ "$dist_sha1" = "$PIN_SHA1" ] || fail "CEF dist archive.json sha1 mismatch: got '$dist_sha1', pinned '$PIN_SHA1'"




tarball="$(dirname "$CEF_DIST")/$PIN_ARCHIVE"
[ -f "$tarball" ] || fail "pinned tarball not found at $tarball — export-cef-dir keeps it beside the export; re-fetch with 'export-cef-dir --force $CEF_DIST' (CDN: $CEF_CDN)"
echo "# verifying the pinned tarball (sha1 + sha256) …"
got_sha1="$("$SHA1SUM" "$tarball" | cut -d' ' -f1)"
got_sha256="$("$SHA256SUM" "$tarball" | cut -d' ' -f1)"
[ "$got_sha1" = "$PIN_SHA1" ] || fail "tarball sha1 mismatch: got $got_sha1, pinned $PIN_SHA1"
[ "$got_sha256" = "$PIN_SHA256" ] || fail "tarball sha256 mismatch: got $got_sha256, pinned $PIN_SHA256"
echo "# pin verified: $PIN_ARCHIVE (sha1 $PIN_SHA1, sha256 $PIN_SHA256)"










prefix="${PIN_ARCHIVE%.tar.bz2}"
SHIP_MEMBERS=(
    Release/libcef.so
    Release/chrome-sandbox
    Release/v8_context_snapshot.bin
    Release/libEGL.so
    Release/libGLESv2.so
    Release/libvk_swiftshader.so
    Release/vk_swiftshader_icd.json
    Release/libvulkan.so.1
    Resources/icudtl.dat
    Resources/resources.pak
    Resources/chrome_100_percent.pak
    Resources/chrome_200_percent.pak
    CREDITS.html
    LICENSE.txt
)
echo "# extracting the ship set from the verified tarball (~1.5 GB under \$TMPDIR) …"
members=()
for m in "${SHIP_MEMBERS[@]}"; do members+=("$prefix/$m"); done
mkdir -p "$work/verified"
"$TAR" -xjf "$tarball" -C "$work/verified" --strip-components=1 --wildcards \
    "$prefix/Resources/locales/en-US*.pak" "${members[@]}" \
    || fail "extraction from $tarball failed (a CEF bump renamed/dropped a SHIP_MEMBERS entry? update SHIP_MEMBERS deliberately; archive: $PIN_ARCHIVE, CDN: $CEF_CDN)"
for m in "${SHIP_MEMBERS[@]}"; do
    [ -f "$work/verified/$m" ] || fail "SHIP_MEMBERS entry missing after extraction: $m"
done
[ -f "$work/verified/Resources/locales/en-US.pak" ] || fail "locales/en-US.pak missing from the pinned tarball"




echo "# verifying the build-input dist libcef.so against the verified tarball bytes …"
dist_libcef_sha="$("$SHA256SUM" "$CEF_DIST/libcef.so" | cut -d' ' -f1)"
verified_libcef_sha="$("$SHA256SUM" "$work/verified/Release/libcef.so" | cut -d' ' -f1)"
[ "$dist_libcef_sha" = "$verified_libcef_sha" ] \
    || fail "$CEF_DIST/libcef.so does not match the verified tarball's libcef.so (post-export modification/corruption?) — re-export with 'export-cef-dir --force $CEF_DIST' and re-run"
echo "# build-input libcef.so matches the verified tarball"


echo "# building eclipse (release) …"
( cd "$repo" && "$CARGO" build --release )
echo "# building eclipse-webview (release, CEF_PATH=$CEF_DIST) …"
( cd "$repo/crates/eclipse-webview" && CEF_PATH="$CEF_DIST" "$CARGO" build --release )
[ -f "$repo/target/release/eclipse" ] || fail "missing $repo/target/release/eclipse after the build"
[ -f "$repo/crates/eclipse-webview/target/release/eclipse-webview" ] || fail "missing the built eclipse-webview helper"


guard_out
rm -rf "$OUT"
mkdir -p "$OUT/locales"


printf 'eclipse-webview payload (tools/webview-dist/package-webview.sh) — safe to wipe on re-package\n' \
    > "$OUT/.eclipse-webview-payload"


cp "$repo/target/release/eclipse" "$OUT/eclipse"
cp "$repo/crates/eclipse-webview/target/release/eclipse-webview" "$OUT/eclipse-webview"



"$READELF" -d "$OUT/eclipse-webview" | grep -Eq 'RUNPATH.*\$ORIGIN' \
    || fail "eclipse-webview lacks RUNPATH=\$ORIGIN (a RUSTFLAGS env override replaced crates/eclipse-webview/.cargo/config.toml's rustflags? rebuild without RUSTFLAGS)"
echo "# RUNPATH=\$ORIGIN verified in eclipse-webview"


echo "# stripping libcef.so ($(stat -c%s "$work/verified/Release/libcef.so") bytes) …"
"$STRIP" -o "$OUT/libcef.so" "$work/verified/Release/libcef.so"
stripped_size="$(stat -c%s "$OUT/libcef.so")"



[ "$stripped_size" -ge 200000000 ] && [ "$stripped_size" -le 300000000 ] \
    || fail "stripped libcef.so is $stripped_size bytes — outside the 200–300 MB sanity envelope (M1 reference 256,322,688)"
echo "# libcef.so stripped to $stripped_size bytes"


for m in "${SHIP_MEMBERS[@]}"; do
    [ "$m" = "Release/libcef.so" ] && continue
    cp "$work/verified/$m" "$OUT/$(basename "$m")"
done
chmod 0755 "$OUT/chrome-sandbox"
[ -s "$OUT/LICENSE.txt" ] || fail "shipped LICENSE.txt is empty"





cp "$work/verified/Resources/locales/en-US.pak" "$OUT/locales/"
for stub in "$work/verified/Resources/locales"/en-US_*.pak; do
    [ -f "$stub" ] && cp "$stub" "$OUT/locales/"
done








echo "# packaged-layout smoke (EOF/non-socket handshake; expects exit 2, no engine init) …"
smoke_code=0
( cd "$OUT" && env -u LD_LIBRARY_PATH ./eclipse-webview --ipc-fd=3 3</dev/null ) \
    >"$work/smoke.log" 2>&1 || smoke_code=$?
[ "$smoke_code" = "2" ] || { cat "$work/smoke.log" >&2; fail "packaged helper smoke: expected exit 2 (handshake stage reached, no engine init), got $smoke_code"; }
grep -Eq "no valid Hello|cannot arm the handshake watchdog" "$work/smoke.log" \
    || { cat "$work/smoke.log" >&2; fail "packaged helper smoke: missing the handshake-stage line (ld.so/\$ORIGIN resolution regression?)"; }
echo "# packaged-layout smoke OK (exit 2 at the handshake stage; \$ORIGIN resolved libcef.so)"


total_bytes="$("$DU" -sb "$OUT" | cut -f1)"
echo "# per-file sizes (bytes):"
"$DU" -sb "$OUT"/* | sort -rn | sed 's/^/#   /'
echo "PACKAGED PAYLOAD: $total_bytes bytes ($("$DU" -sh "$OUT" | cut -f1)) — record in AGENTS.md §6 (plan §7 #5 evidence)"
echo "OK: shippable payload assembled at $OUT"
