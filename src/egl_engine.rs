//! The engine's GLES2/EGL render surface on Eclipse's window (component-map F · engine render path).
//!
//! 2026-06-05: Roblox's x86-64 native engine renders the **whole frame** into an `ANativeWindow`
//! via **EGL + GLES2** (91 EGL/GLESv2 imports → host Mesa GL; **0 Vulkan** imports — see
//! `docs/libroblox-characterization.md`). That is a different render model from the Java-view-app
//! path Eclipse already drives through Vulkan (`src/graphics.rs`, [`crate::graphics`]): the engine
//! draws everything itself; Eclipse only has to give it a real GL-capable window surface.
//!
//! This module builds that surface on Eclipse's existing `winit` window using **host EGL/GLESv2**
//! (`khronos-egl`, `dynamic` feature → dlopens `libEGL.so.1`), so the engine's GL calls can present
//! to Eclipse's window. It is a **separate render mode** from the Vulkan framework path and does not
//! touch it — the Java-app rendering is unaffected (the two are mutually exclusive per run).
//!
//! ## What is real here / what is deferred (honest, dated 2026-06-05)
//! - **REAL:** an EGL display + GLES2 context + on-screen window surface created on Eclipse's
//!   `winit` window from its `raw-window-handle` (Wayland `wl_egl_window` / X11 XID); a GLES2 draw
//!   (clear + a compiled trivial shader program + one triangle) that **presents** via
//!   `eglSwapBuffers`. Validated by the `eclipse __gl-test` harness ([`run_gl_test`]).
//! - **DEFERRED (documented):** routing the *engine's own* `eglCreateWindowSurface(ANativeWindow*)`
//!   onto this surface (WSI translation of the engine's `ANativeWindow*` from
//!   [`ANativeWindow_fromSurface`](crate::loader::native_provider)). That lands when the boot clears
//!   the native-load wall and the engine reaches a frame; this module + the harness prove the host
//!   EGL/GLES path works *now*, in isolation, so it is ready when that happens.
//!
//! ## Portability (§9 detect-don't-assume)
//! The native-window construction is chosen at runtime from the `RawWindowHandle` variant
//! (Wayland vs X11), never assumed. On Wayland it needs `wl_egl_window_create` from
//! `libwayland-egl.so.1` (dlopened on demand); on X11 the XID is used directly. A missing host
//! `libEGL`/`libwayland-egl`, or an unsupported display server, surfaces as a typed [`EglError`]
//! (never a panic / UB) and the window stays usable.
//!
//! ## `unsafe`
//! Confined to the unavoidable FFI: `egl.get_display`/`create_window_surface` (EGL `unsafe` fns over
//! raw native pointers), the `wl_egl_window_*`/GLESv2 `dlsym`'d entry points, and the GL calls. Each
//! block carries a dated `// SAFETY:`; the pure-logic helpers ([`gles2_config_attribs`],
//! [`WindowGeometry`]) are safe and unit-tested GPU-free.

use std::ffi::c_void;
use std::fmt;

use khronos_egl as egl;
use raw_window_handle::{HasDisplayHandle, HasWindowHandle, RawDisplayHandle, RawWindowHandle};
use winit::application::ApplicationHandler;
use winit::event::WindowEvent;
use winit::event_loop::{ActiveEventLoop, EventLoop};
use winit::window::{Window, WindowId};

/// The minimum host EGL version Eclipse requires (1.4 — universal on Mesa/desktop; GLES2 contexts
/// and `eglCreateWindowSurface` are 1.0+, but 1.4 is the safe modern floor `khronos-egl` exposes).
type EglApi = egl::EGL1_4;

/// A dynamically-loaded EGL instance (dlopens `libEGL.so.1` at runtime — detect-don't-assume §9).
type EglInstance = egl::DynamicInstance<EglApi>;

/// `EGL_OPENGL_ES2_BIT` (from `<EGL/egl.h>`) — request a GLES2-renderable config.
const EGL_OPENGL_ES2_BIT: egl::Int = 0x0004;

/// The real, on-screen window geometry the engine renders into — sourced from Eclipse's live `winit`
/// window (NOT a hardcoded phone default). The `ANativeWindow_*` geometry natives report these so the
/// engine sizes its framebuffer to Eclipse's actual window.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct WindowGeometry {
    /// Window width in physical pixels (`ANativeWindow_getWidth`).
    pub width: i32,
    /// Window height in physical pixels (`ANativeWindow_getHeight`).
    pub height: i32,
}

impl WindowGeometry {
    /// Construct from a `winit` physical size, clamping to ≥ 1×1 (EGL/`wl_egl_window` reject a
    /// zero dimension; a minimized/zero-size window falls back to 1×1 until the next resize).
    #[must_use]
    pub fn from_physical(width: u32, height: u32) -> Self {
        Self {
            width: width.max(1).min(i32::MAX as u32) as i32,
            height: height.max(1).min(i32::MAX as u32) as i32,
        }
    }
}

/// The GLES2-renderable EGL config attribute list (key/value pairs, `EGL_NONE`-terminated).
///
/// Pure logic (no GPU/EGL handle) so the config request is unit-testable. An RGBA8888, 24-bit-depth
/// window config with `EGL_OPENGL_ES2_BIT` renderable — matches Android's default engine surface
/// (`WINDOW_FORMAT_RGBA_8888`) and the Khronos `eglChooseConfig` reference (Context7 EGL Registry,
/// 2026-06-05).
#[must_use]
pub fn gles2_config_attribs() -> [egl::Int; 15] {
    [
        egl::SURFACE_TYPE,
        egl::WINDOW_BIT,
        egl::RENDERABLE_TYPE,
        EGL_OPENGL_ES2_BIT,
        egl::RED_SIZE,
        8,
        egl::GREEN_SIZE,
        8,
        egl::BLUE_SIZE,
        8,
        egl::ALPHA_SIZE,
        8,
        egl::DEPTH_SIZE,
        24,
        egl::NONE,
    ]
}

/// The GLES2 context attribute list: request a client version 2 context (`EGL_CONTEXT_CLIENT_VERSION`
/// = 2). GLES3 is backward-compatible, so a host that only offers a GLES3 config still satisfies a
/// v2 request; v2 is the engine's documented target. `EGL_NONE`-terminated.
#[must_use]
pub fn gles2_context_attribs() -> [egl::Int; 3] {
    [egl::CONTEXT_CLIENT_VERSION, 2, egl::NONE]
}

/// Errors building or driving the engine's GLES2/EGL surface. Every fallible EGL/GL/dlopen step maps
/// to one of these so a host without a usable EGL/GL stack (or an unsupported display server) fails
/// cleanly — never a panic or UB across the FFI boundary (AGENTS.md §2.8).
#[derive(Debug)]
pub enum EglError {
    /// The host `libEGL.so.1` could not be loaded (no EGL implementation present).
    LoadEgl(String),
    /// `eglGetDisplay`/`eglInitialize` failed (no usable EGL display for this server).
    Display(String),
    /// `eglChooseConfig` returned no GLES2-renderable config.
    NoConfig,
    /// `eglBindAPI`/`eglCreateContext` failed.
    Context(egl::Error),
    /// `eglCreateWindowSurface` failed.
    Surface(egl::Error),
    /// `eglMakeCurrent`/`eglSwapBuffers` failed.
    Present(egl::Error),
    /// The window's `RawWindowHandle`/`RawDisplayHandle` variant is not a supported display server
    /// (only Wayland and X11/Xlib are handled).
    UnsupportedDisplay,
    /// `libwayland-egl.so.1` (Wayland) could not be loaded, or `wl_egl_window_create` failed.
    WaylandEgl(String),
    /// A GLES2 entry point (`libGLESv2.so.2`) could not be resolved, or a GL operation reported an
    /// error (`glGetError() != GL_NO_ERROR`). Carries the operation + GL error code.
    Gl(String),
}

impl fmt::Display for EglError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::LoadEgl(e) => write!(f, "no host libEGL available: {e}"),
            Self::Display(e) => write!(f, "no usable EGL display: {e}"),
            Self::NoConfig => f.write_str("no GLES2-renderable EGL config found"),
            Self::Context(e) => write!(f, "EGL context creation failed: {e}"),
            Self::Surface(e) => write!(f, "eglCreateWindowSurface failed: {e}"),
            Self::Present(e) => write!(f, "EGL make-current/swap failed: {e}"),
            Self::UnsupportedDisplay => {
                f.write_str("unsupported display server (need Wayland or X11)")
            }
            Self::WaylandEgl(e) => write!(f, "libwayland-egl / wl_egl_window error: {e}"),
            Self::Gl(e) => write!(f, "GLES2 error: {e}"),
        }
    }
}

impl std::error::Error for EglError {}

/// A live EGL display + GLES2 context + on-screen window surface on Eclipse's window, plus the host
/// GLES2 entry points used to render. Dropping it tears the EGL state down in order.
///
/// This is the **render surface for the engine path**. The `eclipse __gl-test` harness builds one,
/// renders a real triangle, and presents it — proving the host EGL/GLES path end-to-end before the
/// engine itself reaches a frame.
pub struct EngineGlSurface {
    egl: EglInstance,
    display: egl::Display,
    context: egl::Context,
    surface: egl::Surface,
    /// The dlopened host `libGLESv2.so.2` + its resolved entry points (kept alive for the surface's
    /// lifetime; the library must outlive every call through it).
    gl: Gles2,
    /// The platform WSI native window the EGL surface was created over (Wayland `wl_egl_window` +
    /// its owning `libwayland-egl`; or the X11 XID). Held purely for RAII: the EGL surface references
    /// this native window, so it must outlive it and is destroyed *after* the EGL surface (field drop
    /// order is declaration order; this is declared after `surface`). It is never read after
    /// construction — 2026-06-05: `#[allow(dead_code)]` is the correct annotation for a drop-guard
    /// field, justified here per AGENTS.md §2.2.
    #[allow(dead_code)]
    native: EngineNativeWindow,
    geometry: WindowGeometry,
}

impl EngineGlSurface {
    /// Build the EGL/GLES2 surface on `window` (a live `winit` window) at `geometry`.
    ///
    /// Steps (Khronos EGL Registry reference sequence, Context7 2026-06-05): load host EGL →
    /// `eglGetDisplay`(native display) → `eglInitialize` → `eglChooseConfig`(GLES2) → `eglBindAPI`
    /// (ES) → `eglCreateContext`(v2) → build the native window (Wayland `wl_egl_window` / X11 XID) →
    /// `eglCreateWindowSurface` → `eglMakeCurrent` → load GLES2. Any failure is a typed [`EglError`].
    pub fn new(
        display_handle: RawDisplayHandle,
        window_handle: RawWindowHandle,
        geometry: WindowGeometry,
    ) -> Result<Self, EglError> {
        // Build the REAL WSI native window — the EGLNativeWindowType host EGL accepts (Wayland
        // `wl_egl_window*` from the `wl_surface`; X11 XID). This is the SAME native-window object the
        // engine path exposes as the `ANativeWindow*` (see [`EngineNativeWindow`]); this surface owns
        // its own here for the `__gl-test` path.
        let native = EngineNativeWindow::new(window_handle, geometry)?;
        Self::build(display_handle, native, geometry)
    }

    /// Build the EGL/GLES2 surface over an **engine-supplied** `ANativeWindow*` (an
    /// `EGLNativeWindowType`) — the exact thing the engine does: it got the `ANativeWindow*` from
    /// `ANativeWindow_fromSurface` (the real WSI handle Eclipse owns) and calls host
    /// `eglCreateWindowSurface(display, config, that pointer, …)`. Eclipse renders over the
    /// engine-owned window without owning/freeing it. Used by the `eclipse __gl-test-anw` validation.
    ///
    /// # Safety
    /// `native_window` must be a valid `EGLNativeWindowType` (a `wl_egl_window*` / XID) for
    /// `display_handle`'s display, alive for the returned surface's lifetime.
    pub fn from_ndk_window(
        display_handle: RawDisplayHandle,
        native_window: egl::NativeWindowType,
        geometry: WindowGeometry,
    ) -> Result<Self, EglError> {
        Self::build(
            display_handle,
            EngineNativeWindow::borrowed(native_window, geometry),
            geometry,
        )
    }

    /// Shared EGL/GLES2 bring-up over a constructed [`EngineNativeWindow`] (owned or borrowed).
    ///
    /// Steps (Khronos EGL Registry reference sequence, Context7 2026-06-05): load host EGL →
    /// `eglGetDisplay`(native display) → `eglInitialize` → `eglChooseConfig`(GLES2) → `eglBindAPI`
    /// (ES) → `eglCreateContext`(v2) → `eglCreateWindowSurface`(native window) → `eglMakeCurrent` →
    /// load GLES2. Any failure is a typed [`EglError`].
    fn build(
        display_handle: RawDisplayHandle,
        native: EngineNativeWindow,
        geometry: WindowGeometry,
    ) -> Result<Self, EglError> {
        // Load host libEGL.so.1 (detect-don't-assume §9). The `.1` soname is the stable ABI name.
        // SAFETY: loading the system EGL shared object by its standard soname; a missing/unloadable
        // library returns Err (handled), never UB. The returned instance owns the library.
        let egl = unsafe {
            let lib = libloading::Library::new("libEGL.so.1")
                .map_err(|e| EglError::LoadEgl(e.to_string()))?;
            EglInstance::load_required_from(lib).map_err(|e| EglError::LoadEgl(e.to_string()))?
        };

        // The native display pointer for `eglGetDisplay`: the wl_display* (Wayland) or the X Display*
        // (Xlib). Chosen from the handle variant — never assumed.
        let native_display: egl::NativeDisplayType = match display_handle {
            RawDisplayHandle::Wayland(d) => d.display.as_ptr(),
            RawDisplayHandle::Xlib(d) => match d.display {
                Some(p) => p.as_ptr(),
                None => egl::DEFAULT_DISPLAY,
            },
            _ => return Err(EglError::UnsupportedDisplay),
        };

        // SAFETY: `native_display` is a live wl_display*/Display* from the winit window, valid for the
        // window's lifetime (which outlives this surface). `get_display` only stores the pointer.
        let display = unsafe { egl.get_display(native_display) }
            .ok_or_else(|| EglError::Display("eglGetDisplay returned EGL_NO_DISPLAY".into()))?;
        let (_major, _minor) = egl
            .initialize(display)
            .map_err(|e| EglError::Display(format!("eglInitialize failed: {e}")))?;

        let config = egl
            .choose_first_config(display, &gles2_config_attribs())
            .map_err(|e| EglError::Display(format!("eglChooseConfig failed: {e}")))?
            .ok_or(EglError::NoConfig)?;

        // Bind the GLES client API before creating the context (required for an ES context).
        egl.bind_api(egl::OPENGL_ES_API)
            .map_err(EglError::Context)?;
        let context = egl
            .create_context(display, config, None, &gles2_context_attribs())
            .map_err(EglError::Context)?;

        // SAFETY: `native.as_native_window()` is a valid wl_egl_window*/XID for this display, kept
        // alive in `native` (Wayland) / owned by the winit window (X11) / engine-owned (Borrowed),
        // all outliving this surface. A bad native window surfaces as `EglError::Surface`
        // (EGL_BAD_NATIVE_WINDOW), never UB.
        let surface = unsafe {
            egl.create_window_surface(display, config, native.as_native_window(), None)
                .map_err(EglError::Surface)?
        };

        egl.make_current(display, Some(surface), Some(surface), Some(context))
            .map_err(EglError::Present)?;

        // Load the GLES2 entry points used by the harness from the now-current context.
        let gl = Gles2::load(&egl)?;

        Ok(Self {
            egl,
            display,
            context,
            surface,
            gl,
            native,
            geometry,
        })
    }

    /// Build the EGL/GLES2 surface from a live `winit` [`Window`] — extracts its raw display/window
    /// handles and physical size. This is the one constructor the run/test path and the eventual
    /// engine render path share. A window with no usable raw handle surfaces as a typed error.
    pub fn from_window(window: &Window) -> Result<Self, EglError> {
        let display_handle = window
            .display_handle()
            .map_err(|e| EglError::Display(format!("no raw display handle: {e}")))?
            .as_raw();
        let window_handle = window
            .window_handle()
            .map_err(|e| EglError::WaylandEgl(format!("no raw window handle: {e}")))?
            .as_raw();
        let size = window.inner_size();
        Self::new(
            display_handle,
            window_handle,
            WindowGeometry::from_physical(size.width, size.height),
        )
    }

    /// The window geometry this surface was created at.
    #[must_use]
    pub fn geometry(&self) -> WindowGeometry {
        self.geometry
    }

    /// Present the back buffer (`eglSwapBuffers`). Returns a typed error on `EGL_BAD_*`.
    pub fn swap_buffers(&self) -> Result<(), EglError> {
        self.egl
            .swap_buffers(self.display, self.surface)
            .map_err(EglError::Present)
    }

    /// Access the loaded GLES2 entry points (used by the render harness).
    #[must_use]
    pub fn gl(&self) -> &Gles2 {
        &self.gl
    }
}

impl Drop for EngineGlSurface {
    fn drop(&mut self) {
        // Release the current context, then destroy surface + context. Best-effort: on a teardown
        // error there is nothing to recover, and dropping must not unwind across FFI (§2.8).
        let _ = self.egl.make_current(self.display, None, None, None);
        let _ = self.egl.destroy_surface(self.display, self.surface);
        let _ = self.egl.destroy_context(self.display, self.context);
        // `native` drops after this, destroying the WSI window (e.g. the wl_egl_window) the EGL
        // surface referenced.
    }
}

// =================================================================================================
// EngineNativeWindow — the REAL WSI native window Eclipse owns and exposes as the ANativeWindow*.
// =================================================================================================

/// The real platform WSI native window for Eclipse's `winit` window — the exact handle host EGL's
/// `eglCreateWindowSurface` accepts as its `EGLNativeWindowType` (Khronos EGL Registry, Context7
/// 2026-06-05): on **Wayland** a `wl_egl_window*` created from the window's `wl_surface` at the
/// window size; on **X11** the window's XID.
///
/// 2026-06-05 — the engine render WSI bind: Roblox's native engine creates its OWN EGL context +
/// surface by calling host `eglCreateWindowSurface(display, config, (EGLNativeWindowType)<the
/// ANativeWindow* it got from `ANativeWindow_fromSurface`>, …)`. For that to land on Eclipse's
/// window, the `ANativeWindow*` it receives must BE this real WSI handle. So **Eclipse owns this
/// native window object and hands its pointer out as the `ANativeWindow*`**; it does NOT pre-create
/// a competing EGL context on the engine path (the engine owns its context — two contexts must not
/// fight over one surface). The `__gl-test`/`__gl-test-anw` paths and [`EngineGlSurface`] reuse the
/// SAME construction so the validation exercises exactly what the engine does.
///
/// The pointer [`as_native_window`](Self::as_native_window) returns is stable for this object's
/// lifetime; the underlying `wl_egl_window`/XID is destroyed (Wayland) or simply forgotten (X11, the
/// `winit` window owns the X resource) on [`Drop`].
pub struct EngineNativeWindow {
    /// The platform-specific backing: a dlopened `wl_egl_window` (Wayland) or a bare XID (X11). Held
    /// purely as a drop guard (the Wayland variant frees its `wl_egl_window` on [`Drop`]); never read
    /// after construction. 2026-06-05.
    #[allow(dead_code)]
    backing: NativeWindowBacking,
    /// The `EGLNativeWindowType` pointer host EGL (and the engine's own EGL) accept: the
    /// `wl_egl_window*` (Wayland) or the XID re-interpreted as a pointer (X11). Stable for the
    /// object's lifetime; this is also the value handed out as the `ANativeWindow*`.
    native_window: *mut c_void,
    geometry: WindowGeometry,
}

/// The platform-specific resource an [`EngineNativeWindow`] owns (or, on X11/Borrowed, borrows).
enum NativeWindowBacking {
    /// A Wayland `wl_egl_window` (with its owning `libwayland-egl` library). Held purely as a drop
    /// guard — its [`Drop`] frees the `wl_egl_window`; the field is never read. 2026-06-05.
    Wayland(#[allow(dead_code)] WaylandEglWindow),
    /// An X11 window: the XID is owned by the `winit` window, so there is nothing to free here. The
    /// XID itself is stored in [`EngineNativeWindow::native_window`].
    X11,
    /// A native window owned by someone else (the engine supplies the `ANativeWindow*`; Eclipse only
    /// renders an EGL surface over it). Nothing is freed and the WSI map is NOT touched — the owner
    /// (the real [`EngineNativeWindow`]) handles registration/teardown.
    Borrowed,
}

impl EngineNativeWindow {
    /// Build the real WSI native window for `window_handle` at `geometry`. Chosen at runtime from the
    /// `RawWindowHandle` variant (detect-don't-assume §9): Wayland → a `wl_egl_window` from the
    /// `wl_surface`; X11 → the XID. An unsupported display server is a typed [`EglError`].
    pub fn new(window_handle: RawWindowHandle, geometry: WindowGeometry) -> Result<Self, EglError> {
        let window = match window_handle {
            RawWindowHandle::Wayland(w) => {
                let wl = WaylandEglWindow::new(w.surface.as_ptr(), geometry)?;
                let native_window = wl.window;
                Self {
                    backing: NativeWindowBacking::Wayland(wl),
                    native_window,
                    geometry,
                }
            }
            RawWindowHandle::Xlib(w) => Self {
                backing: NativeWindowBacking::X11,
                // The X `Window` (XID) is the EGLNativeWindowType directly. Stored as a pointer-sized
                // value (the C `EGLNativeWindowType` on X11 is an unsigned long = the XID).
                native_window: w.window as *mut c_void,
                geometry,
            },
            _ => return Err(EglError::UnsupportedDisplay),
        };
        // Publish this real WSI pointer + geometry so the `ANativeWindow_*` geometry natives, which
        // take the pointer back, resolve it to the real size (the engine render WSI bind). Unregistered
        // on Drop.
        crate::loader::ndk_registry::register_wsi_window(
            window.native_window as usize,
            geometry.width,
            geometry.height,
        );
        Ok(window)
    }

    /// Wrap an externally-owned native window pointer (the engine path: the engine supplies the
    /// `ANativeWindow*` it got from `ANativeWindow_fromSurface`; Eclipse renders an EGL surface over
    /// it without owning/freeing it or touching the WSI map). The pointer must be a valid
    /// `EGLNativeWindowType` for the current display, alive for the surface's lifetime.
    #[must_use]
    pub fn borrowed(native_window: egl::NativeWindowType, geometry: WindowGeometry) -> Self {
        Self {
            backing: NativeWindowBacking::Borrowed,
            native_window,
            geometry,
        }
    }

    /// The `EGLNativeWindowType` pointer — what host EGL's `eglCreateWindowSurface` accepts, and the
    /// value Eclipse hands the engine as the `ANativeWindow*`. Stable for this object's lifetime.
    #[must_use]
    pub fn as_native_window(&self) -> egl::NativeWindowType {
        self.native_window
    }

    /// The window geometry this WSI window was created at (`ANativeWindow_getWidth`/`getHeight`).
    #[must_use]
    pub fn geometry(&self) -> WindowGeometry {
        self.geometry
    }
}

impl Drop for EngineNativeWindow {
    fn drop(&mut self) {
        // A Borrowed wrapper never registered the pointer (its owner did) — leave the WSI map alone.
        if matches!(self.backing, NativeWindowBacking::Borrowed) {
            return;
        }
        // Unregister the WSI pointer→geometry mapping BEFORE `backing` drops and frees the underlying
        // window (the pointer value is still the table key here). After this, a getter for the now-gone
        // pointer returns the NDK `-1` sentinel rather than stale geometry.
        crate::loader::ndk_registry::unregister_wsi_window(self.native_window as usize);
    }
}

/// A Wayland `wl_egl_window` wrapping a `wl_surface`, plus the `libwayland-egl` library it came from.
/// The EGL window surface is created over this; it must outlive that surface (dropped after it).
struct WaylandEglWindow {
    /// Kept alive so the resolved `wl_egl_window_*` function pointers stay valid.
    _lib: libloading::Library,
    /// The `wl_egl_window*` (an opaque pointer used as the `EGLNativeWindowType`).
    window: *mut c_void,
    /// `wl_egl_window_destroy` resolved at construction (called on drop).
    destroy: unsafe extern "C" fn(*mut c_void),
}

impl WaylandEglWindow {
    /// Create a `wl_egl_window` over `wl_surface` at `geometry` by dlopening `libwayland-egl.so.1`.
    fn new(wl_surface: *mut c_void, geometry: WindowGeometry) -> Result<Self, EglError> {
        // SAFETY: dlopening the system wayland-egl shared object by its standard soname and resolving
        // its documented C entry points (`wl_egl_window_create`/`_destroy`). A missing library or
        // symbol returns Err (handled), never UB. The library is kept alive in the returned struct,
        // so the resolved function pointers remain valid for its lifetime.
        unsafe {
            let lib = libloading::Library::new("libwayland-egl.so.1")
                .map_err(|e| EglError::WaylandEgl(e.to_string()))?;
            let create: libloading::Symbol<
                unsafe extern "C" fn(*mut c_void, i32, i32) -> *mut c_void,
            > = lib
                .get(b"wl_egl_window_create\0")
                .map_err(|e| EglError::WaylandEgl(e.to_string()))?;
            let destroy: libloading::Symbol<unsafe extern "C" fn(*mut c_void)> = lib
                .get(b"wl_egl_window_destroy\0")
                .map_err(|e| EglError::WaylandEgl(e.to_string()))?;
            let window = create(wl_surface, geometry.width, geometry.height);
            if window.is_null() {
                return Err(EglError::WaylandEgl(
                    "wl_egl_window_create returned NULL".into(),
                ));
            }
            let destroy = *destroy;
            Ok(Self {
                _lib: lib,
                window,
                destroy,
            })
        }
    }
}

impl Drop for WaylandEglWindow {
    fn drop(&mut self) {
        // SAFETY: `window` is a live wl_egl_window from `wl_egl_window_create`, destroyed exactly
        // once here; `destroy` is the matching resolved entry point, valid while `_lib` is alive.
        unsafe { (self.destroy)(self.window) }
    }
}

// =================================================================================================
// GLES2 entry points (the small set the render harness uses), dlsym'd from libGLESv2.so.2.
// =================================================================================================

/// `GL_NO_ERROR` (from `<GLES2/gl2.h>`) — the success value of `glGetError`.
const GL_NO_ERROR: u32 = 0;
/// `GL_COLOR_BUFFER_BIT` — the clear mask for the color buffer.
pub const GL_COLOR_BUFFER_BIT: u32 = 0x0000_4000;
/// `GL_VERTEX_SHADER` / `GL_FRAGMENT_SHADER`.
pub const GL_VERTEX_SHADER: u32 = 0x8B31;
pub const GL_FRAGMENT_SHADER: u32 = 0x8B30;
/// `GL_COMPILE_STATUS` / `GL_LINK_STATUS` — for shader/program status queries.
const GL_COMPILE_STATUS: u32 = 0x8B81;
const GL_LINK_STATUS: u32 = 0x8B82;
/// `GL_FLOAT` (vertex attribute element type) / `GL_TRIANGLES` (draw mode) / `GL_FALSE`.
const GL_FLOAT: u32 = 0x1406;
pub const GL_TRIANGLES: u32 = 0x0004;
const GL_FALSE: u8 = 0;

/// The host GLES2 entry points the [`run_gl_test`] harness calls, resolved from `libGLESv2.so.2`.
///
/// Hand-rolled (not `glow`) to keep the surface tight (§2.5 no-bloat) and fully controlled: exactly
/// the ~15 functions a clear + trivial-shader + one-triangle render needs. The library is kept alive
/// so the function pointers stay valid for its lifetime.
// GLES2 entry-point signatures (from `<GLES2/gl2.h>`). Named aliases so each `transmute` below has an
// explicit, auditable target type (satisfies clippy `missing_transmute_annotations`). 2026-06-05.
type PfnGlGetError = unsafe extern "C" fn() -> u32;
type PfnGlClearColor = unsafe extern "C" fn(f32, f32, f32, f32);
type PfnGlClear = unsafe extern "C" fn(u32);
type PfnGlViewport = unsafe extern "C" fn(i32, i32, i32, i32);
type PfnGlCreateShader = unsafe extern "C" fn(u32) -> u32;
type PfnGlShaderSource = unsafe extern "C" fn(u32, i32, *const *const i8, *const i32);
type PfnGlCompileShader = unsafe extern "C" fn(u32);
type PfnGlGetShaderiv = unsafe extern "C" fn(u32, u32, *mut i32);
type PfnGlCreateProgram = unsafe extern "C" fn() -> u32;
type PfnGlAttachShader = unsafe extern "C" fn(u32, u32);
type PfnGlLinkProgram = unsafe extern "C" fn(u32);
type PfnGlGetProgramiv = unsafe extern "C" fn(u32, u32, *mut i32);
type PfnGlUseProgram = unsafe extern "C" fn(u32);
type PfnGlGetAttribLocation = unsafe extern "C" fn(u32, *const i8) -> i32;
type PfnGlEnableVertexAttribArray = unsafe extern "C" fn(u32);
type PfnGlVertexAttribPointer = unsafe extern "C" fn(u32, i32, u32, u8, i32, *const c_void);
type PfnGlDrawArrays = unsafe extern "C" fn(u32, i32, i32);
type PfnGlDeleteShader = unsafe extern "C" fn(u32);
type PfnGlDeleteProgram = unsafe extern "C" fn(u32);

#[allow(non_snake_case)] // mirror the GL C names for one-to-one auditability.
pub struct Gles2 {
    _lib: libloading::Library,
    glGetError: PfnGlGetError,
    glClearColor: PfnGlClearColor,
    glClear: PfnGlClear,
    glViewport: PfnGlViewport,
    glCreateShader: PfnGlCreateShader,
    glShaderSource: PfnGlShaderSource,
    glCompileShader: PfnGlCompileShader,
    glGetShaderiv: PfnGlGetShaderiv,
    glCreateProgram: PfnGlCreateProgram,
    glAttachShader: PfnGlAttachShader,
    glLinkProgram: PfnGlLinkProgram,
    glGetProgramiv: PfnGlGetProgramiv,
    glUseProgram: PfnGlUseProgram,
    glGetAttribLocation: PfnGlGetAttribLocation,
    glEnableVertexAttribArray: PfnGlEnableVertexAttribArray,
    glVertexAttribPointer: PfnGlVertexAttribPointer,
    glDrawArrays: PfnGlDrawArrays,
    glDeleteShader: PfnGlDeleteShader,
    glDeleteProgram: PfnGlDeleteProgram,
}

impl Gles2 {
    /// Resolve the GLES2 entry points. Tries `eglGetProcAddress` first (works for core GLES on Mesa
    /// with `EGL_KHR_get_all_proc_addresses`), then falls back to dlsym from `libGLESv2.so.2` — the
    /// robust path for core functions across all drivers (detect-don't-assume §9).
    fn load(egl: &EglInstance) -> Result<Self, EglError> {
        // SAFETY: dlopening the system GLESv2 shared object by its standard soname and resolving its
        // documented C entry points. Missing library/symbol → Err (handled), never UB. The library
        // is kept alive in the returned struct so the function pointers remain valid.
        unsafe {
            let lib = libloading::Library::new("libGLESv2.so.2")
                .map_err(|e| EglError::Gl(format!("no libGLESv2.so.2: {e}")))?;

            // Resolve a symbol: prefer eglGetProcAddress, else dlsym from libGLESv2. Returns the raw
            // pointer (or Err) so each typed transmute below is a single explicit step.
            let resolve = |name: &str| -> Result<*const c_void, EglError> {
                if let Some(p) = egl.get_proc_address(name) {
                    return Ok(p as *const c_void);
                }
                let cname = format!("{name}\0");
                let sym: Result<libloading::Symbol<*const c_void>, _> = lib.get(cname.as_bytes());
                match sym {
                    Ok(s) => Ok(*s),
                    Err(e) => Err(EglError::Gl(format!("unresolved GLES2 symbol {name}: {e}"))),
                }
            };

            // Each resolves the named entry point and transmutes the raw pointer to its typed
            // signature, with explicit source + target types (satisfies clippy
            // `missing_transmute_annotations`). A null/unresolved symbol is an Err above (never
            // transmuted). 2026-06-05.
            macro_rules! load_fn {
                ($name:literal, $ty:ty) => {
                    std::mem::transmute::<*const c_void, $ty>(resolve($name)?)
                };
            }
            Ok(Self {
                glGetError: load_fn!("glGetError", PfnGlGetError),
                glClearColor: load_fn!("glClearColor", PfnGlClearColor),
                glClear: load_fn!("glClear", PfnGlClear),
                glViewport: load_fn!("glViewport", PfnGlViewport),
                glCreateShader: load_fn!("glCreateShader", PfnGlCreateShader),
                glShaderSource: load_fn!("glShaderSource", PfnGlShaderSource),
                glCompileShader: load_fn!("glCompileShader", PfnGlCompileShader),
                glGetShaderiv: load_fn!("glGetShaderiv", PfnGlGetShaderiv),
                glCreateProgram: load_fn!("glCreateProgram", PfnGlCreateProgram),
                glAttachShader: load_fn!("glAttachShader", PfnGlAttachShader),
                glLinkProgram: load_fn!("glLinkProgram", PfnGlLinkProgram),
                glGetProgramiv: load_fn!("glGetProgramiv", PfnGlGetProgramiv),
                glUseProgram: load_fn!("glUseProgram", PfnGlUseProgram),
                glGetAttribLocation: load_fn!("glGetAttribLocation", PfnGlGetAttribLocation),
                glEnableVertexAttribArray: load_fn!(
                    "glEnableVertexAttribArray",
                    PfnGlEnableVertexAttribArray
                ),
                glVertexAttribPointer: load_fn!("glVertexAttribPointer", PfnGlVertexAttribPointer),
                glDrawArrays: load_fn!("glDrawArrays", PfnGlDrawArrays),
                glDeleteShader: load_fn!("glDeleteShader", PfnGlDeleteShader),
                glDeleteProgram: load_fn!("glDeleteProgram", PfnGlDeleteProgram),
                _lib: lib,
            })
        }
    }

    /// `glGetError`. Returns `GL_NO_ERROR` (0) when clean.
    fn get_error(&self) -> u32 {
        // SAFETY: a parameterless GL query on the current context.
        unsafe { (self.glGetError)() }
    }

    /// Drain `glGetError`; return `Err` with the first non-zero code at `op` (the GL contract allows
    /// multiple queued errors — we report the first and clear the rest).
    fn check(&self, op: &str) -> Result<(), EglError> {
        let mut first = GL_NO_ERROR;
        loop {
            let e = self.get_error();
            if e == GL_NO_ERROR {
                break;
            }
            if first == GL_NO_ERROR {
                first = e;
            }
        }
        if first == GL_NO_ERROR {
            Ok(())
        } else {
            Err(EglError::Gl(format!("{op}: glGetError()=0x{first:04x}")))
        }
    }
}

/// Render `frames` of a known-color clear + a single triangle through `surface`, asserting
/// `glGetError() == GL_NO_ERROR` throughout and that every `eglSwapBuffers` succeeds.
///
/// This is the **headless success bar** for the engine GL path: it proves an EGL context + surface +
/// real GLES2 draw + present work on Eclipse's window. On the dev host the triangle is visible; the
/// machine-checkable result is 0 GL/EGL errors + successful swaps. Used by `eclipse __gl-test`.
pub fn render_test_frames(surface: &EngineGlSurface, frames: u32) -> Result<(), EglError> {
    let gl = surface.gl();
    let geo = surface.geometry();

    // Trivial GLES2 shaders: a passthrough vertex shader + a constant-color fragment shader. Minimal
    // but real (compiled + linked + used) — exercises the GLSL ES compile path, not just clear.
    const VERT_SRC: &[u8] =
        b"attribute vec2 aPos;\nvoid main(){gl_Position=vec4(aPos,0.0,1.0);}\n\0";
    const FRAG_SRC: &[u8] =
        b"precision mediump float;\nvoid main(){gl_FragColor=vec4(0.149,0.408,0.722,1.0);}\n\0";

    // SAFETY: every call below runs on the current GLES2 context (made current in `new`). The args
    // are validated (shader objects from glCreateShader, the program from glCreateProgram, the vertex
    // pointer is a live stack slice for the draw); errors are detected via `gl.check(...)`. No raw
    // pointer escapes its scope. This block is the unavoidable foreign-call surface for a real draw.
    unsafe {
        let program = compile_program(gl, VERT_SRC, FRAG_SRC)?;
        let pos_loc = (gl.glGetAttribLocation)(program, c"aPos".as_ptr());
        gl.check("glGetAttribLocation")?;
        if pos_loc < 0 {
            (gl.glDeleteProgram)(program);
            return Err(EglError::Gl("aPos attribute not found".into()));
        }
        let pos_loc = pos_loc as u32;

        // A centered triangle in clip space (NDC).
        let verts: [f32; 6] = [0.0, 0.5, -0.5, -0.5, 0.5, -0.5];

        (gl.glViewport)(0, 0, geo.width, geo.height);
        gl.check("glViewport")?;

        for _ in 0..frames {
            (gl.glClearColor)(0.05, 0.05, 0.08, 1.0);
            (gl.glClear)(GL_COLOR_BUFFER_BIT);
            gl.check("glClear")?;

            (gl.glUseProgram)(program);
            (gl.glEnableVertexAttribArray)(pos_loc);
            (gl.glVertexAttribPointer)(
                pos_loc,
                2,
                GL_FLOAT,
                GL_FALSE,
                0,
                verts.as_ptr() as *const c_void,
            );
            (gl.glDrawArrays)(GL_TRIANGLES, 0, 3);
            gl.check("glDrawArrays")?;

            surface.swap_buffers()?;
        }

        (gl.glDeleteProgram)(program);
        gl.check("frame loop")?;
    }
    Ok(())
}

/// Compile + link a GLES2 program from vertex/fragment GLSL ES source (NUL-terminated). Returns the
/// program object, or an [`EglError::Gl`] on a compile/link failure (the compile/link status is
/// queried, not assumed). `unsafe`: GL calls on the current context.
///
/// # Safety
/// Must be called with a current GLES2 context; `vert`/`frag` must be NUL-terminated GLSL ES source.
unsafe fn compile_program(gl: &Gles2, vert: &[u8], frag: &[u8]) -> Result<u32, EglError> {
    // SAFETY (whole body): GL object-creation/compile/link calls on the current context; each status
    // is queried and a failure returns Err (objects created so far are deleted on the error path).
    unsafe {
        let vs = compile_shader(gl, GL_VERTEX_SHADER, vert)?;
        let fs = match compile_shader(gl, GL_FRAGMENT_SHADER, frag) {
            Ok(fs) => fs,
            Err(e) => {
                (gl.glDeleteShader)(vs);
                return Err(e);
            }
        };
        let program = (gl.glCreateProgram)();
        (gl.glAttachShader)(program, vs);
        (gl.glAttachShader)(program, fs);
        (gl.glLinkProgram)(program);
        // Shaders can be deleted after link (the program retains them).
        (gl.glDeleteShader)(vs);
        (gl.glDeleteShader)(fs);
        let mut linked: i32 = 0;
        (gl.glGetProgramiv)(program, GL_LINK_STATUS, &mut linked);
        if linked == 0 {
            (gl.glDeleteProgram)(program);
            return Err(EglError::Gl("program link failed".into()));
        }
        gl.check("link program")?;
        Ok(program)
    }
}

/// Compile a single GLES2 shader of `kind` from NUL-terminated source. Queries `GL_COMPILE_STATUS`.
///
/// # Safety
/// Must be called with a current GLES2 context; `src` must be NUL-terminated GLSL ES source.
unsafe fn compile_shader(gl: &Gles2, kind: u32, src: &[u8]) -> Result<u32, EglError> {
    // SAFETY (whole body): GL shader create/source/compile on the current context; the source
    // pointer is a single NUL-terminated string with `length = NULL` (GL reads to the NUL).
    unsafe {
        let shader = (gl.glCreateShader)(kind);
        if shader == 0 {
            return Err(EglError::Gl("glCreateShader returned 0".into()));
        }
        let ptr = src.as_ptr() as *const i8;
        let ptrs = [ptr];
        (gl.glShaderSource)(shader, 1, ptrs.as_ptr(), std::ptr::null());
        (gl.glCompileShader)(shader);
        let mut status: i32 = 0;
        (gl.glGetShaderiv)(shader, GL_COMPILE_STATUS, &mut status);
        if status == 0 {
            (gl.glDeleteShader)(shader);
            return Err(EglError::Gl(format!(
                "shader (kind 0x{kind:04x}) compile failed"
            )));
        }
        gl.check("compile shader")?;
        Ok(shader)
    }
}

// silence "never constructed" for the few status constants only used in safety/branch checks above
// when a strict unused-analysis runs (they are documented GL constants kept for auditability).
const _: u8 = GL_FALSE;

// =================================================================================================
// `eclipse __gl-test` harness — the REAL EGL/GLES2 render validation on an Eclipse window.
// =================================================================================================

/// How many frames the `__gl-test` harness renders + presents before exiting cleanly.
const GL_TEST_FRAMES: u32 = 5;

/// Result of the `__gl-test` render: what succeeded, so the harness can report a precise machine-
/// checkable outcome (EGL init/ctx/surface, 0 GL errors throughout, every swap succeeded).
#[derive(Debug)]
pub struct GlTestReport {
    /// The window geometry the surface was built at (Eclipse's real window size).
    pub geometry: WindowGeometry,
    /// Frames rendered + presented (each a clear + triangle + `eglSwapBuffers`).
    pub frames: u32,
}

impl fmt::Display for GlTestReport {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(
            f,
            "EGL+GLES2 OK: surface {}x{}, {} frames rendered + presented, 0 GL errors, all swaps succeeded",
            self.geometry.width, self.geometry.height, self.frames
        )
    }
}

/// The `__gl-test` winit application: creates one window, builds the engine GLES2/EGL surface on it,
/// renders [`GL_TEST_FRAMES`] real triangle frames, records the outcome, and exits the loop.
struct GlTestApp {
    /// Set on the first `RedrawRequested` once the surface is built + the frames rendered (or the
    /// error). `None` until then; the harness reads it after the loop returns.
    outcome: Option<Result<GlTestReport, EglError>>,
    /// Held for the window's lifetime (the EGL surface references it). Dropped after `surface`.
    window: Option<Window>,
    /// Window-create error (winit), surfaced if `resumed` could not make a window.
    create_error: Option<winit::error::OsError>,
}

impl ApplicationHandler for GlTestApp {
    fn resumed(&mut self, event_loop: &ActiveEventLoop) {
        let attrs = Window::default_attributes().with_title("Eclipse __gl-test (engine GLES2/EGL)");
        match event_loop.create_window(attrs) {
            Ok(window) => {
                // Publish the live geometry so the ANativeWindow_* geometry natives report Eclipse's
                // real window — proving the bind the engine will read (see native_provider.rs).
                let size = window.inner_size();
                let geo = WindowGeometry::from_physical(size.width, size.height);
                crate::loader::ndk_registry::set_engine_window_geometry(geo.width, geo.height);
                window.request_redraw();
                self.window = Some(window);
            }
            Err(e) => {
                self.create_error = Some(e);
                event_loop.exit();
            }
        }
    }

    fn window_event(&mut self, event_loop: &ActiveEventLoop, _id: WindowId, event: WindowEvent) {
        match event {
            WindowEvent::CloseRequested => event_loop.exit(),
            // Build the EGL surface + render the test frames exactly once (on the first redraw).
            WindowEvent::RedrawRequested if self.outcome.is_none() => {
                let result = self.window.as_ref().map_or_else(
                    || Err(EglError::UnsupportedDisplay),
                    |window| {
                        let surface = EngineGlSurface::from_window(window)?;
                        let geometry = surface.geometry();
                        render_test_frames(&surface, GL_TEST_FRAMES)?;
                        Ok(GlTestReport {
                            geometry,
                            frames: GL_TEST_FRAMES,
                        })
                        // `surface` drops here, tearing down EGL in order before the window.
                    },
                );
                self.outcome = Some(result);
                event_loop.exit();
            }
            _ => {}
        }
    }
}

/// Run the `eclipse __gl-test` harness: open an Eclipse window, create a real EGL display + GLES2
/// context + window surface on it, render + present a clear-and-triangle for a few frames asserting
/// `glGetError() == GL_NO_ERROR` throughout and that every `eglSwapBuffers` succeeds, then exit.
///
/// This proves the engine's GLES2/EGL render path works on Eclipse's window **independent of Roblox**
/// — the headless success bar is 0 EGL/GL errors + successful swaps (the visible triangle is the
/// dev-host visual check). Returns the [`GlTestReport`] on success, or a typed [`EglError`].
///
/// Dev-host / main-loop only (it opens a real window + drives a GPU); not a `#[test]`.
pub fn run_gl_test() -> Result<GlTestReport, EglError> {
    let event_loop =
        EventLoop::new().map_err(|e| EglError::Display(format!("winit event loop: {e}")))?;
    let mut app = GlTestApp {
        outcome: None,
        window: None,
        create_error: None,
    };
    event_loop
        .run_app(&mut app)
        .map_err(|e| EglError::Display(format!("winit run_app: {e}")))?;
    if let Some(e) = app.create_error {
        return Err(EglError::Display(format!("failed to create window: {e}")));
    }
    app.outcome.unwrap_or(Err(EglError::Display(
        "harness produced no render outcome".into(),
    )))
}

// =================================================================================================
// `eclipse __gl-test-anw` harness — the ENGINE-STYLE WSI bind validation.
//
// 2026-06-05: this proves the engine render WSI bind WITHOUT needing the boot to reach a frame. It
// goes through the engine's exact path: obtain an `ANativeWindow*` via the BOUND
// `ANativeWindow_fromSurface` native (resolved through the Eclipse native provider, as the engine's
// relocation would), then call HOST EGL itself — `eglGetDisplay`/`eglInitialize`/`eglChooseConfig`/
// `eglCreateContext` + `eglCreateWindowSurface(display, config, the ANativeWindow as
// EGLNativeWindowType, null)` + `eglMakeCurrent` + render a triangle + `eglSwapBuffers` — exactly
// what the engine does. Eclipse OWNS the WSI window (the `EngineNativeWindow`) and does NOT pre-create
// a competing context on this path; the surface is created over the engine-supplied `ANativeWindow*`.
// Success bar: surface creation succeeds (no EGL_BAD_NATIVE_WINDOW), the `ANativeWindow*` IS the real
// WSI handle, 0 GL errors, every swap succeeds.
// =================================================================================================

/// Result of the `__gl-test-anw` engine-style render: the geometry, frames, and whether the
/// `ANativeWindow*` the bound native returned was the real WSI handle (vs the geometry-only fallback).
#[derive(Debug)]
pub struct GlAnwTestReport {
    /// The window geometry the surface was built at (Eclipse's real window size).
    pub geometry: WindowGeometry,
    /// Frames rendered + presented through the engine-supplied `ANativeWindow*`.
    pub frames: u32,
    /// True if `ANativeWindow_fromSurface` returned the real WSI native-window pointer (the host-EGL
    /// `EGLNativeWindowType`), i.e. the engine's own `eglCreateWindowSurface` lands on Eclipse's
    /// window — the load-bearing assertion of this harness.
    pub anw_is_real_wsi_handle: bool,
}

impl fmt::Display for GlAnwTestReport {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(
            f,
            "engine-style eglCreateWindowSurface(ANativeWindow) OK: surface {}x{}, {} frames presented, \
             ANativeWindow* is the real WSI handle = {}, 0 GL errors, all swaps succeeded",
            self.geometry.width, self.geometry.height, self.frames, self.anw_is_real_wsi_handle
        )
    }
}

/// The `__gl-test-anw` winit application: creates one window, builds the WSI window Eclipse owns,
/// obtains the `ANativeWindow*` via the bound native, then renders engine-style over it.
struct GlAnwTestApp {
    outcome: Option<Result<GlAnwTestReport, EglError>>,
    window: Option<Window>,
    create_error: Option<winit::error::OsError>,
}

impl GlAnwTestApp {
    /// The engine-style WSI bind on `window`: build the WSI window Eclipse owns + register it, obtain
    /// the `ANativeWindow*` via the bound native, assert it is the real WSI handle, then drive host
    /// EGL over that `ANativeWindow*` (the engine's own path) and render.
    fn render_engine_style(window: &Window) -> Result<GlAnwTestReport, EglError> {
        let display_handle = window
            .display_handle()
            .map_err(|e| EglError::Display(format!("no raw display handle: {e}")))?
            .as_raw();
        let window_handle = window
            .window_handle()
            .map_err(|e| EglError::WaylandEgl(format!("no raw window handle: {e}")))?
            .as_raw();
        let size = window.inner_size();
        let geometry = WindowGeometry::from_physical(size.width, size.height);

        // 1) Eclipse OWNS the real WSI native window (and registers it so the ANativeWindow_* natives
        //    resolve to it). Kept alive for the whole engine-style render below.
        let owned = EngineNativeWindow::new(window_handle, geometry)?;
        let real_wsi = owned.as_native_window() as usize;

        // 2) Obtain the ANativeWindow* the engine would get — through the BOUND native (resolved via
        //    the Eclipse native provider, as the engine's relocation does). It must be the real WSI
        //    handle so the engine's own eglCreateWindowSurface lands on Eclipse's window.
        let anw = crate::loader::native_provider::anativewindow_from_surface_via_provider()
            .ok_or_else(|| EglError::Display("ANativeWindow_fromSurface not bound".into()))?;
        let anw_is_real_wsi_handle = anw as usize == real_wsi && !anw.is_null();
        if !anw_is_real_wsi_handle {
            return Err(EglError::Surface(egl::Error::BadNativeWindow));
        }

        // 3) Engine-style: host EGL creates its surface over the engine-supplied ANativeWindow* (as
        //    the EGLNativeWindowType). EngineGlSurface::from_ndk_window does NOT create its own native
        //    window — it renders over `anw`, exactly as the engine's eglCreateWindowSurface does.
        let surface = EngineGlSurface::from_ndk_window(
            display_handle,
            anw as egl::NativeWindowType,
            geometry,
        )?;
        render_test_frames(&surface, GL_TEST_FRAMES)?;
        // `surface` drops here (EGL torn down), then `owned` drops (WSI window freed + unregistered).
        drop(surface);
        drop(owned);
        Ok(GlAnwTestReport {
            geometry,
            frames: GL_TEST_FRAMES,
            anw_is_real_wsi_handle,
        })
    }
}

impl ApplicationHandler for GlAnwTestApp {
    fn resumed(&mut self, event_loop: &ActiveEventLoop) {
        let attrs = Window::default_attributes()
            .with_title("Eclipse __gl-test-anw (engine WSI bind: ANativeWindow → host EGL)");
        match event_loop.create_window(attrs) {
            Ok(window) => {
                let size = window.inner_size();
                let geo = WindowGeometry::from_physical(size.width, size.height);
                crate::loader::ndk_registry::set_engine_window_geometry(geo.width, geo.height);
                window.request_redraw();
                self.window = Some(window);
            }
            Err(e) => {
                self.create_error = Some(e);
                event_loop.exit();
            }
        }
    }

    fn window_event(&mut self, event_loop: &ActiveEventLoop, _id: WindowId, event: WindowEvent) {
        match event {
            WindowEvent::CloseRequested => event_loop.exit(),
            WindowEvent::RedrawRequested if self.outcome.is_none() => {
                let result = self.window.as_ref().map_or_else(
                    || Err(EglError::UnsupportedDisplay),
                    Self::render_engine_style,
                );
                self.outcome = Some(result);
                event_loop.exit();
            }
            _ => {}
        }
    }
}

/// Run the `eclipse __gl-test-anw` harness: prove the engine render WSI bind by going through the
/// engine's exact path — obtain an `ANativeWindow*` via the bound `ANativeWindow_fromSurface` native,
/// then drive host `eglCreateWindowSurface(display, config, the ANativeWindow as EGLNativeWindowType,
/// null)` + make-current + a real triangle render + `eglSwapBuffers`, asserting the `ANativeWindow*`
/// is the real WSI handle, surface creation succeeds (no EGL_BAD_NATIVE_WINDOW), 0 GL errors, and
/// every swap succeeds. This is the real proof the engine's own `eglCreateWindowSurface(ANativeWindow)`
/// will present to Eclipse's window. Dev-host / main-loop only (opens a real window + drives a GPU).
pub fn run_gl_test_anw() -> Result<GlAnwTestReport, EglError> {
    let event_loop =
        EventLoop::new().map_err(|e| EglError::Display(format!("winit event loop: {e}")))?;
    let mut app = GlAnwTestApp {
        outcome: None,
        window: None,
        create_error: None,
    };
    event_loop
        .run_app(&mut app)
        .map_err(|e| EglError::Display(format!("winit run_app: {e}")))?;
    if let Some(e) = app.create_error {
        return Err(EglError::Display(format!("failed to create window: {e}")));
    }
    app.outcome.unwrap_or(Err(EglError::Display(
        "harness produced no render outcome".into(),
    )))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn config_attribs_request_gles2_window_rgba8888_and_terminate() {
        let a = gles2_config_attribs();
        // EGL_NONE-terminated (required by eglChooseConfig).
        assert_eq!(*a.last().unwrap(), egl::NONE);
        // Renderable type must include EGL_OPENGL_ES2_BIT.
        let pos = a.iter().position(|&v| v == egl::RENDERABLE_TYPE).unwrap();
        assert_eq!(
            a[pos + 1],
            EGL_OPENGL_ES2_BIT,
            "must request a GLES2 config"
        );
        // Surface type must be a window.
        let pos = a.iter().position(|&v| v == egl::SURFACE_TYPE).unwrap();
        assert_eq!(a[pos + 1], egl::WINDOW_BIT, "must request a window surface");
        // RGBA8888 color depth.
        for &(key, want) in &[
            (egl::RED_SIZE, 8),
            (egl::GREEN_SIZE, 8),
            (egl::BLUE_SIZE, 8),
            (egl::ALPHA_SIZE, 8),
        ] {
            let p = a.iter().position(|&v| v == key).unwrap();
            assert_eq!(a[p + 1], want, "color channel must be 8 bits");
        }
    }

    #[test]
    fn context_attribs_request_client_version_2_and_terminate() {
        let a = gles2_context_attribs();
        assert_eq!(*a.last().unwrap(), egl::NONE);
        let p = a
            .iter()
            .position(|&v| v == egl::CONTEXT_CLIENT_VERSION)
            .unwrap();
        assert_eq!(
            a[p + 1],
            2,
            "must request a GLES2 (client version 2) context"
        );
    }

    #[test]
    fn geometry_from_physical_clamps_to_at_least_one() {
        assert_eq!(
            WindowGeometry::from_physical(1280, 720),
            WindowGeometry {
                width: 1280,
                height: 720
            }
        );
        // A zero/minimized dimension clamps to 1 (EGL/wl_egl_window reject 0).
        assert_eq!(
            WindowGeometry::from_physical(0, 0),
            WindowGeometry {
                width: 1,
                height: 1
            }
        );
        assert_eq!(WindowGeometry::from_physical(800, 0).height, 1);
    }
}
