use std::ffi::{c_ulong, c_void, CStr};
use std::fmt;

use ash::vk;
use khronos_egl as egl;
use raw_window_handle::{HasDisplayHandle, HasWindowHandle, RawDisplayHandle, RawWindowHandle};
use winit::dpi::PhysicalSize;
use winit::window::Window;

use super::{
    build_quad_vertices, build_text_vertices, composite_quad_vertices, host_glyph_atlas,
    layout_views, GlyphAtlas, LaidOutView, TextMeasure, TextVertex, CLEAR_COLOR,
    MAX_COMPOSITE_VIEWS, TEXT_COLOR, TEXT_PX,
};
use crate::egl_engine::{
    self, compile_program, gl_string, EglError, EglInstance, EngineNativeWindow, Gles2,
    WindowGeometry, GL_ARRAY_BUFFER, GL_BLEND, GL_CLAMP_TO_EDGE, GL_COLOR_BUFFER_BIT, GL_FALSE,
    GL_FLOAT, GL_ONE_MINUS_SRC_ALPHA, GL_RENDERER, GL_RGBA, GL_STREAM_DRAW, GL_TEXTURE_2D,
    GL_TEXTURE_MAG_FILTER, GL_TEXTURE_MIN_FILTER, GL_TEXTURE_WRAP_S, GL_TEXTURE_WRAP_T,
    GL_TRIANGLES, GL_TRUE, GL_UNSIGNED_BYTE, GL_VERSION,
};
use crate::framework::view_registry::RenderNode;
use crate::framework::{canvas_registry, DrawnCanvas};

const GL_LINEAR: i32 = 0x2601;
const GL_ALPHA: u32 = 0x1906;
const GL_UNPACK_ALIGNMENT: u32 = 0x0CF5;
const GL_SRC_ALPHA: u32 = 0x0302;

const WINDOW_CONFIG: [egl::Int; 5] = [
    egl::SURFACE_TYPE,
    egl::WINDOW_BIT,
    egl::RENDERABLE_TYPE,
    egl::OPENGL_ES2_BIT,
    egl::NONE,
];

const FLOAT_BYTES: usize = std::mem::size_of::<f32>();
const POSITION_FLOATS: i32 = 2;
const COLOR_FLOATS: i32 = 4;
const UV_FLOATS: i32 = 2;
const CANVAS_VERTICES: i32 = 6;

const SOLID_VERTEX: &str = "attribute vec2 aPos;
attribute vec4 aColor;
varying vec4 vColor;
void main() {
  gl_Position = vec4(aPos.x, -aPos.y, 0.0, 1.0);
  vColor = aColor;
}
\0";

const TEXTURED_VERTEX: &str = "attribute vec2 aPos;
attribute vec2 aUv;
varying vec2 vUv;
void main() {
  gl_Position = vec4(aPos.x, -aPos.y, 0.0, 1.0);
  vUv = aUv;
}
\0";

macro_rules! fragment_shader {
    ($body:literal) => {
        concat!(
            "#ifdef GL_FRAGMENT_PRECISION_HIGH\n",
            "precision highp float;\n",
            "#else\n",
            "precision mediump float;\n",
            "#endif\n",
            $body,
            "\0"
        )
    };
}

const SOLID_FRAGMENT: &str = fragment_shader!(
    "varying vec4 vColor;
void main() {
  gl_FragColor = vColor;
}
"
);

const GLYPH_FRAGMENT: &str = fragment_shader!(
    "uniform sampler2D uTexture;
uniform vec4 uColor;
varying vec2 vUv;
void main() {
  gl_FragColor = vec4(uColor.rgb, uColor.a * texture2D(uTexture, vUv).a);
}
"
);

const CANVAS_FRAGMENT: &str = fragment_shader!(
    "uniform sampler2D uTexture;
varying vec2 vUv;
void main() {
  gl_FragColor = texture2D(uTexture, vUv);
}
"
);

pub(super) struct GlesRenderer {
    glyphs: Option<Glyphs>,
    canvases: Vec<CanvasTexture>,
    drawn_canvases: Vec<DrawnCanvas>,
    programs: Programs,
    vertex_buffer: u32,
    extent: vk::Extent2D,
    renderer: String,
    version: String,
    window: EglWindow,
    gl: Gles2,
}

impl GlesRenderer {
    pub(super) fn new(
        window: &Window,
        engine_window: Option<&EngineNativeWindow>,
    ) -> Result<Self, EglError> {
        let display_handle = window
            .display_handle()
            .map_err(|e| EglError::Display(format!("no raw display handle: {e}")))?
            .as_raw();
        let window_handle = window
            .window_handle()
            .map_err(|e| EglError::WaylandEgl(format!("no raw window handle: {e}")))?
            .as_raw();
        let size = window.inner_size();
        match engine_window {
            Some(engine) => {
                let geometry = WindowGeometry::from_physical(size.width, size.height);
                let native = EngineNativeWindow::borrowed(engine.as_native_window(), geometry);
                Self::create(display_handle, window_handle, native, size)
            }
            None => Self::on_own_window(display_handle, window_handle, size),
        }
    }

    pub(super) fn on_own_window(
        display_handle: RawDisplayHandle,
        window_handle: RawWindowHandle,
        size: PhysicalSize<u32>,
    ) -> Result<Self, EglError> {
        let geometry = WindowGeometry::from_physical(size.width, size.height);
        let native = EngineNativeWindow::private(window_handle, geometry)?;
        Self::create(display_handle, window_handle, native, size)
    }

    fn create(
        display_handle: RawDisplayHandle,
        window_handle: RawWindowHandle,
        native: EngineNativeWindow,
        size: PhysicalSize<u32>,
    ) -> Result<Self, EglError> {
        let egl = egl_engine::load_host_egl()?;
        let display = egl_engine::initialized_display(&egl, display_handle)?;
        let config = window_config(&egl, display, native_visual(window_handle))?;
        let context = EglContext::new(egl, display, config)?;
        let surface = unsafe {
            context
                .egl
                .create_window_surface(display, config, native.as_native_window(), None)
        }
        .map_err(EglError::Surface)?;
        let window = EglWindow {
            surface,
            context,
            native,
        };
        window.make_current()?;
        window
            .context
            .egl
            .swap_interval(display, 0)
            .map_err(EglError::SwapInterval)?;
        let gl = Gles2::load(&window.context.egl)?;
        let (renderer, version) = unsafe {
            (
                gl_string(&gl, GL_RENDERER, "GL_RENDERER")?,
                gl_string(&gl, GL_VERSION, "GL_VERSION")?,
            )
        };
        let (programs, vertex_buffer) = unsafe {
            (gl.gl_pixel_storei)(GL_UNPACK_ALIGNMENT, 1);
            (gl.gl_enable)(GL_BLEND);
            (gl.gl_blend_func)(GL_SRC_ALPHA, GL_ONE_MINUS_SRC_ALPHA);
            let mut vertex_buffer = 0;
            (gl.gl_gen_buffers)(1, &mut vertex_buffer);
            (Programs::link(&gl)?, vertex_buffer)
        };
        gl.check("setting up Eclipse's OpenGL ES window")?;
        let mut renderer = Self {
            glyphs: None,
            canvases: Vec::new(),
            drawn_canvases: Vec::new(),
            programs,
            vertex_buffer,
            extent: vk::Extent2D {
                width: size.width,
                height: size.height,
            },
            renderer,
            version,
            gl,
            window,
        };
        renderer.load_glyphs(TEXT_PX)?;
        Ok(renderer)
    }

    pub(super) fn extent(&self) -> vk::Extent2D {
        self.extent
    }

    pub(super) fn gl_renderer(&self) -> &str {
        &self.renderer
    }

    pub(super) fn atlas(&self) -> Option<&GlyphAtlas> {
        self.glyphs.as_ref().map(|glyphs| &glyphs.atlas)
    }

    pub(super) fn mark_resized(&mut self, width: u32, height: u32) {
        if width == 0 || height == 0 {
            return;
        }
        self.extent = vk::Extent2D { width, height };
        self.window
            .native
            .resize(WindowGeometry::from_physical(width, height));
    }

    pub(super) fn set_text_scale(&mut self, scale: f64) -> Result<(), EglError> {
        self.window.make_current()?;
        self.load_glyphs(TEXT_PX * scale as f32)
    }

    pub(super) fn set_drawn_canvases(&mut self, drawn: Vec<DrawnCanvas>) {
        for stale in std::mem::replace(&mut self.drawn_canvases, drawn) {
            let _ = canvas_registry::free(stale.canvas);
        }
    }

    pub(super) fn draw_nodes(
        &mut self,
        window: &Window,
        nodes: &[RenderNode],
    ) -> Result<(), EglError> {
        self.render(nodes)?;
        window.pre_present_notify();
        self.swap_buffers()
    }

    fn swap_buffers(&self) -> Result<(), EglError> {
        self.window
            .context
            .egl
            .swap_buffers(self.window.context.display, self.window.surface)
            .map_err(EglError::Present)
    }

    fn render(&mut self, nodes: &[RenderNode]) -> Result<(), EglError> {
        self.window.make_current()?;
        let extent = self.extent;
        let measure = self.atlas().map(|atlas| TextMeasure { atlas });
        let views = layout_views(nodes, extent, measure);
        let quads = build_quad_vertices(&views, extent);
        let glyph_vertices = self
            .atlas()
            .map_or_else(Vec::new, |atlas| build_text_vertices(&views, atlas, extent));
        let canvas_vertices = self.upload_canvases(&views, extent)?;

        let mut stream = Vec::with_capacity(
            quads.len() * (POSITION_FLOATS + COLOR_FLOATS) as usize
                + (glyph_vertices.len() + canvas_vertices.len())
                    * (POSITION_FLOATS + UV_FLOATS) as usize,
        );
        for vertex in &quads {
            stream.extend_from_slice(&vertex.pos);
            stream.extend_from_slice(&vertex.color);
        }
        let glyph_offset = stream.len() * FLOAT_BYTES;
        for vertex in glyph_vertices.iter().chain(&canvas_vertices) {
            stream.extend_from_slice(&vertex.pos);
            stream.extend_from_slice(&vertex.uv);
        }
        let canvas_offset = glyph_offset + glyph_vertices.len() * Programs::TEXTURED_STRIDE;

        let gl = &self.gl;
        unsafe {
            (gl.gl_viewport)(0, 0, extent.width as i32, extent.height as i32);
            (gl.gl_color_mask)(GL_TRUE, GL_TRUE, GL_TRUE, GL_TRUE);
            let [red, green, blue, alpha] = CLEAR_COLOR;
            (gl.gl_clear_color)(red, green, blue, alpha);
            (gl.gl_clear)(GL_COLOR_BUFFER_BIT);
            (gl.gl_color_mask)(GL_TRUE, GL_TRUE, GL_TRUE, GL_FALSE);
            (gl.gl_bind_buffer)(GL_ARRAY_BUFFER, self.vertex_buffer);
            (gl.gl_buffer_data)(
                GL_ARRAY_BUFFER,
                (stream.len() * FLOAT_BYTES) as isize,
                stream.as_ptr().cast(),
                GL_STREAM_DRAW,
            );
            if !quads.is_empty() {
                self.programs.solid.bind(gl, 0);
                (gl.gl_draw_arrays)(GL_TRIANGLES, 0, quads.len() as i32);
            }
            if let Some(glyphs) = &self.glyphs {
                if !glyph_vertices.is_empty() {
                    self.programs.glyph.bind(gl, glyph_offset);
                    let [red, green, blue, alpha] = TEXT_COLOR;
                    (gl.gl_uniform_4f)(self.programs.glyph_color, red, green, blue, alpha);
                    (gl.gl_bind_texture)(GL_TEXTURE_2D, glyphs.texture);
                    (gl.gl_draw_arrays)(GL_TRIANGLES, 0, glyph_vertices.len() as i32);
                }
            }
            if !canvas_vertices.is_empty() {
                self.programs.canvas.bind(gl, canvas_offset);
                for (first, canvas) in (0..).step_by(CANVAS_VERTICES as usize).zip(&self.canvases) {
                    (gl.gl_bind_texture)(GL_TEXTURE_2D, canvas.texture);
                    (gl.gl_draw_arrays)(GL_TRIANGLES, first, CANVAS_VERTICES);
                }
            }
        }
        gl.check("drawing Eclipse's window with OpenGL ES")
    }

    fn load_glyphs(&mut self, text_px: f32) -> Result<(), EglError> {
        let Some(atlas) = host_glyph_atlas(text_px) else {
            self.glyphs = None;
            return Ok(());
        };
        let gl = &self.gl;
        let texture = match &self.glyphs {
            Some(glyphs) => glyphs.texture,
            None => unsafe { new_texture(gl) },
        };
        unsafe {
            (gl.gl_bind_texture)(GL_TEXTURE_2D, texture);
            (gl.gl_tex_image_2d)(
                GL_TEXTURE_2D,
                0,
                GL_ALPHA as i32,
                atlas.width as i32,
                atlas.height as i32,
                0,
                GL_ALPHA,
                GL_UNSIGNED_BYTE,
                atlas.pixels.as_ptr().cast(),
            );
        }
        gl.check("uploading the glyph atlas")?;
        self.glyphs = Some(Glyphs { atlas, texture });
        Ok(())
    }

    fn upload_canvases(
        &mut self,
        views: &[LaidOutView],
        extent: vk::Extent2D,
    ) -> Result<Vec<TextVertex>, EglError> {
        let drawn = std::mem::take(&mut self.drawn_canvases);
        let mut vertices = Vec::new();
        let mut slot = 0;
        for canvas in &drawn {
            if slot == MAX_COMPOSITE_VIEWS {
                break;
            }
            let Some(rect) = views.iter().find(|view| view.handle == canvas.view) else {
                continue;
            };
            let snapshot = canvas_registry::with_canvas(canvas.canvas, |state| {
                let (width, height) = state.dimensions();
                (width, height, state.rgba())
            });
            let Ok((width, height, rgba)) = snapshot else {
                continue;
            };
            let expected = (width as usize) * (height as usize) * 4;
            if width == 0 || height == 0 || rgba.len() < expected {
                continue;
            }
            self.upload_canvas(slot, (width, height), &rgba[..expected]);
            vertices.extend_from_slice(&composite_quad_vertices(rect, extent));
            slot += 1;
        }
        let stale: Vec<u32> = self
            .canvases
            .drain(slot..)
            .map(|canvas| canvas.texture)
            .collect();
        if !stale.is_empty() {
            unsafe { (self.gl.gl_delete_textures)(stale.len() as i32, stale.as_ptr()) };
        }
        for canvas in drawn {
            let _ = canvas_registry::free(canvas.canvas);
        }
        self.gl.check("uploading custom view canvases")?;
        Ok(vertices)
    }

    fn upload_canvas(&mut self, slot: usize, (width, height): (u32, u32), rgba: &[u8]) {
        let gl = &self.gl;
        if slot == self.canvases.len() {
            self.canvases.push(CanvasTexture {
                texture: unsafe { new_texture(gl) },
                width: 0,
                height: 0,
            });
        }
        let canvas = &mut self.canvases[slot];
        unsafe {
            (gl.gl_bind_texture)(GL_TEXTURE_2D, canvas.texture);
            if (canvas.width, canvas.height) == (width, height) {
                (gl.gl_tex_sub_image_2d)(
                    GL_TEXTURE_2D,
                    0,
                    0,
                    0,
                    width as i32,
                    height as i32,
                    GL_RGBA,
                    GL_UNSIGNED_BYTE,
                    rgba.as_ptr().cast(),
                );
            } else {
                (gl.gl_tex_image_2d)(
                    GL_TEXTURE_2D,
                    0,
                    GL_RGBA as i32,
                    width as i32,
                    height as i32,
                    0,
                    GL_RGBA,
                    GL_UNSIGNED_BYTE,
                    rgba.as_ptr().cast(),
                );
                canvas.width = width;
                canvas.height = height;
            }
        }
    }
}

impl fmt::Display for GlesRenderer {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(
            f,
            "OpenGL ES on {} ({}), {}x{}",
            self.renderer, self.version, self.extent.width, self.extent.height
        )
    }
}

impl Drop for GlesRenderer {
    fn drop(&mut self) {
        for canvas in self.drawn_canvases.drain(..) {
            let _ = canvas_registry::free(canvas.canvas);
        }
    }
}

struct Glyphs {
    atlas: GlyphAtlas,
    texture: u32,
}

struct CanvasTexture {
    texture: u32,
    width: u32,
    height: u32,
}

struct Programs {
    solid: Program,
    glyph: Program,
    canvas: Program,
    glyph_color: i32,
}

impl Programs {
    const TEXTURED_STRIDE: usize = (POSITION_FLOATS + UV_FLOATS) as usize * FLOAT_BYTES;

    unsafe fn link(gl: &Gles2) -> Result<Self, EglError> {
        unsafe {
            let solid = Program::link(gl, SOLID_VERTEX, SOLID_FRAGMENT, c"aColor", COLOR_FLOATS)?;
            let glyph = Program::link(gl, TEXTURED_VERTEX, GLYPH_FRAGMENT, c"aUv", UV_FLOATS)?;
            let canvas = Program::link(gl, TEXTURED_VERTEX, CANVAS_FRAGMENT, c"aUv", UV_FLOATS)?;
            let glyph_color = (gl.gl_get_uniform_location)(glyph.id, c"uColor".as_ptr());
            if glyph_color < 0 {
                return Err(EglError::Gl(
                    "the glyph program has no uColor uniform".to_owned(),
                ));
            }
            Ok(Self {
                solid,
                glyph,
                canvas,
                glyph_color,
            })
        }
    }
}

#[derive(Clone, Copy)]
struct Program {
    id: u32,
    position: u32,
    input: u32,
    input_floats: i32,
}

impl Program {
    unsafe fn link(
        gl: &Gles2,
        vertex: &str,
        fragment: &str,
        input: &CStr,
        input_floats: i32,
    ) -> Result<Self, EglError> {
        unsafe {
            let id = compile_program(gl, vertex.as_bytes(), fragment.as_bytes())?;
            Ok(Self {
                id,
                position: attribute(gl, id, c"aPos")?,
                input: attribute(gl, id, input)?,
                input_floats,
            })
        }
    }

    unsafe fn bind(&self, gl: &Gles2, offset: usize) {
        let stride = (POSITION_FLOATS + self.input_floats) * FLOAT_BYTES as i32;
        let input_offset = offset + POSITION_FLOATS as usize * FLOAT_BYTES;
        unsafe {
            (gl.gl_use_program)(self.id);
            (gl.gl_enable_vertex_attrib_array)(self.position);
            (gl.gl_vertex_attrib_pointer)(
                self.position,
                POSITION_FLOATS,
                GL_FLOAT,
                GL_FALSE,
                stride,
                offset as *const c_void,
            );
            (gl.gl_enable_vertex_attrib_array)(self.input);
            (gl.gl_vertex_attrib_pointer)(
                self.input,
                self.input_floats,
                GL_FLOAT,
                GL_FALSE,
                stride,
                input_offset as *const c_void,
            );
        }
    }
}

unsafe fn attribute(gl: &Gles2, program: u32, name: &CStr) -> Result<u32, EglError> {
    let location = unsafe { (gl.gl_get_attrib_location)(program, name.as_ptr()) };
    u32::try_from(location).map_err(|_| {
        EglError::Gl(format!(
            "the program has no {} attribute",
            name.to_string_lossy()
        ))
    })
}

unsafe fn new_texture(gl: &Gles2) -> u32 {
    let mut texture = 0;
    unsafe {
        (gl.gl_gen_textures)(1, &mut texture);
        (gl.gl_bind_texture)(GL_TEXTURE_2D, texture);
        (gl.gl_tex_parameteri)(GL_TEXTURE_2D, GL_TEXTURE_MIN_FILTER, GL_LINEAR);
        (gl.gl_tex_parameteri)(GL_TEXTURE_2D, GL_TEXTURE_MAG_FILTER, GL_LINEAR);
        (gl.gl_tex_parameteri)(GL_TEXTURE_2D, GL_TEXTURE_WRAP_S, GL_CLAMP_TO_EDGE);
        (gl.gl_tex_parameteri)(GL_TEXTURE_2D, GL_TEXTURE_WRAP_T, GL_CLAMP_TO_EDGE);
    }
    texture
}

struct EglContext {
    egl: EglInstance,
    display: egl::Display,
    context: egl::Context,
}

impl EglContext {
    fn new(egl: EglInstance, display: egl::Display, config: egl::Config) -> Result<Self, EglError> {
        egl.bind_api(egl::OPENGL_ES_API)
            .map_err(EglError::Context)?;
        let context = egl
            .create_context(display, config, None, &egl_engine::gles2_context_attribs())
            .map_err(EglError::Context)?;
        Ok(Self {
            egl,
            display,
            context,
        })
    }
}

impl Drop for EglContext {
    fn drop(&mut self) {
        if let Err(error) = self.egl.destroy_context(self.display, self.context) {
            tracing::warn!(%error, "destroying Eclipse's OpenGL ES context failed");
        }
    }
}

struct EglWindow {
    surface: egl::Surface,
    context: EglContext,
    native: EngineNativeWindow,
}

impl EglWindow {
    fn make_current(&self) -> Result<(), EglError> {
        self.context
            .egl
            .make_current(
                self.context.display,
                Some(self.surface),
                Some(self.surface),
                Some(self.context.context),
            )
            .map_err(EglError::Present)
    }
}

impl Drop for EglWindow {
    fn drop(&mut self) {
        let egl = &self.context.egl;
        let display = self.context.display;
        if egl.get_current_context() == Some(self.context.context) {
            if let Err(error) = egl.make_current(display, None, None, None) {
                tracing::warn!(%error, "releasing Eclipse's OpenGL ES context failed");
            }
        }
        if let Err(error) = egl.destroy_surface(display, self.surface) {
            tracing::warn!(%error, "destroying Eclipse's OpenGL ES window surface failed");
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
struct ConfigTraits {
    visual: c_ulong,
    rgb: [egl::Int; 3],
    alpha: egl::Int,
    depth: egl::Int,
}

impl ConfigTraits {
    fn of(egl: &EglInstance, display: egl::Display, config: egl::Config) -> Result<Self, EglError> {
        let read = |name| {
            egl.get_config_attrib(display, config, name)
                .map_err(|e| EglError::Display(format!("eglGetConfigAttrib failed: {e}")))
        };
        Ok(Self {
            visual: read(egl::NATIVE_VISUAL_ID)? as c_ulong,
            rgb: [
                read(egl::RED_SIZE)?,
                read(egl::GREEN_SIZE)?,
                read(egl::BLUE_SIZE)?,
            ],
            alpha: read(egl::ALPHA_SIZE)?,
            depth: read(egl::DEPTH_SIZE)?,
        })
    }
}

fn native_visual(window_handle: RawWindowHandle) -> Option<c_ulong> {
    match window_handle {
        RawWindowHandle::Xlib(handle) if handle.visual_id != 0 => Some(handle.visual_id),
        _ => None,
    }
}

fn window_config(
    egl: &EglInstance,
    display: egl::Display,
    visual: Option<c_ulong>,
) -> Result<egl::Config, EglError> {
    let choose_failed = |e| EglError::Display(format!("eglChooseConfig failed: {e}"));
    let count = egl
        .matching_config_count(display, &WINDOW_CONFIG)
        .map_err(choose_failed)?;
    let mut configs = Vec::with_capacity(count);
    egl.choose_config(display, &WINDOW_CONFIG, &mut configs)
        .map_err(choose_failed)?;
    let traits = configs
        .iter()
        .map(|&config| ConfigTraits::of(egl, display, config))
        .collect::<Result<Vec<_>, _>>()?;
    pick_config(&traits, visual)
        .map(|index| configs[index])
        .ok_or(EglError::NoConfig)
}

fn pick_config(configs: &[ConfigTraits], visual: Option<c_ulong>) -> Option<usize> {
    configs
        .iter()
        .enumerate()
        .filter(|(_, config)| match visual {
            Some(visual) => config.visual == visual,
            None => config.rgb == [8, 8, 8],
        })
        .min_by_key(|(_, config)| (config.alpha != 0, config.depth != 0))
        .map(|(index, _)| index)
}

#[cfg(test)]
mod tests {
    use std::ffi::c_uint;
    use std::time::Duration;

    use raw_window_handle::{WaylandWindowHandle, XlibDisplayHandle, XlibWindowHandle};

    use super::*;
    use crate::framework::view_registry::{LayoutParams, MATCH_PARENT, WRAP_CONTENT};
    use crate::gpu::{GlesReason, Graphics, Plan, Requested};
    use crate::graphics::roleless_wayland_surface::RolelessWaylandSurface;
    use crate::graphics::unmapped_xlib_window::UnmappedXlibWindow;
    use crate::graphics::{VulkanRenderer, WindowRenderer};
    use crate::loader::native_provider::HOST_GLESV2_SONAME;

    const DRAW_CHILD: &str = "ECLIPSE_TEST_GLES_DRAW_CHILD";

    const PLAN_CHILD: &str = "ECLIPSE_TEST_GRAPHICS_PLAN_CHILD";

    const WAYLAND_CHILD: &str = "ECLIPSE_TEST_GLES_WAYLAND_CHILD";

    const WAYLAND_SWAPS: usize = 3;

    const VULKAN_ICDS: &str = "/usr/share/vulkan/icd.d";

    const EGL_VENDORS: &str = "/usr/share/glvnd/egl_vendor.d";

    const DRAW_CHILD_LIMIT: Duration = Duration::from_secs(60);

    const WIDTH: c_uint = 240;
    const HEIGHT: c_uint = 160;

    const ROOT_BACKGROUND: i32 = 0xFFF3_F5F8_u32 as i32;
    const QUAD_COLOR: [u8; 3] = [0x2F, 0x6F, 0xD6];
    const TEXT_BACKGROUND: i32 = 0xFFFF_FFFF_u32 as i32;
    const CANVAS_COLOR: [u8; 4] = [0x11, 0xAA, 0x33, 0xFF];

    const QUAD: usize = 1;
    const TEXT: usize = 2;
    const CANVAS: usize = 3;

    type GlReadPixels = unsafe extern "C" fn(i32, i32, i32, i32, u32, u32, *mut c_void);
    type GlGetError = unsafe extern "C" fn() -> u32;

    fn config(visual: c_ulong, rgb: egl::Int, alpha: egl::Int, depth: egl::Int) -> ConfigTraits {
        ConfigTraits {
            visual,
            rgb: [rgb; 3],
            alpha,
            depth,
        }
    }

    #[test]
    fn wayland_windows_take_an_rgb888_config_without_alpha_or_depth() {
        let configs = [
            config(0, 8, 8, 24),
            config(0, 5, 0, 0),
            config(0, 8, 0, 24),
            config(0, 8, 0, 0),
            config(0, 8, 8, 0),
        ];
        assert_eq!(pick_config(&configs, None), Some(3));
        assert_eq!(pick_config(&configs[..3], None), Some(2));
        assert_eq!(pick_config(&configs[1..2], None), None);
    }

    #[test]
    fn x11_windows_take_a_config_of_their_own_visual() {
        let configs = [
            config(0x21, 8, 8, 24),
            config(0x22, 8, 0, 0),
            config(0x21, 8, 0, 24),
            config(0x23, 10, 0, 0),
        ];
        assert_eq!(pick_config(&configs, Some(0x21)), Some(2));
        assert_eq!(pick_config(&configs, Some(0x23)), Some(3));
        assert_eq!(pick_config(&configs, Some(0x99)), None);
    }

    #[test]
    fn only_an_xlib_window_with_a_known_visual_names_its_visual() {
        let mut xlib = XlibWindowHandle::new(7);
        assert_eq!(native_visual(RawWindowHandle::Xlib(xlib)), None);
        xlib.visual_id = 0x21;
        assert_eq!(native_visual(RawWindowHandle::Xlib(xlib)), Some(0x21));
        let wayland = WaylandWindowHandle::new(std::ptr::NonNull::dangling());
        assert_eq!(native_visual(RawWindowHandle::Wayland(wayland)), None);
    }

    fn view(
        handle: i64,
        class_name: &str,
        text: Option<&str>,
        (width, height): (i32, i32),
        background_color: Option<i32>,
    ) -> RenderNode {
        RenderNode {
            handle,
            class_name: class_name.to_owned(),
            text: text.map(str::to_owned),
            depth: u32::from(handle != 0),
            layout: LayoutParams {
                width,
                height,
                ..LayoutParams::default()
            },
            clickable: false,
            background_color,
            children: Vec::new(),
        }
    }

    fn test_tree() -> Vec<RenderNode> {
        let [red, green, blue] = QUAD_COLOR;
        let quad = i32::from_be_bytes([0xFF, red, green, blue]);
        let mut root = view(
            0,
            "android.widget.LinearLayout",
            None,
            (MATCH_PARENT, MATCH_PARENT),
            Some(ROOT_BACKGROUND),
        );
        root.children = vec![QUAD, TEXT, CANVAS];
        vec![
            root,
            view(1, "android.widget.FrameLayout", None, (120, 40), Some(quad)),
            view(
                2,
                "android.widget.TextView",
                Some("Eclipse"),
                (WRAP_CONTENT, WRAP_CONTENT),
                Some(TEXT_BACKGROUND),
            ),
            view(3, "com.example.Swatch", None, (60, 40), None),
        ]
    }

    fn pixel(frame: &[u8], (x, y): (f32, f32)) -> [u8; 4] {
        let (x, y) = (x as usize, HEIGHT as usize - 1 - y as usize);
        let at = (y * WIDTH as usize + x) * 4;
        frame[at..at + 4].try_into().expect("four channels")
    }

    fn center(view: &LaidOutView) -> (f32, f32) {
        (view.x + view.w / 2.0, view.y + view.h / 2.0)
    }

    fn draw_on_unmapped_x11_window(window: &UnmappedXlibWindow) {
        let (display_handle, window_handle) = x11_handles(window);
        let native = EngineNativeWindow::private(
            window_handle,
            WindowGeometry::from_physical(WIDTH, HEIGHT),
        )
        .expect("an X11 window backs an EGL window surface");
        let mut renderer = GlesRenderer::create(
            display_handle,
            window_handle,
            native,
            PhysicalSize::new(WIDTH, HEIGHT),
        )
        .expect("an OpenGL ES renderer on the X11 window");

        let canvas = canvas_registry::allocate(60, 40).expect("a 60x40 canvas");
        canvas_registry::with_canvas(canvas, |state| {
            state.draw_color(i32::from_be_bytes([
                CANVAS_COLOR[3],
                CANVAS_COLOR[0],
                CANVAS_COLOR[1],
                CANVAS_COLOR[2],
            ]));
        })
        .expect("the canvas is live");
        renderer.set_drawn_canvases(vec![DrawnCanvas {
            view: CANVAS as i64,
            canvas,
        }]);
        let nodes = test_tree();
        renderer.render(&nodes).expect("the tree draws");

        let gles = unsafe { libloading::Library::new(HOST_GLESV2_SONAME) }.expect("host GLESv2");
        let (read_pixels, get_error) = unsafe {
            (
                *gles
                    .get::<GlReadPixels>(b"glReadPixels\0")
                    .expect("glReadPixels"),
                *gles.get::<GlGetError>(b"glGetError\0").expect("glGetError"),
            )
        };
        let mut frame = vec![0u8; WIDTH as usize * HEIGHT as usize * 4];
        unsafe {
            read_pixels(
                0,
                0,
                WIDTH as i32,
                HEIGHT as i32,
                GL_RGBA,
                GL_UNSIGNED_BYTE,
                frame.as_mut_ptr().cast(),
            );
            assert_eq!(get_error(), 0, "glGetError after drawing and reading back");
        }

        let atlas = renderer.atlas().expect("a host font for the glyph atlas");
        let views = layout_views(&nodes, renderer.extent(), Some(TextMeasure { atlas }));

        let quad = pixel(&frame, center(&views[QUAD]));
        for (channel, (&drawn, &wanted)) in quad.iter().zip(&QUAD_COLOR).enumerate() {
            assert!(
                drawn.abs_diff(wanted) <= 1,
                "channel {channel} of the quad's center is {drawn}, not {wanted}: {quad:?}"
            );
        }

        let text = &views[TEXT];
        let ink = (text.y as usize..(text.y + text.h) as usize)
            .flat_map(|y| (text.x as usize..(text.x + text.w) as usize).map(move |x| (x, y)))
            .filter(|&(x, y)| pixel(&frame, (x as f32, y as f32))[0] < 160)
            .count();
        assert!(ink >= 50, "only {ink} ink pixels in the TextView");

        let swatch = pixel(&frame, center(&views[CANVAS]));
        assert_eq!(swatch[..3], CANVAS_COLOR[..3], "the canvas's center pixel");

        drop(renderer);
        let egl = egl_engine::load_host_egl().expect("host EGL");
        let display = egl_engine::initialized_display(&egl, display_handle).expect("EGL display");
        egl.terminate(display).expect("eglTerminate");

        let maps = std::fs::read_to_string("/proc/self/maps").expect("/proc/self/maps");
        assert!(
            !maps.contains("libvulkan.so.1"),
            "drawing with OpenGL ES loaded the Vulkan loader"
        );
    }

    #[test]
    fn x11_window_draws_views_text_and_canvases_with_opengl_es_only() {
        if std::env::var_os(DRAW_CHILD).is_none() {
            if std::env::var_os("DISPLAY").is_none() {
                eprintln!("SKIP: no X11 display (DISPLAY unset)");
                return;
            }
            let output = crate::bounded_child::output(
                std::process::Command::new(
                    std::env::current_exe().expect("the test harness executable must have a path"),
                )
                .args([
                    "--exact",
                    "graphics::gles_renderer::tests::\
                     x11_window_draws_views_text_and_canvases_with_opengl_es_only",
                    "--test-threads=1",
                    "--nocapture",
                ])
                .env(DRAW_CHILD, "1")
                .env_remove("WAYLAND_DISPLAY")
                .env_remove("WAYLAND_SOCKET"),
                DRAW_CHILD_LIMIT,
            );
            let report = String::from_utf8_lossy(&output.stdout);
            assert!(
                output.status.success() && report.contains("1 passed"),
                "status={:?}, stdout={report}, stderr={}",
                output.status,
                String::from_utf8_lossy(&output.stderr)
            );
            return;
        }
        let window = match UnmappedXlibWindow::open(WIDTH, HEIGHT) {
            Ok(window) => window,
            Err(e) => {
                eprintln!("SKIP: no usable X11 display ({e})");
                return;
            }
        };
        draw_on_unmapped_x11_window(&window);
    }

    fn x11_handles(window: &UnmappedXlibWindow) -> (RawDisplayHandle, RawWindowHandle) {
        let display_handle = RawDisplayHandle::Xlib(XlibDisplayHandle::new(
            std::ptr::NonNull::new(window.display),
            window.screen,
        ));
        let mut xlib = XlibWindowHandle::new(window.window);
        xlib.visual_id = window.visual_id;
        (display_handle, RawWindowHandle::Xlib(xlib))
    }

    fn installed_file(directory: &str, prefix: &str) -> Option<std::path::PathBuf> {
        std::fs::read_dir(directory)
            .ok()?
            .filter_map(Result::ok)
            .map(|entry| entry.path())
            .find(|path| {
                path.file_name()
                    .and_then(|name| name.to_str())
                    .is_some_and(|name| name.starts_with(prefix) && name.ends_with(".json"))
            })
    }

    fn plan_in_child(test: &str, vulkan_driver: &std::path::Path) {
        if std::env::var_os("DISPLAY").is_none() {
            eprintln!("SKIP: no X11 display (DISPLAY unset)");
            return;
        }
        let Some(mesa_egl) = installed_file(EGL_VENDORS, "50_mesa") else {
            eprintln!("SKIP: no Mesa EGL vendor file in {EGL_VENDORS}");
            return;
        };
        let output = crate::bounded_child::output(
            std::process::Command::new(
                std::env::current_exe().expect("the test harness executable must have a path"),
            )
            .args([
                "--exact",
                &format!("graphics::gles_renderer::tests::{test}"),
                "--test-threads=1",
                "--nocapture",
            ])
            .env(PLAN_CHILD, "1")
            .env("VK_DRIVER_FILES", vulkan_driver)
            .env("LIBGL_ALWAYS_SOFTWARE", "1")
            .env("__EGL_VENDOR_LIBRARY_FILENAMES", mesa_egl)
            .env_remove("VK_ICD_FILENAMES")
            .env_remove("WAYLAND_DISPLAY")
            .env_remove("WAYLAND_SOCKET"),
            DRAW_CHILD_LIMIT,
        );
        let report = String::from_utf8_lossy(&output.stdout);
        assert!(
            output.status.success() && report.contains("1 passed"),
            "status={:?}, stdout={report}, stderr={}",
            output.status,
            String::from_utf8_lossy(&output.stderr)
        );
    }

    fn plan_on_unmapped_x11_window(window: &UnmappedXlibWindow) -> (WindowRenderer, Plan) {
        let (display_handle, window_handle) = x11_handles(window);
        let size = PhysicalSize::new(WIDTH, HEIGHT);
        let (renderer, plan) = WindowRenderer::first(
            Requested::Automatic,
            || VulkanRenderer::create(display_handle, window_handle, size),
            || GlesRenderer::on_own_window(display_handle, window_handle, size),
        );
        let renderer = renderer.expect("software OpenGL ES draws the window");
        (renderer, plan)
    }

    fn assert_draws_the_background(renderer: &mut GlesRenderer) {
        let root = view(
            0,
            "android.widget.LinearLayout",
            None,
            (MATCH_PARENT, MATCH_PARENT),
            Some(ROOT_BACKGROUND),
        );
        renderer.render(&[root]).expect("the background draws");
        let gles = unsafe { libloading::Library::new(HOST_GLESV2_SONAME) }.expect("host GLESv2");
        let read_pixels = unsafe {
            *gles
                .get::<GlReadPixels>(b"glReadPixels\0")
                .expect("glReadPixels")
        };
        let mut frame = vec![0u8; WIDTH as usize * HEIGHT as usize * 4];
        unsafe {
            read_pixels(
                0,
                0,
                WIDTH as i32,
                HEIGHT as i32,
                GL_RGBA,
                GL_UNSIGNED_BYTE,
                frame.as_mut_ptr().cast(),
            );
        }
        let [_, red, green, blue] = ROOT_BACKGROUND.to_be_bytes();
        let center = pixel(&frame, (WIDTH as f32 / 2.0, HEIGHT as f32 / 2.0));
        assert_eq!(center[..3], [red, green, blue], "the window's center pixel");
    }

    fn open_child_window() -> Option<UnmappedXlibWindow> {
        UnmappedXlibWindow::open(WIDTH, HEIGHT)
            .inspect_err(|e| eprintln!("SKIP: no usable X11 display ({e})"))
            .ok()
    }

    #[test]
    fn automatic_choice_without_a_vulkan_driver_gives_roblox_software_opengl_es() {
        if std::env::var_os(PLAN_CHILD).is_none() {
            plan_in_child(
                "automatic_choice_without_a_vulkan_driver_gives_roblox_software_opengl_es",
                std::path::Path::new("/nonexistent/eclipse-no-vulkan-driver.json"),
            );
            return;
        }
        let Some(window) = open_child_window() else {
            return;
        };
        let (renderer, plan) = plan_on_unmapped_x11_window(&window);
        assert_eq!(plan.graphics(), Graphics::Gles(GlesReason::NoUsableVulkan));
        assert_eq!(
            plan.warning_texts(),
            ["Roblox is rendering on the CPU (llvmpipe), which is slow."]
        );
        let WindowRenderer::Gles(mut renderer) = renderer else {
            panic!("the window draws with Vulkan although no Vulkan driver exists");
        };
        assert_draws_the_background(&mut renderer);
    }

    #[test]
    fn automatic_choice_on_lavapipe_and_software_gl_keeps_roblox_on_vulkan() {
        if std::env::var_os(PLAN_CHILD).is_none() {
            let Some(lavapipe) = installed_file(VULKAN_ICDS, "lvp_icd") else {
                eprintln!("SKIP: no lavapipe Vulkan driver in {VULKAN_ICDS}");
                return;
            };
            plan_in_child(
                "automatic_choice_on_lavapipe_and_software_gl_keeps_roblox_on_vulkan",
                &lavapipe,
            );
            return;
        }
        let Some(window) = open_child_window() else {
            return;
        };
        let (renderer, plan) = plan_on_unmapped_x11_window(&window);
        assert_eq!(plan.graphics(), Graphics::Vulkan);
        assert_eq!(
            plan.warning_texts(),
            ["Roblox is rendering on the CPU (llvmpipe), which is slow."]
        );
        let WindowRenderer::Gles(mut renderer) = renderer else {
            panic!("software OpenGL ES, already started, should keep drawing the window");
        };
        assert_draws_the_background(&mut renderer);
    }

    #[test]
    fn swaps_on_a_wayland_surface_that_is_never_shown_return() {
        if std::env::var_os(WAYLAND_CHILD).is_none() {
            if std::env::var_os("WAYLAND_DISPLAY").is_none() {
                eprintln!("SKIP: no Wayland display (WAYLAND_DISPLAY unset)");
                return;
            }
            let output = crate::bounded_child::output(
                std::process::Command::new(
                    std::env::current_exe().expect("the test harness executable must have a path"),
                )
                .args([
                    "--exact",
                    "graphics::gles_renderer::tests::\
                     swaps_on_a_wayland_surface_that_is_never_shown_return",
                    "--test-threads=1",
                    "--nocapture",
                ])
                .env(WAYLAND_CHILD, "1"),
                DRAW_CHILD_LIMIT,
            );
            let report = String::from_utf8_lossy(&output.stdout);
            assert!(
                output.status.success() && report.contains("1 passed"),
                "status={:?}, stdout={report}, stderr={}",
                output.status,
                String::from_utf8_lossy(&output.stderr)
            );
            return;
        }
        let surface = match RolelessWaylandSurface::open() {
            Ok(surface) => surface,
            Err(error) => {
                eprintln!("SKIP: {error}");
                return;
            }
        };
        let (display_handle, window_handle) = surface.handles();
        let mut renderer = GlesRenderer::on_own_window(
            display_handle,
            window_handle,
            PhysicalSize::new(WIDTH, HEIGHT),
        )
        .expect("an OpenGL ES renderer on the Wayland surface");
        let nodes = test_tree();
        for _ in 0..WAYLAND_SWAPS {
            renderer.render(&nodes).expect("the tree draws");
            renderer.swap_buffers().expect("the frame is swapped");
        }
    }
}
