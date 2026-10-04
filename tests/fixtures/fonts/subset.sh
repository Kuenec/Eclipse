#!/usr/bin/env bash

set -euo pipefail

here="$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)"

fail() {
    echo "ERROR: $*" >&2
    exit 1
}

for tool in curl sha256sum unzip hb-subset; do
    command -v "$tool" >/dev/null || fail "$tool not found (run this inside org.gnome.Sdk//51)"
done

work="$(mktemp -d)"
trap 'rm -rf "$work"' EXIT

subset() {
    local repo="$1" release="$2" sha256="$3" family="$4" text="$5"
    local archive="$work/$release.zip"
    local font="$family/unhinted/ttf/$family-Regular.ttf"
    curl --fail --location --silent --show-error --output "$archive" \
        "https://github.com/notofonts/$repo/releases/download/$release/$release.zip" \
        || fail "could not download $release"
    echo "$sha256  $archive" | sha256sum --check --quiet - \
        || fail "$release does not match the pinned sha256 $sha256"
    unzip -q "$archive" "$font" -d "$work"
    hb-subset --layout-features='*' --text="$text" \
        --output-file="$here/$family-subset.ttf" "$work/$font"
}

subset latin-greek-cyrillic NotoSans-v2.015 \
    0c34df072a3fa7efbb7cbf34950e1f971a4447cffe365d3a359e2d4089b958f5 NotoSans '<>Привет'
subset hebrew NotoSansHebrew-v3.001 \
    df0a71814b4e63644cf40fcc4529111b61266b7a2dafbe95068b29a7520cc3cb NotoSansHebrew 'שלום'
subset arabic NotoSansArabic-v2.013 \
    1301aceaea84c501cf2e6dcfb3182e2328c8eae5725817fcb239672bda7154f1 NotoSansArabic 'ببب'
