use std::collections::{BTreeMap, BTreeSet};
use std::io::Write as _;
use std::os::fd::AsFd as _;
use std::os::unix::net::UnixStream;
use std::path::{Path, PathBuf};
use std::process::Child;
use std::sync::atomic::{AtomicBool, AtomicI64, AtomicU32, AtomicUsize, Ordering};
use std::sync::{mpsc, Mutex, MutexGuard, OnceLock};
use std::thread::JoinHandle;
use std::time::{Duration, Instant, SystemTime};

use super::cef_profile;
use super::proto::{
    self, ClearScope, ConsumerMsg, CookiePair, HelperMsg, LoadEvent, StoredCookie, PROTO_VERSION,
};
use crate::framework::view_registry;

pub const HELPER_NOT_FOUND_MARKER: &str = "helper binary not found";

const HANDSHAKE_TIMEOUT: Duration = Duration::from_secs(10);

const MIGRATION_TIMEOUT: Duration = Duration::from_secs(5);

const SPAWN_RESULT_TIMEOUT: Duration = HANDSHAKE_TIMEOUT
    .saturating_add(MIGRATION_TIMEOUT)
    .saturating_add(Duration::from_secs(5));

const HELPER_EXIT_LIMIT: u32 = 3;

const MISSING_LIBRARY_EXIT: i32 = 127;

const DATA_DIR_ENV: &str = "ECLIPSE_WEBVIEW_DATA_DIR";

const CACHE_DIR_ENV: &str = "ECLIPSE_WEBVIEW_CACHE_DIR";

const CEF_PROFILE_DIR: &str = "webview-cef";

const ACTIVATION_TOKEN_ENV: [&str; 2] = ["XDG_ACTIVATION_TOKEN", "DESKTOP_STARTUP_ID"];

const CLIENT_SETTINGS_SHIM_FILE_NAME: &str = "libeclipse_client_settings_path.so";

const BRIDGE_RESULT_OVER_CAP: &str = "\"eclipse: bridge result exceeds the frame cap\"";

#[derive(Debug, Clone)]
pub enum ClientError {
    HelperNotFound { probed: Vec<PathBuf> },

    ExplicitPathMissing { source: &'static str, path: PathBuf },

    Spawn(String),

    Storage(String),

    Handshake(String),

    VersionMismatch { helper_version: u16 },

    Migration(String),

    Encode(proto::ProtoError),

    Unavailable(String),

    TimedOut(&'static str),

    Internal(&'static str),
}

impl std::fmt::Display for ClientError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::HelperNotFound { probed } => {
                write!(f, "{HELPER_NOT_FOUND_MARKER}: probed ")?;
                for (i, p) in probed.iter().enumerate() {
                    if i > 0 {
                        f.write_str(", ")?;
                    }
                    write!(f, "{}", p.display())?;
                }
                write!(
                    f,
                    " — set config `webview_helper_path` or ECLIPSE_WEBVIEW_HELPER, or build \
                     crates/eclipse-webview (cargo build --release against GTK 4 and WebKitGTK 6.0)"
                )
            }
            Self::ExplicitPathMissing { source, path } => {
                write!(f, "{source} points at a missing file: {}", path.display())
            }
            Self::Spawn(e) => write!(f, "helper spawn failed: {e}"),
            Self::Storage(e) => write!(f, "persistent webview storage unavailable: {e}"),
            Self::Handshake(e) => write!(f, "helper handshake failed: {e}"),
            Self::VersionMismatch { helper_version } => write!(
                f,
                "helper protocol version mismatch: helper v{helper_version}, consumer \
                 v{PROTO_VERSION}"
            ),
            Self::Migration(e) => write!(f, "the CEF cookie migration failed: {e}"),
            Self::Encode(e) => write!(f, "message rejected before send: {e}"),
            Self::Unavailable(reason) => write!(f, "web engine helper unavailable: {reason}"),
            Self::TimedOut(what) => {
                write!(f, "the web engine helper did not answer {what} in time")
            }
            Self::Internal(what) => write!(f, "webview client internal error: {what}"),
        }
    }
}

impl std::error::Error for ClientError {}

enum ClientSlot {
    Unspawned,

    Live(Client),

    Restarting,

    Failed(String),
}

impl ClientSlot {
    fn live(&self) -> Result<&Client, ClientError> {
        let reason = match self {
            Self::Live(client) => return Ok(client),
            Self::Unspawned => "the helper is not running".to_string(),
            Self::Restarting => {
                "the helper exited and restarts once its open views are closed".to_string()
            }
            Self::Failed(reason) => reason.clone(),
        };
        Err(ClientError::Unavailable(reason))
    }
}

struct Client {
    child: Child,

    writer: UnixStream,

    io: JoinHandle<()>,

    upcall: JoinHandle<()>,
}

static CLIENT: Mutex<ClientSlot> = Mutex::new(ClientSlot::Unspawned);

static UNEXPECTED_EXITS: AtomicU32 = AtomicU32::new(0);

static NEXT_REQUEST_ID: AtomicU32 = AtomicU32::new(1);

pub fn next_request_id() -> u32 {
    loop {
        let id = NEXT_REQUEST_ID.fetch_add(1, Ordering::Relaxed);
        if id != 0 {
            return id;
        }
    }
}

#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct LoadObserved {
    pub started: bool,
    pub finished: bool,
    pub load_upcalls: u32,
}

#[derive(Default)]
struct ViewEntry {
    created: bool,

    user_agent: Option<String>,

    bridges: BTreeMap<String, Vec<String>>,

    shown: bool,

    url: Option<String>,

    can_go_back: bool,

    back_closes: bool,

    observed: LoadObserved,
}

impl ViewEntry {
    fn window_visible(&self) -> bool {
        self.created && self.shown
    }

    fn back_navigates(&self) -> bool {
        self.can_go_back && !self.back_closes
    }

    fn close_on_back(&mut self) {
        self.back_closes = true;
    }

    fn creation(&mut self, view: i64) -> Vec<ConsumerMsg> {
        if self.created {
            return Vec::new();
        }
        self.created = true;
        let mut batch = vec![ConsumerMsg::CreateView { view }];
        if let Some(user_agent) = &self.user_agent {
            batch.push(ConsumerMsg::SetUserAgent {
                view,
                user_agent: user_agent.clone(),
            });
        }
        batch.extend(
            self.bridges
                .iter()
                .map(|(name, methods)| ConsumerMsg::BridgeRegister {
                    view,
                    name: name.clone(),
                    methods: methods.clone(),
                }),
        );
        batch
    }

    fn load_batch(&mut self, view: i64, shown: bool, load: ConsumerMsg) -> Vec<ConsumerMsg> {
        let was_visible = self.window_visible();
        let mut batch = self.creation(view);
        self.shown = shown;
        batch.extend(visibility_change(view, was_visible, self.window_visible()));
        self.observed = LoadObserved::default();
        self.back_closes = false;
        batch.push(load);
        batch
    }

    fn show(&mut self, view: i64, shown: bool) -> Option<ConsumerMsg> {
        let was_visible = self.window_visible();
        self.shown = shown;
        if !self.window_visible() {
            self.back_closes = false;
        }
        visibility_change(view, was_visible, self.window_visible())
    }
}

fn visibility_change(view: i64, was_visible: bool, visible: bool) -> Option<ConsumerMsg> {
    (was_visible != visible).then_some(ConsumerMsg::SetVisible { view, visible })
}

struct Views {
    entries: BTreeMap<i64, ViewEntry>,

    closing: BTreeSet<i64>,
}

impl Views {
    const fn new() -> Self {
        Self {
            entries: BTreeMap::new(),
            closing: BTreeSet::new(),
        }
    }

    fn publish(&self) {
        TRACKED_VIEWS.store(self.entries.len(), Ordering::Release);
        VISIBLE_VIEWS.store(
            self.entries
                .values()
                .filter(|entry| entry.window_visible())
                .count(),
            Ordering::Release,
        );
    }

    fn created(&mut self, view: i64) -> Option<&mut ViewEntry> {
        self.entries.get_mut(&view).filter(|entry| entry.created)
    }

    fn activation_target(&self, last: i64) -> Option<i64> {
        let visible = |view: &i64| {
            self.entries
                .get(view)
                .is_some_and(ViewEntry::window_visible)
        };
        Some(last).filter(visible).or_else(|| {
            self.entries
                .iter()
                .rev()
                .find(|(_, entry)| entry.window_visible())
                .map(|(view, _)| *view)
        })
    }

    fn reset_after_helper_loss(&mut self) -> usize {
        let visible = self
            .entries
            .values()
            .filter(|entry| entry.window_visible())
            .count();
        for entry in self.entries.values_mut() {
            entry.created = false;
            entry.url = None;
            entry.can_go_back = false;
        }
        self.closing.clear();
        visible
    }
}

static VIEWS: Mutex<Views> = Mutex::new(Views::new());

static TRACKED_VIEWS: AtomicUsize = AtomicUsize::new(0);

static VISIBLE_VIEWS: AtomicUsize = AtomicUsize::new(0);

static ACTIVATION_TARGET: AtomicI64 = AtomicI64::new(0);

static ACTIVATION_WANTED: AtomicBool = AtomicBool::new(false);

static COOKIE_GETS: Mutex<BTreeMap<u32, mpsc::Sender<Vec<CookiePair>>>> =
    Mutex::new(BTreeMap::new());

static COOKIE_FLUSHES: Mutex<BTreeMap<u32, mpsc::Sender<bool>>> = Mutex::new(BTreeMap::new());

fn lock_client() -> Result<MutexGuard<'static, ClientSlot>, ClientError> {
    CLIENT
        .lock()
        .map_err(|_| ClientError::Internal("client lock poisoned"))
}

fn lock_views() -> Result<MutexGuard<'static, Views>, ClientError> {
    VIEWS
        .lock()
        .map_err(|_| ClientError::Internal("views lock poisoned"))
}

fn encode(msg: &ConsumerMsg) -> Result<Vec<u8>, ClientError> {
    msg.encode().map_err(ClientError::Encode)
}

fn encode_all(batch: &[ConsumerMsg]) -> Result<Vec<Vec<u8>>, ClientError> {
    batch.iter().map(encode).collect()
}

fn write_frames(slot: &ClientSlot, frames: &[Vec<u8>]) -> Result<(), ClientError> {
    let client = slot.live()?;
    for frame in frames {
        (&mut &client.writer).write_all(frame).map_err(|e| {
            ClientError::Unavailable(format!("control-socket write failed: {}", e.kind()))
        })?;
    }
    Ok(())
}

fn note_visibility(batch: &[ConsumerMsg]) {
    for msg in batch {
        if let ConsumerMsg::SetVisible {
            view,
            visible: true,
        } = msg
        {
            ACTIVATION_TARGET.store(*view, Ordering::Release);
            ACTIVATION_WANTED.store(true, Ordering::Release);
        }
    }
}

fn resolve_helper_from(
    config_path: Option<&Path>,
    env_override: Option<&std::ffi::OsStr>,
    exe: Option<&Path>,
) -> Result<PathBuf, ClientError> {
    let mut probed: Vec<PathBuf> = Vec::new();
    if let Some(p) = config_path {
        if p.is_file() {
            return Ok(p.to_owned());
        }
        return Err(ClientError::ExplicitPathMissing {
            source: "config `webview_helper_path`",
            path: p.to_owned(),
        });
    }
    if let Some(e) = env_override {
        let p = PathBuf::from(e);
        if p.is_file() {
            return Ok(p);
        }
        return Err(ClientError::ExplicitPathMissing {
            source: "ECLIPSE_WEBVIEW_HELPER",
            path: p,
        });
    }
    if let Some(dir) = exe.and_then(Path::parent) {
        let sibling = dir.join("eclipse-webview");
        if sibling.is_file() {
            return Ok(sibling);
        }
        probed.push(sibling);
        for profile in ["release", "debug"] {
            let dev = dir
                .join("../..")
                .join("crates/eclipse-webview/target")
                .join(profile)
                .join("eclipse-webview");
            if dev.is_file() {
                return Ok(dev);
            }
            probed.push(dev);
        }
    }
    Err(ClientError::HelperNotFound { probed })
}

static CONFIG_HELPER_PATH: OnceLock<Option<PathBuf>> = OnceLock::new();

pub fn use_helper_path(path: Option<PathBuf>) -> Result<(), ClientError> {
    CONFIG_HELPER_PATH
        .set(path)
        .map_err(|_| ClientError::Internal("the helper path is chosen once per process"))
}

fn resolve_helper() -> Result<PathBuf, ClientError> {
    let config_path = CONFIG_HELPER_PATH.get().and_then(Option::as_deref);
    let env_override = std::env::var_os("ECLIPSE_WEBVIEW_HELPER");
    let exe = std::env::current_exe().ok();
    resolve_helper_from(config_path, env_override.as_deref(), exe.as_deref())
}

fn helper_ld_preload(inherited: &std::ffi::OsStr) -> Option<std::ffi::OsString> {
    use std::os::unix::ffi::{OsStrExt as _, OsStringExt as _};

    let kept: Vec<&[u8]> = inherited
        .as_bytes()
        .split(|byte| matches!(byte, b':' | b' '))
        .filter(|entry| !entry.is_empty())
        .filter(|entry| {
            Path::new(std::ffi::OsStr::from_bytes(entry)).file_name()
                != Some(std::ffi::OsStr::new(CLIENT_SETTINGS_SHIM_FILE_NAME))
        })
        .collect();
    (!kept.is_empty()).then(|| std::ffi::OsString::from_vec(kept.join(&b':')))
}

fn prepare_private_dir(requested: &Path) -> Result<PathBuf, ClientError> {
    use std::os::unix::fs::PermissionsExt as _;

    std::fs::create_dir_all(requested)
        .map_err(|e| ClientError::Storage(format!("cannot create {}: {e}", requested.display())))?;
    let dir = requested.canonicalize().map_err(|e| {
        ClientError::Storage(format!("cannot canonicalize {}: {e}", requested.display()))
    })?;
    std::fs::set_permissions(&dir, std::fs::Permissions::from_mode(0o700)).map_err(|e| {
        ClientError::Storage(format!(
            "cannot restrict {} to owner-only mode 0700: {e}",
            dir.display()
        ))
    })?;
    Ok(dir)
}

struct Storage {
    data: PathBuf,
    cache: PathBuf,
    cef_profile: PathBuf,
}

fn webview_storage() -> Result<Storage, ClientError> {
    let app_data = crate::framework::app_data_dir().ok_or_else(|| {
        ClientError::Storage(
            "no XDG/home app-data directory is available; set ECLIPSE_APP_DATA_DIR to an \
             absolute writable directory"
                .to_string(),
        )
    })?;
    let dirs = directories::ProjectDirs::from("", "", "eclipse").ok_or_else(|| {
        ClientError::Storage(
            "no XDG/home cache directory is available; set XDG_CACHE_HOME or HOME".to_string(),
        )
    })?;
    Ok(Storage {
        data: prepare_private_dir(&app_data.join("webview"))?,
        cache: prepare_private_dir(&dirs.cache_dir().join("webview"))?,
        cef_profile: app_data.join(CEF_PROFILE_DIR),
    })
}

fn spawn_helper_process(storage: &Storage) -> Result<(UnixStream, Child), ClientError> {
    let helper = resolve_helper()?;
    let mut cmd = std::process::Command::new(&helper);
    cmd.env(DATA_DIR_ENV, &storage.data)
        .env(CACHE_DIR_ENV, &storage.cache);
    for name in ACTIVATION_TOKEN_ENV {
        cmd.env_remove(name);
    }
    if let Some(inherited) = std::env::var_os("LD_PRELOAD") {
        match helper_ld_preload(&inherited) {
            Some(preload) => cmd.env("LD_PRELOAD", preload),
            None => cmd.env_remove("LD_PRELOAD"),
        };
    }
    let spawned = spawn_with_control_socket(cmd)
        .map_err(|e| ClientError::Spawn(format!("spawn {} failed: {e}", helper.display())))?;
    tracing::info!(
        helper = %helper.display(),
        "eclipse-webview helper spawned (fd-3 socketpair, exits when the socket closes, no URL \
         in argv)"
    );
    Ok(spawned)
}

fn spawn_with_control_socket(
    mut cmd: std::process::Command,
) -> std::io::Result<(UnixStream, Child)> {
    use std::os::unix::process::CommandExt as _;

    let (parent_end, child_end) = UnixStream::pair()?;
    let child_fd = child_end.as_fd().try_clone_to_owned()?;
    cmd.arg("--ipc-fd=3");
    unsafe {
        use std::os::fd::AsRawFd as _;
        let raw = child_fd.as_raw_fd();
        cmd.pre_exec(move || {
            if raw == 3 {
                if libc::fcntl(3, libc::F_SETFD, 0) != 0 {
                    return Err(std::io::Error::last_os_error());
                }
            } else if libc::dup2(raw, 3) != 3 {
                return Err(std::io::Error::last_os_error());
            }
            Ok(())
        });
    }
    let child = cmd.spawn()?;
    Ok((parent_end, child))
}

fn perform_handshake(stream: &UnixStream, timeout: Duration) -> Result<String, ClientError> {
    let hello = encode(&ConsumerMsg::Hello {
        version: PROTO_VERSION,
    })?;
    (&mut &*stream)
        .write_all(&hello)
        .map_err(|e| ClientError::Handshake(format!("Hello write failed: {}", e.kind())))?;
    stream
        .set_read_timeout(Some(timeout))
        .map_err(|e| ClientError::Handshake(format!("set_read_timeout failed: {}", e.kind())))?;
    match proto::read_helper_msg(&mut &*stream) {
        Ok(HelperMsg::HelloAck { version, engine }) => {
            if !proto::hello_ack_version_supported(version) {
                return Err(ClientError::VersionMismatch {
                    helper_version: version,
                });
            }
            stream.set_read_timeout(None).map_err(|e| {
                ClientError::Handshake(format!("clearing the read timeout failed: {}", e.kind()))
            })?;
            Ok(engine)
        }
        Ok(other) => Err(ClientError::Handshake(format!(
            "expected HelloAck, got {}",
            other.name()
        ))),
        Err(e) => Err(ClientError::Handshake(format!(
            "protocol error before HelloAck: {e}"
        ))),
    }
}

fn with_exit_status(error: ClientError, status: Option<std::process::ExitStatus>) -> ClientError {
    match (error, status.and_then(|s| s.code())) {
        (ClientError::Handshake(reason), Some(MISSING_LIBRARY_EXIT)) => {
            ClientError::Handshake(format!(
                "{reason} (helper exit status {MISSING_LIBRARY_EXIT}: a library it needs is \
                 missing; install GTK 4.10+ and WebKitGTK 6.0 2.42+)"
            ))
        }
        (ClientError::Handshake(reason), Some(code)) => {
            ClientError::Handshake(format!("{reason} (helper exit status {code})"))
        }
        (error, _) => error,
    }
}

fn cef_profile_to_import(storage: &Storage) -> Option<PathBuf> {
    let profile = &storage.cef_profile;
    match profile.try_exists() {
        Ok(true) => {}
        Ok(false) => return None,
        Err(e) => {
            tracing::warn!(
                profile = %profile.display(),
                "cannot look for the CEF WebView profile, so its cookies were not migrated: {e}"
            );
            return None;
        }
    }
    let webkit_store = storage.data.join(proto::PERSISTENT_COOKIE_FILE);
    if !matches!(webkit_store.try_exists(), Ok(false)) {
        tracing::warn!(
            profile = %profile.display(),
            "keeping the CEF WebView profile without migrating it, because the WebKit cookie \
             store already exists and the old cookies could replace newer ones"
        );
        return None;
    }
    Some(profile.clone())
}

enum Import {
    Saved,

    Unsaved(String),
}

#[derive(Debug, Clone, Copy)]
enum MigrationStage {
    Importing { request_id: u32 },

    Flushing { request_id: u32 },
}

#[derive(Debug)]
struct Migration {
    profile: PathBuf,

    total: usize,

    stage: MigrationStage,
}

enum MigrationStep {
    Send(Migration, ConsumerMsg),

    Done(Migration, Import),

    Unrelated(Migration, HelperMsg),
}

impl Migration {
    fn start(profile: &Path, cookies: Vec<StoredCookie>) -> (Self, ConsumerMsg) {
        let request_id = next_request_id();
        let migration = Self {
            profile: profile.to_path_buf(),
            total: cookies.len(),
            stage: MigrationStage::Importing { request_id },
        };
        (
            migration,
            ConsumerMsg::CookieImport {
                request_id,
                cookies,
            },
        )
    }

    fn advance(self, msg: HelperMsg) -> MigrationStep {
        match (self.stage, msg) {
            (
                MigrationStage::Importing { request_id },
                HelperMsg::CookieImportResult {
                    request_id: answered,
                    imported,
                    ..
                },
            ) if answered == request_id => {
                if usize::try_from(imported) != Ok(self.total) {
                    let reason = format!("the helper stored {imported} of {} cookies", self.total);
                    return MigrationStep::Done(self, Import::Unsaved(reason));
                }
                let request_id = next_request_id();
                MigrationStep::Send(
                    Self {
                        stage: MigrationStage::Flushing { request_id },
                        ..self
                    },
                    ConsumerMsg::CookieFlush { request_id },
                )
            }
            (
                MigrationStage::Flushing { request_id },
                HelperMsg::CookieFlushed {
                    request_id: answered,
                    ok,
                },
            ) if answered == request_id => {
                let outcome = if ok {
                    Import::Saved
                } else {
                    Import::Unsaved("the helper could not save the imported cookies".to_string())
                };
                MigrationStep::Done(self, outcome)
            }
            (_, msg) => MigrationStep::Unrelated(self, msg),
        }
    }

    fn conclude(self, outcome: Import) {
        let profile = self.profile.display();
        match outcome {
            Import::Unsaved(reason) => tracing::warn!(
                %profile,
                reason,
                "keeping the CEF WebView profile because the WebKit cookie store did not save its \
                 cookies"
            ),
            Import::Saved => match std::fs::remove_dir_all(&self.profile) {
                Ok(()) => tracing::info!(
                    cookies = self.total,
                    "migrated the CEF WebView cookies into the WebKit cookie store and removed the \
                     CEF profile"
                ),
                Err(e) => tracing::error!(
                    %profile,
                    cookies = self.total,
                    "migrated the CEF WebView cookies but cannot remove the CEF profile: {e}"
                ),
            },
        }
    }
}

fn migrate_cef_profile(
    stream: &UnixStream,
    profile: &Path,
    now: SystemTime,
    timeout: Duration,
) -> Result<Option<Migration>, ClientError> {
    let cookies = match cef_profile::read_cookies(profile, now) {
        Ok(cookies) => cookies,
        Err(e) => {
            tracing::warn!(
                profile = %profile.display(),
                reason = %e,
                "keeping the CEF WebView profile because its cookies cannot be migrated"
            );
            return Ok(None);
        }
    };
    let (migration, import) = Migration::start(profile, cookies);
    let frame = match encode(&import) {
        Ok(frame) => frame,
        Err(e) => {
            migration.conclude(Import::Unsaved(e.to_string()));
            return Ok(None);
        }
    };
    send_migration_frame(stream, &frame)?;
    let pending = await_migration(stream, migration, Instant::now() + timeout);
    stream.set_read_timeout(None).map_err(|e| {
        ClientError::Migration(format!("clearing the read timeout failed: {}", e.kind()))
    })?;
    pending
}

fn await_migration(
    stream: &UnixStream,
    mut migration: Migration,
    deadline: Instant,
) -> Result<Option<Migration>, ClientError> {
    loop {
        let Some(msg) = read_migration_reply(stream, deadline)? else {
            tracing::warn!(
                profile = %migration.profile.display(),
                "the helper has not finished importing the CEF WebView cookies; the migration \
                 completes when it answers"
            );
            return Ok(Some(migration));
        };
        migration = match migration.advance(msg) {
            MigrationStep::Send(next, request) => {
                send_migration_frame(stream, &encode(&request)?)?;
                next
            }
            MigrationStep::Done(done, outcome) => {
                done.conclude(outcome);
                return Ok(None);
            }
            MigrationStep::Unrelated(_, other) => {
                return Err(ClientError::Migration(format!(
                    "expected the cookie migration's reply, got {}",
                    other.name()
                )))
            }
        };
    }
}

fn send_migration_frame(stream: &UnixStream, frame: &[u8]) -> Result<(), ClientError> {
    (&mut &*stream)
        .write_all(frame)
        .map_err(|e| ClientError::Migration(format!("write failed: {}", e.kind())))
}

fn read_migration_reply(
    stream: &UnixStream,
    deadline: Instant,
) -> Result<Option<HelperMsg>, ClientError> {
    let Some(remaining) = deadline
        .checked_duration_since(Instant::now())
        .filter(|remaining| !remaining.is_zero())
    else {
        return Ok(None);
    };
    stream
        .set_read_timeout(Some(remaining))
        .map_err(|e| ClientError::Migration(format!("set_read_timeout failed: {}", e.kind())))?;
    match proto::read_helper_msg(&mut &*stream) {
        Ok(HelperMsg::Fatal { reason }) => Err(ClientError::Unavailable(reason)),
        Ok(msg) => Ok(Some(msg)),
        Err(proto::ProtoError::Io(
            std::io::ErrorKind::WouldBlock | std::io::ErrorKind::TimedOut,
        )) => Ok(None),
        Err(e) => Err(ClientError::Migration(format!("protocol error: {e}"))),
    }
}

struct Spawned {
    child: Child,
    writer: UnixStream,
    upcall: JoinHandle<()>,
}

fn spawn_client(java_vm: jni::vm::JavaVM) -> Result<Client, ClientError> {
    let (tx, rx) = mpsc::channel::<Result<Spawned, ClientError>>();
    let io = std::thread::Builder::new()
        .name("eclipse-webview-io".into())
        .spawn(move || io_thread_main(&tx, java_vm))
        .map_err(|e| ClientError::Spawn(format!("io-thread spawn failed: {e}")))?;
    match rx.recv_timeout(SPAWN_RESULT_TIMEOUT) {
        Ok(Ok(spawned)) => Ok(Client {
            child: spawned.child,
            writer: spawned.writer,
            io,
            upcall: spawned.upcall,
        }),
        Ok(Err(e)) => {
            let _ = io.join();
            Err(e)
        }
        Err(_) => Err(ClientError::Handshake(
            "helper spawn/handshake verdict timed out".into(),
        )),
    }
}

fn io_thread_main(tx: &mpsc::Sender<Result<Spawned, ClientError>>, java_vm: jni::vm::JavaVM) {
    let spawned = webview_storage().and_then(|storage| {
        let cef_profile = cef_profile_to_import(&storage);
        spawn_helper_process(&storage).map(|spawned| (cef_profile, spawned))
    });
    let (cef_profile, (stream, mut child)) = match spawned {
        Ok(spawned) => spawned,
        Err(e) => {
            let _ = tx.send(Err(e));
            return;
        }
    };
    let started = perform_handshake(&stream, HANDSHAKE_TIMEOUT).and_then(|engine| {
        tracing::info!(
            %engine,
            protocol = u64::from(PROTO_VERSION),
            "eclipse-webview helper handshake complete"
        );
        cef_profile.map_or(Ok(None), |profile| {
            migrate_cef_profile(&stream, &profile, SystemTime::now(), MIGRATION_TIMEOUT)
        })
    });
    let migration = match started {
        Ok(migration) => migration,
        Err(e) => {
            let _ = child.kill();
            let status = child.wait().ok();
            let _ = tx.send(Err(with_exit_status(e, status)));
            return;
        }
    };
    let writer = match stream.try_clone() {
        Ok(writer) => writer,
        Err(e) => {
            let _ = child.kill();
            let _ = child.wait();
            let _ = tx.send(Err(ClientError::Spawn(format!(
                "control-socket clone failed: {e}"
            ))));
            return;
        }
    };
    let (up_tx, up_rx) = mpsc::channel::<Upcall>();
    let upcall = match std::thread::Builder::new()
        .name("eclipse-webview-upcall".into())
        .spawn(move || upcall_thread_main(&up_rx, &java_vm))
    {
        Ok(handle) => handle,
        Err(e) => {
            let _ = child.kill();
            let _ = child.wait();
            let _ = tx.send(Err(ClientError::Spawn(format!(
                "upcall-thread spawn failed: {e}"
            ))));
            return;
        }
    };
    if let Err(mpsc::SendError(Ok(mut spawned))) = tx.send(Ok(Spawned {
        child,
        writer,
        upcall,
    })) {
        let _ = spawned.child.kill();
        let _ = spawned.child.wait();
        return;
    }
    reader_loop(&stream, &up_tx, migration);
}

fn ensure_live(slot: &mut ClientSlot, java_vm: jni::vm::JavaVM) -> Result<(), ClientError> {
    match slot {
        ClientSlot::Live(_) => Ok(()),
        ClientSlot::Unspawned => match spawn_client(java_vm) {
            Ok(client) => {
                *slot = ClientSlot::Live(client);
                Ok(())
            }
            Err(e) => {
                *slot = ClientSlot::Failed(e.to_string());
                Err(e)
            }
        },
        ClientSlot::Restarting | ClientSlot::Failed(_) => slot.live().map(|_| ()),
    }
}

pub struct NavigationRequest {
    pub url: String,
    pub method: String,
    pub redirect: bool,
    pub user_gesture: bool,
}

enum Upcall {
    LoadChanged {
        view: i64,
        state: i32,
        url: String,
    },

    Progress {
        view: i64,
        percent: u8,
    },

    LoadFailed {
        view: i64,
        url: String,
        code: i32,
        description: String,
    },

    ResourceLoad {
        view: i64,
        url: String,
    },

    Policy {
        view: i64,
        policy_id: u32,
        request: NavigationRequest,
    },

    Back,

    BridgeCall {
        view: i64,
        call_id: u32,
        payload_json: String,
    },

    EvaluateJsResult {
        request_id: u32,
        ok: bool,
        value_json: String,
    },

    CookieSetResult {
        request_id: u32,
        ok: bool,
    },

    CookiesCleared {
        request_id: u32,
        removed: bool,
    },

    ViewClosed {
        view: i64,
        upto_era: u64,
    },

    HelperGone {
        visible_views: usize,
    },
}

enum Routed {
    Upcall(Upcall),

    Handled,

    Fatal(String),
}

fn route(msg: HelperMsg, views: &mut Views) -> Routed {
    let upcall = match msg {
        HelperMsg::HelloAck { .. } => {
            return Routed::Fatal("protocol violation: HelloAck after the handshake".into())
        }
        HelperMsg::Fatal { reason } => return Routed::Fatal(reason),
        HelperMsg::LoadChanged { view, event, url } => {
            let Some(entry) = views.created(view) else {
                return Routed::Handled;
            };
            match event {
                LoadEvent::Started => entry.observed.started = true,
                LoadEvent::Finished => entry.observed.finished = true,
                LoadEvent::Redirected | LoadEvent::Committed => {}
            }
            if event == LoadEvent::Redirected {
                return Routed::Handled;
            }
            Upcall::LoadChanged {
                view,
                state: event.android_state(),
                url,
            }
        }
        HelperMsg::NavigationState {
            view,
            url,
            title: _,
            can_go_back,
        } => {
            if let Some(entry) = views.created(view) {
                entry.url = Some(url).filter(|url| !url.is_empty());
                entry.can_go_back = can_go_back;
            }
            return Routed::Handled;
        }
        HelperMsg::Progress { view, percent } => {
            if views.created(view).is_none() {
                return Routed::Handled;
            }
            Upcall::Progress { view, percent }
        }
        HelperMsg::LoadFailed {
            view,
            url,
            error,
            description,
        } => {
            if views.created(view).is_none() {
                return Routed::Handled;
            }
            Upcall::LoadFailed {
                view,
                url,
                code: error.android_code(),
                description,
            }
        }
        HelperMsg::ResourceLoad { view, url } => {
            if views.created(view).is_none() {
                return Routed::Handled;
            }
            Upcall::ResourceLoad { view, url }
        }
        HelperMsg::PolicyRequest {
            view,
            policy_id,
            url,
            redirect,
            user_gesture,
            method,
        } => {
            if views.created(view).is_none() {
                return Routed::Handled;
            }
            Upcall::Policy {
                view,
                policy_id,
                request: NavigationRequest {
                    url,
                    method,
                    redirect,
                    user_gesture,
                },
            }
        }
        HelperMsg::CloseRequested { view } => {
            let Some(entry) = views.created(view).filter(|entry| entry.window_visible()) else {
                return Routed::Handled;
            };
            entry.close_on_back();
            Upcall::Back
        }
        HelperMsg::WebProcessGone { view } => {
            let Some(entry) = views.created(view) else {
                return Routed::Handled;
            };
            entry.close_on_back();
            tracing::warn!(
                view,
                "the web process of a WebView ended; closing the view on the Roblox side"
            );
            if !entry.window_visible() {
                return Routed::Handled;
            }
            Upcall::Back
        }
        HelperMsg::ViewClosed { view } => {
            views.closing.remove(&view);
            Upcall::ViewClosed {
                view,
                upto_era: crate::framework::bump_webview_close_era(),
            }
        }
        HelperMsg::BridgeCall {
            view,
            call_id,
            payload_json,
        } => Upcall::BridgeCall {
            view,
            call_id,
            payload_json,
        },
        HelperMsg::EvaluateJsResult {
            request_id,
            ok,
            value_json,
        } => Upcall::EvaluateJsResult {
            request_id,
            ok,
            value_json,
        },
        HelperMsg::CookieSetResult { request_id, ok } => Upcall::CookieSetResult { request_id, ok },
        HelperMsg::CookiesCleared {
            request_id,
            removed,
        } => Upcall::CookiesCleared {
            request_id,
            removed,
        },
        HelperMsg::CookieImportResult {
            request_id,
            imported,
            failed,
        } => {
            tracing::warn!(
                request_id,
                imported,
                failed,
                "webview client: a cookie import result matches no CEF cookie migration"
            );
            return Routed::Handled;
        }
        HelperMsg::CookieList {
            request_id,
            cookies,
        } => {
            deliver(&COOKIE_GETS, request_id, cookies);
            return Routed::Handled;
        }
        HelperMsg::CookieFlushed { request_id, ok } => {
            deliver(&COOKIE_FLUSHES, request_id, ok);
            return Routed::Handled;
        }
    };
    Routed::Upcall(upcall)
}

fn deliver<T>(waiters: &Mutex<BTreeMap<u32, mpsc::Sender<T>>>, request_id: u32, value: T) {
    let waiter = waiters
        .lock()
        .ok()
        .and_then(|mut waiters| waiters.remove(&request_id));
    match waiter {
        Some(tx) => {
            let _ = tx.send(value);
        }
        None => tracing::debug!(
            request_id,
            "webview client: a cookie reply arrived after its caller stopped waiting"
        ),
    }
}

fn wake_all_cookie_waiters() {
    if let Ok(mut waiters) = COOKIE_GETS.lock() {
        waiters.clear();
    }
    if let Ok(mut waiters) = COOKIE_FLUSHES.lock() {
        waiters.clear();
    }
}

fn reader_loop(
    stream: &UnixStream,
    upcalls: &mpsc::Sender<Upcall>,
    mut migration: Option<Migration>,
) {
    loop {
        let msg = match proto::read_helper_msg(&mut &*stream) {
            Ok(msg) => msg,
            Err(proto::ProtoError::Eof) => {
                helper_lost(
                    Loss::Exited("the helper closed its control socket".into()),
                    upcalls,
                );
                return;
            }
            Err(e) => {
                helper_lost(
                    Loss::Exited(format!("protocol error from the helper: {e}")),
                    upcalls,
                );
                return;
            }
        };
        let msg = match migration.take() {
            None => msg,
            Some(pending) => match pending.advance(msg) {
                MigrationStep::Send(next, request) => {
                    send_reply(&request);
                    migration = Some(next);
                    continue;
                }
                MigrationStep::Done(done, outcome) => {
                    done.conclude(outcome);
                    continue;
                }
                MigrationStep::Unrelated(pending, msg) => {
                    migration = Some(pending);
                    msg
                }
            },
        };
        let routed = match VIEWS.lock() {
            Ok(mut views) => {
                let routed = route(msg, &mut views);
                views.publish();
                routed
            }
            Err(_) => Routed::Fatal("views lock poisoned".into()),
        };
        match routed {
            Routed::Upcall(upcall) => {
                let _ = upcalls.send(upcall);
            }
            Routed::Handled => {}
            Routed::Fatal(reason) => {
                helper_lost(Loss::Fatal(reason), upcalls);
                return;
            }
        }
    }
}

enum Loss {
    Fatal(String),

    Exited(String),
}

fn slot_after(loss: Loss) -> ClientSlot {
    match loss {
        Loss::Fatal(reason) => ClientSlot::Failed(reason),
        Loss::Exited(reason) => {
            let exits = UNEXPECTED_EXITS.fetch_add(1, Ordering::AcqRel) + 1;
            if exits >= HELPER_EXIT_LIMIT {
                ClientSlot::Failed(format!(
                    "{reason}; the helper exited unexpectedly {exits} times"
                ))
            } else {
                ClientSlot::Restarting
            }
        }
    }
}

fn helper_lost(loss: Loss, upcalls: &mpsc::Sender<Upcall>) {
    let Ok(mut slot) = CLIENT.lock() else {
        return;
    };
    if !matches!(&*slot, ClientSlot::Live(_)) {
        tracing::debug!("webview reader exiting after teardown");
        return;
    }
    let reason = match &loss {
        Loss::Fatal(reason) | Loss::Exited(reason) => reason.clone(),
    };
    if let ClientSlot::Live(mut client) = std::mem::replace(&mut *slot, slot_after(loss)) {
        let _ = client.child.kill();
        let _ = client.child.wait();
    }
    let visible_views = match VIEWS.lock() {
        Ok(mut views) => {
            let visible = views.reset_after_helper_loss();
            views.publish();
            visible
        }
        Err(_) => 0,
    };
    wake_all_cookie_waiters();
    tracing::warn!(
        reason,
        visible_views,
        restarts = matches!(&*slot, ClientSlot::Restarting),
        "eclipse-webview helper lost; closing its views on the Roblox side"
    );
    let _ = upcalls.send(Upcall::HelperGone { visible_views });
}

fn finish_restart() {
    if let Ok(mut slot) = CLIENT.lock() {
        if matches!(&*slot, ClientSlot::Restarting) {
            *slot = ClientSlot::Unspawned;
        }
    }
}

fn upcall_thread_main(rx: &mpsc::Receiver<Upcall>, java_vm: &jni::vm::JavaVM) {
    let mut gone = None;
    while let Ok(upcall) = rx.recv() {
        match upcall {
            Upcall::HelperGone { visible_views } => gone = Some(visible_views),
            other => run_upcall(java_vm, other),
        }
    }
    crate::framework::drain_all_webview_callbacks(java_vm, "web engine helper connection closed");
    let Some(visible_views) = gone else {
        return;
    };
    for _ in 0..visible_views {
        crate::framework::dispatch_activity_back(java_vm);
    }
    finish_restart();
}

fn run_upcall(java_vm: &jni::vm::JavaVM, upcall: Upcall) {
    match upcall {
        Upcall::LoadChanged { view, state, url } => {
            if crate::framework::fire_web_view_internal_load_changed(java_vm, view, state, &url) {
                if let Ok(mut views) = VIEWS.lock() {
                    if let Some(entry) = views.entries.get_mut(&view) {
                        entry.observed.load_upcalls += 1;
                    }
                }
            }
        }
        Upcall::Progress { view, percent } => {
            crate::framework::fire_web_view_progress_changed(java_vm, view, i32::from(percent));
        }
        Upcall::LoadFailed {
            view,
            url,
            code,
            description,
        } => {
            crate::framework::fire_web_view_received_error(java_vm, view, &url, code, &description)
        }
        Upcall::ResourceLoad { view, url } => {
            crate::framework::fire_web_view_load_resource(java_vm, view, &url);
        }
        Upcall::Policy {
            view,
            policy_id,
            request,
        } => {
            let override_load =
                crate::framework::fire_web_view_should_override_url_loading(java_vm, view, request);
            send_reply(&ConsumerMsg::PolicyReply {
                policy_id,
                override_load,
            });
        }
        Upcall::Back => crate::framework::dispatch_activity_back(java_vm),
        Upcall::BridgeCall {
            view,
            call_id,
            payload_json,
        } => {
            let (ok, result_json) =
                crate::framework::fire_bridge_call(java_vm, view, call_id, &payload_json);
            send_reply(&bridge_result(call_id, ok, result_json));
        }
        Upcall::EvaluateJsResult {
            request_id,
            ok,
            value_json,
        } => crate::framework::fire_evaluate_js_result(java_vm, request_id, ok, &value_json),
        Upcall::CookieSetResult { request_id, ok } => {
            crate::framework::fire_cookie_set_result(java_vm, request_id, ok);
        }
        Upcall::CookiesCleared {
            request_id,
            removed,
        } => crate::framework::fire_cookies_clear_result(java_vm, request_id, removed),
        Upcall::ViewClosed { view, upto_era } => {
            crate::framework::drop_bridges_for_view_closed(view, upto_era);
            crate::framework::drain_eval_callbacks_for_view(java_vm, view, upto_era);
        }
        Upcall::HelperGone { .. } => {}
    }
}

fn send_reply(msg: &ConsumerMsg) {
    let result = encode(msg).and_then(|frame| {
        let slot = lock_client()?;
        match &*slot {
            ClientSlot::Live(_) => write_frames(&slot, &[frame]),
            _ => Ok(()),
        }
    });
    if let Err(e) = result {
        tracing::warn!(error = %e, "webview client: reply to the helper not delivered");
    }
}

fn bridge_result(call_id: u32, ok: bool, result_json: String) -> ConsumerMsg {
    let reply = ConsumerMsg::BridgeResult {
        call_id,
        ok,
        result_json,
    };
    match reply.encode() {
        Err(proto::ProtoError::Oversized { .. }) => {
            tracing::warn!(
                call_id,
                "webview client: the bridge result exceeds the frame cap — failing the page's \
                 call instead of dropping it"
            );
            ConsumerMsg::BridgeResult {
                call_id,
                ok: false,
                result_json: BRIDGE_RESULT_OVER_CAP.to_string(),
            }
        }
        _ => reply,
    }
}

pub struct DataLoad {
    pub base_url: Option<String>,
    pub data: String,
    pub mime: Option<String>,
    pub encoding: Option<String>,
}

pub fn load_url(java_vm: jni::vm::JavaVM, view: i64, url: String) -> Result<(), ClientError> {
    load(java_vm, view, ConsumerMsg::LoadUrl { view, url })
}

pub fn load_data(
    java_vm: jni::vm::JavaVM,
    view: i64,
    load_data: DataLoad,
) -> Result<(), ClientError> {
    load(
        java_vm,
        view,
        ConsumerMsg::LoadData {
            view,
            base_url: load_data.base_url.unwrap_or_default(),
            data: load_data.data,
            mime: load_data.mime.unwrap_or_default(),
            encoding: load_data.encoding.unwrap_or_default(),
        },
    )
}

fn load(java_vm: jni::vm::JavaVM, view: i64, request: ConsumerMsg) -> Result<(), ClientError> {
    encode(&request)?;
    let shown = view_registry::is_shown(view);
    let mut slot = lock_client()?;
    ensure_live(&mut slot, java_vm)?;
    let batch = {
        let mut views = lock_views()?;
        let batch = views
            .entries
            .entry(view)
            .or_default()
            .load_batch(view, shown, request);
        views.publish();
        batch
    };
    note_visibility(&batch);
    write_frames(&slot, &encode_all(&batch)?)
}

fn send_to_created(view: i64, msg: ConsumerMsg) -> Result<(), ClientError> {
    let frame = encode(&msg)?;
    let slot = lock_client()?;
    if lock_views()?.created(view).is_none() {
        return Ok(());
    }
    write_frames(&slot, &[frame])
}

pub fn reload(view: i64) -> Result<(), ClientError> {
    send_to_created(view, ConsumerMsg::Reload { view })
}

pub fn stop_loading(view: i64) -> Result<(), ClientError> {
    send_to_created(view, ConsumerMsg::StopLoading { view })
}

pub fn go_back(view: i64) -> Result<(), ClientError> {
    send_to_created(view, ConsumerMsg::GoBack { view })
}

pub fn evaluate_js(view: i64, request_id: u32, script: String) -> Result<(), ClientError> {
    let frame = encode(&ConsumerMsg::EvaluateJs {
        view,
        request_id,
        script,
    })?;
    let slot = lock_client()?;
    if lock_views()?.created(view).is_none() {
        return Err(ClientError::Unavailable(
            "the WebView has not loaded a page yet".to_string(),
        ));
    }
    write_frames(&slot, &[frame])
}

pub fn set_user_agent(view: i64, user_agent: Option<String>) -> Result<(), ClientError> {
    let user_agent = user_agent.filter(|ua| !ua.is_empty());
    let frame = encode(&ConsumerMsg::SetUserAgent {
        view,
        user_agent: user_agent.clone().unwrap_or_default(),
    })?;
    let slot = lock_client()?;
    let created = {
        let mut views = lock_views()?;
        let entry = views.entries.entry(view).or_default();
        entry.user_agent = user_agent;
        let created = entry.created;
        views.publish();
        created
    };
    if !created {
        return Ok(());
    }
    write_frames(&slot, &[frame])
}

pub fn register_bridge(view: i64, name: String, methods: Vec<String>) -> Result<(), ClientError> {
    let frame = encode(&ConsumerMsg::BridgeRegister {
        view,
        name: name.clone(),
        methods: methods.clone(),
    })?;
    let slot = lock_client()?;
    let created = {
        let mut views = lock_views()?;
        let entry = views.entries.entry(view).or_default();
        entry.bridges.insert(name, methods);
        let created = entry.created;
        views.publish();
        created
    };
    if !created {
        return Ok(());
    }
    write_frames(&slot, &[frame])
}

pub fn unregister_bridge(view: i64, name: String) -> Result<(), ClientError> {
    let slot = lock_client()?;
    let created = {
        let mut views = lock_views()?;
        let Some(entry) = views.entries.get_mut(&view) else {
            return Ok(());
        };
        if entry.bridges.remove(&name).is_none() {
            return Ok(());
        }
        entry.created
    };
    if !created {
        return Ok(());
    }
    write_frames(
        &slot,
        &[encode(&ConsumerMsg::BridgeUnregister { view, name })?],
    )
}

pub fn can_go_back(view: i64) -> bool {
    VIEWS
        .lock()
        .ok()
        .and_then(|mut views| views.created(view).map(|entry| entry.back_navigates()))
        .unwrap_or(false)
}

pub fn url(view: i64) -> Option<String> {
    VIEWS
        .lock()
        .ok()
        .and_then(|mut views| views.created(view).and_then(|entry| entry.url.clone()))
}

pub fn close_view(view: i64) -> Result<(), ClientError> {
    if TRACKED_VIEWS.load(Ordering::Acquire) == 0 || !lock_views()?.entries.contains_key(&view) {
        return Ok(());
    }
    let slot = lock_client()?;
    let created = {
        let mut views = lock_views()?;
        let Some(entry) = views.entries.remove(&view) else {
            return Ok(());
        };
        if entry.created {
            views.closing.insert(view);
        }
        views.publish();
        entry.created
    };
    if !created {
        return Ok(());
    }
    tracing::info!(
        view,
        "webview client: closing the helper's view for a destroyed WebView"
    );
    write_frames(&slot, &[encode(&ConsumerMsg::CloseView { view })?])
}

pub fn refresh_visibility() {
    if TRACKED_VIEWS.load(Ordering::Acquire) == 0 {
        return;
    }
    let created: Vec<i64> = match VIEWS.lock() {
        Ok(views) => views
            .entries
            .iter()
            .filter(|(_, entry)| entry.created)
            .map(|(view, _)| *view)
            .collect(),
        Err(_) => return,
    };
    if created.is_empty() {
        return;
    }
    let shown: Vec<(i64, bool)> = created
        .into_iter()
        .map(|view| (view, view_registry::is_shown(view)))
        .collect();
    if let Err(e) = apply_shown(&shown) {
        tracing::warn!(error = %e, "webview client: a WebView window visibility change was not sent");
    }
}

fn apply_shown(shown: &[(i64, bool)]) -> Result<(), ClientError> {
    let slot = lock_client()?;
    let batch: Vec<ConsumerMsg> = {
        let mut views = lock_views()?;
        let batch = shown
            .iter()
            .filter_map(|&(view, shown)| views.created(view)?.show(view, shown))
            .collect();
        views.publish();
        batch
    };
    if batch.is_empty() {
        return Ok(());
    }
    note_visibility(&batch);
    write_frames(&slot, &encode_all(&batch)?)
}

pub fn view_window_visible() -> bool {
    VISIBLE_VIEWS.load(Ordering::Acquire) != 0
}

pub fn take_activation_request() -> bool {
    ACTIVATION_WANTED.swap(false, Ordering::AcqRel)
}

pub fn request_activation_of_shown_view() {
    let Ok(views) = VIEWS.lock() else {
        return;
    };
    let Some(view) = views.activation_target(ACTIVATION_TARGET.load(Ordering::Acquire)) else {
        return;
    };
    ACTIVATION_TARGET.store(view, Ordering::Release);
    ACTIVATION_WANTED.store(true, Ordering::Release);
}

pub fn activate(token: String) {
    let view = ACTIVATION_TARGET.load(Ordering::Acquire);
    let result = encode(&ConsumerMsg::Activate { view, token }).and_then(|frame| {
        let slot = lock_client()?;
        let visible = lock_views()?
            .created(view)
            .is_some_and(|entry| entry.window_visible());
        if !visible {
            return Ok(());
        }
        write_frames(&slot, &[frame])
    });
    if let Err(e) = result {
        tracing::warn!(error = %e, "webview client: the activation token was not handed over");
    }
}

fn send_spawning(java_vm: jni::vm::JavaVM, msg: &ConsumerMsg) -> Result<(), ClientError> {
    let frame = encode(msg)?;
    let mut slot = lock_client()?;
    ensure_live(&mut slot, java_vm)?;
    write_frames(&slot, &[frame])
}

pub fn cookie_set(
    java_vm: jni::vm::JavaVM,
    request_id: u32,
    url: String,
    header: String,
) -> Result<(), ClientError> {
    send_spawning(
        java_vm,
        &ConsumerMsg::CookieSet {
            request_id,
            url,
            header,
        },
    )
}

pub fn cookies_clear(
    java_vm: jni::vm::JavaVM,
    request_id: u32,
    scope: ClearScope,
) -> Result<(), ClientError> {
    send_spawning(java_vm, &ConsumerMsg::CookiesClear { request_id, scope })
}

fn request_blocking<T>(
    java_vm: jni::vm::JavaVM,
    waiters: &'static Mutex<BTreeMap<u32, mpsc::Sender<T>>>,
    msg: impl FnOnce(u32) -> ConsumerMsg,
    what: &'static str,
    timeout: Duration,
) -> Result<T, ClientError> {
    let request_id = next_request_id();
    let (tx, rx) = mpsc::channel::<T>();
    waiters
        .lock()
        .map_err(|_| ClientError::Internal("cookie waiters lock poisoned"))?
        .insert(request_id, tx);
    let forget = || {
        if let Ok(mut waiters) = waiters.lock() {
            waiters.remove(&request_id);
        }
    };
    if let Err(e) = send_spawning(java_vm, &msg(request_id)) {
        forget();
        return Err(e);
    }
    match rx.recv_timeout(timeout) {
        Ok(value) => Ok(value),
        Err(mpsc::RecvTimeoutError::Timeout) => {
            forget();
            Err(ClientError::TimedOut(what))
        }
        Err(mpsc::RecvTimeoutError::Disconnected) => Err(ClientError::Unavailable(
            "the helper exited before answering".to_string(),
        )),
    }
}

pub fn cookie_get_blocking(
    java_vm: jni::vm::JavaVM,
    url: String,
    timeout: Duration,
) -> Result<Vec<CookiePair>, ClientError> {
    request_blocking(
        java_vm,
        &COOKIE_GETS,
        |request_id| ConsumerMsg::CookieGet { request_id, url },
        "CookieManager.getCookie",
        timeout,
    )
}

pub fn cookie_flush_blocking(
    java_vm: jni::vm::JavaVM,
    timeout: Duration,
) -> Result<bool, ClientError> {
    request_blocking(
        java_vm,
        &COOKIE_FLUSHES,
        |request_id| ConsumerMsg::CookieFlush { request_id },
        "CookieManager.flush",
        timeout,
    )
}

pub fn view_close_pending(view: i64) -> bool {
    VIEWS
        .lock()
        .is_ok_and(|views| views.closing.contains(&view))
}

pub fn load_observed(view: i64) -> Option<LoadObserved> {
    VIEWS
        .lock()
        .ok()
        .and_then(|mut views| views.created(view).map(|entry| entry.observed))
}

pub fn failed_reason() -> Option<String> {
    match &*CLIENT.lock().ok()? {
        ClientSlot::Failed(reason) => Some(reason.clone()),
        _ => None,
    }
}

pub fn needs_cookie_flush_before_shutdown() -> bool {
    CLIENT
        .lock()
        .is_ok_and(|slot| matches!(&*slot, ClientSlot::Live(_)))
}

#[derive(Debug, Clone, Copy)]
pub struct ShutdownReport {
    pub helper_exit: Option<i32>,
    pub reader_joined: bool,
}

pub fn shutdown(vm: &crate::runtime::Vm, deadline: Duration) -> ShutdownReport {
    let taken = match CLIENT.lock() {
        Ok(mut slot) => match std::mem::replace(
            &mut *slot,
            ClientSlot::Failed("the web engine helper was shut down".into()),
        ) {
            ClientSlot::Live(client) => Some(client),
            ClientSlot::Failed(reason) => {
                *slot = ClientSlot::Failed(reason);
                None
            }
            ClientSlot::Unspawned | ClientSlot::Restarting => None,
        },
        Err(_) => None,
    };
    if let Ok(mut views) = VIEWS.lock() {
        views.entries.clear();
        views.closing.clear();
        views.publish();
    }
    let Some(mut client) = taken else {
        return ShutdownReport {
            helper_exit: None,
            reader_joined: false,
        };
    };
    if let Ok(frame) = ConsumerMsg::Shutdown.encode() {
        let _ = (&mut &client.writer).write_all(&frame);
    }
    let t0 = Instant::now();
    let mut exit: Option<i32> = None;
    while t0.elapsed() < deadline {
        match client.child.try_wait() {
            Ok(Some(status)) => {
                exit = status.code();
                break;
            }
            Ok(None) => std::thread::sleep(Duration::from_millis(50)),
            Err(_) => break,
        }
    }
    if exit.is_none() {
        let _ = client.child.kill();
        if let Ok(status) = client.child.wait() {
            exit = status.code();
        }
    }
    let reader_joined = client.io.join().is_ok();
    let t0 = Instant::now();
    while !client.upcall.is_finished() && t0.elapsed() < deadline {
        let _ = crate::framework::pump_main_looper(vm);
        std::thread::sleep(Duration::from_millis(2));
    }
    crate::framework::retire_main_upcall_dispatch(vm);
    let _ = client.upcall.join();
    wake_all_cookie_waiters();
    crate::framework::drop_all_bridges();
    ShutdownReport {
        helper_exit: exit,
        reader_joined,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn temp_dir(tag: &str) -> PathBuf {
        let dir = std::env::temp_dir().join(format!(
            "eclipse-webview-client-test-{}-{tag}",
            std::process::id()
        ));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).expect("create temp dir");
        dir
    }

    fn touch(path: &Path) {
        std::fs::write(path, b"x").expect("touch");
    }

    #[test]
    fn helper_spawn_drops_only_the_client_settings_preload() {
        use std::ffi::{OsStr, OsString};

        assert_eq!(
            helper_ld_preload(OsStr::new(
                "/home/u/.local/share/eclipse/app-data/runtime/libeclipse_client_settings_path.so"
            )),
            None,
            "the Android client-settings shim alone must leave the helper without LD_PRELOAD"
        );
        assert_eq!(
            helper_ld_preload(OsStr::new(
                "/data/runtime/libeclipse_client_settings_path.so:/usr/lib/libgamemodeauto.so.0"
            )),
            Some(OsString::from("/usr/lib/libgamemodeauto.so.0"))
        );
        assert_eq!(
            helper_ld_preload(OsStr::new(
                "libeclipse_client_settings_path.so:/usr/lib/libgamemodeauto.so.0"
            )),
            Some(OsString::from("/usr/lib/libgamemodeauto.so.0")),
            "the shim preloaded by name through LD_LIBRARY_PATH is Eclipse's too"
        );
        assert_eq!(
            helper_ld_preload(OsStr::new(
                "/a/libone.so /data/runtime/libeclipse_client_settings_path.so::/b/libtwo.so"
            )),
            Some(OsString::from("/a/libone.so:/b/libtwo.so"))
        );
        assert_eq!(
            helper_ld_preload(OsStr::new("/opt/libeclipse_client_settings_path.so.bak")),
            Some(OsString::from(
                "/opt/libeclipse_client_settings_path.so.bak"
            )),
            "only the exact shim file name is Eclipse's"
        );
        assert_eq!(helper_ld_preload(OsStr::new("")), None);
    }

    #[test]
    fn webview_client_resolves_helper_in_the_documented_order_with_actionable_errors() {
        let root = temp_dir("resolve");

        let exe_dir = root.join("target/debug");
        std::fs::create_dir_all(&exe_dir).expect("exe dir");
        let exe = exe_dir.join("eclipse");
        touch(&exe);
        let dev_release_dir = root.join("crates/eclipse-webview/target/release");
        let dev_debug_dir = root.join("crates/eclipse-webview/target/debug");
        std::fs::create_dir_all(&dev_release_dir).expect("dev release dir");
        std::fs::create_dir_all(&dev_debug_dir).expect("dev debug dir");

        let config_helper = root.join("config-helper");
        touch(&config_helper);
        let env_helper = root.join("env-helper");
        touch(&env_helper);
        let sibling = exe_dir.join("eclipse-webview");

        let got = resolve_helper_from(
            Some(&config_helper),
            Some(env_helper.as_os_str()),
            Some(&exe),
        )
        .expect("config tier resolves");
        assert_eq!(got, config_helper);

        touch(&sibling);
        let got = resolve_helper_from(None, Some(env_helper.as_os_str()), Some(&exe))
            .expect("env tier resolves");
        assert_eq!(got, env_helper);

        touch(&dev_release_dir.join("eclipse-webview"));
        let got = resolve_helper_from(None, None, Some(&exe)).expect("sibling tier resolves");
        assert_eq!(got, sibling);

        std::fs::remove_file(&sibling).expect("rm sibling");
        let got = resolve_helper_from(None, None, Some(&exe)).expect("dev release resolves");
        assert!(got.ends_with("crates/eclipse-webview/target/release/eclipse-webview"));
        std::fs::remove_file(dev_release_dir.join("eclipse-webview")).expect("rm release");
        touch(&dev_debug_dir.join("eclipse-webview"));
        let got = resolve_helper_from(None, None, Some(&exe)).expect("dev debug resolves");
        assert!(got.ends_with("crates/eclipse-webview/target/debug/eclipse-webview"));

        let missing = root.join("missing-helper");
        let err = resolve_helper_from(Some(&missing), None, Some(&exe))
            .expect_err("missing config path must error");
        match &err {
            ClientError::ExplicitPathMissing { source, path } => {
                assert_eq!(*source, "config `webview_helper_path`");
                assert_eq!(path, &missing);
            }
            other => panic!("expected ExplicitPathMissing, got {other:?}"),
        }
        assert!(err.to_string().contains("missing-helper"));
        let err = resolve_helper_from(None, Some(missing.as_os_str()), Some(&exe))
            .expect_err("missing env path must error");
        assert!(matches!(
            err,
            ClientError::ExplicitPathMissing {
                source: "ECLIPSE_WEBVIEW_HELPER",
                ..
            }
        ));

        std::fs::remove_file(dev_debug_dir.join("eclipse-webview")).expect("rm debug");
        let err =
            resolve_helper_from(None, None, Some(&exe)).expect_err("nothing resolvable must error");
        let text = err.to_string();
        assert!(text.starts_with(HELPER_NOT_FOUND_MARKER));
        assert!(text.contains("target/debug/eclipse-webview"), "{text}");
        assert!(
            text.contains("crates/eclipse-webview/target/release/eclipse-webview"),
            "{text}"
        );
        assert!(text.contains("ECLIPSE_WEBVIEW_HELPER"), "{text}");
        assert!(!text.contains("CEF"), "{text}");
        let _ = std::fs::remove_dir_all(&root);
    }

    #[test]
    fn the_launch_chooses_the_config_helper_path_once() {
        let root = temp_dir("chosen-helper");
        let chosen = root.join("config-helper");
        touch(&chosen);

        use_helper_path(Some(chosen.clone())).expect("the launch chooses the helper path");
        assert_eq!(
            resolve_helper().expect("the chosen helper resolves"),
            chosen
        );
        assert!(matches!(
            use_helper_path(None),
            Err(ClientError::Internal(_))
        ));
        assert_eq!(resolve_helper().expect("the first choice stays"), chosen);
        let _ = std::fs::remove_dir_all(&root);
    }

    #[test]
    fn the_helper_outlives_the_thread_that_started_it() {
        let (control, mut child) = std::thread::spawn(|| {
            let mut cmd = std::process::Command::new("/bin/sh");
            cmd.args([
                "-c",
                "[ \"$1\" = --ipc-fd=3 ] && [ -S /proc/self/fd/3 ] && sleep 0.3 && exit 7",
                "sh",
            ]);
            spawn_with_control_socket(cmd).expect("spawn a stand-in helper")
        })
        .join()
        .expect("spawning thread");
        let status = child.wait().expect("wait for the stand-in helper");
        drop(control);
        assert_eq!(
            status.code(),
            Some(7),
            "the helper must finish on its own after the io thread that started it exits: \
             {status:?}"
        );
    }

    #[test]
    fn webview_storage_dirs_are_absolute_canonical_and_owner_only() {
        use std::os::unix::fs::PermissionsExt as _;

        let root = temp_dir("storage");
        let requested = root.join("app-data/../app-data/webview");
        let prepared = prepare_private_dir(&requested).expect("prepare the storage dir");
        assert!(prepared.is_absolute());
        assert_eq!(prepared, root.join("app-data/webview"));
        let mode = std::fs::metadata(&prepared)
            .expect("storage metadata")
            .permissions()
            .mode()
            & 0o777;
        assert_eq!(mode, 0o700, "the cookie store must be owner-only");
        let _ = std::fs::remove_dir_all(&root);
    }

    #[test]
    fn webview_client_handshake_gates_on_hello_ack_version() {
        let deadline = Duration::from_secs(2);

        let (client_end, helper_end) = UnixStream::pair().expect("pair");
        let ack = HelperMsg::HelloAck {
            version: PROTO_VERSION,
            engine: "webkitgtk/2.54.0".into(),
        }
        .encode()
        .expect("encode ack");
        (&mut &helper_end).write_all(&ack).expect("write ack");
        let engine = perform_handshake(&client_end, deadline).expect("current-version handshake");
        assert_eq!(engine, "webkitgtk/2.54.0");
        assert_eq!(
            proto::read_consumer_msg(&mut &helper_end).expect("decode Hello"),
            ConsumerMsg::Hello {
                version: PROTO_VERSION
            }
        );

        let (client_end, helper_end) = UnixStream::pair().expect("pair");
        let ack = HelperMsg::HelloAck {
            version: PROTO_VERSION + 1,
            engine: "webkitgtk/future".into(),
        }
        .encode()
        .expect("encode future ack");
        (&mut &helper_end).write_all(&ack).expect("write ack");
        match perform_handshake(&client_end, deadline) {
            Err(ClientError::VersionMismatch { helper_version }) => {
                assert_eq!(helper_version, PROTO_VERSION + 1);
            }
            other => panic!("expected VersionMismatch, got {other:?}"),
        }

        let (client_end, helper_end) = UnixStream::pair().expect("pair");
        let mut junk = Vec::new();
        junk.extend_from_slice(&2u32.to_le_bytes());
        junk.push(0x7F);
        junk.push(0xAA);
        (&mut &helper_end).write_all(&junk).expect("write junk");
        match perform_handshake(&client_end, deadline) {
            Err(ClientError::Handshake(reason)) => {
                assert!(reason.contains("0x7F"), "reason: {reason}");
            }
            other => panic!("expected Handshake error, got {other:?}"),
        }
    }

    #[test]
    fn a_handshake_failure_names_the_helper_exit_status() {
        use std::os::unix::process::ExitStatusExt as _;
        let failed = || ClientError::Handshake("protocol error before HelloAck: EOF".into());
        let exit_1 = std::process::ExitStatus::from_raw(1 << 8);
        let killed = std::process::ExitStatus::from_raw(9);
        assert_eq!(
            with_exit_status(failed(), Some(exit_1)).to_string(),
            "helper handshake failed: protocol error before HelloAck: EOF (helper exit status 1)"
        );
        assert_eq!(
            with_exit_status(failed(), Some(killed)).to_string(),
            "helper handshake failed: protocol error before HelloAck: EOF"
        );
        let unloadable = std::process::ExitStatus::from_raw(127 << 8);
        assert_eq!(
            with_exit_status(failed(), Some(unloadable)).to_string(),
            "helper handshake failed: protocol error before HelloAck: EOF (helper exit status \
             127: a library it needs is missing; install GTK 4.10+ and WebKitGTK 6.0 2.42+)"
        );
        assert!(matches!(
            with_exit_status(
                ClientError::VersionMismatch { helper_version: 5 },
                Some(exit_1)
            ),
            ClientError::VersionMismatch { helper_version: 5 }
        ));
    }

    #[test]
    fn an_over_cap_bridge_result_is_answered_with_a_failure_frame() {
        assert_eq!(
            bridge_result(5, true, "x".repeat(8 * 1024 * 1024)),
            ConsumerMsg::BridgeResult {
                call_id: 5,
                ok: false,
                result_json: BRIDGE_RESULT_OVER_CAP.to_string(),
            }
        );
        assert_eq!(
            bridge_result(6, true, "{\"a\":1}".to_string()),
            ConsumerMsg::BridgeResult {
                call_id: 6,
                ok: true,
                result_json: "{\"a\":1}".to_string(),
            }
        );
    }

    #[test]
    fn next_request_id_is_monotonic_and_skips_zero() {
        let a = next_request_id();
        let b = next_request_id();
        assert_ne!(a, 0);
        assert_ne!(b, 0);
        assert_ne!(a, b);
    }

    const ROBLOX_UA: &str = "Mozilla/5.0 (0MB; 960x540; 160x160; 960x540; HTC unknown; unknown) \
                             AppleWebKit/537.36 (KHTML, like Gecko)  ROBLOX Android App 2.724.735 \
                             Phone Hybrid()  GooglePlayStore RobloxApp/2.724.735 (GlobalDist; \
                             GooglePlayStore)";

    fn load(view: i64) -> ConsumerMsg {
        ConsumerMsg::LoadUrl {
            view,
            url: "https://www.roblox.com/login".into(),
        }
    }

    #[test]
    fn the_first_load_creates_the_view_with_its_user_agent_bridges_and_visibility() {
        let view = 42;
        let mut entry = ViewEntry {
            user_agent: Some(ROBLOX_UA.to_string()),
            ..ViewEntry::default()
        };
        entry.bridges.insert(
            "__globalRobloxAndroidBridge__".into(),
            vec!["executeRoblox".into()],
        );
        assert_eq!(
            entry.load_batch(view, true, load(view)),
            vec![
                ConsumerMsg::CreateView { view },
                ConsumerMsg::SetUserAgent {
                    view,
                    user_agent: ROBLOX_UA.to_string(),
                },
                ConsumerMsg::BridgeRegister {
                    view,
                    name: "__globalRobloxAndroidBridge__".into(),
                    methods: vec!["executeRoblox".into()],
                },
                ConsumerMsg::SetVisible {
                    view,
                    visible: true,
                },
                load(view),
            ]
        );
        assert!(entry.window_visible());
        assert_eq!(
            entry.load_batch(view, true, load(view)),
            vec![load(view)],
            "a later load reuses the helper's view and its window"
        );
    }

    #[test]
    fn a_view_without_an_app_user_agent_keeps_the_helper_default_and_starts_hidden() {
        let view = 43;
        let mut entry = ViewEntry::default();
        assert_eq!(
            entry.load_batch(view, false, load(view)),
            vec![ConsumerMsg::CreateView { view }, load(view)]
        );
        assert!(!entry.window_visible());
    }

    #[test]
    fn the_window_follows_the_android_view_and_changes_only_on_transitions() {
        let view = 44;
        let mut entry = ViewEntry::default();
        assert_eq!(
            entry.show(view, true),
            None,
            "an uncreated view has no window"
        );
        entry.load_batch(view, false, load(view));
        assert_eq!(
            entry.show(view, true),
            Some(ConsumerMsg::SetVisible {
                view,
                visible: true
            })
        );
        assert_eq!(entry.show(view, true), None);
        assert_eq!(
            entry.show(view, false),
            Some(ConsumerMsg::SetVisible {
                view,
                visible: false
            })
        );
        assert_eq!(entry.show(view, false), None);
    }

    #[test]
    fn showing_a_window_asks_for_an_activation_token_for_that_view() {
        let _serial = HELPER_STATE_TEST_LOCK
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        while take_activation_request() {}
        note_visibility(&[ConsumerMsg::SetVisible {
            view: 7,
            visible: false,
        }]);
        assert!(!take_activation_request(), "hiding never asks for focus");
        note_visibility(&[ConsumerMsg::SetVisible {
            view: 7,
            visible: true,
        }]);
        assert!(take_activation_request());
        assert_eq!(ACTIVATION_TARGET.load(Ordering::Acquire), 7);
        assert!(!take_activation_request(), "one request per shown window");
    }

    #[test]
    fn a_press_withheld_from_the_game_asks_to_raise_the_shown_web_view_window() {
        let _serial = HELPER_STATE_TEST_LOCK
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        while take_activation_request() {}
        let hidden = registry_view(false);
        let shown = registry_view(true);
        {
            let mut views = VIEWS.lock().expect("views");
            views.entries.insert(hidden, created_entry(false));
            views.entries.insert(shown, created_entry(true));
            views.publish();
        }
        ACTIVATION_TARGET.store(hidden, Ordering::Release);
        request_activation_of_shown_view();
        let raised = (
            take_activation_request(),
            ACTIVATION_TARGET.load(Ordering::Acquire),
        );
        {
            let mut views = VIEWS.lock().expect("views");
            views.entries.remove(&hidden);
            views.entries.remove(&shown);
            views.publish();
        }
        request_activation_of_shown_view();
        let without_window = take_activation_request();
        free_registry_views(&[hidden, shown]);

        assert_eq!(
            raised,
            (true, shown),
            "the hidden view is skipped and the shown one is raised"
        );
        assert!(
            !without_window,
            "with no WebView window shown nothing is raised"
        );
    }

    #[test]
    fn the_last_activated_window_is_raised_again_while_it_shows() {
        let mut views = Views::new();
        views.entries.insert(90, created_entry(true));
        views.entries.insert(91, created_entry(true));
        views.entries.insert(92, created_entry(false));
        assert_eq!(views.activation_target(90), Some(90));
        assert_eq!(views.activation_target(92), Some(91));
        assert_eq!(views.activation_target(93), Some(91));
        assert_eq!(Views::new().activation_target(90), None);
    }

    fn views_with(view: i64, entry: ViewEntry) -> Views {
        let mut views = Views::new();
        views.entries.insert(view, entry);
        views
    }

    fn registry_view(shown: bool) -> i64 {
        let view = view_registry::allocate("android.webkit.WebView").expect("allocate a WebView");
        if shown {
            view_registry::mark_window_root(view).expect("show the WebView");
        }
        assert_eq!(view_registry::is_shown(view), shown);
        view
    }

    fn free_registry_views(views: &[i64]) {
        for &view in views {
            view_registry::free(view).expect("free the WebView");
        }
    }

    fn created_entry(shown: bool) -> ViewEntry {
        ViewEntry {
            created: true,
            shown,
            can_go_back: true,
            ..ViewEntry::default()
        }
    }

    fn upcall(routed: Routed) -> Upcall {
        match routed {
            Routed::Upcall(upcall) => upcall,
            Routed::Handled => panic!("expected an upcall, the message was handled in place"),
            Routed::Fatal(reason) => panic!("expected an upcall, got fatal {reason}"),
        }
    }

    #[test]
    fn load_events_reach_android_with_the_real_url_except_redirects() {
        let view = 50;
        let mut views = views_with(view, created_entry(true));
        let url = "https://www.roblox.com/login?returnUrl=x";
        for (event, state) in [
            (LoadEvent::Started, 0),
            (LoadEvent::Committed, 2),
            (LoadEvent::Finished, 3),
        ] {
            match upcall(route(
                HelperMsg::LoadChanged {
                    view,
                    event,
                    url: url.into(),
                },
                &mut views,
            )) {
                Upcall::LoadChanged {
                    view: got,
                    state: got_state,
                    url: got_url,
                } => {
                    assert_eq!((got, got_state, got_url.as_str()), (view, state, url));
                }
                _ => panic!("expected a LoadChanged upcall for {event:?}"),
            }
        }
        assert!(matches!(
            route(
                HelperMsg::LoadChanged {
                    view,
                    event: LoadEvent::Redirected,
                    url: url.into(),
                },
                &mut views,
            ),
            Routed::Handled
        ));
        let observed = views.entries[&view].observed;
        assert!(observed.started && observed.finished);

        assert!(matches!(
            route(
                HelperMsg::LoadChanged {
                    view: view + 1,
                    event: LoadEvent::Started,
                    url: url.into(),
                },
                &mut views,
            ),
            Routed::Handled
        ));
    }

    #[test]
    fn navigation_state_backs_get_url_and_can_go_back() {
        let view = 51;
        let mut views = views_with(view, created_entry(true));
        route(
            HelperMsg::NavigationState {
                view,
                url: "https://www.roblox.com/home".into(),
                title: "Home".into(),
                can_go_back: false,
            },
            &mut views,
        );
        let entry = &views.entries[&view];
        assert_eq!(entry.url.as_deref(), Some("https://www.roblox.com/home"));
        assert!(!entry.can_go_back);
        route(
            HelperMsg::NavigationState {
                view,
                url: String::new(),
                title: String::new(),
                can_go_back: true,
            },
            &mut views,
        );
        assert_eq!(views.entries[&view].url, None, "an empty URL is no URL");
        assert!(views.entries[&view].can_go_back);
    }

    fn navigation_state(view: i64, can_go_back: bool) -> HelperMsg {
        HelperMsg::NavigationState {
            view,
            url: "https://www.roblox.com/info/terms".into(),
            title: "Terms".into(),
            can_go_back,
        }
    }

    #[test]
    fn closing_the_window_and_a_dead_page_map_to_android_back() {
        let view = 52;
        let mut views = views_with(view, created_entry(true));
        assert!(views.entries[&view].back_navigates());
        assert!(matches!(
            upcall(route(HelperMsg::CloseRequested { view }, &mut views)),
            Upcall::Back
        ));
        assert!(
            !views.entries[&view].back_navigates(),
            "Roblox's Back handler goes back in web history while canGoBack is true, so a \
             window close must make that Back close the view"
        );
        route(navigation_state(view, true), &mut views);
        assert!(
            !views.entries[&view].back_navigates(),
            "a page update before Roblox handles the Back keeps it closing the view"
        );
        let entry = views.entries.get_mut(&view).expect("entry");
        entry.load_batch(view, true, load(view));
        assert!(entry.back_navigates(), "a new load restores web history");

        let dead = 57;
        let mut views = views_with(dead, created_entry(true));
        assert!(matches!(
            upcall(route(HelperMsg::WebProcessGone { view: dead }, &mut views)),
            Upcall::Back
        ));
        assert!(
            !views.entries[&dead].back_navigates(),
            "after its page died, Back closes the view instead of navigating a dead page"
        );
        let entry = views.entries.get_mut(&dead).expect("entry");
        entry.show(dead, false);
        entry.show(dead, true);
        assert!(
            entry.back_navigates(),
            "once the view is hidden, a later showing navigates web history again"
        );

        let hidden = 53;
        let mut views = views_with(hidden, created_entry(false));
        assert!(matches!(
            route(HelperMsg::WebProcessGone { view: hidden }, &mut views),
            Routed::Handled
        ));
        assert!(!views.entries[&hidden].back_navigates());
        assert!(
            matches!(
                route(HelperMsg::CloseRequested { view: hidden }, &mut views),
                Routed::Handled
            ),
            "closing a window the app already hid sends no Back to the game"
        );
    }

    #[test]
    fn a_destroyed_view_gets_no_navigation_callback() {
        let mut views = Views::new();
        assert!(matches!(
            route(
                HelperMsg::PolicyRequest {
                    view: 56,
                    policy_id: 3,
                    url: "roblox://placeId=1".into(),
                    redirect: false,
                    user_gesture: true,
                    method: "GET".into(),
                },
                &mut views,
            ),
            Routed::Handled
        ));
    }

    #[test]
    fn page_callbacks_map_to_android_codes_and_requests() {
        let view = 54;
        let mut views = views_with(view, created_entry(true));
        match upcall(route(
            HelperMsg::LoadFailed {
                view,
                url: "https://nx.invalid/".into(),
                error: proto::LoadError::HostLookup,
                description: "Error resolving".into(),
            },
            &mut views,
        )) {
            Upcall::LoadFailed {
                code, description, ..
            } => {
                assert_eq!(code, -2);
                assert_eq!(description, "Error resolving");
            }
            _ => panic!("expected LoadFailed"),
        }
        match upcall(route(
            HelperMsg::PolicyRequest {
                view,
                policy_id: 9,
                url: "roblox://placeId=1".into(),
                redirect: false,
                user_gesture: true,
                method: "GET".into(),
            },
            &mut views,
        )) {
            Upcall::Policy {
                policy_id, request, ..
            } => {
                assert_eq!(policy_id, 9);
                assert_eq!(request.url, "roblox://placeId=1");
                assert_eq!(request.method, "GET");
                assert!(request.user_gesture && !request.redirect);
            }
            _ => panic!("expected Policy"),
        }
        assert!(matches!(
            upcall(route(HelperMsg::Progress { view, percent: 40 }, &mut views)),
            Upcall::Progress { percent: 40, .. }
        ));
        assert!(matches!(
            upcall(route(
                HelperMsg::ResourceLoad {
                    view,
                    url: "https://css.rbxcdn.com/a.css".into()
                },
                &mut views
            )),
            Upcall::ResourceLoad { .. }
        ));
    }

    #[test]
    fn view_closed_ends_the_close_and_drains_by_era() {
        let view = 55;
        let mut views = Views::new();
        views.closing.insert(view);
        let before = crate::framework::bump_webview_close_era();
        match upcall(route(HelperMsg::ViewClosed { view }, &mut views)) {
            Upcall::ViewClosed {
                view: closed,
                upto_era,
            } => {
                assert_eq!(closed, view);
                assert!(upto_era > before);
            }
            _ => panic!("expected ViewClosed"),
        }
        assert!(views.closing.is_empty());
    }

    #[test]
    fn cookie_replies_wake_their_blocked_callers() {
        let _serial = HELPER_STATE_TEST_LOCK
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        let mut views = Views::new();
        let request_id = next_request_id();
        let (tx, rx) = mpsc::channel();
        COOKIE_GETS.lock().expect("waiters").insert(request_id, tx);
        let cookies = vec![CookiePair {
            name: ".ROBLOSECURITY".into(),
            value: "v".into(),
        }];
        assert!(matches!(
            route(
                HelperMsg::CookieList {
                    request_id,
                    cookies: cookies.clone(),
                },
                &mut views,
            ),
            Routed::Handled
        ));
        assert_eq!(rx.recv_timeout(Duration::from_secs(1)), Ok(cookies));

        let request_id = next_request_id();
        let (tx, rx) = mpsc::channel();
        COOKIE_FLUSHES
            .lock()
            .expect("waiters")
            .insert(request_id, tx);
        route(
            HelperMsg::CookieFlushed {
                request_id,
                ok: true,
            },
            &mut views,
        );
        assert_eq!(rx.recv_timeout(Duration::from_secs(1)), Ok(true));
    }

    #[test]
    fn helper_failures_are_fatal_and_a_second_hello_ack_is_a_protocol_violation() {
        let mut views = Views::new();
        assert!(matches!(
            route(
                HelperMsg::Fatal {
                    reason: "GTK could not open the display".into()
                },
                &mut views
            ),
            Routed::Fatal(reason) if reason == "GTK could not open the display"
        ));
        assert!(matches!(
            route(
                HelperMsg::HelloAck {
                    version: PROTO_VERSION,
                    engine: String::new()
                },
                &mut views
            ),
            Routed::Fatal(_)
        ));
    }

    #[test]
    fn a_lost_helper_hides_every_window_and_keeps_what_recreates_the_views() {
        let mut views = Views::new();
        let mut shown = created_entry(true);
        shown.user_agent = Some(ROBLOX_UA.to_string());
        shown.url = Some("https://www.roblox.com/login".into());
        views.entries.insert(60, shown);
        views.entries.insert(61, created_entry(false));
        views.entries.insert(62, ViewEntry::default());
        views.closing.insert(63);
        assert_eq!(views.reset_after_helper_loss(), 1);
        for entry in views.entries.values() {
            assert!(!entry.created && !entry.window_visible() && !entry.can_go_back);
            assert_eq!(entry.url, None);
        }
        assert_eq!(views.entries[&60].user_agent.as_deref(), Some(ROBLOX_UA));
        assert!(views.closing.is_empty());

        let view = 60;
        let entry = views.entries.get_mut(&view).expect("entry");
        assert_eq!(
            entry.load_batch(view, true, load(view))[..2],
            [
                ConsumerMsg::CreateView { view },
                ConsumerMsg::SetUserAgent {
                    view,
                    user_agent: ROBLOX_UA.to_string()
                }
            ],
            "the next load recreates the view in the restarted helper"
        );
    }

    static HELPER_STATE_TEST_LOCK: Mutex<()> = Mutex::new(());

    #[test]
    fn a_helper_that_exits_hides_its_windows_and_asks_android_to_close_the_shown_ones() {
        let _serial = HELPER_STATE_TEST_LOCK
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        UNEXPECTED_EXITS.store(0, Ordering::SeqCst);
        let (host_end, helper_end) = UnixStream::pair().expect("socketpair");
        let child = std::process::Command::new("/bin/sh")
            .args(["-c", "exit 0"])
            .spawn()
            .expect("spawn a stand-in helper");
        *CLIENT.lock().expect("client") = ClientSlot::Live(Client {
            child,
            writer: host_end.try_clone().expect("writer"),
            io: std::thread::spawn(|| {}),
            upcall: std::thread::spawn(|| {}),
        });
        let shown_view = registry_view(true);
        let hidden_view = registry_view(false);
        {
            let mut views = VIEWS.lock().expect("views");
            views.entries.insert(shown_view, created_entry(true));
            views.entries.insert(hidden_view, created_entry(false));
            views.publish();
        }
        assert!(view_window_visible());

        let (tx, rx) = mpsc::channel();
        let reader = std::thread::spawn(move || reader_loop(&host_end, &tx, None));
        drop(helper_end);
        reader.join().expect("reader");

        let gone = rx.recv_timeout(Duration::from_secs(1));
        let restarting = matches!(*CLIENT.lock().expect("client"), ClientSlot::Restarting);
        let hidden = !view_window_visible();
        let uncreated = VIEWS
            .lock()
            .expect("views")
            .entries
            .values()
            .all(|entry| !entry.created);
        finish_restart();
        let respawnable = matches!(*CLIENT.lock().expect("client"), ClientSlot::Unspawned);
        {
            let mut views = VIEWS.lock().expect("views");
            views.entries.remove(&shown_view);
            views.entries.remove(&hidden_view);
            views.publish();
        }
        free_registry_views(&[shown_view, hidden_view]);
        UNEXPECTED_EXITS.store(0, Ordering::SeqCst);

        assert!(
            matches!(gone, Ok(Upcall::HelperGone { visible_views: 1 })),
            "exactly the one shown view gets an Android Back"
        );
        assert!(restarting && hidden && uncreated);
        assert!(
            respawnable,
            "the next WebView or cookie call starts a new helper"
        );
    }

    #[test]
    fn unexpected_helper_exits_restart_until_the_limit_then_latch() {
        let _serial = HELPER_STATE_TEST_LOCK
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        UNEXPECTED_EXITS.store(0, Ordering::SeqCst);
        assert!(matches!(
            slot_after(Loss::Exited("EOF".into())),
            ClientSlot::Restarting
        ));
        assert!(matches!(
            slot_after(Loss::Fatal("no display".into())),
            ClientSlot::Failed(reason) if reason == "no display"
        ));
        assert!(matches!(
            slot_after(Loss::Exited("EOF".into())),
            ClientSlot::Restarting
        ));
        match slot_after(Loss::Exited("EOF".into())) {
            ClientSlot::Failed(reason) => {
                assert!(reason.contains("exited unexpectedly 3 times"), "{reason}")
            }
            _ => panic!("the third unexpected exit must latch"),
        }
        UNEXPECTED_EXITS.store(0, Ordering::SeqCst);
    }

    fn cef_storage(tag: &str) -> Storage {
        let root = temp_dir(tag);
        let storage = Storage {
            data: root.join("app-data/webview"),
            cache: root.join("cache/webview"),
            cef_profile: root.join("app-data").join(CEF_PROFILE_DIR),
        };
        std::fs::create_dir_all(&storage.data).expect("create the WebKit data dir");
        cef_profile::install_fixture(&storage.cef_profile);
        storage
    }

    fn migrate(stream: &UnixStream, profile: &Path) -> Result<Option<Migration>, ClientError> {
        migrate_cef_profile(
            stream,
            profile,
            cef_profile::fixture_written_at(),
            MIGRATION_TIMEOUT,
        )
    }

    fn reply(stream: &UnixStream, msg: HelperMsg) {
        let frame = msg.encode().expect("encode the reply");
        (&mut &*stream).write_all(&frame).expect("write the reply");
    }

    fn answer_import(stream: &UnixStream, failed: u32) -> Vec<StoredCookie> {
        let ConsumerMsg::CookieImport {
            request_id,
            cookies,
        } = proto::read_consumer_msg(&mut &*stream).expect("read the import")
        else {
            panic!("the migration must start with CookieImport");
        };
        let total = u32::try_from(cookies.len()).expect("cookie count");
        reply(
            stream,
            HelperMsg::CookieImportResult {
                request_id,
                imported: total - failed,
                failed,
            },
        );
        cookies
    }

    fn answer_flush(stream: &UnixStream, ok: bool) {
        let ConsumerMsg::CookieFlush { request_id } =
            proto::read_consumer_msg(&mut &*stream).expect("read the flush")
        else {
            panic!("an import must be followed by CookieFlush");
        };
        reply(stream, HelperMsg::CookieFlushed { request_id, ok });
    }

    fn assert_nothing_more(stream: &UnixStream) {
        assert_eq!(
            proto::read_consumer_msg(&mut &*stream),
            Err(proto::ProtoError::Eof)
        );
    }

    #[test]
    fn only_a_cef_profile_beside_a_new_webkit_store_is_imported() {
        let storage = cef_storage("cef-plan");
        assert_eq!(
            cef_profile_to_import(&storage),
            Some(storage.cef_profile.clone())
        );
        let webkit_store = storage.data.join(proto::PERSISTENT_COOKIE_FILE);
        touch(&webkit_store);
        assert_eq!(cef_profile_to_import(&storage), None);
        std::fs::remove_file(&webkit_store).expect("remove the WebKit store");
        std::fs::remove_dir_all(&storage.cef_profile).expect("remove the CEF profile");
        assert_eq!(cef_profile_to_import(&storage), None);
    }

    #[test]
    fn a_cef_profile_is_imported_flushed_and_then_removed() {
        let profile = cef_storage("cef-migrated").cef_profile;
        let (client_end, helper_end) = UnixStream::pair().expect("pair");
        let helper = std::thread::spawn(move || {
            let cookies = answer_import(&helper_end, 0);
            answer_flush(&helper_end, true);
            assert_nothing_more(&helper_end);
            cookies
        });
        let pending = migrate(&client_end, &profile).expect("migrate");
        drop(client_end);
        let cookies = helper.join().expect("fake helper");
        assert!(pending.is_none(), "an answered migration is finished");
        assert_eq!(cookies.len(), 12);
        let login = cookies
            .iter()
            .find(|cookie| cookie.name == ".ROBLOSECURITY")
            .expect("the login cookie is imported");
        assert_eq!(
            login.value,
            "synthetic-roblosecurity-for-the-eclipse-cef-migration-test"
        );
        assert!(!profile.exists(), "a migrated CEF profile must be removed");
    }

    #[test]
    fn the_cef_profile_stays_until_every_cookie_is_saved() {
        for (tag, failed, flushed) in [("cef-rejected", 1, true), ("cef-unflushed", 0, false)] {
            let profile = cef_storage(tag).cef_profile;
            let (client_end, helper_end) = UnixStream::pair().expect("pair");
            let helper = std::thread::spawn(move || {
                answer_import(&helper_end, failed);
                if failed == 0 {
                    answer_flush(&helper_end, flushed);
                }
                assert_nothing_more(&helper_end);
            });
            let pending =
                migrate(&client_end, &profile).expect("a refused import is not a protocol failure");
            assert!(pending.is_none(), "{tag}: a refused import is finished");
            drop(client_end);
            helper.join().expect("fake helper");
            assert!(profile.exists(), "{tag}: the CEF profile must stay");
        }
    }

    #[test]
    fn an_unreadable_cef_profile_stays_and_sends_nothing() {
        let profile = cef_storage("cef-unreadable").cef_profile;
        std::fs::write(profile.join(cef_profile::COOKIE_DATABASE), [0x5Au8; 4096])
            .expect("damage the cookie database");
        let (client_end, helper_end) = UnixStream::pair().expect("pair");
        let pending = migrate(&client_end, &profile).expect("migrate");
        drop(client_end);
        assert_nothing_more(&helper_end);
        assert!(pending.is_none());
        assert!(profile.exists());
    }

    #[test]
    fn a_silent_helper_keeps_the_cef_profile_and_the_socket_blocking() {
        let profile = cef_storage("cef-silent").cef_profile;
        let (client_end, _helper_end) = UnixStream::pair().expect("pair");
        let pending = migrate_cef_profile(
            &client_end,
            &profile,
            cef_profile::fixture_written_at(),
            Duration::from_millis(50),
        )
        .expect("an unanswered import is not a protocol failure");
        assert!(pending.is_some(), "the migration waits for the late answer");
        assert!(profile.exists());
        assert_eq!(client_end.read_timeout().expect("read timeout"), None);
    }

    #[test]
    fn a_cef_migration_the_helper_answers_late_finishes_while_the_helper_runs() {
        let _serial = HELPER_STATE_TEST_LOCK
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        UNEXPECTED_EXITS.store(0, Ordering::SeqCst);
        let profile = cef_storage("cef-late").cef_profile;
        let (client_end, helper_end) = UnixStream::pair().expect("pair");
        let (gave_up, host_gave_up) = mpsc::channel::<()>();
        let helper = std::thread::spawn(move || {
            let ConsumerMsg::CookieImport {
                request_id,
                cookies,
            } = proto::read_consumer_msg(&mut &helper_end).expect("read the import")
            else {
                panic!("the migration must start with CookieImport");
            };
            host_gave_up
                .recv()
                .expect("the host stops waiting before the answer");
            reply(
                &helper_end,
                HelperMsg::CookieImportResult {
                    request_id,
                    imported: u32::try_from(cookies.len()).expect("cookie count"),
                    failed: 0,
                },
            );
            answer_flush(&helper_end, true);
        });
        let pending = migrate_cef_profile(
            &client_end,
            &profile,
            cef_profile::fixture_written_at(),
            Duration::from_millis(50),
        )
        .expect("migrate");
        let kept_while_unanswered = profile.exists();
        *CLIENT.lock().expect("client") = ClientSlot::Live(Client {
            child: std::process::Command::new("/bin/sh")
                .args(["-c", "exit 0"])
                .spawn()
                .expect("spawn a stand-in helper"),
            writer: client_end.try_clone().expect("writer"),
            io: std::thread::spawn(|| {}),
            upcall: std::thread::spawn(|| {}),
        });
        gave_up.send(()).expect("release the helper");
        let (tx, _rx) = mpsc::channel();
        let reader = std::thread::spawn(move || reader_loop(&client_end, &tx, pending));
        helper.join().expect("fake helper");
        reader.join().expect("reader");
        finish_restart();
        UNEXPECTED_EXITS.store(0, Ordering::SeqCst);

        assert!(
            kept_while_unanswered,
            "the profile stays while the import is unanswered"
        );
        assert!(
            !profile.exists(),
            "a late but complete import is flushed and the CEF profile removed"
        );
    }

    #[test]
    fn a_helper_failing_during_the_cef_migration_fails_its_start() {
        let profile = cef_storage("cef-fatal").cef_profile;
        let (client_end, helper_end) = UnixStream::pair().expect("pair");
        let helper = std::thread::spawn(move || {
            proto::read_consumer_msg(&mut &helper_end).expect("read the import");
            reply(
                &helper_end,
                HelperMsg::Fatal {
                    reason: "the cookie store is gone".into(),
                },
            );
        });
        match migrate(&client_end, &profile) {
            Err(ClientError::Unavailable(reason)) => {
                assert_eq!(reason, "the cookie store is gone");
            }
            other => panic!("expected the helper's failure, got {other:?}"),
        }
        helper.join().expect("fake helper");
        assert!(profile.exists());
    }
}
