use std::ffi::c_void;
use std::fmt;

use khronos_egl as egl;
use raw_window_handle::{HasDisplayHandle, HasWindowHandle, RawDisplayHandle, RawWindowHandle};
use winit::application::ApplicationHandler;
use winit::event::WindowEvent;
use winit::event_loop::{ActiveEventLoop, EventLoop};
use winit::window::{Window, WindowId};

type EglApi = egl::EGL1_4;

type EglInstance = egl::DynamicInstance<EglApi>;

const EGL_OPENGL_ES2_BIT: egl::Int = 0x0004;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct WindowGeometry {
    pub width: i32,

    pub height: i32,
}

impl WindowGeometry {
    #[must_use]
    pub fn from_physical(width: u32, height: u32) -> Self {
        Self {
            width: width.max(1).min(i32::MAX as u32) as i32,
            height: height.max(1).min(i32::MAX as u32) as i32,
        }
    }
}

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

#[must_use]
pub fn gles2_context_attribs() -> [egl::Int; 3] {
    [egl::CONTEXT_CLIENT_VERSION, 2, egl::NONE]
}

#[derive(Debug)]
pub enum EglError {
    LoadEgl(String),

    Display(String),

    NoConfig,

    Context(egl::Error),

    Surface(egl::Error),

    Present(egl::Error),

    UnsupportedDisplay,

    WaylandEgl(String),

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

pub struct EngineGlSurface {
    egl: EglInstance,
    display: egl::Display,
    context: egl::Context,
    surface: egl::Surface,

    gl: Gles2,

    #[allow(dead_code)]
    native: EngineNativeWindow,
    geometry: WindowGeometry,
}

impl EngineGlSurface {
    pub fn new(
        display_handle: RawDisplayHandle,
        window_handle: RawWindowHandle,
        geometry: WindowGeometry,
    ) -> Result<Self, EglError> {
        let native = EngineNativeWindow::new(window_handle, geometry)?;
        Self::build(display_handle, native, geometry)
    }

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

    fn build(
        display_handle: RawDisplayHandle,
        native: EngineNativeWindow,
        geometry: WindowGeometry,
    ) -> Result<Self, EglError> {
        let egl = unsafe {
            let lib = libloading::Library::new("libEGL.so.1")
                .map_err(|e| EglError::LoadEgl(e.to_string()))?;
            EglInstance::load_required_from(lib).map_err(|e| EglError::LoadEgl(e.to_string()))?
        };

        let native_display: egl::NativeDisplayType = match display_handle {
            RawDisplayHandle::Wayland(d) => d.display.as_ptr(),
            RawDisplayHandle::Xlib(d) => match d.display {
                Some(p) => p.as_ptr(),
                None => egl::DEFAULT_DISPLAY,
            },
            _ => return Err(EglError::UnsupportedDisplay),
        };

        let display = unsafe { egl.get_display(native_display) }
            .ok_or_else(|| EglError::Display("eglGetDisplay returned EGL_NO_DISPLAY".into()))?;
        let (_major, _minor) = egl
            .initialize(display)
            .map_err(|e| EglError::Display(format!("eglInitialize failed: {e}")))?;

        let config = egl
            .choose_first_config(display, &gles2_config_attribs())
            .map_err(|e| EglError::Display(format!("eglChooseConfig failed: {e}")))?
            .ok_or(EglError::NoConfig)?;

        egl.bind_api(egl::OPENGL_ES_API)
            .map_err(EglError::Context)?;
        let context = egl
            .create_context(display, config, None, &gles2_context_attribs())
            .map_err(EglError::Context)?;

        let surface = unsafe {
            egl.create_window_surface(display, config, native.as_native_window(), None)
                .map_err(EglError::Surface)?
        };

        egl.make_current(display, Some(surface), Some(surface), Some(context))
            .map_err(EglError::Present)?;

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

    #[must_use]
    pub fn geometry(&self) -> WindowGeometry {
        self.geometry
    }

    pub fn swap_buffers(&self) -> Result<(), EglError> {
        self.egl
            .swap_buffers(self.display, self.surface)
            .map_err(EglError::Present)
    }

    #[must_use]
    pub fn gl(&self) -> &Gles2 {
        &self.gl
    }
}

impl Drop for EngineGlSurface {
    fn drop(&mut self) {
        let _ = self.egl.make_current(self.display, None, None, None);
        let _ = self.egl.destroy_surface(self.display, self.surface);
        let _ = self.egl.destroy_context(self.display, self.context);
    }
}

pub struct EngineNativeWindow {
    #[allow(dead_code)]
    backing: NativeWindowBacking,

    native_window: *mut c_void,
    geometry: WindowGeometry,
}

enum NativeWindowBacking {
    Wayland(#[allow(dead_code)] WaylandEglWindow),

    X11,

    Borrowed,
}

impl EngineNativeWindow {
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

                native_window: w.window as *mut c_void,
                geometry,
            },
            _ => return Err(EglError::UnsupportedDisplay),
        };

        crate::loader::ndk_registry::register_wsi_window(
            window.native_window as usize,
            geometry.width,
            geometry.height,
        );
        Ok(window)
    }

    #[must_use]
    pub fn borrowed(native_window: egl::NativeWindowType, geometry: WindowGeometry) -> Self {
        Self {
            backing: NativeWindowBacking::Borrowed,
            native_window,
            geometry,
        }
    }

    #[must_use]
    pub fn as_native_window(&self) -> egl::NativeWindowType {
        self.native_window
    }

    #[must_use]
    pub fn geometry(&self) -> WindowGeometry {
        self.geometry
    }
}

impl Drop for EngineNativeWindow {
    fn drop(&mut self) {
        if matches!(self.backing, NativeWindowBacking::Borrowed) {
            return;
        }

        crate::loader::ndk_registry::unregister_wsi_window(self.native_window as usize);
    }
}

struct WaylandEglWindow {
    _lib: libloading::Library,

    window: *mut c_void,

    destroy: unsafe extern "C" fn(*mut c_void),
}

impl WaylandEglWindow {
    fn new(wl_surface: *mut c_void, geometry: WindowGeometry) -> Result<Self, EglError> {
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
        unsafe { (self.destroy)(self.window) }
    }
}

const GL_NO_ERROR: u32 = 0;

pub const GL_COLOR_BUFFER_BIT: u32 = 0x0000_4000;

pub const GL_VERTEX_SHADER: u32 = 0x8B31;
pub const GL_FRAGMENT_SHADER: u32 = 0x8B30;

const GL_COMPILE_STATUS: u32 = 0x8B81;
const GL_LINK_STATUS: u32 = 0x8B82;

const GL_FLOAT: u32 = 0x1406;
pub const GL_TRIANGLES: u32 = 0x0004;
const GL_FALSE: u8 = 0;

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

#[allow(non_snake_case)]
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
    fn load(egl: &EglInstance) -> Result<Self, EglError> {
        unsafe {
            let lib = libloading::Library::new("libGLESv2.so.2")
                .map_err(|e| EglError::Gl(format!("no libGLESv2.so.2: {e}")))?;

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

    fn get_error(&self) -> u32 {
        unsafe { (self.glGetError)() }
    }

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

pub fn render_test_frames(surface: &EngineGlSurface, frames: u32) -> Result<(), EglError> {
    let gl = surface.gl();
    let geo = surface.geometry();

    const VERT_SRC: &[u8] =
        b"attribute vec2 aPos;\nvoid main(){gl_Position=vec4(aPos,0.0,1.0);}\n\0";
    const FRAG_SRC: &[u8] =
        b"precision mediump float;\nvoid main(){gl_FragColor=vec4(0.149,0.408,0.722,1.0);}\n\0";

    unsafe {
        let program = compile_program(gl, VERT_SRC, FRAG_SRC)?;
        let pos_loc = (gl.glGetAttribLocation)(program, c"aPos".as_ptr());
        gl.check("glGetAttribLocation")?;
        if pos_loc < 0 {
            (gl.glDeleteProgram)(program);
            return Err(EglError::Gl("aPos attribute not found".into()));
        }
        let pos_loc = pos_loc as u32;

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

unsafe fn compile_program(gl: &Gles2, vert: &[u8], frag: &[u8]) -> Result<u32, EglError> {
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

unsafe fn compile_shader(gl: &Gles2, kind: u32, src: &[u8]) -> Result<u32, EglError> {
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

const _: u8 = GL_FALSE;

const GL_TEST_FRAMES: u32 = 5;

#[derive(Debug)]
pub struct GlTestReport {
    pub geometry: WindowGeometry,

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

struct GlTestApp {
    outcome: Option<Result<GlTestReport, EglError>>,

    window: Option<Window>,

    create_error: Option<winit::error::OsError>,
}

impl ApplicationHandler for GlTestApp {
    fn resumed(&mut self, event_loop: &ActiveEventLoop) {
        let attrs = Window::default_attributes().with_title("Eclipse __gl-test (engine GLES2/EGL)");
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
                    |window| {
                        let surface = EngineGlSurface::from_window(window)?;
                        let geometry = surface.geometry();
                        render_test_frames(&surface, GL_TEST_FRAMES)?;
                        Ok(GlTestReport {
                            geometry,
                            frames: GL_TEST_FRAMES,
                        })
                    },
                );
                self.outcome = Some(result);
                event_loop.exit();
            }
            _ => {}
        }
    }
}

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

#[derive(Debug)]
pub struct GlAnwTestReport {
    pub geometry: WindowGeometry,

    pub frames: u32,

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

struct GlAnwTestApp {
    outcome: Option<Result<GlAnwTestReport, EglError>>,
    window: Option<Window>,
    create_error: Option<winit::error::OsError>,
}

impl GlAnwTestApp {
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

        let owned = EngineNativeWindow::new(window_handle, geometry)?;
        let real_wsi = owned.as_native_window() as usize;

        let anw = crate::loader::native_provider::anativewindow_from_surface_via_provider()
            .ok_or_else(|| EglError::Display("ANativeWindow_fromSurface not bound".into()))?;
        let anw_is_real_wsi_handle = anw as usize == real_wsi && !anw.is_null();
        if !anw_is_real_wsi_handle {
            return Err(EglError::Surface(egl::Error::BadNativeWindow));
        }

        let surface = EngineGlSurface::from_ndk_window(
            display_handle,
            anw as egl::NativeWindowType,
            geometry,
        )?;
        render_test_frames(&surface, GL_TEST_FRAMES)?;

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

        assert_eq!(*a.last().unwrap(), egl::NONE);

        let pos = a.iter().position(|&v| v == egl::RENDERABLE_TYPE).unwrap();
        assert_eq!(
            a[pos + 1],
            EGL_OPENGL_ES2_BIT,
            "must request a GLES2 config"
        );

        let pos = a.iter().position(|&v| v == egl::SURFACE_TYPE).unwrap();
        assert_eq!(a[pos + 1], egl::WINDOW_BIT, "must request a window surface");

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
