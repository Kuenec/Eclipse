//! Graphics: forward, don't render (component-map F · 🟢 winit / 🟡 ash·egl).
//!
//! Eclipse is **not** a renderer. Roblox's native engine issues its own Vulkan/GLES; this
//! module will provide the `libvulkan.so`/`libEGL.so`/`libGLESv2.so` the engine links and
//! **forward** those calls to the host driver, **translating WSI** (Android
//! `vkCreateAndroidSurfaceKHR` / `ANativeWindow` → host Wayland/X11 surface). Vulkan is
//! preferred; GL is the fallback. Capability is detected at runtime (never assume a vendor).
//!
//! ## What exists now (M2)
//! [`run_windowed`] creates the host game window via **`winit`** (Wayland/X11, **no GTK** — the
//! Step 3.5 win: keeping GTK4/Mesa out of process startup leaves the low_4gb window clear for
//! ART, see `docs/art-and-runtime.md`) and runs the event loop until the window is closed. The
//! window is the surface the framework's Activity/Surface and the engine will render into; the
//! Vulkan/GL forwarding and WSI translation are the next steps.
//!
//! Planned deps: `ash` (+ `ash-window`, `raw-window-handle`), `khronos-egl` for the GL fallback.
//! TODO(M3/M4): Vulkan loader shim + WSI translation; hand the window to the Activity Surface.

#![forbid(unsafe_code)]

use std::fmt;

use winit::application::ApplicationHandler;
use winit::error::{EventLoopError, OsError};
use winit::event::WindowEvent;
use winit::event_loop::{ActiveEventLoop, EventLoop};
use winit::window::{Window, WindowId};

/// The host game window + event loop (winit application state).
struct GameWindow {
    title: String,
    /// `None` until the event loop is `resumed` and the window is created.
    window: Option<Window>,
    /// Set if window creation failed, so [`run_windowed`] can surface a typed error.
    create_error: Option<OsError>,
}

impl ApplicationHandler for GameWindow {
    fn resumed(&mut self, event_loop: &ActiveEventLoop) {
        // winit requires the window to be created on/after `resumed` (the platform display is
        // ready here). Eclipse uses winit directly — no GTK — so process startup never maps the
        // GTK4/Mesa regions that crowded ART's low_4gb window under ATL (Step 3.5).
        let attrs = Window::default_attributes().with_title(self.title.clone());
        match event_loop.create_window(attrs) {
            Ok(window) => {
                tracing::info!(title = %self.title, "host window created (winit, no GTK)");
                self.window = Some(window);
            }
            Err(e) => {
                tracing::error!(error = %e, "failed to create host window");
                self.create_error = Some(e);
                event_loop.exit();
            }
        }
    }

    fn window_event(&mut self, event_loop: &ActiveEventLoop, _id: WindowId, event: WindowEvent) {
        match event {
            WindowEvent::CloseRequested => {
                tracing::info!("window close requested; exiting event loop");
                event_loop.exit();
            }
            // The Activity Surface + Vulkan/GL forwarding will render on redraw (TODO M3/M4).
            WindowEvent::RedrawRequested => {}
            _ => {}
        }
    }
}

/// Open the host game window and run the winit event loop until the window is closed.
///
/// MUST be called on the process main thread (winit requires the event loop there on Linux);
/// `eclipse run` calls this from `main` after the ART VM is booted. Returns when the window is
/// closed, or a typed [`GraphicsError`] if the event loop or window cannot be created.
pub fn run_windowed(title: &str) -> Result<(), GraphicsError> {
    let event_loop = EventLoop::new().map_err(GraphicsError::EventLoop)?;
    let mut app = GameWindow {
        title: title.to_owned(),
        window: None,
        create_error: None,
    };
    event_loop
        .run_app(&mut app)
        .map_err(GraphicsError::EventLoop)?;
    // run_app returns Ok even if `resumed` failed to create the window; surface that as an error.
    if let Some(e) = app.create_error {
        return Err(GraphicsError::CreateWindow(e));
    }
    Ok(())
}

/// Errors from the graphics/window subsystem.
#[derive(Debug)]
pub enum GraphicsError {
    /// The winit event loop could not be created or run (e.g. no display server).
    EventLoop(EventLoopError),
    /// The host window could not be created.
    CreateWindow(OsError),
}

impl fmt::Display for GraphicsError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::EventLoop(e) => write!(f, "winit event loop error: {e}"),
            Self::CreateWindow(e) => write!(f, "failed to create host window: {e}"),
        }
    }
}

impl std::error::Error for GraphicsError {
    fn source(&self) -> Option<&(dyn std::error::Error + 'static)> {
        match self {
            Self::EventLoop(e) => Some(e),
            Self::CreateWindow(e) => Some(e),
        }
    }
}
