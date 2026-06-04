//! Diagnostics & logging (component-map A · 🟢 pure Rust).
//!
//! The backbone for the project's "observe before fixing" policy: subsystems emit
//! structured events via the `tracing` facade (`tracing::info!`, `warn!`, …) and this
//! module installs the global subscriber that formats and filters them.
//!
//! Verbosity is controlled at runtime by the standard `RUST_LOG` env var
//! (e.g. `RUST_LOG=debug`, `RUST_LOG=eclipse::runtime=trace`); when unset it defaults to
//! `info`. This is detect-don't-assume by construction — no hardcoded log level.

use tracing_subscriber::filter::LevelFilter;
use tracing_subscriber::{fmt, EnvFilter};

/// Install the global tracing subscriber.
///
/// Honors `RUST_LOG`, defaulting to `info` when it is unset or empty. Safe to call more
/// than once: if a global subscriber is already installed (e.g. in tests, or a double
/// call), the redundant install is ignored rather than panicking — so this stays
/// infallible for callers like `main`.
pub fn init() {
    // `from_env_lossy` reads RUST_LOG and ignores malformed directives (logging setup must
    // never abort startup); `with_default_directive(INFO)` applies when RUST_LOG is unset.
    let filter = EnvFilter::builder()
        .with_default_directive(LevelFilter::INFO.into())
        .from_env_lossy();

    // try_init returns Err only if a global subscriber is already set; that is not an error
    // we need to surface (idempotency for tests / repeated calls), so discard it.
    let _ = fmt().with_env_filter(filter).try_init();
}

#[cfg(test)]
mod tests {
    /// Regression guard: `init` must be idempotent. A naive `set_global_default().unwrap()`
    /// would panic on the second call; we rely on `try_init` swallowing that.
    #[test]
    fn init_is_idempotent() {
        super::init();
        super::init();
    }
}
