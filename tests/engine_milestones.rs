use std::path::PathBuf;
use std::process::{Command, Output};

fn roblox_apk_present() -> bool {
    if let Some(p) = std::env::var_os("ECLIPSE_ROBLOX_APK") {
        if PathBuf::from(p).exists() {
            return true;
        }
    }
    if let Some(home) = std::env::var_os("HOME") {
        let default =
            PathBuf::from(home).join("eclipse-m0/apk/v2.724.735/roblox-2.724.735-merged.apk");
        if default.exists() {
            return true;
        }
    }
    false
}

fn display_available() -> bool {
    std::env::var_os("WAYLAND_DISPLAY").is_some() || std::env::var_os("DISPLAY").is_some()
}

fn run_eclipse(subcommand: &str) -> Output {
    Command::new(env!("CARGO_BIN_EXE_eclipse"))
        .arg(subcommand)
        .output()
        .unwrap_or_else(|e| panic!("failed to spawn eclipse {subcommand}: {e}"))
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

#[test]
fn run_libroblox_init_runs_all_3427_constructors() {
    if !roblox_apk_present() {
        eprintln!(
            "SKIP: Roblox APK absent (set ECLIPSE_ROBLOX_APK or place it at \
             $HOME/eclipse-m0/apk/v2.724.735/roblox-2.724.735-merged.apk)"
        );
        return;
    }

    let out = run_eclipse("__run-libroblox-init");
    let text = combined(&out);

    assert!(
        out.status.success(),
        "__run-libroblox-init exited non-zero ({:?}); a constructor likely faulted.\n{text}",
        out.status.code()
    );
    assert!(
        text.contains("ALL 3427/3427 constructors completed without a crash"),
        "missing the 'ALL 3427/3427 constructors completed' marker — fewer than 3,427 constructors \
         ran (loader/relocation/resolve regression or a constructor crash?).\n{text}"
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

    let out = run_eclipse("__gl-test");
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

    let out = run_eclipse("__gl-test-anw");
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
            "SKIP: Roblox APK absent (set ECLIPSE_ROBLOX_APK or place it at \
             $HOME/eclipse-m0/apk/v2.724.735/roblox-2.724.735-merged.apk)"
        );
        return;
    }
    if !display_available() {
        eprintln!(
            "SKIP: no display server (WAYLAND_DISPLAY/DISPLAY unset) — the CEF helper needs one"
        );
        return;
    }

    let out = run_eclipse("__webview-test");
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
        text.contains("bound=5"),
        "the WebView native registration count regressed (expected the live bound=5 line — the M4 \
         evaluateJavascript + addJavascriptInterface natives).\n{text}"
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
fn framework_overlay_preserves_activity_manager_memory_contract() {
    let source = include_str!("../tools/framework-overlay/src/android/app/ActivityManager.java");
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
        assert!(
            source.contains(needle),
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
fn input_test_delivers_ident_then_looper_wake() {
    let out = run_eclipse("__input-test");
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
