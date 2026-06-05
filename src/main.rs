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
    let _vm = eclipse::runtime::boot(
        &plan,
        Some(std::path::Path::new(apk_path)),
        Some(&app_lib_dir),
    )?;
    println!("ART VM booted with Roblox's Java on the classpath ✓");

    // Open the host game window via winit (no GTK — keeps the low_4gb window clear for ART, the
    // Step 3.5 win). The Activity Surface + engine rendering will hang off this window next; for
    // now it opens the window and runs the event loop until closed. Runs on the main thread, with
    // `_vm` (the booted VM) still alive on it.
    println!("# Opening the host window (winit; close it to exit)…");
    eclipse::graphics::run_windowed(&format!("Eclipse — {}", manifest.package))?;
    Ok(())
}
