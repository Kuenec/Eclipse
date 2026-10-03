#![forbid(unsafe_code)]

use std::borrow::Cow;
use std::fmt;
use std::num::NonZeroU64;

const MAX_LINK_BYTES: usize = 8 * 1024;
const MAX_LAUNCH_DATA_BYTES: usize = 200;
const MAX_CODE_CHARS: usize = 64;
const MAX_NAME_BYTES: usize = 32;
const MAX_EXTRA_PARAMETERS: usize = 16;
const MAX_EXTRA_VALUE_BYTES: usize = 1024;
const REDACTED: &str = "<redacted>";
const UPPER_HEX: &[u8; 16] = b"0123456789ABCDEF";

const JOIN_SECRET_FIELDS: [Field; 4] = [
    Field::AccessCode,
    Field::LinkCode,
    Field::PrivateServerLinkCode,
    Field::ReservedServerAccessCode,
];

const APP_LINK_FIELDS: &[Field] = &[
    Field::PlaceId,
    Field::UserId,
    Field::GameInstanceId,
    Field::AccessCode,
    Field::LinkCode,
    Field::LaunchData,
    Field::BrowserTrackerId,
    Field::IsPlayTogetherGame,
    Field::IsPartyLeader,
    Field::ReferredByPlayerId,
    Field::JoinAttemptId,
    Field::JoinAttemptOrigin,
    Field::LaunchTime,
    Field::ReferralPage,
];

const PLACE_LAUNCHER_FIELDS: &[Field] = &[
    Field::Request,
    Field::PlaceId,
    Field::UserId,
    Field::GameId,
    Field::AccessCode,
    Field::LinkCode,
    Field::LaunchData,
    Field::BrowserTrackerId,
    Field::IsPlayTogetherGame,
    Field::IsPartyLeader,
    Field::ReferredByPlayerId,
    Field::JoinAttemptId,
    Field::JoinAttemptOrigin,
];

const PLAYER_FIELDS: &[Field] = &[
    Field::LaunchMode,
    Field::GameInfo,
    Field::PlaceLauncherUrl,
    Field::LaunchTime,
    Field::BrowserTrackerId,
    Field::RobloxLocale,
    Field::GameLocale,
    Field::Channel,
    Field::LaunchExperiment,
];

const GAME_PAGE_FIELDS: &[Field] = &[Field::PrivateServerLinkCode];

const SHARE_FIELDS: &[Field] = &[Field::ShareCode, Field::ShareType, Field::Stamp];

const UNSUPPORTED_FIELDS: &[Field] = &[
    Field::EventId,
    Field::CallId,
    Field::ReservedServerAccessCode,
];

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct PlaceId(NonZeroU64);

impl PlaceId {
    fn parse(text: &str) -> Option<Self> {
        decimal(text, false).and_then(NonZeroU64::new).map(Self)
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct UserId(NonZeroU64);

impl UserId {
    fn parse(text: &str) -> Option<Self> {
        decimal(text, false).and_then(NonZeroU64::new).map(Self)
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct JobId(String);

impl JobId {
    fn parse(text: &str) -> Option<Self> {
        is_uuid(text).then(|| Self(text.to_owned()))
    }
}

#[derive(Clone, PartialEq, Eq)]
pub struct AccessCode(String);

impl AccessCode {
    fn parse(text: &str) -> Option<Self> {
        is_uuid(text).then(|| Self(text.to_owned()))
    }
}

impl fmt::Debug for AccessCode {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "AccessCode({REDACTED})")
    }
}

#[derive(Clone, PartialEq, Eq)]
pub struct LinkCode(String);

impl LinkCode {
    fn parse(text: &str) -> Option<Self> {
        let valid = (1..=MAX_CODE_CHARS).contains(&text.len())
            && text.bytes().all(|byte| byte.is_ascii_digit());
        valid.then(|| Self(text.to_owned()))
    }
}

impl fmt::Debug for LinkCode {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "LinkCode({REDACTED})")
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct LaunchData(String);

impl LaunchData {
    fn parse(encoded: &str) -> Option<Self> {
        if encoded.contains('+') {
            return None;
        }
        let decoded = String::from_utf8(percent_decode(encoded)?).ok()?;
        let valid = (1..=MAX_LAUNCH_DATA_BYTES).contains(&decoded.len())
            && !decoded.chars().any(char::is_control)
            && !has_percent_escape(&decoded);
        valid.then_some(Self(decoded))
    }

    fn query(&self) -> String {
        let mut query = String::from("&launchData=");
        for byte in self.0.bytes() {
            if byte.is_ascii_alphanumeric() || matches!(byte, b'-' | b'.' | b'_' | b'~') {
                query.push(char::from(byte));
            } else {
                query.push('%');
                query.push(char::from(UPPER_HEX[usize::from(byte >> 4)]));
                query.push(char::from(UPPER_HEX[usize::from(byte & 0x0f)]));
            }
        }
        query
    }
}

#[derive(Clone, PartialEq, Eq)]
pub struct ShareCode(String);

impl ShareCode {
    fn parse(text: &str) -> Option<Self> {
        let valid = (1..=MAX_CODE_CHARS).contains(&text.len())
            && text.bytes().all(|byte| byte.is_ascii_alphanumeric());
        valid.then(|| Self(text.to_owned()))
    }
}

impl fmt::Debug for ShareCode {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "ShareCode({REDACTED})")
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum PrivateServerAccess {
    AccessCode {
        code: AccessCode,
        link: Option<LinkCode>,
    },
    LinkCode(LinkCode),
}

impl PrivateServerAccess {
    fn field(&self) -> Field {
        match self {
            Self::AccessCode { .. } => Field::AccessCode,
            Self::LinkCode(_) => Field::LinkCode,
        }
    }

    fn query(&self) -> String {
        match self {
            Self::AccessCode { code, link: None } => format!("&accessCode={}", code.0),
            Self::AccessCode {
                code,
                link: Some(link),
            } => format!("&accessCode={}&linkCode={}", code.0, link.0),
            Self::LinkCode(link) => format!("&linkCode={}", link.0),
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ShareKind {
    ExperienceDetails,
    ExperienceInvite,
    ExperienceAffiliate,
    Server,
    Profile,
    ScreenshotInvite,
}

impl ShareKind {
    const ALL: [Self; 6] = [
        Self::ExperienceDetails,
        Self::ExperienceInvite,
        Self::ExperienceAffiliate,
        Self::Server,
        Self::Profile,
        Self::ScreenshotInvite,
    ];

    const fn name(self) -> &'static str {
        match self {
            Self::ExperienceDetails => "ExperienceDetails",
            Self::ExperienceInvite => "ExperienceInvite",
            Self::ExperienceAffiliate => "ExperienceAffiliate",
            Self::Server => "Server",
            Self::Profile => "Profile",
            Self::ScreenshotInvite => "ScreenshotInvite",
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum LaunchTarget {
    Place {
        place: PlaceId,
        launch_data: Option<LaunchData>,
    },
    Server {
        place: PlaceId,
        job: JobId,
        launch_data: Option<LaunchData>,
    },
    PrivateServer {
        place: PlaceId,
        access: PrivateServerAccess,
        launch_data: Option<LaunchData>,
    },
    FollowUser {
        user: UserId,
    },
    Share {
        code: ShareCode,
        kind: ShareKind,
    },
}

impl LaunchTarget {
    pub fn android_uri(&self) -> String {
        let (place, join, launch_data) = match self {
            Self::Place { place, launch_data } => (place, String::new(), launch_data),
            Self::Server {
                place,
                job,
                launch_data,
            } => (place, format!("&gameInstanceId={}", job.0), launch_data),
            Self::PrivateServer {
                place,
                access,
                launch_data,
            } => (place, access.query(), launch_data),
            Self::FollowUser { user } => return format!("roblox://userId={}", user.0),
            Self::Share { code, kind } => {
                return format!(
                    "https://www.roblox.com/share?code={}&type={}",
                    code.0,
                    kind.name()
                )
            }
        };
        let launch_data = launch_data
            .as_ref()
            .map(LaunchData::query)
            .unwrap_or_default();
        format!("roblox://placeId={}{join}{launch_data}", place.0)
    }

    pub fn kind(&self) -> &'static str {
        match self {
            Self::Place { .. } => "game",
            Self::Server { .. } => "server",
            Self::PrivateServer { .. } => "private-server",
            Self::FollowUser { .. } => "friend",
            Self::Share { .. } => "share",
        }
    }
}

impl fmt::Display for LaunchTarget {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Place { place, .. } => write!(f, "place {}", place.0),
            Self::Server { place, .. } => write!(f, "place {} on a specific server", place.0),
            Self::PrivateServer { place, .. } => {
                write!(f, "place {} on a private server", place.0)
            }
            Self::FollowUser { user } => write!(f, "user {}", user.0),
            Self::Share {
                kind: ShareKind::Profile,
                ..
            } => f.write_str("the profile in a share link"),
            Self::Share { .. } => f.write_str("the experience in a share link"),
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct LinkError(Problem);

impl fmt::Display for LinkError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match &self.0 {
            Problem::JoinOrShortLink => f.write_str(
                "Roblox /join and /shortlink links cannot be opened directly yet; open the link \
                 in a web browser and press Play there.",
            ),
            Problem::ShopLink => f.write_str(
                "This is a Roblox shop link; Eclipse opens experience links. Open it in a web \
                 browser.",
            ),
            Problem::StudioLink => {
                f.write_str("This link is for Roblox Studio, which Eclipse does not run.")
            }
            Problem::NotYet(kind) => {
                write!(
                    f,
                    "Eclipse cannot open this kind of Roblox link yet ({kind})."
                )
            }
            Problem::UnknownShareType(kind) => {
                write!(
                    f,
                    "Eclipse does not know Roblox share links of type {kind} yet."
                )
            }
            Problem::Invalid(reason) => {
                write!(f, "This is not a Roblox link Eclipse can open: {reason}")
            }
        }
    }
}

impl std::error::Error for LinkError {}

#[derive(Debug, Clone, PartialEq, Eq)]
enum Problem {
    JoinOrShortLink,
    ShopLink,
    StudioLink,
    NotYet(LinkKind),
    UnknownShareType(Name),
    Invalid(Reason),
}

#[derive(Debug, Clone, PartialEq, Eq)]
enum LinkKind {
    Path(Name),
    Field(Field),
}

impl fmt::Display for LinkKind {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Path(name) => write!(f, "{name}"),
            Self::Field(field) => write!(f, "{field}"),
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
enum Reason {
    Empty,
    TooLong,
    NotPrintableAscii,
    Fragment,
    Scheme,
    Host,
    Path,
    Malformed,
    ProtocolVersion,
    UnknownField(Name),
    Duplicate(Field),
    InvalidValue(Field),
    Missing(Field),
    Conflict(Field, Field),
    Request,
    NestedLink,
    ExtraParameters,
}

impl fmt::Display for Reason {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Empty => f.write_str("the link is empty"),
            Self::TooLong => write!(f, "the link is longer than {} KiB", MAX_LINK_BYTES / 1024),
            Self::NotPrintableAscii => {
                f.write_str("the link has spaces, control characters or non-ASCII text")
            }
            Self::Fragment => f.write_str("the link has a # fragment"),
            Self::Scheme => f.write_str(
                "it is not a roblox://, robloxmobile://, roblox-player: or https://www.roblox.com \
                 link or a place ID",
            ),
            Self::Host => f.write_str("the host is not www.roblox.com, roblox.com or ro.blox.com"),
            Self::Path => f.write_str("the page is not a Roblox game, game start or share page"),
            Self::Malformed => f.write_str("a parameter is not name=value with a plain name"),
            Self::ProtocolVersion => f.write_str("the roblox-player link is not version 1"),
            Self::UnknownField(name) => write!(f, "{name} is not a parameter Eclipse knows"),
            Self::Duplicate(field) => write!(f, "{field} appears more than once"),
            Self::InvalidValue(field) => write!(f, "{field} is not valid"),
            Self::Missing(field) => write!(f, "{field} is missing"),
            Self::Conflict(first, second) => {
                write!(f, "{first} cannot be combined with {second}")
            }
            Self::Request => f.write_str("request does not match the other parameters"),
            Self::NestedLink => write!(f, "{} must hold a roblox:// link", Field::DeepLink),
            Self::ExtraParameters => write!(
                f,
                "the link has more than {MAX_EXTRA_PARAMETERS} parameters Eclipse does not use or \
                 one longer than {} KiB",
                MAX_EXTRA_VALUE_BYTES / 1024
            ),
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
struct Name(String);

impl Name {
    fn new(text: &str) -> Option<Self> {
        let valid = (1..=MAX_NAME_BYTES).contains(&text.len())
            && text
                .bytes()
                .all(|byte| byte.is_ascii_alphanumeric() || byte == b'_');
        valid.then(|| Self(text.to_owned()))
    }
}

impl fmt::Display for Name {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(&self.0)
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Field {
    PlaceId,
    UserId,
    GameInstanceId,
    GameId,
    AccessCode,
    LinkCode,
    PrivateServerLinkCode,
    ReservedServerAccessCode,
    LaunchData,
    Request,
    BrowserTrackerId,
    IsPlayTogetherGame,
    IsPartyLeader,
    ReferredByPlayerId,
    JoinAttemptId,
    JoinAttemptOrigin,
    LaunchTime,
    ReferralPage,
    EventId,
    CallId,
    ShareCode,
    ShareType,
    Stamp,
    DeepLink,
    LaunchMode,
    GameInfo,
    PlaceLauncherUrl,
    RobloxLocale,
    GameLocale,
    Channel,
    LaunchExperiment,
}

impl Field {
    const fn name(self) -> &'static str {
        match self {
            Self::PlaceId => "placeId",
            Self::UserId => "userId",
            Self::GameInstanceId => "gameInstanceId",
            Self::GameId => "gameId",
            Self::AccessCode => "accessCode",
            Self::LinkCode => "linkCode",
            Self::PrivateServerLinkCode => "privateServerLinkCode",
            Self::ReservedServerAccessCode => "reservedServerAccessCode",
            Self::LaunchData => "launchData",
            Self::Request => "request",
            Self::BrowserTrackerId => "browserTrackerId",
            Self::IsPlayTogetherGame => "isPlayTogetherGame",
            Self::IsPartyLeader => "isPartyLeader",
            Self::ReferredByPlayerId => "referredByPlayerId",
            Self::JoinAttemptId => "joinAttemptId",
            Self::JoinAttemptOrigin => "joinAttemptOrigin",
            Self::LaunchTime => "launchtime",
            Self::ReferralPage => "referralPage",
            Self::EventId => "eventId",
            Self::CallId => "callId",
            Self::ShareCode => "code",
            Self::ShareType => "type",
            Self::Stamp => "stamp",
            Self::DeepLink => "af_dp",
            Self::LaunchMode => "launchmode",
            Self::GameInfo => "gameinfo",
            Self::PlaceLauncherUrl => "placelauncherurl",
            Self::RobloxLocale => "robloxLocale",
            Self::GameLocale => "gameLocale",
            Self::Channel => "channel",
            Self::LaunchExperiment => "LaunchExp",
        }
    }

    fn named(self, name: &str) -> bool {
        self.name().eq_ignore_ascii_case(name)
    }

    fn metadata_accepts(self, value: &str) -> bool {
        match self {
            Self::BrowserTrackerId | Self::ReferredByPlayerId | Self::Stamp => {
                decimal(value, true).is_some()
            }
            Self::LaunchTime => decimal(value, false).is_some(),
            Self::IsPlayTogetherGame | Self::IsPartyLeader => value == "false",
            Self::JoinAttemptId | Self::JoinAttemptOrigin | Self::ReferralPage => {
                identifier(value, 64, false)
            }
            Self::RobloxLocale | Self::GameLocale => identifier(value, 16, false),
            Self::Channel => identifier(value, 64, true),
            Self::LaunchExperiment => value == "InApp",
            Self::GameInfo | Self::PlaceLauncherUrl => !value.is_empty(),
            Self::PlaceId
            | Self::UserId
            | Self::GameInstanceId
            | Self::GameId
            | Self::AccessCode
            | Self::LinkCode
            | Self::PrivateServerLinkCode
            | Self::ReservedServerAccessCode
            | Self::LaunchData
            | Self::Request
            | Self::EventId
            | Self::CallId
            | Self::ShareCode
            | Self::ShareType
            | Self::DeepLink
            | Self::LaunchMode => true,
        }
    }
}

impl fmt::Display for Field {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(self.name())
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Request {
    Game,
    GameJob,
    PrivateGame,
    FollowUser,
}

impl Request {
    fn parse(text: &str) -> Option<Self> {
        match text {
            "RequestGame" => Some(Self::Game),
            "RequestGameJob" => Some(Self::GameJob),
            "RequestPrivateGame" => Some(Self::PrivateGame),
            "RequestFollowUser" => Some(Self::FollowUser),
            _ => None,
        }
    }

    fn matches(self, target: &LaunchTarget) -> bool {
        matches!(
            (self, target),
            (Self::Game, LaunchTarget::Place { .. })
                | (Self::GameJob, LaunchTarget::Server { .. })
                | (
                    Self::PrivateGame,
                    LaunchTarget::PrivateServer {
                        access: PrivateServerAccess::AccessCode { .. },
                        ..
                    }
                )
                | (Self::FollowUser, LaunchTarget::FollowUser { .. })
        )
    }
}

#[derive(Clone, Copy)]
enum UnknownParameters {
    Refused,
    Ignored,
}

#[derive(Default)]
struct IgnoredParameters(usize);

impl IgnoredParameters {
    fn admit(&mut self, name: &str, value: &str) -> Result<(), LinkError> {
        if Name::new(name).is_none() {
            return Err(invalid(Reason::Malformed));
        }
        self.0 += 1;
        if self.0 > MAX_EXTRA_PARAMETERS || value.len() > MAX_EXTRA_VALUE_BYTES {
            return Err(invalid(Reason::ExtraParameters));
        }
        Ok(())
    }
}

struct Fields<'a>(Vec<(Field, &'a str)>);

impl<'a> Fields<'a> {
    fn collect(
        entries: impl Iterator<Item = &'a str>,
        separator: char,
        allowed: &[Field],
        unknown: UnknownParameters,
    ) -> Result<Self, LinkError> {
        let mut fields = Vec::new();
        let mut ignored = IgnoredParameters::default();
        for entry in entries {
            let (name, value) = entry
                .split_once(separator)
                .ok_or(invalid(Reason::Malformed))?;
            let Some(field) = allowed.iter().copied().find(|field| field.named(name)) else {
                match (unknown, unsupported_field(name)) {
                    (UnknownParameters::Ignored, None) => {
                        ignored.admit(name, value)?;
                        continue;
                    }
                    _ => return Err(unknown_field(name)),
                }
            };
            if fields.iter().any(|(seen, _)| *seen == field) {
                return Err(invalid(Reason::Duplicate(field)));
            }
            if !field.metadata_accepts(value) {
                return Err(invalid(Reason::InvalidValue(field)));
            }
            fields.push((field, value));
        }
        Ok(Self(fields))
    }

    fn get(&self, field: Field) -> Option<&'a str> {
        self.0
            .iter()
            .find(|(seen, _)| *seen == field)
            .map(|(_, value)| *value)
    }

    fn parse<T>(
        &self,
        field: Field,
        parse: impl FnOnce(&str) -> Option<T>,
    ) -> Result<Option<T>, LinkError> {
        self.get(field)
            .map(|value| parse(value).ok_or(invalid(Reason::InvalidValue(field))))
            .transpose()
    }

    fn require<T>(
        &self,
        field: Field,
        parse: impl FnOnce(&str) -> Option<T>,
    ) -> Result<T, LinkError> {
        self.parse(field, parse)?
            .ok_or(invalid(Reason::Missing(field)))
    }
}

pub fn parse(input: &str) -> Result<LaunchTarget, LinkError> {
    check_text(input)?;
    if input.bytes().all(|byte| byte.is_ascii_digit()) {
        let place = PlaceId::parse(input).ok_or(invalid(Reason::InvalidValue(Field::PlaceId)))?;
        return Ok(LaunchTarget::Place {
            place,
            launch_data: None,
        });
    }
    let (scheme, rest) = input.split_once(':').ok_or(invalid(Reason::Scheme))?;
    if scheme.eq_ignore_ascii_case("roblox-player") {
        return parse_player(rest);
    }
    let rest = rest.strip_prefix("//").ok_or(invalid(Reason::Scheme))?;
    if is_app_scheme(scheme) {
        parse_app_link(rest)
    } else if scheme.eq_ignore_ascii_case("https") {
        parse_website(rest)
    } else {
        Err(invalid(Reason::Scheme))
    }
}

fn check_text(text: &str) -> Result<(), LinkError> {
    if text.is_empty() {
        return Err(invalid(Reason::Empty));
    }
    if text.len() > MAX_LINK_BYTES {
        return Err(invalid(Reason::TooLong));
    }
    if !text.bytes().all(|byte| byte.is_ascii_graphic()) {
        return Err(invalid(Reason::NotPrintableAscii));
    }
    Ok(())
}

fn unfragmented(link: &str) -> Result<&str, LinkError> {
    if link.contains('#') {
        return Err(invalid(Reason::Fragment));
    }
    Ok(link)
}

fn is_app_scheme(scheme: &str) -> bool {
    scheme.eq_ignore_ascii_case("roblox") || scheme.eq_ignore_ascii_case("robloxmobile")
}

fn parse_app_link(link: &str) -> Result<LaunchTarget, LinkError> {
    let link = unfragmented(link)?;
    let link = link.strip_suffix('/').unwrap_or(link);
    match link.split_once('?') {
        Some((path, query)) if path.eq_ignore_ascii_case("experiences/start") => {
            parse_app_query(query)
        }
        Some((path, _)) => Err(unsupported_path(path)),
        None if link.eq_ignore_ascii_case("experiences/start") => {
            Err(invalid(Reason::Missing(Field::PlaceId)))
        }
        None if link.contains('=') => parse_app_query(link),
        None => Err(unsupported_path(link)),
    }
}

fn parse_app_query(query: &str) -> Result<LaunchTarget, LinkError> {
    let fields = Fields::collect(
        query.split('&'),
        '=',
        APP_LINK_FIELDS,
        UnknownParameters::Refused,
    )?;
    join_target(&fields, Field::GameInstanceId)
}

fn unsupported_path(path: &str) -> LinkError {
    let kind = path.split('/').next().unwrap_or_default();
    match Name::new(kind) {
        Some(kind) => LinkError(Problem::NotYet(LinkKind::Path(kind))),
        None => invalid(Reason::Path),
    }
}

fn join_target(fields: &Fields<'_>, job_field: Field) -> Result<LaunchTarget, LinkError> {
    let place = fields.parse(Field::PlaceId, PlaceId::parse)?;
    let user = fields.parse(Field::UserId, UserId::parse)?;
    let job = fields.parse(job_field, JobId::parse)?;
    let access_code = fields.parse(Field::AccessCode, AccessCode::parse)?;
    let link_code = match fields.get(Field::LinkCode) {
        Some("") => None,
        _ => fields.parse(Field::LinkCode, LinkCode::parse)?,
    };
    let launch_data = fields.parse(Field::LaunchData, LaunchData::parse)?;

    let place = match (place, user) {
        (Some(_), Some(_)) => return Err(invalid(Reason::Conflict(Field::PlaceId, Field::UserId))),
        (None, None) => return Err(invalid(Reason::Missing(Field::PlaceId))),
        (None, Some(user)) => {
            let extra = [
                job_field,
                Field::AccessCode,
                Field::LinkCode,
                Field::LaunchData,
            ]
            .into_iter()
            .find(|field| fields.get(*field).is_some());
            return match extra {
                Some(field) => Err(invalid(Reason::Conflict(Field::UserId, field))),
                None => Ok(LaunchTarget::FollowUser { user }),
            };
        }
        (Some(place), None) => place,
    };

    let access = match (access_code, link_code) {
        (Some(code), link) => Some(PrivateServerAccess::AccessCode { code, link }),
        (None, Some(link)) => Some(PrivateServerAccess::LinkCode(link)),
        (None, None) => None,
    };
    match (job, access) {
        (Some(_), Some(access)) => Err(invalid(Reason::Conflict(job_field, access.field()))),
        (Some(job), None) => Ok(LaunchTarget::Server {
            place,
            job,
            launch_data,
        }),
        (None, Some(access)) => Ok(LaunchTarget::PrivateServer {
            place,
            access,
            launch_data,
        }),
        (None, None) => Ok(LaunchTarget::Place { place, launch_data }),
    }
}

fn parse_player(payload: &str) -> Result<LaunchTarget, LinkError> {
    let mut entries = unfragmented(payload)?.split('+');
    if entries.next() != Some("1") {
        return Err(invalid(Reason::ProtocolVersion));
    }
    let studio = entries.clone().any(|entry| {
        entry
            .split_once(':')
            .is_some_and(|(name, mode)| Field::LaunchMode.named(name) && mode != "play")
    });
    if studio {
        return Err(LinkError(Problem::StudioLink));
    }

    let fields = Fields::collect(entries, ':', PLAYER_FIELDS, UnknownParameters::Refused)?;
    for required in [Field::LaunchMode, Field::GameInfo] {
        if fields.get(required).is_none() {
            return Err(invalid(Reason::Missing(required)));
        }
    }
    let launcher = fields
        .get(Field::PlaceLauncherUrl)
        .ok_or(invalid(Reason::Missing(Field::PlaceLauncherUrl)))?;
    let launcher = percent_decode(launcher)
        .and_then(|bytes| String::from_utf8(bytes).ok())
        .filter(|url| url.bytes().all(|byte| byte.is_ascii_graphic()))
        .ok_or(invalid(Reason::InvalidValue(Field::PlaceLauncherUrl)))?;
    if launcher.contains('#') {
        return Err(invalid(Reason::Fragment));
    }
    parse_place_launcher(&launcher)
}

fn parse_place_launcher(url: &str) -> Result<LaunchTarget, LinkError> {
    let bad_url = invalid(Reason::InvalidValue(Field::PlaceLauncherUrl));
    let Some((scheme, rest)) = url.split_once("://") else {
        return Err(bad_url);
    };
    if !scheme.eq_ignore_ascii_case("https") {
        return Err(bad_url);
    }
    let Some((authority, location)) = rest.split_once('/') else {
        return Err(bad_url);
    };
    if !authority.eq_ignore_ascii_case("assetgame.roblox.com")
        && !authority.eq_ignore_ascii_case("www.roblox.com")
    {
        return Err(bad_url);
    }
    let Some((path, query)) = location.split_once('?') else {
        return Err(bad_url);
    };
    if !launcher_path(path) {
        return Err(bad_url);
    }

    let fields = Fields::collect(
        query.split('&'),
        '=',
        PLACE_LAUNCHER_FIELDS,
        UnknownParameters::Ignored,
    )?;
    let request = fields.require(Field::Request, Request::parse)?;
    let target = join_target(&fields, Field::GameId)?;
    if !request.matches(&target) {
        return Err(invalid(Reason::Request));
    }
    Ok(target)
}

fn launcher_path(path: &str) -> bool {
    if matches!(path, "game/PlaceLauncher.ashx" | "Game/PlaceLauncher.ashx") {
        return true;
    }
    let mut segments = path.split('/');
    let (Some(locale), Some("Game"), Some("PlaceLauncher.ashx"), None) = (
        segments.next(),
        segments.next(),
        segments.next(),
        segments.next(),
    ) else {
        return false;
    };
    !locale.is_empty()
        && locale.len() <= 16
        && locale
            .bytes()
            .all(|byte| byte.is_ascii_alphanumeric() || matches!(byte, b'_' | b'-'))
}

fn parse_website(rest: &str) -> Result<LaunchTarget, LinkError> {
    let (authority, location) = rest.split_once('/').unwrap_or((rest, ""));
    if authority.eq_ignore_ascii_case("ro.blox.com") {
        let (path, query) = path_and_query(unfragmented(location)?);
        return parse_deferred_link(path, query);
    }
    if !authority.eq_ignore_ascii_case("www.roblox.com")
        && !authority.eq_ignore_ascii_case("roblox.com")
    {
        return Err(invalid(Reason::Host));
    }

    let page = location.split_once('#').map_or(location, |(page, _)| page);
    let (path, query) = path_and_query(page);
    let path = path.strip_suffix('/').unwrap_or(path);
    let segments: Vec<&str> = path.split('/').collect();
    let segments = match segments.split_first() {
        Some((first, rest)) if is_locale(first) && !rest.is_empty() => rest,
        _ => &segments[..],
    };
    let is = |segment: &str, keyword: &str| segment.eq_ignore_ascii_case(keyword);
    match segments {
        [games, start] if is(games, "games") && is(start, "start") => {
            let query = query.ok_or(invalid(Reason::Missing(Field::PlaceId)))?;
            parse_app_query(query.strip_suffix('/').unwrap_or(query))
        }
        [games, place] if is(games, "games") => parse_game_page(place, query),
        [games, place, slug] if is(games, "games") && is_slug(slug) => {
            parse_game_page(place, query)
        }
        [share] if is(share, "share") => parse_share(query),
        [first, ..] if is(first, "join") || is(first, "shortlink") => {
            Err(LinkError(Problem::JoinOrShortLink))
        }
        [first, ..] if is(first, "catalog") || is(first, "bundles") || is(first, "commerce") => {
            Err(LinkError(Problem::ShopLink))
        }
        _ => Err(invalid(Reason::Path)),
    }
}

fn path_and_query(location: &str) -> (&str, Option<&str>) {
    match location.split_once('?') {
        Some((path, query)) => (path, Some(query)),
        None => (location, None),
    }
}

fn is_locale(segment: &str) -> bool {
    let (language, region) = match segment.split_once('-') {
        Some((language, region)) => (language, Some(region)),
        None => (segment, None),
    };
    language.len() == 2
        && language.bytes().all(|byte| byte.is_ascii_alphabetic())
        && region.is_none_or(|region| {
            (2..=3).contains(&region.len())
                && region.bytes().all(|byte| byte.is_ascii_alphanumeric())
        })
}

fn is_slug(segment: &str) -> bool {
    !segment.is_empty()
        && segment.bytes().all(|byte| {
            byte.is_ascii_alphanumeric() || matches!(byte, b'-' | b'_' | b'.' | b'~' | b'%')
        })
}

fn parse_game_page(place: &str, query: Option<&str>) -> Result<LaunchTarget, LinkError> {
    let place = PlaceId::parse(place).ok_or(invalid(Reason::InvalidValue(Field::PlaceId)))?;
    let link = match query {
        Some(query) => Fields::collect(
            query.split('&'),
            '=',
            GAME_PAGE_FIELDS,
            UnknownParameters::Ignored,
        )?
        .parse(Field::PrivateServerLinkCode, LinkCode::parse)?,
        None => None,
    };
    Ok(match link {
        Some(link) => LaunchTarget::PrivateServer {
            place,
            access: PrivateServerAccess::LinkCode(link),
            launch_data: None,
        },
        None => LaunchTarget::Place {
            place,
            launch_data: None,
        },
    })
}

fn parse_share(query: Option<&str>) -> Result<LaunchTarget, LinkError> {
    let query = query.ok_or(invalid(Reason::Missing(Field::ShareCode)))?;
    let fields = Fields::collect(
        query.split('&'),
        '=',
        SHARE_FIELDS,
        UnknownParameters::Refused,
    )?;
    let code = fields.require(Field::ShareCode, ShareCode::parse)?;
    let kind = fields
        .get(Field::ShareType)
        .ok_or(invalid(Reason::Missing(Field::ShareType)))?;
    let Some(kind_name) = ShareKind::ALL
        .into_iter()
        .find(|known| known.name() == kind)
    else {
        return Err(match Name::new(kind) {
            Some(kind) => LinkError(Problem::UnknownShareType(kind)),
            None => invalid(Reason::InvalidValue(Field::ShareType)),
        });
    };
    Ok(LaunchTarget::Share {
        code,
        kind: kind_name,
    })
}

fn parse_deferred_link(path: &str, query: Option<&str>) -> Result<LaunchTarget, LinkError> {
    if path != "Ebh5" {
        return Err(invalid(Reason::Path));
    }
    let query = query.ok_or(invalid(Reason::Missing(Field::DeepLink)))?;
    let mut deep_link = None;
    let mut ignored = IgnoredParameters::default();
    for entry in query.split('&') {
        let (name, value) = entry.split_once('=').ok_or(invalid(Reason::Malformed))?;
        if Field::DeepLink.named(name) {
            if deep_link.replace(value).is_some() {
                return Err(invalid(Reason::Duplicate(Field::DeepLink)));
            }
            continue;
        }
        ignored.admit(name, value)?;
    }

    let deep_link = deep_link.ok_or(invalid(Reason::Missing(Field::DeepLink)))?;
    let deep_link = percent_decode(deep_link)
        .and_then(|bytes| String::from_utf8(bytes).ok())
        .ok_or(invalid(Reason::InvalidValue(Field::DeepLink)))?;
    check_text(&deep_link)?;
    match deep_link.split_once("://") {
        Some((scheme, rest)) if is_app_scheme(scheme) => parse_app_link(rest),
        _ => Err(invalid(Reason::NestedLink)),
    }
}

fn invalid(reason: Reason) -> LinkError {
    LinkError(Problem::Invalid(reason))
}

fn unsupported_field(name: &str) -> Option<Field> {
    UNSUPPORTED_FIELDS
        .iter()
        .copied()
        .find(|field| field.named(name))
}

fn unknown_field(name: &str) -> LinkError {
    if let Some(field) = unsupported_field(name) {
        return LinkError(Problem::NotYet(LinkKind::Field(field)));
    }
    match Name::new(name) {
        Some(name) => invalid(Reason::UnknownField(name)),
        None => invalid(Reason::Malformed),
    }
}

fn percent_decode(text: &str) -> Option<Vec<u8>> {
    let bytes = text.as_bytes();
    let mut decoded = Vec::with_capacity(bytes.len());
    let mut index = 0;
    while index < bytes.len() {
        if bytes[index] == b'%' {
            let high = hex(*bytes.get(index + 1)?)?;
            let low = hex(*bytes.get(index + 2)?)?;
            decoded.push((high << 4) | low);
            index += 3;
        } else {
            decoded.push(bytes[index]);
            index += 1;
        }
    }
    Some(decoded)
}

fn has_percent_escape(text: &str) -> bool {
    text.as_bytes().windows(3).any(|window| {
        window[0] == b'%' && window[1].is_ascii_hexdigit() && window[2].is_ascii_hexdigit()
    })
}

fn hex(byte: u8) -> Option<u8> {
    match byte {
        b'0'..=b'9' => Some(byte - b'0'),
        b'a'..=b'f' => Some(byte - b'a' + 10),
        b'A'..=b'F' => Some(byte - b'A' + 10),
        _ => None,
    }
}

fn decimal(text: &str, allow_zero: bool) -> Option<u64> {
    if text.is_empty()
        || text.len() > 20
        || !text.bytes().all(|byte| byte.is_ascii_digit())
        || (text.len() > 1 && text.starts_with('0'))
    {
        return None;
    }
    let parsed = text.parse::<u64>().ok()?;
    (allow_zero || parsed > 0).then_some(parsed)
}

fn identifier(text: &str, max_len: usize, allow_empty: bool) -> bool {
    (allow_empty || !text.is_empty())
        && text.len() <= max_len
        && text
            .bytes()
            .all(|byte| byte.is_ascii_alphanumeric() || matches!(byte, b'_' | b'-' | b'.'))
}

fn is_uuid(text: &str) -> bool {
    text.len() == 36
        && text.bytes().enumerate().all(|(index, byte)| match index {
            8 | 13 | 18 | 23 => byte == b'-',
            _ => byte.is_ascii_hexdigit(),
        })
}

pub fn redact_join_secrets(line: &str) -> Cow<'_, str> {
    if !line.contains("ode") {
        return Cow::Borrowed(line);
    }
    let bytes = line.as_bytes();
    let mut redacted: Option<String> = None;
    let mut copied = 0;
    for (index, _) in line.match_indices("ode") {
        if index < copied {
            continue;
        }
        let name_end = index + "ode".len();
        if !names_join_secret(&bytes[..name_end]) {
            continue;
        }
        let Some(value_start) = secret_value_start(&bytes[name_end..]).map(|skip| name_end + skip)
        else {
            continue;
        };
        let value_len = bytes[value_start..]
            .iter()
            .take_while(|byte| byte.is_ascii_alphanumeric() || matches!(byte, b'_' | b'-'))
            .count();
        if value_len == 0 {
            continue;
        }
        let output = redacted.get_or_insert_with(|| String::with_capacity(line.len()));
        output.push_str(&line[copied..value_start]);
        output.push_str(REDACTED);
        copied = value_start + value_len;
    }
    match redacted {
        Some(mut output) => {
            output.push_str(&line[copied..]);
            Cow::Owned(output)
        }
        None => Cow::Borrowed(line),
    }
}

fn names_join_secret(prefix: &[u8]) -> bool {
    if JOIN_SECRET_FIELDS
        .iter()
        .any(|field| ends_with_name(prefix, field.name()))
    {
        return true;
    }
    let share_code = Field::ShareCode.name();
    if !ends_with_name(prefix, share_code) {
        return false;
    }
    let before = &prefix[..prefix.len() - share_code.len()];
    before.ends_with(b"?")
        || before.ends_with(b"&")
        || [b"%3F", b"%26"].iter().any(|escape| {
            before
                .len()
                .checked_sub(escape.len())
                .is_some_and(|start| before[start..].eq_ignore_ascii_case(*escape))
        })
}

fn ends_with_name(text: &[u8], name: &str) -> bool {
    let name = name.as_bytes();
    let Some(start) = text.len().checked_sub(name.len()) else {
        return false;
    };
    let candidate = &text[start..];
    candidate[0].eq_ignore_ascii_case(&name[0]) && candidate[1..] == name[1..]
}

fn secret_value_start(rest: &[u8]) -> Option<usize> {
    if rest.starts_with(b"=") {
        return Some(1);
    }
    if rest
        .get(..3)
        .is_some_and(|escape| escape.eq_ignore_ascii_case(b"%3D"))
    {
        return Some(3);
    }
    let after_colon = rest.strip_prefix(b"\":")?;
    let after_space = after_colon.strip_prefix(b" ").unwrap_or(after_colon);
    let value = after_space.strip_prefix(b"\"").unwrap_or(after_space);
    Some(rest.len() - value.len())
}

#[cfg(test)]
mod tests {
    use super::*;

    const PLACE: u64 = 90_441_122_676_618;
    const JOB: &str = "3a5e0cf4-3e23-46a0-9dc7-887dad37e760";
    const ACCESS: &str = "8f3c2a10-5b6d-4e7f-9a1b-2c3d4e5f6a7b";
    const SHARE: &str = "2f4c6e8a0b1d3f5a7c9e1b3d5f7a9c0e";
    const SAMPLE: &str = "roblox-player:1+launchmode:play+gameinfo:[TICKET]+placelauncherurl:https%3A%2F%2Fassetgame.roblox.com%2Fgame%2FPlaceLauncher.ashx%3Frequest%3DRequestGame%26placeId%3D90441122676618";
    const WEBSITE_SAMPLE: &str = "roblox-player:1+launchmode:play+gameinfo:SECRET_TICKET+launchtime:1788192000000+placelauncherurl:https%3A%2F%2Fwww.roblox.com%2Ffr%2FGame%2FPlaceLauncher.ashx%3FjoinAttemptOrigin%3DPlayButton%26placeId%3D90441122676618%26request%3DRequestGame%26browserTrackerId%3D216042055264%26isPlayTogetherGame%3Dfalse%26isPartyLeader%3Dfalse%26referredByPlayerId%3D0%26joinAttemptId%3D3a5e0cf49-3e23-46a0-9dc7-887dad37e760+browsertrackerid:216042055264+robloxLocale:fr_fr+gameLocale:fr_fr+channel:+LaunchExp:InApp";

    fn player(launcher_query: &str) -> String {
        format!(
            "roblox-player:1+launchmode:play+gameinfo:TICKET+placelauncherurl:https%3A%2F%2Fassetgame.roblox.com%2Fgame%2FPlaceLauncher.ashx%3F{launcher_query}"
        )
    }

    fn place(id: u64) -> PlaceId {
        PlaceId(NonZeroU64::new(id).unwrap())
    }

    fn data(text: &str) -> Option<LaunchData> {
        Some(LaunchData(text.to_owned()))
    }

    fn game(id: u64, launch_data: Option<LaunchData>) -> LaunchTarget {
        LaunchTarget::Place {
            place: place(id),
            launch_data,
        }
    }

    fn private(id: u64, access: PrivateServerAccess) -> LaunchTarget {
        LaunchTarget::PrivateServer {
            place: place(id),
            access,
            launch_data: None,
        }
    }

    fn access_code(link: Option<&str>) -> PrivateServerAccess {
        PrivateServerAccess::AccessCode {
            code: AccessCode(ACCESS.to_owned()),
            link: link.map(|link| LinkCode(link.to_owned())),
        }
    }

    fn link_code(link: &str) -> PrivateServerAccess {
        PrivateServerAccess::LinkCode(LinkCode(link.to_owned()))
    }

    fn server(id: u64) -> LaunchTarget {
        LaunchTarget::Server {
            place: place(id),
            job: JobId(JOB.to_owned()),
            launch_data: None,
        }
    }

    fn accepted() -> Vec<(String, LaunchTarget, String)> {
        let at_place = format!("roblox://placeId={PLACE}");
        vec![
            (SAMPLE.into(), game(PLACE, None), at_place.clone()),
            (WEBSITE_SAMPLE.into(), game(PLACE, None), at_place.clone()),
            (
                player(&format!(
                    "request%3DRequestPrivateGame%26placeId%3D{PLACE}%26accessCode%3D{ACCESS}%26linkCode%3D0123456789"
                )),
                private(PLACE, access_code(Some("0123456789"))),
                format!("{at_place}&accessCode={ACCESS}&linkCode=0123456789"),
            ),
            (
                player(&format!("request%3DRequestGameJob%26placeId%3D{PLACE}%26gameId%3D{JOB}")),
                server(PLACE),
                format!("{at_place}&gameInstanceId={JOB}"),
            ),
            (
                player(&format!(
                    "request%3DRequestGameJob%26browserTrackerId%3D0%26placeId%3D{PLACE}%26gameId%3D{JOB}%26isPlayTogetherGame%3Dfalse%26isTeleport%3Dtrue%26isPrivateParty%3Dfalse%26teleportType%3D0%26gameJoinContext%3D%257B%2522x%2522%253A1%257D"
                )),
                server(PLACE),
                format!("{at_place}&gameInstanceId={JOB}"),
            ),
            (
                player(&format!(
                    "request%3DRequestPrivateGame%26placeId%3D{PLACE}%26accessCode%3D{ACCESS}%26linkCode%3D"
                )),
                private(PLACE, access_code(None)),
                format!("{at_place}&accessCode={ACCESS}"),
            ),
            (
                player("request%3DRequestFollowUser%26userId%3D261"),
                LaunchTarget::FollowUser {
                    user: UserId(NonZeroU64::new(261).unwrap()),
                },
                "roblox://userId=261".into(),
            ),
            (
                player(&format!(
                    "request%3DRequestGame%26placeId%3D{PLACE}%26launchData%3D%257B%2522room%2522%253A2%257D"
                )),
                game(PLACE, data("{\"room\":2}")),
                format!("{at_place}&launchData=%7B%22room%22%3A2%7D"),
            ),
            (
                format!("roblox://experiences/start?placeId={PLACE}&gameInstanceId={JOB}"),
                server(PLACE),
                format!("{at_place}&gameInstanceId={JOB}"),
            ),
            (
                format!("roblox://experiences/start?placeId={PLACE}&joinAttemptId={JOB}&joinAttemptOrigin=PlayButton&browserTrackerId=216042055264&referredByPlayerId=0&referralPage=GameDetail&launchtime=1788192000000"),
                game(PLACE, None),
                at_place.clone(),
            ),
            (
                format!("roblox://joinAttemptOrigin=ShareLink&placeId={PLACE}"),
                game(PLACE, None),
                at_place.clone(),
            ),
            (format!("roblox://placeId={PLACE}/"), game(PLACE, None), at_place.clone()),
            (format!("ROBLOX://PLACEID={PLACE}"), game(PLACE, None), at_place.clone()),
            (format!("robloxmobile://placeId={PLACE}"), game(PLACE, None), at_place.clone()),
            (
                format!("roblox://placeId={PLACE}&accessCode={ACCESS}"),
                private(PLACE, access_code(None)),
                format!("{at_place}&accessCode={ACCESS}"),
            ),
            (
                format!("roblox://placeId={PLACE}&linkCode=42"),
                private(PLACE, link_code("42")),
                format!("{at_place}&linkCode=42"),
            ),
            (
                "roblox://placeId=1818&launchData=%7B%22a%22%3A%221%26b%3D2%20%C3%A9%2B%22%7D".into(),
                game(1818, data("{\"a\":\"1&b=2 é+\"}")),
                "roblox://placeId=1818&launchData=%7B%22a%22%3A%221%26b%3D2%20%C3%A9%2B%22%7D".into(),
            ),
            (
                "https://www.roblox.com/games/start?placeId=1818&launchData=hello%20world".into(),
                game(1818, data("hello world")),
                "roblox://placeId=1818&launchData=hello%20world".into(),
            ),
            (
                "https://www.roblox.com/fr/games/1818/Classic-Crossroads".into(),
                game(1818, None),
                "roblox://placeId=1818".into(),
            ),
            (
                "https://roblox.com/games/1818".into(),
                game(1818, None),
                "roblox://placeId=1818".into(),
            ),
            (
                "https://www.roblox.com/pt-br/games/1818/Slug?privateServerLinkCode=00071".into(),
                private(1818, link_code("00071")),
                "roblox://placeId=1818&linkCode=00071".into(),
            ),
            (
                format!("https://www.roblox.com/games/2753915549/Blox-Fruits?gameSearchSessionInfo={JOB}&isAd=false&nativeAdData=&numberOfLoadedTiles=40&page=searchPage&placeId=2753915549&position=0&universeId=994732206"),
                game(2_753_915_549, None),
                "roblox://placeId=2753915549".into(),
            ),
            (
                format!("https://www.roblox.com/games/920587237/Adopt-Me?gameSetTypeId=100000003&homePageSessionInfo={JOB}&isAd=false&nativeAdData=&page=homePage&placeId=920587237&position=1&universeId=383310974"),
                game(920_587_237, None),
                "roblox://placeId=920587237".into(),
            ),
            (
                "https://www.roblox.com/games/920587237/Adopt-Me#!/game-instances".into(),
                game(920_587_237, None),
                "roblox://placeId=920587237".into(),
            ),
            (
                "https://www.roblox.com/games/920587237/Adopt-Me/".into(),
                game(920_587_237, None),
                "roblox://placeId=920587237".into(),
            ),
            (
                "https://roblox.com/games/1818/?privateServerLinkCode=00071&page=homePage#!/store".into(),
                private(1818, link_code("00071")),
                "roblox://placeId=1818&linkCode=00071".into(),
            ),
            (
                format!("https://www.roblox.com/share?code={SHARE}&type=Server&stamp=1788192000000"),
                LaunchTarget::Share {
                    code: ShareCode(SHARE.to_owned()),
                    kind: ShareKind::Server,
                },
                format!("https://www.roblox.com/share?code={SHARE}&type=Server"),
            ),
            (
                "https://ro.blox.com/Ebh5?af_dp=roblox%3A%2F%2FplaceId%3D1818%26launchData%3Dabc&af_web_dp=https%3A%2F%2Fwww.roblox.com%2Fgames%2F1818".into(),
                game(1818, data("abc")),
                "roblox://placeId=1818&launchData=abc".into(),
            ),
            ("1818".into(), game(1818, None), "roblox://placeId=1818".into()),
        ]
    }

    #[test]
    fn every_accepted_form_maps_to_its_target_and_android_uri() {
        for (input, target, uri) in accepted() {
            assert_eq!(parse(&input), Ok(target.clone()), "{input}");
            assert_eq!(target.android_uri(), uri, "{input}");
        }
    }

    #[test]
    fn every_rendered_uri_parses_back_to_the_same_target() {
        let longest = game(1, data(&"a".repeat(MAX_LAUNCH_DATA_BYTES)));
        let shares = ShareKind::ALL.map(|kind| LaunchTarget::Share {
            code: ShareCode(SHARE.to_owned()),
            kind,
        });
        let targets = accepted()
            .into_iter()
            .map(|(_, target, _)| target)
            .chain([longest])
            .chain(shares);
        for target in targets {
            assert_eq!(
                parse(&target.android_uri()),
                Ok(target.clone()),
                "{target:?}"
            );
        }
    }

    #[test]
    fn the_official_client_routes_every_rendered_uri_to_its_protocol_activity() {
        let Some(paths) =
            crate::apk::ApkSetPaths::from_env().expect("ECLIPSE_ROBLOX_APK must be usable")
        else {
            eprintln!("SKIP: set ECLIPSE_ROBLOX_APK to resolve links in the official manifest");
            return;
        };
        let apks = crate::apk::ApkSet::open(paths).expect("the official Roblox set verifies");

        for (input, _, uri) in accepted() {
            assert_eq!(
                apks.manifest().resolve_view_activity(&uri),
                Some("com.roblox.client.ActivityProtocolLaunch"),
                "{input}"
            );
        }
        assert_eq!(
            apks.manifest()
                .resolve_view_activity("https://www.roblox.com/home"),
            None
        );
    }

    #[test]
    fn rejects_lossy_ambiguous_or_untrusted_links_with_a_named_reason() {
        let not = |reason: &str| format!("This is not a Roblox link Eclipse can open: {reason}");
        let host = not("the host is not www.roblox.com, roblox.com or ro.blox.com");
        let scheme = not("it is not a roblox://, robloxmobile://, roblox-player: or https://www.roblox.com link or a place ID");
        let bad_launcher = not("placelauncherurl is not valid");
        let bad_data = not("launchData is not valid");
        let nested = not("af_dp must hold a roblox:// link");
        let wrong_request = not("request does not match the other parameters");
        let rejected = [
            (String::new(), not("the link is empty")),
            ("roblox://placeId=1&placeId=1".into(), not("placeId appears more than once")),
            ("roblox://placeId=1&PlaceID=1".into(), not("placeId appears more than once")),
            (
                "roblox://placeId=1&joinAttemptOrigin=a&JOINATTEMPTORIGIN=b".into(),
                not("joinAttemptOrigin appears more than once"),
            ),
            ("roblox://placeId=1%26userId=2".into(), not("placeId is not valid")),
            ("roblox://placeId%3D1".into(), not("the page is not a Roblox game, game start or share page")),
            ("roblox://placeId=1&launchData=a%2526b".into(), bad_data.clone()),
            (
                SAMPLE.replacen("%26placeId%3D", "%2526placeId%253D", 1),
                not("request is not valid"),
            ),
            (SAMPLE.replacen("https%3A", "https%253A", 1), bad_launcher.clone()),
            (
                format!("roblox://placeId=1&launchData={}", "a".repeat(MAX_LAUNCH_DATA_BYTES + 1)),
                bad_data.clone(),
            ),
            ("roblox://placeId=1&launchData=%FF".into(), bad_data.clone()),
            ("roblox://placeId=1&launchData=a%0Ab".into(), bad_data.clone()),
            ("roblox://placeId=1&launchData=a+b".into(), bad_data.clone()),
            ("roblox://placeId=1&launchData=".into(), bad_data.clone()),
            ("roblox://placeId=1&launchData=%G1".into(), bad_data),
            (
                "https://ro.blox.com/Ebh5?af_dp=https%3A%2F%2Fro.blox.com%2FEbh5%3Faf_dp%3Droblox%253A%252F%252FplaceId%253D1".into(),
                nested.clone(),
            ),
            (
                "https://ro.blox.com/Ebh5?af_dp=https%3A%2F%2Fwww.roblox.com%2Fgames%2F1".into(),
                nested.clone(),
            ),
            (
                format!("https://ro.blox.com/Ebh5?af_dp={}", SAMPLE.replace(':', "%3A").replace('+', "%2B")),
                nested,
            ),
            (
                "https://ro.blox.com/Ebh5?af_dp=roblox%3A%2F%2FplaceId%3D1&af_dp=roblox%3A%2F%2FplaceId%3D2".into(),
                not("af_dp appears more than once"),
            ),
            ("https://ro.blox.com/Ebh5?af_web_dp=x".into(), not("af_dp is missing")),
            (
                format!(
                    "https://ro.blox.com/Ebh5?af_dp=roblox%3A%2F%2FplaceId%3D1{}",
                    "&x=1".repeat(MAX_EXTRA_PARAMETERS + 1)
                ),
                not("the link has more than 16 parameters Eclipse does not use or one longer than 1 KiB"),
            ),
            (
                format!("https://ro.blox.com/Ebh5?af_dp=roblox%3A%2F%2FplaceId%3D1&x={}", "1".repeat(MAX_EXTRA_VALUE_BYTES + 1)),
                not("the link has more than 16 parameters Eclipse does not use or one longer than 1 KiB"),
            ),
            ("https://ro.blox.com/other?af_dp=roblox%3A%2F%2FplaceId%3D1".into(), not("the page is not a Roblox game, game start or share page")),
            (
                player(&format!("request%3DRequestGame%26placeId%3D1{}", "%26x%3D1".repeat(MAX_EXTRA_PARAMETERS + 1))),
                not("the link has more than 16 parameters Eclipse does not use or one longer than 1 KiB"),
            ),
            (
                player(&format!("request%3DRequestGame%26placeId%3D1%26x%3D{}", "1".repeat(MAX_EXTRA_VALUE_BYTES + 1))),
                not("the link has more than 16 parameters Eclipse does not use or one longer than 1 KiB"),
            ),
            (
                player("request%3DRequestGame%26placeId%3D1%26is-teleport%3Dtrue"),
                not("a parameter is not name=value with a plain name"),
            ),
            (
                player("request%3DRequestGame%26placeId%3D1%26placeId%3D2%26isTeleport%3Dtrue"),
                not("placeId appears more than once"),
            ),
            ("roblox://placeId=1&gameInstanceId=3a5e0cf4".into(), not("gameInstanceId is not valid")),
            (
                "roblox://placeId=1&gameInstanceId=3a5e0cf4-3e23-46a0-9dc7-887dad37e76z".into(),
                not("gameInstanceId is not valid"),
            ),
            ("roblox://placeId=1&accessCode=abc".into(), not("accessCode is not valid")),
            ("roblox://placeId=1&linkCode=12a".into(), not("linkCode is not valid")),
            (
                "https://www.roblox.com/games/1/Slug?privateServerLinkCode=12a".into(),
                not("privateServerLinkCode is not valid"),
            ),
            ("https://user@www.roblox.com/games/1".into(), host.clone()),
            ("https://www.roblox.com:443/games/1".into(), host.clone()),
            ("https://www.roblox.com.evil/games/1".into(), host),
            ("http://www.roblox.com/games/1".into(), scheme.clone()),
            ("roblox:placeId=1".into(), scheme.clone()),
            ("ftp://www.roblox.com/games/1".into(), scheme),
            ("roblox://placeId=1#join".into(), not("the link has a # fragment")),
            (format!("{SAMPLE}#join"), not("the link has a # fragment")),
            ("https://ro.blox.com/Ebh5?af_dp=roblox%3A%2F%2FplaceId%3D1#join".into(), not("the link has a # fragment")),
            ("https://ro.blox.com/Ebh5?af_dp=roblox%3A%2F%2FplaceId%3D1%23join".into(), not("the link has a # fragment")),
            (SAMPLE.replacen("%3Frequest", "%23x%3Frequest", 1), not("the link has a # fragment")),
            ("roblox://placeId=1&launchData=é".into(), not("the link has spaces, control characters or non-ASCII text")),
            (format!("{SAMPLE}\n"), not("the link has spaces, control characters or non-ASCII text")),
            (
                format!("roblox://placeId=1&launchData={}", "a".repeat(MAX_LINK_BYTES)),
                not("the link is longer than 8 KiB"),
            ),
            (SAMPLE.replacen("roblox-player:1", "roblox-player://1", 1), not("the roblox-player link is not version 1")),
            (SAMPLE.replacen("gameinfo:[TICKET]", "gameinfo:", 1), not("gameinfo is not valid")),
            (SAMPLE.replacen("+gameinfo:[TICKET]", "", 1), not("gameinfo is missing")),
            (format!("{SAMPLE}+unknown:value"), not("unknown is not a parameter Eclipse knows")),
            (SAMPLE.replacen("https%3A", "http%3A", 1), bad_launcher.clone()),
            (SAMPLE.replacen("assetgame.roblox.com", "assetgame.roblox.com.evil", 1), bad_launcher.clone()),
            (SAMPLE.replacen("PlaceLauncher.ashx", "Other.ashx", 1), bad_launcher.clone()),
            (SAMPLE.replacen("%2F", "%XZ", 1), bad_launcher),
            (
                SAMPLE.replacen("placeId%3D90441122676618", "placeId%3D090441122676618", 1),
                not("placeId is not valid"),
            ),
            (SAMPLE.replacen("RequestGame", "RequestPrivateGame", 1), wrong_request.clone()),
            (SAMPLE.replacen("RequestGame", "RequestCloudEdit", 1), not("request is not valid")),
            (format!("{SAMPLE}%26gameId%3D{JOB}"), wrong_request.clone()),
            (
                player("request%3DRequestPrivateGame%26placeId%3D1%26linkCode%3D42"),
                wrong_request,
            ),
            ("roblox://placeId=0".into(), not("placeId is not valid")),
            ("roblox://placeId=".into(), not("placeId is not valid")),
            ("0".into(), not("placeId is not valid")),
            ("01818".into(), not("placeId is not valid")),
            ("roblox://experiences/start".into(), not("placeId is missing")),
            ("roblox://experiences/start?".into(), not("a parameter is not name=value with a plain name")),
            ("roblox://?placeId=1".into(), not("the page is not a Roblox game, game start or share page")),
            ("roblox://placeId=1&".into(), not("a parameter is not name=value with a plain name")),
            ("roblox://userId=1&placeId=1".into(), not("placeId cannot be combined with userId")),
            ("roblox://userId=1&launchData=x".into(), not("userId cannot be combined with launchData")),
            (
                format!("roblox://placeId=1&gameInstanceId={JOB}&accessCode={ACCESS}"),
                not("gameInstanceId cannot be combined with accessCode"),
            ),
            (
                format!("roblox://placeId=1&gameInstanceId={JOB}&linkCode=1"),
                not("gameInstanceId cannot be combined with linkCode"),
            ),
            ("roblox://placeId=1&browserTrackerId=x".into(), not("browserTrackerId is not valid")),
            ("roblox://placeId=1&isPartyLeader=true".into(), not("isPartyLeader is not valid")),
            ("roblox://placeId=1&gameId=1".into(), not("gameId is not a parameter Eclipse knows")),
            (
                "https://www.roblox.com/games/1/Slug?privateServerLinkCode=1&page=x&privateServerLinkCode=2".into(),
                not("privateServerLinkCode appears more than once"),
            ),
            (
                format!("https://www.roblox.com/games/1/Slug?{}", "x=1&".repeat(MAX_EXTRA_PARAMETERS + 1)),
                not("the link has more than 16 parameters Eclipse does not use or one longer than 1 KiB"),
            ),
            ("https://www.roblox.com/games/1/Slug?".into(), not("a parameter is not name=value with a plain name")),
            ("https://www.roblox.com/games/1/Slug/More".into(), not("the page is not a Roblox game, game start or share page")),
            ("https://www.roblox.com/games/start".into(), not("placeId is missing")),
            ("https://www.roblox.com/users/1/profile".into(), not("the page is not a Roblox game, game start or share page")),
            ("https://www.roblox.com".into(), not("the page is not a Roblox game, game start or share page")),
            (format!("https://www.roblox.com/share?code={SHARE}"), not("type is missing")),
            ("https://www.roblox.com/share?code=bad-code&type=Server".into(), not("code is not valid")),
            ("https://www.roblox.com/share?type=Server".into(), not("code is missing")),
            (
                format!("https://www.roblox.com/share?code={SHARE}&type=Bad-Type"),
                not("type is not valid"),
            ),
        ];
        for (input, message) in rejected {
            assert_eq!(
                parse(&input).map_err(|error| error.to_string()),
                Err(message),
                "{input}"
            );
        }
    }

    #[test]
    fn specific_link_kinds_get_their_own_messages() {
        let join = "Roblox /join and /shortlink links cannot be opened directly yet; open the link in a web browser and press Play there.";
        let shop =
            "This is a Roblox shop link; Eclipse opens experience links. Open it in a web browser.";
        let studio = "This link is for Roblox Studio, which Eclipse does not run.";
        let reserved =
            player("request%3DRequestGame%26placeId%3D1%26reservedServerAccessCode%3Dabc");
        let cases = [
            ("https://www.roblox.com/join/AbC123", join.to_owned()),
            (
                "https://www.roblox.com/de/join?code=AbC123",
                join.to_owned(),
            ),
            (
                "https://www.roblox.com/shortlink?code=AbC123",
                join.to_owned(),
            ),
            ("https://www.roblox.com/catalog/1/Hat", shop.to_owned()),
            ("https://www.roblox.com/bundles/1/Bundle", shop.to_owned()),
            (
                "https://www.roblox.com/commerce/app-redirect?x=1",
                shop.to_owned(),
            ),
            (
                "roblox-player:1+launchmode:edit+gameinfo:T+task:EditPlace",
                studio.to_owned(),
            ),
            (
                "roblox-player:1+gameinfo:T+LAUNCHMODE:build",
                studio.to_owned(),
            ),
            (
                "roblox://navigation/home",
                "Eclipse cannot open this kind of Roblox link yet (navigation).".to_owned(),
            ),
            (
                "roblox://navigation/home?placeId=1",
                "Eclipse cannot open this kind of Roblox link yet (navigation).".to_owned(),
            ),
            (
                "roblox://placeId=1&eventId=5",
                "Eclipse cannot open this kind of Roblox link yet (eventId).".to_owned(),
            ),
            (
                "roblox://placeId=1&CALLID=5",
                "Eclipse cannot open this kind of Roblox link yet (callId).".to_owned(),
            ),
            (
                "https://www.roblox.com/games/1/Slug?page=homePage&eventId=5",
                "Eclipse cannot open this kind of Roblox link yet (eventId).".to_owned(),
            ),
            (
                "roblox://placeId=1&reservedServerAccessCode=abc",
                "Eclipse cannot open this kind of Roblox link yet (reservedServerAccessCode)."
                    .to_owned(),
            ),
            (
                reserved.as_str(),
                "Eclipse cannot open this kind of Roblox link yet (reservedServerAccessCode)."
                    .to_owned(),
            ),
            (
                "https://www.roblox.com/share?code=abc&type=Bogus",
                "Eclipse does not know Roblox share links of type Bogus yet.".to_owned(),
            ),
        ];
        for (input, message) in cases {
            assert_eq!(
                parse(input).map_err(|error| error.to_string()),
                Err(message),
                "{input}"
            );
        }
    }

    #[test]
    fn errors_never_echo_tickets_codes_or_launch_data() {
        let secret_uuid = "5ec7e700-0000-4000-8000-5ec7e75ec7e7";
        let cases = [
            ("SUPER_SECRET_TICKET_4f9d8c", "roblox-player:1+launchmode:edit+gameinfo:SUPER_SECRET_TICKET_4f9d8c".to_owned()),
            ("SUPER_SECRET_TICKET_4f9d8c", "roblox-player:1+launchmode:play+gameinfo:SUPER_SECRET_TICKET_4f9d8c+bogus:1".to_owned()),
            ("SUPER_SECRET_TICKET_4f9d8c", "roblox-player:1+launchmode:play+gameinfo:SUPER_SECRET_TICKET_4f9d8c+placelauncherurl:http%3A%2F%2Fx".to_owned()),
            ("SECRETACCESS", "roblox://placeId=1&accessCode=SECRETACCESS".to_owned()),
            (secret_uuid, format!("roblox://userId=1&accessCode={secret_uuid}")),
            (secret_uuid, format!("roblox://placeId=1&accessCode={secret_uuid}&accessCode={secret_uuid}")),
            (secret_uuid, player(&format!("request%3DRequestGame%26placeId%3D1%26accessCode%3D{secret_uuid}"))),
            ("98765432109", "roblox://placeId=1&linkCode=98765432109x".to_owned()),
            ("98765432109", "roblox://userId=1&linkCode=98765432109".to_owned()),
            ("98765432109", "https://www.roblox.com/games/1?privateServerLinkCode=98765432109&privateServerLinkCode=98765432109".to_owned()),
            ("98765432109", "https://www.roblox.com/games/1?privateServerLinkCode=98765432109A".to_owned()),
            (SHARE, format!("https://www.roblox.com/share?code={SHARE}&type=Bogus")),
            (SHARE, format!("https://www.roblox.com/share?code={SHARE}")),
            (SHARE, format!("https://www.roblox.com/share?code={SHARE}-&type=Server")),
            ("SECRETDATA", "roblox://placeId=1&launchData=SECRETDATA+1".to_owned()),
            ("SECRETDATA", "roblox://userId=1&launchData=SECRETDATA".to_owned()),
            ("SECRETDATA", "roblox://placeId=1&launchData=SECRETDATA%00".to_owned()),
        ];
        for (secret, input) in cases {
            let error = parse(&input).expect_err(&input);
            assert!(!error.to_string().contains(secret), "{input}: {error}");
            assert!(!format!("{error:?}").contains(secret), "{input}: {error:?}");
        }
    }

    #[test]
    fn descriptions_name_the_kind_and_place_but_no_code_or_launch_data() {
        let share = |kind| LaunchTarget::Share {
            code: ShareCode(SHARE.to_owned()),
            kind,
        };
        let cases = [
            (game(1818, data("SECRETDATA")), "game", "place 1818"),
            (server(1818), "server", "place 1818 on a specific server"),
            (
                private(1818, access_code(Some("98765432109"))),
                "private-server",
                "place 1818 on a private server",
            ),
            (
                private(1818, link_code("98765432109")),
                "private-server",
                "place 1818 on a private server",
            ),
            (
                LaunchTarget::FollowUser {
                    user: UserId(NonZeroU64::new(261).unwrap()),
                },
                "friend",
                "user 261",
            ),
            (
                share(ShareKind::Server),
                "share",
                "the experience in a share link",
            ),
            (
                share(ShareKind::Profile),
                "share",
                "the profile in a share link",
            ),
        ];
        for (target, kind, description) in cases {
            assert_eq!(
                (target.kind(), target.to_string().as_str()),
                (kind, description)
            );
        }
    }

    #[test]
    fn debug_output_of_a_target_hides_join_codes() {
        let targets = [
            private(1, access_code(Some("98765432109"))),
            private(1, link_code("98765432109")),
            LaunchTarget::Share {
                code: ShareCode(SHARE.to_owned()),
                kind: ShareKind::ExperienceInvite,
            },
        ];
        for target in targets {
            let debug = format!("{target:?}");
            for secret in [ACCESS, "98765432109", SHARE] {
                assert!(!debug.contains(secret), "{debug}");
            }
            assert!(debug.contains(REDACTED), "{debug}");
        }
    }

    #[test]
    fn redaction_replaces_join_secrets_in_plain_percent_encoded_and_json_forms() {
        let cases = [
            (
                format!("PlaceLauncher accessCode={ACCESS}&linkCode=0123 placeId=1 gameInstanceId={JOB} errorCode=5"),
                format!("PlaceLauncher accessCode=<redacted>&linkCode=<redacted> placeId=1 gameInstanceId={JOB} errorCode=5"),
            ),
            (
                format!("intent roblox%3A%2F%2FplaceId%3D1%26AccessCode%3D{ACCESS}%26linkCode%3d0123"),
                "intent roblox%3A%2F%2FplaceId%3D1%26AccessCode%3D<redacted>%26linkCode%3d<redacted>".to_owned(),
            ),
            (
                format!("{{\"accessCode\":\"{ACCESS}\",\"linkCode\": \"0123\",\"PrivateServerLinkCode\":77,\"code\":\"x\"}}"),
                "{\"accessCode\":\"<redacted>\",\"linkCode\": \"<redacted>\",\"PrivateServerLinkCode\":<redacted>,\"code\":\"x\"}".to_owned(),
            ),
            (
                "games/1/Slug?privateServerLinkCode=00071 reservedServerAccessCode=abc_DEF-1".to_owned(),
                "games/1/Slug?privateServerLinkCode=<redacted> reservedServerAccessCode=<redacted>".to_owned(),
            ),
            (
                format!("Share link received: https://www.roblox.com/share?code={SHARE}&type=Server"),
                "Share link received: https://www.roblox.com/share?code=<redacted>&type=Server".to_owned(),
            ),
            (
                format!("share%3Fcode%3D{SHARE} x%26Code%3d{SHARE} y&code=abc ?Code=abc"),
                "share%3Fcode%3D<redacted> x%26Code%3d<redacted> y&code=<redacted> ?Code=<redacted>".to_owned(),
            ),
        ];
        for (line, expected) in cases {
            assert_eq!(redact_join_secrets(&line), expected, "{line}");
        }
    }

    #[test]
    fn redaction_keeps_other_values_and_leaves_lines_without_secrets_in_place() {
        for line in [
            format!("errorCode=5 gameInstanceId={JOB} placeId=1 code=7 mode=play accessCode= linkCode=\"\""),
            "frame time 16.6 ms".to_owned(),
        ] {
            assert!(
                matches!(redact_join_secrets(&line), Cow::Borrowed(kept) if kept == line),
                "{line}"
            );
        }
    }
}
