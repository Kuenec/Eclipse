//! Process-global generational-slab registry for Eclipse-owned `android.graphics.Paint` handles.
//!
//! 2026-06-05: AOSP's `Paint.native_create()` creates a native paint object and returns its `long`
//! handle (`Paint.mNativePaint`); the `native_set*` configurators (color, text size, …) and the
//! `Canvas` drawing natives consume it. ATL backs this in C against GTK/Cairo; Eclipse must NOT pull
//! in GTK (AGENTS.md §5 Step 3.5), so a Paint's `long` handle is an **Eclipse-owned generational-slab
//! index into this slab — NOT `Box::into_raw`, NOT a raw pointer**, exactly the soundness pattern of
//! the sibling registries ([`window_registry`](super::window_registry) etc.). A stale/fabricated
//! `jlong` from Java is a bounds+generation-checked `Err`, never a wild dereference / UB.
//!
//! ## Handle layout
//! Identical to the sibling registries: a [`jlong`] packing a `u32` slot index (low 32 bits) + a
//! `u32` generation (high 32 bits). Generations start at 1, so a valid handle is never `0` (the
//! reserved "no paint" / null sentinel).
//!
//! ## Scope (this increment)
//! [`PaintState`] records the **drawing configuration** the Paint setters set (color, text size). It
//! does NO drawing and holds no GTK/Cairo context — the real ash/Vulkan text/shape rendering is the
//! deferred big build (AGENTS.md §5). Recording the config soundly is what lets the View hierarchy's
//! `TextPaint`/`Paint` construct during `setContentView` without GTK.

#![forbid(unsafe_code)]

use std::fmt;
use std::sync::{Mutex, OnceLock, PoisonError};

use jni::sys::jlong;

/// Process-global slab of [`PaintState`], guarded by a [`Mutex`]. Initialized on first use.
static PAINTS: OnceLock<Mutex<Registry>> = OnceLock::new();

/// A paint-registry handle as it travels across JNI: a `jlong` packing the slot index (low 32 bits)
/// and the slot's generation (high 32 bits). `0` is the reserved "no paint" / null sentinel.
pub type PaintHandle = jlong;

/// Errors from the paint registry. Every fallible path returns one of these instead of panicking, so
/// a stale/out-of-range/fabricated `jlong` from Java can never cause UB or unwind across JNI.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum PaintRegistryError {
    /// The handle's slot index is outside the slab (fabricated handle, or the reserved `0`).
    OutOfRange,
    /// The slot exists but its generation does not match: the handle refers to a freed (and possibly
    /// reused) slot. Never aliases the new occupant.
    StaleHandle,
    /// The registry mutex was poisoned by a panic in another holder. Surfaced as an error (not a
    /// re-panic) so the JNI path stays panic-free (AGENTS.md §2.8).
    Poisoned,
}

impl fmt::Display for PaintRegistryError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::OutOfRange => {
                f.write_str("paint handle slot index is out of range (fabricated or null handle)")
            }
            Self::StaleHandle => {
                f.write_str("paint handle refers to a freed slot (stale generation)")
            }
            Self::Poisoned => f.write_str("paint registry mutex was poisoned"),
        }
    }
}

impl std::error::Error for PaintRegistryError {}

/// `android.graphics.Paint.Style` — how a shape is painted: filled, stroked (outlined), or both.
/// 2026-06-05: the Canvas draw natives (drawRect/drawCircle/drawPath) read this to choose tiny-skia
/// `fill_path`/`fill_rect` vs `stroke_path`. Ordinals match AOSP `Paint.Style` (FILL=0, STROKE=1,
/// FILL_AND_STROKE=2), which is what `Paint.setStyle` passes to `native_setStyle`.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum PaintStyle {
    /// Fill the shape's interior (AOSP default).
    #[default]
    Fill,
    /// Stroke the shape's outline only.
    Stroke,
    /// Fill the interior AND stroke the outline.
    FillAndStroke,
}

impl PaintStyle {
    /// Map an AOSP `Paint.Style` ordinal (FILL=0, STROKE=1, FILL_AND_STROKE=2) to a [`PaintStyle`].
    /// An unknown ordinal falls back to `Fill` (the AOSP default) — never panics.
    pub fn from_ordinal(ordinal: i32) -> Self {
        match ordinal {
            1 => Self::Stroke,
            2 => Self::FillAndStroke,
            _ => Self::Fill,
        }
    }

    /// The AOSP `Paint.Style` ordinal for this style — the exact inverse of [`Self::from_ordinal`].
    /// 2026-07-02: `Paint.getStyle()` indexes `Style.values[native_get_style(paint)]`, so the value
    /// served back to Java MUST be a valid ordinal (0..=2) or ART throws
    /// `ArrayIndexOutOfBoundsException`; this is total and in-range by construction.
    pub fn ordinal(self) -> i32 {
        match self {
            Self::Fill => 0,
            Self::Stroke => 1,
            Self::FillAndStroke => 2,
        }
    }
}

/// `android.graphics.Paint.Cap` — how stroke ends are drawn. 2026-07-02: ordinals match AOSP
/// `Paint.Cap` (BUTT=0, ROUND=1, SQUARE=2), which is what `Paint.setStrokeCap` passes to
/// `native_set_stroke_cap` and what `Paint.getStrokeCap` indexes `Cap.values[...]` with (the ATL
/// reference native passes the same ordinals straight to GSK — `GSK_LINE_CAP_*` match 1:1).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum StrokeCap {
    /// The stroke ends with the path, no projection (AOSP default).
    #[default]
    Butt,
    /// The stroke projects out as a semicircle.
    Round,
    /// The stroke projects out as a square.
    Square,
}

impl StrokeCap {
    /// Map an AOSP `Paint.Cap` ordinal (BUTT=0, ROUND=1, SQUARE=2) to a [`StrokeCap`]. An unknown
    /// ordinal falls back to `Butt` (the AOSP default) — never panics.
    pub fn from_ordinal(ordinal: i32) -> Self {
        match ordinal {
            1 => Self::Round,
            2 => Self::Square,
            _ => Self::Butt,
        }
    }

    /// The AOSP `Paint.Cap` ordinal — exact inverse of [`Self::from_ordinal`], always in 0..=2
    /// (`Paint.getStrokeCap` indexes `Cap.values[...]` with it — see [`PaintStyle::ordinal`]).
    pub fn ordinal(self) -> i32 {
        match self {
            Self::Butt => 0,
            Self::Round => 1,
            Self::Square => 2,
        }
    }
}

/// `android.graphics.Paint.Join` — how stroke segments join. 2026-07-02: ordinals match AOSP
/// `Paint.Join` (MITER=0, ROUND=1, BEVEL=2); same set/get ordinal contract as [`StrokeCap`].
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum StrokeJoin {
    /// Outer edges meet at a sharp angle (AOSP default).
    #[default]
    Miter,
    /// Outer edges meet in a circular arc.
    Round,
    /// Outer edges meet with a straight line.
    Bevel,
}

impl StrokeJoin {
    /// Map an AOSP `Paint.Join` ordinal (MITER=0, ROUND=1, BEVEL=2) to a [`StrokeJoin`]. An unknown
    /// ordinal falls back to `Miter` (the AOSP default) — never panics.
    pub fn from_ordinal(ordinal: i32) -> Self {
        match ordinal {
            1 => Self::Round,
            2 => Self::Bevel,
            _ => Self::Miter,
        }
    }

    /// The AOSP `Paint.Join` ordinal — exact inverse of [`Self::from_ordinal`], always in 0..=2
    /// (`Paint.getStrokeJoin` indexes `Join.values[...]` with it).
    pub fn ordinal(self) -> i32 {
        match self {
            Self::Miter => 0,
            Self::Round => 1,
            Self::Bevel => 2,
        }
    }
}

/// Per-paint state held in a registry slot: the non-GTK drawing configuration.
///
/// 2026-06-05: the config the Canvas draw natives consume — `color` (ARGB, `Paint.native_setColor`),
/// `text_size` (`Paint.native_setTextSize`), `stroke_width` (`Paint.native_set_stroke_width`), and
/// `style` (`Paint.native_setStyle`). Holds no GTK/Cairo context and performs no drawing itself; the
/// Canvas natives read it to drive tiny-skia. More fields (typeface, flags) are added when their
/// setters are bound. 2026-07-02: `Clone` because `Paint.native_clone` ([`clone_of`]) copies a whole
/// slot's state (`Paint(Paint)` / `Paint.set(Paint)`).
#[derive(Debug, Clone)]
pub struct PaintState {
    /// ARGB color (`Paint.setColor`); opaque black (0xFF000000, the AOSP default) until set — see
    /// the `Default` impl below.
    pub color: i32,
    /// Text size in pixels (`Paint.setTextSize`); 0.0 until set.
    pub text_size: f32,
    /// Stroke width in pixels (`Paint.setStrokeWidth`); 0.0 until set (a 0 width is AOSP's "hairline").
    pub stroke_width: f32,
    /// Fill vs stroke style (`Paint.setStyle`); [`PaintStyle::Fill`] (AOSP default) until set.
    pub style: PaintStyle,
    /// Stroke end cap (`Paint.setStrokeCap`); [`StrokeCap::Butt`] (AOSP default) until set.
    pub stroke_cap: StrokeCap,
    /// Stroke segment join (`Paint.setStrokeJoin`); [`StrokeJoin::Miter`] (AOSP default) until set.
    pub stroke_join: StrokeJoin,
}

impl Default for PaintState {
    fn default() -> Self {
        // 2026-07-02: a fresh paint's color is OPAQUE BLACK (0xFF000000) — the AOSP default Paint
        // color and exactly what the ATL reference native creates (`native_create` sets
        // `color.alpha = 1.f` over zeroed RGB, so its `native_get_color` reads 0xFF000000). This
        // became load-bearing when `native_get_color`/`native_get_alpha` were bound: Java reads the
        // values back, and a fresh AOSP Paint must report alpha 255, not 0 (a 0 default would make
        // e.g. alpha-scaling drawables compute fully-transparent). Matches
        // `canvas_registry::PaintConfig::default` (already opaque black).
        Self {
            color: 0xFF00_0000u32 as i32,
            text_size: 0.0,
            stroke_width: 0.0,
            style: PaintStyle::Fill,
            stroke_cap: StrokeCap::Butt,
            stroke_join: StrokeJoin::Miter,
        }
    }
}

/// A generational slot: the current generation plus the optional occupant.
struct Slot {
    generation: u32,
    state: Option<PaintState>,
}

/// The slab + free list (same shape as the sibling registries).
#[derive(Default)]
struct Registry {
    slots: Vec<Slot>,
    free: Vec<u32>,
}

/// Pack a slot index + generation into a `jlong` handle (generation high, index low).
fn pack(index: u32, generation: u32) -> PaintHandle {
    ((generation as u64) << 32 | index as u64) as i64
}

/// Unpack a `jlong` handle into (slot index, generation).
fn unpack(handle: PaintHandle) -> (u32, u32) {
    let bits = handle as u64;
    ((bits & 0xFFFF_FFFF) as u32, (bits >> 32) as u32)
}

/// Lock the process-global registry, mapping a poisoned mutex to the typed
/// [`PaintRegistryError::Poisoned`] (never a panic — AGENTS.md §2.8).
fn lock() -> Result<std::sync::MutexGuard<'static, Registry>, PaintRegistryError> {
    PAINTS
        .get_or_init(|| Mutex::new(Registry::default()))
        .lock()
        .map_err(|_: PoisonError<_>| PaintRegistryError::Poisoned)
}

/// Place `state` into a free (or fresh) slot of the already-locked registry and return its packed
/// handle. 2026-07-02: shared allocation core of [`allocate`] and [`clone_of`].
fn allocate_state(
    reg: &mut Registry,
    state: PaintState,
) -> Result<PaintHandle, PaintRegistryError> {
    if let Some(index) = reg.free.pop() {
        let slot = &mut reg.slots[index as usize];
        slot.state = Some(state);
        return Ok(pack(index, slot.generation));
    }
    let index: u32 = reg
        .slots
        .len()
        .try_into()
        .map_err(|_| PaintRegistryError::OutOfRange)?;
    reg.slots.push(Slot {
        generation: 1,
        state: Some(state),
    });
    Ok(pack(index, 1))
}

/// Allocate a fresh paint slot and return its packed [`PaintHandle`] (`jlong`, generation ≥ 1, never
/// the reserved `0`). Reuses a freed slot when available, else grows the slab. Returns
/// [`PaintRegistryError::Poisoned`] only on a poisoned mutex — never panics.
pub fn allocate() -> Result<PaintHandle, PaintRegistryError> {
    let mut reg = lock()?;
    allocate_state(&mut reg, PaintState::default())
}

/// Allocate a new slot whose state is a copy of `source`'s and return its handle — the backing for
/// `Paint.native_clone` (`Paint(Paint)` copy constructor + `Paint.set(Paint)`). 2026-07-02: done
/// under ONE registry lock so the copy is atomic (the source cannot be freed between read and
/// write). Validates `source` exactly like [`with_paint`]: a stale/out-of-range/`0` source is a
/// typed `Err`, never UB — the caller decides the fallback.
pub fn clone_of(source: PaintHandle) -> Result<PaintHandle, PaintRegistryError> {
    let (index, generation) = unpack(source);
    let mut reg = lock()?;
    let slot = reg
        .slots
        .get(index as usize)
        .ok_or(PaintRegistryError::OutOfRange)?;
    if slot.generation != generation {
        return Err(PaintRegistryError::StaleHandle);
    }
    let state = slot
        .state
        .as_ref()
        .ok_or(PaintRegistryError::StaleHandle)?
        .clone();
    allocate_state(&mut reg, state)
}

/// Look up the [`PaintState`] for a `handle` and run `f` against it (mutable) under the registry
/// lock. Bounds-checks the slot index **and** verifies the handle's generation, so a stale/
/// out-of-range/fabricated handle returns `Err` and never dereferences out of bounds or aliases a
/// different paint. The reserved `0` handle fails the check (live generations are ≥ 1).
pub fn with_paint<R>(
    handle: PaintHandle,
    f: impl FnOnce(&mut PaintState) -> R,
) -> Result<R, PaintRegistryError> {
    let (index, generation) = unpack(handle);
    let mut reg = lock()?;
    let slot = reg
        .slots
        .get_mut(index as usize)
        .ok_or(PaintRegistryError::OutOfRange)?;
    if slot.generation != generation {
        return Err(PaintRegistryError::StaleHandle);
    }
    let state = slot.state.as_mut().ok_or(PaintRegistryError::StaleHandle)?;
    Ok(f(state))
}

/// Free the slot a `handle` refers to, bumping its generation so any other handle to it (or this one,
/// reused later) is rejected as [`PaintRegistryError::StaleHandle`]. Validates the handle the same
/// way [`with_paint`] does, so freeing an already-freed/stale/fabricated handle returns `Err`.
pub fn free(handle: PaintHandle) -> Result<(), PaintRegistryError> {
    let (index, generation) = unpack(handle);
    let mut reg = lock()?;
    let slot = reg
        .slots
        .get_mut(index as usize)
        .ok_or(PaintRegistryError::OutOfRange)?;
    if slot.generation != generation || slot.state.is_none() {
        return Err(PaintRegistryError::StaleHandle);
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

    // 2026-06-05: same soundness contract as the sibling registries; fully in-harness (no VM).

    #[test]
    fn allocate_returns_distinct_nonzero_handles() {
        let a = allocate().expect("allocate a");
        let b = allocate().expect("allocate b");
        assert_ne!(a, b, "distinct allocations must yield distinct handles");
        assert_ne!(a, 0, "a valid handle is never the reserved null 0");
        assert_ne!(b, 0, "a valid handle is never the reserved null 0");
        free(a).expect("free a");
        free(b).expect("free b");
    }

    #[test]
    fn with_paint_mutates_the_right_slot() {
        let a = allocate().expect("allocate a");
        let b = allocate().expect("allocate b");
        with_paint(a, |s| s.color = 0x00ff_00ff).expect("with_paint a");
        with_paint(b, |s| s.text_size = 14.0).expect("with_paint b");
        let ca = with_paint(a, |s| s.color).expect("read a");
        let tb = with_paint(b, |s| s.text_size).expect("read b");
        assert_eq!(ca, 0x00ff_00ff, "handle a addresses its own slot");
        assert_eq!(tb, 14.0, "handle b addresses its own slot");
        free(a).expect("free a");
        free(b).expect("free b");
    }

    #[test]
    fn freed_handle_is_stale_and_does_not_alias_reused_slot() {
        let old = allocate().expect("allocate old");
        with_paint(old, |s| s.color = 0x11).expect("write old");
        free(old).expect("free old");

        let new = allocate().expect("allocate new");
        assert_eq!(
            with_paint(old, |s| s.color),
            Err(PaintRegistryError::StaleHandle),
            "a freed handle must be StaleHandle, never alias the reused slot"
        );
        assert_eq!(
            with_paint(new, |s| s.color),
            Ok(0xFF00_0000u32 as i32),
            "the live handle's reused slot is fresh (default opaque black, 2026-07-02)"
        );
        free(new).expect("free new");
    }

    #[test]
    fn out_of_range_and_fabricated_handles_return_err_not_panic() {
        let fabricated = pack(u32::MAX, 1);
        assert_eq!(
            with_paint(fabricated, |_| ()),
            Err(PaintRegistryError::OutOfRange),
            "a fabricated out-of-range index must be OutOfRange, never an out-of-bounds deref"
        );
        let null_lookup = with_paint(0, |_| ());
        assert!(
            matches!(
                null_lookup,
                Err(PaintRegistryError::StaleHandle) | Err(PaintRegistryError::OutOfRange)
            ),
            "the reserved null handle 0 must be rejected, got {null_lookup:?}"
        );
        assert_eq!(free(fabricated), Err(PaintRegistryError::OutOfRange));
    }

    #[test]
    fn double_free_is_rejected() {
        let h = allocate().expect("allocate");
        free(h).expect("first free");
        assert_eq!(free(h), Err(PaintRegistryError::StaleHandle));
    }

    #[test]
    fn paint_style_from_ordinal_maps_aosp_values_and_defaults_to_fill() {
        // AOSP Paint.Style ordinals: FILL=0, STROKE=1, FILL_AND_STROKE=2; anything else → Fill.
        assert_eq!(PaintStyle::from_ordinal(0), PaintStyle::Fill);
        assert_eq!(PaintStyle::from_ordinal(1), PaintStyle::Stroke);
        assert_eq!(PaintStyle::from_ordinal(2), PaintStyle::FillAndStroke);
        assert_eq!(PaintStyle::from_ordinal(99), PaintStyle::Fill);
        assert_eq!(PaintStyle::from_ordinal(-1), PaintStyle::Fill);
        assert_eq!(PaintStyle::default(), PaintStyle::Fill);
    }

    #[test]
    fn style_cap_join_ordinals_round_trip_and_stay_in_java_values_range() {
        // 2026-07-02: Paint.getStyle/getStrokeCap/getStrokeJoin index `Enum.values[ordinal]` with the
        // native's return, so ordinal() must be the exact inverse of from_ordinal for 0..=2 AND stay
        // in-range for ANY recorded value (out-of-range would be a Java ArrayIndexOutOfBoundsException).
        for ordinal in 0..=2 {
            assert_eq!(PaintStyle::from_ordinal(ordinal).ordinal(), ordinal);
            assert_eq!(StrokeCap::from_ordinal(ordinal).ordinal(), ordinal);
            assert_eq!(StrokeJoin::from_ordinal(ordinal).ordinal(), ordinal);
        }
        // AOSP enum constants pinned by name: Cap BUTT/ROUND/SQUARE, Join MITER/ROUND/BEVEL.
        assert_eq!(StrokeCap::from_ordinal(0), StrokeCap::Butt);
        assert_eq!(StrokeCap::from_ordinal(1), StrokeCap::Round);
        assert_eq!(StrokeCap::from_ordinal(2), StrokeCap::Square);
        assert_eq!(StrokeJoin::from_ordinal(0), StrokeJoin::Miter);
        assert_eq!(StrokeJoin::from_ordinal(1), StrokeJoin::Round);
        assert_eq!(StrokeJoin::from_ordinal(2), StrokeJoin::Bevel);
        // Unknown ordinals fall back to the AOSP defaults (BUTT/MITER) — in-range, never a panic.
        assert_eq!(StrokeCap::from_ordinal(99).ordinal(), 0);
        assert_eq!(StrokeCap::from_ordinal(-1).ordinal(), 0);
        assert_eq!(StrokeJoin::from_ordinal(99).ordinal(), 0);
        assert_eq!(StrokeJoin::from_ordinal(-1).ordinal(), 0);
    }

    #[test]
    fn fresh_paint_defaults_are_aosp_reference_values() {
        // 2026-07-02: the value-returning Paint getters serve these back to Java, so a fresh slot
        // must read as AOSP's default Paint: opaque black (alpha 255), FILL, BUTT, MITER, hairline
        // stroke. (The ATL reference native creates alpha=1.0 over zeroed RGB — the same 0xFF000000.)
        let h = allocate().expect("allocate");
        let s = with_paint(h, |s| s.clone()).expect("read");
        assert_eq!(
            s.color, 0xFF00_0000u32 as i32,
            "default color = opaque black"
        );
        assert_eq!((s.color >> 24) & 0xFF, 0xFF, "default alpha byte = 255");
        assert_eq!(s.style, PaintStyle::Fill);
        assert_eq!(s.stroke_cap, StrokeCap::Butt);
        assert_eq!(s.stroke_join, StrokeJoin::Miter);
        assert_eq!(s.stroke_width, 0.0);
        assert_eq!(s.text_size, 0.0);
        free(h).expect("free");
    }

    #[test]
    fn clone_of_copies_state_into_an_independent_slot() {
        // 2026-07-02: Paint.native_clone (Paint(Paint) / set(Paint)) — the clone carries the source's
        // full recorded state but is an independent slot: later writes to either side do not alias.
        let src = allocate().expect("allocate src");
        with_paint(src, |s| {
            s.color = 0x80AB_CDEFu32 as i32;
            s.stroke_width = 2.5;
            s.stroke_cap = StrokeCap::Square;
            s.stroke_join = StrokeJoin::Bevel;
            s.style = PaintStyle::Stroke;
            s.text_size = 11.0;
        })
        .expect("configure src");
        let dup = clone_of(src).expect("clone_of");
        assert_ne!(dup, src, "the clone is its own handle");
        let copied = with_paint(dup, |s| s.clone()).expect("read dup");
        assert_eq!(copied.color, 0x80AB_CDEFu32 as i32);
        assert_eq!(copied.stroke_width, 2.5);
        assert_eq!(copied.stroke_cap, StrokeCap::Square);
        assert_eq!(copied.stroke_join, StrokeJoin::Bevel);
        assert_eq!(copied.style, PaintStyle::Stroke);
        assert_eq!(copied.text_size, 11.0);
        // Independence: mutate the source; the clone must not follow.
        with_paint(src, |s| s.color = 0).expect("mutate src");
        assert_eq!(
            with_paint(dup, |s| s.color).expect("re-read dup"),
            0x80AB_CDEFu32 as i32,
            "the clone is a copy, not an alias"
        );
        free(src).expect("free src");
        free(dup).expect("free dup");
    }

    #[test]
    fn clone_of_rejects_stale_null_and_fabricated_sources() {
        // Same validation contract as with_paint: a dead source is a typed Err (the JNI binding
        // falls back to a fresh default paint), never UB.
        let h = allocate().expect("allocate");
        free(h).expect("free");
        assert_eq!(clone_of(h), Err(PaintRegistryError::StaleHandle));
        assert!(matches!(
            clone_of(0),
            Err(PaintRegistryError::StaleHandle) | Err(PaintRegistryError::OutOfRange)
        ));
        assert_eq!(
            clone_of(pack(u32::MAX, 1)),
            Err(PaintRegistryError::OutOfRange)
        );
    }

    #[test]
    fn with_paint_records_stroke_width_and_style() {
        let h = allocate().expect("allocate");
        with_paint(h, |s| {
            s.stroke_width = 6.5;
            s.style = PaintStyle::Stroke;
        })
        .expect("write");
        let (w, st) = with_paint(h, |s| (s.stroke_width, s.style)).expect("read");
        assert_eq!(w, 6.5);
        assert_eq!(st, PaintStyle::Stroke);
        free(h).expect("free");
    }

    #[test]
    fn pack_unpack_round_trips() {
        for &(index, generation) in &[(0u32, 1u32), (1, 1), (5, 42), (u32::MAX, u32::MAX), (3, 7)] {
            let handle = pack(index, generation);
            assert_eq!(unpack(handle), (index, generation));
        }
    }
}
