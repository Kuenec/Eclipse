use std::ffi::c_void;
use std::fmt;
use std::num::NonZeroU32;
use std::ptr::NonNull;

use raw_window_handle::{RawDisplayHandle, RawWindowHandle};
use wayland_client::backend::{Backend, ObjectId};
use wayland_client::protocol::{wl_registry, wl_surface::WlSurface};
use wayland_client::{Connection, Dispatch, DispatchError, Proxy, QueueHandle};
use wayland_protocols::xdg::foreign::zv2::client::zxdg_exported_v2::{self, ZxdgExportedV2};
use wayland_protocols::xdg::foreign::zv2::client::zxdg_exporter_v2::{self, ZxdgExporterV2};
use winit::dpi::PhysicalSize;

use crate::webview::client;
use crate::webview::proto::{ParentSize, ParentWindow, SizeUnit};

pub(crate) struct WebViewParent {
    _export: Option<ExportedToplevel>,

    unit: SizeUnit,
}

impl WebViewParent {
    pub(crate) unsafe fn for_window_whose_display_outlives_it(
        display: RawDisplayHandle,
        window: RawWindowHandle,
    ) -> Self {
        match game_surface(display, window) {
            GameSurface::Wayland { display, surface } => {
                let exported = unsafe {
                    ExportedToplevel::for_surface_whose_display_outlives_it(display, surface)
                };
                let export = match exported {
                    Ok(export) => {
                        client::set_parent(ParentWindow::Wayland {
                            handle: export.handle.clone(),
                        });
                        Some(export)
                    }
                    Err(error) => {
                        tracing::info!(
                            %error,
                            "WebView windows open as normal windows, not as dialogs of the game window"
                        );
                        None
                    }
                };
                Self {
                    _export: export,
                    unit: SizeUnit::Logical,
                }
            }
            GameSurface::X11 { window } => {
                client::set_parent(ParentWindow::X11 { window });
                Self {
                    _export: None,
                    unit: SizeUnit::DevicePixels,
                }
            }
            GameSurface::Unsupported => {
                tracing::info!(
                    "the game window is neither a Wayland nor an X11 window; WebView windows open as \
                     normal windows"
                );
                Self {
                    _export: None,
                    unit: SizeUnit::DevicePixels,
                }
            }
        }
    }

    pub(crate) fn resized(&self, size: PhysicalSize<u32>, scale_factor: f64) {
        if let Some(size) = parent_size(self.unit, size, scale_factor) {
            client::parent_resized(size);
        }
    }
}

#[derive(Debug, PartialEq, Eq)]
enum GameSurface {
    Wayland {
        display: NonNull<c_void>,
        surface: NonNull<c_void>,
    },
    X11 {
        window: NonZeroU32,
    },
    Unsupported,
}

fn game_surface(display: RawDisplayHandle, window: RawWindowHandle) -> GameSurface {
    match (display, window) {
        (RawDisplayHandle::Wayland(display), RawWindowHandle::Wayland(window)) => {
            GameSurface::Wayland {
                display: display.display,
                surface: window.surface,
            }
        }
        (RawDisplayHandle::Xlib(_) | RawDisplayHandle::Xcb(_), RawWindowHandle::Xlib(window)) => {
            u32::try_from(window.window)
                .ok()
                .and_then(NonZeroU32::new)
                .map_or(GameSurface::Unsupported, |window| GameSurface::X11 {
                    window,
                })
        }
        (RawDisplayHandle::Xlib(_) | RawDisplayHandle::Xcb(_), RawWindowHandle::Xcb(window)) => {
            GameSurface::X11 {
                window: window.window,
            }
        }
        _ => GameSurface::Unsupported,
    }
}

fn parent_size(unit: SizeUnit, size: PhysicalSize<u32>, scale_factor: f64) -> Option<ParentSize> {
    let (width, height) = match unit {
        SizeUnit::Logical => size.to_logical::<u32>(scale_factor).into(),
        SizeUnit::DevicePixels => size.into(),
    };
    Some(ParentSize {
        width: NonZeroU32::new(width)?,
        height: NonZeroU32::new(height)?,
        unit,
    })
}

struct ExportedToplevel {
    connection: Connection,

    exported: ZxdgExportedV2,

    handle: String,
}

#[derive(Debug)]
enum ExportError {
    NotASurface,

    Unsupported,

    Protocol(DispatchError),

    NoHandle,
}

impl fmt::Display for ExportError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::NotASurface => f.write_str("winit's window handle is not a wl_surface"),
            Self::Unsupported => {
                f.write_str("the compositor does not offer zxdg_exporter_v2 (xdg-foreign)")
            }
            Self::Protocol(error) => write!(f, "exporting the game window failed: {error}"),
            Self::NoHandle => {
                f.write_str("the compositor exported the game window without sending a handle")
            }
        }
    }
}

#[derive(Default)]
struct ExportEvents {
    exporter: Option<u32>,

    handle: Option<String>,
}

impl Dispatch<wl_registry::WlRegistry, ()> for ExportEvents {
    fn event(
        state: &mut Self,
        _: &wl_registry::WlRegistry,
        event: wl_registry::Event,
        _: &(),
        _: &Connection,
        _: &QueueHandle<Self>,
    ) {
        if let wl_registry::Event::Global {
            name, interface, ..
        } = event
        {
            if interface == ZxdgExporterV2::interface().name {
                state.exporter = Some(name);
            }
        }
    }
}

impl Dispatch<ZxdgExporterV2, ()> for ExportEvents {
    fn event(
        _: &mut Self,
        _: &ZxdgExporterV2,
        _: zxdg_exporter_v2::Event,
        _: &(),
        _: &Connection,
        _: &QueueHandle<Self>,
    ) {
    }
}

impl Dispatch<ZxdgExportedV2, ()> for ExportEvents {
    fn event(
        state: &mut Self,
        _: &ZxdgExportedV2,
        event: zxdg_exported_v2::Event,
        _: &(),
        _: &Connection,
        _: &QueueHandle<Self>,
    ) {
        if let zxdg_exported_v2::Event::Handle { handle } = event {
            state.handle = Some(handle).filter(|handle| !handle.is_empty());
        }
    }
}

impl ExportedToplevel {
    unsafe fn for_surface_whose_display_outlives_it(
        display: NonNull<c_void>,
        surface: NonNull<c_void>,
    ) -> Result<Self, ExportError> {
        let backend = unsafe { Backend::from_foreign_display(display.as_ptr().cast()) };
        let connection = Connection::from_backend(backend);
        let surface_id =
            unsafe { ObjectId::from_ptr(WlSurface::interface(), surface.as_ptr().cast()) }
                .map_err(|_| ExportError::NotASurface)?;
        let surface =
            WlSurface::from_id(&connection, surface_id).map_err(|_| ExportError::NotASurface)?;
        let mut queue = connection.new_event_queue();
        let queue_handle = queue.handle();
        let registry = connection.display().get_registry(&queue_handle, ());
        let mut state = ExportEvents::default();
        let listed = queue.roundtrip(&mut state);
        let exporter = state
            .exporter
            .map(|name| registry.bind::<ZxdgExporterV2, _, _>(name, 1, &queue_handle, ()));
        let _ = connection.backend().destroy_object(&registry.id());
        listed.map_err(ExportError::Protocol)?;
        let exporter = exporter.ok_or(ExportError::Unsupported)?;
        let exported = exporter.export_toplevel(&surface, &queue_handle, ());
        exporter.destroy();
        let answered = queue.roundtrip(&mut state);
        match (answered, state.handle) {
            (Ok(_), Some(handle)) => Ok(Self {
                connection,
                exported,
                handle,
            }),
            (answered, _) => {
                revoke(&connection, &exported);
                Err(answered.map_or_else(ExportError::Protocol, |_| ExportError::NoHandle))
            }
        }
    }
}

fn revoke(connection: &Connection, exported: &ZxdgExportedV2) {
    exported.destroy();
    if let Err(error) = connection.flush() {
        tracing::debug!(%error, "the game window's xdg-foreign export was not revoked");
    }
}

impl Drop for ExportedToplevel {
    fn drop(&mut self) {
        revoke(&self.connection, &self.exported);
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use raw_window_handle::{
        WaylandDisplayHandle, WaylandWindowHandle, XcbDisplayHandle, XcbWindowHandle,
        XlibDisplayHandle, XlibWindowHandle,
    };

    fn wayland_display() -> RawDisplayHandle {
        RawDisplayHandle::Wayland(WaylandDisplayHandle::new(NonNull::dangling()))
    }

    fn xlib_display() -> RawDisplayHandle {
        RawDisplayHandle::Xlib(XlibDisplayHandle::new(None, 0))
    }

    fn xid(value: u32) -> NonZeroU32 {
        NonZeroU32::new(value).expect("a test xid")
    }

    #[test]
    fn wayland_games_are_exported_and_x11_games_parent_by_window_id() {
        let surface = NonNull::<c_void>::dangling();
        assert_eq!(
            game_surface(
                wayland_display(),
                RawWindowHandle::Wayland(WaylandWindowHandle::new(surface))
            ),
            GameSurface::Wayland {
                display: NonNull::dangling(),
                surface,
            }
        );
        assert_eq!(
            game_surface(
                xlib_display(),
                RawWindowHandle::Xlib(XlibWindowHandle::new(0x0460_0003))
            ),
            GameSurface::X11 {
                window: xid(0x0460_0003)
            }
        );
        assert_eq!(
            game_surface(
                RawDisplayHandle::Xcb(XcbDisplayHandle::new(None, 0)),
                RawWindowHandle::Xcb(XcbWindowHandle::new(xid(0x0280_000a)))
            ),
            GameSurface::X11 {
                window: xid(0x0280_000a)
            }
        );
    }

    #[test]
    fn unusable_or_mismatched_handles_leave_web_views_unparented() {
        for (display, window) in [
            (
                xlib_display(),
                RawWindowHandle::Xlib(XlibWindowHandle::new(0)),
            ),
            (
                xlib_display(),
                RawWindowHandle::Xlib(XlibWindowHandle::new(0x1_0000_0000)),
            ),
            (
                wayland_display(),
                RawWindowHandle::Xlib(XlibWindowHandle::new(0x0460_0003)),
            ),
            (
                xlib_display(),
                RawWindowHandle::Wayland(WaylandWindowHandle::new(NonNull::dangling())),
            ),
        ] {
            assert_eq!(
                game_surface(display, window),
                GameSurface::Unsupported,
                "{display:?} {window:?}"
            );
        }
    }

    #[test]
    fn wayland_sizes_are_logical_and_x11_sizes_stay_in_pixels() {
        let size = |width, height, unit| ParentSize {
            width: xid(width),
            height: xid(height),
            unit,
        };
        assert_eq!(
            parent_size(SizeUnit::Logical, PhysicalSize::new(2560, 1440), 2.0),
            Some(size(1280, 720, SizeUnit::Logical))
        );
        assert_eq!(
            parent_size(SizeUnit::Logical, PhysicalSize::new(1920, 1080), 1.5),
            Some(size(1280, 720, SizeUnit::Logical))
        );
        assert_eq!(
            parent_size(SizeUnit::DevicePixels, PhysicalSize::new(1920, 1080), 1.5),
            Some(size(1920, 1080, SizeUnit::DevicePixels))
        );
        assert_eq!(
            parent_size(SizeUnit::DevicePixels, PhysicalSize::new(0, 1080), 1.0),
            None,
            "a minimised game window has no size to share"
        );
    }
}
