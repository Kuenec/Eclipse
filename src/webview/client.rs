//! Main-process client for the OUT-OF-PROCESS `eclipse-webview` CEF helper (plan M3).
//!
//! 2026-07-03: this module is the consumer side of protocol v1 ([`super::proto`]) — it spawns the
//! helper per the NORMATIVE spawn contract in [`super`]'s module docs (fd-3 socketpair +
//! `--ipc-fd=3`, PDEATHSIG, no URL ever in argv), completes the `Hello`/`HelloAck` handshake,
//! and runs a dedicated socket-reader thread (`eclipse-webview-io`) that stages memfd frames and
//! fires the `WebView.internalLoadChanged(0/3)` JNI upcalls via
//! [`crate::framework::fire_web_view_internal_load_changed`].
//!
//! This file is deliberately MAIN-PROCESS-ONLY: it is NOT part of the helper crate's shared
//! `#[path]` sibling set (`crates/eclipse-webview/src/shared.rs` includes only the five protocol
//! leaf modules). It stays cef-free (std + libc + the already-vendored `jni` handle type).
//!
//! # Design invariants (2026-07-03, plan M3)
//!
//! - **Per-view identity is the existing `view_registry` widget handle** — the protocol's `view`
//!   field IS that jlong ("integrates, never duplicates"; NO webview registry exists here).
//! - **Spawn from the reader thread** (the stable thread the PDEATHSIG contract demands): the
//!   `eclipse-webview-io` thread resolves the binary, spawns the helper, performs the handshake
//!   under the helper's 10 s watchdog, reports the result over an mpsc channel, and continues as
//!   the read loop — PDEATHSIG therefore fires exactly when the client tears down.
//! - **Staged frame consumption**: on a matching-generation `FrameReady` the reader copies the
//!   slot bytes into an owned per-view staging buffer (latest-wins, buffer reused) and only THEN
//!   sends `FrameAck` — the shm aliasing contract (read strictly between Ready and own Ack, all
//!   on one thread) is satisfied by construction, and the engine present thread never touches the
//!   socket or the mapping directly.
//! - **Failure latch, not respawn**: any helper-process-level failure (unresolvable binary, spawn
//!   error, handshake failure, crash, EOF, socket error) sets a process-lifetime `Failed` latch;
//!   every later drive takes the honest one-shot-WARN no-op path in the natives. Per-view
//!   `CloseView`/`ViewClosed` never latches (the helper stays alive for the app's ~60 s-timeout
//!   retry — plan open question #9's answer-for-now).
//! - **The ABSOLUTE URL-redaction rule**: `ViewShared.log_target` is written exactly once, at
//!   drive time, through [`super::redact::url_scheme_and_host_for_log`]; every log macro in this
//!   module binds ONLY that pre-redacted form (or payload-free typed errors). The full URL is
//!   retained solely for the wire and the Java `internalLoadChanged` argument (ATL's reference C
//!   passes the real URI to Java — the redaction rule governs logs, not the app's own contract).
//! - **Ozone selection stays helper-side** (D4): the client passes NO `--ozone-platform=` flag —
//!   the helper's own explicit, never-auto selection runs with the inherited environment, so
//!   there is exactly ONE selection point.

use std::collections::HashMap;
use std::io::Write as _;
use std::os::fd::AsFd as _;
use std::os::unix::net::UnixStream;
use std::path::{Path, PathBuf};
use std::process::Child;
use std::sync::atomic::{AtomicI64, AtomicUsize, Ordering};
use std::sync::mpsc;
use std::sync::{Arc, Mutex, OnceLock};
use std::thread::JoinHandle;
use std::time::{Duration, Instant};

use super::proto::{self, ConsumerMsg, HelperMsg};
use super::redact;
use super::{fdpass, shm};

/// The stable prefix of the unresolvable-helper error. Pinned as a `pub const` so the
/// `tests/engine_milestones.rs` self-skip guard and this Display can never drift apart.
pub const HELPER_NOT_FOUND_MARKER: &str = "helper binary not found";

/// The stable substring of the helper's engine-init-failure latch reason (`Crash { kind: 1 }` —
/// no display / ozone). Pinned for the same guard-skip reason as [`HELPER_NOT_FOUND_MARKER`].
pub const NO_DISPLAY_MARKER: &str = "no display connection";

/// How long the io thread lets the handshake read block. Matches the helper's own 10 s no-`Hello`
/// watchdog (proto module docs) — the two sides share one deadline notion.
const HANDSHAKE_TIMEOUT: Duration = Duration::from_secs(10);

/// How long a driving JNI thread waits for the io thread's spawn/handshake verdict: the 10 s
/// handshake watchdog plus spawn slack. Once per process (first drive only), ~ms when healthy.
const SPAWN_RESULT_TIMEOUT: Duration = Duration::from_secs(15);

/// Typed client error. Every variant carries an ACTIONABLE, payload-free message — these strings
/// reach the natives' one-shot WARN and `__webview-test` output, never a URL or page byte.
#[derive(Debug, Clone)]
pub enum ClientError {
    /// No helper binary at any tier of the documented resolution order.
    HelperNotFound { probed: Vec<PathBuf> },
    /// An EXPLICIT setting (config / env) pointed at a missing file — never silently skipped.
    ExplicitPathMissing { source: &'static str, path: PathBuf },
    /// Spawning the helper process (or its io thread) failed.
    Spawn(String),
    /// The `Hello`/`HelloAck` handshake failed or timed out.
    Handshake(String),
    /// The helper answered with an unsupported protocol version.
    VersionMismatch { helper_version: u16 },
    /// A message failed to encode (e.g. an over-cap `loadDataWithBaseURL` payload). Per-call:
    /// nothing was written, so this does NOT latch.
    Encode(proto::ProtoError),
    /// A previous failure latched the client — the honest no-op path (D5, no respawn).
    Latched(String),
    /// An internal soundness failure (poisoned lock / impossible state).
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
            Self::Handshake(e) => write!(f, "helper handshake failed: {e}"),
            Self::VersionMismatch { helper_version } => write!(
                f,
                "helper protocol version mismatch: helper v{helper_version}, consumer v{}",
                super::PROTO_V1
            ),
            Self::Encode(e) => write!(f, "message rejected before send: {e}"),
            Self::Latched(reason) => write!(f, "web engine helper previously failed: {reason}"),
            Self::Internal(what) => write!(f, "webview client internal error: {what}"),
        }
    }
}

impl std::error::Error for ClientError {}

// ---------------------------------------------------------------------------
// Global state (bounded; see the module docs)
// ---------------------------------------------------------------------------

/// The one helper-process slot. Spawn / control-socket writes / teardown are serialized here.
enum ClientSlot {
    /// No helper has been needed yet (lazy spawn on the first drive).
    Unspawned,
    /// The helper is running; the reader thread is live.
    Live(Client),
    /// Process-lifetime failure latch (D5) carrying the ONE actionable reason.
    Failed(String),
}

/// The live helper process handle (kept inside [`CLIENT`]).
///
/// 2026-07-03 deviation from the M3 design sketch: no `java_vm` field — the `jni::vm::JavaVM`
/// (verified `Send + Sync` in the pinned jni 0.22.4 source) is moved INTO the reader thread at
/// spawn, the only place upcalls happen, so the slot does not need a second copy.
struct Client {
    child: Child,
    /// A `try_clone` of the control socket for consumer→helper writes (the reader thread keeps
    /// the original for reads). ALL writes happen under the [`CLIENT`] mutex.
    writer: UnixStream,
    /// The `eclipse-webview-io` thread handle, joined by [`shutdown`] (bounded: the child's death
    /// forces the reader's EOF, so the join cannot hang).
    reader: Option<JoinHandle<()>>,
}

static CLIENT: Mutex<ClientSlot> = Mutex::new(ClientSlot::Unspawned);

/// The widget handle of the most recently driven (live) WebView; `0` = none. The cheap
/// present/input gate — one atomic load on every hot-path check (the `ACTIVE_TEXT_FIELD`
/// precedent, AGENTS.md §2.4).
static ACTIVE_VIEW: AtomicI64 = AtomicI64::new(0);

/// Count of tracked (driven, not yet closed) views — the [`notify_view_freed`] fast gate so
/// every normal view GC on the FinalizerDaemon thread pays one atomic load.
static LIVE_VIEWS: AtomicUsize = AtomicUsize::new(0);

/// State shared between the reader thread, the drive path, and the compositor.
struct Shared {
    /// One entry per live driven WebView (the challenge flow has exactly one).
    views: Mutex<HashMap<i64, ViewShared>>,
    /// The cached ABSOLUTE composite rect `(x, y, w, h)` (the `TEXTBOX_GEOM` pattern): written by
    /// [`update_composited_rect`] on the main thread, read by the vk-overlay present path.
    rect: Mutex<Option<(i32, i32, u32, u32)>>,
}

fn shared() -> &'static Arc<Shared> {
    static SHARED: OnceLock<Arc<Shared>> = OnceLock::new();
    SHARED.get_or_init(|| {
        Arc::new(Shared {
            views: Mutex::new(HashMap::new()),
            rect: Mutex::new(None),
        })
    })
}

/// 2026-07-03: [`shm::FrameMapping`] stores its mmap base as a `NonNull<u8>`, which makes it
/// auto-`!Send`; this newtype confines the one place M3 must move/hold it across threads (the
/// views map is written by the reader thread and try-locked by the present thread).
struct SendMapping(shm::FrameMapping);
// SAFETY: the mapping is a process-global `PROT_READ`/`MAP_SHARED` region unmapped exactly once
// in Drop; moving ownership between threads is sound (mmap regions are not thread-affine), and
// every byte read goes through `FrameMapping::slice`'s bounds check under the publish/ack
// aliasing contract. Only the reader thread reads slices; other threads see the staging COPY.
unsafe impl Send for SendMapping {}

/// The current generation's mapped frame buffer for one view.
struct FrameMap {
    mapping: SendMapping,
    generation: u32,
    width: u16,
    height: u16,
    stride: u32,
    slot_bytes: u32,
}

/// The owned staging copy of the latest published frame (latest-wins; buffer reused). What the
/// vk-overlay composite reads via [`with_latest_frame`] — never the shared mapping itself.
#[derive(Default)]
pub struct Stage {
    /// Tightly packed BGRA rows (`stride` bytes per row, `height` rows). Empty until the first
    /// frame stages.
    pub bytes: Vec<u8>,
    pub width: u32,
    pub height: u32,
    /// Source row stride in bytes (v1 fixes `4 * width`).
    pub stride: u32,
    pub generation: u32,
    /// The helper's frame sequence number; `0` = nothing staged yet.
    pub seq: u32,
}

/// Per-view client state.
struct ViewShared {
    /// The FULL driven URL (or `loadDataWithBaseURL` base) — the Java `internalLoadChanged`
    /// argument. NEVER bound to any log macro (module-doc redaction invariant).
    driven_url: String,
    /// The pre-redacted scheme+host form — the ONLY loggable target for this view.
    log_target: String,
    mapping: Option<FrameMap>,
    stage: Stage,
    started: bool,
    finished_http: Option<i32>,
    /// Count of SUCCESSFUL `internalLoadChanged` upcall completions (the `__webview-test`
    /// "upcalls 2/2" evidence).
    upcalls_ok: u32,
}

/// Load-progress observation for the `__webview-test` poll loop.
#[derive(Debug, Clone, Copy)]
pub struct LoadObserved {
    pub started: bool,
    pub finished_http: Option<i32>,
    pub upcalls_ok: u32,
}

/// What [`shutdown`] observed while tearing the helper down.
#[derive(Debug, Clone, Copy)]
pub struct ShutdownReport {
    /// The helper's exit code (`Some(0)` = the clean `Shutdown` path), `None` if it was never
    /// live or had to be signal-killed without a code.
    pub helper_exit: Option<i32>,
    pub reader_joined: bool,
}

// ---------------------------------------------------------------------------
// Helper-binary resolution (the spawn contract, tiers 1–4)
// ---------------------------------------------------------------------------

/// Pure, dependency-injected resolver (unit-testable without env mutation — env-var tests are
/// racy under parallel `cargo test`). Order per the spawn contract in [`super`]:
/// (1) config `webview_helper_path`, (2) `$ECLIPSE_WEBVIEW_HELPER` — both STRICT: set-but-missing
/// is an actionable error, never a silent fallthrough (the drive.rs env behavior) —
/// (3) a sibling `eclipse-webview` beside the running executable, (4) the dev-tree builds under
/// `crates/eclipse-webview/target/{release,debug}` (2026-07-03: a purely exe-relative dev-tree
/// convenience — `<exe_dir>/../..` is the checkout root when the exe is `target/<profile>/eclipse`,
/// so this works from any checkout path with no hardcoded location).
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

/// Resolve the helper binary from the real environment (config → env → sibling → dev-tree).
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

// ---------------------------------------------------------------------------
// Spawn + handshake (the io thread's startup half)
// ---------------------------------------------------------------------------

/// Spawn the helper per the spawn contract: socketpair end dup2'd to fd 3, argv `--ipc-fd=3`,
/// `PR_SET_PDEATHSIG(SIGTERM)` in `pre_exec`, NO URL in argv, NO ozone flag (D4 — the helper's
/// own explicit selection runs with the inherited environment).
fn spawn_helper_process() -> Result<(UnixStream, Child), ClientError> {
    use std::os::unix::process::CommandExt as _;

    let helper = resolve_helper()?;
    let (parent_end, child_end) =
        UnixStream::pair().map_err(|e| ClientError::Spawn(format!("socketpair failed: {e}")))?;
    let mut cmd = std::process::Command::new(&helper);
    cmd.arg("--ipc-fd=3");
    // 2026-07-03: the built helper has `NEEDED libcef.so` with NO RPATH/RUNPATH (readelf-verified
    // on the M2 artifact), and cef-dll-sys places libcef.so beside the helper binary — so the
    // child's LD_LIBRARY_PATH gets the RESOLVED binary's own directory prepended (detect the dir
    // it is actually in, don't assume an install path). If libcef is genuinely absent the exec
    // fails and the spawn error degrades honestly. The durable `$ORIGIN` RUNPATH is M5 packaging.
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
    // SAFETY (pre_exec runs between fork and exec — async-signal-safe calls only; copied from the
    // M2 reference consumer, crates/eclipse-webview/src/bin/drive.rs):
    // - dup2 moves the socketpair end onto fd 3 (the spawn contract); dup2 does not copy
    //   FD_CLOEXEC, so fd 3 survives exec. If the fd already IS 3, F_SETFD clears CLOEXEC.
    // - prctl(PR_SET_PDEATHSIG, SIGTERM) is the orphan-prevention secondary layer; it fires when
    //   the spawning THREAD exits — this runs on the dedicated `eclipse-webview-io` thread, which
    //   lives exactly as long as the client, so PDEATHSIG fires precisely on client teardown.
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
    Ok((parent_end, child))
}

/// Send `Hello` and gate on the `HelloAck` version — v1 requires an exact match. `timeout` is
/// injected so the unit pin runs without a 10 s sleep. On success the read timeout is cleared
/// (the reader loop uses plain blocking reads; EOF is its exit signal).
fn perform_handshake(stream: &UnixStream, timeout: Duration) -> Result<String, ClientError> {
    let hello = ConsumerMsg::Hello {
        version: super::PROTO_V1,
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
        // ProtoError Display is payload-free by construction, so folding it into the reason is
        // safe to log/latch verbatim.
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
    }
}

/// Spawn the io thread and wait (bounded) for its spawn+handshake verdict. Called with the
/// [`CLIENT`] lock held (spawn/teardown are serialized there). Once per process, on the first
/// drive; the JNI caller blocks ≤ [`SPAWN_RESULT_TIMEOUT`], ~ms when healthy (the handshake is
/// pre-engine-init on the helper side).
fn spawn_client(java_vm: jni::vm::JavaVM) -> Result<Client, ClientError> {
    let (tx, rx) = mpsc::channel::<Result<(UnixStream, Child), ClientError>>();
    let shared = Arc::clone(shared());
    let handle = std::thread::Builder::new()
        .name("eclipse-webview-io".into())
        .spawn(move || io_thread_main(&tx, &shared, java_vm))
        .map_err(|e| ClientError::Spawn(format!("io-thread spawn failed: {e}")))?;
    match rx.recv_timeout(SPAWN_RESULT_TIMEOUT) {
        Ok(Ok((writer, child))) => Ok(Client {
            child,
            writer,
            reader: Some(handle),
        }),
        Ok(Err(e)) => {
            // The io thread reported and exited (it already reaped any child it spawned).
            let _ = handle.join();
            Err(e)
        }
        // Receiver timeout: dropping `rx` makes the io thread's eventual send fail, and its
        // send-failure path kills + reaps the child — never an orphan.
        Err(_) => Err(ClientError::Handshake(
            "helper spawn/handshake verdict timed out".into(),
        )),
    }
}

/// The `eclipse-webview-io` thread body: spawn + handshake, report, then become the read loop.
fn io_thread_main(
    tx: &mpsc::Sender<Result<(UnixStream, Child), ClientError>>,
    shared: &Arc<Shared>,
    java_vm: jni::vm::JavaVM,
) {
    let (stream, mut child) = match spawn_helper_process() {
        Ok(x) => x,
        Err(e) => {
            let _ = tx.send(Err(e));
            return;
        }
    };
    match perform_handshake(&stream, HANDSHAKE_TIMEOUT) {
        Ok(engine) => {
            tracing::info!(%engine, "eclipse-webview helper handshake complete (protocol v1)");
        }
        Err(e) => {
            let _ = child.kill();
            let _ = child.wait();
            let _ = tx.send(Err(e));
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
    if let Err(mpsc::SendError(returned)) = tx.send(Ok((writer, child))) {
        // The driving thread timed out and dropped the receiver: recover the child and reap it.
        if let Ok((_w, mut c)) = returned {
            let _ = c.kill();
            let _ = c.wait();
        }
        return;
    }
    reader_loop(&stream, shared, &java_vm);
}

// ---------------------------------------------------------------------------
// The reader loop + the pure dispatch state machine
// ---------------------------------------------------------------------------

/// One `internalLoadChanged` upcall the reader must fire (outside all client locks).
struct Upcall {
    widget: i64,
    state: i32,
    /// The recorded driven URL — the Java argument (never bound to a log macro).
    url: String,
}

/// The pure output of [`dispatch`] — the unit-test surface.
#[derive(Default)]
struct DispatchOut {
    /// Replies to write back to the helper (only `FrameAck` in v1), in order AFTER the staging
    /// copy that justified them.
    replies: Vec<ConsumerMsg>,
    upcalls: Vec<Upcall>,
    /// Views removed by `ViewClosed` (the loop clears `ACTIVE_VIEW`/`LIVE_VIEWS` for these).
    closed: Vec<i64>,
    fatal: bool,
    /// The actionable latch reason when `fatal` (payload-free by construction).
    fatal_reason: Option<String>,
}

/// The pure per-message state machine: no I/O, no globals — everything it touches is the views
/// map it is handed (plain `cargo test` pins it with no helper/display/network).
/// `FrameBufferNew` is NOT routed here (its `SCM_RIGHTS` fd receive is I/O-coupled — the reader
/// loop handles it inline, per the sentinel-adjacency rule).
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
            // Unknown view: NEVER fabricate a callback — the driven-loads-only contract's
            // client-side gate (the helper already suppresses bootstrap loads at the source).
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
                    // Stale generation → skip, NO ack (the helper ignores stale acks anyway;
                    // acking a stale slot would double-release under the new generation).
                    if map.generation == generation {
                        let offset = map.slot_bytes as usize * usize::from(slot);
                        if let Some(src) = map.mapping.0.slice(offset, map.slot_bytes as usize) {
                            // Copy BETWEEN FrameReady and our own FrameAck — the shm aliasing
                            // window (D3): latest-wins into the reused staging buffer, THEN ack.
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
            // Structurally text-free (Console::from_raw + the decode re-redaction).
            tracing::debug!(
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
            out.fatal_reason = Some(match kind {
                // kind 1 = engine-init-failed (no display / ozone) per the proto spec.
                1 => format!(
                    "web engine init failed in the helper (crash kind=1 code={code}) — \
                     {NO_DISPLAY_MARKER} or ozone selection failure"
                ),
                k => format!("helper crash (view={view} kind={k} code={code})"),
            });
        }
        HelperMsg::ViewClosed { view } => {
            if views.remove(&view).is_some() {
                out.closed.push(view);
            }
        }
        // M4-shaped / out-of-phase messages: debug-ignore (v1 decodes them; M3 has no consumer).
        other @ (HelperMsg::HelloAck { .. }
        | HelperMsg::CookieList { .. }
        | HelperMsg::FrameBufferNew { .. }) => {
            tracing::debug!(
                msg = helper_msg_name(&other),
                "webview client: ignoring out-of-phase helper message"
            );
        }
    }
    out
}

/// The reader thread's steady state: decode helper messages on the RAW stream (NEVER a
/// `BufReader` — the byte after a `FrameBufferNew` frame is the fd sentinel, and a buffered
/// reader would swallow it and drop the fd; proto.rs module-doc rule), feed the pure state
/// machine, apply its outputs, and fire the JNI upcalls outside all client locks.
fn reader_loop(stream: &UnixStream, shared: &Arc<Shared>, java_vm: &jni::vm::JavaVM) {
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
            // The sentinel byte + SCM_RIGHTS memfd is the very next unread byte on the stream.
            let fd = match fdpass::recv_fd_after_sentinel(stream) {
                Ok(f) => f,
                Err(e) => {
                    reader_fatal(&format!("frame-buffer fd receive failed: {e}"));
                    return;
                }
            };
            let expected = slot_bytes as usize * usize::from(slot_count);
            // Detect-don't-assume: size + F_SEAL_SHRINK verified before mmap (shm guard).
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
                        // Replacing unmaps the previous generation; this single reader thread is
                        // the only slice reader, so no read can be in progress (drive.rs shape).
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
        let out = match shared.views.lock() {
            Ok(mut views) => dispatch(msg, &mut views),
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
        // Upcalls fire OUTSIDE the views/CLIENT locks: onPageStarted/onPageFinished is app code
        // that may synchronously re-enter drive_load_url / the registry.
        for up in out.upcalls {
            let fired = crate::framework::fire_web_view_internal_load_changed(
                java_vm, up.widget, up.state, &up.url,
            );
            if fired {
                if let Ok(mut views) = shared.views.lock() {
                    if let Some(vs) = views.get_mut(&up.widget) {
                        vs.upcalls_ok += 1;
                    }
                }
            }
        }
        for closed in out.closed {
            let _ = ACTIVE_VIEW.compare_exchange(closed, 0, Ordering::Relaxed, Ordering::Relaxed);
            LIVE_VIEWS.fetch_sub(1, Ordering::Relaxed);
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

/// One loud payload-free WARN + the D5 failure latch: replace a `Live` slot with `Failed`,
/// kill+wait the child (never leave the helper running), clear the present/input gate, exit.
/// A slot that is no longer `Live` (a deliberate [`shutdown`] took it, or a drive already
/// latched a write failure) keeps its state and gets only a quiet debug line — the expected
/// EOF after a clean shutdown is not a failure.
fn reader_fatal(reason: &str) {
    ACTIVE_VIEW.store(0, Ordering::Relaxed);
    if let Ok(mut slot) = CLIENT.lock() {
        if matches!(&*slot, ClientSlot::Live(_)) {
            tracing::warn!(
                reason,
                "eclipse-webview client: helper connection lost — latching the honest no-op \
                 path (no respawn; subsequent WebView loads degrade to the one-shot WARN)"
            );
            if let ClientSlot::Live(client) =
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

/// Write one reply frame under the [`CLIENT`] mutex. `true` = written or safely dropped (the
/// slot is no longer live — a shutdown raced the reply); `false` = a real write failure.
fn send_reply_if_live(msg: &ConsumerMsg) -> bool {
    let Ok(bytes) = msg.encode() else {
        return true; // a v1 FrameAck cannot exceed its cap; treat as droppable, not fatal
    };
    match CLIENT.lock() {
        Ok(slot) => match &*slot {
            ClientSlot::Live(c) => (&mut &c.writer).write_all(&bytes).is_ok(),
            _ => true,
        },
        Err(_) => false,
    }
}

// ---------------------------------------------------------------------------
// The drive path (called from the WebView load natives)
// ---------------------------------------------------------------------------

/// The D5 latch check — the FIRST thing a drive does. Pure over the slot so the degradation
/// contract is pinned in plain `cargo test` (a live `JavaVM` cannot be constructed in-harness).
fn latched_error(slot: &ClientSlot) -> Option<ClientError> {
    match slot {
        ClientSlot::Failed(reason) => Some(ClientError::Latched(reason.clone())),
        _ => None,
    }
}

/// Record (or re-record) the driven view BEFORE any send, so no `LoadState` can beat the record.
/// `log_target` is derived here — the ONE place — via the shared redaction contract. Returns
/// `true` when the view is new (the caller must send `CreateView` first).
fn record_view(views: &mut HashMap<i64, ViewShared>, widget: i64, driven_url: String) -> bool {
    let log_target = redact::url_scheme_and_host_for_log(&driven_url);
    match views.entry(widget) {
        std::collections::hash_map::Entry::Occupied(mut e) => {
            let vs = e.get_mut();
            vs.driven_url = driven_url;
            vs.log_target = log_target;
            // A re-drive is a new load: reset the per-load observations (upcall count stays
            // cumulative — the __webview-test evidence).
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
                upcalls_ok: 0,
            });
            true
        }
    }
}

/// Send one frame while holding the [`CLIENT`] lock; a WRITE failure latches (kill+wait, slot →
/// `Failed`) because the stream is no longer trustworthy. An ENCODE failure does not latch —
/// nothing was written, the stream is still framed correctly.
fn send_locked(slot: &mut ClientSlot, msg: &ConsumerMsg) -> Result<(), ClientError> {
    let bytes = msg.encode().map_err(ClientError::Encode)?;
    let write_result = match &*slot {
        ClientSlot::Live(c) => (&mut &c.writer).write_all(&bytes),
        _ => return Err(ClientError::Internal("send on a non-live client slot")),
    };
    if let Err(e) = write_result {
        let reason = format!("control-socket write failed: {}", e.kind());
        tracing::warn!(
            reason,
            "eclipse-webview client: latching the honest no-op path (no respawn)"
        );
        ACTIVE_VIEW.store(0, Ordering::Relaxed);
        if let ClientSlot::Live(client) =
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

/// What a drive forwards.
enum DriveTarget {
    Url(String),
    Data {
        base_url: Option<String>,
        data: String,
        mime: Option<String>,
        encoding: Option<String>,
    },
}

/// The common drive body: latch check → lazy spawn (D2) → record-before-send → `CreateView`
/// (new views) → the load message. `width`/`height` are the caller's best-known dims (already
/// clamped to `1..=u16::MAX` by the native).
fn drive(
    java_vm: jni::vm::JavaVM,
    widget: i64,
    target: DriveTarget,
    width: u16,
    height: u16,
) -> Result<(), ClientError> {
    let mut slot = CLIENT
        .lock()
        .map_err(|_| ClientError::Internal("client lock poisoned"))?;
    if let Some(e) = latched_error(&slot) {
        return Err(e);
    }
    if matches!(&*slot, ClientSlot::Unspawned) {
        match spawn_client(java_vm) {
            Ok(client) => *slot = ClientSlot::Live(client),
            Err(e) => {
                *slot = ClientSlot::Failed(e.to_string());
                return Err(e);
            }
        }
    }
    // The driven URL for the upcall contract: the URL itself, or the loadData base (the installed
    // Java hardcodes "about:blank" on the loadData route — the Android semantics for a null base).
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
    }
    // Record BEFORE any send: from here a LoadState for this widget resolves to a driven view.
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
            // 2026-07-03: the installed dex's native carries NO historyUrl parameter (4 strings —
            // WebView.smali:48); the Java layer already hardcodes history "about:blank" on the
            // loadData route, so v1's field is forwarded empty.
            history_url: String::new(),
        },
    };
    send_locked(&mut slot, &load_msg)
}

/// Forward `WebView.loadUrl` to the helper (spawning it lazily on the first drive). The full
/// `url` crosses the wire and is recorded as the upcall argument — it is never logged here.
pub fn drive_load_url(
    java_vm: jni::vm::JavaVM,
    widget: i64,
    url: String,
    width: u16,
    height: u16,
) -> Result<(), ClientError> {
    drive(java_vm, widget, DriveTarget::Url(url), width, height)
}

/// Forward `WebView.loadDataWithBaseURL` to the helper. `None` fields map to the Android null
/// semantics (`base_url` → `"about:blank"`, `mime`/`encoding` → empty). The `data` payload
/// crosses the wire only (8 MiB proto cap) — never a log macro.
#[allow(clippy::too_many_arguments)] // 2026-07-03: mirrors the 5-arg Android native + dims.
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

// ---------------------------------------------------------------------------
// Hot-path gates + compositor/input surface
// ---------------------------------------------------------------------------

/// The most recently driven live WebView's widget handle (`0` = none) — the one-atomic-load
/// present/input gate.
pub fn active_view() -> i64 {
    ACTIVE_VIEW.load(Ordering::Relaxed)
}

/// The cached ABSOLUTE composite rect of the active WebView, or `None` (the composite then
/// falls back to a centered rect of the staged frame's own dimensions).
pub fn composited_rect() -> Option<(i32, i32, u32, u32)> {
    shared().rect.lock().ok().and_then(|r| *r)
}

/// Refresh the cached composite rect from the view registry (main-thread cadence — called from
/// `graphics::about_to_wait` once per loop iteration while a WebView is live; the TEXTBOX_GEOM
/// pattern, so the engine present thread never walks the registry tree).
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

/// Run `f` against the latest STAGED frame of `view`. `try_lock` — contention (the reader is
/// mid-staging) skips this present rather than stalling the engine present thread. `None` when
/// nothing is staged yet.
pub fn with_latest_frame<R>(view: i64, f: impl FnOnce(&Stage) -> R) -> Option<R> {
    let views = shared().views.try_lock().ok()?;
    let vs = views.get(&view)?;
    if vs.stage.seq == 0 {
        return None;
    }
    Some(f(&vs.stage))
}

/// Write one input frame if the helper is live; a quiet no-op otherwise (input while degraded
/// must never crash or log per-event).
fn send_input(msg: &ConsumerMsg) {
    if let Ok(mut slot) = CLIENT.lock() {
        if matches!(&*slot, ClientSlot::Live(_)) {
            let _ = send_locked(&mut slot, msg);
        }
    }
}

/// Mouse move at VIEW-RELATIVE `(x, y)`.
pub fn send_mouse_move(view: i64, x: i32, y: i32) {
    send_input(&ConsumerMsg::MouseMove {
        view,
        x,
        y,
        modifiers: 0,
        leave: false,
    });
}

/// Left-button press/release at VIEW-RELATIVE `(x, y)`.
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

/// Vertical wheel scroll (`delta_y` in pixels) at VIEW-RELATIVE `(x, y)`.
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

/// Key event: `kind` 0=down, 1=up, 2=char (the `cef_key_event_t` set); `character` is one UTF-16
/// unit (char events). Contents are never logged (house privacy rule).
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

// ---------------------------------------------------------------------------
// Lifecycle (teardown, GC hook, __webview-test observation)
// ---------------------------------------------------------------------------

/// `View.native_destructor` hook: a driven WebView was garbage-collected. Runs on ART's
/// FinalizerDaemon thread — never panics/throws; the fast path for every NORMAL view GC is the
/// two atomic loads on the first line. Sends a best-effort `CloseView` (per-view close never
/// latches by policy — D5).
pub fn notify_view_freed(widget: i64) {
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
        if matches!(&*slot, ClientSlot::Live(_)) {
            let _ = send_locked(&mut slot, &ConsumerMsg::CloseView { view: widget });
        }
    }
}

/// Ask the helper to close `widget`'s browser; the confirming `ViewClosed` removes the local
/// entry (observe via [`view_is_tracked`]).
pub fn close_view(widget: i64) -> Result<(), ClientError> {
    let mut slot = CLIENT
        .lock()
        .map_err(|_| ClientError::Internal("client lock poisoned"))?;
    if let Some(e) = latched_error(&slot) {
        return Err(e);
    }
    send_locked(&mut slot, &ConsumerMsg::CloseView { view: widget })
}

/// Whether `view` still has a live client entry (`ViewClosed`/finalization removes it).
pub fn view_is_tracked(view: i64) -> bool {
    shared()
        .views
        .lock()
        .ok()
        .is_some_and(|v| v.contains_key(&view))
}

/// The load/upcall observations for `view` (the `__webview-test` poll surface).
pub fn load_observed(view: i64) -> Option<LoadObserved> {
    let views = shared().views.lock().ok()?;
    let vs = views.get(&view)?;
    Some(LoadObserved {
        started: vs.started,
        finished_http: vs.finished_http,
        upcalls_ok: vs.upcalls_ok,
    })
}

/// The latch reason, if the client has failed (`__webview-test` bails early + honestly on this).
pub fn failed_reason() -> Option<String> {
    match CLIENT.lock() {
        Ok(slot) => match &*slot {
            ClientSlot::Failed(reason) => Some(reason.clone()),
            _ => None,
        },
        Err(_) => None,
    }
}

/// Deliberate teardown: polite `Shutdown` → bounded wait → kill+wait → join the reader (bounded:
/// the child's death forces the reader's EOF). The slot latches so no later drive respawns.
pub fn shutdown(deadline: Duration) -> ShutdownReport {
    let taken = match CLIENT.lock() {
        Ok(mut slot) => {
            match std::mem::replace(
                &mut *slot,
                ClientSlot::Failed("the web engine helper was shut down".into()),
            ) {
                ClientSlot::Live(c) => Some(c),
                other => {
                    // Never-live (or already failed): keep the original state.
                    *slot = other;
                    None
                }
            }
        }
        Err(_) => None,
    };
    ACTIVE_VIEW.store(0, Ordering::Relaxed);
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
    if let Ok(mut views) = shared().views.lock() {
        views.clear();
    }
    if let Ok(mut rect) = shared().rect.lock() {
        *rect = None;
    }
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

    /// A unique per-test temp dir (portable — `std::env::temp_dir()`, no hardcoded paths).
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
        // 2026-07-03: pins the spawn contract's 4-tier resolution order (config → env → sibling →
        // dev-tree) with the STRICT explicit-setting rule (set-but-missing errors, never falls
        // through) and the all-paths-probed actionable final error. Dependency-injected — no env
        // mutation (racy under parallel cargo test).
        let root = temp_dir("resolve");
        // A fake checkout layout: <root>/target/debug/eclipse + the dev-tree helper builds.
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

        // Tier order: config beats env (both present + existing).
        let got = resolve_helper_from(
            Some(&config_helper),
            Some(env_helper.as_os_str()),
            Some(&exe),
        )
        .expect("config tier resolves");
        assert_eq!(got, config_helper);
        // Env beats sibling/dev-tree when no config is set.
        touch(&sibling);
        let got = resolve_helper_from(None, Some(env_helper.as_os_str()), Some(&exe))
            .expect("env tier resolves");
        assert_eq!(got, env_helper);
        // Sibling beats dev-tree.
        touch(&dev_release_dir.join("eclipse-webview"));
        let got = resolve_helper_from(None, None, Some(&exe)).expect("sibling tier resolves");
        assert_eq!(got, sibling);
        // Dev-tree release beats debug; then debug alone.
        std::fs::remove_file(&sibling).expect("rm sibling");
        let got = resolve_helper_from(None, None, Some(&exe)).expect("dev release resolves");
        assert!(got.ends_with("crates/eclipse-webview/target/release/eclipse-webview"));
        std::fs::remove_file(dev_release_dir.join("eclipse-webview")).expect("rm release");
        touch(&dev_debug_dir.join("eclipse-webview"));
        let got = resolve_helper_from(None, None, Some(&exe)).expect("dev debug resolves");
        assert!(got.ends_with("crates/eclipse-webview/target/debug/eclipse-webview"));

        // STRICT explicit settings: a set-but-missing config/env path is an error NAMING the
        // path — it must never silently fall through to a lower tier.
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

        // Nothing anywhere: the final error carries the marker + EVERY probed path + the fix.
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
    fn webview_client_handshake_gates_on_hello_ack_version() {
        // 2026-07-03: loopback socketpair — no helper binary, no display, no network. The
        // deadline is injected so no test path sleeps 10 s.
        let deadline = Duration::from_secs(2);

        // Correct HelloAck v1 → Ok(engine).
        let (client_end, helper_end) = UnixStream::pair().expect("pair");
        let ack = HelperMsg::HelloAck {
            version: super::super::PROTO_V1,
            engine: "cef/test".into(),
        }
        .encode()
        .expect("encode ack");
        (&mut &helper_end).write_all(&ack).expect("write ack");
        let engine = perform_handshake(&client_end, deadline).expect("v1 handshake");
        assert_eq!(engine, "cef/test");
        // The Hello frame reached the helper side (the consumer's half of the contract).
        let hello = proto::read_consumer_msg(&mut &helper_end).expect("decode Hello");
        assert_eq!(
            hello,
            ConsumerMsg::Hello {
                version: super::super::PROTO_V1
            }
        );

        // Unsupported version → the typed mismatch (the consumer-side exact-version gate).
        let (client_end, helper_end) = UnixStream::pair().expect("pair");
        let ack = HelperMsg::HelloAck {
            version: super::super::PROTO_V1 + 1,
            engine: "cef/future".into(),
        }
        .encode()
        .expect("encode future ack");
        (&mut &helper_end).write_all(&ack).expect("write ack");
        match perform_handshake(&client_end, deadline) {
            Err(ClientError::VersionMismatch { helper_version }) => {
                assert_eq!(helper_version, super::super::PROTO_V1 + 1);
            }
            other => panic!("expected VersionMismatch, got {other:?}"),
        }

        // Garbage instead of HelloAck → a typed handshake error via the total decoder (its
        // Display is payload-free, so latching/logging it verbatim is safe).
        let (client_end, helper_end) = UnixStream::pair().expect("pair");
        let mut junk = Vec::new();
        junk.extend_from_slice(&2u32.to_le_bytes());
        junk.push(0x7F); // unknown type in the helper→consumer direction
        junk.push(0xAA);
        (&mut &helper_end).write_all(&junk).expect("write junk");
        match perform_handshake(&client_end, deadline) {
            Err(ClientError::Handshake(reason)) => {
                assert!(reason.contains("0x7F"), "reason: {reason}");
            }
            other => panic!("expected Handshake error, got {other:?}"),
        }
    }

    /// Build a driven-view map entry + a real (memfd-backed) frame mapping for dispatch tests.
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
        // 2026-07-03: the pure dispatch state machine — the driven-loads-only client gate + the
        // D3 staging/ack ordering, with no helper/display/network.
        let mut views: HashMap<i64, ViewShared> = HashMap::new();
        let widget = 0x0000_0001_0000_0000_i64;
        let payload = tracked_view_with_mapping(
            &mut views,
            widget,
            "https://apps.roblox.com/challenge?t=x",
            7,
        );

        // LoadState for an UNKNOWN view: no upcall is ever fabricated.
        let out = dispatch(
            HelperMsg::LoadState {
                view: 999,
                state: 0,
                http_status: 0,
            },
            &mut views,
        );
        assert!(out.upcalls.is_empty() && out.replies.is_empty() && !out.fatal);

        // LoadState for the driven view: exactly one upcall carrying the recorded driven URL.
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

        // A STALE-generation FrameReady: no staging, NO ack.
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

        // A MATCHING FrameReady: staging updated (the copy) THEN exactly one FrameAck reply —
        // the D3 order (the ack must never precede the copy).
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

        // Crash: fatal with a payload-free reason, and never an upcall.
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

        // ViewClosed removes the entry and reports it (the loop clears the atomics).
        let out = dispatch(HelperMsg::ViewClosed { view: widget }, &mut views);
        assert_eq!(out.closed, vec![widget]);
        assert!(!views.contains_key(&widget));
    }

    #[test]
    fn client_log_bindings_are_scheme_and_host_only_at_the_ipc_boundary() {
        // 2026-07-03: the redaction contract EXTENDED to the IPC boundary — the recorded
        // `log_target` (the only loggable form) is scheme+host, while `driven_url` keeps the
        // full string for the wire + the Java upcall argument (ATL's reference C passes the
        // real URI to internalLoadChanged; redaction governs logs, not the app's contract).
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

        // The dispatch upcall carries the FULL driven URL (the Java argument) — and nothing
        // else in the dispatch output carries URL text (Crash reasons are format-string-only).
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

        // A loadData-style about:blank base redacts to the shared NON_URL literal.
        assert!(!record_view(&mut views, widget, "about:blank".to_string()));
        assert_eq!(views.get(&widget).unwrap().log_target, redact::NON_URL);
    }

    #[test]
    fn webview_client_degrades_to_the_warn_noop_after_failure_latch() {
        // 2026-07-03: the D5 failure latch — a Failed slot yields the latched error carrying
        // the ORIGINAL actionable reason, before any spawn/record/send work (drive() checks
        // this first, so no state is mutated). Pinned on the pure helper because a live
        // `jni::vm::JavaVM` cannot be constructed under the cargo-test harness.
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
        // Non-failed slots never produce a latch error (Unspawned drives spawn; Live drives send).
        assert!(latched_error(&ClientSlot::Unspawned).is_none());
    }
}
