#!/usr/bin/env bash

set -euo pipefail

here="$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)"

fail() {
    echo "ERROR: $*" >&2
    exit 1
}

[ -n "${FRAMEWORK:-}" ] || fail "set FRAMEWORK to the Eclipse framework directory holding api-impl.jar and art/oat"
[ -n "${BOOTCLASSPATH:-}" ] || fail "set BOOTCLASSPATH to the framework's ART boot jars"
JAVAC="${JAVAC:-$(command -v javac || true)}"
DX="${DX:-$(command -v dx || true)}"
DALVIKVM="${DALVIKVM:-$(command -v dalvikvm || true)}"
for tool in "$JAVAC" "$DX" "$DALVIKVM"; do
    if [ -z "$tool" ] || [ ! -x "$tool" ]; then
        fail "javac, dx and dalvikvm are required (set JAVAC, DX or DALVIKVM)"
    fi
done
api_impl="$FRAMEWORK/api-impl.jar"
boot_image="$FRAMEWORK/art/oat/boot.art"
[ -f "$api_impl" ] || fail "api-impl.jar missing at $api_impl"

work="$(mktemp -d)"
trap 'rm -rf "$work"' EXIT
mkdir -p "$work/classes" "$work/cache" "$work/data"
"$JAVAC" --release 8 -Xlint:-options -Xlint:all -Werror -d "$work/classes" \
    "$here/tests/AppDexoptProbe.java" "$here/src/android/webkit/ValueCallback.java"
"$DX" --dex --output="$work/probe.jar" "$work/classes"

status=0
output="$(env ANDROID_DATA="$work/data" XDG_CACHE_HOME="$work/cache" \
    "$DALVIKVM" \
    -Ximage:"$boot_image" \
    -Xbootclasspath:"$BOOTCLASSPATH" \
    -Xbootclasspath-locations:"$BOOTCLASSPATH" \
    -Xcompiler-option --compiler-filter=verify \
    -verbose:oat \
    -cp "$api_impl:$work/probe.jar" \
    AppDexoptProbe 2>&1 | tr -d '\000')" || status=$?

problems=()
[ "$status" = 0 ] || problems+=("dalvikvm exited with status $status")
grep -qxF 'app-dexopt-probe-ok' <<<"$output" || problems+=("the probe did not report success")
compiles="$(grep -F 'Compiling dex file: ' <<<"$output" || true)"
if grep -qF -- "--dex-file=$api_impl " <<<"$compiles"; then
    problems+=("ART compiled api-impl.jar at runtime instead of using its prebuilt odex")
fi
grep -F -- "--dex-file=$work/probe.jar " <<<"$compiles" \
    | grep -qF -- '--compiler-filter=verify --class-loader-context=PCL[' \
    || problems+=("ART did not compile the app dex with the verify filter and its class-loader context")
for rejection in 'Found duplicate classes' 'ClassLoaderContext classpath size mismatch'; do
    if grep -qF "$rejection" <<<"$output"; then
        problems+=("ART rejected the app oat: $rejection")
    fi
done
cached="$(find "$work/cache" -name '*api-impl.jar*' -print)"
[ -z "$cached" ] || problems+=("ART wrote api-impl.jar code to its cache: $cached")

if [ "${#problems[@]}" -gt 0 ]; then
    printf '%s\n' "$output" >&2
    printf 'ERROR: %s\n' "${problems[@]}" >&2
    exit 1
fi
echo "OK: api-impl.jar uses its prebuilt odex; app dex compiles with the verify filter and class-loader context"
