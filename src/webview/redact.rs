//! The absolute URL-redaction contract: scheme+host only, in BOTH processes.
//!
//! 2026-07-03 (plan M2): [`url_scheme_and_host_for_log`] + [`NON_URL`] moved VERBATIM here from
//! `src/framework.rs` (the 2026-07-02 WebView-natives privacy guard) so the out-of-process
//! `eclipse-webview` helper shares the ONE canonical implementation via `#[path]`
//! ("integrates, never duplicates"). `framework.rs` imports these; every call site is
//! unchanged. Load payloads are never bound to any log macro at any level — this function is
//! the only form a WebView/helper log target may take.

#![forbid(unsafe_code)]

/// The safe literal every unparseable/null/unreadable WebView log target redacts to — one const so
/// the log vocabulary cannot drift between the helper and the native bodies' fallbacks (2026-07-02).
pub const NON_URL: &str = "<non-url>";

/// Redact a URL down to `scheme://host` for logging — the ONLY form a WebView target may take in
/// Eclipse's log. Returns the safe literal [`NON_URL`] for anything that does not parse.
///
/// 2026-07-02: challenge URLs may carry session tokens — scheme+host only, never
/// path/query/userinfo/payload; `loadDataWithBaseURL` data payloads are never logged at any level.
/// The rules (each closes a verified leak shape; see the redaction test):
///   1. The literal `"://"` is REQUIRED — absent → `"<non-url>"`. Covers `about:blank` (a
///      GUARANTEED input: the installed `loadData` hardcodes it as baseUrl/history, WebView.smali
///      :238/:240) and payload-bearing `data:` URLs (no `"://"` → the payload can never leak).
///   2. The text before the FIRST `"://"` must be a valid RFC 3986 scheme (ASCII letter first,
///      then only ASCII alphanumerics/`+`/`-`/`.`) — otherwise `"<non-url>"`. Closes the
///      embedded-`"://"` shape: a `data:` payload containing `https://…` has payload text (invalid
///      scheme chars) before its first `"://"`, so no payload substring is ever emitted.
///   3. The authority is cut at the FIRST of `'/'`, `'?'`, `'#'` after `"://"` — a path-less
///      `https://host?token=SECRET` yields exactly `https://host`.
///   4. Userinfo is stripped at the LAST `'@'` within that bounded authority only
///      (`https://user:pass@host/x` → `https://host`); a port, if present, stays (not sensitive).
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
        // 2026-07-02: the WebView load natives' privacy guard — challenge URLs may carry session
        // tokens and loadDataWithBaseURL baseUrls may be data: payloads, so every logged target must
        // reduce to scheme+host (or the safe literal "<non-url>"). Each case below is a verified
        // leak shape from the plan review; a regression here logs secrets to the boot log.
        // (2026-07-03: test moved intact with the function from framework.rs — plan M2.)
        //
        // The happy path: path/query/fragment stripped, host kept.
        assert_eq!(
            url_scheme_and_host_for_log("https://apps.roblox.com/challenge/verify?token=SECRET"),
            "https://apps.roblox.com"
        );
        // Userinfo stripped (last '@' within the bounded authority); port survives.
        assert_eq!(
            url_scheme_and_host_for_log("https://user:pass@host/x"),
            "https://host"
        );
        assert_eq!(
            url_scheme_and_host_for_log("https://host:8443/path#frag"),
            "https://host:8443"
        );
        // Path-less query URL: the authority is cut at the FIRST of '/', '?', '#' — the query
        // string (token) must never ride along as part of the "host".
        assert_eq!(
            url_scheme_and_host_for_log("https://host?token=SECRET"),
            "https://host"
        );
        // about:blank — a GUARANTEED input: the installed loadData hardcodes it as the
        // baseUrl/history args (classes3 WebView.smali :238/:240/:250). No "://" → "<non-url>".
        assert_eq!(url_scheme_and_host_for_log("about:blank"), "<non-url>");
        // A payload-bearing data: URL — no "://" → "<non-url>"; the payload never leaks.
        let plain_data = url_scheme_and_host_for_log("data:text/html,<html>SECRET</html>");
        assert_eq!(plain_data, "<non-url>");
        assert!(!plain_data.contains("SECRET"));
        // The embedded-"://" shape: the first "://" sits INSIDE the payload, so the text before it
        // fails the RFC 3986 scheme-charset rule → "<non-url>", never a payload substring.
        let embedded = url_scheme_and_host_for_log(
            "data:text/html,<a href=\"https://x?token=SECRET\">click</a>",
        );
        assert_eq!(embedded, "<non-url>");
        assert!(!embedded.contains("SECRET"));
        // Garbage / empty / scheme-less inputs → "<non-url>" (total, never panics).
        assert_eq!(url_scheme_and_host_for_log(""), "<non-url>");
        assert_eq!(url_scheme_and_host_for_log("no scheme here"), "<non-url>");
        assert_eq!(url_scheme_and_host_for_log("://host"), "<non-url>");
        assert_eq!(url_scheme_and_host_for_log("1https://host"), "<non-url>");
        // No output ever contains a query, fragment, userinfo marker, path segment, or the
        // sensitive literals from the inputs above.
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
}
