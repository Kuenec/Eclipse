//! CEF glue: one windowless browser per `view_registry` handle, software `OnPaint` BGRA into
//! the shared memfd slots, load/console/crash events onto the wire (web-engine plan M2).
//!
//! 2026-07-03: every shape here is the M1-spike-proven one (browser_host_create_browser_sync,
//! windowless_frame_rate=30, external pump). Engine logging is OFF at the source
//! ([`build_settings`]: `log_severity=DISABLE`, no log_file) and
//! `DisplayHandler::on_console_message` returns 1 — the two stderr URL-leak channels M1
//! measured, both closed. All handler callbacks run on the CEF UI thread == the process main
//! thread (external_message_pump; the pump loop lives in `main.rs`), so the `Mutex` around
//! the engine state is uncontended discipline, not a hot lock.

use crate::logging::{self, RedactedTarget};
use crate::shared::proto::{BridgeMethod, Console, ConsumerMsg, CookieEntry, HelperMsg};
use crate::shared::shm;
use crate::shared::slots::SlotTracker;
use cef::wrapper::message_router::{
    BrowserSideCallback, BrowserSideHandler, BrowserSideRouter, MessageRouterBrowserSide,
    MessageRouterBrowserSideHandlerCallbacks, MessageRouterConfig,
};
use cef::{rc::Rc as _, sys, *};
use std::collections::HashMap;
use std::fs::File;
use std::os::fd::OwnedFd;
use std::os::raw::c_int;
use std::os::unix::fs::FileExt;
use std::sync::atomic::{AtomicBool, AtomicU32, Ordering};
use std::sync::mpsc::{SyncSender, TrySendError};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

const COMPONENT: &str = "engine";
/// v1 fixes two slots per frame buffer (see the protocol spec).
const SLOT_COUNT: u8 = 2;
/// The M1-proven OSR paint rate.
const WINDOWLESS_FPS: c_int = 30;
/// How long a `CookieGet` may wait for its visitor before the accumulated (possibly empty)
/// list is sent anyway. 2026-07-03: CEF destroys an untriggered CefCookieVisitor without any
/// callback when there are zero cookies, and the Rust wrapper exposes no destructor hook —
/// the deadline is the honest completion fallback (revisit at M4 with the router work).
const COOKIE_VISIT_DEADLINE: Duration = Duration::from_secs(5);
/// How long browsers get to fire `on_before_close` after a shutdown request before the
/// helper gives up on a clean CEF shutdown (the spike's unclean-close lesson).
const CLOSE_ALL_DEADLINE: Duration = Duration::from_secs(10);

/// Switches that must NEVER pass through to the engine, stripped defensively in the browser
/// process (`on_before_command_line_processing` in `main.rs` removes each of these):
/// `enable-logging` prints FULL page URLs to stderr (the M1-measured leak channel);
/// `no-sandbox` is banned outright (the M1 rule — the userns sandbox engages by default).
pub const FORBIDDEN_PASSTHROUGH_SWITCHES: &[&str] = &["enable-logging", "no-sandbox"];

/// Whether the env value selects the page-console-TEXT diagnostic (plan M6, 2026-07-10).
/// EXACT-match `"1"` only — never `"true"`, never a truthy substring — so the diagnostic is a
/// deliberate opt-in and can never be tripped by an unrelated env value. Pure/unit-pinned.
///
/// DEV-HOST DIAGNOSTIC ONLY: when enabled, the helper logs raw page console TEXT (page-controlled
/// content that may itself contain URLs) — a diag-enabled log is by definition a dev-host artifact
/// and NEVER a default boot. The source URL stays redacted to scheme+host even in diag mode; only
/// the message text is raw. Off by default (the consumer's structurally text-free INFO line is the
/// default console surface).
pub fn console_text_diag_enabled(v: Option<&str>) -> bool {
    v == Some("1")
}

/// Whether the env value selects the bridge SELF-INTROSPECTION diagnostic (plan M6, 2026-07-16).
/// EXACT-match `"1"` only — mirrors [`console_text_diag_enabled`]'s strictness for the same reason:
/// a deliberate opt-in that no unrelated env value can trip. Pure/unit-pinned.
///
/// DEV-HOST DIAGNOSTIC ONLY. When enabled, the RENDERER installs a load handler that, on each frame
/// load-end, evaluates a script built from Eclipse's OWN bridge inventory and logs what Eclipse's
/// OWN injected stub looks like in that frame (see `build_bridge_introspection_js` in `main.rs`).
/// It answers the M6 frontier question — zero `bridge call received` have EVER reached Eclipse
/// (challenge16..20) with the injection race and callback delivery both already ruled out, so the
/// next evidence needed is what our own injection actually IS at the moment the page has run.
/// Off by default: the renderer returns NO load handler at all, so a default boot pays nothing —
/// no eval, no log, not even an installed handler.
pub fn bridge_diag_enabled(v: Option<&str>) -> bool {
    v == Some("1")
}

/// The per-event page-console-TEXT diagnostic line (plan M6). The `source` stays redacted to
/// scheme+host (a [`RedactedTarget`], unforgeable from a raw URL); only `text` is raw page console
/// content — the sanctioned dev-host-diagnostic exposure gated by [`console_text_diag_enabled`].
/// Pure/unit-pinned. NEVER call this on a default boot.
pub fn format_console_text_line(
    view: i64,
    severity: u8,
    source: &RedactedTarget,
    line: u32,
    text: &str,
) -> String {
    format!(
        "console-text(diag) view={view} level={severity} source={} line={line} text={text}",
        source.as_str()
    )
}

/// The engine identity string for `HelloAck` (from the pinned binding's version constant),
/// e.g. `"cef/149.0.6+g0d0eeb6+chromium-149.0.7827.201"`.
pub fn engine_id() -> String {
    let bytes = sys::CEF_VERSION;
    let text = std::str::from_utf8(&bytes[..bytes.len().saturating_sub(1)]).unwrap_or("unknown");
    format!("cef/{text}")
}

/// The honest, deliberately-identifying User-Agent (plan M4, 2026-07-09). It is GENUINELY
/// Chromium 149 on Linux x86_64 (CEF runs on the host Linux — true), carries a deliberate
/// `Eclipse-WebView/149.0.6` product token, and does NOT impersonate a specific Android device or
/// build. It leaks only the standard reduced-Chrome Linux desktop platform token
/// (`X11; Linux x86_64` — no username/hostname/kernel/GPU). `149.0.0.0` matches the bundled
/// Chromium 149.0.7827.201 under Chromium's UA-reduction convention (minor components zeroed).
/// This literal MUST byte-match the overlay `WebSettings.smali` `getUserAgentString()` FALLBACK
/// const-string and its `getDefaultUserAgent()` return.
///
/// 2026-07-16 (plan M6): this is now the **FALLBACK**, not a policy — it applies only when the app
/// never sets a UA of its own. It is NOT what Eclipse presents when the app configures its WebView:
/// the Roblox app CALLS `WebSettings.setUserAgentString(…)` and ATL's stub silently DISCARDED it
/// for four milestones (§6 2026-07-16 💥), which is what made this literal look like a deliberate
/// "which UA do we present?" honesty policy. It never was one — Eclipse was substituting its own
/// string for the app's. `WebSettings.setUserAgentString` is now honored ([`effective_user_agent`]),
/// so a real boot presents what the app asked for, exactly as AOSP does; this literal is what
/// remains for a WebView nobody configured (e.g. `__webview-test`) and for AOSP's
/// `getDefaultUserAgent`, which is documented to IGNORE `setUserAgentString`
/// (AOSP `frameworks/base/core/java/android/webkit/WebSettings.java`, verified 2026-07-16).
pub const ECLIPSE_USER_AGENT: &str = "Mozilla/5.0 (X11; Linux x86_64) AppleWebKit/537.36 (KHTML, like Gecko) Chrome/149.0.0.0 Safari/537.36 Eclipse-WebView/149.0.6";

/// Resolve the effective User-Agent CEF is initialized with. Pure/unit-pinned. The precedence
/// ladder, highest first (2026-07-16, plan M6):
///
/// 1. `diag` — the `ECLIPSE_WEBVIEW_UA_DIAG` dev-host A/B override, used VERBATIM when non-empty
///    (operator-supplied). It outranks the app so a measurement can still force any UA; while it is
///    in force the Java-side `getUserAgentString()` (which does NOT consult it) deliberately
///    disagrees — acceptable for a dev-host measurement, and one more reason it is never a default.
/// 2. `app` — **the User-Agent the APP set** via `WebSettings.setUserAgentString`, forwarded from
///    the main process by the `ECLIPSE_WEBVIEW_APP_UA` spawn env (see the spawn contract in the
///    root crate's `webview` module docs). This is the normal, shipped path: honoring what the app
///    sets is not impersonation, it is what a faithful runtime does (§6 2026-07-16 💥). Already
///    normalized consumer-side per AOSP's "null or empty ⇒ default" rule, but the empty guard is
///    repeated here so this fn is total on its own inputs.
/// 3. [`ECLIPSE_USER_AGENT`] — the fallback for a WebView the app never configured.
pub fn effective_user_agent<'a>(diag: Option<&'a str>, app: Option<&'a str>) -> &'a str {
    match (diag, app) {
        (Some(ua), _) if !ua.is_empty() => ua,
        (_, Some(ua)) if !ua.is_empty() => ua,
        _ => ECLIPSE_USER_AGENT,
    }
}

/// Pure settings policy (unit-tested): windowless OSR, external pump, sandbox ON
/// (`no_sandbox=0`, never `--no-sandbox`), engine logging DISABLED (`log_severity=DISABLE`,
/// no `log_file`) — the absolute redaction rule at the settings layer — plus the honest
/// deliberate UA ([`ECLIPSE_USER_AGENT`]).
///
/// 2026-07-16 (plan M6): this states the settings policy with the FALLBACK UA. The binary routes
/// through [`build_settings_with_ua`] so the UA has a seam (the app's UA and the
/// `ECLIPSE_WEBVIEW_UA_DIAG` A/B drive it via [`effective_user_agent`]), hence dead-code in a
/// non-test bin build; the suppression is scoped to this one function deliberately (the
/// module-wide `allow` on `mod shared` would hide real rot here).
/// `effective_user_agent_prefers_the_apps_ua_and_falls_back_to_the_eclipse_literal` pins the
/// composition the binary ACTUALLY takes — `build_settings_with_ua(effective_user_agent(..))` — so
/// the guarantee is guarded on the real path, not only on this one.
#[cfg_attr(not(test), allow(dead_code))]
pub fn build_settings() -> Settings {
    build_settings_with_ua(ECLIPSE_USER_AGENT)
}

/// [`build_settings`] with the UA as an explicit input (plan M6, 2026-07-16) — the seam the app's
/// own `setUserAgentString` UA and the `ECLIPSE_WEBVIEW_UA_DIAG` A/B drive via
/// [`effective_user_agent`]. Every other setting is identical to [`build_settings`], which
/// delegates here with the fallback literal; the UA is the ONLY variable this seam exposes.
/// Pure/unit-pinned.
pub fn build_settings_with_ua(ua: &str) -> Settings {
    Settings {
        windowless_rendering_enabled: 1,
        external_message_pump: 1,
        no_sandbox: 0,
        log_severity: LogSeverity::DISABLE,
        user_agent: CefString::from(ua),
        ..Default::default()
    }
}

/// The session-scoped private `RequestContext` settings (plan M4): an EMPTY `cache_path` =
/// in-memory/incognito store (NOT the global default), with `persist_session_cookies` off.
/// Pure/unit-pinned.
pub fn session_context_settings() -> RequestContextSettings {
    RequestContextSettings {
        cache_path: CefString::default(),
        persist_session_cookies: 0,
        ..Default::default()
    }
}

/// Typed, actionable ozone-selection failure.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct NoDisplayError;

impl std::fmt::Display for NoDisplayError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(
            f,
            "no display detected: neither WAYLAND_DISPLAY nor DISPLAY is set and no \
             --ozone-platform override was given; run inside a Wayland or X11 session or pass \
             an explicit --ozone-platform=<wayland|x11>"
        )
    }
}

impl std::error::Error for NoDisplayError {}

/// Explicit ozone-platform selection — NEVER trust Chromium's ozone auto detection (the
/// M1-recorded designed failure: `WAYLAND_DISPLAY` unset while `XDG_SESSION_TYPE=wayland`
/// makes auto pick Wayland and fail to connect). Decision table:
/// explicit override wins; else a set `WAYLAND_DISPLAY` → `wayland`; else a set `DISPLAY` →
/// `x11`; neither → a typed actionable error. Empty env values count as unset;
/// `XDG_SESSION_TYPE` is deliberately never consulted.
pub fn select_ozone(
    override_flag: Option<&str>,
    wayland_display: Option<&str>,
    display: Option<&str>,
) -> Result<String, NoDisplayError> {
    let set = |v: Option<&str>| v.is_some_and(|s| !s.is_empty());
    if let Some(explicit) = override_flag.filter(|s| !s.is_empty()) {
        return Ok(explicit.to_string());
    }
    if set(wayland_display) {
        return Ok("wayland".to_string());
    }
    if set(display) {
        return Ok("x11".to_string());
    }
    Err(NoDisplayError)
}

// ---------------------------------------------------------------------------
// Sandbox-mode selection (plan M5, 2026-07-10 — the dated owner-revisable policy)
// ---------------------------------------------------------------------------

/// The helper's selected sandbox tier (plan M5). Order rationale (2026-07-10, owner-revisable
/// AGENTS.md §6 policy): userns first — the M1-measured default on this class of host AND
/// Chromium's own preference; the SUID `chrome-sandbox` is the fallback tier (Chromium's
/// documented pre-userns mechanism, admin-installed root:root mode 4755); `Degraded` is
/// reachable ONLY through the explicit `webview_allow_unsandboxed` config opt-in — this
/// component renders hostile web content beside the user's session, so silent unsandboxed
/// execution is never acceptable.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum SandboxMode {
    /// Unprivileged user namespaces, verified USABLE by a LIVE probe: `unshare(CLONE_NEWUSER)`
    /// plus a capability-gated `unshare(CLONE_NEWPID)` inside the new namespace (creation alone
    /// false-positives on Ubuntu 24.04+'s permit-then-confine AppArmor default; 2026-07-10).
    Userns,
    /// A root-owned setuid `chrome-sandbox` beside libcef.so (exported via
    /// `CHROME_DEVEL_SANDBOX`, Chromium's documented helper-path override).
    Suid,
    /// The loud, config-gated `--no-sandbox` degradation (`Settings.no_sandbox = 1`).
    Degraded,
}

/// Typed, actionable sandbox refusal: neither tier is available and the opt-in is off.
/// The Display prefix byte-matches the consumer's `SANDBOX_UNAVAILABLE_MARKER`.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SandboxUnavailable;

impl std::fmt::Display for SandboxUnavailable {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(
            f,
            "sandbox unavailable: this host has neither unprivileged user namespaces nor a SUID \
             chrome-sandbox; fixes: enable unprivileged user namespaces (sysctl \
             kernel.unprivileged_userns_clone=1 and user.max_user_namespaces>0; on Ubuntu 23.10+ \
             also kernel.apparmor_restrict_unprivileged_userns=0 or an AppArmor profile), OR \
             install chrome-sandbox beside libcef.so as root:root mode 4755, OR set config \
             webview_allow_unsandboxed=true to accept a loud unsandboxed degradation"
        )
    }
}

impl std::error::Error for SandboxUnavailable {}

/// Pure sandbox-tier decision (the [`select_ozone`] decision-table shape; unit-pinned over the
/// full 2×2×2 input space). Both capability inputs are MEASURED, never knob-file guesses: the
/// caller runs a live userns USABILITY probe (create + capability-use, `probe_userns`) and
/// Chromium's own `chrome-sandbox` acceptance predicate (`probe_suid_sandbox`).
pub fn select_sandbox_mode(
    userns_ok: bool,
    suid_ok: bool,
    allow_unsandboxed: bool,
) -> Result<SandboxMode, SandboxUnavailable> {
    if userns_ok {
        return Ok(SandboxMode::Userns);
    }
    if suid_ok {
        return Ok(SandboxMode::Suid);
    }
    if allow_unsandboxed {
        return Ok(SandboxMode::Degraded);
    }
    Err(SandboxUnavailable)
}

/// Apply the selected mode to the CEF settings: ONLY `Degraded` flips `no_sandbox` (the
/// policy-gated exception to the M1 never---no-sandbox rule). [`build_settings`] itself stays
/// byte-identical (`no_sandbox == 0` — its pin is untouched); this is the one seam that may
/// change it, and only for the helper's own deliberate act.
pub fn apply_sandbox_mode(settings: &mut Settings, mode: &SandboxMode) {
    if matches!(mode, SandboxMode::Degraded) {
        settings.no_sandbox = 1;
    }
}

/// Whether the browser-process strip loop removes `--<name>` from the command line.
/// 2026-07-10 (plan M5): `enable-logging` is ALWAYS stripped (the M1 stderr-URL-leak channel);
/// `no-sandbox` is stripped UNLESS the helper itself entered the policy-gated degradation —
/// CEF propagates `Settings.no_sandbox = 1` onto the command line, and stripping that copy
/// would desync Chromium's sandbox decision from the settings. The strip exists to ban
/// PASS-THROUGH switches, not the helper's own deliberate act.
/// [`FORBIDDEN_PASSTHROUGH_SWITCHES`] stays the documentation constant the loop iterates.
pub fn switch_should_be_stripped(name: &str, degraded: bool) -> bool {
    match name {
        "enable-logging" => true,
        "no-sandbox" => !degraded,
        _ => false,
    }
}

// ---------------------------------------------------------------------------
// Cookie-set rejection classifier (plan M6, 2026-07-10 — observability only)
// ---------------------------------------------------------------------------

/// Name the LIKELY reason a `set_cookie` returned false, mirroring the DOCUMENTED CEF
/// `cef_cookie_manager_t::set_cookie` sync-false predicates (the pinned cef-dll-sys header:
/// "check for disallowed characters (e.g. ';' … within the cookie value) … fail without setting …
/// Returns false (0) if an invalid URL is specified or if cookies cannot be accessed"). Pure and
/// unit-pinned; observability ONLY — it never changes the set outcome. Checked in order; the
/// fallback names the CEF-internal store-unready-at-first-op / engine-sanitization case so a race
/// that matched no local predicate reads honestly. PRIVACY: takes name/value but returns only a
/// static reason — it NEVER embeds them (the caller binds lengths only).
pub fn classify_cookie_set_rejection(
    url: &str,
    name: &str,
    value: &str,
    domain: &str,
    path: &str,
    secure: bool,
) -> &'static str {
    let is_ctrl = |c: char| (c as u32) < 0x20;
    // (1) the URL must redact to a real scheme://host and the scheme must be http(s).
    let redacted = crate::shared::redact::url_scheme_and_host_for_log(url);
    if redacted == crate::shared::redact::NON_URL {
        return "url is not a valid http(s) URL";
    }
    let Some((scheme, host)) = redacted.split_once("://") else {
        return "url is not a valid http(s) URL";
    };
    if scheme != "http" && scheme != "https" {
        return "url is not a valid http(s) URL";
    }
    let host_no_port = host.split(':').next().unwrap_or(host);
    // (2) cookie name.
    if name.chars().any(|c| c == ';' || c == '=' || is_ctrl(c)) {
        return "name contains a disallowed character";
    }
    // (3) cookie value.
    if value.chars().any(|c| c == ';' || is_ctrl(c)) {
        return "value contains a disallowed character (';' or control)";
    }
    // (4) domain charset.
    if domain.chars().any(|c| c == ';' || c == ' ' || is_ctrl(c)) {
        return "domain contains a disallowed character";
    }
    // (5) non-empty domain must domain-match the URL host (strip a leading '.').
    if !domain.is_empty() {
        let d = domain.strip_prefix('.').unwrap_or(domain);
        let matches = host_no_port == d || host_no_port.ends_with(&format!(".{d}"));
        if !matches {
            return "domain does not domain-match the URL host";
        }
    }
    // (6) path charset.
    if path.chars().any(|c| c == ';' || is_ctrl(c)) {
        return "path contains a disallowed character";
    }
    // (7) a Secure cookie requires an https origin.
    if secure && scheme != "https" {
        return "Secure cookie set from a non-https URL";
    }
    // (8) nothing local matched — the CEF-internal case.
    "no local predicate matched — CEF-internal (cookie store unready at first-op, or engine-side sanitization)"
}

// ---------------------------------------------------------------------------
// Render-path detection (plan M5, 2026-07-10 — LOG-ONLY by design)
// ---------------------------------------------------------------------------

/// The render-path detection verdict. LOG-ONLY BY DESIGN: Chromium's own GPU-process fallback
/// is the detect-don't-assume mechanism (M1-measured: NVIDIA via the bundled ANGLE; SwiftShader
/// never mapped on a GPU host) — any Eclipse-side forcing risks exactly the two banned failure
/// modes (failing a no-GPU host / degrading a GPU host into software). The shipped
/// `libvk_swiftshader.so`/`vk_swiftshader_icd.json`/ANGLE set (pinned into the packaged payload
/// by tools/webview-dist/package-webview.sh) is what makes the no-GPU branch a working
/// degradation rather than a failure.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum RenderPathVerdict {
    /// At least one GPU candidate device exists (basenames listed for the log line).
    GpuCandidates(Vec<String>),
    /// No render nodes at all — Chromium will fall back to the bundled SwiftShader.
    SoftwareFallback,
}

/// Pure classification over the enumerated `/dev/dri/renderD*` basenames + the NVIDIA control
/// device's presence. Never gates anything — the caller only logs the verdict.
pub fn classify_render_path(
    dri_render_nodes: &[String],
    nvidia_ctl_present: bool,
) -> RenderPathVerdict {
    let mut devices: Vec<String> = dri_render_nodes.to_vec();
    if nvidia_ctl_present {
        devices.push("nvidiactl".to_string());
    }
    if devices.is_empty() {
        RenderPathVerdict::SoftwareFallback
    } else {
        RenderPathVerdict::GpuCandidates(devices)
    }
}

// ---------------------------------------------------------------------------
// Outbox: CEF callbacks never block on a stalled consumer
// ---------------------------------------------------------------------------

/// What the writer thread sends: a frame, or a frame immediately followed by the
/// sentinel+SCM_RIGHTS memfd (the `FrameBufferNew` adjacency contract).
pub enum Out {
    Msg(HelperMsg),
    MsgWithFd(HelperMsg, OwnedFd),
}

/// Bounded, non-blocking sender into the writer thread. A full queue means the consumer is
/// connected but not reading — policy: treat it as dead (quit path) rather than block CEF
/// callbacks or grow without bound.
#[derive(Clone)]
pub struct Outbox {
    tx: SyncSender<Out>,
    dead: Arc<AtomicBool>,
}

impl Outbox {
    pub fn new(tx: SyncSender<Out>) -> Self {
        Self {
            tx,
            dead: Arc::new(AtomicBool::new(false)),
        }
    }

    fn push(&self, out: Out) {
        if self.dead.load(Ordering::Relaxed) {
            return;
        }
        match self.tx.try_send(out) {
            Ok(()) => {}
            Err(TrySendError::Full(_)) => {
                logging::error(
                    COMPONENT,
                    "outbound queue hit its high-water mark — treating the consumer as dead",
                );
                self.dead.store(true, Ordering::Relaxed);
            }
            Err(TrySendError::Disconnected(_)) => {
                self.dead.store(true, Ordering::Relaxed);
            }
        }
    }

    pub fn send(&self, msg: HelperMsg) {
        self.push(Out::Msg(msg));
    }

    pub fn send_with_fd(&self, msg: HelperMsg, fd: OwnedFd) {
        self.push(Out::MsgWithFd(msg, fd));
    }

    pub fn is_dead(&self) -> bool {
        self.dead.load(Ordering::Relaxed)
    }
}

// ---------------------------------------------------------------------------
// Engine state
// ---------------------------------------------------------------------------

struct PendingData {
    base_url: String,
    data: String,
    mime: String,
}

struct ViewState {
    browser: Option<Browser>,
    width: u16,
    height: u16,
    generation: u32,
    tracker: SlotTracker,
    /// Write side of the current generation's memfd (pwrite; the seals keep size immutable).
    frame_file: Option<File>,
    slot_bytes: u32,
    /// One-shot `loadDataWithBaseURL` payload served by request interception at `base_url`.
    pending_data: Arc<Mutex<Option<PendingData>>>,
    /// The raw target of the last consumer-driven load, if any. 2026-07-03: `CreateView`
    /// bootstraps the browser on an internal `about:blank` navigation — an implementation
    /// artifact, not an app-driven load — and the Android `internalLoadChanged` contract
    /// fires only for driven loads, so `LoadState` events are suppressed until this is set
    /// (and for late `about:blank` bootstrap events once a real target was driven).
    driven_url: Option<String>,
}

#[derive(Default)]
struct CookieAcc {
    cookies: Vec<CookieEntry>,
    finished: bool,
}

struct PendingCookieGet {
    request_id: u32,
    acc: Arc<Mutex<CookieAcc>>,
    deadline: Instant,
}

struct EngineState {
    views: HashMap<i64, ViewState>,
    pending_cookies: Vec<PendingCookieGet>,
    closing_all: bool,
    exit_code: i32,
    close_deadline: Option<Instant>,
    /// The ONE session-scoped private cookie store shared by every view + the CookieManager
    /// (plan M4): an in-memory, incognito `RequestContext` created lazily at the first view/cookie
    /// op. NOT the global default — the `.ROBLOSECURITY` handoff must land in the store the
    /// challenge WebView reads.
    request_context: Option<RequestContext>,
    /// `browser.identifier()` → view handle, built at `create_view` (the bridge router maps a
    /// query's browser back to its view).
    browser_view: HashMap<i32, i64>,
    /// In-flight bridge calls: helper-allocated `call_id` → the router callback that resolves the
    /// page's Promise when the `BridgeResult` arrives (plan M4).
    pending_bridge_calls: HashMap<u32, Arc<Mutex<dyn BrowserSideCallback>>>,
    /// The stored `@JavascriptInterface` inventory per view (interface → methods), re-sent to the
    /// renderer whenever it signals a new main-frame context via `"eclipse.bridge.ready"` — a PULL
    /// model, because a process message sent before the renderer is connected is dropped, and each
    /// navigation is a fresh context (plan M4).
    view_bridges: HashMap<i64, HashMap<String, Vec<BridgeMethod>>>,
}

type Shared = Arc<Mutex<EngineState>>;

fn lock(state: &Shared) -> std::sync::MutexGuard<'_, EngineState> {
    // A poisoned engine mutex means a callback panicked — the state is still structurally
    // valid (no invariants span the lock), so continue with it rather than abort the helper.
    match state.lock() {
        Ok(g) => g,
        Err(poisoned) => poisoned.into_inner(),
    }
}

/// The helper-side engine: the browser table + everything the pump loop drives.
pub struct Engine {
    state: Shared,
    out: Outbox,
    /// The browser-side query router (`cefQuery`), created once (plan M4). The per-view handlers
    /// forward its lifecycle callbacks; `Client::on_process_message_received` forwards replies.
    router: Arc<BrowserSideRouter>,
    /// 2026-07-10 (plan M6): the dev-host page-console-TEXT diagnostic gate (ECLIPSE_WEBVIEW_CONSOLE
    /// =1). Threaded into each view's `HelperDisplayHandler`. Off = the default (no extra line).
    console_text: bool,
}

impl Engine {
    pub fn new(out: Outbox, console_text: bool) -> Self {
        let state: Shared = Arc::new(Mutex::new(EngineState {
            views: HashMap::new(),
            pending_cookies: Vec::new(),
            closing_all: false,
            exit_code: 0,
            close_deadline: None,
            request_context: None,
            browser_view: HashMap::new(),
            pending_bridge_calls: HashMap::new(),
            view_bridges: HashMap::new(),
        }));
        // The browser-side router + its single bridge handler (added on the UI thread == here).
        let router = BrowserSideRouter::new(MessageRouterConfig::default());
        let handler: Arc<dyn BrowserSideHandler> = Arc::new(BridgeHandler {
            state: state.clone(),
            out: out.clone(),
            next_call_id: AtomicU32::new(1),
        });
        router.add_handler(handler, false);
        Self {
            state,
            out,
            router,
            console_text,
        }
    }

    /// The ONE session-scoped in-memory cookie store, created lazily (empty `cache_path` = a
    /// private incognito store, NOT the global default). Shared by every browser + every cookie op.
    fn request_context(&self) -> Option<RequestContext> {
        let mut st = lock(&self.state);
        if st.request_context.is_none() {
            st.request_context =
                request_context_create_context(Some(&session_context_settings()), None);
        }
        st.request_context.clone()
    }

    /// The session store's cookie manager (plan M4): every cookie op routes here, NOT the global
    /// manager, so CookieManager and the WebViews share one coherent jar.
    fn session_cookie_manager(&self) -> Option<CookieManager> {
        self.request_context()
            .and_then(|rc| rc.cookie_manager(None))
    }

    pub fn outbox_dead(&self) -> bool {
        self.out.is_dead()
    }

    /// Handle one decoded consumer message (pump-loop thread == CEF UI thread).
    pub fn handle(&self, msg: ConsumerMsg) {
        match msg {
            ConsumerMsg::Hello { .. } => {
                // Handshake-order violation (main.rs completed the handshake already).
                logging::error(
                    COMPONENT,
                    "protocol violation: repeated Hello — shutting down",
                );
                self.begin_shutdown(2);
            }
            ConsumerMsg::CreateView {
                view,
                width,
                height,
            } => self.create_view(view, width, height),
            ConsumerMsg::CloseView { view } => self.close_view(view),
            ConsumerMsg::ResizeView {
                view,
                width,
                height,
            } => self.resize_view(view, width, height),
            ConsumerMsg::LoadUrl { view, url } => self.load_url(view, &url),
            ConsumerMsg::LoadDataWithBaseUrl {
                view,
                base_url,
                data,
                mime,
                encoding: _,
                history_url: _,
            } => self.load_data_with_base_url(view, base_url, data, mime),
            ConsumerMsg::MouseMove {
                view,
                x,
                y,
                modifiers,
                leave,
            } => self.with_host(view, |host| {
                let event = MouseEvent { x, y, modifiers };
                host.send_mouse_move_event(Some(&event), c_int::from(leave));
            }),
            ConsumerMsg::MouseClick {
                view,
                x,
                y,
                button,
                down,
                click_count,
                modifiers,
            } => self.with_host(view, |host| {
                let event = MouseEvent { x, y, modifiers };
                let button = match button {
                    0 => MouseButtonType::LEFT,
                    1 => MouseButtonType::MIDDLE,
                    _ => MouseButtonType::RIGHT, // decode validated 0..=2
                };
                host.send_mouse_click_event(
                    Some(&event),
                    button,
                    c_int::from(!down),
                    c_int::from(click_count),
                );
            }),
            ConsumerMsg::MouseWheel {
                view,
                x,
                y,
                delta_x,
                delta_y,
                modifiers,
            } => self.with_host(view, |host| {
                let event = MouseEvent { x, y, modifiers };
                host.send_mouse_wheel_event(Some(&event), delta_x, delta_y);
            }),
            ConsumerMsg::Key {
                view,
                kind,
                windows_key_code,
                native_key_code,
                character,
                modifiers,
            } => self.with_host(view, |host| {
                let event = KeyEvent {
                    type_: match kind {
                        0 => KeyEventType::RAWKEYDOWN,
                        1 => KeyEventType::KEYUP,
                        _ => KeyEventType::CHAR, // decode validated 0..=2
                    },
                    modifiers,
                    windows_key_code,
                    native_key_code,
                    character,
                    unmodified_character: character,
                    ..Default::default()
                };
                host.send_key_event(Some(&event));
            }),
            ConsumerMsg::EvaluateJs { view, script } => {
                let browser = self.browser_of(view);
                match browser.as_ref().and_then(|b| b.main_frame()) {
                    Some(frame) => {
                        // The fire-and-forget eval (the v2 EvaluateJsForResult carries the
                        // result path — plan M4, 2026-07-09).
                        frame.execute_java_script(Some(&CefString::from(script.as_str())), None, 0);
                    }
                    None => logging::warn(
                        COMPONENT,
                        &format!("evaluateJs view={view}: no live browser/frame (dropped)"),
                    ),
                }
            }
            ConsumerMsg::CookieSet {
                url,
                name,
                value,
                domain,
                path,
                secure,
                http_only,
                expires_epoch_s,
            } => self.cookie_set(
                &url,
                &name,
                &value,
                &domain,
                &path,
                secure,
                http_only,
                expires_epoch_s,
            ),
            ConsumerMsg::CookieGet { request_id, url } => self.cookie_get(request_id, &url),
            ConsumerMsg::CookiesClear { request_id } => self.cookies_clear(request_id),
            ConsumerMsg::FrameAck {
                view,
                generation,
                seq,
            } => {
                let mut st = lock(&self.state);
                if let Some(v) = st.views.get_mut(&view) {
                    // Stale-generation/seq acks are ignored inside the tracker.
                    if let Some(publish) = v.tracker.on_ack(generation, seq) {
                        self.out.send(HelperMsg::FrameReady {
                            view,
                            generation: publish.generation,
                            slot: publish.slot,
                            seq: publish.seq,
                        });
                    }
                }
            }
            ConsumerMsg::Shutdown => {
                logging::info(COMPONENT, "shutdown requested by the consumer");
                self.begin_shutdown(0);
            }
            // --- v2 (plan M4) ---
            ConsumerMsg::BridgeRegister {
                view,
                name,
                methods,
            } => self.bridge_register(view, &name, &methods),
            ConsumerMsg::BridgeResult {
                call_id,
                ok,
                result_json,
            } => self.bridge_result(call_id, ok, &result_json),
            ConsumerMsg::EvaluateJsForResult {
                view,
                request_id,
                script,
            } => self.evaluate_js_for_result(view, request_id, &script),
            ConsumerMsg::CookieSetForResult {
                request_id,
                url,
                name,
                value,
                domain,
                path,
                secure,
                http_only,
                expires_epoch_s,
            } => self.cookie_set_for_result(
                request_id,
                &url,
                &name,
                &value,
                &domain,
                &path,
                secure,
                http_only,
                expires_epoch_s,
            ),
        }
    }

    /// Store a bridge inventory for `view` (re-sent to the renderer on each `"eclipse.bridge.ready"`
    /// signal — the PULL model) and best-effort forward it now. Payload-free logging.
    fn bridge_register(&self, view: i64, name: &str, methods: &[BridgeMethod]) {
        {
            let mut st = lock(&self.state);
            st.view_bridges
                .entry(view)
                .or_default()
                .insert(name.to_string(), methods.to_vec());
        }
        // Best-effort immediate send (the renderer may not be connected yet — the ready-pull re-sends).
        if let Some(frame) = self.browser_of(view).and_then(|b| b.main_frame()) {
            send_bridge_register_message(&frame, name, methods);
        }
        logging::info(
            COMPONENT,
            &format!(
                "bridge_register view={view} methods={} (names not logged)",
                methods.len()
            ),
        );
    }

    /// Resolve a page bridge Promise: hand the JSON result to the retained router callback.
    fn bridge_result(&self, call_id: u32, ok: bool, result_json: &str) {
        let cb = lock(&self.state).pending_bridge_calls.remove(&call_id);
        let Some(cb) = cb else {
            logging::warn(
                COMPONENT,
                &format!("bridge_result call_id={call_id}: no pending call (dropped)"),
            );
            return;
        };
        // The result JSON is OPAQUE here — never parsed, never logged (only call_id + len).
        if let Ok(guard) = cb.lock() {
            if ok {
                guard.success_str(result_json);
            } else {
                guard.failure(-1, result_json);
            }
        }
        logging::info(
            COMPONENT,
            &format!(
                "bridge_result call_id={call_id} ok={ok} len={}",
                result_json.len()
            ),
        );
    }

    /// Forward an evaluateJavascript request to the view's renderer as `"eclipse.eval"`; if there
    /// is no live browser/frame, answer an honest failure immediately.
    fn evaluate_js_for_result(&self, view: i64, request_id: u32, script: &str) {
        match self.browser_of(view).and_then(|b| b.main_frame()) {
            Some(frame) => {
                if let Some(mut msg) =
                    process_message_create(Some(&CefString::from("eclipse.eval")))
                {
                    if let Some(args) = msg.argument_list() {
                        args.set_size(2);
                        args.set_int(0, request_id as i32);
                        args.set_string(1, Some(&CefString::from(script)));
                    }
                    frame.send_process_message(ProcessId::RENDERER, Some(&mut msg));
                }
            }
            None => self.out.send(HelperMsg::EvaluateJsResult {
                request_id,
                ok: false,
                value_json: "null".to_string(),
            }),
        }
    }

    /// 3-arg setCookie with a real completion (plan M4): set through the SESSION store and answer a
    /// `CookieSetResult` carrying the real success flag (never fabricated).
    #[allow(clippy::too_many_arguments)] // 2026-07-09: mirrors the CookieSetForResult fields.
    fn cookie_set_for_result(
        &self,
        request_id: u32,
        url: &str,
        name: &str,
        value: &str,
        domain: &str,
        path: &str,
        secure: bool,
        http_only: bool,
        expires_epoch_s: i64,
    ) {
        let Some(manager) = self.session_cookie_manager() else {
            self.out.send(HelperMsg::CookieSetResult {
                request_id,
                ok: false,
            });
            return;
        };
        let expires = Basetime {
            val: (expires_epoch_s + 11_644_473_600) * 1_000_000,
        };
        let cookie = Cookie {
            name: CefString::from(name),
            value: CefString::from(value),
            domain: CefString::from(domain),
            path: CefString::from(path),
            secure: c_int::from(secure),
            httponly: c_int::from(http_only),
            has_expires: c_int::from(expires_epoch_s != 0),
            expires,
            ..Default::default()
        };
        let url_cef = CefString::from(url);
        let mut callback = SetCookieResultCallback::new(request_id, self.out.clone());
        if manager.set_cookie(Some(&url_cef), Some(&cookie), Some(&mut callback)) != 1 {
            // 2026-07-10 (plan M6): name the LIKELY sync-false reason (observability only — the
            // ok=false reply below is unchanged). Same redaction as the 2-arg path.
            let predicate = classify_cookie_set_rejection(url, name, value, domain, path, secure);
            logging::warn(
                COMPONENT,
                &format!(
                    "cookie_set: rejected by the cookie manager — {predicate} (url={} domain={} \
                     name_len={} value_len={})",
                    RedactedTarget::from_raw_url(url).as_str(),
                    domain,
                    name.len(),
                    value.len()
                ),
            );
            self.out.send(HelperMsg::CookieSetResult {
                request_id,
                ok: false,
            });
        }
    }

    /// Begin the quit path: close every browser and remember the exit code. The pump loop
    /// keeps pumping until [`Engine::shutdown_state`] reports done.
    pub fn begin_shutdown(&self, exit_code: i32) {
        let mut st = lock(&self.state);
        if st.closing_all {
            // First requested code wins unless a later request escalates to an error.
            if exit_code != 0 && st.exit_code == 0 {
                st.exit_code = exit_code;
            }
            return;
        }
        st.closing_all = true;
        st.exit_code = exit_code;
        st.close_deadline = Some(Instant::now() + CLOSE_ALL_DEADLINE);
        // Views with no live browser can be dropped immediately.
        let no_browser: Vec<i64> = st
            .views
            .iter()
            .filter(|(_, v)| v.browser.is_none())
            .map(|(k, _)| *k)
            .collect();
        for view in no_browser {
            st.views.remove(&view);
            self.out.send(HelperMsg::ViewClosed { view });
        }
        for v in st.views.values() {
            if let Some(host) = v.browser.as_ref().and_then(|b| b.host()) {
                host.close_browser(1);
            }
        }
    }

    /// `Some((exit_code, clean))` once the quit path has finished (all browsers closed, or
    /// the close deadline passed → unclean: skip `cef::shutdown()` like the spike).
    pub fn shutdown_state(&self) -> Option<(i32, bool)> {
        let st = lock(&self.state);
        if !st.closing_all {
            return None;
        }
        if st.views.is_empty() {
            return Some((st.exit_code, true));
        }
        match st.close_deadline {
            Some(deadline) if Instant::now() > deadline => Some((st.exit_code, false)),
            _ => None,
        }
    }

    /// Periodic work off the pump loop: cookie-visit completion/deadlines.
    pub fn poll(&self) {
        let mut due: Vec<(u32, Vec<CookieEntry>, bool)> = Vec::new();
        {
            let mut st = lock(&self.state);
            st.pending_cookies.retain(|p| {
                let acc = match p.acc.lock() {
                    Ok(g) => g,
                    Err(poisoned) => poisoned.into_inner(),
                };
                if acc.finished || Instant::now() > p.deadline {
                    due.push((p.request_id, acc.cookies.clone(), acc.finished));
                    false
                } else {
                    true
                }
            });
        }
        for (request_id, cookies, finished) in due {
            if !finished {
                // Zero cookies never trigger the visitor — see COOKIE_VISIT_DEADLINE.
                logging::warn(
                    COMPONENT,
                    &format!(
                        "cookie visit request_id={request_id} completed by deadline with {} \
                         cookie(s)",
                        cookies.len()
                    ),
                );
            }
            self.out.send(HelperMsg::CookieList {
                request_id,
                cookies,
            });
        }
    }

    fn browser_of(&self, view: i64) -> Option<Browser> {
        lock(&self.state)
            .views
            .get(&view)
            .and_then(|v| v.browser.clone())
    }

    fn with_host(&self, view: i64, f: impl FnOnce(BrowserHost)) {
        match self.browser_of(view).and_then(|b| b.host()) {
            Some(host) => f(host),
            None => logging::warn(
                COMPONENT,
                &format!("input for view={view} dropped: no live browser"),
            ),
        }
    }

    fn create_view(&self, view: i64, width: u16, height: u16) {
        {
            let st = lock(&self.state);
            if st.views.contains_key(&view) {
                logging::warn(
                    COMPONENT,
                    &format!("create_view view={view}: already exists"),
                );
                return;
            }
            if st.closing_all {
                return;
            }
        }
        // 1. The frame buffer for generation 1, announced (with its fd) BEFORE the browser
        //    exists so the consumer can map before the first FrameReady.
        let generation = 1u32;
        let (memfd, slot_bytes) = match shm::create_sealed_frame_memfd(width, height, SLOT_COUNT) {
            Ok(pair) => pair,
            Err(e) => {
                logging::error(COMPONENT, &format!("create_view view={view}: memfd: {e}"));
                self.out.send(HelperMsg::Crash {
                    view,
                    kind: 2,
                    code: 0,
                });
                return;
            }
        };
        let frame_file = File::from(match memfd.try_clone() {
            Ok(dup) => dup,
            Err(e) => {
                logging::error(COMPONENT, &format!("create_view view={view}: dup: {e}"));
                self.out.send(HelperMsg::Crash {
                    view,
                    kind: 2,
                    code: 0,
                });
                return;
            }
        });
        let pending_data: Arc<Mutex<Option<PendingData>>> = Arc::new(Mutex::new(None));
        lock(&self.state).views.insert(
            view,
            ViewState {
                browser: None,
                width,
                height,
                generation,
                tracker: SlotTracker::new(generation),
                frame_file: Some(frame_file),
                slot_bytes,
                pending_data: pending_data.clone(),
                driven_url: None,
            },
        );
        self.out.send_with_fd(
            HelperMsg::FrameBufferNew {
                view,
                generation,
                width,
                height,
                stride: 4 * u32::from(width),
                slot_bytes,
                slot_count: SLOT_COUNT,
            },
            memfd,
        );

        // 2. The windowless browser, with this view's own handler set (the router is threaded into
        //    the life-span/request/client handlers so it can cancel pending queries + route replies).
        let mut client = HelperClient::new(
            HelperLoadHandler::new(view, self.state.clone(), self.out.clone()),
            HelperLifeSpanHandler::new(
                view,
                self.state.clone(),
                self.out.clone(),
                self.router.clone(),
            ),
            HelperRenderHandler::new(view, self.state.clone(), self.out.clone()),
            HelperDisplayHandler::new(view, self.out.clone(), self.console_text),
            HelperRequestHandler::new(
                view,
                self.state.clone(),
                self.out.clone(),
                pending_data,
                self.router.clone(),
            ),
            ClientDeps {
                out: self.out.clone(),
                router: self.router.clone(),
                state: self.state.clone(),
            },
        );
        let window_info = WindowInfo {
            windowless_rendering_enabled: 1,
            ..Default::default()
        };
        let browser_settings = BrowserSettings {
            windowless_frame_rate: WINDOWLESS_FPS,
            ..Default::default()
        };
        let url = CefString::from("about:blank");
        // The ONE session-scoped store (plan M4) — every view shares it, so the cookie handoff is
        // coherent. Created lazily here (or at the first cookie op) and memoized.
        let mut request_context = self.request_context();
        let browser = browser_host_create_browser_sync(
            Some(&window_info),
            Some(&mut client),
            Some(&url),
            Some(&browser_settings),
            None,
            request_context.as_mut(),
        );
        let mut st = lock(&self.state);
        match browser {
            Some(b) => {
                st.browser_view.insert(b.identifier(), view);
                if let Some(v) = st.views.get_mut(&view) {
                    v.browser = Some(b);
                }
                drop(st);
                logging::info(
                    COMPONENT,
                    &format!("view={view} created {width}x{height} generation={generation}"),
                );
            }
            None => {
                st.views.remove(&view);
                drop(st);
                logging::error(
                    COMPONENT,
                    &format!("create_view view={view}: browser creation failed"),
                );
                self.out.send(HelperMsg::Crash {
                    view,
                    kind: 2,
                    code: 0,
                });
            }
        }
    }

    fn resize_view(&self, view: i64, width: u16, height: u16) {
        let (memfd, slot_bytes) = match shm::create_sealed_frame_memfd(width, height, SLOT_COUNT) {
            Ok(pair) => pair,
            Err(e) => {
                logging::error(COMPONENT, &format!("resize_view view={view}: memfd: {e}"));
                self.out.send(HelperMsg::Crash {
                    view,
                    kind: 2,
                    code: 0,
                });
                return;
            }
        };
        let frame_file = match memfd.try_clone() {
            Ok(dup) => File::from(dup),
            Err(e) => {
                logging::error(COMPONENT, &format!("resize_view view={view}: dup: {e}"));
                return;
            }
        };
        let mut st = lock(&self.state);
        let Some(v) = st.views.get_mut(&view) else {
            logging::warn(COMPONENT, &format!("resize_view view={view}: unknown view"));
            return;
        };
        v.width = width;
        v.height = height;
        v.generation += 1;
        let generation = v.generation;
        v.tracker.reset(generation);
        v.frame_file = Some(frame_file);
        v.slot_bytes = slot_bytes;
        let browser = v.browser.clone();
        drop(st);
        self.out.send_with_fd(
            HelperMsg::FrameBufferNew {
                view,
                generation,
                width,
                height,
                stride: 4 * u32::from(width),
                slot_bytes,
                slot_count: SLOT_COUNT,
            },
            memfd,
        );
        // CEF re-queries view_rect (now the new dims) and repaints at the new size; frames
        // still in flight at the old dims are dropped by the on_paint dimension check.
        if let Some(host) = browser.as_ref().and_then(|b| b.host()) {
            host.was_resized();
        }
    }

    fn close_view(&self, view: i64) {
        let browser = {
            let mut st = lock(&self.state);
            match st.views.get(&view) {
                Some(v) => match v.browser.clone() {
                    Some(b) => Some(b),
                    None => {
                        // Never got a browser: complete the close immediately.
                        st.views.remove(&view);
                        None
                    }
                },
                None => {
                    logging::warn(COMPONENT, &format!("close_view view={view}: unknown view"));
                    return;
                }
            }
        };
        match browser.and_then(|b| b.host()) {
            Some(host) => host.close_browser(1), // completion → on_before_close → ViewClosed
            None => self.out.send(HelperMsg::ViewClosed { view }),
        }
    }

    fn load_url(&self, view: i64, url: &str) {
        let target = RedactedTarget::from_raw_url(url);
        match self.browser_of(view).and_then(|b| b.main_frame()) {
            Some(frame) => {
                logging::info(
                    COMPONENT,
                    &logging::format_load_event("drive", view, &target),
                );
                // A load is now DRIVEN: LoadState events for this view become live (see
                // ViewState::driven_url). Set before load_url — same thread as callbacks.
                if let Some(v) = lock(&self.state).views.get_mut(&view) {
                    v.driven_url = Some(url.to_string());
                }
                frame.load_url(Some(&CefString::from(url)));
            }
            None => logging::warn(
                COMPONENT,
                &logging::format_load_event("drive-dropped-no-browser", view, &target),
            ),
        }
    }

    /// `loadDataWithBaseURL`: CEF has no direct 5-arg equivalent — serve `data` at `base_url`
    /// through ONE-SHOT request interception ([`HelperRequestHandler`]) and navigate there.
    /// 2026-07-03: origin/history semantics are a recorded risk; every recorded challenge
    /// boot drives loadUrl, and this path gets real validation at M4/M6.
    fn load_data_with_base_url(&self, view: i64, base_url: String, data: String, mime: String) {
        let base = RedactedTarget::from_raw_url(&base_url);
        let (browser, pending) = {
            let st = lock(&self.state);
            match st.views.get(&view) {
                Some(v) => (v.browser.clone(), v.pending_data.clone()),
                None => (None, Arc::new(Mutex::new(None))),
            }
        };
        match browser.and_then(|b| b.main_frame()) {
            Some(frame) => {
                logging::info(
                    COMPONENT,
                    &logging::format_load_data_event(view, &mime, &base),
                );
                if let Ok(mut slot) = pending.lock() {
                    *slot = Some(PendingData {
                        base_url: base_url.clone(),
                        data,
                        mime,
                    });
                }
                if let Some(v) = lock(&self.state).views.get_mut(&view) {
                    v.driven_url = Some(base_url.clone());
                }
                frame.load_url(Some(&CefString::from(base_url.as_str())));
            }
            None => logging::warn(
                COMPONENT,
                &logging::format_load_data_event(view, &mime, &base),
            ),
        }
    }

    #[allow(clippy::too_many_arguments)] // 2026-07-03: mirrors the 8-field CookieSet wire message 1:1
    fn cookie_set(
        &self,
        url: &str,
        name: &str,
        value: &str,
        domain: &str,
        path: &str,
        secure: bool,
        http_only: bool,
        expires_epoch_s: i64,
    ) {
        let Some(manager) = self.session_cookie_manager() else {
            logging::error(COMPONENT, "cookie_set: no session cookie manager");
            return;
        };
        // CEF Basetime = microseconds since 1601-01-01; Unix epoch offset 11644473600 s.
        let expires = Basetime {
            val: (expires_epoch_s + 11_644_473_600) * 1_000_000,
        };
        let cookie = Cookie {
            name: CefString::from(name),
            value: CefString::from(value),
            domain: CefString::from(domain),
            path: CefString::from(path),
            secure: c_int::from(secure),
            httponly: c_int::from(http_only),
            has_expires: c_int::from(expires_epoch_s != 0),
            expires,
            ..Default::default()
        };
        let url_cef = CefString::from(url);
        // 2026-07-10 (plan M6): the 2-arg path was fire-and-forget (callback=None), so an ASYNC
        // engine-sanitization failure was silent. Attach a LOG-ONLY completion callback — it WARNs
        // on an async failure and emits NO wire message (the v1 fire-and-forget layout stays frozen).
        let mut callback = LogOnlySetCookieCallback::new(
            RedactedTarget::from_raw_url(url).as_str().to_string(),
            domain.to_string(),
            name.len(),
            value.len(),
        );
        if manager.set_cookie(Some(&url_cef), Some(&cookie), Some(&mut callback)) != 1 {
            let predicate = classify_cookie_set_rejection(url, name, value, domain, path, secure);
            logging::warn(
                COMPONENT,
                &format!(
                    "cookie_set: rejected by the cookie manager — {predicate} (url={} domain={} \
                     name_len={} value_len={})",
                    RedactedTarget::from_raw_url(url).as_str(),
                    domain,
                    name.len(),
                    value.len()
                ),
            );
        }
    }

    fn cookie_get(&self, request_id: u32, url: &str) {
        let Some(manager) = self.session_cookie_manager() else {
            logging::error(COMPONENT, "cookie_get: no session cookie manager");
            self.out.send(HelperMsg::CookieList {
                request_id,
                cookies: Vec::new(),
            });
            return;
        };
        let acc: Arc<Mutex<CookieAcc>> = Arc::default();
        let mut visitor = ListCookieVisitor::new(acc.clone());
        let url = CefString::from(url);
        if manager.visit_url_cookies(Some(&url), 1, Some(&mut visitor)) != 1 {
            // Cookies cannot be accessed: answer honestly and immediately.
            self.out.send(HelperMsg::CookieList {
                request_id,
                cookies: Vec::new(),
            });
            return;
        }
        lock(&self.state).pending_cookies.push(PendingCookieGet {
            request_id,
            acc,
            deadline: Instant::now() + COOKIE_VISIT_DEADLINE,
        });
    }

    fn cookies_clear(&self, request_id: u32) {
        let Some(manager) = self.session_cookie_manager() else {
            logging::error(COMPONENT, "cookies_clear: no session cookie manager");
            self.out.send(HelperMsg::CookieList {
                request_id,
                cookies: Vec::new(),
            });
            return;
        };
        let mut callback = ClearDoneCallback::new(request_id, self.out.clone());
        if manager.delete_cookies(None, None, Some(&mut callback)) != 1 {
            self.out.send(HelperMsg::CookieList {
                request_id,
                cookies: Vec::new(),
            });
        }
    }
}

// ---------------------------------------------------------------------------
// Per-view CEF handlers (one client per browser — each carries its view handle)
// ---------------------------------------------------------------------------

/// LoadState fidelity rule (2026-07-03): the Android `internalLoadChanged` contract fires
/// only for loads the app DROVE. `CreateView` bootstraps the browser on an internal
/// `about:blank` navigation, so events are suppressed (a) until the first driven load and
/// (b) for late `about:blank` bootstrap events once a real target was driven — otherwise a
/// consumer would credit the bootstrap's 0/3 to its own load (the drive-run1 finding).
fn suppress_load_state(driven_url: Option<&str>, frame_url: &str) -> bool {
    match driven_url {
        None => true,
        Some(driven) => frame_url == "about:blank" && !urls_equivalent(driven, "about:blank"),
    }
}

wrap_load_handler! {
    struct HelperLoadHandler {
        view: i64,
        state: Shared,
        out: Outbox,
    }

    impl LoadHandler {
        fn on_load_start(
            &self,
            _browser: Option<&mut Browser>,
            frame: Option<&mut Frame>,
            _transition_type: TransitionType,
        ) {
            let Some(frame) = frame else { return };
            if frame.is_main() == 0 {
                return;
            }
            let raw_url = CefString::from(&frame.url()).to_string();
            let target = RedactedTarget::from_raw_url(&raw_url);
            let suppressed = {
                let st = lock(&self.state);
                let driven = st.views.get(&self.view).and_then(|v| v.driven_url.as_deref());
                suppress_load_state(driven, &raw_url)
            };
            if suppressed {
                logging::info(
                    COMPONENT,
                    &logging::format_load_event("started-bootstrap-suppressed", self.view, &target),
                );
                return;
            }
            logging::info(
                COMPONENT,
                &logging::format_load_event("started", self.view, &target),
            );
            // internalLoadChanged code 0 (started).
            self.out.send(HelperMsg::LoadState {
                view: self.view,
                state: 0,
                http_status: 0,
            });
        }

        fn on_load_end(
            &self,
            _browser: Option<&mut Browser>,
            frame: Option<&mut Frame>,
            http_status_code: ::std::os::raw::c_int,
        ) {
            let Some(frame) = frame else { return };
            if frame.is_main() == 0 {
                return;
            }
            let raw_url = CefString::from(&frame.url()).to_string();
            let target = RedactedTarget::from_raw_url(&raw_url);
            let suppressed = {
                let st = lock(&self.state);
                let driven = st.views.get(&self.view).and_then(|v| v.driven_url.as_deref());
                suppress_load_state(driven, &raw_url)
            };
            if suppressed {
                logging::info(
                    COMPONENT,
                    &logging::format_load_event("finished-bootstrap-suppressed", self.view, &target),
                );
                return;
            }
            logging::info(
                COMPONENT,
                &format!(
                    "{} http_status={http_status_code}",
                    logging::format_load_event("finished", self.view, &target)
                ),
            );
            // internalLoadChanged code 3 (finished).
            self.out.send(HelperMsg::LoadState {
                view: self.view,
                state: 3,
                http_status: http_status_code,
            });
        }

        fn on_load_error(
            &self,
            _browser: Option<&mut Browser>,
            frame: Option<&mut Frame>,
            error_code: Errorcode,
            _error_text: Option<&CefString>,
            failed_url: Option<&CefString>,
        ) {
            let is_main = frame.map(|f| f.is_main() != 0).unwrap_or(false);
            let code = sys::cef_errorcode_t::from(error_code) as i32;
            let target = failed_url
                .map(|u| RedactedTarget::from_raw_url(&u.to_string()))
                .unwrap_or_else(|| RedactedTarget::from_raw_url(""));
            logging::warn(
                COMPONENT,
                &format!(
                    "{} code={code} main_frame={is_main}",
                    logging::format_load_event("error", self.view, &target)
                ),
            );
        }
    }
}

wrap_life_span_handler! {
    struct HelperLifeSpanHandler {
        view: i64,
        state: Shared,
        out: Outbox,
        router: Arc<BrowserSideRouter>,
    }

    impl LifeSpanHandler {
        fn on_before_close(&self, browser: Option<&mut Browser>) {
            // Cancel any pending bridge queries for this browser (the router contract), THEN drop
            // the view state (its memfd write handle with it) and complete CloseView/Shutdown.
            self.router.on_before_close(browser.map(|b| b.clone()));
            lock(&self.state).views.remove(&self.view);
            self.out.send(HelperMsg::ViewClosed { view: self.view });
            logging::info(COMPONENT, &format!("view={} closed", self.view));
        }
    }
}

wrap_render_handler! {
    struct HelperRenderHandler {
        view: i64,
        state: Shared,
        out: Outbox,
    }

    impl RenderHandler {
        fn view_rect(&self, _browser: Option<&mut Browser>, rect: Option<&mut Rect>) {
            let Some(rect) = rect else { return };
            let (w, h) = {
                let st = lock(&self.state);
                match st.views.get(&self.view) {
                    Some(v) => (v.width, v.height),
                    // The view can already be gone during teardown; any nonzero rect is fine
                    // (frames at stale dims are dropped by on_paint).
                    None => (1, 1),
                }
            };
            rect.x = 0;
            rect.y = 0;
            rect.width = c_int::from(w);
            rect.height = c_int::from(h);
        }

        fn screen_info(
            &self,
            _browser: Option<&mut Browser>,
            screen_info: Option<&mut ScreenInfo>,
        ) -> ::std::os::raw::c_int {
            if let Some(screen_info) = screen_info {
                screen_info.device_scale_factor = 1.0;
                return 1;
            }
            0
        }

        fn on_paint(
            &self,
            _browser: Option<&mut Browser>,
            type_: PaintElementType,
            _dirty_rects: Option<&[Rect]>,
            buffer: *const u8,
            width: ::std::os::raw::c_int,
            height: ::std::os::raw::c_int,
        ) {
            // PET_VIEW frames only (the default variant) — the M1 spike filter.
            if type_ != PaintElementType::default() {
                return;
            }
            if buffer.is_null() || width <= 0 || height <= 0 {
                return;
            }
            let mut st = lock(&self.state);
            let Some(v) = st.views.get_mut(&self.view) else {
                return;
            };
            // Frames whose dimensions mismatch the current generation are stale (resize in
            // flight) — dropped by protocol definition.
            if width != c_int::from(v.width) || height != c_int::from(v.height) {
                return;
            }
            let (slot, publish) = v.tracker.on_paint();
            let offset = u64::from(v.slot_bytes) * u64::from(slot);
            let len = v.slot_bytes as usize;
            let Some(file) = v.frame_file.as_ref() else {
                return;
            };
            // SAFETY: CEF guarantees `buffer` points to width*height tightly-packed BGRA
            // pixels for the duration of this callback (cef_render_handler_t::on_paint
            // contract), and len == 4*width*height for the matching dims checked above.
            let pixels = unsafe { std::slice::from_raw_parts(buffer, len) };
            if let Err(e) = file.write_all_at(pixels, offset) {
                logging::error(
                    COMPONENT,
                    &format!("on_paint view={}: memfd write failed: {e}", self.view),
                );
                return;
            }
            if let Some(p) = publish {
                let view = self.view;
                drop(st);
                self.out.send(HelperMsg::FrameReady {
                    view,
                    generation: p.generation,
                    slot: p.slot,
                    seq: p.seq,
                });
            }
        }
    }
}

wrap_display_handler! {
    struct HelperDisplayHandler {
        view: i64,
        out: Outbox,
        console_text: bool,
    }

    impl DisplayHandler {
        fn on_console_message(
            &self,
            _browser: Option<&mut Browser>,
            level: LogSeverity,
            message: Option<&CefString>,
            source: Option<&CefString>,
            line: ::std::os::raw::c_int,
        ) -> ::std::os::raw::c_int {
            let severity = sys::cef_log_severity_t::from(level) as i32;
            let source = source.map(|s| s.to_string()).unwrap_or_default();
            let message = message.map(|m| m.to_string()).unwrap_or_default();
            let severity_u8 = severity.clamp(0, 255) as u8;
            let line_u32 = line.max(0) as u32;
            // 2026-07-10 (plan M6): the dev-host page-console-TEXT diagnostic (ECLIPSE_WEBVIEW_CONSOLE
            // =1). Default OFF → NO extra line (the consumer's structurally text-free INFO event is
            // the default surface — no duplication). When ON, log the RAW text HELPER-SIDE only (it
            // never crosses the frozen text-free wire Console); the source stays redacted to
            // scheme+host even here.
            if self.console_text {
                logging::warn(
                    COMPONENT,
                    &format_console_text_line(
                        self.view,
                        severity_u8,
                        &RedactedTarget::from_raw_url(&source),
                        line_u32,
                        &message,
                    ),
                );
            }
            // Console::from_raw redacts the source and STRUCTURALLY drops the text — only
            // its length crosses the wire.
            self.out.send(HelperMsg::Console {
                view: self.view,
                console: Console::from_raw(severity_u8, &source, line_u32, &message),
            });
            // Returning 1 suppresses Chromium's default console-to-stderr forwarding — the
            // exact channel M1 measured leaking full page URLs.
            1
        }
    }
}

wrap_request_handler! {
    struct HelperRequestHandler {
        view: i64,
        state: Shared,
        out: Outbox,
        pending_data: Arc<Mutex<Option<PendingData>>>,
        router: Arc<BrowserSideRouter>,
    }

    impl RequestHandler {
        fn on_render_view_ready(&self, browser: Option<&mut Browser>) {
            // 2026-07-10 (plan M6): the RELIABLE bridge-inventory push. The pinned cef-dll-sys
            // header: "Called on the browser process UI thread when the render view associated
            // with |browser| is ready to receive/handle IPC messages in the render process" — it
            // fires for the INITIAL renderer AND after every renderer process swap, which the
            // best-effort immediate send in bridge_register can miss (BridgeRegister and the
            // renderer connect can share a millisecond; challenge16 log 1233-1235). Re-push this
            // view's whole @JavascriptInterface inventory so the stubs exist before page scripts.
            let Some(browser) = browser else { return };
            let Some(frame) = browser.main_frame() else { return };
            let bridges: Vec<(String, Vec<BridgeMethod>)> = {
                let st = lock(&self.state);
                st.view_bridges
                    .get(&self.view)
                    .map(|m| m.iter().map(|(k, v)| (k.clone(), v.clone())).collect())
                    .unwrap_or_default()
            };
            let ifaces = bridges.len();
            for (name, methods) in bridges {
                send_bridge_register_message(&frame, &name, &methods);
            }
            logging::info(
                COMPONENT,
                &format!(
                    "bridge inventory pushed on render-view-ready view={} ifaces={ifaces}",
                    self.view
                ),
            );
        }

        fn on_before_browse(
            &self,
            browser: Option<&mut Browser>,
            frame: Option<&mut Frame>,
            _request: Option<&mut Request>,
            _user_gesture: ::std::os::raw::c_int,
            _is_redirect: ::std::os::raw::c_int,
        ) -> ::std::os::raw::c_int {
            // Cancel pending queries on a main-frame navigation (the router contract); allow the
            // navigation (return 0).
            self.router
                .on_before_browse(browser.map(|b| b.clone()), frame.map(|f| f.clone()));
            0
        }

        fn on_render_process_terminated(
            &self,
            browser: Option<&mut Browser>,
            _status: TerminationStatus,
            error_code: ::std::os::raw::c_int,
            _error_string: Option<&CefString>,
        ) {
            self.router.on_render_process_terminated(browser.map(|b| b.clone()));
            logging::error(
                COMPONENT,
                &format!("view={}: renderer process terminated code={error_code}", self.view),
            );
            self.out.send(HelperMsg::Crash {
                view: self.view,
                kind: 0,
                code: error_code,
            });
        }

        fn resource_request_handler(
            &self,
            _browser: Option<&mut Browser>,
            _frame: Option<&mut Frame>,
            request: Option<&mut Request>,
            _is_navigation: ::std::os::raw::c_int,
            _is_download: ::std::os::raw::c_int,
            _request_initiator: Option<&CefString>,
            _disable_default_handling: Option<&mut ::std::os::raw::c_int>,
        ) -> Option<ResourceRequestHandler> {
            // One-shot loadDataWithBaseURL interception: serve `data` for the base_url
            // navigation, then fall back to normal network handling.
            let url = request.map(|r| CefString::from(&r.url()).to_string())?;
            let mut pending = self.pending_data.lock().ok()?;
            let matches = pending
                .as_ref()
                .is_some_and(|p| urls_equivalent(&p.base_url, &url));
            if !matches {
                return None;
            }
            let taken = pending.take()?;
            Some(PendingDataResourceHandler::new(
                taken.data.into_bytes(),
                taken.mime,
            ))
        }
    }
}

/// CEF normalizes URLs (e.g. `https://host` → `https://host/`); tolerate exactly the
/// trailing-slash difference when matching the one-shot interception target (2026-07-03).
fn urls_equivalent(a: &str, b: &str) -> bool {
    a == b || a.strip_suffix('/').unwrap_or(a) == b.strip_suffix('/').unwrap_or(b)
}

wrap_resource_request_handler! {
    struct PendingDataResourceHandler {
        data: Vec<u8>,
        mime: String,
    }

    impl ResourceRequestHandler {
        fn resource_handler(
            &self,
            _browser: Option<&mut Browser>,
            _frame: Option<&mut Frame>,
            _request: Option<&mut Request>,
        ) -> Option<ResourceHandler> {
            let stream = stream_reader_create_for_handler(Some(
                &mut wrapper::byte_read_handler::ByteReadHandler::new(Arc::new(Mutex::new(
                    wrapper::byte_read_handler::ByteStream::new(self.data.clone()),
                ))),
            ))?;
            Some(
                wrapper::stream_resource_handler::StreamResourceHandler::new_with_stream(
                    self.mime.clone(),
                    stream,
                ),
            )
        }
    }
}

wrap_cookie_visitor! {
    struct ListCookieVisitor {
        acc: Arc<Mutex<CookieAcc>>,
    }

    impl CookieVisitor {
        fn visit(
            &self,
            cookie: Option<&Cookie>,
            count: ::std::os::raw::c_int,
            total: ::std::os::raw::c_int,
            _delete_cookie: Option<&mut ::std::os::raw::c_int>,
        ) -> ::std::os::raw::c_int {
            if let (Some(cookie), Ok(mut acc)) = (cookie, self.acc.lock()) {
                acc.cookies.push(CookieEntry {
                    name: cookie.name.to_string(),
                    value: cookie.value.to_string(),
                    domain: cookie.domain.to_string(),
                    path: cookie.path.to_string(),
                    secure: cookie.secure != 0,
                    http_only: cookie.httponly != 0,
                });
                if count + 1 >= total {
                    acc.finished = true;
                }
            }
            1
        }
    }
}

wrap_delete_cookies_callback! {
    struct ClearDoneCallback {
        request_id: u32,
        out: Outbox,
    }

    impl DeleteCookiesCallback {
        fn on_complete(&self, num_deleted: ::std::os::raw::c_int) {
            logging::info(
                COMPONENT,
                &format!(
                    "cookies_clear request_id={} deleted {num_deleted}",
                    self.request_id
                ),
            );
            // The solicited empty CookieList completes the request.
            self.out.send(HelperMsg::CookieList {
                request_id: self.request_id,
                cookies: Vec::new(),
            });
        }
    }
}

/// The browser-process dependencies `HelperClient::on_process_message_received` needs (bundled so
/// the macro-generated `new` stays within the arg-count lint). 2026-07-09 (plan M4).
#[derive(Clone)]
struct ClientDeps {
    out: Outbox,
    router: Arc<BrowserSideRouter>,
    state: Shared,
}

wrap_client! {
    struct HelperClient {
        load_handler: LoadHandler,
        life_span_handler: LifeSpanHandler,
        render_handler: RenderHandler,
        display_handler: DisplayHandler,
        request_handler: RequestHandler,
        deps: ClientDeps,
    }

    impl Client {
        fn load_handler(&self) -> Option<LoadHandler> {
            Some(self.load_handler.clone())
        }

        fn life_span_handler(&self) -> Option<LifeSpanHandler> {
            Some(self.life_span_handler.clone())
        }

        fn render_handler(&self) -> Option<RenderHandler> {
            Some(self.render_handler.clone())
        }

        fn display_handler(&self) -> Option<DisplayHandler> {
            Some(self.display_handler.clone())
        }

        fn request_handler(&self) -> Option<RequestHandler> {
            Some(self.request_handler.clone())
        }

        fn on_process_message_received(
            &self,
            browser: Option<&mut Browser>,
            frame: Option<&mut Frame>,
            source_process: ProcessId,
            message: Option<&mut ProcessMessage>,
        ) -> ::std::os::raw::c_int {
            // The renderer's messages (plan M4): eval results + the bridge-ready inventory pull.
            if let Some(msg) = message.as_ref() {
                let name = CefString::from(&msg.name()).to_string();
                if name == "eclipse.eval.result" {
                    if let Some(args) = msg.argument_list() {
                        let request_id = args.int(0) as u32;
                        let ok = args.bool(1) != 0;
                        let value_json = CefString::from(&args.string(2)).to_string();
                        self.deps.out.send(HelperMsg::EvaluateJsResult {
                            request_id,
                            ok,
                            value_json,
                        });
                    }
                    return 1;
                }
                if name == "eclipse.bridge.ready" {
                    // The renderer created a new main-frame context and is asking for the bridge
                    // inventory (the pull model — a pre-connection send would have been dropped).
                    if let (Some(browser), Some(frame)) = (browser.as_ref(), frame.as_ref()) {
                        let view = lock(&self.deps.state)
                            .browser_view
                            .get(&browser.identifier())
                            .copied();
                        if let Some(view) = view {
                            let bridges: Vec<(String, Vec<BridgeMethod>)> = lock(&self.deps.state)
                                .view_bridges
                                .get(&view)
                                .map(|m| m.iter().map(|(k, v)| (k.clone(), v.clone())).collect())
                                .unwrap_or_default();
                            for (name, methods) in bridges {
                                send_bridge_register_message(frame, &name, &methods);
                            }
                        }
                    }
                    return 1;
                }
            }
            // Otherwise let the browser-side router handle cefQuery replies.
            if self.deps.router.on_process_message_received(
                browser.map(|b| b.clone()),
                frame.map(|f| f.clone()),
                source_process,
                message.map(|m| m.clone()),
            ) {
                1
            } else {
                0
            }
        }
    }
}

/// Send one `"eclipse.bridge.register"` process message (name + method names) to `frame`'s renderer.
fn send_bridge_register_message(frame: &Frame, name: &str, methods: &[BridgeMethod]) {
    let Some(mut msg) = process_message_create(Some(&CefString::from("eclipse.bridge.register")))
    else {
        return;
    };
    if let Some(args) = msg.argument_list() {
        args.set_size(1 + methods.len());
        args.set_string(0, Some(&CefString::from(name)));
        for (i, m) in methods.iter().enumerate() {
            args.set_string(1 + i, Some(&CefString::from(m.name.as_str())));
        }
    }
    frame.send_process_message(ProcessId::RENDERER, Some(&mut msg));
}

// --- The browser-side bridge query handler (cefQuery → BridgeCall → BridgeResult) ----------------

/// Handles `window.cefQuery` requests: map the browser to its view, allocate a helper `call_id`,
/// retain the async router callback, and forward a `BridgeCall` to the consumer (ART reflect-
/// invokes the `@JavascriptInterface` method and answers a `BridgeResult`). The request is
/// page-controlled JSON, forwarded WITHOUT parsing and NEVER logged.
struct BridgeHandler {
    state: Shared,
    out: Outbox,
    next_call_id: AtomicU32,
}

impl BrowserSideHandler for BridgeHandler {
    fn on_query_str(
        &self,
        browser: Option<Browser>,
        _frame: Option<Frame>,
        _query_id: i64,
        request: &str,
        _persistent: bool,
        callback: Arc<Mutex<dyn BrowserSideCallback>>,
    ) -> bool {
        let view = match browser
            .as_ref()
            .and_then(|b| lock(&self.state).browser_view.get(&b.identifier()).copied())
        {
            Some(v) => v,
            None => return false, // unknown browser: not ours to handle
        };
        let mut call_id = self.next_call_id.fetch_add(1, Ordering::Relaxed);
        if call_id == 0 {
            call_id = self.next_call_id.fetch_add(1, Ordering::Relaxed);
        }
        lock(&self.state)
            .pending_bridge_calls
            .insert(call_id, callback);
        self.out.send(HelperMsg::BridgeCall {
            view,
            call_id,
            payload_json: request.to_string(),
        });
        true
    }
}

wrap_set_cookie_callback! {
    struct SetCookieResultCallback {
        request_id: u32,
        out: Outbox,
    }

    impl SetCookieCallback {
        fn on_complete(&self, success: ::std::os::raw::c_int) {
            self.out.send(HelperMsg::CookieSetResult {
                request_id: self.request_id,
                ok: success != 0,
            });
        }
    }
}

// 2026-07-10 (plan M6): the LOG-ONLY completion for the 2-arg fire-and-forget `cookie_set`.
// It WARNs on an async engine-sanitization failure (success==0) and emits NO wire message —
// the v1 2-arg `CookieSet` layout stays fire-and-forget/frozen. Fields are pre-redacted /
// lengths only (never the cookie name/value). (Field doc comments are not accepted by the
// wrap_set_cookie_callback! macro grammar — see the wrap_app! precedent in main.rs.)
wrap_set_cookie_callback! {
    struct LogOnlySetCookieCallback {
        url_redacted: String,
        domain: String,
        name_len: usize,
        value_len: usize,
    }

    impl SetCookieCallback {
        fn on_complete(&self, success: ::std::os::raw::c_int) {
            if success == 0 {
                logging::warn(
                    COMPONENT,
                    &format!(
                        "cookie_set: async completion reported failure (engine sanitization) \
                         (url={} domain={} name_len={} value_len={})",
                        self.url_redacted, self.domain, self.name_len, self.value_len
                    ),
                );
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn ozone_selection_is_explicit_and_never_auto() {
        // 2026-07-03: pins the M1-carried "never trust ozone auto" finding as a decision
        // table (the designed failure: WAYLAND_DISPLAY unset + XDG_SESSION_TYPE=wayland +
        // DISPLAY set → auto picks Wayland and cannot connect; we must pick x11).
        // Explicit override wins over everything.
        assert_eq!(
            select_ozone(Some("wayland"), None, Some(":0")).as_deref(),
            Ok("wayland")
        );
        assert_eq!(
            select_ozone(Some("x11"), Some("wayland-1"), None).as_deref(),
            Ok("x11")
        );
        // WAYLAND_DISPLAY set → wayland.
        assert_eq!(
            select_ozone(None, Some("wayland-1"), Some(":0")).as_deref(),
            Ok("wayland")
        );
        // The M1 designed-failure env shape (WAYLAND_DISPLAY unset, DISPLAY set — whatever
        // XDG_SESSION_TYPE claims, which this function deliberately never reads) → x11.
        assert_eq!(select_ozone(None, None, Some(":0")).as_deref(), Ok("x11"));
        // Empty env values count as unset.
        assert_eq!(
            select_ozone(None, Some(""), Some(":0")).as_deref(),
            Ok("x11")
        );
        assert_eq!(
            select_ozone(Some(""), Some("wayland-1"), None).as_deref(),
            Ok("wayland")
        );
        // Neither set → a typed, actionable error (never fall through to ozone auto).
        assert_eq!(select_ozone(None, None, None), Err(NoDisplayError));
        assert_eq!(select_ozone(None, Some(""), Some("")), Err(NoDisplayError));
    }

    #[test]
    fn sandbox_mode_selection_prefers_userns_then_suid_then_policy() {
        // 2026-07-10 (plan M5): the full 2×2×2 decision table — userns wins outright, SUID is
        // the fallback tier, Degraded is reachable ONLY through the explicit opt-in, and
        // neither-without-opt-in is a typed refusal naming BOTH fixes + the opt-in.
        use SandboxMode::*;
        assert_eq!(select_sandbox_mode(true, true, true), Ok(Userns));
        assert_eq!(select_sandbox_mode(true, true, false), Ok(Userns));
        assert_eq!(select_sandbox_mode(true, false, true), Ok(Userns));
        assert_eq!(select_sandbox_mode(true, false, false), Ok(Userns));
        assert_eq!(select_sandbox_mode(false, true, true), Ok(Suid));
        assert_eq!(select_sandbox_mode(false, true, false), Ok(Suid));
        assert_eq!(select_sandbox_mode(false, false, true), Ok(Degraded));
        let err = select_sandbox_mode(false, false, false).expect_err("policy refusal");
        let text = err.to_string();
        assert!(
            text.starts_with("sandbox unavailable"),
            "the Display prefix must byte-match the consumer's SANDBOX_UNAVAILABLE_MARKER: {text}"
        );
        for needle in [
            "kernel.unprivileged_userns_clone=1",
            "user.max_user_namespaces>0",
            "apparmor_restrict_unprivileged_userns",
            "chrome-sandbox beside libcef.so as root:root mode 4755",
            "webview_allow_unsandboxed=true",
        ] {
            assert!(text.contains(needle), "missing {needle:?} in {text}");
        }
    }

    #[test]
    fn apply_sandbox_mode_flips_no_sandbox_only_for_degraded() {
        // 2026-07-10 (plan M5): build_settings stays byte-identical (its no_sandbox == 0 pin
        // is engine_settings_keep_engine_logging_disabled); this is the ONE seam that may flip
        // it, and only for the helper's own policy-gated degradation.
        for (mode, expected) in [
            (SandboxMode::Userns, 0),
            (SandboxMode::Suid, 0),
            (SandboxMode::Degraded, 1),
        ] {
            let mut settings = build_settings();
            apply_sandbox_mode(&mut settings, &mode);
            assert_eq!(settings.no_sandbox, expected, "mode {mode:?}");
        }
    }

    #[test]
    fn switch_strip_keeps_the_ban_except_the_helpers_own_degradation() {
        // 2026-07-10 (plan M5): enable-logging is ALWAYS stripped; no-sandbox is stripped as a
        // pass-through UNLESS the helper itself degraded (CEF propagates Settings.no_sandbox=1
        // onto the command line — stripping that copy would desync Chromium from the settings).
        assert!(switch_should_be_stripped("enable-logging", false));
        assert!(switch_should_be_stripped("enable-logging", true));
        assert!(switch_should_be_stripped("no-sandbox", false));
        assert!(!switch_should_be_stripped("no-sandbox", true));
        // Unknown switches are never the strip loop's business.
        assert!(!switch_should_be_stripped("ozone-platform", false));
        // The documentation constant the loop iterates still names exactly the banned pair.
        for name in FORBIDDEN_PASSTHROUGH_SWITCHES {
            assert!(switch_should_be_stripped(name, false));
        }
    }

    #[test]
    fn render_path_classification_never_gates_and_names_the_devices() {
        // 2026-07-10 (plan M5): log-only classification — empty → SoftwareFallback; any DRI
        // render node OR the NVIDIA control device → GpuCandidates naming the basenames.
        assert_eq!(
            classify_render_path(&[], false),
            RenderPathVerdict::SoftwareFallback
        );
        assert_eq!(
            classify_render_path(&["renderD128".to_string()], false),
            RenderPathVerdict::GpuCandidates(vec!["renderD128".to_string()])
        );
        assert_eq!(
            classify_render_path(&[], true),
            RenderPathVerdict::GpuCandidates(vec!["nvidiactl".to_string()])
        );
        assert_eq!(
            classify_render_path(&["renderD128".to_string(), "renderD129".to_string()], true),
            RenderPathVerdict::GpuCandidates(vec![
                "renderD128".to_string(),
                "renderD129".to_string(),
                "nvidiactl".to_string(),
            ])
        );
    }

    #[test]
    fn engine_settings_keep_engine_logging_disabled() {
        // 2026-07-03: pins the M1 stderr-URL-leak finding and the never---no-sandbox rule at
        // the settings layer, plus the flag-passthrough strip list.
        let settings = build_settings();
        assert_eq!(settings.log_severity, LogSeverity::DISABLE);
        assert_eq!(settings.no_sandbox, 0, "the sandbox must stay ON");
        assert_eq!(settings.windowless_rendering_enabled, 1);
        assert_eq!(settings.external_message_pump, 1);
        assert!(
            settings.log_file.to_string().is_empty(),
            "no log_file may be configured"
        );
        // The strip list the browser process removes from any passed-through command line.
        assert!(FORBIDDEN_PASSTHROUGH_SWITCHES.contains(&"enable-logging"));
        assert!(FORBIDDEN_PASSTHROUGH_SWITCHES.contains(&"no-sandbox"));
    }

    #[test]
    fn build_settings_sets_the_eclipse_fallback_user_agent() {
        // 2026-07-09 (plan M4): the fallback UA is applied at the settings layer, is genuinely
        // Chromium 149 on Linux desktop, carries the Eclipse product token, and is NOT the recorded
        // "GDPR VIOLATION" placeholder. MUST byte-match the overlay WebSettings literal.
        assert!(ECLIPSE_USER_AGENT.contains("Chrome/149"));
        assert!(ECLIPSE_USER_AGENT.contains("Eclipse-WebView"));
        assert!(ECLIPSE_USER_AGENT.contains("X11; Linux x86_64"));
        assert!(!ECLIPSE_USER_AGENT.contains("GDPR VIOLATION"));
        // 2026-07-16: the `!ECLIPSE_USER_AGENT.contains("Android")` assertion that stood here
        // ("must not impersonate a device") is RETIRED — it ASSERTED THE BUG. It was written when
        // `WebSettings.setUserAgentString` was an empty ATL stub, so Eclipse's literal was the ONLY
        // UA any boot could present and the question looked like "which UA do we choose?". It never
        // was: the app SETS its own UA and Eclipse was DISCARDING it (§6 2026-07-16 💥). The pin
        // therefore locked in the discard — it would fail the moment the app's own (Android,
        // Hybrid()-bearing) string was honored, which is precisely the correct behaviour. The
        // constant is a FALLBACK, not a policy, so the honest thing to pin is what it IS (an
        // Eclipse-identifying desktop-Chromium literal, above) and that the app's UA WINS over it
        // when set (`effective_user_agent_prefers_the_apps_ua_and_falls_back_to_the_eclipse_literal`).
        let settings = build_settings();
        assert_eq!(settings.user_agent.to_string(), ECLIPSE_USER_AGENT);
    }

    #[test]
    fn effective_user_agent_prefers_the_apps_ua_and_falls_back_to_the_eclipse_literal() {
        // 2026-07-16 (plan M6): THE REAL CONTRACT, replacing the retired `!contains("Android")` pin.
        // (a) Nothing set anywhere ⇒ the Eclipse fallback literal, byte-for-byte.
        assert_eq!(effective_user_agent(None, None), ECLIPSE_USER_AGENT);
        assert_eq!(effective_user_agent(Some(""), Some("")), ECLIPSE_USER_AGENT);
        // (b) THE FIX: the UA the app set via WebSettings.setUserAgentString WINS over the fallback
        // and is used VERBATIM — the byte-match contract is "what CEF sends == what
        // getUserAgentString() returns", and both are now this one string. This is the exact shape
        // of the app's real UA (§6 2026-07-16 💥): it carries BOTH the `Hybrid()` and `Android`
        // substrings the page's own nativePrefix selector requires (§6 2026-07-16 🏆), so a
        // regression to the old discard-the-app's-UA behaviour fails HERE.
        let app_ua = "Mozilla/5.0 (0MB; 960x540; 160x160; 960x540; HTC unknown; unknown) \
                      AppleWebKit/537.36 (KHTML, like Gecko)  ROBLOX Android App 2.724.735 Phone \
                      Hybrid()  GooglePlayStore RobloxApp/2.724.735 (GlobalDist; GooglePlayStore)";
        assert_eq!(effective_user_agent(None, Some(app_ua)), app_ua);
        assert!(app_ua.to_lowercase().contains("hybrid"));
        assert!(app_ua.to_lowercase().contains("android"));
        // (c) The dev-host A/B still outranks the app (a measurement must be able to force any UA).
        let android_ua = "Mozilla/5.0 (Linux; Android 13) AppleWebKit/537.36 (KHTML, like Gecko) \
                          Version/4.0 Chrome/149.0.0.0 Mobile Safari/537.36";
        assert_eq!(effective_user_agent(Some(android_ua), None), android_ua);
        assert_eq!(
            effective_user_agent(Some(android_ua), Some(app_ua)),
            android_ua
        );
        // An empty diag never masks the app's UA (AOSP's "null or empty" normalization, mirrored).
        assert_eq!(effective_user_agent(Some(""), Some(app_ua)), app_ua);
        // The override reaches the settings layer, and ONLY the UA differs from the default build.
        let settings = build_settings_with_ua(android_ua);
        assert_eq!(settings.user_agent.to_string(), android_ua);
        let default = build_settings();
        assert_eq!(
            settings.windowless_rendering_enabled,
            default.windowless_rendering_enabled
        );
        assert_eq!(
            settings.external_message_pump,
            default.external_message_pump
        );
        assert_eq!(settings.no_sandbox, default.no_sandbox);
        assert_eq!(settings.log_severity, default.log_severity);
        assert!(
            settings.log_file.to_string().is_empty(),
            "the diagnostic must never relax the settings-layer redaction rule"
        );
        // The ladder composes to the fallback when neither env var is present, and to the app's UA
        // when only ECLIPSE_WEBVIEW_APP_UA is — the two compositions the binary actually takes.
        assert_eq!(
            build_settings_with_ua(effective_user_agent(None, None))
                .user_agent
                .to_string(),
            ECLIPSE_USER_AGENT
        );
        assert_eq!(
            build_settings_with_ua(effective_user_agent(None, Some(app_ua)))
                .user_agent
                .to_string(),
            app_ua
        );
    }

    #[test]
    fn session_request_context_uses_empty_cache_path() {
        // 2026-07-09 (plan M4): the session store is in-memory (empty cache_path = incognito),
        // never persisting cookies to disk — the private, session-scoped store the challenge reads.
        let settings = session_context_settings();
        assert!(
            settings.cache_path.to_string().is_empty(),
            "empty cache_path = in-memory/incognito store (NOT the global default)"
        );
        assert_eq!(settings.persist_session_cookies, 0);
    }

    #[test]
    fn urls_equivalent_tolerates_exactly_the_trailing_slash() {
        assert!(urls_equivalent("https://host", "https://host/"));
        assert!(urls_equivalent("https://host/x", "https://host/x"));
        assert!(!urls_equivalent("https://host/x", "https://host/y"));
        assert!(!urls_equivalent("https://host", "https://host/x"));
    }

    #[test]
    fn console_text_diag_gate_is_exact_match_one_only() {
        // 2026-07-10 (plan M6): the page-console-TEXT diagnostic is a deliberate opt-in — EXACTLY
        // "1", never "true"/""/None/"0"/"1 " — so an unrelated env value can never trip it.
        assert!(console_text_diag_enabled(Some("1")));
        assert!(!console_text_diag_enabled(Some("")));
        assert!(!console_text_diag_enabled(Some("0")));
        assert!(!console_text_diag_enabled(Some("true")));
        assert!(!console_text_diag_enabled(Some("1 ")));
        assert!(!console_text_diag_enabled(None));
    }

    #[test]
    fn bridge_diag_gate_is_exact_match_one_only() {
        // 2026-07-16 (plan M6): the bridge self-introspection diagnostic is a deliberate opt-in —
        // EXACTLY "1", mirroring console_text_diag_enabled — so an unrelated env value can never
        // trip it. Unset MUST be false: OFF means the renderer installs no load handler at all.
        assert!(bridge_diag_enabled(Some("1")));
        assert!(!bridge_diag_enabled(Some("")));
        assert!(!bridge_diag_enabled(Some("0")));
        assert!(!bridge_diag_enabled(Some("true")));
        assert!(!bridge_diag_enabled(Some("1 ")));
        assert!(!bridge_diag_enabled(None));
    }

    #[test]
    fn format_console_text_line_keeps_the_source_redacted_even_in_diag_mode() {
        // 2026-07-10 (plan M6): even with the diagnostic ON, the SOURCE stays scheme+host — only
        // the page console TEXT is raw (the sanctioned dev-host exposure). A token-bearing source
        // URL must reduce to scheme+host; the raw text passes through verbatim.
        let source =
            RedactedTarget::from_raw_url("https://apps.roblox.com/challenge?token=SECRETTOKEN");
        let line = format_console_text_line(42, 2, &source, 17, "page said hello");
        assert!(line.contains("source=https://apps.roblox.com"), "{line}");
        assert!(!line.contains("SECRETTOKEN"), "source token leaked: {line}");
        assert!(!line.contains("/challenge"), "source path leaked: {line}");
        assert!(line.contains("text=page said hello"), "{line}");
        assert!(line.contains("view=42") && line.contains("level=2") && line.contains("line=17"));
    }

    #[test]
    fn classify_cookie_set_rejection_names_each_documented_predicate_in_order() {
        // 2026-07-10 (plan M6): the classifier mirrors the documented CEF set_cookie sync-false
        // predicate set, checked in order. Observability only — it never sets a cookie.
        use classify_cookie_set_rejection as c;
        // (1) invalid / non-http(s) URL.
        assert_eq!(
            c("about:blank", "n", "v", "", "/", false),
            "url is not a valid http(s) URL"
        );
        assert_eq!(
            c("ftp://host", "n", "v", "", "/", false),
            "url is not a valid http(s) URL"
        );
        // (2) name charset (checked before value).
        assert_eq!(
            c("https://host", "na=me", "v", "", "/", false),
            "name contains a disallowed character"
        );
        // (3) value charset.
        assert_eq!(
            c("https://host", "n", "va;lue", "", "/", false),
            "value contains a disallowed character (';' or control)"
        );
        // (4) domain charset.
        assert_eq!(
            c("https://host", "n", "v", "ba d.com", "/", false),
            "domain contains a disallowed character"
        );
        // (5) domain does not domain-match the URL host.
        assert_eq!(
            c(
                "https://www.roblox.com",
                "n",
                "v",
                "example.com",
                "/",
                false
            ),
            "domain does not domain-match the URL host"
        );
        // (5) a matching domain (leading '.' stripped, suffix match) passes to later checks.
        assert_eq!(
            c(
                "https://www.roblox.com",
                "n",
                "v",
                ".roblox.com",
                "/x;y",
                false
            ),
            "path contains a disallowed character"
        );
        // (7) Secure cookie from a non-https origin.
        assert_eq!(
            c("http://host", "n", "v", "", "/", true),
            "Secure cookie set from a non-https URL"
        );
        // (8) nothing local matched → the CEF-internal fallback.
        assert!(
            c("https://www.roblox.com", "n", "v", ".roblox.com", "/", true)
                .starts_with("no local predicate matched")
        );
    }

    #[test]
    fn classify_cookie_set_rejection_never_embeds_the_cookie_name_or_value() {
        // 2026-07-10 (plan M6) PRIVACY pin: the classifier returns a STATIC reason and must never
        // echo a `.ROBLOSECURITY` secret value/name back into the log stream.
        for reason in [
            classify_cookie_set_rejection(
                "https://www.roblox.com",
                ".ROBLOSECURITY",
                "SECRETSESSIONTOKEN",
                ".roblox.com",
                "/",
                true,
            ),
            classify_cookie_set_rejection(
                "about:blank",
                ".ROBLOSECURITY",
                "SECRETSESSIONTOKEN",
                "",
                "/",
                false,
            ),
        ] {
            assert!(
                !reason.contains("SECRETSESSIONTOKEN"),
                "value leaked: {reason}"
            );
            assert!(!reason.contains("ROBLOSECURITY"), "name leaked: {reason}");
        }
    }

    #[test]
    fn load_state_suppresses_the_about_blank_bootstrap_but_never_driven_loads() {
        // 2026-07-03 regression guard for the drive-run1 finding: CreateView's internal
        // about:blank navigation emitted LoadState 0/3 that a consumer credited to its OWN
        // driven load. The Android internalLoadChanged contract fires only for driven loads.
        // Before any driven load: everything is bootstrap noise.
        assert!(suppress_load_state(None, "about:blank"));
        assert!(suppress_load_state(None, "https://www.roblox.com/"));
        // After a real load was driven: its own events pass; late about:blank ones do not.
        let driven = Some("https://www.roblox.com");
        assert!(!suppress_load_state(driven, "https://www.roblox.com/"));
        assert!(!suppress_load_state(
            driven,
            "https://apps.roblox.com/challenge"
        ));
        assert!(suppress_load_state(driven, "about:blank"));
        // Driving about:blank itself (loadData's hardcoded base) keeps its events live.
        assert!(!suppress_load_state(Some("about:blank"), "about:blank"));
    }
}
