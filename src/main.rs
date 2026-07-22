//! Eclipse launcher entry point.
//!
//! Thin launcher CLI: `run` parses the APK, builds the ART [`BootPlan`](eclipse::runtime),
//! boots the vendored ART VM (Roblox's Java on the classpath) and opens the host window;
//! `config` shows the effective configuration. The framework that drives the Activity to
//! `onCreate` and renders the engine is the next phase (see `docs/`).

use std::process::ExitCode;

const HELP: &str = "\
eclipse — run the Android Roblox build on Linux (open-source, Rust)

USAGE:
    eclipse <COMMAND>

COMMANDS:
    run [APK]  Parse the APK, boot the ART VM (Roblox on the classpath), open the window.
               With no APK and `auto_fetch_missing`+`apk_url` set (or ECLIPSE_APK_URL),
               auto-downloads from your configured source first.
    fetch      Report the latest upstream Roblox version + download the APK from your
               configured source (config `apk_url` / ECLIPSE_APK_URL) into the cache.
    config     Show effective configuration and its path
    help       Show this help
    --version  Show version

NOTE: Eclipse never hosts or hard-codes a Roblox APK source. You supply your own APK (path
    or a download URL you configure); auto-fetch is opt-in. Eclipse does not redistribute Roblox.

STATUS:
    `run` parses the manifest, prints the ART boot plan, boots the vendored ART VM with
    Roblox's Java on the classpath, then opens the host game window (winit, no GTK). The
    framework that drives the launcher Activity to onCreate and renders the engine into the
    window is the next phase (component-map F). See docs/.
";

fn main() -> ExitCode {
    let args: Vec<String> = std::env::args().skip(1).collect();
    if matches!(
        args.first().map(String::as_str),
        Some("run") | Some("__webview-test")
    ) {
        // 2026-07-17: do this before diagnostics or the WebView smoke's loopback server can create
        // any thread. A self-contained patched ART overlay must set one immutable BOOTCLASSPATH so
        // ATL's separately exec'd dex2oat children reopen the same patched jars as the parent VM.
        if let Err(error) = eclipse::runtime::prepare_art_boot_environment() {
            eprintln!("eclipse ART startup: {error}");
            return ExitCode::FAILURE;
        }
    }

    eclipse::diagnostics::init();

    tracing::debug!(
        version = eclipse::VERSION,
        command = args.first(),
        "eclipse starting"
    );
    match args.first().map(String::as_str) {
        Some("--version") | Some("-V") => {
            println!("eclipse {}", eclipse::VERSION);
            ExitCode::SUCCESS
        }
        Some("run") => {
            let status = match run_apk(args.get(1).map(String::as_str)) {
                Ok(()) => 0,
                Err(e) => {
                    eprintln!("eclipse run: {e}");
                    1
                }
            };
            finish_android_process(status)
        }
        Some("config") => match show_config() {
            Ok(()) => ExitCode::SUCCESS,
            Err(e) => {
                eprintln!("eclipse config: {e}");
                ExitCode::FAILURE
            }
        },
        Some("fetch") => match fetch_apk_command() {
            Ok(()) => ExitCode::SUCCESS,
            Err(e) => {
                eprintln!("eclipse fetch: {e}");
                ExitCode::FAILURE
            }
        },
        // Hidden dev-host diagnostic (NOT in HELP): map+relocate+fully-resolve libroblox.so and run
        // its DT_INIT_ARRAY constructors in order on this (main) thread. A crash is the EXPECTED,
        // VALUABLE result (host-glibc libc baseline is not bionic-ABI-correct); the harness's own
        // signal handler logs the faulting constructor + exits non-zero. See src/loader/init_run.rs.
        Some("__run-libroblox-init") => match eclipse::loader::init_run::run_libroblox_init() {
            Ok(completed) => {
                println!("__run-libroblox-init: {completed} constructor(s) completed");
                ExitCode::SUCCESS
            }
            Err(e) => {
                eprintln!("__run-libroblox-init: {e}");
                ExitCode::FAILURE
            }
        },
        // Hidden dev-host diagnostic (NOT in HELP): build the ENGINE's GLES2/EGL render surface on an
        // Eclipse window via host EGL/GLESv2, render + present a real triangle for a few frames, and
        // assert 0 GL/EGL errors + successful swaps. Proves the engine GL render path on Eclipse's
        // window independent of Roblox (the render path for when the boot clears the native-load
        // wall). Needs a display server + GL (dev host); see src/egl_engine.rs.
        Some("__gl-test") => match eclipse::egl_engine::run_gl_test() {
            Ok(report) => {
                println!("__gl-test: {report}");
                ExitCode::SUCCESS
            }
            Err(e) => {
                eprintln!("__gl-test: {e}");
                ExitCode::FAILURE
            }
        },
        // Hidden dev-host diagnostic (NOT in HELP): the engine render WSI bind. Go through the
        // engine's exact path — obtain an ANativeWindow* via the bound ANativeWindow_fromSurface
        // native, then drive HOST eglCreateWindowSurface(display, config, the ANativeWindow as
        // EGLNativeWindowType, null) + make-current + a real triangle + eglSwapBuffers, asserting the
        // ANativeWindow* is the real WSI handle, surface creation succeeds, 0 GL errors, swaps OK.
        // Proves the engine's own eglCreateWindowSurface(ANativeWindow) presents to Eclipse's window.
        // Needs a display server + GL (dev host); see src/egl_engine.rs::run_gl_test_anw.
        Some("__gl-test-anw") => match eclipse::egl_engine::run_gl_test_anw() {
            Ok(report) => {
                println!("__gl-test-anw: {report}");
                ExitCode::SUCCESS
            }
            Err(e) => {
                eprintln!("__gl-test-anw: {e}");
                ExitCode::FAILURE
            }
        },
        // Hidden dev-host diagnostic (NOT in HELP): drive the REAL ALooper input path end-to-end
        // without the boot reaching the engine's input loop — prepare a looper, register a synthetic
        // engine input fd, inject a synthetic input (fd signal) + a host-input wake, and assert
        // ALooper_pollOnce delivers the registered ident then ALOOPER_POLL_WAKE. Proves the
        // winit-input → looper feed unblocks a parked engine poll. GPU/VM-free. See
        // src/loader/native_provider.rs::run_input_test.
        Some("__input-test") => match eclipse::loader::native_provider::run_input_test() {
            Ok(report) => {
                println!("__input-test: {report}");
                ExitCode::SUCCESS
            }
            Err(e) => {
                eprintln!("__input-test: {e}");
                ExitCode::FAILURE
            }
        },
        // Hidden dev-host diagnostic (NOT in HELP, 2026-07-03 web-engine plan M3): the FIRST
        // ART-booting hidden subcommand — boot ART with the installed framework on the classpath
        // (main thread, like `run`; NO libroblox preload, NO app lifecycle, NO window), then drive
        // the WebView engine pipeline end-to-end against the public https://www.roblox.com page:
        // installed-dex Java loadUrl → registered native → spawn-and-forward → eclipse-webview
        // helper → LoadState over the socket → REAL internalLoadChanged(0/3) JNI upcalls
        // (WebViewClient.onPageStarted/onPageFinished) → memfd frame staged in THIS process →
        // CloseView/Shutdown with a clean helper reap. Prints a deterministic SUCCESS marker that
        // tests/engine_milestones.rs guards. See src/webview/client.rs + framework::drive_webview_smoke.
        Some("__webview-test") => match run_webview_test() {
            Ok(report) => {
                println!("__webview-test: {report}");
                ExitCode::SUCCESS
            }
            Err(e) => {
                eprintln!("__webview-test: {e}");
                ExitCode::FAILURE
            }
        },
        // Hidden dev-host diagnostic (NOT in HELP): drive the REAL OpenSL ES audio path end-to-end —
        // slCreateEngine → Realize → CreateOutputMix → CreateAudioPlayer (buffer-queue PCM source) →
        // SetPlayState(PLAYING) → Enqueue a generated 440 Hz sine PCM buffer, then confirm the cpal
        // host stream drained it + the buffer-queue callback fired with 0 SL errors. On a headless
        // host with no audio device it SKIPs cleanly (the full path is still built + PCM enqueued).
        // See src/loader/opensl.rs::run_audio_test.
        Some("__audio-test") => match eclipse::loader::opensl::run_audio_test() {
            Ok(report) => {
                println!("__audio-test: {report}");
                ExitCode::SUCCESS
            }
            Err(e) => {
                eprintln!("__audio-test: {e}");
                ExitCode::FAILURE
            }
        },
        None | Some("help") | Some("--help") | Some("-h") => {
            print!("{HELP}");
            ExitCode::SUCCESS
        }
        Some(other) => {
            eprintln!("unknown command: {other}\n\n{HELP}");
            ExitCode::FAILURE
        }
    }
}

/// Finish the process that hosted Android without running host C/C++ exit handlers.
///
/// Android's process model does not unload an app's initialized native libraries or run their
/// global destructors after `Activity.onDestroy`; the OS reclaims the process. Roblox likewise
/// leaves native and ART workers alive after its blocking destroy returns. On 2026-07-17 a normal
/// libc `exit` called a `libroblox.so` `atexit` handler after that completed destroy, and the foreign
/// handler deliberately aborted. Eclipse therefore completes every owned boundary first (Android
/// lifecycle, render surface, WebView helper), flushes its Rust output, and uses POSIX `_exit` as
/// the final process boundary. The libc wrapper terminates every thread and skips `atexit` handlers.
fn finish_android_process(status: libc::c_int) -> ! {
    use std::io::Write as _;

    let _ = std::io::stdout().flush();
    let _ = std::io::stderr().flush();

    // SAFETY: this is the launcher main thread's terminal action after `run_apk` has either
    // completed all owned shutdown boundaries or reported its setup error. No Rust or foreign
    // state is accessed afterward; the process and all its threads must terminate together.
    unsafe { libc::_exit(status) }
}

/// Print the effective configuration (file values merged over defaults) and its path.
fn show_config() -> Result<(), eclipse::config::ConfigError> {
    let path = eclipse::config::Config::config_path()?;
    let config = eclipse::config::Config::load()?;
    println!("# {}", path.display());
    println!("{}", config.to_json_pretty()?);
    Ok(())
}

/// The configured APK download URL: `ECLIPSE_APK_URL` (env) wins over `config.apk_url`. `None` = none set.
fn configured_apk_url(config: &eclipse::config::Config) -> Option<String> {
    std::env::var("ECLIPSE_APK_URL")
        .ok()
        .filter(|s| !s.is_empty())
        .or_else(|| config.apk_url.clone())
}

/// `eclipse fetch`: report the latest upstream Roblox version (official oracle) and download the APK from
/// the user-configured source into the cache (verifying `apk_sha256` if set). Eclipse never hard-codes a
/// source — a URL must be configured (`config.apk_url` / `ECLIPSE_APK_URL`).
fn fetch_apk_command() -> Result<(), Box<dyn std::error::Error>> {
    match eclipse::apk::fetch::latest_roblox_version() {
        Ok(v) => {
            let android_major = v.split('.').nth(1).unwrap_or("?");
            println!(
                "# Latest upstream Roblox version (oracle): {v}  (≈ Android 2.{android_major}.x)"
            );
        }
        Err(e) => eprintln!("# version oracle unavailable (non-fatal): {e}"),
    }
    let config = eclipse::config::Config::load()?;
    let url = configured_apk_url(&config).ok_or(
        "no APK source configured — set config `apk_url` or ECLIPSE_APK_URL (Eclipse never hard-codes one)",
    )?;
    println!("# Fetching APK from your configured source: {url}");
    let path = eclipse::apk::fetch::fetch_apk(&url, config.apk_sha256.as_deref())?;
    println!("fetched APK: {} ✓", path.display());
    Ok(())
}

/// `eclipse run <APK>`: open the APK, parse its manifest, build the ART
/// [`BootPlan`](eclipse::runtime::BootPlan) from the manifest + effective config, print the
/// plan and the options it implies, then boot the ART VM from this (main) thread. Today this
/// brings up a libcore VM; reaching Roblox's onCreate is the next step.
///
/// Returns `Box<dyn Error>` because this `main`/setup-layer code composes several typed
/// library errors (APK, config, runtime); the library crates themselves stay strictly typed (§2.8).
fn run_apk(apk_path: Option<&str>) -> Result<(), Box<dyn std::error::Error>> {
    // Resolve the APK: an explicit path wins; otherwise opt-in auto-fetch from the user-configured
    // source (only when `auto_fetch_missing` is on, or `ECLIPSE_APK_URL` is explicitly set). Eclipse
    // never hard-codes a source and never auto-fetches silently without that opt-in.
    let resolved: String = match apk_path {
        Some(p) => p.to_string(),
        None => {
            let config = eclipse::config::Config::load()?;
            let env_url = std::env::var_os("ECLIPSE_APK_URL").is_some();
            match configured_apk_url(&config) {
                Some(url) if config.auto_fetch_missing || env_url => {
                    println!("# No APK supplied — auto-fetching from your configured source: {url}");
                    let path = eclipse::apk::fetch::fetch_apk(&url, config.apk_sha256.as_deref())?;
                    println!("fetched APK: {} ✓", path.display());
                    path.to_string_lossy().into_owned()
                }
                _ => {
                    return Err("missing APK path (usage: eclipse run <APK>); or set config `apk_url` + \
                                `auto_fetch_missing` (or ECLIPSE_APK_URL) to auto-download — `eclipse fetch`"
                        .into())
                }
            }
        }
    };
    let apk_path = resolved.as_str();

    let mut apk = eclipse::apk::Apk::open(std::path::Path::new(apk_path))?;
    // 2026-06-05: configure the asset source for Eclipse's ndk-android natives (libandroid). The
    // engine's `AAssetManager_fromJava`/`AAssetManager_open` serve `assets/*` from this APK via
    // Eclipse's own `src/apk` reader (set once; idempotent — no-op if a later call repeats it).
    eclipse::loader::ndk_registry::set_apk_path(std::path::PathBuf::from(apk_path));
    let manifest = apk.manifest()?;
    let config = eclipse::config::Config::load()?;
    let plan = eclipse::runtime::BootPlan::new(&manifest, &config);

    println!("# ART boot plan (dry run) for {apk_path}");
    println!("package:            {}", manifest.package);
    println!("launcher_activity:  {}", plan.launcher_activity);
    println!("sdk_int:            {}", plan.sdk_int);
    println!(
        "heap:               {} MiB (DisableHSpaceCompactForOOM={})",
        plan.heap_mib, plan.disable_hspace_compact
    );
    println!("graphics_backend:   {}", plan.graphics_backend.as_str());
    println!("instruction_set:    {}", plan.instruction_set_features);
    // Two distinct destinations: VM options go to JNI_CreateJavaVM, the ISA flag to dex2oat.
    println!("\n# VM options (-> JNI_CreateJavaVM):");
    for opt in plan.vm_options() {
        println!("    {opt}");
    }
    println!("# dex2oat options (-> dex2oat AOT compiler):");
    for opt in plan.dex2oat_options() {
        println!("    {opt}");
    }

    // Extract the app's x86_64 native libs (incl. libroblox.so) to the XDG cache dir before boot
    // so `System.loadLibrary("roblox")` can find the engine via java.library.path. The extractor
    // streams (~119 MB) and is idempotent (skips already-extracted files), so repeat boots are
    // cheap. The dir is appended after the framework natives dir on java.library.path in boot().
    let app_lib_dir = eclipse::runtime::native_lib_cache_dir()?;
    println!(
        "\n# Extracting native libs (lib/x86_64/) to {}…",
        app_lib_dir.display()
    );
    let extracted = apk.extract_native_libs("x86_64", &app_lib_dir)?;
    println!("extracted {} native lib(s) ✓", extracted.len());

    // 2026-06-13: extract the APK's bundled `assets/` tree to the engine's content root so the
    // engine can open its shader packs (and fonts/content) from the FILESYSTEM. The engine reads
    // `<app_data_dir>/files/assets/shaders/shaders_*.pack` directly (not via the JNI AssetManager);
    // without this, `app_data_dir/files/assets/` lacks `shaders/` and the engine's SurfaceController
    // fails `Mode 4 ... Error opening shader pack` → `RenderView is NULL` → no frames. The dest is
    // derived from the SAME `framework::app_data_dir()` that `native_get_app_data_dir` returns (one
    // source of truth — the path can't drift from what the engine reads). The extractor streams and
    // is idempotent (skips already-extracted files), so repeat boots don't rewrite ~105 MB. A shader
    // pack that fails to extract means no rendering, so this is fatal (the `?` propagates).
    let assets_dir = eclipse::framework::app_data_dir()
        .ok_or_else(|| {
            std::io::Error::new(
                std::io::ErrorKind::NotFound,
                "cannot resolve the app data directory (no $HOME/XDG base and ECLIPSE_APP_DATA_DIR \
                 unset); set ECLIPSE_APP_DATA_DIR to the engine content root",
            )
        })?
        .join("files")
        .join("assets");
    println!(
        "\n# Extracting Roblox bundled assets (assets/ → files/assets/) to {}…",
        assets_dir.display()
    );
    let asset_count = apk.extract_assets(&assets_dir)?;
    println!("extracted {asset_count} asset file(s) ✓");

    // Boot the ART VM from this (main) thread — the production entry point — with the APK on the
    // classpath, so ART loads Roblox's Java (+ the android.* framework) alongside libcore, and the
    // extracted app-lib dir on java.library.path so System.loadLibrary finds libroblox.so.
    // Driving the launcher Activity to onCreate (the GTK-coupled framework / Eclipse's own
    // winit+Vulkan framework) is the next step. ART logs verbosely to stderr on first run
    // (dex2oat compiles the boot image once).
    println!("\n# Booting the ART VM with Roblox on the classpath…");
    // Bind the owned VM handle (never `let _`, which would drop it immediately) and keep it alive
    // across the winit event loop below: it is `!Send`/`!Sync`, pinning the VM to this main thread
    // so the next increment's JNI calls (driven from inside the event loop) have a reachable VM.
    let vm = eclipse::runtime::boot(
        &plan,
        Some(std::path::Path::new(apk_path)),
        Some(&app_lib_dir),
    )?;
    println!("ART VM booted with Roblox's Java on the classpath ✓");

    // Provision the bare Android sonames the bionic shim linker needs into the app-lib dir. The shim
    // linker resolves a NEEDED entry by searching its path for a file named exactly the bare soname
    // and mmap-parsing it as ELF. 2026-06-05: `libm.so` is provided by Eclipse's OWN apkenv-loadable
    // shim (a clean-relocation, correct-math cdylib copied to <dir>/libm.so), NOT a symlink to the
    // host glibc libm.so.6 — the host libm.so.6 carries an R_X86_64_TPOFF64 ("unknown reloc type 18")
    // + a .relr.dyn section the older apkenv linker cannot apply, so following zstd-jni's
    // `NEEDED libm.so` to it aborted (SIGSEGV) during Roblox's androidx.startup. Must run before the
    // whitelist + lifecycle.
    println!("# Provisioning bionic sonames (libm.so → Eclipse apkenv-loadable shim) …");
    eclipse::runtime::provision_bionic_sonames(&app_lib_dir)?;
    println!("bionic sonames provisioned (Eclipse libm shim) ✓");

    // Whitelist the framework natives dir + the extracted app-lib dir in the bionic shim linker's
    // own search path (apkenv_ldpaths[]) via libdl_bio's dl_parse_library_path. ART's
    // `-Djava.library.path` alone is NOT enough: the shim linker's apkenv_load_library consults its
    // own path array, so an un-whitelisted dir is rejected as "library not found" even when the
    // extracted .so exists at the absolute path the JVM passes. Must run AFTER boot (libart opened
    // RTLD_GLOBAL, so libdl_bio's symbol is global-scope) and BEFORE any System.loadLibrary the
    // framework lifecycle drives (e.g. Roblox's Application.onCreate loading libzstd-jni / libroblox).
    let fw = eclipse::runtime::find_framework()?;
    println!("# Whitelisting the app-lib dir in the bionic linker search path…");
    eclipse::runtime::whitelist_bionic_library_path(&fw, Some(&app_lib_dir))?;
    println!("bionic linker search path whitelisted (dl_parse_library_path) ✓");

    // 2026-06-12: register the framework natives an engine JNI_OnLoad reaches (Log + Process)
    // BEFORE the pre-load below runs that JNI_OnLoad — previously they were registered only inside
    // drive_application_lifecycle, so the engine's JNI_OnLoad-time Log call missed and its
    // LoggingProtocol warned `process timestamps will be inaccurate` for the whole boot.
    println!("# Registering engine-JNI_OnLoad-reachable framework natives (Log + Process)…");
    eclipse::framework::register_engine_preload_natives(&vm)?;
    println!("engine-preload framework natives registered ✓");

    // Route the app's x86_64 JNI libs through Eclipse's OWN Rust loader (NOT the apkenv shim linker,
    // which aborts on their modern relocs — R_X86_64_TPOFF64). Each is mapped + relocated + fully
    // resolved + init-run + (if it exports JNI_OnLoad) handed the REAL ART JavaVM. The returned
    // descriptors own process-lifetime/no-drop images: libroblox spawns workers that continue to
    // execute mapped text after Android's lifecycle has stopped.
    // Skipped entirely for APKs without lib/x86_64/libroblox.so (e.g. the pure-Java demo_app), so the
    // demo lifecycle path is unchanged. Runs on this (main) thread, VM alive + JNI-attached, BEFORE the
    // lifecycle drives Roblox's onCreate (where androidx.startup would call System.loadLibrary for
    // libroblox AND zstd-jni — both now already loaded here, so neither falls to apkenv).
    let _preloaded_libs =
        preload_app_native_libs(&mut apk, std::path::Path::new(apk_path), &app_lib_dir, &vm)?;

    // Drive the confirmed lifecycle recipe on this (main) thread, with the VM alive: wrap the held VM
    // with the `jni` crate, bind Eclipse's own non-GTK backing for the framework natives via
    // RegisterNatives, then drive recipe steps 1–7 — Context.createApplication → createContentProviders
    // → Application.onCreate → Activity.createMainActivity → Activity.onCreate → Activity.onStart →
    // Activity.onResume — to reach the launcher Activity's RESUMED state. The jlong window handle is an
    // Eclipse-owned window_registry index (sound, bounds+generation-checked — never a GtkWidget* cast).
    // Runs before the blocking event loop so the lifecycle is driven while still on the attached main
    // thread.
    println!("# Driving the framework lifecycle (JNI; steps 1–7 to Activity.onResume / RESUMED)…");
    let progress =
        eclipse::framework::drive_application_lifecycle(&vm, apk_path, &plan.launcher_activity)?;
    println!("framework lifecycle driven: {progress:?} (non-GTK Context/Window/View natives bound; launcher Activity = {}) ✓", plan.launcher_activity);

    // Owner-driven, first-party sign-in escape hatch for platforms without Google Play Integrity.
    // It is deliberately opt-in and carries no credential/session material: the owner types into
    // Roblox's official HTTPS page, rendered by the same persistent WebView/CookieManager profile
    // the app uses. Keep this after lifecycle so the app has finalized its WebView User-Agent and
    // initial cookie mutations before the browser is created.
    if std::env::var("ECLIPSE_WEB_LOGIN").is_ok_and(|value| value == "1") {
        println!("# Opening Roblox's official web login in Eclipse…");
        let handle = eclipse::framework::drive_roblox_web_login(&vm)?;
        println!("official Roblox web login opened (WebView handle {handle}) ✓");
    }

    // Open the host game window via winit (no GTK — keeps the low_4gb window clear for ART, the
    // Step 3.5 win). The Activity Surface + engine rendering will hang off this window next; for
    // now it opens the window and runs the event loop until closed. Runs on the main thread, with
    // `vm` (the booted VM) still alive on it.
    // Pass a borrow of the live VM so a pointer click in the window can dispatch View.performClick()
    // to the hit Android view via JNI (the minimal sound input path). `vm` stays alive (bound above)
    // for the whole event loop on this main thread, so the borrow is valid for its duration.
    println!("# Opening the host window (winit; close it to exit)…");
    eclipse::graphics::run_windowed(&format!("Eclipse — {}", manifest.package), Some(&vm))?;
    Ok(())
}

/// The ABI Eclipse runs (Android x86-64) and the engine's file name — kept here so the pre-load loop
/// names them once.
const TARGET_ABI: &str = "x86_64";
const ENGINE_FILENAME: &str = "libroblox.so";

/// Pre-load the app's x86-64 JNI libs through Eclipse's own Rust loader (map, relocate, resolve, init,
/// `JNI_OnLoad`), so each is already loaded — and its native methods registered — BEFORE the framework
/// lifecycle, instead of falling to ART's `Runtime.nativeLoad` → the apkenv shim linker (which aborts
/// on their modern relocations). Returns process-lifetime
/// [`PreloadedLib`](eclipse::loader::engine::PreloadedLib) descriptors, or an empty `Vec` for an APK
/// without `lib/x86_64/libroblox.so` (pure-Java demo APKs keep the framework-only path — no
/// regression). Each descriptor structurally prevents its initialized image from being unmapped by
/// lexical scope teardown; dropping the `Vec` releases metadata only.
///
/// `libroblox` is loaded FIRST and is **mandatory** (its workers + `JNI_OnLoad` are load-bearing; a
/// failure aborts the boot). The remaining libs (e.g. `libzstd-jni`, which `androidx.startup` loads in
/// `onCreate`) are loaded next, each **tolerant of failure** (a per-lib error logs a warning and the
/// loop continues — one optional lib must not abort the boot). Dedup is by soname, so a lib already
/// pulled in as a sibling `DT_NEEDED` is skipped. Runs on the process main thread, VM alive + JNI-attached.
fn preload_app_native_libs(
    apk: &mut eclipse::apk::Apk,
    apk_path: &std::path::Path,
    app_lib_dir: &std::path::Path,
    vm: &eclipse::runtime::Vm,
) -> Result<Vec<eclipse::loader::engine::PreloadedLib>, Box<dyn std::error::Error>> {
    // Cheap presence check (file-name scan, no 111 MiB read): only route when the x86_64 engine is in
    // the APK. demo_app/accelerometerdemo have no native lib here → skip, preserving the framework path.
    let has_engine = apk
        .native_abis()
        .iter()
        .any(|abi| abi.name == TARGET_ABI && abi.has_engine);
    if !has_engine {
        println!("# No lib/x86_64/libroblox.so in APK — skipping the Rust engine loader (framework-only path).");
        return Ok(Vec::new());
    }

    let mut log = std::io::stdout();
    let vm_raw = vm.as_raw();
    let mut loaded: Vec<eclipse::loader::engine::PreloadedLib> = Vec::new();

    // 1) The engine FIRST + mandatory: a failure here is a real boot blocker (its DT_INIT_ARRAY workers
    //    + JNI_OnLoad must run before the lifecycle).
    println!("# Pre-loading the native engine via Eclipse's Rust loader (NOT the apkenv linker)…");
    let engine = eclipse::loader::engine::load_app_native_lib(
        apk_path,
        ENGINE_FILENAME,
        vm_raw,
        app_lib_dir,
        &mut log,
    )?
    .ok_or("libroblox.so unexpectedly deduped on first load")?;
    report_preloaded(&engine);
    loaded.push(engine);

    // 2) The app's other x86_64 JNI libs, each TOLERANT of failure (an optional lib must not abort the
    //    boot). zstd-jni (androidx.startup) is the immediate one; the rest are pre-loaded too so their
    //    later System.loadLibrary also skips apkenv. Dedup by soname skips libroblox (already loaded).
    let filenames = apk.native_lib_filenames(TARGET_ABI);
    println!(
        "# Pre-loading {} other x86_64 JNI lib(s) via the Rust loader (tolerant of per-lib failure)…",
        filenames.iter().filter(|f| *f != ENGINE_FILENAME).count()
    );
    for filename in &filenames {
        if filename == ENGINE_FILENAME {
            continue; // already loaded (step 1)
        }
        match eclipse::loader::engine::load_app_native_lib(
            apk_path,
            filename,
            vm_raw,
            app_lib_dir,
            &mut log,
        ) {
            Ok(Some(lib)) => {
                report_preloaded(&lib);
                loaded.push(lib);
            }
            Ok(None) => {} // deduped (already pulled in as a sibling DT_NEEDED)
            Err(e) => {
                // Tolerate: log + continue. An optional JNI lib that fails to pre-load falls back to
                // ART's nativeLoad if its Java ever loads it; it must not abort the whole boot.
                eprintln!("# WARNING: pre-load of {filename} failed (continuing): {e}");
            }
        }
    }

    println!(
        "engine pre-load complete: {} x86_64 JNI lib(s) loaded via the Rust loader ✓",
        loaded.len()
    );
    Ok(loaded)
}

/// The `__webview-test` result whose `Display` IS the deterministic SUCCESS marker line
/// (guarded by `tests/engine_milestones.rs::webview_test_fires_load_upcalls_and_stages_frames`).
/// 2026-07-09 (plan M4): extended with the bridge/eval/UA/cookie booleans. The marker prints
/// booleans + counts ONLY — never the UA string, cookie value, bridge arg, or eval result.
struct WebViewTestReport {
    upcalls_ok: u32,
    started_ms: u128,
    finished_ms: u128,
    http: i32,
    frame_w: u32,
    frame_h: u32,
    distinct: usize,
}

impl std::fmt::Display for WebViewTestReport {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(
            f,
            "WebView engine pipeline OK: internalLoadChanged upcalls {}/2 (state 0 @ {}ms, \
             state 3 @ {}ms, http {}), frame {}x{} {} distinct pixels, bridge round-trip OK, \
             evaluateJavascript OK, honest UA OK, cookie set/get OK, cookie callback OK, \
             cookie flush OK, \
             ViewClosed, helper exit 0, bound=5",
            self.upcalls_ok,
            self.started_ms,
            self.finished_ms,
            self.http,
            self.frame_w,
            self.frame_h,
            self.distinct
        )
    }
}

/// The offline first-party test page (plan M4 §5.2): solid background + large text (robust nonzero
/// ink) + JS that records the UA and drives the `EclipseTest.echo` bridge round-trip.
const WEBVIEW_TEST_PAGE: &str = "<!doctype html><meta charset=utf-8><title>eclipse</title>\
<body style=\"background:#2244aa;color:#fff;font-size:40px\">Eclipse WebView M4\
<script>window.__eclipseUA=navigator.userAgent;\
function eclipseBridge(){\
if(window.EclipseTest&&window.EclipseTest.echo){\
window.EclipseTest.echo('PING').then(function(r){window.__eclipseBridgeResult=r;},\
function(e){window.__eclipseBridgeResult='ERR:'+e;});}\
else{setTimeout(eclipseBridge,50);}}\
eclipseBridge();</script></body>";

/// Serve [`WEBVIEW_TEST_PAGE`] on a loopback-only ephemeral port (plan M4 §5.1) — a real `http://`
/// origin so cookies + the bridge run in a normal browsing context, fully offline/deterministic.
/// The background thread serves one page for `/` (else 404), `Connection: close`. Returns the port
/// (the thread is a daemon killed at process exit).
fn start_loopback_page() -> std::io::Result<u16> {
    use std::io::{Read, Write};
    use std::net::TcpListener;
    let listener = TcpListener::bind("127.0.0.1:0")?;
    let port = listener.local_addr()?.port();
    std::thread::spawn(move || {
        for stream in listener.incoming() {
            let Ok(mut stream) = stream else { continue };
            let mut buf = [0u8; 2048];
            let n = stream.read(&mut buf).unwrap_or(0);
            let req = String::from_utf8_lossy(&buf[..n]);
            let path = req.split_whitespace().nth(1).unwrap_or("/");
            let (status, body): (&str, &str) = if path == "/" || path.starts_with("/?") {
                ("200 OK", WEBVIEW_TEST_PAGE)
            } else {
                ("404 Not Found", "")
            };
            let resp = format!(
                "HTTP/1.1 {status}\r\nContent-Type: text/html; charset=utf-8\r\n\
                 Content-Length: {}\r\nConnection: close\r\n\r\n{body}",
                body.len()
            );
            let _ = stream.write_all(resp.as_bytes());
        }
    });
    Ok(port)
}

/// `eclipse __webview-test` (2026-07-03, web-engine plan M3): the dev-host WebView pipeline
/// smoke. Boots ART on THIS main thread with the installed framework + APK on the classpath
/// (`android.webkit.WebView` must be the real installed class for the upcalls to dispatch), then
/// verifies natives→socket→helper→memfd→staging + the real `internalLoadChanged` upcalls.
///
/// Deliberate scope (honest divergence recorded for the ship pass): the vk-overlay COMPOSITE
/// branch only executes when the ENGINE presents through the interposed `vkQueuePresentKHR` —
/// i.e. the full M6 libroblox boot — so this harness proves the staged frame has nonzero ink in
/// the MAIN process and leaves the on-screen composite to the M6 live boot (its pure parts are
/// unit-pinned). Loads ONLY the public https://www.roblox.com page (M1/M2 precedent) — never a
/// challenge URL, no APK dex execution beyond framework classes.
/// 2026-07-16 (web-engine M6): one `__webview-test` poll tick — pump the main Looper, then sleep.
/// This subcommand has NO winit loop, so it stands in for `graphics::about_to_wait`: the pump is
/// what runs the client's app-facing WebView callbacks on THIS (main, Looper-prepared) thread and
/// then dispatches the messages they posted. It is deliberately used at EVERY poll site below — a
/// prepared-but-unpumped main Looper is the shape the root-cause analysis rejects as worse than a
/// throw.
fn pump_tick(vm: &eclipse::runtime::Vm, ms: u64) {
    if let Err(e) = eclipse::framework::pump_main_looper(vm) {
        eprintln!("# main Looper pump failed: {e}");
    }
    std::thread::sleep(std::time::Duration::from_millis(ms));
}

fn run_webview_test() -> Result<WebViewTestReport, Box<dyn std::error::Error>> {
    use eclipse::framework;
    use eclipse::webview::client;
    use std::time::{Duration, Instant};
    // Drive.rs deadline precedents (M2): load-start 30 s, load-finish 90 s, ink-settle 20 s.
    const START_DEADLINE: Duration = Duration::from_secs(30);
    const FINISH_DEADLINE: Duration = Duration::from_secs(90);
    const UPCALL_DEADLINE: Duration = Duration::from_secs(10);
    const INK_DEADLINE: Duration = Duration::from_secs(20);
    const LEG_DEADLINE: Duration = Duration::from_secs(15);
    const CLOSE_DEADLINE: Duration = Duration::from_secs(15);

    // 2026-07-10 (plan M5): display-session echo — OBSERVABILITY ONLY. The helper's own
    // `select_ozone` remains the single selection authority (the client still passes no ozone
    // flag); this mirrors its env rule (set AND non-empty counts; `XDG_SESSION_TYPE` never
    // consulted) so the run log shows the session shape beside the helper's own detection line,
    // and a no-display host errors actionably BEFORE the ART boot cost.
    let wayland_set = std::env::var("WAYLAND_DISPLAY").is_ok_and(|v| !v.is_empty());
    let display_set = std::env::var("DISPLAY").is_ok_and(|v| !v.is_empty());
    match (wayland_set, display_set) {
        (true, _) => println!("# display: wayland (WAYLAND_DISPLAY set)"),
        (false, true) => println!("# display: x11 (DISPLAY set, WAYLAND_DISPLAY unset)"),
        (false, false) => {
            return Err(
                "no display detected: neither WAYLAND_DISPLAY nor DISPLAY is set — the \
                        CEF helper needs a Wayland or X11 session (its own select_ozone would \
                        refuse with the same error)"
                    .into(),
            )
        }
    }

    // 2026-07-09 (plan M4): an OFFLINE loopback page — a real http:// origin so the cookie + bridge
    // legs run in a normal browsing context (a data:/about:blank origin has opaque cookies).
    let port = start_loopback_page()?;
    let target_url = format!("http://127.0.0.1:{port}/");
    println!("# __webview-test: loopback page serving at {target_url}");

    let apk_path = eclipse::loader::init_run::find_roblox_apk().ok_or(
        "no Roblox APK (set ECLIPSE_ROBLOX_APK or place it at the default dev-host path) — \
         __webview-test boots ART with the installed framework on the classpath",
    )?;
    println!(
        "# __webview-test: booting ART from {} (framework classpath; no libroblox preload, \
         no lifecycle, no window)…",
        apk_path.display()
    );
    let mut apk = eclipse::apk::Apk::open(&apk_path)?;
    let manifest = apk.manifest()?;
    let config = eclipse::config::Config::load()?;
    let plan = eclipse::runtime::BootPlan::new(&manifest, &config);
    let vm = eclipse::runtime::boot(&plan, Some(apk_path.as_path()), None)?;
    // 2026-07-09 (plan M4): CookieManager.getInstance() triggers Context/AssetManager/Build static
    // initializers that call android.util.Log.println_native — bind the Log/Process natives (the
    // production drive_lifecycle does this pre-preload) so the cookie leg's clinit chain resolves.
    eclipse::framework::register_engine_preload_natives(&vm)?;
    // 2026-07-16 (web-engine M6 root fix): give this harness's main thread the SAME main Looper
    // production's lifecycle step 0 gives it. Without it Looper.getMainLooper() is NULL here, the
    // client's app-facing WebView callbacks have no UI thread to land on, and the harness cannot
    // represent production at all — which is precisely why it was blind to the challenge17/18
    // Looper-less-dispatch bug.
    eclipse::framework::prepare_main_looper(&vm)?;
    println!("# ART booted ✓ — driving the WebView smoke (register → alloc → setWebViewClient → addJavascriptInterface → loadUrl)…");
    let handle = eclipse::framework::drive_webview_smoke(&vm, &target_url)?;

    // Poll the client's observations (the reader thread stages frames + fires the upcalls).
    let fail_reason =
        || client::failed_reason().map(|r| format!("web engine helper unavailable: {r}"));
    let start = Instant::now();
    let mut started_ms: Option<u128> = None;
    let (finished_ms, http) = loop {
        if let Some(reason) = fail_reason() {
            return Err(reason.into());
        }
        let obs = client::load_observed(handle);
        if let Some(obs) = obs {
            if obs.started && started_ms.is_none() {
                started_ms = Some(start.elapsed().as_millis());
                println!(
                    "# load-state 0 observed @ {} ms",
                    start.elapsed().as_millis()
                );
            }
            if let Some(http) = obs.finished_http {
                println!(
                    "# load-state 3 observed @ {} ms http={http}",
                    start.elapsed().as_millis()
                );
                break (start.elapsed().as_millis(), http);
            }
        }
        if started_ms.is_none() && start.elapsed() > START_DEADLINE {
            return Err("load-started (internalLoadChanged 0) not observed within 30 s".into());
        }
        if start.elapsed() > FINISH_DEADLINE {
            return Err("load-finished (internalLoadChanged 3) not observed within 90 s".into());
        }
        pump_tick(&vm, 50);
    };
    let started_ms = started_ms.ok_or("load-finished arrived without load-started")?;

    // Both upcalls (0 and 3) must have COMPLETED through the real Java dispatch.
    let upcall_deadline = Instant::now() + UPCALL_DEADLINE;
    let upcalls_ok = loop {
        let ok = client::load_observed(handle)
            .map(|o| o.upcalls_ok)
            .unwrap_or(0);
        if ok >= 2 {
            break ok;
        }
        if Instant::now() > upcall_deadline {
            return Err(format!(
                "only {ok}/2 internalLoadChanged upcalls completed within 10 s of load-finish"
            )
            .into());
        }
        pump_tick(&vm, 50);
    };

    // Nonzero ink in the MAIN-process staging buffer (the M1/M2 distinct-pixel criterion).
    let ink_deadline = Instant::now() + INK_DEADLINE;
    let (frame_w, frame_h, distinct) = loop {
        if let Some(reason) = fail_reason() {
            return Err(reason.into());
        }
        let census = client::with_latest_frame(handle, |stage| {
            let mut distinct = std::collections::HashSet::new();
            for px in stage.bytes.chunks_exact(4) {
                distinct.insert(u32::from_ne_bytes([px[0], px[1], px[2], px[3]]));
            }
            (stage.width, stage.height, distinct.len())
        });
        if let Some((w, h, count)) = census {
            if count > 1 {
                println!("# staged frame {w}x{h} distinct_pixels={count}");
                break (w, h, count);
            }
        }
        if Instant::now() > ink_deadline {
            return Err("no staged frame with nonzero ink within 20 s of load-finish".into());
        }
        pump_tick(&vm, 50);
    };

    // ---- M4 legs (plan §5.3): evaluateJavascript + honest UA, bridge round-trip, cookies. ----
    // A single evaluateJavascript drive through the JAVA path, polling the probe's recorded result.
    let eval_and_wait = |script: &str| -> Option<String> {
        if framework::webview_evaluate(&vm, handle, script).is_err() {
            return None;
        }
        let end = Instant::now() + LEG_DEADLINE;
        loop {
            if let Some(v) = framework::read_probe_last_value(&vm) {
                return Some(v);
            }
            if Instant::now() > end {
                return None;
            }
            pump_tick(&vm, 50);
        }
    };

    // UA leg: navigator.userAgent must be the honest Eclipse-identified Chromium 149 (never the
    // recorded "GDPR VIOLATION"). The value is checked but NEVER printed (payload-free marker).
    let ua = eval_and_wait("navigator.userAgent")
        .ok_or("evaluateJavascript(navigator.userAgent) produced no result within 15 s")?;
    if !(ua.contains("Eclipse-WebView") && ua.contains("Chrome/149"))
        || ua.contains("GDPR VIOLATION")
    {
        return Err(
            "navigator.userAgent is not the honest Eclipse UA (evaluateJavascript/UA leg failed)"
                .into(),
        );
    }
    println!("# evaluateJavascript OK; honest UA OK (UA value not printed)");

    // Bridge leg: the page called window.EclipseTest.echo('PING') → JNI reflect-invoke → async
    // result back to the page (window.__eclipseBridgeResult == "echo:PING"). Poll the read-back.
    let bridge_deadline = Instant::now() + LEG_DEADLINE;
    loop {
        if let Some(r) = eval_and_wait("window.__eclipseBridgeResult||''") {
            if r.contains("echo:PING") {
                break;
            }
        }
        if Instant::now() > bridge_deadline {
            return Err("bridge round-trip did not complete (window.__eclipseBridgeResult != echo:PING within 15 s)".into());
        }
        pump_tick(&vm, 100);
    }
    // The @JavascriptInterface method also recorded its arg on the ART side.
    match framework::read_probe_last(&vm).as_deref() {
        Some("PING") => {
            println!("# bridge round-trip OK (page JS → JNI reflect-invoke → async result)")
        }
        other => {
            return Err(format!(
                "EclipseBridgeProbe.last != PING (JNI reflect-invoke leg failed: {other:?})"
            )
            .into())
        }
    }

    // Cookie leg: optionally prove a prior helper process's session cookie was restored BEFORE
    // writing this run's value, then exercise set/get, the real 3-arg callback, and blocking flush.
    if std::env::var("ECLIPSE_WEBVIEW_EXPECT_PERSISTED_TEST_COOKIE").as_deref() == Ok("1") {
        let restored = framework::cookie_manager_get_cookie(&vm, &target_url);
        if !restored.contains("ECLIPSE_TEST=1") {
            return Err("persistent-cookie probe did not restore ECLIPSE_TEST before this process's setCookie".into());
        }
        println!("# persisted cookie restored OK (value not printed)");
    }
    framework::cookie_manager_set_cookie(&vm, &target_url, "ECLIPSE_TEST=1; Path=/")
        .map_err(|e| format!("CookieManager.setCookie(2-arg) failed: {e}"))?;
    let cookie_deadline = Instant::now() + LEG_DEADLINE;
    loop {
        let got = framework::cookie_manager_get_cookie(&vm, &target_url);
        if got.contains("ECLIPSE_TEST=1") {
            break;
        }
        if Instant::now() > cookie_deadline {
            return Err("CookieManager.getCookie did not return ECLIPSE_TEST=1 within 15 s".into());
        }
        pump_tick(&vm, 100);
    }
    println!("# cookie set/get OK (values not printed)");
    // The 3-arg setCookie callback carries the REAL success flag.
    framework::cookie_manager_set_cookie_cb(&vm, &target_url, "ECLIPSE_CB=1; Path=/")
        .map_err(|e| format!("CookieManager.setCookie(3-arg) failed: {e}"))?;
    let cb_deadline = Instant::now() + LEG_DEADLINE;
    let cb_ok = loop {
        if let Some(v) = framework::read_probe_last_value(&vm) {
            if v.contains("true") {
                break true;
            }
        }
        if Instant::now() > cb_deadline {
            break false;
        }
        pump_tick(&vm, 50);
    };
    if !cb_ok {
        return Err(
            "3-arg setCookie ValueCallback did not fire with Boolean.TRUE within 15 s".into(),
        );
    }
    println!("# cookie callback OK (real Boolean.TRUE, not fabricated)");
    framework::cookie_manager_flush(&vm).map_err(|e| format!("CookieManager.flush failed: {e}"))?;
    println!("# cookie flush OK (CEF persistent-store completion boundary returned)");

    // CloseView → ViewClosed (the entry disappears), then Shutdown → helper exit 0.
    client::close_view(handle).map_err(|e| format!("CloseView send failed: {e}"))?;
    let close_deadline = Instant::now() + CLOSE_DEADLINE;
    while client::view_is_tracked(handle) {
        if let Some(reason) = fail_reason() {
            return Err(reason.into());
        }
        if Instant::now() > close_deadline {
            return Err("ViewClosed not observed within 15 s".into());
        }
        pump_tick(&vm, 50);
    }
    println!("# view-closed ✓ — shutting the helper down…");
    let report = client::shutdown(&vm, Duration::from_secs(15));
    if report.helper_exit != Some(0) {
        return Err(format!(
            "helper exit status {:?} (expected 0; reader_joined={})",
            report.helper_exit, report.reader_joined
        )
        .into());
    }
    Ok(WebViewTestReport {
        upcalls_ok,
        started_ms,
        finished_ms,
        http,
        frame_w,
        frame_h,
        distinct,
    })
}

/// Print a one-line summary of a pre-loaded lib (constructors run + `JNI_OnLoad` result).
fn report_preloaded(lib: &eclipse::loader::engine::PreloadedLib) {
    let ctors = if lib.constructors_run > 0 {
        format!("{} ctor(s)", lib.constructors_run)
    } else {
        "no ctors".to_string()
    };
    let onload = match lib.jni_onload_version {
        Some(v) if v < 0 => format!("JNI_OnLoad error {v:#x}"),
        Some(v) => format!("JNI_OnLoad → {v:#x}"),
        None => "lazy natives (no JNI_OnLoad)".to_string(),
    };
    println!("  {} ✓ ({ctors}; {onload})", lib.soname);
}

#[cfg(test)]
mod tests {
    use super::finish_android_process;

    const RAW_EXIT_CHILD: &str = "ECLIPSE_TEST_RAW_ANDROID_EXIT_CHILD";

    extern "C" fn abort_if_atexit_runs() {
        std::process::abort();
    }

    #[test]
    fn android_process_exit_skips_unsafe_foreign_atexit_handlers() {
        if std::env::var_os(RAW_EXIT_CHILD).is_some() {
            // SAFETY: the callback has the required C ABI and static lifetime. This child process
            // exists solely to prove that `finish_android_process` bypasses the registered handler.
            let registered = unsafe { libc::atexit(abort_if_atexit_runs) };
            assert_eq!(registered, 0, "the child must register its atexit sentinel");
            finish_android_process(0);
        }

        let output = std::process::Command::new(
            std::env::current_exe().expect("the test harness executable must have a path"),
        )
        .args([
            "--exact",
            "tests::android_process_exit_skips_unsafe_foreign_atexit_handlers",
        ])
        .env(RAW_EXIT_CHILD, "1")
        .output()
        .expect("the raw-exit child must start");

        assert!(
            output.status.success(),
            "the raw-exit child ran an atexit handler: status={:?}, stderr={}",
            output.status.code(),
            String::from_utf8_lossy(&output.stderr)
        );
    }
}
