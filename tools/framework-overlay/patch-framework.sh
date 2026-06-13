#!/usr/bin/env bash
# 2026-06-11: builds the patched ATL framework overlay (`framework-patched`) Eclipse boots
# Roblox against. In-repo successor of the wiped ~/.cache/eclipse/patch-framework.sh —
# see README.md next to this script for the full WHY (Build SUPPORTED_*_BIT_ABIS fields,
# AOSP-shape NetworkRequest$Builder, foreground RunningAppProcessInfo).
#
# Mechanism: multidex first-dex-wins. Output api-impl.jar layout:
#   classes.dex  = javac-patched classes (Build*, NetworkRequest*, ActivityManager*, PowerManager*, LayoutInflater*)
#   classes2.dex = smali-patched View (+View$OnCapturedPointerListener) + Display (+Display$Mode) + Activity + Fragment + Vibrator = installed classes + AOSP gaps
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

# --- 4b. smali-patch the INSTALLED View + Display -> classes2.dex -------------------------
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

# Display.getSupportedRefreshRates — Roblox calls it in Activity.onStart (framerate setup); ATL's Display
# omits it. Same drift-proof smali approach: add the method to the AUTHORITATIVE installed Display, returning
# {60.0f} to match Display.getRefreshRate() (which ATL hardcodes to 60.0f). Anchor-guarded like the rest.
dsm="$work/smali/android/view/Display.smali"
[ -f "$dsm" ] || fail "Display.smali not found after baksmali"
n="$(grep -cF '.method public getRefreshRate()F' "$dsm")" || true
[ "$n" = "1" ] || fail "Display.smali getRefreshRate anchor not unique (found $n, expected 1) — installed Display drifted; update patch-framework.sh"
perl -0pi -e 's{(\.method public getRefreshRate\(\)F.*?\.end method\n)}{$1\n# ECLIPSE PATCH 2026-06-13: AOSP Display.getSupportedRefreshRates() (Roblox queries it in Activity.onStart for framerate setup; ATL omits it). Returns {60.0f} to match getRefreshRate above.\n.method public getSupportedRefreshRates()[F\n    .locals 3\n\n    const/4 v0, 0x1\n\n    new-array v0, v0, [F\n\n    const/4 v1, 0x0\n\n    const/high16 v2, 0x42700000    # 60.0f\n\n    aput v2, v0, v1\n\n    return-object v0\n.end method\n}s' "$dsm"
grep -qF 'getSupportedRefreshRates()[F' "$dsm" || fail "Display.smali getSupportedRefreshRates insert failed (drift?)"
# Display.getMode() -> Display$Mode — Roblox queries it in ActivityNativeMain.onResume startup; ATL omits
# both getMode and the Mode nested class. Build a Display$Mode (committed smali, assembled below) from the
# installed Display's window_width/window_height statics + 60.0f. Anchor on the unique getWidth()I method.
n="$(grep -cF '.method public getWidth()I' "$dsm")" || true
[ "$n" = "1" ] || fail "Display.smali getWidth anchor not unique (found $n, expected 1) — installed Display drifted; update patch-framework.sh"
! grep -qF 'getMode()Landroid/view/Display$Mode;' "$dsm" || fail "Display.smali already declares getMode — installed Display drifted; update patch-framework.sh"
perl -0pi -e 's{(\.method public getWidth\(\)I.*?\.end method\n)}{$1\n# ECLIPSE PATCH 2026-06-13: AOSP Display.getMode() (Roblox onResume startup; ATL omits it + Display\$Mode). Build a Mode from window_width/window_height + 60.0f (consistent with getWidth/getHeight/getRefreshRate).\n.method public getMode()Landroid/view/Display\$Mode;\n    .locals 5\n\n    new-instance v0, Landroid/view/Display\$Mode;\n\n    const/4 v1, 0x0\n\n    sget v2, Landroid/view/Display;->window_width:I\n\n    sget v3, Landroid/view/Display;->window_height:I\n\n    const/high16 v4, 0x42700000    # 60.0f\n\n    invoke-direct {v0, v1, v2, v3, v4}, Landroid/view/Display\$Mode;-><init>(IIIF)V\n\n    return-object v0\n.end method\n}s' "$dsm"
grep -qF 'getMode()Landroid/view/Display$Mode;' "$dsm" || fail "Display.smali getMode insert failed (drift?)"

# 2026-06-13: androidx create-phase ON_CREATE dispatch. Roblox's ActivityNativeMain extends androidx
# ComponentActivity, whose LifecycleRegistry must receive Lifecycle.Event.ON_CREATE during the
# activity's create phase (AOSP: performCreate -> dispatchActivityCreated -> Fragment.onActivityCreated,
# all BEFORE onStart). ATL dispatches NO create-phase fragment hook: Activity.onCreate only loops
# fragment.onCreate(), Activity.onPostCreate is a no-op, and base Fragment has no onActivityCreated.
# So the androidx ReportFragment (injected via getFragmentManager().add() during the onCreate super-
# chain — ATL's FragmentTransaction.add DOES populate activity.fragments) never gets onActivityCreated;
# the FIRST event the registry sees is ReportFragment.onStart -> handleLifecycleEvent(ON_START), which
# advances to STARTED and back-fills ON_CREATE to lagging observers while currentState is already
# STARTED. An observer's registerForActivityResult (ActivityResultRegistry requires state < STARTED at
# register time) then throws IllegalStateException "must call register before STARTED".
# Fix (drift-proof baksmali, like View/Display): (Fragment) add the AOSP base no-op
# onActivityCreated(Bundle) so the dispatch call resolves and androidx's ReportFragment @Override is
# invoked; (Activity) make onPostCreate(Bundle) iterate activity.fragments calling
# onActivityCreated(savedInstanceState). onPostCreate (driven by Eclipse BETWEEN onCreate and onStart,
# matching AOSP) runs AFTER the full onCreate super-chain has injected the ReportFragment, and BEFORE
# onStart — so ON_CREATE is dispatched while the registry is at CREATED. NOT error suppression: the
# registry legitimately reaches CREATED first, so registerForActivityResult passes its guard.
fsm="$work/smali/android/app/Fragment.smali"
[ -f "$fsm" ] || fail "Fragment.smali not found after baksmali"
n="$(grep -cF '.method public onCreate(Landroid/os/Bundle;)V' "$fsm")" || true
[ "$n" = "1" ] || fail "Fragment.smali onCreate anchor not unique (found $n, expected 1) — installed Fragment drifted; update patch-framework.sh"
! grep -qF 'onActivityCreated(Landroid/os/Bundle;)V' "$fsm" || fail "Fragment.smali already declares onActivityCreated — installed Fragment drifted; update patch-framework.sh"
# AOSP base android.app.Fragment.onActivityCreated(Bundle) is an empty hook; add it (registers 2 = this + Bundle).
perl -0pi -e 's{(\.method public onCreate\(Landroid/os/Bundle;\)V.*?\.end method\n)}{$1\n# ECLIPSE PATCH 2026-06-13: AOSP base Fragment.onActivityCreated(Bundle) hook (empty); androidx ReportFragment \@Overrides it to dispatch Lifecycle.Event.ON_CREATE. ATL omitted it.\n.method public onActivityCreated(Landroid/os/Bundle;)V\n    .registers 2\n\n    return-void\n.end method\n}s' "$fsm"
grep -qF 'onActivityCreated(Landroid/os/Bundle;)V' "$fsm" || fail "Fragment.smali onActivityCreated insert failed (drift?)"

asm="$work/smali/android/app/Activity.smali"
[ -f "$asm" ] || fail "Activity.smali not found after baksmali"
# Guard on the EXACT current no-op onPostCreate body (full multi-line match; fail loud on installed drift).
# `grep -F` splits a multi-line pattern into an OR of lines, so it cannot verify a whole body — use a
# perl whole-file substring check instead (the perl substitution below is itself a no-op on drift, but
# this turns that into a loud failure rather than the later back-check's quieter one).
ANCHOR_PC=$'.method protected onPostCreate(Landroid/os/Bundle;)V\n    .registers 4\n\n    const-string v0, "Activity"\n\n    const-string v1, "- onPostCreate - yay!"\n\n    invoke-static {v0, v1}, Landroid/util/Slog;->i(Ljava/lang/String;Ljava/lang/String;)I\n\n    return-void\n.end method'
n="$(grep -cF '.method protected onPostCreate(Landroid/os/Bundle;)V' "$asm")" || true
[ "$n" = "1" ] || fail "Activity.smali onPostCreate anchor not unique (found $n, expected 1) — installed Activity drifted; update patch-framework.sh"
ANCHOR_PC="$ANCHOR_PC" perl -0777 -ne 'exit((index($_, $ENV{ANCHOR_PC}) >= 0) ? 0 : 1)' "$asm" || fail "Activity.smali onPostCreate body changed from the expected no-op — installed Activity drifted; update patch-framework.sh"
! grep -qF 'onActivityCreated(Landroid/os/Bundle;)V' "$asm" || fail "Activity.smali already dispatches onActivityCreated — installed Activity drifted; update patch-framework.sh"
# Replace the no-op onPostCreate with one that dispatches Fragment.onActivityCreated(savedInstanceState).
# Mirrors the installed onCreate fragment loop (registers v0/v1, p1=Bundle), substituting onActivityCreated.
perl -0pi -e 's{\.method protected onPostCreate\(Landroid/os/Bundle;\)V\n    \.registers 4\n\n    const-string v0, "Activity"\n\n    const-string v1, "- onPostCreate - yay!"\n\n    invoke-static \{v0, v1\}, Landroid/util/Slog;->i\(Ljava/lang/String;Ljava/lang/String;\)I\n\n    return-void\n\.end method}{.method protected onPostCreate(Landroid/os/Bundle;)V\n    .registers 4\n\n    const-string v0, "Activity"\n\n    const-string v1, "- onPostCreate - yay!"\n\n    invoke-static \{v0, v1\}, Landroid/util/Slog;->i(Ljava/lang/String;Ljava/lang/String;)I\n\n    # ECLIPSE PATCH 2026-06-13: dispatch Fragment.onActivityCreated(savedInstanceState) (AOSP create-\n    # phase hook ATL omits) so androidx ReportFragment fires Lifecycle.Event.ON_CREATE while the\n    # LifecycleRegistry is at CREATED, BEFORE onStart dispatches ON_START. Eclipse drives onPostCreate\n    # between onCreate and onStart, after the onCreate super-chain has injected the ReportFragment.\n    iget-object v0, p0, Landroid/app/Activity;->fragments:Ljava/util/List;\n\n    invoke-interface \{v0\}, Ljava/util/List;->iterator()Ljava/util/Iterator;\n\n    move-result-object v1\n\n    :goto_pc\n    invoke-interface \{v1\}, Ljava/util/Iterator;->hasNext()Z\n\n    move-result v0\n\n    if-eqz v0, :cond_pc\n\n    invoke-interface \{v1\}, Ljava/util/Iterator;->next()Ljava/lang/Object;\n\n    move-result-object v0\n\n    check-cast v0, Landroid/app/Fragment;\n\n    invoke-virtual \{v0, p1\}, Landroid/app/Fragment;->onActivityCreated(Landroid/os/Bundle;)V\n\n    goto :goto_pc\n\n    :cond_pc\n    return-void\n.end method}s' "$asm"
grep -qF 'invoke-virtual {v0, p1}, Landroid/app/Fragment;->onActivityCreated(Landroid/os/Bundle;)V' "$asm" || fail "Activity.smali onPostCreate dispatch insert failed (drift?)"

# Vibrator.cancel() no-op — Roblox calls it on a Timer thread (caught by its own handler, non-fatal noise);
# ATL's Vibrator (hasVibrator/vibrate only) omits it. Eclipse has no vibration device, so cancel is a no-op,
# matching the no-vibration-device backing. Anchor on the unique vibrate(J)V.
vibsm="$work/smali/android/os/Vibrator.smali"
[ -f "$vibsm" ] || fail "Vibrator.smali not found after baksmali"
n="$(grep -cF '.method public vibrate(J)V' "$vibsm")" || true
[ "$n" = "1" ] || fail "Vibrator.smali vibrate(J)V anchor not unique (found $n, expected 1) — installed Vibrator drifted; update patch-framework.sh"
! grep -qF '.method public cancel()V' "$vibsm" || fail "Vibrator.smali already declares cancel — installed Vibrator drifted; update patch-framework.sh"
perl -0pi -e 's{(\.method public vibrate\(J\)V.*?\.end method\n)}{$1\n# ECLIPSE PATCH 2026-06-13: AOSP Vibrator.cancel() no-op (Roblox calls it on a Timer thread; ATL omits it). No vibration device -> nothing to cancel.\n.method public cancel()V\n    .registers 1\n\n    return-void\n.end method\n}s' "$vibsm"
grep -qF '.method public cancel()V' "$vibsm" || fail "Vibrator.smali cancel insert failed (drift?)"

# assemble View(+nested) + Display(+Mode) + Activity + Fragment + Vibrator -> classes2.dex
mkdir -p "$work/smali-view/android/view" "$work/smali-view/android/app" "$work/smali-view/android/os"
cp "$vsm" "$work/smali-view/android/view/View.smali"
cp "$dsm" "$work/smali-view/android/view/Display.smali"
cp "$here/smali/android/view/View\$OnCapturedPointerListener.smali" "$work/smali-view/android/view/"
cp "$here/smali/android/view/Display\$Mode.smali" "$work/smali-view/android/view/"
cp "$asm" "$work/smali-view/android/app/Activity.smali"
cp "$fsm" "$work/smali-view/android/app/Fragment.smali"
cp "$vibsm" "$work/smali-view/android/os/Vibrator.smali"
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
echo "    classes.dex (javac-patched): $(ls -l "$work/jar/classes.dex" | awk '{print $5}') bytes; classes2.dex (smali View+Display+Activity+Fragment): $(ls -l "$work/jar/classes2.dex" | awk '{print $5}') bytes; classes3.dex (stock): $(ls -l "$work/jar/classes3.dex" | awk '{print $5}') bytes"
echo "    use it with: export ECLIPSE_ANDROID_FRAMEWORK_DIR=\"$OUT\""
