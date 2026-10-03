#![forbid(unsafe_code)]

use std::fmt;
use std::fs::File;
use std::io::{self, Read, Write};
use std::path::{Path, PathBuf};
use std::time::{Duration, Instant};

use ureq::http::header::{CONTENT_LENGTH, CONTENT_RANGE, ETAG, IF_RANGE, LOCATION, RANGE};
use ureq::http::{HeaderMap, Uri};

use crate::status::{continuation_text, transfer_text, StatusSink};

const REQUEST_TIMEOUT: Duration = Duration::from_secs(60);
const REQUEST_CONNECT_TIMEOUT: Duration = Duration::from_secs(10);
const CONNECT_TIMEOUT: Duration = Duration::from_secs(30);
const RESPONSE_TIMEOUT: Duration = Duration::from_secs(60);
const REQUEST_BODY_BUDGET: Duration = Duration::from_secs(2 * 60);
const MAX_CONTINUATIONS: u32 = 256;
const MAX_REDIRECTS: usize = 5;
const BUFFER_BYTES: usize = 256 * 1024;
const PROGRESS_INTERVAL: Duration = Duration::from_secs(5);
const WINDOW_PROGRESS_INTERVAL: Duration = Duration::from_millis(250);
const PARTIAL_CONTENT: u16 = 206;

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

pub(crate) fn request_agent() -> ureq::Agent {
    ureq::Agent::config_builder()
        .https_only(true)
        .http_status_as_error(false)
        .max_redirects(0)
        .timeout_global(Some(REQUEST_TIMEOUT))
        .timeout_connect(Some(REQUEST_CONNECT_TIMEOUT))
        .build()
        .into()
}

pub(crate) fn download_agent() -> ureq::Agent {
    ureq::Agent::config_builder()
        .https_only(true)
        .http_status_as_error(false)
        .max_redirects(0)
        .timeout_connect(Some(CONNECT_TIMEOUT))
        .timeout_recv_response(Some(RESPONSE_TIMEOUT))
        .timeout_recv_body(Some(REQUEST_BODY_BUDGET))
        .build()
        .into()
}

pub(crate) fn redacted(error: ureq::Error) -> String {
    match error {
        ureq::Error::BadUri(_) => "the URL is invalid".to_owned(),
        other => other.to_string(),
    }
}

pub(crate) fn trusted_uri(url: &str, allowed: &'static [Host]) -> Result<Uri, DownloadError> {
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

pub(crate) struct Download<R> {
    pub(crate) body: R,
    pub(crate) extent: Extent,
    pub(crate) validator: Option<String>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum Extent {
    Whole { length: Option<u64> },
    Partial(ContentRange),
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) struct ContentRange {
    pub(crate) start: u64,
    pub(crate) total: Option<u64>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct Resume {
    pub(crate) offset: u64,
    pub(crate) validator: Option<String>,
}

pub(crate) fn open_download(
    agent: &ureq::Agent,
    url: &str,
    allowed: &'static [Host],
    resume: Option<&Resume>,
) -> Result<Download<ureq::BodyReader<'static>>, DownloadError> {
    let mut route = Route::new(allowed);
    let mut uri = trusted_uri(url, allowed)?;
    loop {
        let mut request = agent.get(uri);
        if let Some(resume) = resume {
            request = request.header(RANGE, format!("bytes={}-", resume.offset));
            if let Some(validator) = &resume.validator {
                request = request.header(IF_RANGE, validator.as_str());
            }
        }
        let response = request
            .call()
            .map_err(|error| DownloadError::Transport(redacted(error)))?;
        let status = response.status().as_u16();
        match route.next(status, response.headers())? {
            Hop::Arrived => {
                let headers = response.headers();
                let extent = if status == PARTIAL_CONTENT {
                    Extent::Partial(content_range(headers)?)
                } else {
                    Extent::Whole {
                        length: content_length(headers)?,
                    }
                };
                let validator = strong_etag(headers);
                return Ok(Download {
                    body: response.into_body().into_reader(),
                    extent,
                    validator,
                });
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

fn strong_etag(headers: &HeaderMap) -> Option<String> {
    headers
        .get(ETAG)
        .and_then(|value| value.to_str().ok())
        .filter(|etag| etag.starts_with('"'))
        .map(str::to_owned)
}

fn content_range(headers: &HeaderMap) -> Result<ContentRange, DownloadError> {
    headers
        .get(CONTENT_RANGE)
        .and_then(|value| value.to_str().ok())
        .and_then(parse_content_range)
        .ok_or(DownloadError::InvalidContentRange)
}

fn parse_content_range(value: &str) -> Option<ContentRange> {
    let (range, total) = value.strip_prefix("bytes ")?.split_once('/')?;
    let (start, end) = range.split_once('-')?;
    let start: u64 = start.parse().ok()?;
    let end: u64 = end.parse().ok()?;
    let total = match total {
        "*" => None,
        total => {
            let total: u64 = total.parse().ok()?;
            if end >= total {
                return None;
            }
            Some(total)
        }
    };
    (start <= end).then_some(ContentRange { start, total })
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

pub(crate) fn save_download<R: Read, E: From<DownloadError>>(
    mut open: impl FnMut(Option<&Resume>) -> Result<Download<R>, E>,
    limit: u64,
    dest: &Path,
    status: &StatusSink,
    mut observe: impl FnMut(&[u8]),
) -> Result<u64, E> {
    let first = open(None)?;
    let Extent::Whole { length } = first.extent else {
        return Err(DownloadError::UnrequestedRange.into());
    };
    if length.is_some_and(|length| length > limit) {
        return Err(DownloadError::TooLarge { limit }.into());
    }
    let io_error = |source| DownloadError::Io {
        path: dest.to_path_buf(),
        source,
    };
    let mut output = File::create(dest).map_err(io_error)?;
    let mut buffer = vec![0u8; BUFFER_BYTES];
    let mut progress = Progress::new(length, status);
    let mut body = first.body;
    let mut total = 0u64;
    let mut continuations = 0u32;
    loop {
        let attempt_start = total;
        let interruption = loop {
            let read = match body.read(&mut buffer) {
                Ok(read) => read,
                Err(error) if error.kind() == io::ErrorKind::Interrupted => continue,
                Err(error) => break Some(error),
            };
            if read == 0 {
                break None;
            }
            total += read as u64;
            if total > limit {
                return Err(DownloadError::TooLarge { limit }.into());
            }
            observe(&buffer[..read]);
            output.write_all(&buffer[..read]).map_err(io_error)?;
            progress.report(total);
        };
        let Some(interrupted) = interruption else {
            break;
        };
        let resumable = length.is_some_and(|length| total < length)
            && total > attempt_start
            && continuations < MAX_CONTINUATIONS;
        if !resumable {
            return Err(DownloadError::Interrupted(interrupted).into());
        }
        continuations += 1;
        status.step(continuation_text(total));
        let next = open(Some(&Resume {
            offset: total,
            validator: first.validator.clone(),
        }))?;
        match next.extent {
            Extent::Partial(range) if range.start == total && range.total == length => {
                body = next.body;
            }
            _ => {
                return Err(DownloadError::ResumeRefused {
                    offset: total,
                    interrupted,
                }
                .into())
            }
        }
    }
    output.sync_all().map_err(io_error)?;
    if let Some(announced) = length.filter(|&length| length != total) {
        return Err(DownloadError::LengthMismatch {
            announced,
            actual: total,
        }
        .into());
    }
    Ok(total)
}

struct Progress<'a> {
    total: Option<u64>,
    status: &'a StatusSink,
    last_line: Instant,
    last_update: Instant,
}

impl<'a> Progress<'a> {
    fn new(total: Option<u64>, status: &'a StatusSink) -> Self {
        status.transfer(0, total);
        let now = Instant::now();
        Self {
            total,
            status,
            last_line: now,
            last_update: now,
        }
    }

    fn report(&mut self, done: u64) {
        let now = Instant::now();
        if now.duration_since(self.last_update) >= WINDOW_PROGRESS_INTERVAL
            || self.total == Some(done)
        {
            self.last_update = now;
            self.status.transfer(done, self.total);
        }
        if now.duration_since(self.last_line) >= PROGRESS_INTERVAL {
            self.last_line = now;
            eprintln!("# {}", transfer_text(done, self.total));
        }
    }
}

#[derive(Debug)]
pub enum DownloadError {
    Untrusted { allowed: &'static [Host] },

    Transport(String),

    Status(u16),

    TooManyRedirects,

    InvalidContentLength,

    InvalidContentRange,

    UnrequestedRange,

    TooLarge { limit: u64 },

    LengthMismatch { announced: u64, actual: u64 },

    Interrupted(io::Error),

    ResumeRefused { offset: u64, interrupted: io::Error },

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
            Self::InvalidContentRange => f.write_str("the server sent an invalid Content-Range"),
            Self::UnrequestedRange => f.write_str(
                "the server sent part of the file although the whole file was asked for",
            ),
            Self::TooLarge { limit } => write!(f, "the file is larger than {limit} bytes"),
            Self::LengthMismatch { announced, actual } => write!(
                f,
                "the server announced {announced} bytes but sent {actual}"
            ),
            Self::Interrupted(source) => source.fmt(f),
            Self::ResumeRefused {
                offset,
                interrupted,
            } => write!(
                f,
                "the download stopped after {offset} bytes ({interrupted}) and the server would \
                 not continue it from there"
            ),
            Self::Io { path, source } => write!(f, "{}: {source}", path.display()),
        }
    }
}

impl std::error::Error for DownloadError {
    fn source(&self) -> Option<&(dyn std::error::Error + 'static)> {
        match self {
            Self::Interrupted(source)
            | Self::ResumeRefused {
                interrupted: source,
                ..
            }
            | Self::Io { source, .. } => Some(source),
            Self::Untrusted { .. }
            | Self::Transport(_)
            | Self::Status(_)
            | Self::TooManyRedirects
            | Self::InvalidContentLength
            | Self::InvalidContentRange
            | Self::UnrequestedRange
            | Self::TooLarge { .. }
            | Self::LengthMismatch { .. } => None,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::status::StatusUpdate;

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

    struct Served<'a> {
        data: &'a [u8],
        stalls: bool,
    }

    impl Read for Served<'_> {
        fn read(&mut self, buf: &mut [u8]) -> io::Result<usize> {
            if self.data.is_empty() {
                return if self.stalls {
                    Err(io::Error::new(
                        io::ErrorKind::TimedOut,
                        "timeout: receive body",
                    ))
                } else {
                    Ok(0)
                };
            }
            let read = self.data.len().min(buf.len());
            buf[..read].copy_from_slice(&self.data[..read]);
            self.data = &self.data[read..];
            Ok(read)
        }
    }

    fn served(data: &[u8], stalls: bool) -> Served<'_> {
        Served { data, stalls }
    }

    fn whole<R: Read>(
        body: R,
        length: Option<u64>,
    ) -> impl FnMut(Option<&Resume>) -> Result<Download<R>, DownloadError> {
        let mut body = Some(body);
        move |resume| {
            assert_eq!(resume, None, "an uninterrupted download is not continued");
            Ok(Download {
                body: body.take().expect("the download is opened once"),
                extent: Extent::Whole { length },
                validator: None,
            })
        }
    }

    fn save(
        open: impl FnMut(Option<&Resume>) -> Result<Download<Served<'static>>, DownloadError>,
        limit: u64,
        dest: &Path,
    ) -> Result<u64, DownloadError> {
        save_download(open, limit, dest, &StatusSink::terminal(), |_| {})
    }

    #[test]
    fn downloads_are_saved_within_the_limit_and_the_announced_length() {
        static BODY: [u8; 100] = [7u8; 100];
        let dir = temp_dir("save");
        let dest = dir.join("download");
        let mut seen = Vec::new();

        let total = save_download(
            whole(&BODY[..], Some(100)),
            100,
            &dest,
            &StatusSink::terminal(),
            |chunk| seen.extend_from_slice(chunk),
        )
        .unwrap();
        assert_eq!(total, 100);
        assert_eq!(std::fs::read(&dest).unwrap(), BODY);
        assert_eq!(seen, BODY, "every saved byte is observed");
        assert_eq!(
            save(whole(served(&BODY[..60], false), None), 100, &dest).unwrap(),
            60
        );

        let err = save(whole(served(&BODY, false), Some(101)), 100, &dest).unwrap_err();
        assert!(
            matches!(err, DownloadError::TooLarge { limit: 100 }),
            "{err:?}"
        );
        let err = save(whole(served(&BODY, false), None), 99, &dest).unwrap_err();
        assert!(
            matches!(err, DownloadError::TooLarge { limit: 99 }),
            "{err:?}"
        );
        let err = save(whole(served(&BODY[..95], false), Some(100)), 100, &dest).unwrap_err();
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
        let err = save(whole(served(&[], true), None), 100, &dest).unwrap_err();
        assert!(matches!(err, DownloadError::Interrupted(_)), "{err:?}");
        let err = save(
            whole(served(&BODY, false), None),
            100,
            &dir.join("missing/download"),
        )
        .unwrap_err();
        assert!(matches!(err, DownloadError::Io { .. }), "{err:?}");
        std::fs::remove_dir_all(&dir).ok();
    }

    static NUMBERED: [u8; 100] = {
        let mut bytes = [0u8; 100];
        let mut index = 0;
        while index < bytes.len() {
            bytes[index] = index as u8;
            index += 1;
        }
        bytes
    };

    #[test]
    fn an_interrupted_download_continues_where_it_stopped() {
        let dir = temp_dir("continue");
        let dest = dir.join("download");
        let mut requests = Vec::new();
        let mut seen = Vec::new();
        let (updates, shown) = std::sync::mpsc::channel();

        let total = save_download(
            |resume: Option<&Resume>| {
                requests.push(resume.cloned());
                Ok::<_, DownloadError>(match resume {
                    None => Download {
                        body: served(&NUMBERED[..60], true),
                        extent: Extent::Whole { length: Some(100) },
                        validator: Some("\"v1\"".to_owned()),
                    },
                    Some(resume) => Download {
                        body: served(&NUMBERED[resume.offset as usize..], false),
                        extent: Extent::Partial(ContentRange {
                            start: resume.offset,
                            total: Some(100),
                        }),
                        validator: None,
                    },
                })
            },
            100,
            &dest,
            &StatusSink::with_window(updates),
            |chunk| seen.extend_from_slice(chunk),
        )
        .unwrap();
        assert_eq!(total, 100);
        assert_eq!(std::fs::read(&dest).unwrap(), NUMBERED);
        assert_eq!(seen, NUMBERED, "every byte is observed once, in order");
        assert_eq!(
            requests,
            [
                None,
                Some(Resume {
                    offset: 60,
                    validator: Some("\"v1\"".to_owned())
                })
            ]
        );
        let steps: Vec<StatusUpdate> = shown
            .try_iter()
            .filter(|update| matches!(update, StatusUpdate::Step(_)))
            .collect();
        assert_eq!(
            steps,
            [StatusUpdate::Step(continuation_text(60))],
            "a continuation is announced without claiming the link failed"
        );
        std::fs::remove_dir_all(&dir).ok();
    }

    #[test]
    fn a_download_is_continued_only_while_it_makes_progress() {
        let dir = temp_dir("stalled");
        let dest = dir.join("download");
        let mut requests = 0;
        let err = save(
            |_| {
                requests += 1;
                Ok(Download {
                    body: served(&[], true),
                    extent: Extent::Whole { length: Some(100) },
                    validator: None,
                })
            },
            100,
            &dest,
        )
        .unwrap_err();
        assert!(matches!(err, DownloadError::Interrupted(_)), "{err:?}");
        assert_eq!(
            requests, 1,
            "a request that delivered nothing is not repeated"
        );

        let mut requests = Vec::new();
        let err = save(
            |resume| {
                requests.push(resume.map(|resume| resume.offset));
                Ok(match resume {
                    None => Download {
                        body: served(&NUMBERED[..60], true),
                        extent: Extent::Whole { length: Some(100) },
                        validator: None,
                    },
                    Some(resume) => Download {
                        body: served(&[], true),
                        extent: Extent::Partial(ContentRange {
                            start: resume.offset,
                            total: Some(100),
                        }),
                        validator: None,
                    },
                })
            },
            100,
            &dest,
        )
        .unwrap_err();
        assert!(matches!(err, DownloadError::Interrupted(_)), "{err:?}");
        assert_eq!(requests, [None, Some(60)]);
        std::fs::remove_dir_all(&dir).ok();
    }

    #[test]
    fn a_continuation_must_resume_at_the_interruption_of_the_same_file() {
        let dir = temp_dir("refused");
        let dest = dir.join("download");
        for answer in [
            Extent::Whole { length: Some(100) },
            Extent::Partial(ContentRange {
                start: 0,
                total: Some(100),
            }),
            Extent::Partial(ContentRange {
                start: 60,
                total: Some(101),
            }),
        ] {
            let err = save(
                |resume| {
                    Ok(match resume {
                        None => Download {
                            body: served(&NUMBERED[..60], true),
                            extent: Extent::Whole { length: Some(100) },
                            validator: None,
                        },
                        Some(_) => Download {
                            body: served(&NUMBERED, false),
                            extent: answer,
                            validator: None,
                        },
                    })
                },
                100,
                &dest,
            )
            .unwrap_err();
            assert!(
                matches!(err, DownloadError::ResumeRefused { offset: 60, .. }),
                "{answer:?}: {err:?}"
            );
        }
        let err = save(
            |_| {
                Ok(Download {
                    body: served(&NUMBERED, false),
                    extent: Extent::Partial(ContentRange {
                        start: 0,
                        total: Some(100),
                    }),
                    validator: None,
                })
            },
            100,
            &dest,
        )
        .unwrap_err();
        assert!(matches!(err, DownloadError::UnrequestedRange), "{err:?}");
        std::fs::remove_dir_all(&dir).ok();
    }

    #[test]
    fn content_ranges_and_validators_are_read_strictly() {
        assert_eq!(
            parse_content_range("bytes 60-99/100"),
            Some(ContentRange {
                start: 60,
                total: Some(100)
            })
        );
        assert_eq!(
            parse_content_range("bytes 0-9/*"),
            Some(ContentRange {
                start: 0,
                total: None
            })
        );
        for invalid in [
            "bytes 10-5/100",
            "bytes 0-100/100",
            "items 0-9/10",
            "bytes 0-9",
            "bytes a-9/10",
            "bytes -9/10",
        ] {
            assert_eq!(parse_content_range(invalid), None, "{invalid}");
        }

        let mut headers = HeaderMap::new();
        assert_eq!(strong_etag(&headers), None);
        headers.insert(ETAG, "W/\"weak\"".parse().unwrap());
        assert_eq!(
            strong_etag(&headers),
            None,
            "If-Range needs a strong validator"
        );
        headers.insert(ETAG, "\"55e2ac00\"".parse().unwrap());
        assert_eq!(strong_etag(&headers).as_deref(), Some("\"55e2ac00\""));
    }

    #[test]
    fn transport_errors_never_echo_urls() {
        let detail = redacted(ureq::Error::BadUri(
            "https://files.example.com/?X-Amz-Signature=secret".to_owned(),
        ));
        assert!(!detail.contains("secret"), "{detail}");
    }
}
