use eclipse_webview::redact;
use std::fmt;

pub(crate) struct Redacted<'a>(pub(crate) &'a str);

impl fmt::Display for Redacted<'_> {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(&redact::url_scheme_and_host_for_log(self.0))
    }
}

fn line(level: &str, message: fmt::Arguments<'_>) {
    eprintln!("eclipse-webview {level}: {message}");
}

pub(crate) fn info(message: fmt::Arguments<'_>) {
    line("INFO", message);
}

pub(crate) fn warn(message: fmt::Arguments<'_>) {
    line("WARN", message);
}

pub(crate) fn error(message: fmt::Arguments<'_>) {
    line("ERROR", message);
}

#[cfg(test)]
mod tests {
    use super::Redacted;

    #[test]
    fn redacted_urls_keep_only_scheme_and_host() {
        let shown = Redacted("https://apps.roblox.com/challenge/verify?token=SECRET").to_string();
        assert_eq!(shown, "https://apps.roblox.com");
        assert_eq!(
            Redacted("data:text/html,<p>SECRET</p>").to_string(),
            "<non-url>"
        );
    }
}
