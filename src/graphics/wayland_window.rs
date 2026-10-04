use std::ffi::c_void;
use std::fmt;
use std::marker::PhantomData;
use std::ops::RangeInclusive;
use std::ptr::NonNull;

use raw_window_handle::{
    HandleError, HasDisplayHandle, HasWindowHandle, RawDisplayHandle, RawWindowHandle,
};
use wayland_client::backend::{Backend, ObjectId, WaylandError};
use wayland_client::globals::{registry_queue_init, BindError, GlobalError, GlobalListContents};
use wayland_client::protocol::wl_registry::{self, WlRegistry};
use wayland_client::protocol::wl_surface::WlSurface;
use wayland_client::{Connection, Dispatch, Proxy, QueueHandle};
use winit::window::Window;

pub(super) struct WaylandWindow<'window> {
    display: NonNull<c_void>,
    surface: NonNull<c_void>,
    window: PhantomData<&'window Window>,
}

pub(super) struct SurfaceRequests;

impl Dispatch<WlRegistry, GlobalListContents> for SurfaceRequests {
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

#[derive(Debug)]
pub(super) enum RequestError {
    NotASurface,
    Globals(GlobalError),
    Absent(&'static str),
    TooOld(&'static str),
    Flush(WaylandError),
}

impl fmt::Display for RequestError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::NotASurface => f.write_str("winit's window handle is not a wl_surface"),
            Self::Globals(error) => write!(f, "listing the compositor's globals failed: {error}"),
            Self::Absent(interface) => write!(f, "the compositor does not offer {interface}"),
            Self::TooOld(interface) => {
                write!(
                    f,
                    "the compositor's {interface} is older than Eclipse needs"
                )
            }
            Self::Flush(error) => {
                write!(f, "sending the requests to the compositor failed: {error}")
            }
        }
    }
}

impl<'window> WaylandWindow<'window> {
    pub(super) fn of(window: &'window Window) -> Result<Option<Self>, HandleError> {
        let display = window.display_handle()?.as_raw();
        let handle = window.window_handle()?.as_raw();
        Ok(unsafe { Self::from_handles_that_outlive_it(display, handle) })
    }

    pub(super) unsafe fn from_handles_that_outlive_it(
        display: RawDisplayHandle,
        window: RawWindowHandle,
    ) -> Option<Self> {
        match (display, window) {
            (RawDisplayHandle::Wayland(display), RawWindowHandle::Wayland(window)) => Some(Self {
                display: display.display,
                surface: window.surface,
                window: PhantomData,
            }),
            _ => None,
        }
    }

    pub(super) fn send<Global>(
        &self,
        versions: RangeInclusive<u32>,
        requests: impl FnOnce(&Global, &WlSurface, &QueueHandle<SurfaceRequests>),
    ) -> Result<(), RequestError>
    where
        Global: Proxy + 'static,
        SurfaceRequests: Dispatch<Global, ()>,
    {
        let backend = unsafe { Backend::from_foreign_display(self.display.as_ptr().cast()) };
        let connection = Connection::from_backend(backend);
        let surface_id =
            unsafe { ObjectId::from_ptr(WlSurface::interface(), self.surface.as_ptr().cast()) }
                .map_err(|_| RequestError::NotASurface)?;
        let surface =
            WlSurface::from_id(&connection, surface_id).map_err(|_| RequestError::NotASurface)?;
        let (globals, queue) =
            registry_queue_init::<SurfaceRequests>(&connection).map_err(RequestError::Globals)?;
        let interface = Global::interface().name;
        let global = globals
            .bind(&queue.handle(), versions, ())
            .map_err(|error| match error {
                BindError::NotPresent => RequestError::Absent(interface),
                BindError::UnsupportedVersion => RequestError::TooOld(interface),
            })?;
        requests(&global, &surface, &queue.handle());
        connection.flush().map_err(RequestError::Flush)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use raw_window_handle::{
        WaylandDisplayHandle, WaylandWindowHandle, XcbDisplayHandle, XcbWindowHandle,
        XlibDisplayHandle, XlibWindowHandle,
    };

    #[test]
    fn only_a_wayland_surface_on_a_wayland_display_is_a_wayland_window() {
        let wayland_display =
            RawDisplayHandle::Wayland(WaylandDisplayHandle::new(NonNull::dangling()));
        let wayland_surface =
            RawWindowHandle::Wayland(WaylandWindowHandle::new(NonNull::dangling()));
        let xlib_display = RawDisplayHandle::Xlib(XlibDisplayHandle::new(None, 0));
        let xlib_window = RawWindowHandle::Xlib(XlibWindowHandle::new(0x0460_0003));
        let xcb_window = RawWindowHandle::Xcb(XcbWindowHandle::new(
            std::num::NonZeroU32::new(0x0280_000a).expect("an xid"),
        ));
        let wayland = |display, window| unsafe {
            WaylandWindow::from_handles_that_outlive_it(display, window).is_some()
        };
        assert!(wayland(wayland_display, wayland_surface));
        assert!(!wayland(xlib_display, xlib_window));
        assert!(!wayland(
            RawDisplayHandle::Xcb(XcbDisplayHandle::new(None, 0)),
            xcb_window
        ));
        assert!(!wayland(wayland_display, xlib_window));
        assert!(!wayland(xlib_display, wayland_surface));
    }
}
