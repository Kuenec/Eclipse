use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::sync::{Mutex, OnceLock, PoisonError};
use std::time::Instant;

use khronos_egl as egl;

use super::frame_log::{self, FrameLog, HostCall, MonotonicNs};
use super::native_provider::{host_library_symbol, HOST_EGL_SONAME};
use super::ndk_registry::{self, WsiTarget};
use super::text_overlay::{self, ByteOrder, SurfaceRect, SurfaceSize, TextLayer, TextRequest};
use crate::egl_engine::{
    self, compile_program, EglError, EglInstance, Gles2, GL_ARRAY_BUFFER, GL_BLEND,
    GL_CLAMP_TO_EDGE, GL_FALSE, GL_FLOAT, GL_ONE_MINUS_SRC_ALPHA, GL_RGBA, GL_STREAM_DRAW,
    GL_TEXTURE_2D, GL_TEXTURE_MAG_FILTER, GL_TEXTURE_MIN_FILTER, GL_TEXTURE_WRAP_S,
    GL_TEXTURE_WRAP_T, GL_TRIANGLES, GL_TRUE, GL_UNSIGNED_BYTE,
};

type HostSwap = unsafe extern "C" fn(egl::EGLDisplay, egl::EGLSurface) -> egl::Boolean;
type HostSwapInterval = unsafe extern "C" fn(egl::EGLDisplay, egl::Int) -> egl::Boolean;

const LATEST_FRAME_SWAP_INTERVAL: egl::Int = 0;

const GL_ONE: u32 = 1;
const GL_NEAREST: i32 = 0x2600;

const QUAD_VERTICES: i32 = 6;
const VERTEX_FLOATS: usize = 4;
const VERTEX_STRIDE: i32 = (VERTEX_FLOATS * std::mem::size_of::<f32>()) as i32;
const UV_OFFSET: usize = 2 * std::mem::size_of::<f32>();

const FIELD_VERTEX: &str = "attribute vec2 aPos;
attribute vec2 aUv;
varying vec2 vUv;
void main() {
  gl_Position = vec4(aPos, 0.0, 1.0);
  vUv = aUv;
}
\0";

const FIELD_FRAGMENT: &str = "#ifdef GL_FRAGMENT_PRECISION_HIGH
precision highp float;
#else
precision mediump float;
#endif
uniform sampler2D uTexture;
varying vec2 vUv;
void main() {
  gl_FragColor = texture2D(uTexture, vUv);
}
\0";

static SWAPS: AtomicU64 = AtomicU64::new(0);

static OVERLAY: Mutex<OverlaySlot> = Mutex::new(OverlaySlot::Unloaded);

enum OverlaySlot {
    Unloaded,
    Loaded(Box<FieldOverlay>),
    Unavailable,
}

fn host_swap_buffers() -> Option<HostSwap> {
    static HOST: OnceLock<Option<HostSwap>> = OnceLock::new();
    *HOST.get_or_init(
        || match host_library_symbol(HOST_EGL_SONAME, c"eglSwapBuffers") {
            Ok(address) => Some(unsafe { std::mem::transmute::<usize, HostSwap>(address) }),
            Err(error) => {
                tracing::error!(
                    %error,
                    "host eglSwapBuffers unavailable, so Roblox's OpenGL ES frames cannot be shown"
                );
                None
            }
        },
    )
}

fn host_swap_interval() -> Option<HostSwapInterval> {
    static HOST: OnceLock<Option<HostSwapInterval>> = OnceLock::new();
    *HOST.get_or_init(
        || match host_library_symbol(HOST_EGL_SONAME, c"eglSwapInterval") {
            Ok(address) => Some(unsafe { std::mem::transmute::<usize, HostSwapInterval>(address) }),
            Err(error) => {
                tracing::error!(
                    %error,
                    "host eglSwapInterval unavailable, so Roblox's OpenGL ES frames wait for the \
                     compositor and stop while the window is hidden"
                );
                None
            }
        },
    )
}

fn swaps_latest_frame(target: Option<WsiTarget>) -> bool {
    matches!(target, Some(WsiTarget::Wayland { .. }))
}

pub(crate) unsafe extern "C" fn eclipse_egl_swap_buffers(
    display: egl::EGLDisplay,
    surface: egl::EGLSurface,
) -> egl::Boolean {
    let Some(host) = host_swap_buffers() else {
        return egl::FALSE;
    };
    let latest_frame = swaps_latest_frame(ndk_registry::wsi_target())
        .then(host_swap_interval)
        .flatten();
    unsafe {
        swap_engine_frame(
            host,
            latest_frame,
            frame_log::armed(),
            text_overlay::wanted(),
            display,
            surface,
        )
    }
}

unsafe fn swap_engine_frame(
    host: HostSwap,
    latest_frame: Option<HostSwapInterval>,
    frame_log: Option<&FrameLog>,
    overlay_wanted: bool,
    display: egl::EGLDisplay,
    surface: egl::EGLSurface,
) -> egl::Boolean {
    let entered = frame_log.map(|log| (log, MonotonicNs::now()));
    if SWAPS.fetch_add(1, Ordering::Relaxed) == 0 {
        crate::first_frame::presented();
        match ndk_registry::current_wsi_window().and_then(ndk_registry::wsi_window_geometry) {
            Some((width, height)) => tracing::info!(
                width,
                height,
                "egl seam armed (engine eglSwapBuffers interposed)"
            ),
            None => tracing::info!("egl seam armed (engine eglSwapBuffers interposed)"),
        }
    }
    crate::framework::engine_presented();
    if overlay_wanted {
        unsafe { draw_field_overlay(display, surface) };
    }
    if let Some(set_interval) = latest_frame {
        unsafe { swap_without_waiting(set_interval, display) };
    }
    let Some((log, entered)) = entered else {
        return unsafe { host(display, surface) };
    };
    let started = MonotonicNs::now();
    let swapped = unsafe { host(display, surface) };
    log.record(
        entered,
        HostCall {
            started,
            ended: MonotonicNs::now(),
        },
    );
    swapped
}

unsafe fn swap_without_waiting(set_interval: HostSwapInterval, display: egl::EGLDisplay) {
    if unsafe { set_interval(display, LATEST_FRAME_SWAP_INTERVAL) } == egl::TRUE {
        return;
    }
    static WARNED: AtomicBool = AtomicBool::new(false);
    if !WARNED.swap(true, Ordering::Relaxed) {
        let error = egl_engine::load_host_egl()
            .ok()
            .and_then(|egl| egl.get_error());
        tracing::warn!(
            ?error,
            "eglSwapInterval(0) failed, so Roblox's OpenGL ES frames wait for the compositor and \
             stop while the window is hidden"
        );
    }
}

unsafe fn draw_field_overlay(display: egl::EGLDisplay, surface: egl::EGLSurface) {
    let mut slot = OVERLAY.lock().unwrap_or_else(PoisonError::into_inner);
    if matches!(*slot, OverlaySlot::Unloaded) {
        *slot = match FieldOverlay::load() {
            Ok(overlay) => OverlaySlot::Loaded(Box::new(overlay)),
            Err(error) => {
                tracing::warn!(
                    %error,
                    "egl seam: focused text cannot be drawn over Roblox's OpenGL ES frames"
                );
                OverlaySlot::Unavailable
            }
        };
    }
    let OverlaySlot::Loaded(overlay) = &mut *slot else {
        return;
    };
    let target = unsafe {
        Target {
            display: egl::Display::from_ptr(display),
            surface: egl::Surface::from_ptr(surface),
        }
    };
    match unsafe { overlay.draw(target) } {
        Ok(()) => {}
        Err(OverlayError::Frame(error)) => {
            static WARNED: AtomicBool = AtomicBool::new(false);
            if !WARNED.swap(true, Ordering::Relaxed) {
                tracing::warn!(%error, "egl seam: focused text was not drawn over this frame");
            }
        }
        Err(OverlayError::Setup(error)) => {
            tracing::warn!(
                %error,
                "egl seam: focused text cannot be drawn over Roblox's OpenGL ES frames"
            );
            *slot = OverlaySlot::Unavailable;
        }
    }
}

enum OverlayError {
    Setup(EglError),
    Frame(EglError),
}

#[derive(Clone, Copy)]
struct Target {
    display: egl::Display,
    surface: egl::Surface,
}

struct FieldOverlay {
    egl: EglInstance,
    gl: Gles2,
    context: Option<OverlayContext>,
    texels: Vec<[u8; 4]>,
}

struct OverlayContext {
    display: egl::Display,
    config_id: egl::Int,
    context: egl::Context,
    uploaded: Option<Upload>,
}

unsafe impl Send for OverlayContext {}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
struct Upload {
    generation: u64,
    caret: bool,
    width: u32,
    height: u32,
}

struct CurrentContext {
    display: Option<egl::Display>,
    draw: Option<egl::Surface>,
    read: Option<egl::Surface>,
    context: Option<egl::Context>,
}

impl CurrentContext {
    fn of(egl: &EglInstance) -> Self {
        Self {
            display: egl.get_current_display(),
            draw: egl.get_current_surface(egl::DRAW),
            read: egl.get_current_surface(egl::READ),
            context: egl.get_current_context(),
        }
    }

    fn restore(self, egl: &EglInstance, display: egl::Display) -> Result<(), EglError> {
        egl.make_current(
            self.display.unwrap_or(display),
            self.draw,
            self.read,
            self.context,
        )
        .map_err(EglError::Present)
    }
}

impl FieldOverlay {
    fn load() -> Result<Self, EglError> {
        let egl = egl_engine::load_host_egl()?;
        let gl = Gles2::load(&egl)?;
        Ok(Self {
            egl,
            gl,
            context: None,
            texels: Vec::new(),
        })
    }

    unsafe fn draw(&mut self, target: Target) -> Result<(), OverlayError> {
        let attribute = |name| {
            let value = self
                .egl
                .query_surface(target.display, target.surface, name)
                .map_err(|error| OverlayError::Frame(EglError::Surface(error)))?;
            u32::try_from(value).map_err(|_| {
                OverlayError::Frame(EglError::Display(format!(
                    "eglQuerySurface reported {value} for attribute 0x{name:04x}"
                )))
            })
        };
        let size = SurfaceSize {
            width: attribute(egl::WIDTH)?,
            height: attribute(egl::HEIGHT)?,
        };
        let config_id = attribute(egl::CONFIG_ID)? as egl::Int;
        let Some(plan) = text_overlay::plan(size, ByteOrder::Rgba) else {
            return Ok(());
        };
        let field = plan.overlay.as_ref().and_then(|overlay| {
            let request = TextRequest::of(overlay, plan.rect, plan.order);
            text_overlay::current_text_layer(&request, Instant::now())
        });
        let probing = text_overlay::probe_enabled();
        if field.is_none() && !probing {
            return Ok(());
        }
        let current = CurrentContext::of(&self.egl);
        let drawn = unsafe {
            self.draw_in_context(
                target,
                config_id,
                Frame {
                    size,
                    rect: plan.rect,
                    field: field.as_ref().map(|(layer, caret)| (&**layer, *caret)),
                    probing,
                },
            )
        };
        let restored = current
            .restore(&self.egl, target.display)
            .map_err(OverlayError::Frame);
        drawn.and(restored)
    }

    unsafe fn draw_in_context(
        &mut self,
        target: Target,
        config_id: egl::Int,
        frame: Frame<'_>,
    ) -> Result<(), OverlayError> {
        let Self {
            egl,
            gl,
            context,
            texels,
        } = self;
        let context = unsafe { overlay_context(egl, gl, context, target, config_id) }
            .map_err(OverlayError::Setup)?;
        egl.make_current(
            target.display,
            Some(target.surface),
            Some(target.surface),
            Some(context.context),
        )
        .map_err(|error| OverlayError::Frame(EglError::Present(error)))?;
        unsafe {
            (gl.gl_viewport)(0, 0, frame.size.width as i32, frame.size.height as i32);
            if let Some((layer, caret_on)) = frame.field {
                upload_field(
                    gl,
                    &mut context.uploaded,
                    texels,
                    layer,
                    caret_on,
                    frame.rect,
                )
                .map_err(OverlayError::Frame)?;
                let quad = field_quad(frame.rect, frame.size);
                (gl.gl_buffer_data)(
                    GL_ARRAY_BUFFER,
                    std::mem::size_of_val(&quad) as isize,
                    quad.as_ptr().cast(),
                    GL_STREAM_DRAW,
                );
                (gl.gl_draw_arrays)(GL_TRIANGLES, 0, QUAD_VERTICES);
            }
            if frame.probing {
                read_field_probe(gl, frame.rect, frame.size);
            }
        }
        gl.check("drawing focused text over Roblox's OpenGL ES frame")
            .map_err(OverlayError::Frame)
    }
}

impl Drop for FieldOverlay {
    fn drop(&mut self) {
        if let Some(context) = self.context.take() {
            if let Err(error) = self.egl.destroy_context(context.display, context.context) {
                tracing::warn!(%error, "destroying the focused-text OpenGL ES context failed");
            }
        }
    }
}

struct Frame<'a> {
    size: SurfaceSize,
    rect: SurfaceRect,
    field: Option<(&'a TextLayer, bool)>,
    probing: bool,
}

unsafe fn overlay_context<'a>(
    egl: &EglInstance,
    gl: &Gles2,
    slot: &'a mut Option<OverlayContext>,
    target: Target,
    config_id: egl::Int,
) -> Result<&'a mut OverlayContext, EglError> {
    if let Some(stale) =
        slot.take_if(|context| context.display != target.display || context.config_id != config_id)
    {
        egl.destroy_context(stale.display, stale.context)
            .map_err(EglError::Context)?;
    }
    if let Some(context) = slot {
        return Ok(context);
    }
    let config = egl
        .choose_first_config(target.display, &[egl::CONFIG_ID, config_id, egl::NONE])
        .map_err(|error| EglError::Display(format!("eglChooseConfig failed: {error}")))?
        .ok_or(EglError::NoConfig)?;
    let context = egl
        .create_context(
            target.display,
            config,
            None,
            &egl_engine::gles2_context_attribs(),
        )
        .map_err(EglError::Context)?;
    let prepared = egl
        .make_current(
            target.display,
            Some(target.surface),
            Some(target.surface),
            Some(context),
        )
        .map_err(EglError::Present)
        .and_then(|()| unsafe { prepare_field_drawing(gl) });
    if let Err(error) = prepared {
        if let Err(destroy) = egl.destroy_context(target.display, context) {
            tracing::warn!(%destroy, "destroying the focused-text OpenGL ES context failed");
        }
        return Err(error);
    }
    Ok(slot.insert(OverlayContext {
        display: target.display,
        config_id,
        context,
        uploaded: None,
    }))
}

unsafe fn prepare_field_drawing(gl: &Gles2) -> Result<(), EglError> {
    unsafe {
        let program = compile_program(gl, FIELD_VERTEX.as_bytes(), FIELD_FRAGMENT.as_bytes())?;
        let position = attribute(gl, program, c"aPos")?;
        let uv = attribute(gl, program, c"aUv")?;
        let mut texture = 0;
        (gl.gl_gen_textures)(1, &mut texture);
        (gl.gl_bind_texture)(GL_TEXTURE_2D, texture);
        (gl.gl_tex_parameteri)(GL_TEXTURE_2D, GL_TEXTURE_MIN_FILTER, GL_NEAREST);
        (gl.gl_tex_parameteri)(GL_TEXTURE_2D, GL_TEXTURE_MAG_FILTER, GL_NEAREST);
        (gl.gl_tex_parameteri)(GL_TEXTURE_2D, GL_TEXTURE_WRAP_S, GL_CLAMP_TO_EDGE);
        (gl.gl_tex_parameteri)(GL_TEXTURE_2D, GL_TEXTURE_WRAP_T, GL_CLAMP_TO_EDGE);
        let mut buffer = 0;
        (gl.gl_gen_buffers)(1, &mut buffer);
        (gl.gl_bind_buffer)(GL_ARRAY_BUFFER, buffer);
        (gl.gl_use_program)(program);
        (gl.gl_enable_vertex_attrib_array)(position);
        (gl.gl_vertex_attrib_pointer)(
            position,
            2,
            GL_FLOAT,
            GL_FALSE,
            VERTEX_STRIDE,
            std::ptr::null(),
        );
        (gl.gl_enable_vertex_attrib_array)(uv);
        (gl.gl_vertex_attrib_pointer)(
            uv,
            2,
            GL_FLOAT,
            GL_FALSE,
            VERTEX_STRIDE,
            UV_OFFSET as *const std::ffi::c_void,
        );
        (gl.gl_enable)(GL_BLEND);
        (gl.gl_blend_func)(GL_ONE, GL_ONE_MINUS_SRC_ALPHA);
        (gl.gl_color_mask)(GL_TRUE, GL_TRUE, GL_TRUE, GL_FALSE);
    }
    gl.check("setting up the focused-text overlay")
}

unsafe fn attribute(gl: &Gles2, program: u32, name: &std::ffi::CStr) -> Result<u32, EglError> {
    let location = unsafe { (gl.gl_get_attrib_location)(program, name.as_ptr()) };
    u32::try_from(location).map_err(|_| {
        EglError::Gl(format!(
            "the focused-text program has no {} attribute",
            name.to_string_lossy()
        ))
    })
}

unsafe fn upload_field(
    gl: &Gles2,
    uploaded: &mut Option<Upload>,
    texels: &mut Vec<[u8; 4]>,
    layer: &TextLayer,
    caret_on: bool,
    rect: SurfaceRect,
) -> Result<(), EglError> {
    if layer.pixels.len() != rect.width as usize * rect.height as usize {
        return Err(EglError::Gl(format!(
            "the focused-text layer holds {} pixels, not {}x{}",
            layer.pixels.len(),
            rect.width,
            rect.height
        )));
    }
    let upload = Upload {
        generation: layer.generation,
        caret: caret_on && layer.caret.is_some(),
        width: rect.width,
        height: rect.height,
    };
    if *uploaded == Some(upload) {
        return Ok(());
    }
    let pixels = field_texels(layer, upload.caret, rect.width, texels);
    unsafe {
        (gl.gl_tex_image_2d)(
            GL_TEXTURE_2D,
            0,
            GL_RGBA as i32,
            rect.width as i32,
            rect.height as i32,
            0,
            GL_RGBA,
            GL_UNSIGNED_BYTE,
            pixels.as_ptr().cast(),
        );
    }
    *uploaded = Some(upload);
    Ok(())
}

fn field_texels<'a>(
    layer: &'a TextLayer,
    caret_on: bool,
    width: u32,
    texels: &'a mut Vec<[u8; 4]>,
) -> &'a [[u8; 4]] {
    let Some(caret) = layer.caret.filter(|_| caret_on) else {
        return &layer.pixels;
    };
    let ink = crate::text_layout::premultiply(layer.caret_color);
    texels.clear();
    texels.extend_from_slice(&layer.pixels);
    for y in caret.y0..caret.y1 {
        for x in caret.x0..caret.x1 {
            if let Some(texel) = texels.get_mut((y * width + x) as usize) {
                *texel = crate::text_layout::over(ink, *texel);
            }
        }
    }
    texels
}

fn field_quad(
    rect: SurfaceRect,
    size: SurfaceSize,
) -> [f32; QUAD_VERTICES as usize * VERTEX_FLOATS] {
    let across = |pixel: f64, extent: u32| (2.0 * pixel / f64::from(extent) - 1.0) as f32;
    let left = across(f64::from(rect.x), size.width);
    let right = across(f64::from(rect.x) + f64::from(rect.width), size.width);
    let top = -across(f64::from(rect.y), size.height);
    let bottom = -across(f64::from(rect.y) + f64::from(rect.height), size.height);
    [
        left, top, 0.0, 0.0, right, top, 1.0, 0.0, left, bottom, 0.0, 1.0, left, bottom, 0.0, 1.0,
        right, top, 1.0, 0.0, right, bottom, 1.0, 1.0,
    ]
}

unsafe fn read_field_probe(gl: &Gles2, rect: SurfaceRect, size: SurfaceSize) {
    let (width, height) = (rect.width as usize, rect.height as usize);
    let mut bottom_up = vec![0u8; width * height * 4];
    unsafe {
        (gl.gl_read_pixels)(
            rect.x,
            size.height as i32 - rect.y - rect.height as i32,
            rect.width as i32,
            rect.height as i32,
            GL_RGBA,
            GL_UNSIGNED_BYTE,
            bottom_up.as_mut_ptr().cast(),
        );
    }
    let top_down: Vec<u8> = bottom_up
        .chunks_exact(width * 4)
        .rev()
        .flatten()
        .copied()
        .collect();
    text_overlay::write_field_probe(&top_down, width, height, ByteOrder::Rgba);
}

#[cfg(test)]
mod tests {
    use std::ffi::{c_uint, c_void};
    use std::num::NonZeroU64;
    use std::sync::atomic::AtomicI32;
    use std::time::Duration;

    use raw_window_handle::{RawDisplayHandle, XlibDisplayHandle};

    use super::super::frame_log::tests::{nanos, read_ring};
    use super::super::link::tests::temp_dir;
    use super::super::text_overlay::tests::{
        field_rect, focused_box, host_font_available, text_layer,
    };
    use super::*;
    use crate::egl_engine::{EngineNativeWindow, WindowGeometry};
    use crate::graphics::roleless_wayland_surface::RolelessWaylandSurface;
    use crate::graphics::unmapped_xlib_window::UnmappedXlibWindow;
    use crate::loader::native_provider::HOST_GLESV2_SONAME;

    const DRAW_CHILD: &str = "ECLIPSE_TEST_EGL_SEAM_CHILD";

    const WAYLAND_CHILD: &str = "ECLIPSE_TEST_EGL_SEAM_WAYLAND_CHILD";

    const WAYLAND_SWAPS: usize = 3;

    const DRAW_CHILD_LIMIT: Duration = Duration::from_secs(60);

    const WIDTH: c_uint = 640;
    const HEIGHT: c_uint = 240;

    const SCREEN: SurfaceSize = SurfaceSize {
        width: WIDTH,
        height: HEIGHT,
    };

    const CLIENT_CLEAR: [f32; 4] = [0.2, 0.4, 0.6, 1.0];
    const CLIENT_BACKGROUND: [u8; 3] = [51, 102, 153];
    const CLIENT_VIEWPORT: [i32; 4] = [3, 5, 101, 57];

    const GL_VIEWPORT: u32 = 0x0BA2;
    const GL_CURRENT_PROGRAM: u32 = 0x8B8D;
    const GL_COLOR_CLEAR_VALUE: u32 = 0x0C22;
    const GL_COLOR_BUFFER_BIT: u32 = 0x0000_4000;

    const CLIENT_VERTEX: &[u8] =
        b"attribute vec2 aPos;\nvoid main(){gl_Position=vec4(aPos,0.0,1.0);}\n\0";
    const CLIENT_FRAGMENT: &[u8] =
        b"precision mediump float;\nvoid main(){gl_FragColor=vec4(1.0,0.0,0.0,1.0);}\n\0";

    type GlGetIntegerv = unsafe extern "C" fn(u32, *mut i32);
    type GlGetFloatv = unsafe extern "C" fn(u32, *mut f32);
    type GlIsEnabled = unsafe extern "C" fn(u32) -> u8;

    static PLAIN_SWAPS: AtomicU64 = AtomicU64::new(0);
    static PACED_CALLS: AtomicU64 = AtomicU64::new(0);
    static PACED_INTERVAL: AtomicI32 = AtomicI32::new(-1);
    static PACED_SWAP_SAW_INTERVALS: AtomicU64 = AtomicU64::new(u64::MAX);
    static LOGGED_SWAP_AT: AtomicU64 = AtomicU64::new(0);
    static DRAWN_SWAPS: AtomicU64 = AtomicU64::new(0);

    unsafe extern "C" fn count_plain_swap(
        _display: egl::EGLDisplay,
        _surface: egl::EGLSurface,
    ) -> egl::Boolean {
        PLAIN_SWAPS.fetch_add(1, Ordering::SeqCst);
        egl::TRUE
    }

    unsafe extern "C" fn record_interval(
        _display: egl::EGLDisplay,
        interval: egl::Int,
    ) -> egl::Boolean {
        PACED_CALLS.fetch_add(1, Ordering::SeqCst);
        PACED_INTERVAL.store(interval, Ordering::SeqCst);
        egl::TRUE
    }

    unsafe extern "C" fn record_paced_swap(
        _display: egl::EGLDisplay,
        _surface: egl::EGLSurface,
    ) -> egl::Boolean {
        PACED_SWAP_SAW_INTERVALS.store(PACED_CALLS.load(Ordering::SeqCst), Ordering::SeqCst);
        egl::TRUE
    }

    unsafe extern "C" fn time_logged_swap(
        _display: egl::EGLDisplay,
        _surface: egl::EGLSurface,
    ) -> egl::Boolean {
        LOGGED_SWAP_AT.store(nanos(MonotonicNs::now()), Ordering::SeqCst);
        egl::TRUE
    }

    unsafe extern "C" fn count_drawn_swap(
        _display: egl::EGLDisplay,
        _surface: egl::EGLSurface,
    ) -> egl::Boolean {
        DRAWN_SWAPS.fetch_add(1, Ordering::SeqCst);
        egl::TRUE
    }

    #[test]
    fn a_frame_without_a_text_box_reaches_the_host_swap_once_and_draws_nothing() {
        let swapped = unsafe {
            swap_engine_frame(
                count_plain_swap,
                None,
                None,
                false,
                std::ptr::null_mut(),
                std::ptr::null_mut(),
            )
        };
        assert_eq!(swapped, egl::TRUE);
        assert_eq!(PLAIN_SWAPS.load(Ordering::SeqCst), 1);
        assert!(
            matches!(
                *OVERLAY.lock().unwrap_or_else(PoisonError::into_inner),
                OverlaySlot::Unloaded
            ),
            "a frame without a text box loaded nothing to draw with"
        );
    }

    #[test]
    fn only_wayland_engine_swaps_present_the_latest_frame() {
        assert!(swaps_latest_frame(Some(WsiTarget::Wayland {
            display: 1,
            surface: 2
        })));
        assert!(!swaps_latest_frame(Some(WsiTarget::Xlib {
            display: 1,
            window: 2
        })));
        assert!(!swaps_latest_frame(None));
    }

    #[test]
    fn a_latest_frame_swap_sets_interval_zero_before_the_host_swap() {
        let swapped = unsafe {
            swap_engine_frame(
                record_paced_swap,
                Some(record_interval),
                None,
                false,
                std::ptr::null_mut(),
                std::ptr::null_mut(),
            )
        };
        assert_eq!(swapped, egl::TRUE);
        assert_eq!(PACED_CALLS.load(Ordering::SeqCst), 1);
        assert_eq!(PACED_INTERVAL.load(Ordering::SeqCst), 0);
        assert_eq!(PACED_SWAP_SAW_INTERVALS.load(Ordering::SeqCst), 1);
    }

    #[test]
    fn a_logged_swap_records_one_entry_timed_around_the_host_swap() {
        let dir = temp_dir("egl-seam-frame-log");
        let path = dir.join("frames.bin");
        let log =
            FrameLog::create(&path, NonZeroU64::new(4).expect("four slots")).expect("frame log");
        let before = nanos(MonotonicNs::now());
        let swapped = unsafe {
            swap_engine_frame(
                time_logged_swap,
                None,
                Some(&log),
                false,
                std::ptr::null_mut(),
                std::ptr::null_mut(),
            )
        };
        let swapped_at = LOGGED_SWAP_AT.load(Ordering::SeqCst);
        let after = nanos(MonotonicNs::now());
        let ring = read_ring(&path);
        drop(log);
        std::fs::remove_dir_all(&dir).expect("remove the frame log");

        assert_eq!(swapped, egl::TRUE);
        assert_eq!(ring.written, 1);
        let entry = ring.entries[0];
        let host_started = entry.entered + u64::from(entry.seam);
        let host_ended = host_started + u64::from(entry.driver);
        assert!(
            before <= entry.entered
                && host_started <= swapped_at
                && swapped_at <= host_ended
                && host_ended <= after,
            "the entry is timed from the seam's entry around the host's swap: before {before}, \
             {entry:?}, host swapped at {swapped_at}, after {after}"
        );
    }

    #[test]
    fn the_field_quad_covers_its_rect_with_the_first_texture_row_on_top() {
        let quad = field_quad(
            SurfaceRect {
                x: 200,
                y: 150,
                width: 400,
                height: 75,
            },
            SurfaceSize {
                width: 800,
                height: 600,
            },
        );
        assert_eq!(
            quad,
            [
                -0.5, 0.5, 0.0, 0.0, 0.5, 0.5, 1.0, 0.0, -0.5, 0.25, 0.0, 1.0, -0.5, 0.25, 0.0,
                1.0, 0.5, 0.5, 1.0, 0.0, 0.5, 0.25, 1.0, 1.0,
            ]
        );
    }

    #[test]
    fn the_caret_is_drawn_into_the_field_texture_only_while_it_shows() {
        if !host_font_available() {
            return;
        }
        let overlay = focused_box("", 0);
        let rect = field_rect(&overlay, SCREEN);
        let layer = text_layer(&overlay, rect, ByteOrder::Rgba);
        let caret = layer.caret.expect("an empty focused box keeps its caret");
        let mut texels = Vec::new();
        assert_eq!(
            field_texels(&layer, false, rect.width, &mut texels),
            &layer.pixels[..]
        );
        let shown = field_texels(&layer, true, rect.width, &mut texels).to_vec();
        let ink = crate::text_layout::premultiply(layer.caret_color);
        for (index, (texel, pixel)) in shown.iter().zip(&layer.pixels).enumerate() {
            let (x, y) = (index as u32 % rect.width, index as u32 / rect.width);
            let on_caret = (caret.x0..caret.x1).contains(&x) && (caret.y0..caret.y1).contains(&y);
            let expected = if on_caret {
                crate::text_layout::over(ink, *pixel)
            } else {
                *pixel
            };
            assert_eq!(*texel, expected, "texel {x},{y}");
        }
        assert_ne!(shown, layer.pixels, "the caret shows");
    }

    struct ClientGl {
        egl: EglInstance,
        gl: Gles2,
        display: egl::Display,
        context: egl::Context,
        surface: egl::Surface,
        program: u32,
        gles: libloading::Library,
    }

    impl ClientGl {
        fn on_x11(window: &UnmappedXlibWindow) -> Self {
            Self::on(
                RawDisplayHandle::Xlib(XlibDisplayHandle::new(
                    std::ptr::NonNull::new(window.display),
                    window.screen,
                )),
                window.window as egl::NativeWindowType,
                Some(window.visual_id),
            )
        }

        fn on(
            display_handle: RawDisplayHandle,
            native_window: egl::NativeWindowType,
            visual: Option<std::ffi::c_ulong>,
        ) -> Self {
            let egl = egl_engine::load_host_egl().expect("host EGL");
            let display =
                egl_engine::initialized_display(&egl, display_handle).expect("EGL display");
            let wanted = [
                egl::SURFACE_TYPE,
                egl::WINDOW_BIT,
                egl::RENDERABLE_TYPE,
                egl::OPENGL_ES2_BIT,
                egl::NONE,
            ];
            let mut configs = Vec::with_capacity(
                egl.matching_config_count(display, &wanted)
                    .expect("eglChooseConfig"),
            );
            egl.choose_config(display, &wanted, &mut configs)
                .expect("eglChooseConfig");
            let config = configs
                .into_iter()
                .find(|&config| {
                    visual.is_none_or(|wanted| {
                        egl.get_config_attrib(display, config, egl::NATIVE_VISUAL_ID)
                            .is_ok_and(|visual| visual as std::ffi::c_ulong == wanted)
                    })
                })
                .expect("an EGL config for the window's visual");
            let context = egl
                .create_context(display, config, None, &egl_engine::gles2_context_attribs())
                .expect("the client's context");
            let surface =
                unsafe { egl.create_window_surface(display, config, native_window, None) }
                    .expect("the client's window surface");
            egl.make_current(display, Some(surface), Some(surface), Some(context))
                .expect("the client's context is current");
            let gl = Gles2::load(&egl).expect("GLES");
            let program = unsafe { compile_program(&gl, CLIENT_VERTEX, CLIENT_FRAGMENT) }
                .expect("the client's program");
            let gles = unsafe { libloading::Library::new(HOST_GLESV2_SONAME) }.expect("GLESv2");
            unsafe {
                let [x, y, width, height] = CLIENT_VIEWPORT;
                (gl.gl_viewport)(x, y, width, height);
                (gl.gl_use_program)(program);
                (gl.gl_enable)(GL_BLEND);
                let [red, green, blue, alpha] = CLIENT_CLEAR;
                (gl.gl_clear_color)(red, green, blue, alpha);
            }
            gl.check("the client's setup").expect("no GL error");
            Self {
                egl,
                gl,
                display,
                context,
                surface,
                program,
                gles,
            }
        }

        fn clear(&self) {
            unsafe { (self.gl.gl_clear)(GL_COLOR_BUFFER_BIT) };
        }

        fn frame(&self) -> Vec<u8> {
            let mut frame = vec![0u8; WIDTH as usize * HEIGHT as usize * 4];
            unsafe {
                (self.gl.gl_read_pixels)(
                    0,
                    0,
                    WIDTH as i32,
                    HEIGHT as i32,
                    GL_RGBA,
                    GL_UNSIGNED_BYTE,
                    frame.as_mut_ptr().cast::<c_void>(),
                );
            }
            frame
        }

        fn integers<const N: usize>(&self, name: u32) -> [i32; N] {
            let get = unsafe {
                *self
                    .gles
                    .get::<GlGetIntegerv>(b"glGetIntegerv\0")
                    .expect("glGetIntegerv")
            };
            let mut values = [0; N];
            unsafe { get(name, values.as_mut_ptr()) };
            values
        }

        fn clear_color(&self) -> [f32; 4] {
            let get = unsafe {
                *self
                    .gles
                    .get::<GlGetFloatv>(b"glGetFloatv\0")
                    .expect("glGetFloatv")
            };
            let mut values = [0.0; 4];
            unsafe { get(GL_COLOR_CLEAR_VALUE, values.as_mut_ptr()) };
            values
        }

        fn blends(&self) -> bool {
            let enabled = unsafe {
                *self
                    .gles
                    .get::<GlIsEnabled>(b"glIsEnabled\0")
                    .expect("glIsEnabled")
            };
            unsafe { enabled(GL_BLEND) == GL_TRUE }
        }
    }

    impl Drop for ClientGl {
        fn drop(&mut self) {
            self.egl
                .make_current(self.display, None, None, None)
                .expect("release the client's context");
            self.egl
                .destroy_surface(self.display, self.surface)
                .expect("destroy the client's surface");
            self.egl
                .destroy_context(self.display, self.context)
                .expect("destroy the client's context");
            self.egl.terminate(self.display).expect("eglTerminate");
        }
    }

    fn rgb(frame: &[u8], x: u32, y: u32) -> [u8; 3] {
        let at = (((HEIGHT - 1 - y) * WIDTH + x) * 4) as usize;
        [frame[at], frame[at + 1], frame[at + 2]]
    }

    fn inside(rect: SurfaceRect, x: u32, y: u32) -> bool {
        let (left, top) = (rect.x as u32, rect.y as u32);
        (left..left + rect.width).contains(&x) && (top..top + rect.height).contains(&y)
    }

    fn ink(frame: &[u8], rect: SurfaceRect) -> usize {
        (0..HEIGHT)
            .flat_map(|y| (0..WIDTH).map(move |x| (x, y)))
            .filter(|&(x, y)| inside(rect, x, y) && rgb(frame, x, y) != CLIENT_BACKGROUND)
            .count()
    }

    fn draw_over_the_client_frame(window: &UnmappedXlibWindow) {
        let client = ClientGl::on_x11(window);
        let rect = text_overlay::plan(SCREEN, ByteOrder::Rgba)
            .expect("ECLIPSE_VK_TEXT_TEST asks for the login field")
            .rect;
        let started = Instant::now();
        let mut swaps = 0;
        let frame = loop {
            client.clear();
            let swapped = unsafe {
                swap_engine_frame(
                    count_drawn_swap,
                    None,
                    None,
                    true,
                    client.display.as_ptr(),
                    client.surface.as_ptr(),
                )
            };
            swaps += 1;
            assert_eq!(swapped, egl::TRUE);
            let frame = client.frame();
            if ink(&frame, rect) >= 50 {
                break frame;
            }
            assert!(
                started.elapsed() < Duration::from_secs(10),
                "the seam never drew the focused text"
            );
            std::thread::sleep(Duration::from_millis(5));
        };

        assert_eq!(DRAWN_SWAPS.load(Ordering::SeqCst), swaps);
        for y in 0..HEIGHT {
            for x in (0..WIDTH).filter(|&x| !inside(rect, x, y)) {
                assert_eq!(rgb(&frame, x, y), CLIENT_BACKGROUND, "pixel {x},{y}");
            }
        }
        assert_eq!(client.egl.get_current_context(), Some(client.context));
        assert_eq!(
            client.egl.get_current_surface(egl::DRAW),
            Some(client.surface)
        );
        assert_eq!(
            client.egl.get_current_surface(egl::READ),
            Some(client.surface)
        );
        assert_eq!(client.integers::<4>(GL_VIEWPORT), CLIENT_VIEWPORT);
        assert_eq!(
            client.integers::<1>(GL_CURRENT_PROGRAM),
            [client.program as i32]
        );
        assert!(client.blends(), "the client's blending stays on");
        assert_eq!(client.clear_color(), CLIENT_CLEAR);
        client
            .gl
            .check("the client's context after the seam")
            .expect("the seam left no GL error in the client's context");
    }

    #[test]
    fn focused_text_is_drawn_over_the_client_frame_without_touching_its_gl_state() {
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
                    "loader::egl_seam::tests::\
                     focused_text_is_drawn_over_the_client_frame_without_touching_its_gl_state",
                    "--test-threads=1",
                    "--nocapture",
                ])
                .env(DRAW_CHILD, "1")
                .env("ECLIPSE_VK_TEXT_TEST", "abc")
                .env_remove("ECLIPSE_VK_PROBE")
                .env_remove("ECLIPSE_NO_VK_OVERLAY")
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
        if !host_font_available() {
            return;
        }
        let window = match UnmappedXlibWindow::open(WIDTH, HEIGHT) {
            Ok(window) => window,
            Err(error) => {
                eprintln!("SKIP: no usable X11 display ({error})");
                return;
            }
        };
        draw_over_the_client_frame(&window);
    }

    #[test]
    fn engine_swaps_on_a_wayland_surface_that_is_never_shown_return() {
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
                    "loader::egl_seam::tests::\
                     engine_swaps_on_a_wayland_surface_that_is_never_shown_return",
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
        let native = EngineNativeWindow::private(
            window_handle,
            WindowGeometry::from_physical(WIDTH, HEIGHT),
        )
        .expect("a wl_egl_window on the surface");
        let client = ClientGl::on(display_handle, native.as_native_window(), None);
        let host = host_swap_buffers().expect("the host's eglSwapBuffers");
        for _ in 0..WAYLAND_SWAPS {
            client.clear();
            let swapped = unsafe {
                swap_engine_frame(
                    host,
                    host_swap_interval(),
                    None,
                    false,
                    client.display.as_ptr(),
                    client.surface.as_ptr(),
                )
            };
            assert_eq!(swapped, egl::TRUE);
        }
    }
}
