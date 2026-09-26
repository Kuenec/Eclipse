use std::fmt;
use std::fs::File;
use std::io::{self, Read, Write};
use std::path::{Path, PathBuf};
use std::time::{Duration, Instant};

use ureq::http::header::{CONTENT_LENGTH, LOCATION};
use ureq::http::{HeaderMap, Uri};

const REQUEST_TIMEOUT: Duration = Duration::from_secs(60);
const CONNECT_TIMEOUT: Duration = Duration::from_secs(30);
const RESPONSE_TIMEOUT: Duration = Duration::from_secs(60);
const BODY_TIMEOUT: Duration = Duration::from_secs(30 * 60);
const MAX_REDIRECTS: usize = 5;
const BUFFER_BYTES: usize = 256 * 1024;
const PROGRESS_INTERVAL: Duration = Duration::from_secs(5);
const MEBIBYTE: f64 = 1024.0 * 1024.0;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Host {
    Exact(&'static str),
    SubdomainOf(&'static str),
}

impl Host {
    fn admits(self, host: &str) -> bool {
        match self {
            Self::Exact(name) => host == name,
            Self::SubdomainOf(domain) => host
                .strip_suffix(domain)
                .and_then(|subdomain| subdomain.strip_suffix('.'))
                .is_some_and(|label| !label.is_empty()),
        }
    }
}

impl fmt::Display for Host {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Exact(name) => f.write_str(name),
            Self::SubdomainOf(domain) => write!(f, "*.{domain}"),
        }
    }
}

pub(super) fn request_agent() -> ureq::Agent {
    ureq::Agent::config_builder()
        .https_only(true)
        .http_status_as_error(false)
        .max_redirects(0)
        .timeout_global(Some(REQUEST_TIMEOUT))
        .build()
        .into()
}

pub(super) fn download_agent() -> ureq::Agent {
    ureq::Agent::config_builder()
        .https_only(true)
        .http_status_as_error(false)
        .max_redirects(0)
        .timeout_connect(Some(CONNECT_TIMEOUT))
        .timeout_recv_response(Some(RESPONSE_TIMEOUT))
        .timeout_recv_body(Some(BODY_TIMEOUT))
        .build()
        .into()
}

pub(super) fn redacted(error: ureq::Error) -> String {
    match error {
        ureq::Error::BadUri(_) => "the URL is invalid".to_owned(),
        other => other.to_string(),
    }
}

pub(super) fn trusted_uri(url: &str, allowed: &'static [Host]) -> Result<Uri, DownloadError> {
    let untrusted = || DownloadError::Untrusted { allowed };
    let uri = Uri::try_from(url).map_err(|_| untrusted())?;
    let authority = uri.authority().ok_or_else(untrusted)?;
    let host = authority.host().to_ascii_lowercase();
    let trusted = uri.scheme_str() == Some("https")
        && !authority.as_str().contains('@')
        && authority.port_u16().is_none_or(|port| port == 443)
        && allowed.iter().any(|rule| rule.admits(&host));
    if trusted {
        Ok(uri)
    } else {
        Err(untrusted())
    }
}

pub(super) struct Download {
    pub(super) body: ureq::Body,
    pub(super) content_length: Option<u64>,
}

pub(super) fn open_download(
    agent: &ureq::Agent,
    url: &str,
    allowed: &'static [Host],
) -> Result<Download, DownloadError> {
    let mut route = Route::new(allowed);
    let mut uri = trusted_uri(url, allowed)?;
    loop {
        let response = agent
            .get(uri)
            .call()
            .map_err(|error| DownloadError::Transport(redacted(error)))?;
        match route.next(response.status().as_u16(), response.headers())? {
            Hop::Arrived => {
                return Ok(Download {
                    content_length: content_length(response.headers())?,
                    body: response.into_body(),
                })
            }
            Hop::Redirect(next) => uri = next,
        }
    }
}

#[derive(Debug, PartialEq, Eq)]
enum Hop {
    Arrived,
    Redirect(Uri),
}

struct Route {
    allowed: &'static [Host],
    redirects: usize,
}

impl Route {
    fn new(allowed: &'static [Host]) -> Self {
        Self {
            allowed,
            redirects: 0,
        }
    }

    fn next(&mut self, status: u16, headers: &HeaderMap) -> Result<Hop, DownloadError> {
        if (200..300).contains(&status) {
            return Ok(Hop::Arrived);
        }
        if !(300..400).contains(&status) {
            return Err(DownloadError::Status(status));
        }
        self.redirects += 1;
        if self.redirects > MAX_REDIRECTS {
            return Err(DownloadError::TooManyRedirects);
        }
        let location = headers
            .get(LOCATION)
            .and_then(|location| location.to_str().ok())
            .ok_or(DownloadError::Untrusted {
                allowed: self.allowed,
            })?;
        trusted_uri(location, self.allowed).map(Hop::Redirect)
    }
}

fn content_length(headers: &HeaderMap) -> Result<Option<u64>, DownloadError> {
    headers
        .get(CONTENT_LENGTH)
        .map(|value| {
            value
                .to_str()
                .ok()
                .and_then(|value| value.parse().ok())
                .ok_or(DownloadError::InvalidContentLength)
        })
        .transpose()
}

pub(super) fn save_download(
    body: impl Read,
    content_length: Option<u64>,
    limit: u64,
    dest: &Path,
    mut observe: impl FnMut(&[u8]),
) -> Result<u64, DownloadError> {
    if content_length.is_some_and(|length| length > limit) {
        return Err(DownloadError::TooLarge { limit });
    }
    let io_error = |source| DownloadError::Io {
        path: dest.to_path_buf(),
        source,
    };
    let mut output = File::create(dest).map_err(io_error)?;
    let mut reader = body.take(limit.saturating_add(1));
    let mut buffer = vec![0u8; BUFFER_BYTES];
    let mut progress = Progress::new(content_length);
    let mut total = 0u64;
    loop {
        let read = reader
            .read(&mut buffer)
            .map_err(DownloadError::Interrupted)?;
        if read == 0 {
            break;
        }
        total += read as u64;
        if total > limit {
            return Err(DownloadError::TooLarge { limit });
        }
        observe(&buffer[..read]);
        output.write_all(&buffer[..read]).map_err(io_error)?;
        progress.report(total);
    }
    output.sync_all().map_err(io_error)?;
    if let Some(announced) = content_length.filter(|&length| length != total) {
        return Err(DownloadError::LengthMismatch {
            announced,
            actual: total,
        });
    }
    Ok(total)
}

struct Progress {
    total: Option<u64>,
    last: Instant,
}

impl Progress {
    fn new(total: Option<u64>) -> Self {
        Self {
            total,
            last: Instant::now(),
        }
    }

    fn report(&mut self, done: u64) {
        if self.last.elapsed() < PROGRESS_INTERVAL {
            return;
        }
        self.last = Instant::now();
        eprintln!("{}", progress_line(done, self.total));
    }
}

fn progress_line(done: u64, total: Option<u64>) -> String {
    let mebibytes = |bytes: u64| bytes as f64 / MEBIBYTE;
    match total {
        Some(total) if total > 0 => format!(
            "# Downloaded {:.1} of {:.1} MiB ({}%)",
            mebibytes(done),
            mebibytes(total),
            done.saturating_mul(100) / total
        ),
        _ => format!("# Downloaded {:.1} MiB", mebibytes(done)),
    }
}

#[derive(Debug)]
pub enum DownloadError {
    Untrusted { allowed: &'static [Host] },

    Transport(String),

    Status(u16),

    TooManyRedirects,

    InvalidContentLength,

    TooLarge { limit: u64 },

    LengthMismatch { announced: u64, actual: u64 },

    Interrupted(io::Error),

    Io { path: PathBuf, source: io::Error },
}

impl fmt::Display for DownloadError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Untrusted { allowed } => {
                f.write_str("the download link is not an HTTPS URL on ")?;
                for (index, host) in allowed.iter().enumerate() {
                    if index > 0 {
                        f.write_str(" or ")?;
                    }
                    host.fmt(f)?;
                }
                Ok(())
            }
            Self::Transport(detail) => f.write_str(detail),
            Self::Status(status) => write!(f, "the server answered HTTP {status}"),
            Self::TooManyRedirects => {
                write!(f, "the server redirected more than {MAX_REDIRECTS} times")
            }
            Self::InvalidContentLength => f.write_str("the server sent an invalid Content-Length"),
            Self::TooLarge { limit } => write!(f, "the file is larger than {limit} bytes"),
            Self::LengthMismatch { announced, actual } => write!(
                f,
                "the server announced {announced} bytes but sent {actual}"
            ),
            Self::Interrupted(source) => source.fmt(f),
            Self::Io { path, source } => write!(f, "{}: {source}", path.display()),
        }
    }
}

impl std::error::Error for DownloadError {
    fn source(&self) -> Option<&(dyn std::error::Error + 'static)> {
        match self {
            Self::Interrupted(source) | Self::Io { source, .. } => Some(source),
            Self::Untrusted { .. }
            | Self::Transport(_)
            | Self::Status(_)
            | Self::TooManyRedirects
            | Self::InvalidContentLength
            | Self::TooLarge { .. }
            | Self::LengthMismatch { .. } => None,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    const HOSTS: &[Host] = &[
        Host::Exact("files.example.com"),
        Host::SubdomainOf("cdn.example.net"),
    ];

    fn temp_dir(tag: &str) -> PathBuf {
        let dir = std::env::temp_dir().join(format!(
            "eclipse-https-test-{tag}-{:?}",
            std::thread::current().id()
        ));
        std::fs::remove_dir_all(&dir).ok();
        std::fs::create_dir_all(&dir).expect("create the test directory");
        dir
    }

    fn redirect_to(location: &str) -> HeaderMap {
        let mut headers = HeaderMap::new();
        headers.insert(LOCATION, location.parse().unwrap());
        headers
    }

    #[test]
    fn only_https_links_on_the_allowed_hosts_are_trusted() {
        for url in [
            "https://files.example.com/roblox.xapk",
            "https://FILES.EXAMPLE.COM:443/roblox.xapk",
            "https://a.cdn.example.net/roblox.apk",
            "https://a.b.cdn.example.net/roblox.apk?token=1",
        ] {
            assert!(trusted_uri(url, HOSTS).is_ok(), "{url}");
        }
        for url in [
            "http://files.example.com/x",
            "https://sub.files.example.com/x",
            "https://files.example.com.evil.example/x",
            "https://cdn.example.net/x",
            "https://evilcdn.example.net/x",
            "https://.cdn.example.net/x",
            "https://user@files.example.com/x",
            "https://files.example.com:8443/x",
            "/relative/redirect",
            "not a url",
        ] {
            let err = trusted_uri(url, HOSTS).unwrap_err();
            assert!(matches!(err, DownloadError::Untrusted { .. }), "{url}");
            assert!(!err.to_string().contains(url), "{err}");
        }
        assert_eq!(
            trusted_uri("http://x", HOSTS).unwrap_err().to_string(),
            "the download link is not an HTTPS URL on files.example.com or *.cdn.example.net"
        );
    }

    #[test]
    fn redirects_are_followed_only_to_trusted_hosts_and_only_five_times() {
        let mut route = Route::new(HOSTS);
        assert_eq!(route.next(200, &HeaderMap::new()).unwrap(), Hop::Arrived);
        for _ in 0..MAX_REDIRECTS {
            assert_eq!(
                route
                    .next(302, &redirect_to("https://x.cdn.example.net/next"))
                    .unwrap(),
                Hop::Redirect(Uri::from_static("https://x.cdn.example.net/next"))
            );
        }
        assert!(matches!(
            route.next(302, &redirect_to("https://x.cdn.example.net/next")),
            Err(DownloadError::TooManyRedirects)
        ));

        for (status, headers) in [
            (301, HeaderMap::new()),
            (302, redirect_to("/relative")),
            (307, redirect_to("https://evil.example/roblox.apk")),
            (308, redirect_to("http://files.example.com/roblox.apk")),
        ] {
            let err = Route::new(HOSTS).next(status, &headers).unwrap_err();
            assert!(
                matches!(err, DownloadError::Untrusted { .. }),
                "{status} {headers:?}: {err:?}"
            );
        }
        for status in [100, 403, 404, 500] {
            let err = Route::new(HOSTS)
                .next(status, &HeaderMap::new())
                .unwrap_err();
            assert!(
                matches!(err, DownloadError::Status(code) if code == status),
                "{status}: {err:?}"
            );
        }
    }

    #[test]
    fn content_length_is_absent_a_number_or_an_error() {
        let mut headers = HeaderMap::new();
        assert_eq!(content_length(&headers).unwrap(), None);
        headers.insert(CONTENT_LENGTH, "245807054".parse().unwrap());
        assert_eq!(content_length(&headers).unwrap(), Some(245_807_054));
        headers.insert(CONTENT_LENGTH, "-1".parse().unwrap());
        assert!(matches!(
            content_length(&headers),
            Err(DownloadError::InvalidContentLength)
        ));
    }

    struct FailingBody;

    impl Read for FailingBody {
        fn read(&mut self, _: &mut [u8]) -> io::Result<usize> {
            Err(io::Error::new(io::ErrorKind::TimedOut, "timeout: RecvBody"))
        }
    }

    #[test]
    fn downloads_are_saved_within_the_limit_and_the_announced_length() {
        let dir = temp_dir("save");
        let dest = dir.join("download");
        let body = [7u8; 100];
        let mut seen = Vec::new();

        let total = save_download(&body[..], Some(100), 100, &dest, |chunk| {
            seen.extend_from_slice(chunk)
        })
        .unwrap();
        assert_eq!(total, 100);
        assert_eq!(std::fs::read(&dest).unwrap(), body);
        assert_eq!(seen, body, "every saved byte is observed");
        assert_eq!(
            save_download(&body[..60], None, 100, &dest, |_| {}).unwrap(),
            60
        );

        let err = save_download(&body[..], Some(101), 100, &dest, |_| {}).unwrap_err();
        assert!(
            matches!(err, DownloadError::TooLarge { limit: 100 }),
            "{err:?}"
        );
        let err = save_download(&body[..], None, 99, &dest, |_| {}).unwrap_err();
        assert!(
            matches!(err, DownloadError::TooLarge { limit: 99 }),
            "{err:?}"
        );
        let err = save_download(&body[..95], Some(100), 100, &dest, |_| {}).unwrap_err();
        assert!(
            matches!(
                err,
                DownloadError::LengthMismatch {
                    announced: 100,
                    actual: 95
                }
            ),
            "{err:?}"
        );
        let err = save_download(FailingBody, None, 100, &dest, |_| {}).unwrap_err();
        assert!(matches!(err, DownloadError::Interrupted(_)), "{err:?}");
        let err =
            save_download(&body[..], None, 100, &dir.join("missing/download"), |_| {}).unwrap_err();
        assert!(matches!(err, DownloadError::Io { .. }), "{err:?}");
        std::fs::remove_dir_all(&dir).ok();
    }

    #[test]
    fn progress_lines_report_mebibytes_and_percent() {
        assert_eq!(
            progress_line(120 * 1024 * 1024, Some(240 * 1024 * 1024)),
            "# Downloaded 120.0 of 240.0 MiB (50%)"
        );
        assert_eq!(
            progress_line(1024 * 1024 + 512 * 1024, None),
            "# Downloaded 1.5 MiB"
        );
        assert_eq!(progress_line(0, Some(0)), "# Downloaded 0.0 MiB");
    }

    #[test]
    fn transport_errors_never_echo_urls() {
        let detail = redacted(ureq::Error::BadUri(
            "https://files.example.com/?X-Amz-Signature=secret".to_owned(),
        ));
        assert!(!detail.contains("secret"), "{detail}");
    }
}
