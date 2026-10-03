#!/usr/bin/env bash

set -euo pipefail

LIMIT_BYTES=106000000

fail() { echo "ERROR: $*" >&2; exit 1; }

[ $# -eq 2 ] || fail "usage: $0 REPO REF"
repo="$1"
ref="$2"
command -v ostree >/dev/null || fail "ostree not found (install it and re-run)"

listing="$(ostree --repo="$repo" ls -R "$ref" /)" || fail "cannot list $ref in $repo"
installed="$(awk '$1 ~ /^-/ { total += int(($4 + 511) / 512) * 512 } END { printf "%.0f", total }' <<<"$listing")"

megabytes() { awk -v bytes="$1" 'BEGIN { printf "%.1f MB", bytes / 1000000 }'; }

if [ "$installed" -gt "$LIMIT_BYTES" ]; then
    fail "$ref installs $installed bytes ($(megabytes "$installed")), above LIMIT_BYTES=$LIMIT_BYTES ($(megabytes "$LIMIT_BYTES")); raise it in $0 only with the reason in the commit message"
fi
echo "$ref installs $installed bytes ($(megabytes "$installed")), within LIMIT_BYTES=$LIMIT_BYTES ($(megabytes "$LIMIT_BYTES"))"
