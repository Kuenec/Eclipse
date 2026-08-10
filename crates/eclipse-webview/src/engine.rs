










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
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, AtomicU32, Ordering};
use std::sync::mpsc::{SyncSender, TrySendError};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

const COMPONENT: &str = "engine";

const SLOT_COUNT: u8 = 2;

const WINDOWLESS_FPS: c_int = 30;




const COOKIE_VISIT_DEADLINE: Duration = Duration::from_secs(5);


const CLOSE_ALL_DEADLINE: Duration = Duration::from_secs(10);





pub const FORBIDDEN_PASSTHROUGH_SWITCHES: &[&str] = &["enable-logging", "no-sandbox"];










pub fn console_text_diag_enabled(v: Option<&str>) -> bool {
    v == Some("1")
}













pub fn bridge_diag_enabled(v: Option<&str>) -> bool {
    v == Some("1")
}





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



pub fn engine_id() -> String {
    let bytes = sys::CEF_VERSION;
    let text = std::str::from_utf8(&bytes[..bytes.len().saturating_sub(1)]).unwrap_or("unknown");
    format!("cef/{text}")
}




















pub const ECLIPSE_USER_AGENT: &str = "Mozilla/5.0 (X11; Linux x86_64) AppleWebKit/537.36 (KHTML, like Gecko) Chrome/149.0.0.0 Safari/537.36 Eclipse-WebView/149.0.6";















pub fn effective_user_agent<'a>(diag: Option<&'a str>, app: Option<&'a str>) -> &'a str {
    match (diag, app) {
        (Some(ua), _) if !ua.is_empty() => ua,
        (_, Some(ua)) if !ua.is_empty() => ua,
        _ => ECLIPSE_USER_AGENT,
    }
}














#[cfg_attr(not(test), allow(dead_code))]
pub fn build_settings() -> Settings {
    build_settings_with_ua(ECLIPSE_USER_AGENT)
}






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


#[derive(Debug, Clone, PartialEq, Eq)]
pub struct PersistentProfilePaths {
    pub root: String,
    pub profile: String,
}





pub fn persistent_profile_paths(root: &Path) -> Result<PersistentProfilePaths, &'static str> {
    if !root.is_absolute() {
        return Err("ECLIPSE_WEBVIEW_DATA_DIR must be an absolute path");
    }
    let root = root
        .to_str()
        .ok_or("ECLIPSE_WEBVIEW_DATA_DIR must be valid UTF-8 for CEF")?;
    let profile = PathBuf::from(root).join("profile");
    Ok(PersistentProfilePaths {
        root: root.to_string(),
        profile: profile
            .to_str()
            .ok_or("derived CEF profile path is not valid UTF-8")?
            .to_string(),
    })
}






pub fn apply_persistent_profile(settings: &mut Settings, paths: &PersistentProfilePaths) {
    settings.root_cache_path = CefString::from(paths.root.as_str());
    settings.cache_path = CefString::from(paths.profile.as_str());
    settings.persist_session_cookies = 1;
}


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












#[derive(Debug, Clone, PartialEq, Eq)]
pub enum SandboxMode {



    Userns,


    Suid,

    Degraded,
}



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





pub fn apply_sandbox_mode(settings: &mut Settings, mode: &SandboxMode) {
    if matches!(mode, SandboxMode::Degraded) {
        settings.no_sandbox = 1;
    }
}








pub fn switch_should_be_stripped(name: &str, degraded: bool) -> bool {
    match name {
        "enable-logging" => true,
        "no-sandbox" => !degraded,
        _ => false,
    }
}













pub fn classify_cookie_set_rejection(
    url: &str,
    name: &str,
    value: &str,
    domain: &str,
    path: &str,
    secure: bool,
) -> &'static str {
    let is_ctrl = |c: char| (c as u32) < 0x20;

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

    if name.chars().any(|c| c == ';' || c == '=' || is_ctrl(c)) {
        return "name contains a disallowed character";
    }

    if value.chars().any(|c| c == ';' || is_ctrl(c)) {
        return "value contains a disallowed character (';' or control)";
    }

    if domain.chars().any(|c| c == ';' || c == ' ' || is_ctrl(c)) {
        return "domain contains a disallowed character";
    }

    if !domain.is_empty() {
        let d = domain.strip_prefix('.').unwrap_or(domain);
        let matches = host_no_port == d || host_no_port.ends_with(&format!(".{d}"));
        if !matches {
            return "domain does not domain-match the URL host";
        }
    }

    if path.chars().any(|c| c == ';' || is_ctrl(c)) {
        return "path contains a disallowed character";
    }

    if secure && scheme != "https" {
        return "Secure cookie set from a non-https URL";
    }

    "no local predicate matched — CEF-internal (cookie store unready at first-op, or engine-side sanitization)"
}












#[derive(Debug, Clone, PartialEq, Eq)]
pub enum RenderPathVerdict {

    GpuCandidates(Vec<String>),

    SoftwareFallback,
}



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







pub enum Out {
    Msg(HelperMsg),
    MsgWithFd(HelperMsg, OwnedFd),
}




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

    frame_file: Option<File>,
    slot_bytes: u32,

    pending_data: Arc<Mutex<Option<PendingData>>>,





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

#[derive(Default)]
struct SessionCookieClearAcc {
    deleted: usize,
    finished: bool,
}

struct PendingSessionCookieClear {
    request_id: u32,
    acc: Arc<Mutex<SessionCookieClearAcc>>,
    deadline: Instant,
}

struct EngineState {
    views: HashMap<i64, ViewState>,
    pending_cookies: Vec<PendingCookieGet>,
    pending_session_cookie_clears: Vec<PendingSessionCookieClear>,
    closing_all: bool,
    exit_code: i32,
    close_deadline: Option<Instant>,



    request_context: Option<RequestContext>,


    browser_view: HashMap<i32, i64>,


    pending_bridge_calls: HashMap<u32, Arc<Mutex<dyn BrowserSideCallback>>>,




    view_bridges: HashMap<i64, HashMap<String, Vec<BridgeMethod>>>,
}

type Shared = Arc<Mutex<EngineState>>;

fn lock(state: &Shared) -> std::sync::MutexGuard<'_, EngineState> {


    match state.lock() {
        Ok(g) => g,
        Err(poisoned) => poisoned.into_inner(),
    }
}


pub struct Engine {
    state: Shared,
    out: Outbox,


    router: Arc<BrowserSideRouter>,


    console_text: bool,
}

impl Engine {
    pub fn new(out: Outbox, console_text: bool) -> Self {
        let state: Shared = Arc::new(Mutex::new(EngineState {
            views: HashMap::new(),
            pending_cookies: Vec::new(),
            pending_session_cookie_clears: Vec::new(),
            closing_all: false,
            exit_code: 0,
            close_deadline: None,
            request_context: None,
            browser_view: HashMap::new(),
            pending_bridge_calls: HashMap::new(),
            view_bridges: HashMap::new(),
        }));

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




    fn request_context(&self) -> Option<RequestContext> {
        let mut st = lock(&self.state);
        if st.request_context.is_none() {
            st.request_context = request_context_get_global_context();
        }
        st.request_context.clone()
    }



    fn persistent_cookie_manager(&self) -> Option<CookieManager> {
        self.request_context()
            .and_then(|rc| rc.cookie_manager(None))
    }

    pub fn outbox_dead(&self) -> bool {
        self.out.is_dead()
    }


    pub fn handle(&self, msg: ConsumerMsg) {
        match msg {
            ConsumerMsg::Hello { .. } => {

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
                    _ => MouseButtonType::RIGHT,
                };






                if down {
                    host.set_focus(1);
                }
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


                host.set_focus(1);
                let event = KeyEvent {
                    type_: match kind {
                        0 => KeyEventType::RAWKEYDOWN,
                        1 => KeyEventType::KEYUP,
                        _ => KeyEventType::CHAR,
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
            ConsumerMsg::CookiesClear { request_id } => self.cookies_clear_all(request_id),
            ConsumerMsg::FrameAck {
                view,
                generation,
                seq,
            } => {
                let mut st = lock(&self.state);
                if let Some(v) = st.views.get_mut(&view) {

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
            ConsumerMsg::CookieFlush { request_id } => self.cookie_flush(request_id),
            ConsumerMsg::CookiesClearSession { request_id } => {
                self.cookies_clear_session(request_id)
            }
        }
    }



    fn bridge_register(&self, view: i64, name: &str, methods: &[BridgeMethod]) {
        {
            let mut st = lock(&self.state);
            st.view_bridges
                .entry(view)
                .or_default()
                .insert(name.to_string(), methods.to_vec());
        }

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


    fn bridge_result(&self, call_id: u32, ok: bool, result_json: &str) {
        let cb = lock(&self.state).pending_bridge_calls.remove(&call_id);
        let Some(cb) = cb else {
            logging::warn(
                COMPONENT,
                &format!("bridge_result call_id={call_id}: no pending call (dropped)"),
            );
            return;
        };

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



    #[allow(clippy::too_many_arguments)]
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
        let Some(manager) = self.persistent_cookie_manager() else {
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



    pub fn begin_shutdown(&self, exit_code: i32) {
        let mut st = lock(&self.state);
        if st.closing_all {

            if exit_code != 0 && st.exit_code == 0 {
                st.exit_code = exit_code;
            }
            return;
        }
        st.closing_all = true;
        st.exit_code = exit_code;
        st.close_deadline = Some(Instant::now() + CLOSE_ALL_DEADLINE);

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


    pub fn poll(&self) {
        let mut due: Vec<(u32, Vec<CookieEntry>, bool)> = Vec::new();
        let mut clear_due: Vec<(u32, bool, bool)> = Vec::new();
        {
            let mut st = lock(&self.state);
            let now = Instant::now();
            st.pending_cookies.retain(|p| {
                let acc = match p.acc.lock() {
                    Ok(g) => g,
                    Err(poisoned) => poisoned.into_inner(),
                };
                if acc.finished || now > p.deadline {
                    due.push((p.request_id, acc.cookies.clone(), acc.finished));
                    false
                } else {
                    true
                }
            });
            st.pending_session_cookie_clears.retain(|p| {
                let acc = match p.acc.lock() {
                    Ok(g) => g,
                    Err(poisoned) => poisoned.into_inner(),
                };
                if acc.finished || now > p.deadline {
                    clear_due.push((p.request_id, acc.deleted != 0, acc.finished));
                    false
                } else {
                    true
                }
            });
        }
        for (request_id, cookies, finished) in due {
            if !finished {

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
        for (request_id, removed, finished) in clear_due {
            if !finished {

                logging::warn(
                    COMPONENT,
                    &format!(
                        "session-cookie clear request_id={request_id} completed by deadline; \
                         removed={removed}"
                    ),
                );
            }
            self.out.send(HelperMsg::CookiesClearDone {
                request_id,
                removed,
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
            Some(host) => host.close_browser(1),
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

    #[allow(clippy::too_many_arguments)]
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
        let Some(manager) = self.persistent_cookie_manager() else {
            logging::error(COMPONENT, "cookie_set: no persistent cookie manager");
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
        let Some(manager) = self.persistent_cookie_manager() else {
            logging::error(COMPONENT, "cookie_get: no persistent cookie manager");
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

    fn cookies_clear_all(&self, request_id: u32) {
        let Some(manager) = self.persistent_cookie_manager() else {
            logging::error(COMPONENT, "cookies_clear_all: no persistent cookie manager");
            self.out.send(HelperMsg::CookiesClearDone {
                request_id,
                removed: false,
            });
            return;
        };
        let mut callback = ClearDoneCallback::new(request_id, self.out.clone());
        if manager.delete_cookies(None, None, Some(&mut callback)) != 1 {
            self.out.send(HelperMsg::CookiesClearDone {
                request_id,
                removed: false,
            });
        }
    }





    fn cookies_clear_session(&self, request_id: u32) {
        let Some(manager) = self.persistent_cookie_manager() else {
            logging::error(
                COMPONENT,
                "cookies_clear_session: no persistent cookie manager",
            );
            self.out.send(HelperMsg::CookiesClearDone {
                request_id,
                removed: false,
            });
            return;
        };
        let acc: Arc<Mutex<SessionCookieClearAcc>> = Arc::default();
        let mut visitor = SessionCookieClearVisitor::new(acc.clone());
        if manager.visit_all_cookies(Some(&mut visitor)) != 1 {
            self.out.send(HelperMsg::CookiesClearDone {
                request_id,
                removed: false,
            });
            return;
        }
        lock(&self.state)
            .pending_session_cookie_clears
            .push(PendingSessionCookieClear {
                request_id,
                acc,
                deadline: Instant::now() + COOKIE_VISIT_DEADLINE,
            });
    }




    fn cookie_flush(&self, request_id: u32) {
        let Some(manager) = self.persistent_cookie_manager() else {
            logging::error(COMPONENT, "cookie_flush: no persistent cookie manager");
            self.out.send(HelperMsg::CookieFlushDone {
                request_id,
                ok: false,
            });
            return;
        };
        let mut callback = FlushDoneCallback::new(request_id, self.out.clone());
        if manager.flush_store(Some(&mut callback)) != 1 {
            logging::error(
                COMPONENT,
                "cookie_flush: CEF refused to schedule the store flush",
            );
            self.out.send(HelperMsg::CookieFlushDone {
                request_id,
                ok: false,
            });
        }
    }
}










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


            if width != c_int::from(v.width) || height != c_int::from(v.height) {
                return;
            }
            let (slot, publish) = v.tracker.on_paint();
            let offset = u64::from(v.slot_bytes) * u64::from(slot);
            let len = v.slot_bytes as usize;
            let Some(file) = v.frame_file.as_ref() else {
                return;
            };



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


            self.out.send(HelperMsg::Console {
                view: self.view,
                console: Console::from_raw(severity_u8, &source, line_u32, &message),
            });


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



const fn session_cookie_should_delete(has_expires: c_int) -> bool {
    has_expires == 0
}

wrap_cookie_visitor! {
    struct SessionCookieClearVisitor {
        acc: Arc<Mutex<SessionCookieClearAcc>>,
    }

    impl CookieVisitor {
        fn visit(
            &self,
            cookie: Option<&Cookie>,
            count: ::std::os::raw::c_int,
            total: ::std::os::raw::c_int,
            delete_cookie: Option<&mut ::std::os::raw::c_int>,
        ) -> ::std::os::raw::c_int {
            let mut acc = match self.acc.lock() {
                Ok(g) => g,
                Err(poisoned) => poisoned.into_inner(),
            };
            if let (Some(cookie), Some(delete_cookie)) = (cookie, delete_cookie) {
                if session_cookie_should_delete(cookie.has_expires) {
                    *delete_cookie = 1;
                    acc.deleted = acc.deleted.saturating_add(1);
                }
            }
            if count.saturating_add(1) >= total {
                acc.finished = true;
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
            self.out.send(HelperMsg::CookiesClearDone {
                request_id: self.request_id,
                removed: num_deleted > 0,
            });
        }
    }
}

wrap_completion_callback! {
    struct FlushDoneCallback {
        request_id: u32,
        out: Outbox,
    }

    impl CompletionCallback {
        fn on_complete(&self) {
            self.out.send(HelperMsg::CookieFlushDone {
                request_id: self.request_id,
                ok: true,
            });
        }
    }
}



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
            None => return false,
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




        assert_eq!(
            select_ozone(Some("wayland"), None, Some(":0")).as_deref(),
            Ok("wayland")
        );
        assert_eq!(
            select_ozone(Some("x11"), Some("wayland-1"), None).as_deref(),
            Ok("x11")
        );

        assert_eq!(
            select_ozone(None, Some("wayland-1"), Some(":0")).as_deref(),
            Ok("wayland")
        );


        assert_eq!(select_ozone(None, None, Some(":0")).as_deref(), Ok("x11"));

        assert_eq!(
            select_ozone(None, Some(""), Some(":0")).as_deref(),
            Ok("x11")
        );
        assert_eq!(
            select_ozone(Some(""), Some("wayland-1"), None).as_deref(),
            Ok("wayland")
        );

        assert_eq!(select_ozone(None, None, None), Err(NoDisplayError));
        assert_eq!(select_ozone(None, Some(""), Some("")), Err(NoDisplayError));
    }

    #[test]
    fn sandbox_mode_selection_prefers_userns_then_suid_then_policy() {



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



        assert!(switch_should_be_stripped("enable-logging", false));
        assert!(switch_should_be_stripped("enable-logging", true));
        assert!(switch_should_be_stripped("no-sandbox", false));
        assert!(!switch_should_be_stripped("no-sandbox", true));

        assert!(!switch_should_be_stripped("ozone-platform", false));

        for name in FORBIDDEN_PASSTHROUGH_SWITCHES {
            assert!(switch_should_be_stripped(name, false));
        }
    }

    #[test]
    fn render_path_classification_never_gates_and_names_the_devices() {


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


        let settings = build_settings();
        assert_eq!(settings.log_severity, LogSeverity::DISABLE);
        assert_eq!(settings.no_sandbox, 0, "the sandbox must stay ON");
        assert_eq!(settings.windowless_rendering_enabled, 1);
        assert_eq!(settings.external_message_pump, 1);
        assert!(
            settings.log_file.to_string().is_empty(),
            "no log_file may be configured"
        );

        assert!(FORBIDDEN_PASSTHROUGH_SWITCHES.contains(&"enable-logging"));
        assert!(FORBIDDEN_PASSTHROUGH_SWITCHES.contains(&"no-sandbox"));
    }

    #[test]
    fn build_settings_sets_the_eclipse_fallback_user_agent() {



        assert!(ECLIPSE_USER_AGENT.contains("Chrome/149"));
        assert!(ECLIPSE_USER_AGENT.contains("Eclipse-WebView"));
        assert!(ECLIPSE_USER_AGENT.contains("X11; Linux x86_64"));
        assert!(!ECLIPSE_USER_AGENT.contains("GDPR VIOLATION"));










        let settings = build_settings();
        assert_eq!(settings.user_agent.to_string(), ECLIPSE_USER_AGENT);
    }

    #[test]
    fn effective_user_agent_prefers_the_apps_ua_and_falls_back_to_the_eclipse_literal() {


        assert_eq!(effective_user_agent(None, None), ECLIPSE_USER_AGENT);
        assert_eq!(effective_user_agent(Some(""), Some("")), ECLIPSE_USER_AGENT);






        let app_ua = "Mozilla/5.0 (0MB; 960x540; 160x160; 960x540; HTC unknown; unknown) \
                      AppleWebKit/537.36 (KHTML, like Gecko)  ROBLOX Android App 2.724.735 Phone \
                      Hybrid()  GooglePlayStore RobloxApp/2.724.735 (GlobalDist; GooglePlayStore)";
        assert_eq!(effective_user_agent(None, Some(app_ua)), app_ua);
        assert!(app_ua.to_lowercase().contains("hybrid"));
        assert!(app_ua.to_lowercase().contains("android"));

        let android_ua = "Mozilla/5.0 (Linux; Android 13) AppleWebKit/537.36 (KHTML, like Gecko) \
                          Version/4.0 Chrome/149.0.0.0 Mobile Safari/537.36";
        assert_eq!(effective_user_agent(Some(android_ua), None), android_ua);
        assert_eq!(
            effective_user_agent(Some(android_ua), Some(app_ua)),
            android_ua
        );

        assert_eq!(effective_user_agent(Some(""), Some(app_ua)), app_ua);

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
    fn persistent_profile_path_is_an_absolute_private_child_and_restores_session_cookies() {

        let paths = persistent_profile_paths(Path::new("/tmp/eclipse-test-profile"))
            .expect("absolute profile root");
        assert_eq!(paths.root, "/tmp/eclipse-test-profile");
        assert_eq!(paths.profile, "/tmp/eclipse-test-profile/profile");
        assert!(persistent_profile_paths(Path::new("relative/profile")).is_err());

        let mut global = build_settings_with_ua(ECLIPSE_USER_AGENT);
        apply_persistent_profile(&mut global, &paths);
        assert_eq!(global.root_cache_path.to_string(), paths.root);
        assert_eq!(global.cache_path.to_string(), paths.profile);
        assert_eq!(global.persist_session_cookies, 1);
    }

    #[test]
    fn remove_session_cookies_deletes_only_cookies_without_an_expiry() {


        assert!(session_cookie_should_delete(0));
        assert!(!session_cookie_should_delete(1));
        assert!(!session_cookie_should_delete(-1));
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


        assert!(console_text_diag_enabled(Some("1")));
        assert!(!console_text_diag_enabled(Some("")));
        assert!(!console_text_diag_enabled(Some("0")));
        assert!(!console_text_diag_enabled(Some("true")));
        assert!(!console_text_diag_enabled(Some("1 ")));
        assert!(!console_text_diag_enabled(None));
    }

    #[test]
    fn bridge_diag_gate_is_exact_match_one_only() {



        assert!(bridge_diag_enabled(Some("1")));
        assert!(!bridge_diag_enabled(Some("")));
        assert!(!bridge_diag_enabled(Some("0")));
        assert!(!bridge_diag_enabled(Some("true")));
        assert!(!bridge_diag_enabled(Some("1 ")));
        assert!(!bridge_diag_enabled(None));
    }

    #[test]
    fn format_console_text_line_keeps_the_source_redacted_even_in_diag_mode() {



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


        use classify_cookie_set_rejection as c;

        assert_eq!(
            c("about:blank", "n", "v", "", "/", false),
            "url is not a valid http(s) URL"
        );
        assert_eq!(
            c("ftp://host", "n", "v", "", "/", false),
            "url is not a valid http(s) URL"
        );

        assert_eq!(
            c("https://host", "na=me", "v", "", "/", false),
            "name contains a disallowed character"
        );

        assert_eq!(
            c("https://host", "n", "va;lue", "", "/", false),
            "value contains a disallowed character (';' or control)"
        );

        assert_eq!(
            c("https://host", "n", "v", "ba d.com", "/", false),
            "domain contains a disallowed character"
        );

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

        assert_eq!(
            c("http://host", "n", "v", "", "/", true),
            "Secure cookie set from a non-https URL"
        );

        assert!(
            c("https://www.roblox.com", "n", "v", ".roblox.com", "/", true)
                .starts_with("no local predicate matched")
        );
    }

    #[test]
    fn classify_cookie_set_rejection_never_embeds_the_cookie_name_or_value() {


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




        assert!(suppress_load_state(None, "about:blank"));
        assert!(suppress_load_state(None, "https://www.roblox.com/"));

        let driven = Some("https://www.roblox.com");
        assert!(!suppress_load_state(driven, "https://www.roblox.com/"));
        assert!(!suppress_load_state(
            driven,
            "https://apps.roblox.com/challenge"
        ));
        assert!(suppress_load_state(driven, "about:blank"));

        assert!(!suppress_load_state(Some("about:blank"), "about:blank"));
    }
}
