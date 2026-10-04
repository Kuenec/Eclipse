use std::ffi::c_void;
use std::ptr::NonNull;

use raw_window_handle::{
    RawDisplayHandle, RawWindowHandle, WaylandDisplayHandle, WaylandWindowHandle,
};
use wayland_client::globals::{registry_queue_init, GlobalListContents};
use wayland_client::protocol::wl_compositor::WlCompositor;
use wayland_client::protocol::wl_registry::{self, WlRegistry};
use wayland_client::protocol::wl_surface::WlSurface;
use wayland_client::{delegate_noop, Connection, Dispatch, EventQueue, Proxy, QueueHandle};

pub(crate) struct RolelessWaylandSurface {
    surface: WlSurface,
    _queue: EventQueue<SurfaceEvents>,
    connection: Connection,
}

struct SurfaceEvents;

impl Dispatch<WlRegistry, GlobalListContents> for SurfaceEvents {
    fn event(
        _: &mut Self,
        _: &WlRegistry,
        _: wl_registry::Event,
        _: &GlobalListContents,
        _: &Connection,
        _: &QueueHandle<Self>,
    ) {
    }
}

delegate_noop!(SurfaceEvents: WlCompositor);
delegate_noop!(SurfaceEvents: ignore WlSurface);

impl RolelessWaylandSurface {
    pub(crate) fn open() -> Result<Self, String> {
        let connection = Connection::connect_to_env()
            .map_err(|error| format!("cannot connect to the Wayland compositor: {error}"))?;
        let (globals, queue) = registry_queue_init::<SurfaceEvents>(&connection)
            .map_err(|error| format!("cannot read the Wayland globals: {error}"))?;
        let compositor: WlCompositor = globals
            .bind(&queue.handle(), 1..=4, ())
            .map_err(|error| format!("cannot bind wl_compositor: {error}"))?;
        let surface = compositor.create_surface(&queue.handle(), ());
        connection
            .flush()
            .map_err(|error| format!("cannot send the new wl_surface: {error}"))?;
        Ok(Self {
            surface,
            _queue: queue,
            connection,
        })
    }

    pub(crate) fn handles(&self) -> (RawDisplayHandle, RawWindowHandle) {
        let display = NonNull::new(self.connection.backend().display_ptr().cast::<c_void>())
            .expect("a connected wl_display");
        let surface =
            NonNull::new(self.surface.id().as_ptr().cast::<c_void>()).expect("a live wl_surface");
        (
            RawDisplayHandle::Wayland(WaylandDisplayHandle::new(display)),
            RawWindowHandle::Wayland(WaylandWindowHandle::new(surface)),
        )
    }
}
