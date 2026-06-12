#!/usr/bin/env bash
# 2026-06-11: builds the patched ATL framework overlay (`framework-patched`) Eclipse boots
# Roblox against. In-repo successor of the wiped ~/.cache/eclipse/patch-framework.sh —
# see README.md next to this script for the full WHY (Build SUPPORTED_*_BIT_ABIS fields,
# AOSP-shape NetworkRequest$Builder, foreground RunningAppProcessInfo).
#
# Mechanism: multidex first-dex-wins. Output api-impl.jar layout:
#   classes.dex  = ONLY the patched classes (Build*, NetworkRequest*, ActivityManager*)
#   classes2.dex = ATL's original whole api-impl dex
# ART's DexPathList resolves each class from the first dex defining it.
set -euo pipefail

here="$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)"
repo="$(cd "$here/../.." && pwd)"

# --- inputs (env-overridable; no user-specific hardcoding) -------------------------------
ATL_SRC="${ATL_SRC:-$repo/vendor/atl/src/api-impl}"
ORIG_FW="${ORIG_FW:-/usr/lib/java/dex/android_translation_layer}"
OUT="${OUT:-${XDG_CACHE_HOME:-$HOME/.cache}/eclipse/framework-patched}"

# --- tool discovery: $JAVAC/$JAR > vendored JDK > PATH; $DX > PATH -----------------------
find_jdk_tool() {
    local tool="$1" cand
    for cand in "$repo"/vendor/toolchain/jdk-*/bin/"$tool"; do
        [ -x "$cand" ] && { echo "$cand"; return; }
    done
    command -v "$tool" || true
}
JAVAC="${JAVAC:-$(find_jdk_tool javac)}"
JAR="${JAR:-$(find_jdk_tool jar)}"
DX="${DX:-$(command -v dx || true)}"

fail() { echo "ERROR: $*" >&2; exit 1; }
[ -n "$JAVAC" ] && [ -x "$JAVAC" ] || fail "javac not found (set JAVAC, or vendor a JDK at vendor/toolchain/jdk-*/)"
[ -n "$JAR" ] && [ -x "$JAR" ] || fail "jar not found (set JAR, or vendor a JDK at vendor/toolchain/jdk-*/)"
[ -n "$DX" ] && [ -x "$DX" ] || fail "dx not found (set DX or install the Android dx tool; d8 is NOT compatible with this script)"
[ -f "$ATL_SRC/android/os/Build.java" ] || fail "ATL api-impl sources not found at $ATL_SRC (set ATL_SRC)"
[ -f "$ORIG_FW/api-impl.jar" ] || fail "stock framework not found at $ORIG_FW (set ORIG_FW; install android-translation-layer)"

work="$(mktemp -d)"
trap 'rm -rf "$work"' EXIT
mkdir -p "$work/gen/android/os" "$work/classes" "$work/stage" "$work/jar"

# --- 1. generate the patched Build.java from the vendored ATL source ---------------------
# Insert the two AOSP split-ABI fields after the unique SUPPORTED_ABIS anchor (matched as
# a FIXED string). Anchor count != 1 means ATL's source drifted -> fail loudly, never guess.
anchor='public static final String[] SUPPORTED_ABIS'
hits="$(grep -cF "$anchor" "$ATL_SRC/android/os/Build.java")" || true
[ "$hits" = "1" ] || fail "Build.java anchor 'SUPPORTED_ABIS' found $hits times (expected 1) — ATL source drifted; update this script"
awk -v anchor="$anchor" '
    { print }
    index($0, anchor) {
        print ""
        print "\t/* ECLIPSE PATCH 2026-06-11: AOSP-standard split-ABI lists (ATL omits them;"
        print "\t   Roblox reads SUPPORTED_64_BIT_ABIS in RobloxApplication.onCreate). Read the"
        print "\t   AOSP properties with x86/x86_64 fallbacks: ATL'\''s SystemProperties seeds"
        print "\t   abilist but not abilist32/64. */"
        print "\tpublic static final String[] SUPPORTED_32_BIT_ABIS = SystemProperties.get(\"ro.product.cpu.abilist32\", \"x86\").split(\",\");"
        print "\tpublic static final String[] SUPPORTED_64_BIT_ABIS = SystemProperties.get(\"ro.product.cpu.abilist64\", \"x86_64\").split(\",\");"
    }
' "$ATL_SRC/android/os/Build.java" > "$work/gen/android/os/Build.java"

# --- 2. compile patched sources against the compile-only stubs ---------------------------
# --release 8: dx 1.x accepts class files <= v52. -Xlint:-options silences the
# "release 8 is obsolete" note; real warnings still show.
"$JAVAC" --release 8 -Xlint:-options -d "$work/classes" \
    -sourcepath "$work/gen:$here/src:$here/stubs" \
    "$work/gen/android/os/Build.java" \
    "$here/src/android/net/NetworkRequest.java" \
    "$here/src/android/app/ActivityManager.java" \
    "$here/src/android/os/PowerManager.java"

# --- 3. stage ONLY the patched classes (stubs must never reach the dex) ------------------
for pattern in 'android/os/Build*.class' 'android/os/PowerManager*.class' 'android/net/NetworkRequest*.class' 'android/app/ActivityManager*.class'; do
    dir="${pattern%/*}"
    mkdir -p "$work/stage/$dir"
    cp "$work/classes/"$pattern "$work/stage/$dir/"
done

# --- 4. dex the patched classes, compose the multidex overlay jar ------------------------
"$DX" --dex --output="$work/jar/classes.dex" "$work/stage"
unzip -p "$ORIG_FW/api-impl.jar" classes.dex > "$work/jar/classes2.dex"
(cd "$work/jar" && "$JAR" cf api-impl.jar classes.dex classes2.dex)

# --- 5. install: overlay jar + symlinks to the stock res/natives -------------------------
mkdir -p "$OUT"
cp "$work/jar/api-impl.jar" "$OUT/api-impl.jar"
ln -sfn "$ORIG_FW/framework-res.apk" "$OUT/framework-res.apk"
ln -sfn "$ORIG_FW/natives" "$OUT/natives"

echo "OK: patched framework overlay installed at $OUT"
echo "    classes.dex (patched): $(ls -l "$work/jar/classes.dex" | awk '{print $5}') bytes; classes2.dex (stock): $(ls -l "$work/jar/classes2.dex" | awk '{print $5}') bytes"
echo "    use it with: export ECLIPSE_ANDROID_FRAMEWORK_DIR=\"$OUT\""
