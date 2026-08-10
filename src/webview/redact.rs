#![forbid(unsafe_code)]

pub const NON_URL: &str = "<non-url>";

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
}
