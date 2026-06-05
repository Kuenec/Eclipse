//! Process-global generational-slab registry for Eclipse-owned `android.graphics.Canvas` handles.
//!
//! 2026-06-05: AOSP's `Canvas` natives draw shapes/text/bitmaps into a backing buffer. ATL backs
//! `Canvas` in C against GTK/Cairo; Eclipse must NOT pull in GTK (AGENTS.md §5 Step 3.5), so a
//! `Canvas`'s `long` handle is an **Eclipse-owned generational-slab index into this slab — NOT a raw
//! pointer**, exactly the soundness pattern of the sibling registries
//! ([`paint_registry`](super::paint_registry), [`path_registry`](super::path_registry), etc.). A
//! stale/fabricated `jlong` from Java is a bounds+generation-checked `Err`, never a wild dereference.
//!
//! ## What it draws into
//! Each [`CanvasState`] owns a pure-Rust **tiny-skia [`Pixmap`]** (the same Skia-subset rasterizer the
//! `graphics` module already uses for vector paths — no C/GTK/Cairo). The Canvas draw natives
//! (`drawColor`/`drawRect`/`drawCircle`/`drawPath`/`drawText`) call this module's methods, which issue
//! **real tiny-skia fills/strokes** using the [`PaintConfig`] the caller built from
//! [`paint_registry`](super::paint_registry) and the [`PathGeometry`](super::path_registry) the caller
//! built from [`path_registry`](super::path_registry). The pixels are real raster output — never
//! fabricated (AGENTS.md core principle). The compositor (`graphics`) uploads `pixmap.data()` as an
//! RGBA GPU texture and draws it over the owning view's rect.
//!
//! ## Handle layout
//! Identical to the sibling registries: a [`jlong`] packing a `u32` slot index (low 32 bits) + a
//! `u32` generation (high 32 bits). Generations start at 1, so a valid handle is never `0`.

#![forbid(unsafe_code)]

use std::fmt;
use std::sync::{Mutex, OnceLock, PoisonError};

use jni::sys::jlong;
use tiny_skia::{FillRule, Paint, PathBuilder, Pixmap, Rect, Stroke, Transform};

use super::paint_registry::PaintStyle;
use super::path_registry::{PathGeometry, Verb};

/// Process-global slab of [`CanvasState`], guarded by a [`Mutex`]. Initialized on first use.
static CANVASES: OnceLock<Mutex<Registry>> = OnceLock::new();

/// A canvas-registry handle as it travels across JNI: a `jlong` packing the slot index (low 32 bits)
/// and the slot's generation (high 32 bits). `0` is the reserved "no canvas" / null sentinel.
pub type CanvasHandle = jlong;

/// The maximum pixel dimension a single Canvas target may have. Bounds the backing allocation so a
/// fabricated/huge view rect cannot request an unreasonable buffer (2026-06-05; well above any real
/// on-screen view at the swapchain extent).
pub const MAX_CANVAS_DIMENSION: u32 = 8192;

/// Errors from the canvas registry. Every fallible path returns one of these instead of panicking, so
/// a stale/out-of-range/fabricated `jlong` from Java can never cause UB or unwind across JNI.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum CanvasRegistryError {
    /// The handle's slot index is outside the slab (fabricated handle, or the reserved `0`).
    OutOfRange,
    /// The slot exists but its generation does not match: the handle refers to a freed (and possibly
    /// reused) slot. Never aliases the new occupant.
    StaleHandle,
    /// The registry mutex was poisoned by a panic in another holder. Surfaced as an error (not a
    /// re-panic) so the JNI path stays panic-free (AGENTS.md §2.8).
    Poisoned,
    /// The requested backing dimensions are zero or exceed [`MAX_CANVAS_DIMENSION`] (a bad view rect).
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

/// The drawing configuration a Canvas draw consumes, snapshotted from a `Paint` (`paint_registry`) at
/// the call site. Kept as a plain value (not a `paint_registry` handle) so the canvas lock and the
/// paint lock are never held at once.
///
/// 2026-06-05: `argb` (`Paint.getColor`), `style` (FILL/STROKE/FILL_AND_STROKE), `stroke_width`
/// (pixels), and the path fill rule (`even_odd`). Anti-aliasing is always on (AOSP `Paint` defaults
/// `ANTI_ALIAS` off, but for the on-screen composite smooth edges read better; the source coverage is
/// still real raster).
#[derive(Debug, Clone, Copy)]
pub struct PaintConfig {
    /// ARGB color (`0xAARRGGBB`) as `Paint.setColor` stores it.
    pub argb: i32,
    /// Fill vs stroke style.
    pub style: PaintStyle,
    /// Stroke width in pixels (0 = tiny-skia hairline).
    pub stroke_width: f32,
    /// `true` for even-odd, `false` for non-zero winding (AOSP `Path` default).
    pub even_odd: bool,
}

impl Default for PaintConfig {
    fn default() -> Self {
        // Opaque black, fill, winding — AOSP's default Paint color + Path fill type.
        Self {
            argb: 0xFF00_0000u32 as i32,
            style: PaintStyle::Fill,
            stroke_width: 0.0,
            even_odd: false,
        }
    }
}

/// Per-canvas state: the tiny-skia [`Pixmap`] draw target. Holds no GTK/Cairo context.
///
/// 2026-06-05: the canvas owns its target so the draw natives write real coverage into it; the
/// compositor reads `pixmap` back as straight RGBA. A future save/restore matrix stack hangs here.
pub struct CanvasState {
    /// The RGBA draw target (premultiplied storage, transparent-black background until drawn).
    pixmap: Pixmap,
}

impl CanvasState {
    /// The backing pixel dimensions (width, height).
    pub fn dimensions(&self) -> (u32, u32) {
        (self.pixmap.width(), self.pixmap.height())
    }

    /// Straight (un-premultiplied) RGBA8 bytes of the current target, for a GPU texture upload.
    /// tiny-skia stores premultiplied; `take`-style demultiply yields what a straight-alpha sampler
    /// expects. Borrows (does not consume) so the canvas can keep drawing.
    pub fn rgba(&self) -> Vec<u8> {
        // `Pixmap::data` is premultiplied; convert to straight RGBA via a clone+demultiply. The clone
        // is off the gameplay hot path (a per-view composite snapshot), so the copy is acceptable.
        self.pixmap.clone().take_demultiplied()
    }

    /// `Canvas.drawColor(color)` / `drawARGB` / `drawRGB` — fill the entire target with a solid ARGB
    /// color (AOSP src-over; here a plain fill since the background is transparent and this is the
    /// first op typical custom views issue to clear the canvas).
    pub fn draw_color(&mut self, argb: i32) {
        let (r, g, b, a) = argb_channels(argb);
        // tiny-skia `Color::from_rgba8` takes straight channels.
        self.pixmap.fill(tiny_skia::Color::from_rgba8(r, g, b, a));
    }

    /// `Canvas.drawRect(left, top, right, bottom, paint)` — fill or stroke an axis-aligned rectangle.
    /// A degenerate (non-positive) rect is a no-op (matches AOSP, which draws nothing).
    pub fn draw_rect(&mut self, left: f32, top: f32, right: f32, bottom: f32, cfg: &PaintConfig) {
        let (w, h) = (right - left, bottom - top);
        let Some(rect) = Rect::from_xywh(left, top, w, h) else {
            return; // zero/negative size or non-finite → nothing to draw.
        };
        let paint = build_paint(cfg);
        if fills(cfg.style) {
            self.pixmap
                .fill_rect(rect, &paint, Transform::identity(), None);
        }
        if strokes(cfg.style) {
            // tiny-skia has no stroke_rect; build the rect as a closed path and stroke it.
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

    /// `Canvas.drawCircle(cx, cy, radius, paint)` — fill or stroke a circle. A non-positive radius is a
    /// no-op (matches AOSP).
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

    /// `Canvas.drawPath(path, paint)` — fill or stroke an arbitrary contour built from a
    /// [`PathGeometry`] (`path_registry`). A degenerate/empty geometry is a no-op.
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

/// `true` if a [`PaintStyle`] paints the interior.
fn fills(style: PaintStyle) -> bool {
    matches!(style, PaintStyle::Fill | PaintStyle::FillAndStroke)
}

/// `true` if a [`PaintStyle`] paints the outline.
fn strokes(style: PaintStyle) -> bool {
    matches!(style, PaintStyle::Stroke | PaintStyle::FillAndStroke)
}

/// Split an AOSP `0xAARRGGBB` int into straight RGBA8 channels.
fn argb_channels(argb: i32) -> (u8, u8, u8, u8) {
    let v = argb as u32;
    (
        (v >> 16) as u8, // r
        (v >> 8) as u8,  // g
        v as u8,         // b
        (v >> 24) as u8, // a
    )
}

/// A tiny-skia [`Paint`] from a [`PaintConfig`]'s color (anti-aliased).
fn build_paint(cfg: &PaintConfig) -> Paint<'static> {
    let (r, g, b, a) = argb_channels(cfg.argb);
    let mut paint = Paint::default();
    paint.set_color_rgba8(r, g, b, a);
    paint.anti_alias = true;
    paint
}

/// A tiny-skia [`Stroke`] from a [`PaintConfig`]'s width (0 → hairline). Default cap/join.
fn build_stroke(cfg: &PaintConfig) -> Stroke {
    Stroke {
        width: cfg.stroke_width.max(0.0),
        ..Stroke::default()
    }
}

/// Build a closed axis-aligned-rect path for stroking.
fn rect_path(left: f32, top: f32, right: f32, bottom: f32) -> Option<tiny_skia::Path> {
    let mut pb = PathBuilder::new();
    pb.move_to(left, top);
    pb.line_to(right, top);
    pb.line_to(right, bottom);
    pb.line_to(left, bottom);
    pb.close();
    pb.finish()
}

/// Walk an Eclipse [`PathGeometry`] verb/point buffer into a tiny-skia path. A malformed buffer (a
/// verb wanting more points than remain — impossible from the registry ops, but checked so a
/// fabricated geometry can never panic/overrun) ends the walk early. `None` for an empty/degenerate
/// path. Mirrors `graphics::build_tiny_skia_path` (kept local so this module owns its draw logic).
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

/// A generational slot: the current generation plus the optional occupant.
struct Slot {
    generation: u32,
    state: Option<CanvasState>,
}

/// The slab + free list (same shape as the sibling registries).
#[derive(Default)]
struct Registry {
    slots: Vec<Slot>,
    free: Vec<u32>,
}

/// Pack a slot index + generation into a `jlong` handle (generation high, index low).
fn pack(index: u32, generation: u32) -> CanvasHandle {
    ((generation as u64) << 32 | index as u64) as i64
}

/// Unpack a `jlong` handle into (slot index, generation).
fn unpack(handle: CanvasHandle) -> (u32, u32) {
    let bits = handle as u64;
    ((bits & 0xFFFF_FFFF) as u32, (bits >> 32) as u32)
}

/// Lock the process-global registry, mapping a poisoned mutex to the typed
/// [`CanvasRegistryError::Poisoned`] (never a panic — AGENTS.md §2.8).
fn lock() -> Result<std::sync::MutexGuard<'static, Registry>, CanvasRegistryError> {
    CANVASES
        .get_or_init(|| Mutex::new(Registry::default()))
        .lock()
        .map_err(|_: PoisonError<_>| CanvasRegistryError::Poisoned)
}

/// Allocate a fresh canvas slot with a `width`×`height` transparent RGBA target and return its packed
/// [`CanvasHandle`] (`jlong`, generation ≥ 1, never the reserved `0`). Rejects zero/oversize
/// dimensions ([`CanvasRegistryError::BadDimensions`]). Reuses a freed slot when available, else grows
/// the slab. Returns [`CanvasRegistryError::Poisoned`] only on a poisoned mutex — never panics.
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

/// Look up the [`CanvasState`] for a `handle` and run `f` against it (mutable) under the registry
/// lock. Bounds-checks the slot index **and** verifies the handle's generation, so a stale/
/// out-of-range/fabricated handle returns `Err` and never dereferences out of bounds or aliases a
/// different canvas. The reserved `0` handle fails the check (live generations are ≥ 1).
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

/// Free the slot a `handle` refers to, bumping its generation so any other handle to it (or this one,
/// reused later) is rejected as [`CanvasRegistryError::StaleHandle`]. Validates the handle the same
/// way [`with_canvas`] does, so freeing an already-freed/stale/fabricated handle returns `Err`.
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
    // Bump (saturating) so the freed handle and any copy become stale and can never alias a reuse.
    slot.generation = slot.generation.saturating_add(1);
    reg.free.push(index);
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    // 2026-06-05: same soundness contract as the sibling registries + real pixel assertions on the
    // tiny-skia draws (no GPU, no VM). RGBA layout is [r,g,b,a] per pixel, row-major.

    /// Read the straight-RGBA pixel at (x, y) from a canvas handle.
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
        // Opaque red.
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
            argb: 0xFF00_FF00u32 as i32, // opaque green
            style: PaintStyle::Fill,
            ..Default::default()
        };
        with_canvas(h, |c| c.draw_rect(5.0, 5.0, 15.0, 15.0, &cfg)).expect("draw_rect");
        // Interior is green.
        assert_eq!(pixel(h, 10, 10), [0, 255, 0, 255], "interior filled green");
        // Outside the rect is untouched (transparent).
        assert_eq!(pixel(h, 0, 0), [0, 0, 0, 0], "exterior transparent");
        assert_eq!(pixel(h, 19, 19), [0, 0, 0, 0], "exterior transparent");
        free(h).expect("free");
    }

    #[test]
    fn draw_circle_fills_center_and_leaves_corners_transparent() {
        let h = allocate(40, 40).expect("allocate");
        let cfg = PaintConfig {
            argb: 0xFF00_00FFu32 as i32, // opaque blue
            style: PaintStyle::Fill,
            ..Default::default()
        };
        with_canvas(h, |c| c.draw_circle(20.0, 20.0, 12.0, &cfg)).expect("draw_circle");
        // Center is inside the circle → blue.
        assert_eq!(
            pixel(h, 20, 20),
            [0, 0, 255, 255],
            "circle center filled blue"
        );
        // A corner is well outside the radius-12 circle → transparent.
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
        // A triangle covering the center.
        geo.move_to(5.0, 25.0);
        geo.line_to(25.0, 25.0);
        geo.line_to(15.0, 5.0);
        geo.close();
        let cfg = PaintConfig {
            argb: 0xFFFF_FF00u32 as i32, // opaque yellow
            style: PaintStyle::Fill,
            ..Default::default()
        };
        with_canvas(h, |c| c.draw_path(&geo, &cfg)).expect("draw_path");
        // A point inside the triangle is yellow.
        assert_eq!(
            pixel(h, 15, 20),
            [255, 255, 0, 255],
            "triangle interior yellow"
        );
        // The top-left corner is outside the triangle → transparent.
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
        // A STROKE-only circle leaves the center transparent (only the ring is drawn).
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
        // The reused slot is a FRESH transparent target.
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
