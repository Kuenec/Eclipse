use std::collections::VecDeque;
use std::fmt;
use std::net::{IpAddr, Ipv4Addr, Ipv6Addr};
use std::time::{Duration, Instant};

use crate::https::{self, DownloadError, Host};

const LOOKUP_HOSTS: &[Host] = &[Host::Exact("ipinfo.io")];
const RESPONSE_LIMIT: u64 = 64 * 1024;
const REMEMBERED: usize = 64;
const PAUSE_AFTER_FAILURE: Duration = Duration::from_secs(60);
const PLACE_FIELDS: [&str; 3] = ["city", "region", "country"];

fn is_global(addr: IpAddr) -> bool {
    match addr {
        IpAddr::V4(addr) => is_global_v4(addr),
        IpAddr::V6(addr) => is_global_v6(addr),
    }
}

fn is_global_v4(addr: Ipv4Addr) -> bool {
    let [first, second, ..] = addr.octets();
    let shared = first == 100 && second & 0xc0 == 64;
    let benchmarking = first == 198 && second & 0xfe == 18;
    let reserved = first >= 240;
    !(addr.is_private()
        || addr.is_loopback()
        || addr.is_link_local()
        || addr.is_unspecified()
        || addr.is_broadcast()
        || addr.is_multicast()
        || addr.is_documentation()
        || shared
        || benchmarking
        || reserved)
}

fn is_global_v6(addr: Ipv6Addr) -> bool {
    let [first, second, ..] = addr.segments();
    let documentation = first == 0x2001 && second == 0x0db8;
    first & 0xe000 == 0x2000 && !documentation
}

fn lookup_url(addr: IpAddr) -> String {
    format!("https://ipinfo.io/{addr}/json")
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub(crate) struct Location(String);

impl Location {
    pub(crate) fn from_ipinfo(body: &[u8]) -> Result<Self, LookupError> {
        let value: serde_json::Value = serde_json::from_slice(body).map_err(LookupError::Json)?;
        let fields = value.as_object().ok_or(LookupError::NotAnObject)?;
        let mut parts: Vec<&str> = PLACE_FIELDS
            .iter()
            .filter_map(|field| fields.get(*field)?.as_str())
            .map(str::trim)
            .filter(|part| !part.is_empty())
            .collect();
        parts.dedup();
        if parts.is_empty() {
            return Err(LookupError::NoPlace);
        }
        Ok(Self(parts.join(", ")))
    }

    pub(crate) fn as_str(&self) -> &str {
        &self.0
    }
}

#[derive(Debug)]
pub(crate) enum LookupError {
    Fetch(DownloadError),
    Json(serde_json::Error),
    NotAnObject,
    NoPlace,
}

impl fmt::Display for LookupError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Fetch(error) => write!(f, "ipinfo.io: {error}"),
            Self::Json(error) => write!(f, "ipinfo.io sent invalid JSON: {error}"),
            Self::NotAnObject => f.write_str("ipinfo.io did not answer with a JSON object"),
            Self::NoPlace => f.write_str("ipinfo.io named no city, region or country"),
        }
    }
}

impl std::error::Error for LookupError {
    fn source(&self) -> Option<&(dyn std::error::Error + 'static)> {
        match self {
            Self::Fetch(error) => Some(error),
            Self::Json(error) => Some(error),
            Self::NotAnObject | Self::NoPlace => None,
        }
    }
}

pub(crate) fn ipinfo() -> impl FnMut(IpAddr) -> Result<Location, LookupError> {
    let mut agent = None;
    move |addr| {
        let agent = agent.get_or_insert_with(https::api_agent);
        let body = https::fetch_capped(agent, &lookup_url(addr), LOOKUP_HOSTS, RESPONSE_LIMIT)
            .map_err(LookupError::Fetch)?;
        Location::from_ipinfo(&body)
    }
}

pub(crate) struct Locator<Fetch> {
    fetch: Fetch,
    known: VecDeque<(IpAddr, Location)>,
    paused_until: Option<Instant>,
}

impl<Fetch: FnMut(IpAddr) -> Result<Location, LookupError>> Locator<Fetch> {
    pub(crate) fn new(fetch: Fetch) -> Self {
        Self {
            fetch,
            known: VecDeque::with_capacity(REMEMBERED),
            paused_until: None,
        }
    }

    pub(crate) fn locate(&mut self, addr: IpAddr, now: Instant) -> Option<Location> {
        if !is_global(addr) {
            return None;
        }
        if let Some((_, location)) = self.known.iter().find(|(known, _)| *known == addr) {
            return Some(location.clone());
        }
        if self.paused_until.is_some_and(|until| now < until) {
            return None;
        }
        match (self.fetch)(addr) {
            Ok(location) => {
                self.remember(addr, location.clone());
                Some(location)
            }
            Err(error) => {
                self.paused_until = Some(now + PAUSE_AFTER_FAILURE);
                tracing::warn!(
                    %error,
                    "cannot look up where the Roblox server is; Eclipse asks again after {} s",
                    PAUSE_AFTER_FAILURE.as_secs()
                );
                None
            }
        }
    }

    fn remember(&mut self, addr: IpAddr, location: Location) {
        if self.known.len() == REMEMBERED {
            self.known.pop_front();
        }
        self.known.push_back((addr, location));
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::cell::Cell;

    const IPINFO: &[u8] = include_bytes!("../tests/fixtures/ipinfo.json");
    const SERVER: IpAddr = IpAddr::V4(Ipv4Addr::new(128, 116, 0, 1));

    fn fixture(_: IpAddr) -> Result<Location, LookupError> {
        Location::from_ipinfo(IPINFO)
    }

    fn never_fetched(addr: IpAddr) -> Result<Location, LookupError> {
        panic!("{addr} is not looked up")
    }

    fn rate_limited(_: IpAddr) -> Result<Location, LookupError> {
        Err(LookupError::Fetch(DownloadError::Status(429)))
    }

    #[test]
    fn only_global_addresses_are_looked_up() {
        for addr in [
            "10.0.0.1",
            "172.16.0.1",
            "192.168.1.1",
            "100.64.0.1",
            "127.0.0.1",
            "169.254.1.1",
            "0.0.0.0",
            "255.255.255.255",
            "224.0.0.1",
            "192.0.2.1",
            "198.18.0.1",
            "240.0.0.1",
            "::1",
            "fe80::1",
            "fc00::1",
            "2001:db8::1",
        ] {
            assert!(!is_global(addr.parse().unwrap()), "{addr}");
        }
        for addr in ["128.116.0.1", "8.8.8.8", "100.128.0.1", "2606:4700::1"] {
            assert!(is_global(addr.parse().unwrap()), "{addr}");
        }
    }

    #[test]
    fn the_lookup_url_carries_only_the_address() {
        assert_eq!(lookup_url(SERVER), "https://ipinfo.io/128.116.0.1/json");
        for addr in [SERVER, "2606:4700::1".parse().unwrap()] {
            https::trusted_uri(&lookup_url(addr), LOOKUP_HOSTS).unwrap();
        }
    }

    #[test]
    fn an_ipinfo_answer_reads_as_city_region_and_country() {
        assert_eq!(
            Location::from_ipinfo(IPINFO).unwrap().as_str(),
            "San Mateo, California, US"
        );
        for (body, shown) in [
            (
                r#"{"city": "", "region": "Hesse", "country": "DE"}"#,
                "Hesse, DE",
            ),
            (r#"{"region": " Hesse ", "country": "DE"}"#, "Hesse, DE"),
            (
                r#"{"city": "Singapore", "region": "Singapore", "country": "SG"}"#,
                "Singapore, SG",
            ),
            (r#"{"city": null, "region": 5, "country": "US"}"#, "US"),
        ] {
            assert_eq!(
                Location::from_ipinfo(body.as_bytes()).unwrap().as_str(),
                shown
            );
        }
    }

    #[test]
    fn an_answer_without_a_place_is_an_error() {
        assert!(matches!(
            Location::from_ipinfo(br#"["San Mateo", "California", "US"]"#),
            Err(LookupError::NotAnObject)
        ));
        assert!(matches!(
            Location::from_ipinfo(br#""San Mateo""#),
            Err(LookupError::NotAnObject)
        ));
        assert!(matches!(
            Location::from_ipinfo(br#"{"ip": "128.116.0.1", "bogon": true}"#),
            Err(LookupError::NoPlace)
        ));
        assert!(matches!(
            Location::from_ipinfo(b"<html>"),
            Err(LookupError::Json(_))
        ));
    }

    #[test]
    fn a_located_server_is_remembered_and_the_oldest_of_65_is_forgotten() {
        let fetched = Cell::new(0);
        let mut locator = Locator::new(|addr| {
            fetched.set(fetched.get() + 1);
            fixture(addr)
        });
        let now = Instant::now();
        let servers: Vec<IpAddr> = (1..=65)
            .map(|host| IpAddr::V4(Ipv4Addr::new(128, 116, 1, host)))
            .collect();
        for &server in &servers {
            assert!(locator.locate(server, now).is_some());
        }
        assert_eq!(fetched.get(), 65);
        assert_eq!(locator.known.len(), REMEMBERED);

        for &server in &servers[1..] {
            locator.locate(server, now);
        }
        assert_eq!(fetched.get(), 65, "the last 64 servers are remembered");
        locator.locate(servers[0], now);
        assert_eq!(fetched.get(), 66, "the first server was forgotten");
    }

    #[test]
    fn a_failed_lookup_pauses_lookups_for_a_minute() {
        let fetched = Cell::new(0);
        let failing = Cell::new(true);
        let mut locator = Locator::new(|addr| {
            fetched.set(fetched.get() + 1);
            if failing.get() {
                rate_limited(addr)
            } else {
                fixture(addr)
            }
        });
        let failed_at = Instant::now();
        assert_eq!(locator.locate(SERVER, failed_at), None);
        assert_eq!(fetched.get(), 1);

        failing.set(false);
        let other = IpAddr::V4(Ipv4Addr::new(128, 116, 0, 2));
        for server in [SERVER, other] {
            assert_eq!(
                locator.locate(server, failed_at + Duration::from_secs(59)),
                None
            );
        }
        assert_eq!(fetched.get(), 1, "no request goes out within 60 s");

        assert!(locator
            .locate(SERVER, failed_at + PAUSE_AFTER_FAILURE)
            .is_some());
        assert_eq!(fetched.get(), 2);
    }

    #[test]
    fn a_non_global_server_is_never_looked_up() {
        let mut locator = Locator::new(never_fetched);
        for addr in ["10.0.0.1", "192.168.1.1", "fc00::1"] {
            assert_eq!(locator.locate(addr.parse().unwrap(), Instant::now()), None);
        }
    }

    #[test]
    fn lookup_errors_name_ipinfo_and_the_cause() {
        assert_eq!(
            rate_limited(SERVER).unwrap_err().to_string(),
            "ipinfo.io: the server answered HTTP 429"
        );
        assert_eq!(
            Location::from_ipinfo(b"[]").unwrap_err().to_string(),
            "ipinfo.io did not answer with a JSON object"
        );
    }
}
