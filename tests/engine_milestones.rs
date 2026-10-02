#[path = "../src/bounded_child.rs"]
mod bounded_child;

use std::ffi::OsStr;
use std::path::PathBuf;
use std::process::{Command, Output};
use std::time::Duration;

const DIAGNOSTIC_LIMIT: Duration = Duration::from_secs(60);

const ENGINE_LIMIT: Duration = Duration::from_secs(300);

fn roblox_apk_present() -> bool {
    eclipse::apk::ApkSetPaths::from_env()
        .expect("ECLIPSE_ROBLOX_APK must name an APK or a directory holding base.apk")
        .is_some()
}

fn completed_constructor_counts(output: &str) -> Option<(u64, u64)> {
    let (_, tail) = output.split_once("ALL ")?;
    let (counts, rest) = tail.split_once(' ')?;
    if !rest.starts_with("constructors completed without a crash") {
        return None;
    }
    let (done, total) = counts.split_once('/')?;
    Some((done.parse().ok()?, total.parse().ok()?))
}

fn declared_constructor_count(output: &str) -> Option<u64> {
    let (_, tail) = output.split_once("DT_INIT_ARRAY: ")?;
    let line = tail.lines().next()?;
    let (_, count) = line.rsplit_once("-> ")?;
    count.strip_suffix(" constructors")?.parse().ok()
}

fn android_runtime_present() -> bool {
    eclipse::runtime::find_libart().is_ok()
        && eclipse::runtime::find_framework().is_ok()
        && eclipse::runtime::find_boot_image().is_ok()
}

fn display_available() -> bool {
    std::env::var_os("WAYLAND_DISPLAY").is_some() || std::env::var_os("DISPLAY").is_some()
}

fn run_eclipse(subcommand: &str, args: &[&OsStr], limit: Duration) -> Output {
    bounded_child::output(
        Command::new(env!("CARGO_BIN_EXE_eclipse"))
            .arg(subcommand)
            .args(args),
        limit,
    )
}

fn combined(out: &Output) -> String {
    let mut s = String::from_utf8_lossy(&out.stdout).into_owned();
    s.push_str(&String::from_utf8_lossy(&out.stderr));
    s
}

fn gl_env_unavailable(output: &str) -> bool {
    output.contains("winit event loop")
        || output.contains("EGL_NO_DISPLAY")
        || output.contains("eglInitialize")
        || output.contains("no available configs")
}

struct HarnessLibDir(PathBuf);

impl HarnessLibDir {
    fn create() -> Self {
        let path = PathBuf::from(env!("CARGO_TARGET_TMPDIR"))
            .join(format!("run-libroblox-init-{}", std::process::id()));
        std::fs::create_dir_all(&path).expect("create the harness lib dir");
        Self(path)
    }
}

impl Drop for HarnessLibDir {
    fn drop(&mut self) {
        let removed = std::fs::remove_dir_all(&self.0);
        if !std::thread::panicking() {
            removed.expect("remove the harness lib dir");
        }
    }
}

#[test]
fn run_libroblox_init_runs_every_constructor() {
    if !roblox_apk_present() {
        eprintln!(
            "SKIP: Roblox APK absent (set ECLIPSE_ROBLOX_APK to an APK file or to a directory \
             holding base.apk and split_config.x86_64.apk)"
        );
        return;
    }

    let lib_dir = HarnessLibDir::create();
    let out = run_eclipse(
        "__run-libroblox-init",
        &[lib_dir.0.as_os_str()],
        ENGINE_LIMIT,
    );
    drop(lib_dir);
    let text = combined(&out);

    assert!(
        out.status.success(),
        "__run-libroblox-init exited non-zero ({:?}); a constructor likely faulted.\n{text}",
        out.status.code()
    );
    let declared = declared_constructor_count(&text)
        .unwrap_or_else(|| panic!("missing the DT_INIT_ARRAY constructor count line.\n{text}"));
    let (completed, total) = completed_constructor_counts(&text).unwrap_or_else(|| {
        panic!(
            "missing the 'ALL n/n constructors completed' marker (loader/relocation/resolve \
             regression or a constructor crash?).\n{text}"
        )
    });
    assert!(declared > 0, "libroblox declares constructors.\n{text}");
    assert_eq!(
        (completed, total),
        (declared, declared),
        "every DT_INIT_ARRAY constructor must run.\n{text}"
    );
}

#[test]
fn gl_test_renders_engine_surface_with_zero_gl_errors() {
    if !display_available() {
        eprintln!(
            "SKIP: no display server (WAYLAND_DISPLAY/DISPLAY unset) — GL render path needs one"
        );
        return;
    }

    let out = run_eclipse("__gl-test", &[], DIAGNOSTIC_LIMIT);
    let text = combined(&out);

    if !out.status.success() && gl_env_unavailable(&text) {
        eprintln!("SKIP: display advertised but EGL/event-loop unavailable on this host (env limitation)\n{text}");
        return;
    }

    assert!(
        out.status.success(),
        "__gl-test exited non-zero ({:?}); engine GL render-path regression.\n{text}",
        out.status.code()
    );

    assert!(
        text.contains("EGL+GLES2 OK:") && text.contains("0 GL errors, all swaps succeeded"),
        "missing the engine GLES2/EGL success marker (render-path regression?).\n{text}"
    );
}

#[test]
fn gl_test_anw_binds_real_wsi_handle() {
    if !display_available() {
        eprintln!(
            "SKIP: no display server (WAYLAND_DISPLAY/DISPLAY unset) — engine WSI bind needs one"
        );
        return;
    }

    let out = run_eclipse("__gl-test-anw", &[], DIAGNOSTIC_LIMIT);
    let text = combined(&out);

    if !out.status.success() && gl_env_unavailable(&text) {
        eprintln!("SKIP: display advertised but EGL/event-loop unavailable on this host (env limitation)\n{text}");
        return;
    }

    assert!(
        out.status.success(),
        "__gl-test-anw exited non-zero ({:?}); engine WSI-bind regression.\n{text}",
        out.status.code()
    );

    assert!(
        text.contains("ANativeWindow* is the real WSI handle = true")
            && text.contains("0 GL errors, all swaps succeeded"),
        "missing the engine-style eglCreateWindowSurface(ANativeWindow) success marker, or the \
         ANativeWindow* was NOT the real WSI handle (WSI-bind regression?).\n{text}"
    );
}

#[test]
fn webview_test_fires_load_upcalls_and_stages_frames() {
    if !roblox_apk_present() {
        eprintln!(
            "SKIP: Roblox APK absent (set ECLIPSE_ROBLOX_APK to an APK file or to a directory \
             holding base.apk and split_config.x86_64.apk)"
        );
        return;
    }
    if !display_available() {
        eprintln!(
            "SKIP: no display server (WAYLAND_DISPLAY/DISPLAY unset) — the CEF helper needs one"
        );
        return;
    }
    if !android_runtime_present() {
        eprintln!(
            "SKIP: Android runtime absent (ART, its boot image or the patched framework; set \
             ECLIPSE_LIBART and ECLIPSE_ANDROID_FRAMEWORK_DIR)"
        );
        return;
    }

    let out = run_eclipse("__webview-test", &[], ENGINE_LIMIT);
    let text = combined(&out);

    if !out.status.success()
        && (text.contains(eclipse::webview::client::HELPER_NOT_FOUND_MARKER)
            || text.contains(eclipse::webview::client::NO_DISPLAY_MARKER)
            || text.contains(eclipse::webview::client::SANDBOX_UNAVAILABLE_MARKER))
    {
        eprintln!(
            "SKIP: eclipse-webview helper/CEF unavailable on this host (env limitation)\n{text}"
        );
        return;
    }

    assert!(
        out.status.success(),
        "__webview-test exited non-zero ({:?}); WebView engine pipeline regression.\n{text}",
        out.status.code()
    );

    assert!(
        text.contains("WebView engine pipeline OK:") && text.contains("upcalls 2/2"),
        "missing the WebView pipeline success marker (natives→socket→helper→upcall regression?).\n{text}"
    );
    for needle in [
        "bridge round-trip OK",
        "evaluateJavascript OK",
        "honest UA OK",
        "cookie set/get OK",
        "cookie callback OK",
        "cookie flush OK",
    ] {
        assert!(
            text.contains(needle),
            "missing the M4 marker substring {needle:?} (bridge/eval/UA/cookie regression?).\n{text}"
        );
    }
    assert!(
        text.contains("bound=7"),
        "the WebView native registration count regressed (expected the live bound=7 line — load, \
         history, evaluateJavascript, and addJavascriptInterface natives).\n{text}"
    );

    for needle in [
        "ozone platform selected explicitly: ",
        "sandbox mode selected: ",
        "webview host-lib probe: ",
        "render path: ",
    ] {
        assert!(
            text.contains(needle),
            "missing the M5 detection line {needle:?} (detect-don't-assume regression?).\n{text}"
        );
    }
}

#[test]
fn root_lockfile_stays_cef_free() {
    let lock_path = concat!(env!("CARGO_MANIFEST_DIR"), "/Cargo.lock");
    let lock = std::fs::read_to_string(lock_path)
        .unwrap_or_else(|e| panic!("cannot read {lock_path}: {e}"));
    for package in [
        "name = \"cef\"",
        "name = \"cef-dll-sys\"",
        "name = \"download-cef\"",
        "name = \"export-cef-dir\"",
    ] {
        assert!(
            !lock.contains(package),
            "the root Cargo.lock gained a CEF package entry ({package}) — the engine must stay \
             confined to the workspace-detached crates/eclipse-webview helper"
        );
    }
}

#[test]
fn framework_overlay_preserves_location_manager_provider_query_contract() {
    let generator = include_str!("../tools/framework-overlay/patch-framework.sh");
    for needle in [
        ".method public isProviderEnabled(Ljava/lang/String;)Z",
        "Ljava/lang/IllegalArgumentException;-><init>(Ljava/lang/String;)V",
        ":eclipse_location_provider_non_null",
        "cp \"$lmsm\" \"$work/smali-view/android/location/LocationManager.smali\"",
    ] {
        assert!(
            generator.contains(needle),
            "framework overlay lost LocationManager provider-query contract fragment {needle:?}; \
             the current client would regress to NoSuchMethodError/System.exit(10)"
        );
    }
}

#[test]
fn framework_overlay_preserves_shortcut_manager_contract() {
    let generator = include_str!("../tools/framework-overlay/patch-framework.sh");
    for needle in [
        ".method public getMaxShortcutCountPerActivity()I",
        "setDynamicShortcuts honest-false patch failed",
        ".method public getDynamicShortcuts()Ljava/util/List;",
        "cp \"$shortcut_sm\" \"$work/smali-view/android/content/pm/ShortcutManager.smali\"",
    ] {
        assert!(
            generator.contains(needle),
            "framework overlay lost ShortcutManager contract fragment {needle:?}; Roblox's OS-search worker would regress to NoSuchMethodError/System.exit(10)"
        );
    }
    let max_body = generator
        .split_once(".method public getMaxShortcutCountPerActivity()I")
        .expect("ShortcutManager max-count method missing")
        .1
        .split_once(".end method")
        .expect("ShortcutManager max-count method is incomplete")
        .0;
    assert!(
        max_body.contains("const/4 v0, 0x0"),
        "ShortcutManager must report zero capacity when Eclipse has no desktop shortcut backend"
    );
}

#[test]
fn framework_overlay_preserves_connectivity_v2_contract() {
    let generator = include_str!("../tools/framework-overlay/patch-framework.sh");
    for needle in [
        ".method public getLinkProperties(Landroid/net/Network;)Landroid/net/LinkProperties;",
        ".method public getNetworkInfo(Landroid/net/Network;)Landroid/net/NetworkInfo;",
        "cp \"$connectivity_sm\" \"$work/smali-view/android/net/ConnectivityManager.smali\"",
        "smali/android/net/LinkProperties.smali",
        "smali/android/net/LinkAddress.smali",
    ] {
        assert!(
            generator.contains(needle),
            "framework overlay lost ConnectivityManager contract fragment {needle:?}; Roblox ConnectivityV2 would regress to NoSuchMethodError"
        );
    }

    let link_properties =
        include_str!("../tools/framework-overlay/smali/android/net/LinkProperties.smali");
    assert!(
        link_properties.contains("getLinkAddresses()Ljava/util/List;")
            && link_properties.contains("Ljava/util/Collections;->emptyList()Ljava/util/List;"),
        "LinkProperties must expose a non-null empty address list when host addresses are unavailable"
    );
    let link_address =
        include_str!("../tools/framework-overlay/smali/android/net/LinkAddress.smali");
    for needle in ["getAddress()Ljava/net/InetAddress;", "getPrefixLength()I"] {
        assert!(
            link_address.contains(needle),
            "LinkAddress lost app-referenced contract fragment {needle:?}"
        );
    }
}

#[test]
fn framework_overlay_preserves_key_generation_quote_contract() {
    let root = PathBuf::from(env!("CARGO_MANIFEST_DIR"));
    let source_path =
        root.join("tools/framework-overlay/src/android/security/keystore/KeyGenParameterSpec.java");
    let source = std::fs::read_to_string(&source_path)
        .unwrap_or_else(|error| panic!("cannot read {}: {error}", source_path.display()));
    for needle in [
        "implements AlgorithmParameterSpec",
        "setAlgorithmParameterSpec(AlgorithmParameterSpec spec)",
        "setDigests(String... digests)",
        "setAttestationChallenge(byte[] attestationChallenge)",
        "setKeyValidityStart(Date keyValidityStart)",
        "setIsStrongBoxBacked(boolean strongBoxBacked)",
        "setCertificateSubject(X500Principal certificateSubject)",
        "setCertificateSerialNumber(BigInteger certificateSerialNumber)",
        "setCertificateNotBefore(Date certificateNotBefore)",
        "setCertificateNotAfter(Date certificateNotAfter)",
        "getAlgorithmParameterSpec()",
        "getAttestationChallenge()",
        "digests.clone()",
        "cloneBytes(builder.attestationChallenge)",
        "return cloneBytes(attestationChallenge);",
    ] {
        assert!(
            source.contains(needle),
            "KeyGenParameterSpec overlay lost app-used contract fragment {needle:?}"
        );
    }

    let generator = include_str!("../tools/framework-overlay/patch-framework.sh");
    for needle in [
        "KeyGenParameterSpec.java",
        "android/security/keystore/KeyGenParameterSpec*.class",
        "KeyGenParameterSpecProbe.java",
        "\"$JAVA\" -cp \"$work/classes:$work/keygen-probe/classes\"",
        "keygen-parameter-spec-ok",
    ] {
        assert!(
            generator.contains(needle),
            "framework generator lost the key-generation runtime guard {needle:?}"
        );
    }
    assert!(
        !generator.contains("--output=\"$work/keygen-probe/probe.jar\""),
        "the key-generation semantic probe must not use the raw Dalvik shutdown path; exact DEX \
         descriptors are verified independently"
    );
    for needle in [
        "boot_class_path_locations",
        "/system/framework/$art_jar",
        "date_time_boot_class_path=\"$boot_class_path:$work/date-time-probe/probe.jar\"",
        "date_time_boot_class_path_locations=\"$boot_class_path_locations:/system/framework/probe.jar\"",
        "-Ximage-compiler-option --no-generate-debug-info",
        "-Ximage-compiler-option --no-generate-mini-debug-info",
    ] {
        assert!(
            generator.contains(needle),
            "framework verification lost its warning-free ART probe fragment {needle:?}"
        );
    }
    assert!(
        !generator.contains("        --release \\")
            && !generator.contains("-cp \"$work/date-time-probe/probe.jar\""),
        "the old ART probe must retain compatible D8 output and avoid a separately compiled app \
         dex path"
    );
}

#[test]
fn framework_overlay_preserves_webview_back_history_contract() {
    let generator = include_str!("../tools/framework-overlay/patch-framework.sh");
    for needle in [
        ".method public canGoBack()Z",
        "->native_canGoBack(J)Z",
        ".method public goBack()V",
        "->native_goBack(J)V",
    ] {
        assert!(
            generator.contains(needle),
            "framework overlay lost WebView history contract fragment {needle:?}; Roblox Back would throw or remain inside Chromium"
        );
    }
}

#[test]
fn framework_overlay_avoids_caught_wolfssl_null_store_signal() {
    let generator = include_str!("../tools/framework-overlay/patch-framework.sh");
    for needle in [
        "WolfSSLKeyX509.smali",
        "if-nez v2, :eclipse_wolf_has_key_store",
        ":eclipse_wolf_has_key_store",
        "built wolfssljni-hostdex.jar does not guard its nullable key store",
    ] {
        assert!(
            generator.contains(needle),
            "framework overlay lost the WolfSSL nullable-key-store guard fragment {needle:?}; ART would again implement the caught null dereference as SIGSEGV"
        );
    }
}

#[test]
fn framework_overlay_preserves_activity_manager_memory_contract() {
    let source = include_str!("../tools/framework-overlay/src/android/app/ActivityManager.java");
    let source_without_whitespace: String = source
        .chars()
        .filter(|character| !character.is_ascii_whitespace())
        .collect();
    for needle in [
        "private static native void native_fillMemoryInfo(MemoryInfo outInfo);",
        "native_fillMemoryInfo(outInfo);",
        "private static native int native_getMemoryClass();",
        "public int getMemoryClass() {return native_getMemoryClass();}",
        "private static native int native_getLargeMemoryClass();",
        "public int getLargeMemoryClass() {return native_getLargeMemoryClass();}",
        "private static native boolean native_isLowRamDevice();",
        "public boolean isLowRamDevice() {return native_isLowRamDevice();}",
    ] {
        let needle_without_whitespace: String = needle
            .chars()
            .filter(|character| !character.is_ascii_whitespace())
            .collect();
        assert!(
            source_without_whitespace.contains(&needle_without_whitespace),
            "framework overlay lost ActivityManager memory contract fragment {needle:?}"
        );
    }
    assert!(
        !source.contains("outInfo = new MemoryInfo();"),
        "getMemoryInfo again reassigns only its local parameter instead of filling the caller"
    );

    let memory_info = source
        .split_once("public static class MemoryInfo")
        .map(|(_, tail)| tail)
        .expect("ActivityManager overlay must define MemoryInfo");
    let mut remaining = memory_info;
    for field_write in [
        "dest.writeLong(availMem);",
        "dest.writeLong(totalMem);",
        "dest.writeLong(threshold);",
        "dest.writeInt(lowMemory ? 1 : 0);",
        "dest.writeLong(hiddenAppThreshold);",
        "dest.writeLong(secondaryServerThreshold);",
        "dest.writeLong(visibleAppThreshold);",
        "dest.writeLong(foregroundAppThreshold);",
    ] {
        let (_, tail) = remaining.split_once(field_write).unwrap_or_else(|| {
            panic!("MemoryInfo parcel lost or reordered AOSP field write {field_write:?}")
        });
        remaining = tail;
    }
}

#[test]
fn framework_overlay_answers_activity_permission_requests() {
    let generator = include_str!("../tools/framework-overlay/patch-framework.sh");
    for needle in [
        "invoke-static {p0, p1, p2}, Landroid/app/PermissionResultDelivery;->post(Landroid/app/Activity;[Ljava/lang/String;I)V",
        ".method public onRequestPermissionsResult(I[Ljava/lang/String;[I)V",
        "smali/android/app/PermissionResultDelivery.smali",
    ] {
        assert!(
            generator.contains(needle),
            "framework overlay lost Activity permission-request fragment {needle:?}; Roblox's \
             permission requests would never be answered"
        );
    }

    let root = PathBuf::from(env!("CARGO_MANIFEST_DIR"));
    let delivery_path =
        root.join("tools/framework-overlay/smali/android/app/PermissionResultDelivery.smali");
    let delivery = std::fs::read_to_string(&delivery_path)
        .unwrap_or_else(|error| panic!("cannot read {}: {error}", delivery_path.display()));
    for needle in [
        ".implements Ljava/lang/Runnable;",
        "Ljava/lang/IllegalArgumentException;",
        "Landroid/app/Activity;->checkSelfPermission(Ljava/lang/String;)I",
        "Landroid/os/Looper;->getMainLooper()Landroid/os/Looper;",
        "Landroid/os/Handler;->post(Ljava/lang/Runnable;)Z",
        "Landroid/app/Activity;->onRequestPermissionsResult(I[Ljava/lang/String;[I)V",
    ] {
        assert!(
            delivery.contains(needle),
            "PermissionResultDelivery lost fragment {needle:?}; requestPermissions would no \
             longer answer on the main looper with the current grant results"
        );
    }
}

#[test]
fn framework_overlay_ships_tzdata_and_the_host_default_time_zone() {
    let generator = include_str!("../tools/framework-overlay/patch-framework.sh");
    for needle in [
        "TzdataCompactor \"$ZONEINFO_DIR\" \"$work/art/zoneinfo/tzdata\"",
        "cp \"$work/art/zoneinfo/tzdata\" \"$OUT/art/zoneinfo/tzdata\"",
        "const-string v1, \"eclipse.zoneinfo_dir\"",
        "Lorg/apache/harmony/luni/internal/util/UserTimezoneGetter;-><init>()V",
        "-Declipse.zoneinfo_dir=\"$work/art/zoneinfo\"",
        "-Duser.timezone=America/Los_Angeles",
        "time-zone-ok",
    ] {
        assert!(
            generator.contains(needle),
            "framework overlay lost time-zone fragment {needle:?}; Java would again report GMT \
             for every zone"
        );
    }
    let getter = include_str!(
        "../tools/framework-overlay/smali/org/apache/harmony/luni/internal/util/UserTimezoneGetter.smali"
    );
    assert!(
        getter.contains("const-string v0, \"user.timezone\""),
        "the libcore TimezoneGetter must answer with the user.timezone Eclipse publishes"
    );
}

#[test]
fn input_test_delivers_ident_then_looper_wake() {
    let out = run_eclipse("__input-test", &[], DIAGNOSTIC_LIMIT);
    let text = combined(&out);

    assert!(
        out.status.success(),
        "__input-test exited non-zero ({:?}); ALooper input-path regression.\n{text}",
        out.status.code()
    );

    assert!(
        text.contains("input path OK:")
            && text.contains("pollOnce returned ident 11")
            && text.contains("parked pollOnce returned ALOOPER_POLL_WAKE"),
        "missing the ALooper poll/wake success marker (input-path regression?).\n{text}"
    );
}
