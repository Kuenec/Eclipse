use std::collections::HashMap;
use std::io::Write as _;
use std::os::fd::AsFd as _;
use std::os::unix::net::UnixStream;
use std::path::{Path, PathBuf};
use std::process::Child;
use std::sync::atomic::{AtomicI64, AtomicU32, AtomicUsize, Ordering};
use std::sync::mpsc;
use std::sync::{Arc, Mutex, OnceLock};
use std::thread::JoinHandle;
use std::time::{Duration, Instant};

use super::proto::{self, BridgeMethod, ConsumerMsg, CookieEntry, HelperMsg};
use super::redact;
use super::{fdpass, hostprobe, shm};

pub const HELPER_NOT_FOUND_MARKER: &str = "helper binary not found";

pub const NO_DISPLAY_MARKER: &str = "no display connection";

pub const SANDBOX_UNAVAILABLE_MARKER: &str = "sandbox unavailable";

const HANDSHAKE_TIMEOUT: Duration = Duration::from_secs(10);

const SPAWN_RESULT_TIMEOUT: Duration = Duration::from_secs(15);

#[derive(Debug, Clone)]
pub enum ClientError {
    HelperNotFound { probed: Vec<PathBuf> },

    ExplicitPathMissing { source: &'static str, path: PathBuf },

    Spawn(String),

    Storage(String),

    Handshake(String),

    VersionMismatch { helper_version: u16 },

    Encode(proto::ProtoError),

    Latched(String),

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
                     crates/eclipse-webview (cargo build --release with CEF_PATH set)"
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
                "helper protocol version mismatch: helper v{helper_version}, consumer v{}",
                super::PROTO_VERSION
            ),
            Self::Encode(e) => write!(f, "message rejected before send: {e}"),
            Self::Latched(reason) => write!(f, "web engine helper previously failed: {reason}"),
            Self::Internal(what) => write!(f, "webview client internal error: {what}"),
        }
    }
}

impl std::error::Error for ClientError {}

enum ClientSlot {
    Unspawned(EarlyCookies),

    Live(Client, EarlyCookies),

    Failed(String),
}

const RESPAWN_IN_PROGRESS: &str =
    "the web engine helper is being REPLACED so the User-Agent the app set via \
     WebSettings.setUserAgentString reaches the engine (CefSettings.user_agent is global and \
     consumed by CefInitialize) — this op arrived inside the swap window and degrades honestly \
     rather than being answered from a store that is mid-move (§6 2026-07-16 respawn)";

struct Client {
    child: Child,

    writer: UnixStream,

    reader: Option<JoinHandle<()>>,

    upcall: Option<JoinHandle<()>>,
}

static CLIENT: Mutex<ClientSlot> = Mutex::new(ClientSlot::Unspawned(EarlyCookies::new()));

static ACTIVE_VIEW: AtomicI64 = AtomicI64::new(0);

static LIVE_VIEWS: AtomicUsize = AtomicUsize::new(0);

static NEXT_REQUEST_ID: AtomicU32 = AtomicU32::new(1);

pub fn next_request_id() -> u32 {
    loop {
        let id = NEXT_REQUEST_ID.fetch_add(1, Ordering::Relaxed);
        if id != 0 {
            return id;
        }
    }
}

static APP_USER_AGENT: Mutex<Option<String>> = Mutex::new(None);

static HELPER_UA_FIXED: std::sync::atomic::AtomicBool = std::sync::atomic::AtomicBool::new(false);

fn normalize_app_user_agent(ua: Option<String>) -> Option<String> {
    ua.filter(|s| !s.is_empty())
}

pub fn set_app_user_agent(ua: Option<String>) {
    let ua = normalize_app_user_agent(ua);

    if let Some(t0) = FIRST_DEFER_AT.get() {
        let outstanding = DEFERRED_CB_IDS.lock().map(|ids| ids.len()).unwrap_or(0);
        tracing::warn!(
            target: "android.webkit.WebSettings",
            outstanding,
            elapsed_ms = t0.elapsed().as_millis(),
            "ECLIPSE-DEFER-CB ua-set — the app reached WebSettings.setUserAgentString with \
             {outstanding} probe-deferred setCookie ValueCallback(s) STILL unanswered. If a \
             load-drive follows, the app TOLERATES the deferred reply and the ordering fix \
             completes with no fabrication (§5 2026-07-16 ⏳➜🎲 / ☠️)."
        );
    }

    if HELPER_UA_FIXED.load(Ordering::Relaxed) {
        tracing::warn!(
            target: "android.webkit.WebSettings",
            ua = ua.as_deref().unwrap_or("<reset to default>"),
            "setUserAgentString called AFTER the LIVE helper's engine User-Agent was fixed at its \
             spawn — CefSettings.user_agent is global and consumed by CefInitialize, so THIS engine \
             cannot present it. 2026-07-16 (§6 respawn): that is now RECOVERABLE — if a load-drive \
             follows while no browser exists, the helper is REPLACED with one carrying this string \
             (`maybe_respawn_for_app_ua`, which names its verdict either way). This WARN means the \
             early spawn cost a wasted CefInitialize, NOT that the UA is lost."
        );
    }
    match APP_USER_AGENT.lock() {
        Ok(mut slot) => {
            tracing::info!(
                target: "android.webkit.WebSettings",
                ua = ua.as_deref().unwrap_or("<reset to default>"),
                "the app set its WebView User-Agent via WebSettings.setUserAgentString — Eclipse will \
                 present it (AOSP contract: null/empty resets to the default)"
            );
            *slot = ua;
        }

        Err(_) => tracing::warn!(
            target: "android.webkit.WebSettings",
            "setUserAgentString: the app-UA store is poisoned — Eclipse's fallback UA stands"
        ),
    }
}

pub fn app_user_agent() -> Option<String> {
    APP_USER_AGENT.lock().ok().and_then(|s| s.clone())
}

static HELPER_BOOT_UA: Mutex<Option<String>> = Mutex::new(None);

fn helper_boot_ua() -> Option<String> {
    HELPER_BOOT_UA.lock().ok().and_then(|s| s.clone())
}

fn ua_diag_forced() -> bool {
    static ON: OnceLock<bool> = OnceLock::new();
    *ON.get_or_init(|| std::env::var("ECLIPSE_WEBVIEW_UA_DIAG").is_ok_and(|v| !v.is_empty()))
}

fn defer_cookie_cb_enabled(v: Option<&str>) -> bool {
    v == Some("1")
}

fn defer_cookie_cb() -> bool {
    static ON: OnceLock<bool> = OnceLock::new();
    *ON.get_or_init(|| {
        let raw = std::env::var("ECLIPSE_WEBVIEW_DEFER_COOKIE_CB").ok();
        let on = defer_cookie_cb_enabled(raw.as_deref());
        if on {
            tracing::warn!(
                target: "android.webkit.CookieManager",
                "ECLIPSE-DEFER-CB probe ENABLED (ECLIPSE_WEBVIEW_DEFER_COOKIE_CB=1) — a DEV-HOST \
                 DIAGNOSTIC, never a default boot and never a fix. An early 3-arg setCookie(url, \
                 value, ValueCallback) will now BUFFER like a 2-arg set instead of cold-starting the \
                 helper, and its ValueCallback is held UNANSWERED until the flush replays the \
                 app's ORIGINAL frame to the live engine (the REAL flag then routes back unchanged \
                 — nothing is fabricated, nothing is dropped). AOSP states no deadline for this \
                 callback; whether the APP tolerates the delay is exactly what this measures. If \
                 the app stalls, this boot stalls — that IS the result."
            );
        }
        on
    })
}

static DEFERRED_CB_IDS: Mutex<Vec<u32>> = Mutex::new(Vec::new());

static FIRST_DEFER_AT: OnceLock<Instant> = OnceLock::new();

fn deferred_cb_request_id(msg: &ConsumerMsg) -> Option<u32> {
    match msg {
        ConsumerMsg::CookieSetForResult { request_id, .. } => Some(*request_id),
        _ => None,
    }
}

fn note_deferred_callback(request_id: u32) {
    let _ = FIRST_DEFER_AT.set(Instant::now());
    let outstanding = match DEFERRED_CB_IDS.lock() {
        Ok(mut ids) => {
            ids.push(request_id);
            ids.len()
        }
        Err(_) => 0,
    };
    tracing::warn!(
        target: "android.webkit.CookieManager",
        outstanding,
        "ECLIPSE-DEFER-CB deferred id={request_id} — holding the app's 3-arg setCookie \
         ValueCallback UNANSWERED so this op does not cold-start the helper (and fix the engine's \
         global User-Agent before the app has set its own). The app's ORIGINAL frame is buffered \
         verbatim; the REAL flag is routed at flush. Watch for `ECLIPSE-DEFER-CB ua-set` — if it \
         arrives, the app did NOT block on this callback."
    );
}

fn note_deferred_callback_answered(request_id: u32, ok: bool) {
    if !defer_cookie_cb() {
        return;
    }
    let held = match DEFERRED_CB_IDS.lock() {
        Ok(mut ids) => {
            let before = ids.len();
            ids.retain(|id| *id != request_id);
            before != ids.len()
        }
        Err(_) => false,
    };
    if !held {
        return;
    }
    let waited_ms = FIRST_DEFER_AT
        .get()
        .map(|t0| t0.elapsed().as_millis())
        .unwrap_or_default();
    tracing::warn!(
        target: "android.webkit.CookieManager",
        waited_ms,
        "ECLIPSE-DEFER-CB answered id={request_id} ok={ok} — the ENGINE's REAL success flag \
         reached the app's ValueCallback (not a fabricated one). The deferral cost the app this \
         much wait for its reply and nothing else."
    );
}

#[derive(Debug, PartialEq, Eq)]
enum Deferral {
    Buffer,

    NeedsEngine(&'static str),
}

#[derive(Debug, PartialEq, Eq)]
pub enum SendOutcome {
    Sent,

    Buffered,
}

struct EarlyCookies {
    mutations: Vec<ConsumerMsg>,

    replayable: bool,
}

impl EarlyCookies {
    const CAP: usize = 256;

    const fn new() -> Self {
        Self {
            mutations: Vec::new(),
            replayable: true,
        }
    }

    fn push_mutation(&mut self, msg: &ConsumerMsg) {
        if self.mutations.len() < Self::CAP {
            self.mutations.push(msg.clone());
            return;
        }
        tracing::warn!(
            target: "android.webkit.CookieManager",
            cap = Self::CAP,
            "the webview cookie-mutation log hit its bound — it can no longer reproduce the \
             engine's changes over the persistent store, so the app-UA respawn is now REFUSED \
             for this boot. Honest degradation: the engine keeps the User-Agent it booted with."
        );
        self.replayable = false;
    }

    fn record_sent(&mut self, msg: &ConsumerMsg) {
        if !self.replayable {
            return;
        }
        match msg {
            ConsumerMsg::CookieSet { .. } | ConsumerMsg::CookieSetForResult { .. } => {
                self.push_mutation(msg);
            }

            ConsumerMsg::CookiesClear { .. } | ConsumerMsg::CookiesClearSession { .. } => {
                self.push_mutation(msg);
            }

            ConsumerMsg::CookieGet { .. } | ConsumerMsg::CookieFlush { .. } => {}
            _ => {}
        }
    }

    fn retire(&mut self) {
        self.mutations.clear();
        self.replayable = false;
    }

    #[cfg(test)]
    fn holds_unanswered_callback(&self) -> bool {
        self.mutations
            .iter()
            .any(|m| deferred_cb_request_id(m).is_some())
    }

    fn offer(&mut self, msg: &ConsumerMsg, defer_cb: bool) -> Deferral {
        match msg {

            ConsumerMsg::CookieSet { .. } if self.mutations.len() < Self::CAP => {
                self.mutations.push(msg.clone());
                Deferral::Buffer
            }
            ConsumerMsg::CookieSet { .. } => {
                Deferral::NeedsEngine("the deferred-cookie buffer is full")
            }

            ConsumerMsg::CookieSetForResult { .. }
                if defer_cb && self.mutations.len() < Self::CAP =>
            {
                self.mutations.push(msg.clone());
                Deferral::Buffer
            }
            ConsumerMsg::CookieSetForResult { .. } if defer_cb => {
                Deferral::NeedsEngine("the deferred-cookie buffer is full")
            }
            ConsumerMsg::CookieSetForResult { .. } => Deferral::NeedsEngine(
                "setCookie(url, value, ValueCallback) — only the engine yields the REAL success flag",
            ),
            ConsumerMsg::CookiesClear { .. } => Deferral::NeedsEngine(
                "removeAllCookies — the persistent store may contain prior-boot cookies",
            ),
            ConsumerMsg::CookiesClearSession { .. } => Deferral::NeedsEngine(
                "removeSessionCookies — only CEF can identify cookies without an expiry",
            ),
            ConsumerMsg::CookieGet { .. } => Deferral::NeedsEngine(
                "getCookie — CEF owns the persistent jar and url/domain/path matching",
            ),
            ConsumerMsg::CookieFlush { .. } => Deferral::NeedsEngine(
                "CookieManager.flush — only CEF can confirm persistent-store completion",
            ),
            _ => Deferral::NeedsEngine("an op that needs the engine reached the pre-engine gate"),
        }
    }
}

#[derive(Clone, Copy)]
struct DrawnRect {
    view: i64,
    x: i32,
    y: i32,
    w: u32,
    h: u32,
}

struct Shared {
    views: Mutex<HashMap<i64, ViewShared>>,

    rect: Mutex<Option<(i32, i32, u32, u32)>>,

    screen_rect: Mutex<Option<DrawnRect>>,

    cookie_get_waiters: Mutex<HashMap<u32, mpsc::Sender<Vec<CookieEntry>>>>,

    cookie_flush_waiters: Mutex<HashMap<u32, mpsc::Sender<bool>>>,
}

fn shared() -> &'static Arc<Shared> {
    static SHARED: OnceLock<Arc<Shared>> = OnceLock::new();
    SHARED.get_or_init(|| {
        Arc::new(Shared {
            views: Mutex::new(HashMap::new()),
            rect: Mutex::new(None),
            screen_rect: Mutex::new(None),
            cookie_get_waiters: Mutex::new(HashMap::new()),
            cookie_flush_waiters: Mutex::new(HashMap::new()),
        })
    })
}

struct SendMapping(shm::FrameMapping);

unsafe impl Send for SendMapping {}

struct FrameMap {
    mapping: SendMapping,
    generation: u32,
    width: u16,
    height: u16,
    stride: u32,
    slot_bytes: u32,
}

#[derive(Default)]
pub struct Stage {
    pub bytes: Vec<u8>,
    pub width: u32,
    pub height: u32,

    pub stride: u32,
    pub generation: u32,

    pub seq: u32,
}

struct ViewShared {
    driven_url: String,

    log_target: String,
    mapping: Option<FrameMap>,
    stage: Stage,
    started: bool,
    finished_http: Option<i32>,
    can_go_back: bool,

    upcalls_ok: u32,
}

#[derive(Debug, Clone, Copy)]
pub struct LoadObserved {
    pub started: bool,
    pub finished_http: Option<i32>,
    pub upcalls_ok: u32,
}

#[derive(Debug, Clone, Copy)]
pub struct ShutdownReport {
    pub helper_exit: Option<i32>,
    pub reader_joined: bool,
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

fn resolve_helper() -> Result<PathBuf, ClientError> {
    let config_path = crate::config::Config::load()
        .ok()
        .and_then(|c| c.webview_helper_path)
        .filter(|s| !s.is_empty())
        .map(PathBuf::from);
    let env_override = std::env::var_os("ECLIPSE_WEBVIEW_HELPER");
    let exe = std::env::current_exe().ok();
    resolve_helper_from(
        config_path.as_deref(),
        env_override.as_deref(),
        exe.as_deref(),
    )
}

fn prepare_webview_data_root_from(base: &Path, current_dir: &Path) -> Result<PathBuf, ClientError> {
    use std::os::unix::fs::PermissionsExt as _;

    let absolute_base = if base.is_absolute() {
        base.to_path_buf()
    } else {
        current_dir.join(base)
    };
    let requested = absolute_base.join("webview-cef");
    std::fs::create_dir_all(&requested)
        .map_err(|e| ClientError::Storage(format!("cannot create {}: {e}", requested.display())))?;
    let root = requested.canonicalize().map_err(|e| {
        ClientError::Storage(format!("cannot canonicalize {}: {e}", requested.display()))
    })?;
    if root.to_str().is_none() {
        return Err(ClientError::Storage(format!(
            "CEF requires a UTF-8 profile path, but {} is not UTF-8",
            root.display()
        )));
    }
    std::fs::set_permissions(&root, std::fs::Permissions::from_mode(0o700)).map_err(|e| {
        ClientError::Storage(format!(
            "cannot restrict {} to owner-only mode 0700: {e}",
            root.display()
        ))
    })?;
    Ok(root)
}

fn prepare_webview_data_root() -> Result<PathBuf, ClientError> {
    let base = crate::framework::app_data_dir().ok_or_else(|| {
        ClientError::Storage(
            "no XDG/home app-data directory is available; set ECLIPSE_APP_DATA_DIR to an \
             absolute writable directory"
                .to_string(),
        )
    })?;
    let current_dir = std::env::current_dir()
        .map_err(|e| ClientError::Storage(format!("cannot resolve current directory: {e}")))?;
    prepare_webview_data_root_from(&base, &current_dir)
}

fn spawn_helper_process() -> Result<(UnixStream, Child, hostprobe::ProbeOutcome), ClientError> {
    use std::os::unix::process::CommandExt as _;

    let helper = resolve_helper()?;
    let webview_data_root = prepare_webview_data_root()?;

    let probe = match helper.parent() {
        Some(dir) => hostprobe::probe(dir),
        None => hostprobe::ProbeOutcome::Unavailable("helper path has no parent dir".to_string()),
    };
    match hostprobe::log_line(&probe) {
        (false, line) => tracing::info!("{line}"),
        (true, line) => tracing::warn!("{line}"),
    }
    let (parent_end, child_end) =
        UnixStream::pair().map_err(|e| ClientError::Spawn(format!("socketpair failed: {e}")))?;
    let mut cmd = std::process::Command::new(&helper);
    cmd.arg("--ipc-fd=3");

    cmd.env("ECLIPSE_WEBVIEW_DATA_DIR", &webview_data_root);

    if crate::config::Config::load()
        .map(|c| c.webview_allow_unsandboxed)
        .unwrap_or(false)
    {
        cmd.arg("--allow-unsandboxed");
    }

    let boot_ua = app_user_agent();
    if let Some(ua) = &boot_ua {
        cmd.env("ECLIPSE_WEBVIEW_APP_UA", ua);
    }
    if let Ok(mut slot) = HELPER_BOOT_UA.lock() {
        *slot = boot_ua;
    }

    HELPER_UA_FIXED.store(true, Ordering::Relaxed);

    if let Some(dir) = helper.parent() {
        let mut ld = dir.as_os_str().to_owned();
        if let Some(inherited) = std::env::var_os("LD_LIBRARY_PATH") {
            if !inherited.is_empty() {
                ld.push(":");
                ld.push(inherited);
            }
        }
        cmd.env("LD_LIBRARY_PATH", ld);
    }
    let child_fd = child_end
        .as_fd()
        .try_clone_to_owned()
        .map_err(|e| ClientError::Spawn(format!("fd clone failed: {e}")))?;

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
            if libc::prctl(libc::PR_SET_PDEATHSIG, libc::SIGTERM, 0, 0, 0) != 0 {
                return Err(std::io::Error::last_os_error());
            }
            Ok(())
        });
    }
    let child = cmd
        .spawn()
        .map_err(|e| ClientError::Spawn(format!("spawn {} failed: {e}", helper.display())))?;
    drop(child_fd);
    drop(child_end);
    tracing::info!(
        helper = %helper.display(),
        "eclipse-webview helper spawned (fd-3 socketpair, PDEATHSIG, no URL in argv)"
    );
    Ok((parent_end, child, probe))
}

fn enrich_spawn_failure(
    base: ClientError,
    probe: &hostprobe::ProbeOutcome,
    status: Option<std::process::ExitStatus>,
) -> ClientError {
    let ClientError::Handshake(inner) = &base else {
        return base;
    };
    let Some(code) = status.and_then(|s| s.code()) else {
        return base;
    };
    match probe {
        hostprobe::ProbeOutcome::Report(r) if !r.missing.is_empty() => {
            ClientError::Handshake(format!(
                "helper exited before HelloAck (exit status {code}) — the dynamic linker could \
                 not start the CEF payload; missing host libraries per the pre-spawn probe: {} \
                 — install them and retry (handshake: {inner})",
                r.display_missing()
            ))
        }
        hostprobe::ProbeOutcome::PayloadMissing { libcef_path } => ClientError::Handshake(format!(
            "helper exited before HelloAck (exit status {code}) — the CEF payload (libcef.so) is \
             missing at {} — run tools/webview-dist/package-webview.sh, or build \
             crates/eclipse-webview with CEF_PATH set (handshake: {inner})",
            libcef_path.display()
        )),
        _ => ClientError::Handshake(format!(
            "{inner} (helper exit status {code}) — likely a missing host library; run: \
             ldd <helper-dir>/libcef.so"
        )),
    }
}

fn perform_handshake(stream: &UnixStream, timeout: Duration) -> Result<String, ClientError> {
    let hello = ConsumerMsg::Hello {
        version: super::PROTO_VERSION,
    }
    .encode()
    .map_err(ClientError::Encode)?;
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
            let _ = stream.set_read_timeout(None);
            Ok(engine)
        }

        Ok(other) => Err(ClientError::Handshake(format!(
            "expected HelloAck, got {}",
            helper_msg_name(&other)
        ))),
        Err(e) => Err(ClientError::Handshake(format!(
            "protocol error before HelloAck: {e}"
        ))),
    }
}

fn helper_msg_name(msg: &HelperMsg) -> &'static str {
    match msg {
        HelperMsg::HelloAck { .. } => "HelloAck",
        HelperMsg::LoadState { .. } => "LoadState",
        HelperMsg::FrameBufferNew { .. } => "FrameBufferNew",
        HelperMsg::FrameReady { .. } => "FrameReady",
        HelperMsg::Console { .. } => "Console",
        HelperMsg::Crash { .. } => "Crash",
        HelperMsg::CookieList { .. } => "CookieList",
        HelperMsg::ViewClosed { .. } => "ViewClosed",
        HelperMsg::BridgeCall { .. } => "BridgeCall",
        HelperMsg::EvaluateJsResult { .. } => "EvaluateJsResult",
        HelperMsg::CookieSetResult { .. } => "CookieSetResult",
        HelperMsg::CookieFlushDone { .. } => "CookieFlushDone",
        HelperMsg::CookiesClearDone { .. } => "CookiesClearDone",
        HelperMsg::NavigationState { .. } => "NavigationState",
    }
}

fn spawn_client(java_vm: jni::vm::JavaVM) -> Result<Client, ClientError> {
    let (tx, rx) = mpsc::channel::<SpawnVerdict>();
    let shared = Arc::clone(shared());
    let handle = std::thread::Builder::new()
        .name("eclipse-webview-io".into())
        .spawn(move || io_thread_main(&tx, &shared, java_vm))
        .map_err(|e| ClientError::Spawn(format!("io-thread spawn failed: {e}")))?;
    match rx.recv_timeout(SPAWN_RESULT_TIMEOUT) {
        Ok(Ok((writer, child, upcall))) => Ok(Client {
            child,
            writer,
            reader: Some(handle),
            upcall: Some(upcall),
        }),
        Ok(Err(e)) => {
            let _ = handle.join();
            Err(e)
        }

        Err(_) => Err(ClientError::Handshake(
            "helper spawn/handshake verdict timed out".into(),
        )),
    }
}

fn ensure_spawned(
    slot: &mut ClientSlot,
    java_vm: jni::vm::JavaVM,
    trigger: &str,
) -> Result<(), ClientError> {
    let (deferred, replayable) = match slot {
        ClientSlot::Unspawned(early) => (std::mem::take(&mut early.mutations), early.replayable),
        _ => return Ok(()),
    };
    tracing::info!(
        trigger,
        app_ua_known = app_user_agent().is_some(),
        deferred = deferred.len(),
        "cold-starting the eclipse-webview helper — this FIXES the engine's global \
         CefSettings.user_agent for the whole life of THIS helper (§6 2026-07-16 🩹➜⛔; a load-drive \
         REPLACES it if the app's UA arrived too late — `maybe_respawn_for_app_ua`)"
    );
    match spawn_client(java_vm) {
        Ok(client) => *slot = ClientSlot::Live(client, EarlyCookies::new()),
        Err(e) => {
            *slot = ClientSlot::Failed(e.to_string());
            return Err(e);
        }
    }

    for msg in &deferred {
        if let Some(request_id) = deferred_cb_request_id(msg) {
            tracing::warn!(
                target: "android.webkit.CookieManager",
                "ECLIPSE-DEFER-CB replay id={request_id} — replaying the app's ORIGINAL 3-arg \
                 setCookie frame to the now-live engine; its ValueCallback will be answered with \
                 the engine's REAL flag, exactly as it is without the probe"
            );
        }
        send_locked(slot, msg)?;
    }

    if let ClientSlot::Live(_, log) = slot {
        log.mutations = deferred;
        log.replayable = replayable;
    }
    Ok(())
}

#[derive(Debug, PartialEq, Eq)]
enum RespawnVerdict {
    Respawn,

    Keep(&'static str),
}

fn respawn_verdict(
    app_ua: Option<&str>,
    boot_ua: Option<&str>,
    ua_diag_forced: bool,
    live_views: usize,
    log_replayable: bool,
    ops_in_flight: usize,
) -> RespawnVerdict {
    if ua_diag_forced {
        return RespawnVerdict::Keep(
            "ECLIPSE_WEBVIEW_UA_DIAG is forcing a diagnostic User-Agent, which OUTRANKS the app's \
             in the helper's own ladder — a replacement would boot the identical string",
        );
    }
    let Some(app_ua) = app_ua else {
        return RespawnVerdict::Keep(
            "the app never called WebSettings.setUserAgentString — the helper's fallback \
             User-Agent is already the right answer",
        );
    };
    if boot_ua == Some(app_ua) {
        return RespawnVerdict::Keep(
            "the live helper already booted with the User-Agent the app set — nothing to correct",
        );
    }
    if live_views != 0 {
        return RespawnVerdict::Keep(
            "a WebView already has a browser — replacing the helper would DESTROY it, and a \
             network Set-Cookie may have populated the store behind the log; degrading to the \
             User-Agent the engine booted with (the pre-respawn behaviour, said out loud)",
        );
    }
    if !log_replayable {
        return RespawnVerdict::Keep(
            "the cookie log can no longer reproduce the engine's store (bound reached, or a \
             browser retired it) — a replay would silently LOSE cookies, which is exactly the \
             lossy read-back this design refuses",
        );
    }
    if ops_in_flight != 0 {
        return RespawnVerdict::Keep(
            "an app cookie operation is still in flight against the live helper — tearing it down \
             would answer that ValueCallback false for a cookie the replay then sets",
        );
    }
    RespawnVerdict::Respawn
}

const RESPAWN_TEARDOWN_DEADLINE: Duration = Duration::from_secs(3);

fn ops_in_flight() -> usize {
    let get_parked = shared()
        .cookie_get_waiters
        .lock()
        .map(|w| w.len())
        .unwrap_or(0);
    let flush_parked = shared()
        .cookie_flush_waiters
        .lock()
        .map(|w| w.len())
        .unwrap_or(0);
    crate::framework::webview_callbacks_in_flight() + get_parked + flush_parked
}

fn maybe_respawn_for_app_ua() -> bool {
    let (old, log) = {
        let mut slot = match CLIENT.lock() {
            Ok(s) => s,
            Err(_) => return false,
        };
        let ClientSlot::Live(_, log) = &*slot else {
            return false;
        };
        let app_ua = app_user_agent();
        let boot_ua = helper_boot_ua();
        let verdict = respawn_verdict(
            app_ua.as_deref(),
            boot_ua.as_deref(),
            ua_diag_forced(),
            LIVE_VIEWS.load(Ordering::Relaxed),
            log.replayable,
            ops_in_flight(),
        );
        match verdict {
            RespawnVerdict::Keep(reason) => {
                tracing::info!(
                    target: "android.webkit.WebSettings",
                    reason,
                    app_ua_known = app_ua.is_some(),
                    "webview client: NOT replacing the helper for the app's User-Agent"
                );
                return false;
            }
            RespawnVerdict::Respawn => {}
        }
        let ClientSlot::Live(old, log) = std::mem::replace(
            &mut *slot,
            ClientSlot::Failed(RESPAWN_IN_PROGRESS.to_string()),
        ) else {
            return false;
        };

        HELPER_UA_FIXED.store(false, Ordering::Relaxed);
        tracing::info!(
            target: "android.webkit.WebSettings",
            boot_ua = boot_ua.as_deref().unwrap_or("<the Eclipse fallback literal>"),
            app_ua = app_ua.as_deref().unwrap_or(""),
            logged_mutations = log.mutations.len(),
            "webview client: REPLACING the eclipse-webview helper so the engine presents the \
             User-Agent the app set via WebSettings.setUserAgentString — CefSettings.user_agent is \
             global and consumed by CefInitialize, so an engine that booted on the wrong one can \
             only be replaced, never corrected (§6 2026-07-16 respawn). The old helper never \
             created a browser, so the ordered logged mutations completely describe its changes \
             over the same persistent base; they replay into the replacement verbatim."
        );
        (old, log)
    };

    teardown_replaced_helper(old);

    if let Ok(mut slot) = CLIENT.lock() {
        if matches!(&*slot, ClientSlot::Failed(r) if r == RESPAWN_IN_PROGRESS) {
            *slot = ClientSlot::Unspawned(log);
            return true;
        }
        tracing::warn!(
            "webview client: the helper replacement was overtaken (a shutdown raced the swap) — \
             the cookie log is dropped and the winning state stands"
        );
    }
    false
}

fn teardown_replaced_helper(mut old: Client) {
    ACTIVE_VIEW.store(0, Ordering::Relaxed);
    if let Ok(bytes) = ConsumerMsg::Shutdown.encode() {
        let _ = (&mut &old.writer).write_all(&bytes);
    }
    let t0 = Instant::now();
    let mut exit: Option<i32> = None;
    while t0.elapsed() < RESPAWN_TEARDOWN_DEADLINE {
        match old.child.try_wait() {
            Ok(Some(status)) => {
                exit = status.code();
                break;
            }
            Ok(None) => std::thread::sleep(Duration::from_millis(5)),
            Err(_) => break,
        }
    }
    let killed = exit.is_none();
    if killed {
        let _ = old.child.kill();
        if let Ok(status) = old.child.wait() {
            exit = status.code();
        }
    }

    let reader_joined = old.reader.take().map(|h| h.join().is_ok()).unwrap_or(false);
    drop(old.upcall.take());
    if killed || !reader_joined {
        tracing::warn!(
            killed,
            reader_joined,
            helper_exit = exit,
            "webview client: the replaced helper did not exit cleanly — its CEF children may still \
             hold the root_cache_path process singleton, which can make the replacement's \
             CefInitialize exit early (pinned bindings: \"only a single app instance is allowed to \
             run for a given CefSettings.root_cache_path value\")"
        );
    } else {
        tracing::info!(
            helper_exit = exit,
            "webview client: the replaced helper exited cleanly (no views ⇒ cef_shutdown ran ⇒ its \
             CEF children and the process singleton are released)"
        );
    }
}

type SpawnVerdict = Result<(UnixStream, Child, JoinHandle<()>), ClientError>;

static IO_THREAD_ID: Mutex<Option<std::thread::ThreadId>> = Mutex::new(None);

fn io_thread_main(tx: &mpsc::Sender<SpawnVerdict>, shared: &Arc<Shared>, java_vm: jni::vm::JavaVM) {
    if let Ok(mut id) = IO_THREAD_ID.lock() {
        *id = Some(std::thread::current().id());
    }
    let (stream, mut child, probe) = match spawn_helper_process() {
        Ok(x) => x,
        Err(e) => {
            let _ = tx.send(Err(e));
            return;
        }
    };
    match perform_handshake(&stream, HANDSHAKE_TIMEOUT) {
        Ok(engine) => {
            tracing::info!(
                %engine,
                protocol = u64::from(super::PROTO_VERSION),
                "eclipse-webview helper handshake complete"
            );
        }
        Err(e) => {
            let _ = child.kill();
            let status = child.wait().ok();
            let _ = tx.send(Err(enrich_spawn_failure(e, &probe, status)));
            return;
        }
    }
    let writer = match stream.try_clone() {
        Ok(w) => w,
        Err(e) => {
            let _ = child.kill();
            let _ = child.wait();
            let _ = tx.send(Err(ClientError::Spawn(format!(
                "control-socket clone failed: {e}"
            ))));
            return;
        }
    };

    let (up_tx, up_rx) = mpsc::channel::<UpcallEvent>();
    let upcall_shared = Arc::clone(shared);
    let upcall_handle = match std::thread::Builder::new()
        .name("eclipse-webview-upcall".into())
        .spawn(move || upcall_thread_main(&up_rx, &upcall_shared, &java_vm))
    {
        Ok(h) => h,
        Err(e) => {
            let _ = child.kill();
            let _ = child.wait();
            let _ = tx.send(Err(ClientError::Spawn(format!(
                "upcall-thread spawn failed: {e}"
            ))));
            return;
        }
    };
    if let Err(mpsc::SendError(returned)) = tx.send(Ok((writer, child, upcall_handle))) {
        if let Ok((_w, mut c, _h)) = returned {
            let _ = c.kill();
            let _ = c.wait();
        }
        return;
    }
    reader_loop(&stream, shared, &up_tx);

    wake_all_blocking_cookie_waiters();
}

struct Upcall {
    widget: i64,
    state: i32,

    url: String,
}

#[derive(Default)]
struct DispatchOut {
    replies: Vec<ConsumerMsg>,
    upcalls: Vec<Upcall>,

    closed: Vec<i64>,

    bridge_calls: Vec<(i64, u32, String)>,

    eval_results: Vec<(u32, bool, String)>,

    cookie_set_results: Vec<(u32, bool)>,

    cookie_lists: Vec<(u32, Vec<CookieEntry>)>,

    cookie_flush_results: Vec<(u32, bool)>,

    cookie_clear_results: Vec<(u32, bool)>,
    fatal: bool,

    fatal_reason: Option<String>,
}

fn dispatch(msg: HelperMsg, views: &mut HashMap<i64, ViewShared>) -> DispatchOut {
    let mut out = DispatchOut::default();
    match msg {
        HelperMsg::LoadState {
            view,
            state,
            http_status,
        } => match views.get_mut(&view) {
            Some(vs) => {
                if state == 0 {
                    vs.started = true;
                } else {
                    vs.finished_http = Some(http_status);
                }
                out.upcalls.push(Upcall {
                    widget: view,
                    state: i32::from(state),
                    url: vs.driven_url.clone(),
                });
            }

            None => {
                tracing::debug!(
                    view,
                    state,
                    "webview client: LoadState for an untracked view (no upcall fabricated)"
                );
            }
        },
        HelperMsg::FrameReady {
            view,
            generation,
            slot,
            seq,
        } => {
            if let Some(vs) = views.get_mut(&view) {
                if let Some(map) = vs.mapping.as_ref() {
                    if map.generation == generation {
                        let offset = map.slot_bytes as usize * usize::from(slot);
                        if let Some(src) = map.mapping.0.slice(offset, map.slot_bytes as usize) {
                            vs.stage.bytes.clear();
                            vs.stage.bytes.extend_from_slice(src);
                            vs.stage.width = u32::from(map.width);
                            vs.stage.height = u32::from(map.height);
                            vs.stage.stride = map.stride;
                            vs.stage.generation = generation;
                            vs.stage.seq = seq;
                            out.replies.push(ConsumerMsg::FrameAck {
                                view,
                                generation,
                                seq,
                            });
                        }
                    }
                }
            }
        }
        HelperMsg::Console { view, console } => {
            tracing::info!(
                view,
                severity = console.severity(),
                source = console.source(),
                line = console.line(),
                len = console.message_len(),
                "webview helper console event"
            );
        }
        HelperMsg::Crash { view, kind, code } => {
            out.fatal = true;

            out.fatal_reason = Some(match (kind, code) {
                (1, 2) => format!(
                    "web engine sandbox refused in the helper (crash kind=1 code=2) — \
                     {SANDBOX_UNAVAILABLE_MARKER}: this host has neither unprivileged user \
                     namespaces nor a SUID chrome-sandbox; fixes: enable unprivileged user \
                     namespaces (sysctl kernel.unprivileged_userns_clone=1 and \
                     user.max_user_namespaces>0; on Ubuntu 23.10+ also \
                     kernel.apparmor_restrict_unprivileged_userns=0 or an AppArmor profile), OR \
                     install chrome-sandbox beside libcef.so as root:root mode 4755, OR set \
                     config webview_allow_unsandboxed=true to accept a loud unsandboxed \
                     degradation"
                ),
                (1, 3) => "web engine init failed in the helper (crash kind=1 code=3) — \
                           persistent webview storage is unavailable or violates CEF's absolute \
                           root/cache-path contract; verify ECLIPSE_APP_DATA_DIR is writable"
                    .to_string(),

                (1, code) => format!(
                    "web engine init failed in the helper (crash kind=1 code={code}) — \
                     {NO_DISPLAY_MARKER} or ozone selection failure"
                ),
                (k, code) => format!("helper crash (view={view} kind={k} code={code})"),
            });
        }
        HelperMsg::ViewClosed { view } => {
            if views.remove(&view).is_some() {
                out.closed.push(view);
            }
        }

        HelperMsg::BridgeCall {
            view,
            call_id,
            payload_json,
        } => out.bridge_calls.push((view, call_id, payload_json)),
        HelperMsg::EvaluateJsResult {
            request_id,
            ok,
            value_json,
        } => out.eval_results.push((request_id, ok, value_json)),
        HelperMsg::CookieSetResult { request_id, ok } => {
            out.cookie_set_results.push((request_id, ok))
        }
        HelperMsg::CookieList {
            request_id,
            cookies,
        } => out.cookie_lists.push((request_id, cookies)),
        HelperMsg::CookieFlushDone { request_id, ok } => {
            out.cookie_flush_results.push((request_id, ok));
        }
        HelperMsg::CookiesClearDone {
            request_id,
            removed,
        } => out.cookie_clear_results.push((request_id, removed)),
        HelperMsg::NavigationState { view, can_go_back } => {
            if let Some(state) = views.get_mut(&view) {
                state.can_go_back = can_go_back;
            }
        }

        other @ (HelperMsg::HelloAck { .. } | HelperMsg::FrameBufferNew { .. }) => {
            tracing::debug!(
                msg = helper_msg_name(&other),
                "webview client: ignoring out-of-phase helper message"
            );
        }
    }
    out
}

enum UpcallEvent {
    LoadChanged {
        widget: i64,
        state: i32,
        url: String,
    },

    BridgeCall {
        view: i64,
        call_id: u32,
        payload_json: String,
    },

    EvalResult {
        request_id: u32,
        ok: bool,
        value_json: String,
    },

    CookieSetResult {
        request_id: u32,
        ok: bool,
    },

    CookiesClearResult {
        request_id: u32,
        removed: bool,
    },

    ViewClosedDrain {
        widget: i64,
        upto_era: u64,
    },
}

fn upcall_thread_main(
    rx: &mpsc::Receiver<UpcallEvent>,
    shared: &Arc<Shared>,
    java_vm: &jni::vm::JavaVM,
) {
    while let Ok(event) = rx.recv() {
        match event {
            UpcallEvent::LoadChanged { widget, state, url } => {
                let fired = crate::framework::fire_web_view_internal_load_changed(
                    java_vm, widget, state, &url,
                );
                if fired {
                    if let Ok(mut views) = shared.views.lock() {
                        if let Some(vs) = views.get_mut(&widget) {
                            vs.upcalls_ok += 1;
                        }
                    }
                }
            }
            UpcallEvent::BridgeCall {
                view,
                call_id,
                payload_json,
            } => {
                let (ok, result_json) =
                    crate::framework::fire_bridge_call(java_vm, view, call_id, &payload_json);
                if !send_reply_if_live(&ConsumerMsg::BridgeResult {
                    call_id,
                    ok,
                    result_json,
                }) {
                    reader_fatal("control-socket write failed (BridgeResult)");
                }
            }
            UpcallEvent::EvalResult {
                request_id,
                ok,
                value_json,
            } => {
                crate::framework::fire_evaluate_js_result(java_vm, request_id, ok, &value_json);
            }
            UpcallEvent::CookieSetResult { request_id, ok } => {
                note_deferred_callback_answered(request_id, ok);
                crate::framework::fire_cookie_set_result(java_vm, request_id, ok);
            }
            UpcallEvent::CookiesClearResult {
                request_id,
                removed,
            } => {
                crate::framework::fire_cookies_clear_result(java_vm, request_id, removed);
            }
            UpcallEvent::ViewClosedDrain { widget, upto_era } => {
                crate::framework::drop_bridges_for_view_closed(widget, upto_era);
                crate::framework::drain_eval_callbacks_for_view(java_vm, widget, upto_era);
            }
        }
    }

    crate::framework::drain_all_webview_callbacks(java_vm, "web engine helper connection closed");
}

fn reader_loop(stream: &UnixStream, shared: &Arc<Shared>, upcalls: &mpsc::Sender<UpcallEvent>) {
    loop {
        let msg = match proto::read_helper_msg(&mut &*stream) {
            Ok(m) => m,
            Err(proto::ProtoError::Eof) => {
                reader_fatal("helper closed the control socket (EOF)");
                return;
            }
            Err(e) => {
                reader_fatal(&format!("protocol error from helper: {e}"));
                return;
            }
        };
        if let HelperMsg::FrameBufferNew {
            view,
            generation,
            width,
            height,
            stride,
            slot_bytes,
            slot_count,
        } = msg
        {
            let fd = match fdpass::recv_fd_after_sentinel(stream) {
                Ok(f) => f,
                Err(e) => {
                    reader_fatal(&format!("frame-buffer fd receive failed: {e}"));
                    return;
                }
            };
            let expected = slot_bytes as usize * usize::from(slot_count);

            let mapping = match shm::map_frame_buffer(fd.as_fd(), expected) {
                Ok(m) => m,
                Err(e) => {
                    reader_fatal(&format!("frame-buffer memfd rejected: {e}"));
                    return;
                }
            };
            match shared.views.lock() {
                Ok(mut views) => match views.get_mut(&view) {
                    Some(vs) => {
                        vs.mapping = Some(FrameMap {
                            mapping: SendMapping(mapping),
                            generation,
                            width,
                            height,
                            stride,
                            slot_bytes,
                        });
                    }
                    None => {
                        tracing::debug!(
                            view,
                            generation,
                            "webview client: frame buffer for an untracked view (dropped)"
                        );
                    }
                },
                Err(_) => {
                    reader_fatal("views lock poisoned");
                    return;
                }
            }
            continue;
        }
        let (out, close_eras) = match shared.views.lock() {
            Ok(mut views) => {
                let out = dispatch(msg, &mut views);

                let eras: Vec<u64> = out
                    .closed
                    .iter()
                    .map(|_| crate::framework::bump_webview_close_era())
                    .collect();
                (out, eras)
            }
            Err(_) => {
                reader_fatal("views lock poisoned");
                return;
            }
        };
        for reply in &out.replies {
            if !send_reply_if_live(reply) {
                reader_fatal("control-socket write failed (FrameAck)");
                return;
            }
        }

        for up in out.upcalls {
            let _ = upcalls.send(UpcallEvent::LoadChanged {
                widget: up.widget,
                state: up.state,
                url: up.url,
            });
        }
        for (view, call_id, payload_json) in out.bridge_calls {
            let _ = upcalls.send(UpcallEvent::BridgeCall {
                view,
                call_id,
                payload_json,
            });
        }
        for (request_id, ok, value_json) in out.eval_results {
            let _ = upcalls.send(UpcallEvent::EvalResult {
                request_id,
                ok,
                value_json,
            });
        }
        for (request_id, ok) in out.cookie_set_results {
            let _ = upcalls.send(UpcallEvent::CookieSetResult { request_id, ok });
        }
        for (request_id, cookies) in out.cookie_lists {
            let waiter = shared
                .cookie_get_waiters
                .lock()
                .ok()
                .and_then(|mut w| w.remove(&request_id));
            if let Some(tx) = waiter {
                let _ = tx.send(cookies);
            } else {
                tracing::debug!(
                    request_id,
                    "webview client: dropping a late CookieList with no getCookie waiter"
                );
            }
        }
        for (request_id, ok) in out.cookie_flush_results {
            let waiter = shared
                .cookie_flush_waiters
                .lock()
                .ok()
                .and_then(|mut w| w.remove(&request_id));
            if let Some(tx) = waiter {
                let _ = tx.send(ok);
            }
        }
        for (request_id, removed) in out.cookie_clear_results {
            let _ = upcalls.send(UpcallEvent::CookiesClearResult {
                request_id,
                removed,
            });
        }
        for (closed, upto_era) in out.closed.into_iter().zip(close_eras) {
            let _ = ACTIVE_VIEW.compare_exchange(closed, 0, Ordering::Relaxed, Ordering::Relaxed);
            LIVE_VIEWS.fetch_sub(1, Ordering::Relaxed);

            remove_pending_bridges(closed);
            let _ = upcalls.send(UpcallEvent::ViewClosedDrain {
                widget: closed,
                upto_era,
            });
            tracing::info!(view = closed, "webview helper confirmed ViewClosed");
        }
        if out.fatal {
            let reason = out
                .fatal_reason
                .unwrap_or_else(|| "helper reported a crash".into());
            reader_fatal(&reason);
            return;
        }
    }
}

fn reader_fatal(reason: &str) {
    ACTIVE_VIEW.store(0, Ordering::Relaxed);
    if let Ok(mut slot) = CLIENT.lock() {
        if matches!(&*slot, ClientSlot::Live(_, _)) {
            tracing::warn!(
                reason,
                "eclipse-webview client: helper connection lost — latching the honest no-op \
                 path (no respawn; subsequent WebView loads degrade to the one-shot WARN)"
            );
            if let ClientSlot::Live(client, _log) =
                std::mem::replace(&mut *slot, ClientSlot::Failed(reason.to_string()))
            {
                let mut child = client.child;
                let _ = child.kill();
                let _ = child.wait();
            }
        } else {
            tracing::debug!(reason, "webview reader exiting after teardown");
        }
    }
}

fn send_reply_if_live(msg: &ConsumerMsg) -> bool {
    let Ok(bytes) = msg.encode() else {
        return true;
    };
    match CLIENT.lock() {
        Ok(slot) => match &*slot {
            ClientSlot::Live(c, _) => (&mut &c.writer).write_all(&bytes).is_ok(),
            _ => true,
        },
        Err(_) => false,
    }
}

fn latched_error(slot: &ClientSlot) -> Option<ClientError> {
    match slot {
        ClientSlot::Failed(reason) => Some(ClientError::Latched(reason.clone())),
        _ => None,
    }
}

fn record_view(views: &mut HashMap<i64, ViewShared>, widget: i64, driven_url: String) -> bool {
    let log_target = redact::url_scheme_and_host_for_log(&driven_url);
    match views.entry(widget) {
        std::collections::hash_map::Entry::Occupied(mut e) => {
            let vs = e.get_mut();
            vs.driven_url = driven_url;
            vs.log_target = log_target;

            vs.started = false;
            vs.finished_http = None;
            false
        }
        std::collections::hash_map::Entry::Vacant(e) => {
            e.insert(ViewShared {
                driven_url,
                log_target,
                mapping: None,
                stage: Stage::default(),
                started: false,
                finished_http: None,
                can_go_back: false,
                upcalls_ok: 0,
            });
            true
        }
    }
}

fn send_locked(slot: &mut ClientSlot, msg: &ConsumerMsg) -> Result<(), ClientError> {
    let bytes = msg.encode().map_err(ClientError::Encode)?;
    let write_result = match &*slot {
        ClientSlot::Live(c, _) => (&mut &c.writer).write_all(&bytes),
        _ => return Err(ClientError::Internal("send on a non-live client slot")),
    };
    if let Err(e) = write_result {
        let reason = format!("control-socket write failed: {}", e.kind());
        tracing::warn!(
            reason,
            "eclipse-webview client: latching the honest no-op path (no respawn)"
        );
        ACTIVE_VIEW.store(0, Ordering::Relaxed);
        if let ClientSlot::Live(client, _log) =
            std::mem::replace(slot, ClientSlot::Failed(reason.clone()))
        {
            let mut child = client.child;
            let _ = child.kill();
            let _ = child.wait();
        }
        return Err(ClientError::Latched(reason));
    }
    Ok(())
}

enum DriveTarget {
    Url(String),
    Data {
        base_url: Option<String>,
        data: String,
        mime: Option<String>,
        encoding: Option<String>,
    },
}

fn drive(
    java_vm: jni::vm::JavaVM,
    widget: i64,
    target: DriveTarget,
    width: u16,
    height: u16,
) -> Result<(), ClientError> {
    let respawned = maybe_respawn_for_app_ua();

    let mut slot = CLIENT
        .lock()
        .map_err(|_| ClientError::Internal("client lock poisoned"))?;
    if let Some(e) = latched_error(&slot) {
        return Err(e);
    }
    ensure_spawned(
        &mut slot,
        java_vm,
        if respawned {
            "WebView load-drive after the app-UA helper replacement — replaying the cookie log into \
             an engine that carries the User-Agent the app set"
        } else {
            "WebView load-drive (loadUrl/loadDataWithBaseURL) — the app's UA is final"
        },
    )?;

    let driven_url = match &target {
        DriveTarget::Url(url) => url.clone(),
        DriveTarget::Data { base_url, .. } => base_url
            .clone()
            .unwrap_or_else(|| "about:blank".to_string()),
    };
    let is_new = {
        let shared = shared();
        let mut views = shared
            .views
            .lock()
            .map_err(|_| ClientError::Internal("views lock poisoned"))?;
        record_view(&mut views, widget, driven_url)
    };
    if is_new {
        LIVE_VIEWS.fetch_add(1, Ordering::Relaxed);

        if let ClientSlot::Live(_, log) = &mut *slot {
            log.retire();
        }
    }

    ACTIVE_VIEW.store(widget, Ordering::Relaxed);
    if is_new {
        send_locked(
            &mut slot,
            &ConsumerMsg::CreateView {
                view: widget,
                width,
                height,
            },
        )?;

        for (name, methods) in drain_pending_bridges(widget) {
            send_locked(
                &mut slot,
                &ConsumerMsg::BridgeRegister {
                    view: widget,
                    name,
                    methods,
                },
            )?;
        }
    }
    let load_msg = match target {
        DriveTarget::Url(url) => ConsumerMsg::LoadUrl { view: widget, url },
        DriveTarget::Data {
            base_url,
            data,
            mime,
            encoding,
        } => ConsumerMsg::LoadDataWithBaseUrl {
            view: widget,
            base_url: base_url.unwrap_or_else(|| "about:blank".to_string()),
            data,
            mime: mime.unwrap_or_default(),
            encoding: encoding.unwrap_or_default(),

            history_url: String::new(),
        },
    };
    send_locked(&mut slot, &load_msg)
}

pub fn drive_load_url(
    java_vm: jni::vm::JavaVM,
    widget: i64,
    url: String,
    width: u16,
    height: u16,
) -> Result<(), ClientError> {
    drive(java_vm, widget, DriveTarget::Url(url), width, height)
}

#[allow(clippy::too_many_arguments)]
pub fn drive_load_data(
    java_vm: jni::vm::JavaVM,
    widget: i64,
    base_url: Option<String>,
    data: String,
    mime: Option<String>,
    encoding: Option<String>,
    width: u16,
    height: u16,
) -> Result<(), ClientError> {
    drive(
        java_vm,
        widget,
        DriveTarget::Data {
            base_url,
            data,
            mime,
            encoding,
        },
        width,
        height,
    )
}

#[allow(clippy::type_complexity)]
fn pending_bridges() -> &'static Mutex<HashMap<i64, HashMap<String, Vec<BridgeMethod>>>> {
    static P: OnceLock<Mutex<HashMap<i64, HashMap<String, Vec<BridgeMethod>>>>> = OnceLock::new();
    P.get_or_init(|| Mutex::new(HashMap::new()))
}

static PENDING_BRIDGE_VIEWS: AtomicUsize = AtomicUsize::new(0);

fn buffer_pending_bridge(widget: i64, name: String, methods: Vec<BridgeMethod>) {
    if let Ok(mut m) = pending_bridges().lock() {
        m.entry(widget).or_default().insert(name, methods);
        PENDING_BRIDGE_VIEWS.store(m.len(), Ordering::Relaxed);
    }
}

fn remove_pending_bridges(widget: i64) {
    if let Ok(mut m) = pending_bridges().lock() {
        m.remove(&widget);
        PENDING_BRIDGE_VIEWS.store(m.len(), Ordering::Relaxed);
    }
}

fn drain_pending_bridges(widget: i64) -> Vec<(String, Vec<BridgeMethod>)> {
    pending_bridges()
        .lock()
        .ok()
        .and_then(|mut m| {
            let taken = m.remove(&widget);
            PENDING_BRIDGE_VIEWS.store(m.len(), Ordering::Relaxed);
            taken
        })
        .map(|b| b.into_iter().collect())
        .unwrap_or_default()
}

fn send_with_lazy_spawn(
    java_vm: jni::vm::JavaVM,
    msg: &ConsumerMsg,
) -> Result<SendOutcome, ClientError> {
    let mut slot = CLIENT
        .lock()
        .map_err(|_| ClientError::Internal("client lock poisoned"))?;
    if let Some(e) = latched_error(&slot) {
        return Err(e);
    }

    let verdict = match &mut *slot {
        ClientSlot::Unspawned(early) => Some(early.offer(msg, defer_cookie_cb())),
        _ => None,
    };
    match verdict {
        Some(Deferral::Buffer) => {
            if let Some(request_id) = deferred_cb_request_id(msg) {
                note_deferred_callback(request_id);
            }
            return Ok(SendOutcome::Buffered);
        }
        Some(Deferral::NeedsEngine(why)) => {
            tracing::warn!(
                reason = why,
                "an early op is forcing the eclipse-webview helper to start BEFORE the app has \
                 configured its WebView — CefSettings.user_agent is GLOBAL and consumed by \
                 CefInitialize, so THIS engine will present Eclipse's FALLBACK User-Agent, not the \
                 app's (§6 2026-07-16 🏆/💥). 2026-07-16 (§6 respawn): no longer a lost boot — the \
                 first load-drive REPLACES this helper with one carrying the app's UA and replays \
                 the cookie log into it. The cost is one wasted CefInitialize (~122 ms), paid here, \
                 refunded there."
            );
            ensure_spawned(&mut slot, java_vm, why)?;
        }
        None => {}
    }
    let outcome = send_locked(&mut slot, msg).map(|()| SendOutcome::Sent)?;

    if let ClientSlot::Live(_, log) = &mut *slot {
        log.record_sent(msg);
    }
    Ok(outcome)
}

pub fn register_bridge(
    java_vm: jni::vm::JavaVM,
    widget: i64,
    name: String,
    methods: Vec<BridgeMethod>,
) -> Result<(), ClientError> {
    buffer_pending_bridge(widget, name.clone(), methods.clone());
    if view_is_tracked(widget) {
        send_with_lazy_spawn(
            java_vm,
            &ConsumerMsg::BridgeRegister {
                view: widget,
                name,
                methods,
            },
        )
        .map(|_| ())
    } else {
        Ok(())
    }
}

pub fn evaluate_js(
    java_vm: jni::vm::JavaVM,
    widget: i64,
    request_id: u32,
    script: String,
) -> Result<(), ClientError> {
    send_with_lazy_spawn(
        java_vm,
        &ConsumerMsg::EvaluateJsForResult {
            view: widget,
            request_id,
            script,
        },
    )
    .map(|_| ())
}

#[allow(clippy::too_many_arguments)]
pub fn cookie_set(
    java_vm: jni::vm::JavaVM,
    url: String,
    name: String,
    value: String,
    domain: String,
    path: String,
    secure: bool,
    http_only: bool,
    expires_epoch_s: i64,
) -> Result<(), ClientError> {
    send_with_lazy_spawn(
        java_vm,
        &ConsumerMsg::CookieSet {
            url,
            name,
            value,
            domain,
            path,
            secure,
            http_only,
            expires_epoch_s,
        },
    )
    .map(|_| ())
}

#[allow(clippy::too_many_arguments)]
pub fn cookie_set_with_result(
    java_vm: jni::vm::JavaVM,
    request_id: u32,
    url: String,
    name: String,
    value: String,
    domain: String,
    path: String,
    secure: bool,
    http_only: bool,
    expires_epoch_s: i64,
) -> Result<(), ClientError> {
    send_with_lazy_spawn(
        java_vm,
        &ConsumerMsg::CookieSetForResult {
            request_id,
            url,
            name,
            value,
            domain,
            path,
            secure,
            http_only,
            expires_epoch_s,
        },
    )
    .map(|_| ())
}

pub fn cookies_clear_all(java_vm: jni::vm::JavaVM, request_id: u32) -> Result<(), ClientError> {
    send_with_lazy_spawn(java_vm, &ConsumerMsg::CookiesClear { request_id }).map(|_| ())
}

pub fn cookies_clear_session(java_vm: jni::vm::JavaVM, request_id: u32) -> Result<(), ClientError> {
    send_with_lazy_spawn(java_vm, &ConsumerMsg::CookiesClearSession { request_id }).map(|_| ())
}

pub fn cookie_get_blocking(
    java_vm: jni::vm::JavaVM,
    url: String,
    timeout: Duration,
) -> Result<Vec<CookieEntry>, ClientError> {
    if IO_THREAD_ID.lock().ok().and_then(|id| *id) == Some(std::thread::current().id()) {
        tracing::warn!(
            "cookie_get_blocking called ON the eclipse-webview-io thread — the reply could never \
             be delivered; serving the honest empty list immediately (fix the caller: app-code \
             upcalls belong on the upcall thread)"
        );
        return Ok(Vec::new());
    }
    let request_id = next_request_id();
    let (tx, rx) = mpsc::channel::<Vec<CookieEntry>>();

    match shared().cookie_get_waiters.lock() {
        Ok(mut w) => {
            w.insert(request_id, tx);
        }
        Err(_) => return Err(ClientError::Internal("cookie waiters lock poisoned")),
    }
    match send_with_lazy_spawn(java_vm, &ConsumerMsg::CookieGet { request_id, url }) {
        Ok(SendOutcome::Sent) => {}
        Ok(SendOutcome::Buffered) => {
            remove_cookie_waiter(request_id);
            return Err(ClientError::Internal(
                "CookieGet was buffered even though the persistent store owns its answer",
            ));
        }
        Err(e) => {
            remove_cookie_waiter(request_id);
            return Err(e);
        }
    }
    match rx.recv_timeout(timeout) {
        Ok(cookies) => Ok(cookies),

        Err(_) => {
            remove_cookie_waiter(request_id);
            Ok(Vec::new())
        }
    }
}

fn remove_cookie_waiter(request_id: u32) {
    if let Ok(mut w) = shared().cookie_get_waiters.lock() {
        w.remove(&request_id);
    }
}

pub fn cookie_flush_blocking(
    java_vm: jni::vm::JavaVM,
    timeout: Duration,
) -> Result<bool, ClientError> {
    if IO_THREAD_ID.lock().ok().and_then(|id| *id) == Some(std::thread::current().id()) {
        tracing::warn!(
            "cookie_flush_blocking called ON the eclipse-webview-io thread — the completion could \
             never be delivered; refusing the self-deadlock"
        );
        return Ok(false);
    }
    let request_id = next_request_id();
    let (tx, rx) = mpsc::channel::<bool>();
    match shared().cookie_flush_waiters.lock() {
        Ok(mut w) => {
            w.insert(request_id, tx);
        }
        Err(_) => return Err(ClientError::Internal("cookie flush waiters lock poisoned")),
    }
    match send_with_lazy_spawn(java_vm, &ConsumerMsg::CookieFlush { request_id }) {
        Ok(SendOutcome::Sent) => {}
        Ok(SendOutcome::Buffered) => {
            remove_cookie_flush_waiter(request_id);
            return Err(ClientError::Internal(
                "cookie flush was not sent to the persistent engine",
            ));
        }
        Err(e) => {
            remove_cookie_flush_waiter(request_id);
            return Err(e);
        }
    }
    match rx.recv_timeout(timeout) {
        Ok(ok) => Ok(ok),
        Err(_) => {
            remove_cookie_flush_waiter(request_id);
            Ok(false)
        }
    }
}

fn remove_cookie_flush_waiter(request_id: u32) {
    if let Ok(mut w) = shared().cookie_flush_waiters.lock() {
        w.remove(&request_id);
    }
}

fn wake_all_blocking_cookie_waiters() {
    if let Ok(mut w) = shared().cookie_get_waiters.lock() {
        w.clear();
    }
    if let Ok(mut w) = shared().cookie_flush_waiters.lock() {
        w.clear();
    }
}

pub fn active_view() -> i64 {
    ACTIVE_VIEW.load(Ordering::Relaxed)
}

pub fn composited_rect() -> Option<(i32, i32, u32, u32)> {
    shared().rect.lock().ok().and_then(|r| *r)
}

pub fn publish_composited_screen_rect(view: i64, rect: (i32, i32, u32, u32)) {
    let (x, y, w, h) = rect;
    if let Ok(mut r) = shared().screen_rect.lock() {
        *r = Some(DrawnRect { view, x, y, w, h });
    }
}

pub fn composited_screen_rect(view: i64) -> Option<(i32, i32, u32, u32)> {
    match *shared().screen_rect.lock().ok()? {
        Some(r) if r.view == view => Some((r.x, r.y, r.w, r.h)),
        _ => None,
    }
}

pub fn update_composited_rect() {
    let view = ACTIVE_VIEW.load(Ordering::Relaxed);
    let rect = if view == 0 {
        None
    } else {
        crate::framework::view_registry::absolute_frame(view)
    };
    if let Ok(mut r) = shared().rect.lock() {
        *r = rect;
    }
}

pub fn with_latest_frame<R>(view: i64, f: impl FnOnce(&Stage) -> R) -> Option<R> {
    let views = shared().views.try_lock().ok()?;
    let vs = views.get(&view)?;
    if vs.stage.seq == 0 {
        return None;
    }
    Some(f(&vs.stage))
}

fn send_input(msg: &ConsumerMsg) {
    if let Ok(mut slot) = CLIENT.lock() {
        if matches!(&*slot, ClientSlot::Live(_, _)) {
            let _ = send_locked(&mut slot, msg);
        }
    }
}

pub fn send_mouse_move(view: i64, x: i32, y: i32) {
    send_input(&ConsumerMsg::MouseMove {
        view,
        x,
        y,
        modifiers: 0,
        leave: false,
    });
}

pub fn send_mouse_click(view: i64, x: i32, y: i32, down: bool) {
    send_input(&ConsumerMsg::MouseClick {
        view,
        x,
        y,
        button: 0,
        down,
        click_count: 1,
        modifiers: 0,
    });
}

pub fn send_mouse_wheel(view: i64, x: i32, y: i32, delta_y: i32) {
    send_input(&ConsumerMsg::MouseWheel {
        view,
        x,
        y,
        delta_x: 0,
        delta_y,
        modifiers: 0,
    });
}

pub fn send_key(view: i64, kind: u8, windows_key_code: i32, character: u16) {
    send_input(&ConsumerMsg::Key {
        view,
        kind,
        windows_key_code,
        native_key_code: 0,
        character,
        modifiers: 0,
    });
}

pub fn can_go_back(view: i64) -> bool {
    shared()
        .views
        .lock()
        .ok()
        .and_then(|views| views.get(&view).map(|state| state.can_go_back))
        .unwrap_or(false)
}

pub fn go_back(view: i64) {
    send_input(&ConsumerMsg::GoBack { view });
}

pub fn notify_view_freed(widget: i64) {
    if crate::framework::has_webview_bridges() {
        crate::framework::drop_bridges_for(widget);
    }
    if PENDING_BRIDGE_VIEWS.load(Ordering::Relaxed) != 0 {
        remove_pending_bridges(widget);
    }
    if ACTIVE_VIEW.load(Ordering::Relaxed) == 0 && LIVE_VIEWS.load(Ordering::Relaxed) == 0 {
        return;
    }
    let tracked = shared()
        .views
        .lock()
        .ok()
        .map(|mut v| v.remove(&widget).is_some())
        .unwrap_or(false);
    if !tracked {
        return;
    }
    LIVE_VIEWS.fetch_sub(1, Ordering::Relaxed);
    let _ = ACTIVE_VIEW.compare_exchange(widget, 0, Ordering::Relaxed, Ordering::Relaxed);
    tracing::info!(
        widget,
        "webview client: driven WebView finalized — sending CloseView (helper stays alive)"
    );
    if let Ok(mut slot) = CLIENT.lock() {
        if matches!(&*slot, ClientSlot::Live(_, _)) {
            let _ = send_locked(&mut slot, &ConsumerMsg::CloseView { view: widget });
        }
    }
}

pub fn notify_view_detached(widget: i64) {
    if ACTIVE_VIEW
        .compare_exchange(widget, 0, Ordering::Relaxed, Ordering::Relaxed)
        .is_err()
    {
        return;
    }
    tracing::info!(
        view = widget,
        "webview client: active WebView detached from the view tree — eager CloseView (composite \
         released; ViewClosed completes teardown)"
    );
    if let Ok(mut slot) = CLIENT.lock() {
        if matches!(&*slot, ClientSlot::Live(_, _)) {
            let _ = send_locked(&mut slot, &ConsumerMsg::CloseView { view: widget });
        }
    }
}

pub fn close_view(widget: i64) -> Result<(), ClientError> {
    let mut slot = CLIENT
        .lock()
        .map_err(|_| ClientError::Internal("client lock poisoned"))?;
    if let Some(e) = latched_error(&slot) {
        return Err(e);
    }
    send_locked(&mut slot, &ConsumerMsg::CloseView { view: widget })
}

pub fn view_is_tracked(view: i64) -> bool {
    shared()
        .views
        .lock()
        .ok()
        .is_some_and(|v| v.contains_key(&view))
}

pub fn load_observed(view: i64) -> Option<LoadObserved> {
    let views = shared().views.lock().ok()?;
    let vs = views.get(&view)?;
    Some(LoadObserved {
        started: vs.started,
        finished_http: vs.finished_http,
        upcalls_ok: vs.upcalls_ok,
    })
}

pub fn failed_reason() -> Option<String> {
    match CLIENT.lock() {
        Ok(slot) => match &*slot {
            ClientSlot::Failed(reason) => Some(reason.clone()),
            _ => None,
        },
        Err(_) => None,
    }
}

pub fn needs_cookie_flush_before_shutdown() -> bool {
    CLIENT
        .lock()
        .map(|slot| slot_needs_cookie_flush(&slot))
        .unwrap_or(false)
}

fn slot_needs_cookie_flush(slot: &ClientSlot) -> bool {
    match slot {
        ClientSlot::Live(_, _) => true,
        ClientSlot::Unspawned(early) => !early.mutations.is_empty(),
        ClientSlot::Failed(_) => false,
    }
}

fn answer_stranded_deferred_callbacks(vm: &crate::runtime::Vm, ids: &[u32]) {
    tracing::warn!(
        target: "android.webkit.CookieManager",
        stranded = ids.len(),
        "ECLIPSE-DEFER-CB shutdown — {} probe-deferred 3-arg setCookie ValueCallback(s) were never \
         replayed (this boot drove no WebView, so the flush never ran). Answering each FALSE now: \
         those frames never reached the persistent engine, so those cookie operations genuinely \
         did not complete. Nothing is left stranded.",
        ids.len()
    );
    crate::framework::drain_deferred_cookie_set_callbacks(
        vm,
        "the web engine helper was shut down with probe-deferred setCookie replies outstanding",
    );
}

pub fn shutdown(vm: &crate::runtime::Vm, deadline: Duration) -> ShutdownReport {
    let mut stranded_cb_ids: Vec<u32> = Vec::new();
    let taken = match CLIENT.lock() {
        Ok(mut slot) => {
            match std::mem::replace(
                &mut *slot,
                ClientSlot::Failed("the web engine helper was shut down".into()),
            ) {
                ClientSlot::Live(c, _log) => Some(c),
                mut other => {
                    if let ClientSlot::Unspawned(early) = &mut other {
                        stranded_cb_ids = early
                            .mutations
                            .iter()
                            .filter_map(deferred_cb_request_id)
                            .collect();
                        early.mutations.clear();
                    }

                    if !matches!(&other, ClientSlot::Failed(r) if r == RESPAWN_IN_PROGRESS) {
                        *slot = other;
                    }
                    None
                }
            }
        }
        Err(_) => None,
    };
    ACTIVE_VIEW.store(0, Ordering::Relaxed);
    if !stranded_cb_ids.is_empty() {
        answer_stranded_deferred_callbacks(vm, &stranded_cb_ids);
    }
    let Some(mut client) = taken else {
        return ShutdownReport {
            helper_exit: None,
            reader_joined: false,
        };
    };
    if let Ok(bytes) = ConsumerMsg::Shutdown.encode() {
        let _ = (&mut &client.writer).write_all(&bytes);
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
    let reader_joined = client
        .reader
        .take()
        .map(|h| h.join().is_ok())
        .unwrap_or(false);

    if let Some(h) = client.upcall.take() {
        let t0 = Instant::now();
        while !h.is_finished() && t0.elapsed() < deadline {
            let _ = crate::framework::pump_main_looper(vm);
            std::thread::sleep(Duration::from_millis(2));
        }
        crate::framework::retire_main_upcall_dispatch(vm);
        let _ = h.join();
    }
    if let Ok(mut views) = shared().views.lock() {
        views.clear();
    }
    if let Ok(mut rect) = shared().rect.lock() {
        *rect = None;
    }

    wake_all_blocking_cookie_waiters();
    if let Ok(mut b) = pending_bridges().lock() {
        b.clear();
        PENDING_BRIDGE_VIEWS.store(0, Ordering::Relaxed);
    }

    crate::framework::drop_all_bridges();
    LIVE_VIEWS.store(0, Ordering::Relaxed);
    ShutdownReport {
        helper_exit: exit,
        reader_joined,
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::fs::File;
    use std::os::unix::fs::FileExt as _;

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
        let _ = std::fs::remove_dir_all(&root);
    }

    #[test]
    fn persistent_webview_root_is_absolute_canonical_and_owner_only() {
        use std::os::unix::fs::PermissionsExt as _;

        let root = temp_dir("persistent-profile");
        let cwd = root.join("cwd");
        std::fs::create_dir_all(&cwd).expect("cwd");
        let prepared = prepare_webview_data_root_from(Path::new("relative-data"), &cwd)
            .expect("prepare persistent profile");
        assert!(prepared.is_absolute());
        assert_eq!(prepared, cwd.join("relative-data/webview-cef"));
        let mode = std::fs::metadata(&prepared)
            .expect("profile metadata")
            .permissions()
            .mode()
            & 0o777;
        assert_eq!(mode, 0o700, "cookie profile must be owner-only");
        let _ = std::fs::remove_dir_all(&root);
    }

    #[test]
    fn webview_client_handshake_gates_on_hello_ack_version() {
        let deadline = Duration::from_secs(2);

        let (client_end, helper_end) = UnixStream::pair().expect("pair");
        let ack = HelperMsg::HelloAck {
            version: super::super::PROTO_VERSION,
            engine: "cef/test".into(),
        }
        .encode()
        .expect("encode ack");
        (&mut &helper_end).write_all(&ack).expect("write ack");
        let engine = perform_handshake(&client_end, deadline).expect("current-version handshake");
        assert_eq!(engine, "cef/test");

        let hello = proto::read_consumer_msg(&mut &helper_end).expect("decode Hello");
        assert_eq!(
            hello,
            ConsumerMsg::Hello {
                version: super::super::PROTO_VERSION
            }
        );

        let (client_end, helper_end) = UnixStream::pair().expect("pair");
        let ack = HelperMsg::HelloAck {
            version: super::super::PROTO_VERSION + 1,
            engine: "cef/future".into(),
        }
        .encode()
        .expect("encode future ack");
        (&mut &helper_end).write_all(&ack).expect("write ack");
        match perform_handshake(&client_end, deadline) {
            Err(ClientError::VersionMismatch { helper_version }) => {
                assert_eq!(helper_version, super::super::PROTO_VERSION + 1);
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

    fn tracked_view_with_mapping(
        views: &mut HashMap<i64, ViewShared>,
        widget: i64,
        driven_url: &str,
        generation: u32,
    ) -> Vec<u8> {
        assert!(record_view(views, widget, driven_url.to_string()));
        let (memfd, slot_bytes) = shm::create_sealed_frame_memfd(4, 2, 2).expect("memfd");
        let payload: Vec<u8> = (0..slot_bytes as usize)
            .map(|i| (i * 5 + 1) as u8)
            .collect();
        let file = File::from(memfd.try_clone().expect("dup"));
        file.write_at(&payload, 0).expect("write slot 0");
        let mapping = shm::map_frame_buffer(memfd.as_fd(), slot_bytes as usize * 2).expect("map");
        views.get_mut(&widget).expect("tracked").mapping = Some(FrameMap {
            mapping: SendMapping(mapping),
            generation,
            width: 4,
            height: 2,
            stride: 16,
            slot_bytes,
        });
        payload
    }

    #[test]
    fn webview_reader_never_fabricates_upcalls_and_acks_only_matching_generations() {
        let mut views: HashMap<i64, ViewShared> = HashMap::new();
        let widget = 0x0000_0001_0000_0000_i64;
        let payload = tracked_view_with_mapping(
            &mut views,
            widget,
            "https://apps.roblox.com/challenge?t=x",
            7,
        );

        let out = dispatch(
            HelperMsg::LoadState {
                view: 999,
                state: 0,
                http_status: 0,
            },
            &mut views,
        );
        assert!(out.upcalls.is_empty() && out.replies.is_empty() && !out.fatal);

        let out = dispatch(
            HelperMsg::LoadState {
                view: widget,
                state: 0,
                http_status: 0,
            },
            &mut views,
        );
        assert_eq!(out.upcalls.len(), 1);
        assert_eq!(out.upcalls[0].widget, widget);
        assert_eq!(out.upcalls[0].state, 0);
        assert_eq!(out.upcalls[0].url, "https://apps.roblox.com/challenge?t=x");
        assert!(views.get(&widget).unwrap().started);
        let out = dispatch(
            HelperMsg::LoadState {
                view: widget,
                state: 3,
                http_status: 200,
            },
            &mut views,
        );
        assert_eq!(out.upcalls.len(), 1);
        assert_eq!(out.upcalls[0].state, 3);
        assert_eq!(views.get(&widget).unwrap().finished_http, Some(200));

        let out = dispatch(
            HelperMsg::NavigationState {
                view: widget,
                can_go_back: true,
            },
            &mut views,
        );
        assert!(!out.fatal);
        assert!(views.get(&widget).unwrap().can_go_back);

        let out = dispatch(
            HelperMsg::FrameReady {
                view: widget,
                generation: 6,
                slot: 0,
                seq: 1,
            },
            &mut views,
        );
        assert!(out.replies.is_empty());
        assert_eq!(views.get(&widget).unwrap().stage.seq, 0);

        let out = dispatch(
            HelperMsg::FrameReady {
                view: widget,
                generation: 7,
                slot: 0,
                seq: 2,
            },
            &mut views,
        );
        assert_eq!(
            out.replies,
            vec![ConsumerMsg::FrameAck {
                view: widget,
                generation: 7,
                seq: 2,
            }]
        );
        let vs = views.get(&widget).unwrap();
        assert_eq!(vs.stage.bytes, payload);
        assert_eq!(
            (vs.stage.width, vs.stage.height, vs.stage.stride),
            (4, 2, 16)
        );
        assert_eq!((vs.stage.generation, vs.stage.seq), (7, 2));

        let out = dispatch(
            HelperMsg::Crash {
                view: 0,
                kind: 1,
                code: -7,
            },
            &mut views,
        );
        assert!(out.fatal && out.upcalls.is_empty());
        let reason = out.fatal_reason.expect("fatal reason");
        assert!(reason.contains(NO_DISPLAY_MARKER), "reason: {reason}");

        let out = dispatch(HelperMsg::ViewClosed { view: widget }, &mut views);
        assert_eq!(out.closed, vec![widget]);
        assert!(!views.contains_key(&widget));
    }

    #[test]
    fn crash_kind1_code2_maps_to_the_sandbox_unavailable_reason_and_code0_stays_no_display() {
        let mut views: HashMap<i64, ViewShared> = HashMap::new();
        let out = dispatch(
            HelperMsg::Crash {
                view: 0,
                kind: 1,
                code: 2,
            },
            &mut views,
        );
        assert!(out.fatal);
        let reason = out.fatal_reason.expect("fatal reason");
        for needle in [
            SANDBOX_UNAVAILABLE_MARKER,
            "unprivileged user namespaces",
            "kernel.unprivileged_userns_clone=1",
            "root:root mode 4755",
            "webview_allow_unsandboxed=true",
        ] {
            assert!(reason.contains(needle), "missing {needle:?} in {reason}");
        }
        assert!(!reason.contains(NO_DISPLAY_MARKER), "reason: {reason}");

        for code in [0, 1, -7] {
            let out = dispatch(
                HelperMsg::Crash {
                    view: 0,
                    kind: 1,
                    code,
                },
                &mut views,
            );
            let reason = out.fatal_reason.expect("fatal reason");
            assert!(reason.contains(NO_DISPLAY_MARKER), "code {code}: {reason}");
            assert!(
                !reason.contains(SANDBOX_UNAVAILABLE_MARKER),
                "code {code}: {reason}"
            );
        }
    }

    #[test]
    fn enrich_spawn_failure_names_the_probed_missing_libs_and_exit_status() {
        use std::os::unix::process::ExitStatusExt as _;
        let exit_127 = std::process::ExitStatus::from_raw(127 << 8);
        let killed = std::process::ExitStatus::from_raw(9);

        let missing = hostprobe::ProbeOutcome::Report(hostprobe::HostLibReport {
            total: 26,
            resolved: 25,
            missing: vec![hostprobe::MissingLib {
                soname: "libnss3.so".into(),
                family_hint: hostprobe::classify("libnss3.so"),
            }],
            inconclusive: 0,
        });
        let base =
            || ClientError::Handshake("protocol error before HelloAck: unexpected EOF".into());
        let enriched = enrich_spawn_failure(base(), &missing, Some(exit_127));
        let ClientError::Handshake(text) = &enriched else {
            panic!("expected Handshake, got {enriched:?}");
        };
        for needle in [
            "exit status 127",
            "dynamic linker could not start the CEF payload",
            "libnss3.so",
            "apt: libnss3",
            "dnf: nss",
            "pacman: nss",
            "install them and retry",
        ] {
            assert!(text.contains(needle), "missing {needle:?} in {text}");
        }

        let payload = hostprobe::ProbeOutcome::PayloadMissing {
            libcef_path: std::path::PathBuf::from("/pkg/libcef.so"),
        };
        let ClientError::Handshake(text) = enrich_spawn_failure(base(), &payload, Some(exit_127))
        else {
            panic!("expected Handshake");
        };
        assert!(text.contains("/pkg/libcef.so") && text.contains("package-webview.sh"));

        let clean = hostprobe::ProbeOutcome::Report(hostprobe::HostLibReport {
            total: 26,
            resolved: 26,
            missing: Vec::new(),
            inconclusive: 0,
        });
        let ClientError::Handshake(text) = enrich_spawn_failure(base(), &clean, Some(exit_127))
        else {
            panic!("expected Handshake");
        };
        assert!(text.contains("likely a missing host library") && text.contains("ldd"));

        let ClientError::Handshake(text) = enrich_spawn_failure(base(), &missing, Some(killed))
        else {
            panic!("expected Handshake");
        };
        assert_eq!(text, "protocol error before HelloAck: unexpected EOF");

        let ClientError::Handshake(text) = enrich_spawn_failure(base(), &missing, None) else {
            panic!("expected Handshake");
        };
        assert_eq!(text, "protocol error before HelloAck: unexpected EOF");

        let vm = enrich_spawn_failure(
            ClientError::VersionMismatch { helper_version: 1 },
            &missing,
            Some(exit_127),
        );
        assert!(matches!(vm, ClientError::VersionMismatch { .. }));
    }

    #[test]
    fn client_log_bindings_are_scheme_and_host_only_at_the_ipc_boundary() {
        let mut views: HashMap<i64, ViewShared> = HashMap::new();
        let widget = 42_i64;
        assert!(record_view(
            &mut views,
            widget,
            "https://host/challenge?token=SECRET".to_string(),
        ));
        let vs = views.get(&widget).unwrap();
        assert_eq!(vs.log_target, "https://host");
        assert!(!vs.log_target.contains("SECRET"));
        assert_eq!(vs.driven_url, "https://host/challenge?token=SECRET");

        let out = dispatch(
            HelperMsg::LoadState {
                view: widget,
                state: 0,
                http_status: 0,
            },
            &mut views,
        );
        assert_eq!(out.upcalls[0].url, "https://host/challenge?token=SECRET");
        assert!(out.fatal_reason.is_none());

        assert!(!record_view(&mut views, widget, "about:blank".to_string()));
        assert_eq!(views.get(&widget).unwrap().log_target, redact::NON_URL);
    }

    #[test]
    fn webview_client_degrades_to_the_warn_noop_after_failure_latch() {
        let reason = format!("{HELPER_NOT_FOUND_MARKER}: probed nothing");
        let slot = ClientSlot::Failed(reason.clone());
        match latched_error(&slot) {
            Some(ClientError::Latched(r)) => {
                assert_eq!(r, reason);
                assert!(
                    ClientError::Latched(r)
                        .to_string()
                        .contains(HELPER_NOT_FOUND_MARKER),
                    "the latched Display must preserve the actionable marker"
                );
            }
            other => panic!("expected the latched error, got {other:?}"),
        }

        assert!(latched_error(&ClientSlot::Unspawned(EarlyCookies::new())).is_none());
    }

    #[test]
    fn dispatch_extracts_bridge_eval_cookie_clear_and_flush_outputs() {
        let mut views: HashMap<i64, ViewShared> = HashMap::new();

        let out = dispatch(
            HelperMsg::BridgeCall {
                view: 7,
                call_id: 3,
                payload_json: "{\"iface\":\"X\",\"method\":\"m\",\"args\":[]}".to_string(),
            },
            &mut views,
        );
        assert_eq!(
            out.bridge_calls,
            vec![(
                7,
                3,
                "{\"iface\":\"X\",\"method\":\"m\",\"args\":[]}".to_string()
            )]
        );
        assert!(out.upcalls.is_empty() && !out.fatal);

        let out = dispatch(
            HelperMsg::EvaluateJsResult {
                request_id: 11,
                ok: true,
                value_json: "\"echo:PING\"".to_string(),
            },
            &mut views,
        );
        assert_eq!(
            out.eval_results,
            vec![(11, true, "\"echo:PING\"".to_string())]
        );

        let out = dispatch(
            HelperMsg::CookieSetResult {
                request_id: 12,
                ok: true,
            },
            &mut views,
        );
        assert_eq!(out.cookie_set_results, vec![(12, true)]);

        let cookies = vec![CookieEntry {
            name: "ECLIPSE_TEST".to_string(),
            value: "1".to_string(),
            domain: "127.0.0.1".to_string(),
            path: "/".to_string(),
            secure: false,
            http_only: false,
        }];
        let out = dispatch(
            HelperMsg::CookieList {
                request_id: 13,
                cookies: cookies.clone(),
            },
            &mut views,
        );
        assert_eq!(out.cookie_lists, vec![(13, cookies)]);

        let out = dispatch(
            HelperMsg::CookieFlushDone {
                request_id: 14,
                ok: true,
            },
            &mut views,
        );
        assert_eq!(out.cookie_flush_results, vec![(14, true)]);

        let out = dispatch(
            HelperMsg::CookiesClearDone {
                request_id: 15,
                removed: false,
            },
            &mut views,
        );
        assert_eq!(out.cookie_clear_results, vec![(15, false)]);
    }

    #[test]
    fn normalize_app_user_agent_treats_null_and_empty_as_a_reset_to_the_default() {
        assert_eq!(normalize_app_user_agent(None), None);
        assert_eq!(normalize_app_user_agent(Some(String::new())), None);

        let app_ua = "Mozilla/5.0 (0MB; 960x540; 160x160; 960x540; HTC unknown; unknown) \
                      AppleWebKit/537.36 (KHTML, like Gecko)  ROBLOX Android App 2.724.735 Phone \
                      Hybrid()  GooglePlayStore RobloxApp/2.724.735 (GlobalDist; GooglePlayStore)";
        assert_eq!(
            normalize_app_user_agent(Some(app_ua.to_string())),
            Some(app_ua.to_string())
        );

        assert_eq!(
            normalize_app_user_agent(Some(" ".to_string())),
            Some(" ".to_string())
        );
    }

    fn a_cookie_set(name: &str) -> ConsumerMsg {
        ConsumerMsg::CookieSet {
            url: "https://www.roblox.com/".into(),
            name: name.into(),
            value: "v".into(),
            domain: ".roblox.com".into(),
            path: "/".into(),
            secure: true,
            http_only: true,
            expires_epoch_s: 0,
        }
    }

    fn a_cookie_set_cb(request_id: u32) -> ConsumerMsg {
        ConsumerMsg::CookieSetForResult {
            request_id,
            url: "https://www.roblox.com/".into(),
            name: "n".into(),
            value: "v".into(),
            domain: ".roblox.com".into(),
            path: "/".into(),
            secure: true,
            http_only: true,
            expires_epoch_s: 1_800_000_000,
        }
    }

    #[test]
    fn defer_cookie_cb_gate_is_exact_match_one_only() {
        assert!(defer_cookie_cb_enabled(Some("1")));
        assert!(!defer_cookie_cb_enabled(Some("")));
        assert!(!defer_cookie_cb_enabled(Some("0")));
        assert!(!defer_cookie_cb_enabled(Some("true")));
        assert!(!defer_cookie_cb_enabled(Some("yes")));
        assert!(!defer_cookie_cb_enabled(Some("1 ")));
        assert!(!defer_cookie_cb_enabled(Some(" 1")));
        assert!(!defer_cookie_cb_enabled(Some("11")));
        assert!(!defer_cookie_cb_enabled(None));
    }

    #[test]
    fn host_shutdown_does_not_spawn_cef_for_an_empty_cookie_deferral() {
        let empty = ClientSlot::Unspawned(EarlyCookies::new());
        assert!(!slot_needs_cookie_flush(&empty));

        let mut dirty = EarlyCookies::new();
        assert_eq!(
            dirty.offer(&a_cookie_set("shutdown"), false),
            Deferral::Buffer
        );
        assert!(slot_needs_cookie_flush(&ClientSlot::Unspawned(dirty)));
        assert!(!slot_needs_cookie_flush(&ClientSlot::Failed(
            "already retired".into()
        )));
    }

    #[test]
    fn defer_cookie_cb_off_keeps_only_fire_and_forget_sets_bufferable() {
        let mut early = EarlyCookies::new();
        assert_eq!(
            early.offer(&a_cookie_set_cb(1), false),
            Deferral::NeedsEngine(
                "setCookie(url, value, ValueCallback) — only the engine yields the REAL success flag"
            )
        );

        assert!(early.mutations.is_empty());
        assert!(!early.holds_unanswered_callback());

        assert_eq!(early.offer(&a_cookie_set("a"), false), Deferral::Buffer);
        assert!(matches!(
            early.offer(&ConsumerMsg::CookiesClear { request_id: 2 }, false),
            Deferral::NeedsEngine(_)
        ));
        assert!(matches!(
            early.offer(&ConsumerMsg::CookiesClearSession { request_id: 3 }, false),
            Deferral::NeedsEngine(_)
        ));
        assert_eq!(early.mutations, vec![a_cookie_set("a")]);
        assert!(matches!(
            early.offer(
                &ConsumerMsg::CookieGet {
                    request_id: 3,
                    url: "https://www.roblox.com/".into(),
                },
                false
            ),
            Deferral::NeedsEngine(_)
        ));
        assert!(matches!(
            early.offer(&ConsumerMsg::CookieFlush { request_id: 4 }, false),
            Deferral::NeedsEngine(_)
        ));
    }

    #[test]
    fn defer_cookie_cb_on_buffers_the_three_arg_set_losslessly_instead_of_spawning() {
        let mut early = EarlyCookies::new();
        assert_eq!(early.offer(&a_cookie_set_cb(7), true), Deferral::Buffer);
        assert_eq!(early.mutations.len(), 1);
        assert!(early.holds_unanswered_callback());

        assert_eq!(early.mutations[0], a_cookie_set_cb(7));
        assert_eq!(deferred_cb_request_id(&early.mutations[0]), Some(7));

        assert_eq!(early.offer(&a_cookie_set("later"), true), Deferral::Buffer);
        assert_eq!(
            early.mutations,
            vec![a_cookie_set_cb(7), a_cookie_set("later")]
        );
    }

    #[test]
    fn defer_cookie_cb_never_lets_a_clear_drop_an_unanswered_callback() {
        let mut early = EarlyCookies::new();
        assert_eq!(early.offer(&a_cookie_set_cb(1), true), Deferral::Buffer);
        assert!(matches!(
            early.offer(&ConsumerMsg::CookiesClear { request_id: 2 }, true),
            Deferral::NeedsEngine(_)
        ));
        assert!(matches!(
            early.offer(&ConsumerMsg::CookiesClearSession { request_id: 3 }, true),
            Deferral::NeedsEngine(_)
        ));

        assert_eq!(early.mutations.len(), 1);
        assert!(early.holds_unanswered_callback());
    }

    #[test]
    fn defer_cookie_cb_respects_the_lemma_boundary_and_the_buffer_cap() {
        let mut early = EarlyCookies::new();
        assert_eq!(early.offer(&a_cookie_set_cb(1), true), Deferral::Buffer);
        assert!(matches!(
            early.offer(
                &ConsumerMsg::CookieGet {
                    request_id: 2,
                    url: "https://www.roblox.com/".into(),
                },
                true
            ),
            Deferral::NeedsEngine(_)
        ));

        let mut full = EarlyCookies::new();
        for i in 0..EarlyCookies::CAP {
            assert_eq!(
                full.offer(&a_cookie_set(&format!("c{i}")), true),
                Deferral::Buffer
            );
        }
        assert_eq!(
            full.offer(&a_cookie_set_cb(9), true),
            Deferral::NeedsEngine("the deferred-cookie buffer is full")
        );
        assert_eq!(full.mutations.len(), EarlyCookies::CAP);
        assert!(!full.holds_unanswered_callback());
    }

    #[test]
    fn early_cookies_defer_sets_so_a_cookie_op_never_cold_starts_the_engine() {
        let mut early = EarlyCookies::new();
        assert_eq!(early.offer(&a_cookie_set("a"), false), Deferral::Buffer);
        assert_eq!(early.offer(&a_cookie_set("b"), false), Deferral::Buffer);
        assert_eq!(early.mutations.len(), 2);
    }

    #[test]
    fn early_cookies_never_guess_about_the_persistent_base() {
        let mut early = EarlyCookies::new();
        assert!(matches!(
            early.offer(
                &ConsumerMsg::CookieGet {
                    request_id: 1,
                    url: "https://www.roblox.com/".into(),
                },
                false
            ),
            Deferral::NeedsEngine(_)
        ));
        assert_eq!(early.offer(&a_cookie_set("a"), false), Deferral::Buffer);
        assert!(matches!(
            early.offer(&ConsumerMsg::CookiesClear { request_id: 2 }, false),
            Deferral::NeedsEngine(_)
        ));
        assert!(matches!(
            early.offer(&ConsumerMsg::CookiesClearSession { request_id: 3 }, false),
            Deferral::NeedsEngine(_)
        ));
        assert_eq!(early.mutations, vec![a_cookie_set("a")]);
        assert!(matches!(
            early.offer(&ConsumerMsg::CookieFlush { request_id: 4 }, false),
            Deferral::NeedsEngine(_)
        ));
    }

    #[test]
    fn early_cookies_demand_the_engine_for_matching_and_for_the_real_set_flag() {
        let mut early = EarlyCookies::new();
        assert_eq!(early.offer(&a_cookie_set("a"), false), Deferral::Buffer);
        assert!(matches!(
            early.offer(
                &ConsumerMsg::CookieGet {
                    request_id: 1,
                    url: "https://www.roblox.com/".into(),
                },
                false
            ),
            Deferral::NeedsEngine(_)
        ));
        assert!(matches!(
            early.offer(
                &ConsumerMsg::CookieSetForResult {
                    request_id: 2,
                    url: "https://www.roblox.com/".into(),
                    name: "n".into(),
                    value: "v".into(),
                    domain: ".roblox.com".into(),
                    path: "/".into(),
                    secure: true,
                    http_only: true,
                    expires_epoch_s: 0,
                },
                false
            ),
            Deferral::NeedsEngine(_)
        ));

        assert_eq!(early.mutations.len(), 1);
    }

    #[test]
    fn early_cookies_are_bounded_and_overflow_forces_the_honest_spawn() {
        let mut early = EarlyCookies::new();
        for i in 0..EarlyCookies::CAP {
            assert_eq!(
                early.offer(&a_cookie_set(&format!("c{i}")), false),
                Deferral::Buffer
            );
        }
        assert!(matches!(
            early.offer(&a_cookie_set("overflow"), false),
            Deferral::NeedsEngine(_)
        ));
        assert_eq!(early.mutations.len(), EarlyCookies::CAP);
    }

    #[test]
    fn early_cookie_sets_replay_in_arrival_order() {
        let mut early = EarlyCookies::new();
        for n in ["first", "second", "third"] {
            assert_eq!(early.offer(&a_cookie_set(n), false), Deferral::Buffer);
        }
        let taken = std::mem::take(&mut early.mutations);
        let names: Vec<&str> = taken
            .iter()
            .map(|m| match m {
                ConsumerMsg::CookieSet { name, .. } => name.as_str(),
                _ => "not-a-set",
            })
            .collect();
        assert_eq!(names, vec!["first", "second", "third"]);
    }

    const MEASURED_APP_UA: &str = "Mozilla/5.0 (0MB; 960x540; 160x160; 960x540; HTC unknown; \
                                   unknown) AppleWebKit/537.36 (KHTML, like Gecko)  ROBLOX Android \
                                   App 2.724.735 Phone Hybrid()  GooglePlayStore \
                                   RobloxApp/2.724.735 (GlobalDist; GooglePlayStore)";

    #[test]
    fn a_cookie_forced_helper_is_replaced_so_the_apps_user_agent_reaches_the_engine() {
        assert_eq!(
            respawn_verdict(Some(MEASURED_APP_UA), None, false, 0, true, 0),
            RespawnVerdict::Respawn
        );

        let lower = MEASURED_APP_UA.to_lowercase();
        assert!(
            lower.contains("hybrid"),
            "the app's UA must carry the Hybrid() token"
        );
        assert!(
            lower.contains("android"),
            "the app's UA must carry the android token"
        );
    }

    #[test]
    fn respawn_verdict_keeps_the_live_helper_for_every_recorded_reason() {
        let app = Some(MEASURED_APP_UA);

        assert!(matches!(
            respawn_verdict(app, None, true, 0, true, 0),
            RespawnVerdict::Keep(_)
        ));

        assert!(matches!(
            respawn_verdict(None, None, false, 0, true, 0),
            RespawnVerdict::Keep(_)
        ));

        assert!(matches!(
            respawn_verdict(app, app, false, 0, true, 0),
            RespawnVerdict::Keep(_)
        ));

        assert!(matches!(
            respawn_verdict(app, None, false, 1, true, 0),
            RespawnVerdict::Keep(_)
        ));

        assert!(matches!(
            respawn_verdict(app, None, false, 0, false, 0),
            RespawnVerdict::Keep(_)
        ));

        assert!(matches!(
            respawn_verdict(app, None, false, 0, true, 1),
            RespawnVerdict::Keep(_)
        ));

        assert!(matches!(
            respawn_verdict(app, None, true, 1, true, 1),
            RespawnVerdict::Keep(_)
        ));
    }

    #[test]
    fn cookie_log_replays_sets_and_clears_over_the_persistent_base_in_order() {
        let mut log = EarlyCookies::new();
        log.record_sent(&a_cookie_set("a"));
        log.record_sent(&a_cookie_set_cb(1));
        assert_eq!(log.mutations, vec![a_cookie_set("a"), a_cookie_set_cb(1)]);

        log.record_sent(&ConsumerMsg::CookieGet {
            request_id: 2,
            url: "https://www.roblox.com/".into(),
        });
        assert_eq!(log.mutations.len(), 2);
        let clear = ConsumerMsg::CookiesClear { request_id: 3 };
        log.record_sent(&clear);
        assert_eq!(
            log.mutations,
            vec![a_cookie_set("a"), a_cookie_set_cb(1), clear.clone()]
        );
        assert!(log.replayable);
        let clear_session = ConsumerMsg::CookiesClearSession { request_id: 4 };
        log.record_sent(&clear_session);
        assert_eq!(log.mutations.last(), Some(&clear_session));

        log.record_sent(&a_cookie_set("after"));
        assert_eq!(log.mutations.last(), Some(&a_cookie_set("after")));

        assert_eq!(log.mutations[4], a_cookie_set("after"));
    }

    #[test]
    fn cookie_log_overflow_and_retirement_refuse_the_respawn_instead_of_lying() {
        let mut log = EarlyCookies::new();
        for i in 0..EarlyCookies::CAP {
            log.record_sent(&a_cookie_set(&format!("c{i}")));
        }
        assert!(log.replayable);
        log.record_sent(&a_cookie_set("overflow"));
        assert_eq!(log.mutations.len(), EarlyCookies::CAP, "the bound holds");
        assert!(
            !log.replayable,
            "and the respawn is surrendered, not the bound"
        );
        assert!(matches!(
            respawn_verdict(Some(MEASURED_APP_UA), None, false, 0, log.replayable, 0),
            RespawnVerdict::Keep(_)
        ));

        let mut log = EarlyCookies::new();
        log.record_sent(&a_cookie_set("a"));
        log.retire();
        assert!(log.mutations.is_empty() && !log.replayable);
        log.record_sent(&a_cookie_set("auth-token-shaped"));
        assert!(
            log.mutations.is_empty(),
            "post-CreateView auth cookies must not be retained in the ART process"
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

    #[test]
    fn reader_exit_wakes_all_blocking_cookie_calls_immediately() {
        let (tx, rx) = mpsc::channel::<Vec<CookieEntry>>();
        let request_id = next_request_id();
        shared()
            .cookie_get_waiters
            .lock()
            .expect("waiters lock")
            .insert(request_id, tx);
        let (flush_tx, flush_rx) = mpsc::channel::<bool>();
        shared()
            .cookie_flush_waiters
            .lock()
            .expect("flush waiters lock")
            .insert(next_request_id(), flush_tx);
        wake_all_blocking_cookie_waiters();
        match rx.recv_timeout(Duration::from_millis(200)) {
            Err(mpsc::RecvTimeoutError::Disconnected) => {}
            other => panic!("expected an immediate Disconnected wake, got {other:?}"),
        }
        match flush_rx.recv_timeout(Duration::from_millis(200)) {
            Err(mpsc::RecvTimeoutError::Disconnected) => {}
            other => panic!("expected an immediate flush Disconnected wake, got {other:?}"),
        }
    }

    #[test]
    fn notify_view_freed_releases_pending_bridges_for_a_never_driven_view() {
        let widget = 0x5EED_0001_i64;
        buffer_pending_bridge(widget, "EclipseTest".into(), Vec::new());
        assert!(pending_bridges()
            .lock()
            .expect("pending lock")
            .contains_key(&widget));
        assert_ne!(PENDING_BRIDGE_VIEWS.load(Ordering::Relaxed), 0);
        assert!(!view_is_tracked(widget), "never driven — never tracked");
        notify_view_freed(widget);
        assert!(
            !pending_bridges()
                .lock()
                .expect("pending lock")
                .contains_key(&widget),
            "a never-driven view's buffered bridge inventory must be released at finalize"
        );
    }

    #[test]
    fn notify_view_detached_clears_only_the_active_view() {
        let active = 0x00A0_0001_0000_0000_i64;
        let other = 0x00B0_0002_0000_0000_i64;
        ACTIVE_VIEW.store(active, Ordering::Relaxed);

        notify_view_detached(other);
        assert_eq!(
            ACTIVE_VIEW.load(Ordering::Relaxed),
            active,
            "detaching a non-active view must not clear the active gate"
        );

        notify_view_detached(active);
        assert_eq!(
            ACTIVE_VIEW.load(Ordering::Relaxed),
            0,
            "detaching the active view clears ACTIVE_VIEW (composite gate off)"
        );

        notify_view_detached(active);
        assert_eq!(ACTIVE_VIEW.load(Ordering::Relaxed), 0);
        ACTIVE_VIEW.store(0, Ordering::Relaxed);
    }

    #[test]
    fn reader_loop_stays_jni_free_and_hands_bridge_drops_to_the_upcall_thread() {
        let src = include_str!("client.rs");
        let reader_start = src.find("fn reader_loop").expect("reader_loop present");
        let reader_end = src[reader_start..]
            .find("fn reader_fatal")
            .expect("reader_fatal follows reader_loop")
            + reader_start;
        let reader_body = &src[reader_start..reader_end];
        assert!(
            !reader_body.contains("drop_bridges_for"),
            "the reader thread must stay JNI-free (bridge drops belong to the upcall thread)"
        );
        assert!(
            !reader_body.contains("drain_eval_callbacks"),
            "the reader thread must stay JNI-free (eval drains belong to the upcall thread)"
        );
        let upcall_start = src
            .find("fn upcall_thread_main")
            .expect("upcall_thread_main present");
        let upcall_body = &src[upcall_start..reader_start];
        assert!(
            upcall_body.contains("drop_bridges_for_view_closed"),
            "the era-gated bridge drop must run on the upcall thread"
        );

        let banned = ["protocol ", "v1"].concat();
        assert!(
            !src.contains(&banned),
            "hardcoded protocol generation string found — log/document PROTO_VERSION instead"
        );
    }
}
