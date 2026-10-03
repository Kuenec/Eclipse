use std::collections::{BTreeMap, BTreeSet};
use std::io::Write as _;
use std::os::fd::AsFd as _;
use std::os::unix::net::UnixStream;
use std::path::{Path, PathBuf};
use std::process::Child;
use std::sync::atomic::{AtomicBool, AtomicI64, AtomicU32, AtomicU64, AtomicUsize, Ordering};
use std::sync::{mpsc, Condvar, Mutex, MutexGuard, OnceLock};
use std::thread::JoinHandle;
use std::time::{Duration, Instant, SystemTime};

use super::cef_profile;
use super::cookie_jar::{self, CookieJar, JarFileError};
use super::proto::{
    self, ClearScope, ConsumerMsg, CookiePair, HelperMsg, LoadEvent, StoredCookie, PROTO_VERSION,
};
use super::redact::url_scheme_and_host_for_log;
use crate::framework::view_registry;

pub const HELPER_NOT_FOUND_MARKER: &str = "helper binary not found";

const HANDSHAKE_TIMEOUT: Duration = Duration::from_secs(10);

const COOKIE_IMPORT_TIMEOUT: Duration = Duration::from_secs(5);

const SPAWN_RESULT_TIMEOUT: Duration = HANDSHAKE_TIMEOUT
    .saturating_add(COOKIE_IMPORT_TIMEOUT)
    .saturating_add(Duration::from_secs(5));

const COOKIE_GET_TIMEOUT: Duration = Duration::from_secs(5);

const COOKIE_FLUSH_TIMEOUT: Duration = Duration::from_secs(10);

pub const HELPER_IDLE_GRACE: Duration = Duration::from_secs(30);

const IDLE_STOP_WAIT: Duration = Duration::from_secs(5);

const HELPER_EXIT_LIMIT: u32 = 3;

const MISSING_LIBRARY_EXIT: i32 = 127;

const DATA_DIR_ENV: &str = "ECLIPSE_WEBVIEW_DATA_DIR";

const CACHE_DIR_ENV: &str = "ECLIPSE_WEBVIEW_CACHE_DIR";

const CEF_PROFILE_DIR: &str = "webview-cef";

const ACTIVATION_TOKEN_ENV: [&str; 2] = ["XDG_ACTIVATION_TOKEN", "DESKTOP_STARTUP_ID"];

const CLIENT_SETTINGS_SHIM_FILE_NAME: &str = "libeclipse_client_settings_path.so";

const BRIDGE_RESULT_OVER_CAP: &str = "\"eclipse: bridge result exceeds the frame cap\"";

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ClientError {
    HelperNotFound { probed: Vec<PathBuf> },

    ExplicitPathMissing { source: &'static str, path: PathBuf },

    Spawn(String),

    Storage(String),

    Handshake(String),

    VersionMismatch { helper_version: u16 },

    CookieImport(String),

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
            Self::CookieImport(e) => write!(f, "the helper did not take the cookie jar: {e}"),
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

    idle: JoinHandle<()>,

    generation: HelperGeneration,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
struct HelperGeneration(u64);

static NEXT_GENERATION: AtomicU64 = AtomicU64::new(1);

impl HelperGeneration {
    fn next() -> Self {
        Self(NEXT_GENERATION.fetch_add(1, Ordering::Relaxed))
    }
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

    last_load: Option<ConsumerMsg>,
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
        self.last_load = Some(load.clone());
        batch.push(load);
        batch
    }

    fn restorable(&self) -> bool {
        !self.created && !self.shown && self.last_load.is_some()
    }

    fn restore(&mut self, view: i64) -> Vec<ConsumerMsg> {
        match self.last_load.clone() {
            Some(load) => self.load_batch(view, true, load),
            None => Vec::new(),
        }
    }

    fn note_page(&mut self, view: i64, url: &str) {
        let web = url.starts_with("https://") || url.starts_with("http://");
        let data_page = matches!(
            &self.last_load,
            Some(ConsumerMsg::LoadData { base_url, .. }) if base_url == url
        );
        if web && !data_page {
            self.last_load = Some(ConsumerMsg::LoadUrl {
                view,
                url: url.to_string(),
            });
        }
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

    helper: Option<HelperGeneration>,

    activity: u64,
}

impl Views {
    const fn new() -> Self {
        Self {
            entries: BTreeMap::new(),
            closing: BTreeSet::new(),
            helper: None,
            activity: 0,
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
        VIEWS_CHANGED.notify_all();
    }

    fn idle(&self) -> bool {
        !self.entries.values().any(ViewEntry::window_visible)
    }

    fn still_idle(&self, generation: HelperGeneration, activity: u64) -> bool {
        self.helper == Some(generation) && self.idle() && self.activity == activity
    }

    fn note_activity(&mut self) {
        self.activity = self.activity.wrapping_add(1);
    }

    fn adopt(&mut self, generation: HelperGeneration) {
        self.helper = Some(generation);
        self.note_activity();
        self.publish();
    }

    fn restores(&self, shown: &[(i64, bool)]) -> bool {
        shown.iter().any(|&(view, shown)| {
            shown && self.entries.get(&view).is_some_and(ViewEntry::restorable)
        })
    }

    fn apply_shown(&mut self, shown: &[(i64, bool)], can_create: bool) -> Vec<ConsumerMsg> {
        let mut batch = Vec::new();
        for &(view, shown) in shown {
            let Some(entry) = self.entries.get_mut(&view) else {
                continue;
            };
            if shown && can_create && entry.restorable() {
                batch.extend(entry.restore(view));
            } else {
                batch.extend(entry.show(view, shown));
            }
        }
        if batch
            .iter()
            .any(|msg| matches!(msg, ConsumerMsg::SetVisible { visible: true, .. }))
        {
            self.note_activity();
        }
        batch
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
        self.helper = None;
        visible
    }
}

static VIEWS: Mutex<Views> = Mutex::new(Views::new());

static VIEWS_CHANGED: Condvar = Condvar::new();

static TRACKED_VIEWS: AtomicUsize = AtomicUsize::new(0);

static VISIBLE_VIEWS: AtomicUsize = AtomicUsize::new(0);

static ACTIVATION_TARGET: AtomicI64 = AtomicI64::new(0);

static ACTIVATION_WANTED: AtomicBool = AtomicBool::new(false);

static COOKIE_GETS: Mutex<BTreeMap<u32, mpsc::Sender<Vec<CookiePair>>>> =
    Mutex::new(BTreeMap::new());

static COOKIE_FLUSHES: Mutex<BTreeMap<u32, mpsc::Sender<bool>>> = Mutex::new(BTreeMap::new());

static OWED_REPLIES: Mutex<BTreeMap<u32, HelperGeneration>> = Mutex::new(BTreeMap::new());

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

impl Client {
    fn send(&self, frames: &[Vec<u8>]) -> Result<(), ClientError> {
        for frame in frames {
            (&mut &self.writer).write_all(frame).map_err(|e| {
                ClientError::Unavailable(format!("control-socket write failed: {}", e.kind()))
            })?;
        }
        Ok(())
    }
}

fn write_frames(slot: &ClientSlot, frames: &[Vec<u8>]) -> Result<(), ClientError> {
    slot.live()?.send(frames)
}

fn send_owing_reply(slot: &ClientSlot, request_id: u32, frame: Vec<u8>) -> Result<(), ClientError> {
    let client = slot.live()?;
    OWED_REPLIES
        .lock()
        .map_err(|_| ClientError::Internal("owed replies lock poisoned"))?
        .insert(request_id, client.generation);
    client
        .send(&[frame])
        .inspect_err(|_| reply_received(request_id))
}

fn reply_received(request_id: u32) {
    if let Ok(mut owed) = OWED_REPLIES.lock() {
        owed.remove(&request_id);
    }
}

fn replies_still_owed(generation: HelperGeneration) -> Vec<u32> {
    let Ok(mut owed) = OWED_REPLIES.lock() else {
        return Vec::new();
    };
    let mut unanswered = Vec::new();
    owed.retain(|request_id, owner| {
        let retired = *owner == generation;
        if retired {
            unanswered.push(*request_id);
        }
        !retired
    });
    unanswered
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

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum CookieOwner {
    Jar,

    Helper(HelperGeneration),
}

struct CookieStore {
    jar: CookieJar,

    file: PathBuf,

    owner: CookieOwner,

    dirty: bool,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum CookieAnswer {
    Now(bool),

    FromHelper,
}

static COOKIES: Mutex<Option<CookieStore>> = Mutex::new(None);

static JAR_WRITE: Mutex<()> = Mutex::new(());

fn lock_cookie_store() -> Result<MutexGuard<'static, Option<CookieStore>>, ClientError> {
    COOKIES
        .lock()
        .map_err(|_| ClientError::Internal("cookie store lock poisoned"))
}

fn with_cookie_store<T>(action: impl FnOnce(&mut CookieStore) -> T) -> Result<T, ClientError> {
    let mut slot = lock_cookie_store()?;
    let store = match &mut *slot {
        Some(store) => store,
        empty => empty.insert(CookieStore::open()?),
    };
    Ok(action(store))
}

impl CookieStore {
    fn open() -> Result<Self, ClientError> {
        let storage = webview_storage()?;
        Self::load(
            storage.data.join(cookie_jar::COOKIE_JAR_FILE),
            &storage.cef_profile,
            SystemTime::now(),
        )
    }

    fn load(file: PathBuf, cef_profile: &Path, now: SystemTime) -> Result<Self, ClientError> {
        let jar = match cookie_jar::read_file(&file, now) {
            Ok(Some(jar)) => {
                warn_about_an_unmigrated_cef_profile(cef_profile);
                jar
            }
            Ok(None) => migrate_cef_profile(&file, cef_profile, now),
            Err(JarFileError::Io(error)) => {
                return Err(ClientError::Storage(format!(
                    "cannot read the cookie jar {}: {error}",
                    file.display()
                )))
            }
            Err(error @ (JarFileError::Oversized | JarFileError::Codec(_))) => {
                set_aside_a_corrupt_jar(&file, &error);
                CookieJar::default()
            }
        };
        Ok(Self {
            jar,
            file,
            owner: CookieOwner::Jar,
            dirty: false,
        })
    }

    fn answers(&self) -> bool {
        self.owner == CookieOwner::Jar
    }

    fn set(&mut self, url: &str, header: &str) -> bool {
        match self.jar.set_from_header(url, header, SystemTime::now()) {
            Ok(changed) => {
                self.dirty |= changed;
                true
            }
            Err(rejected) => {
                tracing::warn!(
                    target: "android.webkit.CookieManager",
                    url = %url_scheme_and_host_for_log(url),
                    %rejected,
                    "CookieManager.setCookie: the cookie jar refused a cookie"
                );
                false
            }
        }
    }

    fn clear(&mut self, scope: ClearScope) -> bool {
        let removed = self.jar.clear(scope);
        self.dirty |= removed;
        removed
    }

    fn take_unsaved(&mut self) -> Result<Option<(PathBuf, Vec<u8>)>, proto::ProtoError> {
        if !self.dirty {
            return Ok(None);
        }
        let bytes = self.jar.encode()?;
        self.dirty = false;
        Ok(Some((self.file.clone(), bytes)))
    }
}

fn warn_about_an_unmigrated_cef_profile(profile: &Path) {
    if matches!(profile.try_exists(), Ok(false)) {
        return;
    }
    tracing::warn!(
        profile = %profile.display(),
        "keeping the CEF WebView profile without migrating it, because the cookie jar already \
         exists and the old cookies could replace newer ones"
    );
}

fn set_aside_a_corrupt_jar(file: &Path, error: &JarFileError) {
    match cookie_jar::quarantine(file) {
        Ok(moved_to) => tracing::error!(
            jar = %file.display(),
            moved_to = %moved_to.display(),
            %error,
            "the WebView cookie jar is corrupt; it was moved aside and the jar starts empty"
        ),
        Err(move_error) => tracing::error!(
            jar = %file.display(),
            %error,
            %move_error,
            "the WebView cookie jar is corrupt and cannot be moved aside; the jar starts \
             empty and its next save replaces the file"
        ),
    }
}

fn migrate_cef_profile(file: &Path, profile: &Path, now: SystemTime) -> CookieJar {
    match profile.try_exists() {
        Ok(true) => {}
        Ok(false) => return CookieJar::default(),
        Err(error) => {
            tracing::warn!(
                profile = %profile.display(),
                "cannot look for the CEF WebView profile, so its cookies were not migrated: {error}"
            );
            return CookieJar::default();
        }
    }
    let jar = match cef_profile::read_cookies(profile, now) {
        Ok(cookies) => CookieJar::from_cookies(cookies, now),
        Err(error) => {
            tracing::warn!(
                profile = %profile.display(),
                reason = %error,
                "keeping the CEF WebView profile because its cookies cannot be migrated"
            );
            return CookieJar::default();
        }
    };
    let cookies = jar.all_unexpired(now).len();
    match save_and_read_back(file, &jar, now) {
        Err(reason) => tracing::warn!(
            profile = %profile.display(),
            reason,
            "keeping the CEF WebView profile because the cookie jar did not save its cookies"
        ),
        Ok(()) => match std::fs::remove_dir_all(profile) {
            Ok(()) => tracing::info!(
                cookies,
                "migrated the CEF WebView cookies into the cookie jar and removed the CEF profile"
            ),
            Err(error) => tracing::error!(
                profile = %profile.display(),
                cookies,
                "migrated the CEF WebView cookies but cannot remove the CEF profile: {error}"
            ),
        },
    }
    jar
}

fn save_and_read_back(file: &Path, jar: &CookieJar, now: SystemTime) -> Result<(), String> {
    let bytes = jar.encode().map_err(|error| error.to_string())?;
    cookie_jar::write_file(file, &bytes).map_err(|error| error.to_string())?;
    match cookie_jar::read_file(file, now) {
        Ok(Some(saved)) if saved == *jar => Ok(()),
        Ok(_) => Err("the saved jar reads back differently".to_string()),
        Err(error) => Err(error.to_string()),
    }
}

fn with_helper_cookies(
    generation: HelperGeneration,
    action: impl FnOnce(&mut CookieStore),
) -> bool {
    let mut slot = match lock_cookie_store() {
        Ok(slot) => slot,
        Err(error) => {
            tracing::warn!(%error, "the web engine helper's cookies were not kept");
            return false;
        }
    };
    match slot.as_mut() {
        Some(store) if store.owner == CookieOwner::Helper(generation) => {
            action(store);
            true
        }
        _ => false,
    }
}

fn install_snapshot(generation: HelperGeneration, cookies: Vec<StoredCookie>) {
    let installed = with_helper_cookies(generation, |store| {
        store.dirty |= store.jar.replace_all(cookies, SystemTime::now());
    });
    if !installed {
        tracing::debug!("ignoring a cookie snapshot from a retired web engine helper");
    }
}

fn return_cookies_to_the_jar(generation: HelperGeneration) {
    with_helper_cookies(generation, |store| store.owner = CookieOwner::Jar);
}

fn hand_over_cookies(stream: &UnixStream, timeout: Duration) -> Result<(), ClientError> {
    let cookies = with_cookie_store(|store| store.jar.all_unexpired(SystemTime::now()))?;
    let total = cookies.len();
    let request_id = next_request_id();
    send_spawn_frame(
        stream,
        &encode(&ConsumerMsg::CookieImport {
            request_id,
            cookies,
        })?,
    )?;
    let deadline = Instant::now() + timeout;
    let imported = loop {
        match read_spawn_reply(stream, deadline)? {
            None => return Err(ClientError::TimedOut("the cookie import")),
            Some(HelperMsg::CookieImportResult {
                request_id: answered,
                imported,
                ..
            }) if answered == request_id => break imported,
            Some(HelperMsg::CookieSnapshot { .. }) => {}
            Some(other) => {
                return Err(ClientError::CookieImport(format!(
                    "expected the import's result, got {}",
                    other.name()
                )))
            }
        }
    };
    stream.set_read_timeout(None).map_err(|e| {
        ClientError::CookieImport(format!("clearing the read timeout failed: {}", e.kind()))
    })?;
    if usize::try_from(imported) != Ok(total) {
        tracing::warn!(
            imported,
            total,
            "the web engine helper did not take every cookie of the jar"
        );
    }
    Ok(())
}

fn send_spawn_frame(stream: &UnixStream, frame: &[u8]) -> Result<(), ClientError> {
    (&mut &*stream)
        .write_all(frame)
        .map_err(|e| ClientError::CookieImport(format!("write failed: {}", e.kind())))
}

fn read_spawn_reply(
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
        .map_err(|e| ClientError::CookieImport(format!("set_read_timeout failed: {}", e.kind())))?;
    match proto::read_helper_msg(&mut &*stream) {
        Ok(HelperMsg::Fatal { reason }) => Err(ClientError::Unavailable(reason)),
        Ok(msg) => Ok(Some(msg)),
        Err(proto::ProtoError::Io(
            std::io::ErrorKind::WouldBlock | std::io::ErrorKind::TimedOut,
        )) => Ok(None),
        Err(e) => Err(ClientError::CookieImport(format!("protocol error: {e}"))),
    }
}

struct Spawned {
    child: Child,
    writer: UnixStream,
    upcall: JoinHandle<()>,
}

fn spawn_client(java_vm: jni::vm::JavaVM) -> Result<Client, ClientError> {
    let generation = HelperGeneration::next();
    let (tx, rx) = mpsc::channel::<Result<Spawned, ClientError>>();
    let io = std::thread::Builder::new()
        .name("eclipse-webview-io".into())
        .spawn(move || io_thread_main(&tx, java_vm, generation))
        .map_err(|e| ClientError::Spawn(format!("io-thread spawn failed: {e}")))?;
    match rx.recv_timeout(SPAWN_RESULT_TIMEOUT) {
        Ok(Ok(spawned)) => {
            let mut child = spawned.child;
            match adopt(generation) {
                Ok(idle) => Ok(Client {
                    child,
                    writer: spawned.writer,
                    io,
                    upcall: spawned.upcall,
                    idle,
                    generation,
                }),
                Err(e) => {
                    let _ = child.kill();
                    let _ = child.wait();
                    Err(e)
                }
            }
        }
        Ok(Err(e)) => {
            let _ = io.join();
            Err(e)
        }
        Err(_) => Err(ClientError::Handshake(
            "helper spawn/handshake verdict timed out".into(),
        )),
    }
}

fn adopt(generation: HelperGeneration) -> Result<JoinHandle<()>, ClientError> {
    lock_views()?.adopt(generation);
    let idle = std::thread::Builder::new()
        .name("eclipse-webview-idle".into())
        .spawn(move || watch_idle(generation, HELPER_IDLE_GRACE))
        .map_err(|e| ClientError::Spawn(format!("idle-thread spawn failed: {e}")))?;
    with_cookie_store(|store| store.owner = CookieOwner::Helper(generation))?;
    Ok(idle)
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum IdleWait {
    UntilChange,

    For(Duration),

    Elapsed { activity: u64 },
}

#[derive(Default)]
struct IdleTimer {
    since: Option<(u64, Instant)>,
}

impl IdleTimer {
    fn next(&mut self, idle: bool, activity: u64, now: Instant, grace: Duration) -> IdleWait {
        if !idle {
            self.since = None;
            return IdleWait::UntilChange;
        }
        let start = match self.since {
            Some((seen, start)) if seen == activity => start,
            _ => {
                self.since = Some((activity, now));
                now
            }
        };
        let left = grace.saturating_sub(now.saturating_duration_since(start));
        if left.is_zero() {
            IdleWait::Elapsed { activity }
        } else {
            IdleWait::For(left)
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum IdleStop {
    Stopped,

    Busy,

    Retired,
}

fn watch_idle(generation: HelperGeneration, grace: Duration) {
    loop {
        let outcome = match wait_for_idle(generation, grace) {
            Ok(Some(activity)) => stop_idle_helper(generation, activity),
            Ok(None) => Ok(IdleStop::Retired),
            Err(error) => Err(error),
        };
        match outcome {
            Ok(IdleStop::Busy) => {}
            Ok(IdleStop::Stopped | IdleStop::Retired) => return,
            Err(error) => {
                tracing::warn!(
                    error = %error,
                    "webview client: the idle web engine helper keeps running"
                );
                return;
            }
        }
    }
}

fn wait_for_idle(
    generation: HelperGeneration,
    grace: Duration,
) -> Result<Option<u64>, ClientError> {
    let mut views = lock_views()?;
    let mut timer = IdleTimer::default();
    loop {
        if views.helper != Some(generation) {
            return Ok(None);
        }
        views = match timer.next(views.idle(), views.activity, Instant::now(), grace) {
            IdleWait::UntilChange => VIEWS_CHANGED
                .wait(views)
                .map_err(|_| ClientError::Internal("views lock poisoned"))?,
            IdleWait::For(left) => {
                VIEWS_CHANGED
                    .wait_timeout(views, left)
                    .map_err(|_| ClientError::Internal("views lock poisoned"))?
                    .0
            }
            IdleWait::Elapsed { activity } => return Ok(Some(activity)),
        };
    }
}

fn stop_idle_helper(generation: HelperGeneration, activity: u64) -> Result<IdleStop, ClientError> {
    let mut slot = lock_client()?;
    let still_idle = lock_views()?.still_idle(generation, activity);
    let mut client = match std::mem::replace(&mut *slot, ClientSlot::Unspawned) {
        ClientSlot::Live(client) if client.generation == generation && still_idle => client,
        ClientSlot::Live(client) if client.generation == generation => {
            *slot = ClientSlot::Live(client);
            return Ok(IdleStop::Busy);
        }
        other => {
            *slot = other;
            return Ok(IdleStop::Retired);
        }
    };
    let flushed = ask_helper(&client, &COOKIE_FLUSHES, |request_id| {
        ConsumerMsg::CookieFlush { request_id }
    })
    .and_then(|reply| reply.wait("the cookie flush before its idle stop", IDLE_STOP_WAIT));
    match flushed {
        Ok(true) => {}
        Ok(false) => tracing::warn!(
            "webview client: the idle web engine helper could not hand back its cookies; the jar \
             keeps their last snapshot"
        ),
        Err(error) => tracing::warn!(
            error = %error,
            "webview client: the idle web engine helper did not hand back its cookies; the jar \
             keeps their last snapshot"
        ),
    }
    if let Ok(mut views) = VIEWS.lock() {
        views.reset_after_helper_loss();
        views.publish();
    }
    return_cookies_to_the_jar(generation);
    wake_all_cookie_waiters();
    drop(slot);
    match stop_helper_process(&mut client, IDLE_STOP_WAIT) {
        Some(status) if status.success() => tracing::info!(
            %status,
            "webview client: stopped the web engine helper because no WebView window was shown"
        ),
        status => tracing::warn!(
            status = ?status,
            "webview client: the idle web engine helper did not exit cleanly"
        ),
    }
    let io_joined = client.io.join().is_ok();
    let upcall_joined = client.upcall.join().is_ok();
    if !(io_joined && upcall_joined) {
        tracing::warn!(
            io_joined,
            upcall_joined,
            "webview client: a thread of the idle web engine helper panicked"
        );
    }
    if let Err(error) = save_cookie_jar() {
        tracing::warn!(
            error = %error,
            "webview client: the cookies of the idle web engine helper were not saved"
        );
    }
    Ok(IdleStop::Stopped)
}

fn stop_helper_process(
    client: &mut Client,
    deadline: Duration,
) -> Option<std::process::ExitStatus> {
    if let Ok(frame) = ConsumerMsg::Shutdown.encode() {
        let _ = (&mut &client.writer).write_all(&frame);
    }
    let t0 = Instant::now();
    while t0.elapsed() < deadline {
        match client.child.try_wait() {
            Ok(Some(status)) => return Some(status),
            Ok(None) => std::thread::sleep(Duration::from_millis(50)),
            Err(_) => break,
        }
    }
    let _ = client.child.kill();
    client.child.wait().ok()
}

fn io_thread_main(
    tx: &mpsc::Sender<Result<Spawned, ClientError>>,
    java_vm: jni::vm::JavaVM,
    generation: HelperGeneration,
) {
    let spawned = webview_storage().and_then(|storage| spawn_helper_process(&storage));
    let (stream, mut child) = match spawned {
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
        hand_over_cookies(&stream, COOKIE_IMPORT_TIMEOUT)
    });
    if let Err(e) = started {
        let _ = child.kill();
        let status = child.wait().ok();
        let _ = tx.send(Err(with_exit_status(e, status)));
        return;
    }
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
        .spawn(move || upcall_thread_main(&up_rx, &java_vm, generation))
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
    reader_loop(&stream, &up_tx, generation);
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

impl Upcall {
    fn answered_request(&self) -> Option<u32> {
        match self {
            Self::EvaluateJsResult { request_id, .. }
            | Self::CookieSetResult { request_id, .. }
            | Self::CookiesCleared { request_id, .. } => Some(*request_id),
            Self::LoadChanged { .. }
            | Self::Progress { .. }
            | Self::LoadFailed { .. }
            | Self::ResourceLoad { .. }
            | Self::Policy { .. }
            | Self::Back
            | Self::BridgeCall { .. }
            | Self::ViewClosed { .. }
            | Self::HelperGone { .. } => None,
        }
    }
}

enum Routed {
    Upcall(Upcall),

    Snapshot(Vec<StoredCookie>),

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
                entry.note_page(view, &url);
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
                "webview client: a cookie import result arrived after the helper started"
            );
            return Routed::Handled;
        }
        HelperMsg::CookieSnapshot { cookies } => return Routed::Snapshot(cookies),
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

fn reader_loop(stream: &UnixStream, upcalls: &mpsc::Sender<Upcall>, generation: HelperGeneration) {
    loop {
        let msg = match proto::read_helper_msg(&mut &*stream) {
            Ok(msg) => msg,
            Err(proto::ProtoError::Eof) => {
                helper_lost(
                    Loss::Exited("the helper closed its control socket".into()),
                    upcalls,
                    generation,
                );
                return;
            }
            Err(e) => {
                helper_lost(
                    Loss::Exited(format!("protocol error from the helper: {e}")),
                    upcalls,
                    generation,
                );
                return;
            }
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
                if let Some(request_id) = upcall.answered_request() {
                    reply_received(request_id);
                }
                let _ = upcalls.send(upcall);
            }
            Routed::Snapshot(cookies) => install_snapshot(generation, cookies),
            Routed::Handled => {}
            Routed::Fatal(reason) => {
                helper_lost(Loss::Fatal(reason), upcalls, generation);
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

fn helper_lost(loss: Loss, upcalls: &mpsc::Sender<Upcall>, generation: HelperGeneration) {
    let Ok(mut slot) = CLIENT.lock() else {
        return;
    };
    if !matches!(&*slot, ClientSlot::Live(client) if client.generation == generation) {
        tracing::debug!("webview reader exiting after its helper was retired");
        return;
    }
    let reason = match &loss {
        Loss::Fatal(reason) | Loss::Exited(reason) => reason.clone(),
    };
    if let ClientSlot::Live(mut client) = std::mem::replace(&mut *slot, slot_after(loss)) {
        let _ = client.child.kill();
        let _ = client.child.wait();
    }
    return_cookies_to_the_jar(generation);
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

fn upcall_thread_main(
    rx: &mpsc::Receiver<Upcall>,
    java_vm: &jni::vm::JavaVM,
    generation: HelperGeneration,
) {
    let mut gone = None;
    while let Ok(upcall) = rx.recv() {
        match upcall {
            Upcall::HelperGone { visible_views } => gone = Some(visible_views),
            other => run_upcall(java_vm, other, generation),
        }
    }
    crate::framework::fail_webview_callbacks(
        java_vm,
        &replies_still_owed(generation),
        "web engine helper connection closed",
    );
    let Some(visible_views) = gone else {
        return;
    };
    for _ in 0..visible_views {
        crate::framework::dispatch_activity_back(java_vm);
    }
    finish_restart();
}

fn run_upcall(java_vm: &jni::vm::JavaVM, upcall: Upcall, generation: HelperGeneration) {
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
            send_reply(
                generation,
                &ConsumerMsg::PolicyReply {
                    policy_id,
                    override_load,
                },
            );
        }
        Upcall::Back => crate::framework::dispatch_activity_back(java_vm),
        Upcall::BridgeCall {
            view,
            call_id,
            payload_json,
        } => {
            let (ok, result_json) =
                crate::framework::fire_bridge_call(java_vm, view, call_id, &payload_json);
            send_reply(generation, &bridge_result(call_id, ok, result_json));
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

fn send_reply(generation: HelperGeneration, msg: &ConsumerMsg) {
    let result = encode(msg).and_then(|frame| {
        let slot = lock_client()?;
        match &*slot {
            ClientSlot::Live(client) if client.generation == generation => {
                write_frames(&slot, &[frame])
            }
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
        views.note_activity();
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
    send_owing_reply(&slot, request_id, frame)
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

pub fn refresh_visibility(env: &jni::Env<'_>) {
    if TRACKED_VIEWS.load(Ordering::Acquire) == 0 {
        return;
    }
    let loaded: Vec<i64> = match VIEWS.lock() {
        Ok(views) => views
            .entries
            .iter()
            .filter(|(_, entry)| entry.last_load.is_some())
            .map(|(view, _)| *view)
            .collect(),
        Err(_) => return,
    };
    if loaded.is_empty() {
        return;
    }
    let shown: Vec<(i64, bool)> = loaded
        .into_iter()
        .map(|view| (view, view_registry::is_shown(view)))
        .collect();
    if let Err(e) = apply_shown(env, &shown) {
        tracing::warn!(error = %e, "webview client: a WebView window visibility change was not sent");
    }
}

fn apply_shown(env: &jni::Env<'_>, shown: &[(i64, bool)]) -> Result<(), ClientError> {
    let mut slot = lock_client()?;
    let spawned = if lock_views()?.restores(shown) {
        env.get_java_vm()
            .map_err(|e| ClientError::Spawn(format!("JavaVM unavailable: {e}")))
            .and_then(|java_vm| ensure_live(&mut slot, java_vm))
    } else {
        Ok(())
    };
    let batch = {
        let mut views = lock_views()?;
        let batch = views.apply_shown(shown, spawned.is_ok());
        views.publish();
        batch
    };
    spawned?;
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

struct HelperReply<T: 'static> {
    request_id: u32,

    waiters: &'static Mutex<BTreeMap<u32, mpsc::Sender<T>>>,

    answer: mpsc::Receiver<T>,
}

impl<T> HelperReply<T> {
    fn forget(&self) {
        if let Ok(mut waiters) = self.waiters.lock() {
            waiters.remove(&self.request_id);
        }
    }

    fn wait(self, what: &'static str, timeout: Duration) -> Result<T, ClientError> {
        match self.answer.recv_timeout(timeout) {
            Ok(value) => Ok(value),
            Err(mpsc::RecvTimeoutError::Timeout) => {
                self.forget();
                Err(ClientError::TimedOut(what))
            }
            Err(mpsc::RecvTimeoutError::Disconnected) => Err(ClientError::Unavailable(
                "the helper exited before answering".to_string(),
            )),
        }
    }
}

fn ask_helper<T>(
    client: &Client,
    waiters: &'static Mutex<BTreeMap<u32, mpsc::Sender<T>>>,
    msg: impl FnOnce(u32) -> ConsumerMsg,
) -> Result<HelperReply<T>, ClientError> {
    let request_id = next_request_id();
    let frame = encode(&msg(request_id))?;
    let (tx, answer) = mpsc::channel::<T>();
    waiters
        .lock()
        .map_err(|_| ClientError::Internal("cookie waiters lock poisoned"))?
        .insert(request_id, tx);
    let reply = HelperReply {
        request_id,
        waiters,
        answer,
    };
    if let Err(e) = client.send(&[frame]) {
        reply.forget();
        return Err(e);
    }
    Ok(reply)
}

pub fn cookie_get(url: String) -> Result<Vec<CookiePair>, ClientError> {
    let slot = lock_client()?;
    let from_jar = with_cookie_store(|store| {
        store
            .answers()
            .then(|| store.jar.matching(&url, SystemTime::now()))
    })?;
    if let Some(cookies) = from_jar {
        return Ok(cookies);
    }
    let reply = ask_helper(slot.live()?, &COOKIE_GETS, |request_id| {
        ConsumerMsg::CookieGet { request_id, url }
    })?;
    drop(slot);
    reply.wait("CookieManager.getCookie", COOKIE_GET_TIMEOUT)
}

pub fn cookie_set(
    request_id: u32,
    url: String,
    header: String,
) -> Result<CookieAnswer, ClientError> {
    let slot = lock_client()?;
    if let Some(ok) = with_cookie_store(|store| store.answers().then(|| store.set(&url, &header)))?
    {
        return Ok(CookieAnswer::Now(ok));
    }
    send_owing_reply(
        &slot,
        request_id,
        encode(&ConsumerMsg::CookieSet {
            request_id,
            url,
            header,
        })?,
    )?;
    Ok(CookieAnswer::FromHelper)
}

pub fn cookies_clear(request_id: u32, scope: ClearScope) -> Result<CookieAnswer, ClientError> {
    let slot = lock_client()?;
    if let Some(removed) = with_cookie_store(|store| store.answers().then(|| store.clear(scope)))? {
        return Ok(CookieAnswer::Now(removed));
    }
    send_owing_reply(
        &slot,
        request_id,
        encode(&ConsumerMsg::CookiesClear { request_id, scope })?,
    )?;
    Ok(CookieAnswer::FromHelper)
}

pub fn cookie_flush() -> Result<bool, ClientError> {
    let pending = {
        let slot = lock_client()?;
        if with_cookie_store(|store| store.answers())? {
            None
        } else {
            Some(ask_helper(slot.live()?, &COOKIE_FLUSHES, |request_id| {
                ConsumerMsg::CookieFlush { request_id }
            })?)
        }
    };
    let helper_saved = pending
        .map(|reply| reply.wait("CookieManager.flush", COOKIE_FLUSH_TIMEOUT))
        .transpose();
    let jar_saved = save_cookie_jar()?;
    Ok(helper_saved?.unwrap_or(true) && jar_saved)
}

fn save_cookie_jar() -> Result<bool, ClientError> {
    let _writer = JAR_WRITE
        .lock()
        .map_err(|_| ClientError::Internal("cookie jar writer lock poisoned"))?;
    let Some((file, bytes)) =
        with_cookie_store(CookieStore::take_unsaved)?.map_err(ClientError::Encode)?
    else {
        return Ok(true);
    };
    match cookie_jar::write_file(&file, &bytes) {
        Ok(()) => Ok(true),
        Err(error) => {
            tracing::error!(
                jar = %file.display(),
                %error,
                "cannot save the WebView cookie jar; the next flush tries again"
            );
            with_cookie_store(|store| store.dirty = true)?;
            Ok(false)
        }
    }
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

pub fn helper_running() -> bool {
    CLIENT
        .lock()
        .is_ok_and(|slot| matches!(&*slot, ClientSlot::Live(_)))
}

pub fn needs_cookie_flush_before_shutdown() -> bool {
    COOKIES.lock().is_ok_and(|store| {
        store
            .as_ref()
            .is_some_and(|store| store.dirty || store.owner != CookieOwner::Jar)
    })
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
        views.helper = None;
        views.publish();
    }
    let Some(mut client) = taken else {
        return ShutdownReport {
            helper_exit: None,
            reader_joined: false,
        };
    };
    return_cookies_to_the_jar(client.generation);
    let exit = stop_helper_process(&mut client, deadline).and_then(|status| status.code());
    let reader_joined = client.io.join().is_ok();
    let _ = client.idle.join();
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
            Routed::Snapshot(_) => panic!("expected an upcall, got a cookie snapshot"),
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
        let generation = HelperGeneration::next();
        let dir = install_store("helper-exit", CookieOwner::Helper(generation));
        let (host_end, helper_end) = UnixStream::pair().expect("socketpair");
        *CLIENT.lock().expect("client") = ClientSlot::Live(stand_in_client(&host_end, generation));
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
        let reader = std::thread::spawn(move || reader_loop(&host_end, &tx, generation));
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
        let owner = cookie_owner();
        remove_store(&dir);

        assert!(
            matches!(gone, Ok(Upcall::HelperGone { visible_views: 1 })),
            "exactly the one shown view gets an Android Back"
        );
        assert!(restarting && hidden && uncreated);
        assert!(respawnable, "the next WebView load starts a new helper");
        assert_eq!(
            owner,
            Some(CookieOwner::Jar),
            "the jar answers cookie calls again once the helper is gone"
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

    fn stand_in_client(writer: &UnixStream, generation: HelperGeneration) -> Client {
        Client {
            child: std::process::Command::new("/bin/sh")
                .args(["-c", "exit 0"])
                .spawn()
                .expect("spawn a stand-in helper"),
            writer: writer.try_clone().expect("writer"),
            io: std::thread::spawn(|| {}),
            upcall: std::thread::spawn(|| {}),
            idle: std::thread::spawn(|| {}),
            generation,
        }
    }

    fn install_store(tag: &str, owner: CookieOwner) -> PathBuf {
        let dir = temp_dir(tag);
        *COOKIES.lock().expect("cookies") = Some(CookieStore {
            jar: CookieJar::default(),
            file: dir.join(cookie_jar::COOKIE_JAR_FILE),
            owner,
            dirty: false,
        });
        dir
    }

    fn remove_store(dir: &Path) {
        *COOKIES.lock().expect("cookies") = None;
        let _ = std::fs::remove_dir_all(dir);
    }

    fn cookie_owner() -> Option<CookieOwner> {
        COOKIES
            .lock()
            .expect("cookies")
            .as_ref()
            .map(|store| store.owner)
    }

    fn reply(stream: &UnixStream, msg: HelperMsg) {
        let frame = msg.encode().expect("encode the reply");
        (&mut &*stream).write_all(&frame).expect("write the reply");
    }

    fn names(cookies: &[CookiePair]) -> Vec<&str> {
        cookies.iter().map(|cookie| cookie.name.as_str()).collect()
    }

    const PAGE: &str = "https://www.roblox.com/home";

    #[test]
    fn cookie_calls_answer_from_the_jar_and_never_start_the_helper() {
        use std::os::unix::fs::MetadataExt as _;

        let _serial = HELPER_STATE_TEST_LOCK
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        *CLIENT.lock().expect("client") = ClientSlot::Unspawned;
        let dir = install_store("jar-answers", CookieOwner::Jar);
        let file = dir.join(cookie_jar::COOKIE_JAR_FILE);

        let set = cookie_set(1, PAGE.into(), "a=1; Domain=roblox.com; Max-Age=600".into());
        let refused = cookie_set(2, PAGE.into(), "b=1; Domain=example.com".into());
        let session = cookie_set(3, PAGE.into(), "s=1".into());
        let got = cookie_get(PAGE.into()).expect("getCookie");
        let needs_flush = needs_cookie_flush_before_shutdown();
        let flushed = cookie_flush();
        let saved = cookie_jar::read_file(&file, SystemTime::now()).expect("read the jar");
        let first = std::fs::metadata(&file).expect("the saved jar");
        std::thread::sleep(Duration::from_millis(20));
        let unchanged_flush = cookie_flush();
        let second = std::fs::metadata(&file).expect("the saved jar");
        let cleared = cookies_clear(4, ClearScope::Session);
        let cleared_again = cookies_clear(5, ClearScope::Session);
        let after_clear = cookie_get(PAGE.into()).expect("getCookie");
        let unspawned = matches!(*CLIENT.lock().expect("client"), ClientSlot::Unspawned);
        remove_store(&dir);

        assert_eq!(set, Ok(CookieAnswer::Now(true)));
        assert_eq!(refused, Ok(CookieAnswer::Now(false)));
        assert_eq!(session, Ok(CookieAnswer::Now(true)));
        assert_eq!(names(&got), ["a", "s"]);
        assert!(needs_flush, "unsaved cookies must be flushed at exit");
        assert_eq!(flushed, Ok(true));
        let saved = saved.expect("the flush wrote the jar");
        assert_eq!(names(&saved.matching(PAGE, SystemTime::now())), ["a", "s"]);
        assert_eq!(unchanged_flush, Ok(true));
        assert_eq!(
            (second.ino(), second.mtime(), second.mtime_nsec()),
            (first.ino(), first.mtime(), first.mtime_nsec()),
            "a flush with nothing new must not rewrite the jar"
        );
        assert_eq!(cleared, Ok(CookieAnswer::Now(true)));
        assert_eq!(cleared_again, Ok(CookieAnswer::Now(false)));
        assert_eq!(names(&after_clear), ["a"]);
        assert!(unspawned, "no cookie call may start the web engine helper");
    }

    #[test]
    fn cookie_calls_go_to_the_running_helper() {
        let _serial = HELPER_STATE_TEST_LOCK
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        let generation = HelperGeneration::next();
        let dir = install_store("helper-answers", CookieOwner::Helper(generation));
        let (host_end, helper_end) = UnixStream::pair().expect("socketpair");
        *CLIENT.lock().expect("client") = ClientSlot::Live(stand_in_client(&host_end, generation));
        let snapshot = vec![StoredCookie {
            name: "page".into(),
            value: "1".into(),
            domain: "www.roblox.com".into(),
            path: "/".into(),
            secure: false,
            http_only: true,
            same_site: proto::SameSite::Lax,
            expiry: proto::CookieExpiry::Session,
        }];
        let helper_snapshot = snapshot.clone();
        let helper = std::thread::spawn(move || {
            let mut seen = Vec::new();
            for _ in 0..3 {
                let msg = proto::read_consumer_msg(&mut &helper_end).expect("a cookie request");
                match &msg {
                    ConsumerMsg::CookieGet { request_id, .. } => reply(
                        &helper_end,
                        HelperMsg::CookieList {
                            request_id: *request_id,
                            cookies: vec![CookiePair {
                                name: "page".into(),
                                value: "1".into(),
                            }],
                        },
                    ),
                    ConsumerMsg::CookieFlush { request_id } => {
                        reply(
                            &helper_end,
                            HelperMsg::CookieSnapshot {
                                cookies: helper_snapshot.clone(),
                            },
                        );
                        reply(
                            &helper_end,
                            HelperMsg::CookieFlushed {
                                request_id: *request_id,
                                ok: true,
                            },
                        );
                    }
                    _ => {}
                }
                seen.push(msg);
            }
            seen
        });
        let (tx, _rx) = mpsc::channel();
        let reader = std::thread::spawn(move || reader_loop(&host_end, &tx, generation));

        let set = cookie_set(7, PAGE.into(), "x=1".into());
        let got = cookie_get(PAGE.into());
        let flushed = cookie_flush();
        let seen = helper.join().expect("fake helper");
        reader.join().expect("reader");
        let saved = COOKIES
            .lock()
            .expect("cookies")
            .as_ref()
            .map(|store| (store.jar.all_unexpired(SystemTime::now()), store.dirty));
        let file = dir.join(cookie_jar::COOKIE_JAR_FILE);
        let on_disk = cookie_jar::read_file(&file, SystemTime::now()).expect("read the jar");
        let owner = cookie_owner();
        *CLIENT.lock().expect("client") = ClientSlot::Unspawned;
        UNEXPECTED_EXITS.store(0, Ordering::SeqCst);
        remove_store(&dir);

        assert_eq!(set, Ok(CookieAnswer::FromHelper));
        assert_eq!(
            got.map(|cookies| names(&cookies).join(",")),
            Ok("page".into())
        );
        assert_eq!(flushed, Ok(true));
        assert!(matches!(
            seen.as_slice(),
            [
                ConsumerMsg::CookieSet { request_id: 7, .. },
                ConsumerMsg::CookieGet { .. },
                ConsumerMsg::CookieFlush { .. }
            ]
        ));
        assert_eq!(
            saved,
            Some((snapshot.clone(), false)),
            "the flush installs the helper's snapshot and saves it"
        );
        assert_eq!(
            on_disk.map(|jar| jar.all_unexpired(SystemTime::now())),
            Some(snapshot)
        );
        assert_eq!(
            owner,
            Some(CookieOwner::Jar),
            "the helper's exit returns the cookies"
        );
    }

    #[test]
    fn a_snapshot_from_a_retired_helper_is_ignored() {
        let _serial = HELPER_STATE_TEST_LOCK
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        let retired = HelperGeneration::next();
        let current = HelperGeneration::next();
        let dir = install_store("stale-snapshot", CookieOwner::Helper(current));
        let cookie = |name: &str| StoredCookie {
            name: name.into(),
            value: "1".into(),
            domain: "www.roblox.com".into(),
            path: "/".into(),
            secure: false,
            http_only: false,
            same_site: proto::SameSite::Lax,
            expiry: proto::CookieExpiry::Session,
        };
        let jar_state = || {
            COOKIES.lock().expect("cookies").as_ref().map(|store| {
                (
                    names(&store.jar.matching(PAGE, SystemTime::now())).join(","),
                    store.dirty,
                )
            })
        };
        install_snapshot(retired, vec![cookie("stale")]);
        let after_retired = jar_state();
        install_snapshot(current, vec![cookie("fresh")]);
        let after_current = jar_state();
        return_cookies_to_the_jar(retired);
        let owner_after_retired = cookie_owner();
        return_cookies_to_the_jar(current);
        let owner_after_current = cookie_owner();
        remove_store(&dir);

        assert_eq!(after_retired, Some((String::new(), false)));
        assert_eq!(after_current, Some(("fresh".to_string(), true)));
        assert_eq!(owner_after_retired, Some(CookieOwner::Helper(current)));
        assert_eq!(owner_after_current, Some(CookieOwner::Jar));
    }

    #[test]
    fn a_new_helper_gets_the_jar_before_anything_else() {
        let _serial = HELPER_STATE_TEST_LOCK
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        let dir = install_store("hand-over", CookieOwner::Jar);
        with_cookie_store(|store| {
            store.set(PAGE, "kept=1; Max-Age=600");
            store.set(PAGE, "gone=1; Max-Age=0");
            store.set(PAGE, "session=1");
        })
        .expect("fill the jar");
        let (client_end, helper_end) = UnixStream::pair().expect("pair");
        let (answered_tx, answered_rx) = mpsc::channel::<Instant>();
        let helper = std::thread::spawn(move || {
            let hello = proto::read_consumer_msg(&mut &helper_end).expect("Hello");
            reply(
                &helper_end,
                HelperMsg::HelloAck {
                    version: PROTO_VERSION,
                    engine: "webkitgtk/test".into(),
                },
            );
            let import = proto::read_consumer_msg(&mut &helper_end).expect("CookieImport");
            let ConsumerMsg::CookieImport {
                request_id,
                cookies,
            } = &import
            else {
                return (hello, import);
            };
            reply(
                &helper_end,
                HelperMsg::CookieSnapshot {
                    cookies: Vec::new(),
                },
            );
            std::thread::sleep(Duration::from_millis(100));
            answered_tx.send(Instant::now()).expect("note the answer");
            reply(
                &helper_end,
                HelperMsg::CookieImportResult {
                    request_id: *request_id,
                    imported: u32::try_from(cookies.len()).expect("count"),
                    failed: 0,
                },
            );
            (hello, import)
        });
        let started = perform_handshake(&client_end, Duration::from_secs(2))
            .and_then(|_| hand_over_cookies(&client_end, Duration::from_secs(2)));
        let returned = Instant::now();
        let (hello, import) = helper.join().expect("fake helper");
        let answered = answered_rx.recv().expect("the helper answered");
        let owner = cookie_owner();
        remove_store(&dir);

        assert_eq!(started, Ok(()));
        assert_eq!(
            hello,
            ConsumerMsg::Hello {
                version: PROTO_VERSION
            }
        );
        match import {
            ConsumerMsg::CookieImport { cookies, .. } => assert_eq!(
                cookies
                    .iter()
                    .map(|cookie| cookie.name.as_str())
                    .collect::<Vec<_>>(),
                ["kept", "session"],
                "the import carries every unexpired cookie, session ones included"
            ),
            other => panic!("the jar must follow the handshake, got {other:?}"),
        }
        assert!(
            returned >= answered,
            "the start must wait for the import, so no view is created before it"
        );
        assert_eq!(
            client_end.read_timeout().expect("read timeout"),
            None,
            "the reader thread blocks without a timeout"
        );
        assert_eq!(
            owner,
            Some(CookieOwner::Jar),
            "the spawner hands the cookies to the helper only once it is live"
        );
    }

    #[test]
    fn a_helper_that_never_takes_the_jar_fails_its_start() {
        let _serial = HELPER_STATE_TEST_LOCK
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        let dir = install_store("import-silent", CookieOwner::Jar);
        let (client_end, helper_end) = UnixStream::pair().expect("pair");
        let silent = hand_over_cookies(&client_end, Duration::from_millis(50));
        let (client_end, fatal_end) = UnixStream::pair().expect("pair");
        reply(
            &fatal_end,
            HelperMsg::Fatal {
                reason: "the cookie store is gone".into(),
            },
        );
        let fatal = hand_over_cookies(&client_end, Duration::from_secs(2));
        drop(helper_end);
        remove_store(&dir);

        assert!(matches!(
            silent,
            Err(ClientError::TimedOut("the cookie import"))
        ));
        assert!(matches!(
            fatal,
            Err(ClientError::Unavailable(reason)) if reason == "the cookie store is gone"
        ));
    }

    fn jar_location(tag: &str) -> (PathBuf, PathBuf, PathBuf) {
        let root = temp_dir(tag);
        let data = root.join("app-data/webview");
        std::fs::create_dir_all(&data).expect("create the jar dir");
        let profile = root.join("app-data").join(CEF_PROFILE_DIR);
        (root, data.join(cookie_jar::COOKIE_JAR_FILE), profile)
    }

    #[test]
    fn a_corrupt_jar_is_moved_aside_and_never_overwritten() {
        let (root, file, profile) = jar_location("corrupt-jar");
        std::fs::write(&file, [9, 0, 1, 2]).expect("damage the jar");
        let store = CookieStore::load(file.clone(), &profile, SystemTime::now()).expect("load");
        assert!(store.jar.all_unexpired(SystemTime::now()).is_empty());
        assert!(!store.dirty);
        assert!(!file.exists());
        assert_eq!(
            std::fs::read(file.with_extension("corrupt")).expect("the damaged jar is kept"),
            [9, 0, 1, 2]
        );
        let _ = std::fs::remove_dir_all(&root);
    }

    #[test]
    fn a_jar_that_cannot_be_read_stays_in_place_and_loads_once_it_can() {
        let (root, file, profile) = jar_location("unreadable-jar");
        std::fs::create_dir(&file).expect("put a directory where the jar is");
        let unreadable = CookieStore::load(file.clone(), &profile, SystemTime::now());
        let kept = file.is_dir();
        let moved_aside = file.with_extension("corrupt").exists();
        std::fs::remove_dir(&file).expect("remove the directory");
        let mut jar = CookieJar::default();
        jar.set_from_header(PAGE, "a=1; Max-Age=600", SystemTime::now())
            .expect("a cookie");
        cookie_jar::write_file(&file, &jar.encode().expect("encode")).expect("write the jar");
        let readable = CookieStore::load(file, &profile, SystemTime::now());
        let _ = std::fs::remove_dir_all(&root);

        assert!(matches!(unreadable, Err(ClientError::Storage(_))));
        assert!(kept, "a jar that cannot be read must stay where it is");
        assert!(!moved_aside, "only a corrupt jar is moved aside");
        let readable = readable.expect("the jar loads once it can be read");
        assert_eq!(
            names(&readable.jar.matching(PAGE, SystemTime::now())),
            ["a"]
        );
    }

    #[test]
    fn a_cef_profile_moves_into_the_jar_and_is_then_removed() {
        let (root, file, profile) = jar_location("cef-migrated");
        cef_profile::install_fixture(&profile);
        let now = cef_profile::fixture_written_at();
        let store = CookieStore::load(file.clone(), &profile, now).expect("load");
        let cookies = store.jar.all_unexpired(now);
        let saved = cookie_jar::read_file(&file, now).expect("read the jar");
        let _ = std::fs::remove_dir_all(&root);

        assert_eq!(cookies.len(), 12);
        let login = cookies
            .iter()
            .find(|cookie| cookie.name == ".ROBLOSECURITY")
            .expect("the login cookie is migrated");
        assert_eq!(
            login.value,
            "synthetic-roblosecurity-for-the-eclipse-cef-migration-test"
        );
        assert_eq!(saved, Some(store.jar));
        assert!(!profile.exists(), "a migrated CEF profile must be removed");
    }

    #[test]
    fn a_cef_profile_stays_unless_its_cookies_reach_the_jar_file() {
        let (root, file, profile) = jar_location("cef-unsaved");
        cef_profile::install_fixture(&profile);
        let now = cef_profile::fixture_written_at();
        let unwritable = root.join("missing-dir").join(cookie_jar::COOKIE_JAR_FILE);
        let store = CookieStore::load(unwritable, &profile, now).expect("load");
        assert_eq!(store.jar.all_unexpired(now).len(), 12);
        assert!(
            profile.exists(),
            "the profile stays when the jar cannot be saved"
        );

        std::fs::write(profile.join(cef_profile::COOKIE_DATABASE), [0x5Au8; 4096])
            .expect("damage the cookie database");
        let store = CookieStore::load(file.clone(), &profile, now).expect("load");
        assert!(store.jar.all_unexpired(now).is_empty());
        assert!(profile.exists(), "an unreadable profile stays");
        assert!(!file.exists(), "nothing is saved for an unreadable profile");

        cef_profile::install_fixture(&profile);
        cookie_jar::write_file(&file, &CookieJar::default().encode().expect("encode"))
            .expect("an existing jar");
        let store = CookieStore::load(file, &profile, now).expect("load");
        assert!(store.jar.all_unexpired(now).is_empty());
        assert!(
            profile.exists(),
            "a CEF profile beside an existing jar is kept, so old cookies never replace newer ones"
        );
        let _ = std::fs::remove_dir_all(&root);
    }

    #[test]
    fn the_idle_grace_runs_from_the_last_load_or_shown_window() {
        let grace = Duration::from_secs(30);
        let t0 = Instant::now();
        let at = |secs: u64| t0 + Duration::from_secs(secs);
        let mut timer = IdleTimer::default();
        assert_eq!(timer.next(false, 1, at(0), grace), IdleWait::UntilChange);
        assert_eq!(timer.next(true, 1, at(1), grace), IdleWait::For(grace));
        assert_eq!(
            timer.next(true, 1, at(11), grace),
            IdleWait::For(Duration::from_secs(20))
        );
        assert_eq!(
            timer.next(true, 2, at(21), grace),
            IdleWait::For(grace),
            "a load inside the grace starts it again"
        );
        assert_eq!(
            timer.next(true, 2, at(50), grace),
            IdleWait::For(Duration::from_secs(1))
        );
        assert_eq!(
            timer.next(true, 2, at(51), grace),
            IdleWait::Elapsed { activity: 2 }
        );
        assert_eq!(timer.next(false, 2, at(52), grace), IdleWait::UntilChange);
        assert_eq!(
            timer.next(true, 2, at(53), grace),
            IdleWait::For(grace),
            "a window shown and hidden again starts the grace again"
        );
    }

    fn page_cookie() -> StoredCookie {
        StoredCookie {
            name: "page".into(),
            value: "1".into(),
            domain: "www.roblox.com".into(),
            path: "/".into(),
            secure: false,
            http_only: false,
            same_site: proto::SameSite::Lax,
            expiry: proto::CookieExpiry::Session,
        }
    }

    fn running_helper(generation: HelperGeneration) -> (Client, UnixStream) {
        let (host_end, helper_end) = UnixStream::pair().expect("socketpair");
        let reader_end = host_end.try_clone().expect("reader end");
        let (up_tx, up_rx) = mpsc::channel();
        let io = std::thread::spawn(move || reader_loop(&reader_end, &up_tx, generation));
        let upcall = std::thread::spawn(move || while up_rx.recv().is_ok() {});
        let client = Client {
            io,
            upcall,
            ..stand_in_client(&host_end, generation)
        };
        (client, helper_end)
    }

    fn answer_until_shutdown(
        helper_end: UnixStream,
        snapshot: Vec<StoredCookie>,
    ) -> Vec<ConsumerMsg> {
        let mut seen = Vec::new();
        loop {
            let msg = proto::read_consumer_msg(&mut &helper_end).expect("a host message");
            if let ConsumerMsg::CookieFlush { request_id } = &msg {
                reply(
                    &helper_end,
                    HelperMsg::CookieSnapshot {
                        cookies: snapshot.clone(),
                    },
                );
                reply(
                    &helper_end,
                    HelperMsg::CookieFlushed {
                        request_id: *request_id,
                        ok: true,
                    },
                );
            }
            let shutdown = msg == ConsumerMsg::Shutdown;
            seen.push(msg);
            if shutdown {
                return seen;
            }
        }
    }

    fn hidden_page(view: i64) -> ViewEntry {
        let mut entry = ViewEntry::default();
        entry.load_batch(view, false, load(view));
        entry
    }

    #[test]
    fn an_idle_helper_hands_back_its_cookies_then_shuts_down() {
        let _serial = HELPER_STATE_TEST_LOCK
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        UNEXPECTED_EXITS.store(0, Ordering::SeqCst);
        let generation = HelperGeneration::next();
        let dir = install_store("idle-stop", CookieOwner::Helper(generation));
        let (client, helper_end) = running_helper(generation);
        *CLIENT.lock().expect("client") = ClientSlot::Live(client);
        let view = registry_view(false);
        {
            let mut views = VIEWS.lock().expect("views");
            views.entries.insert(view, hidden_page(view));
            views.entries.get_mut(&view).expect("entry").created = true;
            views.adopt(generation);
        }
        let helper =
            std::thread::spawn(move || answer_until_shutdown(helper_end, vec![page_cookie()]));

        watch_idle(generation, Duration::from_millis(50));
        let seen = helper.join().expect("stand-in helper");
        let unspawned = matches!(*CLIENT.lock().expect("client"), ClientSlot::Unspawned);
        let owner = cookie_owner();
        let from_jar = cookie_get(PAGE.into());
        let still_unspawned = matches!(*CLIENT.lock().expect("client"), ClientSlot::Unspawned);
        let saved =
            cookie_jar::read_file(&dir.join(cookie_jar::COOKIE_JAR_FILE), SystemTime::now())
                .expect("read the jar");
        let (helper_left, entry_kept) = {
            let mut views = VIEWS.lock().expect("views");
            let helper_left = views.helper;
            let entry = views.entries.remove(&view).expect("the view stays tracked");
            views.publish();
            (
                helper_left,
                !entry.created && entry.last_load == Some(load(view)),
            )
        };
        free_registry_views(&[view]);
        let exits = UNEXPECTED_EXITS.load(Ordering::SeqCst);
        remove_store(&dir);

        assert!(
            matches!(
                seen.as_slice(),
                [ConsumerMsg::CookieFlush { .. }, ConsumerMsg::Shutdown]
            ),
            "the helper is asked for its cookies, then shut down: {seen:?}"
        );
        assert!(unspawned && still_unspawned);
        assert_eq!(exits, 0, "an idle stop is no unexpected exit");
        assert_eq!(owner, Some(CookieOwner::Jar));
        assert_eq!(
            from_jar.map(|cookies| names(&cookies).join(",")),
            Ok("page".to_string()),
            "the jar answers with the helper's last cookies"
        );
        assert_eq!(
            saved.map(|jar| jar.all_unexpired(SystemTime::now())),
            Some(vec![page_cookie()]),
            "the stop saves the cookies it took back"
        );
        assert_eq!(helper_left, None);
        assert!(entry_kept, "the hidden view keeps the page it shows again");
    }

    #[test]
    fn a_load_or_a_shown_window_inside_the_grace_keeps_the_helper() {
        let _serial = HELPER_STATE_TEST_LOCK
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        let generation = HelperGeneration::next();
        let dir = install_store("idle-busy", CookieOwner::Helper(generation));
        let (host_end, helper_end) = UnixStream::pair().expect("socketpair");
        *CLIENT.lock().expect("client") = ClientSlot::Live(stand_in_client(&host_end, generation));
        let view = registry_view(true);
        let quiet = {
            let mut views = VIEWS.lock().expect("views");
            views.entries.insert(view, hidden_page(view));
            views.adopt(generation);
            views.activity
        };
        VIEWS.lock().expect("views").note_activity();
        let loaded = stop_idle_helper(generation, quiet);
        let shown = {
            let mut views = VIEWS.lock().expect("views");
            let entry = views.entries.get_mut(&view).expect("entry");
            entry.created = true;
            entry.shown = true;
            let activity = views.activity;
            drop(views);
            stop_idle_helper(generation, activity)
        };
        let retired = stop_idle_helper(HelperGeneration::next(), quiet);
        helper_end.set_nonblocking(true).expect("nonblocking");
        let sent = proto::read_consumer_msg(&mut &helper_end);
        let live = matches!(&*CLIENT.lock().expect("client"), ClientSlot::Live(client) if client.generation == generation);
        {
            let mut views = VIEWS.lock().expect("views");
            views.entries.remove(&view);
            views.helper = None;
            views.publish();
        }
        free_registry_views(&[view]);
        *CLIENT.lock().expect("client") = ClientSlot::Unspawned;
        remove_store(&dir);

        assert_eq!(loaded, Ok(IdleStop::Busy), "a load after the grace began");
        assert_eq!(shown, Ok(IdleStop::Busy), "a shown window");
        assert_eq!(retired, Ok(IdleStop::Retired), "another helper is live");
        assert!(
            matches!(
                sent,
                Err(proto::ProtoError::Io(std::io::ErrorKind::WouldBlock))
            ),
            "nothing reaches a helper that stays: {sent:?}"
        );
        assert!(live);
    }

    #[test]
    fn eof_from_a_retired_helper_leaves_the_newer_helper_alone() {
        let _serial = HELPER_STATE_TEST_LOCK
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        UNEXPECTED_EXITS.store(0, Ordering::SeqCst);
        let retired = HelperGeneration::next();
        let current = HelperGeneration::next();
        let dir = install_store("retired-eof", CookieOwner::Helper(current));
        let (host_end, _helper_end) = UnixStream::pair().expect("socketpair");
        *CLIENT.lock().expect("client") = ClientSlot::Live(stand_in_client(&host_end, current));
        let view = registry_view(true);
        {
            let mut views = VIEWS.lock().expect("views");
            views.entries.insert(view, created_entry(true));
            views.adopt(current);
        }

        let (old_host, old_helper) = UnixStream::pair().expect("socketpair");
        drop(old_helper);
        let (tx, rx) = mpsc::channel();
        reader_loop(&old_host, &tx, retired);

        let live = matches!(&*CLIENT.lock().expect("client"), ClientSlot::Live(client) if client.generation == current);
        let owner = cookie_owner();
        let (window_kept, helper_kept) = {
            let mut views = VIEWS.lock().expect("views");
            let kept = (views.entries[&view].window_visible(), views.helper);
            views.entries.remove(&view);
            views.helper = None;
            views.publish();
            kept
        };
        free_registry_views(&[view]);
        let exits = UNEXPECTED_EXITS.load(Ordering::SeqCst);
        *CLIENT.lock().expect("client") = ClientSlot::Unspawned;
        remove_store(&dir);

        assert!(live, "the newer helper keeps running");
        assert_eq!(exits, 0);
        assert_eq!(owner, Some(CookieOwner::Helper(current)));
        assert!(window_kept);
        assert_eq!(helper_kept, Some(current));
        assert!(rx.try_recv().is_err(), "no Back reaches Android");
    }

    #[test]
    fn a_retired_helpers_replies_never_reach_a_newer_one() {
        let _serial = HELPER_STATE_TEST_LOCK
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        let retired = HelperGeneration::next();
        let current = HelperGeneration::next();
        let (host_end, helper_end) = UnixStream::pair().expect("socketpair");
        *CLIENT.lock().expect("client") = ClientSlot::Live(stand_in_client(&host_end, current));
        let reply_to = |generation, policy_id| {
            send_reply(
                generation,
                &ConsumerMsg::PolicyReply {
                    policy_id,
                    override_load: false,
                },
            );
        };
        reply_to(retired, 5);
        reply_to(current, 6);
        let delivered = proto::read_consumer_msg(&mut &helper_end);
        *CLIENT.lock().expect("client") = ClientSlot::Unspawned;

        assert_eq!(
            delivered,
            Ok(ConsumerMsg::PolicyReply {
                policy_id: 6,
                override_load: false
            }),
            "only the live helper's own reply arrives"
        );
    }

    #[test]
    fn a_retired_helper_fails_only_the_callbacks_it_still_owes() {
        let _serial = HELPER_STATE_TEST_LOCK
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        UNEXPECTED_EXITS.store(0, Ordering::SeqCst);
        let retired = HelperGeneration::next();
        let current = HelperGeneration::next();
        let dir = install_store("owed-replies", CookieOwner::Helper(retired));
        let (old_host, old_helper) = UnixStream::pair().expect("socketpair");
        *CLIENT.lock().expect("client") = ClientSlot::Live(stand_in_client(&old_host, retired));
        let answered = next_request_id();
        let owed = next_request_id();
        let to_retired = [
            cookie_set(answered, PAGE.into(), "a=1".into()),
            cookie_set(owed, PAGE.into(), "b=1".into()),
        ];

        with_cookie_store(|store| store.owner = CookieOwner::Helper(current)).expect("store");
        let (new_host, _new_helper) = UnixStream::pair().expect("socketpair");
        *CLIENT.lock().expect("client") = ClientSlot::Live(stand_in_client(&new_host, current));
        let newer = next_request_id();
        let to_current = cookies_clear(newer, ClearScope::All);

        reply(
            &old_helper,
            HelperMsg::CookieSetResult {
                request_id: answered,
                ok: true,
            },
        );
        drop(old_helper);
        let (tx, rx) = mpsc::channel();
        reader_loop(&old_host, &tx, retired);
        let failed = replies_still_owed(retired);
        let still_owed = replies_still_owed(current);
        let live = matches!(&*CLIENT.lock().expect("client"), ClientSlot::Live(client) if client.generation == current);
        *CLIENT.lock().expect("client") = ClientSlot::Unspawned;
        remove_store(&dir);

        assert_eq!(
            to_retired,
            [Ok(CookieAnswer::FromHelper), Ok(CookieAnswer::FromHelper)]
        );
        assert_eq!(to_current, Ok(CookieAnswer::FromHelper));
        assert!(matches!(
            rx.try_recv(),
            Ok(Upcall::CookieSetResult { request_id, ok: true }) if request_id == answered
        ));
        assert_eq!(
            failed,
            [owed],
            "the retired helper fails only what it never answered"
        );
        assert_eq!(
            still_owed,
            [newer],
            "the newer helper's callback stays pending until that helper answers it"
        );
        assert!(live, "the newer helper keeps running");
    }

    #[test]
    fn showing_a_view_the_helper_lost_recreates_it_with_its_last_page() {
        let view = 70;
        let mut views = Views::new();
        let mut entry = ViewEntry {
            user_agent: Some(ROBLOX_UA.to_string()),
            ..ViewEntry::default()
        };
        entry.bridges.insert(
            "__globalRobloxAndroidBridge__".into(),
            vec!["executeRoblox".into()],
        );
        entry.load_batch(view, false, load(view));
        views.entries.insert(view, entry);
        let page = "https://www.roblox.com/games/1818";
        route(
            HelperMsg::NavigationState {
                view,
                url: page.into(),
                title: "Servers".into(),
                can_go_back: true,
            },
            &mut views,
        );
        views.reset_after_helper_loss();
        let before = views.activity;
        let shown = [(view, true)];

        assert!(views.restores(&shown));
        assert!(!views.restores(&[(view, false)]));
        assert_eq!(
            views.apply_shown(&shown, true),
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
                ConsumerMsg::LoadUrl {
                    view,
                    url: page.into(),
                },
            ]
        );
        assert_ne!(views.activity, before, "a shown window restarts the grace");
        assert!(views.entries[&view].window_visible());
        assert!(views.apply_shown(&shown, true).is_empty(), "shown once");

        views.reset_after_helper_loss();
        views.apply_shown(&[(view, false)], true);
        assert!(
            views.apply_shown(&shown, false).is_empty(),
            "no window without a helper"
        );
        assert!(
            !views.restores(&shown),
            "a failed start is not retried while the view stays shown"
        );
    }

    #[test]
    fn the_page_to_restore_follows_navigation_but_keeps_loaded_data() {
        let view = 71;
        let mut entry = ViewEntry::default();
        entry.note_page(view, "https://www.roblox.com/home");
        assert_eq!(
            entry.last_load,
            Some(ConsumerMsg::LoadUrl {
                view,
                url: "https://www.roblox.com/home".into()
            })
        );
        entry.note_page(view, "about:blank");
        assert_eq!(
            entry.last_load,
            Some(ConsumerMsg::LoadUrl {
                view,
                url: "https://www.roblox.com/home".into()
            }),
            "only web pages can be loaded again"
        );
        let data = ConsumerMsg::LoadData {
            view,
            base_url: "https://www.roblox.com/".into(),
            data: "<p>hi</p>".into(),
            mime: "text/html".into(),
            encoding: String::new(),
        };
        entry.load_batch(view, false, data.clone());
        entry.note_page(view, "https://www.roblox.com/");
        assert_eq!(
            entry.last_load,
            Some(data),
            "loaded data is not replaced by its base URL"
        );
    }
}
