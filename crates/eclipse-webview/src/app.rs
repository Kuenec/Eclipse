use crate::cookies;
use crate::logging::{self, Redacted};
use crate::view::{self, Frame, Route, Unparented, View};
use crate::wire::{Inbound, Wire};
use eclipse_webview::proto::{
    ClearScope, ConsumerMsg, CookiePair, HelperMsg, ParentSize, ParentWindow, ProtoError,
    StoredCookie,
};
use gtk4 as gtk;
use gtk4::prelude::*;
use gtk4::{gio, glib};
use std::cell::{Cell, RefCell};
use std::collections::HashMap;
use std::path::PathBuf;
use std::rc::Rc;
use std::time::Duration;
use webkit6::javascriptcore;
use webkit6::prelude::*;
use webkit6::soup;

const SNAPSHOT_DELAY: Duration = Duration::from_millis(500);

const MAX_PENDING: usize = 256;

const FETCH_DESTINATION_HEADER: &str = "Sec-Fetch-Dest";

pub(crate) const EXIT_REQUESTED: u8 = 0;

pub(crate) const EXIT_CONSUMER_LOST: u8 = 2;

pub(crate) struct Storage {
    pub(crate) data: PathBuf,
    pub(crate) cache: PathBuf,
}

pub(crate) struct Engine {
    session: webkit6::NetworkSession,
}

fn utf8(path: &std::path::Path) -> Result<&str, String> {
    path.to_str()
        .ok_or_else(|| format!("{} is not UTF-8", path.display()))
}

pub(crate) fn configure_web_context() -> Result<(), String> {
    webkit6::WebContext::default()
        .ok_or("WebKit has no default web context")?
        .set_cache_model(webkit6::CacheModel::DocumentBrowser);
    Ok(())
}

impl Engine {
    pub(crate) fn open(storage: &Storage) -> Result<Engine, String> {
        let (data, cache) = (utf8(&storage.data)?, utf8(&storage.cache)?);
        let session = webkit6::NetworkSession::new(Some(data), Some(cache));
        session
            .cookie_manager()
            .ok_or("the WebKit network session has no cookie manager")?;
        session.connect_download_started(|_, download| {
            let url = download
                .request()
                .and_then(|request| request.uri())
                .unwrap_or_default();
            logging::info(format_args!(
                "dropping a download from {}: Roblox sets no DownloadListener",
                Redacted(&url)
            ));
            download.cancel();
        });
        Ok(Engine { session })
    }

    fn cookie_manager(&self) -> webkit6::CookieManager {
        self.session
            .cookie_manager()
            .expect("Engine::open checked that the session has a cookie manager")
    }
}

struct PendingReply {
    view: i64,
    reply: webkit6::ScriptMessageReply,
    context: javascriptcore::Context,
}

struct PendingPolicy {
    view: i64,
    decision: webkit6::PolicyDecision,
}

pub(crate) struct App {
    wire: Wire,
    engine: Engine,
    views: RefCell<HashMap<i64, Rc<View>>>,
    replies: RefCell<HashMap<u32, PendingReply>>,
    policies: RefCell<HashMap<u32, PendingPolicy>>,
    next_id: Cell<u32>,
    snapshot_scheduled: Cell<bool>,
    exit: Cell<Option<u8>>,
    main_loop: glib::MainLoop,
    parent: RefCell<Option<ParentWindow>>,
    parent_size: Cell<Option<ParentSize>>,
    unparented_logged: Cell<bool>,
    show_count: Cell<u64>,
    awaiting_first_frame: Cell<Option<i64>>,
}

fn failure_reply(msg: &HelperMsg) -> Option<HelperMsg> {
    match msg {
        HelperMsg::EvaluateJsResult { request_id, .. } => Some(HelperMsg::EvaluateJsResult {
            request_id: *request_id,
            ok: false,
            value_json: "null".to_string(),
        }),
        HelperMsg::CookieList { request_id, .. } => Some(HelperMsg::CookieList {
            request_id: *request_id,
            cookies: Vec::new(),
        }),
        _ => None,
    }
}

fn cookie_pairs(cookies: Vec<soup::Cookie>) -> Vec<CookiePair> {
    cookies
        .into_iter()
        .map(|mut cookie| CookiePair {
            name: cookie.name().map(String::from).unwrap_or_default(),
            value: cookie.value().map(String::from).unwrap_or_default(),
        })
        .collect()
}

impl App {
    pub(crate) fn new(wire: Wire, engine: Engine, main_loop: glib::MainLoop) -> Rc<App> {
        let app = Rc::new(App {
            wire,
            engine,
            views: RefCell::default(),
            replies: RefCell::default(),
            policies: RefCell::default(),
            next_id: Cell::new(1),
            snapshot_scheduled: Cell::new(false),
            exit: Cell::new(None),
            main_loop,
            parent: RefCell::default(),
            parent_size: Cell::new(None),
            unparented_logged: Cell::new(false),
            show_count: Cell::new(0),
            awaiting_first_frame: Cell::new(None),
        });
        let weak = Rc::downgrade(&app);
        app.engine.cookie_manager().connect_changed(move |_| {
            if let Some(app) = weak.upgrade() {
                app.schedule_snapshot();
            }
        });
        app
    }

    pub(crate) fn listen(self: &Rc<Self>) {
        let weak = Rc::downgrade(self);
        let source = gio::prelude::SocketExtManual::create_source(
            self.wire.socket(),
            glib::IOCondition::IN | glib::IOCondition::HUP | glib::IOCondition::ERR,
            None::<&gio::Cancellable>,
            Some("eclipse-webview control socket"),
            glib::Priority::DEFAULT,
            move |_, _| match weak.upgrade() {
                Some(app) => app.drain_socket(),
                None => glib::ControlFlow::Break,
            },
        );
        source.attach(None);
    }

    pub(crate) fn exit_code(&self) -> u8 {
        self.exit.get().unwrap_or(EXIT_CONSUMER_LOST)
    }

    pub(crate) fn session(&self) -> &webkit6::NetworkSession {
        &self.engine.session
    }

    pub(crate) fn view(&self, id: i64) -> Option<Rc<View>> {
        self.views.borrow().get(&id).cloned()
    }

    fn all_views(&self) -> Vec<Rc<View>> {
        self.views.borrow().values().cloned().collect()
    }

    pub(crate) fn parent_size(&self) -> Option<ParentSize> {
        self.parent_size.get()
    }

    fn adopt_parent(&self, window: &gtk::Window) {
        let Err(reason) = view::adopt_parent(window, self.parent.borrow().as_ref()) else {
            return;
        };
        if self.unparented_logged.replace(true) {
            return;
        }
        let log: fn(std::fmt::Arguments<'_>) = match reason {
            Unparented::NoGameWindow => logging::info,
            Unparented::BackendMismatch { .. }
            | Unparented::NoSurface
            | Unparented::ImportRefused
            | Unparented::NoXlib(_) => logging::warn,
        };
        log(format_args!(
            "WebView windows open as normal windows, not as dialogs of the game window: {reason}"
        ));
    }

    fn set_parent(&self, parent: ParentWindow) {
        *self.parent.borrow_mut() = Some(parent);
        for view in self.all_views() {
            self.adopt_parent(&view.window);
        }
    }

    fn set_visible(&self, id: i64, visible: bool) {
        self.with_view(id, |view| {
            if !visible {
                view.show_order.set(None);
                view.window.set_visible(false);
            } else if view.show_order.get().is_none() {
                let order = self.show_count.get();
                self.show_count.set(order + 1);
                view.show_order.set(Some(order));
            }
        });
        self.present_next();
    }

    fn present_next(&self) {
        let next = {
            let views = self.views.borrow();
            let awaiting = self
                .awaiting_first_frame
                .get()
                .and_then(|id| views.get(&id))
                .is_some_and(|view| view.window.is_visible());
            if awaiting {
                return;
            }
            views
                .iter()
                .filter(|(_, view)| view.show_order.get().is_some() && !view.window.is_visible())
                .min_by_key(|(_, view)| view.show_order.get())
                .map(|(id, view)| (*id, Rc::clone(view)))
        };
        self.awaiting_first_frame
            .set(next.as_ref().map(|(id, _)| *id));
        if let Some((_, view)) = next {
            self.adopt_parent(&view.window);
            view.window.present();
        }
    }

    pub(crate) fn frame_painted(&self, id: i64) {
        if self.awaiting_first_frame.get() == Some(id) {
            self.awaiting_first_frame.set(None);
            self.present_next();
        }
    }

    fn parent_resized(&self, size: ParentSize) {
        self.parent_size.set(Some(size));
        for view in self.all_views() {
            view.window.set_visible(false);
            view.fit(Some(size));
        }
        self.present_next();
    }

    fn next_id(&self) -> u32 {
        let id = self.next_id.get();
        self.next_id.set(id.checked_add(1).unwrap_or(1));
        id
    }

    pub(crate) fn send(&self, msg: HelperMsg) {
        match msg.encode() {
            Ok(frame) => self.transmit(&frame, msg.name()),
            Err(error) => self.drop_unencodable(&msg, &error),
        }
    }

    fn drop_unencodable(&self, msg: &HelperMsg, error: &ProtoError) {
        logging::error(format_args!("dropping {}: {error}", msg.name()));
        let Some(failure) = failure_reply(msg) else {
            return;
        };
        match failure.encode() {
            Ok(frame) => self.transmit(&frame, failure.name()),
            Err(error) => logging::error(format_args!("dropping {}: {error}", failure.name())),
        }
    }

    fn transmit(&self, frame: &[u8], name: &str) {
        if self.exit.get().is_some() {
            return;
        }
        if let Err(error) = self.wire.send(frame) {
            logging::error(format_args!("cannot send {name}: {error}"));
            self.stop(EXIT_CONSUMER_LOST);
        }
    }

    fn drain_socket(self: &Rc<Self>) -> glib::ControlFlow {
        loop {
            match self.wire.receive() {
                Ok(Inbound::Messages(batch)) => {
                    let more = batch.len() == crate::wire::BATCH_LIMIT;
                    for msg in batch {
                        self.handle(msg);
                    }
                    if !more || self.exit.get().is_some() {
                        break;
                    }
                }
                Ok(Inbound::Closed) => {
                    logging::info(format_args!("the host closed the control socket"));
                    self.stop(EXIT_CONSUMER_LOST);
                    return glib::ControlFlow::Break;
                }
                Err(error) => {
                    logging::error(format_args!("{error}"));
                    self.stop(EXIT_CONSUMER_LOST);
                    return glib::ControlFlow::Break;
                }
            }
        }
        if self.exit.get().is_some() {
            glib::ControlFlow::Break
        } else {
            glib::ControlFlow::Continue
        }
    }

    fn stop(&self, code: u8) {
        if self.exit.get().is_some() {
            return;
        }
        self.exit.set(Some(code));
        let views: Vec<Rc<View>> = self
            .views
            .borrow_mut()
            .drain()
            .map(|(_, view)| view)
            .collect();
        for view in views {
            view.window.destroy();
        }
        self.main_loop.quit();
    }

    fn handle(self: &Rc<Self>, msg: ConsumerMsg) {
        match msg {
            ConsumerMsg::Hello { .. } => {
                logging::error(format_args!("protocol violation: a second Hello"));
                self.stop(EXIT_CONSUMER_LOST);
            }
            ConsumerMsg::CreateView { view } => self.create_view(view),
            ConsumerMsg::CloseView { view } => self.close_view(view),
            ConsumerMsg::SetVisible { view, visible } => self.set_visible(view, visible),
            ConsumerMsg::Activate { view, token } => {
                self.with_view(view, |view| view.activate(&token))
            }
            ConsumerMsg::SetUserAgent { view, user_agent } => {
                self.with_view(view, |view| view.set_user_agent(&user_agent))
            }
            ConsumerMsg::LoadUrl { view, url } => self.with_view(view, |view| view.load_url(&url)),
            ConsumerMsg::LoadData {
                view,
                base_url,
                data,
                mime,
                encoding,
            } => self.with_view(view, |view| {
                let mime = if mime.is_empty() { "text/html" } else { &mime };
                view.web_view.load_bytes(
                    &glib::Bytes::from_owned(data.into_bytes()),
                    Some(mime),
                    Some(encoding.as_str()).filter(|encoding| !encoding.is_empty()),
                    Some(base_url.as_str()).filter(|base_url| !base_url.is_empty()),
                );
            }),
            ConsumerMsg::Reload { view } => self.with_view(view, |view| view.web_view.reload()),
            ConsumerMsg::StopLoading { view } => {
                self.with_view(view, |view| view.web_view.stop_loading())
            }
            ConsumerMsg::GoBack { view } => self.with_view(view, |view| {
                if view.web_view.can_go_back() {
                    view.web_view.go_back();
                }
            }),
            ConsumerMsg::EvaluateJs {
                view,
                request_id,
                script,
            } => self.evaluate(view, request_id, &script),
            ConsumerMsg::BridgeRegister {
                view,
                name,
                methods,
            } => self.with_view(view, |view| view.register_bridge(name, &methods)),
            ConsumerMsg::BridgeUnregister { view, name } => self.with_view(view, |view| {
                if !view.unregister_bridge(&name) {
                    logging::warn(format_args!("no bridge named {name:?} to remove"));
                }
            }),
            ConsumerMsg::BridgeResult {
                call_id,
                ok,
                result_json,
            } => self.bridge_result(call_id, ok, &result_json),
            ConsumerMsg::PolicyReply {
                policy_id,
                override_load,
            } => match self.policies.borrow_mut().remove(&policy_id) {
                Some(pending) if override_load => pending.decision.ignore(),
                Some(pending) => pending.decision.use_(),
                None => logging::warn(format_args!("reply for unknown policy {policy_id}")),
            },
            ConsumerMsg::CookieSet {
                request_id,
                url,
                header,
            } => self.set_cookie(request_id, &url, &header),
            ConsumerMsg::CookieImport {
                request_id,
                cookies,
            } => self.import_cookies(request_id, cookies),
            ConsumerMsg::CookieGet { request_id, url } => self.get_cookies(request_id, &url),
            ConsumerMsg::CookiesClear { request_id, scope } => {
                self.clear_cookies(request_id, scope)
            }
            ConsumerMsg::CookieFlush { request_id } => {
                let app = Rc::clone(self);
                self.snapshot(move |ok| app.send(HelperMsg::CookieFlushed { request_id, ok }));
            }
            ConsumerMsg::Shutdown => {
                logging::info(format_args!("shutdown requested by the host"));
                self.stop(EXIT_REQUESTED);
            }
            ConsumerMsg::SetParent { parent } => self.set_parent(parent),
            ConsumerMsg::ParentResized { size } => self.parent_resized(size),
        }
    }

    fn with_view(&self, id: i64, action: impl FnOnce(&View)) {
        match self.view(id) {
            Some(view) => action(&view),
            None => logging::warn(format_args!("no view {id}")),
        }
    }

    fn create_view(self: &Rc<Self>, id: i64) {
        if self.views.borrow().contains_key(&id) {
            logging::warn(format_args!("view {id} already exists"));
            return;
        }
        let view = View::create(self, id);
        self.views.borrow_mut().insert(id, view);
    }

    fn close_view(&self, id: i64) {
        let removed = self.views.borrow_mut().remove(&id);
        let Some(view) = removed else {
            logging::warn(format_args!("no view {id} to close"));
            self.send(HelperMsg::ViewClosed { view: id });
            return;
        };
        let replies: Vec<PendingReply> = self
            .replies
            .borrow_mut()
            .extract_if(|_, pending| pending.view == id)
            .map(|(_, pending)| pending)
            .collect();
        for pending in replies {
            pending.reply.return_error_message("the WebView was closed");
        }
        let policies: Vec<PendingPolicy> = self
            .policies
            .borrow_mut()
            .extract_if(|_, pending| pending.view == id)
            .map(|(_, pending)| pending)
            .collect();
        for pending in policies {
            pending.decision.ignore();
        }
        view.window.destroy();
        self.send(HelperMsg::ViewClosed { view: id });
        self.present_next();
    }

    pub(crate) fn bridge_message(
        &self,
        id: i64,
        value: &javascriptcore::Value,
        reply: &webkit6::ScriptMessageReply,
    ) {
        let (Some(context), true) = (value.context(), value.is_string()) else {
            reply.return_error_message("Eclipse bridge calls carry a JSON string");
            return;
        };
        if self.replies.borrow().len() >= MAX_PENDING {
            logging::warn(format_args!(
                "{MAX_PENDING} bridge calls are unanswered; rejecting one from view {id}"
            ));
            reply.return_error_message("too many unanswered bridge calls");
            return;
        }
        let call_id = self.next_id();
        let call = HelperMsg::BridgeCall {
            view: id,
            call_id,
            payload_json: value.to_str().to_string(),
        };
        let frame = match call.encode() {
            Ok(frame) => frame,
            Err(error) => {
                logging::warn(format_args!(
                    "rejecting a bridge call from view {id}: {error}"
                ));
                reply.return_error_message("the bridge call exceeds Eclipse's message size limit");
                return;
            }
        };
        self.replies.borrow_mut().insert(
            call_id,
            PendingReply {
                view: id,
                reply: reply.clone(),
                context,
            },
        );
        self.transmit(&frame, call.name());
    }

    fn bridge_result(&self, call_id: u32, ok: bool, result_json: &str) {
        let Some(pending) = self.replies.borrow_mut().remove(&call_id) else {
            logging::warn(format_args!("result for unknown bridge call {call_id}"));
            return;
        };
        if !ok {
            pending.reply.return_error_message(result_json);
        } else if result_json.is_empty() {
            pending
                .reply
                .return_value(&javascriptcore::Value::new_undefined(&pending.context));
        } else {
            pending
                .reply
                .return_value(&javascriptcore::Value::from_json(
                    &pending.context,
                    result_json,
                ));
        }
    }

    pub(crate) fn navigation_policy(&self, id: i64, decision: &webkit6::PolicyDecision) -> bool {
        let Some(view) = self.view(id) else {
            return false;
        };
        let Some((request, redirect, user_gesture, kind)) = decision
            .downcast_ref::<webkit6::NavigationPolicyDecision>()
            .and_then(webkit6::NavigationPolicyDecision::navigation_action)
            .and_then(|action| {
                let request = action.request()?;
                Some((
                    request,
                    action.is_redirect(),
                    action.is_user_gesture(),
                    action.navigation_type(),
                ))
            })
        else {
            return false;
        };
        let url = request.uri().map(String::from).unwrap_or_default();
        let method = request.http_method();
        let app_initiated = !redirect && view.take_app_load(&url);
        let (navigated_frame, user_gesture) = if redirect {
            let destination = request
                .http_headers()
                .and_then(|headers| headers.one(FETCH_DESTINATION_HEADER));
            (view::redirect_frame(destination.as_deref()), user_gesture)
        } else {
            match view.take_intent(&url, kind) {
                Some(intent) => (Frame::Main, user_gesture || intent.activation),
                None => (Frame::Unknown, user_gesture),
            }
        };
        let route = view::navigation_route(
            &url,
            method.as_deref(),
            redirect,
            app_initiated,
            navigated_frame,
        );
        if route == Route::Engine {
            return false;
        }
        if self.policies.borrow().len() >= MAX_PENDING {
            logging::warn(format_args!(
                "{MAX_PENDING} navigations await the app; letting WebKit handle {} in view {id}",
                Redacted(&url)
            ));
            return false;
        }
        let policy_id = self.next_id();
        let request = HelperMsg::PolicyRequest {
            view: id,
            policy_id,
            url,
            redirect,
            user_gesture,
            method: method.map_or_else(|| "GET".to_string(), String::from),
        };
        let frame = match request.encode() {
            Ok(frame) => frame,
            Err(error) => {
                logging::warn(format_args!(
                    "letting WebKit handle a navigation in view {id} that cannot reach the app: \
                     {error}"
                ));
                return false;
            }
        };
        self.policies.borrow_mut().insert(
            policy_id,
            PendingPolicy {
                view: id,
                decision: decision.clone(),
            },
        );
        self.transmit(&frame, request.name());
        true
    }

    pub(crate) fn web_process_gone(&self, id: i64, reason: webkit6::WebProcessTerminationReason) {
        logging::error(format_args!(
            "the web process of view {id} ended: {reason:?}"
        ));
        self.send(HelperMsg::WebProcessGone { view: id });
    }

    fn evaluate(self: &Rc<Self>, id: i64, request_id: u32, script: &str) {
        let Some(view) = self.view(id) else {
            logging::warn(format_args!("evaluateJavascript for missing view {id}"));
            self.send(HelperMsg::EvaluateJsResult {
                request_id,
                ok: false,
                value_json: "null".to_string(),
            });
            return;
        };
        let app = Rc::clone(self);
        view.web_view.evaluate_javascript(
            script,
            None,
            None,
            None::<&gio::Cancellable>,
            move |result| {
                let (ok, value_json) = match result {
                    Ok(value) => (
                        true,
                        value
                            .to_json(0)
                            .map(String::from)
                            .unwrap_or_else(|| "null".to_string()),
                    ),
                    Err(error) => {
                        logging::warn(format_args!("evaluateJavascript failed: {error}"));
                        (false, "null".to_string())
                    }
                };
                app.send(HelperMsg::EvaluateJsResult {
                    request_id,
                    ok,
                    value_json,
                });
            },
        );
    }

    fn set_cookie(self: &Rc<Self>, request_id: u32, url: &str, header: &str) {
        let parsed = glib::Uri::parse(url, glib::UriFlags::NONE)
            .ok()
            .and_then(|origin| soup::Cookie::parse(header, Some(&origin)));
        let Some(cookie) = parsed else {
            logging::warn(format_args!(
                "rejected an unparsable cookie for {}",
                Redacted(url)
            ));
            self.send(HelperMsg::CookieSetResult {
                request_id,
                ok: false,
            });
            return;
        };
        let app = Rc::clone(self);
        self.engine.cookie_manager().add_cookie(
            &cookie,
            None::<&gio::Cancellable>,
            move |result| {
                if let Err(error) = &result {
                    logging::warn(format_args!("setCookie failed: {error}"));
                }
                app.send(HelperMsg::CookieSetResult {
                    request_id,
                    ok: result.is_ok(),
                });
            },
        );
    }

    fn import_cookies(self: &Rc<Self>, request_id: u32, cookies: Vec<StoredCookie>) {
        let tally = Rc::new(ImportTally::new(request_id, cookies.len()));
        if cookies.is_empty() {
            tally.finish(self);
            return;
        }
        for stored in cookies {
            match cookies::to_soup(&stored) {
                Ok(cookie) => {
                    let app = Rc::clone(self);
                    let tally = Rc::clone(&tally);
                    self.engine.cookie_manager().add_cookie(
                        &cookie,
                        None::<&gio::Cancellable>,
                        move |result| {
                            tally.record(&app, result.is_ok());
                        },
                    );
                }
                Err(error) => {
                    logging::warn(format_args!("cannot import a cookie: {error}"));
                    tally.record(self, false);
                }
            }
        }
    }

    fn get_cookies(self: &Rc<Self>, request_id: u32, url: &str) {
        let app = Rc::clone(self);
        self.engine
            .cookie_manager()
            .cookies(url, None::<&gio::Cancellable>, move |result| {
                let cookies = match result {
                    Ok(cookies) => cookie_pairs(cookies),
                    Err(error) => {
                        logging::warn(format_args!("getCookie failed: {error}"));
                        Vec::new()
                    }
                };
                app.send(HelperMsg::CookieList {
                    request_id,
                    cookies,
                });
            });
    }

    fn clear_cookies(self: &Rc<Self>, request_id: u32, scope: ClearScope) {
        let app = Rc::clone(self);
        self.engine
            .cookie_manager()
            .all_cookies(None::<&gio::Cancellable>, move |result| {
                let doomed: Vec<soup::Cookie> = match result {
                    Ok(cookies) => cookies
                        .into_iter()
                        .filter_map(|mut cookie| {
                            (scope == ClearScope::All || cookie.expires().is_none())
                                .then_some(cookie)
                        })
                        .collect(),
                    Err(error) => {
                        logging::warn(format_args!("cannot list cookies to clear: {error}"));
                        Vec::new()
                    }
                };
                let removed = !doomed.is_empty();
                let remaining = Rc::new(Cell::new(doomed.len()));
                if doomed.is_empty() {
                    app.send(HelperMsg::CookiesCleared {
                        request_id,
                        removed,
                    });
                }
                for cookie in &doomed {
                    let app = Rc::clone(&app);
                    let remaining = Rc::clone(&remaining);
                    app.engine.cookie_manager().delete_cookie(
                        cookie,
                        None::<&gio::Cancellable>,
                        move |result| {
                            if let Err(error) = result {
                                logging::warn(format_args!("cannot delete a cookie: {error}"));
                            }
                            remaining.set(remaining.get() - 1);
                            if remaining.get() == 0 {
                                app.send(HelperMsg::CookiesCleared {
                                    request_id,
                                    removed,
                                });
                            }
                        },
                    );
                }
            });
    }

    fn schedule_snapshot(self: &Rc<Self>) {
        if self.snapshot_scheduled.replace(true) {
            return;
        }
        let weak = Rc::downgrade(self);
        glib::timeout_add_local_once(SNAPSHOT_DELAY, move || {
            if let Some(app) = weak.upgrade() {
                app.snapshot_scheduled.set(false);
                app.snapshot(|_| {});
            }
        });
    }

    fn snapshot(self: &Rc<Self>, done: impl FnOnce(bool) + 'static) {
        let app = Rc::clone(self);
        self.engine
            .cookie_manager()
            .all_cookies(None::<&gio::Cancellable>, move |result| {
                let cookies = match result {
                    Ok(cookies) => cookies,
                    Err(error) => {
                        logging::error(format_args!("cannot list cookies for the host: {error}"));
                        done(false);
                        return;
                    }
                };
                let snapshot = HelperMsg::CookieSnapshot {
                    cookies: cookies
                        .into_iter()
                        .map(|mut cookie| cookies::from_soup(&mut cookie))
                        .collect(),
                };
                match snapshot.encode() {
                    Ok(frame) => {
                        app.transmit(&frame, snapshot.name());
                        done(true);
                    }
                    Err(error) => {
                        logging::error(format_args!(
                            "cannot send the cookies to the host: {error}"
                        ));
                        done(false);
                    }
                }
            });
    }
}

struct ImportTally {
    request_id: u32,
    remaining: Cell<usize>,
    imported: Cell<u32>,
    failed: Cell<u32>,
}

impl ImportTally {
    fn new(request_id: u32, total: usize) -> Self {
        Self {
            request_id,
            remaining: Cell::new(total),
            imported: Cell::new(0),
            failed: Cell::new(0),
        }
    }

    fn record(&self, app: &App, ok: bool) {
        let counter = if ok { &self.imported } else { &self.failed };
        counter.set(counter.get().saturating_add(1));
        self.remaining.set(self.remaining.get().saturating_sub(1));
        if self.remaining.get() == 0 {
            self.finish(app);
        }
    }

    fn finish(&self, app: &App) {
        app.send(HelperMsg::CookieImportResult {
            request_id: self.request_id,
            imported: self.imported.get(),
            failed: self.failed.get(),
        });
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::intent;
    use crate::wire::Wire;
    use eclipse_webview::proto::{self, LoadEvent, GLOBAL_FRAME_CAP};
    use std::io::{BufRead, BufReader, Write};
    use std::net::TcpListener;
    use std::os::fd::OwnedFd;
    use std::os::unix::net::UnixStream;
    use std::sync::mpsc;
    use std::time::Instant;

    fn storage(tag: &str) -> (PathBuf, Storage) {
        let root =
            std::env::temp_dir().join(format!("eclipse-webview-{tag}-{}", std::process::id()));
        let storage = Storage {
            data: root.join("data"),
            cache: root.join("cache"),
        };
        std::fs::create_dir_all(&storage.data).expect("data dir");
        std::fs::create_dir_all(&storage.cache).expect("cache dir");
        (root, storage)
    }

    fn app_with_host(tag: &str) -> (PathBuf, Rc<App>, UnixStream) {
        let (root, storage) = storage(tag);
        let (host, helper) = UnixStream::pair().expect("socketpair");
        host.set_read_timeout(Some(Duration::from_secs(5)))
            .expect("read timeout");
        let wire = Wire::new(OwnedFd::from(helper)).expect("wire");
        let engine = Engine::open(&storage).expect("engine");
        let app = App::new(wire, engine, glib::MainLoop::new(None, false));
        (root, app, host)
    }

    fn oversized(prefix: &str) -> String {
        format!("{prefix}{}", "x".repeat(GLOBAL_FRAME_CAP as usize))
    }

    fn an_engine_tears_down_without_touching_a_finalized_session() {
        let (root, storage) = storage("engine");
        drop(Engine::open(&storage).expect("engine"));
        std::fs::remove_dir_all(&root).expect("cleanup");
    }

    fn a_page_message_too_large_to_send_is_dropped_without_ending_the_helper() {
        let (root, app, host) = app_with_host("oversized-event");
        app.send(HelperMsg::BridgeCall {
            view: 1,
            call_id: 1,
            payload_json: oversized("\""),
        });
        app.send(HelperMsg::LoadChanged {
            view: 1,
            event: LoadEvent::Committed,
            url: oversized("https://www.roblox.com/?"),
        });
        assert_eq!(
            app.exit.get(),
            None,
            "a page can make these messages too large, and that must not end the helper"
        );
        app.send(HelperMsg::Progress {
            view: 1,
            percent: 40,
        });
        assert_eq!(
            proto::read_helper_msg(&mut &host),
            Ok(HelperMsg::Progress {
                view: 1,
                percent: 40
            }),
            "the oversized messages were dropped and the next one arrives"
        );
        drop(app);
        std::fs::remove_dir_all(&root).expect("cleanup");
    }

    fn a_reply_too_large_to_send_reaches_the_host_as_a_failure() {
        let (root, app, host) = app_with_host("oversized-reply");
        app.send(HelperMsg::EvaluateJsResult {
            request_id: 7,
            ok: true,
            value_json: oversized("\""),
        });
        assert_eq!(
            proto::read_helper_msg(&mut &host),
            Ok(HelperMsg::EvaluateJsResult {
                request_id: 7,
                ok: false,
                value_json: "null".to_string(),
            })
        );
        app.send(HelperMsg::CookieList {
            request_id: 8,
            cookies: vec![CookiePair {
                name: "big".to_string(),
                value: oversized(""),
            }],
        });
        assert_eq!(
            proto::read_helper_msg(&mut &host),
            Ok(HelperMsg::CookieList {
                request_id: 8,
                cookies: Vec::new(),
            })
        );
        assert_eq!(app.exit.get(), None);
        drop(app);
        std::fs::remove_dir_all(&root).expect("cleanup");
    }

    fn pump_until(what: &str, mut done: impl FnMut() -> bool) {
        let context = glib::MainContext::default();
        let deadline = Instant::now() + Duration::from_secs(10);
        while !done() {
            assert!(
                Instant::now() < deadline,
                "WebKit did not {what} within 10 s"
            );
            if !context.iteration(false) {
                std::thread::sleep(Duration::from_millis(5));
            }
        }
    }

    fn files_under(dir: &std::path::Path) -> Vec<PathBuf> {
        let mut files = Vec::new();
        for entry in std::fs::read_dir(dir).expect("list the storage") {
            let path = entry.expect("a storage entry").path();
            if path.is_dir() {
                files.extend(files_under(&path));
            } else {
                files.push(path);
            }
        }
        files
    }

    fn webkit_keeps_cookies_in_memory_and_hands_them_to_the_host() {
        let (root, app, host) = app_with_host("memory-cookies");
        let cookie = soup::Cookie::new("ECLIPSE_MEMORY", "1", "www.roblox.com", "/", 3600);
        let added = Rc::new(Cell::new(None));
        let noted = Rc::clone(&added);
        app.engine
            .cookie_manager()
            .add_cookie(&cookie, None::<&gio::Cancellable>, move |result| {
                noted.set(Some(result.is_ok()))
            });
        pump_until("store a cookie", || added.get().is_some());
        let sent = Rc::new(Cell::new(None));
        let noted = Rc::clone(&sent);
        app.snapshot(move |ok| noted.set(Some(ok)));
        pump_until("list its cookies", || sent.get().is_some());
        let snapshot = proto::read_helper_msg(&mut &host);
        drop(app);
        let files = files_under(&root);
        std::fs::remove_dir_all(&root).expect("cleanup");

        assert_eq!(added.get(), Some(true));
        assert_eq!(sent.get(), Some(true));
        match snapshot {
            Ok(HelperMsg::CookieSnapshot { cookies }) => assert!(
                cookies.iter().any(|cookie| cookie.name == "ECLIPSE_MEMORY"),
                "{cookies:?}"
            ),
            other => panic!("expected the cookie snapshot, got {other:?}"),
        }
        assert!(
            files.iter().all(|file| !file.ends_with("cookies.sqlite")),
            "WebKit must keep cookies in memory, the host's jar is their only file: {files:?}"
        );
    }

    const SITE_PAGE: &str = "<!doctype html><title>page</title>\
                             <iframe id=frame src=/frame-1></iframe>\
                             <a id=jump href=#here>jump</a><a id=link href=/linked>link</a>\
                             <form id=search action=/search><input name=q value=a></form>";

    fn site_response(target: &str) -> String {
        let path = target.split(['?', '#']).next().unwrap_or(target);
        let (status, location, body) = match path {
            "/main" | "/buy" | "/linked" | "/search" | "/landed" => ("200 OK", "", SITE_PAGE),
            "/frame-1" | "/frame-2" | "/frame-3" | "/frame-4" => {
                ("200 OK", "", "<!doctype html>frame")
            }
            "/hop" => ("302 Found", "Location: /landed\r\n", ""),
            "/frame-hop" => ("302 Found", "Location: /frame-4\r\n", ""),
            _ => ("404 Not Found", "", ""),
        };
        format!(
            "HTTP/1.1 {status}\r\n{location}Content-Type: text/html; charset=utf-8\r\n\
             Cache-Control: no-store\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{body}",
            body.len()
        )
    }

    struct Site {
        origin: String,
        requests: mpsc::Receiver<String>,
        served: RefCell<Vec<String>>,
    }

    impl Site {
        fn start() -> Site {
            let listener = TcpListener::bind("127.0.0.1:0").expect("bind the test site");
            let port = listener.local_addr().expect("the test site address").port();
            let (sender, requests) = mpsc::channel();
            std::thread::spawn(move || {
                for stream in listener.incoming() {
                    let Ok(stream) = stream else { return };
                    let mut reader = BufReader::new(&stream);
                    let mut request_line = String::new();
                    if reader.read_line(&mut request_line).is_err() {
                        continue;
                    }
                    let mut header = String::new();
                    while reader.read_line(&mut header).is_ok_and(|read| read > 2) {
                        header.clear();
                    }
                    let target = request_line.split(' ').nth(1).unwrap_or("/").to_string();
                    if (&stream)
                        .write_all(site_response(&target).as_bytes())
                        .is_err()
                    {
                        continue;
                    }
                    if sender.send(target).is_err() {
                        return;
                    }
                }
            });
            Site {
                origin: format!("http://127.0.0.1:{port}"),
                requests,
                served: RefCell::default(),
            }
        }

        fn url(&self, target: &str) -> String {
            format!("{}{target}", self.origin)
        }

        fn served(&self, target: &str) -> usize {
            self.served.borrow_mut().extend(self.requests.try_iter());
            self.served
                .borrow()
                .iter()
                .filter(|served| *served == target)
                .count()
        }
    }

    struct Host {
        received: mpsc::Receiver<HelperMsg>,
        recent: RefCell<Vec<HelperMsg>>,
        policies: RefCell<Vec<(String, bool, String)>>,
    }

    impl Host {
        fn listen(stream: UnixStream) -> Host {
            stream.set_read_timeout(None).expect("block on host reads");
            let (sender, received) = mpsc::channel();
            std::thread::spawn(move || {
                while let Ok(msg) = proto::read_helper_msg(&mut &stream) {
                    if sender.send(msg).is_err() {
                        return;
                    }
                }
            });
            Host {
                received,
                recent: RefCell::default(),
                policies: RefCell::default(),
            }
        }

        fn drain(&self) {
            for msg in self.received.try_iter() {
                if let HelperMsg::PolicyRequest {
                    url,
                    redirect,
                    method,
                    ..
                } = &msg
                {
                    self.policies
                        .borrow_mut()
                        .push((url.clone(), *redirect, method.clone()));
                }
                self.recent.borrow_mut().push(msg);
            }
        }

        fn take_policy(&self, url: &str) -> Option<(u32, bool)> {
            self.drain();
            let mut recent = self.recent.borrow_mut();
            let index = recent.iter().position(
                |msg| matches!(msg, HelperMsg::PolicyRequest { url: asked, .. } if asked == url),
            )?;
            match recent.remove(index) {
                HelperMsg::PolicyRequest {
                    policy_id,
                    redirect,
                    ..
                } => Some((policy_id, redirect)),
                _ => None,
            }
        }

        fn take_finished(&self, url: &str) -> bool {
            self.drain();
            let mut recent = self.recent.borrow_mut();
            let load_event = |event: LoadEvent| {
                move |msg: &HelperMsg| {
                    matches!(
                        msg,
                        HelperMsg::LoadChanged { event: seen, url: loaded, .. }
                            if *seen == event && loaded == url
                    )
                }
            };
            let Some(committed) = recent.iter().position(load_event(LoadEvent::Committed)) else {
                return false;
            };
            let Some(finished) = recent[committed..]
                .iter()
                .position(load_event(LoadEvent::Finished))
            else {
                return false;
            };
            recent.drain(..=committed + finished);
            true
        }

        fn take_result(&self, request_id: u32) -> Option<String> {
            self.drain();
            let mut recent = self.recent.borrow_mut();
            let index = recent.iter().position(|msg| {
                matches!(msg, HelperMsg::EvaluateJsResult { request_id: id, .. } if *id == request_id)
            })?;
            match recent.remove(index) {
                HelperMsg::EvaluateJsResult { value_json, .. } => Some(value_json),
                _ => None,
            }
        }
    }

    fn main_frame_navigations_reach_the_app_and_frame_navigations_do_not() {
        if gtk::init().is_err() {
            eprintln!("SKIP: no display for a WebKit view (WAYLAND_DISPLAY or DISPLAY)");
            return;
        }
        let site = Site::start();
        let (root, app, host) = app_with_host("navigation");
        let host = Host::listen(host);
        let view = 1;
        let evaluations = Cell::new(0);
        let evaluate = |script: &str| {
            let request_id = evaluations.get() + 1;
            evaluations.set(request_id);
            app.handle(ConsumerMsg::EvaluateJs {
                view,
                request_id,
                script: script.to_string(),
            });
            let mut result = None;
            pump_until("run a page script", || {
                result = host.take_result(request_id);
                result.is_some()
            });
            result.expect("pump_until waited for the result")
        };
        let load = |target: &str| {
            app.handle(ConsumerMsg::LoadUrl {
                view,
                url: site.url(target),
            });
        };
        let finish = |target: &str| {
            pump_until("finish loading the page", || {
                host.take_finished(&site.url(target))
            });
        };
        let answer = |target: &str, redirect: bool, override_load: bool| {
            let mut asked = None;
            pump_until("ask the app about a main-frame navigation", || {
                asked = host.take_policy(&site.url(target));
                asked.is_some()
            });
            let (policy_id, asked_redirect) = asked.expect("pump_until waited for the request");
            assert_eq!(asked_redirect, redirect, "{target}");
            app.handle(ConsumerMsg::PolicyReply {
                policy_id,
                override_load,
            });
        };
        let served = |target: &str| {
            pump_until("fetch a frame document", || site.served(target) > 0);
        };

        app.handle(ConsumerMsg::CreateView { view });
        load("/main");
        finish("/main");
        served("/frame-1");
        evaluate("document.getElementById('frame').src='/frame-2'");
        served("/frame-2");
        evaluate("document.getElementById('frame').contentWindow.location.href='/frame-3'");
        served("/frame-3");
        evaluate("document.getElementById('frame').src='/frame-hop'");
        served("/frame-4");
        evaluate(
            "var l=document.getElementById('link');\
             l.addEventListener('click',function(e){\
             e.preventDefault();document.getElementById('frame').src=l.href;});l.click()",
        );
        served("/linked");
        evaluate(
            "var f=document.getElementById('search');\
             f.addEventListener('submit',function(e){\
             e.preventDefault();document.getElementById('frame').src='/search?q=frame';});\
             f.requestSubmit()",
        );
        served("/search?q=frame");
        evaluate("document.getElementById('jump').click()");
        assert_eq!(evaluate("location.hash"), "\"#here\"");

        evaluate("location.href='/buy?id=com.roblox.robloxmobile.premium80robux'");
        answer(
            "/buy?id=com.roblox.robloxmobile.premium80robux",
            false,
            true,
        );
        load("/buy?id=com.roblox.robloxmobile.premium80robux");
        finish("/buy?id=com.roblox.robloxmobile.premium80robux");
        assert_eq!(
            site.served("/buy?id=com.roblox.robloxmobile.premium80robux"),
            1,
            "the app took the page's navigation, so only its own load fetched the page"
        );

        evaluate("document.getElementById('link').click()");
        answer("/linked", false, false);
        finish("/linked");
        evaluate("document.getElementById('search').requestSubmit()");
        answer("/search?q=a", false, false);
        finish("/search?q=a");
        evaluate("location.href='/hop'");
        answer("/hop", false, false);
        answer("/landed", true, false);
        finish("/landed");

        load("/hop");
        answer("/landed", true, true);
        load("/landed");
        finish("/landed");

        let posted = Rc::new(Cell::new(false));
        let noted = Rc::clone(&posted);
        app.view(view)
            .expect("the view")
            .web_view
            .evaluate_javascript(
                &format!(
                    "window.webkit.messageHandlers.{}.postMessage(\
                     {{kind:'page',url:'{}',activation:true}});true",
                    intent::MESSAGE_HANDLER,
                    site.url("/main?again")
                ),
                Some(intent::SCRIPT_WORLD),
                None,
                None::<&gio::Cancellable>,
                move |result| noted.set(result.is_ok()),
            );
        pump_until("post a navigation intent", || posted.get());
        load("/main?again");
        finish("/main?again");
        evaluate(
            "var link=document.getElementById('link');\
             link.addEventListener('click',function(e){\
             e.preventDefault();history.pushState(null,'','/linked');});link.click()",
        );
        evaluate("location.reload()");
        finish("/linked");

        host.drain();
        let get = |target: &str, redirect: bool| (site.url(target), redirect, "GET".to_string());
        assert_eq!(
            *host.policies.borrow(),
            vec![
                get("/buy?id=com.roblox.robloxmobile.premium80robux", false),
                get("/linked", false),
                get("/search?q=a", false),
                get("/hop", false),
                get("/landed", true),
                get("/landed", true),
            ],
            "only main-frame page navigations and main-frame redirects reach the app"
        );
        app.handle(ConsumerMsg::CloseView { view });
        drop(app);
        std::fs::remove_dir_all(&root).expect("cleanup");
    }

    #[test]
    fn webkit_backed_checks_run_on_the_one_thread_that_starts_webkit() {
        an_engine_tears_down_without_touching_a_finalized_session();
        a_page_message_too_large_to_send_is_dropped_without_ending_the_helper();
        a_reply_too_large_to_send_reaches_the_host_as_a_failure();
        webkit_keeps_cookies_in_memory_and_hands_them_to_the_host();
        main_frame_navigations_reach_the_app_and_frame_navigations_do_not();
    }
}
