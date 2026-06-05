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
    run <APK>  Parse the APK, boot the ART VM (Roblox on the classpath), open the window
    config     Show effective configuration and its path
    help       Show this help
    --version  Show version

STATUS:
    `run` parses the manifest, prints the ART boot plan, boots the vendored ART VM with
    Roblox's Java on the classpath, then opens the host game window (winit, no GTK). The
    framework that drives the launcher Activity to onCreate and renders the engine into the
    window is the next phase (component-map F). See docs/.
";

fn main() -> ExitCode {
    eclipse::diagnostics::init();

    let args: Vec<String> = std::env::args().skip(1).collect();
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
        Some("run") => match run_apk(args.get(1).map(String::as_str)) {
            Ok(()) => ExitCode::SUCCESS,
            Err(e) => {
                eprintln!("eclipse run: {e}");
                ExitCode::FAILURE
            }
        },
        Some("config") => match show_config() {
            Ok(()) => ExitCode::SUCCESS,
            Err(e) => {
                eprintln!("eclipse config: {e}");
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

/// Print the effective configuration (file values merged over defaults) and its path.
fn show_config() -> Result<(), eclipse::config::ConfigError> {
    let path = eclipse::config::Config::config_path()?;
    let config = eclipse::config::Config::load()?;
    println!("# {}", path.display());
    println!("{}", config.to_json_pretty()?);
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
    let Some(apk_path) = apk_path else {
        return Err("missing APK path (usage: eclipse run <APK>)".into());
    };

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

    // Route the app's x86_64 JNI libs through Eclipse's OWN Rust loader (NOT the apkenv shim linker,
    // which aborts on their modern relocs — R_X86_64_TPOFF64). Each is mapped + relocated + fully
    // resolved + init-run + (if it exports JNI_OnLoad) handed the REAL ART JavaVM. The returned images
    // are BOUND for the process lifetime (libroblox spawns workers that execute the mapped text).
    // Skipped entirely for APKs without lib/x86_64/libroblox.so (e.g. the pure-Java demo_app), so the
    // demo lifecycle path is unchanged. Runs on this (main) thread, VM alive + JNI-attached, BEFORE the
    // lifecycle drives Roblox's onCreate (where androidx.startup would call System.loadLibrary for
    // libroblox AND zstd-jni — both now already loaded here, so neither falls to apkenv).
    let _engines =
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
/// on their modern relocations). Returns the persistent
/// [`PreloadedLib`](eclipse::loader::engine::PreloadedLib)s the caller binds for the process lifetime,
/// or an empty `Vec` for an APK without `lib/x86_64/libroblox.so` (pure-Java demo APKs keep the
/// framework-only path — no regression).
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
