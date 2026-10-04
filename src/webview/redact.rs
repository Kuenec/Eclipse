#![forbid(unsafe_code)]

use std::borrow::Cow;
use std::ops::Range;

pub const NON_URL: &str = "<non-url>";

const SCHEME_SEPARATOR: &str = "://";
const REDACTED_SCHEMES: [&str; 2] = ["https", "http"];

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Schemes {
    Web,
    Any,
}

impl Schemes {
    fn start_in(self, before_separator: &str) -> Option<usize> {
        match self {
            Self::Web => web_scheme_start(before_separator),
            Self::Any => any_scheme_start(before_separator),
        }
    }
}

pub struct UrlSpans<'a> {
    text: &'a str,
    schemes: Schemes,
    cursor: usize,
    floor: usize,
}

impl Iterator for UrlSpans<'_> {
    type Item = Range<usize>;

    fn next(&mut self) -> Option<Range<usize>> {
        while let Some(offset) = self.text[self.cursor..].find(SCHEME_SEPARATOR) {
            let separator = self.cursor + offset;
            let authority = separator + SCHEME_SEPARATOR.len();
            self.cursor = authority;
            let Some(start) = self.schemes.start_in(&self.text[self.floor..separator]) else {
                continue;
            };
            let start = self.floor + start;
            let end = self.text[authority..]
                .find(ends_url)
                .map_or(self.text.len(), |length| authority + length);
            self.floor = end;
            self.cursor = end;
            return Some(start..end);
        }
        None
    }
}

pub fn url_spans(text: &str, schemes: Schemes) -> UrlSpans<'_> {
    UrlSpans {
        text,
        schemes,
        cursor: 0,
        floor: 0,
    }
}

pub fn redact_urls_for_log(text: &str) -> Cow<'_, str> {
    let mut redacted = String::new();
    let mut copied = 0;
    for span in url_spans(text, Schemes::Web) {
        redacted.push_str(&text[copied..span.start]);
        redacted.push_str(&url_scheme_and_host_for_log(&text[span.clone()]));
        copied = span.end;
    }
    if copied == 0 {
        return Cow::Borrowed(text);
    }
    redacted.push_str(&text[copied..]);
    Cow::Owned(redacted)
}

fn web_scheme_start(before_separator: &str) -> Option<usize> {
    REDACTED_SCHEMES.iter().find_map(|scheme| {
        let start = before_separator.len().checked_sub(scheme.len())?;
        before_separator
            .get(start..)?
            .eq_ignore_ascii_case(scheme)
            .then_some(start)
    })
}

fn any_scheme_start(before_separator: &str) -> Option<usize> {
    let run = before_separator
        .bytes()
        .rev()
        .take_while(|byte| byte.is_ascii_alphanumeric() || matches!(byte, b'+' | b'-' | b'.'))
        .count();
    let run_start = before_separator.len() - run;
    before_separator[run_start..]
        .bytes()
        .position(|byte| byte.is_ascii_alphabetic())
        .map(|offset| run_start + offset)
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
    fn url_spans_of_any_scheme_cover_each_link_and_skip_non_schemes() {
        let text = "open roblox://placeId=1818&accessCode=x, then rbxasset://textures/a.png \
                    and 1https://a.example/?t=1 but not 3:// or 日本://語";
        let spans: Vec<&str> = url_spans(text, Schemes::Any)
            .map(|span| &text[span])
            .collect();
        assert_eq!(
            spans,
            [
                "roblox://placeId=1818&accessCode=x,",
                "rbxasset://textures/a.png",
                "https://a.example/?t=1",
            ]
        );
        let web: Vec<&str> = url_spans(text, Schemes::Web)
            .map(|span| &text[span])
            .collect();
        assert_eq!(web, ["https://a.example/?t=1"]);
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
