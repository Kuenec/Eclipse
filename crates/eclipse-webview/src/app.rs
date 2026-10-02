use crate::cookies::{self, SessionJar};
use crate::logging::{self, Redacted};
use crate::view::{self, Route, View};
use crate::wire::{Inbound, Wire};
use eclipse_webview::proto::{
    ClearScope, ConsumerMsg, CookieExpiry, CookiePair, HelperMsg, ProtoError, StoredCookie,
    PERSISTENT_COOKIE_FILE,
};
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

pub(crate) const EXIT_REQUESTED: u8 = 0;

pub(crate) const EXIT_CONSUMER_LOST: u8 = 2;

pub(crate) struct Storage {
    pub(crate) data: PathBuf,
    pub(crate) cache: PathBuf,
}

pub(crate) struct Engine {
    session: webkit6::NetworkSession,
    jar: SessionJar,
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
        let cookie_file = storage.data.join(PERSISTENT_COOKIE_FILE);
        let (data, cache, cookie_file) = (
            utf8(&storage.data)?,
            utf8(&storage.cache)?,
            utf8(&cookie_file)?,
        );
        let session = webkit6::NetworkSession::new(Some(data), Some(cache));
        session
            .cookie_manager()
            .ok_or("the WebKit network session has no cookie manager")?
            .set_persistent_storage(cookie_file, webkit6::CookiePersistentStorage::Sqlite);
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
        Ok(Engine {
            session,
            jar: SessionJar::new(storage.data.join("session-cookies")),
        })
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
        });
        app.restore_session_cookies();
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
        let main_loop = self.main_loop.clone();
        self.snapshot(move |_| main_loop.quit());
    }

    fn handle(self: &Rc<Self>, msg: ConsumerMsg) {
        match msg {
            ConsumerMsg::Hello { .. } => {
                logging::error(format_args!("protocol violation: a second Hello"));
                self.stop(EXIT_CONSUMER_LOST);
            }
            ConsumerMsg::CreateView { view } => self.create_view(view),
            ConsumerMsg::CloseView { view } => self.close_view(view),
            ConsumerMsg::SetVisible { view, visible } => self.with_view(view, |view| {
                if !visible {
                    view.window.set_visible(false);
                } else if !view.window.is_visible() {
                    view.window.present();
                }
            }),
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
        let Some((request, redirect, user_gesture)) = decision
            .downcast_ref::<webkit6::NavigationPolicyDecision>()
            .and_then(webkit6::NavigationPolicyDecision::navigation_action)
            .and_then(|action| {
                let request = action.request()?;
                Some((request, action.is_redirect(), action.is_user_gesture()))
            })
        else {
            return false;
        };
        let url = request.uri().map(String::from).unwrap_or_default();
        let method = request.http_method();
        let app_initiated = !redirect && view.take_app_load(&url);
        if view::navigation_route(&url, method.as_deref(), redirect, app_initiated) == Route::Engine
        {
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

    fn restore_session_cookies(self: &Rc<Self>) {
        let stored = match self.engine.jar.load() {
            Ok(stored) => stored,
            Err(error) => {
                logging::warn(format_args!(
                    "ignoring the saved session cookies in {}: {error}",
                    self.engine.jar.path().display()
                ));
                return;
            }
        };
        let restored = stored.len();
        for cookie in stored {
            match cookies::to_soup(&cookie) {
                Ok(cookie) => self.engine.cookie_manager().add_cookie(
                    &cookie,
                    None::<&gio::Cancellable>,
                    |result| {
                        if let Err(error) = result {
                            logging::warn(format_args!("cannot restore a session cookie: {error}"));
                        }
                    },
                ),
                Err(error) => {
                    logging::warn(format_args!("cannot restore a session cookie: {error}"))
                }
            }
        }
        if restored > 0 {
            logging::info(format_args!("restoring {restored} session cookie(s)"));
        }
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

    fn snapshot(&self, done: impl FnOnce(bool) + 'static) {
        let path = self.engine.jar.path().to_path_buf();
        self.engine
            .cookie_manager()
            .all_cookies(None::<&gio::Cancellable>, move |result| {
                let cookies = match result {
                    Ok(cookies) => cookies,
                    Err(error) => {
                        logging::error(format_args!("cannot list cookies to save: {error}"));
                        done(false);
                        return;
                    }
                };
                let session: Vec<StoredCookie> = cookies
                    .into_iter()
                    .map(|mut cookie| cookies::from_soup(&mut cookie))
                    .filter(|cookie| cookie.expiry == CookieExpiry::Session)
                    .collect();
                match SessionJar::new(path).store(&session) {
                    Ok(()) => done(true),
                    Err(error) => {
                        logging::error(format_args!("cannot save the session cookies: {error}"));
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
    use crate::wire::Wire;
    use eclipse_webview::proto::{self, LoadEvent, GLOBAL_FRAME_CAP};
    use std::os::fd::OwnedFd;
    use std::os::unix::net::UnixStream;

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

    #[test]
    fn webkit_backed_checks_run_on_the_one_thread_that_starts_webkit() {
        an_engine_tears_down_without_touching_a_finalized_session();
        a_page_message_too_large_to_send_is_dropped_without_ending_the_helper();
        a_reply_too_large_to_send_reaches_the_host_as_a_failure();
    }
}
