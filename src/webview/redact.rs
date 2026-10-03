#![forbid(unsafe_code)]

use std::borrow::Cow;

pub const NON_URL: &str = "<non-url>";

const SCHEME_SEPARATOR: &str = "://";
const REDACTED_SCHEMES: [&str; 2] = ["https", "http"];

pub fn redact_urls_for_log(text: &str) -> Cow<'_, str> {
    let mut redacted = String::new();
    let mut copied = 0;
    let mut cursor = 0;
    while let Some(offset) = text[cursor..].find(SCHEME_SEPARATOR) {
        let separator = cursor + offset;
        let authority = separator + SCHEME_SEPARATOR.len();
        cursor = authority;
        let Some(start) = redacted_scheme_start(&text[copied..separator]) else {
            continue;
        };
        let start = copied + start;
        let end = text[authority..]
            .find(ends_url)
            .map_or(text.len(), |length| authority + length);
        redacted.push_str(&text[copied..start]);
        redacted.push_str(&url_scheme_and_host_for_log(&text[start..end]));
        copied = end;
        cursor = end;
    }
    if copied == 0 {
        return Cow::Borrowed(text);
    }
    redacted.push_str(&text[copied..]);
    Cow::Owned(redacted)
}

fn redacted_scheme_start(before_separator: &str) -> Option<usize> {
    REDACTED_SCHEMES.iter().find_map(|scheme| {
        let start = before_separator.len().checked_sub(scheme.len())?;
        before_separator
            .get(start..)?
            .eq_ignore_ascii_case(scheme)
            .then_some(start)
    })
}

fn ends_url(c: char) -> bool {
    c.is_whitespace()
        || c.is_control()
        || matches!(c, '"' | '<' | '>' | '\\' | '^' | '`' | '{' | '|' | '}')
}

pub fn url_scheme_and_host_for_log(url: &str) -> String {
    let Some(sep) = url.find("://") else {
        return NON_URL.to_string();
    };
    let scheme = &url[..sep];
    let mut chars = scheme.chars();
    let scheme_valid = match chars.next() {
        Some(first) if first.is_ascii_alphabetic() => {
            chars.all(|c| c.is_ascii_alphanumeric() || matches!(c, '+' | '-' | '.'))
        }
        _ => false,
    };
    if !scheme_valid {
        return NON_URL.to_string();
    }
    let rest = &url[sep + 3..];
    let authority = &rest[..rest.find(['/', '?', '#']).unwrap_or(rest.len())];
    let host = match authority.rfind('@') {
        Some(at) => &authority[at + 1..],
        None => authority,
    };
    format!("{scheme}://{host}")
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn url_scheme_and_host_for_log_serves_scheme_and_host_only_and_never_query_payload_or_credentials(
    ) {
        assert_eq!(
            url_scheme_and_host_for_log("https://apps.roblox.com/challenge/verify?token=SECRET"),
            "https://apps.roblox.com"
        );

        assert_eq!(
            url_scheme_and_host_for_log("https://user:pass@host/x"),
            "https://host"
        );
        assert_eq!(
            url_scheme_and_host_for_log("https://host:8443/path#frag"),
            "https://host:8443"
        );

        assert_eq!(
            url_scheme_and_host_for_log("https://host?token=SECRET"),
            "https://host"
        );

        assert_eq!(url_scheme_and_host_for_log("about:blank"), "<non-url>");

        let plain_data = url_scheme_and_host_for_log("data:text/html,<html>SECRET</html>");
        assert_eq!(plain_data, "<non-url>");
        assert!(!plain_data.contains("SECRET"));

        let embedded = url_scheme_and_host_for_log(
            "data:text/html,<a href=\"https://x?token=SECRET\">click</a>",
        );
        assert_eq!(embedded, "<non-url>");
        assert!(!embedded.contains("SECRET"));

        assert_eq!(url_scheme_and_host_for_log(""), "<non-url>");
        assert_eq!(url_scheme_and_host_for_log("no scheme here"), "<non-url>");
        assert_eq!(url_scheme_and_host_for_log("://host"), "<non-url>");
        assert_eq!(url_scheme_and_host_for_log("1https://host"), "<non-url>");

        for input in [
            "https://apps.roblox.com/challenge/verify?token=SECRET",
            "https://user:pass@host/x",
            "https://host?token=SECRET",
            "about:blank",
            "data:text/html,<html>SECRET</html>",
            "data:text/html,<a href=\"https://x?token=SECRET\">click</a>",
        ] {
            let out = url_scheme_and_host_for_log(input);
            assert!(
                !out.contains('?') && !out.contains('#') && !out.contains('@'),
                "redacted output {out:?} for {input:?} leaked a query/fragment/userinfo marker"
            );
            assert!(
                !out.contains("SECRET") && !out.contains("pass") && !out.contains("/challenge"),
                "redacted output {out:?} for {input:?} leaked a sensitive substring"
            );
        }
    }

    #[test]
    fn redact_urls_for_log_keeps_only_scheme_and_host_of_each_http_url_in_text() {
        assert_eq!(
            redact_urls_for_log("open https://www.roblox.com/login?token=abc&x=1 now"),
            "open https://www.roblox.com now"
        );
        assert_eq!(
            redact_urls_for_log("a http://user:pw@a.example/p b HTTPS://B.example:8443/q?k=v#f"),
            "a http://a.example b HTTPS://B.example:8443"
        );
        assert_eq!(
            redact_urls_for_log("url:{ \"https://apis.roblox.com/v1/users/1?t=2\" } ip:1"),
            "url:{ \"https://apis.roblox.com\" } ip:1"
        );
        assert_eq!(
            redact_urls_for_log("id=1https://a.example/?t=1\tend"),
            "id=1https://a.example\tend"
        );
        assert_eq!(
            redact_urls_for_log("https://a.example/r?next=https://b.example/?t=1"),
            "https://a.example"
        );
        assert_eq!(
            redact_urls_for_log("data:text/html,<a href=\"https://x?token=SECRET\">click</a>"),
            "data:text/html,<a href=\"https://x\">click</a>"
        );
    }

    #[test]
    fn redact_urls_for_log_borrows_text_without_http_urls_unchanged() {
        for text in [
            "",
            "Roblox started in 2.1 s",
            "rbxasset://textures/ui/close.png",
            "ratio 3:// odd",
            "日本://語",
        ] {
            assert!(
                matches!(redact_urls_for_log(text), Cow::Borrowed(same) if same == text),
                "{text:?}"
            );
        }
    }
}
