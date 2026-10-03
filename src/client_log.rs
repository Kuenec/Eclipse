use std::ffi::CStr;
use std::fmt;
use std::net::IpAddr;
use std::str::FromStr;
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::sync::mpsc::{self, Receiver, SyncSender};
use std::sync::{Mutex, OnceLock, PoisonError, TryLockError};

use tracing::Level;

const CLIENT_TAG: &str = "Roblox";
const LIBLOG_MESSAGE_MAX: usize = 4095;
const EVENT_QUEUE: usize = 32;
const HEADER_SEARCH: usize = 64;
const CHANNEL_SEARCH: usize = 40;
const TIMESTAMP_SHAPE: &[u8] = b"0000-00-00T00:00:00.000Z";

static TAP: OnceLock<Tap> = OnceLock::new();

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct PlaceId(pub u64);

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct UniverseId(pub u64);

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct JobId(u128);

impl JobId {
    fn parse(text: &str) -> Option<Self> {
        let bytes = text.as_bytes();
        if bytes.len() != 36 {
            return None;
        }
        let mut value = 0_u128;
        for (index, &byte) in bytes.iter().enumerate() {
            if matches!(index, 8 | 13 | 18 | 23) {
                if byte != b'-' {
                    return None;
                }
                continue;
            }
            value = value << 4 | u128::from(char::from(byte).to_digit(16)?);
        }
        Some(Self(value))
    }
}

impl fmt::Display for JobId {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        let value = self.0;
        write!(
            f,
            "{:08x}-{:04x}-{:04x}-{:04x}-{:012x}",
            value >> 96,
            (value >> 80) & 0xffff,
            (value >> 64) & 0xffff,
            (value >> 48) & 0xffff,
            value & 0xffff_ffff_ffff
        )
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum ClientEvent {
    JoiningPlace {
        place: PlaceId,
        job: JobId,
    },
    UniverseKnown {
        place: PlaceId,
        universe: UniverseId,
    },
    JoinedServer {
        addr: IpAddr,
        port: u16,
    },
    Teleporting,
    Left,
    ReturnedToApp,
    AttestationRequired,
    RpcPending,
}

#[derive(Debug, PartialEq, Eq)]
enum Record<'a> {
    Event(ClientEvent),
    Crash(CrashKind<'a>),
    BloxstrapRpc(&'a str),
}

#[derive(Debug, PartialEq, Eq)]
enum CrashKind<'a> {
    OutOfMemory {
        pool: MemoryPool,
        bytes: Option<u64>,
    },
    Other(&'a str),
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum MemoryPool {
    Graphics,
    System,
}

impl CrashKind<'_> {
    fn explanation(&self) -> String {
        match self {
            Self::OutOfMemory { pool, bytes } => {
                let memory = match pool {
                    MemoryPool::Graphics => "graphics memory",
                    MemoryPool::System => "memory",
                };
                let size = bytes
                    .map(|bytes| format!(" ({bytes} bytes)"))
                    .unwrap_or_default();
                format!(
                    "Roblox stopped: it reported that it could not allocate {memory}{size}. \
                     If this repeats, lower the graphics quality in Roblox's settings and report \
                     it with this log."
                )
            }
            Self::Other(kind) => format!("Roblox stopped with RBXCRASH: {kind}."),
        }
    }
}

#[derive(Clone, Copy)]
enum Channel {
    Output,
    GameJoinLoadTime,
    Network,
    UgcExperienceController,
    SingleSurfaceApp,
    Crash,
    CreatorOutput,
}

impl Channel {
    fn named(name: &str) -> Option<Self> {
        Some(match name {
            "FLog::Output" => Self::Output,
            "FLog::GameJoinLoadTime" => Self::GameJoinLoadTime,
            "FLog::Network" => Self::Network,
            "FLog::UgcExperienceController" => Self::UgcExperienceController,
            "FLog::SingleSurfaceApp" => Self::SingleSurfaceApp,
            "LOGCHANNELS + 1" => Self::Crash,
            "FLog::CreatorOutput" => Self::CreatorOutput,
            _ => return None,
        })
    }

    fn record(self, body: &str) -> Option<Record<'_>> {
        match self {
            Self::Output => joining_place(body).map(Record::Event),
            Self::GameJoinLoadTime => universe_known(body).map(Record::Event),
            Self::Network => network(body).map(Record::Event),
            Self::UgcExperienceController => body
                .starts_with("UgcExperienceController: doTeleport:")
                .then_some(Record::Event(ClientEvent::Teleporting)),
            Self::SingleSurfaceApp => match body {
                "leaveUGCGameInternal" => Some(Record::Event(ClientEvent::Left)),
                "setStage: (stage:LuaApp)" => Some(Record::Event(ClientEvent::ReturnedToApp)),
                _ => None,
            },
            Self::Crash => crash(body).map(Record::Crash),
            Self::CreatorOutput => body
                .strip_prefix("[BloxstrapRPC] ")
                .map(Record::BloxstrapRpc),
        }
    }
}

fn parse(record: &str) -> Option<Record<'_>> {
    if record.len() >= LIBLOG_MESSAGE_MAX {
        return None;
    }
    let (header, tagged) = split_header(record)?;
    let inner = tagged.strip_prefix('[')?;
    let close = inner
        .bytes()
        .take(CHANNEL_SEARCH)
        .position(|byte| byte == b']')?;
    let channel = Channel::named(&inner[..close])?;
    let body = inner[close + 1..].strip_prefix(' ')?;
    if !header.is_none_or(valid_header) {
        return None;
    }
    channel.record(body)
}

fn split_header(record: &str) -> Option<(Option<&str>, &str)> {
    match record.as_bytes().first()? {
        b'[' => Some((None, record)),
        byte if byte.is_ascii_digit() => {
            let space = record
                .bytes()
                .take(HEADER_SEARCH)
                .position(|byte| byte == b' ')?;
            Some((Some(&record[..space]), &record[space + 1..]))
        }
        _ => None,
    }
}

fn valid_header(header: &str) -> bool {
    let mut fields = header.split(',');
    let (Some(time), Some(seconds), Some(thread), Some(level)) =
        (fields.next(), fields.next(), fields.next(), fields.next())
    else {
        return false;
    };
    let severity = fields.next();
    fields.next().is_none()
        && time.len() == TIMESTAMP_SHAPE.len()
        && time.bytes().zip(TIMESTAMP_SHAPE).all(|(byte, &shape)| {
            if shape == b'0' {
                byte.is_ascii_digit()
            } else {
                byte == shape
            }
        })
        && seconds.split_once('.').is_some_and(|(whole, fraction)| {
            is_digits(whole, usize::MAX) && is_digits(fraction, usize::MAX)
        })
        && (1..=16).contains(&thread.len())
        && thread.bytes().all(|byte| byte.is_ascii_hexdigit())
        && is_digits(level, 3)
        && severity.is_none_or(|word| {
            (1..=16).contains(&word.len()) && word.bytes().all(|byte| byte.is_ascii_alphabetic())
        })
}

fn is_digits(text: &str, max_len: usize) -> bool {
    (1..=max_len).contains(&text.len()) && text.bytes().all(|byte| byte.is_ascii_digit())
}

fn decimal<T: FromStr>(text: &str) -> Option<T> {
    if !is_digits(text, usize::MAX) {
        return None;
    }
    text.parse().ok()
}

fn joining_place(body: &str) -> Option<ClientEvent> {
    let rest = body.strip_prefix("! Joining game '")?;
    let job = JobId::parse(rest.get(..36)?)?;
    let rest = rest[36..].strip_prefix("' place ")?;
    let place = PlaceId(decimal(&rest[..rest.find(" at ")?])?);
    Some(ClientEvent::JoiningPlace { place, job })
}

fn universe_known(body: &str) -> Option<ClientEvent> {
    let pairs = body.strip_prefix("Report game_join_loadtime: ")?;
    let place = PlaceId(join_report_value(pairs, ", placeid:")?);
    let universe = UniverseId(join_report_value(pairs, ", universeid:")?);
    Some(ClientEvent::UniverseKnown { place, universe })
}

fn join_report_value(pairs: &str, key: &str) -> Option<u64> {
    let start = pairs.find(key)? + key.len();
    decimal(pairs[start..].split(',').next()?)
}

fn network(body: &str) -> Option<ClientEvent> {
    if body == "Disconnect reason received: 318" {
        return Some(ClientEvent::AttestationRequired);
    }
    let (addr, port) = body.strip_prefix("serverId: ")?.split_once('|')?;
    Some(ClientEvent::JoinedServer {
        addr: addr.parse().ok()?,
        port: decimal(port)?,
    })
}

fn crash(body: &str) -> Option<CrashKind<'_>> {
    let text = body
        .strip_prefix("RBXCRASH: ")?
        .lines()
        .next()?
        .trim_end()
        .trim_end_matches('.');
    let (kind, detail) = match text.split_once(" (") {
        Some((kind, detail)) => (kind, Some(detail)),
        None => (text, None),
    };
    let pool = match kind {
        "OutOfMemoryGraphics" => MemoryPool::Graphics,
        "OutOfMemory" => MemoryPool::System,
        _ => return (!text.is_empty()).then_some(CrashKind::Other(text)),
    };
    Some(CrashKind::OutOfMemory {
        pool,
        bytes: detail.and_then(allocation_size),
    })
}

fn allocation_size(detail: &str) -> Option<u64> {
    const SIZE: &str = "size = ";
    let size = &detail[detail.find(SIZE)? + SIZE.len()..];
    let end = size
        .find(|c: char| !c.is_ascii_digit())
        .unwrap_or(size.len());
    decimal(&size[..end])
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum GameRpc {
    Ignored,
    Kept,
}

struct RpcSlot {
    latest: Mutex<String>,
    pending: AtomicBool,
}

pub struct Tap {
    events: SyncSender<ClientEvent>,
    dropped: AtomicU64,
    game_rpc: Option<RpcSlot>,
}

impl Tap {
    pub fn new(game_rpc: GameRpc) -> (Self, Receiver<ClientEvent>) {
        let (events, receiver) = mpsc::sync_channel(EVENT_QUEUE);
        let game_rpc = match game_rpc {
            GameRpc::Ignored => None,
            GameRpc::Kept => Some(RpcSlot {
                latest: Mutex::new(String::with_capacity(LIBLOG_MESSAGE_MAX)),
                pending: AtomicBool::new(false),
            }),
        };
        let tap = Self {
            events,
            dropped: AtomicU64::new(0),
            game_rpc,
        };
        (tap, receiver)
    }

    pub fn take_rpc(&self, into: &mut String) -> bool {
        let Some(slot) = &self.game_rpc else {
            return false;
        };
        slot.pending.store(false, Ordering::Release);
        let mut latest = slot.latest.lock().unwrap_or_else(PoisonError::into_inner);
        into.clear();
        into.push_str(&latest);
        latest.clear();
        !into.is_empty()
    }

    pub fn dropped_events(&self) -> u64 {
        self.dropped.load(Ordering::Relaxed)
    }

    fn send(&self, event: ClientEvent) -> bool {
        if self.events.try_send(event).is_ok() {
            return true;
        }
        self.dropped.fetch_add(1, Ordering::Relaxed);
        false
    }

    fn keep_rpc(&self, message: &str) {
        let Some(slot) = &self.game_rpc else {
            return;
        };
        let mut latest = match slot.latest.try_lock() {
            Ok(latest) => latest,
            Err(TryLockError::Poisoned(poisoned)) => poisoned.into_inner(),
            Err(TryLockError::WouldBlock) => return,
        };
        latest.clear();
        latest.push_str(message);
        drop(latest);
        if !slot.pending.swap(true, Ordering::AcqRel) && !self.send(ClientEvent::RpcPending) {
            slot.pending.store(false, Ordering::Release);
        }
    }
}

pub fn install(tap: Tap) -> Result<&'static Tap, Tap> {
    TAP.set(tap)?;
    Ok(TAP.get().expect("the client log tap was installed above"))
}

pub(crate) fn tap_installed() -> bool {
    #[cfg(test)]
    if TEST_TAP.with_borrow(Option::is_some) {
        return true;
    }
    TAP.get().is_some()
}

pub(crate) fn is_client_tag(tag: &CStr) -> bool {
    tag.to_bytes() == CLIENT_TAG.as_bytes()
}

pub(crate) fn offer(tag: &str, message: &str) {
    if !tap_installed() || tag != CLIENT_TAG {
        return;
    }
    match parse(message) {
        Some(Record::Event(event)) => route(|tap| {
            tap.send(event);
        }),
        Some(Record::BloxstrapRpc(message)) => route(|tap| tap.keep_rpc(message)),
        Some(Record::Crash(kind)) => explain(&kind),
        None => {}
    }
}

fn route(deliver: impl Fn(&Tap)) {
    #[cfg(test)]
    if TEST_TAP
        .with_borrow(|test| test.as_ref().map(|test| deliver(&test.tap)))
        .is_some()
    {
        return;
    }
    if let Some(tap) = TAP.get() {
        deliver(tap);
    }
}

fn explain(kind: &CrashKind<'_>) {
    let explanation = kind.explanation();
    #[cfg(test)]
    if TEST_TAP
        .with_borrow_mut(|test| {
            test.as_mut()
                .map(|test| test.explained.push(explanation.clone()))
        })
        .is_some()
    {
        return;
    }
    crate::diagnostics::record_status(Level::ERROR, &explanation);
}

#[cfg(test)]
thread_local! {
    static TEST_TAP: std::cell::RefCell<Option<Tapped>> = const { std::cell::RefCell::new(None) };
}

#[cfg(test)]
pub(crate) struct Tapped {
    pub(crate) tap: Tap,
    pub(crate) explained: Vec<String>,
}

#[cfg(test)]
pub(crate) fn with_test_tap(tap: Tap, body: impl FnOnce()) -> Tapped {
    TEST_TAP.with_borrow_mut(|test| {
        *test = Some(Tapped {
            tap,
            explained: Vec::new(),
        })
    });
    body();
    TEST_TAP
        .with_borrow_mut(Option::take)
        .expect("the test tap was set above")
}

#[cfg(test)]
mod tests;
