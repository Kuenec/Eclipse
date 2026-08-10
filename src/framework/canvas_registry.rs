#![forbid(unsafe_code)]

use std::fmt;
use std::sync::{Mutex, OnceLock, PoisonError};

use jni::sys::jlong;
use tiny_skia::{FillRule, Paint, PathBuilder, Pixmap, Rect, Stroke, Transform};

use super::paint_registry::PaintStyle;
use super::path_registry::{PathGeometry, Verb};

static CANVASES: OnceLock<Mutex<Registry>> = OnceLock::new();

pub type CanvasHandle = jlong;

pub const MAX_CANVAS_DIMENSION: u32 = 8192;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum CanvasRegistryError {
    OutOfRange,

    StaleHandle,

    Poisoned,

    BadDimensions,
}

impl fmt::Display for CanvasRegistryError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::OutOfRange => {
                f.write_str("canvas handle slot index is out of range (fabricated or null handle)")
            }
            Self::StaleHandle => {
                f.write_str("canvas handle refers to a freed slot (stale generation)")
            }
            Self::Poisoned => f.write_str("canvas registry mutex was poisoned"),
            Self::BadDimensions => {
                f.write_str("canvas backing dimensions are zero or exceed the maximum")
            }
        }
    }
}

impl std::error::Error for CanvasRegistryError {}

#[derive(Debug, Clone, Copy)]
pub struct PaintConfig {
    pub argb: i32,

    pub style: PaintStyle,

    pub stroke_width: f32,

    pub even_odd: bool,
}

impl Default for PaintConfig {
    fn default() -> Self {
        Self {
            argb: 0xFF00_0000u32 as i32,
            style: PaintStyle::Fill,
            stroke_width: 0.0,
            even_odd: false,
        }
    }
}

pub struct CanvasState {
    pixmap: Pixmap,
}

impl CanvasState {
    pub fn dimensions(&self) -> (u32, u32) {
        (self.pixmap.width(), self.pixmap.height())
    }

    pub fn rgba(&self) -> Vec<u8> {
        self.pixmap.clone().take_demultiplied()
    }

    pub fn draw_color(&mut self, argb: i32) {
        let (r, g, b, a) = argb_channels(argb);

        self.pixmap.fill(tiny_skia::Color::from_rgba8(r, g, b, a));
    }

    pub fn draw_rect(&mut self, left: f32, top: f32, right: f32, bottom: f32, cfg: &PaintConfig) {
        let (w, h) = (right - left, bottom - top);
        let Some(rect) = Rect::from_xywh(left, top, w, h) else {
            return;
        };
        let paint = build_paint(cfg);
        if fills(cfg.style) {
            self.pixmap
                .fill_rect(rect, &paint, Transform::identity(), None);
        }
        if strokes(cfg.style) {
            if let Some(path) = rect_path(left, top, right, bottom) {
                self.pixmap.stroke_path(
                    &path,
                    &paint,
                    &build_stroke(cfg),
                    Transform::identity(),
                    None,
                );
            }
        }
    }

    pub fn draw_circle(&mut self, cx: f32, cy: f32, radius: f32, cfg: &PaintConfig) {
        if !radius.is_finite() || radius <= 0.0 || !cx.is_finite() || !cy.is_finite() {
            return;
        }
        let mut pb = PathBuilder::new();
        pb.push_circle(cx, cy, radius);
        let Some(path) = pb.finish() else {
            return;
        };
        let paint = build_paint(cfg);
        if fills(cfg.style) {
            self.pixmap.fill_path(
                &path,
                &paint,
                FillRule::Winding,
                Transform::identity(),
                None,
            );
        }
        if strokes(cfg.style) {
            self.pixmap.stroke_path(
                &path,
                &paint,
                &build_stroke(cfg),
                Transform::identity(),
                None,
            );
        }
    }

    pub fn draw_path(&mut self, geometry: &PathGeometry, cfg: &PaintConfig) {
        let Some(path) = geometry_to_path(geometry) else {
            return;
        };
        let paint = build_paint(cfg);
        let fill_rule = if cfg.even_odd {
            FillRule::EvenOdd
        } else {
            FillRule::Winding
        };
        if fills(cfg.style) {
            self.pixmap
                .fill_path(&path, &paint, fill_rule, Transform::identity(), None);
        }
        if strokes(cfg.style) {
            self.pixmap.stroke_path(
                &path,
                &paint,
                &build_stroke(cfg),
                Transform::identity(),
                None,
            );
        }
    }
}

fn fills(style: PaintStyle) -> bool {
    matches!(style, PaintStyle::Fill | PaintStyle::FillAndStroke)
}

fn strokes(style: PaintStyle) -> bool {
    matches!(style, PaintStyle::Stroke | PaintStyle::FillAndStroke)
}

fn argb_channels(argb: i32) -> (u8, u8, u8, u8) {
    let v = argb as u32;
    ((v >> 16) as u8, (v >> 8) as u8, v as u8, (v >> 24) as u8)
}

fn build_paint(cfg: &PaintConfig) -> Paint<'static> {
    let (r, g, b, a) = argb_channels(cfg.argb);
    let mut paint = Paint::default();
    paint.set_color_rgba8(r, g, b, a);
    paint.anti_alias = true;
    paint
}

fn build_stroke(cfg: &PaintConfig) -> Stroke {
    Stroke {
        width: cfg.stroke_width.max(0.0),
        ..Stroke::default()
    }
}

fn rect_path(left: f32, top: f32, right: f32, bottom: f32) -> Option<tiny_skia::Path> {
    let mut pb = PathBuilder::new();
    pb.move_to(left, top);
    pb.line_to(right, top);
    pb.line_to(right, bottom);
    pb.line_to(left, bottom);
    pb.close();
    pb.finish()
}

fn geometry_to_path(geometry: &PathGeometry) -> Option<tiny_skia::Path> {
    let mut pb = PathBuilder::new();
    let pts = &geometry.points;
    let mut i = 0usize;
    for verb in &geometry.verbs {
        let need = verb.point_count() * 2;
        if i + need > pts.len() {
            break;
        }
        match verb {
            Verb::MoveTo => pb.move_to(pts[i], pts[i + 1]),
            Verb::LineTo => pb.line_to(pts[i], pts[i + 1]),
            Verb::QuadTo => pb.quad_to(pts[i], pts[i + 1], pts[i + 2], pts[i + 3]),
            Verb::CubicTo => pb.cubic_to(
                pts[i],
                pts[i + 1],
                pts[i + 2],
                pts[i + 3],
                pts[i + 4],
                pts[i + 5],
            ),
            Verb::Close => pb.close(),
        }
        i += need;
    }
    pb.finish()
}

struct Slot {
    generation: u32,
    state: Option<CanvasState>,
}

#[derive(Default)]
struct Registry {
    slots: Vec<Slot>,
    free: Vec<u32>,
}

fn pack(index: u32, generation: u32) -> CanvasHandle {
    ((generation as u64) << 32 | index as u64) as i64
}

fn unpack(handle: CanvasHandle) -> (u32, u32) {
    let bits = handle as u64;
    ((bits & 0xFFFF_FFFF) as u32, (bits >> 32) as u32)
}

fn lock() -> Result<std::sync::MutexGuard<'static, Registry>, CanvasRegistryError> {
    CANVASES
        .get_or_init(|| Mutex::new(Registry::default()))
        .lock()
        .map_err(|_: PoisonError<_>| CanvasRegistryError::Poisoned)
}

pub fn allocate(width: u32, height: u32) -> Result<CanvasHandle, CanvasRegistryError> {
    if width == 0 || height == 0 || width > MAX_CANVAS_DIMENSION || height > MAX_CANVAS_DIMENSION {
        return Err(CanvasRegistryError::BadDimensions);
    }
    let pixmap = Pixmap::new(width, height).ok_or(CanvasRegistryError::BadDimensions)?;
    let state = CanvasState { pixmap };
    let mut reg = lock()?;
    if let Some(index) = reg.free.pop() {
        let slot = &mut reg.slots[index as usize];
        slot.state = Some(state);
        return Ok(pack(index, slot.generation));
    }
    let index: u32 = reg
        .slots
        .len()
        .try_into()
        .map_err(|_| CanvasRegistryError::OutOfRange)?;
    reg.slots.push(Slot {
        generation: 1,
        state: Some(state),
    });
    Ok(pack(index, 1))
}

pub fn with_canvas<R>(
    handle: CanvasHandle,
    f: impl FnOnce(&mut CanvasState) -> R,
) -> Result<R, CanvasRegistryError> {
    let (index, generation) = unpack(handle);
    let mut reg = lock()?;
    let slot = reg
        .slots
        .get_mut(index as usize)
        .ok_or(CanvasRegistryError::OutOfRange)?;
    if slot.generation != generation {
        return Err(CanvasRegistryError::StaleHandle);
    }
    let state = slot
        .state
        .as_mut()
        .ok_or(CanvasRegistryError::StaleHandle)?;
    Ok(f(state))
}

pub fn free(handle: CanvasHandle) -> Result<(), CanvasRegistryError> {
    let (index, generation) = unpack(handle);
    let mut reg = lock()?;
    let slot = reg
        .slots
        .get_mut(index as usize)
        .ok_or(CanvasRegistryError::OutOfRange)?;
    if slot.generation != generation || slot.state.is_none() {
        return Err(CanvasRegistryError::StaleHandle);
    }
    slot.state = None;

    slot.generation = slot.generation.saturating_add(1);
    reg.free.push(index);
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    fn pixel(handle: CanvasHandle, x: u32, y: u32) -> [u8; 4] {
        with_canvas(handle, |c| {
            let (w, _h) = c.dimensions();
            let rgba = c.rgba();
            let off = ((y * w + x) * 4) as usize;
            [rgba[off], rgba[off + 1], rgba[off + 2], rgba[off + 3]]
        })
        .expect("read pixel")
    }

    #[test]
    fn allocate_returns_distinct_nonzero_handles_and_rejects_bad_dimensions() {
        let a = allocate(10, 10).expect("allocate a");
        let b = allocate(10, 10).expect("allocate b");
        assert_ne!(a, b);
        assert_ne!(a, 0);
        assert_eq!(allocate(0, 10), Err(CanvasRegistryError::BadDimensions));
        assert_eq!(
            allocate(MAX_CANVAS_DIMENSION + 1, 10),
            Err(CanvasRegistryError::BadDimensions)
        );
        free(a).expect("free a");
        free(b).expect("free b");
    }

    #[test]
    fn draw_color_fills_the_whole_target() {
        let h = allocate(4, 4).expect("allocate");

        with_canvas(h, |c| c.draw_color(0xFFFF_0000u32 as i32)).expect("draw_color");
        for (x, y) in [(0, 0), (3, 3), (2, 1)] {
            assert_eq!(
                pixel(h, x, y),
                [255, 0, 0, 255],
                "every pixel is opaque red"
            );
        }
        free(h).expect("free");
    }

    #[test]
    fn draw_rect_fills_interior_and_leaves_exterior_transparent() {
        let h = allocate(20, 20).expect("allocate");
        let cfg = PaintConfig {
            argb: 0xFF00_FF00u32 as i32,
            style: PaintStyle::Fill,
            ..Default::default()
        };
        with_canvas(h, |c| c.draw_rect(5.0, 5.0, 15.0, 15.0, &cfg)).expect("draw_rect");

        assert_eq!(pixel(h, 10, 10), [0, 255, 0, 255], "interior filled green");

        assert_eq!(pixel(h, 0, 0), [0, 0, 0, 0], "exterior transparent");
        assert_eq!(pixel(h, 19, 19), [0, 0, 0, 0], "exterior transparent");
        free(h).expect("free");
    }

    #[test]
    fn draw_circle_fills_center_and_leaves_corners_transparent() {
        let h = allocate(40, 40).expect("allocate");
        let cfg = PaintConfig {
            argb: 0xFF00_00FFu32 as i32,
            style: PaintStyle::Fill,
            ..Default::default()
        };
        with_canvas(h, |c| c.draw_circle(20.0, 20.0, 12.0, &cfg)).expect("draw_circle");

        assert_eq!(
            pixel(h, 20, 20),
            [0, 0, 255, 255],
            "circle center filled blue"
        );

        assert_eq!(
            pixel(h, 0, 0),
            [0, 0, 0, 0],
            "corner outside circle transparent"
        );
        free(h).expect("free");
    }

    #[test]
    fn draw_path_fills_a_triangle_interior() {
        let h = allocate(30, 30).expect("allocate");
        let mut geo = PathGeometry::default();

        geo.move_to(5.0, 25.0);
        geo.line_to(25.0, 25.0);
        geo.line_to(15.0, 5.0);
        geo.close();
        let cfg = PaintConfig {
            argb: 0xFFFF_FF00u32 as i32,
            style: PaintStyle::Fill,
            ..Default::default()
        };
        with_canvas(h, |c| c.draw_path(&geo, &cfg)).expect("draw_path");

        assert_eq!(
            pixel(h, 15, 20),
            [255, 255, 0, 255],
            "triangle interior yellow"
        );

        assert_eq!(pixel(h, 0, 0), [0, 0, 0, 0], "outside triangle transparent");
        free(h).expect("free");
    }

    #[test]
    fn draw_circle_stroke_leaves_center_transparent() {
        let h = allocate(40, 40).expect("allocate");
        let cfg = PaintConfig {
            argb: 0xFFFF_0000u32 as i32,
            style: PaintStyle::Stroke,
            stroke_width: 2.0,
            ..Default::default()
        };
        with_canvas(h, |c| c.draw_circle(20.0, 20.0, 12.0, &cfg)).expect("draw_circle stroke");

        assert_eq!(
            pixel(h, 20, 20),
            [0, 0, 0, 0],
            "stroke-only circle has a hollow center"
        );
        free(h).expect("free");
    }

    #[test]
    fn freed_handle_is_stale_and_does_not_alias_reused_slot() {
        let old = allocate(8, 8).expect("allocate old");
        with_canvas(old, |c| c.draw_color(0x11u32 as i32)).expect("draw old");
        free(old).expect("free old");
        let new = allocate(8, 8).expect("allocate new");
        assert_eq!(
            with_canvas(old, |_| ()),
            Err(CanvasRegistryError::StaleHandle),
            "a freed handle must be StaleHandle, never alias the reused slot"
        );

        assert_eq!(
            pixel(new, 0, 0),
            [0, 0, 0, 0],
            "reused slot is fresh/transparent"
        );
        free(new).expect("free new");
    }

    #[test]
    fn out_of_range_and_double_free_return_err_not_panic() {
        let fabricated = pack(u32::MAX, 1);
        assert_eq!(
            with_canvas(fabricated, |_| ()),
            Err(CanvasRegistryError::OutOfRange)
        );
        let h = allocate(4, 4).expect("allocate");
        free(h).expect("first free");
        assert_eq!(free(h), Err(CanvasRegistryError::StaleHandle));
    }

    #[test]
    fn pack_unpack_round_trips() {
        for &(index, generation) in &[(0u32, 1u32), (1, 1), (5, 42), (u32::MAX, u32::MAX), (3, 7)] {
            let handle = pack(index, generation);
            assert_eq!(unpack(handle), (index, generation));
        }
    }
}
