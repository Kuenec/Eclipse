









use crate::shared::redact;
use std::time::{SystemTime, UNIX_EPOCH};



pub struct RedactedTarget(String);

impl RedactedTarget {

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



pub fn format_load_event(event: &str, view: i64, target: &RedactedTarget) -> String {
    format!("load {event} view={view} target={}", target.as_str())
}



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


        let data_base = RedactedTarget::from_raw_url("data:text/html,<html>SECRET</html>");
        assert_eq!(data_base.as_str(), "<non-url>");
    }
}
