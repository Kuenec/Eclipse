use crate::app::App;
use crate::bridge;
use eclipse_webview::proto::{HelperMsg, LoadError, LoadEvent, ParentSize, ParentWindow, SizeUnit};
use gtk4 as gtk;
use gtk4::prelude::*;
use gtk4::{gdk, gio, glib};
use std::cell::{Cell, RefCell};
use std::collections::HashMap;
use std::fmt;
use std::num::NonZeroU32;
use std::rc::{Rc, Weak};
use std::time::{Duration, Instant};
use webkit6::prelude::*;

const TITLE_PREFIX: &str = "Eclipse — ";

const TITLE_FALLBACK: &str = "Roblox";

const DEFAULT_USER_AGENT: &str = "Mozilla/5.0 (X11; Linux x86_64) AppleWebKit/537.36 (KHTML, like Gecko) Chrome/152.0.0.0 Safari/537.36 Eclipse-WebView/152.0.6";

const DEFAULT_SIZE: (i32, i32) = (960, 760);

const MIN_SIZE: (i32, i32) = (480, 360);

const CLOSE_BUTTON_LAYOUT: &str = ":close";

const BACK_BUTTON: u32 = 8;

const RESOURCE_EVENT_INTERVAL: Duration = Duration::from_millis(250);

const ENGINE_SCHEMES: [&str; 7] = [
    "http",
    "https",
    "about",
    "data",
    "blob",
    "javascript",
    "file",
];

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum Route {
    Engine,
    App,
}

#[derive(Clone, PartialEq, Eq)]
struct Navigation {
    url: String,
    title: String,
    can_go_back: bool,
}

#[derive(Default)]
struct ResourceThrottle {
    last_sent: Cell<Option<Instant>>,
    pending: RefCell<Option<String>>,
}

pub(crate) struct View {
    pub(crate) window: gtk::Window,
    pub(crate) web_view: webkit6::WebView,
    pub(crate) show_order: Cell<Option<u64>>,
    content: webkit6::UserContentManager,
    bridges: RefCell<HashMap<String, webkit6::UserScript>>,
    navigation: RefCell<Option<Navigation>>,
    progress: Cell<Option<u8>>,
    resources: ResourceThrottle,
    app_load: RefCell<Option<String>>,
    failed_url: RefCell<Option<String>>,
    composing: Cell<bool>,
    escape_held: Cell<bool>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum DisplayBackend {
    Wayland,
    X11,
    Other,
}

#[derive(Debug, PartialEq, Eq)]
pub(crate) enum Parenting<'a> {
    Exported(&'a str),
    TransientFor(NonZeroU32),
    Unparented(Unparented),
}

#[derive(Debug, PartialEq, Eq)]
pub(crate) enum Unparented {
    NoGameWindow,
    BackendMismatch {
        gtk: DisplayBackend,
        game: DisplayBackend,
    },
    NoSurface,
    ImportRefused,
    NoXlib(String),
}

impl fmt::Display for Unparented {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::NoGameWindow => f.write_str("the host sent no game window handle"),
            Self::BackendMismatch { gtk, game } => write!(
                f,
                "GTK runs on {gtk:?} but the game window is a {game:?} window"
            ),
            Self::NoSurface => f.write_str("the WebView window has no toplevel surface"),
            Self::ImportRefused => {
                f.write_str("GTK could not import the game window's xdg-foreign handle")
            }
            Self::NoXlib(error) => write!(f, "Xlib is unavailable: {error}"),
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum EscapeTarget {
    Window,
    InputMethod,
    Popup,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum KeyResponse {
    ClosePage,
    Dismiss,
    Swallow,
    PassToPage,
}

pub(crate) fn window_title(page_title: Option<&str>) -> String {
    let shown = page_title
        .map(str::trim)
        .filter(|title| !title.is_empty())
        .unwrap_or(TITLE_FALLBACK);
    format!("{TITLE_PREFIX}{shown}")
}

fn effective_user_agent(requested: &str) -> &str {
    if requested.is_empty() {
        DEFAULT_USER_AGENT
    } else {
        requested
    }
}

fn engine_handles_scheme(url: &str) -> bool {
    let scheme = url.split_once(':').map_or("", |(scheme, _)| scheme);
    ENGINE_SCHEMES
        .iter()
        .any(|handled| handled.eq_ignore_ascii_case(scheme))
}

pub(crate) fn navigation_route(
    url: &str,
    method: Option<&str>,
    redirect: bool,
    app_initiated: bool,
) -> Route {
    if engine_handles_scheme(url) {
        return Route::Engine;
    }
    if method.is_some_and(|method| !method.eq_ignore_ascii_case("GET")) {
        return Route::Engine;
    }
    if app_initiated && !redirect {
        return Route::Engine;
    }
    Route::App
}

pub(crate) fn key_response(key: gdk::Key, target: EscapeTarget, escape_held: bool) -> KeyResponse {
    if key != gdk::Key::Escape {
        return KeyResponse::PassToPage;
    }
    match (target, escape_held) {
        (EscapeTarget::InputMethod | EscapeTarget::Popup, _) => KeyResponse::Dismiss,
        (EscapeTarget::Window, true) => KeyResponse::Swallow,
        (EscapeTarget::Window, false) => KeyResponse::ClosePage,
    }
}

pub(crate) fn display_backend(display: &gdk::Display) -> DisplayBackend {
    if display.is::<gdk4_wayland::WaylandDisplay>() {
        DisplayBackend::Wayland
    } else if display.is::<gdk4_x11::X11Display>() {
        DisplayBackend::X11
    } else {
        DisplayBackend::Other
    }
}

fn game_backend(parent: &ParentWindow) -> DisplayBackend {
    match parent {
        ParentWindow::Wayland { .. } => DisplayBackend::Wayland,
        ParentWindow::X11 { .. } => DisplayBackend::X11,
    }
}

pub(crate) fn parenting(backend: DisplayBackend, parent: Option<&ParentWindow>) -> Parenting<'_> {
    match (backend, parent) {
        (_, None) => Parenting::Unparented(Unparented::NoGameWindow),
        (DisplayBackend::Wayland, Some(ParentWindow::Wayland { handle })) => {
            Parenting::Exported(handle)
        }
        (DisplayBackend::X11, Some(ParentWindow::X11 { window })) => {
            Parenting::TransientFor(*window)
        }
        (gtk, Some(parent)) => Parenting::Unparented(Unparented::BackendMismatch {
            gtk,
            game: game_backend(parent),
        }),
    }
}

pub(crate) fn adopt_parent(
    window: &gtk::Window,
    parent: Option<&ParentWindow>,
) -> Result<(), Unparented> {
    let display = WidgetExt::display(window);
    let surface = window.surface().ok_or(Unparented::NoSurface)?;
    match parenting(display_backend(&display), parent) {
        Parenting::Exported(handle) => {
            let toplevel = surface
                .downcast_ref::<gdk4_wayland::WaylandToplevel>()
                .ok_or(Unparented::NoSurface)?;
            if toplevel.set_transient_for_exported(handle) {
                Ok(())
            } else {
                Err(Unparented::ImportRefused)
            }
        }
        Parenting::TransientFor(parent) => {
            let (Some(display), Some(surface)) = (
                display.downcast_ref::<gdk4_x11::X11Display>(),
                surface.downcast_ref::<gdk4_x11::X11Surface>(),
            ) else {
                return Err(Unparented::NoSurface);
            };
            set_x11_transient_for(display, surface, parent)
        }
        Parenting::Unparented(reason) => Err(reason),
    }
}

fn set_x11_transient_for(
    display: &gdk4_x11::X11Display,
    surface: &gdk4_x11::X11Surface,
    parent: NonZeroU32,
) -> Result<(), Unparented> {
    let xlib =
        gdk4_x11::x11::xlib::Xlib::open().map_err(|error| Unparented::NoXlib(error.to_string()))?;
    display.error_trap_push();
    unsafe {
        (xlib.XSetTransientForHint)(
            display.xdisplay(),
            surface.xid(),
            std::ffi::c_ulong::from(parent.get()),
        );
    }
    display.error_trap_pop_ignored();
    Ok(())
}

pub(crate) fn window_size(parent: ParentSize, surface_scale: i32) -> (i32, i32) {
    let pixels_per_unit = match parent.unit {
        SizeUnit::Logical => 1,
        SizeUnit::DevicePixels => surface_scale.max(1).unsigned_abs(),
    };
    let cover = |length: NonZeroU32, minimum: i32| {
        i32::try_from(length.get().div_ceil(pixels_per_unit))
            .unwrap_or(i32::MAX)
            .max(minimum)
    };
    (
        cover(parent.width, MIN_SIZE.0),
        cover(parent.height, MIN_SIZE.1),
    )
}

fn default_window_size() -> (i32, i32) {
    let largest = gdk::Display::default().and_then(|display| {
        display
            .monitors()
            .iter::<gdk::Monitor>()
            .filter_map(Result::ok)
            .map(|monitor| monitor.geometry())
            .max_by_key(|area| i64::from(area.width()) * i64::from(area.height()))
    });
    match largest {
        Some(area) => (
            DEFAULT_SIZE.0.min(area.width()),
            DEFAULT_SIZE.1.min(area.height()),
        ),
        None => DEFAULT_SIZE,
    }
}

fn x11_user_time(token: &str) -> Option<u32> {
    let (_, after) = token.rsplit_once("_TIME")?;
    let digits = after
        .find(|c: char| !c.is_ascii_digit())
        .unwrap_or(after.len());
    after[..digits].parse().ok().filter(|&time| time != 0)
}

fn x11_raise_id(token: &str, x11: bool) -> Option<String> {
    if !x11 {
        return None;
    }
    x11_user_time(token).map(|time| format!("_TIME{time}"))
}

pub(crate) fn load_error(error: &glib::Error) -> Option<LoadError> {
    if let Some(code) = error.kind::<webkit6::NetworkError>() {
        return match code {
            webkit6::NetworkError::Cancelled => None,
            webkit6::NetworkError::UnknownProtocol => Some(LoadError::UnsupportedScheme),
            webkit6::NetworkError::FileDoesNotExist => Some(LoadError::FileNotFound),
            webkit6::NetworkError::Transport => Some(LoadError::Io),
            _ => Some(LoadError::Unknown),
        };
    }
    if let Some(code) = error.kind::<webkit6::PolicyError>() {
        return match code {
            webkit6::PolicyError::FrameLoadInterruptedByPolicyChange => None,
            webkit6::PolicyError::CannotShowUri => Some(LoadError::UnsupportedScheme),
            _ => Some(LoadError::Unknown),
        };
    }
    if error.kind::<gio::ResolverError>().is_some() {
        return Some(LoadError::HostLookup);
    }
    if error.kind::<gio::TlsError>().is_some() {
        return Some(LoadError::FailedSslHandshake);
    }
    if let Some(code) = error.kind::<gio::IOErrorEnum>() {
        return Some(match code {
            gio::IOErrorEnum::Cancelled => return None,
            gio::IOErrorEnum::TimedOut => LoadError::Timeout,
            gio::IOErrorEnum::ConnectionRefused
            | gio::IOErrorEnum::HostUnreachable
            | gio::IOErrorEnum::NetworkUnreachable
            | gio::IOErrorEnum::BrokenPipe => LoadError::Connect,
            gio::IOErrorEnum::NotFound => LoadError::FileNotFound,
            gio::IOErrorEnum::InvalidArgument => LoadError::BadUrl,
            _ => LoadError::Io,
        });
    }
    Some(LoadError::Unknown)
}

fn load_event(event: webkit6::LoadEvent) -> Option<LoadEvent> {
    match event {
        webkit6::LoadEvent::Started => Some(LoadEvent::Started),
        webkit6::LoadEvent::Redirected => Some(LoadEvent::Redirected),
        webkit6::LoadEvent::Committed => Some(LoadEvent::Committed),
        webkit6::LoadEvent::Finished => Some(LoadEvent::Finished),
        _ => None,
    }
}

impl View {
    pub(crate) fn create(app: &Rc<App>, id: i64) -> Rc<View> {
        let content = webkit6::UserContentManager::new();
        content.register_script_message_handler_with_reply(bridge::MESSAGE_HANDLER, None);
        let web_view = webkit6::WebView::builder()
            .network_session(app.session())
            .user_content_manager(&content)
            .build();
        let title_bar = gtk::HeaderBar::builder()
            .decoration_layout(CLOSE_BUTTON_LAYOUT)
            .build();
        let window = gtk::Window::builder()
            .title(window_title(None))
            .titlebar(&title_bar)
            .modal(true)
            .child(&web_view)
            .build();
        gtk::WindowGroup::new().add_window(&window);
        WidgetExt::realize(&window);
        let view = Rc::new(View {
            window,
            web_view,
            show_order: Cell::new(None),
            content,
            bridges: RefCell::default(),
            navigation: RefCell::default(),
            progress: Cell::default(),
            resources: ResourceThrottle::default(),
            app_load: RefCell::default(),
            failed_url: RefCell::default(),
            composing: Cell::new(false),
            escape_held: Cell::new(false),
        });
        view.fit(app.parent_size());
        view.set_user_agent("");
        view.connect(Rc::downgrade(app), id);
        view
    }

    pub(crate) fn fit(&self, parent: Option<ParentSize>) {
        let (width, height) = match parent {
            Some(parent) => {
                let scale = self
                    .window
                    .surface()
                    .map_or(1, |surface| surface.scale_factor());
                window_size(parent, scale)
            }
            None => default_window_size(),
        };
        self.window.set_default_size(width, height);
    }

    fn escape_target(&self) -> EscapeTarget {
        if self.composing.get() {
            return EscapeTarget::InputMethod;
        }
        let focus_in_popup = GtkWindowExt::focus(&self.window)
            .and_then(|focus| focus.ancestor(gtk::Popover::static_type()))
            .is_some();
        if focus_in_popup {
            EscapeTarget::Popup
        } else {
            EscapeTarget::Window
        }
    }

    pub(crate) fn set_user_agent(&self, requested: &str) {
        if let Some(settings) = WebViewExt::settings(&self.web_view) {
            settings.set_user_agent(Some(effective_user_agent(requested)));
        }
    }

    pub(crate) fn activate(&self, token: &str) {
        self.window.set_startup_id(token);
        if !self.window.is_visible() {
            return;
        }
        let x11 = display_backend(&WidgetExt::display(&self.window)) == DisplayBackend::X11;
        if let Some(raise) = x11_raise_id(token, x11) {
            self.window.set_startup_id(&raise);
        }
    }

    pub(crate) fn load_url(&self, url: &str) {
        *self.app_load.borrow_mut() = (!engine_handles_scheme(url)).then(|| url.to_string());
        self.web_view.load_uri(url);
    }

    pub(crate) fn take_app_load(&self, url: &str) -> bool {
        let mut app_load = self.app_load.borrow_mut();
        let matched = app_load.as_deref() == Some(url);
        if matched {
            app_load.take();
        }
        matched
    }

    fn connect(&self, app: Weak<App>, id: i64) {
        let weak = app.clone();
        self.content.connect_script_message_with_reply_received(
            Some(bridge::MESSAGE_HANDLER),
            move |_, value, reply| {
                if let Some(app) = weak.upgrade() {
                    app.bridge_message(id, value, reply);
                }
                true
            },
        );

        let weak = app.clone();
        self.window.connect_close_request(move |_| {
            if let Some(app) = weak.upgrade() {
                app.send(HelperMsg::CloseRequested { view: id });
            }
            glib::Propagation::Stop
        });

        let keys = gtk::EventControllerKey::new();
        keys.set_propagation_phase(gtk::PropagationPhase::Capture);
        let weak = app.clone();
        keys.connect_key_pressed(move |_, key, _, _| {
            let Some(app) = weak.upgrade() else {
                return glib::Propagation::Proceed;
            };
            let Some(view) = app.view(id) else {
                return glib::Propagation::Proceed;
            };
            match key_response(key, view.escape_target(), view.escape_held.get()) {
                KeyResponse::PassToPage => glib::Propagation::Proceed,
                KeyResponse::Swallow => glib::Propagation::Stop,
                KeyResponse::Dismiss => {
                    view.escape_held.set(true);
                    glib::Propagation::Proceed
                }
                KeyResponse::ClosePage => {
                    view.escape_held.set(true);
                    app.send(HelperMsg::CloseRequested { view: id });
                    glib::Propagation::Stop
                }
            }
        });
        let weak = app.clone();
        keys.connect_key_released(move |_, key, _, _| {
            if key != gdk::Key::Escape {
                return;
            }
            if let Some(view) = weak.upgrade().and_then(|app| app.view(id)) {
                view.escape_held.set(false);
            }
        });
        self.window.add_controller(keys);
        let weak = app.clone();
        self.window.connect_hide(move |_| {
            if let Some(view) = weak.upgrade().and_then(|app| app.view(id)) {
                view.escape_held.set(false);
            }
        });
        let weak = app.clone();
        self.window
            .frame_clock()
            .expect("View::create realizes the window before connecting it")
            .connect_after_paint(move |_| {
                if let Some(app) = weak.upgrade() {
                    app.frame_painted(id);
                }
            });

        let back_button = gtk::GestureClick::builder()
            .button(BACK_BUTTON)
            .propagation_phase(gtk::PropagationPhase::Capture)
            .build();
        let weak = app.clone();
        back_button.connect_pressed(move |gesture, _, _, _| {
            gesture.set_state(gtk::EventSequenceState::Claimed);
            if let Some(app) = weak.upgrade() {
                app.send(HelperMsg::BackRequested { view: id });
            }
        });
        self.window.add_controller(back_button);

        if let Some(input_method) = self.web_view.input_method_context() {
            let weak = app.clone();
            input_method.connect_preedit_started(move |_| {
                if let Some(view) = weak.upgrade().and_then(|app| app.view(id)) {
                    view.composing.set(true);
                }
            });
            let weak = app.clone();
            input_method.connect_preedit_finished(move |_| {
                if let Some(view) = weak.upgrade().and_then(|app| app.view(id)) {
                    view.composing.set(false);
                }
            });
        }

        let weak = app.clone();
        self.web_view.connect_load_changed(move |web_view, event| {
            let (Some(app), Some(event)) = (weak.upgrade(), load_event(event)) else {
                return;
            };
            let Some(view) = app.view(id) else {
                return;
            };
            let failed_url = match event {
                LoadEvent::Started => {
                    view.failed_url.take();
                    None
                }
                LoadEvent::Finished => view.failed_url.take(),
                LoadEvent::Redirected | LoadEvent::Committed => None,
            };
            let url =
                failed_url.unwrap_or_else(|| web_view.uri().map(String::from).unwrap_or_default());
            app.send(HelperMsg::LoadChanged {
                view: id,
                event,
                url,
            });
            view.report_navigation(&app, id);
        });

        let weak = app.clone();
        self.web_view
            .connect_load_failed(move |_, _event, failing_uri, error| {
                let Some(app) = weak.upgrade() else {
                    return true;
                };
                let (Some(view), Some(code)) = (app.view(id), load_error(error)) else {
                    return true;
                };
                *view.failed_url.borrow_mut() = Some(failing_uri.to_string());
                app.send(HelperMsg::LoadFailed {
                    view: id,
                    url: failing_uri.to_string(),
                    error: code,
                    description: error.message().to_string(),
                });
                true
            });

        let weak = app.clone();
        self.web_view.connect_title_notify(move |web_view| {
            let Some(app) = weak.upgrade() else { return };
            let Some(view) = app.view(id) else { return };
            view.window
                .set_title(Some(&window_title(web_view.title().as_deref())));
            view.report_navigation(&app, id);
        });

        let weak = app.clone();
        self.web_view.connect_uri_notify(move |_| {
            let Some(app) = weak.upgrade() else { return };
            if let Some(view) = app.view(id) {
                view.report_navigation(&app, id);
            }
        });

        let weak = app.clone();
        self.web_view
            .connect_estimated_load_progress_notify(move |web_view| {
                let Some(app) = weak.upgrade() else { return };
                let Some(view) = app.view(id) else { return };
                let percent =
                    (web_view.estimated_load_progress().clamp(0.0, 1.0) * 100.0).round() as u8;
                if view.progress.replace(Some(percent)) != Some(percent) {
                    app.send(HelperMsg::Progress { view: id, percent });
                }
            });

        let weak = app.clone();
        self.web_view
            .connect_resource_load_started(move |_, resource, _| {
                let Some(app) = weak.upgrade() else { return };
                if let Some(view) = app.view(id) {
                    view.resource_started(
                        &app,
                        id,
                        resource.uri().map(String::from).unwrap_or_default(),
                    );
                }
            });

        let weak = app.clone();
        self.web_view
            .connect_decide_policy(move |_, decision, kind| match kind {
                webkit6::PolicyDecisionType::NavigationAction => weak
                    .upgrade()
                    .is_some_and(|app| app.navigation_policy(id, decision)),
                webkit6::PolicyDecisionType::NewWindowAction => {
                    decision.ignore();
                    true
                }
                _ => false,
            });

        self.web_view
            .connect_web_process_terminated(move |_, reason| {
                if let Some(app) = app.upgrade() {
                    app.web_process_gone(id, reason);
                }
            });
    }

    fn report_navigation(&self, app: &App, id: i64) {
        let current = Navigation {
            url: self.web_view.uri().map(String::from).unwrap_or_default(),
            title: self.web_view.title().map(String::from).unwrap_or_default(),
            can_go_back: self.web_view.can_go_back(),
        };
        if self.navigation.borrow().as_ref() == Some(&current) {
            return;
        }
        app.send(HelperMsg::NavigationState {
            view: id,
            url: current.url.clone(),
            title: current.title.clone(),
            can_go_back: current.can_go_back,
        });
        *self.navigation.borrow_mut() = Some(current);
    }

    fn resource_started(&self, app: &Rc<App>, id: i64, url: String) {
        let now = Instant::now();
        let since = self
            .resources
            .last_sent
            .get()
            .map(|sent| now.saturating_duration_since(sent));
        if since.is_none_or(|elapsed| elapsed >= RESOURCE_EVENT_INTERVAL) {
            self.resources.last_sent.set(Some(now));
            app.send(HelperMsg::ResourceLoad { view: id, url });
            return;
        }
        let already_scheduled = self.resources.pending.replace(Some(url)).is_some();
        if already_scheduled {
            return;
        }
        let wait = RESOURCE_EVENT_INTERVAL.saturating_sub(since.unwrap_or_default());
        let weak = Rc::downgrade(app);
        glib::timeout_add_local_once(wait, move || {
            let Some(app) = weak.upgrade() else { return };
            let Some(view) = app.view(id) else { return };
            if let Some(url) = view.resources.pending.take() {
                view.resources.last_sent.set(Some(Instant::now()));
                app.send(HelperMsg::ResourceLoad { view: id, url });
            }
        });
    }

    pub(crate) fn register_bridge(&self, name: String, methods: &[String]) {
        let script = webkit6::UserScript::new(
            &bridge::interface_script(&name, methods),
            webkit6::UserContentInjectedFrames::AllFrames,
            webkit6::UserScriptInjectionTime::Start,
            &[],
            &[],
        );
        self.content.add_script(&script);
        if let Some(replaced) = self.bridges.borrow_mut().insert(name, script) {
            self.content.remove_script(&replaced);
        }
    }

    pub(crate) fn unregister_bridge(&self, name: &str) -> bool {
        let removed = self.bridges.borrow_mut().remove(name);
        if let Some(script) = &removed {
            self.content.remove_script(script);
        }
        removed.is_some()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn window_titles_start_with_the_eclipse_prefix_the_desktop_rules_match() {
        assert_eq!(
            window_title(Some("Log in | Roblox")),
            "Eclipse — Log in | Roblox"
        );
        assert_eq!(window_title(Some("   ")), "Eclipse — Roblox");
        assert_eq!(window_title(None), "Eclipse — Roblox");
    }

    #[test]
    fn escape_closes_the_page_once_per_press_unless_a_composition_or_popup_takes_it() {
        assert_eq!(
            key_response(gdk::Key::Escape, EscapeTarget::Window, false),
            KeyResponse::ClosePage
        );
        assert_eq!(
            key_response(gdk::Key::Escape, EscapeTarget::Window, true),
            KeyResponse::Swallow,
            "a held Escape repeats; one press must close only one thing"
        );
        for target in [EscapeTarget::InputMethod, EscapeTarget::Popup] {
            for held in [false, true] {
                assert_eq!(
                    key_response(gdk::Key::Escape, target, held),
                    KeyResponse::Dismiss,
                    "Escape cancels a composition or closes a select or context menu popup \
                     before it closes the page: {target:?}, held {held}"
                );
            }
        }
        for key in [
            gdk::Key::a,
            gdk::Key::Return,
            gdk::Key::BackSpace,
            gdk::Key::F11,
        ] {
            for target in [
                EscapeTarget::Window,
                EscapeTarget::InputMethod,
                EscapeTarget::Popup,
            ] {
                assert_eq!(key_response(key, target, false), KeyResponse::PassToPage);
            }
        }
    }

    fn game_window(width: u32, height: u32, unit: SizeUnit) -> ParentSize {
        ParentSize {
            width: NonZeroU32::new(width).expect("width"),
            height: NonZeroU32::new(height).expect("height"),
            unit,
        }
    }

    #[test]
    fn web_windows_cover_the_whole_game_window_and_never_shrink_below_the_minimum() {
        assert_eq!(
            window_size(game_window(1345, 886, SizeUnit::Logical), 2),
            (1345, 886),
            "logical sizes are GTK's own units on every scale"
        );
        assert_eq!(
            window_size(game_window(3840, 2160, SizeUnit::DevicePixels), 2),
            (1920, 1080)
        );
        assert_eq!(
            window_size(game_window(1920, 1080, SizeUnit::DevicePixels), 0),
            (1920, 1080)
        );
        assert_eq!(
            window_size(game_window(1345, 887, SizeUnit::DevicePixels), 2),
            (673, 444),
            "an odd device-pixel length rounds up so no strip of the game stays visible"
        );
        assert_eq!(
            window_size(game_window(320, 200, SizeUnit::Logical), 1),
            MIN_SIZE
        );
        assert_eq!(
            window_size(game_window(u32::MAX, u32::MAX, SizeUnit::Logical), 1),
            (i32::MAX, i32::MAX)
        );
    }

    #[test]
    fn each_display_backend_parents_only_to_a_game_window_of_its_own_kind() {
        let wayland = ParentWindow::Wayland {
            handle: "game-window-export".to_string(),
        };
        let xid = NonZeroU32::new(0x0460_0003).expect("xid");
        let x11 = ParentWindow::X11 { window: xid };
        assert_eq!(
            parenting(DisplayBackend::Wayland, Some(&wayland)),
            Parenting::Exported("game-window-export")
        );
        assert_eq!(
            parenting(DisplayBackend::X11, Some(&x11)),
            Parenting::TransientFor(xid)
        );
        for (gtk, parent, game) in [
            (DisplayBackend::Wayland, &x11, DisplayBackend::X11),
            (DisplayBackend::X11, &wayland, DisplayBackend::Wayland),
            (DisplayBackend::Other, &wayland, DisplayBackend::Wayland),
        ] {
            assert_eq!(
                parenting(gtk, Some(parent)),
                Parenting::Unparented(Unparented::BackendMismatch { gtk, game })
            );
        }
        for backend in [
            DisplayBackend::Wayland,
            DisplayBackend::X11,
            DisplayBackend::Other,
        ] {
            assert_eq!(
                parenting(backend, None),
                Parenting::Unparented(Unparented::NoGameWindow)
            );
        }
    }

    #[test]
    fn an_empty_user_agent_restores_the_default_and_any_other_passes_through() {
        assert_eq!(effective_user_agent(""), DEFAULT_USER_AGENT);
        let roblox = "Mozilla/5.0 (0MB; 960x540) AppleWebKit/537.36 (KHTML, like Gecko)  \
                      ROBLOX Android App 2.724.735 Phone Hybrid()";
        assert_eq!(effective_user_agent(roblox), roblox);
        assert_eq!(effective_user_agent(" "), " ");
    }

    #[test]
    fn schemes_the_engine_loads_never_reach_the_app() {
        for url in [
            "https://www.roblox.com/login",
            "HTTP://host/",
            "about:blank",
            "data:text/html,x",
            "blob:https://host/id",
            "javascript:void(0)",
            "file:///etc/passwd",
        ] {
            assert_eq!(
                navigation_route(url, Some("GET"), false, false),
                Route::Engine,
                "{url}"
            );
            assert_eq!(
                navigation_route(url, None, true, false),
                Route::Engine,
                "{url}"
            );
        }
    }

    #[test]
    fn page_navigations_to_other_schemes_ask_the_app() {
        for url in [
            "roblox://placeId=1",
            "robloxmobile://x",
            "mailto:a@b",
            "tel:123",
            "intent://x#Intent;end",
        ] {
            assert_eq!(
                navigation_route(url, None, false, false),
                Route::App,
                "{url}"
            );
            assert_eq!(
                navigation_route(url, Some("GET"), true, false),
                Route::App,
                "{url}"
            );
        }
        assert_eq!(navigation_route("nourl", None, false, false), Route::App);
    }

    #[test]
    fn app_loads_and_non_get_requests_stay_with_the_engine_but_app_load_redirects_ask() {
        assert_eq!(
            navigation_route("roblox://x", None, false, true),
            Route::Engine
        );
        assert_eq!(navigation_route("roblox://x", None, true, true), Route::App);
        assert_eq!(
            navigation_route("roblox://x", Some("POST"), false, false),
            Route::Engine
        );
        assert_eq!(
            navigation_route("roblox://x", Some("get"), false, false),
            Route::App
        );
    }

    #[test]
    fn x11_activation_tokens_raise_the_window_with_their_event_time() {
        let winit_x11 = "eclipse-host1234_TIME5678";
        assert_eq!(x11_raise_id(winit_x11, true).as_deref(), Some("_TIME5678"));
        assert_eq!(
            x11_raise_id("a_TIME1_TIME42x", true).as_deref(),
            Some("_TIME42")
        );
        assert_eq!(
            x11_raise_id(winit_x11, false),
            None,
            "Wayland uses the token itself"
        );
        for no_time in [
            "9f2c6a1e-5b7d",
            "host_TIME",
            "host_TIME0",
            "host_TIME99999999999",
        ] {
            assert_eq!(x11_raise_id(no_time, true), None, "{no_time}");
        }
    }

    #[test]
    fn cancellations_are_not_load_errors_but_lookups_and_schemes_are() {
        let cancelled = glib::Error::new(webkit6::NetworkError::Cancelled, "cancelled");
        assert_eq!(load_error(&cancelled), None);
        let interrupted = glib::Error::new(
            webkit6::PolicyError::FrameLoadInterruptedByPolicyChange,
            "interrupted",
        );
        assert_eq!(load_error(&interrupted), None);
        let lookup = glib::Error::new(gio::ResolverError::NotFound, "no host");
        assert_eq!(load_error(&lookup), Some(LoadError::HostLookup));
        let scheme = glib::Error::new(webkit6::NetworkError::UnknownProtocol, "scheme");
        assert_eq!(load_error(&scheme), Some(LoadError::UnsupportedScheme));
        let cannot_show = glib::Error::new(webkit6::PolicyError::CannotShowUri, "cannot show");
        assert_eq!(load_error(&cannot_show), Some(LoadError::UnsupportedScheme));
        let timeout = glib::Error::new(gio::IOErrorEnum::TimedOut, "slow");
        assert_eq!(load_error(&timeout), Some(LoadError::Timeout));
    }
}
