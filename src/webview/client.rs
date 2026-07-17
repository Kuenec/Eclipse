//! Main-process client for the OUT-OF-PROCESS `eclipse-webview` CEF helper (plan M3).
//!
//! 2026-07-03: this module is the consumer side of the owned wire protocol ([`super::proto`];
//! the negotiated version is [`super::PROTO_VERSION`] — v2 since the 2026-07-09 M4 additive
//! extension) — it spawns the helper per the NORMATIVE spawn contract in [`super`]'s module docs
//! (fd-3 socketpair + `--ipc-fd=3`, PDEATHSIG, no URL ever in argv), completes the
//! `Hello`/`HelloAck` handshake, and runs a dedicated socket-reader thread (`eclipse-webview-io`)
//! that stages memfd frames.
//! 2026-07-09 (M4 fix): every JNI upcall that executes APP code — `internalLoadChanged`,
//! `@JavascriptInterface` bridge invokes, ValueCallback deliveries — runs on a SEPARATE
//! `eclipse-webview-upcall` thread fed by an in-order mpsc channel, NEVER on the reader thread:
//! app code may synchronously re-enter a blocking native (`CookieManager.getCookie`) whose reply
//! only the reader can deliver, which self-deadlocked the io loop for the full timeout and then
//! returned a wrong empty result when upcalls ran inline on the reader.
//! 2026-07-10 fix: the reader thread is now fully JNI-FREE — even the non-app JNI of dropping
//! retained `Global`s on a helper-confirmed `ViewClosed` moved to the upcall thread, because
//! jni 0.22.4 `Global::drop` on an unattached thread performs a scoped
//! AttachCurrentThread/DetachCurrentThread per ref, and an ART suspend-all pause during that
//! attach could stall the io loop until the helper's bounded outbox declared the consumer dead.
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
use std::sync::atomic::{AtomicI64, AtomicU32, AtomicUsize, Ordering};
use std::sync::mpsc;
use std::sync::{Arc, Mutex, OnceLock};
use std::thread::JoinHandle;
use std::time::{Duration, Instant};

use super::proto::{self, BridgeMethod, ConsumerMsg, CookieEntry, HelperMsg};
use super::redact;
use super::{fdpass, hostprobe, shm};

/// The stable prefix of the unresolvable-helper error. Pinned as a `pub const` so the
/// `tests/engine_milestones.rs` self-skip guard and this Display can never drift apart.
pub const HELPER_NOT_FOUND_MARKER: &str = "helper binary not found";

/// The stable substring of the helper's engine-init-failure latch reason (`Crash { kind: 1 }` —
/// no display / ozone). Pinned for the same guard-skip reason as [`HELPER_NOT_FOUND_MARKER`].
pub const NO_DISPLAY_MARKER: &str = "no display connection";

/// The stable substring of the helper's sandbox-refusal latch reason (`Crash { kind: 1,
/// code: 2 }` — 2026-07-10, plan M5: the host has neither unprivileged userns nor a SUID
/// `chrome-sandbox` and `webview_allow_unsandboxed` is off). Pinned for the same guard-skip
/// reason as the two markers above (a host genuinely lacking both sandbox tiers is an env
/// limitation, like no-display). Byte-matches the helper's `SandboxUnavailable` Display prefix.
pub const SANDBOX_UNAVAILABLE_MARKER: &str = "sandbox unavailable";

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
                super::PROTO_VERSION
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
    /// No helper has been needed yet (lazy spawn), carrying the cookie ops deferred so far
    /// (2026-07-16, the §6 🩹➜⛔ ordering fix: `CefSettings.user_agent` is GLOBAL and consumed by
    /// `CefInitialize`, so the spawn must not happen before the app has set its UA).
    Unspawned(EarlyCookies),
    /// The helper is running; the reader thread is live. 2026-07-16 (the §6 respawn): it CARRIES
    /// the cookie log forward — the frames Eclipse has sent to THIS helper, which (while no
    /// `CreateView` has been sent) are its entire store. [`maybe_respawn_for_app_ua`] replays them
    /// into a replacement.
    Live(Client, EarlyCookies),
    /// Process-lifetime failure latch (D5) carrying the ONE actionable reason. 2026-07-16 (the §6
    /// respawn): also the TRANSIENT park state of a respawn ([`RESPAWN_IN_PROGRESS`]) — a latch by
    /// construction, so no thread can spawn a second helper while the first still holds CEF's
    /// `root_cache_path` process-singleton lock.
    Failed(String),
}

/// The reason string a respawn parks the slot with while the old helper is torn down WITHOUT the
/// [`CLIENT`] lock (2026-07-16, the §6 respawn). Compared BY VALUE in phase 3, so a real failure
/// that races the swap wins and the log dies with it rather than resurrecting a helper nobody
/// wants. [`shutdown`] deliberately does not restore it (see there).
const RESPAWN_IN_PROGRESS: &str =
    "the web engine helper is being REPLACED so the User-Agent the app set via \
     WebSettings.setUserAgentString reaches the engine (CefSettings.user_agent is global and \
     consumed by CefInitialize) — this op arrived inside the swap window and degrades honestly \
     rather than being answered from a store that is mid-move (§6 2026-07-16 respawn)";

/// The live helper process handle (kept inside [`CLIENT`]).
///
/// 2026-07-03 deviation from the M3 design sketch: no `java_vm` field — the `jni::vm::JavaVM`
/// (verified `Send + Sync` in the pinned jni 0.22.4 source) is moved into the upcall thread at
/// spawn (2026-07-09: previously the reader thread — upcalls moved off it, see the module docs),
/// the only place upcalls happen, so the slot does not need a second copy.
struct Client {
    child: Child,
    /// A `try_clone` of the control socket for consumer→helper writes (the reader thread keeps
    /// the original for reads). ALL writes happen under the [`CLIENT`] mutex.
    writer: UnixStream,
    /// The `eclipse-webview-io` thread handle, joined by [`shutdown`] (bounded: the child's death
    /// forces the reader's EOF, so the join cannot hang).
    reader: Option<JoinHandle<()>>,
    /// The `eclipse-webview-upcall` thread handle, joined by [`shutdown`] AFTER the reader
    /// (bounded: the reader's exit drops the channel sender, so the upcall loop terminates once
    /// its queued events — ending in the drain of every pending ValueCallback — are processed).
    upcall: Option<JoinHandle<()>>,
}

static CLIENT: Mutex<ClientSlot> = Mutex::new(ClientSlot::Unspawned(EarlyCookies::new()));

/// The widget handle of the most recently driven (live) WebView; `0` = none. The cheap
/// present/input gate — one atomic load on every hot-path check (the `ACTIVE_TEXT_FIELD`
/// precedent, AGENTS.md §2.4).
static ACTIVE_VIEW: AtomicI64 = AtomicI64::new(0);

/// Count of tracked (driven, not yet closed) views — the [`notify_view_freed`] fast gate so
/// every normal view GC on the FinalizerDaemon thread pays one atomic load.
static LIVE_VIEWS: AtomicUsize = AtomicUsize::new(0);

/// Consumer-allocated correlation ids for the v2 request/reply pairs (`evaluateJavascript`,
/// 3-arg `setCookie`, `getCookie`, `removeAll/SessionCookies`). Monotonic, skips 0 (`0` is a
/// sentinel in several native paths). Bridge calls use a helper-allocated `call_id` instead — no
/// consumer id — so the two id spaces never collide.
static NEXT_REQUEST_ID: AtomicU32 = AtomicU32::new(1);

/// A consumer-allocated request id (monotonic; never 0). 2026-07-09 (plan M4).
pub fn next_request_id() -> u32 {
    loop {
        let id = NEXT_REQUEST_ID.fetch_add(1, Ordering::Relaxed);
        if id != 0 {
            return id;
        }
    }
}

/// The User-Agent the app set via `WebSettings.setUserAgentString`, or `None` when it never set one
/// (2026-07-16, plan M6 — the §6 2026-07-16 💥 fix). THE ONE SOURCE OF TRUTH for the app's UA: both
/// the engine side (forwarded to the helper at spawn by [`spawn_helper_process`]) and the Java side
/// (`framework.rs`'s `native_getUserAgentString`) read exactly this, so M4's recorded byte-match
/// contract — `getUserAgentString()` returns what CEF actually sends — holds by construction rather
/// than by two literals being kept in sync.
///
/// PROCESS-WIDE, deliberately, and this is a DIVERGENCE FROM AOSP recorded honestly: AOSP's
/// `WebSettings` is per-WebView state. ATL's is not and cannot be — its `WebSettings` has ZERO
/// instance fields and `WebView.getSettings()` returns a FRESH THROWAWAY on every call
/// (`new-instance` + `<init>` + `return`, verified against the installed dex 2026-07-16), so the
/// canonical `webView.getSettings().setUserAgentString(ua)` writes to an object that is immediately
/// garbage with no back-reference to its WebView. Storing per-instance would therefore require
/// inventing a WebSettings→WebView association ATL does not have. A process-wide store is CORRECT
/// here for two independent reasons: ATL's WebSettings is genuinely stateless/per-call, so there is
/// no per-instance state to be wrong about; and Eclipse drives exactly ONE challenge WebView, whose
/// UA is what `CefSettings.user_agent` — itself GLOBAL and fixed at `CefInitialize` — carries. If
/// Eclipse ever drives two WebViews with different UAs, this is the seam that must grow a key, and
/// the engine side would need a per-browser UA channel first (CEF has none at the settings layer).
static APP_USER_AGENT: Mutex<Option<String>> = Mutex::new(None);

/// Set once [`spawn_helper_process`] has read [`APP_USER_AGENT`] into the child's environment: past
/// that point the helper's GLOBAL `CefSettings.user_agent` is fixed (it is consumed by
/// `CefInitialize`), so a later `setUserAgentString` can no longer reach the engine. One atomic —
/// no lock is taken against [`CLIENT`] from the setter, so no lock-order inversion with the spawn
/// path (which holds [`CLIENT`] while reading [`APP_USER_AGENT`]) is possible.
static HELPER_UA_FIXED: std::sync::atomic::AtomicBool = std::sync::atomic::AtomicBool::new(false);

/// Normalize an app-supplied User-Agent per AOSP's documented contract (2026-07-16): *"Sets the
/// WebView's user-agent string. If the string is null **or empty**, the system default value will
/// be used."* (AOSP `frameworks/base/core/java/android/webkit/WebSettings.java`
/// `setUserAgentString`, verified 2026-07-16). `None` here therefore means "use Eclipse's fallback"
/// for BOTH an explicit `null` and an empty string — a reset, not an empty UA. Pure/unit-pinned:
/// the empty rule is easy to drop and would silently make Eclipse send an empty UA where AOSP sends
/// the default.
fn normalize_app_user_agent(ua: Option<String>) -> Option<String> {
    ua.filter(|s| !s.is_empty())
}

/// Record the User-Agent the app set via `WebSettings.setUserAgentString` (2026-07-16, plan M6).
/// `None`/empty resets to Eclipse's fallback, per AOSP ([`normalize_app_user_agent`]).
///
/// Logs the UA in full at INFO — this is the fix's observability, and it replaces the 2026-07-16
/// overlay `ECLIPSE-UA-SET` smali diagnostic it supersedes (one place now logs it, and it is the
/// same place that stores it, so the log can never disagree with what Eclipse will present). A UA
/// is the app's own public product token — sent in cleartext to every server on every request by
/// design — so it is neither a URL nor a secret and the ABSOLUTE URL-redaction rule does not reach
/// it; full text (not a length) is deliberate, because Eclipse must present this string EXACTLY and
/// a byte count could not be checked against what CEF sends.
pub fn set_app_user_agent(ua: Option<String>) {
    let ua = normalize_app_user_agent(ua);
    // 2026-07-16 PROBE (`ECLIPSE_WEBVIEW_DEFER_COOKIE_CB`) — THE MEASUREMENT. Reaching this line
    // with a 3-arg setCookie reply still outstanding is the app DEMONSTRATING, not asserting, that
    // it does not block on that callback: the very thing the deferral was rejected on an unmeasured
    // assumption about (`EarlyCookies`). `FIRST_DEFER_AT` is set only by the probe, so this is
    // structurally dead on a default boot.
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
    // Read the fixed-flag BEFORE storing: the store is still worth doing (the Java-side getter must
    // report what the app set either way — that is the app's own contract), but the engine can no
    // longer honor it, and a SILENT discard is precisely the bug being fixed here. So say so.
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
        // A poisoned lock means a panic while holding it; the honest degradation is the fallback UA
        // (an unfaithful-but-loud UA), never a fabricated one.
        Err(_) => tracing::warn!(
            target: "android.webkit.WebSettings",
            "setUserAgentString: the app-UA store is poisoned — Eclipse's fallback UA stands"
        ),
    }
}

/// The User-Agent the app set via `WebSettings.setUserAgentString`, or `None` if it never set one
/// (2026-07-16, plan M6). Read by the Java-side `native_getUserAgentString` and by
/// [`spawn_helper_process`].
pub fn app_user_agent() -> Option<String> {
    APP_USER_AGENT.lock().ok().and_then(|s| s.clone())
}

/// The User-Agent the CURRENT helper process was spawned with — i.e. what its
/// `CefSettings.user_agent` actually holds (2026-07-16, plan M6, the §6 respawn). `None` = it booted
/// on the helper's own fallback literal because the app had set no UA yet.
///
/// Written by [`spawn_helper_process`] from the SAME [`app_user_agent`] value it puts into the
/// child's environment, at the same instant — one read, one record, so the two cannot disagree. That
/// is the same byte-match-by-construction discipline the §6 💥 fix used for [`APP_USER_AGENT`], and
/// it matters here because `spawn_helper_process` runs on the io thread while the app's UA can be
/// written concurrently from a JNI thread: only the value the child ACTUALLY received is a truthful
/// answer to "what does this engine present?".
///
/// Lock order: the spawn's write is serialized by [`CLIENT`] (the thread in `ensure_spawned` holds
/// it and is parked on the io thread's verdict), and [`respawn_verdict`]'s caller holds [`CLIENT`]
/// while reading. Nothing ever takes this lock and THEN [`CLIENT`], so no inversion is possible.
static HELPER_BOOT_UA: Mutex<Option<String>> = Mutex::new(None);

/// The User-Agent the live helper booted with ([`HELPER_BOOT_UA`]).
fn helper_boot_ua() -> Option<String> {
    HELPER_BOOT_UA.lock().ok().and_then(|s| s.clone())
}

/// Whether `ECLIPSE_WEBVIEW_UA_DIAG` is forcing a diagnostic User-Agent (2026-07-16). Read ONCE.
///
/// The helper's `engine::effective_user_agent(diag, app)` puts the diag rung ABOVE the app's UA, so
/// while it is set a respawn would boot the SAME User-Agent it tore down — pure cost, zero effect,
/// and it would corrupt the A/B by making the measurement's own instrument respawn. This does NOT
/// duplicate the ladder (a second source of truth is the §6 💥 error class); it only recognizes that
/// the ladder's TOP rung pins the UA regardless of what the app sets.
fn ua_diag_forced() -> bool {
    static ON: OnceLock<bool> = OnceLock::new();
    *ON.get_or_init(|| std::env::var("ECLIPSE_WEBVIEW_UA_DIAG").is_ok_and(|v| !v.is_empty()))
}

// ---------------------------------------------------------------------------
// The deferred-3-arg-reply PROBE (2026-07-16) — a DEV-HOST DIAGNOSTIC, NOT a fix
// ---------------------------------------------------------------------------

/// Whether the env value selects the deferred-3-arg-setCookie-reply PROBE (2026-07-16).
/// EXACT-match `"1"` only — never `"true"`, never a truthy substring — mirroring the helper crate's
/// `engine::console_text_diag_enabled` / `engine::bridge_diag_enabled` for the same reason: a
/// deliberate opt-in that no unrelated env value can trip. Pure/unit-pinned.
fn defer_cookie_cb_enabled(v: Option<&str>) -> bool {
    v == Some("1")
}

/// The PROBE's verdict for this process, read ONCE (the env is not re-read per op) — and the place
/// its one-shot startup WARN is emitted.
///
/// # What it measures, and why it is a probe rather than a fix (2026-07-16)
///
/// THE ONE OPEN QUESTION behind the §5 ⏳➜🎲 / ☠️ ordering wall: the app's FIRST WebView-relevant
/// call is a 3-arg `setCookie(url, value, ValueCallback)` ~30–60 s BEFORE `setUserAgentString`, and
/// it cold-starts the helper — which FIXES the global `CefSettings.user_agent` before the app's UA
/// exists. Its reply cannot be answered locally (M4 deliberately removed the fabricated
/// `Boolean.TRUE`; `engine::classify_cookie_set_rejection` is observability-ONLY by its own doc and
/// explicitly cannot decide the store-unready-at-first-op case — which is precisely this one). The
/// only remaining honest option is to DEFER the reply until the engine exists, which AOSP permits:
/// *"This method is asynchronous. If a `ValueCallback` is provided, `ValueCallback#onReceiveValue`
/// will be called on the current thread's `Looper` once the operation is complete"*
/// (`frameworks/base/core/java/android/webkit/CookieManager.java`, fetched + read 2026-07-16) —
/// **no deadline is stated, for either `setCookie` or `removeAllCookies`.**
///
/// So the contract permits it and the mechanism is proven (§5 🏆: forcing the `Hybrid()` token makes
/// the challenge COMPLETE). **The unknown is purely BEHAVIOURAL: does the app WAIT on that callback
/// before proceeding to login?** Nobody knows; it cannot be settled first-party without RE. This
/// probe measures it — see [`set_app_user_agent`], whose `ECLIPSE-DEFER-CB ua-set` line reports the
/// app reaching `setUserAgentString` WITH a reply outstanding, which is the answer.
///
/// OFF (the default, and the shipped behaviour) this is a structural no-op: [`EarlyCookies::offer`]
/// returns `NeedsEngine` for a 3-arg set exactly as before, nothing is ever pushed to
/// [`DEFERRED_CB_IDS`], [`FIRST_DEFER_AT`] is never set, and every branch this gate guards is dead.
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

/// Request ids of 3-arg setCookies whose `ValueCallback` the PROBE is currently holding.
/// PROBE-ONLY: nothing is ever pushed here with the gate off. Bounded by [`EarlyCookies::CAP`].
static DEFERRED_CB_IDS: Mutex<Vec<u32>> = Mutex::new(Vec::new());

/// When the PROBE deferred its FIRST 3-arg reply — the clock [`set_app_user_agent`] reports
/// against. PROBE-ONLY (never set with the gate off), so its `get()` is itself the gate on the
/// measurement line.
static FIRST_DEFER_AT: OnceLock<Instant> = OnceLock::new();

/// The request id of a 3-arg setCookie frame (the shape that OWES the app a `ValueCallback`), or
/// `None` for every other message. The one place that knowledge lives.
fn deferred_cb_request_id(msg: &ConsumerMsg) -> Option<u32> {
    match msg {
        ConsumerMsg::CookieSetForResult { request_id, .. } => Some(*request_id),
        _ => None,
    }
}

/// Record + announce one 3-arg setCookie whose `ValueCallback` the PROBE is now holding.
/// Reached only under the gate (its only caller acts on a `Buffer` verdict for a frame
/// [`deferred_cb_request_id`] matched, which [`EarlyCookies::offer`] only returns when `defer_cb`).
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

/// Announce that a PROBE-deferred reply's REAL flag has now reached the app. A no-op for any id the
/// probe never held — and structurally unreachable with the gate off ([`DEFERRED_CB_IDS`] is then
/// always empty), so a default boot's cookie round-trip is unchanged.
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

// ---------------------------------------------------------------------------
// The deferred-spawn cookie window (2026-07-16, plan M6 — the §6 🩹➜⛔ ordering fix)
// ---------------------------------------------------------------------------

/// What [`EarlyCookies::offer`] decided about one message while the helper is UNSPAWNED.
#[derive(Debug, PartialEq, Eq)]
enum Deferral {
    /// Hold the raw frame; nothing crosses the wire and no reply is coming.
    Buffer,
    /// PROVABLY answerable without the engine — the caller produces CEF's own answer itself.
    AnswerWithoutEngine,
    /// Not answerable without CEF: spawn NOW. Carries the reason for the honest WARN, which
    /// doubles as the `trigger=` the spawn is logged with.
    NeedsEngine(&'static str),
}

/// What [`send_with_lazy_spawn`] did.
#[derive(Debug, PartialEq, Eq)]
pub enum SendOutcome {
    /// Written to the live helper; any reply arrives normally.
    Sent,
    /// Held in [`EarlyCookies`] for the flush (fire-and-forget only — nothing waits on it).
    Buffered,
    /// Answered without the engine; no reply is coming.
    AnsweredWithoutEngine,
}

/// The cookie ops deferred while the helper spawn is held back.
///
/// # Why this is CORRECT and is NOT a reimplementation of cookie semantics (2026-07-16)
///
/// It buffers RAW [`ConsumerMsg`] frames and replays them verbatim; it never parses, matches or
/// expires a cookie. It answers exactly two questions, and only where the answer is a THEOREM:
///
/// **The empty-store lemma.** While this variant is live, no helper process exists ⇒ no
/// `cef_initialize` ⇒ no `CefContext` (CEF `docs/architecture.md`: *"initialized when CefInitialize
/// is called"*; the pinned `cef_initialize` doc forbids ANY other CEF call before it) ⇒ no
/// `RequestContext`, no cookie store, no browser. On the eventual spawn the store is created fresh
/// from `engine::session_context_settings()`, whose `cache_path` is EMPTY — pinned
/// `_cef_settings_t::cache_path`: *"If this value is empty then browsers will be created in
/// \"incognito mode\" where in-memory caches are used for storage and no profile-specific data is
/// persisted to disk"* — so it starts EMPTY on every process start, with no disk and no prior boot.
/// It can then gain a cookie by exactly two routes: (a) `cef_cookie_manager_t::set_cookie`, whose
/// only callers in the whole helper are the `CookieSet` / `CookieSetForResult` handlers
/// (`engine.rs` — grep-verified 2026-07-16), and (b) a `Set-Cookie` response header, which needs a
/// browser — and `CreateView` is sent from exactly ONE site ([`drive`]), which spawns first. (b) is
/// therefore impossible here and (a) is exactly what this buffer holds.
/// **⇒ store_contents ≡ replay(self.sets), always.**
///
/// From the lemma:
/// * `CookiesClear` — `engine::cookies_clear` calls `delete_cookies(None, None, cb)`; pinned doc:
///   *"If |url| is NULL all cookies for all hosts and domains will be deleted"*. A blanket delete
///   over a store whose entire content is `sets` is reproduced EXACTLY by dropping `sets`. Its wire
///   reply is a pure completion signal, and `framework::fire_cookies_clear_result` passes `true`
///   UNCONDITIONALLY — so the locally delivered `true` is the same value, not a fabricated ack.
/// * `CookieGet` with an EMPTY buffer — the store is empty ⇒ CEF answers the empty list. (It does
///   so only via the 5 s `COOKIE_VISIT_DEADLINE`: *"Zero cookies never trigger the visitor"*,
///   `engine::poll`.) Byte-identical, and ~5 s faster.
///
/// And what it must NOT try to answer — for reasons that are also quotes:
/// * `CookieGet` with a NON-EMPTY buffer — `visit_url_cookies` results are *"filtered by the given
///   url scheme, host, domain and path"*. That matching is Chromium's. Eclipse does not implement
///   RFC 6265 anywhere and must not start here.
/// * `CookieSetForResult` — its whole purpose is the REAL verdict: `set_cookie` *"will check for
///   disallowed characters ... and fail without setting the cookie"* and returns *"false (0) if an
///   invalid URL is specified"* (the measured boot shows CEF genuinely rejecting two of the app's
///   sets). Only CEF knows. Deferring its reply instead was REJECTED **on an assumption that was
///   never measured**: that the app's `ValueCallback` firing only at flush — never, on a boot that
///   drives no WebView — would HANG an app that blocks on it at `AppManager.initialize`. Forcing
///   the spawn is exactly the pre-fix behaviour and can regress nothing, so it remains the SHIPPED
///   default. **2026-07-16: that assumption is now measurable, and the answer decides the whole
///   ordering fix** — see [`defer_cookie_cb`], the `ECLIPSE_WEBVIEW_DEFER_COOKIE_CB` dev-host probe,
///   which flips exactly this arm to `Buffer`. AOSP states NO deadline for the callback, so the
///   deferral is contract-legal; only the app's behaviour is unknown. The probe bounds the strand
///   at both ends it can: a `CookiesClear` that would DROP an unanswered frame forces the spawn
///   instead (below), and [`shutdown`] answers whatever is still deferred.
struct EarlyCookies {
    /// Deferred `CookieSet` frames in ARRIVAL ORDER, replayed verbatim by [`ensure_spawned`].
    /// RAW frames: nothing is parsed here, so nothing (expiry, creation time, sanitization) can be
    /// lost — `ConsumerMsg::CookieSet` carries `expires_epoch_s`, which the read-back type
    /// `CookieEntry` does NOT. That asymmetry is exactly why a read-back+replay was rejected and
    /// why buffering the app's ORIGINAL request is sound.
    ///
    /// 2026-07-16 (the §6 respawn): this is no longer only the PRE-spawn buffer — it is the
    /// APPEND-ONLY LOG of every cookie-mutating frame Eclipse has sent to the CURRENT helper, kept
    /// across its whole life by [`Self::record_sent`]. While no `CreateView` has been sent, the log
    /// IS the helper's store (the empty-store lemma above), so replaying it into a replacement
    /// reproduces that store exactly.
    sets: Vec<ConsumerMsg>,
    /// Whether `sets` can still be trusted to REPRODUCE the helper's store. Cleared — never
    /// silently — when the log stops being a faithful transcript:
    /// * [`Self::CAP`] overflow while Live (the frame is not appended, so the store gains a cookie
    ///   the log does not have ⇒ a replay would LOSE it);
    /// * [`Self::retire`] (a browser now exists ⇒ a network `Set-Cookie` can reach the store, which
    ///   no log can transcribe).
    ///
    /// A false value REFUSES the respawn ([`respawn_verdict`]) — it never silently degrades a replay
    /// into a lossy one. `true` at construction: an empty log faithfully describes an empty store.
    replayable: bool,
}

impl EarlyCookies {
    /// Bound on the buffer. NOT speculative future-proofing — this design creates a genuinely new
    /// unbounded path and this is the evidence for it: a boot with NO login challenge (the COMMON
    /// case) never drives a WebView, so the buffer NEVER flushes while the app keeps setting
    /// cookies for the whole session (the measured log shows `Updated WebViewCookieHandler with
    /// Cookies from URL …` recurring). Unbounded growth of cookie VALUES — including the
    /// `.ROBLOSECURITY` auth token — in the ART process is not acceptable (AGENTS.md §2: global
    /// state is bounded). 256 is ~10x the measured pre-load set count, so a challenge boot cannot
    /// reach it; overflow degrades to the honest forced spawn, i.e. exactly today's behaviour.
    const CAP: usize = 256;

    const fn new() -> Self {
        Self {
            sets: Vec::new(),
            replayable: true,
        }
    }

    /// Record one frame that has JUST been written to the LIVE helper (2026-07-16, the §6 respawn),
    /// so the log keeps describing that helper's store. The mirror image of [`Self::offer`]'s
    /// mutations — same shapes, same order, no verdict — and the ONE place the Live-side log is
    /// maintained.
    ///
    /// Called ONLY after a successful `send_locked` from [`send_with_lazy_spawn`], never from the
    /// input/frame path: `send_locked`'s other callers carry `MouseMove`/`Key`/`FrameAck`, which are
    /// per-event hot-path frames (AGENTS.md §2.4) and can never touch a cookie store. The cookie
    /// variants are matched EXPLICITLY — no `_` arm covers them — so a future cookie message cannot
    /// be added without deciding whether it mutates the store.
    fn record_sent(&mut self, msg: &ConsumerMsg) {
        match msg {
            // A set the engine has now applied. Order is the jar's semantics (a later set of the
            // same name overwrites an earlier one), so push, never dedupe.
            ConsumerMsg::CookieSet { .. } | ConsumerMsg::CookieSetForResult { .. } => {
                if self.sets.len() < Self::CAP {
                    self.sets.push(msg.clone());
                } else {
                    // The store now holds a cookie the log does not. Say so LOUDLY and once: a
                    // silent divergence here would turn the replay into the lossy read-back this
                    // design exists to avoid. The bound STAYS (cookie VALUES, incl.
                    // `.ROBLOSECURITY`, must not grow without limit in the ART process); only the
                    // respawn is given up.
                    if self.replayable {
                        tracing::warn!(
                            target: "android.webkit.CookieManager",
                            cap = Self::CAP,
                            "the webview cookie log hit its bound — it can no longer reproduce the \
                             engine's cookie store, so the app-UA respawn is now REFUSED for this \
                             boot (§6 2026-07-16 respawn). Honest degradation: the engine keeps the \
                             User-Agent it booted with."
                        );
                    }
                    self.replayable = false;
                }
            }
            // `delete_cookies(NULL, NULL)` = "If |url| is NULL all cookies for all hosts and
            // domains will be deleted" (pinned bindings). The engine applies frames in send order
            // (one socket, one pump thread), so truncating at the send point mirrors the engine
            // applying the clear at that same point in that same order. Without this, a replay would
            // RESURRECT cookies the app deliberately cleared.
            ConsumerMsg::CookiesClear { .. } => self.sets.clear(),
            // Read-only: `visit_url_cookies` "Visit a subset of cookies" (pinned bindings) cannot
            // change the store, so a get is not part of the transcript.
            ConsumerMsg::CookieGet { .. } => {}
            _ => {}
        }
    }

    /// Retire the log: a browser now exists on this helper (2026-07-16, the §6 respawn).
    ///
    /// Two independent reasons, and they land on the same line:
    /// 1. **Correctness** — from the first `CreateView` a network `Set-Cookie` response header can
    ///    populate the store, and no log Eclipse keeps can transcribe that. The empty-store lemma
    ///    stops holding, so a replay would silently lose cookies.
    /// 2. **Bound** — a live view means the respawn is forbidden anyway ([`respawn_verdict`]'s
    ///    live-view guard: replacing the helper would destroy the app's browser). Holding the app's
    ///    cookie VALUES (incl. the `.ROBLOSECURITY` auth token) in the ART process for the rest of
    ///    the session past that point buys nothing and costs disclosure.
    fn retire(&mut self) {
        self.sets.clear();
        self.replayable = false;
    }

    /// Does the buffer hold a 3-arg set whose `ValueCallback` is still owed the engine's REAL flag?
    /// Only the [`defer_cookie_cb`] probe can make this true (with the gate off a
    /// `CookieSetForResult` never buffers), so every branch it guards is dead on a default boot.
    fn holds_unanswered_callback(&self) -> bool {
        self.sets
            .iter()
            .any(|m| deferred_cb_request_id(m).is_some())
    }

    /// Offer one consumer message to the deferral. Total; the caller acts on the verdict. The
    /// cookie variants are matched EXPLICITLY — no `_` arm covers them — so a future cookie message
    /// cannot be added without deciding its pre-engine behaviour. That is the structural guard
    /// against re-opening this defect.
    ///
    /// `defer_cb` is the [`defer_cookie_cb`] PROBE (2026-07-16, dev-host only). It is a parameter
    /// rather than an env read so this stays pure and BOTH settings are unit-pinned — `false` must
    /// reproduce the shipped verdicts exactly.
    fn offer(&mut self, msg: &ConsumerMsg, defer_cb: bool) -> Deferral {
        match msg {
            // Fire-and-forget (v1): no reply obligation, so buffering can never strand a callback.
            ConsumerMsg::CookieSet { .. } if self.sets.len() < Self::CAP => {
                self.sets.push(msg.clone());
                Deferral::Buffer
            }
            ConsumerMsg::CookieSet { .. } => {
                Deferral::NeedsEngine("the deferred-cookie buffer is full")
            }
            // PROBE ONLY (`ECLIPSE_WEBVIEW_DEFER_COOKIE_CB=1`): buffer the 3-arg set EXACTLY like
            // the 2-arg one — the app's ORIGINAL frame, so `expires_epoch_s` and every other field
            // ride along losslessly — and hold its ValueCallback until [`ensure_spawned`] replays
            // it to the live engine, where the REAL flag routes back through the unchanged
            // `CookieSetResult` path. Nothing is fabricated; nothing is dropped. The ONLY thing that
            // changes is WHEN the app is answered, which AOSP leaves open ([`defer_cookie_cb`]).
            ConsumerMsg::CookieSetForResult { .. } if defer_cb && self.sets.len() < Self::CAP => {
                self.sets.push(msg.clone());
                Deferral::Buffer
            }
            ConsumerMsg::CookieSetForResult { .. } if defer_cb => {
                Deferral::NeedsEngine("the deferred-cookie buffer is full")
            }
            ConsumerMsg::CookieSetForResult { .. } => Deferral::NeedsEngine(
                "setCookie(url, value, ValueCallback) — only the engine yields the REAL success flag",
            ),
            // PROBE-only strand guard: a blanket clear is answerable locally by DROPPING `sets`
            // (the empty-store lemma) — but dropping a frame whose ValueCallback is still owed the
            // engine's REAL flag would strand the app forever. Force the spawn instead: the sets
            // replay, every held callback is answered by CEF, and the clear then rides the wire
            // normally. Unreachable with the gate off.
            ConsumerMsg::CookiesClear { .. } if self.holds_unanswered_callback() => {
                Deferral::NeedsEngine(
                    "removeAll/SessionCookies would DROP a probe-deferred setCookie frame whose \
                     ValueCallback is still owed the engine's REAL flag",
                )
            }
            ConsumerMsg::CookiesClear { .. } => {
                self.sets.clear();
                Deferral::AnswerWithoutEngine
            }
            ConsumerMsg::CookieGet { .. } if self.sets.is_empty() => Deferral::AnswerWithoutEngine,
            ConsumerMsg::CookieGet { .. } => Deferral::NeedsEngine(
                "getCookie after this boot set a cookie — CEF owns url/domain/path matching",
            ),
            _ => Deferral::NeedsEngine("an op that needs the engine reached the pre-engine gate"),
        }
    }
}

/// 2026-07-17: the rect the compositor last DREW a WebView at, and the view it drew. Keying by
/// `view` is what makes a rect left over from a closed view unusable for its successor WITHOUT
/// clearing it at all six `ACTIVE_VIEW` write sites (a "forget one" hazard; challenge16's stale
/// full-window composite is the recorded precedent for stale-rect bugs on this surface).
#[derive(Clone, Copy)]
struct DrawnRect {
    view: i64,
    x: i32,
    y: i32,
    w: u32,
    h: u32,
}

/// State shared between the reader thread, the drive path, and the compositor.
struct Shared {
    /// One entry per live driven WebView (the challenge flow has exactly one).
    views: Mutex<HashMap<i64, ViewShared>>,
    /// The cached ABSOLUTE composite rect `(x, y, w, h)` (the `TEXTBOX_GEOM` pattern): written by
    /// [`update_composited_rect`] on the main thread, read by the vk-overlay present path.
    rect: Mutex<Option<(i32, i32, u32, u32)>>,
    /// 2026-07-17: the CLAMPED screen rect the compositor ACTUALLY drew the WebView at: written by
    /// the vk-overlay present path (the ONE place that resolves it), read by the input hit-test —
    /// which is what stops the two from diverging. Leaf lock: no other lock is taken while held.
    screen_rect: Mutex<Option<DrawnRect>>,
    /// The blocking `getCookie` waiters, keyed by consumer request id: the reader thread delivers
    /// the solicited `CookieList` into the channel, waking [`cookie_get_blocking`] (which parked
    /// on the receiver WITHOUT holding [`CLIENT`], so the reader is never blocked). A request id is
    /// registered in exactly ONE sink — either here (getCookie) or a framework ValueCallback
    /// (removeAll/Session) — so the reader's routing is unambiguous.
    cookie_get_waiters: Mutex<HashMap<u32, mpsc::Sender<Vec<CookieEntry>>>>,
}

fn shared() -> &'static Arc<Shared> {
    static SHARED: OnceLock<Arc<Shared>> = OnceLock::new();
    SHARED.get_or_init(|| {
        Arc::new(Shared {
            views: Mutex::new(HashMap::new()),
            rect: Mutex::new(None),
            screen_rect: Mutex::new(None),
            cookie_get_waiters: Mutex::new(HashMap::new()),
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
/// own explicit selection runs with the inherited environment). 2026-07-10 (plan M5): also runs
/// the ADVISORY pre-spawn host-lib probe (returned for the handshake post-mortem) and forwards
/// the config-gated `--allow-unsandboxed` opt-in.
fn spawn_helper_process() -> Result<(UnixStream, Child, hostprobe::ProbeOutcome), ClientError> {
    use std::os::unix::process::CommandExt as _;

    let helper = resolve_helper()?;
    // 2026-07-10 (plan M5): the pre-spawn host-lib probe — ADVISORY ONLY (§2.9: a probe
    // false-negative must never degrade a capable host); the spawn proceeds in EVERY case and
    // the spawn/handshake outcome stays the authority. The outcome rides along so
    // `enrich_spawn_failure` can name the missing libraries in the handshake post-mortem.
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
    // 2026-07-10 (plan M5): the config-gated loud-degradation opt-in (spawn contract §3 — a
    // boolean flag, argv-safe, no secrets). A second Config::load beside resolve_helper()'s is
    // acceptable: this is the once-per-process cold spawn path, never per-frame/per-event.
    if crate::config::Config::load()
        .map(|c| c.webview_allow_unsandboxed)
        .unwrap_or(false)
    {
        cmd.arg("--allow-unsandboxed");
    }
    // 2026-07-16 (plan M6, the §6 2026-07-16 💥 fix): forward the UA the app set via
    // `WebSettings.setUserAgentString` so CEF actually SENDS it. `CefSettings.user_agent` is global
    // and fixed at `CefInitialize`, so THIS read is THIS HELPER's one chance to get it right.
    //
    // 2026-07-16 (§6 🩹➜⛔) — the ordering claim that stood here ("the spawn is lazy, it happens on
    // the first load-drive, AFTER the app configures its WebView, so the ordering works") was
    // DISPROVED by the live boot: a COOKIE op cold-started the helper 61 s BEFORE
    // setUserAgentString. `EarlyCookies` is what makes the ordering actually hold — cookie ops no
    // longer spawn the helper, so by the time this runs the app's UA is normally already set.
    //
    // 2026-07-16 (the §6 respawn) — and where an op genuinely still forces an early spawn (the
    // 3-arg setCookie's REAL flag; a getCookie against a non-empty log), the first load-drive
    // REPLACES that helper with one carrying the app's UA and replays the log into it
    // (`maybe_respawn_for_app_ua`). So a wrong UA is no longer a lost boot — it is a bounded
    // correction on the drive path.
    //
    // THE ENVIRONMENT, NOT ARGV, and this is the §6 🌱 provenance test applied, not a shape
    // argument. The spawn contract's operative predicate is "no secrets in argv" (§4 bans
    // token-bearing URLs because /proc/*/cmdline is WORLD-READABLE; it already admits
    // `--allow-unsandboxed` as argv-safe). A User-Agent is not a secret BY PROVENANCE: the app
    // composes it from ATL's own synthetic `SystemProperties`/`Build` values (`0MB`, `960x540`,
    // `HTC unknown` — no real hardware, no user data), and by design it is broadcast in cleartext to
    // every server it contacts. It is also not a URL, so the absolute redaction rule does not reach
    // it. Argv would nonetheless be the WRONG channel for a per-boot app-supplied string when a
    // strictly better one exists at equal cost: /proc/PID/environ is owner-only where
    // /proc/PID/cmdline is world-readable. Choose the tighter channel — the disclosure this adds is
    // then zero, not merely "acceptable".
    //
    // ONE read, TWO consumers, so they cannot disagree: the env the child receives, and
    // [`HELPER_BOOT_UA`] — which is the ONLY truthful answer to "what UA did this engine initialize
    // with?" and therefore the ONLY sound input to the respawn decision.
    let boot_ua = app_user_agent();
    if let Some(ua) = &boot_ua {
        cmd.env("ECLIPSE_WEBVIEW_APP_UA", ua);
    }
    if let Ok(mut slot) = HELPER_BOOT_UA.lock() {
        *slot = boot_ua;
    }
    // Past this point THIS helper's global CefSettings.user_agent is fixed; a later
    // setUserAgentString is honestly WARNed rather than silently dropped (`set_app_user_agent`).
    HELPER_UA_FIXED.store(true, Ordering::Relaxed);
    // 2026-07-03 / updated 2026-07-10 (plan M5): the M5-built helper carries RUNPATH=$ORIGIN
    // (crates/eclipse-webview/.cargo/config.toml), so libcef.so resolves beside the binary with
    // no env mutation; this LD_LIBRARY_PATH prepend stays as belt-and-suspenders for helpers
    // built before M5 or under a user RUSTFLAGS override (which silently replaces the crate's
    // rustflags — the packaging script re-verifies the RUNPATH with readelf). If the payload or
    // a host lib is genuinely absent, the failure is NOT a spawn error: `spawn()` succeeds and
    // the child dies INSIDE ld.so before `main`, so the consumer sees a handshake-EOF — which
    // `enrich_spawn_failure` post-mortems with the pre-spawn probe findings above.
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
    Ok((parent_end, child, probe))
}

/// Post-mortem a handshake failure with the pre-spawn probe findings (2026-07-10, plan M5).
/// Pure and unit-pinned. Enriches ONLY when (a) the failure is the `Handshake` class (an
/// EOF/protocol error — a `VersionMismatch` helper was alive and spoke protocol) AND (b) the
/// child exited on its own with a REAL exit code (`status.code()` is `Some`; a signal status is
/// the consumer's own `kill()` of a hung-but-alive helper and must not be misattributed to
/// ld.so). An ld.so start failure exits 127 before `main`, which is exactly this shape.
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

/// Send `Hello` and gate on the `HelloAck` version — the handshake requires an exact
/// [`super::PROTO_VERSION`] match. `timeout` is injected so the unit pin runs without a 10 s
/// sleep. On success the read timeout is cleared (the reader loop uses plain blocking reads;
/// EOF is its exit signal).
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
        HelperMsg::BridgeCall { .. } => "BridgeCall",
        HelperMsg::EvaluateJsResult { .. } => "EvaluateJsResult",
        HelperMsg::CookieSetResult { .. } => "CookieSetResult",
    }
}

/// Spawn the io thread and wait (bounded) for its spawn+handshake verdict. Called with the
/// [`CLIENT`] lock held (spawn/teardown are serialized there). Once per process, on the first
/// drive; the JNI caller blocks ≤ [`SPAWN_RESULT_TIMEOUT`], ~ms when healthy (the handshake is
/// pre-engine-init on the helper side).
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

/// Ensure the [`CLIENT`] slot is `Live` (lazy spawn, D2). Called with the [`CLIENT`] lock held.
/// On spawn failure the slot latches `Failed` and the actionable error is returned (the honest
/// no-op contract). A caller MUST have checked [`latched_error`] first.
///
/// 2026-07-16 (plan M6, §6 🩹➜⛔): this is where [`spawn_helper_process`] reads [`APP_USER_AGENT`]
/// into the child's env, so this is the instant the engine's GLOBAL `CefSettings.user_agent` is
/// fixed for the whole boot. `trigger` is therefore load-bearing, not decoration: challenge28's log
/// could not say WHAT cold-started the helper 61 s before the app set its UA, and that ambiguity is
/// what this line ends. It is also where the deferred cookie ops replay, in arrival order, BEFORE
/// the message that triggered the spawn.
fn ensure_spawned(
    slot: &mut ClientSlot,
    java_vm: jni::vm::JavaVM,
    trigger: &str,
) -> Result<(), ClientError> {
    // 2026-07-16 (the §6 respawn): MOVE the frames out rather than clone — `deferred` is then a
    // local the replay loop can borrow while `send_locked` mutably borrows `slot`. They are
    // re-installed into the Live log below, because once written they ARE the new engine's store and
    // a later respawn must be able to replay them again.
    let (deferred, replayable) = match slot {
        ClientSlot::Unspawned(early) => (std::mem::take(&mut early.sets), early.replayable),
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
            // The deferred sets die with the latch — identical to the pre-fix behaviour, where
            // every one of them would have been a no-op on a latched slot.
            *slot = ClientSlot::Failed(e.to_string());
            return Err(e);
        }
    }
    // Replay BEFORE the triggering message (and before drive()'s CreateView/LoadUrl), so nothing
    // can observe the jar between the spawn and the flush. Cookie-before-CreateView is correct:
    // the store is request-context-scoped, not view-scoped.
    for msg in &deferred {
        // PROBE (2026-07-16): this is where a held ValueCallback stops being held — the app's
        // ORIGINAL frame goes to the live engine and the REAL flag routes back on the normal
        // `CookieSetResult` path (`note_deferred_callback_answered` logs its arrival). Unreachable
        // with the gate off: `offer` never buffers a `CookieSetForResult` then.
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
    // 2026-07-16 (the §6 respawn): the engine now holds exactly these frames, so the Live log must
    // too. `replayable` rides across: a log that had already lost the ability to describe its
    // predecessor's store must not silently regain it here. (A `send_locked` failure above latched
    // the slot, so this no-ops and the log dies with the latch — today's behaviour.)
    if let ClientSlot::Live(_, log) = slot {
        log.sets = deferred;
        log.replayable = replayable;
    }
    Ok(())
}

// ---------------------------------------------------------------------------
// The app-UA helper replacement (2026-07-16, plan M6 — the §6 respawn)
// ---------------------------------------------------------------------------

/// What [`respawn_verdict`] decided about replacing the live helper (2026-07-16, the §6 respawn).
#[derive(Debug, PartialEq, Eq)]
enum RespawnVerdict {
    /// Tear the live helper down and spawn a replacement carrying the app's UA; replay the log.
    Respawn,
    /// Leave the live helper alone. Carries the reason, which IS the log line's `reason=` — every
    /// arm is nameable, so no boot can leave "why didn't it respawn?" ambiguous. That is the
    /// property the §6 ⏳ / 🩹 passes proved worth having: each named its own culprit on the first
    /// boot.
    Keep(&'static str),
}

/// PURE (unit-pinned): decide whether the LIVE helper must be replaced so the engine's GLOBAL
/// `CefSettings.user_agent` carries the User-Agent the app set (2026-07-16, plan M6, the §6
/// respawn). Every input is injected — no env read, no lock, no `JavaVM` — so all seven arms are
/// pinned in plain `cargo test`.
///
/// # The guards, in order, and why each one comes BEFORE the respawn
///
/// * `ua_diag_forced` — `ECLIPSE_WEBVIEW_UA_DIAG` OUTRANKS the app's UA in the helper's own ladder
///   (`engine::effective_user_agent`), so a replacement would boot the identical string.
/// * `app_ua == None` — the app never called `setUserAgentString`. The helper's fallback literal is
///   already the right answer (this is the `__webview-test` path: nothing changes for it, ever).
/// * `boot_ua == app_ua` — the live helper already carries it. The normal case once the deferral
///   holds: the load-drive spawns ONE helper, with the app's UA, and this never fires.
/// * `live_views != 0` — a browser exists. Replacing the helper would DESTROY the app's WebView
///   mid-flight, and the empty-store lemma no longer holds anyway (a network `Set-Cookie` can have
///   populated the store behind the log). Measured 2026-07-16: the app's order is create →
///   setUserAgentString → addJavascriptInterface → loadUrl, so this arm should never fire; if it
///   does, that measurement is wrong and the design is falsified as SUFFICIENT.
/// * `!log_replayable` — the log can no longer reproduce the store (CAP overflow / retired). A
///   replay would silently LOSE cookies, which is the lossy read-back this design exists to avoid.
/// * `ops_in_flight != 0` — an app `ValueCallback` or a parked `getCookie` is outstanding against
///   the live helper. Tearing it down runs `framework::drain_all_webview_callbacks`, which answers
///   each one `false`/`"null"`. That is ACCURATE for a helper that is GONE and WRONG for one that is
///   being REPLACED (the replayed frame lands in the successor's store, so a `false` flag would
///   contradict it). It is ALSO what makes the teardown deadlock-free: with the maps empty the drain
///   early-returns and never dispatches to the main Looper.
///
/// Every `Keep` is strictly today's behaviour, said out loud — no arm can regress anything.
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

/// How long the respawn lets the OLD helper exit before killing it (2026-07-16, the §6 respawn).
///
/// The old helper has NO views, so `engine::shutdown_state`'s `if st.views.is_empty() { return
/// Some((exit_code, true)) }` fires on the very next pump iteration — the clean path, which runs
/// `cef_shutdown()` and therefore tears down CEF's own child processes. 3 s is ~20x the expected
/// cost and exists only so a wedged helper cannot park the load-drive forever; the drive already
/// tolerates up to [`SPAWN_RESULT_TIMEOUT`] (15 s).
const RESPAWN_TEARDOWN_DEADLINE: Duration = Duration::from_secs(3);

/// Count of app operations outstanding against the live helper (2026-07-16, the §6 respawn):
/// framework-retained `ValueCallback`s plus parked blocking `getCookie` waiters.
/// [`respawn_verdict`] refuses a teardown while any is nonzero.
///
/// Lock order — AUDITED 2026-07-16, because this is called UNDER [`CLIENT`] and both locks it takes
/// are also touched by the reader and by JNI threads. Every other site takes them in a scope that
/// ENDS before any [`CLIENT`] acquisition: `cookie_get_blocking` registers its waiter and releases
/// before `send_with_lazy_spawn`; the reader removes a waiter and releases before `tx.send`;
/// `framework.rs`'s three registries are each locked in a block that closes before the `client::`
/// call that follows. Nothing anywhere takes a waiter/registry lock and THEN [`CLIENT`], so the
/// order is one-directional (CLIENT → these) and no inversion is possible.
fn ops_in_flight() -> usize {
    let parked = shared()
        .cookie_get_waiters
        .lock()
        .map(|w| w.len())
        .unwrap_or(0);
    crate::framework::webview_callbacks_in_flight() + parked
}

/// Replace the live helper with one carrying the User-Agent the app set, replaying the cookie log
/// into it (2026-07-16, plan M6 — the §6 respawn). Returns `true` when it did.
///
/// # Why this is a REPLACEMENT and not a correction (CLAUDE.md's no-workaround rule)
///
/// The engine's User-Agent is `CefSettings.user_agent`, which is GLOBAL and consumed by
/// `CefInitialize` (pinned bindings: *"Value that will be returned as the User-Agent HTTP header"*).
/// An engine that initialized with the wrong one is, permanently, an engine configured wrongly — and
/// `cef_shutdown` is documented *"Do not call any other CEF functions after calling this function"*,
/// so it cannot even be re-initialized in place. CDP's `Emulation.setUserAgentOverride` would leave
/// it wrong and paper over the symptom in the renderer ("changes behavior to avoid the problem").
/// This instead makes the engine that serves the app's WebView the engine the app configured, which
/// is the actual mechanism, corrected at its source. The cost is one extra `CefInitialize` (~122 ms
/// measured) on the drive path, paid once, only on a boot that reaches a challenge.
///
/// # The lock discipline — the reason this is three phases and not one
///
/// The old helper's reader thread takes [`CLIENT`] on EOF ([`reader_fatal`]), so the teardown MUST
/// NOT hold it: joining under the lock is a guaranteed hang. But releasing it naively opens a window
/// in which another thread could spawn a SECOND helper while the first is still alive — and CEF's
/// documented process singleton on `root_cache_path` (which `build_settings_with_ua` leaves empty,
/// so it is the shared `~/.config/cef_user_data` default) means the second `CefInitialize` would EXIT
/// EARLY: *"only a single app instance is allowed to run for a given CefSettings.root_cache_path
/// value… Client apps should therefore check the cef_initialize() return value for early exit"*
/// (`_cef_browser_process_handler_t::on_already_running_app_relaunch`, pinned bindings). So phase 1
/// parks the slot in a `Failed` latch — which no code path can spawn out of — and phase 3 lifts it.
/// A stale reader that wakes inside the window finds a non-`Live` slot and quietly exits, exactly as
/// it does after a deliberate [`shutdown`] ([`reader_fatal`]'s existing else-arm), so it can neither
/// latch nor kill its successor.
fn maybe_respawn_for_app_ua() -> bool {
    // ---- Phase 1: decide and detach, UNDER the lock. ----
    let (old, log) = {
        let mut slot = match CLIENT.lock() {
            Ok(s) => s,
            Err(_) => return false, // poisoned: the drive's own lock take reports it honestly
        };
        let ClientSlot::Live(_, log) = &*slot else {
            // Unspawned: `ensure_spawned` is about to spawn with the app's UA anyway — there is
            // nothing to replace. Failed: the drive's latch check has already reported it.
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
                // Named on EVERY boot, at INFO, including the healthy "nothing to correct" case:
                // the §6 ⏳/🩹 passes were cheap precisely because the instrument spoke on the first
                // boot. A silent no-op here would cost the next session a whole boot.
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
            // Unreachable: the borrow above proved `Live` and the lock has not been released.
            return false;
        };
        // No engine exists from here until phase 3's successor spawns, so a `setUserAgentString`
        // landing in the window CAN still reach it — `spawn_helper_process` re-reads the store.
        // Leaving this true would make `set_app_user_agent` warn about a fix that is in progress.
        HELPER_UA_FIXED.store(false, Ordering::Relaxed);
        tracing::info!(
            target: "android.webkit.WebSettings",
            boot_ua = boot_ua.as_deref().unwrap_or("<the Eclipse fallback literal>"),
            app_ua = app_ua.as_deref().unwrap_or(""),
            logged_sets = log.sets.len(),
            "webview client: REPLACING the eclipse-webview helper so the engine presents the \
             User-Agent the app set via WebSettings.setUserAgentString — CefSettings.user_agent is \
             global and consumed by CefInitialize, so an engine that booted on the wrong one can \
             only be replaced, never corrected (§6 2026-07-16 respawn). The old helper never \
             created a browser, so its cookie store is EXACTLY the logged frames; they replay into \
             the replacement verbatim."
        );
        (old, log)
    }; // <<< CLIENT RELEASED HERE — the teardown below must not hold it.

    // ---- Phase 2: tear the old helper down, WITHOUT the lock. ----
    teardown_replaced_helper(old);

    // ---- Phase 3: hand the log to the deferral, UNDER the lock. ----
    // Guarded BY VALUE: if a real failure raced the swap (a deliberate `shutdown`), that reason WINS
    // and the log dies with it — never resurrect a helper nobody wants.
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

/// Tear down the helper a respawn is replacing (2026-07-16, the §6 respawn). Called with NO locks
/// held (see [`maybe_respawn_for_app_ua`]'s phase 2).
///
/// Deliberately NOT a call to [`shutdown`]: that one latches the slot permanently, takes `&Vm` (a
/// main-thread proof this path cannot produce — a load-drive runs on whatever thread the app calls
/// `loadUrl` from), and pumps the main Looper while joining the upcall thread. Here the slot is
/// already parked, and the pump is unnecessary BY THE QUIESCENCE GUARD: [`respawn_verdict`] refused
/// unless every ValueCallback map was empty, so the upcall thread's exit drain hits
/// `framework::drain_all_webview_callbacks`'s all-empty early return and never dispatches to main.
///
/// The reader IS joined (it must be: an unjoined reader could still be inside [`reader_fatal`] when
/// the successor goes `Live`, and it cannot tell the two apart). The upcall thread is NOT — it is
/// detached by dropping its handle. Joining it would park this thread against a thread that may park
/// on main, and it has nothing left to do: the reader's exit drops the channel sender, so its loop
/// ends, its (empty) drain early-returns, and it exits on its own. Nothing is dropped.
///
/// The child is fully REAPED (`wait()` returns) before this function does. That is load-bearing, not
/// hygiene: the successor's `CefInitialize` must not race the old process's singleton lock.
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
    // Bounded: the child is dead, so the reader's next read is EOF; its `reader_fatal` finds the
    // parked non-`Live` slot, takes the quiet debug arm, and returns.
    let reader_joined = old.reader.take().map(|h| h.join().is_ok()).unwrap_or(false);
    drop(old.upcall.take()); // detach — see the fn doc
    if killed || !reader_joined {
        // A helper that had to be SIGKILLed may leave CEF child processes briefly alive, and they
        // hold the same root_cache_path process singleton the successor is about to need. Say so: if
        // the successor then fails to initialize, this line is the explanation.
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

/// The io thread's spawn/handshake verdict: writer + child + the upcall-thread handle.
type SpawnVerdict = Result<(UnixStream, Child, JoinHandle<()>), ClientError>;

/// The ThreadId of the CURRENT `eclipse-webview-io` thread. The [`cookie_get_blocking`] boundary
/// assertion checks it: a blocking wait ON the reader thread can never be answered (the reply's only
/// reader is the parked caller itself). 2026-07-09.
///
/// 2026-07-16 (the §6 respawn) — WAS a `OnceLock`, on the recorded reasoning "set once — the client
/// never respawns". THAT IS NO LONGER TRUE, and a `OnceLock` would leave the guard SILENTLY DEAD: it
/// would pin the FIRST (now exited) io thread's id forever, so the check could never match the
/// CURRENT io thread and a future io-thread `getCookie` — exactly what it exists to catch — would
/// sail through into the guaranteed self-stall it is there to prevent. (It could NOT go the other
/// way: `std::thread::ThreadId` is a monotonic std-owned counter — *"ThreadIds are guaranteed not to
/// be reused, even when a thread terminates"*, std `thread/id.rs` — so a stale id can never
/// false-positive onto a live thread.) Overwritten by each io thread at entry; the respawn joins the
/// old reader before spawning the new one, so the two writes cannot race.
static IO_THREAD_ID: Mutex<Option<std::thread::ThreadId>> = Mutex::new(None);

/// The `eclipse-webview-io` thread body: spawn + handshake, spawn the upcall thread, report, then
/// become the read loop. On ANY read-loop exit (crash/EOF/protocol error or a deliberate
/// shutdown) it wakes every parked `getCookie` waiter immediately and lets the upcall thread —
/// whose channel sender drops here — drain the pending ValueCallbacks honestly (2026-07-09 fix:
/// neither happened before, leaking the JNI globals and stalling parked getters the full timeout).
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
            // 2026-07-10: log the negotiated version from the one source of truth — the old
            // hardcoded generation literal went stale when M4 bumped PROTO_VERSION to 2.
            tracing::info!(
                %engine,
                protocol = u64::from(super::PROTO_VERSION),
                "eclipse-webview helper handshake complete"
            );
        }
        Err(e) => {
            // 2026-07-10 (plan M5): a child that died inside ld.so (missing host lib / missing
            // payload) surfaces HERE as a handshake EOF — kill() is a no-op on the corpse,
            // wait() recovers the real exit status, and the probe findings turn the anonymous
            // EOF into an actionable post-mortem (→ the latch → the one-shot WARN →
            // __webview-test's failure output).
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
    // The upcall thread owns the JavaVM: ALL app-code JNI (load upcalls, bridge invokes,
    // ValueCallback deliveries) runs there, in channel order, never on this reader thread.
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
        // The driving thread timed out and dropped the receiver: recover the child and reap it.
        if let Ok((_w, mut c, _h)) = returned {
            let _ = c.kill();
            let _ = c.wait();
        }
        return;
    }
    reader_loop(&stream, shared, &up_tx);
    // Reader gone (any reason): wake parked getCookie callers NOW (Disconnected → honest empty,
    // not a full-timeout stall), then drop `up_tx` so the upcall thread processes its remaining
    // queue and drains every still-pending ValueCallback honestly before exiting.
    wake_all_cookie_waiters();
}

// ---------------------------------------------------------------------------
// The reader loop + the pure dispatch state machine
// ---------------------------------------------------------------------------

/// One `internalLoadChanged` upcall extracted by [`dispatch`] (handed to the upcall thread).
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
    /// v2 (plan M4): page bridge calls `(view, call_id, payload_json)` — dispatched to ART's
    /// reflective invoke OUTSIDE the locks (payload never logged).
    bridge_calls: Vec<(i64, u32, String)>,
    /// v2: eval-with-result completions `(request_id, ok, value_json)`.
    eval_results: Vec<(u32, bool, String)>,
    /// v2: 3-arg setCookie completions `(request_id, ok)`.
    cookie_set_results: Vec<(u32, bool)>,
    /// v2/v1: solicited cookie lists `(request_id, cookies)` — routed by the reader loop to either
    /// the blocking-getCookie channel or a framework clear-callback.
    cookie_lists: Vec<(u32, Vec<CookieEntry>)>,
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
            // 2026-07-10 (M6): promoted debug!→info! so page console events are visible on a
            // default boot (RUST_LOG unset). The event is STRUCTURALLY text-free (Console::from_raw
            // drops the text at construction — proto.rs — and the decode re-redacts); this line
            // binds only severity + the already-scheme+host source + line + byte length, so the
            // default privacy greps stay clean. The full text is a helper-side, env-gated diagnostic
            // (ECLIPSE_WEBVIEW_CONSOLE=1), never on the wire.
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
            // 2026-07-10 (plan M5): kind=1 is code-KEYED — code 2 = the helper's sandbox-policy
            // refusal (distinct actionable text + skip marker); any other code (incl. the legacy
            // 0 of an M4-built helper resolved via ECLIPSE_WEBVIEW_HELPER) keeps the existing
            // no-display/engine-init reason. Values are data; the wire layout is unchanged.
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
                // kind 1 = engine-init-failed (no display / ozone) per the proto spec.
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
        // v2 (plan M4): the JS-bridge / eval-result / cookie-result surface. dispatch stays PURE
        // over `views` — it only EXTRACTS these into DispatchOut; the reader loop routes them
        // (waiter channel wakes inline, JNI upcalls to the upcall thread — 2026-07-09). An
        // untracked view on a BridgeCall is fine: framework.rs validates the bridge registry at
        // dispatch (the LoadState untracked-view precedent). Payloads are never logged.
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
        // Out-of-phase messages: debug-ignore (v1 decodes them; the reader has no consumer here).
        other @ (HelperMsg::HelloAck { .. } | HelperMsg::FrameBufferNew { .. }) => {
            tracing::debug!(
                msg = helper_msg_name(&other),
                "webview client: ignoring out-of-phase helper message"
            );
        }
    }
    out
}

/// One in-order event for the `eclipse-webview-upcall` thread — everything that executes APP code
/// over JNI. 2026-07-09: introduced so the socket-reader thread NEVER runs app code (a bridge
/// method / page callback that synchronously calls the blocking `CookieManager.getCookie` parks
/// its calling thread on a reply only the reader can deliver — inline dispatch self-deadlocked
/// the io loop for the full 5 s timeout and then served a wrong empty cookie string).
enum UpcallEvent {
    /// `WebView.internalLoadChanged(state, url)` (the url is the Java argument, never logged).
    LoadChanged {
        widget: i64,
        state: i32,
        url: String,
    },
    /// A page bridge call to reflect-invoke; the `BridgeResult` reply is written from here.
    BridgeCall {
        view: i64,
        call_id: u32,
        payload_json: String,
    },
    /// `evaluateJavascript` result → the retained ValueCallback.
    EvalResult {
        request_id: u32,
        ok: bool,
        value_json: String,
    },
    /// 3-arg setCookie completion → the retained ValueCallback<Boolean>.
    CookieSetResult { request_id: u32, ok: bool },
    /// removeAll/Session completion (a CookieList with no getCookie waiter registered).
    CookiesClearResult { request_id: u32 },
    /// The helper confirmed a view close: drop the view's `@JavascriptInterface` bridge globals
    /// and fail its in-flight eval callbacks honestly. Era-gated (`upto_era` = the close's
    /// [`crate::framework::bump_webview_close_era`] value), so state born AFTER the close — a
    /// legal close+re-drive — survives a stale queued drain (2026-07-10 fix). The bridge drop
    /// runs HERE (this thread is permanently ART-attached after its first upcall), never on the
    /// reader: jni 0.22.4 `Global::drop` on an unattached thread does a scoped attach/detach per
    /// ref (2026-07-10 fix — the reader must stay JNI-free).
    ViewClosedDrain { widget: i64, upto_era: u64 },
}

/// The `eclipse-webview-upcall` thread body: run every app-code JNI upcall in channel order.
/// When the channel disconnects (the reader thread exited — crash, EOF, protocol error, or a
/// deliberate shutdown), drain EVERY still-pending ValueCallback honestly (eval → `"null"`,
/// cookie set/clear → `Boolean.FALSE`) so the fire-exactly-once contract holds and no JNI global
/// outlives the helper (2026-07-09 fix — previously nothing drained these on helper death).
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
                // 2026-07-16 (web-engine M6): DELIBERATELY still dispatched on THIS thread while
                // every other app-facing upcall now runs on the main/UI Looper.
                // WebView.java:1915-1918 puts @JavascriptInterface methods on "a private,
                // background thread of this WebView" — this thread already IS that identity,
                // precisely so a bridge method MAY BLOCK (Eclipse's CookieManager.getCookie is a
                // 5 s round-trip; on main it would park winit). The only divergence from AOSP
                // (whose thread is a Chromium JavaHandlerThread with a prepared AND DRAINED
                // Looper) is Looper presence, and NO bridge call has ever reached Eclipse — the
                // mechanism is code-path-confirmed but the TRIGGER is not (CLAUDE.md). Preparing an
                // UNDRAINED Looper here would be WORSE than the loud throw (every Handler.post
                // would silently vanish). Deferred + instrumented: framework's
                // note_first_bridge_call_thread logs the verdict on the first call ever.
                // Recorded in AGENTS.md §6 2026-07-16.
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
                // PROBE (2026-07-16): logs ONLY for an id the probe held — the evidence that a
                // deferred reply completed honestly with the engine's own flag.
                note_deferred_callback_answered(request_id, ok);
                crate::framework::fire_cookie_set_result(java_vm, request_id, ok);
            }
            UpcallEvent::CookiesClearResult { request_id } => {
                crate::framework::fire_cookies_clear_result(java_vm, request_id);
            }
            UpcallEvent::ViewClosedDrain { widget, upto_era } => {
                // 2026-07-10: the bridge-global drop moved here from the reader thread (which
                // must stay JNI-free) — and queue order means a BridgeCall received before the
                // ViewClosed still finds its registry entry when it fires above.
                crate::framework::drop_bridges_for_view_closed(widget, upto_era);
                crate::framework::drain_eval_callbacks_for_view(java_vm, widget, upto_era);
            }
        }
    }
    // Channel closed: the reader is gone. Queued results above fired normally, in order; whatever
    // is STILL pending can never be answered — fail each callback honestly, exactly once.
    crate::framework::drain_all_webview_callbacks(java_vm, "web engine helper connection closed");
}

/// The reader thread's steady state: decode helper messages on the RAW stream (NEVER a
/// `BufReader` — the byte after a `FrameBufferNew` frame is the fd sentinel, and a buffered
/// reader would swallow it and drop the fd; proto.rs module-doc rule), feed the pure state
/// machine, apply its outputs, and hand every app-code JNI upcall to the upcall thread
/// (2026-07-09: never dispatched inline here — see [`UpcallEvent`]).
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
        let (out, close_eras) = match shared.views.lock() {
            Ok(mut views) => {
                let out = dispatch(msg, &mut views);
                // 2026-07-10: bump the close era for each removed view UNDER the same lock hold
                // that removed it — a re-drive must take this lock to re-insert, so anything
                // registered after the removal always observes the bumped era (the stale-drain
                // gate for ViewClosedDrain; see framework::WEBVIEW_CLOSE_ERA).
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
        // App-code JNI (load upcalls / bridge invokes / ValueCallback deliveries) is HANDED OFF to
        // the upcall thread, in order — never dispatched here (2026-07-09; see [`UpcallEvent`]).
        // A send error means the upcall thread is gone (it drains on exit); nothing to do here.
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
            // Exactly one sink per request id: a blocking getCookie waiter (delivered HERE — a
            // channel send, no JNI, so a stalled upcall can never block it), else a framework
            // clear-callback (removeAll/Session). Remove-then-send so a timed-out waiter is gone.
            let waiter = shared
                .cookie_get_waiters
                .lock()
                .ok()
                .and_then(|mut w| w.remove(&request_id));
            match waiter {
                Some(tx) => {
                    let _ = tx.send(cookies);
                }
                None => {
                    let _ = upcalls.send(UpcallEvent::CookiesClearResult { request_id });
                }
            }
        }
        for (closed, upto_era) in out.closed.into_iter().zip(close_eras) {
            let _ = ACTIVE_VIEW.compare_exchange(closed, 0, Ordering::Relaxed, Ordering::Relaxed);
            LIVE_VIEWS.fetch_sub(1, Ordering::Relaxed);
            // Clear the (proto-only, JNI-free) buffered inventory inline; the framework-side
            // bridge-global drop + the eval-callback drain run on the UPCALL thread via
            // ViewClosedDrain (2026-07-10 fix: dropping `Global`s here did a hidden scoped JNI
            // attach/detach per ref on this deliberately JNI-free reader; the era gates a stale
            // queued drain so a close+re-drive's fresh callbacks/bridges survive).
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

/// One loud payload-free WARN + the D5 failure latch: replace a `Live` slot with `Failed`,
/// kill+wait the child (never leave the helper running), clear the present/input gate, exit.
/// A slot that is no longer `Live` (a deliberate [`shutdown`] took it, or a drive already
/// latched a write failure) keeps its state and gets only a quiet debug line — the expected
/// EOF after a clean shutdown is not a failure.
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

/// Write one reply frame under the [`CLIENT`] mutex. `true` = written or safely dropped (the
/// slot is no longer live — a shutdown raced the reply); `false` = a real write failure.
fn send_reply_if_live(msg: &ConsumerMsg) -> bool {
    let Ok(bytes) = msg.encode() else {
        return true; // a v1 FrameAck cannot exceed its cap; treat as droppable, not fatal
    };
    match CLIENT.lock() {
        Ok(slot) => match &*slot {
            ClientSlot::Live(c, _) => (&mut &c.writer).write_all(&bytes).is_ok(),
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
    // 2026-07-16 (plan M6, the §6 respawn): BEFORE the drive takes CLIENT, because tearing the old
    // helper down must NOT hold it (its reader takes CLIENT on EOF — `reader_fatal`). A no-op unless
    // a live helper booted with the WRONG User-Agent and every guard passes; it parks the slot
    // itself, so the window is inert. Measured 2026-07-16: the app calls setUserAgentString ~110 µs
    // before this, and ~30–60 s AFTER a cookie op has already cold-started the helper.
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
        // 2026-07-16 (the §6 respawn): a browser is about to exist. From here a network Set-Cookie
        // can populate the store behind the log, so the log stops being a faithful transcript — AND
        // `respawn_verdict`'s live-view guard forbids any future replacement anyway. Retire it at
        // the earliest honest point rather than holding the app's cookie VALUES (incl. the
        // .ROBLOSECURITY auth token) in the ART process for the rest of the session.
        if let ClientSlot::Live(_, log) = &mut *slot {
            log.retire();
        }
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
        // Flush any bridges registered BEFORE this view's first load
        // (addJavascriptInterface-before-loadUrl is the common order) so the helper receives
        // CreateView THEN BridgeRegister — the browser exists before the inventory arrives.
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
// v2 (plan M4): JS bridge / evaluateJavascript-with-result / cookie set/get/clear
// ---------------------------------------------------------------------------

/// Bridges registered BEFORE their view's first load (the common `addJavascriptInterface` →
/// `loadUrl` order), keyed by widget then interface name. Flushed by [`drive`] right after
/// `CreateView`, so the helper always receives `CreateView` before the first `BridgeRegister`
/// (the browser exists before the inventory arrives). Bounded by the app's own interface count.
#[allow(clippy::type_complexity)] // 2026-07-09: widget → (iface → methods).
fn pending_bridges() -> &'static Mutex<HashMap<i64, HashMap<String, Vec<BridgeMethod>>>> {
    static P: OnceLock<Mutex<HashMap<i64, HashMap<String, Vec<BridgeMethod>>>>> = OnceLock::new();
    P.get_or_init(|| Mutex::new(HashMap::new()))
}

/// Count of widgets with a buffered bridge inventory — [`notify_view_freed`]'s fast gate (one
/// atomic load per normal view GC on the FinalizerDaemon). Maintained under the
/// [`pending_bridges`] lock by storing `len()` after every mutation (recount-from-truth, never
/// arithmetic). 2026-07-10.
static PENDING_BRIDGE_VIEWS: AtomicUsize = AtomicUsize::new(0);

/// Buffer one `addJavascriptInterface` inventory for `widget` (the ONE insert site — keeps
/// [`PENDING_BRIDGE_VIEWS`] true to the map). 2026-07-10.
fn buffer_pending_bridge(widget: i64, name: String, methods: Vec<BridgeMethod>) {
    if let Ok(mut m) = pending_bridges().lock() {
        m.entry(widget).or_default().insert(name, methods);
        PENDING_BRIDGE_VIEWS.store(m.len(), Ordering::Relaxed);
    }
}

/// Remove `widget`'s buffered bridge inventory (counter-maintaining). 2026-07-10.
fn remove_pending_bridges(widget: i64) {
    if let Ok(mut m) = pending_bridges().lock() {
        m.remove(&widget);
        PENDING_BRIDGE_VIEWS.store(m.len(), Ordering::Relaxed);
    }
}

/// Take (and clear) the buffered bridge inventory for `widget`.
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

/// Latch check → the deferred-spawn cookie window ([`EarlyCookies`]) → lazy spawn (D2) → send one
/// frame under the [`CLIENT`] lock (the register/eval/cookie ops that need no per-view record). A
/// send failure latches (the stream is untrustworthy); a latched slot returns the actionable reason
/// before any work.
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
    // Decide and drop the borrow before acting (the verdict owns its &'static str).
    let verdict = match &mut *slot {
        ClientSlot::Unspawned(early) => Some(early.offer(msg, defer_cookie_cb())),
        _ => None,
    };
    match verdict {
        Some(Deferral::Buffer) => {
            // PROBE (2026-07-16): a BUFFERED 3-arg set is the one shape that leaves an app callback
            // outstanding. Announce it; the gate-off path can never reach this (`offer` only
            // buffers a `CookieSetForResult` when `defer_cb`).
            if let Some(request_id) = deferred_cb_request_id(msg) {
                note_deferred_callback(request_id);
            }
            return Ok(SendOutcome::Buffered);
        }
        Some(Deferral::AnswerWithoutEngine) => return Ok(SendOutcome::AnsweredWithoutEngine),
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
        None => {} // already Live
    }
    let outcome = send_locked(&mut slot, msg).map(|()| SendOutcome::Sent)?;
    // 2026-07-16 (the §6 respawn): the frame is now IN the live engine's store, so the log — which
    // must keep describing that store — records it here, and ONLY here. This is the cookie/bridge/
    // eval entry point; `send_locked`'s other callers carry per-event hot-path frames
    // (MouseMove/Key/FrameAck) that can never touch a cookie store, so the hot path pays nothing.
    if let ClientSlot::Live(_, log) = &mut *slot {
        log.record_sent(msg);
    }
    Ok(outcome)
}

/// Register (or re-register) an `addJavascriptInterface(object, name)` bridge on `widget`. The
/// Java object + its resolved `@JavascriptInterface` methods are retained in `framework.rs` BEFORE
/// this call; here we only forward the method inventory. If the view is not yet loaded, the
/// registration is buffered and flushed by [`drive`] after `CreateView`; if it is already live,
/// it is sent immediately. Degrades honestly on a latched/absent helper (2026-07-09).
pub fn register_bridge(
    java_vm: jni::vm::JavaVM,
    widget: i64,
    name: String,
    methods: Vec<BridgeMethod>,
) -> Result<(), ClientError> {
    // Buffer first so a pre-load registration survives until the first CreateView.
    buffer_pending_bridge(widget, name.clone(), methods.clone());
    if view_is_tracked(widget) {
        // The view already has a browser: send now (and the buffered copy is harmlessly re-sent
        // by a future re-drive only if this view is closed + re-created).
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
        // Deferred to drive()'s post-CreateView flush; nothing to send (and no reason to spawn).
        Ok(())
    }
}

/// Forward `evaluateJavascript(script, ValueCallback)` — the JSON result routes back as an
/// `EvaluateJsResult` correlated by `request_id` (framework.rs retained the ValueCallback under
/// that id first). Lazily spawns; degrades honestly.
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

/// Fire-and-forget 2-arg `CookieManager.setCookie(url, value)` (v1 `CookieSet`). Lazily spawns.
#[allow(clippy::too_many_arguments)] // 2026-07-09: mirrors the parsed Set-Cookie fields 1:1.
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

/// 3-arg `CookieManager.setCookie(url, value, ValueCallback)` (v2 `CookieSetForResult`): the REAL
/// success flag returns as a `CookieSetResult` correlated by `request_id` (framework.rs retained
/// the callback under that id first). Lazily spawns.
#[allow(clippy::too_many_arguments)] // 2026-07-09: mirrors the parsed Set-Cookie fields + id.
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

/// `removeAllCookies` / `removeSessionCookies` (v1 `CookiesClear`): the completion returns as a
/// (routed) empty `CookieList` correlated by `request_id`. 2026-07-09 divergence: `CookiesClear`
/// is a blanket clear, so `removeSessionCookies` also clears persistent cookies — harmless for the
/// in-memory session store (nothing persists). Lazily spawns.
///
/// 2026-07-16 (§6 🩹➜⛔): during the deferred-spawn window a blanket clear is PROVABLY complete
/// without the engine ([`EarlyCookies`]) and returns [`SendOutcome::AnsweredWithoutEngine`] — the
/// caller must then fire the app's `ValueCallback` itself, because no helper reply is coming.
pub fn cookies_clear(
    java_vm: jni::vm::JavaVM,
    request_id: u32,
) -> Result<SendOutcome, ClientError> {
    send_with_lazy_spawn(java_vm, &ConsumerMsg::CookiesClear { request_id })
}

/// Blocking `CookieManager.getCookie(url)` (v1 `CookieGet`): register a channel waiter, send the
/// request, then park the CALLING thread on the receiver WITHOUT holding [`CLIENT`] (so the reader
/// thread can deliver the reply). On timeout / a latched-or-torn-down helper the waiter is removed
/// and an empty list is returned (the native formats that to `""` — honest degradation). Cookie
/// VALUES never touch a log macro on any path (2026-07-09).
pub fn cookie_get_blocking(
    java_vm: jni::vm::JavaVM,
    url: String,
    timeout: Duration,
) -> Result<Vec<CookieEntry>, ClientError> {
    // 2026-07-09 boundary assertion: a blocking wait ON the io thread can never be answered — the
    // reply's only reader is the parked caller itself. App-code upcalls now run on the dedicated
    // upcall thread, so this cannot happen; if a future change re-introduces an io-thread JNI
    // upcall, fail fast + loud instead of a guaranteed self-stall for the full timeout.
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
    // Register BEFORE sending so no reply can beat the registration.
    match shared().cookie_get_waiters.lock() {
        Ok(mut w) => {
            w.insert(request_id, tx);
        }
        Err(_) => return Err(ClientError::Internal("cookie waiters lock poisoned")),
    }
    match send_with_lazy_spawn(java_vm, &ConsumerMsg::CookieGet { request_id, url }) {
        Ok(SendOutcome::Sent) => {}
        // 2026-07-16 (§6 🩹➜⛔): answered without the engine — the session store is PROVABLY empty
        // ([`EarlyCookies`]), so this IS what CEF would have replied (and ~5 s sooner: a zero-cookie
        // visit only completes on the COOKIE_VISIT_DEADLINE). No reply is coming, so do not park.
        Ok(_) => {
            remove_cookie_waiter(request_id);
            return Ok(Vec::new());
        }
        Err(e) => {
            remove_cookie_waiter(request_id);
            return Err(e);
        }
    }
    match rx.recv_timeout(timeout) {
        Ok(cookies) => Ok(cookies),
        // Timeout OR channel disconnected (shutdown dropped the sender) → honest empty.
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

/// Drop EVERY parked `getCookie` waiter's `Sender` so [`cookie_get_blocking`] wakes immediately
/// (channel `Disconnected` → its honest empty list) instead of blocking its full timeout.
/// 2026-07-09 fix: called on reader-thread exit (helper crash/EOF/protocol error — previously
/// only [`shutdown`] cleared the map, so an in-flight getCookie at helper-death time stalled the
/// full remaining 5 s on the calling ART thread) and by [`shutdown`] (idempotent).
fn wake_all_cookie_waiters() {
    if let Ok(mut w) = shared().cookie_get_waiters.lock() {
        w.clear();
    }
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
/// falls back to a centered rect of the staged frame's own dimensions). 2026-07-17: this is the
/// registry-measured REQUEST, not what is on screen — it is one INPUT to
/// `vk_overlay::resolve_webview_rect`, whose CLAMPED result (published below) is what both the
/// composite and the input hit-test use. Input must never read this directly: it is `None` for the
/// challenge WebView (never measured headless) and it is un-clamped.
pub fn composited_rect() -> Option<(i32, i32, u32, u32)> {
    shared().rect.lock().ok().and_then(|r| *r)
}

/// 2026-07-17: publish the CLAMPED rect the composite just drew `view` at — called by the
/// vk-overlay present path once per present whose blit landed.
pub fn publish_composited_screen_rect(view: i64, rect: (i32, i32, u32, u32)) {
    let (x, y, w, h) = rect;
    if let Ok(mut r) = shared().screen_rect.lock() {
        *r = Some(DrawnRect { view, x, y, w, h });
    }
}

/// 2026-07-17: the rect `view` is ACTUALLY composited at on screen — what the input hit-test maps
/// against, so a click lands where the page is drawn. `None` until this view's first composite
/// lands (nothing drawn ⇒ nothing to click ⇒ input correctly stays with the engine), and `None`
/// for any view other than the one the rect was drawn for (a successor never inherits it).
pub fn composited_screen_rect(view: i64) -> Option<(i32, i32, u32, u32)> {
    match *shared().screen_rect.lock().ok()? {
        Some(r) if r.view == view => Some((r.x, r.y, r.w, r.h)),
        _ => None,
    }
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
        if matches!(&*slot, ClientSlot::Live(_, _)) {
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

/// `View.native_destructor` hook: a WebView was garbage-collected. Runs on ART's FinalizerDaemon
/// thread — never panics/throws; the fast path for every NORMAL view GC is a few atomic loads
/// (2026-07-10: bridge-cleanup gates + the two drive-tracking gates). Sends a best-effort
/// `CloseView` for driven views (per-view close never latches by policy — D5).
pub fn notify_view_freed(widget: i64) {
    // 2026-07-10 fix: bridge registration is INDEPENDENT of drive-tracking —
    // `addJavascriptInterface` retains its JNI globals and buffers the wire inventory BEFORE any
    // helper-availability check, so a never-driven WebView (absent/latched helper: `drive`
    // returns before `record_view`, the view is never tracked) still owns them at finalize time.
    // Release BOTH before the drive-tracking gates below — the old order early-returned first and
    // leaked one BridgeEntry of JNI globals per failed challenge attempt for the process
    // lifetime. The FinalizerDaemon is ART-attached, so the `Global` drops here are cheap.
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

/// 2026-07-10 (web-engine M6, plan §7 #9): the active WebView was detached from the view tree
/// (a fragment teardown funnels `ViewGroup.remove*` → `native_removeView`; challenge16 showed the
/// GC-only `notify_view_freed` left the stale full-window composite covering LoginV2 for ~40 s with
/// NO ViewClosed). Eagerly send `CloseView` so the composite stops on the next present
/// (`vk_overlay` gate = `active_view() != 0`) and input routing to the dead view stops with it; the
/// helper's confirming `ViewClosed` completes teardown through the existing path (its `ACTIVE_VIEW`
/// CAS then no-ops).
///
/// Scoped to the ACTIVE view via CAS `widget → 0`: a MISS (this is not the active view) returns
/// immediately (non-active tracked views keep the GC path). It deliberately does NOT drop the
/// bridge globals or the tracked entry — the Java object is still alive (GC/`notify_view_freed`
/// owns that), and the helper's `ViewClosed` reply removes the tracked entry through the existing
/// reader path. Lock order: no registry lock held here (the caller released it) → CLIENT lock, the
/// same discipline as `notify_view_freed`.
///
/// Recorded divergence (dated): AOSP allows detach-then-reattach without destroy, but no recorded
/// challenge boot re-parents the WebView; a false trigger only blanks the composite (a re-drive
/// restores it) and is made visible by the INFO line below.
pub fn notify_view_detached(widget: i64) {
    if ACTIVE_VIEW
        .compare_exchange(widget, 0, Ordering::Relaxed, Ordering::Relaxed)
        .is_err()
    {
        return; // not the active view — only the active view eager-closes
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

/// PROBE (2026-07-16, `ECLIPSE_WEBVIEW_DEFER_COOKIE_CB`): answer every 3-arg setCookie the deferral
/// is STILL holding at teardown. A permanently stranded app callback is not acceptable even in a
/// diagnostic, so this is the deferral's hard bound: nothing leaves this function still owed.
///
/// # ANSWER, not force-the-spawn — and why that is the honest choice here
///
/// The two options are to cold-start CEF now purely to obtain real flags, or to answer. Answering
/// is honest and forcing is not, for reasons specific to THIS moment:
/// * `false` here is **true**. `setCookie`'s callback value *"indicates whether the cookie was set
///   successfully"* (AOSP `CookieManager.java`, verified 2026-07-16). No engine ever existed on this
///   path — no `CefInitialize`, no cookie store (the empty-store lemma, [`EarlyCookies`]) — so the
///   cookie was, as a matter of fact, never set. Reporting that is an accurate report of a real
///   non-completion, NOT a fabricated verdict: it is exactly what
///   [`crate::framework::drain_all_webview_callbacks`] already means by `false`, and what the
///   3-arg native's own send-failure arm already answers. The flag M4 refused to fabricate is a
///   `true` nobody measured; a `false` for an operation that provably did not happen is the
///   opposite of that.
/// * Forcing a spawn would be the dishonest one. It would run a full engine init (sandbox, GPU,
///   `CefInitialize`) DURING VM teardown, to set cookies into a store that is destroyed
///   milliseconds later, and to answer callbacks whose app is already exiting — a real risk of a
///   hang or a late crash in exchange for a flag with no consumer. That is behaviour changed to
///   satisfy a diagnostic's bookkeeping, which is what CLAUDE.md forbids.
///
/// Delivery reuses the shipped drain: `&Vm` is `!Send`, so the borrow is the type-level proof we are
/// MAIN, and `dispatch_webview_callback_on_main` therefore takes its `InlineOnMainThread` path —
/// the AOSP UI-thread contract is satisfied with no pump and no deadline. Called BEFORE
/// [`retire_main_upcall_dispatch`] and before any join, so nothing is racing it.
fn answer_stranded_deferred_callbacks(vm: &crate::runtime::Vm, ids: &[u32]) {
    tracing::warn!(
        target: "android.webkit.CookieManager",
        stranded = ids.len(),
        "ECLIPSE-DEFER-CB shutdown — {} probe-deferred 3-arg setCookie ValueCallback(s) were never \
         replayed (this boot drove no WebView, so the flush never ran). Answering each FALSE now: \
         no engine ever existed, so the cookie genuinely was never set. Nothing is left stranded.",
        ids.len()
    );
    crate::framework::drain_deferred_cookie_set_callbacks(
        vm,
        "the web engine helper was shut down with probe-deferred setCookie replies outstanding",
    );
}

/// Deliberate teardown: polite `Shutdown` → bounded wait → kill+wait → join the reader (bounded:
/// the child's death forces the reader's EOF) → PUMP while joining the upcall thread → retire the
/// main dispatch. The slot latches so no later drive respawns.
///
/// 2026-07-16 (web-engine M6): takes `&Vm` because this is the one place MAIN would join the
/// upcall thread — and the upcall thread's remaining events (including the honest
/// `drain_all_webview_callbacks` it runs as it exits) now dispatch their app-facing JNI on main and
/// BLOCK until main runs them. A bare `join()` would park main against a thread parked on main.
/// `Vm` is `!Send`, so the borrow is also the type-level proof we ARE main.
pub fn shutdown(vm: &crate::runtime::Vm, deadline: Duration) -> ShutdownReport {
    // PROBE (2026-07-16): 3-arg setCookie ids the deferral is still holding at teardown — nothing
    // will ever replay them, so their ValueCallbacks must be answered HERE (below). Always empty
    // with the gate off, where a `CookieSetForResult` never buffers.
    let mut stranded_cb_ids: Vec<u32> = Vec::new();
    let taken = match CLIENT.lock() {
        Ok(mut slot) => {
            match std::mem::replace(
                &mut *slot,
                ClientSlot::Failed("the web engine helper was shut down".into()),
            ) {
                ClientSlot::Live(c, _log) => Some(c),
                mut other => {
                    // Never-live (or already failed): keep the original state. 2026-07-16: an
                    // Unspawned slot may hold deferred cookie SETs nothing will ever replay — drop
                    // their values here, matching the pending_bridges clear below.
                    if let ClientSlot::Unspawned(early) = &mut other {
                        stranded_cb_ids = early
                            .sets
                            .iter()
                            .filter_map(deferred_cb_request_id)
                            .collect();
                        early.sets.clear();
                    }
                    // 2026-07-16 (the §6 respawn): a `RESPAWN_IN_PROGRESS` park is the ONE state
                    // that must NOT be restored — this latch has to WIN. Restoring it would let
                    // `maybe_respawn_for_app_ua`'s phase 3 match its own park value and install
                    // `Unspawned` OVER this shutdown, so a later drive could spawn a helper after
                    // teardown — breaking this function's own contract ("the slot latches so no
                    // later drive respawns"). The old helper is already being reaped by phase 2, so
                    // there is nothing here to tear down; phase 3 sees the changed reason, drops the
                    // log, and stands down.
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
    // Join the upcall thread AFTER the reader: the reader's exit dropped the channel sender, so
    // the upcall loop finishes its queue, drains every pending ValueCallback honestly
    // (`framework::drain_all_webview_callbacks`), and exits — bounded like the reader join.
    if let Some(h) = client.upcall.take() {
        // 2026-07-16: pump while joining, so every teardown callback still fires on the Looper
        // thread, exactly once, with no timeout and no AOSP divergence. Bounded by `deadline`:
        // the reader is already joined, so its channel sender is dropped and this thread WILL
        // finish its queue and exit. If it somehow has not by the deadline, retire the slot first
        // so its next post degrades to an inline (loudly logged) delivery instead of parking on a
        // main that is about to stop pumping — then the join always completes. Never drops a job.
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
    // Drop every getCookie sender: any thread parked in cookie_get_blocking wakes (Disconnected)
    // and returns its honest empty list instead of blocking the full timeout. (The reader-exit
    // path already did this — idempotent; this also covers a reader that could not be joined.)
    wake_all_cookie_waiters();
    if let Ok(mut b) = pending_bridges().lock() {
        b.clear();
        PENDING_BRIDGE_VIEWS.store(0, Ordering::Relaxed);
    }
    // 2026-07-09 same-pattern audit: after shutdown no upcall thread exists to run a queued
    // drain, so drop every retained @JavascriptInterface global here (2026-07-10: the
    // finalize-time drop_bridges_for path now runs unconditionally, but shutdown must not
    // depend on future finalizers).
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

        // Correct current-version HelloAck → Ok(engine).
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
        // The Hello frame reached the helper side (the consumer's half of the contract).
        let hello = proto::read_consumer_msg(&mut &helper_end).expect("decode Hello");
        assert_eq!(
            hello,
            ConsumerMsg::Hello {
                version: super::super::PROTO_VERSION
            }
        );

        // Unsupported version → the typed mismatch (the consumer-side exact-version gate).
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
    fn crash_kind1_code2_maps_to_the_sandbox_unavailable_reason_and_code0_stays_no_display() {
        // 2026-07-10 (plan M5): the Crash arm is code-KEYED — kind=1 code=2 is the helper's
        // sandbox-policy refusal (both fixes named + the skip marker); kind=1 with any other
        // code (incl. the legacy 0 of an M4-built helper) keeps the no-display reason.
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

        // Legacy code 0 (and every non-2 code) stays the no-display/engine-init reason —
        // backward compatible with an M4-built helper resolved via ECLIPSE_WEBVIEW_HELPER.
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
        // 2026-07-10 (plan M5): the handshake-EOF post-mortem — a child that died inside ld.so
        // (exit 127 before main) gets the pre-spawn probe's missing-lib findings folded into
        // the latched reason; the consumer's own kill() of a hung helper (signal status, no
        // exit code) and non-Handshake failures stay untouched.
        use std::os::unix::process::ExitStatusExt as _;
        let exit_127 = std::process::ExitStatus::from_raw(127 << 8);
        let killed = std::process::ExitStatus::from_raw(9); // SIGKILL — code() is None

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

        // PayloadMissing → names the path + the packaging fix.
        let payload = hostprobe::ProbeOutcome::PayloadMissing {
            libcef_path: std::path::PathBuf::from("/pkg/libcef.so"),
        };
        let ClientError::Handshake(text) = enrich_spawn_failure(base(), &payload, Some(exit_127))
        else {
            panic!("expected Handshake");
        };
        assert!(text.contains("/pkg/libcef.so") && text.contains("package-webview.sh"));

        // No probe findings → the generic actionable ldd hint appended.
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

        // Signal status (our own kill of a hung helper) → base unchanged, no misattribution.
        let ClientError::Handshake(text) = enrich_spawn_failure(base(), &missing, Some(killed))
        else {
            panic!("expected Handshake");
        };
        assert_eq!(text, "protocol error before HelloAck: unexpected EOF");
        // No status at all → base unchanged.
        let ClientError::Handshake(text) = enrich_spawn_failure(base(), &missing, None) else {
            panic!("expected Handshake");
        };
        assert_eq!(text, "protocol error before HelloAck: unexpected EOF");

        // Non-Handshake classes (a live helper that spoke protocol) are never rewritten.
        let vm = enrich_spawn_failure(
            ClientError::VersionMismatch { helper_version: 1 },
            &missing,
            Some(exit_127),
        );
        assert!(matches!(vm, ClientError::VersionMismatch { .. }));
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
        assert!(latched_error(&ClientSlot::Unspawned(EarlyCookies::new())).is_none());
    }

    #[test]
    fn dispatch_extracts_v2_bridge_eval_and_cookie_outputs() {
        // 2026-07-09 (plan M4): the pure dispatch state machine extracts the v2 outputs WITHOUT
        // touching the views map or any global — the reader loop performs the JNI/channel work.
        let mut views: HashMap<i64, ViewShared> = HashMap::new();

        // BridgeCall for an UNTRACKED view is fine (framework validates the registry).
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
    }

    #[test]
    fn normalize_app_user_agent_treats_null_and_empty_as_a_reset_to_the_default() {
        // 2026-07-16 (plan M6): AOSP's setUserAgentString contract, verbatim: "If the string is
        // null OR EMPTY, the system default value will be used"
        // (frameworks/base/core/java/android/webkit/WebSettings.java, verified 2026-07-16). Both
        // reset — `None` here means "Eclipse's fallback", never "send an empty User-Agent".
        assert_eq!(normalize_app_user_agent(None), None);
        assert_eq!(normalize_app_user_agent(Some(String::new())), None);
        // Anything else is the app's intent and is carried VERBATIM — no trimming, no rewriting.
        // This is the app's REAL UA (§6 2026-07-16 💥); the double spaces and the empty `Hybrid()`
        // argument are the app's own bytes, and Eclipse must present them exactly.
        let app_ua = "Mozilla/5.0 (0MB; 960x540; 160x160; 960x540; HTC unknown; unknown) \
                      AppleWebKit/537.36 (KHTML, like Gecko)  ROBLOX Android App 2.724.735 Phone \
                      Hybrid()  GooglePlayStore RobloxApp/2.724.735 (GlobalDist; GooglePlayStore)";
        assert_eq!(
            normalize_app_user_agent(Some(app_ua.to_string())),
            Some(app_ua.to_string())
        );
        // A whitespace-only UA is NOT empty: AOSP resets on empty only, so it is carried through.
        assert_eq!(
            normalize_app_user_agent(Some(" ".to_string())),
            Some(" ".to_string())
        );
    }

    /// A `CookieSet` frame for the deferral pins (values are irrelevant — `offer` never parses).
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

    /// A 3-arg `setCookie` frame — the shape that OWES the app a `ValueCallback`. `expires_epoch_s`
    /// is deliberately NON-zero: it is the field the read-back `CookieEntry` cannot carry, so it is
    /// what proves buffering the ORIGINAL frame is lossless.
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
        // 2026-07-16 (the ECLIPSE_WEBVIEW_DEFER_COOKIE_CB probe). Mirrors the helper crate's
        // `engine::console_text_diag_enabled` / `engine::bridge_diag_enabled` strictness for the
        // same reason: this probe holds a real app callback unanswered, so it must be a DELIBERATE
        // opt-in that no unrelated env value can ever trip.
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
    fn defer_cookie_cb_off_is_a_structural_no_op_for_every_cookie_shape() {
        // THE PIN THAT MAKES THE PROBE SAFE TO SHIP DARK (2026-07-16): with the gate off, `offer`
        // must reproduce the SHIPPED verdicts byte-for-byte — the probe is a measurement, and a
        // measurement that changes the default boot measures itself. Every cookie shape, and the
        // reason strings the boot log greps for, are asserted verbatim.
        let mut early = EarlyCookies::new();
        assert_eq!(
            early.offer(&a_cookie_set_cb(1), false),
            Deferral::NeedsEngine(
                "setCookie(url, value, ValueCallback) — only the engine yields the REAL success flag"
            )
        );
        // The forced spawn must not buffer it: nothing is held, so nothing can strand.
        assert!(early.sets.is_empty());
        assert!(!early.holds_unanswered_callback());
        // And the ops around it are untouched.
        assert_eq!(early.offer(&a_cookie_set("a"), false), Deferral::Buffer);
        assert_eq!(
            early.offer(&ConsumerMsg::CookiesClear { request_id: 2 }, false),
            Deferral::AnswerWithoutEngine
        );
        assert!(early.sets.is_empty());
        assert_eq!(
            early.offer(
                &ConsumerMsg::CookieGet {
                    request_id: 3,
                    url: "https://www.roblox.com/".into(),
                },
                false
            ),
            Deferral::AnswerWithoutEngine
        );
    }

    #[test]
    fn defer_cookie_cb_on_buffers_the_three_arg_set_losslessly_instead_of_spawning() {
        // THE PROBE'S WHOLE POINT (2026-07-16): the app's FIRST cookie op is a 3-arg setCookie, and
        // with the gate off it cold-starts CEF — fixing the GLOBAL CefSettings.user_agent ~30-60 s
        // before the app calls setUserAgentString (§5 ⏳➜🎲). Under the probe it must BUFFER, so no
        // cookie op can fix the UA, and the frame must be the app's ORIGINAL — nothing re-derived.
        let mut early = EarlyCookies::new();
        assert_eq!(early.offer(&a_cookie_set_cb(7), true), Deferral::Buffer);
        assert_eq!(early.sets.len(), 1);
        assert!(early.holds_unanswered_callback());
        // Lossless: the buffered frame IS the app's original, expiry and all (a read-back
        // `CookieEntry` has no `expires_epoch_s` — that asymmetry is why replay must use the frame).
        assert_eq!(early.sets[0], a_cookie_set_cb(7));
        assert_eq!(deferred_cb_request_id(&early.sets[0]), Some(7));
        // Arrival order is preserved across the 2-arg/3-arg mix — the jar's overwrite semantics.
        assert_eq!(early.offer(&a_cookie_set("later"), true), Deferral::Buffer);
        assert_eq!(early.sets, vec![a_cookie_set_cb(7), a_cookie_set("later")]);
    }

    #[test]
    fn defer_cookie_cb_never_lets_a_clear_drop_an_unanswered_callback() {
        // THE STRAND GUARD (2026-07-16). A blanket clear is normally answerable locally by dropping
        // `sets` (the empty-store lemma) — but dropping a frame whose ValueCallback is still owed
        // the engine's REAL flag would strand the app FOREVER, which is unacceptable even in a
        // diagnostic. It must force the spawn instead, so the sets replay and CEF answers each.
        let mut early = EarlyCookies::new();
        assert_eq!(early.offer(&a_cookie_set_cb(1), true), Deferral::Buffer);
        assert!(matches!(
            early.offer(&ConsumerMsg::CookiesClear { request_id: 2 }, true),
            Deferral::NeedsEngine(_)
        ));
        // The frame SURVIVES the refused clear — it must still be there to replay and be answered.
        assert_eq!(early.sets.len(), 1);
        assert!(early.holds_unanswered_callback());
    }

    #[test]
    fn defer_cookie_cb_respects_the_lemma_boundary_and_the_buffer_cap() {
        // The probe widens exactly ONE arm. It must not weaken the proof's boundary: a get with a
        // non-empty buffer still needs Chromium's url/domain/path matching...
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
        // ...and the cap still bounds the held cookie VALUES, degrading to the honest forced spawn.
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
        assert_eq!(full.sets.len(), EarlyCookies::CAP);
        assert!(!full.holds_unanswered_callback());
    }

    #[test]
    fn early_cookies_defer_sets_so_a_cookie_op_never_cold_starts_the_engine() {
        // 2026-07-16 THE ROOT-CAUSE PIN (§6 🩹➜⛔). The confirmed bug: a COOKIE op spawned the
        // helper at AppManager.initialize — 61 s BEFORE the app called setUserAgentString — and
        // `CefSettings.user_agent` is GLOBAL and consumed by `CefInitialize`, so the app's UA (the
        // one carrying the `Hybrid()` token the page's own bridge selector requires) could never
        // reach the engine. This fails the moment a fire-and-forget cookie set force-spawns again.
        let mut early = EarlyCookies::new();
        assert_eq!(early.offer(&a_cookie_set("a"), false), Deferral::Buffer);
        assert_eq!(early.offer(&a_cookie_set("b"), false), Deferral::Buffer);
        assert_eq!(early.sets.len(), 2);
    }

    #[test]
    fn early_cookies_answer_a_blanket_clear_and_an_empty_store_get_without_the_engine() {
        // The empty-store lemma (see `EarlyCookies`): with no helper there is no CefContext, hence
        // no cookie store; the store is created fresh with an EMPTY cache_path (in-memory/incognito)
        // and only Eclipse's own sets can populate it. So a get on an empty buffer IS the empty list
        // CEF would return, and a blanket delete_cookies(NULL, NULL) over a store whose entire
        // content is `sets` is reproduced exactly by dropping `sets`.
        let mut early = EarlyCookies::new();
        assert_eq!(
            early.offer(
                &ConsumerMsg::CookieGet {
                    request_id: 1,
                    url: "https://www.roblox.com/".into(),
                },
                false
            ),
            Deferral::AnswerWithoutEngine
        );
        assert_eq!(early.offer(&a_cookie_set("a"), false), Deferral::Buffer);
        assert_eq!(
            early.offer(&ConsumerMsg::CookiesClear { request_id: 2 }, false),
            Deferral::AnswerWithoutEngine
        );
        // The clear emptied the jar, so a get is answerable again — the post-state matches CEF's.
        assert!(early.sets.is_empty());
        assert_eq!(
            early.offer(
                &ConsumerMsg::CookieGet {
                    request_id: 3,
                    url: "https://www.roblox.com/".into(),
                },
                false
            ),
            Deferral::AnswerWithoutEngine
        );
    }

    #[test]
    fn early_cookies_demand_the_engine_for_matching_and_for_the_real_set_flag() {
        // The proof's BOUNDARY, pinned so it is never quietly widened. A get with a non-empty buffer
        // needs `visit_url_cookies`, whose results are "filtered by the given url scheme, host,
        // domain and path" — Chromium's matching, which Eclipse does not implement and must not
        // start implementing here. And the 3-arg setCookie exists ONLY for the REAL verdict
        // (`set_cookie` "will check for disallowed characters ... and fail without setting the
        // cookie"), so its reply must never be fabricated — nor deferred, which could strand the
        // app's ValueCallback forever on a boot that never drives a WebView.
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
        // A forced spawn must not also lose the buffered sets: they still flush at `ensure_spawned`.
        assert_eq!(early.sets.len(), 1);
    }

    #[test]
    fn early_cookies_are_bounded_and_overflow_forces_the_honest_spawn() {
        // A boot with no login challenge never drives a WebView, so the buffer never flushes while
        // the app keeps setting cookies all session. Cookie VALUES (incl. the auth token) must not
        // grow without bound in the ART process; overflow degrades to the pre-fix behaviour, loudly.
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
        assert_eq!(early.sets.len(), EarlyCookies::CAP);
    }

    #[test]
    fn early_cookie_sets_replay_in_arrival_order() {
        // `ensure_spawned` replays `sets` verbatim, in order, BEFORE the triggering message. Order
        // is the cookie jar's semantics (a later set of the same name overwrites an earlier one),
        // and the frames are the app's ORIGINALS — so `expires_epoch_s`, which the read-back type
        // `CookieEntry` cannot carry, survives. That is why buffering is lossless where a
        // read-back+replay would not be. (The spawn itself is not unit-reachable — no live JavaVM.)
        let mut early = EarlyCookies::new();
        for n in ["first", "second", "third"] {
            assert_eq!(early.offer(&a_cookie_set(n), false), Deferral::Buffer);
        }
        let taken = std::mem::take(&mut early.sets);
        let names: Vec<&str> = taken
            .iter()
            .map(|m| match m {
                ConsumerMsg::CookieSet { name, .. } => name.as_str(),
                _ => "not-a-set",
            })
            .collect();
        assert_eq!(names, vec!["first", "second", "third"]);
    }

    // -----------------------------------------------------------------------
    // The app-UA helper replacement (2026-07-16, the §6 respawn)
    // -----------------------------------------------------------------------

    /// The app's REAL measured User-Agent (§6 2026-07-16 💥). Shared by the pins below so no test
    /// can assert against a UA the app does not actually send.
    const MEASURED_APP_UA: &str = "Mozilla/5.0 (0MB; 960x540; 160x160; 960x540; HTC unknown; \
                                   unknown) AppleWebKit/537.36 (KHTML, like Gecko)  ROBLOX Android \
                                   App 2.724.735 Phone Hybrid()  GooglePlayStore \
                                   RobloxApp/2.724.735 (GlobalDist; GooglePlayStore)";

    #[test]
    fn a_cookie_forced_helper_is_replaced_so_the_apps_user_agent_reaches_the_engine() {
        // 2026-07-16 THE ROOT-CAUSE PIN (§6 respawn). THE CONFIRMED BUG, measured end to end: the
        // app's FIRST WebView-relevant call is a cookie op at AppManager.initialize, which
        // cold-starts the helper ~30–60 s BEFORE setUserAgentString (03:31:24.724 vs 03:32:25.912 —
        // 61 s). CEF's `CefSettings.user_agent` is GLOBAL and consumed by CefInitialize, so that
        // engine presented Eclipse's FALLBACK literal forever, `navigator.userAgent` carried neither
        // `hybrid` nor `android`, the page's own selector returned nativePrefix=null, no bridge
        // existed, and `Load generic challenge failed` fired on EVERY boot.
        //
        // This asserts the EXACT measured state at the load-drive: a live helper that booted with NO
        // app UA (boot_ua=None), the app's real UA now known, no browser yet, a clean replayable
        // log, nothing in flight. It MUST replace the helper. Revert the respawn and this fails.
        assert_eq!(
            respawn_verdict(Some(MEASURED_APP_UA), None, false, 0, true, 0),
            RespawnVerdict::Respawn
        );
        // ...and the string it delivers must be the one that unblocks the page: the wrapper's
        // `nativePrefix` selector requires BOTH substrings, case-insensitively (§6 🏆).
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
        // One assertion per `Keep` arm — every refusal is strictly today's behaviour, said out loud.
        let app = Some(MEASURED_APP_UA);
        // A forced diagnostic UA outranks the app's — a replacement boots the same string.
        assert!(matches!(
            respawn_verdict(app, None, true, 0, true, 0),
            RespawnVerdict::Keep(_)
        ));
        // The app never set a UA: the __webview-test path — nothing changes for it, EVER.
        assert!(matches!(
            respawn_verdict(None, None, false, 0, true, 0),
            RespawnVerdict::Keep(_)
        ));
        // Already correct.
        assert!(matches!(
            respawn_verdict(app, app, false, 0, true, 0),
            RespawnVerdict::Keep(_)
        ));
        // A live browser: a respawn would DESTROY the app's WebView (and the lemma is broken).
        assert!(matches!(
            respawn_verdict(app, None, false, 1, true, 0),
            RespawnVerdict::Keep(_)
        ));
        // The log cannot reproduce the store: a replay would silently lose cookies.
        assert!(matches!(
            respawn_verdict(app, None, false, 0, false, 0),
            RespawnVerdict::Keep(_)
        ));
        // An app callback is in flight: the teardown drain would answer it wrongly (and could park
        // main).
        assert!(matches!(
            respawn_verdict(app, None, false, 0, true, 1),
            RespawnVerdict::Keep(_)
        ));
        // Precedence: the diag rung is checked FIRST, so it wins even over a live view.
        assert!(matches!(
            respawn_verdict(app, None, true, 1, true, 1),
            RespawnVerdict::Keep(_)
        ));
    }

    #[test]
    fn cookie_log_records_sets_after_the_spawn_and_a_clear_truncates_it() {
        // 2026-07-16 (§6 respawn). THE TWO OBLIGATIONS THE DESIGN OWES, pinned together because they
        // are one invariant: the log must equal `apply(frames)` on a store whose entire content is
        // those frames. A set after the helper spawned lands in ITS store, so it MUST be logged; and
        // `delete_cookies(NULL, NULL)` — "If |url| is NULL all cookies for all hosts and domains
        // will be deleted" (pinned bindings) — empties that store, so the log MUST truncate or a
        // replay would RESURRECT cookies the app deliberately cleared.
        let mut log = EarlyCookies::new();
        log.record_sent(&a_cookie_set("a"));
        log.record_sent(&a_cookie_set_cb(1));
        assert_eq!(log.sets, vec![a_cookie_set("a"), a_cookie_set_cb(1)]);
        // A get is read-only (`visit_url_cookies` "Visit a subset of cookies") — never a transcript
        // entry.
        log.record_sent(&ConsumerMsg::CookieGet {
            request_id: 2,
            url: "https://www.roblox.com/".into(),
        });
        assert_eq!(log.sets.len(), 2);
        // The clear truncates — this is the resurrection guard.
        log.record_sent(&ConsumerMsg::CookiesClear { request_id: 3 });
        assert!(log.sets.is_empty());
        assert!(
            log.replayable,
            "a clear leaves an EMPTY log that faithfully describes an EMPTY store"
        );
        // Post-clear sets rebuild the transcript from the truncation point.
        log.record_sent(&a_cookie_set("after"));
        assert_eq!(log.sets, vec![a_cookie_set("after")]);
        // Lossless: the logged frame IS the app's original, expiry and all (`CookieEntry` has no
        // expires_epoch_s — that asymmetry is why the replay must use the frame).
        assert_eq!(log.sets[0], a_cookie_set("after"));
    }

    #[test]
    fn cookie_log_overflow_and_retirement_refuse_the_respawn_instead_of_lying() {
        // The bound must stay (cookie VALUES incl. .ROBLOSECURITY must not grow unbounded in ART),
        // so on overflow the store gains a cookie the log does not — the transcript is broken and
        // the respawn must be REFUSED, never silently degraded into the lossy replay this design
        // rejects.
        let mut log = EarlyCookies::new();
        for i in 0..EarlyCookies::CAP {
            log.record_sent(&a_cookie_set(&format!("c{i}")));
        }
        assert!(log.replayable);
        log.record_sent(&a_cookie_set("overflow"));
        assert_eq!(log.sets.len(), EarlyCookies::CAP, "the bound holds");
        assert!(
            !log.replayable,
            "and the respawn is surrendered, not the bound"
        );
        assert!(matches!(
            respawn_verdict(Some(MEASURED_APP_UA), None, false, 0, log.replayable, 0),
            RespawnVerdict::Keep(_)
        ));
        // Retirement (a browser exists) clears AND poisons — the values must not linger for the
        // session.
        let mut log = EarlyCookies::new();
        log.record_sent(&a_cookie_set("a"));
        log.retire();
        assert!(log.sets.is_empty() && !log.replayable);
    }

    #[test]
    fn next_request_id_is_monotonic_and_skips_zero() {
        // Ids are strictly increasing and never 0 (the sentinel). Two draws differ and are nonzero.
        let a = next_request_id();
        let b = next_request_id();
        assert_ne!(a, 0);
        assert_ne!(b, 0);
        assert_ne!(a, b);
    }

    #[test]
    fn reader_exit_wakes_parked_cookie_getters_immediately() {
        // 2026-07-09 fix pin: the reader's exit path previously left `cookie_get_waiters`
        // populated (only shutdown() cleared it), so a getCookie in flight at helper-death time
        // blocked its FULL 5 s timeout. Reader exit and shutdown now both drop every waiter
        // Sender: a parked receiver wakes with Disconnected (→ the honest empty list) instead of
        // timing out. Pinned on the shared helper both paths call.
        let (tx, rx) = mpsc::channel::<Vec<CookieEntry>>();
        let request_id = next_request_id();
        shared()
            .cookie_get_waiters
            .lock()
            .expect("waiters lock")
            .insert(request_id, tx);
        wake_all_cookie_waiters();
        match rx.recv_timeout(Duration::from_millis(200)) {
            Err(mpsc::RecvTimeoutError::Disconnected) => {}
            other => panic!("expected an immediate Disconnected wake, got {other:?}"),
        }
    }

    #[test]
    fn notify_view_freed_releases_pending_bridges_for_a_never_driven_view() {
        // 2026-07-10 fix pin: bridge state is registered by addJavascriptInterface BEFORE any
        // helper-availability check, so a NEVER-DRIVEN WebView (absent/latched helper — drive()
        // returns before record_view, the view is never tracked, LIVE_VIEWS stays 0) must still
        // have its buffered inventory (and, on the same code path, the framework-side BridgeEntry
        // JNI globals) released at finalize. The old notify_view_freed early-returned at the
        // two-atomic fast gate / !tracked check FIRST, leaking one inventory per failed challenge
        // attempt for the process lifetime. This widget handle is unique to this test.
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
        // 2026-07-10 (web-engine M6): CAS semantics — detaching the ACTIVE view clears ACTIVE_VIEW
        // (the composite/input gate flips off) and best-effort CloseViews; detaching a NON-active
        // view is a no-op on ACTIVE_VIEW (non-active tracked views keep the GC path). In-harness
        // the CLIENT slot is Unspawned, so no wire send happens — this pins the atomic gate only.
        // Unique handles (in-harness, notify_view_detached is the only nonzero writer of ACTIVE_VIEW,
        // so exact-value assertions are stable under parallel `cargo test`).
        let active = 0x00A0_0001_0000_0000_i64;
        let other = 0x00B0_0002_0000_0000_i64;
        ACTIVE_VIEW.store(active, Ordering::Relaxed);
        // A non-active widget: the CAS misses, ACTIVE_VIEW is untouched.
        notify_view_detached(other);
        assert_eq!(
            ACTIVE_VIEW.load(Ordering::Relaxed),
            active,
            "detaching a non-active view must not clear the active gate"
        );
        // The active widget: the CAS clears the gate to 0.
        notify_view_detached(active);
        assert_eq!(
            ACTIVE_VIEW.load(Ordering::Relaxed),
            0,
            "detaching the active view clears ACTIVE_VIEW (composite gate off)"
        );
        // A second detach of the now-cleared widget is a no-op (idempotent).
        notify_view_detached(active);
        assert_eq!(ACTIVE_VIEW.load(Ordering::Relaxed), 0);
        ACTIVE_VIEW.store(0, Ordering::Relaxed);
    }

    #[test]
    fn reader_loop_stays_jni_free_and_hands_bridge_drops_to_the_upcall_thread() {
        // 2026-07-10 fix pin (source-shape, the lifecycle_drivers_call_on_post_create house
        // pattern): dropping BridgeEntry `Global`s on the unattached reader thread performed a
        // hidden scoped AttachCurrentThread/DetachCurrentThread per ref (jni 0.22.4
        // refs/global.rs::drop), so an ART suspend-all pause could stall the io loop until the
        // helper's bounded outbox declared the consumer dead and QUIT. The bridge drop + eval
        // drain must live in upcall_thread_main, never between reader_loop and reader_fatal.
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
        // 2026-07-10 (stale-string pin): the handshake log binds super::PROTO_VERSION — no
        // hardcoded protocol generation may reappear in this module's strings/docs.
        let banned = ["protocol ", "v1"].concat();
        assert!(
            !src.contains(&banned),
            "hardcoded protocol generation string found — log/document PROTO_VERSION instead"
        );
    }
}
