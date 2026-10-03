use std::ffi::OsStr;
use std::fmt::{self, Write as _};
use std::mem;
use std::os::unix::ffi::OsStrExt as _;
use std::os::unix::fs::FileTypeExt as _;
use std::path::Path;
use std::sync::mpsc::{self, Receiver, SyncSender, TrySendError};
use std::sync::OnceLock;
use std::time::Duration;

use dbus::arg::{AppendAll, Dict, PropMap, ReadAll, Variant};
use dbus::blocking::Connection;

const QUEUED_REQUESTS: usize = 32;
const CALL_TIMEOUT: Duration = Duration::from_secs(5);

const PORTAL_BUS_NAME: &str = "org.freedesktop.portal.Desktop";
const PORTAL_OBJECT: &str = "/org/freedesktop/portal/desktop";
const OPEN_URI: &str = "org.freedesktop.portal.OpenURI";
const GAME_MODE: &str = "org.freedesktop.portal.GameMode";
const REGISTER_GAME: &str = "RegisterGameByPIDFd";
const GAME_REGISTERED: i32 = 0;
const GAME_MODE_UNREACHABLE: i32 = -2;
const INHIBIT: &str = "org.freedesktop.portal.Inhibit";
const NOTIFICATION: &str = "org.freedesktop.portal.Notification";
const REQUEST: &str = "org.freedesktop.portal.Request";

const INHIBIT_IDLE: u32 = 8;
const INHIBIT_REASON: &str = "Playing Roblox";

const GAMEMODE_AUTO_PRELOAD: &[u8] = b"libgamemodeauto";

const URI_LIMIT: usize = 8 * 1024;
const REPORTED_SCHEME_LIMIT: usize = 16;

static WORKER: OnceLock<Option<PortalWorker>> = OnceLock::new();

#[derive(PartialEq, Eq)]
pub struct ExternalUri(String);

#[derive(Clone, Debug, PartialEq, Eq)]
pub enum UriRefusal {
    TooLong,
    NotPrintableAscii,
    NoScheme,
    UnsupportedScheme(Option<String>),
    NoHost,
    NoAddress,
}

impl ExternalUri {
    pub fn parse(text: &str) -> Result<Self, UriRefusal> {
        if text.len() > URI_LIMIT {
            return Err(UriRefusal::TooLong);
        }
        if !text.bytes().all(|byte| byte.is_ascii_graphic()) {
            return Err(UriRefusal::NotPrintableAscii);
        }
        let Some((scheme, rest)) = text.split_once(':') else {
            return Err(UriRefusal::NoScheme);
        };
        let scheme = scheme.to_ascii_lowercase();
        match scheme.as_str() {
            "http" | "https" => {
                let authority = rest.strip_prefix("//").ok_or(UriRefusal::NoHost)?;
                if authority.is_empty() || authority.starts_with(['/', '?', '#']) {
                    return Err(UriRefusal::NoHost);
                }
            }
            "mailto" => {
                if rest.is_empty() || rest.starts_with(['?', '#']) {
                    return Err(UriRefusal::NoAddress);
                }
            }
            _ => return Err(UriRefusal::UnsupportedScheme(reportable_scheme(scheme))),
        }
        Ok(Self(format!("{scheme}:{rest}")))
    }
}

fn reportable_scheme(scheme: String) -> Option<String> {
    let reportable = (1..=REPORTED_SCHEME_LIMIT).contains(&scheme.len())
        && scheme.bytes().all(|byte| byte.is_ascii_alphanumeric());
    reportable.then_some(scheme)
}

impl fmt::Display for UriRefusal {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::TooLong => write!(f, "the link is longer than {URI_LIMIT} bytes"),
            Self::NotPrintableAscii => {
                f.write_str("the link holds spaces, control characters or non-ASCII text")
            }
            Self::NoScheme => f.write_str("the link names no scheme"),
            Self::UnsupportedScheme(Some(scheme)) => write!(
                f,
                "Eclipse opens web and email links only, not {scheme} links"
            ),
            Self::UnsupportedScheme(None) => {
                f.write_str("Eclipse opens web and email links only, not links of this scheme")
            }
            Self::NoHost => f.write_str("the web link names no host"),
            Self::NoAddress => f.write_str("the email link names no address"),
        }
    }
}

impl std::error::Error for UriRefusal {}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum IdleInhibit {
    Hold,
    Release,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum NoticeId {
    Share,
    Link,
    Attestation,
    ServerLocation,
    Client(i32),
}

impl NoticeId {
    fn wire_id(self) -> String {
        match self {
            Self::Share => "eclipse-share".to_owned(),
            Self::Link => "eclipse-link".to_owned(),
            Self::Attestation => "eclipse-attestation".to_owned(),
            Self::ServerLocation => "eclipse-server-location".to_owned(),
            Self::Client(id) => format!("client-{id}"),
        }
    }
}

#[derive(Debug, PartialEq, Eq)]
pub struct Notice {
    pub id: NoticeId,
    pub title: String,
    pub body: String,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum SteamKeyboardMode {
    SingleLine,
    MultipleLines,
}

impl SteamKeyboardMode {
    fn wire_value(self) -> u8 {
        match self {
            Self::SingleLine => 0,
            Self::MultipleLines => 1,
        }
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum SteamKeyboard {
    Open {
        x: i32,
        y: i32,
        width: u32,
        height: u32,
        mode: SteamKeyboardMode,
    },
    Close,
}

impl SteamKeyboard {
    fn uri(self) -> String {
        match self {
            Self::Open {
                x,
                y,
                width,
                height,
                mode,
            } => format!(
                "steam://open/keyboard?XPosition={x}&YPosition={y}&Width={width}&Height={height}\
                 &Mode={}",
                mode.wire_value()
            ),
            Self::Close => "steam://close/keyboard".to_owned(),
        }
    }
}

pub enum PortalRequest {
    OpenUri(ExternalUri),
    SteamKeyboard(SteamKeyboard),
    RegisterGame,
    IdleInhibit(IdleInhibit),
    Notify(Notice),
    Withdraw(NoticeId),
    Flush(SyncSender<()>),
}

#[derive(Debug)]
pub enum PortalError {
    Busy,
    Stopped,
    Spawn(std::io::Error),
    NoSessionBus,
    NonUtf8BusAddress,
    Autolaunch(String),
    Connect {
        address: BusAddress,
        cause: dbus::Error,
    },
    Call {
        interface: &'static str,
        method: &'static str,
        cause: dbus::Error,
    },
    Receive(dbus::Error),
    OwnPidfd(std::io::Error),
    GameModeRejected(i32),
}

impl fmt::Display for PortalError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Busy => write!(
                f,
                "the desktop portal queue is full ({QUEUED_REQUESTS} requests are waiting)"
            ),
            Self::Stopped => f.write_str("the desktop portal worker is not running"),
            Self::Spawn(error) => {
                write!(
                    f,
                    "the desktop portal worker thread could not start: {error}"
                )
            }
            Self::NoSessionBus => f.write_str(
                "no D-Bus session bus: DBUS_SESSION_BUS_ADDRESS is unset and \
                 $XDG_RUNTIME_DIR/bus is not a socket",
            ),
            Self::NonUtf8BusAddress => f.write_str("DBUS_SESSION_BUS_ADDRESS is not UTF-8"),
            Self::Autolaunch(address) => write!(
                f,
                "refusing the session bus address {address}: it would make libdbus start a bus \
                 from the game process"
            ),
            Self::Connect { address, cause } => write!(
                f,
                "cannot connect to the session bus at {address}: {}",
                DbusCause(cause)
            ),
            Self::Call {
                interface,
                method,
                cause,
            } => write!(f, "{interface}.{method} failed: {}", DbusCause(cause)),
            Self::Receive(cause) => {
                write!(f, "cannot read from the session bus: {}", DbusCause(cause))
            }
            Self::OwnPidfd(error) => write!(
                f,
                "cannot open a pidfd for Eclipse's own process to send to {GAME_MODE}: {error}"
            ),
            Self::GameModeRejected(result) => write!(
                f,
                "{GAME_MODE}.{REGISTER_GAME} refused to register Eclipse (result {result})"
            ),
        }
    }
}

impl std::error::Error for PortalError {
    fn source(&self) -> Option<&(dyn std::error::Error + 'static)> {
        match self {
            Self::Spawn(error) | Self::OwnPidfd(error) => Some(error),
            Self::Connect { cause, .. } | Self::Call { cause, .. } | Self::Receive(cause) => {
                Some(cause)
            }
            Self::Busy
            | Self::Stopped
            | Self::NoSessionBus
            | Self::NonUtf8BusAddress
            | Self::Autolaunch(_)
            | Self::GameModeRejected(_) => None,
        }
    }
}

struct DbusCause<'a>(&'a dbus::Error);

impl fmt::Display for DbusCause<'_> {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(
            f,
            "{}: {}",
            self.0.name().unwrap_or("an unnamed D-Bus error"),
            self.0.message().unwrap_or("no message")
        )
    }
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct BusAddress(String);

impl BusAddress {
    pub fn as_str(&self) -> &str {
        &self.0
    }
}

impl fmt::Display for BusAddress {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(&self.0)
    }
}

pub fn session_bus_address(
    env_address: Option<&OsStr>,
    runtime_dir: Option<&Path>,
) -> Result<BusAddress, PortalError> {
    if let Some(address) = env_address.filter(|address| !address.is_empty()) {
        let address = address.to_str().ok_or(PortalError::NonUtf8BusAddress)?;
        if address.contains("autolaunch:") {
            return Err(PortalError::Autolaunch(address.to_owned()));
        }
        return Ok(BusAddress(address.to_owned()));
    }
    let socket = runtime_dir
        .filter(|dir| dir.is_absolute())
        .map(|dir| dir.join("bus"))
        .filter(|socket| {
            std::fs::metadata(socket).is_ok_and(|metadata| metadata.file_type().is_socket())
        })
        .ok_or(PortalError::NoSessionBus)?;
    Ok(BusAddress(format!(
        "unix:path={}",
        escape_address_value(socket.as_os_str())
    )))
}

fn escape_address_value(value: &OsStr) -> String {
    let mut escaped = String::with_capacity(value.len());
    for &byte in value.as_bytes() {
        if byte.is_ascii_alphanumeric() || b"-_/.\\*".contains(&byte) {
            escaped.push(char::from(byte));
        } else {
            write!(escaped, "%{byte:02x}").expect("writing to a String cannot fail");
        }
    }
    escaped
}

pub fn gamemode_preloaded(ld_preload: Option<&OsStr>) -> bool {
    ld_preload.is_some_and(|preload| {
        preload
            .as_bytes()
            .split(|byte| matches!(byte, b':' | b' '))
            .filter_map(|entry| Path::new(OsStr::from_bytes(entry)).file_name())
            .any(|name| name.as_bytes().starts_with(GAMEMODE_AUTO_PRELOAD))
    })
}

pub fn submit(request: PortalRequest) -> Result<(), PortalError> {
    WORKER
        .get_or_init(spawn_session_worker)
        .as_ref()
        .ok_or(PortalError::Stopped)?
        .submit(request)
}

fn spawn_session_worker() -> Option<PortalWorker> {
    let runtime_dir = std::env::var_os("XDG_RUNTIME_DIR");
    let address = session_bus_address(
        std::env::var_os("DBUS_SESSION_BUS_ADDRESS").as_deref(),
        runtime_dir.as_deref().map(Path::new),
    );
    PortalWorker::spawn(address)
        .inspect_err(|error| tracing::warn!(%error, "desktop portals are unavailable"))
        .ok()
}

pub struct PortalWorker {
    requests: SyncSender<PortalRequest>,
}

impl PortalWorker {
    pub fn spawn(address: Result<BusAddress, PortalError>) -> Result<Self, PortalError> {
        let (requests, received) = mpsc::sync_channel(QUEUED_REQUESTS);
        std::thread::Builder::new()
            .name("eclipse-portal".to_owned())
            .spawn(move || serve(address, received))
            .map_err(PortalError::Spawn)?;
        Ok(Self { requests })
    }

    pub fn submit(&self, request: PortalRequest) -> Result<(), PortalError> {
        self.requests
            .try_send(request)
            .map_err(|error| match error {
                TrySendError::Full(_) => PortalError::Busy,
                TrySendError::Disconnected(_) => PortalError::Stopped,
            })
    }
}

fn serve(address: Result<BusAddress, PortalError>, requests: Receiver<PortalRequest>) {
    let mut session = match address.and_then(|address| PortalSession::open(&address)) {
        Ok(session) => Some(session),
        Err(error) => {
            tracing::warn!(%error, "desktop portals are unavailable");
            None
        }
    };
    let mut warned = Vec::new();
    for request in requests {
        let Some(session) = session.as_mut() else {
            answer_without_bus(request);
            continue;
        };
        let every_failure = matches!(request, PortalRequest::OpenUri(_));
        let kind = mem::discriminant(&request);
        let Err(error) = session.handle(request) else {
            continue;
        };
        let first_of_kind = !warned.contains(&kind);
        if first_of_kind {
            warned.push(kind);
        }
        if every_failure || first_of_kind {
            tracing::warn!(%error, "a desktop portal request failed");
        } else {
            tracing::debug!(%error, "a desktop portal request failed");
        }
    }
}

fn answer_without_bus(request: PortalRequest) {
    match request {
        PortalRequest::Flush(done) => acknowledge(done),
        PortalRequest::OpenUri(_)
        | PortalRequest::SteamKeyboard(_)
        | PortalRequest::RegisterGame
        | PortalRequest::IdleInhibit(_)
        | PortalRequest::Notify(_)
        | PortalRequest::Withdraw(_) => {
            tracing::debug!("dropped a desktop portal request: the session bus is unavailable");
        }
    }
}

fn acknowledge(done: SyncSender<()>) {
    if done.send(()).is_err() {
        tracing::debug!("a desktop portal flush stopped waiting before its acknowledgement");
    }
}

pub struct PortalSession {
    connection: Connection,
    inhibit: Option<dbus::Path<'static>>,
    game_registered: bool,
}

impl PortalSession {
    pub fn open(address: &BusAddress) -> Result<Self, PortalError> {
        let connection =
            Connection::new_address(address.as_str()).map_err(|cause| PortalError::Connect {
                address: address.clone(),
                cause,
            })?;
        Ok(Self {
            connection,
            inhibit: None,
            game_registered: false,
        })
    }

    pub fn handle(&mut self, request: PortalRequest) -> Result<(), PortalError> {
        let handled = self.dispatch(request);
        let drained = self.drain();
        handled.and(drained)
    }

    fn dispatch(&mut self, request: PortalRequest) -> Result<(), PortalError> {
        match request {
            PortalRequest::OpenUri(uri) => self.open_uri(&uri.0),
            PortalRequest::SteamKeyboard(keyboard) => self.open_uri(&keyboard.uri()),
            PortalRequest::RegisterGame => self.register_game(),
            PortalRequest::IdleInhibit(IdleInhibit::Hold) => self.hold_idle_inhibit(),
            PortalRequest::IdleInhibit(IdleInhibit::Release) => self.release_idle_inhibit(),
            PortalRequest::Notify(notice) => self.call(
                PORTAL_OBJECT,
                NOTIFICATION,
                "AddNotification",
                (
                    notice.id.wire_id(),
                    Dict::new([
                        ("title", Variant(notice.title.as_str())),
                        ("body", Variant(notice.body.as_str())),
                    ]),
                ),
            ),
            PortalRequest::Withdraw(id) => self.call(
                PORTAL_OBJECT,
                NOTIFICATION,
                "RemoveNotification",
                (id.wire_id(),),
            ),
            PortalRequest::Flush(done) => {
                acknowledge(done);
                Ok(())
            }
        }
    }

    fn open_uri(&self, uri: &str) -> Result<(), PortalError> {
        self.call::<(dbus::Path<'static>,), _>(
            PORTAL_OBJECT,
            OPEN_URI,
            "OpenURI",
            ("", uri, PropMap::new()),
        )
        .map(drop)
    }

    fn register_game(&mut self) -> Result<(), PortalError> {
        if self.game_registered {
            return Ok(());
        }
        self.game_registered = true;
        let own = rustix::process::pidfd_open(
            rustix::process::getpid(),
            rustix::process::PidfdFlags::empty(),
        )
        .map_err(|error| PortalError::OwnPidfd(error.into()))?;
        let (result,): (i32,) = self.call(PORTAL_OBJECT, GAME_MODE, REGISTER_GAME, (&own, &own))?;
        match result {
            GAME_REGISTERED => Ok(()),
            GAME_MODE_UNREACHABLE => {
                tracing::info!("GameMode is not available");
                Ok(())
            }
            rejected => Err(PortalError::GameModeRejected(rejected)),
        }
    }

    fn hold_idle_inhibit(&mut self) -> Result<(), PortalError> {
        if self.inhibit.is_some() {
            return Ok(());
        }
        let (handle,): (dbus::Path<'static>,) = self.call(
            PORTAL_OBJECT,
            INHIBIT,
            "Inhibit",
            (
                "",
                INHIBIT_IDLE,
                Dict::new([("reason", Variant(INHIBIT_REASON))]),
            ),
        )?;
        self.inhibit = Some(handle);
        Ok(())
    }

    fn release_idle_inhibit(&mut self) -> Result<(), PortalError> {
        let Some(handle) = self.inhibit.take() else {
            return Ok(());
        };
        self.call(&handle, REQUEST, "Close", ())
    }

    fn call<R: ReadAll, A: AppendAll>(
        &self,
        object: &str,
        interface: &'static str,
        method: &'static str,
        args: A,
    ) -> Result<R, PortalError> {
        self.connection
            .with_proxy(PORTAL_BUS_NAME, object, CALL_TIMEOUT)
            .method_call(interface, method, args)
            .map_err(|cause| PortalError::Call {
                interface,
                method,
                cause,
            })
    }

    fn drain(&self) -> Result<(), PortalError> {
        while self
            .connection
            .process(Duration::ZERO)
            .map_err(PortalError::Receive)?
        {}
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::os::unix::net::UnixListener;
    use std::path::PathBuf;

    struct Scratch(PathBuf);

    impl Scratch {
        fn new(tag: &str) -> Self {
            let root = std::env::temp_dir().join(format!("ep-{tag}-{}", std::process::id()));
            std::fs::remove_dir_all(&root).ok();
            std::fs::create_dir_all(&root).unwrap();
            Self(root)
        }
    }

    impl Drop for Scratch {
        fn drop(&mut self) {
            std::fs::remove_dir_all(&self.0).ok();
        }
    }

    fn address(env: Option<&str>, runtime_dir: Option<&Path>) -> Result<String, PortalError> {
        session_bus_address(env.map(OsStr::new), runtime_dir).map(|address| address.0)
    }

    #[test]
    fn the_environment_address_wins_over_the_runtime_socket() {
        let runtime = Scratch::new("env");
        let _socket = UnixListener::bind(runtime.0.join("bus")).unwrap();
        assert_eq!(
            address(Some("unix:path=/elsewhere/bus"), Some(&runtime.0)).unwrap(),
            "unix:path=/elsewhere/bus"
        );
    }

    #[test]
    fn the_runtime_socket_is_used_when_the_environment_names_no_bus() {
        let runtime = Scratch::new("a b");
        let _socket = UnixListener::bind(runtime.0.join("bus")).unwrap();
        let tail = format!("/ep-a%20b-{}/bus", std::process::id());
        for env in [None, Some("")] {
            let found = address(env, Some(&runtime.0)).unwrap();
            assert!(
                found.starts_with("unix:path=/") && found.ends_with(&tail),
                "{found}"
            );
        }
    }

    #[test]
    fn autolaunch_addresses_are_refused() {
        for env in ["autolaunch:", "unix:path=/nowhere/bus;autolaunch:"] {
            assert!(
                matches!(address(Some(env), None), Err(PortalError::Autolaunch(refused)) if refused == env),
                "{env}"
            );
        }
    }

    #[test]
    fn without_an_address_or_a_runtime_socket_there_is_no_session_bus() {
        let runtime = Scratch::new("none");
        let file_runtime = Scratch::new("file");
        std::fs::write(file_runtime.0.join("bus"), b"").unwrap();
        let relative_runtime = Scratch::new("rel");
        let _socket = UnixListener::bind(relative_runtime.0.join("bus")).unwrap();
        let depth = std::env::current_dir().unwrap().components().count() - 1;
        let relative =
            PathBuf::from("../".repeat(depth)).join(relative_runtime.0.strip_prefix("/").unwrap());
        assert!(relative.is_relative() && relative.join("bus").exists());
        for runtime_dir in [
            None,
            Some(runtime.0.as_path()),
            Some(file_runtime.0.as_path()),
            Some(relative.as_path()),
        ] {
            assert!(
                matches!(address(None, runtime_dir), Err(PortalError::NoSessionBus)),
                "{runtime_dir:?}"
            );
        }
    }

    #[test]
    fn a_non_utf8_environment_address_is_refused() {
        assert!(matches!(
            session_bus_address(Some(OsStr::from_bytes(b"unix:path=/\xff")), None),
            Err(PortalError::NonUtf8BusAddress)
        ));
    }

    #[test]
    fn address_values_escape_every_byte_outside_the_optionally_escaped_set() {
        assert_eq!(
            escape_address_value(OsStr::from_bytes(b"/run/a b,c=d;\xff-_.\\*Z9")),
            "/run/a%20b%2cc%3dd%3b%ff-_.\\*Z9"
        );
    }

    #[test]
    fn gamemoderun_is_seen_in_any_preload_entry() {
        for preload in [
            "/usr/lib/libgamemodeauto.so.0",
            "libeclipse_client_settings_path.so:/usr/lib/libgamemodeauto.so.0",
            "/usr/lib/libmangohud.so /usr/lib32/libgamemodeauto.so.0",
            "libgamemodeauto.so",
        ] {
            assert!(gamemode_preloaded(Some(OsStr::new(preload))), "{preload}");
        }
    }

    #[test]
    fn without_gamemoderun_eclipse_registers_by_itself() {
        assert!(!gamemode_preloaded(None));
        for preload in [
            "",
            "/usr/lib/libgamemode.so.0",
            "libeclipse_client_settings_path.so",
            "/opt/libgamemodeauto/libother.so",
            ": :",
        ] {
            assert!(
                !gamemode_preloaded(Some(OsStr::new(preload))),
                "{preload:?}"
            );
        }
    }

    #[test]
    fn web_and_email_links_are_accepted() {
        for (text, forwarded) in [
            (
                "https://www.roblox.com/upgrades/robux",
                "https://www.roblox.com/upgrades/robux",
            ),
            ("HTTPS://example.org", "https://example.org"),
            ("http://a.b/c", "http://a.b/c"),
            ("mailto:x@y.z", "mailto:x@y.z"),
        ] {
            assert_eq!(ExternalUri::parse(text).unwrap().0, forwarded);
        }
    }

    #[test]
    fn other_links_are_refused_without_repeating_them() {
        let prefix = "https://example.org/";
        let long = format!("{prefix}{}", "a".repeat(URI_LIMIT + 1 - prefix.len()));
        for (text, refusal) in [
            ("", UriRefusal::NoScheme),
            ("https://", UriRefusal::NoHost),
            ("https:///p", UriRefusal::NoHost),
            ("https:example.org", UriRefusal::NoHost),
            ("mailto:", UriRefusal::NoAddress),
            ("mailto:?subject=x", UriRefusal::NoAddress),
            (
                "roblox://placeId=1",
                UriRefusal::UnsupportedScheme(Some("roblox".to_owned())),
            ),
            (
                "robloxmobile://x",
                UriRefusal::UnsupportedScheme(Some("robloxmobile".to_owned())),
            ),
            (
                "intent://x#Intent;end",
                UriRefusal::UnsupportedScheme(Some("intent".to_owned())),
            ),
            (
                "file:///etc/passwd",
                UriRefusal::UnsupportedScheme(Some("file".to_owned())),
            ),
            (
                "javascript:alert(1)",
                UriRefusal::UnsupportedScheme(Some("javascript".to_owned())),
            ),
            (
                "steam://open/keyboard",
                UriRefusal::UnsupportedScheme(Some("steam".to_owned())),
            ),
            (
                "content://x",
                UriRefusal::UnsupportedScheme(Some("content".to_owned())),
            ),
            (
                "view-source:https://example.org",
                UriRefusal::UnsupportedScheme(None),
            ),
            (
                "averyveryverylongscheme:x",
                UriRefusal::UnsupportedScheme(None),
            ),
            (" https://example.org", UriRefusal::NotPrintableAscii),
            ("https://example.org/\n", UriRefusal::NotPrintableAscii),
            ("https://example.org/\0", UriRefusal::NotPrintableAscii),
            ("https://exämple.org", UriRefusal::NotPrintableAscii),
            (long.as_str(), UriRefusal::TooLong),
        ] {
            let refused = ExternalUri::parse(text).err();
            assert_eq!(refused.as_ref(), Some(&refusal), "{text:?}");
            if !text.is_empty() {
                assert!(!refusal.to_string().contains(text), "{text:?}");
            }
        }
    }

    #[test]
    fn a_link_of_exactly_the_limit_is_accepted() {
        let prefix = "https://example.org/";
        let text = format!("{prefix}{}", "a".repeat(URI_LIMIT - prefix.len()));
        assert!(ExternalUri::parse(&text).is_ok());
        assert!(matches!(
            ExternalUri::parse(&format!("{text}a")),
            Err(UriRefusal::TooLong)
        ));
    }

    #[test]
    fn steam_keyboard_requests_name_the_text_box_and_its_mode() {
        for (keyboard, uri) in [
            (
                SteamKeyboard::Open {
                    x: 12,
                    y: 340,
                    width: 600,
                    height: 48,
                    mode: SteamKeyboardMode::SingleLine,
                },
                "steam://open/keyboard?XPosition=12&YPosition=340&Width=600&Height=48&Mode=0",
            ),
            (
                SteamKeyboard::Open {
                    x: -4,
                    y: 0,
                    width: 1280,
                    height: 300,
                    mode: SteamKeyboardMode::MultipleLines,
                },
                "steam://open/keyboard?XPosition=-4&YPosition=0&Width=1280&Height=300&Mode=1",
            ),
            (SteamKeyboard::Close, "steam://close/keyboard"),
        ] {
            assert_eq!(keyboard.uri(), uri);
        }
    }

    #[test]
    fn notice_ids_map_to_stable_wire_ids() {
        assert_eq!(NoticeId::Share.wire_id(), "eclipse-share");
        assert_eq!(NoticeId::Link.wire_id(), "eclipse-link");
        assert_eq!(NoticeId::Attestation.wire_id(), "eclipse-attestation");
        assert_eq!(
            NoticeId::ServerLocation.wire_id(),
            "eclipse-server-location"
        );
        assert_eq!(NoticeId::Client(7).wire_id(), "client-7");
        assert_eq!(NoticeId::Client(-2).wire_id(), "client--2");
    }
}
