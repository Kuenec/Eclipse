#![forbid(unsafe_code)]

use std::collections::HashMap;
use std::io::{Read as _, Write as _};
use std::net::IpAddr;
use std::os::unix::fs::OpenOptionsExt as _;
use std::path::{Path, PathBuf};
use std::time::{SystemTime, UNIX_EPOCH};

use super::proto::{self, ClearScope, CookieExpiry, CookiePair, SameSite, StoredCookie};

pub const COOKIE_JAR_FILE: &str = "cookies";

const CORRUPT_EXTENSION: &str = "corrupt";

const STAGING_EXTENSION: &str = "tmp";

const MAX_NAME_AND_VALUE_BYTES: usize = 4096;

const MAX_PATH_BYTES: usize = 1024;

const MAX_DOMAIN_BYTES: usize = 253;

const MAX_COOKIES_PER_DOMAIN: usize = 180;

const MAX_COOKIES: usize = 1000;

const DEFAULT_SAME_SITE: SameSite = SameSite::Lax;

const SECONDS_PER_DAY: i64 = 86_400;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum CookieRejected {
    NotAUrl,

    NoName,

    TooLarge,

    DomainMismatch,

    PublicDomain,
}

impl std::fmt::Display for CookieRejected {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(match self {
            Self::NotAUrl => "the cookie's URL has no scheme and host",
            Self::NoName => "the cookie has no name=value pair",
            Self::TooLarge => "the cookie exceeds the jar's size limits",
            Self::DomainMismatch => "the cookie's Domain does not cover the URL's host",
            Self::PublicDomain => "the cookie's Domain is a single-label domain",
        })
    }
}

impl std::error::Error for CookieRejected {}

#[derive(Debug)]
pub enum JarFileError {
    Io(std::io::Error),

    Oversized,

    Codec(proto::ProtoError),
}

impl std::fmt::Display for JarFileError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::Io(error) => write!(f, "{error}"),
            Self::Oversized => write!(f, "larger than {} bytes", proto::GLOBAL_FRAME_CAP),
            Self::Codec(error) => write!(f, "{error}"),
        }
    }
}

impl std::error::Error for JarFileError {}

#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct CookieJar {
    cookies: Vec<StoredCookie>,
}

type CookieKey<'a> = (&'a str, &'a str, &'a str);

fn key(cookie: &StoredCookie) -> CookieKey<'_> {
    (&cookie.name, &cookie.domain, &cookie.path)
}

fn unix_seconds(time: SystemTime) -> i64 {
    time.duration_since(UNIX_EPOCH).map_or(0, |since| {
        i64::try_from(since.as_secs()).unwrap_or(i64::MAX)
    })
}

fn is_expired(cookie: &StoredCookie, now_s: i64) -> bool {
    matches!(cookie.expiry, CookieExpiry::At { epoch_s } if epoch_s <= now_s)
}

fn domain_key(cookie: &StoredCookie) -> &str {
    cookie.domain.strip_prefix('.').unwrap_or(&cookie.domain)
}

fn admissible(cookie: &StoredCookie) -> bool {
    !cookie.name.is_empty()
        && !cookie.domain.is_empty()
        && cookie.name.len() + cookie.value.len() <= MAX_NAME_AND_VALUE_BYTES
        && cookie.path.len() <= MAX_PATH_BYTES
        && domain_key(cookie).len() <= MAX_DOMAIN_BYTES
}

fn without_duplicate_keys(cookies: Vec<StoredCookie>) -> Vec<StoredCookie> {
    let mut kept: Vec<StoredCookie> = Vec::with_capacity(cookies.len());
    let mut positions: HashMap<(String, String, String), usize> = HashMap::new();
    for cookie in cookies {
        let owned_key = (
            cookie.name.clone(),
            cookie.domain.clone(),
            cookie.path.clone(),
        );
        match positions.get(&owned_key) {
            Some(&position) => kept[position] = cookie,
            None => {
                positions.insert(owned_key, kept.len());
                kept.push(cookie);
            }
        }
    }
    kept
}

impl CookieJar {
    pub fn from_cookies(cookies: Vec<StoredCookie>, now: SystemTime) -> Self {
        let mut jar = Self {
            cookies: without_duplicate_keys(cookies.into_iter().filter(admissible).collect()),
        };
        jar.enforce_limits(unix_seconds(now));
        jar
    }

    pub fn set_from_header(
        &mut self,
        url: &str,
        header: &str,
        now: SystemTime,
    ) -> Result<bool, CookieRejected> {
        let cookie = parse_set_cookie(url, header, now)?;
        let now_s = unix_seconds(now);
        if is_expired(&cookie, now_s) {
            let before = self.cookies.len();
            self.cookies.retain(|stored| key(stored) != key(&cookie));
            return Ok(self.cookies.len() != before);
        }
        let changed = match self
            .cookies
            .iter()
            .position(|stored| key(stored) == key(&cookie))
        {
            Some(position) if self.cookies[position] == cookie => false,
            Some(position) => {
                self.cookies[position] = cookie;
                true
            }
            None => {
                self.cookies.push(cookie);
                true
            }
        };
        let evicted = self.enforce_limits(now_s);
        Ok(changed || evicted)
    }

    pub fn matching(&self, url: &str, now: SystemTime) -> Vec<CookiePair> {
        let Some(request) = RequestUrl::parse(url) else {
            return Vec::new();
        };
        let now_s = unix_seconds(now);
        let mut sent: Vec<&StoredCookie> = self
            .cookies
            .iter()
            .filter(|cookie| !is_expired(cookie, now_s) && request.sends(cookie))
            .collect();
        sent.sort_by_key(|cookie| std::cmp::Reverse(cookie.path.len()));
        sent.into_iter()
            .map(|cookie| CookiePair {
                name: cookie.name.clone(),
                value: cookie.value.clone(),
            })
            .collect()
    }

    pub fn clear(&mut self, scope: ClearScope) -> bool {
        let before = self.cookies.len();
        match scope {
            ClearScope::All => self.cookies.clear(),
            ClearScope::Session => self
                .cookies
                .retain(|cookie| cookie.expiry != CookieExpiry::Session),
        }
        self.cookies.len() != before
    }

    pub fn replace_all(&mut self, snapshot: Vec<StoredCookie>, now: SystemTime) -> bool {
        let incoming = without_duplicate_keys(snapshot.into_iter().filter(admissible).collect());
        let positions: HashMap<CookieKey<'_>, usize> = incoming
            .iter()
            .enumerate()
            .map(|(position, cookie)| (key(cookie), position))
            .collect();
        let mut taken = vec![false; incoming.len()];
        let mut next: Vec<StoredCookie> = Vec::with_capacity(incoming.len());
        for existing in &self.cookies {
            if let Some(&position) = positions.get(&key(existing)) {
                taken[position] = true;
                next.push(incoming[position].clone());
            }
        }
        next.extend(
            incoming
                .iter()
                .zip(&taken)
                .filter(|(_, taken)| !**taken)
                .map(|(cookie, _)| cookie.clone()),
        );
        let mut replaced = Self { cookies: next };
        replaced.enforce_limits(unix_seconds(now));
        let changed = replaced != *self;
        *self = replaced;
        changed
    }

    pub fn all_unexpired(&self, now: SystemTime) -> Vec<StoredCookie> {
        let now_s = unix_seconds(now);
        self.cookies
            .iter()
            .filter(|cookie| !is_expired(cookie, now_s))
            .cloned()
            .collect()
    }

    pub fn encode(&self) -> Result<Vec<u8>, proto::ProtoError> {
        proto::encode_cookies(&self.cookies)
    }

    fn enforce_limits(&mut self, now_s: i64) -> bool {
        let before = self.cookies.len();
        self.cookies.retain(|cookie| !is_expired(cookie, now_s));
        let mut keep = vec![true; self.cookies.len()];
        let mut per_domain: HashMap<&str, usize> = HashMap::new();
        let mut kept = 0;
        for (position, cookie) in self.cookies.iter().enumerate().rev() {
            let in_domain = per_domain.entry(domain_key(cookie)).or_insert(0);
            *in_domain += 1;
            if *in_domain > MAX_COOKIES_PER_DOMAIN || kept == MAX_COOKIES {
                keep[position] = false;
            } else {
                kept += 1;
            }
        }
        let mut keep = keep.into_iter();
        self.cookies.retain(|_| keep.next().unwrap_or(false));
        self.cookies.len() != before
    }
}

pub fn read_file(path: &Path, now: SystemTime) -> Result<Option<CookieJar>, JarFileError> {
    let file = match std::fs::File::open(path) {
        Ok(file) => file,
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => return Ok(None),
        Err(error) => return Err(JarFileError::Io(error)),
    };
    let mut bytes = Vec::new();
    file.take(u64::from(proto::GLOBAL_FRAME_CAP) + 1)
        .read_to_end(&mut bytes)
        .map_err(JarFileError::Io)?;
    if bytes.len() > proto::GLOBAL_FRAME_CAP as usize {
        return Err(JarFileError::Oversized);
    }
    let cookies = proto::decode_cookies(&bytes).map_err(JarFileError::Codec)?;
    Ok(Some(CookieJar::from_cookies(cookies, now)))
}

pub fn write_file(path: &Path, bytes: &[u8]) -> std::io::Result<()> {
    let staging = path.with_extension(STAGING_EXTENSION);
    let mut file = std::fs::OpenOptions::new()
        .write(true)
        .create(true)
        .truncate(true)
        .mode(0o600)
        .open(&staging)?;
    file.write_all(bytes)?;
    file.sync_all()?;
    std::fs::rename(&staging, path)
}

pub fn quarantine(path: &Path) -> std::io::Result<PathBuf> {
    let corrupt = path.with_extension(CORRUPT_EXTENSION);
    std::fs::rename(path, &corrupt)?;
    Ok(corrupt)
}

struct RequestUrl<'a> {
    secure: bool,
    host: String,
    path: &'a str,
}

impl<'a> RequestUrl<'a> {
    fn parse(url: &'a str) -> Option<Self> {
        let (scheme, rest) = url.split_once("://")?;
        if scheme.is_empty() {
            return None;
        }
        let (authority, tail) = rest.split_at(rest.find(['/', '?', '#']).unwrap_or(rest.len()));
        let host_and_port = authority
            .rsplit_once('@')
            .map_or(authority, |(_, host)| host);
        let host = match host_and_port.strip_prefix('[') {
            Some(bracketed) => bracketed.split_once(']')?.0,
            None => host_and_port
                .rsplit_once(':')
                .filter(|(_, port)| port.bytes().all(|byte| byte.is_ascii_digit()))
                .map_or(host_and_port, |(host, _)| host),
        };
        if host.is_empty() {
            return None;
        }
        let path = &tail[..tail.find(['?', '#']).unwrap_or(tail.len())];
        Some(Self {
            secure: scheme.eq_ignore_ascii_case("https"),
            host: host.to_ascii_lowercase(),
            path: if path.is_empty() { "/" } else { path },
        })
    }

    fn sends(&self, cookie: &StoredCookie) -> bool {
        let domain_matched = match cookie.domain.strip_prefix('.') {
            Some(domain) => domain_matches(&self.host, domain),
            None => self.host == cookie.domain,
        };
        domain_matched && path_matches(self.path, &cookie.path) && (self.secure || !cookie.secure)
    }
}

fn domain_matches(host: &str, domain: &str) -> bool {
    host == domain
        || (host
            .strip_suffix(domain)
            .is_some_and(|prefix| prefix.ends_with('.'))
            && host.parse::<IpAddr>().is_err())
}

fn path_matches(request_path: &str, cookie_path: &str) -> bool {
    request_path == cookie_path
        || request_path
            .strip_prefix(cookie_path)
            .is_some_and(|rest| cookie_path.ends_with('/') || rest.starts_with('/'))
}

fn default_path(request_path: &str) -> String {
    match request_path.rfind('/') {
        Some(slash) if slash > 0 && request_path.starts_with('/') => {
            request_path[..slash].to_string()
        }
        _ => "/".to_string(),
    }
}

fn trim_whitespace(text: &str) -> &str {
    text.trim_matches([' ', '\t'])
}

#[derive(Default)]
struct Attributes {
    expires: Option<i64>,
    max_age: Option<i64>,
    domain: Option<String>,
    path: Option<String>,
    secure: bool,
    http_only: bool,
    same_site: Option<SameSite>,
}

impl Attributes {
    fn parse(attributes: &str) -> Self {
        let mut parsed = Self::default();
        for attribute in attributes.split(';') {
            let (name, value) = attribute.split_once('=').unwrap_or((attribute, ""));
            let (name, value) = (trim_whitespace(name), trim_whitespace(value));
            if name.eq_ignore_ascii_case("expires") {
                parsed.expires = parse_cookie_date(value).or(parsed.expires);
            } else if name.eq_ignore_ascii_case("max-age") {
                parsed.max_age = parse_max_age(value).or(parsed.max_age);
            } else if name.eq_ignore_ascii_case("domain") && !value.is_empty() {
                parsed.domain = Some(
                    value
                        .strip_prefix('.')
                        .unwrap_or(value)
                        .to_ascii_lowercase(),
                );
            } else if name.eq_ignore_ascii_case("path") {
                parsed.path = value.starts_with('/').then(|| value.to_string());
            } else if name.eq_ignore_ascii_case("secure") {
                parsed.secure = true;
            } else if name.eq_ignore_ascii_case("httponly") {
                parsed.http_only = true;
            } else if name.eq_ignore_ascii_case("samesite") {
                parsed.same_site = parse_same_site(value).or(parsed.same_site);
            }
        }
        parsed
    }
}

pub fn parse_set_cookie(
    url: &str,
    header: &str,
    now: SystemTime,
) -> Result<StoredCookie, CookieRejected> {
    let request = RequestUrl::parse(url).ok_or(CookieRejected::NotAUrl)?;
    let (pair, attributes) = header.split_once(';').unwrap_or((header, ""));
    let (name, value) = pair.split_once('=').ok_or(CookieRejected::NoName)?;
    let (name, value) = (trim_whitespace(name), trim_whitespace(value));
    if name.is_empty() {
        return Err(CookieRejected::NoName);
    }
    let attributes = Attributes::parse(attributes);
    let domain = match attributes.domain {
        None => request.host.clone(),
        Some(domain) if !domain.contains('.') => {
            if domain != request.host {
                return Err(CookieRejected::PublicDomain);
            }
            domain
        }
        Some(domain) if domain_matches(&request.host, &domain) => format!(".{domain}"),
        Some(_) => return Err(CookieRejected::DomainMismatch),
    };
    let expiry = match (attributes.max_age, attributes.expires) {
        (Some(delta_s), _) => CookieExpiry::At {
            epoch_s: unix_seconds(now).saturating_add(delta_s.max(0)),
        },
        (None, Some(epoch_s)) => CookieExpiry::At { epoch_s },
        (None, None) => CookieExpiry::Session,
    };
    let cookie = StoredCookie {
        name: name.to_string(),
        value: value.to_string(),
        domain,
        path: attributes
            .path
            .unwrap_or_else(|| default_path(request.path)),
        secure: attributes.secure,
        http_only: attributes.http_only,
        same_site: attributes.same_site.unwrap_or(DEFAULT_SAME_SITE),
        expiry,
    };
    if !admissible(&cookie) {
        return Err(CookieRejected::TooLarge);
    }
    Ok(cookie)
}

fn parse_max_age(value: &str) -> Option<i64> {
    let digits = value.strip_prefix('-').unwrap_or(value);
    if digits.is_empty() || !digits.bytes().all(|byte| byte.is_ascii_digit()) {
        return None;
    }
    Some(value.parse().unwrap_or(if digits.len() < value.len() {
        i64::MIN
    } else {
        i64::MAX
    }))
}

fn parse_same_site(value: &str) -> Option<SameSite> {
    [
        ("strict", SameSite::Strict),
        ("lax", SameSite::Lax),
        ("none", SameSite::None),
    ]
    .into_iter()
    .find(|(name, _)| value.eq_ignore_ascii_case(name))
    .map(|(_, policy)| policy)
}

fn is_date_delimiter(c: char) -> bool {
    matches!(c, '\t' | ' '..='/' | ';'..='@' | '['..='`' | '{'..='~')
}

fn leading_number(token: &str, min_digits: usize, max_digits: usize) -> Option<u32> {
    let digits = token.bytes().take_while(u8::is_ascii_digit).count();
    if !(min_digits..=max_digits).contains(&digits) {
        return None;
    }
    token[..digits].parse().ok()
}

fn parse_time(token: &str) -> Option<(u32, u32, u32)> {
    let mut fields = [0u32; 3];
    let mut rest = token;
    for (index, field) in fields.iter_mut().enumerate() {
        if index > 0 {
            rest = rest.strip_prefix(':')?;
        }
        *field = leading_number(rest, 1, 2)?;
        rest = rest.trim_start_matches(|c: char| c.is_ascii_digit());
    }
    Some((fields[0], fields[1], fields[2]))
}

fn parse_month(token: &str) -> Option<u32> {
    let month = match token.get(..3)?.to_ascii_lowercase().as_str() {
        "jan" => 1,
        "feb" => 2,
        "mar" => 3,
        "apr" => 4,
        "may" => 5,
        "jun" => 6,
        "jul" => 7,
        "aug" => 8,
        "sep" => 9,
        "oct" => 10,
        "nov" => 11,
        "dec" => 12,
        _ => return None,
    };
    Some(month)
}

fn is_leap_year(year: i64) -> bool {
    (year % 4 == 0 && year % 100 != 0) || year % 400 == 0
}

fn days_in_month(year: i64, month: u32) -> u32 {
    match month {
        2 if is_leap_year(year) => 29,
        2 => 28,
        4 | 6 | 9 | 11 => 30,
        _ => 31,
    }
}

fn days_from_civil(year: i64, month: u32, day: u32) -> i64 {
    let year = if month <= 2 { year - 1 } else { year };
    let era = year.div_euclid(400);
    let year_of_era = year - era * 400;
    let month_from_march = i64::from((month + 9) % 12);
    let day_of_year = (153 * month_from_march + 2) / 5 + i64::from(day) - 1;
    let day_of_era = year_of_era * 365 + year_of_era / 4 - year_of_era / 100 + day_of_year;
    era * 146_097 + day_of_era - 719_468
}

fn parse_cookie_date(value: &str) -> Option<i64> {
    let mut time = None;
    let mut day = None;
    let mut month = None;
    let mut year = None;
    for token in value
        .split(is_date_delimiter)
        .filter(|token| !token.is_empty())
    {
        if time.is_none() {
            time = parse_time(token);
            if time.is_some() {
                continue;
            }
        }
        if day.is_none() {
            day = leading_number(token, 1, 2);
            if day.is_some() {
                continue;
            }
        }
        if month.is_none() {
            month = parse_month(token);
            if month.is_some() {
                continue;
            }
        }
        if year.is_none() {
            year = leading_number(token, 2, 4);
        }
    }
    let ((hour, minute, second), day, month, year) = (time?, day?, month?, year?);
    let year = i64::from(match year {
        70..=99 => year + 1900,
        0..=69 => year + 2000,
        _ => year,
    });
    if year < 1601 || hour > 23 || minute > 59 || second > 59 {
        return None;
    }
    if day == 0 || day > days_in_month(year, month) {
        return None;
    }
    Some(
        days_from_civil(year, month, day) * SECONDS_PER_DAY
            + i64::from(hour * 3600 + minute * 60 + second),
    )
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::time::Duration;

    fn at(epoch_s: u64) -> SystemTime {
        UNIX_EPOCH + Duration::from_secs(epoch_s)
    }

    const NOW_S: u64 = 1_790_000_000;

    fn now() -> SystemTime {
        at(NOW_S)
    }

    fn parse(url: &str, header: &str) -> Result<StoredCookie, CookieRejected> {
        parse_set_cookie(url, header, now())
    }

    fn names(pairs: &[CookiePair]) -> Vec<&str> {
        pairs.iter().map(|pair| pair.name.as_str()).collect()
    }

    fn set(jar: &mut CookieJar, url: &str, header: &str) {
        jar.set_from_header(url, header, now())
            .unwrap_or_else(|error| panic!("{header:?} must be accepted: {error}"));
    }

    fn stored(name: &str, domain: &str) -> StoredCookie {
        StoredCookie {
            name: name.to_string(),
            value: "v".to_string(),
            domain: domain.to_string(),
            path: "/".to_string(),
            secure: false,
            http_only: false,
            same_site: DEFAULT_SAME_SITE,
            expiry: CookieExpiry::Session,
        }
    }

    #[test]
    fn a_roblox_login_cookie_keeps_every_attribute() {
        assert_eq!(
            parse(
                "https://www.roblox.com/login",
                ".ROBLOSECURITY=_|WARNING|_token; domain=.roblox.com; HttpOnly; secure; \
                 expires=Fri, 02 Oct 2054 21:00:00 GMT; path=/; SameSite=None",
            ),
            Ok(StoredCookie {
                name: ".ROBLOSECURITY".to_string(),
                value: "_|WARNING|_token".to_string(),
                domain: ".roblox.com".to_string(),
                path: "/".to_string(),
                secure: true,
                http_only: true,
                same_site: SameSite::None,
                expiry: CookieExpiry::At {
                    epoch_s: 2_674_587_600
                },
            })
        );
    }

    #[test]
    fn a_cookie_without_domain_is_host_only_with_the_default_path() {
        let cookie = parse(
            "https://apis.roblox.com/browser-tracker-api/device/initialize?x=1",
            " RBXEventTrackerV2 = CreateDate=1&browserid=2 ",
        )
        .expect("accepted");
        assert_eq!(cookie.name, "RBXEventTrackerV2");
        assert_eq!(cookie.value, "CreateDate=1&browserid=2");
        assert_eq!(cookie.domain, "apis.roblox.com");
        assert_eq!(cookie.path, "/browser-tracker-api/device");
        assert_eq!(cookie.expiry, CookieExpiry::Session);
        assert_eq!(cookie.same_site, DEFAULT_SAME_SITE);
        assert!(!cookie.secure && !cookie.http_only);
        assert_eq!(
            parse("https://a.roblox.com", "a=1").map(|c| c.path),
            Ok("/".into())
        );
        assert_eq!(
            parse("https://a.roblox.com/x", "a=1").map(|c| c.path),
            Ok("/".into())
        );
        assert_eq!(
            parse("https://A.Roblox.COM:443/x/y", "a=1; Path=nope").map(|c| (c.domain, c.path)),
            Ok(("a.roblox.com".into(), "/x".into()))
        );
    }

    #[test]
    fn max_age_overrides_expires_and_the_last_attribute_wins() {
        let cookie = parse(
            "http://127.0.0.1:8080/",
            "a=1; Max-Age=3600; Expires=Wed, 10 May 2000 00:00:00 GMT; Path=/one; Path=/two; \
             SameSite=Strict; SameSite=bogus",
        )
        .expect("accepted");
        assert_eq!(
            cookie.expiry,
            CookieExpiry::At {
                epoch_s: NOW_S as i64 + 3600
            }
        );
        assert_eq!(cookie.path, "/two");
        assert_eq!(cookie.same_site, SameSite::Strict);
        assert_eq!(cookie.domain, "127.0.0.1");
        assert!(
            parse("http://127.0.0.1/", "a=1; Secure")
                .expect("accepted")
                .secure,
            "Secure from an http URL is kept, as the WebKit helper does"
        );
        assert_eq!(
            parse("https://roblox.com", "a=1; Max-Age=x1; Max-Age=-5").map(|c| c.expiry),
            Ok(CookieExpiry::At {
                epoch_s: NOW_S as i64
            })
        );
    }

    #[test]
    fn domains_must_cover_the_host_and_have_more_than_one_label() {
        assert_eq!(
            parse("https://www.roblox.com/", "a=1; Domain=ROBLOX.com").map(|c| c.domain),
            Ok(".roblox.com".into())
        );
        assert_eq!(
            parse("https://www.roblox.com/", "a=1; Domain=www.roblox.com").map(|c| c.domain),
            Ok(".www.roblox.com".into())
        );
        assert_eq!(
            parse("https://www.roblox.com/", "a=1; Domain=example.com"),
            Err(CookieRejected::DomainMismatch)
        );
        assert_eq!(
            parse("https://www.roblox.com/", "a=1; Domain=xroblox.com"),
            Err(CookieRejected::DomainMismatch)
        );
        assert_eq!(
            parse("https://www.roblox.com/", "a=1; Domain=com"),
            Err(CookieRejected::PublicDomain)
        );
        assert_eq!(
            parse("http://localhost/", "a=1; Domain=localhost").map(|c| c.domain),
            Ok("localhost".into())
        );
        assert_eq!(
            parse("http://10.0.0.1/", "a=1; Domain=0.0.1"),
            Err(CookieRejected::DomainMismatch),
            "an IP address never matches a parent domain"
        );
        assert_eq!(
            parse("https://www.roblox.com/", "a=1; Domain=").map(|c| c.domain),
            Ok("www.roblox.com".into())
        );
    }

    #[test]
    fn malformed_cookies_and_urls_are_rejected() {
        assert_eq!(
            parse("https://roblox.com/", "novalue"),
            Err(CookieRejected::NoName)
        );
        assert_eq!(
            parse("https://roblox.com/", "=v"),
            Err(CookieRejected::NoName)
        );
        assert_eq!(parse("roblox.com/", "a=1"), Err(CookieRejected::NotAUrl));
        assert_eq!(parse("https:///path", "a=1"), Err(CookieRejected::NotAUrl));
        assert_eq!(
            parse("https://roblox.com/", &format!("a={}", "v".repeat(4096))),
            Err(CookieRejected::TooLarge)
        );
        assert_eq!(
            parse(
                "https://roblox.com/",
                &format!("a=1; Path=/{}", "p".repeat(1024))
            ),
            Err(CookieRejected::TooLarge)
        );
    }

    #[test]
    fn cookie_dates_follow_the_rfc_6265_algorithm() {
        assert_eq!(parse_cookie_date("Thu, 01 Jan 1970 00:00:00 GMT"), Some(0));
        assert_eq!(
            parse_cookie_date("Wed, 10 May 2000 12:34:56 GMT"),
            Some(957_962_096)
        );
        assert_eq!(
            parse_cookie_date("Wednesday, 10-May-00 12:34:56 GMT"),
            Some(957_962_096)
        );
        assert_eq!(
            parse_cookie_date("Wed May 10 12:34:56 2000"),
            Some(957_962_096)
        );
        assert_eq!(
            parse_cookie_date("29 Feb 2024 23:59:59"),
            Some(1_709_251_199)
        );
        assert_eq!(parse_cookie_date("31 Dec 1969 23:59:59"), Some(-1));
        assert_eq!(
            parse_cookie_date("Sat, 31 Dec 2069 00:00:00"),
            Some(3_155_673_600)
        );
        assert_eq!(parse_cookie_date("29 Feb 2023 00:00:00"), None);
        assert_eq!(parse_cookie_date("10 May 2000 24:00:00"), None);
        assert_eq!(parse_cookie_date("10 May 1600 00:00:00"), None);
        assert_eq!(parse_cookie_date("10 May 2000"), None);
        assert_eq!(parse_cookie_date("tomorrow"), None);
    }

    #[test]
    fn the_roblox_deletion_header_removes_the_login_cookie() {
        let mut jar = CookieJar::default();
        set(
            &mut jar,
            "https://www.roblox.com/",
            ".ROBLOSECURITY=secret; domain=.roblox.com; HttpOnly; secure; path=/; \
             expires=Fri, 02 Oct 2054 21:00:00 GMT",
        );
        set(
            &mut jar,
            "https://www.roblox.com/",
            "keep=1; domain=.roblox.com",
        );
        assert_eq!(
            jar.set_from_header(
                "https://www.roblox.com/",
                ".ROBLOSECURITY=;domain=.roblox.com;path=/;expires=Wed, 10 May 2000 00:00:00 GMT",
                now(),
            ),
            Ok(true)
        );
        assert_eq!(
            names(&jar.matching("https://www.roblox.com/", now())),
            ["keep"]
        );
        assert_eq!(
            jar.set_from_header(
                "https://www.roblox.com/",
                ".ROBLOSECURITY=; domain=.roblox.com; path=/; Max-Age=0",
                now(),
            ),
            Ok(false),
            "deleting a missing cookie changes nothing"
        );
    }

    #[test]
    fn matching_follows_rfc_6265_section_5_4() {
        let mut jar = CookieJar::default();
        set(
            &mut jar,
            "https://www.roblox.com/",
            "root=1; Domain=roblox.com",
        );
        set(
            &mut jar,
            "https://www.roblox.com/",
            "deep=1; Path=/games/1818",
        );
        set(&mut jar, "https://www.roblox.com/", "games=1; Path=/games");
        set(&mut jar, "https://www.roblox.com/", "host=1");
        set(&mut jar, "https://www.roblox.com/", "secret=1; Secure");
        set(&mut jar, "https://www.roblox.com/", "page=1; HttpOnly");
        set(&mut jar, "https://www.roblox.com/", "soon=1; Max-Age=10");
        set(&mut jar, "https://www.roblox.com/", "older=1; Path=/games");
        set(&mut jar, "https://www.roblox.com/", "games=2; Path=/games");

        assert_eq!(
            names(&jar.matching("https://www.roblox.com/games/1818/x?y#z", now())),
            ["deep", "games", "older", "root", "host", "secret", "page", "soon"],
            "longest path first, then oldest; a replaced cookie keeps its place"
        );
        assert_eq!(
            jar.matching("https://www.roblox.com/games", now())
                .iter()
                .find(|pair| pair.name == "games")
                .map(|pair| pair.value.as_str()),
            Some("2")
        );
        assert_eq!(
            names(&jar.matching("http://www.roblox.com/gamesx", now())),
            ["root", "host", "page", "soon"],
            "no Secure cookie over http and /games does not cover /gamesx"
        );
        assert_eq!(
            names(&jar.matching("https://apis.roblox.com/", at(NOW_S + 10))),
            ["root"],
            "host-only cookies stay on their host and expired cookies are skipped"
        );
        assert!(jar.matching("https://roblox.com.evil/", now()).is_empty());
        assert!(jar.matching("not a url", now()).is_empty());
    }

    #[test]
    fn clearing_session_cookies_keeps_persistent_ones() {
        let mut jar = CookieJar::default();
        set(&mut jar, "https://roblox.com/", "session=1");
        set(&mut jar, "https://roblox.com/", "persistent=1; Max-Age=600");
        assert!(jar.clear(ClearScope::Session));
        assert!(!jar.clear(ClearScope::Session));
        assert_eq!(
            names(&jar.matching("https://roblox.com/", now())),
            ["persistent"]
        );
        assert!(jar.clear(ClearScope::All));
        assert!(!jar.clear(ClearScope::All));
    }

    #[test]
    fn a_full_domain_and_a_full_jar_evict_their_oldest_cookies() {
        let mut jar = CookieJar::default();
        set(
            &mut jar,
            "https://www.roblox.com/",
            "expired-soon=1; Max-Age=1",
        );
        for index in 0..MAX_COOKIES_PER_DOMAIN - 1 {
            set(&mut jar, "https://www.roblox.com/", &format!("c{index}=1"));
        }
        let later = at(NOW_S + 5);
        assert_eq!(
            jar.set_from_header("https://www.roblox.com/", "newest=1", later),
            Ok(true)
        );
        let sent = jar.matching("https://www.roblox.com/", later);
        assert_eq!(sent.len(), MAX_COOKIES_PER_DOMAIN);
        assert_eq!(sent[0].name, "c0", "the expired cookie went first");
        jar.set_from_header("https://www.roblox.com/", "newer=1", later)
            .expect("accepted");
        let sent = jar.matching("https://www.roblox.com/", later);
        assert_eq!(sent.len(), MAX_COOKIES_PER_DOMAIN);
        assert_eq!(
            (
                sent[0].name.as_str(),
                sent[MAX_COOKIES_PER_DOMAIN - 1].name.as_str()
            ),
            ("c1", "newer"),
            "then the oldest cookie of the full domain"
        );

        let mut jar = CookieJar::default();
        for index in 0..=MAX_COOKIES {
            set(
                &mut jar,
                &format!("https://h{}.roblox.com/", index / 100),
                &format!("c{index}=1"),
            );
        }
        let all = jar.all_unexpired(now());
        assert_eq!(all.len(), MAX_COOKIES);
        assert_eq!(all[0].name, "c1", "the oldest cookie of a full jar goes");
        let encoded = jar.encode().expect("a full jar encodes");
        assert!(encoded.len() < proto::GLOBAL_FRAME_CAP as usize);
    }

    #[test]
    fn a_snapshot_replaces_the_jar_and_keeps_creation_order() {
        let mut jar = CookieJar::default();
        set(&mut jar, "https://roblox.com/", "a=1");
        set(&mut jar, "https://roblox.com/", "b=1");
        set(&mut jar, "https://roblox.com/", "c=1");
        let snapshot = vec![
            stored("new", "roblox.com"),
            StoredCookie {
                value: "2".to_string(),
                ..stored("c", "roblox.com")
            },
            stored("a", "roblox.com"),
        ];
        assert!(jar.replace_all(snapshot.clone(), now()));
        assert_eq!(
            jar.matching("https://roblox.com/", now())
                .into_iter()
                .map(|pair| (pair.name, pair.value))
                .collect::<Vec<_>>(),
            [
                ("a".to_string(), "v".to_string()),
                ("c".to_string(), "2".to_string()),
                ("new".to_string(), "v".to_string())
            ]
        );
        assert!(
            !jar.replace_all(snapshot, now()),
            "the same snapshot changes nothing"
        );
        assert!(jar.replace_all(Vec::new(), now()));
        assert!(jar.all_unexpired(now()).is_empty());
    }

    #[test]
    fn the_jar_file_round_trips_privately_and_drops_expired_cookies() {
        use std::os::unix::fs::PermissionsExt as _;

        let dir = std::env::temp_dir().join(format!("eclipse-cookie-jar-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).expect("scratch dir");
        let path = dir.join(COOKIE_JAR_FILE);
        assert!(read_file(&path, now()).expect("a missing jar").is_none());

        let mut jar = CookieJar::default();
        set(&mut jar, "https://roblox.com/", "session=1");
        set(&mut jar, "https://roblox.com/", "brief=1; Max-Age=5");
        set(&mut jar, "https://roblox.com/", "lasting=1; Max-Age=600");
        write_file(&path, &jar.encode().expect("encode")).expect("store");
        assert_eq!(read_file(&path, now()).expect("load"), Some(jar.clone()));
        let mode = std::fs::metadata(&path)
            .expect("metadata")
            .permissions()
            .mode();
        assert_eq!(mode & 0o777, 0o600);
        assert!(!path.with_extension(STAGING_EXTENSION).exists());
        let later = read_file(&path, at(NOW_S + 5))
            .expect("load")
            .expect("a jar");
        assert_eq!(
            names(&later.matching("https://roblox.com/", at(NOW_S + 5))),
            ["session", "lasting"]
        );

        std::fs::write(&path, [5, 0, 1]).expect("damage the jar");
        assert!(matches!(
            read_file(&path, now()),
            Err(JarFileError::Codec(_))
        ));
        assert_eq!(
            quarantine(&path).expect("quarantine"),
            dir.join("cookies.corrupt")
        );
        assert!(!path.exists());
        std::fs::remove_dir_all(&dir).expect("cleanup");
    }
}
