mod globals;
mod positioner;
mod session;
#[cfg(test)]
pub(super) mod tests;
mod wire;

use std::cell::RefCell;
use std::ffi::{c_void, CString};
use std::fmt;
use std::io;
use std::os::fd::{OwnedFd, RawFd};
use std::os::unix::net::UnixStream;
use std::ptr::NonNull;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex, MutexGuard, PoisonError};
use std::thread::JoinHandle;
use std::time::{Duration, Instant};

use rustix::event::{eventfd, poll, EventfdFlags, PollFd, PollFlags, Timespec};
use wayland_client::backend::protocol::{Argument, Interface, Message};
use wayland_client::backend::smallvec::SmallVec;
use wayland_client::backend::{
    Backend, InvalidId, ObjectData, ObjectId, ReadEventsGuard, WaylandError,
};
use wayland_client::protocol::{
    wl_callback::WlCallback, wl_display, wl_registry, wl_registry::WlRegistry, wl_shm,
    wl_shm::WlShm, wl_subcompositor, wl_subcompositor::WlSubcompositor, wl_surface::WlSurface,
};
use wayland_client::Proxy as _;

use globals::{Global, Provision};
use session::{Session, SessionEnd};

pub(crate) const HELPER_DISPLAY_FD: RawFd = 4;

const SETUP_TIMEOUT: Duration = Duration::from_secs(2);

const RECENTLY_REMOVED_LIMIT: usize = 32;

const THREAD_NAME: &str = "webview-embed";

#[derive(Debug)]
pub(crate) enum StartError {
    NotASurface,
    Wayland(String),
    TimedOut,
    Missing(&'static str),
    Thread(io::Error),
}

impl fmt::Display for StartError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::NotASurface => f.write_str("winit's window handle is not a wl_surface"),
            Self::Wayland(error) => write!(f, "listing the compositor's globals failed: {error}"),
            Self::TimedOut => write!(
                f,
                "the compositor did not list its globals within {} s",
                SETUP_TIMEOUT.as_secs()
            ),
            Self::Missing(interface) => {
                write!(f, "the compositor does not offer {interface}")
            }
            Self::Thread(error) => write!(f, "the embedding thread did not start: {error}"),
        }
    }
}

#[derive(Default)]
struct Requests {
    size: Option<(i32, i32)>,
    helper: Option<UnixStream>,
    stop: bool,
}

struct Control {
    wake: OwnedFd,
    requests: Mutex<Requests>,
}

impl Control {
    fn requests(&self) -> MutexGuard<'_, Requests> {
        self.requests.lock().unwrap_or_else(PoisonError::into_inner)
    }

    fn change(&self, change: impl FnOnce(&mut Requests)) {
        change(&mut self.requests());
        if let Err(error) = rustix::io::write(&self.wake, &1u64.to_ne_bytes()) {
            if error != rustix::io::Errno::AGAIN {
                tracing::warn!(%error, "the WebView embedding thread was not woken");
            }
        }
    }

    fn take(&self) -> Requests {
        let mut counter = [0u8; 8];
        let _ = rustix::io::read(&self.wake, &mut counter);
        std::mem::take(&mut *self.requests())
    }
}

pub(crate) struct Embedder {
    control: Arc<Control>,
    thread: Option<JoinHandle<()>>,
}

#[derive(Clone)]
pub(crate) struct Connector {
    control: Arc<Control>,
    alive: Arc<AtomicBool>,
}

impl Embedder {
    pub(crate) unsafe fn for_surface_whose_display_outlives_it(
        display: NonNull<c_void>,
        surface: NonNull<c_void>,
    ) -> Result<(Self, Connector), StartError> {
        let backend = unsafe { Backend::from_foreign_display(display.as_ptr().cast()) };
        let game_surface =
            unsafe { ObjectId::from_ptr(WlSurface::interface(), surface.as_ptr().cast()) }
                .map_err(|_| StartError::NotASurface)?;
        Self::start(backend, game_surface)
    }

    fn start(backend: Backend, game_surface: ObjectId) -> Result<(Self, Connector), StartError> {
        let upstream = Upstream::connect(backend, game_surface)?;
        let control = Arc::new(Control {
            wake: eventfd(0, EventfdFlags::CLOEXEC | EventfdFlags::NONBLOCK)
                .map_err(|error| StartError::Thread(error.into()))?,
            requests: Mutex::default(),
        });
        let alive = Arc::new(AtomicBool::new(true));
        let proxy = Proxy {
            up: upstream,
            control: Arc::clone(&control),
            size: None,
            session: None,
        };
        let running = Arc::clone(&alive);
        let thread = std::thread::Builder::new()
            .name(THREAD_NAME.into())
            .spawn(move || {
                proxy.run();
                running.store(false, Ordering::Release);
            })
            .map_err(StartError::Thread)?;
        let connector = Connector {
            control: Arc::clone(&control),
            alive,
        };
        Ok((
            Self {
                control,
                thread: Some(thread),
            },
            connector,
        ))
    }

    pub(crate) fn resize(&self, width: i32, height: i32) {
        self.control
            .change(|requests| requests.size = Some((width, height)));
    }
}

impl Drop for Embedder {
    fn drop(&mut self) {
        self.control.change(|requests| requests.stop = true);
        if let Some(thread) = self.thread.take() {
            if thread.join().is_err() {
                tracing::error!("the WebView embedding thread panicked");
            }
        }
    }
}

impl Connector {
    pub(crate) fn helper_display(&self) -> io::Result<OwnedFd> {
        if !self.alive.load(Ordering::Acquire) {
            return Err(io::Error::other("the WebView embedding thread has stopped"));
        }
        let (ours, theirs) = UnixStream::pair()?;
        self.control.change(|requests| requests.helper = Some(ours));
        Ok(theirs.into())
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum Route {
    Forwarded,
    Registry,
    Synced,
    Internal,
}

struct Sink {
    events: Mutex<Vec<(Route, Message<ObjectId, OwnedFd>)>>,
    synced: AtomicBool,
}

struct Receiver {
    route: Route,
    sink: Arc<Sink>,
}

impl ObjectData for Receiver {
    fn event(
        self: Arc<Self>,
        _: &Backend,
        msg: Message<ObjectId, OwnedFd>,
    ) -> Option<Arc<dyn ObjectData>> {
        let creates = msg
            .args
            .iter()
            .any(|arg| matches!(arg, Argument::NewId(id) if !id.is_null()));
        match self.route {
            Route::Synced => self.sink.synced.store(true, Ordering::Release),
            Route::Internal => {}
            Route::Forwarded | Route::Registry => self
                .sink
                .events
                .lock()
                .unwrap_or_else(PoisonError::into_inner)
                .push((self.route, msg)),
        }
        creates.then_some(self)
    }

    fn destroyed(&self, _: ObjectId) {}
}

struct SharedBinding {
    name: u32,
    version: u32,
    id: ObjectId,
}

struct Upstream {
    backend: Backend,
    sink: Arc<Sink>,
    forwarded: Arc<Receiver>,
    internal: Arc<Receiver>,
    registry: ObjectId,
    subcompositor: ObjectId,
    game_surface: ObjectId,
    globals: Vec<Global>,
    removed: Vec<Global>,
    shared: RefCell<Vec<SharedBinding>>,
}

type Outgoing = Message<ObjectId, RawFd>;

fn outgoing(sender: ObjectId, opcode: u16, args: Vec<Argument<ObjectId, RawFd>>) -> Outgoing {
    Message {
        sender_id: sender,
        opcode,
        args: SmallVec::from_vec(args),
    }
}

impl Upstream {
    fn connect(backend: Backend, game_surface: ObjectId) -> Result<Self, StartError> {
        let sink = Arc::new(Sink {
            events: Mutex::default(),
            synced: AtomicBool::new(false),
        });
        let receiver = |route| {
            Arc::new(Receiver {
                route,
                sink: Arc::clone(&sink),
            })
        };
        let registry = backend
            .send_request(
                outgoing(
                    backend.display_id(),
                    wl_display::REQ_GET_REGISTRY_OPCODE,
                    vec![Argument::NewId(ObjectId::null())],
                ),
                Some(receiver(Route::Registry)),
                Some((WlRegistry::interface(), 1)),
            )
            .map_err(|_| StartError::Wayland("the display is gone".into()))?;
        let mut up = Self {
            forwarded: receiver(Route::Forwarded),
            internal: receiver(Route::Internal),
            sink: Arc::clone(&sink),
            backend,
            registry,
            subcompositor: ObjectId::null(),
            game_surface,
            globals: Vec::new(),
            removed: Vec::new(),
            shared: RefCell::default(),
        };
        up.roundtrip()?;
        let advertised: Vec<(u32, String, u32)> = up
            .take_events()
            .into_iter()
            .filter_map(|(_, msg)| match msg.args.as_slice() {
                [Argument::Uint(name), Argument::Str(Some(interface)), Argument::Uint(version)]
                    if msg.opcode == wl_registry::EVT_GLOBAL_OPCODE =>
                {
                    Some((*name, interface.to_string_lossy().into_owned(), *version))
                }
                _ => None,
            })
            .collect();
        if let Some(missing) = globals::missing_required(
            advertised
                .iter()
                .map(|(_, interface, _)| interface.as_str()),
        ) {
            up.backend.destroy_object(&up.registry).ok();
            return Err(StartError::Missing(missing));
        }
        up.globals = advertised
            .iter()
            .filter_map(|(name, interface, version)| globals::offer(*name, interface, *version))
            .collect();
        let subcompositor = up
            .globals
            .iter()
            .find(|global| global.interface.name == WlSubcompositor::interface().name)
            .copied();
        if let Some(global) = subcompositor {
            up.subcompositor = up
                .bind(
                    global.name,
                    WlSubcompositor::interface(),
                    1,
                    Route::Internal,
                )
                .map_err(|_| StartError::Wayland("binding wl_subcompositor failed".into()))?;
        }
        up.backend
            .flush()
            .map_err(|error| StartError::Wayland(error.to_string()))?;
        Ok(up)
    }

    fn roundtrip(&self) -> Result<(), StartError> {
        let wayland = |error: WaylandError| StartError::Wayland(error.to_string());
        self.sink.synced.store(false, Ordering::Release);
        self.backend
            .send_request(
                outgoing(
                    self.backend.display_id(),
                    wl_display::REQ_SYNC_OPCODE,
                    vec![Argument::NewId(ObjectId::null())],
                ),
                Some(Arc::new(Receiver {
                    route: Route::Synced,
                    sink: Arc::clone(&self.sink),
                })),
                Some((WlCallback::interface(), 1)),
            )
            .map_err(|_| StartError::Wayland("the display is gone".into()))?;
        let deadline = Instant::now() + SETUP_TIMEOUT;
        loop {
            match self.backend.flush() {
                Ok(()) => {}
                Err(WaylandError::Io(error)) if error.kind() == io::ErrorKind::WouldBlock => {}
                Err(error) => return Err(wayland(error)),
            }
            self.backend.dispatch_inner_queue().map_err(wayland)?;
            if self.sink.synced.load(Ordering::Acquire) {
                return Ok(());
            }
            let left = deadline
                .checked_duration_since(Instant::now())
                .ok_or(StartError::TimedOut)?;
            let Some(guard) = self.backend.prepare_read() else {
                continue;
            };
            let readable = {
                let fd = guard.connection_fd();
                let mut fds = [PollFd::new(&fd, PollFlags::IN)];
                let timeout = Timespec::try_from(left).unwrap_or(Timespec {
                    tv_sec: 0,
                    tv_nsec: 0,
                });
                poll(&mut fds, Some(&timeout))
                    .map_err(|error| StartError::Wayland(error.to_string()))?
                    > 0
            };
            if readable {
                match guard.read() {
                    Ok(_) => {}
                    Err(WaylandError::Io(error)) if error.kind() == io::ErrorKind::WouldBlock => {}
                    Err(error) => return Err(wayland(error)),
                }
            }
        }
    }

    fn take_events(&self) -> Vec<(Route, Message<ObjectId, OwnedFd>)> {
        std::mem::take(
            &mut *self
                .sink
                .events
                .lock()
                .unwrap_or_else(PoisonError::into_inner),
        )
    }

    fn bind(
        &self,
        name: u32,
        interface: &'static Interface,
        version: u32,
        route: Route,
    ) -> Result<ObjectId, InvalidId> {
        let data = match route {
            Route::Forwarded => Arc::clone(&self.forwarded),
            _ => Arc::clone(&self.internal),
        };
        let interface_name = CString::new(interface.name).map_err(|_| InvalidId)?;
        self.backend.send_request(
            outgoing(
                self.registry.clone(),
                wl_registry::REQ_BIND_OPCODE,
                vec![
                    Argument::Uint(name),
                    Argument::Str(Some(Box::new(interface_name))),
                    Argument::Uint(version),
                    Argument::NewId(ObjectId::null()),
                ],
            ),
            Some(data),
            Some((interface, version)),
        )
    }

    fn sync(&self) -> Result<ObjectId, InvalidId> {
        self.backend.send_request(
            outgoing(
                self.backend.display_id(),
                wl_display::REQ_SYNC_OPCODE,
                vec![Argument::NewId(ObjectId::null())],
            ),
            Some(Arc::clone(&self.forwarded) as Arc<dyn ObjectData>),
            Some((WlCallback::interface(), 1)),
        )
    }

    fn send(&self, message: Outgoing, creates: bool) -> Result<ObjectId, InvalidId> {
        let data = creates.then(|| Arc::clone(&self.forwarded) as Arc<dyn ObjectData>);
        self.backend.send_request(message, data, None)
    }

    fn create_role_subsurface(&self, surface: &ObjectId) -> Result<ObjectId, InvalidId> {
        let subsurface = self.backend.send_request(
            outgoing(
                self.subcompositor.clone(),
                wl_subcompositor::REQ_GET_SUBSURFACE_OPCODE,
                vec![
                    Argument::NewId(ObjectId::null()),
                    Argument::Object(surface.clone()),
                    Argument::Object(self.game_surface.clone()),
                ],
            ),
            Some(Arc::clone(&self.internal) as Arc<dyn ObjectData>),
            None,
        )?;
        self.backend.send_request(
            outgoing(
                subsurface.clone(),
                wayland_client::protocol::wl_subsurface::REQ_SET_DESYNC_OPCODE,
                Vec::new(),
            ),
            None,
            None,
        )?;
        Ok(subsurface)
    }

    fn destroy(&self, id: &ObjectId) {
        if self.shared.borrow().iter().any(|binding| binding.id == *id) {
            return;
        }
        let Ok(info) = self.backend.info(id.clone()) else {
            return;
        };
        let destructor = info.interface.requests.iter().position(|request| {
            request.is_destructor && request.since <= info.version && request.signature.is_empty()
        });
        match destructor.and_then(|opcode| u16::try_from(opcode).ok()) {
            Some(opcode) => {
                let _ =
                    self.backend
                        .send_request(outgoing(id.clone(), opcode, Vec::new()), None, None);
            }
            None => {
                let _ = self.backend.destroy_object(id);
            }
        }
    }

    fn global(&self, name: u32) -> Option<Global> {
        self.globals
            .iter()
            .find(|global| global.name == name)
            .copied()
    }

    fn removed_global(&self, name: u32) -> Option<Global> {
        self.removed
            .iter()
            .find(|global| global.name == name)
            .copied()
    }

    fn globals(&self) -> &[Global] {
        &self.globals
    }

    fn bind_for_helper(&self, global: Global, version: u32) -> Result<ObjectId, InvalidId> {
        if !shareable(global.interface, version) {
            let version = releasable_version(global, version);
            return self.bind(global.name, global.interface, version, Route::Forwarded);
        }
        let mut shared = self.shared.borrow_mut();
        let existing = shared
            .iter()
            .find(|binding| binding.name == global.name && binding.version == version);
        if let Some(binding) = existing {
            return Ok(binding.id.clone());
        }
        let id = self.bind(global.name, global.interface, version, Route::Forwarded)?;
        shared.push(SharedBinding {
            name: global.name,
            version,
            id: id.clone(),
        });
        Ok(id)
    }

    fn registry_event(&mut self, msg: &Message<ObjectId, OwnedFd>) -> Option<RegistryChange> {
        match (msg.opcode, msg.args.as_slice()) {
            (
                wl_registry::EVT_GLOBAL_OPCODE,
                [Argument::Uint(name), Argument::Str(Some(interface)), Argument::Uint(version)],
            ) => {
                let global = globals::offer(*name, &interface.to_string_lossy(), *version)?;
                if global.provision == Provision::Emulated {
                    return None;
                }
                self.globals.push(global);
                Some(RegistryChange::Added(global))
            }
            (wl_registry::EVT_GLOBAL_REMOVE_OPCODE, [Argument::Uint(name)]) => {
                let index = self
                    .globals
                    .iter()
                    .position(|global| global.name == *name)?;
                let global = self.globals.remove(index);
                if self.removed.len() == RECENTLY_REMOVED_LIMIT {
                    self.removed.remove(0);
                }
                self.removed.push(global);
                Some(RegistryChange::Removed(*name))
            }
            _ => None,
        }
    }

    fn shutdown(self) {
        if !self.subcompositor.is_null() {
            self.destroy(&self.subcompositor);
        }
        for binding in self.shared.take() {
            let _ = self.backend.destroy_object(&binding.id);
        }
        let _ = self.backend.destroy_object(&self.registry);
        let _ = self.backend.flush();
    }
}

fn shareable(interface: &Interface, version: u32) -> bool {
    let destructible = interface
        .requests
        .iter()
        .any(|request| request.is_destructor && request.since <= version);
    let speaks = interface.events.iter().any(|event| event.since <= version);
    !destructible && !speaks
}

fn releasable_version(global: Global, version: u32) -> u32 {
    let releasable = wl_shm::REQ_RELEASE_SINCE;
    if global.interface.name == WlShm::interface().name && global.version >= releasable {
        version.max(releasable)
    } else {
        version
    }
}

enum RegistryChange {
    Added(Global),
    Removed(u32),
}

enum Flow {
    Continue,
    Stop,
}

struct Proxy {
    up: Upstream,
    control: Arc<Control>,
    size: Option<(i32, i32)>,
    session: Option<Session>,
}

impl Proxy {
    fn run(mut self) {
        loop {
            match self.turn() {
                Ok(Flow::Continue) => {}
                Ok(Flow::Stop) => break,
                Err(error) => {
                    tracing::warn!(
                        %error,
                        "the game's Wayland connection failed; WebView embedding stops"
                    );
                    break;
                }
            }
        }
        self.shut_down();
    }

    fn shut_down(mut self) {
        if let Some(session) = self.session.take() {
            self.retire(session, None);
        }
        self.up.shutdown();
    }

    fn retire(&mut self, session: Session, end: Option<&SessionEnd>) {
        let reads_held = self.settle_and_hold_reads();
        match end {
            Some(end) => session.disconnect(&self.up, end),
            None => session.teardown(&self.up),
        }
        drop(reads_held);
    }

    fn settle_and_hold_reads(&mut self) -> Option<ReadEventsGuard> {
        loop {
            if self.up.backend.dispatch_inner_queue().is_err() {
                return None;
            }
            self.route_events();
            if let Some(guard) = self.up.backend.prepare_read() {
                return Some(guard);
            }
        }
    }

    fn turn(&mut self) -> Result<Flow, WaylandError> {
        self.up.backend.dispatch_inner_queue()?;
        self.route_events();
        if let Some(Err(end)) = self.session.as_mut().map(Session::flush) {
            self.end_session(SessionEnd::Wire(end));
        }
        let blocked = match self.up.backend.flush() {
            Ok(()) => false,
            Err(WaylandError::Io(error)) if error.kind() == io::ErrorKind::WouldBlock => true,
            Err(error) => return Err(error),
        };
        if self.session.is_none() {
            return self.wait_for_control(blocked);
        }
        let reading = !blocked && self.session.as_ref().is_some_and(Session::wants_read);
        let Some(guard) = self.up.backend.prepare_read() else {
            return Ok(Flow::Continue);
        };
        let ready = {
            let upstream = guard.connection_fd();
            let mut upstream_flags = PollFlags::IN;
            if blocked {
                upstream_flags |= PollFlags::OUT;
            }
            let helper = self.session.as_ref().and_then(|session| {
                let mut flags = PollFlags::empty();
                if reading {
                    flags |= PollFlags::IN;
                }
                if session.wants_write() {
                    flags |= PollFlags::OUT;
                }
                (!flags.is_empty()).then(|| (session.fd(), flags))
            });
            let mut fds = vec![
                PollFd::new(&upstream, upstream_flags),
                PollFd::new(&self.control.wake, PollFlags::IN),
            ];
            if let Some((fd, flags)) = &helper {
                fds.push(PollFd::new(fd, *flags));
            }
            match poll(&mut fds, None) {
                Ok(_) => fds.iter().map(PollFd::revents).collect(),
                Err(rustix::io::Errno::INTR) => Vec::new(),
                Err(error) => return Err(WaylandError::Io(error.into())),
            }
        };
        let revents = |index: usize| ready.get(index).copied().unwrap_or(PollFlags::empty());
        let is_ready = |index: usize| !revents(index).is_empty();
        if revents(0).intersects(PollFlags::IN | PollFlags::ERR | PollFlags::HUP) {
            match guard.read() {
                Ok(_) => {}
                Err(WaylandError::Io(error)) if error.kind() == io::ErrorKind::WouldBlock => {}
                Err(error) => return Err(error),
            }
        } else {
            drop(guard);
        }
        if is_ready(1) && matches!(self.control_changed(), Flow::Stop) {
            return Ok(Flow::Stop);
        }
        if is_ready(2) && reading {
            self.route_events();
            let served = self.session.as_mut().map(|session| session.serve(&self.up));
            if let Some(Err(end)) = served {
                self.end_session(end);
            }
        }
        Ok(Flow::Continue)
    }

    fn wait_for_control(&mut self, blocked: bool) -> Result<Flow, WaylandError> {
        let changed = {
            let upstream = self.up.backend.poll_fd();
            let mut fds = vec![PollFd::new(&self.control.wake, PollFlags::IN)];
            if blocked {
                fds.push(PollFd::new(&upstream, PollFlags::OUT));
            }
            match poll(&mut fds, None) {
                Ok(_) => !fds[0].revents().is_empty(),
                Err(rustix::io::Errno::INTR) => false,
                Err(error) => return Err(WaylandError::Io(error.into())),
            }
        };
        if changed {
            return Ok(self.control_changed());
        }
        Ok(Flow::Continue)
    }

    fn control_changed(&mut self) -> Flow {
        let requests = self.control.take();
        if requests.stop {
            return Flow::Stop;
        }
        if let Some(size) = requests.size {
            self.size = Some(size);
            if let Some(session) = &mut self.session {
                session.resize(size);
            }
        }
        if let Some(stream) = requests.helper {
            self.attach(stream);
        }
        Flow::Continue
    }

    fn attach(&mut self, stream: UnixStream) {
        if let Some(previous) = self.session.take() {
            tracing::info!("a new WebView helper replaces the previous one's embedded pages");
            self.retire(previous, None);
        }
        match Session::new(stream, self.size) {
            Ok(session) => self.session = Some(session),
            Err(error) => tracing::warn!(%error, "the WebView helper's display socket is unusable"),
        }
    }

    fn end_session(&mut self, end: SessionEnd) {
        let Some(session) = self.session.take() else {
            return;
        };
        match &end {
            SessionEnd::Wire(wire::WireError::Closed) => {
                tracing::info!("the WebView helper closed its display; its pages are gone")
            }
            other => tracing::warn!(
                reason = %other,
                "disconnected the WebView helper from the game window; its pages are gone"
            ),
        }
        self.retire(session, Some(&end));
    }

    fn route_events(&mut self) {
        for (route, msg) in self.up.take_events() {
            match route {
                Route::Registry => {
                    let change = self.up.registry_event(&msg);
                    if let (Some(change), Some(session)) = (change, &mut self.session) {
                        session.registry_changed(&change);
                    }
                }
                Route::Forwarded => match &mut self.session {
                    Some(session) => session.deliver(&self.up, msg),
                    None => session::discard(&self.up, msg),
                },
                Route::Synced | Route::Internal => {}
            }
        }
        if let Some(Err(end)) = self.session.as_mut().map(Session::health) {
            self.end_session(SessionEnd::Wire(end));
        }
        let resumed = self
            .session
            .as_mut()
            .map(|session| session.process(&self.up));
        if let Some(Err(end)) = resumed {
            self.end_session(end);
        }
    }
}

pub(crate) fn hand_display_to(command: &mut std::process::Command) {
    command
        .env("WAYLAND_SOCKET", HELPER_DISPLAY_FD.to_string())
        .env("GDK_BACKEND", "wayland")
        .env_remove("WAYLAND_DISPLAY");
}
