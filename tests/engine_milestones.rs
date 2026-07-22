//! Gated regression guards for the dev-host-validated engine-load milestones (2026-06-05).
//!
//! Each milestone is a hidden `eclipse` subcommand that prints a deterministic SUCCESS marker
//! (see `src/main.rs`). These integration tests run the built binary via
//! `env!("CARGO_BIN_EXE_eclipse")` (cargo builds + locates it) and assert the REAL marker string
//! the harness emits, so a regression in the loader / render / input path makes the test FAIL.
//!
//! Each test SKIPS CLEANLY (prints `SKIP: <reason>` and returns) when its precondition is absent —
//! the Roblox APK missing, or no display server for the GL tests — so the suite never fails
//! spuriously in a headless/CI environment. None of the assertions is weakened to a trivial
//! always-pass: each requires the harness's exact success marker AND a success exit status.

use std::path::PathBuf;
use std::process::{Command, Output};

/// The dev-host Roblox APK the init milestone needs. Mirrors `init_run::find_roblox_apk`
/// (kept in sync, 2026-06-05): the `ECLIPSE_ROBLOX_APK` env override, then the default
/// `$HOME/eclipse-m0/apk/v2.724.735/roblox-2.724.735-merged.apk`. The test SKIPS if neither exists,
/// so it never fails on a machine without the (large, un-committable) APK.
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

/// True when a display server is reachable (the GL milestones need one). A Wayland or X11
/// display env is the portable signal; with neither set the host is headless and the GL tests
/// SKIP rather than fail. (When a display IS advertised but the harness still cannot create the
/// event loop / EGL display — an environment limitation, not a code regression — the GL tests
/// also skip; see `gl_env_unavailable`.)
fn display_available() -> bool {
    std::env::var_os("WAYLAND_DISPLAY").is_some() || std::env::var_os("DISPLAY").is_some()
}

/// Run the built `eclipse` binary with one subcommand, capturing stdout+stderr+status.
/// `CARGO_BIN_EXE_eclipse` makes cargo build the binary and hands us its absolute path, so the
/// test exercises the REAL shipped entry point (no path assumptions, portable across machines).
fn run_eclipse(subcommand: &str) -> Output {
    Command::new(env!("CARGO_BIN_EXE_eclipse"))
        .arg(subcommand)
        .output()
        .unwrap_or_else(|e| panic!("failed to spawn eclipse {subcommand}: {e}"))
}

/// Combined stdout+stderr of an `Output` as a String (the markers land on either stream).
fn combined(out: &Output) -> String {
    let mut s = String::from_utf8_lossy(&out.stdout).into_owned();
    s.push_str(&String::from_utf8_lossy(&out.stderr));
    s
}

/// True when the harness output shows the host could not bring up a display/EGL surface — a
/// dev-host/CI environment limitation (no compositor, no Mesa), NOT a render-path regression. The
/// GL harnesses report this as an `EglError::Display(...)` whose message names the winit event
/// loop or an EGL display/context failure (see `src/egl_engine.rs`). When this is what happened
/// the GL test SKIPS; otherwise (display came up but the marker is wrong) it FAILS.
fn gl_env_unavailable(output: &str) -> bool {
    output.contains("winit event loop")
        || output.contains("EGL_NO_DISPLAY")
        || output.contains("eglInitialize")
        || output.contains("no available configs")
}

/// Guards `eclipse __run-libroblox-init` (2026-06-05): map + relocate + fully-resolve libroblox.so
/// under Eclipse's own Rust loader and run ALL 3,427 DT_INIT_ARRAY constructors in order on the main
/// thread. A regression in the loader/relocation/resolve pipeline (or a constructor crash) drops the
/// completed count below 3,427 or flips the exit status — this test fails then. SKIPS when the
/// Roblox APK is absent (it streams ~111 MiB from a 224 MiB APK that cannot be committed).
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

    // 2026-06-05: on FULL success the harness calls `libc::_exit(0)` from inside
    // `run_libroblox_init` itself (init_run.rs:333 — it must NOT return through `main` and run
    // teardown that would fault the engine's still-live worker threads / exit-time finalizers). So
    // the success signature is: exit status 0, AND the authoritative stderr marker
    // "ALL 3427/3427 constructors completed without a crash" (init_run.rs:308 — emitted ONLY after
    // every one of the 3,427 constructors ran). The main.rs stdout line is intentionally unreachable
    // on success (the `_exit(0)` precedes it), so it is NOT asserted here. A constructor crash makes
    // the signal handler `_exit` NON-ZERO without printing that marker → the test fails.
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

/// Guards `eclipse __gl-test` (2026-06-05): the engine GLES2/EGL render path on an Eclipse window —
/// build the surface, render + present 5 real triangle frames, assert 0 GL/EGL errors and every
/// swap succeeded. A regression in the EGL/GLESv2 render path makes the marker absent → fail. SKIPS
/// on a headless host (no display) or when the host cannot bring up the display/EGL (env limitation).
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
    // Display marker (egl_engine::GlTestReport): "EGL+GLES2 OK: surface WxH, N frames rendered +
    // presented, 0 GL errors, all swaps succeeded".
    assert!(
        text.contains("EGL+GLES2 OK:") && text.contains("0 GL errors, all swaps succeeded"),
        "missing the engine GLES2/EGL success marker (render-path regression?).\n{text}"
    );
}

/// Guards `eclipse __gl-test-anw` (2026-06-05): the ENGINE render WSI bind — obtain an
/// `ANativeWindow*` via the bound `ANativeWindow_fromSurface` native, then drive HOST
/// `eglCreateWindowSurface(ANativeWindow)` + make-current + a real triangle + swap, asserting the
/// `ANativeWindow*` IS the real WSI handle, 0 GL errors, swaps OK. A regression in the WSI bind makes
/// the marker absent or flips `= true` to `= false` → fail. SKIPS headless / when EGL is unavailable.
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
    // Display marker (egl_engine::GlAnwTestReport): "... ANativeWindow* is the real WSI handle = true,
    // 0 GL errors, all swaps succeeded". The `= true` is load-bearing — `= false` is the
    // geometry-only fallback (a WSI-bind regression), so we require `true` explicitly.
    assert!(
        text.contains("ANativeWindow* is the real WSI handle = true")
            && text.contains("0 GL errors, all swaps succeeded"),
        "missing the engine-style eglCreateWindowSurface(ANativeWindow) success marker, or the \
         ANativeWindow* was NOT the real WSI handle (WSI-bind regression?).\n{text}"
    );
}

/// Guards `eclipse __webview-test` (2026-07-03, web-engine plan M3): the WebView engine pipeline —
/// boot ART (framework classpath), drive the installed-dex `WebView.loadUrl` through the
/// registered spawn-and-forward native into the out-of-process `eclipse-webview` helper, and
/// require BOTH real `internalLoadChanged` upcalls (0 and 3), a nonzero-ink staged frame in the
/// main process, a clean `ViewClosed`, and helper exit 0 — plus the live `bound=3` WebView
/// registration line (previously only live-boot-pinned). SKIPS when the Roblox APK or a display
/// server is absent, and when the helper/CEF payload is unavailable on the host (the two
/// actionable-error substrings are pinned to the client's own `pub const` markers so the skip
/// condition and the error text can never drift apart) — never a generic-failure skip.
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
            // 2026-07-10 (plan M5): a host genuinely lacking BOTH sandbox tiers (unprivileged
            // userns AND a SUID chrome-sandbox) with the config opt-in off is an env
            // limitation like no-display — the helper's refusal is the DESIGNED outcome.
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
    // Display marker (main.rs::WebViewTestReport): "WebView engine pipeline OK:
    // internalLoadChanged upcalls 2/2 (state 0 @ Xms, state 3 @ Yms, http Z), frame WxH N
    // distinct pixels, bridge round-trip OK, evaluateJavascript OK, honest UA OK, cookie set/get
    // OK, cookie callback OK, cookie flush OK, ViewClosed, helper exit 0, bound=5". "upcalls 2/2" is load-bearing
    // (SUCCESSFUL Java dispatches of the internalLoadChanged seam); the M4 substrings prove the JS
    // bridge round-trip, evaluateJavascript result routing, the honest UA, and cookie set/get + the
    // real (non-fabricated) 3-arg setCookie callback. The marker is payload-free by construction.
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
    // 2026-07-10 (plan M5): the four detect-don't-assume detection lines (stable prefixes only —
    // the values are session-dependent: wayland vs x11, userns/suid/DEGRADED, resolved counts,
    // gpu vs SwiftShader). Three are the helper's own stderr lines, one is the client's
    // pre-spawn host-lib probe. The SUCCESS marker string itself is unchanged from M4.
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

/// 2026-07-03 (web-engine plan M3): the root crate must stay cef-free — the CEF engine lives ONLY
/// in the workspace-detached `crates/eclipse-webview` helper, and the main (ART) process speaks
/// nothing but the owned std-only IPC protocol in `src/webview/`. This automates the M2-manual
/// `Cargo.lock` check; it always runs (no precondition), so a dependency edit that drags cef into
/// the root lockfile fails the plain `cargo test` gate.
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

/// 2026-07-17: the current client calls Android's API-level-1
/// `LocationManager.isProviderEnabled(String)` from three SDK paths. ATL's installed class omits it,
/// and Backtrace's watchdog lets the resulting `NoSuchMethodError` reach Roblox's process-fatal
/// uncaught handler (`System.exit(10)`) before login becomes interactive. The overlay generator is
/// the durable source of the Java-level fix, so pin all three contract pieces without requiring a
/// local ART/toolchain install: method present, null rejected like AOSP, and ATL's advertised-empty
/// provider set answered disabled rather than fabricating GPS support.
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

/// 2026-07-18: ATL's `ActivityManager.getMemoryInfo(out)` reassigned only its local parameter,
/// leaving the app's object at a 10,000-byte placeholder and producing the measured `0MB` device
/// profile. Pin the committed Java half of the repair independently from JNI's Rust constants:
/// the caller object must be filled, both heap classes must come from Rust, and Android 13's eight
/// `MemoryInfo` fields must remain in AOSP parcel order. This test needs no ART/JDK/host capability.
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

/// Guards `eclipse __input-test` (2026-06-05): the REAL `ALooper` input path — register a synthetic
/// engine input fd, park in `pollOnce`, inject the fd signal (expect the registered ident), then park
/// again and inject a host-input wake (expect `ALOOPER_POLL_WAKE`). A regression in the looper
/// poll/wake feed makes the marker absent → fail. GPU/VM-free with no external precondition, so it
/// runs everywhere (no skip path).
#[test]
fn input_test_delivers_ident_then_looper_wake() {
    let out = run_eclipse("__input-test");
    let text = combined(&out);

    assert!(
        out.status.success(),
        "__input-test exited non-zero ({:?}); ALooper input-path regression.\n{text}",
        out.status.code()
    );
    // Marker (native_provider::run_input_test): "input path OK: registered fd → pollOnce returned
    // ident 11 ...; host-input wake → parked pollOnce returned ALOOPER_POLL_WAKE; N looper(s) woken".
    assert!(
        text.contains("input path OK:")
            && text.contains("pollOnce returned ident 11")
            && text.contains("parked pollOnce returned ALOOPER_POLL_WAKE"),
        "missing the ALooper poll/wake success marker (input-path regression?).\n{text}"
    );
}
