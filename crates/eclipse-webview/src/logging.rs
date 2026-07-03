//! Tiny owned std-only stderr logger for the helper (timestamp / level / component).
//!
//! 2026-07-03: the absolute URL-redaction rule crosses the process boundary by TYPE
//! DISCIPLINE — every load-event formatter accepts ONLY [`RedactedTarget`] (constructible
//! solely through the shared redaction contract), so a raw URL can never be bound to a log
//! call; `loadDataWithBaseURL`'s `data` payload has no formatter parameter at all (only the
//! mime and the redacted base URL, mirroring the M3-seam natives in `framework.rs`).
//! Engine-side logging is disabled at the source (`build_settings`), so this logger is the
//! helper's ONLY stderr voice.

use crate::shared::redact;
use std::time::{SystemTime, UNIX_EPOCH};

/// A URL already reduced to scheme+host (or `"<non-url>"`) via the shared contract — the ONLY
/// string shape the load-event formatters accept. Private field: unforgeable.
pub struct RedactedTarget(String);

impl RedactedTarget {
    /// The one constructor: redacts through [`redact::url_scheme_and_host_for_log`].
    pub fn from_raw_url(url: &str) -> Self {
        Self(redact::url_scheme_and_host_for_log(url))
    }

    pub fn as_str(&self) -> &str {
        &self.0
    }
}

fn timestamp_ms() -> u128 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_millis())
        .unwrap_or(0)
}

fn line(level: &str, component: &str, msg: &str) {
    eprintln!("[{} {level} {component}] {msg}", timestamp_ms());
}

pub fn info(component: &str, msg: &str) {
    line("INFO", component, msg);
}

pub fn warn(component: &str, msg: &str) {
    line("WARN", component, msg);
}

pub fn error(component: &str, msg: &str) {
    line("ERROR", component, msg);
}

/// Load-event line (loadUrl / OnLoadStart / OnLoadEnd / on_load_error): binds ONLY the
/// pre-redacted target.
pub fn format_load_event(event: &str, view: i64, target: &RedactedTarget) -> String {
    format!("load {event} view={view} target={}", target.as_str())
}

/// `loadDataWithBaseURL` line: the `data` payload is structurally absent (no parameter);
/// only the non-sensitive mime + the redacted base URL are bound.
pub fn format_load_data_event(view: i64, mime: &str, base: &RedactedTarget) -> String {
    format!(
        "load data-with-base-url view={view} mime={mime} base={}",
        base.as_str()
    )
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn helper_log_lines_redact_urls_to_scheme_and_host() {
        // 2026-07-03: pins helper-side logging redaction — a token-bearing URL fed through
        // the load-event formatter emits scheme+host only, and the data-payload formatter
        // binds no data at all (it has no data parameter — checked structurally by its
        // signature; asserted on output here).
        let target =
            RedactedTarget::from_raw_url("https://apps.roblox.com/challenge/verify?token=SECRET");
        let line = format_load_event("started", 42, &target);
        assert_eq!(line, "load started view=42 target=https://apps.roblox.com");
        assert!(!line.contains("SECRET") && !line.contains("/challenge"));

        let base = RedactedTarget::from_raw_url("https://host/base?sid=TOPSECRET");
        let line = format_load_data_event(7, "text/html", &base);
        assert_eq!(
            line,
            "load data-with-base-url view=7 mime=text/html base=https://host"
        );
        assert!(!line.contains("TOPSECRET"));

        // A data: base URL (payload-bearing) reduces to the safe literal.
        let data_base = RedactedTarget::from_raw_url("data:text/html,<html>SECRET</html>");
        assert_eq!(data_base.as_str(), "<non-url>");
    }
}
