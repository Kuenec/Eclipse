//! Process-global generational-slab registry for Eclipse-owned `android.graphics.Path` geometry.
//!
//! 2026-06-05: AOSP's modern `Path` routes its construction operations through a *builder*:
//! `Path.getBuilder()` calls `native long native_create_builder(long nativePath, long reserve)` once
//! (lazily, on the first `moveTo`/`lineTo`/… after a mutation), then each `moveTo`/`lineTo`/`quadTo`/
//! `cubicTo`/`close`/`addRoundRect`/… is a native op against that builder handle, and the builder is
//! folded back into the `Path`'s native object. ATL backs this in C against Skia/GTK; Eclipse must NOT
//! pull in GTK (AGENTS.md §5 Step 3.5), and a `Path` is **real vector geometry** — so a `Path`'s/
//! builder's `long` handle is an **Eclipse-owned generational-slab index into this slab — NOT
//! `Box::into_raw`, NOT a raw pointer**, exactly the soundness pattern of the sibling registries
//! ([`matrix_registry`](super::matrix_registry) etc.). A stale/fabricated `jlong` from Java is a
//! bounds+generation-checked `Err`, never a wild dereference / UB.
//!
//! ## Handle layout
//! Identical to the sibling registries: a [`jlong`] packing a `u32` slot index (low 32 bits) + a
//! `u32` generation (high 32 bits). Generations start at 1, so a valid handle is never `0` (the
//! reserved "no path" / null sentinel AOSP `Path.java` uses for an empty native object).
//!
//! ## The geometry value
//! [`PathGeometry`] holds the **real** path as an ordered list of [`Verb`]s + a flat point buffer (the
//! exact contour data the `moveTo`/`lineTo`/`quadTo`/`cubicTo`/`close` ops build). This is the
//! resolution-independent source of truth; the software rasterizer ([`crate::graphics`]) walks it into
//! a tiny-skia path for filling. No GPU/GTK is needed to build geometry — faking it is forbidden
//! (AGENTS.md core principle); these ops record the actual parsed coordinates.

#![forbid(unsafe_code)]

use std::fmt;
use std::sync::{Mutex, OnceLock, PoisonError};

use jni::sys::jlong;

/// Process-global slab of [`PathGeometry`], guarded by a [`Mutex`]. Initialized on first use.
static PATHS: OnceLock<Mutex<Registry>> = OnceLock::new();

/// A path-registry handle as it travels across JNI: a `jlong` packing the slot index (low 32 bits)
/// and the slot's generation (high 32 bits). `0` is the reserved "no path" / null sentinel.
pub type PathHandle = jlong;

/// Errors from the path registry. Every fallible path returns one of these instead of panicking, so a
/// stale/out-of-range/fabricated `jlong` from Java can never cause UB or unwind across JNI.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum PathRegistryError {
    /// The handle's slot index is outside the slab (fabricated handle, or the reserved `0`).
    OutOfRange,
    /// The slot exists but its generation does not match: the handle refers to a freed (and possibly
    /// reused) slot. Never aliases the new occupant.
    StaleHandle,
    /// The registry mutex was poisoned by a panic in another holder. Surfaced as an error (not a
    /// re-panic) so the JNI path stays panic-free (AGENTS.md §2.8).
    Poisoned,
}

impl fmt::Display for PathRegistryError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::OutOfRange => {
                f.write_str("path handle slot index is out of range (fabricated or null handle)")
            }
            Self::StaleHandle => {
                f.write_str("path handle refers to a freed slot (stale generation)")
            }
            Self::Poisoned => f.write_str("path registry mutex was poisoned"),
        }
    }
}

impl std::error::Error for PathRegistryError {}

/// A single path command, mirroring `android.graphics.Path` / Skia contour verbs. Each verb's point
/// arguments are appended to [`PathGeometry::points`] in order; the verb names how many of those
/// trailing points it consumes.
#[derive(Debug, Clone, Copy, PartialEq)]
pub enum Verb {
    /// `Path.moveTo(x, y)` — start a new contour at the next 1 point.
    MoveTo,
    /// `Path.lineTo(x, y)` — straight segment to the next 1 point.
    LineTo,
    /// `Path.quadTo(cx, cy, x, y)` — quadratic Bézier through the next 2 points (control, end).
    QuadTo,
    /// `Path.cubicTo(c1x, c1y, c2x, c2y, x, y)` — cubic Bézier through the next 3 points.
    CubicTo,
    /// `Path.close()` — close the current contour back to its start. Consumes no points.
    Close,
}

impl Verb {
    /// How many `(x, y)` points this verb consumes from the flat point buffer.
    pub const fn point_count(self) -> usize {
        match self {
            Self::MoveTo | Self::LineTo => 1,
            Self::QuadTo => 2,
            Self::CubicTo => 3,
            Self::Close => 0,
        }
    }
}

/// The real vector geometry of one `android.graphics.Path` / `PathBuilder`.
///
/// 2026-06-05: an ordered [`Verb`] list plus a flat `(x, y)` point buffer (`points[2*i]` = x,
/// `points[2*i+1]` = y). This is exactly the contour data `PathParser`/`Path.moveTo`… build; it is the
/// resolution-independent source the rasterizer consumes. Default is an empty path.
#[derive(Debug, Default, Clone, PartialEq)]
pub struct PathGeometry {
    /// Path commands in build order.
    pub verbs: Vec<Verb>,
    /// Flat `[x0, y0, x1, y1, …]` point buffer; consumed by the verbs in order.
    pub points: Vec<f32>,
}

impl PathGeometry {
    /// `Path.moveTo(x, y)`.
    pub fn move_to(&mut self, x: f32, y: f32) {
        self.verbs.push(Verb::MoveTo);
        self.points.push(x);
        self.points.push(y);
    }

    /// `Path.lineTo(x, y)`.
    pub fn line_to(&mut self, x: f32, y: f32) {
        self.verbs.push(Verb::LineTo);
        self.points.push(x);
        self.points.push(y);
    }

    /// `Path.quadTo(cx, cy, x, y)` (control point then end point).
    pub fn quad_to(&mut self, cx: f32, cy: f32, x: f32, y: f32) {
        self.verbs.push(Verb::QuadTo);
        self.points.extend_from_slice(&[cx, cy, x, y]);
    }

    /// `Path.cubicTo(c1x, c1y, c2x, c2y, x, y)` (two control points then end point).
    #[allow(clippy::too_many_arguments)] // mirrors AOSP Path.cubicTo's 6 float args exactly.
    pub fn cubic_to(&mut self, c1x: f32, c1y: f32, c2x: f32, c2y: f32, x: f32, y: f32) {
        self.verbs.push(Verb::CubicTo);
        self.points.extend_from_slice(&[c1x, c1y, c2x, c2y, x, y]);
    }

    /// `Path.close()`.
    pub fn close(&mut self) {
        self.verbs.push(Verb::Close);
    }

    /// `Path.reset()` / `Path.rewind()` — drop all geometry, keeping the allocation for reuse.
    pub fn reset(&mut self) {
        self.verbs.clear();
        self.points.clear();
    }

    /// `true` iff this path has no contours (`Path.isEmpty()`).
    pub fn is_empty(&self) -> bool {
        self.verbs.is_empty()
    }

    /// Axis-aligned bounds of all points (`Path.computeBounds`-style), as `(min_x, min_y, max_x,
    /// max_y)`, or `None` for an empty path. Uses the raw control/anchor points (a conservative
    /// superset of the true curve bounds — sufficient for sizing a raster target without overflow).
    pub fn bounds(&self) -> Option<(f32, f32, f32, f32)> {
        let mut it = self.points.chunks_exact(2);
        let first = it.next()?;
        let (mut min_x, mut min_y) = (first[0], first[1]);
        let (mut max_x, mut max_y) = (first[0], first[1]);
        for p in std::iter::once(first).chain(it) {
            min_x = min_x.min(p[0]);
            min_y = min_y.min(p[1]);
            max_x = max_x.max(p[0]);
            max_y = max_y.max(p[1]);
        }
        Some((min_x, min_y, max_x, max_y))
    }
}

/// A generational slot: the current generation plus the optional occupant.
struct Slot {
    generation: u32,
    geometry: Option<PathGeometry>,
}

/// The slab + free list (same shape as the sibling registries).
#[derive(Default)]
struct Registry {
    slots: Vec<Slot>,
    free: Vec<u32>,
}

/// Pack a slot index + generation into a `jlong` handle (generation high, index low).
fn pack(index: u32, generation: u32) -> PathHandle {
    ((generation as u64) << 32 | index as u64) as i64
}

/// Unpack a `jlong` handle into (slot index, generation).
fn unpack(handle: PathHandle) -> (u32, u32) {
    let bits = handle as u64;
    ((bits & 0xFFFF_FFFF) as u32, (bits >> 32) as u32)
}

/// Lock the process-global registry, mapping a poisoned mutex to the typed
/// [`PathRegistryError::Poisoned`] (never a panic — AGENTS.md §2.8).
fn lock() -> Result<std::sync::MutexGuard<'static, Registry>, PathRegistryError> {
    PATHS
        .get_or_init(|| Mutex::new(Registry::default()))
        .lock()
        .map_err(|_: PoisonError<_>| PathRegistryError::Poisoned)
}

/// Allocate a fresh path slot holding `geometry` and return its packed [`PathHandle`] (`jlong`,
/// generation ≥ 1, never the reserved `0`). Reuses a freed slot when available, else grows the slab.
/// Returns [`PathRegistryError::Poisoned`] only on a poisoned mutex — never panics.
pub fn allocate(geometry: PathGeometry) -> Result<PathHandle, PathRegistryError> {
    let mut reg = lock()?;
    if let Some(index) = reg.free.pop() {
        let slot = &mut reg.slots[index as usize];
        slot.geometry = Some(geometry);
        return Ok(pack(index, slot.generation));
    }
    let index: u32 = reg
        .slots
        .len()
        .try_into()
        .map_err(|_| PathRegistryError::OutOfRange)?;
    reg.slots.push(Slot {
        generation: 1,
        geometry: Some(geometry),
    });
    Ok(pack(index, 1))
}

/// Read a clone of the [`PathGeometry`] a `handle` refers to. Bounds-checks the slot index **and**
/// verifies the handle's generation, so a stale/out-of-range/fabricated handle returns `Err` and never
/// dereferences out of bounds. Used by the rasterizer to snapshot a path's geometry.
pub fn get(handle: PathHandle) -> Result<PathGeometry, PathRegistryError> {
    let (index, generation) = unpack(handle);
    let reg = lock()?;
    let slot = reg
        .slots
        .get(index as usize)
        .ok_or(PathRegistryError::OutOfRange)?;
    if slot.generation != generation {
        return Err(PathRegistryError::StaleHandle);
    }
    slot.geometry.clone().ok_or(PathRegistryError::StaleHandle)
}

/// Look up the [`PathGeometry`] for a `handle` and run `f` against it (mutable) under the registry
/// lock. Bounds-checks the slot index **and** verifies the handle's generation, so a stale/
/// out-of-range/fabricated handle returns `Err` and never dereferences out of bounds or aliases a
/// different path. The reserved `0` handle fails the check (live generations are ≥ 1).
pub fn with_path<R>(
    handle: PathHandle,
    f: impl FnOnce(&mut PathGeometry) -> R,
) -> Result<R, PathRegistryError> {
    let (index, generation) = unpack(handle);
    let mut reg = lock()?;
    let slot = reg
        .slots
        .get_mut(index as usize)
        .ok_or(PathRegistryError::OutOfRange)?;
    if slot.generation != generation {
        return Err(PathRegistryError::StaleHandle);
    }
    let geometry = slot
        .geometry
        .as_mut()
        .ok_or(PathRegistryError::StaleHandle)?;
    Ok(f(geometry))
}

/// Free the slot a `handle` refers to, bumping its generation so any other handle to it (or this one,
/// reused later) is rejected as [`PathRegistryError::StaleHandle`]. Validates the handle the same way
/// [`with_path`] does, so freeing an already-freed/stale/fabricated handle returns `Err`.
pub fn free(handle: PathHandle) -> Result<(), PathRegistryError> {
    let (index, generation) = unpack(handle);
    let mut reg = lock()?;
    let slot = reg
        .slots
        .get_mut(index as usize)
        .ok_or(PathRegistryError::OutOfRange)?;
    if slot.generation != generation || slot.geometry.is_none() {
        return Err(PathRegistryError::StaleHandle);
    }
    slot.geometry = None;
    // Bump (saturating) so the freed handle and any copy become stale and can never alias a reuse.
    slot.generation = slot.generation.saturating_add(1);
    reg.free.push(index);
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    // 2026-06-05: soundness contract matches the sibling registries + the verb-buffer ops are exact.
    // Fully in-harness (no VM, GPU-free).

    #[test]
    fn allocate_returns_distinct_nonzero_handles() {
        let a = allocate(PathGeometry::default()).expect("allocate a");
        let b = allocate(PathGeometry::default()).expect("allocate b");
        assert_ne!(a, b, "distinct allocations must yield distinct handles");
        assert_ne!(a, 0, "a valid handle is never the reserved null 0");
        assert_ne!(b, 0, "a valid handle is never the reserved null 0");
        free(a).expect("free a");
        free(b).expect("free b");
    }

    #[test]
    fn with_path_records_real_geometry() {
        let h = allocate(PathGeometry::default()).expect("allocate");
        with_path(h, |g| {
            g.move_to(10.0, 20.0);
            g.line_to(30.0, 40.0);
            g.quad_to(50.0, 60.0, 70.0, 80.0);
            g.cubic_to(90.0, 100.0, 110.0, 120.0, 130.0, 140.0);
            g.close();
        })
        .expect("with_path");
        let g = get(h).expect("get");
        assert_eq!(
            g.verbs,
            vec![
                Verb::MoveTo,
                Verb::LineTo,
                Verb::QuadTo,
                Verb::CubicTo,
                Verb::Close
            ]
        );
        // 1 + 1 + 2 + 3 points = 7 points = 14 floats; Close consumes none.
        assert_eq!(g.points.len(), 14);
        assert_eq!(&g.points[0..4], &[10.0, 20.0, 30.0, 40.0]);
        assert_eq!(&g.points[4..8], &[50.0, 60.0, 70.0, 80.0]);
        assert_eq!(&g.points[8..14], &[90.0, 100.0, 110.0, 120.0, 130.0, 140.0]);
        free(h).expect("free");
    }

    #[test]
    fn verb_point_counts_match_the_flat_buffer() {
        // The sum of verb point-counts (×2 floats) must equal the buffer length — the invariant the
        // rasterizer relies on to walk verbs against points without overrun.
        let mut g = PathGeometry::default();
        g.move_to(0.0, 0.0);
        g.cubic_to(1.0, 2.0, 3.0, 4.0, 5.0, 6.0);
        g.close();
        g.line_to(7.0, 8.0);
        let consumed: usize = g.verbs.iter().map(|v| v.point_count()).sum();
        assert_eq!(consumed * 2, g.points.len());
    }

    #[test]
    fn bounds_covers_all_points() {
        let mut g = PathGeometry::default();
        g.move_to(10.0, 5.0);
        g.line_to(-3.0, 40.0);
        g.line_to(100.0, -2.0);
        let (min_x, min_y, max_x, max_y) = g.bounds().expect("non-empty path has bounds");
        assert_eq!((min_x, min_y, max_x, max_y), (-3.0, -2.0, 100.0, 40.0));
        assert!(PathGeometry::default().bounds().is_none());
    }

    #[test]
    fn reset_clears_geometry() {
        let mut g = PathGeometry::default();
        g.move_to(1.0, 1.0);
        g.line_to(2.0, 2.0);
        assert!(!g.is_empty());
        g.reset();
        assert!(g.is_empty());
        assert!(g.points.is_empty());
    }

    #[test]
    fn freed_handle_is_stale_and_does_not_alias_reused_slot() {
        let old = allocate(PathGeometry::default()).expect("allocate old");
        with_path(old, |g| g.move_to(1.0, 1.0)).expect("write old");
        free(old).expect("free old");

        let new = allocate(PathGeometry::default()).expect("allocate new");
        assert_eq!(
            get(old),
            Err(PathRegistryError::StaleHandle),
            "a freed handle must be StaleHandle, never alias the reused slot"
        );
        assert!(
            get(new).expect("read new").is_empty(),
            "reused slot is a fresh empty path"
        );
        free(new).expect("free new");
    }

    #[test]
    fn out_of_range_and_fabricated_handles_return_err_not_panic() {
        let fabricated = pack(u32::MAX, 1);
        assert_eq!(
            with_path(fabricated, |_| ()),
            Err(PathRegistryError::OutOfRange),
            "a fabricated out-of-range index must be OutOfRange, never an out-of-bounds deref"
        );
        assert_eq!(get(fabricated), Err(PathRegistryError::OutOfRange));
        assert_eq!(free(fabricated), Err(PathRegistryError::OutOfRange));
        let null_lookup = with_path(0, |_| ());
        assert!(
            matches!(
                null_lookup,
                Err(PathRegistryError::StaleHandle) | Err(PathRegistryError::OutOfRange)
            ),
            "the reserved null handle 0 must be rejected, got {null_lookup:?}"
        );
    }

    #[test]
    fn double_free_is_rejected() {
        let h = allocate(PathGeometry::default()).expect("allocate");
        free(h).expect("first free");
        assert_eq!(free(h), Err(PathRegistryError::StaleHandle));
    }

    #[test]
    fn pack_unpack_round_trips() {
        for &(index, generation) in &[(0u32, 1u32), (1, 1), (5, 42), (u32::MAX, u32::MAX), (3, 7)] {
            let handle = pack(index, generation);
            assert_eq!(unpack(handle), (index, generation));
        }
    }
}
