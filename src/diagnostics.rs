//! Diagnostics & logging (component-map A · 🟢 pure Rust).
//!
//! The backbone for the project's "observe before fixing" policy: subsystems emit
//! structured events via the `tracing` facade (`tracing::info!`, `warn!`, …) and this
//! module installs the global subscriber that formats and filters them.
//!
//! Verbosity is controlled at runtime by the standard `RUST_LOG` env var
//! (e.g. `RUST_LOG=debug`, `RUST_LOG=eclipse::runtime=trace`); when unset it defaults to
//! `info`. This is detect-don't-assume by construction — no hardcoded log level.

use std::io::Write as _;

use tracing::field::{Field, Visit};
use tracing::{Event, Subscriber};
use tracing_subscriber::filter::LevelFilter;
use tracing_subscriber::fmt::format::Writer;
use tracing_subscriber::fmt::time::{FormatTime, SystemTime};
use tracing_subscriber::layer::{Context, Layer, SubscriberExt};
use tracing_subscriber::registry::Registry;
use tracing_subscriber::util::SubscriberInitExt;
use tracing_subscriber::EnvFilter;

/// A **panic-safe** stderr event layer: it formats each event into a function-LOCAL `String` and
/// writes it with one `stderr().write_all`, touching **no thread-local**.
///
/// 2026-06-12: Eclipse routes the Android engine's `android.util.Log`/`liblog` firehose AND its own
/// native diagnostics through `tracing`, so events are emitted from ART/bionic-created WORKER threads,
/// not just the main thread. `tracing-subscriber`'s default `fmt` layer formats through a
/// `thread_local! BUF` (`fmt_layer.rs:1018`, `BUF.with(...)`); a worker logging *during its TLS
/// teardown* hits `LocalKey::with` on a destroyed TLS → `AccessError` → **panic**, and under
/// `panic = "abort"` (AGENTS.md §2.4) that ABORTS the whole process. The main-Looper pump exposed this
/// by advancing Roblox into spawning+exiting many such worker threads. This layer removes the only
/// thread-local in the hot path; `EnvFilter`'s level filtering is target-based (no span-stack access
/// for Eclipse's bare events), so the event path stays teardown-safe.
struct PanicSafeStderr;

/// Appends an event's fields to the line buffer: the `message` field (the `tracing!` format string,
/// a `fmt::Arguments` whose `Debug` is the unquoted text) bare, every other field as ` name=value`.
struct EventFieldVisitor<'a>(&'a mut String);

impl Visit for EventFieldVisitor<'_> {
    fn record_debug(&mut self, field: &Field, value: &dyn std::fmt::Debug) {
        use std::fmt::Write as _;
        if field.name() == "message" {
            let _ = write!(self.0, "{value:?}");
        } else {
            let _ = write!(self.0, " {}={value:?}", field.name());
        }
    }
}

impl<S: Subscriber> Layer<S> for PanicSafeStderr {
    fn on_event(&self, event: &Event<'_>, _ctx: Context<'_, S>) {
        use std::fmt::Write as _;
        let meta = event.metadata();
        let mut line = String::with_capacity(256);
        // RFC3339 UTC timestamp via the same formatter the default layer uses — but written into our
        // LOCAL buffer (FormatTime::format_time takes a Writer, holds no thread-local state).
        let _ = SystemTime.format_time(&mut Writer::new(&mut line));
        let _ = write!(line, " {:>5} {}: ", meta.level(), meta.target());
        event.record(&mut EventFieldVisitor(&mut line));
        line.push('\n');
        // One write of a fully-formed line: never touches a thread-local that could be torn down under
        // the caller (the regression the default fmt layer caused, 2026-06-12).
        let _ = std::io::stderr().write_all(line.as_bytes());
    }
}

/// Install the global tracing subscriber.
///
/// Honors `RUST_LOG`, defaulting to `info` when it is unset or empty. Safe to call more
/// than once: if a global subscriber is already installed (e.g. in tests, or a double
/// call), the redundant install is ignored rather than panicking — so this stays
/// infallible for callers like `main`. Uses the teardown-safe [`PanicSafeStderr`] layer (see its
/// docs) instead of the default `fmt` layer, whose thread-local buffer aborts when a worker thread
/// logs during its TLS teardown.
pub fn init() {
    // `from_env_lossy` reads RUST_LOG and ignores malformed directives (logging setup must
    // never abort startup); `with_default_directive(INFO)` applies when RUST_LOG is unset.
    let filter = EnvFilter::builder()
        .with_default_directive(LevelFilter::INFO.into())
        .from_env_lossy();

    // try_init returns Err only if a global subscriber is already set; that is not an error
    // we need to surface (idempotency for tests / repeated calls), so discard it.
    let _ = Registry::default()
        .with(filter)
        .with(PanicSafeStderr)
        .try_init();
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
