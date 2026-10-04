#!/usr/bin/env bash

set -euo pipefail

here="$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)"
repo="$(cd "$here/../.." && pwd)"

ATL_SRC="${ATL_SRC:-$repo/vendor/atl/src/api-impl}"
ORIG_FW="${ORIG_FW:-/usr/lib/java/dex/android_translation_layer}"
ART_DIR="${ART_DIR:-/usr/lib/java/dex/art}"
OUT="${OUT:-${XDG_CACHE_HOME:-$HOME/.cache}/eclipse/framework-patched}"
CORE_ALL_CLASSES="${CORE_ALL_CLASSES:-$ART_DIR/../../core-all_classes.jar}"
R8_JAR="${R8_JAR:-$repo/vendor/toolchain/r8/r8-8.13.23.jar}"
R8_SHA256='e3cdcb003d9beca956209ad6b9e9df31f26b732bfaed9c7c8674e903ca9f3b81'
WOLFSSL_SOURCE="${WOLFSSL_SOURCE:-$repo/vendor/atl/thirdparty/art_standalone/external/wolfssljni/src/java/com/wolfssl/provider/jsse/WolfSSLImplementSSLSession.java}"
ZONEINFO_DIR="${ZONEINFO_DIR:-/usr/share/zoneinfo}"

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

find_r8_java() {
    local candidate
    for candidate in "$repo"/vendor/toolchain/jdk-*/bin/java /usr/lib/jvm/*/bin/java "$(command -v java || true)"; do
        if [ -x "$candidate" ] && "$candidate" -cp "$R8_JAR" com.android.tools.r8.D8 --version 2>/dev/null | grep -q '^D8 8\.13\.23 ';
        then
            echo "$candidate"
            return
        fi
    done
}

JAVAC="${JAVAC:-$(find_jdk_tool javac)}"
JAR="${JAR:-$(find_jdk_tool jar)}"
DX="${DX:-$(command -v dx || true)}"

fail() { echo "ERROR: $*" >&2; exit 1; }
require_executable() {
    local executable="$1"
    local failure="$2"
    if [ -z "$executable" ] || [ ! -x "$executable" ]; then
        fail "$failure"
    fi
}
require_executable "$JAVAC" "javac not found (set JAVAC, or vendor a JDK at vendor/toolchain/jdk-*/)"
require_executable "$JAR" "jar not found (set JAR, or vendor a JDK at vendor/toolchain/jdk-*/)"
require_executable "$DX" "dx not found (set DX or install the Android dx tool)"
if "$JAVAC" --release 8 -version >/dev/null 2>&1; then
    JAVAC_8_FLAGS=(--release 8 -Xlint:-options)
else
    javac_version="$("$JAVAC" -version 2>&1)"
    case "$javac_version" in
        'javac 1.8.'*) JAVAC_8_FLAGS=(-source 8 -target 8) ;;
        *) fail "javac cannot target Java 8: $javac_version" ;;
    esac
fi
[ -f "$ATL_SRC/android/os/Build.java" ] || fail "ATL api-impl sources not found at $ATL_SRC (set ATL_SRC)"
[ -f "$ORIG_FW/api-impl.jar" ] || fail "stock framework not found at $ORIG_FW (set ORIG_FW; install android-translation-layer)"
[ -f "$CORE_ALL_CLASSES" ] || fail "ART core class archive not found at $CORE_ALL_CLASSES (set CORE_ALL_CLASSES)"
[ -f "$R8_JAR" ] || fail "R8 8.13.23 not found at $R8_JAR (set R8_JAR)"
[ -f "$ZONEINFO_DIR/tzdata.zi" ] || fail "IANA zoneinfo with tzdata.zi not found at $ZONEINFO_DIR (set ZONEINFO_DIR)"
r8_sha256="$(sha256sum "$R8_JAR" | awk '{print $1}')"
[ "$r8_sha256" = "$R8_SHA256" ] || fail "R8 checksum mismatch at $R8_JAR (expected $R8_SHA256, got $r8_sha256)"
R8_JAVA="${R8_JAVA:-$(find_r8_java)}"
require_executable "$R8_JAVA" "Java runtime compatible with R8 8.13.23 not found (set R8_JAVA)"
DALVIKVM="${DALVIKVM:-$(command -v dalvikvm || true)}"
require_executable "$DALVIKVM" "dalvikvm not found (set DALVIKVM or install art_standalone)"
for art_jar in "${ART_BOOT_JARS[@]}"; do
    [ -f "$ART_DIR/$art_jar" ] || fail "ART boot jar missing at $ART_DIR/$art_jar (set ART_DIR; install the pinned art_standalone runtime)"
done

JAVA="${JAVA:-$(find_jdk_tool java)}"
BAKSMALI_JAR="${BAKSMALI_JAR:-$repo/vendor/toolchain/smali/baksmali-2.5.2.jar}"
SMALI_JAR="${SMALI_JAR:-$repo/vendor/toolchain/smali/smali-2.5.2.jar}"
require_executable "$JAVA" "java not found (set JAVA, or vendor a JDK at vendor/toolchain/jdk-*/)"
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

kgps_src="$here/src/android/security/keystore/KeyGenParameterSpec.java"
[ -f "$kgps_src" ] || fail "KeyGenParameterSpec compatibility surface missing at $kgps_src"
for kgps_needle in \
    'implements AlgorithmParameterSpec' \
    'setAlgorithmParameterSpec(AlgorithmParameterSpec spec)' \
    'setDigests(String... digests)' \
    'setAttestationChallenge(byte[] attestationChallenge)' \
    'setKeyValidityStart(Date keyValidityStart)' \
    'setIsStrongBoxBacked(boolean strongBoxBacked)' \
    'setCertificateSubject(X500Principal certificateSubject)' \
    'setCertificateSerialNumber(BigInteger certificateSerialNumber)' \
    'setCertificateNotBefore(Date certificateNotBefore)' \
    'setCertificateNotAfter(Date certificateNotAfter)'
do
    grep -qF "$kgps_needle" "$kgps_src" || fail "KeyGenParameterSpec app contract regressed: missing '$kgps_needle'"
done

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

wre_src="$here/src/android/webkit/WebResourceError.java"
[ -f "$wre_src" ] || fail "WebResourceError.java missing at $wre_src"
grep -qF 'WebResourceError(int errorCode, CharSequence description)' "$wre_src" || fail "WebResourceError.java lost the (int, CharSequence) constructor Eclipse's onReceivedError upcall builds"
wrr_src="$here/src/android/webkit/EclipseWebResourceRequest.java"
[ -f "$wrr_src" ] || fail "EclipseWebResourceRequest.java missing at $wrr_src"
grep -qF 'implements WebResourceRequest' "$wrr_src" || fail "EclipseWebResourceRequest.java no longer implements WebResourceRequest"

pc_src="$here/src/android/view/PixelCopy.java"
[ -f "$pc_src" ] || fail "PixelCopy.java compatibility surface missing at $pc_src"
grep -qF 'public interface OnPixelCopyFinishedListener' "$pc_src" || fail "PixelCopy.java lost its completion-listener API"
grep -qF 'listenerThread.post(new Runnable()' "$pc_src" || fail "PixelCopy.java no longer dispatches completion through the caller's Handler"
grep -qF 'listener.onPixelCopyFinished(ERROR_SOURCE_NO_DATA);' "$pc_src" || fail "PixelCopy.java no longer reports the honest ERROR_SOURCE_NO_DATA result"
! grep -qF 'listener.onPixelCopyFinished(SUCCESS);' "$pc_src" || fail "PixelCopy.java fabricates SUCCESS without a pixel-copy backend"

si_src="$here/src/android/content/pm/SigningInfo.java"
[ -f "$si_src" ] || fail "SigningInfo implementation missing at $si_src"
grep -qF 'return pastSigningCertificates;' "$si_src" || fail "SigningInfo.java no longer reports the rotation history — the AOSP getSigningCertificateHistory contract regressed"
sc_src="$here/src/android/content/pm/SigningCertificates.java"
[ -f "$sc_src" ] || fail "host signing-certificate bridge missing at $sc_src"
grep -qF 'private static native byte[][] native_signingCertificateHistory();' "$sc_src" || fail "SigningCertificates.java lost its host-verified certificate native"

pl_src="$here/src/android/os/PreloadedLibrary.java"
[ -f "$pl_src" ] || fail "preloaded-library bridge missing at $pl_src"
grep -qF 'public static native String initialize(String name);' "$pl_src" || fail "PreloadedLibrary.java lost its JNI_OnLoad trampoline native"

r_src="$ATL_SRC/com/android/internal/R.java"
[ -f "$r_src" ] || fail "vendored com/android/internal/R.java not found at $r_src (set ATL_SRC)"
grep -qE 'public[[:space:]]+static[[:space:]]+final[[:space:]]+int[[:space:]]+id[[:space:]]*=[[:space:]]*0x010100d0;' "$r_src" || fail "vendored internal R.attr.id != 0x010100d0 — ATL source drifted; re-verify the overlay's inlined constants"
grep -qE 'public[[:space:]]+static[[:space:]]+final[[:space:]]+int[[:space:]]+theme[[:space:]]*=[[:space:]]*0x01010000;' "$r_src" || fail "vendored internal R.attr.theme != 0x01010000 — ATL source drifted; re-verify the overlay's inlined constants"
[ ! -e "$here/stubs/com/android/internal/R.java" ] || fail "stub com/android/internal/R.java re-appeared — javac would inline its placeholder constants into the overlay dex (the 2026-07-02 include-id NPE class); delete it (the vendored R.java is the compile input)"

"$JAVAC" "${JAVAC_8_FLAGS[@]}" -Xlint:all -Werror -d "$work/classes" \
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
    "$wre_src" \
    "$wrr_src" \
    "$here/src/android/app/KeyguardManager.java" \
    "$kgps_src" \
    "$pc_src" \
    "$si_src" \
    "$sc_src" \
    "$pl_src" \
    "$r_src"

for pattern in 'android/os/Build*.class' 'android/os/PowerManager*.class' 'android/net/NetworkRequest*.class' 'android/app/ActivityManager*.class' 'android/view/LayoutInflater*.class' 'android/view/PixelCopy*.class' 'android/webkit/ValueCallback*.class' 'android/webkit/JavascriptInterface*.class' 'android/webkit/EclipseBridgeProbe*.class' 'android/webkit/EclipseWebViewClientProbe*.class' 'android/webkit/WebResourceError*.class' 'android/webkit/EclipseWebResourceRequest*.class' 'android/app/KeyguardManager*.class' 'android/security/keystore/KeyGenParameterSpec*.class' 'android/content/pm/SigningInfo*.class' 'android/content/pm/SigningCertificates*.class' 'android/os/PreloadedLibrary*.class'; do
    dir="${pattern%/*}"
    mkdir -p "$work/stage/$dir"
    mapfile -t class_files < <(compgen -G "$work/classes/$pattern")
    [ "${#class_files[@]}" -gt 0 ] || fail "compiled classes missing for $pattern"
    cp "${class_files[@]}" "$work/stage/$dir/"
done

for forbidden in 'android/webkit/WebView.class' 'android/webkit/WebViewClient.class' \
                 'android/webkit/WebChromeClient.class' 'android/webkit/WebSettings.class' \
                 'android/webkit/WebResourceRequest.class' 'android/net/Uri.class' \
                 'android/os/Handler.class' 'android/os/Looper.class' \
                 'android/graphics/Bitmap.class' 'android/view/SurfaceView.class' \
                 'android/atl/ATLLoadedApp.class' \
                 'android/atl/EarlyPackageParser.class' \
                 'android/content/pm/PackageParser.class' \
                 "android/content/pm/PackageParser\$Package.class" \
                 'android/content/pm/PackageInfo.class' \
                 'android/content/pm/PackageManager.class' \
                 'android/content/pm/Signature.class' \
                 'android/util/DisplayMetrics.class'; do
    [ ! -e "$work/stage/$forbidden" ] || fail "compile-only stub $forbidden was staged into classes.dex — it would SHADOW the real class (first-dex-wins); fix the step-3 stage whitelist"
done
for stub in android/webkit/WebView.java android/webkit/WebViewClient.java \
            android/webkit/WebChromeClient.java android/webkit/WebSettings.java \
            android/webkit/WebResourceRequest.java android/net/Uri.java \
            android/os/Handler.java android/os/Looper.java android/graphics/Bitmap.java \
            android/view/SurfaceView.java; do
    [ -f "$here/stubs/$stub" ] || fail "compile-only stub $stub missing — the overlay's WebView classes and probes would not compile"
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

wresm="$work/smali-check/android/webkit/WebResourceError.smali"
[ -f "$wresm" ] || fail "WebResourceError.smali not in the built classes.dex — onReceivedError would throw NoClassDefFoundError"
grep -qF '.method constructor <init>(ILjava/lang/CharSequence;)V' "$wresm" || fail "dexed WebResourceError lost the (ILjava/lang/CharSequence;)V constructor src/framework.rs builds"
wrrsm="$work/smali-check/android/webkit/EclipseWebResourceRequest.smali"
[ -f "$wrrsm" ] || fail "EclipseWebResourceRequest.smali not in the built classes.dex"
grep -qF '.implements Landroid/webkit/WebResourceRequest;' "$wrrsm" || fail "dexed EclipseWebResourceRequest does not implement WebResourceRequest"
grep -qF '.method constructor <init>(Ljava/lang/String;Ljava/lang/String;ZZ)V' "$wrrsm" || fail "dexed EclipseWebResourceRequest lost the (Ljava/lang/String;Ljava/lang/String;ZZ)V constructor src/framework.rs builds"

pcsm="$work/smali-check/android/view/PixelCopy.smali"
pcrsm="$work/smali-check/android/view/PixelCopy\$1.smali"
[ -f "$pcsm" ] || fail "PixelCopy.smali not in the built classes.dex — the shutdown compatibility surface did not stage"
[ -f "$pcrsm" ] || fail "PixelCopy anonymous completion Runnable not in the built classes.dex"
grep -qF "request(Landroid/view/SurfaceView;Landroid/graphics/Bitmap;Landroid/view/PixelCopy\$OnPixelCopyFinishedListener;Landroid/os/Handler;)V" "$pcsm" || fail "dexed PixelCopy lost its SurfaceView request overload"
grep -qF 'Landroid/os/Handler;->post(Ljava/lang/Runnable;)Z' "$pcsm" || fail "dexed PixelCopy no longer posts completion through Handler"
grep -qF "Landroid/view/PixelCopy\$OnPixelCopyFinishedListener;->onPixelCopyFinished(I)V" "$pcrsm" || fail "dexed PixelCopy Runnable no longer invokes its listener"
grep -qE 'const/4 v[0-9]+, 0x3' "$pcrsm" || fail "dexed PixelCopy Runnable no longer reports ERROR_SOURCE_NO_DATA (3)"

scsm="$work/smali-check/android/content/pm/SigningCertificates.smali"
[ -f "$scsm" ] || fail "SigningCertificates.smali not in the built classes.dex"
[ -f "$work/smali-check/android/content/pm/SigningInfo.smali" ] || fail "SigningInfo.smali not in the built classes.dex — the stock stub would answer null"
grep -qF '.method private static native native_signingCertificateHistory()[[B' "$scsm" || fail "dexed SigningCertificates lost its host-verified certificate native"
grep -qF '0x8000000' "$scsm" || fail "dexed SigningCertificates lost the inlined GET_SIGNING_CERTIFICATES constant (0x08000000)"

plsm="$work/smali-check/android/os/PreloadedLibrary.smali"
[ -f "$plsm" ] || fail "PreloadedLibrary.smali not in the built classes.dex"
grep -qF '.method public static native initialize(Ljava/lang/String;)Ljava/lang/String;' "$plsm" || fail "dexed PreloadedLibrary lost its JNI_OnLoad trampoline native"

kgpssm="$work/smali-check/android/security/keystore/KeyGenParameterSpec.smali"
kgpsbsm="$work/smali-check/android/security/keystore/KeyGenParameterSpec\$Builder.smali"
[ -f "$kgpssm" ] || fail "KeyGenParameterSpec.smali not in the built classes.dex"
[ -f "$kgpsbsm" ] || fail "KeyGenParameterSpec Builder smali not in the built classes.dex"
grep -qF '.implements Ljava/security/spec/AlgorithmParameterSpec;' "$kgpssm" || fail "dexed KeyGenParameterSpec does not implement AlgorithmParameterSpec"
for kgps_method in \
    "setAlgorithmParameterSpec(Ljava/security/spec/AlgorithmParameterSpec;)Landroid/security/keystore/KeyGenParameterSpec\$Builder;" \
    "setDigests([Ljava/lang/String;)Landroid/security/keystore/KeyGenParameterSpec\$Builder;" \
    "setAttestationChallenge([B)Landroid/security/keystore/KeyGenParameterSpec\$Builder;" \
    "setKeyValidityStart(Ljava/util/Date;)Landroid/security/keystore/KeyGenParameterSpec\$Builder;" \
    "setIsStrongBoxBacked(Z)Landroid/security/keystore/KeyGenParameterSpec\$Builder;" \
    "setCertificateSubject(Ljavax/security/auth/x500/X500Principal;)Landroid/security/keystore/KeyGenParameterSpec\$Builder;" \
    "setCertificateSerialNumber(Ljava/math/BigInteger;)Landroid/security/keystore/KeyGenParameterSpec\$Builder;" \
    "setCertificateNotBefore(Ljava/util/Date;)Landroid/security/keystore/KeyGenParameterSpec\$Builder;" \
    "setCertificateNotAfter(Ljava/util/Date;)Landroid/security/keystore/KeyGenParameterSpec\$Builder;"
do
    grep -qF "$kgps_method" "$kgpsbsm" || fail "dexed KeyGenParameterSpec Builder lost '$kgps_method'"
done

unzip -p "$ORIG_FW/api-impl.jar" classes.dex > "$work/stock-classes.dex"
"$JAVA" -jar "$BAKSMALI_JAR" disassemble --debug-info false \
    "$work/stock-classes.dex" -o "$work/smali" >/dev/null

crsm="$work/smali/android/content/ContentResolver.smali"
[ -f "$crsm" ] || fail "ContentResolver.smali not found after baksmali of the installed framework"
open_typed_asset_file='openTypedAssetFile(Landroid/net/Uri;Ljava/lang/String;Landroid/os/Bundle;Landroid/os/CancellationSignal;)Landroid/content/res/AssetFileDescriptor;'
! grep -qF "$open_typed_asset_file" "$crsm" \
    || fail "ContentResolver.smali already declares $open_typed_asset_file — installed framework drifted; update patch-framework.sh"
for cr_method in \
    'acquireContentProviderClient(Landroid/net/Uri;)Landroid/content/ContentProviderClient;' \
    'acquireContentProviderClient(Ljava/lang/String;)Landroid/content/ContentProviderClient;' \
    'acquireUnstableContentProviderClient(Landroid/net/Uri;)Landroid/content/ContentProviderClient;' \
    'acquireUnstableContentProviderClient(Ljava/lang/String;)Landroid/content/ContentProviderClient;'
do
    grep -qF "$cr_method" "$crsm" \
        || fail "ContentResolver.smali lost upstream $cr_method — installed framework drifted; update patch-framework.sh"
done
cat >> "$crsm" <<'ECLIPSE_CONTENT_RESOLVER_METHODS'


.method public openTypedAssetFile(Landroid/net/Uri;Ljava/lang/String;Landroid/os/Bundle;Landroid/os/CancellationSignal;)Landroid/content/res/AssetFileDescriptor;
    .registers 5

    invoke-virtual {p0, p1, p2, p3, p4}, Landroid/content/ContentResolver;->openTypedAssetFileDescriptor(Landroid/net/Uri;Ljava/lang/String;Landroid/os/Bundle;Landroid/os/CancellationSignal;)Landroid/content/res/AssetFileDescriptor;

    move-result-object v0

    return-object v0
.end method
ECLIPSE_CONTENT_RESOLVER_METHODS
grep -qF "$open_typed_asset_file" "$crsm" || fail "ContentResolver $open_typed_asset_file insert failed"
grep -qF -- '->openTypedAssetFileDescriptor(Landroid/net/Uri;Ljava/lang/String;Landroid/os/Bundle;Landroid/os/CancellationSignal;)Landroid/content/res/AssetFileDescriptor;' "$crsm" \
    || fail "ContentResolver openTypedAssetFile bridge body insert failed"

connectivity_sm="$work/smali/android/net/ConnectivityManager.smali"
[ -f "$connectivity_sm" ] || fail "ConnectivityManager.smali not found after baksmali of the installed framework"
for connectivity_method in \
    'getLinkProperties(Landroid/net/Network;)Landroid/net/LinkProperties;' \
    'getNetworkInfo(Landroid/net/Network;)Landroid/net/NetworkInfo;'
do
    ! grep -qF "$connectivity_method" "$connectivity_sm" \
        || fail "ConnectivityManager.smali already declares $connectivity_method — installed framework drifted; update patch-framework.sh"
done
cat >> "$connectivity_sm" <<'ECLIPSE_CONNECTIVITY_MANAGER_METHODS'


.method public getLinkProperties(Landroid/net/Network;)Landroid/net/LinkProperties;
    .registers 3

    new-instance v0, Landroid/net/LinkProperties;

    invoke-direct {v0}, Landroid/net/LinkProperties;-><init>()V

    return-object v0
.end method

.method public getNetworkInfo(Landroid/net/Network;)Landroid/net/NetworkInfo;
    .registers 3

    invoke-virtual {p0}, Landroid/net/ConnectivityManager;->getActiveNetworkInfo()Landroid/net/NetworkInfo;

    move-result-object v0

    return-object v0
.end method
ECLIPSE_CONNECTIVITY_MANAGER_METHODS
for connectivity_method in \
    'getLinkProperties(Landroid/net/Network;)Landroid/net/LinkProperties;' \
    'getNetworkInfo(Landroid/net/Network;)Landroid/net/NetworkInfo;'
do
    grep -qF "$connectivity_method" "$connectivity_sm" \
        || fail "ConnectivityManager $connectivity_method insert failed"
done

shortcut_sm="$work/smali/android/content/pm/ShortcutManager.smali"
[ -f "$shortcut_sm" ] || fail "ShortcutManager.smali not found after baksmali of the installed framework"
for stock_shortcut_method in \
    'getShortcuts(I)Ljava/util/List;' \
    'removeAllDynamicShortcuts()V' \
    'setDynamicShortcuts(Ljava/util/List;)Z'
do
    grep -qF "$stock_shortcut_method" "$shortcut_sm" \
        || fail "ShortcutManager.smali lost stock method $stock_shortcut_method — installed framework drifted; update patch-framework.sh"
done
set_dynamic_true=$'    const/4 v0, 0x1\n\n    return v0\n.end method'
set_dynamic_false=$'    const/4 v0, 0x0\n\n    return v0\n.end method'
SET_DYNAMIC_TRUE="$set_dynamic_true" SET_DYNAMIC_FALSE="$set_dynamic_false" perl -0pi -e '
    $method = qr{(\.method public setDynamicShortcuts\(Ljava/util/List;\)Z.*?)(?=\.method|\z)}s;
    s{$method}{
        $body = $1;
        index($body, $ENV{SET_DYNAMIC_TRUE}) >= 0
            or die "setDynamicShortcuts body changed";
        $body =~ s/\Q$ENV{SET_DYNAMIC_TRUE}\E/$ENV{SET_DYNAMIC_FALSE}/;
        $body
    }e;
' "$shortcut_sm" || fail "ShortcutManager.setDynamicShortcuts body drifted; update patch-framework.sh"
grep -A20 -F '.method public setDynamicShortcuts(Ljava/util/List;)Z' "$shortcut_sm" \
    | grep -qF 'const/4 v0, 0x0' \
    || fail "ShortcutManager.setDynamicShortcuts honest-false patch failed"
for shortcut_method in \
    'addDynamicShortcuts(Ljava/util/List;)Z' \
    'disableShortcuts(Ljava/util/List;)V' \
    'enableShortcuts(Ljava/util/List;)V' \
    'getDynamicShortcuts()Ljava/util/List;' \
    'getManifestShortcuts()Ljava/util/List;' \
    'getMaxShortcutCountPerActivity()I' \
    'getPinnedShortcuts()Ljava/util/List;' \
    'isRateLimitingActive()Z' \
    'isRequestPinShortcutSupported()Z' \
    'pushDynamicShortcut(Landroid/content/pm/ShortcutInfo;)V' \
    'removeDynamicShortcuts(Ljava/util/List;)V' \
    'reportShortcutUsed(Ljava/lang/String;)V' \
    'requestPinShortcut(Landroid/content/pm/ShortcutInfo;Landroid/content/IntentSender;)Z'
do
    ! grep -qF "$shortcut_method" "$shortcut_sm" \
        || fail "ShortcutManager.smali already declares $shortcut_method — installed framework drifted; update patch-framework.sh"
done
cat >> "$shortcut_sm" <<'ECLIPSE_SHORTCUT_MANAGER_METHODS'


.method public addDynamicShortcuts(Ljava/util/List;)Z
    .registers 3

    const/4 v0, 0x0

    return v0
.end method

.method public disableShortcuts(Ljava/util/List;)V
    .registers 2

    return-void
.end method

.method public enableShortcuts(Ljava/util/List;)V
    .registers 2

    return-void
.end method

.method public getDynamicShortcuts()Ljava/util/List;
    .registers 2

    invoke-static {}, Ljava/util/Collections;->emptyList()Ljava/util/List;

    move-result-object v0

    return-object v0
.end method

.method public getManifestShortcuts()Ljava/util/List;
    .registers 2

    invoke-static {}, Ljava/util/Collections;->emptyList()Ljava/util/List;

    move-result-object v0

    return-object v0
.end method

.method public getMaxShortcutCountPerActivity()I
    .registers 2

    const/4 v0, 0x0

    return v0
.end method

.method public getPinnedShortcuts()Ljava/util/List;
    .registers 2

    invoke-static {}, Ljava/util/Collections;->emptyList()Ljava/util/List;

    move-result-object v0

    return-object v0
.end method

.method public isRateLimitingActive()Z
    .registers 2

    const/4 v0, 0x0

    return v0
.end method

.method public isRequestPinShortcutSupported()Z
    .registers 2

    const/4 v0, 0x0

    return v0
.end method

.method public pushDynamicShortcut(Landroid/content/pm/ShortcutInfo;)V
    .registers 2

    return-void
.end method

.method public removeDynamicShortcuts(Ljava/util/List;)V
    .registers 2

    return-void
.end method

.method public reportShortcutUsed(Ljava/lang/String;)V
    .registers 2

    return-void
.end method

.method public requestPinShortcut(Landroid/content/pm/ShortcutInfo;Landroid/content/IntentSender;)Z
    .registers 4

    const/4 v0, 0x0

    return v0
.end method
ECLIPSE_SHORTCUT_MANAGER_METHODS
for shortcut_method in \
    'addDynamicShortcuts(Ljava/util/List;)Z' \
    'disableShortcuts(Ljava/util/List;)V' \
    'enableShortcuts(Ljava/util/List;)V' \
    'getDynamicShortcuts()Ljava/util/List;' \
    'getManifestShortcuts()Ljava/util/List;' \
    'getMaxShortcutCountPerActivity()I' \
    'getPinnedShortcuts()Ljava/util/List;' \
    'isRateLimitingActive()Z' \
    'isRequestPinShortcutSupported()Z' \
    'pushDynamicShortcut(Landroid/content/pm/ShortcutInfo;)V' \
    'removeDynamicShortcuts(Ljava/util/List;)V' \
    'reportShortcutUsed(Ljava/lang/String;)V' \
    'requestPinShortcut(Landroid/content/pm/ShortcutInfo;Landroid/content/IntentSender;)Z'
do
    grep -qF "$shortcut_method" "$shortcut_sm" || fail "ShortcutManager $shortcut_method insert failed"
done

vsm="$work/smali/android/view/View.smali"
[ -f "$vsm" ] || fail "View.smali not found after baksmali of the installed framework"
for a in \
    ".field private on_touch_listener:Landroid/view/View\$OnTouchListener;" \
    ".method public setOnClickListener(Landroid/view/View\$OnClickListener;)V" \
    "        Landroid/view/View\$DeclaredOnClickListener;,"; do
    n="$(grep -cF "$a" "$vsm")" || true
    [ "$n" = "1" ] || fail "View.smali anchor not unique (found $n, expected 1): $a — installed View drifted; update patch-framework.sh"
done

perl -0pi -e 's{(\.field private on_touch_listener:Landroid/view/View\$OnTouchListener;\n)}{$1.field private mCapturedPointerListener:Landroid/view/View\$OnCapturedPointerListener;\n}' "$vsm"

perl -0pi -e 's{(\.method public setOnClickListener\(Landroid/view/View\$OnClickListener;\)V.*?\.end method\n)}{$1.method public setOnCapturedPointerListener(Landroid/view/View\$OnCapturedPointerListener;)V\n    .registers 2\n\n    iput-object p1, p0, Landroid/view/View;->mCapturedPointerListener:Landroid/view/View\$OnCapturedPointerListener;\n\n    return-void\n.end method\n}s' "$vsm"

perl -0pi -e 's{(value = \{\n)(        Landroid/view/View\$DeclaredOnClickListener;,\n)}{$1        Landroid/view/View\$OnCapturedPointerListener;,\n$2}' "$vsm"
grep -qF "setOnCapturedPointerListener(Landroid/view/View\$OnCapturedPointerListener;)V" "$vsm" || fail "View.smali setter insert failed (drift?)"
grep -qF "mCapturedPointerListener:Landroid/view/View\$OnCapturedPointerListener;" "$vsm" || fail "View.smali field insert failed (drift?)"

n="$(grep -cF '.method public setImportantForAutofill(I)V' "$vsm")" || true
[ "$n" = "1" ] || fail "View.smali upstream setImportantForAutofill count = $n (expected 1) — installed View drifted; update patch-framework.sh"
! grep -qF 'setAutofillHints([Ljava/lang/String;)V' "$vsm" || fail "View.smali already declares setAutofillHints — installed View drifted; update patch-framework.sh"
perl -0pi -e 's{(\.method public setOnCapturedPointerListener\(Landroid/view/View\$OnCapturedPointerListener;\)V.*?\.end method\n)}{$1.method public setAutofillHints([Ljava/lang/String;)V\n    .registers 2\n\n    return-void\n.end method\n}s' "$vsm"
grep -qF 'setAutofillHints([Ljava/lang/String;)V' "$vsm" || fail "View.smali setAutofillHints insert failed (drift?)"
grep -qF 'setImportantForAutofill(I)V' "$vsm" || fail "View.smali setImportantForAutofill insert failed (drift?)"

etsm="$work/smali/android/widget/EditText.smali"
[ -f "$etsm" ] || fail "EditText.smali not found after baksmali of the installed framework"
grep -qF 'iget-wide v0, p0, Landroid/widget/EditText;->widget:J' "$etsm" || fail "EditText.smali no longer reads its widget handle — installed EditText drifted; update patch-framework.sh"
for edit_text_method in 'getSelectionStart' 'getSelectionEnd' 'setSelection'; do
    ! grep -qF "$edit_text_method" "$etsm" || fail "EditText.smali already declares $edit_text_method — installed EditText drifted; update patch-framework.sh"
done
cat >> "$etsm" <<'ECLIPSE_EDIT_TEXT_SELECTION'


.method public getSelectionStart()I
    .registers 3

    iget-wide v0, p0, Landroid/widget/EditText;->widget:J

    invoke-direct {p0, v0, v1}, Landroid/widget/EditText;->native_getSelectionStart(J)I

    move-result v0

    return v0
.end method

.method public getSelectionEnd()I
    .registers 3

    iget-wide v0, p0, Landroid/widget/EditText;->widget:J

    invoke-direct {p0, v0, v1}, Landroid/widget/EditText;->native_getSelectionEnd(J)I

    move-result v0

    return v0
.end method

.method public setSelection(I)V
    .registers 4

    iget-wide v0, p0, Landroid/widget/EditText;->widget:J

    invoke-direct {p0, v0, v1, p1}, Landroid/widget/EditText;->native_setSelection(JI)V

    return-void
.end method

.method private native native_getSelectionStart(J)I
.end method

.method private native native_getSelectionEnd(J)I
.end method

.method private native native_setSelection(JI)V
.end method
ECLIPSE_EDIT_TEXT_SELECTION
for edit_text_method in '.method public getSelectionStart()I' '.method public getSelectionEnd()I' '.method public setSelection(I)V'; do
    n="$(grep -cF "$edit_text_method" "$etsm")" || true
    [ "$n" = "1" ] || fail "EditText.smali selection override '$edit_text_method' found $n times (expected 1)"
done

dsm="$work/smali/android/view/Display.smali"
[ -f "$dsm" ] || fail "Display.smali not found after baksmali"
! grep -qF 'refresh_rate' "$dsm" || fail "Display.smali already declares refresh-rate state — installed Display drifted; update patch-framework.sh"
! grep -qF 'getSupportedRefreshRates()[F' "$dsm" || fail "Display.smali already declares getSupportedRefreshRates — installed Display drifted; update patch-framework.sh"
n="$(grep -cF '.field public static window_width:I' "$dsm")" || true
[ "$n" = "1" ] || fail "Display.smali window_width field anchor not unique (found $n, expected 1) — installed Display drifted; update patch-framework.sh"
smali_count() {
    NEEDLE="$2" perl -0777 -ne 'print scalar(() = /\Q$ENV{NEEDLE}\E/g)' "$1"
}
replace_upstream_method() {
    local smali="$1" upstream="$2" patched="$3" method="$4" n
    n="$(smali_count "$smali" "$upstream")"
    [ "$n" = "1" ] || fail "${smali##*/} $method is not the upstream body (found $n, expected 1) — installed framework drifted; update patch-framework.sh"
    ANCHOR="$upstream" PATCHED="$patched" perl -0777 -pi -e 's{\Q$ENV{ANCHOR}\E}{$ENV{PATCHED}}' "$smali"
    n="$(smali_count "$smali" "$patched")"
    [ "$n" = "1" ] || fail "${smali##*/} $method insert found $n times (expected 1)"
}
UPSTREAM_DISPLAY_CLINIT=$'.method static constructor <clinit>()V\n    .registers 1\n\n    const/16 v0, 0x3c0\n\n    sput v0, Landroid/view/Display;->window_width:I\n\n    const/16 v0, 0x21c\n\n    sput v0, Landroid/view/Display;->window_height:I\n\n    return-void\n.end method\n'
DISPLAY_CLINIT=$'.method static constructor <clinit>()V\n    .registers 4\n\n    const/16 v0, 0x3c0\n\n    sput v0, Landroid/view/Display;->window_width:I\n\n    const/16 v0, 0x21c\n\n    sput v0, Landroid/view/Display;->window_height:I\n\n    const/4 v0, 0x2\n\n    new-array v0, v0, [F\n\n    const/high16 v1, 0x42700000\n\n    const/4 v2, 0x0\n\n    aput v1, v0, v2\n\n    const/4 v3, 0x1\n\n    aput v1, v0, v3\n\n    sput-object v0, Landroid/view/Display;->refresh_rates:[F\n\n    return-void\n.end method\n'
replace_upstream_method "$dsm" "$UPSTREAM_DISPLAY_CLINIT" "$DISPLAY_CLINIT" '<clinit> window-size initializer'
perl -0pi -e 's{(\.field public static window_width:I\n)}{$1\n.field public static volatile refresh_rates:[F\n}' "$dsm"
UPSTREAM_GET_REFRESH_RATE_PATTERN='\.method public getRefreshRate\(\)F\n    \.registers 2\n\n    const/high16 v0, 0x42700000[^\n]*\n\n    return v0\n\.end method\n'
n="$(PATTERN="$UPSTREAM_GET_REFRESH_RATE_PATTERN" perl -0777 -ne 'print scalar(() = /$ENV{PATTERN}/g)' "$dsm")"
[ "$n" = "1" ] || fail "Display.smali getRefreshRate is not the upstream constant 60 Hz body (found $n, expected 1) — installed Display drifted; update patch-framework.sh"
DISPLAY_REFRESH_RATE_METHODS=$'.method public getRefreshRate()F\n    .registers 3\n\n    sget-object v0, Landroid/view/Display;->refresh_rates:[F\n\n    const/4 v1, 0x0\n\n    aget v0, v0, v1\n\n    return v0\n.end method\n\n.method public getSupportedRefreshRates()[F\n    .registers 4\n\n    sget-object v0, Landroid/view/Display;->refresh_rates:[F\n\n    const/4 v1, 0x1\n\n    array-length v2, v0\n\n    invoke-static {v0, v1, v2}, Ljava/util/Arrays;->copyOfRange([FII)[F\n\n    move-result-object v0\n\n    return-object v0\n.end method\n\n.method public static setRefreshRates(F[F)V\n    .registers 6\n\n    array-length v0, p1\n\n    add-int/lit8 v1, v0, 0x1\n\n    new-array v1, v1, [F\n\n    const/4 v2, 0x0\n\n    aput p0, v1, v2\n\n    const/4 v3, 0x1\n\n    invoke-static {p1, v2, v1, v3, v0}, Ljava/lang/System;->arraycopy(Ljava/lang/Object;ILjava/lang/Object;II)V\n\n    sput-object v1, Landroid/view/Display;->refresh_rates:[F\n\n    return-void\n.end method\n'
PATTERN="$UPSTREAM_GET_REFRESH_RATE_PATTERN" PATCHED="$DISPLAY_REFRESH_RATE_METHODS" perl -0777 -pi -e 's{$ENV{PATTERN}}{$ENV{PATCHED}}' "$dsm"
n="$(smali_count "$dsm" "$DISPLAY_REFRESH_RATE_METHODS")"
[ "$n" = "1" ] || fail "Display.smali refresh-rate methods insert found $n times (expected 1)"
UPSTREAM_GET_MODE=$'.method public getMode()Landroid/view/Display$Mode;\n    .registers 2\n\n    new-instance v0, Landroid/view/Display$Mode;\n\n    invoke-direct {v0}, Landroid/view/Display$Mode;-><init>()V\n\n    return-object v0\n.end method\n'
DISPLAY_GET_MODE=$'.method public getMode()Landroid/view/Display$Mode;\n    .registers 7\n\n    sget-object v0, Landroid/view/Display;->refresh_rates:[F\n\n    array-length v1, v0\n\n    const/4 v2, 0x0\n\n    aget v3, v0, v2\n\n    const/4 v2, 0x1\n\n    :find_current_mode\n    if-ge v2, v1, :current_mode_found\n\n    aget v4, v0, v2\n\n    cmpl-float v5, v4, v3\n\n    if-eqz v5, :current_mode_found\n\n    add-int/lit8 v2, v2, 0x1\n\n    goto :find_current_mode\n\n    :current_mode_found\n    new-instance v0, Landroid/view/Display$Mode;\n\n    sget v1, Landroid/view/Display;->window_width:I\n\n    sget v4, Landroid/view/Display;->window_height:I\n\n    invoke-direct {v0, v2, v1, v4, v3}, Landroid/view/Display$Mode;-><init>(IIIF)V\n\n    return-object v0\n.end method\n'
replace_upstream_method "$dsm" "$UPSTREAM_GET_MODE" "$DISPLAY_GET_MODE" 'getMode'
UPSTREAM_GET_SUPPORTED_MODES=$'.method public getSupportedModes()[Landroid/view/Display$Mode;\n    .registers 4\n\n    const/4 v0, 0x1\n\n    new-array v0, v0, [Landroid/view/Display$Mode;\n\n    const/4 v1, 0x0\n\n    invoke-virtual {p0}, Landroid/view/Display;->getMode()Landroid/view/Display$Mode;\n\n    move-result-object v2\n\n    aput-object v2, v0, v1\n\n    return-object v0\n.end method\n'
DISPLAY_GET_SUPPORTED_MODES=$'.method public getSupportedModes()[Landroid/view/Display$Mode;\n    .registers 10\n\n    sget-object v0, Landroid/view/Display;->refresh_rates:[F\n\n    array-length v1, v0\n\n    add-int/lit8 v2, v1, -0x1\n\n    new-array v2, v2, [Landroid/view/Display$Mode;\n\n    sget v3, Landroid/view/Display;->window_width:I\n\n    sget v4, Landroid/view/Display;->window_height:I\n\n    const/4 v5, 0x1\n\n    :next_supported_mode\n    if-ge v5, v1, :supported_modes_done\n\n    aget v6, v0, v5\n\n    new-instance v7, Landroid/view/Display$Mode;\n\n    invoke-direct {v7, v5, v3, v4, v6}, Landroid/view/Display$Mode;-><init>(IIIF)V\n\n    add-int/lit8 v8, v5, -0x1\n\n    aput-object v7, v2, v8\n\n    add-int/lit8 v5, v5, 0x1\n\n    goto :next_supported_mode\n\n    :supported_modes_done\n    return-object v2\n.end method\n'
replace_upstream_method "$dsm" "$UPSTREAM_GET_SUPPORTED_MODES" "$DISPLAY_GET_SUPPORTED_MODES" 'getSupportedModes'
for display_needle in \
    '.field public static volatile refresh_rates:[F' \
    '.method public getRefreshRate()F' \
    '.method public getSupportedRefreshRates()[F' \
    '.method public static setRefreshRates(F[F)V' \
    ".method public getMode()Landroid/view/Display\$Mode;" \
    ".method public getSupportedModes()[Landroid/view/Display\$Mode;"
do
    n="$(grep -cF -- "$display_needle" "$dsm")" || true
    [ "$n" = "1" ] || fail "Display.smali refresh-rate declaration '$display_needle' found $n times (expected 1)"
done
n="$(grep -cF '0x42700000' "$dsm")" || true
[ "$n" = "1" ] || fail "Display.smali holds $n 60 Hz constants (expected only the <clinit> default)"

mesm="$work/smali/android/view/MotionEvent.smali"
[ -f "$mesm" ] || fail "MotionEvent.smali not found after baksmali"
! grep -qF 'eclipseDownTime' "$mesm" || fail "MotionEvent.smali already declares eclipseDownTime — installed MotionEvent drifted; update patch-framework.sh"
n="$(grep -cxF '.field source:I' "$mesm")" || true
[ "$n" = "1" ] || fail "MotionEvent.smali source field anchor found $n times (expected 1) — installed MotionEvent drifted; update patch-framework.sh"
perl -0pi -e 's{^(\.field source:I\n)}{$1\n.field eclipseDownTime:J\n}m' "$mesm"
replace_upstream_method "$mesm" \
    $'    iput-wide p3, p0, Landroid/view/MotionEvent;->eventTime:J\n' \
    $'    iput-wide p3, p0, Landroid/view/MotionEvent;->eventTime:J\n\n    iput-wide p3, p0, Landroid/view/MotionEvent;->eclipseDownTime:J\n' \
    'constructor down time'
replace_upstream_method "$mesm" \
    $'    iput-wide p2, v0, Landroid/view/MotionEvent;->eventTime:J\n' \
    $'    iput-wide p2, v0, Landroid/view/MotionEvent;->eventTime:J\n\n    iput-wide p0, v0, Landroid/view/MotionEvent;->eclipseDownTime:J\n' \
    'single-pointer obtain down time'
replace_upstream_method "$mesm" \
    $'    iput-wide p2, v1, Landroid/view/MotionEvent;->eventTime:J\n' \
    $'    iput-wide p2, v1, Landroid/view/MotionEvent;->eventTime:J\n\n    iput-wide p0, v1, Landroid/view/MotionEvent;->eclipseDownTime:J\n' \
    'multi-pointer obtain down time'
replace_upstream_method "$mesm" \
    $'    iget-wide v2, p0, Landroid/view/MotionEvent;->eventTime:J\n\n    iput-wide v2, v0, Landroid/view/MotionEvent;->eventTime:J\n' \
    $'    iget-wide v2, p0, Landroid/view/MotionEvent;->eventTime:J\n\n    iput-wide v2, v0, Landroid/view/MotionEvent;->eventTime:J\n\n    iget-wide v2, p0, Landroid/view/MotionEvent;->eclipseDownTime:J\n\n    iput-wide v2, v0, Landroid/view/MotionEvent;->eclipseDownTime:J\n' \
    'copying obtain down time'
replace_upstream_method "$mesm" \
    $'.method public final getDownTime()J\n    .registers 3\n\n    invoke-virtual {p0}, Landroid/view/MotionEvent;->getEventTime()J\n\n    move-result-wide v0\n\n    return-wide v0\n.end method\n' \
    $'.method public final getDownTime()J\n    .registers 3\n\n    iget-wide v0, p0, Landroid/view/MotionEvent;->eclipseDownTime:J\n\n    return-wide v0\n.end method\n' \
    'getDownTime'
replace_upstream_method "$mesm" \
    $'.method public final findPointerIndex(I)I\n    .registers 3\n\n    const/4 v0, 0x0\n\n    return v0\n.end method\n' \
    $'.method public final findPointerIndex(I)I\n    .registers 6\n\n    iget-object v0, p0, Landroid/view/MotionEvent;->ids:[I\n\n    array-length v1, v0\n\n    const/4 v2, 0x0\n\n    :eclipse_next_pointer\n    if-ge v2, v1, :eclipse_pointer_missing\n\n    aget v3, v0, v2\n\n    if-ne v3, p1, :eclipse_other_pointer\n\n    return v2\n\n    :eclipse_other_pointer\n    add-int/lit8 v2, v2, 0x1\n\n    goto :eclipse_next_pointer\n\n    :eclipse_pointer_missing\n    const/4 v2, -0x1\n\n    return v2\n.end method\n' \
    'findPointerIndex'
for axis in X:0 Y:1; do
    name="${axis%%:*}"
    offset="${axis##*:}"
    replace_upstream_method "$mesm" \
        ".method public final get$name(I)F"$'\n    .registers 4\n\n    iget-object v0, p0, Landroid/view/MotionEvent;->coords:[F\n\n    mul-int/lit8 v1, p1, 0x4\n\n'"    add-int/lit8 v1, v1, 0x$offset"$'\n\n    aget v0, v0, v1\n\n    return v0\n.end method\n' \
        ".method public final get$name(I)F"$'\n    .registers 4\n\n    invoke-direct {p0, p1}, Landroid/view/MotionEvent;->eclipseRequirePointerIndex(I)V\n\n    iget-object v0, p0, Landroid/view/MotionEvent;->coords:[F\n\n    mul-int/lit8 v1, p1, 0x4\n\n'"    add-int/lit8 v1, v1, 0x$offset"$'\n\n    aget v0, v0, v1\n\n    return v0\n.end method\n' \
        "get$name(int)"
done
cat >> "$mesm" <<'ECLIPSE_MOTION_EVENT_POINTER_INDEX'

.method private eclipseRequirePointerIndex(I)V
    .registers 4

    if-ltz p1, :eclipse_pointer_index_out_of_range

    iget-object v0, p0, Landroid/view/MotionEvent;->ids:[I

    array-length v0, v0

    if-ge p1, v0, :eclipse_pointer_index_out_of_range

    return-void

    :eclipse_pointer_index_out_of_range
    new-instance v0, Ljava/lang/IllegalArgumentException;

    const-string v1, "pointerIndex out of range"

    invoke-direct {v0, v1}, Ljava/lang/IllegalArgumentException;-><init>(Ljava/lang/String;)V

    throw v0
.end method
ECLIPSE_MOTION_EVENT_POINTER_INDEX
event_time_writes="$(grep -cE '^    iput-wide [pv][0-9]+, [pv][0-9]+, Landroid/view/MotionEvent;->eventTime:J$' "$mesm")" || true
down_time_writes="$(grep -cE '^    iput-wide [pv][0-9]+, [pv][0-9]+, Landroid/view/MotionEvent;->eclipseDownTime:J$' "$mesm")" || true
[ "$event_time_writes/$down_time_writes" = "4/4" ] \
    || fail "MotionEvent.smali writes eventTime $event_time_writes times and eclipseDownTime $down_time_writes times (expected 4 each) — installed MotionEvent drifted; update patch-framework.sh"

fsm="$work/smali/android/app/Fragment.smali"
[ -f "$fsm" ] || fail "Fragment.smali not found after baksmali"
n="$(grep -cF '.method public onCreate(Landroid/os/Bundle;)V' "$fsm")" || true
[ "$n" = "1" ] || fail "Fragment.smali onCreate anchor not unique (found $n, expected 1) — installed Fragment drifted; update patch-framework.sh"
! grep -qF 'onActivityCreated(Landroid/os/Bundle;)V' "$fsm" || fail "Fragment.smali already declares onActivityCreated — installed Fragment drifted; update patch-framework.sh"

perl -0pi -e 's{(\.method public onCreate\(Landroid/os/Bundle;\)V.*?\.end method\n)}{$1.method public onActivityCreated(Landroid/os/Bundle;)V\n    .registers 2\n\n    return-void\n.end method\n}s' "$fsm"
grep -qF 'onActivityCreated(Landroid/os/Bundle;)V' "$fsm" || fail "Fragment.smali onActivityCreated insert failed (drift?)"

asm="$work/smali/android/app/Activity.smali"
[ -f "$asm" ] || fail "Activity.smali not found after baksmali"

ANCHOR_PC=$'.method protected onPostCreate(Landroid/os/Bundle;)V\n    .registers 4\n\n    const-string v0, "Activity"\n\n    const-string v1, "- onPostCreate - yay!"\n\n    invoke-static {v0, v1}, Landroid/util/Slog;->i(Ljava/lang/String;Ljava/lang/String;)I\n\n    return-void\n.end method'
n="$(grep -cF '.method protected onPostCreate(Landroid/os/Bundle;)V' "$asm")" || true
[ "$n" = "1" ] || fail "Activity.smali onPostCreate anchor not unique (found $n, expected 1) — installed Activity drifted; update patch-framework.sh"
ANCHOR_PC="$ANCHOR_PC" perl -0777 -ne 'exit((index($_, $ENV{ANCHOR_PC}) >= 0) ? 0 : 1)' "$asm" || fail "Activity.smali onPostCreate body changed from the expected no-op — installed Activity drifted; update patch-framework.sh"
! grep -qF 'onActivityCreated(Landroid/os/Bundle;)V' "$asm" || fail "Activity.smali already dispatches onActivityCreated — installed Activity drifted; update patch-framework.sh"

perl -0pi -e 's{\.method protected onPostCreate\(Landroid/os/Bundle;\)V\n    \.registers 4\n\n    const-string v0, "Activity"\n\n    const-string v1, "- onPostCreate - yay!"\n\n    invoke-static \{v0, v1\}, Landroid/util/Slog;->i\(Ljava/lang/String;Ljava/lang/String;\)I\n\n    return-void\n\.end method}{.method protected onPostCreate(Landroid/os/Bundle;)V\n    .registers 4\n\n    const-string v0, "Activity"\n\n    const-string v1, "- onPostCreate - yay!"\n\n    invoke-static \{v0, v1\}, Landroid/util/Slog;->i(Ljava/lang/String;Ljava/lang/String;)I\n\n    iget-object v0, p0, Landroid/app/Activity;->fragments:Ljava/util/List;\n\n    invoke-interface \{v0\}, Ljava/util/List;->iterator()Ljava/util/Iterator;\n\n    move-result-object v1\n\n    :goto_pc\n    invoke-interface \{v1\}, Ljava/util/Iterator;->hasNext()Z\n\n    move-result v0\n\n    if-eqz v0, :cond_pc\n\n    invoke-interface \{v1\}, Ljava/util/Iterator;->next()Ljava/lang/Object;\n\n    move-result-object v0\n\n    check-cast v0, Landroid/app/Fragment;\n\n    invoke-virtual \{v0, p1\}, Landroid/app/Fragment;->onActivityCreated(Landroid/os/Bundle;)V\n\n    goto :goto_pc\n\n    :cond_pc\n    return-void\n.end method}s' "$asm"
grep -qF 'invoke-virtual {v0, p1}, Landroid/app/Fragment;->onActivityCreated(Landroid/os/Bundle;)V' "$asm" || fail "Activity.smali onPostCreate dispatch insert failed (drift?)"

prd_sm="$here/smali/android/app/PermissionResultDelivery.smali"
[ -f "$prd_sm" ] || fail "PermissionResultDelivery.smali missing at $prd_sm"
n="$(grep -cF '.method public requestPermissions([Ljava/lang/String;I)V' "$asm")" || true
[ "$n" = "1" ] || fail "Activity.smali requestPermissions anchor not unique (found $n, expected 1) — installed Activity drifted; update patch-framework.sh"
! grep -qF 'onRequestPermissionsResult(I[Ljava/lang/String;[I)V' "$asm" || fail "Activity.smali already declares onRequestPermissionsResult — installed Activity drifted; update patch-framework.sh"
perl -0pi -e '
    s{\.method public requestPermissions\(\[Ljava/lang/String;I\)V\n.*?\.end method\n}{
        index($&, q{const-string v2, "): not handled"}) >= 0
            or die "requestPermissions body changed";
        ".method public requestPermissions([Ljava/lang/String;I)V\n    .registers 3\n\n    invoke-static {p0, p1, p2}, Landroid/app/PermissionResultDelivery;->post(Landroid/app/Activity;[Ljava/lang/String;I)V\n\n    return-void\n.end method\n\n.method public onRequestPermissionsResult(I[Ljava/lang/String;[I)V\n    .registers 4\n\n    return-void\n.end method\n"
    }se;
' "$asm" || fail "Activity.requestPermissions no longer only logs 'not handled' — installed Activity drifted; update patch-framework.sh"
grep -qF 'invoke-static {p0, p1, p2}, Landroid/app/PermissionResultDelivery;->post(Landroid/app/Activity;[Ljava/lang/String;I)V' "$asm" || fail "Activity.smali requestPermissions answer insert failed (drift?)"
grep -qF '.method public onRequestPermissionsResult(I[Ljava/lang/String;[I)V' "$asm" || fail "Activity.smali onRequestPermissionsResult insert failed (drift?)"
! grep -qF '): not handled' "$asm" || fail "Activity.requestPermissions still only logs 'not handled' — permission requests would never be answered"

lmsm="$work/smali/android/location/LocationManager.smali"
[ -f "$lmsm" ] || fail "LocationManager.smali not found after baksmali"
n="$(grep -cF '.method public getAllProviders()Ljava/util/List;' "$lmsm")" || true
[ "$n" = "1" ] || fail "LocationManager.smali getAllProviders anchor not unique (found $n, expected 1) — installed LocationManager drifted; update patch-framework.sh"
! grep -qF 'isProviderEnabled(Ljava/lang/String;)Z' "$lmsm" || fail "LocationManager.smali already declares isProviderEnabled — installed framework drifted; re-evaluate this patch"
perl -0pi -e 's{(\.method public getAllProviders\(\)Ljava/util/List;.*?\.end method\n)}{$1.method public isProviderEnabled(Ljava/lang/String;)Z\n    .locals 2\n\n    if-nez p1, :eclipse_location_provider_non_null\n\n    new-instance v0, Ljava/lang/IllegalArgumentException;\n\n    const-string v1, "invalid null provider"\n\n    invoke-direct {v0, v1}, Ljava/lang/IllegalArgumentException;-><init>(Ljava/lang/String;)V\n\n    throw v0\n\n    :eclipse_location_provider_non_null\n    const/4 v0, 0x0\n\n    return v0\n.end method\n}s' "$lmsm"
grep -qF '.method public isProviderEnabled(Ljava/lang/String;)Z' "$lmsm" || fail "LocationManager.smali isProviderEnabled insert failed (drift?)"
grep -qF 'Ljava/lang/IllegalArgumentException;-><init>(Ljava/lang/String;)V' "$lmsm" || fail "LocationManager.smali null-provider contract insert failed"
grep -qF ':eclipse_location_provider_non_null' "$lmsm" || fail "LocationManager.smali disabled-provider return path insert failed"

vibsm="$work/smali/android/os/Vibrator.smali"
[ -f "$vibsm" ] || fail "Vibrator.smali not found after baksmali"
n="$(grep -cF '.method public vibrate(J)V' "$vibsm")" || true
[ "$n" = "1" ] || fail "Vibrator.smali vibrate(J)V anchor not unique (found $n, expected 1) — installed Vibrator drifted; update patch-framework.sh"
n="$(grep -cF '.method public cancel()V' "$vibsm")" || true
[ "$n" = "1" ] || fail "Vibrator.smali upstream cancel count = $n (expected 1) — installed Vibrator drifted; update patch-framework.sh"

envsm="$work/smali/android/os/Environment.smali"
[ -f "$envsm" ] || fail "Environment.smali not found after baksmali"
! grep -qF 'native_get_pictures_dir' "$envsm" || fail "Environment.smali already declares native_get_pictures_dir — installed Environment drifted; update patch-framework.sh"
UPSTREAM_ENVIRONMENT_NATIVE=$'.method private static native native_get_app_data_dir()Ljava/lang/String;\n.end method\n'
ENVIRONMENT_NATIVES=$'.method private static native native_get_app_data_dir()Ljava/lang/String;\n.end method\n\n.method private static native native_get_pictures_dir()Ljava/lang/String;\n.end method\n'
replace_upstream_method "$envsm" "$UPSTREAM_ENVIRONMENT_NATIVE" "$ENVIRONMENT_NATIVES" 'native_get_app_data_dir declaration'
UPSTREAM_PUBLIC_DIRECTORY=$'.method public static getExternalStoragePublicDirectory(Ljava/lang/String;)Ljava/io/File;\n    .registers 3\n\n    invoke-static {}, Landroid/os/Environment;->throwIfUserRequired()V\n\n    sget-object v0, Landroid/os/Environment;->sCurrentUser:Landroid/os/Environment$UserEnvironment;\n\n    invoke-virtual {v0, p0}, Landroid/os/Environment$UserEnvironment;->buildExternalStoragePublicDirs(Ljava/lang/String;)[Ljava/io/File;\n\n    move-result-object v0\n\n    const/4 v1, 0x0\n\n    aget-object v0, v0, v1\n\n    return-object v0\n.end method\n'
ENVIRONMENT_PUBLIC_DIRECTORY=$'.method public static getExternalStoragePublicDirectory(Ljava/lang/String;)Ljava/io/File;\n    .registers 3\n\n    sget-object v0, Landroid/os/Environment;->DIRECTORY_PICTURES:Ljava/lang/String;\n\n    invoke-virtual {v0, p0}, Ljava/lang/String;->equals(Ljava/lang/Object;)Z\n\n    move-result v0\n\n    if-eqz v0, :eclipse_not_pictures\n\n    new-instance v0, Ljava/io/File;\n\n    invoke-static {}, Landroid/os/Environment;->native_get_pictures_dir()Ljava/lang/String;\n\n    move-result-object v1\n\n    invoke-direct {v0, v1}, Ljava/io/File;-><init>(Ljava/lang/String;)V\n\n    return-object v0\n\n    :eclipse_not_pictures\n    invoke-static {}, Landroid/os/Environment;->throwIfUserRequired()V\n\n    sget-object v0, Landroid/os/Environment;->sCurrentUser:Landroid/os/Environment$UserEnvironment;\n\n    invoke-virtual {v0, p0}, Landroid/os/Environment$UserEnvironment;->buildExternalStoragePublicDirs(Ljava/lang/String;)[Ljava/io/File;\n\n    move-result-object v0\n\n    const/4 v1, 0x0\n\n    aget-object v0, v0, v1\n\n    return-object v0\n.end method\n'
replace_upstream_method "$envsm" "$UPSTREAM_PUBLIC_DIRECTORY" "$ENVIRONMENT_PUBLIC_DIRECTORY" 'getExternalStoragePublicDirectory'

winsm="$work/smali/android/view/Window.smali"
[ -f "$winsm" ] || fail "Window.smali not found after baksmali"
! grep -qF 'nativeSetKeepScreenOn' "$winsm" || fail "Window.smali already declares nativeSetKeepScreenOn — installed Window drifted; update patch-framework.sh"
UPSTREAM_WINDOW_NATIVE=$'.method private native set_title(JLjava/lang/String;)V\n.end method\n'
WINDOW_NATIVES=$'.method private native set_title(JLjava/lang/String;)V\n.end method\n\n.method private static native nativeSetKeepScreenOn(Z)V\n.end method\n'
replace_upstream_method "$winsm" "$UPSTREAM_WINDOW_NATIVE" "$WINDOW_NATIVES" 'set_title declaration'
UPSTREAM_WINDOW_ADD_FLAGS=$'.method public addFlags(I)V\n    .registers 2\n\n    return-void\n.end method\n'
WINDOW_ADD_FLAGS=$'.method public addFlags(I)V\n    .registers 3\n\n    and-int/lit16 v0, p1, 0x80\n\n    if-eqz v0, :eclipse_keep_screen_on_unchanged\n\n    const/4 v0, 0x1\n\n    invoke-static {v0}, Landroid/view/Window;->nativeSetKeepScreenOn(Z)V\n\n    :eclipse_keep_screen_on_unchanged\n    return-void\n.end method\n'
replace_upstream_method "$winsm" "$UPSTREAM_WINDOW_ADD_FLAGS" "$WINDOW_ADD_FLAGS" 'addFlags'
UPSTREAM_WINDOW_CLEAR_FLAGS=$'.method public clearFlags(I)V\n    .registers 2\n\n    return-void\n.end method\n'
WINDOW_CLEAR_FLAGS=$'.method public clearFlags(I)V\n    .registers 3\n\n    and-int/lit16 v0, p1, 0x80\n\n    if-eqz v0, :eclipse_keep_screen_on_unchanged\n\n    const/4 v0, 0x0\n\n    invoke-static {v0}, Landroid/view/Window;->nativeSetKeepScreenOn(Z)V\n\n    :eclipse_keep_screen_on_unchanged\n    return-void\n.end method\n'
replace_upstream_method "$winsm" "$UPSTREAM_WINDOW_CLEAR_FLAGS" "$WINDOW_CLEAR_FLAGS" 'clearFlags'
UPSTREAM_WINDOW_SET_FLAGS=$'.method public setFlags(II)V\n    .registers 3\n\n    return-void\n.end method\n'
WINDOW_SET_FLAGS=$'.method public setFlags(II)V\n    .registers 4\n\n    and-int/lit16 v0, p2, 0x80\n\n    if-eqz v0, :eclipse_keep_screen_on_unchanged\n\n    and-int/lit16 v0, p1, 0x80\n\n    if-nez v0, :eclipse_keep_screen_on_set\n\n    const/4 v0, 0x0\n\n    goto :eclipse_keep_screen_on_apply\n\n    :eclipse_keep_screen_on_set\n    const/4 v0, 0x1\n\n    :eclipse_keep_screen_on_apply\n    invoke-static {v0}, Landroid/view/Window;->nativeSetKeepScreenOn(Z)V\n\n    :eclipse_keep_screen_on_unchanged\n    return-void\n.end method\n'
replace_upstream_method "$winsm" "$UPSTREAM_WINDOW_SET_FLAGS" "$WINDOW_SET_FLAGS" 'setFlags'

ctxsm="$work/smali/android/content/Context.smali"
[ -f "$ctxsm" ] || fail "Context.smali not found after baksmali"
UPSTREAM_CACHE_DIR=$'    new-instance v0, Ljava/io/File;\n\n    new-instance v1, Ljava/lang/StringBuilder;\n\n    invoke-direct {v1}, Ljava/lang/StringBuilder;-><init>()V\n\n    const-string v2, "/tmp/atl_cache/"\n\n    invoke-virtual {v1, v2}, Ljava/lang/StringBuilder;->append(Ljava/lang/String;)Ljava/lang/StringBuilder;\n\n    move-result-object v1\n\n    invoke-virtual {p0}, Landroid/content/Context;->getPackageName()Ljava/lang/String;\n\n    move-result-object v2\n\n    invoke-virtual {v1, v2}, Ljava/lang/StringBuilder;->append(Ljava/lang/String;)Ljava/lang/StringBuilder;\n\n    move-result-object v1\n\n    invoke-virtual {v1}, Ljava/lang/StringBuilder;->toString()Ljava/lang/String;\n\n    move-result-object v1\n\n    invoke-direct {v0, v1}, Ljava/io/File;-><init>(Ljava/lang/String;)V\n\n    iput-object v0, p0, Landroid/content/Context;->cache_dir:Ljava/io/File;\n'
CLIENT_CACHE_DIR=$'    const-string v0, "eclipse.client_cache_dir"\n\n    invoke-static {v0}, Ljava/lang/System;->getProperty(Ljava/lang/String;)Ljava/lang/String;\n\n    move-result-object v1\n\n    if-nez v1, :eclipse_client_cache_dir_set\n\n    new-instance v0, Ljava/lang/IllegalStateException;\n\n    const-string v1, "eclipse.client_cache_dir is unset; Eclipse sets it when it boots Roblox"\n\n    invoke-direct {v0, v1}, Ljava/lang/IllegalStateException;-><init>(Ljava/lang/String;)V\n\n    throw v0\n\n    :eclipse_client_cache_dir_set\n    new-instance v0, Ljava/io/File;\n\n    invoke-direct {v0, v1}, Ljava/io/File;-><init>(Ljava/lang/String;)V\n\n    iput-object v0, p0, Landroid/content/Context;->cache_dir:Ljava/io/File;\n'
replace_upstream_method "$ctxsm" "$UPSTREAM_CACHE_DIR" "$CLIENT_CACHE_DIR" 'getCacheDir /tmp/atl_cache location'
! grep -qF '/tmp/atl_cache' "$ctxsm" || fail "Context.smali still names /tmp/atl_cache; Roblox's cache would stay in /tmp"

afm="$work/smali/android/view/autofill/AutofillManager.smali"
[ -f "$afm" ] || fail "AutofillManager.smali not found after baksmali"
n="$(grep -cF ".method public unregisterCallback(Landroid/view/autofill/AutofillManager\$AutofillCallback;)V" "$afm")" || true
[ "$n" = "1" ] || fail "AutofillManager.smali unregisterCallback anchor not unique (found $n, expected 1) — installed AutofillManager drifted; update patch-framework.sh"
! grep -qF '.method public cancel()V' "$afm" || fail "AutofillManager.smali already declares cancel — installed AutofillManager drifted; update patch-framework.sh"
perl -0pi -e 's{(\.method public unregisterCallback\(Landroid/view/autofill/AutofillManager\$AutofillCallback;\)V.*?\.end method\n)}{$1.method public cancel()V\n    .registers 1\n\n    return-void\n.end method\n}s' "$afm"
grep -qF '.method public cancel()V' "$afm" || fail "AutofillManager.smali cancel insert failed (drift?)"

! grep -qF 'requestAutofill(Landroid/view/View;)V' "$afm" || fail "AutofillManager.smali already declares requestAutofill — drifted; update patch-framework.sh"
perl -0pi -e 's{(\.method public cancel\(\)V.*?\.end method\n)}{$1.method public requestAutofill(Landroid/view/View;)V\n    .registers 2\n\n    return-void\n.end method\n}s' "$afm"
grep -qF 'requestAutofill(Landroid/view/View;)V' "$afm" || fail "AutofillManager.smali requestAutofill insert failed (drift?)"

csm="$work/smali/android/webkit/CookieManager.smali"
[ -f "$csm" ] || fail "CookieManager.smali not found after baksmali"
! grep -qF 'native_getCookie' "$csm" || fail "CookieManager.smali already carries native_getCookie — drifted; update patch-framework.sh"

perl -0pi -e 's{\.method public getCookie\(Ljava/lang/String;\)Ljava/lang/String;.*?\.end method\n}{.method public getCookie(Ljava/lang/String;)Ljava/lang/String;\n    .registers 3\n\n    invoke-direct {p0, p1}, Landroid/webkit/CookieManager;->native_getCookie(Ljava/lang/String;)Ljava/lang/String;\n\n    move-result-object v0\n\n    return-object v0\n.end method\n}s' "$csm"
grep -qF -- '->native_getCookie(Ljava/lang/String;)Ljava/lang/String;' "$csm" || fail "CookieManager getCookie native-body insert failed (drift?)"

perl -0pi -e 's{\.method public setCookie\(Ljava/lang/String;Ljava/lang/String;\)V.*?\.end method\n}{.method public setCookie(Ljava/lang/String;Ljava/lang/String;)V\n    .registers 3\n\n    invoke-direct {p0, p1, p2}, Landroid/webkit/CookieManager;->native_setCookie(Ljava/lang/String;Ljava/lang/String;)V\n\n    return-void\n.end method\n\n.method public setCookie(Ljava/lang/String;Ljava/lang/String;Landroid/webkit/ValueCallback;)V\n    .registers 4\n\n    invoke-direct {p0, p1, p2, p3}, Landroid/webkit/CookieManager;->native_setCookie(Ljava/lang/String;Ljava/lang/String;Landroid/webkit/ValueCallback;)V\n\n    return-void\n.end method\n}s' "$csm"
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
! grep -qF 'canGoBack()Z' "$wvsm" || fail "WebView.smali already declares canGoBack — installed framework drifted; update patch-framework.sh"
for wv_member in \
    'getUrl()Ljava/lang/String;' \
    'reload()V' \
    'removeJavascriptInterface(Ljava/lang/String;)V' \
    'internalProgressChanged(' \
    'internalLoadResource(' \
    'internalReceivedError(' \
    'internalShouldOverrideUrlLoading(' \
    'webChromeClient:' \
    '>settings:' \
    'private settings:'
do
    ! grep -qF -- "$wv_member" "$wvsm" || fail "WebView.smali already declares $wv_member — installed WebView drifted; update patch-framework.sh"
done
n="$(grep -cxF '.field private webViewClient:Landroid/webkit/WebViewClient;' "$wvsm")" || true
[ "$n" = "1" ] || fail "WebView.smali webViewClient field anchor not unique (found $n, expected 1) — installed WebView drifted; update patch-framework.sh"
perl -0pi -e 's{(\.field private webViewClient:Landroid/webkit/WebViewClient;\n)}{$1\n.field private webChromeClient:Landroid/webkit/WebChromeClient;\n\n.field private settings:Landroid/webkit/WebSettings;\n}' "$wvsm"
grep -qxF '.field private webChromeClient:Landroid/webkit/WebChromeClient;' "$wvsm" || fail "WebView.smali webChromeClient field insert failed"
grep -qxF '.field private settings:Landroid/webkit/WebSettings;' "$wvsm" || fail "WebView.smali settings field insert failed"
replace_upstream_method "$wvsm" \
    $'.method public setWebChromeClient(Landroid/webkit/WebChromeClient;)V\n    .registers 2\n\n    return-void\n.end method\n' \
    $'.method public setWebChromeClient(Landroid/webkit/WebChromeClient;)V\n    .registers 2\n\n    iput-object p1, p0, Landroid/webkit/WebView;->webChromeClient:Landroid/webkit/WebChromeClient;\n\n    return-void\n.end method\n' \
    'setWebChromeClient'
replace_upstream_method "$wvsm" \
    $'.method public getSettings()Landroid/webkit/WebSettings;\n    .registers 2\n\n    new-instance v0, Landroid/webkit/WebSettings;\n\n    invoke-direct {v0}, Landroid/webkit/WebSettings;-><init>()V\n\n    return-object v0\n.end method\n' \
    $'.method public getSettings()Landroid/webkit/WebSettings;\n    .registers 4\n\n    iget-object v0, p0, Landroid/webkit/WebView;->settings:Landroid/webkit/WebSettings;\n\n    if-nez v0, :cond_eclipse_settings_ready\n\n    new-instance v0, Landroid/webkit/WebSettings;\n\n    invoke-direct {v0}, Landroid/webkit/WebSettings;-><init>()V\n\n    iget-wide v1, p0, Landroid/view/View;->widget:J\n\n    iput-wide v1, v0, Landroid/webkit/WebSettings;->widget:J\n\n    iput-object v0, p0, Landroid/webkit/WebView;->settings:Landroid/webkit/WebSettings;\n\n    :cond_eclipse_settings_ready\n    return-object v0\n.end method\n' \
    'getSettings'
replace_upstream_method "$wvsm" \
    $'.method public stopLoading()V\n    .registers 1\n\n    return-void\n.end method\n' \
    $'.method public stopLoading()V\n    .registers 3\n\n    iget-wide v0, p0, Landroid/view/View;->widget:J\n\n    invoke-direct {p0, v0, v1}, Landroid/webkit/WebView;->native_stopLoading(J)V\n\n    return-void\n.end method\n' \
    'stopLoading'
replace_upstream_method "$wvsm" \
    $'.method public destroy()V\n    .registers 1\n\n    return-void\n.end method\n' \
    $'.method public destroy()V\n    .registers 3\n\n    iget-wide v0, p0, Landroid/view/View;->widget:J\n\n    invoke-direct {p0, v0, v1}, Landroid/webkit/WebView;->native_destroy(J)V\n\n    return-void\n.end method\n' \
    'destroy'

grep -qF 'const-string v2, " - not implemented yet"' "$wvsm" || fail "WebView.smali loadUrl no longer carries the javascript: println (installed shape drifted; update patch-framework.sh)"
perl -0pi -e 's{\.method public loadUrl\(Ljava/lang/String;\)V.*?\.end method\n}{.method public loadUrl(Ljava/lang/String;)V\n    .registers 7\n\n    const-string v0, "javascript:"\n\n    invoke-virtual {p1, v0}, Ljava/lang/String;->startsWith(Ljava/lang/String;)Z\n\n    move-result v0\n\n    iget-wide v1, p0, Landroid/view/View;->widget:J\n\n    if-eqz v0, :cond_eclipse_loadurl_normal\n\n    const/16 v3, 0xb\n\n    invoke-virtual {p1, v3}, Ljava/lang/String;->substring(I)Ljava/lang/String;\n\n    move-result-object v3\n\n    const/4 v4, 0x0\n\n    invoke-direct {p0, v1, v2, v3, v4}, Landroid/webkit/WebView;->native_evaluateJavascript(JLjava/lang/String;Landroid/webkit/ValueCallback;)V\n\n    return-void\n\n    :cond_eclipse_loadurl_normal\n    invoke-direct {p0, v1, v2, p1}, Landroid/webkit/WebView;->native_loadUrl(JLjava/lang/String;)V\n\n    return-void\n.end method\n}s' "$wvsm"
grep -qF -- '->native_evaluateJavascript(JLjava/lang/String;Landroid/webkit/ValueCallback;)V' "$wvsm" || fail "WebView loadUrl javascript:-route insert failed (drift?)"
! grep -qF 'const-string v2, " - not implemented yet"' "$wvsm" || fail "WebView loadUrl still carries the full-URL println (leak-fix regressed)"

perl -0pi -e 's{\.method public evaluateJavascript\(Ljava/lang/String;Landroid/webkit/ValueCallback;\)V.*?\.end method\n}{.method public evaluateJavascript(Ljava/lang/String;Landroid/webkit/ValueCallback;)V\n    .registers 5\n\n    iget-wide v0, p0, Landroid/view/View;->widget:J\n\n    invoke-direct {p0, v0, v1, p1, p2}, Landroid/webkit/WebView;->native_evaluateJavascript(JLjava/lang/String;Landroid/webkit/ValueCallback;)V\n\n    return-void\n.end method\n}s' "$wvsm"

perl -0pi -e 's{\.method public addJavascriptInterface\(Ljava/lang/Object;Ljava/lang/String;\)V.*?\.end method\n}{.method public addJavascriptInterface(Ljava/lang/Object;Ljava/lang/String;)V\n    .registers 5\n\n    iget-wide v0, p0, Landroid/view/View;->widget:J\n\n    invoke-direct {p0, v0, v1, p1, p2}, Landroid/webkit/WebView;->native_addJavascriptInterface(JLjava/lang/Object;Ljava/lang/String;)V\n\n    return-void\n.end method\n}s' "$wvsm"
grep -qF -- '->native_addJavascriptInterface(JLjava/lang/Object;Ljava/lang/String;)V' "$wvsm" || fail "WebView addJavascriptInterface native-body insert failed (drift?)"

cat >> "$wvsm" <<'ECLIPSE_WV_HISTORY'


.method public canGoBack()Z
    .registers 3

    iget-wide v0, p0, Landroid/view/View;->widget:J

    invoke-direct {p0, v0, v1}, Landroid/webkit/WebView;->native_canGoBack(J)Z

    move-result v0

    return v0
.end method

.method public goBack()V
    .registers 3

    iget-wide v0, p0, Landroid/view/View;->widget:J

    invoke-direct {p0, v0, v1}, Landroid/webkit/WebView;->native_goBack(J)V

    return-void
.end method

.method public getUrl()Ljava/lang/String;
    .registers 3

    iget-wide v0, p0, Landroid/view/View;->widget:J

    invoke-direct {p0, v0, v1}, Landroid/webkit/WebView;->native_getUrl(J)Ljava/lang/String;

    move-result-object v0

    return-object v0
.end method

.method public reload()V
    .registers 3

    iget-wide v0, p0, Landroid/view/View;->widget:J

    invoke-direct {p0, v0, v1}, Landroid/webkit/WebView;->native_reload(J)V

    return-void
.end method

.method public removeJavascriptInterface(Ljava/lang/String;)V
    .registers 4

    iget-wide v0, p0, Landroid/view/View;->widget:J

    invoke-direct {p0, v0, v1, p1}, Landroid/webkit/WebView;->native_removeJavascriptInterface(JLjava/lang/String;)V

    return-void
.end method

.method internalProgressChanged(I)V
    .registers 3

    iget-object v0, p0, Landroid/webkit/WebView;->webChromeClient:Landroid/webkit/WebChromeClient;

    if-eqz v0, :cond_eclipse_progress_done

    invoke-virtual {v0, p0, p1}, Landroid/webkit/WebChromeClient;->onProgressChanged(Landroid/webkit/WebView;I)V

    :cond_eclipse_progress_done
    return-void
.end method

.method internalLoadResource(Ljava/lang/String;)V
    .registers 3

    iget-object v0, p0, Landroid/webkit/WebView;->webViewClient:Landroid/webkit/WebViewClient;

    if-eqz v0, :cond_eclipse_resource_done

    invoke-virtual {v0, p0, p1}, Landroid/webkit/WebViewClient;->onLoadResource(Landroid/webkit/WebView;Ljava/lang/String;)V

    :cond_eclipse_resource_done
    return-void
.end method

.method internalReceivedError(Landroid/webkit/WebResourceRequest;Landroid/webkit/WebResourceError;)V
    .registers 4

    iget-object v0, p0, Landroid/webkit/WebView;->webViewClient:Landroid/webkit/WebViewClient;

    if-eqz v0, :cond_eclipse_error_done

    invoke-virtual {v0, p0, p1, p2}, Landroid/webkit/WebViewClient;->onReceivedError(Landroid/webkit/WebView;Landroid/webkit/WebResourceRequest;Landroid/webkit/WebResourceError;)V

    :cond_eclipse_error_done
    return-void
.end method

.method internalShouldOverrideUrlLoading(Landroid/webkit/WebResourceRequest;)Z
    .registers 3

    iget-object v0, p0, Landroid/webkit/WebView;->webViewClient:Landroid/webkit/WebViewClient;

    if-eqz v0, :cond_eclipse_policy_engine

    invoke-virtual {v0, p0, p1}, Landroid/webkit/WebViewClient;->shouldOverrideUrlLoading(Landroid/webkit/WebView;Landroid/webkit/WebResourceRequest;)Z

    move-result v0

    return v0

    :cond_eclipse_policy_engine
    const/4 v0, 0x0

    return v0
.end method
ECLIPSE_WV_HISTORY

grep -qF -- '->native_canGoBack(J)Z' "$wvsm" || fail "WebView canGoBack native route insert failed"
grep -qF -- '->native_goBack(J)V' "$wvsm" || fail "WebView goBack native route insert failed"
grep -qF -- '->native_getUrl(J)Ljava/lang/String;' "$wvsm" || fail "WebView getUrl native route insert failed"
grep -qF -- '->onProgressChanged(Landroid/webkit/WebView;I)V' "$wvsm" || fail "WebView progress dispatch insert failed"

cat >> "$wvsm" <<'ECLIPSE_WV_NATIVES'


.method private native native_evaluateJavascript(JLjava/lang/String;Landroid/webkit/ValueCallback;)V
.end method

.method private native native_addJavascriptInterface(JLjava/lang/Object;Ljava/lang/String;)V
.end method

.method private native native_canGoBack(J)Z
.end method

.method private native native_goBack(J)V
.end method

.method private native native_reload(J)V
.end method

.method private native native_stopLoading(J)V
.end method

.method private native native_getUrl(J)Ljava/lang/String;
.end method

.method private native native_removeJavascriptInterface(JLjava/lang/String;)V
.end method

.method private native native_destroy(J)V
.end method
ECLIPSE_WV_NATIVES

grep -qF 'const-string v0, "GDPR VIOLATION"' "$wssm" || fail "WebSettings.smali no longer returns \"GDPR VIOLATION\" (installed shape drifted; update patch-framework.sh)"
! grep -qF 'userAgent:' "$wssm" || fail "WebSettings.smali already declares a userAgent field — drifted; update patch-framework.sh"
ECLIPSE_UA='Mozilla/5.0 (X11; Linux x86_64) AppleWebKit/537.36 (KHTML, like Gecko) Chrome/152.0.0.0 Safari/537.36 Eclipse-WebView/152.0.6'

ECLIPSE_UA="$ECLIPSE_UA" perl -0pi -e 'my $ua=$ENV{ECLIPSE_UA}; s{\.method public getUserAgentString\(\)Ljava/lang/String;.*?\.end method\n}{".method public getUserAgentString()Ljava/lang/String;\n    .registers 3\n\n    iget-object v0, p0, Landroid/webkit/WebSettings;->userAgent:Ljava/lang/String;\n\n    if-eqz v0, :cond_eclipse_ua_default\n\n    invoke-virtual {v0}, Ljava/lang/String;->isEmpty()Z\n\n    move-result v1\n\n    if-eqz v1, :cond_eclipse_ua_app\n\n    :cond_eclipse_ua_default\n    const-string v0, \"$ua\"\n\n    :cond_eclipse_ua_app\n    return-object v0\n.end method\n"}se' "$wssm"
grep -qF -- '->userAgent:Ljava/lang/String;' "$wssm" || fail "WebSettings getUserAgentString per-WebView insert failed (drift?)"
ECLIPSE_UA="$ECLIPSE_UA" perl -0pi -e 'my $ua=$ENV{ECLIPSE_UA}; s{\.method public static getDefaultUserAgent\(Landroid/content/Context;\)Ljava/lang/String;.*?\.end method\n}{".method public static getDefaultUserAgent(Landroid/content/Context;)Ljava/lang/String;\n    .registers 2\n\n    const-string v0, \"$ua\"\n\n    return-object v0\n.end method\n"}se' "$wssm"
grep -qF 'Eclipse-WebView/152.0.6' "$wssm" || fail "WebSettings honest-UA insert failed (drift?)"
! grep -qF 'GDPR VIOLATION' "$wssm" || fail "WebSettings still returns \"GDPR VIOLATION\" (honest-UA fix incomplete)"

! grep -qF 'native_setUserAgentString' "$wssm" || fail "WebSettings.smali already carries native_setUserAgentString — drifted; update patch-framework.sh"
! grep -qF 'widget:J' "$wssm" || fail "WebSettings.smali already declares a widget field — drifted; update patch-framework.sh"
n="$(grep -cF '.method public setUserAgentString(Ljava/lang/String;)V' "$wssm")" || true
[ "$n" = "1" ] || fail "WebSettings.smali setUserAgentString anchor not unique (found $n, expected 1) — installed WebSettings drifted; update patch-framework.sh"

ANCHOR_UAS=$'.method public setUserAgentString(Ljava/lang/String;)V\n    .registers 2\n\n    return-void\n.end method'
ANCHOR_UAS="$ANCHOR_UAS" perl -0777 -ne 'exit((index($_, $ENV{ANCHOR_UAS}) >= 0) ? 0 : 1)' "$wssm" || fail "WebSettings.smali setUserAgentString body changed from the expected empty no-op — installed WebSettings drifted; update patch-framework.sh"

perl -0pi -e 's{\.method public setUserAgentString\(Ljava/lang/String;\)V.*?\.end method\n}{.method public setUserAgentString(Ljava/lang/String;)V\n    .registers 4\n\n    iput-object p1, p0, Landroid/webkit/WebSettings;->userAgent:Ljava/lang/String;\n\n    iget-wide v0, p0, Landroid/webkit/WebSettings;->widget:J\n\n    invoke-direct {p0, v0, v1, p1}, Landroid/webkit/WebSettings;->native_setUserAgentString(JLjava/lang/String;)V\n\n    return-void\n.end method\n}s' "$wssm"
grep -qF -- '->native_setUserAgentString(JLjava/lang/String;)V' "$wssm" || fail "WebSettings setUserAgentString native-body insert failed (drift?)"

ANCHOR_UAS="$ANCHOR_UAS" perl -0777 -ne 'exit((index($_, $ENV{ANCHOR_UAS}) >= 0) ? 1 : 0)' "$wssm" || fail "WebSettings.setUserAgentString is still the empty no-op — the app's UA would be silently discarded again (§6 2026-07-16 💥)"

cat >> "$wssm" <<'ECLIPSE_WS_NATIVES'


.field widget:J

.field private userAgent:Ljava/lang/String;

.method private native native_setUserAgentString(JLjava/lang/String;)V
.end method
ECLIPSE_WS_NATIVES

n="$(grep -cF '.method internalLoadChanged(ILjava/lang/String;)V' "$wvsm")" || true
[ "$n" = "1" ] || fail "WebView.smali internalLoadChanged anchor not unique (found $n, expected 1) — installed WebView drifted; update patch-framework.sh"
ANCHOR_ILC=$'.method internalLoadChanged(ILjava/lang/String;)V\n    .registers 4\n\n    if-nez p1, :cond_c\n\n    iget-object v0, p0, Landroid/webkit/WebView;->webViewClient:Landroid/webkit/WebViewClient;\n\n    if-eqz v0, :cond_c\n\n    iget-object v0, p0, Landroid/webkit/WebView;->webViewClient:Landroid/webkit/WebViewClient;\n\n    invoke-virtual {v0, p0, p2}, Landroid/webkit/WebViewClient;->onPageStarted(Landroid/webkit/WebView;Ljava/lang/String;)V\n\n    :cond_b\n    :goto_b\n    return-void\n\n    :cond_c\n    const/4 v0, 0x3\n\n    if-ne p1, v0, :cond_b\n\n    iget-object v0, p0, Landroid/webkit/WebView;->webViewClient:Landroid/webkit/WebViewClient;\n\n    if-eqz v0, :cond_b\n\n    iget-object v0, p0, Landroid/webkit/WebView;->webViewClient:Landroid/webkit/WebViewClient;\n\n    invoke-virtual {v0, p0, p2}, Landroid/webkit/WebViewClient;->onPageFinished(Landroid/webkit/WebView;Ljava/lang/String;)V\n\n    goto :goto_b\n.end method'
ANCHOR_ILC="$ANCHOR_ILC" perl -0777 -ne 'exit((index($_, $ENV{ANCHOR_ILC}) >= 0) ? 0 : 1)' "$wvsm" || fail "WebView.smali internalLoadChanged body changed from the expected 2-arg shape — installed WebView drifted; update patch-framework.sh"
perl -0pi -e 's{\.method internalLoadChanged\(ILjava/lang/String;\)V.*?\.end method\n}{.method internalLoadChanged(ILjava/lang/String;)V\n    .registers 5\n\n    iget-object v0, p0, Landroid/webkit/WebView;->webViewClient:Landroid/webkit/WebViewClient;\n\n    if-eqz v0, :cond_eclipse_ilc_done\n\n    if-nez p1, :cond_eclipse_ilc_committed\n\n    const/4 v1, 0x0\n\n    invoke-virtual {v0, p0, p2, v1}, Landroid/webkit/WebViewClient;->onPageStarted(Landroid/webkit/WebView;Ljava/lang/String;Landroid/graphics/Bitmap;)V\n\n    return-void\n\n    :cond_eclipse_ilc_committed\n    const/4 v1, 0x2\n\n    if-ne p1, v1, :cond_eclipse_ilc_finished\n\n    invoke-virtual {v0, p0, p2}, Landroid/webkit/WebViewClient;->onPageCommitVisible(Landroid/webkit/WebView;Ljava/lang/String;)V\n\n    return-void\n\n    :cond_eclipse_ilc_finished\n    const/4 v1, 0x3\n\n    if-ne p1, v1, :cond_eclipse_ilc_done\n\n    invoke-virtual {v0, p0, p2}, Landroid/webkit/WebViewClient;->onPageFinished(Landroid/webkit/WebView;Ljava/lang/String;)V\n\n    :cond_eclipse_ilc_done\n    return-void\n.end method\n}s' "$wvsm"
grep -qF -- '->onPageStarted(Landroid/webkit/WebView;Ljava/lang/String;Landroid/graphics/Bitmap;)V' "$wvsm" || fail "WebView.smali internalLoadChanged 3-arg onPageStarted dispatch insert failed (drift?)"
! grep -qF -- '->onPageStarted(Landroid/webkit/WebView;Ljava/lang/String;)V' "$wvsm" || fail "WebView.smali still dispatches the 2-arg onPageStarted (M6 3-arg dispatch incomplete)"
grep -qF -- '->onPageCommitVisible(Landroid/webkit/WebView;Ljava/lang/String;)V' "$wvsm" || fail "WebView.smali internalLoadChanged onPageCommitVisible dispatch insert failed (drift?)"

wvcsm="$work/smali/android/webkit/WebViewClient.smali"
[ -f "$wvcsm" ] || fail "WebViewClient.smali not found after baksmali"
! grep -qF 'Landroid/graphics/Bitmap;)V' "$wvcsm" || fail "WebViewClient.smali already declares a Bitmap-arg method (3-arg onPageStarted?) — installed WebViewClient drifted; update patch-framework.sh"
! grep -qF 'shouldOverrideUrlLoading' "$wvcsm" || fail "WebViewClient.smali already declares shouldOverrideUrlLoading — installed WebViewClient drifted; update patch-framework.sh"
for wvc_method in onPageCommitVisible onLoadResource onReceivedError; do
    ! grep -qF "$wvc_method" "$wvcsm" || fail "WebViewClient.smali already declares $wvc_method — installed WebViewClient drifted; update patch-framework.sh"
done
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

.method public shouldOverrideUrlLoading(Landroid/webkit/WebView;Landroid/webkit/WebResourceRequest;)Z
    .registers 4

    invoke-interface {p2}, Landroid/webkit/WebResourceRequest;->getUrl()Landroid/net/Uri;

    move-result-object v0

    invoke-virtual {v0}, Landroid/net/Uri;->toString()Ljava/lang/String;

    move-result-object v0

    invoke-virtual {p0, p1, v0}, Landroid/webkit/WebViewClient;->shouldOverrideUrlLoading(Landroid/webkit/WebView;Ljava/lang/String;)Z

    move-result v0

    return v0
.end method

.method public onPageCommitVisible(Landroid/webkit/WebView;Ljava/lang/String;)V
    .registers 3

    return-void
.end method

.method public onLoadResource(Landroid/webkit/WebView;Ljava/lang/String;)V
    .registers 3

    return-void
.end method

.method public onReceivedError(Landroid/webkit/WebView;Landroid/webkit/WebResourceRequest;Landroid/webkit/WebResourceError;)V
    .registers 7

    invoke-interface {p2}, Landroid/webkit/WebResourceRequest;->isForMainFrame()Z

    move-result v0

    if-eqz v0, :cond_eclipse_error_subframe

    invoke-virtual {p3}, Landroid/webkit/WebResourceError;->getErrorCode()I

    move-result v0

    invoke-virtual {p3}, Landroid/webkit/WebResourceError;->getDescription()Ljava/lang/CharSequence;

    move-result-object v1

    invoke-interface {v1}, Ljava/lang/CharSequence;->toString()Ljava/lang/String;

    move-result-object v1

    invoke-interface {p2}, Landroid/webkit/WebResourceRequest;->getUrl()Landroid/net/Uri;

    move-result-object v2

    invoke-virtual {v2}, Landroid/net/Uri;->toString()Ljava/lang/String;

    move-result-object v2

    invoke-virtual {p0, p1, v0, v1, v2}, Landroid/webkit/WebViewClient;->onReceivedError(Landroid/webkit/WebView;ILjava/lang/String;Ljava/lang/String;)V

    :cond_eclipse_error_subframe
    return-void
.end method

.method public onReceivedError(Landroid/webkit/WebView;ILjava/lang/String;Ljava/lang/String;)V
    .registers 5

    return-void
.end method
ECLIPSE_WVC_METHODS
grep -qF -- 'onPageStarted(Landroid/webkit/WebView;Ljava/lang/String;Landroid/graphics/Bitmap;)V' "$wvcsm" || fail "WebViewClient 3-arg onPageStarted insert failed (drift?)"
grep -qF -- 'shouldOverrideUrlLoading(Landroid/webkit/WebView;Ljava/lang/String;)Z' "$wvcsm" || fail "WebViewClient shouldOverrideUrlLoading insert failed (drift?)"
grep -qF -- 'shouldOverrideUrlLoading(Landroid/webkit/WebView;Landroid/webkit/WebResourceRequest;)Z' "$wvcsm" || fail "WebViewClient request shouldOverrideUrlLoading insert failed (drift?)"
grep -qF -- 'onReceivedError(Landroid/webkit/WebView;Landroid/webkit/WebResourceRequest;Landroid/webkit/WebResourceError;)V' "$wvcsm" || fail "WebViewClient onReceivedError insert failed (drift?)"

for overlay_class in WebResourceError EclipseWebResourceRequest; do
    [ ! -e "$work/smali/android/webkit/$overlay_class.smali" ] || fail "the installed framework already declares android.webkit.$overlay_class — the overlay copy would shadow it; update patch-framework.sh"
done
wrr_stock="$work/smali/android/webkit/WebResourceRequest.smali"
[ -f "$wrr_stock" ] || fail "WebResourceRequest.smali not found after baksmali"
wrr_expected=$'.method public abstract getMethod()Ljava/lang/String;\n.method public abstract getRequestHeaders()Ljava/util/Map;\n.method public abstract getUrl()Landroid/net/Uri;\n.method public abstract hasGesture()Z\n.method public abstract isForMainFrame()Z\n.method public abstract isRedirect()Z'
wrr_methods="$(grep '^\.method' "$wrr_stock" | LC_ALL=C sort)"
[ "$wrr_methods" = "$wrr_expected" ] || fail "WebResourceRequest.smali no longer declares exactly the six methods EclipseWebResourceRequest implements — installed WebResourceRequest drifted; update patch-framework.sh"
uri_stock="$work/smali/android/net/Uri.smali"
[ -f "$uri_stock" ] || fail "Uri.smali not found after baksmali"
grep -qxF '.method public static parse(Ljava/lang/String;)Landroid/net/Uri;' "$uri_stock" || fail "Uri.smali lost the static parse(String) EclipseWebResourceRequest calls — installed Uri drifted; update patch-framework.sh"

wccsm="$work/smali/android/webkit/WebChromeClient.smali"
[ -f "$wccsm" ] || fail "WebChromeClient.smali not found after baksmali"
n="$(grep -c '^\.method' "$wccsm")" || true
[ "$n" = "1" ] || fail "WebChromeClient.smali declares $n methods (expected only its constructor) — installed WebChromeClient drifted; update patch-framework.sh"
cat >> "$wccsm" <<'ECLIPSE_WCC_METHODS'

.method public onProgressChanged(Landroid/webkit/WebView;I)V
    .registers 3

    return-void
.end method
ECLIPSE_WCC_METHODS
grep -qF '.method public onProgressChanged(Landroid/webkit/WebView;I)V' "$wccsm" || fail "WebChromeClient onProgressChanged insert failed"

jpm="$work/smali/android/app/job/JobParameters.smali"
[ -f "$jpm" ] || fail "JobParameters.smali not found after baksmali"
n="$(grep -cF '.method public getExtras()Landroid/os/PersistableBundle;' "$jpm")" || true
[ "$n" = "1" ] || fail "JobParameters.smali getExtras anchor not unique (found $n, expected 1) — installed JobParameters drifted; update patch-framework.sh"
if ! grep -qF 'getNetwork()Landroid/net/Network;' "$jpm"; then
    perl -0pi -e 's{(\.method public getExtras\(\)Landroid/os/PersistableBundle;.*?\.end method\n)}{$1.method public getNetwork()Landroid/net/Network;\n    .locals 1\n\n    const/4 v0, 0x0\n\n    return-object v0\n.end method\n}s' "$jpm"
fi
grep -qF 'getNetwork()Landroid/net/Network;' "$jpm" || fail "JobParameters.smali getNetwork insert failed (drift?)"

psm="$work/smali/android/graphics/Paint.smali"
[ -f "$psm" ] || fail "Paint.smali not found after baksmali"
n="$(grep -cF '.method public set(Landroid/graphics/Paint;)V' "$psm")" || true
[ "$n" = "1" ] || fail "Paint.smali set(Paint) anchor not unique (found $n, expected 1) — installed Paint drifted; update patch-framework.sh"
ANCHOR_PSET=$'.method public set(Landroid/graphics/Paint;)V\n    .registers 4\n\n    iget-wide v0, p0, Landroid/graphics/Paint;->paint:J\n\n    invoke-static {v0, v1}, Landroid/graphics/Paint;->native_recycle(J)V\n\n    iget-wide v0, p1, Landroid/graphics/Paint;->paint:J\n\n    invoke-static {v0, v1}, Landroid/graphics/Paint;->native_clone(J)J\n\n    move-result-wide v0\n\n    iput-wide v0, p0, Landroid/graphics/Paint;->paint:J\n\n    return-void\n.end method'
ANCHOR_PSET="$ANCHOR_PSET" perl -0777 -ne 'exit((index($_, $ENV{ANCHOR_PSET}) >= 0) ? 0 : 1)' "$psm" || fail "Paint.smali set(Paint) body changed from the expected recycle-before-clone shape — installed Paint drifted; update patch-framework.sh"
! grep -qF ':eclipse_not_self_set' "$psm" || fail "Paint.smali already carries the self-set guard — installed Paint drifted; update patch-framework.sh"
perl -0pi -e 's{(\.method public set\(Landroid/graphics/Paint;\)V\n    \.registers 4\n)}{$1    if-ne p0, p1, :eclipse_not_self_set\n\n    return-void\n\n    :eclipse_not_self_set\n}s' "$psm"
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
perl -0pi -e 's{(\.method public hasSystemFeature\(Ljava/lang/String;\)Z\n    \.registers 6\n)}{$1    const-string v0, "android.hardware.type.pc"\n\n    invoke-virtual {p1, v0}, Ljava/lang/String;->equals(Ljava/lang/Object;)Z\n\n    move-result v0\n\n    if-eqz v0, :eclipse_not_desktop_pc\n\n    const-string v1, "eclipse.touch_mode"\n\n    const-string v2, "off"\n\n    invoke-static {v1, v2}, Ljava/lang/System;->getProperty(Ljava/lang/String;Ljava/lang/String;)Ljava/lang/String;\n\n    move-result-object v1\n\n    const-string v2, "on"\n\n    invoke-virtual {v1, v2}, Ljava/lang/String;->equals(Ljava/lang/Object;)Z\n\n    move-result v0\n\n    xor-int/lit8 v0, v0, 0x1\n\n    return v0\n\n    :eclipse_not_desktop_pc\n    const-string v0, "android.hardware.touchscreen"\n\n    invoke-virtual {p1, v0}, Ljava/lang/String;->equals(Ljava/lang/Object;)Z\n\n    move-result v0\n\n    if-eqz v0, :eclipse_not_touchscreen\n\n    const-string v1, "eclipse.touch_mode"\n\n    const-string v2, "off"\n\n    invoke-static {v1, v2}, Ljava/lang/System;->getProperty(Ljava/lang/String;Ljava/lang/String;)Ljava/lang/String;\n\n    move-result-object v1\n\n    const-string v2, "on"\n\n    invoke-virtual {v1, v2}, Ljava/lang/String;->equals(Ljava/lang/Object;)Z\n\n    move-result v0\n\n    return v0\n\n    :eclipse_not_touchscreen\n    const-string v0, "android.hardware.audio.low_latency"\n\n    invoke-virtual {p1, v0}, Ljava/lang/String;->equals(Ljava/lang/Object;)Z\n\n    move-result v0\n\n    if-eqz v0, :eclipse_not_low_latency_audio\n\n    const/4 v0, 0x1\n\n    return v0\n\n    :eclipse_not_low_latency_audio\n}s' "$pmsm"
grep -qF ':eclipse_not_desktop_pc' "$pmsm" || fail "PackageManager.smali desktop feature insert failed (drift?)"
grep -qF ':eclipse_not_touchscreen' "$pmsm" || fail "PackageManager.smali touchscreen feature insert failed (drift?)"
grep -qF ':eclipse_not_low_latency_audio' "$pmsm" || fail "PackageManager.smali audio feature insert failed (drift?)"
grep -qF '"android.hardware.type.pc"' "$pmsm" || fail "PackageManager.smali lost the exact desktop-PC feature literal"
grep -qF '"android.hardware.touchscreen"' "$pmsm" || fail "PackageManager.smali lost the exact touchscreen feature literal"
grep -qF '"android.hardware.audio.low_latency"' "$pmsm" || fail "PackageManager.smali lost the exact low-latency feature literal"

! grep -qF ':eclipse_grant_audio_permission' "$pmsm" || fail "PackageManager.smali already carries the Eclipse audio permission grant — installed framework drifted; update patch-framework.sh"
UPSTREAM_CHECK_PERMISSION_HEAD=$'.method public checkPermission(Ljava/lang/String;Ljava/lang/String;)I\n    .registers 7\n\n    const/4 v1, -0x1\n'
AUDIO_PERMISSION_GRANT_HEAD=$'.method public checkPermission(Ljava/lang/String;Ljava/lang/String;)I\n    .registers 7\n\n    const-string v0, "android.permission.RECORD_AUDIO"\n\n    invoke-virtual {v0, p1}, Ljava/lang/String;->equals(Ljava/lang/Object;)Z\n\n    move-result v0\n\n    if-nez v0, :eclipse_grant_audio_permission\n\n    const-string v0, "android.permission.MODIFY_AUDIO_SETTINGS"\n\n    invoke-virtual {v0, p1}, Ljava/lang/String;->equals(Ljava/lang/Object;)Z\n\n    move-result v0\n\n    if-eqz v0, :eclipse_not_audio_permission\n\n    :eclipse_grant_audio_permission\n    const/4 v0, 0x0\n\n    return v0\n\n    :eclipse_not_audio_permission\n    const/4 v1, -0x1\n'
replace_upstream_method "$pmsm" "$UPSTREAM_CHECK_PERMISSION_HEAD" "$AUDIO_PERMISSION_GRANT_HEAD" 'checkPermission audio grant'

audio_manager_sm="$work/smali/android/media/AudioManager.smali"
[ -f "$audio_manager_sm" ] || fail "AudioManager.smali not found after baksmali"
[ ! -e "$work/smali/android/media/AudioFocusRequest.smali" ] || fail "the installed framework already ships AudioFocusRequest — installed framework drifted; drop Eclipse's copy"
for voice_method in 'requestAudioFocus(Landroid/media/AudioFocusRequest;)I' 'abandonAudioFocusRequest(Landroid/media/AudioFocusRequest;)I' 'isBluetoothScoAvailableOffCall()Z' 'isVolumeFixed()Z'; do
    ! grep -qF "$voice_method" "$audio_manager_sm" || fail "AudioManager.smali already declares $voice_method — installed AudioManager drifted; update patch-framework.sh"
done
UPSTREAM_ABANDON_AUDIO_FOCUS=$'.method public abandonAudioFocus(Landroid/media/AudioManager$OnAudioFocusChangeListener;)I\n    .registers 3\n\n    const/4 v0, 0x1\n\n    return v0\n.end method\n'
AUDIO_MANAGER_VOICE_METHODS=$'.method public abandonAudioFocus(Landroid/media/AudioManager$OnAudioFocusChangeListener;)I\n    .registers 3\n\n    const/4 v0, 0x1\n\n    return v0\n.end method\n\n.method public requestAudioFocus(Landroid/media/AudioFocusRequest;)I\n    .registers 3\n\n    const/4 v0, 0x1\n\n    return v0\n.end method\n\n.method public abandonAudioFocusRequest(Landroid/media/AudioFocusRequest;)I\n    .registers 3\n\n    const/4 v0, 0x1\n\n    return v0\n.end method\n\n.method public isBluetoothScoAvailableOffCall()Z\n    .registers 2\n\n    const/4 v0, 0x0\n\n    return v0\n.end method\n\n.method public isVolumeFixed()Z\n    .registers 2\n\n    const/4 v0, 0x0\n\n    return v0\n.end method\n'
replace_upstream_method "$audio_manager_sm" "$UPSTREAM_ABANDON_AUDIO_FOCUS" "$AUDIO_MANAGER_VOICE_METHODS" 'abandonAudioFocus voice-call methods'
UPSTREAM_AUDIO_OUTPUT_PROPERTIES=$'    :pswitch_40\n    const-string v0, "256"\n\n    goto :goto_2b\n\n    :pswitch_43\n    const-string v0, "44100"\n\n    goto :goto_2b\n'
AUDIO_OUTPUT_PROPERTIES=$'    :pswitch_40\n    const-string v0, "eclipse.audio.output_frames_per_buffer"\n\n    invoke-static {v0}, Ljava/lang/System;->getProperty(Ljava/lang/String;)Ljava/lang/String;\n\n    move-result-object v0\n\n    goto :goto_2b\n\n    :pswitch_43\n    const-string v0, "eclipse.audio.output_sample_rate"\n\n    invoke-static {v0}, Ljava/lang/System;->getProperty(Ljava/lang/String;)Ljava/lang/String;\n\n    move-result-object v0\n\n    goto :goto_2b\n'
replace_upstream_method "$audio_manager_sm" "$UPSTREAM_AUDIO_OUTPUT_PROPERTIES" "$AUDIO_OUTPUT_PROPERTIES" 'getProperty output sample rate and frames per buffer'
audio_attributes_builder_sm="$work/smali/android/media/AudioAttributes\$Builder.smali"
[ -f "$audio_attributes_builder_sm" ] || fail "AudioAttributes\$Builder.smali not found after baksmali"
! grep -qF '.method public constructor <init>()V' "$audio_attributes_builder_sm" || fail "AudioAttributes\$Builder.smali already has a public no-argument constructor — installed framework drifted; update patch-framework.sh"
UPSTREAM_AUDIO_ATTRIBUTES_BUILDER_INIT=$'.method public constructor <init>(Landroid/media/AudioAttributes;)V\n    .registers 2\n\n    iput-object p1, p0, Landroid/media/AudioAttributes$Builder;->this$0:Landroid/media/AudioAttributes;\n\n    invoke-direct {p0}, Ljava/lang/Object;-><init>()V\n\n    return-void\n.end method\n'
AUDIO_ATTRIBUTES_BUILDER_INITS=$'.method public constructor <init>()V\n    .registers 2\n\n    new-instance v0, Landroid/media/AudioAttributes;\n\n    invoke-direct {v0}, Landroid/media/AudioAttributes;-><init>()V\n\n    invoke-direct {p0, v0}, Landroid/media/AudioAttributes$Builder;-><init>(Landroid/media/AudioAttributes;)V\n\n    return-void\n.end method\n\n.method public constructor <init>(Landroid/media/AudioAttributes;)V\n    .registers 2\n\n    iput-object p1, p0, Landroid/media/AudioAttributes$Builder;->this$0:Landroid/media/AudioAttributes;\n\n    invoke-direct {p0}, Ljava/lang/Object;-><init>()V\n\n    return-void\n.end method\n'
replace_upstream_method "$audio_attributes_builder_sm" "$UPSTREAM_AUDIO_ATTRIBUTES_BUILDER_INIT" "$AUDIO_ATTRIBUTES_BUILDER_INITS" 'public no-argument constructor'
audio_focus_request_sm="$here/smali/android/media/AudioFocusRequest.smali"
audio_focus_builder_sm="$here/smali/android/media/AudioFocusRequest\$Builder.smali"
[ -f "$audio_focus_request_sm" ] || fail "AudioFocusRequest.smali missing at $audio_focus_request_sm"
[ -f "$audio_focus_builder_sm" ] || fail "AudioFocusRequest\$Builder.smali missing at $audio_focus_builder_sm"

grep -qxF '.field public static final GET_SIGNATURES:I = 0x40' "$pmsm" || fail "PackageManager.smali GET_SIGNATURES is no longer 0x40 — the overlay stub constant would disagree with the framework"
! grep -qF 'GET_SIGNING_CERTIFICATES' "$pmsm" || fail "PackageManager.smali already declares GET_SIGNING_CERTIFICATES — installed framework drifted; update patch-framework.sh"
perl -0pi -e 's{(\.field public static final GET_SIGNATURES:I = 0x40\n)}{$1\n.field public static final GET_SIGNING_CERTIFICATES:I = 0x8000000\n}' "$pmsm"
grep -qxF '.field public static final GET_SIGNING_CERTIFICATES:I = 0x8000000' "$pmsm" || fail "PackageManager.smali GET_SIGNING_CERTIFICATES insert failed (drift?)"

ppkg_sm="$work/smali/android/content/pm/PackageParser\$Package.smali"
[ -f "$ppkg_sm" ] || fail "PackageParser\$Package.smali not found after baksmali"
n="$(grep -cxF '.field public mSignatures:[Landroid/content/pm/Signature;' "$ppkg_sm")" || true
[ "$n" = "1" ] || fail "PackageParser\$Package.smali mSignatures anchor not unique (found $n, expected 1) — installed PackageParser drifted; update patch-framework.sh"
! grep -qF 'mPastSigningCertificates' "$ppkg_sm" || fail "PackageParser\$Package.smali already declares mPastSigningCertificates — installed PackageParser drifted; update patch-framework.sh"
perl -0pi -e 's{(\.field public mSignatures:\[Landroid/content/pm/Signature;\n)}{$1\n.field public mPastSigningCertificates:[Landroid/content/pm/Signature;\n}' "$ppkg_sm"
grep -qxF '.field public mPastSigningCertificates:[Landroid/content/pm/Signature;' "$ppkg_sm" || fail "PackageParser\$Package.smali mPastSigningCertificates insert failed (drift?)"

ppsm="$work/smali/android/content/pm/PackageParser.smali"
[ -f "$ppsm" ] || fail "PackageParser.smali not found after baksmali"
ANCHOR_GPI_SIGNATURES=$'    :cond_319\n    and-int/lit8 v14, p2, 0x40\n\n    if-eqz v14, :cond_b\n\n    move-object/from16 v0, p0\n\n    iget-object v14, v0, Landroid/content/pm/PackageParser$Package;->mSignatures:[Landroid/content/pm/Signature;\n\n    if-eqz v14, :cond_342\n\n    move-object/from16 v0, p0\n\n    iget-object v14, v0, Landroid/content/pm/PackageParser$Package;->mSignatures:[Landroid/content/pm/Signature;\n\n    array-length v4, v14\n\n    :goto_328\n    if-lez v4, :cond_b\n\n    new-array v14, v4, [Landroid/content/pm/Signature;\n\n    iput-object v14, v11, Landroid/content/pm/PackageInfo;->signatures:[Landroid/content/pm/Signature;\n\n    move-object/from16 v0, p0\n\n    iget-object v14, v0, Landroid/content/pm/PackageParser$Package;->mSignatures:[Landroid/content/pm/Signature;\n\n    const/4 v15, 0x0\n\n    iget-object v0, v11, Landroid/content/pm/PackageInfo;->signatures:[Landroid/content/pm/Signature;\n\n    move-object/from16 v16, v0\n\n    const/16 v17, 0x0\n\n    move-object/from16 v0, v16\n\n    move/from16 v1, v17\n\n    invoke-static {v14, v15, v0, v1, v4}, Ljava/lang/System;->arraycopy(Ljava/lang/Object;ILjava/lang/Object;II)V\n\n    goto/16 :goto_b\n\n    :cond_342\n    const/4 v4, 0x0\n\n    goto :goto_328\n'
FILL_GPI_SIGNATURES=$'    :cond_319\n    move-object/from16 v0, p0\n\n    move/from16 v1, p2\n\n    invoke-static {v0, v1, v11}, Landroid/content/pm/SigningCertificates;->fillPackageInfo(Landroid/content/pm/PackageParser$Package;ILandroid/content/pm/PackageInfo;)V\n\n    goto/16 :goto_b\n'
n="$(ANCHOR="$ANCHOR_GPI_SIGNATURES" perl -0777 -ne 'print scalar(() = /\Q$ENV{ANCHOR}\E/g)' "$ppsm")"
[ "$n" = "1" ] || fail "PackageParser.generatePackageInfo GET_SIGNATURES block found $n times (expected 1) — installed PackageParser drifted; update patch-framework.sh"
ANCHOR="$ANCHOR_GPI_SIGNATURES" FILL="$FILL_GPI_SIGNATURES" perl -0777 -pi -e 's{\Q$ENV{ANCHOR}\E}{$ENV{FILL}}' "$ppsm"
grep -qF -- "->fillPackageInfo(Landroid/content/pm/PackageParser\$Package;ILandroid/content/pm/PackageInfo;)V" "$ppsm" || fail "PackageParser.generatePackageInfo signing-certificate insert failed (drift?)"

atlsm="$work/smali/android/atl/ATLLoadedApp.smali"
[ -f "$atlsm" ] || fail "ATLLoadedApp.smali not found after baksmali"
ANCHOR_SYSTEM_CERTIFICATES=$'    move-result-object v3\n\n    invoke-virtual {v2, v3, v8}, Landroid/content/pm/PackageParser;->collectCertificates(Landroid/content/pm/PackageParser$Package;I)Z\n\n    new-instance v5, Landroid/atl/ATLLoadedApp;\n'
HOST_SYSTEM_CERTIFICATES=$'    move-result-object v3\n\n    invoke-static {v3}, Landroid/content/pm/SigningCertificates;->collectHostVerified(Landroid/content/pm/PackageParser$Package;)V\n\n    new-instance v5, Landroid/atl/ATLLoadedApp;\n'
n="$(ANCHOR="$ANCHOR_SYSTEM_CERTIFICATES" perl -0777 -ne 'print scalar(() = /\Q$ENV{ANCHOR}\E/g)' "$atlsm")"
[ "$n" = "1" ] || fail "ATLLoadedApp.getSystemApplication collectCertificates call found $n times (expected 1) — installed ATLLoadedApp drifted; update patch-framework.sh"
ANCHOR="$ANCHOR_SYSTEM_CERTIFICATES" HOST="$HOST_SYSTEM_CERTIFICATES" perl -0777 -pi -e 's{\Q$ENV{ANCHOR}\E}{$ENV{HOST}}' "$atlsm"
perl -0777 -ne 'exit(/\.method public static getSystemApplication\(\)(?:(?!\.end method).)*SigningCertificates;->collectHostVerified\(Landroid\/content\/pm\/PackageParser\$Package;\)V/s ? 0 : 1)' "$atlsm" || fail "ATLLoadedApp.getSystemApplication host-verified certificate insert failed (drift?)"

amsm="$work/smali/android/content/res/AssetManager.smali"
[ -f "$amsm" ] || fail "AssetManager.smali not found after baksmali"
n="$(grep -cxF '.method private final native deleteTheme(J)V' "$amsm")" || true
[ "$n" = "1" ] || fail "AssetManager.smali deleteTheme native declaration found $n times (expected 1) — installed AssetManager drifted; update patch-framework.sh"
ANCHOR_RELEASE_THEME=$'.method final releaseTheme(J)V\n    .registers 4\n\n    monitor-enter p0\n\n    :try_start_1\n    new-instance v0, Ljava/lang/Long;\n'
RELEASE_THEME=$'.method final releaseTheme(J)V\n    .registers 4\n\n    monitor-enter p0\n\n    :try_start_1\n    invoke-direct {p0, p1, p2}, Landroid/content/res/AssetManager;->deleteTheme(J)V\n\n    new-instance v0, Ljava/lang/Long;\n'
n="$(ANCHOR="$ANCHOR_RELEASE_THEME" perl -0777 -ne 'print scalar(() = /\Q$ENV{ANCHOR}\E/g)' "$amsm")"
[ "$n" = "1" ] || fail "AssetManager.releaseTheme is not the upstream body (found $n, expected 1) — installed AssetManager drifted; update patch-framework.sh"
ANCHOR="$ANCHOR_RELEASE_THEME" PATCHED="$RELEASE_THEME" perl -0777 -pi -e 's{\Q$ENV{ANCHOR}\E}{$ENV{PATCHED}}' "$amsm"
perl -0777 -ne 'exit(/\.method final releaseTheme\(J\)V(?:(?!\.end method).)*->deleteTheme\(J\)V(?:(?!\.end method).)*->decRefsLocked\(I\)V/s ? 0 : 1)' "$amsm" || fail "AssetManager.releaseTheme no longer frees the native theme before dropping its reference"
mkdir -p "$work/smali-view/android/content/res"
cp "$amsm" "$work/smali-view/android/content/res/AssetManager.smali"

mkdir -p "$work/smali-view/android/atl" "$work/smali-view/android/view" "$work/smali-view/android/app" "$work/smali-view/android/location" "$work/smali-view/android/os" "$work/smali-view/android/content" "$work/smali-view/android/content/pm" "$work/smali-view/android/net" "$work/smali-view/android/view/autofill" "$work/smali-view/android/webkit" "$work/smali-view/android/app/job" "$work/smali-view/android/graphics" "$work/smali-view/android/media"
cp "$crsm" "$work/smali-view/android/content/ContentResolver.smali"
cp "$connectivity_sm" "$work/smali-view/android/net/ConnectivityManager.smali"
cp "$here/smali/android/net/LinkProperties.smali" "$work/smali-view/android/net/"
cp "$here/smali/android/net/LinkAddress.smali" "$work/smali-view/android/net/"
cp "$vsm" "$work/smali-view/android/view/View.smali"
cp "$dsm" "$work/smali-view/android/view/Display.smali"
cp "$mesm" "$work/smali-view/android/view/MotionEvent.smali"
cp "$here/smali/android/view/View\$OnCapturedPointerListener.smali" "$work/smali-view/android/view/"
cp "$here/smali/android/view/Display\$Mode.smali" "$work/smali-view/android/view/"
cp "$asm" "$work/smali-view/android/app/Activity.smali"
cp "$prd_sm" "$work/smali-view/android/app/"
cp "$fsm" "$work/smali-view/android/app/Fragment.smali"
cp "$lmsm" "$work/smali-view/android/location/LocationManager.smali"
cp "$vibsm" "$work/smali-view/android/os/Vibrator.smali"
cp "$envsm" "$work/smali-view/android/os/Environment.smali"
cp "$winsm" "$work/smali-view/android/view/Window.smali"
cp "$ctxsm" "$work/smali-view/android/content/Context.smali"
cp "$spsm" "$work/smali-view/android/os/SystemProperties.smali"
cp "$pmsm" "$work/smali-view/android/content/pm/PackageManager.smali"
cp "$audio_manager_sm" "$work/smali-view/android/media/AudioManager.smali"
cp "$audio_attributes_builder_sm" "$work/smali-view/android/media/"
cp "$audio_focus_request_sm" "$audio_focus_builder_sm" "$work/smali-view/android/media/"
cp "$ppsm" "$work/smali-view/android/content/pm/PackageParser.smali"
cp "$ppkg_sm" "$work/smali-view/android/content/pm/PackageParser\$Package.smali"
cp "$atlsm" "$work/smali-view/android/atl/ATLLoadedApp.smali"
cp "$shortcut_sm" "$work/smali-view/android/content/pm/ShortcutManager.smali"
cp "$afm" "$work/smali-view/android/view/autofill/AutofillManager.smali"
cp "$csm" "$work/smali-view/android/webkit/CookieManager.smali"
cp "$wvsm" "$work/smali-view/android/webkit/WebView.smali"
cp "$wssm" "$work/smali-view/android/webkit/WebSettings.smali"
cp "$wvcsm" "$work/smali-view/android/webkit/WebViewClient.smali"
cp "$wccsm" "$work/smali-view/android/webkit/WebChromeClient.smali"
cp "$jpm" "$work/smali-view/android/app/job/JobParameters.smali"
cp "$psm" "$work/smali-view/android/graphics/Paint.smali"
mkdir -p "$work/smali-view/android/widget"
cp "$etsm" "$work/smali-view/android/widget/EditText.smali"
"$JAVA" -jar "$SMALI_JAR" assemble "$work/smali-view" -o "$work/jar/classes2.dex" >/dev/null

cp "$work/stock-classes.dex" "$work/jar/classes3.dex"
(cd "$work/jar" && "$JAR" cf api-impl.jar classes.dex classes2.dex classes3.dex)

wolf_src="$WOLFSSL_SOURCE"
[ -f "$wolf_src" ] || fail "vendored WolfSSLImplementSSLSession.java missing at $wolf_src"
grep -qF 'if (numCerts == 0)' "$wolf_src" || fail "vendored wolfSSL source lost its zero-peer-certificate guard"
grep -qF 'throw new SSLPeerUnverifiedException("No peer certificate")' "$wolf_src" || fail "vendored wolfSSL source no longer throws SSLPeerUnverifiedException for an absent peer certificate"

for art_jar in "${ART_BOOT_JARS[@]}"; do
    cp "$ART_DIR/$art_jar" "$work/art/$art_jar"
done

mkdir -p "$work/core-classes"
unzip -q "$CORE_ALL_CLASSES" -d "$work/core-classes"

for core_jar in core-oj-hostdex.jar core-libart-hostdex.jar; do
    core_module="${core_jar%-hostdex.jar}"
    core_work="$work/$core_module-desugar"
    mkdir -p "$core_work/input" "$core_work/output" "$core_work/verify"
    unzip -p "$work/art/$core_jar" classes.dex > "$core_work/original.dex"
    "$JAVA" -jar "$BAKSMALI_JAR" list classes "$core_work/original.dex" \
        | sed -e 's/^L//' -e 's/;$/\.class/' \
        | sort -u > "$core_work/classes.list"
    [ -s "$core_work/classes.list" ] || fail "$core_jar contained no classes"
    while IFS= read -r class_file; do
        [ -f "$work/core-classes/$class_file" ] \
            || fail "$core_jar class input missing from $CORE_ALL_CLASSES: $class_file"
    done < "$core_work/classes.list"
    (cd "$work/core-classes" && tar -cf - -T "$core_work/classes.list") \
        | (cd "$core_work/input" && tar -xf -)
    (cd "$core_work/input" && "$JAR" cf "$core_work/classes.jar" .)
    "$R8_JAVA" -cp "$R8_JAR" com.android.tools.r8.D8 \
        --android-platform-build \
        --min-api 26 \
        --output "$core_work/output" \
        "$core_work/classes.jar"
    [ -f "$core_work/output/classes.dex" ] || fail "D8 produced no classes.dex for $core_jar"
    dex_count="$(find "$core_work/output" -maxdepth 1 -type f -name 'classes*.dex' | wc -l)"
    [ "$dex_count" = "1" ] || fail "D8 produced $dex_count dex files for $core_jar (expected one)"
    "$JAVA" -jar "$BAKSMALI_JAR" disassemble "$core_work/output/classes.dex" -o "$core_work/verify" >/dev/null
    if grep -R -qF 'invoke-custom' "$core_work/verify"; then
        fail "$core_jar still contains unsupported invoke-custom instructions after D8 desugaring"
    fi
    mkdir -p "$core_work/update"
    cp "$core_work/output/classes.dex" "$core_work/update/classes.dex"
    (cd "$core_work/update" && "$JAR" uf "$work/art/$core_jar" classes.dex)
done

mkdir -p "$work/tzdata-compactor" "$work/art/zoneinfo"
"$JAVAC" "${JAVAC_8_FLAGS[@]}" -Xlint:all -Werror -d "$work/tzdata-compactor" "$here/TzdataCompactor.java"
"$JAVA" -cp "$work/tzdata-compactor" TzdataCompactor "$ZONEINFO_DIR" "$work/art/zoneinfo/tzdata"

libart_work="$work/core-libart-time-zone"
mkdir -p "$libart_work/update"
unzip -p "$work/art/core-libart-hostdex.jar" classes.dex > "$libart_work/classes.dex"
"$JAVA" -jar "$BAKSMALI_JAR" disassemble "$libart_work/classes.dex" -o "$libart_work/smali" >/dev/null
tz_files_sm="$libart_work/smali/libcore/util/TimeZoneDataFiles.smali"
tz_getter_dir="$libart_work/smali/org/apache/harmony/luni/internal/util"
tz_getter_sm="$tz_getter_dir/TimezoneGetter.smali"
[ -f "$tz_files_sm" ] || fail "TimeZoneDataFiles.smali not found in core-libart-hostdex.jar"
[ -f "$tz_getter_sm" ] || fail "TimezoneGetter.smali not found in core-libart-hostdex.jar"
perl -0pi -e '
    s{(\.method public static getSystemTimeZoneFile\(Ljava/lang/String;\)Ljava/lang/String;\n.*?\.end method\n)}{
        my $body = $1;
        $body =~ s{const-string v1, "ANDROID_ROOT"\n\n    invoke-static \{v1\}, Ljava/lang/System;->getenv\(Ljava/lang/String;\)Ljava/lang/String;}{const-string v1, "eclipse.zoneinfo_dir"\n\n    invoke-static {v1}, Ljava/lang/System;->getProperty(Ljava/lang/String;)Ljava/lang/String;}
            or die "getSystemTimeZoneFile no longer reads ANDROID_ROOT";
        $body =~ s{const-string v1, "/usr/share/zoneinfo/"}{const-string v1, "/"}
            or die "getSystemTimeZoneFile no longer appends /usr/share/zoneinfo/";
        $body
    }se or die "getSystemTimeZoneFile not found";
' "$tz_files_sm" || fail "libcore TimeZoneDataFiles.getSystemTimeZoneFile drifted; update patch-framework.sh"
grep -qF 'const-string v1, "eclipse.zoneinfo_dir"' "$tz_files_sm" || fail "TimeZoneDataFiles zoneinfo-directory insert failed (drift?)"
grep -qxF '.field private static instance:Lorg/apache/harmony/luni/internal/util/TimezoneGetter;' "$tz_getter_sm" || fail "TimezoneGetter lost its instance field — core-libart drifted; update patch-framework.sh"
! grep -qF '<clinit>' "$tz_getter_sm" || fail "TimezoneGetter already has a static initializer — core-libart drifted; update patch-framework.sh"
cat >> "$tz_getter_sm" <<'ECLIPSE_TIMEZONE_GETTER'

.method static constructor <clinit>()V
    .registers 1

    new-instance v0, Lorg/apache/harmony/luni/internal/util/UserTimezoneGetter;

    invoke-direct {v0}, Lorg/apache/harmony/luni/internal/util/UserTimezoneGetter;-><init>()V

    sput-object v0, Lorg/apache/harmony/luni/internal/util/TimezoneGetter;->instance:Lorg/apache/harmony/luni/internal/util/TimezoneGetter;

    return-void
.end method
ECLIPSE_TIMEZONE_GETTER
cp "$here/smali/org/apache/harmony/luni/internal/util/UserTimezoneGetter.smali" "$tz_getter_dir/"
"$JAVA" -jar "$SMALI_JAR" assemble --api 26 "$libart_work/smali" -o "$libart_work/update/classes.dex" >/dev/null
(cd "$libart_work/update" && "$JAR" uf "$work/art/core-libart-hostdex.jar" classes.dex)

unzip -p "$work/art/wolfssljni-hostdex.jar" classes.dex > "$work/wolf-classes.dex"
"$JAVA" -jar "$BAKSMALI_JAR" disassemble "$work/wolf-classes.dex" -o "$work/wolf-smali" >/dev/null
wolf_smali="$work/wolf-smali/com/wolfssl/provider/jsse/WolfSSLImplementSSLSession.smali"
[ -f "$wolf_smali" ] || fail "WolfSSLImplementSSLSession.smali not found in wolfssljni-hostdex.jar"
wolf_key_smali="$work/wolf-smali/com/wolfssl/provider/jsse/WolfSSLKeyX509.smali"
[ -f "$wolf_key_smali" ] || fail "WolfSSLKeyX509.smali not found in wolfssljni-hostdex.jar"
n="$(grep -cF '.method public declared-synchronized getPeerCertificates()[Ljava/security/cert/Certificate;' "$wolf_smali")" || true
[ "$n" = "1" ] || fail "wolfSSL getPeerCertificates method anchor not unique (found $n, expected 1) — ART hostdex drifted"
perl -0777 -ne 'if (/(\.method public declared-synchronized getPeerCertificates\(\)\[Ljava\/security\/cert\/Certificate;.*?\.end method)/s) { print $1 }' "$wolf_smali" > "$work/wolf-peer-method.smali"

WOLF_ZERO_ANCHOR=$'    .line 319\n    .local v7, "numCerts":I\n    :try_start_17\n    new-array v1, v7, [Ljava/security/cert/Certificate;'
if WOLF_ZERO_ANCHOR="$WOLF_ZERO_ANCHOR" perl -0777 -ne 'exit(index($_, $ENV{WOLF_ZERO_ANCHOR}) >= 0 ? 0 : 1)' "$work/wolf-peer-method.smali"; then

    WOLF_ZERO_ANCHOR="$WOLF_ZERO_ANCHOR" perl -0777 -pi -e 's{\Q$ENV{WOLF_ZERO_ANCHOR}\E}{    .line 319\n    .local v7, "numCerts":I\n    :try_start_17\n    if-nez v7, :eclipse_wolf_has_peer_certificate\n\n    new-instance v10, Ljavax/net/ssl/SSLPeerUnverifiedException;\n\n    const-string v11, "No peer certificate"\n\n    invoke-direct {v10, v11}, Ljavax/net/ssl/SSLPeerUnverifiedException;-><init>(Ljava/lang/String;)V\n\n    throw v10\n\n    :eclipse_wolf_has_peer_certificate\n    new-array v1, v7, [Ljava/security/cert/Certificate;}s' "$wolf_smali"
    grep -qF ':eclipse_wolf_has_peer_certificate' "$wolf_smali" || fail "wolfSSL zero-peer-certificate guard insert failed"
else

    perl -0777 -ne 'exit(/getPeerCertificateNum\(\)I.*?move-result v7.*?if-nez v7,.*?new-instance .*?SSLPeerUnverifiedException;.*?const-string .*?"No peer certificate".*?throw .*?new-array v1, v7/s ? 0 : 1)' "$work/wolf-peer-method.smali" || fail "wolfSSL getPeerCertificates no longer matches either the known stale body or the source-correct zero guard — ART hostdex drifted"
fi

n="$(grep -cF '.method public getPrivateKey(Ljava/lang/String;)Ljava/security/PrivateKey;' "$wolf_key_smali")" || true
[ "$n" = "1" ] || fail "wolfSSL getPrivateKey method anchor not unique (found $n, expected 1) — ART hostdex drifted"
perl -0777 -ne 'if (/(\.method public getPrivateKey\(Ljava\/lang\/String;\)Ljava\/security\/PrivateKey;.*?\.end method)/s) { print $1 }' "$wolf_key_smali" > "$work/wolf-key-method.smali"

WOLF_KEY_STORE_ANCHOR=$'    .line 245\n    :try_start_1d\n    iget-object v2, p0, Lcom/wolfssl/provider/jsse/WolfSSLKeyX509;->store:Ljava/security/KeyStore;'
if WOLF_KEY_STORE_ANCHOR="$WOLF_KEY_STORE_ANCHOR" perl -0777 -ne 'exit(index($_, $ENV{WOLF_KEY_STORE_ANCHOR}) >= 0 ? 0 : 1)' "$work/wolf-key-method.smali"; then
    WOLF_KEY_STORE_ANCHOR="$WOLF_KEY_STORE_ANCHOR" perl -0777 -pi -e 's{\Q$ENV{WOLF_KEY_STORE_ANCHOR}\E}{    .line 245\n    iget-object v2, p0, Lcom/wolfssl/provider/jsse/WolfSSLKeyX509;->store:Ljava/security/KeyStore;\n\n    if-nez v2, :eclipse_wolf_has_key_store\n\n    return-object v1\n\n    :eclipse_wolf_has_key_store\n    :try_start_1d}s' "$wolf_key_smali"
    grep -qF 'if-nez v2, :eclipse_wolf_has_key_store' "$wolf_key_smali" || fail "wolfSSL nullable key-store guard insert failed"
else
    perl -0777 -ne 'exit(/iget-object v2, p0, Lcom\/wolfssl\/provider\/jsse\/WolfSSLKeyX509;->store:Ljava\/security\/KeyStore;.*?if-nez v2, (:[[:alnum:]_]+).*?return-object v1.*?\1.*?invoke-virtual \{v2, p1, v3\}, Ljava\/security\/KeyStore;->getKey/s ? 0 : 1)' "$work/wolf-key-method.smali" || fail "wolfSSL getPrivateKey no longer matches either the known nullable-store body or the explicit null guard — ART hostdex drifted"
fi

"$JAVA" -jar "$SMALI_JAR" assemble "$work/wolf-smali" -o "$work/wolf-classes-patched.dex" >/dev/null
mkdir -p "$work/wolf-jar-update"
cp "$work/wolf-classes-patched.dex" "$work/wolf-jar-update/classes.dex"
(cd "$work/wolf-jar-update" && "$JAR" uf "$work/art/wolfssljni-hostdex.jar" classes.dex)

unzip -p "$work/art/wolfssljni-hostdex.jar" classes.dex > "$work/wolf-verify.dex"
"$JAVA" -jar "$BAKSMALI_JAR" disassemble "$work/wolf-verify.dex" -o "$work/wolf-verify-smali" >/dev/null
wolf_verify="$work/wolf-verify-smali/com/wolfssl/provider/jsse/WolfSSLImplementSSLSession.smali"
perl -0777 -ne 'if (/(\.method public declared-synchronized getPeerCertificates\(\)\[Ljava\/security\/cert\/Certificate;.*?\.end method)/s) { print $1 }' "$wolf_verify" > "$work/wolf-peer-method-verify.smali"
perl -0777 -ne 'exit(/getPeerCertificateNum\(\)I.*?move-result v7.*?if-nez v7,.*?new-instance .*?SSLPeerUnverifiedException;.*?const-string .*?"No peer certificate".*?throw .*?new-array v1, v7/s ? 0 : 1)' "$work/wolf-peer-method-verify.smali" || fail "built wolfssljni-hostdex.jar does not enforce the zero-peer-certificate SSLSession contract"
wolf_key_verify="$work/wolf-verify-smali/com/wolfssl/provider/jsse/WolfSSLKeyX509.smali"
[ -f "$wolf_key_verify" ] || fail "built wolfssljni-hostdex.jar lost WolfSSLKeyX509"
perl -0777 -ne 'if (/(\.method public getPrivateKey\(Ljava\/lang\/String;\)Ljava\/security\/PrivateKey;.*?\.end method)/s) { print $1 }' "$wolf_key_verify" > "$work/wolf-key-method-verify.smali"
perl -0777 -ne 'exit(/iget-object v2, p0, Lcom\/wolfssl\/provider\/jsse\/WolfSSLKeyX509;->store:Ljava\/security\/KeyStore;.*?if-nez v2, (:[[:alnum:]_]+).*?return-object v1.*?\1.*?invoke-virtual \{v2, p1, v3\}, Ljava\/security\/KeyStore;->getKey/s ? 0 : 1)' "$work/wolf-key-method-verify.smali" || fail "built wolfssljni-hostdex.jar does not guard its nullable key store"

date_time_probe="$here/tests/DateTimeFormatterProbe.java"
[ -f "$date_time_probe" ] || fail "date-time formatter regression probe missing at $date_time_probe"
mkdir -p "$work/date-time-probe/classes" "$work/date-time-probe/cache" "$work/date-time-probe/data"
"$JAVAC" "${JAVAC_8_FLAGS[@]}" -Xlint:all -Werror -d "$work/date-time-probe/classes" "$date_time_probe"
"$DX" --dex --output="$work/date-time-probe/probe.jar" "$work/date-time-probe/classes"
boot_class_path=''
boot_class_path_locations=''
for art_jar in "${ART_BOOT_JARS[@]}"; do
    if [ -z "$boot_class_path" ]; then
        boot_class_path="$work/art/$art_jar"
        boot_class_path_locations="/system/framework/$art_jar"
    else
        boot_class_path="$boot_class_path:$work/art/$art_jar"
        boot_class_path_locations="$boot_class_path_locations:/system/framework/$art_jar"
    fi
done
date_time_boot_class_path="$boot_class_path:$work/date-time-probe/probe.jar"
date_time_boot_class_path_locations="$boot_class_path_locations:/system/framework/probe.jar"
probe_output="$(env \
    ANDROID_DATA="$work/date-time-probe/data" \
    XDG_CACHE_HOME="$work/date-time-probe/cache" \
    BOOTCLASSPATH="$date_time_boot_class_path" \
    "$DALVIKVM" \
    -Ximage:"$work/art/oat/boot.art" \
    -Xbootclasspath:"$date_time_boot_class_path" \
    -Xbootclasspath-locations:"$date_time_boot_class_path_locations" \
    -Ximage-compiler-option --no-generate-debug-info \
    -Ximage-compiler-option --no-generate-mini-debug-info \
    DateTimeFormatterProbe)"
[ "$probe_output" = '2026-08-30 13:00:00' ] \
    || fail "date-time formatter regression probe returned '$probe_output'"

time_zone_probe="$here/tests/TimeZoneProbe.java"
[ -f "$time_zone_probe" ] || fail "time-zone regression probe missing at $time_zone_probe"
mkdir -p "$work/time-zone-probe/classes" "$work/time-zone-probe/cache" "$work/time-zone-probe/data"
"$JAVAC" "${JAVAC_8_FLAGS[@]}" -Xlint:all -Werror -d "$work/time-zone-probe/classes" "$time_zone_probe"
"$DX" --dex --output="$work/time-zone-probe/probe.jar" "$work/time-zone-probe/classes"
time_zone_boot_class_path="$boot_class_path:$work/time-zone-probe/probe.jar"
time_zone_boot_class_path_locations="$boot_class_path_locations:/system/framework/probe.jar"
time_zone_output="$(env \
    ANDROID_DATA="$work/time-zone-probe/data" \
    XDG_CACHE_HOME="$work/time-zone-probe/cache" \
    BOOTCLASSPATH="$time_zone_boot_class_path" \
    "$DALVIKVM" \
    -Ximage:"$work/art/oat/boot.art" \
    -Xbootclasspath:"$time_zone_boot_class_path" \
    -Xbootclasspath-locations:"$time_zone_boot_class_path_locations" \
    -Ximage-compiler-option --no-generate-debug-info \
    -Ximage-compiler-option --no-generate-mini-debug-info \
    -Declipse.zoneinfo_dir="$work/art/zoneinfo" \
    -Duser.timezone=America/Los_Angeles \
    TimeZoneProbe)"
[ "$time_zone_output" = 'time-zone-ok' ] \
    || fail "time-zone regression probe returned '$time_zone_output'"

keygen_probe="$here/tests/KeyGenParameterSpecProbe.java"
[ -f "$keygen_probe" ] || fail "key-generation regression probe missing at $keygen_probe"
mkdir -p "$work/keygen-probe/classes"
"$JAVAC" "${JAVAC_8_FLAGS[@]}" -Xlint:all -Werror -cp "$work/classes" \
    -d "$work/keygen-probe/classes" "$keygen_probe"
keygen_output="$("$JAVA" -cp "$work/classes:$work/keygen-probe/classes" KeyGenParameterSpecProbe)"
[ "$keygen_output" = 'keygen-parameter-spec-ok' ] \
    || fail "key-generation regression probe returned '$keygen_output'"

signing_probe="$here/tests/SigningCertificatesProbe.java"
[ -f "$signing_probe" ] || fail "signing-certificate regression probe missing at $signing_probe"
mkdir -p "$work/signing-probe/classes"
"$JAVAC" "${JAVAC_8_FLAGS[@]}" -Xlint:all -Werror -cp "$work/classes" \
    -d "$work/signing-probe/classes" "$signing_probe"
signing_output="$("$JAVA" -cp "$work/classes:$work/signing-probe/classes" android.content.pm.SigningCertificatesProbe)"
[ "$signing_output" = 'signing-certificates-ok' ] \
    || fail "signing-certificate regression probe returned '$signing_output'"

display_probe="$here/tests/DisplayRefreshRateProbe.java"
[ -f "$display_probe" ] || fail "display refresh-rate regression probe missing at $display_probe"
mkdir -p "$work/display-probe/classes" "$work/display-probe/cache" "$work/display-probe/data"
"$JAVAC" "${JAVAC_8_FLAGS[@]}" -Xlint:all -Werror -d "$work/display-probe/classes" "$display_probe"
"$DX" --dex --output="$work/display-probe/probe.jar" "$work/display-probe/classes"
display_boot_class_path="$boot_class_path:$work/jar/api-impl.jar:$work/display-probe/probe.jar"
display_boot_class_path_locations="$boot_class_path_locations:/system/framework/api-impl.jar:/system/framework/probe.jar"
display_output="$(env \
    ANDROID_DATA="$work/display-probe/data" \
    XDG_CACHE_HOME="$work/display-probe/cache" \
    BOOTCLASSPATH="$display_boot_class_path" \
    "$DALVIKVM" \
    -Ximage:"$work/art/oat/boot.art" \
    -Xbootclasspath:"$display_boot_class_path" \
    -Xbootclasspath-locations:"$display_boot_class_path_locations" \
    -Ximage-compiler-option --no-generate-debug-info \
    -Ximage-compiler-option --no-generate-mini-debug-info \
    DisplayRefreshRateProbe)"
[ "$display_output" = 'display-refresh-rates-ok' ] \
    || fail "display refresh-rate regression probe returned '$display_output'"

motion_event_probe="$here/tests/MotionEventPointersProbe.java"
[ -f "$motion_event_probe" ] || fail "MotionEvent pointer regression probe missing at $motion_event_probe"
mkdir -p "$work/motion-event-probe/classes" "$work/motion-event-probe/cache" "$work/motion-event-probe/data"
"$JAVAC" "${JAVAC_8_FLAGS[@]}" -Xlint:all -Werror -d "$work/motion-event-probe/classes" "$motion_event_probe"
"$DX" --dex --output="$work/motion-event-probe/probe.jar" "$work/motion-event-probe/classes"
motion_event_boot_class_path="$boot_class_path:$work/jar/api-impl.jar:$work/motion-event-probe/probe.jar"
motion_event_boot_class_path_locations="$boot_class_path_locations:/system/framework/api-impl.jar:/system/framework/probe.jar"
motion_event_output="$(env \
    ANDROID_DATA="$work/motion-event-probe/data" \
    XDG_CACHE_HOME="$work/motion-event-probe/cache" \
    BOOTCLASSPATH="$motion_event_boot_class_path" \
    "$DALVIKVM" \
    -Ximage:"$work/art/oat/boot.art" \
    -Xbootclasspath:"$motion_event_boot_class_path" \
    -Xbootclasspath-locations:"$motion_event_boot_class_path_locations" \
    -Ximage-compiler-option --no-generate-debug-info \
    -Ximage-compiler-option --no-generate-mini-debug-info \
    MotionEventPointersProbe)"
[ "$motion_event_output" = 'motion-event-pointers-ok' ] \
    || fail "MotionEvent pointer regression probe returned '$motion_event_output'"

voice_chat_probe="$here/tests/VoiceChatProbe.java"
[ -f "$voice_chat_probe" ] || fail "voice chat regression probe missing at $voice_chat_probe"
mkdir -p "$work/voice-chat-probe/classes" "$work/voice-chat-probe/cache" "$work/voice-chat-probe/data"
"$JAVAC" "${JAVAC_8_FLAGS[@]}" -Xlint:all -Werror -d "$work/voice-chat-probe/classes" "$voice_chat_probe"
"$DX" --dex --output="$work/voice-chat-probe/probe.jar" "$work/voice-chat-probe/classes"
voice_chat_boot_class_path="$boot_class_path:$work/jar/api-impl.jar:$work/voice-chat-probe/probe.jar"
voice_chat_boot_class_path_locations="$boot_class_path_locations:/system/framework/api-impl.jar:/system/framework/probe.jar"
voice_chat_output="$(env \
    -u ATL_UGLY_ENABLE_MICROPHONE \
    -u ATL_UGLY_ENABLE_LOCATION \
    ANDROID_DATA="$work/voice-chat-probe/data" \
    XDG_CACHE_HOME="$work/voice-chat-probe/cache" \
    BOOTCLASSPATH="$voice_chat_boot_class_path" \
    "$DALVIKVM" \
    -Ximage:"$work/art/oat/boot.art" \
    -Xbootclasspath:"$voice_chat_boot_class_path" \
    -Xbootclasspath-locations:"$voice_chat_boot_class_path_locations" \
    -Ximage-compiler-option --no-generate-debug-info \
    -Ximage-compiler-option --no-generate-mini-debug-info \
    VoiceChatProbe)"
[ "$voice_chat_output" = 'voice-chat-ok' ] \
    || fail "voice chat regression probe returned '$voice_chat_output'"

audio_properties_probe="$here/tests/AudioPropertiesProbe.java"
[ -f "$audio_properties_probe" ] || fail "audio properties regression probe missing at $audio_properties_probe"
mkdir -p "$work/audio-properties-probe/classes" "$work/audio-properties-probe/cache" "$work/audio-properties-probe/data"
"$JAVAC" "${JAVAC_8_FLAGS[@]}" -Xlint:all -Werror -d "$work/audio-properties-probe/classes" "$audio_properties_probe"
"$DX" --dex --output="$work/audio-properties-probe/probe.jar" "$work/audio-properties-probe/classes"
audio_properties_boot_class_path="$boot_class_path:$work/jar/api-impl.jar:$work/audio-properties-probe/probe.jar"
audio_properties_boot_class_path_locations="$boot_class_path_locations:/system/framework/api-impl.jar:/system/framework/probe.jar"
audio_properties_output="$(env \
    ANDROID_DATA="$work/audio-properties-probe/data" \
    XDG_CACHE_HOME="$work/audio-properties-probe/cache" \
    BOOTCLASSPATH="$audio_properties_boot_class_path" \
    "$DALVIKVM" \
    -Ximage:"$work/art/oat/boot.art" \
    -Xbootclasspath:"$audio_properties_boot_class_path" \
    -Xbootclasspath-locations:"$audio_properties_boot_class_path_locations" \
    -Ximage-compiler-option --no-generate-debug-info \
    -Ximage-compiler-option --no-generate-mini-debug-info \
    -Declipse.audio.output_sample_rate=48000 \
    -Declipse.audio.output_frames_per_buffer=512 \
    AudioPropertiesProbe)"
[ "$audio_properties_output" = 'audio-properties-ok' ] \
    || fail "audio properties regression probe returned '$audio_properties_output'"

webview_probe="$here/tests/WebViewCallbacksProbe.java"
[ -f "$webview_probe" ] || fail "WebView callback regression probe missing at $webview_probe"
mkdir -p "$work/webview-probe/classes" "$work/webview-probe/cache" "$work/webview-probe/data"
"$JAVAC" "${JAVAC_8_FLAGS[@]}" -Xlint:all -Werror -implicit:none \
    -sourcepath "$here/src:$here/stubs" -d "$work/webview-probe/classes" "$webview_probe"
"$DX" --dex --output="$work/webview-probe/probe.jar" "$work/webview-probe/classes"
webview_boot_class_path="$boot_class_path:$work/jar/api-impl.jar:$work/webview-probe/probe.jar"
webview_boot_class_path_locations="$boot_class_path_locations:/system/framework/api-impl.jar:/system/framework/probe.jar"
webview_output="$(env \
    ANDROID_DATA="$work/webview-probe/data" \
    XDG_CACHE_HOME="$work/webview-probe/cache" \
    BOOTCLASSPATH="$webview_boot_class_path" \
    "$DALVIKVM" \
    -Ximage:"$work/art/oat/boot.art" \
    -Xbootclasspath:"$webview_boot_class_path" \
    -Xbootclasspath-locations:"$webview_boot_class_path_locations" \
    -Ximage-compiler-option --no-generate-debug-info \
    -Ximage-compiler-option --no-generate-mini-debug-info \
    android.webkit.WebViewCallbacksProbe)"
[ "$webview_output" = 'webview-callbacks-ok' ] \
    || fail "WebView callback regression probe returned '$webview_output'"

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
mkdir -p "$OUT/art/zoneinfo"
cp "$work/art/zoneinfo/tzdata" "$OUT/art/zoneinfo/tzdata"
printf '%s\n' 'eclipse-art-overlay-v1' > "$art_ready"

echo "OK: patched framework overlay installed at $OUT"
classes_dex_size="$(stat -c '%s' "$work/jar/classes.dex")"
classes2_dex_size="$(stat -c '%s' "$work/jar/classes2.dex")"
classes3_dex_size="$(stat -c '%s' "$work/jar/classes3.dex")"
echo "    classes.dex (javac-patched): $classes_dex_size bytes; classes2.dex (smali Android API gaps, including LocationManager): $classes2_dex_size bytes; classes3.dex (stock): $classes3_dex_size bytes"
echo "    ART boot jars: ${#ART_BOOT_JARS[@]} copied to $OUT/art; key generation, signing certificates, date-time, display refresh-rate, MotionEvent pointer, voice chat, audio properties, WebView callback, and wolfSSL contracts verified"
echo "    use it with: export ECLIPSE_ANDROID_FRAMEWORK_DIR=\"$OUT\""
