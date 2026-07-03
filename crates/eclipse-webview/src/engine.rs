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
use crate::shared::proto::{Console, ConsumerMsg, CookieEntry, HelperMsg};
use crate::shared::shm;
use crate::shared::slots::SlotTracker;
use cef::{rc::Rc as _, sys, *};
use std::collections::HashMap;
use std::fs::File;
use std::os::fd::OwnedFd;
use std::os::raw::c_int;
use std::os::unix::fs::FileExt;
use std::sync::atomic::{AtomicBool, Ordering};
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

/// The engine identity string for `HelloAck` (from the pinned binding's version constant),
/// e.g. `"cef/149.0.6+g0d0eeb6+chromium-149.0.7827.201"`.
pub fn engine_id() -> String {
    let bytes = sys::CEF_VERSION;
    let text = std::str::from_utf8(&bytes[..bytes.len().saturating_sub(1)]).unwrap_or("unknown");
    format!("cef/{text}")
}

/// Pure settings policy (unit-tested): windowless OSR, external pump, sandbox ON
/// (`no_sandbox=0`, never `--no-sandbox`), engine logging DISABLED (`log_severity=DISABLE`,
/// no `log_file`) — the absolute redaction rule at the settings layer.
pub fn build_settings() -> Settings {
    Settings {
        windowless_rendering_enabled: 1,
        external_message_pump: 1,
        no_sandbox: 0,
        log_severity: LogSeverity::DISABLE,
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
}

impl Engine {
    pub fn new(out: Outbox) -> Self {
        Self {
            state: Arc::new(Mutex::new(EngineState {
                views: HashMap::new(),
                pending_cookies: Vec::new(),
                closing_all: false,
                exit_code: 0,
                close_deadline: None,
            })),
            out,
        }
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
                        // Fire-and-forget in v1 (result routing is M4's message-router work).
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

        // 2. The windowless browser, with this view's own handler set.
        let mut client = HelperClient::new(
            HelperLoadHandler::new(view, self.state.clone(), self.out.clone()),
            HelperLifeSpanHandler::new(view, self.state.clone(), self.out.clone()),
            HelperRenderHandler::new(view, self.state.clone(), self.out.clone()),
            HelperDisplayHandler::new(view, self.out.clone()),
            HelperRequestHandler::new(view, self.out.clone(), pending_data),
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
        let browser = browser_host_create_browser_sync(
            Some(&window_info),
            Some(&mut client),
            Some(&url),
            Some(&browser_settings),
            None,
            None,
        );
        let mut st = lock(&self.state);
        match browser {
            Some(b) => {
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
        let Some(manager) = cookie_manager_get_global_manager(None) else {
            logging::error(COMPONENT, "cookie_set: no global cookie manager");
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
        let url = CefString::from(url);
        if manager.set_cookie(Some(&url), Some(&cookie), None) != 1 {
            logging::warn(COMPONENT, "cookie_set: rejected by the cookie manager");
        }
    }

    fn cookie_get(&self, request_id: u32, url: &str) {
        let Some(manager) = cookie_manager_get_global_manager(None) else {
            logging::error(COMPONENT, "cookie_get: no global cookie manager");
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
        let Some(manager) = cookie_manager_get_global_manager(None) else {
            logging::error(COMPONENT, "cookies_clear: no global cookie manager");
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
    }

    impl LifeSpanHandler {
        fn on_before_close(&self, _browser: Option<&mut Browser>) {
            // The browser is gone: drop the view state (its memfd write handle with it) and
            // complete the CloseView/Shutdown contract.
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
            // Console::from_raw redacts the source and STRUCTURALLY drops the text — only
            // its length crosses the wire.
            self.out.send(HelperMsg::Console {
                view: self.view,
                console: Console::from_raw(
                    severity.clamp(0, 255) as u8,
                    &source,
                    line.max(0) as u32,
                    &message,
                ),
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
        out: Outbox,
        pending_data: Arc<Mutex<Option<PendingData>>>,
    }

    impl RequestHandler {
        fn on_render_process_terminated(
            &self,
            _browser: Option<&mut Browser>,
            _status: TerminationStatus,
            error_code: ::std::os::raw::c_int,
            _error_string: Option<&CefString>,
        ) {
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

wrap_client! {
    struct HelperClient {
        load_handler: LoadHandler,
        life_span_handler: LifeSpanHandler,
        render_handler: RenderHandler,
        display_handler: DisplayHandler,
        request_handler: RequestHandler,
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
    fn urls_equivalent_tolerates_exactly_the_trailing_slash() {
        assert!(urls_equivalent("https://host", "https://host/"));
        assert!(urls_equivalent("https://host/x", "https://host/x"));
        assert!(!urls_equivalent("https://host/x", "https://host/y"));
        assert!(!urls_equivalent("https://host", "https://host/x"));
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
