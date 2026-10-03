mod dmabuf;
mod input;
mod shell;

use std::collections::{BTreeMap, HashMap, HashSet};
use std::ffi::CString;
use std::fmt;
use std::io;
use std::os::fd::{AsFd, AsRawFd, BorrowedFd, OwnedFd};
use std::os::unix::net::UnixStream;

use rustix::fs::{FileType, OFlags, SealFlags};
use wayland_client::backend::protocol::{
    AllowNull, Argument, ArgumentType, Interface, Message, MessageDesc,
};
use wayland_client::backend::ObjectId;
use wayland_client::protocol::{
    wl_buffer::WlBuffer, wl_callback::WlCallback, wl_data_device, wl_data_device::WlDataDevice,
    wl_display, wl_display::WlDisplay, wl_keyboard::WlKeyboard, wl_pointer, wl_pointer::WlPointer,
    wl_registry, wl_registry::WlRegistry, wl_seat, wl_seat::WlSeat, wl_shm, wl_shm::WlShm,
    wl_shm_pool, wl_shm_pool::WlShmPool, wl_subcompositor, wl_subcompositor::WlSubcompositor,
    wl_subsurface, wl_subsurface::WlSubsurface, wl_surface, wl_surface::WlSurface,
    wl_touch::WlTouch,
};
use wayland_client::Proxy;
use wayland_protocols::wp::commit_timing::v1::client::wp_commit_timing_manager_v1::{
    self, WpCommitTimingManagerV1,
};
use wayland_protocols::wp::cursor_shape::v1::client::wp_cursor_shape_device_v1::{
    self, WpCursorShapeDeviceV1,
};
use wayland_protocols::wp::fifo::v1::client::wp_fifo_manager_v1::{self, WpFifoManagerV1};
use wayland_protocols::wp::fractional_scale::v1::client::wp_fractional_scale_manager_v1::{
    self, WpFractionalScaleManagerV1,
};
use wayland_protocols::wp::linux_dmabuf::zv1::client::{
    zwp_linux_buffer_params_v1::{self, ZwpLinuxBufferParamsV1},
    zwp_linux_dmabuf_feedback_v1::{self, ZwpLinuxDmabufFeedbackV1},
};
use wayland_protocols::wp::linux_drm_syncobj::v1::client::{
    wp_linux_drm_syncobj_manager_v1::{self, WpLinuxDrmSyncobjManagerV1},
    wp_linux_drm_syncobj_surface_v1::{self, WpLinuxDrmSyncobjSurfaceV1},
};
use wayland_protocols::wp::pointer_gestures::zv1::client::{
    zwp_pointer_gesture_hold_v1::{self, ZwpPointerGestureHoldV1},
    zwp_pointer_gesture_pinch_v1::{self, ZwpPointerGesturePinchV1},
    zwp_pointer_gesture_swipe_v1::{self, ZwpPointerGestureSwipeV1},
};
use wayland_protocols::wp::single_pixel_buffer::v1::client::wp_single_pixel_buffer_manager_v1::{
    self, WpSinglePixelBufferManagerV1,
};
use wayland_protocols::wp::text_input::zv3::client::zwp_text_input_v3::{self, ZwpTextInputV3};
use wayland_protocols::wp::viewporter::client::{
    wp_viewport::{self, WpViewport},
    wp_viewporter::{self, WpViewporter},
};

use super::globals::{Global, Provision};
use super::wire::{Args, Message as Request, Wire, WireError};
use super::{outgoing, RegistryChange, Upstream};

const DISPLAY_ID: u32 = 1;

const SERVER_ID_START: u32 = 0xff00_0000;

const OBJECT_LIMIT: usize = 16 * 1024;

const SHM_FORMAT_LIMIT: usize = 256;

const ALWAYS_SUPPORTED_SHM_FORMATS: [u32; 2] = [0, 1];

const POOL_LIMIT: usize = 64;

#[derive(Debug)]
pub(super) struct Violation {
    object: u32,
    interface: &'static str,
    request: &'static str,
    reason: &'static str,
}

impl fmt::Display for Violation {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(
            f,
            "{}.{} on object {}: {}",
            self.interface, self.request, self.object, self.reason
        )
    }
}

#[derive(Debug)]
pub(super) enum SessionEnd {
    Wire(WireError),
    Violation(Violation),
}

impl fmt::Display for SessionEnd {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Wire(error) => error.fmt(f),
            Self::Violation(violation) => write!(f, "the helper broke the protocol: {violation}"),
        }
    }
}

impl From<Violation> for SessionEnd {
    fn from(violation: Violation) -> Self {
        Self::Violation(violation)
    }
}

impl From<WireError> for SessionEnd {
    fn from(error: WireError) -> Self {
        Self::Wire(error)
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum Class {
    Other,
    Surface,
    Subcompositor,
    Subsurface,
    Seat,
    Pointer,
    Keyboard,
    Touch,
    TextInput,
    DataDevice,
    Gesture { end: u16 },
    Shm,
    ShmPool,
    Viewport,
    CursorShape,
    BufferParams,
    DmabufFeedback,
    Buffer,
}

fn class_of(interface: &Interface) -> Class {
    let classes = [
        (WlSurface::interface(), Class::Surface),
        (WlSubcompositor::interface(), Class::Subcompositor),
        (WlSubsurface::interface(), Class::Subsurface),
        (WlSeat::interface(), Class::Seat),
        (WlPointer::interface(), Class::Pointer),
        (WlKeyboard::interface(), Class::Keyboard),
        (WlTouch::interface(), Class::Touch),
        (ZwpTextInputV3::interface(), Class::TextInput),
        (WlDataDevice::interface(), Class::DataDevice),
        (
            ZwpPointerGestureSwipeV1::interface(),
            Class::Gesture {
                end: zwp_pointer_gesture_swipe_v1::EVT_END_OPCODE,
            },
        ),
        (
            ZwpPointerGesturePinchV1::interface(),
            Class::Gesture {
                end: zwp_pointer_gesture_pinch_v1::EVT_END_OPCODE,
            },
        ),
        (
            ZwpPointerGestureHoldV1::interface(),
            Class::Gesture {
                end: zwp_pointer_gesture_hold_v1::EVT_END_OPCODE,
            },
        ),
        (WlShm::interface(), Class::Shm),
        (WlShmPool::interface(), Class::ShmPool),
        (WpViewport::interface(), Class::Viewport),
        (WpCursorShapeDeviceV1::interface(), Class::CursorShape),
        (ZwpLinuxBufferParamsV1::interface(), Class::BufferParams),
        (ZwpLinuxDmabufFeedbackV1::interface(), Class::DmabufFeedback),
        (WlBuffer::interface(), Class::Buffer),
    ];
    classes
        .into_iter()
        .find(|(known, _)| known.name == interface.name)
        .map_or(Class::Other, |(_, class)| class)
}

fn creates_surface_extension(manager: &Interface, opcode: u16) -> bool {
    let constructors = [
        (
            WpViewporter::interface(),
            wp_viewporter::REQ_GET_VIEWPORT_OPCODE,
        ),
        (
            WpFractionalScaleManagerV1::interface(),
            wp_fractional_scale_manager_v1::REQ_GET_FRACTIONAL_SCALE_OPCODE,
        ),
        (
            WpLinuxDrmSyncobjManagerV1::interface(),
            wp_linux_drm_syncobj_manager_v1::REQ_GET_SURFACE_OPCODE,
        ),
        (
            WpFifoManagerV1::interface(),
            wp_fifo_manager_v1::REQ_GET_FIFO_OPCODE,
        ),
        (
            WpCommitTimingManagerV1::interface(),
            wp_commit_timing_manager_v1::REQ_GET_TIMER_OPCODE,
        ),
    ];
    constructors
        .into_iter()
        .any(|(interface, constructor)| interface.name == manager.name && constructor == opcode)
}

fn completes_a_commit(interface: &Interface, opcode: u16) -> bool {
    let requests = [
        (WlSurface::interface(), wl_surface::REQ_ATTACH_OPCODE),
        (
            WpLinuxDrmSyncobjSurfaceV1::interface(),
            wp_linux_drm_syncobj_surface_v1::REQ_SET_ACQUIRE_POINT_OPCODE,
        ),
        (
            WpLinuxDrmSyncobjSurfaceV1::interface(),
            wp_linux_drm_syncobj_surface_v1::REQ_SET_RELEASE_POINT_OPCODE,
        ),
    ];
    requests
        .into_iter()
        .any(|(known, request)| known.name == interface.name && request == opcode)
}

enum Kind {
    Display,
    Registry,
    Inert,
    Shell(shell::Role),
    Upstream { id: ObjectId, class: Class },
}

struct Object {
    interface: &'static Interface,
    version: u32,
    instance: u64,
    kind: Kind,
}

#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
enum SurfaceRole {
    #[default]
    None,
    Xdg,
    Subsurface,
    Cursor,
    DragIcon,
}

struct Surface {
    instance: u64,
    role: SurfaceRole,
    xdg_surface: Option<u32>,
    parent: Option<SurfaceLink>,
    synchronized: bool,
    attached: Option<bool>,
    content: Content,
}

#[derive(Clone, Copy)]
struct Content {
    buffer: Option<(i32, i32)>,
    scale: i32,
    transform: i32,
    source: Option<[i32; 4]>,
    destination: bool,
}

impl Default for Content {
    fn default() -> Self {
        Self {
            buffer: None,
            scale: 1,
            transform: 0,
            source: None,
            destination: false,
        }
    }
}

impl Content {
    fn refusal(&self, role: SurfaceRole) -> Option<&'static str> {
        const FIXED_ONE: i64 = 256;
        let whole = |fixed: i32| i64::from(fixed) % FIXED_ONE == 0;
        if let Some([_, _, width, height]) = self.source {
            if !self.destination && !(whole(width) && whole(height)) {
                return Some("a fractional viewport source without a destination size");
            }
        }
        let (width, height) = self.buffer?;
        let scale = self.scale.max(1);
        let Some([x, y, source_width, source_height]) = self.source else {
            let checked = !matches!(role, SurfaceRole::None | SurfaceRole::Cursor);
            let divisible = width % scale == 0 && height % scale == 0;
            return (checked && !divisible)
                .then_some("a buffer size that is not a multiple of the buffer scale");
        };
        let (mut surface_width, mut surface_height) = (width / scale, height / scale);
        if self.transform % 2 == 1 {
            std::mem::swap(&mut surface_width, &mut surface_height);
        }
        let fits = |start: i32, length: i32, limit: i32| {
            i64::from(start) + i64::from(length) <= i64::from(limit) * FIXED_ONE
        };
        let inside = fits(x, source_width, surface_width)
            && fits(y, source_height, surface_height)
            && fits(x, source_width, width)
            && fits(y, source_height, height);
        (!inside).then_some("a viewport source outside the buffer")
    }
}

#[derive(Clone, Copy)]
struct SurfaceLink {
    surface: u32,
    instance: u64,
}

struct Pool {
    size: i32,
    file: OwnedFd,
}

#[derive(Clone, Copy)]
struct ImmediateBuffer {
    params: u32,
    buffer: u32,
    version: u32,
}

#[derive(Default)]
struct ServerIds {
    next: u32,
    free: Vec<u32>,
}

impl ServerIds {
    fn allocate(&mut self) -> Option<u32> {
        if let Some(id) = self.free.pop() {
            return Some(id);
        }
        let id = SERVER_ID_START.checked_add(self.next)?;
        self.next += 1;
        Some(id)
    }

    fn release(&mut self, id: u32) {
        self.free.push(id);
    }
}

pub(super) struct Session {
    wire: Wire,
    failed: Option<WireError>,
    objects: HashMap<u32, Object>,
    upstream: HashMap<ObjectId, u32>,
    owned: BTreeMap<u64, ObjectId>,
    instances: u64,
    server_ids: ServerIds,
    registries: Vec<u32>,
    size: Option<(i32, i32)>,
    surfaces: HashMap<u32, Surface>,
    subsurfaces: HashMap<u32, SurfaceLink>,
    seats: HashMap<u32, u32>,
    shm_formats: HashSet<u32>,
    pools: HashMap<u32, Pool>,
    buffers: HashMap<u32, (i32, i32)>,
    dmabuf: dmabuf::Dmabuf,
    extensions: HashMap<u32, SurfaceLink>,
    exclusive: HashSet<(&'static str, u64)>,
    awaiting: Option<ImmediateBuffer>,
    shell: shell::Shell,
    input: input::Input,
}

impl Session {
    pub(super) fn new(stream: UnixStream, size: Option<(i32, i32)>) -> io::Result<Self> {
        let mut objects = HashMap::new();
        objects.insert(
            DISPLAY_ID,
            Object {
                interface: WlDisplay::interface(),
                version: 1,
                instance: 0,
                kind: Kind::Display,
            },
        );
        Ok(Self {
            wire: Wire::requests(stream)?,
            failed: None,
            objects,
            upstream: HashMap::new(),
            owned: BTreeMap::new(),
            instances: 1,
            server_ids: ServerIds::default(),
            registries: Vec::new(),
            size,
            surfaces: HashMap::new(),
            subsurfaces: HashMap::new(),
            seats: HashMap::new(),
            shm_formats: HashSet::new(),
            pools: HashMap::new(),
            buffers: HashMap::new(),
            dmabuf: dmabuf::Dmabuf::default(),
            extensions: HashMap::new(),
            exclusive: HashSet::new(),
            awaiting: None,
            shell: shell::Shell::default(),
            input: input::Input::default(),
        })
    }

    pub(super) fn fd(&self) -> BorrowedFd<'_> {
        self.wire.fd()
    }

    pub(super) fn wants_write(&self) -> bool {
        self.wire.wants_write()
    }

    pub(super) fn wants_read(&self) -> bool {
        self.awaiting.is_none()
    }

    pub(super) fn flush(&mut self) -> Result<(), WireError> {
        self.wire.flush()
    }

    pub(super) fn health(&mut self) -> Result<(), WireError> {
        self.failed.take().map_or(Ok(()), Err)
    }

    fn send(&mut self, object: u32, opcode: u16, args: Args) {
        if self.failed.is_some() {
            return;
        }
        if let Err(error) = self.wire.push(object, opcode, args) {
            self.failed = Some(error);
        }
    }

    pub(super) fn serve(&mut self, up: &Upstream) -> Result<(), SessionEnd> {
        self.wire.receive()?;
        self.process(up)
    }

    pub(super) fn process(&mut self, up: &Upstream) -> Result<(), SessionEnd> {
        while self.wants_read() {
            let Some(header) = self.wire.header()? else {
                break;
            };
            let Some(object) = self.objects.get(&header.object) else {
                return Err(Violation {
                    object: header.object,
                    interface: "unknown",
                    request: "unknown",
                    reason: "a request to an object that does not exist",
                }
                .into());
            };
            let (interface, version) = (object.interface, object.version);
            let Some(desc) = interface.requests.get(usize::from(header.opcode)) else {
                return Err(Violation {
                    object: header.object,
                    interface: interface.name,
                    request: "unknown",
                    reason: "an opcode the interface does not have",
                }
                .into());
            };
            if desc.since > version {
                return Err(
                    violation(header.object, interface, desc, "newer than the object").into(),
                );
            }
            let request = self.wire.decode(header, desc.signature)?;
            self.request(up, request, interface, desc)?;
            if let Some(error) = self.failed.take() {
                return Err(error.into());
            }
        }
        Ok(())
    }

    fn next_instance(&mut self) -> u64 {
        self.instances += 1;
        self.instances
    }

    fn adopt(
        &mut self,
        id: u32,
        interface: &'static Interface,
        version: u32,
        kind: Kind,
    ) -> Result<(), &'static str> {
        if self.objects.len() >= OBJECT_LIMIT {
            return Err("the helper made too many objects");
        }
        let instance = self.next_instance();
        if let Kind::Upstream {
            id: upstream,
            class,
        } = &kind
        {
            self.upstream.insert(upstream.clone(), id);
            self.owned.insert(instance, upstream.clone());
            match class {
                Class::Surface => {
                    self.surfaces.insert(
                        id,
                        Surface {
                            instance,
                            role: SurfaceRole::None,
                            xdg_surface: None,
                            parent: None,
                            synchronized: false,
                            attached: None,
                            content: Content::default(),
                        },
                    );
                }
                Class::Seat => {
                    self.seats.insert(id, 0);
                }
                class => self.input.adopt(id, *class),
            }
        }
        self.objects.insert(
            id,
            Object {
                interface,
                version,
                instance,
                kind,
            },
        );
        Ok(())
    }

    fn forget(&mut self, id: u32) {
        let Some(object) = self.objects.remove(&id) else {
            return;
        };
        match object.kind {
            Kind::Upstream {
                id: upstream,
                class,
            } => {
                self.upstream.remove(&upstream);
                self.owned.remove(&object.instance);
                match class {
                    Class::Surface => {
                        self.surfaces.remove(&id);
                        self.input.surface_gone(id);
                    }
                    Class::Subsurface => {
                        if let Some(link) = self.subsurfaces.remove(&id) {
                            if let Some(state) = self.surfaces.get_mut(&link.surface) {
                                if state.instance == link.instance {
                                    state.parent = None;
                                }
                            }
                        }
                    }
                    Class::Seat => {
                        self.seats.remove(&id);
                    }
                    Class::ShmPool => {
                        self.pools.remove(&id);
                    }
                    Class::Buffer => {
                        self.buffers.remove(&id);
                    }
                    Class::BufferParams | Class::DmabufFeedback => self.dmabuf.forget(id),
                    class => self.input.forget(id, class),
                }
                if let Some(link) = self.extensions.remove(&id) {
                    self.exclusive
                        .remove(&(object.interface.name, link.instance));
                    if class == Class::Viewport {
                        if let Some(content) = self.live_content(link) {
                            content.source = None;
                            content.destination = false;
                        }
                    }
                }
            }
            Kind::Registry => self.registries.retain(|registry| *registry != id),
            Kind::Display | Kind::Inert | Kind::Shell(_) => {}
        }
        if id >= SERVER_ID_START {
            self.server_ids.release(id);
        } else {
            self.send(
                DISPLAY_ID,
                wl_display::EVT_DELETE_ID_OPCODE,
                vec![Argument::Uint(id)],
            );
        }
    }

    fn request(
        &mut self,
        up: &Upstream,
        request: Request,
        interface: &'static Interface,
        desc: &'static MessageDesc,
    ) -> Result<(), Violation> {
        self.check_arguments(&request, interface, desc)?;
        let kind = match &self.objects[&request.object].kind {
            Kind::Display => return self.display_request(up, request, desc),
            Kind::Registry => return self.registry_request(up, request, desc),
            Kind::Inert => return self.inert_request(request, interface, desc),
            Kind::Shell(role) => return self.shell_request(up, *role, request, desc),
            Kind::Upstream { id, class } => (id.clone(), *class),
        };
        self.upstream_request(up, kind, request, interface, desc)
    }

    fn check_arguments(
        &self,
        request: &Request,
        interface: &'static Interface,
        desc: &'static MessageDesc,
    ) -> Result<(), Violation> {
        let fail = |reason| violation(request.object, interface, desc, reason);
        let mut expected = desc.arg_interfaces.iter();
        for (arg, kind) in request.args.iter().zip(desc.signature) {
            match (arg, kind) {
                (Argument::Object(0), ArgumentType::Object(AllowNull::Yes)) => {
                    expected.next();
                }
                (Argument::Object(0), _) => {
                    return Err(fail("a null object where one is required"))
                }
                (Argument::Object(id), _) => {
                    let wanted = expected.next().ok_or_else(|| fail("an untyped object"))?;
                    let found = self
                        .objects
                        .get(id)
                        .ok_or_else(|| fail("an object argument that does not exist"))?;
                    if found.interface.name != wanted.name {
                        return Err(fail("an object argument of the wrong interface"));
                    }
                }
                (Argument::NewId(id), _)
                    if *id >= SERVER_ID_START || self.objects.contains_key(id) =>
                {
                    return Err(fail("a new id that is taken or outside the client range"));
                }
                _ => {}
            }
        }
        Ok(())
    }

    fn display_request(
        &mut self,
        up: &Upstream,
        request: Request,
        desc: &'static MessageDesc,
    ) -> Result<(), Violation> {
        let interface = WlDisplay::interface();
        let fail = |reason| violation(DISPLAY_ID, interface, desc, reason);
        let [Argument::NewId(id)] = request.args.as_slice() else {
            return Err(fail("unexpected arguments"));
        };
        let id = *id;
        match request.opcode {
            wl_display::REQ_SYNC_OPCODE => {
                let callback = up.sync().map_err(|_| fail("the game's display is gone"))?;
                self.adopt(
                    id,
                    WlCallback::interface(),
                    1,
                    Kind::Upstream {
                        id: callback.clone(),
                        class: Class::Other,
                    },
                )
                .map_err(|reason| {
                    up.destroy(&callback);
                    fail(reason)
                })
            }
            wl_display::REQ_GET_REGISTRY_OPCODE => {
                self.adopt(id, WlRegistry::interface(), 1, Kind::Registry)
                    .map_err(fail)?;
                self.registries.push(id);
                for global in up.globals().to_vec() {
                    self.announce(id, global);
                }
                Ok(())
            }
            _ => Err(fail("an unknown display request")),
        }
    }

    fn announce(&mut self, registry: u32, global: Global) {
        let Ok(name) = CString::new(global.interface.name) else {
            return;
        };
        self.send(
            registry,
            wl_registry::EVT_GLOBAL_OPCODE,
            vec![
                Argument::Uint(global.name),
                Argument::Str(Some(Box::new(name))),
                Argument::Uint(global.version),
            ],
        );
    }

    pub(super) fn registry_changed(&mut self, change: &RegistryChange) {
        for registry in self.registries.clone() {
            match change {
                RegistryChange::Added(global) => self.announce(registry, *global),
                RegistryChange::Removed(name) => self.send(
                    registry,
                    wl_registry::EVT_GLOBAL_REMOVE_OPCODE,
                    vec![Argument::Uint(*name)],
                ),
            }
        }
    }

    fn registry_request(
        &mut self,
        up: &Upstream,
        request: Request,
        desc: &'static MessageDesc,
    ) -> Result<(), Violation> {
        let interface = WlRegistry::interface();
        let fail = |reason| violation(request.object, interface, desc, reason);
        let [Argument::Uint(name), Argument::Str(Some(asked)), Argument::Uint(version), Argument::NewId(id)] =
            request.args.as_slice()
        else {
            return Err(fail("unexpected arguments"));
        };
        let (live, global) = match (up.global(*name), up.removed_global(*name)) {
            (Some(global), _) => (true, global),
            (None, Some(global)) => (false, global),
            (None, None) => return Err(fail("a bind to a global that was never offered")),
        };
        if global.interface.name.as_bytes() != asked.as_bytes() {
            return Err(fail("a bind that names the wrong interface"));
        }
        if *version == 0 || *version > global.version {
            return Err(fail("a bind above the offered version"));
        }
        let kind = match (live, global.provision) {
            (false, _) => Kind::Inert,
            (true, Provision::Emulated) => Kind::Shell(shell::Role::WmBase),
            (true, Provision::Forwarded) => {
                let bound = up
                    .bind_for_helper(global, *version)
                    .map_err(|_| fail("the game's registry is gone"))?;
                Kind::Upstream {
                    id: bound,
                    class: class_of(global.interface),
                }
            }
        };
        let bound = match &kind {
            Kind::Upstream { id, .. } => Some(id.clone()),
            _ => None,
        };
        self.adopt(*id, global.interface, *version, kind)
            .map_err(|reason| {
                if let Some(bound) = &bound {
                    up.destroy(bound);
                }
                fail(reason)
            })
    }

    fn inert_request(
        &mut self,
        request: Request,
        interface: &'static Interface,
        desc: &'static MessageDesc,
    ) -> Result<(), Violation> {
        let version = self.objects[&request.object].version;
        for arg in &request.args {
            if let Argument::NewId(id) = arg {
                let child = desc.child_interface.ok_or_else(|| {
                    violation(request.object, interface, desc, "an untyped object")
                })?;
                self.adopt(*id, child, version, Kind::Inert)
                    .map_err(|reason| violation(request.object, interface, desc, reason))?;
            }
        }
        if desc.is_destructor {
            self.forget(request.object);
        }
        Ok(())
    }

    fn names_inert_object(&self, request: &Request) -> bool {
        request.args.iter().any(|arg| {
            matches!(arg, Argument::Object(id) if matches!(self.objects.get(id).map(|object| &object.kind), Some(Kind::Inert)))
        })
    }

    fn upstream_request(
        &mut self,
        up: &Upstream,
        (sender, class): (ObjectId, Class),
        mut request: Request,
        interface: &'static Interface,
        desc: &'static MessageDesc,
    ) -> Result<(), Violation> {
        if self.names_inert_object(&request) {
            if completes_a_commit(interface, request.opcode) {
                return Err(violation(
                    request.object,
                    interface,
                    desc,
                    "a buffer or timeline the game's connection does not hold",
                ));
            }
            return self.inert_request(request, interface, desc);
        }
        self.check_request(class, &request, interface, desc)?;
        if class == Class::BufferParams {
            let version = self.objects[&request.object].version;
            self.buffer_params_request(version, &request)
                .map_err(|reason| violation(request.object, interface, desc, reason))?;
        }
        if class == Class::BufferParams
            && request.opcode == zwp_linux_buffer_params_v1::REQ_CREATE_IMMED_OPCODE
        {
            return self.create_buffer_awaited(up, sender, &request, interface, desc);
        }
        self.rewrite_request(class, &mut request);
        if class == Class::Surface && desc.is_destructor {
            self.surface_destroying(up, request.object);
        }
        let version = self.objects[&request.object].version;
        let fail = |reason| violation(request.object, interface, desc, reason);
        let mut new_id = None;
        let mut args = Vec::with_capacity(request.args.len());
        for arg in &request.args {
            args.push(match arg {
                Argument::Int(value) => Argument::Int(*value),
                Argument::Uint(value) => Argument::Uint(*value),
                Argument::Fixed(value) => Argument::Fixed(*value),
                Argument::Str(text) => Argument::Str(text.clone()),
                Argument::Array(bytes) => Argument::Array(bytes.clone()),
                Argument::Object(0) => Argument::Object(ObjectId::null()),
                Argument::Object(id) => Argument::Object(
                    self.upstream_id(*id)
                        .ok_or_else(|| fail("an object that only the proxy knows"))?,
                ),
                Argument::NewId(id) => {
                    new_id = Some(*id);
                    Argument::NewId(ObjectId::null())
                }
                Argument::Fd(fd) => Argument::Fd(fd.as_raw_fd()),
            });
        }
        let created = up
            .send(outgoing(sender, request.opcode, args), new_id.is_some())
            .map_err(|_| fail("the object is gone from the game's connection"))?;
        if let Some(id) = new_id {
            let child = desc
                .child_interface
                .ok_or_else(|| fail("an untyped object"))?;
            let kind = Kind::Upstream {
                id: created.clone(),
                class: class_of(child),
            };
            if let Err(reason) = self.adopt(id, child, version, kind) {
                up.destroy(&created);
                return Err(fail(reason));
            }
        }
        let object = request.object;
        self.after_request(up, class, interface, request, desc);
        if desc.is_destructor {
            self.forget(object);
        }
        Ok(())
    }

    fn create_buffer_awaited(
        &mut self,
        up: &Upstream,
        params: ObjectId,
        request: &Request,
        interface: &'static Interface,
        desc: &'static MessageDesc,
    ) -> Result<(), Violation> {
        let fail = |reason| violation(request.object, interface, desc, reason);
        let [Argument::NewId(buffer), Argument::Int(width), Argument::Int(height), Argument::Uint(format), Argument::Uint(flags)] =
            request.args.as_slice()
        else {
            return Err(fail("unexpected arguments"));
        };
        up.send(
            outgoing(
                params,
                zwp_linux_buffer_params_v1::REQ_CREATE_OPCODE,
                vec![
                    Argument::Int(*width),
                    Argument::Int(*height),
                    Argument::Uint(*format),
                    Argument::Uint(*flags),
                ],
            ),
            false,
        )
        .map_err(|_| fail("the object is gone from the game's connection"))?;
        self.awaiting = Some(ImmediateBuffer {
            params: request.object,
            buffer: *buffer,
            version: self.objects[&request.object].version,
        });
        Ok(())
    }

    fn buffer_answered(
        &mut self,
        up: &Upstream,
        awaited: ImmediateBuffer,
        event: Message<ObjectId, OwnedFd>,
    ) {
        let created = event.args.into_iter().find_map(|arg| match arg {
            Argument::NewId(buffer) if !buffer.is_null() => Some(buffer),
            _ => None,
        });
        let adopted = match created {
            Some(buffer) => {
                let kind = Kind::Upstream {
                    id: buffer.clone(),
                    class: Class::Buffer,
                };
                self.adopt(awaited.buffer, WlBuffer::interface(), awaited.version, kind)
                    .inspect_err(|_| up.destroy(&buffer))
            }
            None => Err("the compositor could not import a dmabuf the helper made"),
        };
        match adopted {
            Ok(()) => self.remember_dmabuf_size(awaited.params, awaited.buffer),
            Err(reason) => {
                self.failed.get_or_insert(WireError::Backlog(reason));
            }
        }
    }

    fn remember_dmabuf_size(&mut self, params: u32, buffer: u32) {
        if let Some(size) = self.dmabuf.requested_size(params) {
            self.buffers.insert(buffer, size);
        }
    }

    fn buffer_params_request(
        &mut self,
        version: u32,
        request: &Request,
    ) -> Result<(), &'static str> {
        let params = request.object;
        let creation = |width: i32, height: i32, format: u32, flags: u32| dmabuf::Creation {
            version,
            width,
            height,
            format,
            flags,
        };
        match (request.opcode, request.args.as_slice()) {
            (
                zwp_linux_buffer_params_v1::REQ_ADD_OPCODE,
                [Argument::Fd(file), Argument::Uint(plane), Argument::Uint(offset), Argument::Uint(stride), Argument::Uint(high), Argument::Uint(low)],
            ) => self.dmabuf.add(
                params,
                file.as_fd(),
                *plane,
                (*offset, *stride),
                (u64::from(*high) << 32) | u64::from(*low),
            ),
            (
                zwp_linux_buffer_params_v1::REQ_CREATE_OPCODE,
                [Argument::Int(width), Argument::Int(height), Argument::Uint(format), Argument::Uint(flags)],
            )
            | (
                zwp_linux_buffer_params_v1::REQ_CREATE_IMMED_OPCODE,
                [_, Argument::Int(width), Argument::Int(height), Argument::Uint(format), Argument::Uint(flags)],
            ) => self
                .dmabuf
                .create(params, &creation(*width, *height, *format, *flags)),
            _ => Ok(()),
        }
    }

    fn upstream_id(&self, id: u32) -> Option<ObjectId> {
        match &self.objects.get(&id)?.kind {
            Kind::Upstream { id, .. } => Some(id.clone()),
            _ => None,
        }
    }

    fn surface_role(&self, surface: u32) -> Option<SurfaceRole> {
        self.surfaces.get(&surface).map(|state| state.role)
    }

    fn surface_alive(&self, link: SurfaceLink) -> bool {
        self.surfaces
            .get(&link.surface)
            .is_some_and(|surface| surface.instance == link.instance)
    }

    fn link(&self, surface: u32) -> Option<SurfaceLink> {
        let instance = self.surfaces.get(&surface)?.instance;
        Some(SurfaceLink { surface, instance })
    }

    fn live_parent(&self, surface: u32) -> Option<u32> {
        let parent = self.surfaces.get(&surface)?.parent?;
        self.surface_alive(parent).then_some(parent.surface)
    }

    fn live_content(&mut self, link: SurfaceLink) -> Option<&mut Content> {
        self.surfaces
            .get_mut(&link.surface)
            .filter(|surface| surface.instance == link.instance)
            .map(|surface| &mut surface.content)
    }

    fn orphaned(&self, surface: u32) -> bool {
        let mut current = surface;
        for _ in 0..=self.surfaces.len() {
            let Some(parent) = self.surfaces.get(&current).and_then(|state| state.parent) else {
                return false;
            };
            if !self.surface_alive(parent) {
                return true;
            }
            current = parent.surface;
        }
        true
    }

    fn check_commit(&self, surface: u32) -> Result<(), &'static str> {
        let Some(state) = self.surfaces.get(&surface) else {
            return Ok(());
        };
        let parent_alive = state
            .parent
            .is_some_and(|parent| self.surface_alive(parent));
        if state.synchronized && parent_alive && self.orphaned(surface) {
            return Err("a synchronized subsurface with a destroyed ancestor");
        }
        state.content.refusal(state.role).map_or(Ok(()), Err)
    }

    fn descends_from(&self, surface: u32, ancestor: u32) -> bool {
        let mut current = Some(surface);
        let mut steps = 0;
        while let Some(at) = current {
            if at == ancestor {
                return true;
            }
            steps += 1;
            if steps > self.surfaces.len() {
                return true;
            }
            current = self.live_parent(at);
        }
        false
    }

    fn check_request(
        &self,
        class: Class,
        request: &Request,
        interface: &'static Interface,
        desc: &'static MessageDesc,
    ) -> Result<(), Violation> {
        let fail = |reason| violation(request.object, interface, desc, reason);
        let object = &self.objects[&request.object];
        if let Some(link) = self.extensions.get(&request.object) {
            if !desc.is_destructor && !self.surface_alive(*link) {
                return Err(fail("a request after its surface was destroyed"));
            }
        }
        let args = request.args.as_slice();
        match (class, request.opcode, args) {
            (
                Class::Surface,
                wl_surface::REQ_ATTACH_OPCODE,
                [_, Argument::Int(x), Argument::Int(y)],
            ) => {
                if object.version >= wl_surface::REQ_OFFSET_SINCE && (*x, *y) != (0, 0) {
                    return Err(fail("a buffer offset in attach"));
                }
            }
            (Class::Surface, wl_surface::REQ_SET_BUFFER_SCALE_OPCODE, [Argument::Int(scale)]) => {
                if *scale <= 0 {
                    return Err(fail("a buffer scale below 1"));
                }
            }
            (
                Class::Surface,
                wl_surface::REQ_SET_BUFFER_TRANSFORM_OPCODE,
                [Argument::Int(transform)],
            ) => {
                if !(0..=7).contains(transform) {
                    return Err(fail("an unknown buffer transform"));
                }
            }
            (
                Class::Subcompositor,
                wl_subcompositor::REQ_GET_SUBSURFACE_OPCODE,
                [_, Argument::Object(surface), Argument::Object(parent)],
            ) => {
                let state = self
                    .surfaces
                    .get(surface)
                    .ok_or_else(|| fail("a destroyed surface"))?;
                if !matches!(state.role, SurfaceRole::None | SurfaceRole::Subsurface)
                    || state.parent.is_some()
                {
                    return Err(fail("a surface that already has another role"));
                }
                if self.descends_from(*parent, *surface) {
                    return Err(fail("a subsurface of itself"));
                }
                if self.orphaned(*parent) {
                    return Err(fail("a parent subsurface with a destroyed ancestor"));
                }
            }
            (Class::Surface, wl_surface::REQ_COMMIT_OPCODE, _) => {
                self.check_commit(request.object).map_err(fail)?;
            }
            (Class::Surface, wl_surface::REQ_DESTROY_OPCODE, _) => {
                let subsurface_alive = self.surfaces.get(&request.object).is_some_and(|state| {
                    state.role == SurfaceRole::Subsurface && state.parent.is_some()
                });
                if subsurface_alive {
                    return Err(fail("a surface destroyed before its wl_subsurface"));
                }
            }
            (
                Class::Subsurface,
                wl_subsurface::REQ_PLACE_ABOVE_OPCODE | wl_subsurface::REQ_PLACE_BELOW_OPCODE,
                [Argument::Object(sibling)],
            ) => {
                let (surface, parent) = self
                    .subsurfaces
                    .get(&request.object)
                    .filter(|link| self.surface_alive(**link))
                    .and_then(|link| Some((link.surface, self.live_parent(link.surface)?)))
                    .ok_or_else(|| fail("a subsurface whose surface or parent is gone"))?;
                if *sibling == surface
                    || (*sibling != parent && self.live_parent(*sibling) != Some(parent))
                {
                    return Err(fail("a sibling that is neither the parent nor a sibling"));
                }
            }
            (
                Class::Pointer,
                wl_pointer::REQ_SET_CURSOR_OPCODE,
                [_, Argument::Object(surface), ..],
            ) if *surface != 0 => {
                if !matches!(
                    self.surface_role(*surface),
                    Some(SurfaceRole::None | SurfaceRole::Cursor)
                ) {
                    return Err(fail("a cursor surface that has another role"));
                }
            }
            (
                Class::DataDevice,
                wl_data_device::REQ_START_DRAG_OPCODE,
                [_, _, Argument::Object(icon), _],
            ) if *icon != 0 => {
                if !matches!(
                    self.surface_role(*icon),
                    Some(SurfaceRole::None | SurfaceRole::DragIcon)
                ) {
                    return Err(fail("a drag icon that has another role"));
                }
            }
            (Class::Seat, opcode, _) => {
                let needed = match opcode {
                    wl_seat::REQ_GET_POINTER_OPCODE => wl_seat::Capability::Pointer,
                    wl_seat::REQ_GET_KEYBOARD_OPCODE => wl_seat::Capability::Keyboard,
                    wl_seat::REQ_GET_TOUCH_OPCODE => wl_seat::Capability::Touch,
                    _ => return Ok(()),
                };
                let ever = self.seats.get(&request.object).copied().unwrap_or(0);
                if ever & needed.bits() == 0 {
                    return Err(fail("a device the seat never had"));
                }
            }
            (
                Class::Shm,
                wl_shm::REQ_CREATE_POOL_OPCODE,
                [_, Argument::Fd(file), Argument::Int(size)],
            ) => {
                if *size <= 0 {
                    return Err(fail("an empty pool"));
                }
                if self.pools.len() >= POOL_LIMIT {
                    return Err(fail("too many shm pools"));
                }
                shm_file_holds(file.as_fd(), *size).map_err(fail)?;
            }
            (
                Class::ShmPool,
                wl_shm_pool::REQ_CREATE_BUFFER_OPCODE,
                [_, Argument::Int(offset), Argument::Int(width), Argument::Int(height), Argument::Int(stride), Argument::Uint(format)],
            ) => {
                let pool = self.pools.get(&request.object).map_or(0, |pool| pool.size);
                let bytes_per_pixel = shm_bytes_per_pixel(*format)
                    .ok_or_else(|| fail("a buffer format whose layout the proxy does not know"))?;
                let fits = *width > 0
                    && *height > 0
                    && *offset >= 0
                    && i64::from(*stride) >= i64::from(*width) * bytes_per_pixel
                    && i64::from(*offset) + i64::from(*stride) * i64::from(*height)
                        <= i64::from(pool);
                if !fits {
                    return Err(fail("a buffer outside its pool"));
                }
                if !ALWAYS_SUPPORTED_SHM_FORMATS.contains(format)
                    && !self.shm_formats.contains(format)
                {
                    return Err(fail("a buffer format the compositor did not offer"));
                }
            }
            (Class::ShmPool, wl_shm_pool::REQ_RESIZE_OPCODE, [Argument::Int(size)]) => {
                let pool = self
                    .pools
                    .get(&request.object)
                    .ok_or_else(|| fail("a pool without state"))?;
                if *size < pool.size {
                    return Err(fail("a pool that shrinks"));
                }
                shm_file_holds(pool.file.as_fd(), *size).map_err(fail)?;
            }
            (
                Class::Viewport,
                wp_viewport::REQ_SET_SOURCE_OPCODE,
                [Argument::Fixed(x), Argument::Fixed(y), Argument::Fixed(width), Argument::Fixed(height)],
            ) => {
                const UNSET: i32 = -256;
                let unset = [*x, *y, *width, *height] == [UNSET; 4];
                if !unset && !(*x >= 0 && *y >= 0 && *width > 0 && *height > 0) {
                    return Err(fail("a source rectangle out of range"));
                }
            }
            (
                Class::Viewport,
                wp_viewport::REQ_SET_DESTINATION_OPCODE,
                [Argument::Int(width), Argument::Int(height)],
            ) => {
                if (*width, *height) != (-1, -1) && !(*width > 0 && *height > 0) {
                    return Err(fail("a destination size out of range"));
                }
            }
            (
                Class::CursorShape,
                wp_cursor_shape_device_v1::REQ_SET_SHAPE_OPCODE,
                [_, Argument::Uint(shape)],
            ) => {
                let last = if object.version >= 2 { 36 } else { 34 };
                if !(1..=last).contains(shape) {
                    return Err(fail("an unknown cursor shape"));
                }
            }
            (_, opcode, [_, Argument::Object(surface)])
                if creates_surface_extension(interface, opcode) =>
            {
                let instance = self
                    .surfaces
                    .get(surface)
                    .map(|state| state.instance)
                    .ok_or_else(|| fail("a destroyed surface"))?;
                let child = desc.child_interface.map_or("", |child| child.name);
                if self.exclusive.contains(&(child, instance)) {
                    return Err(fail("a second extension object for one surface"));
                }
            }
            _ => {}
        }
        Ok(())
    }

    fn rewrite_request(&self, class: Class, request: &mut Request) {
        if class != Class::TextInput
            || request.opcode != zwp_text_input_v3::REQ_SET_CURSOR_RECTANGLE_OPCODE
        {
            return;
        }
        let Some(surface) = self.input.text_input_surface(request.object) else {
            return;
        };
        let (dx, dy) = self.surface_position(surface);
        if let [Argument::Int(x), Argument::Int(y), ..] = request.args.as_mut_slice() {
            *x = x.saturating_add(dx);
            *y = y.saturating_add(dy);
        }
    }

    fn after_request(
        &mut self,
        up: &Upstream,
        class: Class,
        interface: &'static Interface,
        request: Request,
        desc: &'static MessageDesc,
    ) {
        let id = request.object;
        match (class, request.opcode, request.args.as_slice()) {
            (Class::Surface, wl_surface::REQ_ATTACH_OPCODE, [Argument::Object(buffer), ..]) => {
                let size = self.buffers.get(buffer).copied();
                if let Some(state) = self.surfaces.get_mut(&id) {
                    state.attached = Some(*buffer != 0);
                    state.content.buffer = size;
                }
            }
            (Class::Surface, wl_surface::REQ_SET_BUFFER_SCALE_OPCODE, [Argument::Int(scale)]) => {
                if let Some(state) = self.surfaces.get_mut(&id) {
                    state.content.scale = *scale;
                }
            }
            (
                Class::Surface,
                wl_surface::REQ_SET_BUFFER_TRANSFORM_OPCODE,
                [Argument::Int(transform)],
            ) => {
                if let Some(state) = self.surfaces.get_mut(&id) {
                    state.content.transform = *transform;
                }
            }
            (Class::Surface, wl_surface::REQ_COMMIT_OPCODE, _) => self.committed(up, id),
            (
                Class::Subcompositor,
                wl_subcompositor::REQ_GET_SUBSURFACE_OPCODE,
                [Argument::NewId(subsurface), Argument::Object(surface), Argument::Object(parent)],
            ) => {
                let (Some(link), Some(parent)) = (self.link(*surface), self.link(*parent)) else {
                    return;
                };
                if let Some(state) = self.surfaces.get_mut(surface) {
                    state.role = SurfaceRole::Subsurface;
                    state.parent = Some(parent);
                    state.synchronized = true;
                }
                self.subsurfaces.insert(*subsurface, link);
            }
            (
                Class::Subsurface,
                opcode
                @ (wl_subsurface::REQ_SET_SYNC_OPCODE | wl_subsurface::REQ_SET_DESYNC_OPCODE),
                _,
            ) => {
                let link = self.subsurfaces.get(&id).copied();
                let state = link.and_then(|link| {
                    self.surfaces
                        .get_mut(&link.surface)
                        .filter(|state| state.instance == link.instance)
                });
                if let Some(state) = state {
                    state.synchronized = opcode == wl_subsurface::REQ_SET_SYNC_OPCODE;
                }
            }
            (
                Class::ShmPool,
                wl_shm_pool::REQ_CREATE_BUFFER_OPCODE,
                [Argument::NewId(buffer), _, Argument::Int(width), Argument::Int(height), ..],
            ) => {
                self.buffers.insert(*buffer, (*width, *height));
            }
            (
                Class::Viewport,
                wp_viewport::REQ_SET_SOURCE_OPCODE,
                [Argument::Fixed(x), Argument::Fixed(y), Argument::Fixed(width), Argument::Fixed(height)],
            ) => {
                const UNSET: i32 = -256;
                let source = [*x, *y, *width, *height];
                let link = self.extensions.get(&id).copied();
                if let Some(content) = link.and_then(|link| self.live_content(link)) {
                    content.source = (source != [UNSET; 4]).then_some(source);
                }
            }
            (
                Class::Viewport,
                wp_viewport::REQ_SET_DESTINATION_OPCODE,
                [Argument::Int(width), Argument::Int(height)],
            ) => {
                let link = self.extensions.get(&id).copied();
                if let Some(content) = link.and_then(|link| self.live_content(link)) {
                    content.destination = (*width, *height) != (-1, -1);
                }
            }
            (
                _,
                wp_single_pixel_buffer_manager_v1::REQ_CREATE_U32_RGBA_BUFFER_OPCODE,
                [Argument::NewId(buffer), ..],
            ) if interface.name == WpSinglePixelBufferManagerV1::interface().name => {
                self.buffers.insert(*buffer, (1, 1));
            }
            (
                Class::Pointer,
                wl_pointer::REQ_SET_CURSOR_OPCODE,
                [_, Argument::Object(surface), ..],
            ) => {
                if let Some(state) = self.surfaces.get_mut(surface) {
                    state.role = SurfaceRole::Cursor;
                }
            }
            (
                Class::DataDevice,
                wl_data_device::REQ_START_DRAG_OPCODE,
                [_, _, Argument::Object(icon), _],
            ) => {
                if let Some(state) = self.surfaces.get_mut(icon) {
                    state.role = SurfaceRole::DragIcon;
                }
            }
            (
                Class::Shm,
                wl_shm::REQ_CREATE_POOL_OPCODE,
                [Argument::NewId(pool), Argument::Fd(_), Argument::Int(size)],
            ) => {
                let (pool, size) = (*pool, *size);
                if let Some(Argument::Fd(file)) = request.args.into_iter().nth(1) {
                    self.pools.insert(pool, Pool { size, file });
                }
            }
            (Class::ShmPool, wl_shm_pool::REQ_RESIZE_OPCODE, [Argument::Int(size)]) => {
                if let Some(pool) = self.pools.get_mut(&id) {
                    pool.size = *size;
                }
            }
            (_, opcode, [Argument::NewId(extension), Argument::Object(surface)])
                if creates_surface_extension(interface, opcode) =>
            {
                let Some(instance) = self.surfaces.get(surface).map(|state| state.instance) else {
                    return;
                };
                self.extensions.insert(
                    *extension,
                    SurfaceLink {
                        surface: *surface,
                        instance,
                    },
                );
                if let Some(child) = desc.child_interface {
                    self.exclusive.insert((child.name, instance));
                }
            }
            _ => {}
        }
    }

    pub(super) fn deliver(&mut self, up: &Upstream, event: Message<ObjectId, OwnedFd>) {
        let Some(&target) = self.upstream.get(&event.sender_id) else {
            discard(up, event);
            return;
        };
        let object = &self.objects[&target];
        let (interface, version) = (object.interface, object.version);
        let class = match object.kind {
            Kind::Upstream { class, .. } => class,
            _ => Class::Other,
        };
        let Some(desc) = interface.events.get(usize::from(event.opcode)) else {
            discard(up, event);
            return;
        };
        if let Some(awaited) = self.awaiting.filter(|awaited| awaited.params == target) {
            self.awaiting = None;
            self.buffer_answered(up, awaited, event);
            return;
        }
        let mut foreign = Vec::new();
        let mut args = Vec::with_capacity(event.args.len());
        for (index, arg) in event.args.into_iter().enumerate() {
            args.push(match arg {
                Argument::Int(value) => Argument::Int(value),
                Argument::Uint(value) => Argument::Uint(value),
                Argument::Fixed(value) => Argument::Fixed(value),
                Argument::Str(text) => Argument::Str(text),
                Argument::Array(bytes) => Argument::Array(bytes),
                Argument::Fd(fd) => Argument::Fd(fd),
                Argument::Object(id) if id.is_null() => {
                    if matches!(
                        desc.signature.get(index),
                        Some(ArgumentType::Object(AllowNull::No))
                    ) {
                        foreign.push(index);
                    }
                    Argument::Object(0)
                }
                Argument::Object(id) => match self.upstream.get(&id) {
                    Some(known) => Argument::Object(*known),
                    None => {
                        foreign.push(index);
                        Argument::Object(0)
                    }
                },
                Argument::NewId(id) => {
                    let child = desc.child_interface.unwrap_or_else(|| id.interface());
                    let adopted = match self.server_ids.allocate() {
                        Some(server_id) => self
                            .adopt(
                                server_id,
                                child,
                                version,
                                Kind::Upstream {
                                    id: id.clone(),
                                    class: class_of(child),
                                },
                            )
                            .map(|()| server_id),
                        None => Err("the proxy ran out of object ids"),
                    };
                    match adopted {
                        Ok(server_id) => Argument::NewId(server_id),
                        Err(_) => {
                            up.destroy(&id);
                            self.failed.get_or_insert(WireError::Backlog(
                                "the helper holds too many objects",
                            ));
                            return;
                        }
                    }
                }
            });
        }
        self.route_event(target, class, event.opcode, args, &foreign);
        if desc.is_destructor {
            self.forget(target);
        }
    }

    fn route_event(
        &mut self,
        target: u32,
        class: Class,
        opcode: u16,
        args: Args,
        foreign: &[usize],
    ) {
        match class {
            Class::Seat if opcode == wl_seat::EVT_CAPABILITIES_OPCODE => {
                if let (Some(ever), [Argument::Uint(capabilities)]) =
                    (self.seats.get_mut(&target), args.as_slice())
                {
                    *ever |= *capabilities;
                }
                self.send(target, opcode, args);
            }
            Class::Shm if opcode == wl_shm::EVT_FORMAT_OPCODE => {
                if let [Argument::Uint(format)] = args.as_slice() {
                    if self.shm_formats.len() < SHM_FORMAT_LIMIT {
                        self.shm_formats.insert(*format);
                    }
                }
                self.send(target, opcode, args);
            }
            Class::DmabufFeedback => {
                match (opcode, args.as_slice()) {
                    (
                        zwp_linux_dmabuf_feedback_v1::EVT_FORMAT_TABLE_OPCODE,
                        [Argument::Fd(file), Argument::Uint(size)],
                    ) => self.dmabuf.format_table(target, file.as_fd(), *size),
                    (
                        zwp_linux_dmabuf_feedback_v1::EVT_TRANCHE_FORMATS_OPCODE,
                        [Argument::Array(indices)],
                    ) => self.dmabuf.tranche_formats(target, indices),
                    _ => {}
                }
                self.send(target, opcode, args);
            }
            Class::BufferParams if opcode == zwp_linux_buffer_params_v1::EVT_CREATED_OPCODE => {
                if let [Argument::NewId(buffer)] = args.as_slice() {
                    self.remember_dmabuf_size(target, *buffer);
                }
                self.send(target, opcode, args);
            }
            Class::Pointer => self.pointer_event(target, opcode, args, foreign),
            Class::Keyboard => self.keyboard_event(target, opcode, args, foreign),
            Class::Touch => self.touch_event(target, opcode, args, foreign),
            Class::TextInput => self.text_input_event(target, opcode, args, foreign),
            Class::DataDevice => self.data_device_event(target, opcode, args, foreign),
            Class::Gesture { end } => self.gesture_event(target, opcode, end, args, foreign),
            _ if !foreign.is_empty() => {}
            _ => self.send(target, opcode, args),
        }
    }

    pub(super) fn resize(&mut self, size: (i32, i32)) {
        if self.size == Some(size) {
            return;
        }
        self.size = Some(size);
        self.configure_toplevels();
    }

    pub(super) fn teardown(self, up: &Upstream) {
        for upstream in self.owned.values().rev() {
            up.destroy(upstream);
        }
        let _ = up.backend.flush();
    }

    pub(super) fn disconnect(mut self, up: &Upstream, end: &SessionEnd) {
        if let SessionEnd::Violation(violation) = end {
            let object = if self.objects.contains_key(&violation.object) {
                violation.object
            } else {
                DISPLAY_ID
            };
            let message = CString::new(violation.to_string()).unwrap_or_default();
            self.failed = None;
            self.send(
                DISPLAY_ID,
                wl_display::EVT_ERROR_OPCODE,
                vec![
                    Argument::Object(object),
                    Argument::Uint(wl_display::Error::InvalidMethod as u32),
                    Argument::Str(Some(Box::new(message))),
                ],
            );
            let _ = self.wire.flush();
        }
        self.teardown(up);
    }
}

fn violation(
    object: u32,
    interface: &'static Interface,
    desc: &'static MessageDesc,
    reason: &'static str,
) -> Violation {
    Violation {
        object,
        interface: interface.name,
        request: desc.name,
        reason,
    }
}

fn shm_file_holds(file: BorrowedFd<'_>, size: i32) -> Result<(), &'static str> {
    const UNREADABLE: &str = "a pool file that cannot be inspected";
    let stat = rustix::fs::fstat(file).map_err(|_| UNREADABLE)?;
    if FileType::from_raw_mode(stat.st_mode) != FileType::RegularFile {
        return Err("a pool that is not a shared memory file");
    }
    if stat.st_size < i64::from(size) {
        return Err("a pool larger than its file");
    }
    let access = rustix::fs::fcntl_getfl(file).map_err(|_| UNREADABLE)?;
    if access & OFlags::RWMODE != OFlags::RDWR {
        return Err("a pool file that is not open for reading and writing");
    }
    let seals = match rustix::fs::fcntl_get_seals(file) {
        Ok(seals) => seals,
        Err(rustix::io::Errno::INVAL) => SealFlags::empty(),
        Err(_) => return Err(UNREADABLE),
    };
    if seals.intersects(SealFlags::WRITE | SealFlags::FUTURE_WRITE) {
        return Err("a pool file sealed against writes");
    }
    Ok(())
}

fn shm_bytes_per_pixel(format: u32) -> Option<i64> {
    use wl_shm::Format;
    match Format::try_from(format).ok()? {
        Format::Rgb565
        | Format::Argb1555
        | Format::Xrgb1555
        | Format::Argb4444
        | Format::Xrgb4444 => Some(2),
        Format::Argb8888
        | Format::Xrgb8888
        | Format::Abgr8888
        | Format::Xbgr8888
        | Format::Rgba8888
        | Format::Rgbx8888
        | Format::Bgra8888
        | Format::Bgrx8888
        | Format::Argb2101010
        | Format::Xrgb2101010
        | Format::Abgr2101010
        | Format::Xbgr2101010 => Some(4),
        Format::Argb16161616f
        | Format::Xrgb16161616f
        | Format::Abgr16161616f
        | Format::Xbgr16161616f => Some(8),
        _ => None,
    }
}

pub(super) fn discard(up: &Upstream, event: Message<ObjectId, OwnedFd>) {
    for arg in event.args {
        if let Argument::NewId(id) = arg {
            if !id.is_null() {
                up.destroy(&id);
            }
        }
    }
}
