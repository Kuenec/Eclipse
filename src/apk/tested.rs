use std::collections::BTreeMap;
use std::fmt;
use std::str::FromStr;
use std::time::Instant;

use serde::Deserialize;

use super::VersionCode;
use crate::abi::Abi;
use crate::https::{self, DownloadError, Host};

pub const LISTED_ABI: Abi = Abi::X86_64;
const LIST_URL: &str =
    "https://raw.githubusercontent.com/Kuenec/Eclipse/compatibility/roblox-x86_64.json";
const LIST_HOSTS: &[Host] = &[Host::Exact("raw.githubusercontent.com")];
const LIST_SCHEMA: u32 = 1;
const MAX_LIST_BYTES: u64 = 64 * 1024;

#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Deserialize)]
#[serde(try_from = "String")]
pub struct EclipseVersion([u32; 3]);

impl EclipseVersion {
    pub const RUNNING: Self = Self([
        decimal(env!("CARGO_PKG_VERSION_MAJOR")),
        decimal(env!("CARGO_PKG_VERSION_MINOR")),
        decimal(env!("CARGO_PKG_VERSION_PATCH")),
    ]);
}

const fn decimal(digits: &str) -> u32 {
    let digits = digits.as_bytes();
    assert!(!digits.is_empty(), "a version part has digits");
    let mut value = 0;
    let mut index = 0;
    while index < digits.len() {
        assert!(digits[index].is_ascii_digit(), "a version part is decimal");
        value = value * 10 + (digits[index] - b'0') as u32;
        index += 1;
    }
    value
}

impl FromStr for EclipseVersion {
    type Err = String;

    fn from_str(text: &str) -> Result<Self, Self::Err> {
        let mut parts = text.split('.').map(|part| {
            let plain = !part.is_empty() && part.bytes().all(|byte| byte.is_ascii_digit());
            plain.then(|| part.parse::<u32>().ok()).flatten()
        });
        match (parts.next(), parts.next(), parts.next(), parts.next()) {
            (Some(Some(major)), Some(Some(minor)), Some(Some(patch)), None) => {
                Ok(Self([major, minor, patch]))
            }
            _ => Err(format!("{text:?} is not an Eclipse version")),
        }
    }
}

impl TryFrom<String> for EclipseVersion {
    type Error = String;

    fn try_from(text: String) -> Result<Self, Self::Error> {
        text.parse()
    }
}

impl fmt::Display for EclipseVersion {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        let [major, minor, patch] = self.0;
        write!(f, "{major}.{minor}.{patch}")
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Standing {
    Tested,

    TestedWithNewer(EclipseVersion),

    Untested,
}

#[derive(Deserialize)]
struct Versions {
    #[serde(default)]
    roblox: Vec<Entry>,
}

#[derive(Deserialize)]
struct Entry {
    version_code: VersionCode,

    eclipse: EclipseVersion,
}

#[derive(Debug, Default, PartialEq, Eq)]
pub struct TestedVersions {
    since: BTreeMap<VersionCode, EclipseVersion>,
}

impl TestedVersions {
    pub fn parse(body: &[u8]) -> Result<Self, ListError> {
        let schema = serde_json::from_slice::<Schema>(body)
            .map_err(ListError::Malformed)?
            .schema;
        if schema != LIST_SCHEMA {
            return Err(ListError::Schema(schema));
        }
        let versions: Versions = serde_json::from_slice(body).map_err(ListError::Malformed)?;
        let mut since = BTreeMap::new();
        for entry in versions.roblox {
            since
                .entry(entry.version_code)
                .and_modify(|eclipse: &mut EclipseVersion| *eclipse = (*eclipse).min(entry.eclipse))
                .or_insert(entry.eclipse);
        }
        Ok(Self { since })
    }

    pub fn standing(&self, roblox: VersionCode, eclipse: EclipseVersion) -> Standing {
        match self.since.get(&roblox) {
            Some(&since) if since <= eclipse => Standing::Tested,
            Some(&since) => Standing::TestedWithNewer(since),
            None => Standing::Untested,
        }
    }
}

#[derive(Deserialize)]
struct Schema {
    schema: u32,
}

pub fn fetch(deadline: Option<Instant>) -> Result<TestedVersions, ListError> {
    let agent = match deadline {
        None => https::api_agent(),
        Some(deadline) => {
            let left = deadline.saturating_duration_since(Instant::now());
            if left.is_zero() {
                return Err(ListError::OutOfTime);
            }
            https::api_agent_within(left)
        }
    };
    let body = https::fetch_capped(&agent, LIST_URL, LIST_HOSTS, MAX_LIST_BYTES)
        .map_err(ListError::Fetch)?;
    TestedVersions::parse(&body)
}

#[derive(Debug)]
pub enum ListError {
    OutOfTime,

    Fetch(DownloadError),

    Malformed(serde_json::Error),

    Schema(u32),
}

impl fmt::Display for ListError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::OutOfTime => f.write_str("the check ran out of time before asking for it"),
            Self::Fetch(error) => error.fmt(f),
            Self::Malformed(error) => write!(f, "it is not a list Eclipse can read: {error}"),
            Self::Schema(schema) => write!(
                f,
                "it has schema {schema}, and this Eclipse reads schema {LIST_SCHEMA}"
            ),
        }
    }
}

impl std::error::Error for ListError {
    fn source(&self) -> Option<&(dyn std::error::Error + 'static)> {
        match self {
            Self::Fetch(error) => Some(error),
            Self::Malformed(error) => Some(error),
            Self::OutOfTime | Self::Schema(_) => None,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    const LIST: &[u8] = br#"{
        "schema": 1,
        "roblox": [
            {"version_code": 20836, "version_name": "2.746.1266", "eclipse": "0.1.20"},
            {"version_code": 22294, "version_name": "2.748.1527", "eclipse": "0.1.24"},
            {"version_code": 22294, "version_name": "2.748.1527", "eclipse": "0.1.30"}
        ]
    }"#;

    fn eclipse(text: &str) -> EclipseVersion {
        text.parse().unwrap()
    }

    #[test]
    fn a_roblox_version_is_tested_from_the_eclipse_version_the_list_names() {
        let tested = TestedVersions::parse(LIST).unwrap();
        let roblox = VersionCode(22294);
        assert_eq!(tested.standing(roblox, eclipse("0.1.24")), Standing::Tested);
        assert_eq!(tested.standing(roblox, eclipse("0.2.0")), Standing::Tested);
        assert_eq!(
            tested.standing(roblox, eclipse("0.1.23")),
            Standing::TestedWithNewer(eclipse("0.1.24"))
        );
        assert_eq!(
            tested.standing(VersionCode(22400), eclipse("0.1.24")),
            Standing::Untested
        );
    }

    #[test]
    fn eclipse_versions_order_by_number_and_reject_other_text() {
        assert!(eclipse("0.1.9") < eclipse("0.1.10"));
        assert!(eclipse("0.9.9") < eclipse("1.0.0"));
        assert_eq!(eclipse("0.1.24").to_string(), "0.1.24");
        assert_eq!(
            EclipseVersion::RUNNING.to_string(),
            crate::VERSION,
            "the running version is the package version"
        );
        for text in [
            "", "0.1", "0.1.24.1", "0.1.x", "v0.1.24", "0.1.-2", "0.1.+2",
        ] {
            assert!(text.parse::<EclipseVersion>().is_err(), "{text}");
        }
    }

    #[test]
    fn a_list_eclipse_cannot_read_is_an_error() {
        let newer = TestedVersions::parse(br#"{"schema": 2, "roblox": "changed"}"#).unwrap_err();
        assert!(matches!(newer, ListError::Schema(2)), "{newer:?}");
        assert_eq!(
            newer.to_string(),
            "it has schema 2, and this Eclipse reads schema 1"
        );
        for body in [
            &b"<html>not found</html>"[..],
            br#"{"roblox": []}"#,
            br#"{"schema": 1, "roblox": [{"version_code": 22294, "eclipse": "soon"}]}"#,
            br#"{"schema": 1, "roblox": [{"eclipse": "0.1.24"}]}"#,
        ] {
            let error = TestedVersions::parse(body).unwrap_err();
            assert!(matches!(error, ListError::Malformed(_)), "{error:?}");
        }
        assert_eq!(
            TestedVersions::parse(br#"{"schema": 1}"#).unwrap(),
            TestedVersions::default()
        );
    }

    #[test]
    fn a_list_asked_for_after_the_deadline_is_not_requested() {
        let error = fetch(Some(Instant::now())).unwrap_err();
        assert!(matches!(error, ListError::OutOfTime), "{error:?}");
    }
}
