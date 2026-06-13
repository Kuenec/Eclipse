#!/usr/bin/env bash
# 2026-06-11: builds the patched ATL framework overlay (`framework-patched`) Eclipse boots
# Roblox against. In-repo successor of the wiped ~/.cache/eclipse/patch-framework.sh —
# see README.md next to this script for the full WHY (Build SUPPORTED_*_BIT_ABIS fields,
# AOSP-shape NetworkRequest$Builder, foreground RunningAppProcessInfo).
#
# Mechanism: multidex first-dex-wins. Output api-impl.jar layout:
#   classes.dex  = javac-patched classes (Build*, NetworkRequest*, ActivityManager*, PowerManager*, LayoutInflater*)
#   classes2.dex = smali-patched View (+ View$OnCapturedPointerListener) = installed View + AOSP pointer-capture API
#   classes3.dex = ATL's original whole api-impl dex
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

# 2026-06-13: smali/baksmali (run via the vendored JDK's java) for the View pointer-capture patch (step 4b).
# Vendored at vendor/toolchain/smali/ so a clean checkout builds with no system install; env-overridable.
JAVA="${JAVA:-$(find_jdk_tool java)}"
BAKSMALI_JAR="${BAKSMALI_JAR:-$repo/vendor/toolchain/smali/baksmali-2.5.2.jar}"
SMALI_JAR="${SMALI_JAR:-$repo/vendor/toolchain/smali/smali-2.5.2.jar}"
[ -n "$JAVA" ] && [ -x "$JAVA" ] || fail "java not found (set JAVA, or vendor a JDK at vendor/toolchain/jdk-*/)"
[ -f "$BAKSMALI_JAR" ] || fail "baksmali not found at $BAKSMALI_JAR (vendored at vendor/toolchain/smali/; set BAKSMALI_JAR, or 'pacman -S smali')"
[ -f "$SMALI_JAR" ] || fail "smali not found at $SMALI_JAR (vendored at vendor/toolchain/smali/; set SMALI_JAR, or 'pacman -S smali')"

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

# --- 1b. guard the patched LayoutInflater (<requestFocus/> fix) against silent regression --
# 2026-06-13: the patch replaces ATL's `throw "<requestFocus /> not supported atm"` in rInflate
# with parseRequestFocus(parser, parent) (consume-and-skip; the engine owns input focus headlessly).
# Fail loudly if that fix is ever reverted, mirroring the Build.java anchor guard above.
li_src="$here/src/android/view/LayoutInflater.java"
[ -f "$li_src" ] || fail "patched LayoutInflater.java missing at $li_src"
grep -qF 'parseRequestFocus(parser, parent);' "$li_src" || fail "patched LayoutInflater.java no longer calls parseRequestFocus — the <requestFocus/> fix regressed"
! grep -qF '<requestFocus /> not supported atm' "$li_src" || fail "patched LayoutInflater.java still throws the old <requestFocus/> 'not supported atm' — the fix regressed"

# --- 2. compile patched sources against the compile-only stubs ---------------------------
# --release 8: dx 1.x accepts class files <= v52. -Xlint:-options silences the
# "release 8 is obsolete" note; real warnings still show.
"$JAVAC" --release 8 -Xlint:-options -d "$work/classes" \
    -sourcepath "$work/gen:$here/src:$here/stubs" \
    "$work/gen/android/os/Build.java" \
    "$here/src/android/net/NetworkRequest.java" \
    "$here/src/android/app/ActivityManager.java" \
    "$here/src/android/os/PowerManager.java" \
    "$here/src/android/view/LayoutInflater.java"

# --- 3. stage ONLY the patched classes (stubs must never reach the dex) ------------------
for pattern in 'android/os/Build*.class' 'android/os/PowerManager*.class' 'android/net/NetworkRequest*.class' 'android/app/ActivityManager*.class' 'android/view/LayoutInflater*.class'; do
    dir="${pattern%/*}"
    mkdir -p "$work/stage/$dir"
    cp "$work/classes/"$pattern "$work/stage/$dir/"
done

# --- 4. dex the javac-patched classes -> classes.dex -------------------------------------
"$DX" --dex --output="$work/jar/classes.dex" "$work/stage"

# --- 4b. smali-patch the INSTALLED View -> classes2.dex ----------------------------------
# 2026-06-13: ATL's installed View omits AOSP's pointer-capture API (View.OnCapturedPointerListener +
# setOnCapturedPointerListener) that Roblox calls in ActivityNativeMain.d1. Adding a *method* needs the
# whole View class, and the repo's vendored View source has DRIFTED from the installed jar (e.g.
# setBackgroundColor is native in vendored, plain-Java installed) — so recompiling vendored re-breaks it.
# Instead, disassemble the AUTHORITATIVE installed View, add ONLY the field + setter + nested interface,
# reassemble. Anchored inserts with exact-count guards (fail loud on drift) mirror the Build.java approach.
unzip -p "$ORIG_FW/api-impl.jar" classes.dex > "$work/stock-classes.dex"
"$JAVA" -jar "$BAKSMALI_JAR" disassemble "$work/stock-classes.dex" -o "$work/smali" >/dev/null
vsm="$work/smali/android/view/View.smali"
[ -f "$vsm" ] || fail "View.smali not found after baksmali of the installed framework"
for a in \
    '.field private on_touch_listener:Landroid/view/View$OnTouchListener;' \
    '.method public setOnClickListener(Landroid/view/View$OnClickListener;)V' \
    '        Landroid/view/View$DeclaredOnClickListener;,'; do
    n="$(grep -cF "$a" "$vsm")" || true
    [ "$n" = "1" ] || fail "View.smali anchor not unique (found $n, expected 1): $a — installed View drifted; update patch-framework.sh"
done
# (i) backing field after on_touch_listener
perl -0pi -e 's{(\.field private on_touch_listener:Landroid/view/View\$OnTouchListener;\n)}{$1\n# ECLIPSE PATCH 2026-06-13: AOSP View.OnCapturedPointerListener backing field (pointer-capture API ATL omits)\n.field private mCapturedPointerListener:Landroid/view/View\$OnCapturedPointerListener;\n}' "$vsm"
# (ii) setter right after setOnClickListener's .end method
perl -0pi -e 's{(\.method public setOnClickListener\(Landroid/view/View\$OnClickListener;\)V.*?\.end method\n)}{$1\n# ECLIPSE PATCH 2026-06-13: AOSP View.setOnCapturedPointerListener (API 26); headless (engine owns pointer input), pure-Java record\n.method public setOnCapturedPointerListener(Landroid/view/View\$OnCapturedPointerListener;)V\n    .registers 2\n\n    iput-object p1, p0, Landroid/view/View;->mCapturedPointerListener:Landroid/view/View\$OnCapturedPointerListener;\n\n    return-void\n.end method\n}s' "$vsm"
# (iii) register the nested class in MemberClasses (reflection completeness)
perl -0pi -e 's{(value = \{\n)(        Landroid/view/View\$DeclaredOnClickListener;,\n)}{$1        Landroid/view/View\$OnCapturedPointerListener;,\n$2}' "$vsm"
grep -qF 'setOnCapturedPointerListener(Landroid/view/View$OnCapturedPointerListener;)V' "$vsm" || fail "View.smali setter insert failed (drift?)"
grep -qF 'mCapturedPointerListener:Landroid/view/View$OnCapturedPointerListener;' "$vsm" || fail "View.smali field insert failed (drift?)"
# assemble ONLY the patched View + the committed nested interface -> classes2.dex (other View$* stay stock)
mkdir -p "$work/smali-view/android/view"
cp "$vsm" "$work/smali-view/android/view/View.smali"
cp "$here/smali/android/view/View\$OnCapturedPointerListener.smali" "$work/smali-view/android/view/"
"$JAVA" -jar "$SMALI_JAR" assemble "$work/smali-view" -o "$work/jar/classes2.dex" >/dev/null

# --- 4c. stock api-impl as classes3.dex; compose the 3-dex overlay jar --------------------
# DexPathList resolves first-dex-wins across classes.dex < classes2.dex < classes3.dex: View resolves from
# the patched classes2.dex, the javac-patched classes from classes.dex, everything else from stock classes3.dex.
cp "$work/stock-classes.dex" "$work/jar/classes3.dex"
(cd "$work/jar" && "$JAR" cf api-impl.jar classes.dex classes2.dex classes3.dex)

# --- 5. install: overlay jar + symlinks to the stock res/natives -------------------------
mkdir -p "$OUT"
cp "$work/jar/api-impl.jar" "$OUT/api-impl.jar"
ln -sfn "$ORIG_FW/framework-res.apk" "$OUT/framework-res.apk"
ln -sfn "$ORIG_FW/natives" "$OUT/natives"

echo "OK: patched framework overlay installed at $OUT"
echo "    classes.dex (javac-patched): $(ls -l "$work/jar/classes.dex" | awk '{print $5}') bytes; classes2.dex (smali View): $(ls -l "$work/jar/classes2.dex" | awk '{print $5}') bytes; classes3.dex (stock): $(ls -l "$work/jar/classes3.dex" | awk '{print $5}') bytes"
echo "    use it with: export ECLIPSE_ANDROID_FRAMEWORK_DIR=\"$OUT\""
