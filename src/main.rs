//! Eclipse launcher entry point.
//!
//! This is a placeholder CLI: the subsystems it will drive are still stubs (see
//! `src/lib.rs`). It dispatches the intended subcommands and reports honest status so the
//! binary is runnable today (`cargo run -- help`).

use std::process::ExitCode;

const HELP: &str = "\
eclipse — run the Android Roblox build on Linux (open-source, Rust)

USAGE:
    eclipse <COMMAND>

COMMANDS:
    run <APK>  Parse the APK, print the ART boot plan, and boot the ART VM
    config     Show effective configuration and its path
    help       Show this help
    --version  Show version

STATUS:
    `run` opens the APK, parses the manifest, prints the ART boot plan (heap, host ISA,
    graphics backend, launcher), then boots the vendored ART VM. Today that brings up a
    libcore VM (proving ART boots from Eclipse's graphics-free process); reaching Roblox's
    onCreate (app classpath/Activity/native-lib/winit) is the next step. See docs/.
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

    // Boot the ART VM from this (main) thread — the production entry point — with the APK on the
    // classpath, so ART loads Roblox's Java (+ the android.* framework) alongside libcore.
    // Driving the launcher Activity to onCreate (the GTK-coupled framework / Eclipse's own
    // winit+Vulkan framework) is the next step. ART logs verbosely to stderr on first run
    // (dex2oat compiles the boot image once).
    println!("\n# Booting the ART VM with Roblox on the classpath…");
    eclipse::runtime::boot(&plan, Some(std::path::Path::new(apk_path)))?;
    println!("ART VM booted with Roblox's Java on the classpath ✓ (onCreate pending)");
    Ok(())
}
