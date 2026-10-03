#![forbid(unsafe_code)]

#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct ViewHandler {
    pub(super) activity: String,
    pub(super) schemes: Vec<String>,
    pub(super) hosts: Vec<String>,
    pub(super) paths: Vec<PathMatcher>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub(super) enum PathMatcher {
    Literal(String),
    Prefix(String),
    SimpleGlob(String),
}

pub(super) struct ViewUri<'a> {
    scheme: &'a str,
    host: Option<&'a str>,
    path: &'a str,
}

impl<'a> ViewUri<'a> {
    pub(super) fn parse(uri: &'a str) -> Option<Self> {
        let (scheme, rest) = uri.split_once(':')?;
        let hierarchy = rest.find(['?', '#']).map_or(rest, |end| &rest[..end]);
        let Some(authority_and_path) = hierarchy.strip_prefix("//") else {
            return Some(Self {
                scheme,
                host: None,
                path: hierarchy,
            });
        };
        let path_start = authority_and_path
            .find('/')
            .unwrap_or(authority_and_path.len());
        let (host, path) = authority_and_path.split_at(path_start);
        Some(Self {
            scheme,
            host: Some(host),
            path,
        })
    }
}

impl ViewHandler {
    pub(super) fn accepts(&self, uri: &ViewUri<'_>) -> bool {
        if !self.schemes.iter().any(|scheme| scheme == uri.scheme) {
            return false;
        }
        if self.hosts.is_empty() {
            return true;
        }
        let Some(host) = uri.host else {
            return false;
        };
        self.hosts
            .iter()
            .any(|known| known.eq_ignore_ascii_case(host))
            && (self.paths.is_empty() || self.paths.iter().any(|path| path.matches(uri.path)))
    }
}

impl PathMatcher {
    fn matches(&self, path: &str) -> bool {
        match self {
            Self::Literal(literal) => path == literal,
            Self::Prefix(prefix) => path.starts_with(prefix.as_str()),
            Self::SimpleGlob(pattern) => simple_glob_matches(pattern, path),
        }
    }
}

fn simple_glob_matches(pattern: &str, text: &str) -> bool {
    let pattern: Vec<char> = pattern.chars().collect();
    let text: Vec<char> = text.chars().collect();
    let pattern_at = |index: usize| pattern.get(index).copied().unwrap_or('\0');
    let (pattern_len, text_len) = (pattern.len(), text.len());
    if pattern_len == 0 {
        return text_len == 0;
    }
    let (mut ip, mut it) = (0, 0);
    let mut next = pattern[0];
    while ip < pattern_len && it < text_len {
        let mut current = next;
        ip += 1;
        next = pattern_at(ip);
        let escaped = current == '\\';
        if escaped {
            current = next;
            ip += 1;
            next = pattern_at(ip);
        }
        if next != '*' {
            if current != '.' && text[it] != current {
                return false;
            }
            it += 1;
        } else if !escaped && current == '.' {
            if ip + 1 >= pattern_len {
                return true;
            }
            ip += 1;
            next = pattern[ip];
            if next == '\\' {
                ip += 1;
                next = pattern_at(ip);
            }
            while it < text_len && text[it] != next {
                it += 1;
            }
            if it == text_len {
                return false;
            }
            ip += 1;
            next = pattern_at(ip);
            it += 1;
        } else {
            while it < text_len && text[it] == current {
                it += 1;
            }
            ip += 1;
            next = pattern_at(ip);
        }
    }
    (ip >= pattern_len && it >= text_len)
        || (ip + 2 == pattern_len && pattern[ip] == '.' && pattern[ip + 1] == '*')
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn simple_globs_match_as_android_pattern_matcher_does() {
        let cases = [
            ("/games/..*", "/games/1", true),
            ("/games/..*", "/games/1818/Slug", true),
            ("/games/..*", "/games/", false),
            ("/games/..*", "/games", false),
            ("/.*/games/..*", "/fr/games/1", true),
            ("/.*/games/..*", "/games/1", false),
            ("/.*/join/*", "/fr/join/", true),
            ("/.*/join/*", "/fr/join//", true),
            ("/commerce/app-redirect", "/commerce/app-redirect", true),
            ("/commerce/app-redirect", "/commerce/app-redirect/", false),
            ("/x*y", "/y", true),
            ("/x*y", "/xxxy", true),
            ("/x*y", "/xzy", false),
            ("/a.c", "/abc", true),
            ("/a.*b", "/axb", true),
            ("/a\\.*b", "/a..b", true),
            ("/a\\.*b", "/axb", false),
            ("/a\\*", "/a*", true),
            ("/a\\*", "/ab", false),
            ("/a.*", "/a", true),
            ("", "", true),
            ("", "/", false),
        ];
        for (pattern, path, expected) in cases {
            assert_eq!(
                simple_glob_matches(pattern, path),
                expected,
                "{pattern} against {path}"
            );
        }
    }

    #[test]
    fn uris_split_into_scheme_host_and_path_without_query_or_fragment() {
        let parts = |uri| ViewUri::parse(uri).map(|parts| (parts.scheme, parts.host, parts.path));
        assert_eq!(
            parts("https://www.roblox.com/share?code=1/2#x"),
            Some(("https", Some("www.roblox.com"), "/share"))
        );
        assert_eq!(
            parts("roblox://placeId=1818&launchData=a%2Fb"),
            Some(("roblox", Some("placeId=1818&launchData=a%2Fb"), ""))
        );
        assert_eq!(
            parts("roblox://placeId=1?x/y"),
            Some(("roblox", Some("placeId=1"), ""))
        );
        assert_eq!(parts("mailto:a@b"), Some(("mailto", None, "a@b")));
        assert_eq!(parts("no scheme"), None);
    }
}
