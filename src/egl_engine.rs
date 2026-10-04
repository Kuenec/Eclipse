use std::ffi::{c_char, c_void, CStr};
use std::fmt;

use crate::loader::native_provider::{HOST_EGL_SONAME, HOST_GLESV2_SONAME};
use khronos_egl as egl;
use raw_window_handle::{HasDisplayHandle, HasWindowHandle, RawDisplayHandle, RawWindowHandle};
use winit::application::ApplicationHandler;
use winit::event::WindowEvent;
use winit::event_loop::{ActiveEventLoop, EventLoop};
use winit::window::{Window, WindowAttributes, WindowId};

type EglApi = egl::EGL1_4;

pub(crate) type EglInstance = egl::DynamicInstance<EglApi>;

const EGL_OPENGL_ES2_BIT: egl::Int = 0x0004;

pub(crate) const MIN_ENGINE_EDGE: u32 = 2;

const SURFACELESS_CONFIG: [egl::Int; 5] = [
    egl::RENDERABLE_TYPE,
    EGL_OPENGL_ES2_BIT,
    egl::SURFACE_TYPE,
    0,
    egl::NONE,
];

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct WindowGeometry {
    pub width: i32,

    pub height: i32,
}

impl WindowGeometry {
    #[must_use]
    pub fn from_physical(width: u32, height: u32) -> Self {
        Self {
            width: width.clamp(MIN_ENGINE_EDGE, i32::MAX as u32) as i32,
            height: height.clamp(MIN_ENGINE_EDGE, i32::MAX as u32) as i32,
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

    SwapInterval(egl::Error),

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
            Self::SwapInterval(e) => write!(f, "eglSwapInterval(0) failed: {e}"),
            Self::UnsupportedDisplay => {
                f.write_str("unsupported display server (need Wayland or X11)")
            }
            Self::WaylandEgl(e) => write!(f, "libwayland-egl / wl_egl_window error: {e}"),
            Self::Gl(e) => write!(f, "GLES2 error: {e}"),
        }
    }
}

impl std::error::Error for EglError {}

pub(crate) fn load_host_egl() -> Result<EglInstance, EglError> {
    unsafe {
        let lib = libloading::Library::new(HOST_EGL_SONAME)
            .map_err(|e| EglError::LoadEgl(e.to_string()))?;
        EglInstance::load_required_from(lib).map_err(|e| EglError::LoadEgl(e.to_string()))
    }
}

pub(crate) fn initialized_display(
    egl: &EglInstance,
    display_handle: RawDisplayHandle,
) -> Result<egl::Display, EglError> {
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
    egl.initialize(display)
        .map_err(|e| EglError::Display(format!("eglInitialize failed: {e}")))?;
    Ok(display)
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct GlDriver {
    pub(crate) renderer: String,
    pub(crate) version: String,
}

pub(crate) fn probe_gl(display_handle: RawDisplayHandle) -> Result<GlDriver, EglError> {
    let egl = load_host_egl()?;
    let display = initialized_display(&egl, display_handle)?;
    let driver = surfaceless_gl_driver(&egl, display);
    if let Err(error) = egl.terminate(display) {
        tracing::warn!(%error, "terminating the EGL display that named the OpenGL ES driver failed");
    }
    driver
}

fn surfaceless_gl_driver(egl: &EglInstance, display: egl::Display) -> Result<GlDriver, EglError> {
    egl.bind_api(egl::OPENGL_ES_API)
        .map_err(EglError::Context)?;
    let config = egl
        .choose_first_config(display, &SURFACELESS_CONFIG)
        .map_err(|e| EglError::Display(format!("eglChooseConfig failed: {e}")))?
        .ok_or(EglError::NoConfig)?;
    let context = egl
        .create_context(display, config, None, &gles2_context_attribs())
        .map_err(EglError::Context)?;
    let driver = egl
        .make_current(display, None, None, Some(context))
        .map_err(EglError::Present)
        .and_then(|()| {
            let gl = Gles2::load(egl)?;
            unsafe {
                Ok(GlDriver {
                    renderer: gl_string(&gl, GL_RENDERER, "GL_RENDERER")?,
                    version: gl_string(&gl, GL_VERSION, "GL_VERSION")?,
                })
            }
        });
    if let Err(error) = egl.make_current(display, None, None, None) {
        tracing::warn!(%error, "releasing the OpenGL ES context that named the driver failed");
    }
    if let Err(error) = egl.destroy_context(display, context) {
        tracing::warn!(%error, "destroying the OpenGL ES context that named the driver failed");
    }
    driver
}

pub struct EngineGlSurface {
    egl: EglInstance,
    display: egl::Display,
    context: egl::Context,
    surface: egl::Surface,

    gl: Gles2,

    _native: EngineNativeWindow,
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
        let egl = load_host_egl()?;
        let display = initialized_display(&egl, display_handle)?;

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
            _native: native,
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
    backing: NativeWindowBacking,

    publication: Publication,

    native_window: *mut c_void,
    geometry: WindowGeometry,
}

enum NativeWindowBacking {
    Wayland(WaylandEglWindow),

    X11,

    Borrowed,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Publication {
    Engine,

    Private,
}

impl EngineNativeWindow {
    pub fn new(window_handle: RawWindowHandle, geometry: WindowGeometry) -> Result<Self, EglError> {
        let mut window = Self::private(window_handle, geometry)?;
        window.publication = Publication::Engine;
        crate::loader::ndk_registry::register_wsi_window(
            window.native_window as usize,
            geometry.width,
            geometry.height,
        );
        Ok(window)
    }

    pub(crate) fn private(
        window_handle: RawWindowHandle,
        geometry: WindowGeometry,
    ) -> Result<Self, EglError> {
        match window_handle {
            RawWindowHandle::Wayland(w) => {
                let wl = WaylandEglWindow::new(w.surface.as_ptr(), geometry)?;
                let native_window = wl.window;
                Ok(Self {
                    backing: NativeWindowBacking::Wayland(wl),
                    publication: Publication::Private,
                    native_window,
                    geometry,
                })
            }
            RawWindowHandle::Xlib(w) => Ok(Self {
                backing: NativeWindowBacking::X11,
                publication: Publication::Private,
                native_window: w.window as *mut c_void,
                geometry,
            }),
            _ => Err(EglError::UnsupportedDisplay),
        }
    }

    #[must_use]
    pub fn borrowed(native_window: egl::NativeWindowType, geometry: WindowGeometry) -> Self {
        Self {
            backing: NativeWindowBacking::Borrowed,
            publication: Publication::Private,
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

    pub fn resize(&mut self, geometry: WindowGeometry) {
        match &self.backing {
            NativeWindowBacking::Wayland(wl) => wl.resize(geometry),
            NativeWindowBacking::X11 | NativeWindowBacking::Borrowed => {}
        }
        self.geometry = geometry;
    }
}

impl Drop for EngineNativeWindow {
    fn drop(&mut self) {
        match self.publication {
            Publication::Engine => {
                crate::loader::ndk_registry::unregister_wsi_window(self.native_window as usize);
            }
            Publication::Private => {}
        }
    }
}

struct WaylandEglWindow {
    _lib: libloading::Library,

    window: *mut c_void,

    resize: unsafe extern "C" fn(*mut c_void, i32, i32, i32, i32),

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
            let resize: libloading::Symbol<unsafe extern "C" fn(*mut c_void, i32, i32, i32, i32)> =
                lib.get(b"wl_egl_window_resize\0")
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
            let resize = *resize;
            let destroy = *destroy;
            Ok(Self {
                _lib: lib,
                window,
                resize,
                destroy,
            })
        }
    }

    fn resize(&self, geometry: WindowGeometry) {
        unsafe { (self.resize)(self.window, geometry.width, geometry.height, 0, 0) }
    }
}

impl Drop for WaylandEglWindow {
    fn drop(&mut self) {
        unsafe { (self.destroy)(self.window) }
    }
}

const GL_NO_ERROR: u32 = 0;

pub(crate) const GL_RENDERER: u32 = 0x1F01;
pub(crate) const GL_VERSION: u32 = 0x1F02;

pub const GL_COLOR_BUFFER_BIT: u32 = 0x0000_4000;

pub const GL_VERTEX_SHADER: u32 = 0x8B31;
pub const GL_FRAGMENT_SHADER: u32 = 0x8B30;

const GL_COMPILE_STATUS: u32 = 0x8B81;
const GL_LINK_STATUS: u32 = 0x8B82;

pub(crate) const GL_FLOAT: u32 = 0x1406;
pub const GL_TRIANGLES: u32 = 0x0004;
pub(crate) const GL_FALSE: u8 = 0;
pub(crate) const GL_TRUE: u8 = 1;
pub(crate) const GL_ARRAY_BUFFER: u32 = 0x8892;
pub(crate) const GL_STREAM_DRAW: u32 = 0x88E0;
pub(crate) const GL_TEXTURE_2D: u32 = 0x0DE1;
pub(crate) const GL_TEXTURE_MAG_FILTER: u32 = 0x2800;
pub(crate) const GL_TEXTURE_MIN_FILTER: u32 = 0x2801;
pub(crate) const GL_TEXTURE_WRAP_S: u32 = 0x2802;
pub(crate) const GL_TEXTURE_WRAP_T: u32 = 0x2803;
pub(crate) const GL_CLAMP_TO_EDGE: i32 = 0x812F;
pub(crate) const GL_RGBA: u32 = 0x1908;
pub(crate) const GL_UNSIGNED_BYTE: u32 = 0x1401;
pub(crate) const GL_BLEND: u32 = 0x0BE2;
pub(crate) const GL_ONE_MINUS_SRC_ALPHA: u32 = 0x0303;

type PfnGlGetError = unsafe extern "C" fn() -> u32;
type PfnGlClearColor = unsafe extern "C" fn(f32, f32, f32, f32);
type PfnGlClear = unsafe extern "C" fn(u32);
type PfnGlViewport = unsafe extern "C" fn(i32, i32, i32, i32);
type PfnGlCreateShader = unsafe extern "C" fn(u32) -> u32;
type PfnGlShaderSource = unsafe extern "C" fn(u32, i32, *const *const c_char, *const i32);
type PfnGlCompileShader = unsafe extern "C" fn(u32);
type PfnGlGetShaderiv = unsafe extern "C" fn(u32, u32, *mut i32);
type PfnGlCreateProgram = unsafe extern "C" fn() -> u32;
type PfnGlAttachShader = unsafe extern "C" fn(u32, u32);
type PfnGlLinkProgram = unsafe extern "C" fn(u32);
type PfnGlGetProgramiv = unsafe extern "C" fn(u32, u32, *mut i32);
type PfnGlUseProgram = unsafe extern "C" fn(u32);
type PfnGlGetAttribLocation = unsafe extern "C" fn(u32, *const c_char) -> i32;
type PfnGlEnableVertexAttribArray = unsafe extern "C" fn(u32);
type PfnGlVertexAttribPointer = unsafe extern "C" fn(u32, i32, u32, u8, i32, *const c_void);
type PfnGlDrawArrays = unsafe extern "C" fn(u32, i32, i32);
type PfnGlDeleteShader = unsafe extern "C" fn(u32);
type PfnGlDeleteProgram = unsafe extern "C" fn(u32);
type PfnGlGetUniformLocation = unsafe extern "C" fn(u32, *const c_char) -> i32;
type PfnGlUniform4f = unsafe extern "C" fn(i32, f32, f32, f32, f32);
type PfnGlGenObjects = unsafe extern "C" fn(i32, *mut u32);
type PfnGlDeleteObjects = unsafe extern "C" fn(i32, *const u32);
type PfnGlBindObject = unsafe extern "C" fn(u32, u32);
type PfnGlBufferData = unsafe extern "C" fn(u32, isize, *const c_void, u32);
type PfnGlTexImage2D = unsafe extern "C" fn(u32, i32, i32, i32, i32, i32, u32, u32, *const c_void);
type PfnGlTexSubImage2D =
    unsafe extern "C" fn(u32, i32, i32, i32, i32, i32, u32, u32, *const c_void);
type PfnGlTexParameteri = unsafe extern "C" fn(u32, u32, i32);
type PfnGlPixelStorei = unsafe extern "C" fn(u32, i32);
type PfnGlEnable = unsafe extern "C" fn(u32);
type PfnGlBlendFunc = unsafe extern "C" fn(u32, u32);
type PfnGlColorMask = unsafe extern "C" fn(u8, u8, u8, u8);
type PfnGlGetString = unsafe extern "C" fn(u32) -> *const c_char;
type PfnGlReadPixels = unsafe extern "C" fn(i32, i32, i32, i32, u32, u32, *mut c_void);

pub struct Gles2 {
    _lib: libloading::Library,
    pub(crate) gl_get_error: PfnGlGetError,
    pub(crate) gl_clear_color: PfnGlClearColor,
    pub(crate) gl_clear: PfnGlClear,
    pub(crate) gl_viewport: PfnGlViewport,
    gl_create_shader: PfnGlCreateShader,
    gl_shader_source: PfnGlShaderSource,
    gl_compile_shader: PfnGlCompileShader,
    gl_get_shaderiv: PfnGlGetShaderiv,
    gl_create_program: PfnGlCreateProgram,
    gl_attach_shader: PfnGlAttachShader,
    gl_link_program: PfnGlLinkProgram,
    gl_get_programiv: PfnGlGetProgramiv,
    pub(crate) gl_use_program: PfnGlUseProgram,
    pub(crate) gl_get_attrib_location: PfnGlGetAttribLocation,
    pub(crate) gl_enable_vertex_attrib_array: PfnGlEnableVertexAttribArray,
    pub(crate) gl_vertex_attrib_pointer: PfnGlVertexAttribPointer,
    pub(crate) gl_draw_arrays: PfnGlDrawArrays,
    gl_delete_shader: PfnGlDeleteShader,
    gl_delete_program: PfnGlDeleteProgram,
    pub(crate) gl_get_uniform_location: PfnGlGetUniformLocation,
    pub(crate) gl_uniform_4f: PfnGlUniform4f,
    pub(crate) gl_gen_buffers: PfnGlGenObjects,
    pub(crate) gl_bind_buffer: PfnGlBindObject,
    pub(crate) gl_buffer_data: PfnGlBufferData,
    pub(crate) gl_gen_textures: PfnGlGenObjects,
    pub(crate) gl_delete_textures: PfnGlDeleteObjects,
    pub(crate) gl_bind_texture: PfnGlBindObject,
    pub(crate) gl_tex_image_2d: PfnGlTexImage2D,
    pub(crate) gl_tex_sub_image_2d: PfnGlTexSubImage2D,
    pub(crate) gl_tex_parameteri: PfnGlTexParameteri,
    pub(crate) gl_pixel_storei: PfnGlPixelStorei,
    pub(crate) gl_enable: PfnGlEnable,
    pub(crate) gl_blend_func: PfnGlBlendFunc,
    pub(crate) gl_color_mask: PfnGlColorMask,
    pub(crate) gl_get_string: PfnGlGetString,
    pub(crate) gl_read_pixels: PfnGlReadPixels,
}

impl Gles2 {
    pub(crate) fn load(egl: &EglInstance) -> Result<Self, EglError> {
        unsafe {
            let lib = libloading::Library::new(HOST_GLESV2_SONAME)
                .map_err(|e| EglError::Gl(format!("no {HOST_GLESV2_SONAME}: {e}")))?;

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
                gl_get_error: load_fn!("glGetError", PfnGlGetError),
                gl_clear_color: load_fn!("glClearColor", PfnGlClearColor),
                gl_clear: load_fn!("glClear", PfnGlClear),
                gl_viewport: load_fn!("glViewport", PfnGlViewport),
                gl_create_shader: load_fn!("glCreateShader", PfnGlCreateShader),
                gl_shader_source: load_fn!("glShaderSource", PfnGlShaderSource),
                gl_compile_shader: load_fn!("glCompileShader", PfnGlCompileShader),
                gl_get_shaderiv: load_fn!("glGetShaderiv", PfnGlGetShaderiv),
                gl_create_program: load_fn!("glCreateProgram", PfnGlCreateProgram),
                gl_attach_shader: load_fn!("glAttachShader", PfnGlAttachShader),
                gl_link_program: load_fn!("glLinkProgram", PfnGlLinkProgram),
                gl_get_programiv: load_fn!("glGetProgramiv", PfnGlGetProgramiv),
                gl_use_program: load_fn!("glUseProgram", PfnGlUseProgram),
                gl_get_attrib_location: load_fn!("glGetAttribLocation", PfnGlGetAttribLocation),
                gl_enable_vertex_attrib_array: load_fn!(
                    "glEnableVertexAttribArray",
                    PfnGlEnableVertexAttribArray
                ),
                gl_vertex_attrib_pointer: load_fn!(
                    "glVertexAttribPointer",
                    PfnGlVertexAttribPointer
                ),
                gl_draw_arrays: load_fn!("glDrawArrays", PfnGlDrawArrays),
                gl_delete_shader: load_fn!("glDeleteShader", PfnGlDeleteShader),
                gl_delete_program: load_fn!("glDeleteProgram", PfnGlDeleteProgram),
                gl_get_uniform_location: load_fn!("glGetUniformLocation", PfnGlGetUniformLocation),
                gl_uniform_4f: load_fn!("glUniform4f", PfnGlUniform4f),
                gl_gen_buffers: load_fn!("glGenBuffers", PfnGlGenObjects),
                gl_bind_buffer: load_fn!("glBindBuffer", PfnGlBindObject),
                gl_buffer_data: load_fn!("glBufferData", PfnGlBufferData),
                gl_gen_textures: load_fn!("glGenTextures", PfnGlGenObjects),
                gl_delete_textures: load_fn!("glDeleteTextures", PfnGlDeleteObjects),
                gl_bind_texture: load_fn!("glBindTexture", PfnGlBindObject),
                gl_tex_image_2d: load_fn!("glTexImage2D", PfnGlTexImage2D),
                gl_tex_sub_image_2d: load_fn!("glTexSubImage2D", PfnGlTexSubImage2D),
                gl_tex_parameteri: load_fn!("glTexParameteri", PfnGlTexParameteri),
                gl_pixel_storei: load_fn!("glPixelStorei", PfnGlPixelStorei),
                gl_enable: load_fn!("glEnable", PfnGlEnable),
                gl_blend_func: load_fn!("glBlendFunc", PfnGlBlendFunc),
                gl_color_mask: load_fn!("glColorMask", PfnGlColorMask),
                gl_get_string: load_fn!("glGetString", PfnGlGetString),
                gl_read_pixels: load_fn!("glReadPixels", PfnGlReadPixels),
                _lib: lib,
            })
        }
    }

    fn get_error(&self) -> u32 {
        unsafe { (self.gl_get_error)() }
    }

    pub(crate) fn check(&self, op: &str) -> Result<(), EglError> {
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
    surface
        .egl
        .swap_interval(surface.display, 0)
        .map_err(EglError::SwapInterval)?;

    const VERT_SRC: &[u8] =
        b"attribute vec2 aPos;\nvoid main(){gl_Position=vec4(aPos,0.0,1.0);}\n\0";
    const FRAG_SRC: &[u8] =
        b"precision mediump float;\nvoid main(){gl_FragColor=vec4(0.149,0.408,0.722,1.0);}\n\0";

    unsafe {
        let program = compile_program(gl, VERT_SRC, FRAG_SRC)?;
        let pos_loc = (gl.gl_get_attrib_location)(program, c"aPos".as_ptr());
        gl.check("glGetAttribLocation")?;
        if pos_loc < 0 {
            (gl.gl_delete_program)(program);
            return Err(EglError::Gl("aPos attribute not found".into()));
        }
        let pos_loc = pos_loc as u32;

        let verts: [f32; 6] = [0.0, 0.5, -0.5, -0.5, 0.5, -0.5];

        (gl.gl_viewport)(0, 0, geo.width, geo.height);
        gl.check("glViewport")?;

        for _ in 0..frames {
            (gl.gl_clear_color)(0.05, 0.05, 0.08, 1.0);
            (gl.gl_clear)(GL_COLOR_BUFFER_BIT);
            gl.check("glClear")?;

            (gl.gl_use_program)(program);
            (gl.gl_enable_vertex_attrib_array)(pos_loc);
            (gl.gl_vertex_attrib_pointer)(
                pos_loc,
                2,
                GL_FLOAT,
                GL_FALSE,
                0,
                verts.as_ptr() as *const c_void,
            );
            (gl.gl_draw_arrays)(GL_TRIANGLES, 0, 3);
            gl.check("glDrawArrays")?;

            surface.swap_buffers()?;
        }

        (gl.gl_delete_program)(program);
        gl.check("frame loop")?;
    }
    Ok(())
}

pub(crate) unsafe fn gl_string(gl: &Gles2, name: u32, label: &str) -> Result<String, EglError> {
    let text = unsafe { (gl.gl_get_string)(name) };
    if text.is_null() {
        return Err(EglError::Gl(format!("glGetString({label}) returned NULL")));
    }
    Ok(unsafe { CStr::from_ptr(text) }
        .to_string_lossy()
        .into_owned())
}

pub(crate) unsafe fn compile_program(
    gl: &Gles2,
    vert: &[u8],
    frag: &[u8],
) -> Result<u32, EglError> {
    unsafe {
        let vs = compile_shader(gl, GL_VERTEX_SHADER, vert)?;
        let fs = match compile_shader(gl, GL_FRAGMENT_SHADER, frag) {
            Ok(fs) => fs,
            Err(e) => {
                (gl.gl_delete_shader)(vs);
                return Err(e);
            }
        };
        let program = (gl.gl_create_program)();
        (gl.gl_attach_shader)(program, vs);
        (gl.gl_attach_shader)(program, fs);
        (gl.gl_link_program)(program);

        (gl.gl_delete_shader)(vs);
        (gl.gl_delete_shader)(fs);
        let mut linked: i32 = 0;
        (gl.gl_get_programiv)(program, GL_LINK_STATUS, &mut linked);
        if linked == 0 {
            (gl.gl_delete_program)(program);
            return Err(EglError::Gl("program link failed".into()));
        }
        gl.check("link program")?;
        Ok(program)
    }
}

unsafe fn compile_shader(gl: &Gles2, kind: u32, src: &[u8]) -> Result<u32, EglError> {
    unsafe {
        let shader = (gl.gl_create_shader)(kind);
        if shader == 0 {
            return Err(EglError::Gl("glCreateShader returned 0".into()));
        }
        let ptr = src.as_ptr().cast::<c_char>();
        let ptrs = [ptr];
        (gl.gl_shader_source)(shader, 1, ptrs.as_ptr(), std::ptr::null());
        (gl.gl_compile_shader)(shader);
        let mut status: i32 = 0;
        (gl.gl_get_shaderiv)(shader, GL_COMPILE_STATUS, &mut status);
        if status == 0 {
            (gl.gl_delete_shader)(shader);
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

const GL_TEST_WINDOW: &str = "__gl-test (engine GLES2/EGL)";

const GL_TEST_ANW_WINDOW: &str = "__gl-test-anw (engine WSI bind: ANativeWindow → host EGL)";

fn test_window(subject: &str) -> WindowAttributes {
    Window::default_attributes().with_title(crate::window_title(subject))
}

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
        let attrs = test_window(GL_TEST_WINDOW);
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
        let attrs = test_window(GL_TEST_ANW_WINDOW);
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
    fn gl_test_windows_carry_the_title_prefix_of_every_eclipse_window() {
        for subject in [GL_TEST_WINDOW, GL_TEST_ANW_WINDOW] {
            let title = test_window(subject).title;
            assert_eq!(title, format!("Eclipse — {subject}"));
        }
    }

    #[test]
    fn geometry_from_physical_keeps_every_edge_at_two_pixels_or_more() {
        assert_eq!(
            WindowGeometry::from_physical(1280, 720),
            WindowGeometry {
                width: 1280,
                height: 720
            }
        );
        assert_eq!(
            WindowGeometry::from_physical(2, 2),
            WindowGeometry {
                width: 2,
                height: 2
            }
        );

        for (width, height) in [(0, 0), (1, 1)] {
            assert_eq!(
                WindowGeometry::from_physical(width, height),
                WindowGeometry {
                    width: 2,
                    height: 2
                },
                "Roblox halves the surface for some targets and asserts on a 0-pixel texture"
            );
        }
        assert_eq!(WindowGeometry::from_physical(800, 1).height, 2);
        assert_eq!(WindowGeometry::from_physical(1, 600).width, 2);
    }

    #[repr(C)]
    struct WlEglWindowHead {
        version: isize,
        width: i32,
        height: i32,
    }

    fn wl_egl_window_size(window: &EngineNativeWindow) -> (i32, i32) {
        let head = unsafe { &*window.as_native_window().cast::<WlEglWindowHead>() };
        assert!(
            head.version >= 3,
            "unexpected wl_egl_window ABI {}",
            head.version
        );
        (head.width, head.height)
    }

    #[test]
    fn wayland_engine_window_resize_resizes_the_wl_egl_window() {
        let sentinel_surface = 0x1000 as *mut c_void;
        let initial = WindowGeometry {
            width: 800,
            height: 600,
        };
        let wl = match WaylandEglWindow::new(sentinel_surface, initial) {
            Ok(wl) => wl,
            Err(e) => {
                eprintln!("SKIP: libwayland-egl.so.1 unavailable ({e})");
                return;
            }
        };
        let native_window = wl.window;
        let mut window = EngineNativeWindow {
            backing: NativeWindowBacking::Wayland(wl),
            publication: Publication::Private,
            native_window,
            geometry: initial,
        };
        assert_eq!(wl_egl_window_size(&window), (800, 600));

        let resized = WindowGeometry {
            width: 1280,
            height: 720,
        };
        window.resize(resized);

        assert_eq!(
            wl_egl_window_size(&window),
            (1280, 720),
            "EGL sizes Wayland window-surface buffers from the wl_egl_window, \
             so it must follow the window"
        );
        assert_eq!(window.geometry(), resized);
    }

    #[test]
    fn borrowed_engine_window_resize_only_updates_its_geometry() {
        let mut window = EngineNativeWindow::borrowed(
            0x2000 as egl::NativeWindowType,
            WindowGeometry {
                width: 640,
                height: 480,
            },
        );
        let resized = WindowGeometry {
            width: 1024,
            height: 768,
        };
        window.resize(resized);
        assert_eq!(window.geometry(), resized);
        assert_eq!(window.as_native_window() as usize, 0x2000);
    }
}
