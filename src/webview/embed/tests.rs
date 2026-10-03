use std::collections::{HashMap, VecDeque};
use std::ffi::CString;
use std::os::fd::OwnedFd;
use std::os::unix::net::UnixStream;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::mpsc;
use std::sync::Arc;
use std::thread::JoinHandle;
use std::time::{Duration, Instant};

use rustix::event::{poll, PollFd, PollFlags, Timespec};
use wayland_client::backend::protocol::{Argument, Interface, Message};
use wayland_client::backend::{Backend, ObjectData, ObjectId};
use wayland_client::protocol::{
    wl_callback, wl_compositor, wl_compositor::WlCompositor,
    wl_data_device_manager::WlDataDeviceManager, wl_display, wl_display::WlDisplay, wl_keyboard,
    wl_output, wl_output::WlOutput, wl_pointer, wl_region, wl_registry, wl_seat, wl_seat::WlSeat,
    wl_shm, wl_shm::WlShm, wl_shm_pool, wl_subcompositor, wl_subcompositor::WlSubcompositor,
    wl_subsurface, wl_surface,
};
use wayland_client::Proxy as _;
use wayland_protocols::wp::linux_dmabuf::zv1::client::{
    zwp_linux_buffer_params_v1, zwp_linux_buffer_params_v1::ZwpLinuxBufferParamsV1,
    zwp_linux_dmabuf_v1, zwp_linux_dmabuf_v1::ZwpLinuxDmabufV1,
};
use wayland_protocols::xdg::shell::client::{
    xdg_popup, xdg_positioner, xdg_surface, xdg_toplevel, xdg_wm_base, xdg_wm_base::XdgWmBase,
};

use super::wire::{Args, Wire, WireError};
use super::{Connector, Embedder};

const WAIT: Duration = Duration::from_secs(5);

const QUIET: Duration = Duration::from_millis(300);

const SIZE: (i32, i32) = (800, 500);

#[derive(Debug)]
struct Seen {
    object: u32,
    interface: &'static str,
    message: &'static str,
    args: Args,
}

fn new_id(args: &Args) -> Option<u32> {
    args.iter().find_map(|arg| match arg {
        Argument::NewId(id) => Some(*id),
        _ => None,
    })
}

fn object_args(args: &Args) -> Vec<u32> {
    args.iter()
        .filter_map(|arg| match arg {
            Argument::Object(id) => Some(*id),
            _ => None,
        })
        .collect()
}

fn read_messages(
    wire: &mut Wire,
    objects: &mut HashMap<u32, (&'static Interface, u32)>,
    requests: bool,
) -> Result<Vec<Seen>, WireError> {
    let mut seen = Vec::new();
    while let Some(header) = wire.header()? {
        let (interface, version) = objects[&header.object];
        let messages = if requests {
            interface.requests
        } else {
            interface.events
        };
        let desc = &messages[usize::from(header.opcode)];
        let message = wire.decode(header, desc.signature)?;
        if let Some(id) = new_id(&message.args) {
            let child = match message.args.as_slice() {
                [Argument::Uint(_), Argument::Str(Some(name)), Argument::Uint(bound), _] => {
                    named(&name.to_string_lossy()).map(|interface| (interface, *bound))
                }
                _ => desc.child_interface.map(|child| (child, version)),
            };
            if let Some(child) = child {
                objects.insert(id, child);
            }
        }
        seen.push(Seen {
            object: header.object,
            interface: interface.name,
            message: desc.name,
            args: message.args,
        });
    }
    Ok(seen)
}

fn named(name: &str) -> Option<&'static Interface> {
    [
        WlCompositor::interface(),
        WlSubcompositor::interface(),
        WlShm::interface(),
        WlDataDeviceManager::interface(),
        XdgWmBase::interface(),
        WlSeat::interface(),
        WlOutput::interface(),
        ZwpLinuxDmabufV1::interface(),
    ]
    .into_iter()
    .find(|interface| interface.name == name)
}

fn wait_readable(fd: std::os::fd::BorrowedFd<'_>, timeout: Duration) {
    let mut fds = [PollFd::new(&fd, PollFlags::IN)];
    let timeout = Timespec::try_from(timeout).expect("timeout");
    let _ = poll(&mut fds, Some(&timeout));
}

const GLOBALS: [(u32, &str, u32); 10] = [
    (1, "wl_compositor", 6),
    (2, "wl_subcompositor", 1),
    (3, "wl_shm", 2),
    (4, "wl_data_device_manager", 3),
    (5, "xdg_wm_base", 6),
    (6, "wl_seat", 9),
    (7, "xdg_activation_v1", 1),
    (8, "zxdg_exporter_v2", 1),
    (9, "wl_output", 4),
    (10, "zwp_linux_dmabuf_v1", 4),
];

const IMPORTABLE_DMABUF: u32 = u32::from_le_bytes(*b"AR24");

const UNIMPORTABLE_DMABUF: u32 = u32::from_le_bytes(*b"XR24");

const FIRST_SERVER_ID: u32 = 0xff00_0000;

const SHM_FORMATS: [wl_shm::Format; 3] = [
    wl_shm::Format::Argb8888,
    wl_shm::Format::Xrgb8888,
    wl_shm::Format::Abgr16161616f,
];

struct Compositor {
    events: mpsc::Sender<(u32, u16, Args)>,
    seen: mpsc::Receiver<Seen>,
    log: Vec<Seen>,
    stalled: Arc<AtomicBool>,
    thread: Option<JoinHandle<()>>,
}

impl Compositor {
    fn start(server: UnixStream) -> Self {
        let (events, inbox) = mpsc::channel();
        let (report, seen) = mpsc::channel();
        let stalled = Arc::new(AtomicBool::new(false));
        let reading = Arc::clone(&stalled);
        let thread =
            std::thread::spawn(move || serve_compositor(server, &inbox, &report, &reading));
        Self {
            events,
            seen,
            log: Vec::new(),
            stalled,
            thread: Some(thread),
        }
    }

    fn stall(&self, stalled: bool) {
        self.stalled.store(stalled, Ordering::Release);
    }

    fn send(&self, object: u32, opcode: u16, args: Args) {
        self.events
            .send((object, opcode, args))
            .expect("the fake compositor is running");
    }

    fn wait_until<T>(&mut self, what: &str, find: impl Fn(&[Seen]) -> Option<T>) -> T {
        let deadline = Instant::now() + WAIT;
        loop {
            if let Some(found) = find(&self.log) {
                return found;
            }
            let left = deadline
                .checked_duration_since(Instant::now())
                .unwrap_or_else(|| panic!("the compositor never saw {what}: {:#?}", self.log));
            if let Ok(seen) = self.seen.recv_timeout(left) {
                self.log.push(seen);
            }
        }
    }

    fn wait_for(&mut self, what: &str, matches: impl Fn(&Seen) -> bool) {
        self.wait_until(what, |log| log.iter().any(&matches).then_some(()));
    }

    fn settle(&mut self) {
        while let Ok(seen) = self.seen.recv_timeout(QUIET) {
            self.log.push(seen);
        }
    }

    fn saw(&mut self, interface: &str, message: &str) -> Vec<&Seen> {
        self.settle();
        self.log
            .iter()
            .filter(|seen| seen.interface == interface && seen.message == message)
            .collect()
    }

    fn created(&mut self, interface: &str, message: &str, nth: usize) -> u32 {
        self.wait_until(&format!("{interface}.{message} #{nth}"), |log| {
            log.iter()
                .filter(|seen| seen.interface == interface && seen.message == message)
                .nth(nth)
                .and_then(|seen| new_id(&seen.args))
        })
    }
}

impl Drop for Compositor {
    fn drop(&mut self) {
        let (closed, _) = mpsc::channel();
        self.events = closed;
        if let Some(thread) = self.thread.take() {
            let _ = thread.join();
        }
    }
}

fn serve_compositor(
    server: UnixStream,
    inbox: &mpsc::Receiver<(u32, u16, Args)>,
    report: &mpsc::Sender<Seen>,
    stalled: &AtomicBool,
) {
    let mut wire = Wire::requests(server).expect("compositor socket");
    let mut objects: HashMap<u32, (&'static Interface, u32)> =
        HashMap::from([(1, (WlDisplay::interface(), 1))]);
    let mut serial = 0u32;
    let mut newest_surface = None;
    let mut next_server_id = FIRST_SERVER_ID;
    loop {
        loop {
            match inbox.try_recv() {
                Ok((object, opcode, args)) => wire.push(object, opcode, args).expect("event"),
                Err(mpsc::TryRecvError::Empty) => break,
                Err(mpsc::TryRecvError::Disconnected) => return,
            }
        }
        if wire.flush().is_err() {
            return;
        }
        if stalled.load(Ordering::Acquire) {
            std::thread::sleep(Duration::from_millis(10));
            continue;
        }
        wait_readable(wire.fd(), Duration::from_millis(10));
        match wire.receive() {
            Ok(()) => {}
            Err(_) => return,
        }
        let Ok(requests) = read_messages(&mut wire, &mut objects, true) else {
            return;
        };
        for seen in requests {
            match (seen.interface, seen.message, seen.args.as_slice()) {
                ("wl_compositor", "create_surface", [Argument::NewId(surface)]) => {
                    newest_surface = Some(*surface);
                }
                ("wl_output", "release", []) => {
                    if let Some(surface) = newest_surface {
                        let _ = wire.push(
                            surface,
                            wl_surface::EVT_LEAVE_OPCODE,
                            vec![Argument::Object(seen.object)],
                        );
                    }
                }
                ("zwp_linux_buffer_params_v1", "create", [_, _, Argument::Uint(format), _]) => {
                    if *format == IMPORTABLE_DMABUF {
                        let (_, version) = objects[&seen.object];
                        objects.insert(
                            next_server_id,
                            (
                                wayland_client::protocol::wl_buffer::WlBuffer::interface(),
                                version,
                            ),
                        );
                        let _ = wire.push(
                            seen.object,
                            zwp_linux_buffer_params_v1::EVT_CREATED_OPCODE,
                            vec![Argument::NewId(next_server_id)],
                        );
                        next_server_id += 1;
                    } else {
                        let _ = wire.push(
                            seen.object,
                            zwp_linux_buffer_params_v1::EVT_FAILED_OPCODE,
                            Vec::new(),
                        );
                    }
                }
                ("wl_display", "sync", [Argument::NewId(callback)]) => {
                    serial += 1;
                    let _ = wire.push(
                        *callback,
                        wl_callback::EVT_DONE_OPCODE,
                        vec![Argument::Uint(serial)],
                    );
                    let _ = wire.push(
                        1,
                        wl_display::EVT_DELETE_ID_OPCODE,
                        vec![Argument::Uint(*callback)],
                    );
                    objects.remove(callback);
                }
                ("wl_display", "get_registry", [Argument::NewId(registry)]) => {
                    for (name, interface, version) in GLOBALS {
                        let _ = wire.push(
                            *registry,
                            wl_registry::EVT_GLOBAL_OPCODE,
                            vec![
                                Argument::Uint(name),
                                Argument::Str(Some(Box::new(
                                    CString::new(interface).expect("name"),
                                ))),
                                Argument::Uint(version),
                            ],
                        );
                    }
                }
                (
                    "wl_registry",
                    "bind",
                    [_, Argument::Str(Some(name)), _, Argument::NewId(seat)],
                ) if name.to_bytes() == b"wl_seat" => {
                    let capabilities =
                        (wl_seat::Capability::Pointer | wl_seat::Capability::Keyboard).bits();
                    let _ = wire.push(
                        *seat,
                        wl_seat::EVT_CAPABILITIES_OPCODE,
                        vec![Argument::Uint(capabilities)],
                    );
                }
                (
                    "wl_registry",
                    "bind",
                    [_, Argument::Str(Some(name)), _, Argument::NewId(shm)],
                ) if name.to_bytes() == b"wl_shm" => {
                    for format in SHM_FORMATS {
                        let _ = wire.push(
                            *shm,
                            wl_shm::EVT_FORMAT_OPCODE,
                            vec![Argument::Uint(format as u32)],
                        );
                    }
                }
                _ => {}
            }
            let (interface, _) = objects
                .get(&seen.object)
                .copied()
                .unwrap_or((WlDisplay::interface(), 1));
            let destructor = interface
                .requests
                .iter()
                .find(|request| request.name == seen.message)
                .is_some_and(|request| request.is_destructor);
            if destructor {
                objects.remove(&seen.object);
                let _ = wire.push(
                    1,
                    wl_display::EVT_DELETE_ID_OPCODE,
                    vec![Argument::Uint(seen.object)],
                );
            }
            if report.send(seen).is_err() {
                return;
            }
        }
    }
}

struct Ignore;

impl ObjectData for Ignore {
    fn event(
        self: Arc<Self>,
        _: &Backend,
        msg: Message<ObjectId, OwnedFd>,
    ) -> Option<Arc<dyn ObjectData>> {
        let creates = msg
            .args
            .iter()
            .any(|arg| matches!(arg, Argument::NewId(id) if !id.is_null()));
        creates.then_some(self as Arc<dyn ObjectData>)
    }

    fn destroyed(&self, _: ObjectId) {}
}

struct Game {
    compositor: Compositor,
    embedder: Option<Embedder>,
    connector: Connector,
    surface: ObjectId,
    registry: ObjectId,
    backend: Backend,
}

fn game_request(
    backend: &Backend,
    sender: ObjectId,
    opcode: u16,
    args: Vec<Argument<ObjectId, i32>>,
    child: Option<(&'static Interface, u32)>,
) -> ObjectId {
    backend
        .send_request(
            super::outgoing(sender, opcode, args),
            Some(Arc::new(Ignore)),
            child,
        )
        .expect("a game request")
}

fn game_bind(
    backend: &Backend,
    registry: &ObjectId,
    name: u32,
    interface: &'static Interface,
    version: u32,
) -> ObjectId {
    game_request(
        backend,
        registry.clone(),
        wl_registry::REQ_BIND_OPCODE,
        vec![
            Argument::Uint(name),
            Argument::Str(Some(Box::new(CString::new(interface.name).expect("name")))),
            Argument::Uint(version),
            Argument::NewId(ObjectId::null()),
        ],
        Some((interface, version)),
    )
}

impl Game {
    fn start() -> Self {
        let (client, server) = UnixStream::pair().expect("socketpair");
        let mut compositor = Compositor::start(server);
        let backend = Backend::connect(client).expect("libwayland-client");
        let registry = game_request(
            &backend,
            backend.display_id(),
            wl_display::REQ_GET_REGISTRY_OPCODE,
            vec![Argument::NewId(ObjectId::null())],
            Some((wl_registry::WlRegistry::interface(), 1)),
        );
        let game_compositor = game_bind(&backend, &registry, 1, WlCompositor::interface(), 4);
        let surface = game_request(
            &backend,
            game_compositor,
            wl_compositor::REQ_CREATE_SURFACE_OPCODE,
            vec![Argument::NewId(ObjectId::null())],
            None,
        );
        backend.flush().expect("flush the game's requests");
        compositor.wait_for("the game surface", |seen| seen.message == "create_surface");
        let (embedder, connector) = unsafe {
            Embedder::for_surface_whose_display_outlives_it(
                std::ptr::NonNull::new(backend.display_ptr().cast()).expect("display"),
                std::ptr::NonNull::new(surface.as_ptr().cast()).expect("surface"),
            )
        }
        .expect("the embedder starts on the game's connection");
        embedder.resize(SIZE.0, SIZE.1);
        Self {
            compositor,
            embedder: Some(embedder),
            connector,
            surface,
            registry,
            backend,
        }
    }

    fn bind(&self, name: u32, interface: &'static Interface, version: u32) -> ObjectId {
        let bound = game_bind(&self.backend, &self.registry, name, interface, version);
        self.backend.flush().expect("flush the game's bind");
        bound
    }

    fn helper(&self) -> Helper {
        Helper::new(
            self.connector
                .helper_display()
                .expect("a display for the helper"),
        )
    }

    fn game_surface(&self) -> u32 {
        self.surface.protocol_id()
    }
}

impl Drop for Game {
    fn drop(&mut self) {
        drop(self.embedder.take());
        let _ = self.backend.flush();
    }
}

#[derive(Debug)]
struct Event {
    object: u32,
    interface: &'static str,
    message: &'static str,
    args: Args,
}

struct Helper {
    wire: Wire,
    objects: HashMap<u32, (&'static Interface, u32)>,
    next: u32,
    backlog: VecDeque<Event>,
}

impl Helper {
    fn new(display: OwnedFd) -> Self {
        Self {
            wire: Wire::events(UnixStream::from(display)).expect("helper socket"),
            objects: HashMap::from([(1, (WlDisplay::interface(), 1))]),
            next: 2,
            backlog: VecDeque::new(),
        }
    }

    fn create(&mut self, interface: &'static Interface, version: u32) -> u32 {
        let id = self.next;
        self.next += 1;
        self.objects.insert(id, (interface, version));
        id
    }

    fn request(&mut self, object: u32, opcode: u16, args: Args) {
        self.wire
            .push(object, opcode, args)
            .expect("encode a request");
        self.wire.flush().expect("send a request");
    }

    fn pump(&mut self, timeout: Duration) -> Result<(), WireError> {
        wait_readable(self.wire.fd(), timeout);
        let received = self.wire.receive();
        for seen in read_messages(&mut self.wire, &mut self.objects, false)? {
            self.backlog.push_back(Event {
                object: seen.object,
                interface: seen.interface,
                message: seen.message,
                args: seen.args,
            });
        }
        received
    }

    fn wait_for(&mut self, what: &str, matches: impl Fn(&Event) -> bool) -> Event {
        let deadline = Instant::now() + WAIT;
        loop {
            if let Some(index) = self.backlog.iter().position(&matches) {
                return self.backlog.remove(index).expect("an event");
            }
            let left = deadline
                .checked_duration_since(Instant::now())
                .unwrap_or_else(|| panic!("the helper never got {what}: {:#?}", self.backlog));
            self.pump(left.min(QUIET))
                .unwrap_or_else(|error| panic!("waiting for {what}: {error}"));
        }
    }

    fn quiet(&mut self) -> Vec<Event> {
        let _ = self.pump(QUIET);
        self.backlog.drain(..).collect()
    }

    fn disconnected(&mut self) -> Vec<Event> {
        let deadline = Instant::now() + WAIT;
        while Instant::now() < deadline {
            match self.pump(QUIET) {
                Ok(()) => {}
                Err(WireError::Closed) => return self.backlog.drain(..).collect(),
                Err(error) => panic!("the helper socket failed: {error}"),
            }
        }
        panic!("the proxy kept the helper connected: {:#?}", self.backlog);
    }

    fn registry(&mut self) -> (u32, Vec<(u32, String, u32)>) {
        let registry = self.create(wl_registry::WlRegistry::interface(), 1);
        self.request(
            1,
            wl_display::REQ_GET_REGISTRY_OPCODE,
            vec![Argument::NewId(registry)],
        );
        self.roundtrip();
        let globals = self
            .backlog
            .drain(..)
            .filter(|event| event.object == registry)
            .filter_map(|event| match event.args.as_slice() {
                [Argument::Uint(name), Argument::Str(Some(interface)), Argument::Uint(version)] => {
                    Some((*name, interface.to_string_lossy().into_owned(), *version))
                }
                _ => None,
            })
            .collect();
        (registry, globals)
    }

    fn roundtrip(&mut self) {
        let callback = self.create(wl_callback::WlCallback::interface(), 1);
        self.request(
            1,
            wl_display::REQ_SYNC_OPCODE,
            vec![Argument::NewId(callback)],
        );
        self.wait_for("a round trip", |event| event.object == callback);
    }

    fn bind(
        &mut self,
        registry: u32,
        name: u32,
        interface: &'static Interface,
        version: u32,
    ) -> u32 {
        let id = self.create(interface, version);
        self.request(
            registry,
            wl_registry::REQ_BIND_OPCODE,
            vec![
                Argument::Uint(name),
                Argument::Str(Some(Box::new(CString::new(interface.name).expect("name")))),
                Argument::Uint(version),
                Argument::NewId(id),
            ],
        );
        id
    }
}

struct Window {
    compositor: u32,
    surface: u32,
    xdg_surface: u32,
    toplevel: u32,
    wm_base: u32,
}

fn mapped_window(game: &mut Game, helper: &mut Helper) -> Window {
    let (registry, _) = helper.registry();
    let compositor = helper.bind(registry, 1, WlCompositor::interface(), 6);
    let wm_base = helper.bind(registry, 5, XdgWmBase::interface(), 6);
    let shm = helper.bind(registry, 3, WlShm::interface(), 2);
    let surface = helper.create(wl_surface::WlSurface::interface(), 6);
    helper.request(
        compositor,
        wl_compositor::REQ_CREATE_SURFACE_OPCODE,
        vec![Argument::NewId(surface)],
    );
    let xdg_surface = helper.create(xdg_surface::XdgSurface::interface(), 6);
    helper.request(
        wm_base,
        xdg_wm_base::REQ_GET_XDG_SURFACE_OPCODE,
        vec![Argument::NewId(xdg_surface), Argument::Object(surface)],
    );
    let toplevel = helper.create(xdg_toplevel::XdgToplevel::interface(), 6);
    helper.request(
        xdg_surface,
        xdg_surface::REQ_GET_TOPLEVEL_OPCODE,
        vec![Argument::NewId(toplevel)],
    );
    helper.request(surface, wl_surface::REQ_COMMIT_OPCODE, Vec::new());
    let configure = helper.wait_for("the first configure", |event| {
        event.object == xdg_surface && event.message == "configure"
    });
    helper.request(
        xdg_surface,
        xdg_surface::REQ_ACK_CONFIGURE_OPCODE,
        configure.args,
    );
    let memory = rustix::fs::memfd_create("eclipse-embed-test", rustix::fs::MemfdFlags::CLOEXEC)
        .expect("memfd");
    rustix::fs::ftruncate(&memory, 64 * 64 * 4).expect("size the pool");
    let pool = helper.create(wl_shm_pool::WlShmPool::interface(), 2);
    helper.request(
        shm,
        wl_shm::REQ_CREATE_POOL_OPCODE,
        vec![
            Argument::NewId(pool),
            Argument::Fd(memory),
            Argument::Int(64 * 64 * 4),
        ],
    );
    let buffer = helper.create(
        wayland_client::protocol::wl_buffer::WlBuffer::interface(),
        2,
    );
    helper.request(
        pool,
        wl_shm_pool::REQ_CREATE_BUFFER_OPCODE,
        vec![
            Argument::NewId(buffer),
            Argument::Int(0),
            Argument::Int(64),
            Argument::Int(64),
            Argument::Int(256),
            Argument::Uint(wl_shm::Format::Argb8888 as u32),
        ],
    );
    helper.request(
        surface,
        wl_surface::REQ_ATTACH_OPCODE,
        vec![Argument::Object(buffer), Argument::Int(0), Argument::Int(0)],
    );
    helper.request(surface, wl_surface::REQ_COMMIT_OPCODE, Vec::new());
    game.compositor.wait_for("the mapping commit", |seen| {
        seen.interface == "wl_surface" && seen.message == "attach"
    });
    Window {
        compositor,
        surface,
        xdg_surface,
        toplevel,
        wm_base,
    }
}

fn int_args(args: &Args) -> Vec<i32> {
    args.iter()
        .filter_map(|arg| match arg {
            Argument::Int(value) => Some(*value),
            _ => None,
        })
        .collect()
}

#[test]
fn the_helper_sees_only_allowed_globals_and_a_hidden_bind_cuts_it_off_alone() {
    let mut game = Game::start();
    let mut helper = game.helper();
    let (registry, globals) = helper.registry();
    let names: Vec<&str> = globals.iter().map(|(_, name, _)| name.as_str()).collect();
    assert_eq!(
        names,
        [
            "wl_compositor",
            "wl_subcompositor",
            "wl_shm",
            "wl_data_device_manager",
            "xdg_wm_base",
            "wl_seat",
            "wl_output",
            "zwp_linux_dmabuf_v1"
        ],
        "xdg-activation and xdg-foreign stay hidden"
    );
    assert!(
        game.compositor
            .saw("wl_registry", "bind")
            .iter()
            .all(|seen| !matches!(
                seen.args.as_slice(),
                [_, Argument::Str(Some(name)), ..] if name.to_bytes() == b"xdg_wm_base"
            )),
        "the proxy emulates xdg_wm_base instead of binding the game's"
    );

    helper.bind(registry, 7, WlCompositor::interface(), 1);
    let last = helper.disconnected();
    assert!(
        last.iter().any(|event| event.message == "error"),
        "the helper learns why it was cut off: {last:?}"
    );

    let mut next = game.helper();
    let (_, globals) = next.registry();
    assert_eq!(
        globals.len(),
        names.len(),
        "the game's connection survives and serves the next helper"
    );
}

#[test]
fn a_toplevel_becomes_a_subsurface_of_the_game_window_sized_to_it() {
    let mut game = Game::start();
    let mut helper = game.helper();
    let window = mapped_window(&mut game, &mut helper);
    let subsurface = game.compositor.saw("wl_subcompositor", "get_subsurface");
    assert_eq!(subsurface.len(), 1, "one subsurface carries the page");
    let parents = object_args(&subsurface[0].args);
    assert_eq!(
        parents[1],
        game.game_surface(),
        "the page's parent is the game's own surface"
    );
    assert_eq!(game.compositor.saw("wl_subsurface", "set_desync").len(), 1);
    assert!(game
        .compositor
        .saw("xdg_wm_base", "get_xdg_surface")
        .is_empty());

    game.embedder.as_ref().expect("embedder").resize(1200, 800);
    let resized = helper.wait_for("a configure at the new size", |event| {
        event.object == window.toplevel
            && event.message == "configure"
            && int_args(&event.args) == [1200, 800]
    });
    assert_eq!(resized.interface, "xdg_toplevel");
}

#[test]
fn a_popup_flips_inside_the_game_window_and_sits_at_its_absolute_position() {
    let mut game = Game::start();
    let mut helper = game.helper();
    let window = mapped_window(&mut game, &mut helper);
    let positioner = helper.create(xdg_positioner::XdgPositioner::interface(), 6);
    helper.request(
        window.wm_base,
        xdg_wm_base::REQ_CREATE_POSITIONER_OPCODE,
        vec![Argument::NewId(positioner)],
    );
    for (opcode, args) in [
        (
            xdg_positioner::REQ_SET_SIZE_OPCODE,
            vec![Argument::Int(240), Argument::Int(106)],
        ),
        (
            xdg_positioner::REQ_SET_ANCHOR_RECT_OPCODE,
            vec![
                Argument::Int(520),
                Argument::Int(410),
                Argument::Int(240),
                Argument::Int(30),
            ],
        ),
        (
            xdg_positioner::REQ_SET_ANCHOR_OPCODE,
            vec![Argument::Uint(xdg_positioner::Anchor::BottomLeft as u32)],
        ),
        (
            xdg_positioner::REQ_SET_GRAVITY_OPCODE,
            vec![Argument::Uint(xdg_positioner::Gravity::BottomRight as u32)],
        ),
        (
            xdg_positioner::REQ_SET_CONSTRAINT_ADJUSTMENT_OPCODE,
            vec![Argument::Uint(
                (xdg_positioner::ConstraintAdjustment::FlipY
                    | xdg_positioner::ConstraintAdjustment::SlideX)
                    .bits(),
            )],
        ),
    ] {
        helper.request(positioner, opcode, args);
    }
    let surface = helper.create(wl_surface::WlSurface::interface(), 6);
    helper.request(
        window.compositor,
        wl_compositor::REQ_CREATE_SURFACE_OPCODE,
        vec![Argument::NewId(surface)],
    );
    let xdg = helper.create(xdg_surface::XdgSurface::interface(), 6);
    helper.request(
        window.wm_base,
        xdg_wm_base::REQ_GET_XDG_SURFACE_OPCODE,
        vec![Argument::NewId(xdg), Argument::Object(surface)],
    );
    let popup = helper.create(xdg_popup::XdgPopup::interface(), 6);
    helper.request(
        xdg,
        xdg_surface::REQ_GET_POPUP_OPCODE,
        vec![
            Argument::NewId(popup),
            Argument::Object(window.xdg_surface),
            Argument::Object(positioner),
        ],
    );
    helper.request(surface, wl_surface::REQ_COMMIT_OPCODE, Vec::new());
    let configure = helper.wait_for("the popup configure", |event| {
        event.object == popup && event.message == "configure"
    });
    assert_eq!(
        int_args(&configure.args),
        [520, 304, 240, 106],
        "a menu at the bottom edge flips above its field"
    );
    let positions: Vec<Vec<i32>> = game
        .compositor
        .saw("wl_subsurface", "set_position")
        .iter()
        .map(|seen| int_args(&seen.args))
        .collect();
    assert!(
        positions.contains(&vec![520, 304]),
        "the popup's subsurface sits where the game window shows it: {positions:?}"
    );
}

#[test]
fn keys_and_pointers_reach_the_page_and_never_the_game_surfaces_events() {
    let mut game = Game::start();
    let mut helper = game.helper();
    let window = mapped_window(&mut game, &mut helper);
    let (registry, _) = helper.registry();
    let seat = helper.bind(registry, 6, WlSeat::interface(), 9);
    helper.wait_for("the seat's capabilities", |event| event.object == seat);
    let keyboard = helper.create(wl_keyboard::WlKeyboard::interface(), 9);
    helper.request(
        seat,
        wl_seat::REQ_GET_KEYBOARD_OPCODE,
        vec![Argument::NewId(keyboard)],
    );
    let pointer = helper.create(wl_pointer::WlPointer::interface(), 9);
    helper.request(
        seat,
        wl_seat::REQ_GET_POINTER_OPCODE,
        vec![Argument::NewId(pointer)],
    );
    let upstream_keyboard = game.compositor.created("wl_seat", "get_keyboard", 0);
    let upstream_pointer = game.compositor.created("wl_seat", "get_pointer", 0);
    let upstream_surface = game
        .compositor
        .created("wl_compositor", "create_surface", 1);
    let game_surface = game.game_surface();

    game.compositor.send(
        upstream_keyboard,
        wl_keyboard::EVT_ENTER_OPCODE,
        vec![
            Argument::Uint(41),
            Argument::Object(game_surface),
            Argument::Array(Box::default()),
        ],
    );
    let enter = helper.wait_for("keyboard focus on the page", |event| {
        event.object == keyboard && event.message == "enter"
    });
    assert_eq!(
        object_args(&enter.args),
        [window.surface],
        "the game's keyboard focus moves to the page"
    );
    assert!(
        matches!(enter.args[0], Argument::Uint(41)),
        "with the compositor's serial"
    );
    let activated = xdg_toplevel::State::Activated as u32;
    helper.wait_for("the page activated", |event| {
        event.object == window.toplevel
            && event.message == "configure"
            && matches!(event.args.as_slice(), [_, _, Argument::Array(states)]
                if states.chunks(4).any(|state| state == activated.to_ne_bytes()))
    });
    game.compositor.send(
        upstream_keyboard,
        wl_keyboard::EVT_KEY_OPCODE,
        vec![
            Argument::Uint(42),
            Argument::Uint(7),
            Argument::Uint(30),
            Argument::Uint(1),
        ],
    );
    helper.wait_for("the key", |event| {
        event.object == keyboard && event.message == "key"
    });

    game.compositor.send(
        upstream_pointer,
        wl_pointer::EVT_ENTER_OPCODE,
        vec![
            Argument::Uint(43),
            Argument::Object(game_surface),
            Argument::Fixed(0),
            Argument::Fixed(0),
        ],
    );
    game.compositor.send(
        upstream_pointer,
        wl_pointer::EVT_MOTION_OPCODE,
        vec![
            Argument::Uint(8),
            Argument::Fixed(256),
            Argument::Fixed(256),
        ],
    );
    game.compositor
        .send(upstream_pointer, wl_pointer::EVT_FRAME_OPCODE, Vec::new());
    assert!(
        helper.quiet().iter().all(|event| event.object != pointer),
        "the pointer over the game's own surface stays the game's"
    );

    game.compositor.send(
        upstream_pointer,
        wl_pointer::EVT_LEAVE_OPCODE,
        vec![Argument::Uint(44), Argument::Object(game_surface)],
    );
    game.compositor.send(
        upstream_pointer,
        wl_pointer::EVT_ENTER_OPCODE,
        vec![
            Argument::Uint(45),
            Argument::Object(upstream_surface),
            Argument::Fixed(10 * 256),
            Argument::Fixed(20 * 256),
        ],
    );
    game.compositor
        .send(upstream_pointer, wl_pointer::EVT_FRAME_OPCODE, Vec::new());
    let entered = helper.wait_for("the pointer on the page", |event| {
        event.object == pointer && event.message == "enter"
    });
    assert_eq!(object_args(&entered.args), [window.surface]);
    helper.wait_for("the pointer frame", |event| {
        event.object == pointer && event.message == "frame"
    });

    game.compositor.send(
        upstream_keyboard,
        wl_keyboard::EVT_LEAVE_OPCODE,
        vec![Argument::Uint(46), Argument::Object(game_surface)],
    );
    let left = helper.wait_for("the page losing focus", |event| {
        event.object == keyboard && event.message == "leave"
    });
    assert_eq!(object_args(&left.args), [window.surface]);
}

#[test]
fn destroyed_ids_are_returned_and_taken_or_server_range_ids_cut_the_helper_off() {
    let mut game = Game::start();
    let mut helper = game.helper();
    let (registry, _) = helper.registry();
    let compositor = helper.bind(registry, 1, WlCompositor::interface(), 6);
    let region = helper.create(wl_region::WlRegion::interface(), 6);
    helper.request(
        compositor,
        wl_compositor::REQ_CREATE_REGION_OPCODE,
        vec![Argument::NewId(region)],
    );
    helper.request(region, wl_region::REQ_DESTROY_OPCODE, Vec::new());
    let deleted = helper.wait_for("the region's id back", |event| event.message == "delete_id");
    assert!(matches!(deleted.args.as_slice(), [Argument::Uint(id)] if *id == region));
    helper
        .objects
        .insert(region, (wl_region::WlRegion::interface(), 6));
    helper.request(
        compositor,
        wl_compositor::REQ_CREATE_REGION_OPCODE,
        vec![Argument::NewId(region)],
    );
    game.compositor.settle();
    assert_eq!(
        game.compositor.saw("wl_compositor", "create_region").len(),
        2,
        "a returned id may name a new object"
    );

    helper.request(
        compositor,
        wl_compositor::REQ_CREATE_REGION_OPCODE,
        vec![Argument::NewId(region)],
    );
    assert!(helper
        .disconnected()
        .iter()
        .any(|event| event.message == "error"));

    let mut next = game.helper();
    let (registry, _) = next.registry();
    let compositor = next.bind(registry, 1, WlCompositor::interface(), 6);
    next.request(
        compositor,
        wl_compositor::REQ_CREATE_REGION_OPCODE,
        vec![Argument::NewId(0xff00_0001)],
    );
    assert!(next
        .disconnected()
        .iter()
        .any(|event| event.message == "error"));
}

#[test]
fn a_crashed_helper_leaves_nothing_over_the_game() {
    let mut game = Game::start();
    let mut helper = game.helper();
    mapped_window(&mut game, &mut helper);
    let upstream_surface = game
        .compositor
        .created("wl_compositor", "create_surface", 1);
    let upstream_subsurface = game
        .compositor
        .created("wl_subcompositor", "get_subsurface", 0);
    let upstream_pool = game.compositor.created("wl_shm", "create_pool", 0);
    let upstream_buffer = game.compositor.created("wl_shm_pool", "create_buffer", 0);
    drop(helper);
    for (object, interface) in [
        (upstream_subsurface, "wl_subsurface"),
        (upstream_buffer, "wl_buffer"),
        (upstream_pool, "wl_shm_pool"),
        (upstream_surface, "wl_surface"),
    ] {
        game.compositor
            .wait_for(&format!("{interface} destroyed"), |seen| {
                seen.object == object && seen.message == "destroy"
            });
    }
    let destroyed: Vec<u32> = game
        .compositor
        .log
        .iter()
        .filter(|seen| seen.message == "destroy")
        .map(|seen| seen.object)
        .collect();
    let order = |object: u32| destroyed.iter().position(|seen| *seen == object);
    assert!(
        order(upstream_subsurface) < order(upstream_surface),
        "the page leaves the game window before its surface goes: {destroyed:?}"
    );
    let game_surface = game.game_surface();
    assert!(
        game.compositor
            .saw("wl_surface", "destroy")
            .iter()
            .all(|seen| seen.object != game_surface),
        "the game's own surface stays"
    );
}

#[test]
fn a_leave_naming_an_output_the_game_just_released_never_reaches_the_helper() {
    let mut game = Game::start();
    let output = game.bind(9, WlOutput::interface(), 4);
    let mut helper = game.helper();
    let (registry, _) = helper.registry();
    let compositor = helper.bind(registry, 1, WlCompositor::interface(), 6);
    let surface = helper.create(wl_surface::WlSurface::interface(), 6);
    helper.request(
        compositor,
        wl_compositor::REQ_CREATE_SURFACE_OPCODE,
        vec![Argument::NewId(surface)],
    );
    game.compositor
        .created("wl_compositor", "create_surface", 1);

    game_request(
        &game.backend,
        output,
        wl_output::REQ_RELEASE_OPCODE,
        Vec::new(),
        None,
    );
    game.backend.flush().expect("flush the release");
    game.compositor.wait_for("the game's release", |seen| {
        seen.interface == "wl_output" && seen.message == "release"
    });
    helper.roundtrip();
    let surface_events: Vec<&Event> = helper
        .backlog
        .iter()
        .filter(|event| event.object == surface)
        .collect();
    assert!(
        surface_events.is_empty(),
        "libwayland nulls the released output, and a null in leave's non-nullable slot \
         is fatal to the helper: {surface_events:?}"
    );
}

fn surface_with_id(helper: &mut Helper, compositor: u32, id: u32) {
    helper
        .objects
        .insert(id, (wl_surface::WlSurface::interface(), 6));
    helper.request(
        compositor,
        wl_compositor::REQ_CREATE_SURFACE_OPCODE,
        vec![Argument::NewId(id)],
    );
}

fn new_surface(helper: &mut Helper, compositor: u32) -> u32 {
    let id = helper.create(wl_surface::WlSurface::interface(), 6);
    surface_with_id(helper, compositor, id);
    id
}

fn subsurface_of(helper: &mut Helper, subcompositor: u32, surface: u32, parent: u32) -> u32 {
    let subsurface = helper.create(wl_subsurface::WlSubsurface::interface(), 1);
    helper.request(
        subcompositor,
        wl_subcompositor::REQ_GET_SUBSURFACE_OPCODE,
        vec![
            Argument::NewId(subsurface),
            Argument::Object(surface),
            Argument::Object(parent),
        ],
    );
    subsurface
}

#[test]
fn a_subsurface_whose_parent_died_is_no_sibling_of_a_surface_that_reuses_the_parents_id() {
    let mut game = Game::start();
    let mut helper = game.helper();
    let (registry, _) = helper.registry();
    let compositor = helper.bind(registry, 1, WlCompositor::interface(), 6);
    let subcompositor = helper.bind(registry, 2, WlSubcompositor::interface(), 1);
    let parent = new_surface(&mut helper, compositor);
    let orphan = new_surface(&mut helper, compositor);
    subsurface_of(&mut helper, subcompositor, orphan, parent);
    helper.request(parent, wl_surface::REQ_DESTROY_OPCODE, Vec::new());
    helper.wait_for("the parent's id back", |event| {
        event.message == "delete_id"
            && matches!(event.args.as_slice(), [Argument::Uint(id)] if *id == parent)
    });
    surface_with_id(&mut helper, compositor, parent);
    let child = new_surface(&mut helper, compositor);
    let child_subsurface = subsurface_of(&mut helper, subcompositor, child, parent);

    helper.request(
        child_subsurface,
        wl_subsurface::REQ_PLACE_ABOVE_OPCODE,
        vec![Argument::Object(orphan)],
    );
    assert!(
        helper
            .disconnected()
            .iter()
            .any(|event| event.message == "error"),
        "the orphan's parent is gone, so naming it as a sibling cuts off only the helper"
    );
    assert!(
        game.compositor
            .saw("wl_subsurface", "place_above")
            .is_empty(),
        "compositors end the game's whole connection for a sibling that is not one"
    );
}

fn shm_file(size: u64) -> OwnedFd {
    let file = rustix::fs::memfd_create("eclipse-embed-test", rustix::fs::MemfdFlags::CLOEXEC)
        .expect("memfd");
    rustix::fs::ftruncate(&file, size).expect("size the pool file");
    file
}

#[test]
fn a_pool_larger_than_its_file_cuts_off_only_the_helper() {
    let mut game = Game::start();
    let mut short = game.helper();
    let (registry, _) = short.registry();
    let shm = short.bind(registry, 3, WlShm::interface(), 1);
    let pool = short.create(wl_shm_pool::WlShmPool::interface(), 1);
    short.request(
        shm,
        wl_shm::REQ_CREATE_POOL_OPCODE,
        vec![
            Argument::NewId(pool),
            Argument::Fd(shm_file(16)),
            Argument::Int(4096),
        ],
    );
    assert!(short
        .disconnected()
        .iter()
        .any(|event| event.message == "error"));
    assert!(
        game.compositor.saw("wl_shm", "create_pool").is_empty(),
        "compositors end the game's connection for a pool its file cannot hold"
    );

    let mut grown = game.helper();
    let (registry, _) = grown.registry();
    let shm = grown.bind(registry, 3, WlShm::interface(), 1);
    let file = shm_file(4096);
    let pool = grown.create(wl_shm_pool::WlShmPool::interface(), 1);
    grown.request(
        shm,
        wl_shm::REQ_CREATE_POOL_OPCODE,
        vec![
            Argument::NewId(pool),
            Argument::Fd(file.try_clone().expect("dup the pool file")),
            Argument::Int(4096),
        ],
    );
    rustix::fs::ftruncate(&file, 8192).expect("grow the pool file");
    grown.request(
        pool,
        wl_shm_pool::REQ_RESIZE_OPCODE,
        vec![Argument::Int(8192)],
    );
    game.compositor
        .wait_for("the pool grown with its file", |seen| {
            seen.interface == "wl_shm_pool" && seen.message == "resize"
        });
    grown.request(
        pool,
        wl_shm_pool::REQ_RESIZE_OPCODE,
        vec![Argument::Int(16384)],
    );
    assert!(grown
        .disconnected()
        .iter()
        .any(|event| event.message == "error"));
    assert_eq!(
        game.compositor.saw("wl_shm_pool", "resize").len(),
        1,
        "a resize past the end of the file stays with the proxy"
    );
}

#[test]
fn a_buffer_stride_is_checked_with_its_formats_own_pixel_size() {
    let mut game = Game::start();
    let mut helper = game.helper();
    let (registry, _) = helper.registry();
    let shm = helper.bind(registry, 3, WlShm::interface(), 1);
    helper.roundtrip();
    let pool = helper.create(wl_shm_pool::WlShmPool::interface(), 1);
    helper.request(
        shm,
        wl_shm::REQ_CREATE_POOL_OPCODE,
        vec![
            Argument::NewId(pool),
            Argument::Fd(shm_file(64 * 64 * 8)),
            Argument::Int(64 * 64 * 8),
        ],
    );
    let buffer = |helper: &mut Helper, stride: i32| {
        let id = helper.create(
            wayland_client::protocol::wl_buffer::WlBuffer::interface(),
            1,
        );
        helper.request(
            pool,
            wl_shm_pool::REQ_CREATE_BUFFER_OPCODE,
            vec![
                Argument::NewId(id),
                Argument::Int(0),
                Argument::Int(64),
                Argument::Int(64),
                Argument::Int(stride),
                Argument::Uint(wl_shm::Format::Abgr16161616f as u32),
            ],
        );
    };
    buffer(&mut helper, 64 * 8);
    game.compositor
        .wait_for("a half-float buffer with a whole stride", |seen| {
            seen.message == "create_buffer"
        });
    buffer(&mut helper, 64 * 4);
    assert!(helper
        .disconnected()
        .iter()
        .any(|event| event.message == "error"));
    assert_eq!(
        game.compositor.saw("wl_shm_pool", "create_buffer").len(),
        1,
        "eight bytes a pixel need a stride of at least eight times the width"
    );
}

#[test]
fn a_compositor_that_stops_reading_stops_the_proxy_reading_the_helper() {
    const DAMAGE_SIZE: usize = 24;
    const BATCH: usize = 1024;
    const OFFERED: usize = 8 * 1024 * 1024;
    let mut game = Game::start();
    let mut helper = game.helper();
    let (registry, _) = helper.registry();
    let compositor = helper.bind(registry, 1, WlCompositor::interface(), 6);
    let surface = helper.create(wl_surface::WlSurface::interface(), 6);
    helper.request(
        compositor,
        wl_compositor::REQ_CREATE_SURFACE_OPCODE,
        vec![Argument::NewId(surface)],
    );
    game.compositor
        .created("wl_compositor", "create_surface", 1);

    game.compositor.stall(true);
    let mut sent = 0;
    while sent < OFFERED {
        for _ in 0..BATCH {
            helper
                .wire
                .push(
                    surface,
                    wl_surface::REQ_DAMAGE_OPCODE,
                    vec![
                        Argument::Int(0),
                        Argument::Int(0),
                        Argument::Int(1),
                        Argument::Int(1),
                    ],
                )
                .expect("encode a damage request");
        }
        let deadline = Instant::now() + QUIET;
        helper.wire.flush().expect("send damage");
        while helper.wire.wants_write() && Instant::now() < deadline {
            let fd = helper.wire.fd();
            let mut fds = [PollFd::new(&fd, PollFlags::OUT)];
            let _ = poll(&mut fds, Some(&Timespec::try_from(QUIET).expect("timeout")));
            helper.wire.flush().expect("send damage");
        }
        if helper.wire.wants_write() {
            break;
        }
        sent += BATCH * DAMAGE_SIZE;
    }
    game.compositor.stall(false);
    assert!(
        sent < OFFERED / 4,
        "the helper pushed {sent} bytes into the game's unbounded libwayland buffer while the \
         compositor read nothing"
    );
}

fn import_immediately(helper: &mut Helper, dmabuf: u32, surface: u32, format: u32) -> (u32, u32) {
    let params = helper.create(ZwpLinuxBufferParamsV1::interface(), 4);
    helper.request(
        dmabuf,
        zwp_linux_dmabuf_v1::REQ_CREATE_PARAMS_OPCODE,
        vec![Argument::NewId(params)],
    );
    helper.request(
        params,
        zwp_linux_buffer_params_v1::REQ_ADD_OPCODE,
        vec![
            Argument::Fd(shm_file(64 * 64 * 4)),
            Argument::Uint(0),
            Argument::Uint(0),
            Argument::Uint(64 * 4),
            Argument::Uint(0),
            Argument::Uint(0),
        ],
    );
    let buffer = helper.create(
        wayland_client::protocol::wl_buffer::WlBuffer::interface(),
        4,
    );
    helper.request(
        params,
        zwp_linux_buffer_params_v1::REQ_CREATE_IMMED_OPCODE,
        vec![
            Argument::NewId(buffer),
            Argument::Int(64),
            Argument::Int(64),
            Argument::Uint(format),
            Argument::Uint(0),
        ],
    );
    helper.request(
        params,
        zwp_linux_buffer_params_v1::REQ_DESTROY_OPCODE,
        Vec::new(),
    );
    helper.request(
        surface,
        wl_surface::REQ_ATTACH_OPCODE,
        vec![Argument::Object(buffer), Argument::Int(0), Argument::Int(0)],
    );
    helper.request(surface, wl_surface::REQ_COMMIT_OPCODE, Vec::new());
    (params, buffer)
}

#[test]
fn an_immediate_dmabuf_import_waits_for_the_compositor_and_a_failed_one_stays_with_the_proxy() {
    let mut game = Game::start();
    let mut helper = game.helper();
    let (registry, _) = helper.registry();
    let compositor = helper.bind(registry, 1, WlCompositor::interface(), 6);
    let dmabuf = helper.bind(registry, 10, ZwpLinuxDmabufV1::interface(), 4);
    let surface = helper.create(wl_surface::WlSurface::interface(), 6);
    helper.request(
        compositor,
        wl_compositor::REQ_CREATE_SURFACE_OPCODE,
        vec![Argument::NewId(surface)],
    );

    import_immediately(&mut helper, dmabuf, surface, IMPORTABLE_DMABUF);
    let attached = game
        .compositor
        .wait_until("the imported buffer attached", |log| {
            log.iter()
                .find(|seen| seen.message == "attach")
                .map(|seen| object_args(&seen.args))
        });
    assert_eq!(
        attached,
        [FIRST_SERVER_ID],
        "the helper's buffer is the one the compositor created"
    );

    let (params, _) = import_immediately(&mut helper, dmabuf, surface, UNIMPORTABLE_DMABUF);
    helper.wait_for("the failed import", |event| {
        event.object == params && event.message == "failed"
    });
    helper.roundtrip();
    game.compositor.settle();
    let log = &game.compositor.log;
    let requests: Vec<&str> = log.iter().map(|seen| seen.message).collect();
    assert!(
        !requests.contains(&"create_immed"),
        "a failed immediate import may end the game's whole connection: {requests:?}"
    );
    assert_eq!(
        log.iter().filter(|seen| seen.message == "create").count(),
        2
    );
    assert_eq!(
        log.iter().filter(|seen| seen.message == "attach").count(),
        1,
        "the buffer that failed to import never reaches the compositor"
    );
    assert_eq!(
        log.iter().filter(|seen| seen.message == "commit").count(),
        2
    );
}

#[test]
fn bindings_without_destructors_serve_every_helper_and_wl_shm_is_released() {
    let mut game = Game::start();
    for _ in 0..2 {
        let mut helper = game.helper();
        let (registry, _) = helper.registry();
        let compositor = helper.bind(registry, 1, WlCompositor::interface(), 6);
        helper.bind(registry, 4, WlDataDeviceManager::interface(), 3);
        helper.bind(registry, 3, WlShm::interface(), 1);
        let surface = helper.create(wl_surface::WlSurface::interface(), 6);
        helper.request(
            compositor,
            wl_compositor::REQ_CREATE_SURFACE_OPCODE,
            vec![Argument::NewId(surface)],
        );
        helper.roundtrip();
        drop(helper);
    }
    game.compositor
        .wait_until("both helpers' wl_shm released", |log| {
            (log.iter()
                .filter(|seen| seen.interface == "wl_shm" && seen.message == "release")
                .count()
                == 2)
                .then_some(())
        });
    let binds: Vec<(String, u32)> = game
        .compositor
        .saw("wl_registry", "bind")
        .iter()
        .filter_map(|seen| match seen.args.as_slice() {
            [_, Argument::Str(Some(name)), Argument::Uint(version), _] => {
                Some((name.to_string_lossy().into_owned(), *version))
            }
            _ => None,
        })
        .collect();
    let count = |name: &str, version: u32| {
        binds
            .iter()
            .filter(|bound| bound.0 == name && bound.1 == version)
            .count()
    };
    assert_eq!(
        count("wl_compositor", 6),
        1,
        "the compositor keeps every binding without a destructor until the game exits: {binds:?}"
    );
    assert_eq!(count("wl_data_device_manager", 3), 1, "{binds:?}");
    assert_eq!(
        count("wl_shm", 2),
        2,
        "wl_shm 2 only adds release, so the proxy binds it to give it back: {binds:?}"
    );
    assert_eq!(
        game.compositor.saw("wl_compositor", "create_surface").len(),
        3,
        "the game's surface and one page surface per helper"
    );
}

#[test]
fn a_subsurface_whose_parent_died_cannot_become_a_parent() {
    let mut game = Game::start();
    let mut helper = game.helper();
    let (registry, _) = helper.registry();
    let compositor = helper.bind(registry, 1, WlCompositor::interface(), 6);
    let subcompositor = helper.bind(registry, 2, WlSubcompositor::interface(), 1);
    let grandparent = new_surface(&mut helper, compositor);
    let orphan = new_surface(&mut helper, compositor);
    let child = new_surface(&mut helper, compositor);
    subsurface_of(&mut helper, subcompositor, orphan, grandparent);
    helper.request(grandparent, wl_surface::REQ_DESTROY_OPCODE, Vec::new());

    subsurface_of(&mut helper, subcompositor, child, orphan);
    assert!(helper
        .disconnected()
        .iter()
        .any(|event| event.message == "error"));
    assert_eq!(
        game.compositor
            .saw("wl_subcompositor", "get_subsurface")
            .len(),
        1,
        "Hyprland walks a new subsurface's parents without checking that they are alive"
    );
}
