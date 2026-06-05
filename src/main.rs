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
    run <APK>  Show the ART boot plan for an APK (dry run; VM boot not yet implemented)
    config     Show effective configuration and its path
    help       Show this help
    --version  Show version

STATUS:
    `run` is a dry run: it opens the APK, parses the manifest, and prints the ART boot
    plan (heap, host ISA, graphics backend, launcher) that the runtime will pass — the
    VM boot itself (ART FFI) is pending. See docs/ for the full design.
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
        Some("run") => match run_dry(args.get(1).map(String::as_str)) {
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

/// Dry run for `eclipse run <APK>`: open the APK, parse its manifest, build the ART
/// [`BootPlan`](eclipse::runtime::BootPlan) from the manifest + effective config, and print
/// the plan plus the ART options it would pass. The VM boot itself is not implemented yet
/// (ART FFI pending) — this is the honest, demonstrable step before that lands.
///
/// Returns `Box<dyn Error>` because this `main`/setup-layer code composes several typed
/// library errors (APK, config); the library crates themselves stay strictly typed (§2.8).
fn run_dry(apk_path: Option<&str>) -> Result<(), Box<dyn std::error::Error>> {
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
    println!("\n# ART/dex2oat options this plan would pass:");
    for opt in plan.art_options() {
        println!("    {opt}");
    }

    // Do not fake a boot: report that the VM boot is pending, the same honest posture as the
    // rest of the launcher. boot() returns NotImplemented by design (see runtime::boot).
    println!();
    match eclipse::runtime::boot(&plan) {
        Ok(()) => println!("VM booted."),
        Err(e) => println!("VM boot not started: {e}"),
    }
    Ok(())
}
