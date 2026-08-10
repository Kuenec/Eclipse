#!/usr/bin/env bash










set -euo pipefail

here="$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)"
repo="$(cd "$here/../.." && pwd)"


ATL_SRC="${ATL_SRC:-$repo/vendor/atl/src/api-impl}"
ORIG_FW="${ORIG_FW:-/usr/lib/java/dex/android_translation_layer}"
ART_DIR="${ART_DIR:-/usr/lib/java/dex/art}"
OUT="${OUT:-${XDG_CACHE_HOME:-$HOME/.cache}/eclipse/framework-patched}"



ART_BOOT_JARS=(
    core-oj-hostdex.jar
    apachehttp-hostdex.jar
    apache-xml-hostdex.jar
    bouncycastle-hostdex.jar
    core-junit-hostdex.jar
    core-libart-hostdex.jar
    hamcrest-hostdex.jar
    junit-runner-hostdex.jar
    okhttp-hostdex.jar
    wolfssljni-hostdex.jar
)


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
for art_jar in "${ART_BOOT_JARS[@]}"; do
    [ -f "$ART_DIR/$art_jar" ] || fail "ART boot jar missing at $ART_DIR/$art_jar (set ART_DIR; install the pinned art_standalone runtime)"
done



JAVA="${JAVA:-$(find_jdk_tool java)}"
BAKSMALI_JAR="${BAKSMALI_JAR:-$repo/vendor/toolchain/smali/baksmali-2.5.2.jar}"
SMALI_JAR="${SMALI_JAR:-$repo/vendor/toolchain/smali/smali-2.5.2.jar}"
[ -n "$JAVA" ] && [ -x "$JAVA" ] || fail "java not found (set JAVA, or vendor a JDK at vendor/toolchain/jdk-*/)"
[ -f "$BAKSMALI_JAR" ] || fail "baksmali not found at $BAKSMALI_JAR (vendored at vendor/toolchain/smali/; set BAKSMALI_JAR, or 'pacman -S smali')"
[ -f "$SMALI_JAR" ] || fail "smali not found at $SMALI_JAR (vendored at vendor/toolchain/smali/; set SMALI_JAR, or 'pacman -S smali')"

work="$(mktemp -d)"
trap 'rm -rf "$work"' EXIT
mkdir -p "$work/gen/android/os" "$work/classes" "$work/stage" "$work/jar" "$work/art"




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



li_src="$here/src/android/view/LayoutInflater.java"
[ -f "$li_src" ] || fail "patched LayoutInflater.java missing at $li_src"
grep -qF 'parseRequestFocus(parser, parent);' "$li_src" || fail "patched LayoutInflater.java no longer calls parseRequestFocus — the <requestFocus/> fix regressed"
! grep -qF '<requestFocus /> not supported atm' "$li_src" || fail "patched LayoutInflater.java still throws the old <requestFocus/> 'not supported atm' — the fix regressed"






vc_src="$here/src/android/webkit/ValueCallback.java"
[ -f "$vc_src" ] || fail "patched ValueCallback.java missing at $vc_src"
grep -qE 'public[[:space:]]+interface[[:space:]]+ValueCallback' "$vc_src" || fail "patched ValueCallback.java is not an interface — the IncompatibleClassChangeError fix regressed"






kg_src="$here/src/android/app/KeyguardManager.java"
[ -f "$kg_src" ] || fail "patched KeyguardManager.java missing at $kg_src"
grep -qF 'public boolean isDeviceSecure()' "$kg_src" || fail "patched KeyguardManager.java no longer declares isDeviceSecure() — the NoSuchMethodError fix regressed"





am_src="$here/src/android/app/ActivityManager.java"
[ -f "$am_src" ] || fail "patched ActivityManager.java missing at $am_src"
for am_needle in \
    'private static native void native_fillMemoryInfo(MemoryInfo outInfo);' \
    'private static native int native_getMemoryClass();' \
    'private static native int native_getLargeMemoryClass();' \
    'private static native boolean native_isLowRamDevice();' \
    'public long hiddenAppThreshold;' \
    'public long foregroundAppThreshold;' \
    'dest.writeLong(foregroundAppThreshold);'
do
    grep -qF "$am_needle" "$am_src" || fail "ActivityManager memory patch regressed: missing '$am_needle'"
done
! grep -qF 'outInfo = new MemoryInfo();' "$am_src" || fail "ActivityManager.getMemoryInfo again reassigns only its local parameter — the 0MB bug regressed"





ji_src="$here/src/android/webkit/JavascriptInterface.java"
[ -f "$ji_src" ] || fail "M4 JavascriptInterface.java missing at $ji_src"
grep -qF 'public @interface JavascriptInterface' "$ji_src" || fail "JavascriptInterface.java is not an @interface — the M4 bridge annotation regressed"
grep -qF 'RetentionPolicy.RUNTIME' "$ji_src" || fail "JavascriptInterface.java is not RUNTIME-retention — reflection filtering would fail"
bp_src="$here/src/android/webkit/EclipseBridgeProbe.java"
[ -f "$bp_src" ] || fail "M4 EclipseBridgeProbe.java missing at $bp_src"
grep -qF '@JavascriptInterface' "$bp_src" || fail "EclipseBridgeProbe.java lost its @JavascriptInterface echo — __webview-test bridge leg regressed"









wvcp_src="$here/src/android/webkit/EclipseWebViewClientProbe.java"
[ -f "$wvcp_src" ] || fail "M6 EclipseWebViewClientProbe.java missing at $wvcp_src"
grep -qF 'new Handler();' "$wvcp_src" || fail "EclipseWebViewClientProbe.java no longer constructs a Handler — __webview-test would go blind to the Looper-less-dispatch class (2026-07-16)"
grep -qF 'Looper.myLooper() != Looper.getMainLooper()' "$wvcp_src" || fail "EclipseWebViewClientProbe.java lost its UI-thread assertion — a prepared-but-undrained Looper on the upcall thread would pass this guard green"
grep -qF 'onPageStarted(WebView view, String url, Bitmap favicon)' "$wvcp_src" || fail "EclipseWebViewClientProbe.java no longer overrides the AOSP 3-arg onPageStarted — the M6 state-0 dispatch would go unpinned"
grep -qF 'onPageFinished(WebView view, String url)' "$wvcp_src" || fail "EclipseWebViewClientProbe.java no longer overrides onPageFinished — half the confirmed 2026-07-16 defect would go unpinned"






pc_src="$here/src/android/view/PixelCopy.java"
[ -f "$pc_src" ] || fail "PixelCopy.java compatibility surface missing at $pc_src"
grep -qF 'public interface OnPixelCopyFinishedListener' "$pc_src" || fail "PixelCopy.java lost its completion-listener API"
grep -qF 'listenerThread.post(new Runnable()' "$pc_src" || fail "PixelCopy.java no longer dispatches completion through the caller's Handler"
grep -qF 'listener.onPixelCopyFinished(ERROR_SOURCE_NO_DATA);' "$pc_src" || fail "PixelCopy.java no longer reports the honest ERROR_SOURCE_NO_DATA result"
! grep -qF 'listener.onPixelCopyFinished(SUCCESS);' "$pc_src" || fail "PixelCopy.java fabricates SUCCESS without a pixel-copy backend"












r_src="$ATL_SRC/com/android/internal/R.java"
[ -f "$r_src" ] || fail "vendored com/android/internal/R.java not found at $r_src (set ATL_SRC)"
grep -qF 'public static final int id=0x010100d0;' "$r_src" || fail "vendored internal R.attr.id != 0x010100d0 — ATL source drifted; re-verify the overlay's inlined constants"
grep -qF 'public static final int theme=0x01010000;' "$r_src" || fail "vendored internal R.attr.theme != 0x01010000 — ATL source drifted; re-verify the overlay's inlined constants"
[ ! -e "$here/stubs/com/android/internal/R.java" ] || fail "stub com/android/internal/R.java re-appeared — javac would inline its placeholder constants into the overlay dex (the 2026-07-02 include-id NPE class); delete it (the vendored R.java is the compile input)"




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
    "$pc_src" \
    "$r_src"


for pattern in 'android/os/Build*.class' 'android/os/PowerManager*.class' 'android/net/NetworkRequest*.class' 'android/app/ActivityManager*.class' 'android/view/LayoutInflater*.class' 'android/view/PixelCopy*.class' 'android/webkit/ValueCallback*.class' 'android/webkit/JavascriptInterface*.class' 'android/webkit/EclipseBridgeProbe*.class' 'android/webkit/EclipseWebViewClientProbe*.class' 'android/app/KeyguardManager*.class'; do
    dir="${pattern%/*}"
    mkdir -p "$work/stage/$dir"
    cp "$work/classes/"$pattern "$work/stage/$dir/"
done









for forbidden in 'android/webkit/WebView.class' 'android/webkit/WebViewClient.class' \
                 'android/os/Handler.class' 'android/os/Looper.class' \
                 'android/graphics/Bitmap.class' 'android/view/SurfaceView.class'; do
    [ ! -e "$work/stage/$forbidden" ] || fail "compile-only stub $forbidden was staged into classes.dex — it would SHADOW the real class (first-dex-wins); fix the step-3 stage whitelist"
done
for stub in android/webkit/WebView.java android/webkit/WebViewClient.java \
            android/os/Handler.java android/os/Looper.java android/graphics/Bitmap.java \
            android/view/SurfaceView.java; do
    [ -f "$here/stubs/$stub" ] || fail "M6 compile-only stub $stub missing — EclipseWebViewClientProbe would not compile"
    ! grep -qE 'static[[:space:]]+final' "$here/stubs/$stub" || fail "M6 stub $stub declares a constant — javac would INLINE its placeholder value into the overlay dex (the 2026-07-02 guard-1e class)"
done


"$DX" --dex --output="$work/jar/classes.dex" "$work/stage"






"$JAVA" -jar "$BAKSMALI_JAR" disassemble "$work/jar/classes.dex" -o "$work/smali-check" >/dev/null
lism="$work/smali-check/android/view/LayoutInflater.smali"
[ -f "$lism" ] || fail "LayoutInflater.smali not found in the built classes.dex"
grep -qF '0x10100d0' "$lism" || fail "dexed LayoutInflater lost the inlined android:id constant (0x010100d0) — the <include android:id> override would silently drop (2026-07-02 RobloxToolbar NPE class)"
grep -qF '0x1010000' "$lism" || fail "dexed LayoutInflater lost the inlined android:theme constant (0x01010000) — createView's android:theme handling would silently drop"






wvcpsm="$work/smali-check/android/webkit/EclipseWebViewClientProbe.smali"
[ -f "$wvcpsm" ] || fail "EclipseWebViewClientProbe.smali not in the built classes.dex — the __webview-test Looper-contract probe did not stage"
grep -qF 'Landroid/os/Handler;-><init>()V' "$wvcpsm" || fail "dexed EclipseWebViewClientProbe lost the no-arg Handler construction — the 2026-07-16 Looper-less-dispatch guard would not fire"

grep -qF 'Landroid/os/Looper;->getMainLooper()' "$wvcpsm" || fail "dexed EclipseWebViewClientProbe lost its UI-thread assertion"
grep -qF 'onPageStarted(Landroid/webkit/WebView;Ljava/lang/String;Landroid/graphics/Bitmap;)V' "$wvcpsm" || fail "dexed EclipseWebViewClientProbe lost the AOSP 3-arg onPageStarted override — internalLoadChanged's state-0 dispatch would miss it (and the stub has drifted from the classes2 shadow)"




pcsm="$work/smali-check/android/view/PixelCopy.smali"
pcrsm="$work/smali-check/android/view/PixelCopy\$1.smali"
[ -f "$pcsm" ] || fail "PixelCopy.smali not in the built classes.dex — the shutdown compatibility surface did not stage"
[ -f "$pcrsm" ] || fail "PixelCopy anonymous completion Runnable not in the built classes.dex"
grep -qF 'request(Landroid/view/SurfaceView;Landroid/graphics/Bitmap;Landroid/view/PixelCopy$OnPixelCopyFinishedListener;Landroid/os/Handler;)V' "$pcsm" || fail "dexed PixelCopy lost its SurfaceView request overload"
grep -qF 'Landroid/os/Handler;->post(Ljava/lang/Runnable;)Z' "$pcsm" || fail "dexed PixelCopy no longer posts completion through Handler"
grep -qF 'Landroid/view/PixelCopy$OnPixelCopyFinishedListener;->onPixelCopyFinished(I)V' "$pcrsm" || fail "dexed PixelCopy Runnable no longer invokes its listener"
grep -qE 'const/4 v[0-9]+, 0x3' "$pcrsm" || fail "dexed PixelCopy Runnable no longer reports ERROR_SOURCE_NO_DATA (3)"








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

perl -0pi -e 's{(\.field private on_touch_listener:Landroid/view/View\$OnTouchListener;\n)}{$1nCapturedPointerListener backing field (pointer-capture API ATL omits)\n.field private mCapturedPointerListener:Landroid/view/View\$OnCapturedPointerListener;\n}' "$vsm"

perl -0pi -e 's{(\.method public setOnClickListener\(Landroid/view/View\$OnClickListener;\)V.*?\.end method\n)}{$1nCapturedPointerListener (API 26); headless (engine owns pointer input), pure-Java record\n.method public setOnCapturedPointerListener(Landroid/view/View\$OnCapturedPointerListener;)V\n    .registers 2\n\n    iput-object p1, p0, Landroid/view/View;->mCapturedPointerListener:Landroid/view/View\$OnCapturedPointerListener;\n\n    return-void\n.end method\n}s' "$vsm"

perl -0pi -e 's{(value = \{\n)(        Landroid/view/View\$DeclaredOnClickListener;,\n)}{$1        Landroid/view/View\$OnCapturedPointerListener;,\n$2}' "$vsm"
grep -qF 'setOnCapturedPointerListener(Landroid/view/View$OnCapturedPointerListener;)V' "$vsm" || fail "View.smali setter insert failed (drift?)"
grep -qF 'mCapturedPointerListener:Landroid/view/View$OnCapturedPointerListener;' "$vsm" || fail "View.smali field insert failed (drift?)"




perl -0pi -e 's{(\.method public setOnCapturedPointerListener\(Landroid/view/View\$OnCapturedPointerListener;\)V.*?\.end method\n)}{$1no-ops (RbxKeyboard configures the login EditText for autofill; ATL omits these). No autofill service.\n.method public setAutofillHints([Ljava/lang/String;)V\n    .registers 2\n\n    return-void\n.end method\n\n.method public setImportantForAutofill(I)V\n    .registers 2\n\n    return-void\n.end method\n}s' "$vsm"
grep -qF 'setAutofillHints([Ljava/lang/String;)V' "$vsm" || fail "View.smali setAutofillHints insert failed (drift?)"
grep -qF 'setImportantForAutofill(I)V' "$vsm" || fail "View.smali setImportantForAutofill insert failed (drift?)"




dsm="$work/smali/android/view/Display.smali"
[ -f "$dsm" ] || fail "Display.smali not found after baksmali"
n="$(grep -cF '.method public getRefreshRate()F' "$dsm")" || true
[ "$n" = "1" ] || fail "Display.smali getRefreshRate anchor not unique (found $n, expected 1) — installed Display drifted; update patch-framework.sh"
perl -0pi -e 's{(\.method public getRefreshRate\(\)F.*?\.end method\n)}{$1n Activity.onStart for framerate setup; ATL omits it). Returns {60.0f} to match getRefreshRate above.\n.method public getSupportedRefreshRates()[F\n    .locals 3\n\n    const/4 v0, 0x1\n\n    new-array v0, v0, [F\n\n    const/4 v1, 0x0\n\n    const/high16 v2, 0x42700000    # 60.0f\n\n    aput v2, v0, v1\n\n    return-object v0\n.end method\n}s' "$dsm"
grep -qF 'getSupportedRefreshRates()[F' "$dsm" || fail "Display.smali getSupportedRefreshRates insert failed (drift?)"



n="$(grep -cF '.method public getWidth()I' "$dsm")" || true
[ "$n" = "1" ] || fail "Display.smali getWidth anchor not unique (found $n, expected 1) — installed Display drifted; update patch-framework.sh"
! grep -qF 'getMode()Landroid/view/Display$Mode;' "$dsm" || fail "Display.smali already declares getMode — installed Display drifted; update patch-framework.sh"
perl -0pi -e 's{(\.method public getWidth\(\)I.*?\.end method\n)}{$1nResume startup; ATL omits it + Display\$Mode). Build a Mode from window_width/window_height + 60.0f (consistent with getWidth/getHeight/getRefreshRate).\n.method public getMode()Landroid/view/Display\$Mode;\n    .locals 5\n\n    new-instance v0, Landroid/view/Display\$Mode;\n\n    const/4 v1, 0x0\n\n    sget v2, Landroid/view/Display;->window_width:I\n\n    sget v3, Landroid/view/Display;->window_height:I\n\n    const/high16 v4, 0x42700000    # 60.0f\n\n    invoke-direct {v0, v1, v2, v3, v4}, Landroid/view/Display\$Mode;-><init>(IIIF)V\n\n    return-object v0\n.end method\n}s' "$dsm"
grep -qF 'getMode()Landroid/view/Display$Mode;' "$dsm" || fail "Display.smali getMode insert failed (drift?)"



















fsm="$work/smali/android/app/Fragment.smali"
[ -f "$fsm" ] || fail "Fragment.smali not found after baksmali"
n="$(grep -cF '.method public onCreate(Landroid/os/Bundle;)V' "$fsm")" || true
[ "$n" = "1" ] || fail "Fragment.smali onCreate anchor not unique (found $n, expected 1) — installed Fragment drifted; update patch-framework.sh"
! grep -qF 'onActivityCreated(Landroid/os/Bundle;)V' "$fsm" || fail "Fragment.smali already declares onActivityCreated — installed Fragment drifted; update patch-framework.sh"

perl -0pi -e 's{(\.method public onCreate\(Landroid/os/Bundle;\)V.*?\.end method\n)}{$1nt.onActivityCreated(Bundle) hook (empty); androidx ReportFragment \@Overrides it to dispatch Lifecycle.Event.ON_CREATE. ATL omitted it.\n.method public onActivityCreated(Landroid/os/Bundle;)V\n    .registers 2\n\n    return-void\n.end method\n}s' "$fsm"
grep -qF 'onActivityCreated(Landroid/os/Bundle;)V' "$fsm" || fail "Fragment.smali onActivityCreated insert failed (drift?)"

asm="$work/smali/android/app/Activity.smali"
[ -f "$asm" ] || fail "Activity.smali not found after baksmali"




ANCHOR_PC=$'.method protected onPostCreate(Landroid/os/Bundle;)V\n    .registers 4\n\n    const-string v0, "Activity"\n\n    const-string v1, "- onPostCreate - yay!"\n\n    invoke-static {v0, v1}, Landroid/util/Slog;->i(Ljava/lang/String;Ljava/lang/String;)I\n\n    return-void\n.end method'
n="$(grep -cF '.method protected onPostCreate(Landroid/os/Bundle;)V' "$asm")" || true
[ "$n" = "1" ] || fail "Activity.smali onPostCreate anchor not unique (found $n, expected 1) — installed Activity drifted; update patch-framework.sh"
ANCHOR_PC="$ANCHOR_PC" perl -0777 -ne 'exit((index($_, $ENV{ANCHOR_PC}) >= 0) ? 0 : 1)' "$asm" || fail "Activity.smali onPostCreate body changed from the expected no-op — installed Activity drifted; update patch-framework.sh"
! grep -qF 'onActivityCreated(Landroid/os/Bundle;)V' "$asm" || fail "Activity.smali already dispatches onActivityCreated — installed Activity drifted; update patch-framework.sh"


perl -0pi -e 's{\.method protected onPostCreate\(Landroid/os/Bundle;\)V\n    \.registers 4\n\n    const-string v0, "Activity"\n\n    const-string v1, "- onPostCreate - yay!"\n\n    invoke-static \{v0, v1\}, Landroid/util/Slog;->i\(Ljava/lang/String;Ljava/lang/String;\)I\n\n    return-void\n\.end method}{.method protected onPostCreate(Landroid/os/Bundle;)V\n    .registers 4\n\n    const-string v0, "Activity"\n\n    const-string v1, "- onPostCreate - yay!"\n\n    invoke-static \{v0, v1\}, Landroid/util/Slog;->i(Ljava/lang/String;Ljava/lang/String;)I\nnt.onActivityCreated(savedInstanceState) (AOSP create-ndroidx ReportFragment fires Lifecycle.Event.ON_CREATE while thenStart dispatches ON_START. Eclipse drives onPostCreaten onCreate and onStart, after the onCreate super-chain has injected the ReportFragment.\n    iget-object v0, p0, Landroid/app/Activity;->fragments:Ljava/util/List;\n\n    invoke-interface \{v0\}, Ljava/util/List;->iterator()Ljava/util/Iterator;\n\n    move-result-object v1\n\n    :goto_pc\n    invoke-interface \{v1\}, Ljava/util/Iterator;->hasNext()Z\n\n    move-result v0\n\n    if-eqz v0, :cond_pc\n\n    invoke-interface \{v1\}, Ljava/util/Iterator;->next()Ljava/lang/Object;\n\n    move-result-object v0\n\n    check-cast v0, Landroid/app/Fragment;\n\n    invoke-virtual \{v0, p1\}, Landroid/app/Fragment;->onActivityCreated(Landroid/os/Bundle;)V\n\n    goto :goto_pc\n\n    :cond_pc\n    return-void\n.end method}s' "$asm"
grep -qF 'invoke-virtual {v0, p1}, Landroid/app/Fragment;->onActivityCreated(Landroid/os/Bundle;)V' "$asm" || fail "Activity.smali onPostCreate dispatch insert failed (drift?)"










lmsm="$work/smali/android/location/LocationManager.smali"
[ -f "$lmsm" ] || fail "LocationManager.smali not found after baksmali"
n="$(grep -cF '.method public getAllProviders()Ljava/util/List;' "$lmsm")" || true
[ "$n" = "1" ] || fail "LocationManager.smali getAllProviders anchor not unique (found $n, expected 1) — installed LocationManager drifted; update patch-framework.sh"
! grep -qF 'isProviderEnabled(Ljava/lang/String;)Z' "$lmsm" || fail "LocationManager.smali already declares isProviderEnabled — installed framework drifted; re-evaluate this patch"
perl -0pi -e 's{(\.method public getAllProviders\(\)Ljava/util/List;.*?\.end method\n)}{$1nManager.isProviderEnabled(String). ATL advertises annon-null provider is disabled; null retains AOSP\x27s IllegalArgumentException.\n.method public isProviderEnabled(Ljava/lang/String;)Z\n    .locals 2\n\n    if-nez p1, :eclipse_location_provider_non_null\n\n    new-instance v0, Ljava/lang/IllegalArgumentException;\n\n    const-string v1, "invalid null provider"\n\n    invoke-direct {v0, v1}, Ljava/lang/IllegalArgumentException;-><init>(Ljava/lang/String;)V\n\n    throw v0\n\n    :eclipse_location_provider_non_null\n    const/4 v0, 0x0\n\n    return v0\n.end method\n}s' "$lmsm"
grep -qF '.method public isProviderEnabled(Ljava/lang/String;)Z' "$lmsm" || fail "LocationManager.smali isProviderEnabled insert failed (drift?)"
grep -qF 'Ljava/lang/IllegalArgumentException;-><init>(Ljava/lang/String;)V' "$lmsm" || fail "LocationManager.smali null-provider contract insert failed"
grep -qF ':eclipse_location_provider_non_null' "$lmsm" || fail "LocationManager.smali disabled-provider return path insert failed"




vibsm="$work/smali/android/os/Vibrator.smali"
[ -f "$vibsm" ] || fail "Vibrator.smali not found after baksmali"
n="$(grep -cF '.method public vibrate(J)V' "$vibsm")" || true
[ "$n" = "1" ] || fail "Vibrator.smali vibrate(J)V anchor not unique (found $n, expected 1) — installed Vibrator drifted; update patch-framework.sh"
! grep -qF '.method public cancel()V' "$vibsm" || fail "Vibrator.smali already declares cancel — installed Vibrator drifted; update patch-framework.sh"
perl -0pi -e 's{(\.method public vibrate\(J\)V.*?\.end method\n)}{$1ncel() no-op (Roblox calls it on a Timer thread; ATL omits it). No vibration device -> nothing to cancel.\n.method public cancel()V\n    .registers 1\n\n    return-void\n.end method\n}s' "$vibsm"
grep -qF '.method public cancel()V' "$vibsm" || fail "Vibrator.smali cancel insert failed (drift?)"






afm="$work/smali/android/view/autofill/AutofillManager.smali"
[ -f "$afm" ] || fail "AutofillManager.smali not found after baksmali"
n="$(grep -cF '.method public unregisterCallback(Landroid/view/autofill/AutofillManager$AutofillCallback;)V' "$afm")" || true
[ "$n" = "1" ] || fail "AutofillManager.smali unregisterCallback anchor not unique (found $n, expected 1) — installed AutofillManager drifted; update patch-framework.sh"
! grep -qF '.method public cancel()V' "$afm" || fail "AutofillManager.smali already declares cancel — installed AutofillManager drifted; update patch-framework.sh"
perl -0pi -e 's{(\.method public unregisterCallback\(Landroid/view/autofill/AutofillManager\$AutofillCallback;\)V.*?\.end method\n)}{$1nager.cancel() no-op (Roblox RbxKeyboard.i() calls it when showing text input for a focused login field; ATL omits it). No autofill service -> nothing to cancel.\n.method public cancel()V\n    .registers 1\n\n    return-void\n.end method\n}s' "$afm"
grep -qF '.method public cancel()V' "$afm" || fail "AutofillManager.smali cancel insert failed (drift?)"




! grep -qF 'requestAutofill(Landroid/view/View;)V' "$afm" || fail "AutofillManager.smali already declares requestAutofill — drifted; update patch-framework.sh"
perl -0pi -e 's{(\.method public cancel\(\)V.*?\.end method\n)}{$1nager.requestAutofill(View) no-op (Roblox RbxKeyboard.i() requests autofill for a focused login field; ATL omits it). No autofill service -> no-op.\n.method public requestAutofill(Landroid/view/View;)V\n    .registers 2\n\n    return-void\n.end method\n}s' "$afm"
grep -qF 'requestAutofill(Landroid/view/View;)V' "$afm" || fail "AutofillManager.smali requestAutofill insert failed (drift?)"







csm="$work/smali/android/webkit/CookieManager.smali"
[ -f "$csm" ] || fail "CookieManager.smali not found after baksmali"
! grep -qF 'native_getCookie' "$csm" || fail "CookieManager.smali already carries native_getCookie — drifted; update patch-framework.sh"

perl -0pi -e 's{\.method public getCookie\(Ljava/lang/String;\)Ljava/lang/String;.*?\.end method\n}{.method public getCookie(Ljava/lang/String;)Ljava/lang/String;\n    .registers 3\n\n    invoke-direct {p0, p1}, Landroid/webkit/CookieManager;->native_getCookie(Ljava/lang/String;)Ljava/lang/String;\n\n    move-result-object v0\n\n    return-object v0\n.end method\n}s' "$csm"
grep -qF -- '->native_getCookie(Ljava/lang/String;)Ljava/lang/String;' "$csm" || fail "CookieManager getCookie native-body insert failed (drift?)"

perl -0pi -e 's{\.method public setCookie\(Ljava/lang/String;Ljava/lang/String;\)V.*?\.end method\n}{.method public setCookie(Ljava/lang/String;Ljava/lang/String;)V\n    .registers 3\n\n    invoke-direct {p0, p1, p2}, Landroid/webkit/CookieManager;->native_setCookie(Ljava/lang/String;Ljava/lang/String;)V\n\n    return-void\n.end method\nn>) — the REAL success flag routes back through the native, never a fabricated Boolean.TRUE.\n.method public setCookie(Ljava/lang/String;Ljava/lang/String;Landroid/webkit/ValueCallback;)V\n    .registers 4\n\n    invoke-direct {p0, p1, p2, p3}, Landroid/webkit/CookieManager;->native_setCookie(Ljava/lang/String;Ljava/lang/String;Landroid/webkit/ValueCallback;)V\n\n    return-void\n.end method\n}s' "$csm"
grep -qF -- '->native_setCookie(Ljava/lang/String;Ljava/lang/String;)V' "$csm" || fail "CookieManager setCookie(2-arg) native-body insert failed (drift?)"
grep -qF 'setCookie(Ljava/lang/String;Ljava/lang/String;Landroid/webkit/ValueCallback;)V' "$csm" || fail "CookieManager setCookie(3-arg) insert failed (drift?)"

perl -0pi -e 's{\.method public removeAllCookies\(Landroid/webkit/ValueCallback;\)V.*?\.end method\n}{.method public removeAllCookies(Landroid/webkit/ValueCallback;)V\n    .registers 2\n\n    invoke-direct {p0, p1}, Landroid/webkit/CookieManager;->native_removeAllCookies(Landroid/webkit/ValueCallback;)V\n\n    return-void\n.end method\n}s' "$csm"
grep -qF -- '->native_removeAllCookies(Landroid/webkit/ValueCallback;)V' "$csm" || fail "CookieManager removeAllCookies native-body insert failed (drift?)"

perl -0pi -e 's{\.method public removeSessionCookies\(Landroid/webkit/ValueCallback;\)V.*?\.end method\n}{.method public removeSessionCookies(Landroid/webkit/ValueCallback;)V\n    .registers 2\n\n    invoke-direct {p0, p1}, Landroid/webkit/CookieManager;->native_removeSessionCookies(Landroid/webkit/ValueCallback;)V\n\n    return-void\n.end method\n}s' "$csm"
grep -qF -- '->native_removeSessionCookies(Landroid/webkit/ValueCallback;)V' "$csm" || fail "CookieManager removeSessionCookies native-body insert failed (drift?)"

perl -0pi -e 's{\.method public flush\(\)V.*?\.end method\n}{.method public flush()V\n    .registers 1\n\n    invoke-direct {p0}, Landroid/webkit/CookieManager;->native_flush()V\n\n    return-void\n.end method\n}s' "$csm"
grep -qF -- '->native_flush()V' "$csm" || fail "CookieManager flush native-body insert failed (drift?)"

cat >> "$csm" <<'ECLIPSE_CM_NATIVES'


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






wvsm="$work/smali/android/webkit/WebView.smali"
wssm="$work/smali/android/webkit/WebSettings.smali"
[ -f "$wvsm" ] || fail "WebView.smali not found after baksmali"
[ -f "$wssm" ] || fail "WebSettings.smali not found after baksmali"
! grep -qF 'native_evaluateJavascript' "$wvsm" || fail "WebView.smali already carries native_evaluateJavascript — drifted; update patch-framework.sh"

grep -qF 'const-string v2, " - not implemented yet"' "$wvsm" || fail "WebView.smali loadUrl no longer carries the javascript: println (installed shape drifted; update patch-framework.sh)"
perl -0pi -e 's{\.method public loadUrl\(Ljava/lang/String;\)V.*?\.end method\n}{.method public loadUrl(Ljava/lang/String;)V\n    .registers 7\n\n    const-string v0, "javascript:"\n\n    invoke-virtual {p1, v0}, Ljava/lang/String;->startsWith(Ljava/lang/String;)Z\n\n    move-result v0\n\n    iget-wide v1, p0, Landroid/view/View;->widget:J\n\n    if-eqz v0, :cond_eclipse_loadurl_normal\nngine (NO full-URL println leak).\n    const/16 v3, 0xb\n\n    invoke-virtual {p1, v3}, Ljava/lang/String;->substring(I)Ljava/lang/String;\n\n    move-result-object v3\n\n    const/4 v4, 0x0\n\n    invoke-direct {p0, v1, v2, v3, v4}, Landroid/webkit/WebView;->native_evaluateJavascript(JLjava/lang/String;Landroid/webkit/ValueCallback;)V\n\n    return-void\n\n    :cond_eclipse_loadurl_normal\n    invoke-direct {p0, v1, v2, p1}, Landroid/webkit/WebView;->native_loadUrl(JLjava/lang/String;)V\n\n    return-void\n.end method\n}s' "$wvsm"
grep -qF -- '->native_evaluateJavascript(JLjava/lang/String;Landroid/webkit/ValueCallback;)V' "$wvsm" || fail "WebView loadUrl javascript:-route insert failed (drift?)"
! grep -qF 'const-string v2, " - not implemented yet"' "$wvsm" || fail "WebView loadUrl still carries the full-URL println (leak-fix regressed)"

perl -0pi -e 's{\.method public evaluateJavascript\(Ljava/lang/String;Landroid/webkit/ValueCallback;\)V.*?\.end method\n}{.method public evaluateJavascript(Ljava/lang/String;Landroid/webkit/ValueCallback;)V\n    .registers 5\n\n    iget-wide v0, p0, Landroid/view/View;->widget:J\n\n    invoke-direct {p0, v0, v1, p1, p2}, Landroid/webkit/WebView;->native_evaluateJavascript(JLjava/lang/String;Landroid/webkit/ValueCallback;)V\n\n    return-void\n.end method\n}s' "$wvsm"

perl -0pi -e 's{\.method public addJavascriptInterface\(Ljava/lang/Object;Ljava/lang/String;\)V.*?\.end method\n}{.method public addJavascriptInterface(Ljava/lang/Object;Ljava/lang/String;)V\n    .registers 5\n\n    iget-wide v0, p0, Landroid/view/View;->widget:J\n\n    invoke-direct {p0, v0, v1, p1, p2}, Landroid/webkit/WebView;->native_addJavascriptInterface(JLjava/lang/Object;Ljava/lang/String;)V\n\n    return-void\n.end method\n}s' "$wvsm"
grep -qF -- '->native_addJavascriptInterface(JLjava/lang/Object;Ljava/lang/String;)V' "$wvsm" || fail "WebView addJavascriptInterface native-body insert failed (drift?)"

cat >> "$wvsm" <<'ECLIPSE_WV_NATIVES'


.method private native native_evaluateJavascript(JLjava/lang/String;Landroid/webkit/ValueCallback;)V
.end method

.method private native native_addJavascriptInterface(JLjava/lang/Object;Ljava/lang/String;)V
.end method
ECLIPSE_WV_NATIVES

grep -qF 'const-string v0, "GDPR VIOLATION"' "$wssm" || fail "WebSettings.smali no longer returns \"GDPR VIOLATION\" (installed shape drifted; update patch-framework.sh)"
ECLIPSE_UA='Mozilla/5.0 (X11; Linux x86_64) AppleWebKit/537.36 (KHTML, like Gecko) Chrome/149.0.0.0 Safari/537.36 Eclipse-WebView/149.0.6'






ECLIPSE_UA="$ECLIPSE_UA" perl -0pi -e 'my $ua=$ENV{ECLIPSE_UA}; s{\.method public getUserAgentString\(\)Ljava/lang/String;.*?\.end method\n}{".method public getUserAgentString()Ljava/lang/String;\n    .registers 2\nntString wins; null = it set none.\n    invoke-direct {p0}, Landroid/webkit/WebSettings;->native_getUserAgentString()Ljava/lang/String;\n\n    move-result-object v0\n\n    if-nez v0, :cond_eclipse_ua_app\nno UA: the Eclipse fallback literal (MUST byte-match engine.rs ECLIPSE_USER_AGENT).\n    const-string v0, \"$ua\"\n\n    :cond_eclipse_ua_app\n    return-object v0\n.end method\n"}se' "$wssm"
grep -qF -- '->native_getUserAgentString()Ljava/lang/String;' "$wssm" || fail "WebSettings getUserAgentString native-body insert failed (drift?)"
ECLIPSE_UA="$ECLIPSE_UA" perl -0pi -e 'my $ua=$ENV{ECLIPSE_UA}; s{\.method public static getDefaultUserAgent\(Landroid/content/Context;\)Ljava/lang/String;.*?\.end method\n}{".method public static getDefaultUserAgent(Landroid/content/Context;)Ljava/lang/String;\n    .registers 2\n\n    const-string v0, \"$ua\"\n\n    return-object v0\n.end method\n"}se' "$wssm"
grep -qF 'Eclipse-WebView/149.0.6' "$wssm" || fail "WebSettings honest-UA insert failed (drift?)"
! grep -qF 'GDPR VIOLATION' "$wssm" || fail "WebSettings still returns \"GDPR VIOLATION\" (honest-UA fix incomplete)"
























! grep -qF 'native_setUserAgentString' "$wssm" || fail "WebSettings.smali already carries native_setUserAgentString — drifted; update patch-framework.sh"
n="$(grep -cF '.method public setUserAgentString(Ljava/lang/String;)V' "$wssm")" || true
[ "$n" = "1" ] || fail "WebSettings.smali setUserAgentString anchor not unique (found $n, expected 1) — installed WebSettings drifted; update patch-framework.sh"


ANCHOR_UAS=$'.method public setUserAgentString(Ljava/lang/String;)V\n    .registers 2\n\n    return-void\n.end method'
ANCHOR_UAS="$ANCHOR_UAS" perl -0777 -ne 'exit((index($_, $ENV{ANCHOR_UAS}) >= 0) ? 0 : 1)' "$wssm" || fail "WebSettings.smali setUserAgentString body changed from the expected empty no-op — installed WebSettings drifted; update patch-framework.sh"


perl -0pi -e 's{\.method public setUserAgentString\(Ljava/lang/String;\)V.*?\.end method\n}{.method public setUserAgentString(Ljava/lang/String;)V\n    .registers 2\n\x27s UA — ATL\x27s stub silently discarded it (§6 💥).\n    invoke-direct {p0, p1}, Landroid/webkit/WebSettings;->native_setUserAgentString(Ljava/lang/String;)V\n\n    return-void\n.end method\n}s' "$wssm"
grep -qF -- '->native_setUserAgentString(Ljava/lang/String;)V' "$wssm" || fail "WebSettings setUserAgentString native-body insert failed (drift?)"



ANCHOR_UAS="$ANCHOR_UAS" perl -0777 -ne 'exit((index($_, $ENV{ANCHOR_UAS}) >= 0) ? 1 : 0)' "$wssm" || fail "WebSettings.setUserAgentString is still the empty no-op — the app's UA would be silently discarded again (§6 2026-07-16 💥)"

cat >> "$wssm" <<'ECLIPSE_WS_NATIVES'


.method private native native_setUserAgentString(Ljava/lang/String;)V
.end method

.method private native native_getUserAgentString()Ljava/lang/String;
.end method
ECLIPSE_WS_NATIVES

















n="$(grep -cF '.method internalLoadChanged(ILjava/lang/String;)V' "$wvsm")" || true
[ "$n" = "1" ] || fail "WebView.smali internalLoadChanged anchor not unique (found $n, expected 1) — installed WebView drifted; update patch-framework.sh"
ANCHOR_ILC=$'.method internalLoadChanged(ILjava/lang/String;)V\n    .registers 4\n\n    if-nez p1, :cond_c\n\n    iget-object v0, p0, Landroid/webkit/WebView;->webViewClient:Landroid/webkit/WebViewClient;\n\n    if-eqz v0, :cond_c\n\n    iget-object v0, p0, Landroid/webkit/WebView;->webViewClient:Landroid/webkit/WebViewClient;\n\n    invoke-virtual {v0, p0, p2}, Landroid/webkit/WebViewClient;->onPageStarted(Landroid/webkit/WebView;Ljava/lang/String;)V\n\n    :cond_b\n    :goto_b\n    return-void\n\n    :cond_c\n    const/4 v0, 0x3\n\n    if-ne p1, v0, :cond_b\n\n    iget-object v0, p0, Landroid/webkit/WebView;->webViewClient:Landroid/webkit/WebViewClient;\n\n    if-eqz v0, :cond_b\n\n    iget-object v0, p0, Landroid/webkit/WebView;->webViewClient:Landroid/webkit/WebViewClient;\n\n    invoke-virtual {v0, p0, p2}, Landroid/webkit/WebViewClient;->onPageFinished(Landroid/webkit/WebView;Ljava/lang/String;)V\n\n    goto :goto_b\n.end method'
ANCHOR_ILC="$ANCHOR_ILC" perl -0777 -ne 'exit((index($_, $ENV{ANCHOR_ILC}) >= 0) ? 0 : 1)' "$wvsm" || fail "WebView.smali internalLoadChanged body changed from the expected 2-arg shape — installed WebView drifted; update patch-framework.sh"
perl -0pi -e 's{\.method internalLoadChanged\(ILjava/lang/String;\)V.*?\.end method\n}{.method internalLoadChanged(ILjava/lang/String;)V\n    .registers 5\nnPageStarted(WebView,String,Bitmap) atn AOSP-compiled onPageStarted \@Override never received the ATL-only 2-arg form —nge16 saw onPageFinished fire but never onPageStarted). onPageFinished stays 2-arg (itsnull Bitmap (OSR has no favicon). The base 3-arg chains to the 2-arg form.\n    iget-object v0, p0, Landroid/webkit/WebView;->webViewClient:Landroid/webkit/WebViewClient;\n\n    if-eqz v0, :cond_eclipse_ilc_done\n\n    if-nez p1, :cond_eclipse_ilc_finished\n\n    const/4 v1, 0x0\n\n    invoke-virtual {v0, p0, p2, v1}, Landroid/webkit/WebViewClient;->onPageStarted(Landroid/webkit/WebView;Ljava/lang/String;Landroid/graphics/Bitmap;)V\n\n    return-void\n\n    :cond_eclipse_ilc_finished\n    const/4 v1, 0x3\n\n    if-ne p1, v1, :cond_eclipse_ilc_done\n\n    invoke-virtual {v0, p0, p2}, Landroid/webkit/WebViewClient;->onPageFinished(Landroid/webkit/WebView;Ljava/lang/String;)V\n\n    :cond_eclipse_ilc_done\n    return-void\n.end method\n}s' "$wvsm"
grep -qF -- '->onPageStarted(Landroid/webkit/WebView;Ljava/lang/String;Landroid/graphics/Bitmap;)V' "$wvsm" || fail "WebView.smali internalLoadChanged 3-arg onPageStarted dispatch insert failed (drift?)"
! grep -qF -- '->onPageStarted(Landroid/webkit/WebView;Ljava/lang/String;)V' "$wvsm" || fail "WebView.smali still dispatches the 2-arg onPageStarted (M6 3-arg dispatch incomplete)"




wvcsm="$work/smali/android/webkit/WebViewClient.smali"
[ -f "$wvcsm" ] || fail "WebViewClient.smali not found after baksmali"
! grep -qF 'Landroid/graphics/Bitmap;)V' "$wvcsm" || fail "WebViewClient.smali already declares a Bitmap-arg method (3-arg onPageStarted?) — installed WebViewClient drifted; update patch-framework.sh"
! grep -qF 'shouldOverrideUrlLoading' "$wvcsm" || fail "WebViewClient.smali already declares shouldOverrideUrlLoading — installed WebViewClient drifted; update patch-framework.sh"
cat >> "$wvcsm" <<'ECLIPSE_WVC_METHODS'





.method public onPageStarted(Landroid/webkit/WebView;Ljava/lang/String;Landroid/graphics/Bitmap;)V
    .registers 4

    invoke-virtual {p0, p1, p2}, Landroid/webkit/WebViewClient;->onPageStarted(Landroid/webkit/WebView;Ljava/lang/String;)V

    return-void
.end method







.method public shouldOverrideUrlLoading(Landroid/webkit/WebView;Ljava/lang/String;)Z
    .registers 4

    const/4 v0, 0x0

    return v0
.end method
ECLIPSE_WVC_METHODS
grep -qF -- 'onPageStarted(Landroid/webkit/WebView;Ljava/lang/String;Landroid/graphics/Bitmap;)V' "$wvcsm" || fail "WebViewClient 3-arg onPageStarted insert failed (drift?)"
grep -qF -- 'shouldOverrideUrlLoading(Landroid/webkit/WebView;Ljava/lang/String;)Z' "$wvcsm" || fail "WebViewClient shouldOverrideUrlLoading insert failed (drift?)"



jpm="$work/smali/android/app/job/JobParameters.smali"
[ -f "$jpm" ] || fail "JobParameters.smali not found after baksmali"
n="$(grep -cF '.method public getExtras()Landroid/os/PersistableBundle;' "$jpm")" || true
[ "$n" = "1" ] || fail "JobParameters.smali getExtras anchor not unique (found $n, expected 1) — installed JobParameters drifted; update patch-framework.sh"
! grep -qF 'getNetwork()Landroid/net/Network;' "$jpm" || fail "JobParameters.smali already declares getNetwork — drifted; update patch-framework.sh"
perl -0pi -e 's{(\.method public getExtras\(\)Landroid/os/PersistableBundle;.*?\.end method\n)}{$1ns null (no Network bound — AOSP-valid; the caller handles null).\n.method public getNetwork()Landroid/net/Network;\n    .locals 1\n\n    const/4 v0, 0x0\n\n    return-object v0\n.end method\n}s' "$jpm"
grep -qF 'getNetwork()Landroid/net/Network;' "$jpm" || fail "JobParameters.smali getNetwork insert failed (drift?)"









psm="$work/smali/android/graphics/Paint.smali"
[ -f "$psm" ] || fail "Paint.smali not found after baksmali"
n="$(grep -cF '.method public set(Landroid/graphics/Paint;)V' "$psm")" || true
[ "$n" = "1" ] || fail "Paint.smali set(Paint) anchor not unique (found $n, expected 1) — installed Paint drifted; update patch-framework.sh"
ANCHOR_PSET=$'.method public set(Landroid/graphics/Paint;)V\n    .registers 4\n\n    iget-wide v0, p0, Landroid/graphics/Paint;->paint:J\n\n    invoke-static {v0, v1}, Landroid/graphics/Paint;->native_recycle(J)V\n\n    iget-wide v0, p1, Landroid/graphics/Paint;->paint:J\n\n    invoke-static {v0, v1}, Landroid/graphics/Paint;->native_clone(J)J\n\n    move-result-wide v0\n\n    iput-wide v0, p0, Landroid/graphics/Paint;->paint:J\n\n    return-void\n.end method'
ANCHOR_PSET="$ANCHOR_PSET" perl -0777 -ne 'exit((index($_, $ENV{ANCHOR_PSET}) >= 0) ? 0 : 1)' "$psm" || fail "Paint.smali set(Paint) body changed from the expected recycle-before-clone shape — installed Paint drifted; update patch-framework.sh"
! grep -qF ':eclipse_not_self_set' "$psm" || fail "Paint.smali already carries the self-set guard — installed Paint drifted; update patch-framework.sh"
perl -0pi -e 's{(\.method public set\(Landroid/graphics/Paint;\)V\n    \.registers 4\n)}{$1nt.set(Paint src) no-ops when src == this.nt BEFORE cloning paint.paint, so an unguarded self-set clones a freedndle (use-after-free in the ATL reference C native; a warn-logged reset to a DEFAULT paintnder the Eclipse paint registry, where AOSP preserves the state). Guard restores the contract.\n    if-ne p0, p1, :eclipse_not_self_set\n\n    return-void\n\n    :eclipse_not_self_set\n}s' "$psm"
grep -qF 'if-ne p0, p1, :eclipse_not_self_set' "$psm" || fail "Paint.smali self-set guard insert failed (drift?)"












spsm="$work/smali/android/os/SystemProperties.smali"
[ -f "$spsm" ] || fail "SystemProperties.smali not found after baksmali"
rk="$(grep -cF '"release-keys"' "$spsm")" || true
[ "$rk" = "1" ] || fail "SystemProperties.smali '\"release-keys\"' count = $rk (expected 1) — ATL SystemProperties drifted; re-verify the honest-tags patch"
perl -0pi -e 's{(const-string [vp]\d+, )"release-keys"}{$1"test-keys"}' "$spsm"
grep -qF '"test-keys"' "$spsm" || fail "SystemProperties.smali test-keys insert failed (drift?)"
! grep -qF '"release-keys"' "$spsm" || fail "SystemProperties.smali still reports release-keys — honest-tags fix regressed"















pmsm="$work/smali/android/content/pm/PackageManager.smali"
[ -f "$pmsm" ] || fail "PackageManager.smali not found after baksmali"
n="$(grep -cF '.method public hasSystemFeature(Ljava/lang/String;)Z' "$pmsm")" || true
[ "$n" = "1" ] || fail "PackageManager.smali hasSystemFeature anchor not unique (found $n, expected 1) — installed PackageManager drifted; update patch-framework.sh"
! grep -qF ':eclipse_not_desktop_pc' "$pmsm" || fail "PackageManager.smali already carries the Eclipse desktop feature patch — installed framework drifted; update patch-framework.sh"
perl -0pi -e 's{(\.method public hasSystemFeature\(Ljava/lang/String;\)Z\n    \.registers 6\n)}{$1nput presentation.\n    const-string v0, "android.hardware.type.pc"\n\n    invoke-virtual {p1, v0}, Ljava/lang/String;->equals(Ljava/lang/Object;)Z\n\n    move-result v0\n\n    if-eqz v0, :eclipse_not_desktop_pc\n\n    const-string v1, "eclipse.touch_mode"\n\n    const-string v2, "off"\n\n    invoke-static {v1, v2}, Ljava/lang/System;->getProperty(Ljava/lang/String;Ljava/lang/String;)Ljava/lang/String;\n\n    move-result-object v1\n\n    const-string v2, "on"\n\n    invoke-virtual {v1, v2}, Ljava/lang/String;->equals(Ljava/lang/Object;)Z\n\n    move-result v0\n\n    xor-int/lit8 v0, v0, 0x1\n\n    return v0\n\n    :eclipse_not_desktop_pc\n    const-string v0, "android.hardware.touchscreen"\n\n    invoke-virtual {p1, v0}, Ljava/lang/String;->equals(Ljava/lang/Object;)Z\n\n    move-result v0\n\n    if-eqz v0, :eclipse_not_touchscreen\n\n    const-string v1, "eclipse.touch_mode"\n\n    const-string v2, "off"\n\n    invoke-static {v1, v2}, Ljava/lang/System;->getProperty(Ljava/lang/String;Ljava/lang/String;)Ljava/lang/String;\n\n    move-result-object v1\n\n    const-string v2, "on"\n\n    invoke-virtual {v1, v2}, Ljava/lang/String;->equals(Ljava/lang/Object;)Z\n\n    move-result v0\n\n    return v0\n\n    :eclipse_not_touchscreennSL+cpal implements this exact capability.\n    const-string v0, "android.hardware.audio.low_latency"\n\n    invoke-virtual {p1, v0}, Ljava/lang/String;->equals(Ljava/lang/Object;)Z\n\n    move-result v0\n\n    if-eqz v0, :eclipse_not_low_latency_audio\n\n    const/4 v0, 0x1\n\n    return v0\n\n    :eclipse_not_low_latency_audio\n}s' "$pmsm"
grep -qF ':eclipse_not_desktop_pc' "$pmsm" || fail "PackageManager.smali desktop feature insert failed (drift?)"
grep -qF ':eclipse_not_touchscreen' "$pmsm" || fail "PackageManager.smali touchscreen feature insert failed (drift?)"
grep -qF ':eclipse_not_low_latency_audio' "$pmsm" || fail "PackageManager.smali audio feature insert failed (drift?)"
grep -qF '"android.hardware.type.pc"' "$pmsm" || fail "PackageManager.smali lost the exact desktop-PC feature literal"
grep -qF '"android.hardware.touchscreen"' "$pmsm" || fail "PackageManager.smali lost the exact touchscreen feature literal"
grep -qF '"android.hardware.audio.low_latency"' "$pmsm" || fail "PackageManager.smali lost the exact low-latency feature literal"



mkdir -p "$work/smali-view/android/view" "$work/smali-view/android/app" "$work/smali-view/android/location" "$work/smali-view/android/os" "$work/smali-view/android/content/pm" "$work/smali-view/android/view/autofill" "$work/smali-view/android/webkit" "$work/smali-view/android/app/job" "$work/smali-view/android/graphics"
cp "$vsm" "$work/smali-view/android/view/View.smali"
cp "$dsm" "$work/smali-view/android/view/Display.smali"
cp "$here/smali/android/view/View\$OnCapturedPointerListener.smali" "$work/smali-view/android/view/"
cp "$here/smali/android/view/Display\$Mode.smali" "$work/smali-view/android/view/"
cp "$asm" "$work/smali-view/android/app/Activity.smali"
cp "$fsm" "$work/smali-view/android/app/Fragment.smali"
cp "$lmsm" "$work/smali-view/android/location/LocationManager.smali"
cp "$vibsm" "$work/smali-view/android/os/Vibrator.smali"
cp "$spsm" "$work/smali-view/android/os/SystemProperties.smali"
cp "$pmsm" "$work/smali-view/android/content/pm/PackageManager.smali"
cp "$afm" "$work/smali-view/android/view/autofill/AutofillManager.smali"
cp "$csm" "$work/smali-view/android/webkit/CookieManager.smali"
cp "$wvsm" "$work/smali-view/android/webkit/WebView.smali"
cp "$wssm" "$work/smali-view/android/webkit/WebSettings.smali"
cp "$wvcsm" "$work/smali-view/android/webkit/WebViewClient.smali"
cp "$jpm" "$work/smali-view/android/app/job/JobParameters.smali"
cp "$psm" "$work/smali-view/android/graphics/Paint.smali"
"$JAVA" -jar "$SMALI_JAR" assemble "$work/smali-view" -o "$work/jar/classes2.dex" >/dev/null




cp "$work/stock-classes.dex" "$work/jar/classes3.dex"
(cd "$work/jar" && "$JAR" cf api-impl.jar classes.dex classes2.dex classes3.dex)









wolf_src="$repo/vendor/atl/thirdparty/art_standalone/external/wolfssljni/src/java/com/wolfssl/provider/jsse/WolfSSLImplementSSLSession.java"
[ -f "$wolf_src" ] || fail "vendored WolfSSLImplementSSLSession.java missing at $wolf_src"
grep -qF 'if (numCerts == 0)' "$wolf_src" || fail "vendored wolfSSL source lost its zero-peer-certificate guard"
grep -qF 'throw new SSLPeerUnverifiedException("No peer certificate")' "$wolf_src" || fail "vendored wolfSSL source no longer throws SSLPeerUnverifiedException for an absent peer certificate"

for art_jar in "${ART_BOOT_JARS[@]}"; do
    cp "$ART_DIR/$art_jar" "$work/art/$art_jar"
done

unzip -p "$work/art/wolfssljni-hostdex.jar" classes.dex > "$work/wolf-classes.dex"
"$JAVA" -jar "$BAKSMALI_JAR" disassemble "$work/wolf-classes.dex" -o "$work/wolf-smali" >/dev/null
wolf_smali="$work/wolf-smali/com/wolfssl/provider/jsse/WolfSSLImplementSSLSession.smali"
[ -f "$wolf_smali" ] || fail "WolfSSLImplementSSLSession.smali not found in wolfssljni-hostdex.jar"
n="$(grep -cF '.method public declared-synchronized getPeerCertificates()[Ljava/security/cert/Certificate;' "$wolf_smali")" || true
[ "$n" = "1" ] || fail "wolfSSL getPeerCertificates method anchor not unique (found $n, expected 1) — ART hostdex drifted"
perl -0777 -ne 'if (/(\.method public declared-synchronized getPeerCertificates\(\)\[Ljava\/security\/cert\/Certificate;.*?\.end method)/s) { print $1 }' "$wolf_smali" > "$work/wolf-peer-method.smali"

WOLF_ZERO_ANCHOR=$'    .line 319\n    .local v7, "numCerts":I\n    :try_start_17\n    new-array v1, v7, [Ljava/security/cert/Certificate;'
if WOLF_ZERO_ANCHOR="$WOLF_ZERO_ANCHOR" perl -0777 -ne 'exit(index($_, $ENV{WOLF_ZERO_ANCHOR}) >= 0 ? 0 : 1)' "$work/wolf-peer-method.smali"; then

    WOLF_ZERO_ANCHOR="$WOLF_ZERO_ANCHOR" perl -0777 -pi -e 's{\Q$ENV{WOLF_ZERO_ANCHOR}\E}{    .line 319\n    .local v7, "numCerts":I\n    :try_start_17ndored source and SSLSession contract.\n    if-nez v7, :eclipse_wolf_has_peer_certificate\n\n    new-instance v10, Ljavax/net/ssl/SSLPeerUnverifiedException;\n\n    const-string v11, "No peer certificate"\n\n    invoke-direct {v10, v11}, Ljavax/net/ssl/SSLPeerUnverifiedException;-><init>(Ljava/lang/String;)V\n\n    throw v10\n\n    :eclipse_wolf_has_peer_certificate\n    new-array v1, v7, [Ljava/security/cert/Certificate;}s' "$wolf_smali"
    grep -qF ':eclipse_wolf_has_peer_certificate' "$wolf_smali" || fail "wolfSSL zero-peer-certificate guard insert failed"
    "$JAVA" -jar "$SMALI_JAR" assemble "$work/wolf-smali" -o "$work/wolf-classes-patched.dex" >/dev/null
    mkdir -p "$work/wolf-jar-update"
    cp "$work/wolf-classes-patched.dex" "$work/wolf-jar-update/classes.dex"
    (cd "$work/wolf-jar-update" && "$JAR" uf "$work/art/wolfssljni-hostdex.jar" classes.dex)
else



    perl -0777 -ne 'exit(/getPeerCertificateNum\(\)I.*?move-result v7.*?if-nez v7,.*?new-instance .*?SSLPeerUnverifiedException;.*?const-string .*?"No peer certificate".*?throw .*?new-array v1, v7/s ? 0 : 1)' "$work/wolf-peer-method.smali" || fail "wolfSSL getPeerCertificates no longer matches either the known stale body or the source-correct zero guard — ART hostdex drifted"
fi


unzip -p "$work/art/wolfssljni-hostdex.jar" classes.dex > "$work/wolf-verify.dex"
"$JAVA" -jar "$BAKSMALI_JAR" disassemble "$work/wolf-verify.dex" -o "$work/wolf-verify-smali" >/dev/null
wolf_verify="$work/wolf-verify-smali/com/wolfssl/provider/jsse/WolfSSLImplementSSLSession.smali"
perl -0777 -ne 'if (/(\.method public declared-synchronized getPeerCertificates\(\)\[Ljava\/security\/cert\/Certificate;.*?\.end method)/s) { print $1 }' "$wolf_verify" > "$work/wolf-peer-method-verify.smali"
perl -0777 -ne 'exit(/getPeerCertificateNum\(\)I.*?move-result v7.*?if-nez v7,.*?new-instance .*?SSLPeerUnverifiedException;.*?const-string .*?"No peer certificate".*?throw .*?new-array v1, v7/s ? 0 : 1)' "$work/wolf-peer-method-verify.smali" || fail "built wolfssljni-hostdex.jar does not enforce the zero-peer-certificate SSLSession contract"


mkdir -p "$OUT"
cp "$work/jar/api-impl.jar" "$OUT/api-impl.jar"
ln -sfn "$ORIG_FW/framework-res.apk" "$OUT/framework-res.apk"
ln -sfn "$ORIG_FW/natives" "$OUT/natives"




mkdir -p "$OUT/art"
art_ready="$OUT/art/.eclipse-art-overlay-v1"
rm -f "$art_ready"
for art_jar in "${ART_BOOT_JARS[@]}"; do
    cp "$work/art/$art_jar" "$OUT/art/$art_jar"
done
printf '%s\n' 'eclipse-art-overlay-v1' > "$art_ready"

echo "OK: patched framework overlay installed at $OUT"
echo "    classes.dex (javac-patched): $(ls -l "$work/jar/classes.dex" | awk '{print $5}') bytes; classes2.dex (smali Android API gaps, including LocationManager): $(ls -l "$work/jar/classes2.dex" | awk '{print $5}') bytes; classes3.dex (stock): $(ls -l "$work/jar/classes3.dex" | awk '{print $5}') bytes"
echo "    ART boot jars: ${#ART_BOOT_JARS[@]} copied to $OUT/art; wolfSSL zero-peer-certificate contract verified"
echo "    use it with: export ECLIPSE_ANDROID_FRAMEWORK_DIR=\"$OUT\""
