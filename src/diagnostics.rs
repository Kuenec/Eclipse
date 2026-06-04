//! Diagnostics & logging (component-map A · 🟢 pure Rust).
//!
//! Will use `tracing` + `tracing-subscriber` for structured, leveled diagnostics — the
//! backbone for the project's "observe before fixing" policy. For now this is a no-op so
//! the binary runs without pulling a dependency.
//!
//! TODO(M1): initialize a `tracing` subscriber honoring `RUST_LOG` / an env filter.

/// Initialize diagnostics. No-op until `tracing` is wired up (M1).
pub fn init() {}
