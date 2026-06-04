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
    run        Launch Roblox            (not yet implemented)
    config     Show effective configuration and its path
    help       Show this help
    --version  Show version

STATUS:
    Research/scoping is complete and the plan is locked. The next step is the manual
    foundation validation in docs/m0-runbook.md (M0). See docs/ for the full design.
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
        Some("run") => {
            eprintln!(
                "`eclipse run` is not implemented yet.\n\
                 Foundation validation comes first — see docs/m0-runbook.md."
            );
            ExitCode::FAILURE
        }
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
