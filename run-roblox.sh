#!/usr/bin/env bash

set -euo pipefail

repo_dir="$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)"
atl_root="/home/yoshi/Documents/android_translation_layer"
atl_prefix="$atl_root/build/install"
apk="$repo_dir/target/apkm-roblox-2.735.1138/roblox-2.735.1138-x86_64.apk"
webview_helper="$repo_dir/dist/eclipse-linux-x86_64/eclipse-webview"

runtime_libs="$atl_prefix/lib/art:$atl_prefix/lib:$atl_prefix/lib/java/dex/art/natives:$atl_prefix/lib/java/dex/android_translation_layer/natives:$atl_root/build/lib"
if [[ -n "${LD_LIBRARY_PATH:-}" ]]; then
    runtime_libs="$runtime_libs:$LD_LIBRARY_PATH"
fi

[[ -x "$repo_dir/target/release/eclipse" ]] || {
    echo "Eclipse n'est pas construit : lance cargo build --release" >&2
    exit 1
}
[[ -f "$apk" ]] || {
    echo "APK Roblox introuvable : $apk" >&2
    exit 1
}
[[ -x "$webview_helper" ]] || {
    echo "Helper WebView introuvable : $webview_helper" >&2
    echo "Lance d'abord ./tools/webview-dist/package-webview.sh" >&2
    exit 1
}

cd "$repo_dir"
exec env \
    -u ANDROID_ROOT \
    -u ANDROID_DATA \
    -u ECLIPSE_ART_BOOT_IMAGE \
    -u BOOTCLASSPATH \
    -u DEX2OATBOOTCLASSPATH \
    PATH="$atl_prefix/bin:$PATH" \
    LD_LIBRARY_PATH="$runtime_libs" \
    ECLIPSE_LIBART="$atl_prefix/lib/art/libart.so" \
    ECLIPSE_ANDROID_FRAMEWORK_DIR="/home/yoshi/.cache/eclipse/framework-patched" \
    ECLIPSE_WEBVIEW_HELPER="$webview_helper" \
    "$repo_dir/target/release/eclipse" run "$apk"
