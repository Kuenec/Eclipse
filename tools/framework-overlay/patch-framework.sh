#!/usr/bin/env bash
# 2026-06-11: builds the patched ATL framework overlay (`framework-patched`) Eclipse boots
# Roblox against. In-repo successor of the wiped ~/.cache/eclipse/patch-framework.sh —
# see README.md next to this script for the full WHY (Build SUPPORTED_*_BIT_ABIS fields,
# AOSP-shape NetworkRequest$Builder, foreground RunningAppProcessInfo).
#
# Mechanism: multidex first-dex-wins. Output api-impl.jar layout:
#   classes.dex  = javac-patched classes (Build*, NetworkRequest*, ActivityManager*, PowerManager*, LayoutInflater*, KeyguardManager*)
#   classes2.dex = smali-patched View (+View$OnCapturedPointerListener) + Display (+Display$Mode) + Activity + Fragment + Vibrator + AutofillManager + CookieManager + JobParameters + Paint = installed classes + AOSP gaps
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

# --- 1c. guard the patched ValueCallback (must be an interface, not a class) ---------------
# 2026-06-14: android.webkit.ValueCallback is an INTERFACE in AOSP, but ATL ships it as an empty
# `public class`. Roblox's ql.b `implements ValueCallback`, so a class form makes ART throw
# IncompatibleClassChangeError at CookieProtocol.<init> (auth/cookie path) and wedge the main-looper
# pump (freezing the winit event loop -> no host input). Fail loudly if the patch regresses.
vc_src="$here/src/android/webkit/ValueCallback.java"
[ -f "$vc_src" ] || fail "patched ValueCallback.java missing at $vc_src"
grep -qE 'public[[:space:]]+interface[[:space:]]+ValueCallback' "$vc_src" || fail "patched ValueCallback.java is not an interface — the IncompatibleClassChangeError fix regressed"

# --- 1d. guard the patched KeyguardManager (isDeviceSecure) against silent regression ------
# 2026-07-01: Roblox's security/device-check path calls KeyguardManager.isDeviceSecure() (AOSP API 23+);
# ATL omits it, and the resulting NoSuchMethodError propagates as a pending exception that trips ART's
# `runtime.cc:650 No pending exception expected` FATAL abort (EXIT=134) at the login screen. Fail loudly
# if the added method is ever removed.
kg_src="$here/src/android/app/KeyguardManager.java"
[ -f "$kg_src" ] || fail "patched KeyguardManager.java missing at $kg_src"
grep -qF 'public boolean isDeviceSecure()' "$kg_src" || fail "patched KeyguardManager.java no longer declares isDeviceSecure() — the NoSuchMethodError fix regressed"

# --- 1f. guard the M4 JavascriptInterface annotation + the EclipseBridgeProbe test class -----
# 2026-07-09 (web-engine plan M4): @JavascriptInterface must be a RUNTIME-retention annotation for
# the reflective bridge filtering (framework.rs) + the real app's bridge methods to resolve; the
# EclipseBridgeProbe is the inert __webview-test probe (@JavascriptInterface echo + ValueCallback).
ji_src="$here/src/android/webkit/JavascriptInterface.java"
[ -f "$ji_src" ] || fail "M4 JavascriptInterface.java missing at $ji_src"
grep -qF 'public @interface JavascriptInterface' "$ji_src" || fail "JavascriptInterface.java is not an @interface — the M4 bridge annotation regressed"
grep -qF 'RetentionPolicy.RUNTIME' "$ji_src" || fail "JavascriptInterface.java is not RUNTIME-retention — reflection filtering would fail"
bp_src="$here/src/android/webkit/EclipseBridgeProbe.java"
[ -f "$bp_src" ] || fail "M4 EclipseBridgeProbe.java missing at $bp_src"
grep -qF '@JavascriptInterface' "$bp_src" || fail "EclipseBridgeProbe.java lost its @JavascriptInterface echo — __webview-test bridge leg regressed"

# --- 1g. guard the M6 EclipseWebViewClientProbe (the __webview-test Looper-contract probe) ------
# 2026-07-16 (web-engine plan M6): the probe is __webview-test's driven WebViewClient. It carries
# the app's real callback shape — `new Handler()` (Roblox: SwipeRefreshLayout.setRefreshing ->
# View.startAnimation -> Animation.start -> new Handler()) — which THROWS on a Looper-less dispatch
# thread, PLUS the AOSP UI-thread assertion (without which "just Looper.prepare() on the upcall
# thread", the tempting wrong fix, would pass green while silently swallowing every post). Lose
# either line and the harness goes blind to the 2026-07-16 root cause exactly as the stock
# `new WebViewClient()` did.
wvcp_src="$here/src/android/webkit/EclipseWebViewClientProbe.java"
[ -f "$wvcp_src" ] || fail "M6 EclipseWebViewClientProbe.java missing at $wvcp_src"
grep -qF 'new Handler();' "$wvcp_src" || fail "EclipseWebViewClientProbe.java no longer constructs a Handler — __webview-test would go blind to the Looper-less-dispatch class (2026-07-16)"
grep -qF 'Looper.myLooper() != Looper.getMainLooper()' "$wvcp_src" || fail "EclipseWebViewClientProbe.java lost its UI-thread assertion — a prepared-but-undrained Looper on the upcall thread would pass this guard green"
grep -qF 'onPageStarted(WebView view, String url, Bitmap favicon)' "$wvcp_src" || fail "EclipseWebViewClientProbe.java no longer overrides the AOSP 3-arg onPageStarted — the M6 state-0 dispatch would go unpinned"
grep -qF 'onPageFinished(WebView view, String url)' "$wvcp_src" || fail "EclipseWebViewClientProbe.java no longer overrides onPageFinished — half the confirmed 2026-07-16 defect would go unpinned"

# --- 1e. compile against the VENDORED com.android.internal.R (javac constant-inlining guard) --
# 2026-07-02: javac inlines `static final int` constants from compile inputs into the emitted
# bytecode. A hand-written stub R.java with placeholder values (attr.id = 0, attr.theme = 0)
# compiled LayoutInflater.parseInclude's <include android:id> override into
# obtainStyledAttributes(attrs, new int[]{0}) — attribute id 0 never resolves, the include-tag id
# was never applied to the included root, and the challenge fragment's findViewById(R.id.toolbar1/2)
# returned null (RobloxToolbar.setVisibility NPE at yh.d.onCreateView). The stub is GONE; the
# authoritative vendored R.java (what the stock classes3.dex was compiled with) is a compile input
# instead, so any internal-R constant an overlay source uses inlines with its real value. Its
# R*.class files are compile-only (the step-3 stage whitelist keeps them out of the dex). Guard the
# two load-bearing constants against vendored drift, mirroring the Build.java anchor guard.
r_src="$ATL_SRC/com/android/internal/R.java"
[ -f "$r_src" ] || fail "vendored com/android/internal/R.java not found at $r_src (set ATL_SRC)"
grep -qF 'public static final int id=0x010100d0;' "$r_src" || fail "vendored internal R.attr.id != 0x010100d0 — ATL source drifted; re-verify the overlay's inlined constants"
grep -qF 'public static final int theme=0x01010000;' "$r_src" || fail "vendored internal R.attr.theme != 0x01010000 — ATL source drifted; re-verify the overlay's inlined constants"
[ ! -e "$here/stubs/com/android/internal/R.java" ] || fail "stub com/android/internal/R.java re-appeared — javac would inline its placeholder constants into the overlay dex (the 2026-07-02 include-id NPE class); delete it (the vendored R.java is the compile input)"

# --- 2. compile patched sources against the compile-only stubs ---------------------------
# --release 8: dx 1.x accepts class files <= v52. -Xlint:-options silences the
# "release 8 is obsolete" note; real warnings still show.
"$JAVAC" --release 8 -Xlint:-options -d "$work/classes" \
    -sourcepath "$work/gen:$here/src:$here/stubs" \
    "$work/gen/android/os/Build.java" \
    "$here/src/android/net/NetworkRequest.java" \
    "$here/src/android/app/ActivityManager.java" \
    "$here/src/android/os/PowerManager.java" \
    "$here/src/android/view/LayoutInflater.java" \
    "$here/src/android/webkit/ValueCallback.java" \
    "$here/src/android/webkit/JavascriptInterface.java" \
    "$here/src/android/webkit/EclipseBridgeProbe.java" \
    "$here/src/android/webkit/EclipseWebViewClientProbe.java" \
    "$here/src/android/app/KeyguardManager.java" \
    "$r_src"

# --- 3. stage ONLY the patched classes (stubs must never reach the dex) ------------------
for pattern in 'android/os/Build*.class' 'android/os/PowerManager*.class' 'android/net/NetworkRequest*.class' 'android/app/ActivityManager*.class' 'android/view/LayoutInflater*.class' 'android/webkit/ValueCallback*.class' 'android/webkit/JavascriptInterface*.class' 'android/webkit/EclipseBridgeProbe*.class' 'android/webkit/EclipseWebViewClientProbe*.class' 'android/app/KeyguardManager*.class'; do
    dir="${pattern%/*}"
    mkdir -p "$work/stage/$dir"
    cp "$work/classes/"$pattern "$work/stage/$dir/"
done

# --- 3a. the M6 compile-only stubs must never be dexed, and must never carry constants ---------
# 2026-07-16: javac DOES emit .class files for sourcepath-resolved stubs into $work/classes
# (verified). classes.dex is FIRST-dex-wins, so a staged stub would SHADOW the real installed class
# and silently gut it — a stub android/os/Looper would take out the ENTIRE main-Looper pump, and a
# stub WebViewClient would shadow the classes2 shadow that carries the M6 3-arg onPageStarted. The
# step-3 whitelist excludes them by construction; verify DIRECTLY, so a future whitelist edit fails
# loud. Separately: guard 1e's real lesson is that javac INLINES `static final` constants from
# compile inputs — these stubs declare none, and must keep declaring none.
for forbidden in 'android/webkit/WebView.class' 'android/webkit/WebViewClient.class' \
                 'android/os/Handler.class' 'android/os/Looper.class'; do
    [ ! -e "$work/stage/$forbidden" ] || fail "compile-only stub $forbidden was staged into classes.dex — it would SHADOW the real class (first-dex-wins); fix the step-3 stage whitelist"
done
for stub in android/webkit/WebView.java android/webkit/WebViewClient.java \
            android/os/Handler.java android/os/Looper.java; do
    [ -f "$here/stubs/$stub" ] || fail "M6 compile-only stub $stub missing — EclipseWebViewClientProbe would not compile"
    ! grep -qE 'static[[:space:]]+final' "$here/stubs/$stub" || fail "M6 stub $stub declares a constant — javac would INLINE its placeholder value into the overlay dex (the 2026-07-02 guard-1e class)"
done

# --- 4. dex the javac-patched classes -> classes.dex -------------------------------------
"$DX" --dex --output="$work/jar/classes.dex" "$work/stage"

# --- 4a. verify the DEXED LayoutInflater carries the REAL inlined internal-R constants ----
# 2026-07-02: the dex-level check that catches the constant-inlining bug class directly (a source
# grep cannot see what javac folded). parseInclude must request android:id (0x010100d0) and
# createView android:theme (0x01010000); baksmali renders the inlined literals without the leading
# zero. With the old zero-constant stub these greps fail (the array cell is `aput v5(=0)`).
"$JAVA" -jar "$BAKSMALI_JAR" disassemble "$work/jar/classes.dex" -o "$work/smali-check" >/dev/null
lism="$work/smali-check/android/view/LayoutInflater.smali"
[ -f "$lism" ] || fail "LayoutInflater.smali not found in the built classes.dex"
grep -qF '0x10100d0' "$lism" || fail "dexed LayoutInflater lost the inlined android:id constant (0x010100d0) — the <include android:id> override would silently drop (2026-07-02 RobloxToolbar NPE class)"
grep -qF '0x1010000' "$lism" || fail "dexed LayoutInflater lost the inlined android:theme constant (0x01010000) — createView's android:theme handling would silently drop"

# 2026-07-16 (M6): the probe must reach the dex with BOTH its Handler constructions and the AOSP
# 3-arg onPageStarted override — the latter is ALSO the coupling proof that the compile-only
# WebViewClient stub agrees with the 3-arg form the classes2 shadow lands (the §(2) post-append
# grep below keys the IDENTICAL descriptor literal; if the two ever disagree, the probe's method
# stops overriding and silently never runs).
wvcpsm="$work/smali-check/android/webkit/EclipseWebViewClientProbe.smali"
[ -f "$wvcpsm" ] || fail "EclipseWebViewClientProbe.smali not in the built classes.dex — the __webview-test Looper-contract probe did not stage"
grep -qF 'Landroid/os/Handler;-><init>()V' "$wvcpsm" || fail "dexed EclipseWebViewClientProbe lost the no-arg Handler construction — the 2026-07-16 Looper-less-dispatch guard would not fire"
grep -qF 'Landroid/os/Looper;->getMainLooper()' "$wvcpsm" || fail "dexed EclipseWebViewClientProbe lost its UI-thread assertion"
grep -qF 'onPageStarted(Landroid/webkit/WebView;Ljava/lang/String;Landroid/graphics/Bitmap;)V' "$wvcpsm" || fail "dexed EclipseWebViewClientProbe lost the AOSP 3-arg onPageStarted override — internalLoadChanged's state-0 dispatch would miss it (and the stub has drifted from the classes2 shadow)"

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
# (ii.b) AOSP autofill no-ops after the captured-pointer setter — Roblox's com.roblox.client.RbxKeyboard
# configures the focused login EditText for autofill (View.setAutofillHints(String[]) +
# setImportantForAutofill(int)); ATL's View omits both, so RbxKeyboard's text-input setup throws
# NoSuchMethodError and typing into the field never works. Eclipse has no autofill service -> no-ops.
perl -0pi -e 's{(\.method public setOnCapturedPointerListener\(Landroid/view/View\$OnCapturedPointerListener;\)V.*?\.end method\n)}{$1\n# ECLIPSE PATCH 2026-06-14: AOSP autofill no-ops (RbxKeyboard configures the login EditText for autofill; ATL omits these). No autofill service.\n.method public setAutofillHints([Ljava/lang/String;)V\n    .registers 2\n\n    return-void\n.end method\n\n.method public setImportantForAutofill(I)V\n    .registers 2\n\n    return-void\n.end method\n}s' "$vsm"
grep -qF 'setAutofillHints([Ljava/lang/String;)V' "$vsm" || fail "View.smali setAutofillHints insert failed (drift?)"
grep -qF 'setImportantForAutofill(I)V' "$vsm" || fail "View.smali setImportantForAutofill insert failed (drift?)"

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

# AutofillManager.cancel() no-op — Roblox's com.roblox.client.RbxKeyboard.i() calls it when showing the
# soft keyboard / text input for a focused field (e.g. the LoginV2 username/password fields); ATL's
# AutofillManager (only <init>/registerCallback/unregisterCallback) omits cancel(), so the keyboard-show
# path throws NoSuchMethodError and text entry never sets up (the field focuses but typing does nothing).
# Eclipse has no autofill service, so cancel is a no-op. Anchor on the unique unregisterCallback.
afm="$work/smali/android/view/autofill/AutofillManager.smali"
[ -f "$afm" ] || fail "AutofillManager.smali not found after baksmali"
n="$(grep -cF '.method public unregisterCallback(Landroid/view/autofill/AutofillManager$AutofillCallback;)V' "$afm")" || true
[ "$n" = "1" ] || fail "AutofillManager.smali unregisterCallback anchor not unique (found $n, expected 1) — installed AutofillManager drifted; update patch-framework.sh"
! grep -qF '.method public cancel()V' "$afm" || fail "AutofillManager.smali already declares cancel — installed AutofillManager drifted; update patch-framework.sh"
perl -0pi -e 's{(\.method public unregisterCallback\(Landroid/view/autofill/AutofillManager\$AutofillCallback;\)V.*?\.end method\n)}{$1\n# ECLIPSE PATCH 2026-06-14: AOSP AutofillManager.cancel() no-op (Roblox RbxKeyboard.i() calls it when showing text input for a focused login field; ATL omits it). No autofill service -> nothing to cancel.\n.method public cancel()V\n    .registers 1\n\n    return-void\n.end method\n}s' "$afm"
grep -qF '.method public cancel()V' "$afm" || fail "AutofillManager.smali cancel insert failed (drift?)"
# AutofillManager.requestAutofill(View) no-op — Roblox's RbxKeyboard.i() ALSO calls it (AOSP API 26)
# when configuring the focused login field for autofill; ATL/overlay AutofillManager omits it, so the
# post-login keyboard-setup path throws NoSuchMethodError. No autofill service -> no-op. Anchor on the
# cancel() method inserted just above (unique after that insert).
! grep -qF 'requestAutofill(Landroid/view/View;)V' "$afm" || fail "AutofillManager.smali already declares requestAutofill — drifted; update patch-framework.sh"
perl -0pi -e 's{(\.method public cancel\(\)V.*?\.end method\n)}{$1\n# ECLIPSE PATCH 2026-07-01: AOSP AutofillManager.requestAutofill(View) no-op (Roblox RbxKeyboard.i() requests autofill for a focused login field; ATL omits it). No autofill service -> no-op.\n.method public requestAutofill(Landroid/view/View;)V\n    .registers 2\n\n    return-void\n.end method\n}s' "$afm"
grep -qF 'requestAutofill(Landroid/view/View;)V' "$afm" || fail "AutofillManager.smali requestAutofill insert failed (drift?)"

# === CookieManager real backing (web-engine plan M4, 2026-07-09) =========================
# Roblox's CookieProtocol/.ROBLOSECURITY handoff needs a REAL cookie store. Replace the stock
# no-op bodies (getCookie->"" / setCookie->void / removeAll->void / flush->void) with native calls
# into the session-scoped helper cookie store, add the 3-arg setCookie (real callback, replacing
# the 2026-06-14 fabricated Boolean.TRUE), and declare the six natives. Each body rewrite is a
# whole-method perl replace guarded by a not-already-patched check + a post-insert grep back-check.
csm="$work/smali/android/webkit/CookieManager.smali"
[ -f "$csm" ] || fail "CookieManager.smali not found after baksmali"
! grep -qF 'native_getCookie' "$csm" || fail "CookieManager.smali already carries native_getCookie — drifted; update patch-framework.sh"
# getCookie(url) -> return native_getCookie(url)
perl -0pi -e 's{\.method public getCookie\(Ljava/lang/String;\)Ljava/lang/String;.*?\.end method\n}{.method public getCookie(Ljava/lang/String;)Ljava/lang/String;\n    .registers 3\n\n    invoke-direct {p0, p1}, Landroid/webkit/CookieManager;->native_getCookie(Ljava/lang/String;)Ljava/lang/String;\n\n    move-result-object v0\n\n    return-object v0\n.end method\n}s' "$csm"
grep -qF -- '->native_getCookie(Ljava/lang/String;)Ljava/lang/String;' "$csm" || fail "CookieManager getCookie native-body insert failed (drift?)"
# setCookie(url, value) -> native_setCookie(url, value)  (then INSERT the 3-arg variant after it)
perl -0pi -e 's{\.method public setCookie\(Ljava/lang/String;Ljava/lang/String;\)V.*?\.end method\n}{.method public setCookie(Ljava/lang/String;Ljava/lang/String;)V\n    .registers 3\n\n    invoke-direct {p0, p1, p2}, Landroid/webkit/CookieManager;->native_setCookie(Ljava/lang/String;Ljava/lang/String;)V\n\n    return-void\n.end method\n\n# ECLIPSE PATCH 2026-07-09 (M4): 3-arg setCookie(url, value, ValueCallback<Boolean>) — the REAL success flag routes back through the native, never a fabricated Boolean.TRUE.\n.method public setCookie(Ljava/lang/String;Ljava/lang/String;Landroid/webkit/ValueCallback;)V\n    .registers 4\n\n    invoke-direct {p0, p1, p2, p3}, Landroid/webkit/CookieManager;->native_setCookie(Ljava/lang/String;Ljava/lang/String;Landroid/webkit/ValueCallback;)V\n\n    return-void\n.end method\n}s' "$csm"
grep -qF -- '->native_setCookie(Ljava/lang/String;Ljava/lang/String;)V' "$csm" || fail "CookieManager setCookie(2-arg) native-body insert failed (drift?)"
grep -qF 'setCookie(Ljava/lang/String;Ljava/lang/String;Landroid/webkit/ValueCallback;)V' "$csm" || fail "CookieManager setCookie(3-arg) insert failed (drift?)"
# removeAllCookies(cb) -> native_removeAllCookies(cb)
perl -0pi -e 's{\.method public removeAllCookies\(Landroid/webkit/ValueCallback;\)V.*?\.end method\n}{.method public removeAllCookies(Landroid/webkit/ValueCallback;)V\n    .registers 2\n\n    invoke-direct {p0, p1}, Landroid/webkit/CookieManager;->native_removeAllCookies(Landroid/webkit/ValueCallback;)V\n\n    return-void\n.end method\n}s' "$csm"
grep -qF -- '->native_removeAllCookies(Landroid/webkit/ValueCallback;)V' "$csm" || fail "CookieManager removeAllCookies native-body insert failed (drift?)"
# removeSessionCookies(cb) -> native_removeSessionCookies(cb)
perl -0pi -e 's{\.method public removeSessionCookies\(Landroid/webkit/ValueCallback;\)V.*?\.end method\n}{.method public removeSessionCookies(Landroid/webkit/ValueCallback;)V\n    .registers 2\n\n    invoke-direct {p0, p1}, Landroid/webkit/CookieManager;->native_removeSessionCookies(Landroid/webkit/ValueCallback;)V\n\n    return-void\n.end method\n}s' "$csm"
grep -qF -- '->native_removeSessionCookies(Landroid/webkit/ValueCallback;)V' "$csm" || fail "CookieManager removeSessionCookies native-body insert failed (drift?)"
# flush() -> native_flush()
perl -0pi -e 's{\.method public flush\(\)V.*?\.end method\n}{.method public flush()V\n    .registers 1\n\n    invoke-direct {p0}, Landroid/webkit/CookieManager;->native_flush()V\n\n    return-void\n.end method\n}s' "$csm"
grep -qF -- '->native_flush()V' "$csm" || fail "CookieManager flush native-body insert failed (drift?)"
# Declare the six natives (appended; a native decl has no body — order is irrelevant in smali).
cat >> "$csm" <<'ECLIPSE_CM_NATIVES'

# ECLIPSE PATCH 2026-07-09 (M4): CookieManager natives (backed by Eclipse's session-scoped helper store).
.method private native native_getCookie(Ljava/lang/String;)Ljava/lang/String;
.end method

.method private native native_setCookie(Ljava/lang/String;Ljava/lang/String;)V
.end method

.method private native native_setCookie(Ljava/lang/String;Ljava/lang/String;Landroid/webkit/ValueCallback;)V
.end method

.method private native native_removeAllCookies(Landroid/webkit/ValueCallback;)V
.end method

.method private native native_removeSessionCookies(Landroid/webkit/ValueCallback;)V
.end method

.method private native native_flush()V
.end method
ECLIPSE_CM_NATIVES

# === WebView + WebSettings shadow (web-engine plan M4, 2026-07-09) ========================
# WebView is stock in classes3; shadow it (+ WebSettings) into classes2.dex with: loadUrl's
# javascript: branch routed to native_evaluateJavascript (removing the full-URL System.out.println
# leak); evaluateJavascript + addJavascriptInterface backed by their natives; and WebSettings'
# honest deliberate UA replacing "GDPR VIOLATION" (a method-return literal, no javac inlining).
wvsm="$work/smali/android/webkit/WebView.smali"
wssm="$work/smali/android/webkit/WebSettings.smali"
[ -f "$wvsm" ] || fail "WebView.smali not found after baksmali"
[ -f "$wssm" ] || fail "WebSettings.smali not found after baksmali"
! grep -qF 'native_evaluateJavascript' "$wvsm" || fail "WebView.smali already carries native_evaluateJavascript — drifted; update patch-framework.sh"
# loadUrl(String): route javascript: URLs to native_evaluateJavascript(widget, script, null) — no full-URL println.
grep -qF 'const-string v2, " - not implemented yet"' "$wvsm" || fail "WebView.smali loadUrl no longer carries the javascript: println (installed shape drifted; update patch-framework.sh)"
perl -0pi -e 's{\.method public loadUrl\(Ljava/lang/String;\)V.*?\.end method\n}{.method public loadUrl(Ljava/lang/String;)V\n    .registers 7\n\n    const-string v0, "javascript:"\n\n    invoke-virtual {p1, v0}, Ljava/lang/String;->startsWith(Ljava/lang/String;)Z\n\n    move-result v0\n\n    iget-wide v1, p0, Landroid/view/View;->widget:J\n\n    if-eqz v0, :cond_eclipse_loadurl_normal\n\n    # ECLIPSE PATCH 2026-07-09 (M4): route the javascript: script to the engine (NO full-URL println leak).\n    const/16 v3, 0xb\n\n    invoke-virtual {p1, v3}, Ljava/lang/String;->substring(I)Ljava/lang/String;\n\n    move-result-object v3\n\n    const/4 v4, 0x0\n\n    invoke-direct {p0, v1, v2, v3, v4}, Landroid/webkit/WebView;->native_evaluateJavascript(JLjava/lang/String;Landroid/webkit/ValueCallback;)V\n\n    return-void\n\n    :cond_eclipse_loadurl_normal\n    invoke-direct {p0, v1, v2, p1}, Landroid/webkit/WebView;->native_loadUrl(JLjava/lang/String;)V\n\n    return-void\n.end method\n}s' "$wvsm"
grep -qF -- '->native_evaluateJavascript(JLjava/lang/String;Landroid/webkit/ValueCallback;)V' "$wvsm" || fail "WebView loadUrl javascript:-route insert failed (drift?)"
! grep -qF 'const-string v2, " - not implemented yet"' "$wvsm" || fail "WebView loadUrl still carries the full-URL println (leak-fix regressed)"
# evaluateJavascript(String, ValueCallback) -> native_evaluateJavascript(widget, script, cb)
perl -0pi -e 's{\.method public evaluateJavascript\(Ljava/lang/String;Landroid/webkit/ValueCallback;\)V.*?\.end method\n}{.method public evaluateJavascript(Ljava/lang/String;Landroid/webkit/ValueCallback;)V\n    .registers 5\n\n    iget-wide v0, p0, Landroid/view/View;->widget:J\n\n    invoke-direct {p0, v0, v1, p1, p2}, Landroid/webkit/WebView;->native_evaluateJavascript(JLjava/lang/String;Landroid/webkit/ValueCallback;)V\n\n    return-void\n.end method\n}s' "$wvsm"
# addJavascriptInterface(Object, String) -> native_addJavascriptInterface(widget, object, name)
perl -0pi -e 's{\.method public addJavascriptInterface\(Ljava/lang/Object;Ljava/lang/String;\)V.*?\.end method\n}{.method public addJavascriptInterface(Ljava/lang/Object;Ljava/lang/String;)V\n    .registers 5\n\n    iget-wide v0, p0, Landroid/view/View;->widget:J\n\n    invoke-direct {p0, v0, v1, p1, p2}, Landroid/webkit/WebView;->native_addJavascriptInterface(JLjava/lang/Object;Ljava/lang/String;)V\n\n    return-void\n.end method\n}s' "$wvsm"
grep -qF -- '->native_addJavascriptInterface(JLjava/lang/Object;Ljava/lang/String;)V' "$wvsm" || fail "WebView addJavascriptInterface native-body insert failed (drift?)"
# Declare the two new WebView natives.
cat >> "$wvsm" <<'ECLIPSE_WV_NATIVES'

# ECLIPSE PATCH 2026-07-09 (M4): WebView JS-bridge / evaluateJavascript natives.
.method private native native_evaluateJavascript(JLjava/lang/String;Landroid/webkit/ValueCallback;)V
.end method

.method private native native_addJavascriptInterface(JLjava/lang/Object;Ljava/lang/String;)V
.end method
ECLIPSE_WV_NATIVES
# WebSettings: honest deliberate UA (getUserAgentString + getDefaultUserAgent) replacing "GDPR VIOLATION".
grep -qF 'const-string v0, "GDPR VIOLATION"' "$wssm" || fail "WebSettings.smali no longer returns \"GDPR VIOLATION\" (installed shape drifted; update patch-framework.sh)"
ECLIPSE_UA='Mozilla/5.0 (X11; Linux x86_64) AppleWebKit/537.36 (KHTML, like Gecko) Chrome/149.0.0.0 Safari/537.36 Eclipse-WebView/149.0.6'
ECLIPSE_UA="$ECLIPSE_UA" perl -0pi -e 'my $ua=$ENV{ECLIPSE_UA}; s{\.method public getUserAgentString\(\)Ljava/lang/String;.*?\.end method\n}{".method public getUserAgentString()Ljava/lang/String;\n    .registers 2\n\n    # ECLIPSE PATCH 2026-07-09 (M4): the honest deliberate UA (MUST byte-match engine.rs ECLIPSE_USER_AGENT).\n    const-string v0, \"$ua\"\n\n    return-object v0\n.end method\n"}se' "$wssm"
ECLIPSE_UA="$ECLIPSE_UA" perl -0pi -e 'my $ua=$ENV{ECLIPSE_UA}; s{\.method public static getDefaultUserAgent\(Landroid/content/Context;\)Ljava/lang/String;.*?\.end method\n}{".method public static getDefaultUserAgent(Landroid/content/Context;)Ljava/lang/String;\n    .registers 2\n\n    const-string v0, \"$ua\"\n\n    return-object v0\n.end method\n"}se' "$wssm"
grep -qF 'Eclipse-WebView/149.0.6' "$wssm" || fail "WebSettings honest-UA insert failed (drift?)"
! grep -qF 'GDPR VIOLATION' "$wssm" || fail "WebSettings still returns \"GDPR VIOLATION\" (honest-UA fix incomplete)"

# === WebViewClient 3-arg onPageStarted + WebView.internalLoadChanged dispatch (M6, 2026-07-10) ====
# ATL's WebViewClient declares only 2-arg onPageStarted(WebView,String); AOSP declares ONLY the 3-arg
# onPageStarted(WebView,String,Bitmap). An AOSP-compiled app's onPageStarted @Override therefore
# never received state-0 (challenge16: onPageFinished fired, the app-side onPageStarted never did).
# Land the AOSP base surface on WebViewClient (3-arg onPageStarted chaining to the 2-arg for legacy
# overriders + shouldOverrideUrlLoading returning false) and dispatch the 3-arg form from
# WebView.internalLoadChanged at state 0. shouldOverrideUrlLoading DISPATCH is SCOPED DOWN this pass:
# the base method lands for AOSP class-shape parity so an app @Override resolves, but NO call site is
# wired — under the driven-loads-only contract the wire LoadState carries no per-navigation URL and
# the consumer substitutes the driven URL for every event, so the overlay cannot honestly identify a
# non-driven navigation, and honoring a `true` return would need a new synchronous consumer->helper
# navigation-cancel (a frozen-protocol change). A later additive pass wires the dispatch.

# (1) WebView.internalLoadChanged: dispatch the AOSP 3-arg onPageStarted at state 0 (onPageFinished
#     stays 2-arg at state 3). Whole-method anchored replace (the ANCHOR_PC pristine-body pattern) so
#     installed drift fails loud, never a silent mis-insert. Placed AFTER the M4 WebView patches.
n="$(grep -cF '.method internalLoadChanged(ILjava/lang/String;)V' "$wvsm")" || true
[ "$n" = "1" ] || fail "WebView.smali internalLoadChanged anchor not unique (found $n, expected 1) — installed WebView drifted; update patch-framework.sh"
ANCHOR_ILC=$'.method internalLoadChanged(ILjava/lang/String;)V\n    .registers 4\n\n    if-nez p1, :cond_c\n\n    iget-object v0, p0, Landroid/webkit/WebView;->webViewClient:Landroid/webkit/WebViewClient;\n\n    if-eqz v0, :cond_c\n\n    iget-object v0, p0, Landroid/webkit/WebView;->webViewClient:Landroid/webkit/WebViewClient;\n\n    invoke-virtual {v0, p0, p2}, Landroid/webkit/WebViewClient;->onPageStarted(Landroid/webkit/WebView;Ljava/lang/String;)V\n\n    :cond_b\n    :goto_b\n    return-void\n\n    :cond_c\n    const/4 v0, 0x3\n\n    if-ne p1, v0, :cond_b\n\n    iget-object v0, p0, Landroid/webkit/WebView;->webViewClient:Landroid/webkit/WebViewClient;\n\n    if-eqz v0, :cond_b\n\n    iget-object v0, p0, Landroid/webkit/WebView;->webViewClient:Landroid/webkit/WebViewClient;\n\n    invoke-virtual {v0, p0, p2}, Landroid/webkit/WebViewClient;->onPageFinished(Landroid/webkit/WebView;Ljava/lang/String;)V\n\n    goto :goto_b\n.end method'
ANCHOR_ILC="$ANCHOR_ILC" perl -0777 -ne 'exit((index($_, $ENV{ANCHOR_ILC}) >= 0) ? 0 : 1)' "$wvsm" || fail "WebView.smali internalLoadChanged body changed from the expected 2-arg shape — installed WebView drifted; update patch-framework.sh"
perl -0pi -e 's{\.method internalLoadChanged\(ILjava/lang/String;\)V.*?\.end method\n}{.method internalLoadChanged(ILjava/lang/String;)V\n    .registers 5\n\n    # ECLIPSE PATCH 2026-07-10 (M6): dispatch AOSP 3-arg onPageStarted(WebView,String,Bitmap) at\n    # state 0 (an AOSP-compiled onPageStarted \@Override never received the ATL-only 2-arg form —\n    # challenge16 saw onPageFinished fire but never onPageStarted). onPageFinished stays 2-arg (its\n    # AOSP shape); null Bitmap (OSR has no favicon). The base 3-arg chains to the 2-arg form.\n    iget-object v0, p0, Landroid/webkit/WebView;->webViewClient:Landroid/webkit/WebViewClient;\n\n    if-eqz v0, :cond_eclipse_ilc_done\n\n    if-nez p1, :cond_eclipse_ilc_finished\n\n    const/4 v1, 0x0\n\n    invoke-virtual {v0, p0, p2, v1}, Landroid/webkit/WebViewClient;->onPageStarted(Landroid/webkit/WebView;Ljava/lang/String;Landroid/graphics/Bitmap;)V\n\n    return-void\n\n    :cond_eclipse_ilc_finished\n    const/4 v1, 0x3\n\n    if-ne p1, v1, :cond_eclipse_ilc_done\n\n    invoke-virtual {v0, p0, p2}, Landroid/webkit/WebViewClient;->onPageFinished(Landroid/webkit/WebView;Ljava/lang/String;)V\n\n    :cond_eclipse_ilc_done\n    return-void\n.end method\n}s' "$wvsm"
grep -qF -- '->onPageStarted(Landroid/webkit/WebView;Ljava/lang/String;Landroid/graphics/Bitmap;)V' "$wvsm" || fail "WebView.smali internalLoadChanged 3-arg onPageStarted dispatch insert failed (drift?)"
! grep -qF -- '->onPageStarted(Landroid/webkit/WebView;Ljava/lang/String;)V' "$wvsm" || fail "WebView.smali still dispatches the 2-arg onPageStarted (M6 3-arg dispatch incomplete)"

# (2) WebViewClient: NEW shadow into classes2 (stock-only today). Add the AOSP base 3-arg
#     onPageStarted (chaining to the 2-arg for legacy overriders) + shouldOverrideUrlLoading
#     returning false. Pre-check drift (neither must already exist), append, post-grep both.
wvcsm="$work/smali/android/webkit/WebViewClient.smali"
[ -f "$wvcsm" ] || fail "WebViewClient.smali not found after baksmali"
! grep -qF 'Landroid/graphics/Bitmap;)V' "$wvcsm" || fail "WebViewClient.smali already declares a Bitmap-arg method (3-arg onPageStarted?) — installed WebViewClient drifted; update patch-framework.sh"
! grep -qF 'shouldOverrideUrlLoading' "$wvcsm" || fail "WebViewClient.smali already declares shouldOverrideUrlLoading — installed WebViewClient drifted; update patch-framework.sh"
cat >> "$wvcsm" <<'ECLIPSE_WVC_METHODS'

# ECLIPSE PATCH 2026-07-10 (M6): AOSP base WebViewClient.onPageStarted(WebView, String, Bitmap).
# AOSP declares ONLY this 3-arg form; ATL declared only the 2-arg. The default body CHAINS to the
# 2-arg onPageStarted so a legacy ATL-2-arg overrider still receives when the subclass overrides the
# 2-arg but not the 3-arg (WebView.internalLoadChanged now dispatches this 3-arg form).
.method public onPageStarted(Landroid/webkit/WebView;Ljava/lang/String;Landroid/graphics/Bitmap;)V
    .registers 4

    invoke-virtual {p0, p1, p2}, Landroid/webkit/WebViewClient;->onPageStarted(Landroid/webkit/WebView;Ljava/lang/String;)V

    return-void
.end method

# ECLIPSE PATCH 2026-07-10 (M6): AOSP base WebViewClient.shouldOverrideUrlLoading(WebView, String).
# Returns false (WebView proceeds with the load) — the AOSP base default. SCOPED DOWN this pass: the
# base method lands for AOSP class-shape parity so an app @Override resolves, but NO call site is
# wired (the driven-loads-only contract carries no per-navigation URL to honestly gate on, and a
# true return would need a new synchronous consumer->helper navigation-cancel — a frozen-protocol
# change). A later additive pass wires the dispatch.
.method public shouldOverrideUrlLoading(Landroid/webkit/WebView;Ljava/lang/String;)Z
    .registers 4

    const/4 v0, 0x0

    return v0
.end method
ECLIPSE_WVC_METHODS
grep -qF -- 'onPageStarted(Landroid/webkit/WebView;Ljava/lang/String;Landroid/graphics/Bitmap;)V' "$wvcsm" || fail "WebViewClient 3-arg onPageStarted insert failed (drift?)"
grep -qF -- 'shouldOverrideUrlLoading(Landroid/webkit/WebView;Ljava/lang/String;)Z' "$wvcsm" || fail "WebViewClient shouldOverrideUrlLoading insert failed (drift?)"

# JobParameters.getNetwork() -> Network — AOSP API 28; Roblox queries it on a scheduled network job; ATL
# omits it. Returns null (no Network bound — AOSP-valid; the caller handles null). Anchor on getExtras.
jpm="$work/smali/android/app/job/JobParameters.smali"
[ -f "$jpm" ] || fail "JobParameters.smali not found after baksmali"
n="$(grep -cF '.method public getExtras()Landroid/os/PersistableBundle;' "$jpm")" || true
[ "$n" = "1" ] || fail "JobParameters.smali getExtras anchor not unique (found $n, expected 1) — installed JobParameters drifted; update patch-framework.sh"
! grep -qF 'getNetwork()Landroid/net/Network;' "$jpm" || fail "JobParameters.smali already declares getNetwork — drifted; update patch-framework.sh"
perl -0pi -e 's{(\.method public getExtras\(\)Landroid/os/PersistableBundle;.*?\.end method\n)}{$1\n# ECLIPSE PATCH 2026-06-14: AOSP JobParameters.getNetwork() (API 28); ATL omits it. Returns null (no Network bound — AOSP-valid; the caller handles null).\n.method public getNetwork()Landroid/net/Network;\n    .locals 1\n\n    const/4 v0, 0x0\n\n    return-object v0\n.end method\n}s' "$jpm"
grep -qF 'getNetwork()Landroid/net/Network;' "$jpm" || fail "JobParameters.smali getNetwork insert failed (drift?)"

# Paint.set(Paint) self-set guard — AOSP Paint.set(Paint src) no-ops a self-set (its `if (this != src)`
# guard), but ATL's set(Paint) has NO guard and calls native_recycle(this.paint) BEFORE
# native_clone(paint.paint), so `p.set(p)` hands native_clone a just-freed handle: a use-after-free under
# ATL's own reference C native, and under Eclipse's paint registry a StaleHandle that degrades the paint
# to a FRESH DEFAULT (warn-logged) — losing the recorded color/alpha/style/cap/join/width/text-size that
# AOSP's set(Paint) contract preserves (2026-07-02 review finding on the Paint-native pass). Insert the
# AOSP self-set guard at method entry. Anchor on the unique set(Paint) + a whole-body pristine check
# (the onPostCreate ANCHOR_PC pattern) so installed drift fails loud, never a silent mis-insert.
psm="$work/smali/android/graphics/Paint.smali"
[ -f "$psm" ] || fail "Paint.smali not found after baksmali"
n="$(grep -cF '.method public set(Landroid/graphics/Paint;)V' "$psm")" || true
[ "$n" = "1" ] || fail "Paint.smali set(Paint) anchor not unique (found $n, expected 1) — installed Paint drifted; update patch-framework.sh"
ANCHOR_PSET=$'.method public set(Landroid/graphics/Paint;)V\n    .registers 4\n\n    iget-wide v0, p0, Landroid/graphics/Paint;->paint:J\n\n    invoke-static {v0, v1}, Landroid/graphics/Paint;->native_recycle(J)V\n\n    iget-wide v0, p1, Landroid/graphics/Paint;->paint:J\n\n    invoke-static {v0, v1}, Landroid/graphics/Paint;->native_clone(J)J\n\n    move-result-wide v0\n\n    iput-wide v0, p0, Landroid/graphics/Paint;->paint:J\n\n    return-void\n.end method'
ANCHOR_PSET="$ANCHOR_PSET" perl -0777 -ne 'exit((index($_, $ENV{ANCHOR_PSET}) >= 0) ? 0 : 1)' "$psm" || fail "Paint.smali set(Paint) body changed from the expected recycle-before-clone shape — installed Paint drifted; update patch-framework.sh"
! grep -qF ':eclipse_not_self_set' "$psm" || fail "Paint.smali already carries the self-set guard — installed Paint drifted; update patch-framework.sh"
perl -0pi -e 's{(\.method public set\(Landroid/graphics/Paint;\)V\n    \.registers 4\n)}{$1\n    # ECLIPSE PATCH 2026-07-02: AOSP self-set guard — AOSP Paint.set(Paint src) no-ops when src == this.\n    # ATL recycles this.paint BEFORE cloning paint.paint, so an unguarded self-set clones a freed\n    # handle (use-after-free in the ATL reference C native; a warn-logged reset to a DEFAULT paint\n    # under the Eclipse paint registry, where AOSP preserves the state). Guard restores the contract.\n    if-ne p0, p1, :eclipse_not_self_set\n\n    return-void\n\n    :eclipse_not_self_set\n}s' "$psm"
grep -qF 'if-ne p0, p1, :eclipse_not_self_set' "$psm" || fail "Paint.smali self-set guard insert failed (drift?)"

# assemble View(+nested) + Display(+Mode) + Activity + Fragment + Vibrator + AutofillManager + CookieManager
# + JobParameters + Paint -> classes2.dex
mkdir -p "$work/smali-view/android/view" "$work/smali-view/android/app" "$work/smali-view/android/os" "$work/smali-view/android/view/autofill" "$work/smali-view/android/webkit" "$work/smali-view/android/app/job" "$work/smali-view/android/graphics"
cp "$vsm" "$work/smali-view/android/view/View.smali"
cp "$dsm" "$work/smali-view/android/view/Display.smali"
cp "$here/smali/android/view/View\$OnCapturedPointerListener.smali" "$work/smali-view/android/view/"
cp "$here/smali/android/view/Display\$Mode.smali" "$work/smali-view/android/view/"
cp "$asm" "$work/smali-view/android/app/Activity.smali"
cp "$fsm" "$work/smali-view/android/app/Fragment.smali"
cp "$vibsm" "$work/smali-view/android/os/Vibrator.smali"
cp "$afm" "$work/smali-view/android/view/autofill/AutofillManager.smali"
cp "$csm" "$work/smali-view/android/webkit/CookieManager.smali"
cp "$wvsm" "$work/smali-view/android/webkit/WebView.smali"
cp "$wssm" "$work/smali-view/android/webkit/WebSettings.smali"
cp "$wvcsm" "$work/smali-view/android/webkit/WebViewClient.smali"
cp "$jpm" "$work/smali-view/android/app/job/JobParameters.smali"
cp "$psm" "$work/smali-view/android/graphics/Paint.smali"
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
