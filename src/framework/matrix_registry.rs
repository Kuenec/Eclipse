//! Process-global generational-slab registry for Eclipse-owned `android.graphics.Matrix` handles.
//!
//! 2026-06-05: AOSP's `Matrix.native_create(long src)` creates a native 3x3 matrix object and
//! returns its `long` handle (`Matrix.native_instance`); `native_create(0)` yields the identity,
//! `native_create(srcHandle)` yields a copy of `src`. The `native_set*`/`native_*Concat`/
//! `native_mapPoints` natives consume that handle. ATL backs this in C against Skia/GTK; Eclipse must
//! NOT pull in GTK (AGENTS.md §5 Step 3.5), and a Matrix is **pure float math** (no GPU/raster needed),
//! so a Matrix's `long` handle is an **Eclipse-owned generational-slab index into this slab — NOT
//! `Box::into_raw`, NOT a raw pointer**, exactly the soundness pattern of the sibling registries
//! ([`paint_registry`](super::paint_registry) etc.). A stale/fabricated `jlong` from Java is a
//! bounds+generation-checked `Err`, never a wild dereference / UB.
//!
//! ## Handle layout
//! Identical to the sibling registries: a [`jlong`] packing a `u32` slot index (low 32 bits) + a
//! `u32` generation (high 32 bits). Generations start at 1, so a valid handle is never `0` (which
//! AOSP `Matrix.java` itself uses as the "identity / no native object" sentinel passed to
//! `native_create`).
//!
//! ## The matrix value
//! [`Affine`] is the full AOSP 3x3 matrix stored row-major as 9 `f32`s in the same index order AOSP
//! uses (`Matrix.MSCALE_X=0 … MPERSP_2=8`):
//! ```text
//!   | m[0] m[1] m[2] |   | MSCALE_X MSKEW_X  MTRANS_X |
//!   | m[3] m[4] m[5] | = | MSKEW_Y  MSCALE_Y MTRANS_Y |
//!   | m[6] m[7] m[8] |   | MPERSP_0 MPERSP_1 MPERSP_2 |
//! ```
//! Storing all 9 (not just the 6 affine coefficients) keeps `setConcat`/`mapPoints` exact even when a
//! perspective row is present, matching `android.graphics.Matrix` semantics faithfully. Points are
//! transformed with the full perspective divide (AOSP `SkMatrix::mapPoints`).

#![forbid(unsafe_code)]

use std::fmt;
use std::sync::{Mutex, OnceLock, PoisonError};

use jni::sys::jlong;

/// Process-global slab of [`Affine`], guarded by a [`Mutex`]. Initialized on first use.
static MATRICES: OnceLock<Mutex<Registry>> = OnceLock::new();

/// A matrix-registry handle as it travels across JNI: a `jlong` packing the slot index (low 32 bits)
/// and the slot's generation (high 32 bits). `0` is the reserved "identity / no native object"
/// sentinel (AOSP `Matrix.java` passes `0` to `native_create` for a fresh identity matrix).
pub type MatrixHandle = jlong;

/// Errors from the matrix registry. Every fallible path returns one of these instead of panicking, so
/// a stale/out-of-range/fabricated `jlong` from Java can never cause UB or unwind across JNI.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum MatrixRegistryError {
    /// The handle's slot index is outside the slab (fabricated handle, or the reserved `0`).
    OutOfRange,
    /// The slot exists but its generation does not match: the handle refers to a freed (and possibly
    /// reused) slot. Never aliases the new occupant.
    StaleHandle,
    /// The registry mutex was poisoned by a panic in another holder. Surfaced as an error (not a
    /// re-panic) so the JNI path stays panic-free (AGENTS.md §2.8).
    Poisoned,
}

impl fmt::Display for MatrixRegistryError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::OutOfRange => {
                f.write_str("matrix handle slot index is out of range (fabricated or null handle)")
            }
            Self::StaleHandle => {
                f.write_str("matrix handle refers to a freed slot (stale generation)")
            }
            Self::Poisoned => f.write_str("matrix registry mutex was poisoned"),
        }
    }
}

impl std::error::Error for MatrixRegistryError {}

/// A 3x3 matrix in AOSP `android.graphics.Matrix` index order (row-major, 9 `f32`s).
///
/// 2026-06-05: this is the actual matrix value, not a config record — all the `set*`/`concat`/
/// `mapPoints` operations compute exact 3x3 affine/perspective math on it. Default is the identity.
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct Affine {
    /// The 9 elements in AOSP order: `[MSCALE_X, MSKEW_X, MTRANS_X, MSKEW_Y, MSCALE_Y, MTRANS_Y,
    /// MPERSP_0, MPERSP_1, MPERSP_2]`.
    pub m: [f32; 9],
}

impl Default for Affine {
    fn default() -> Self {
        Self::IDENTITY
    }
}

impl Affine {
    /// The 3x3 identity matrix.
    pub const IDENTITY: Self = Self {
        m: [1.0, 0.0, 0.0, 0.0, 1.0, 0.0, 0.0, 0.0, 1.0],
    };

    /// Reset to the identity matrix (`Matrix.reset()`).
    pub fn reset(&mut self) {
        *self = Self::IDENTITY;
    }

    /// Set this matrix from another (`Matrix.set(src)`); `src == None` resets to identity, matching
    /// AOSP `native_set(dst, 0)`.
    pub fn set_from(&mut self, src: Option<&Affine>) {
        match src {
            Some(s) => self.m = s.m,
            None => self.reset(),
        }
    }

    /// Full 3x3 matrix product `a * b` (rows of `a` against columns of `b`). Used by all the concat
    /// operations; correct for perspective rows, not just affine.
    fn multiply(a: &Affine, b: &Affine) -> Affine {
        let a = &a.m;
        let b = &b.m;
        let mut r = [0.0f32; 9];
        for row in 0..3 {
            for col in 0..3 {
                r[row * 3 + col] =
                    a[row * 3] * b[col] + a[row * 3 + 1] * b[3 + col] + a[row * 3 + 2] * b[6 + col];
            }
        }
        Affine { m: r }
    }

    /// `Matrix.setConcat(a, b)` → `this = a * b`.
    pub fn set_concat(&mut self, a: &Affine, b: &Affine) {
        *self = Self::multiply(a, b);
    }

    /// `Matrix.preConcat(other)` → `this = this * other` (post-multiply on the right).
    pub fn pre_concat(&mut self, other: &Affine) {
        *self = Self::multiply(self, other);
    }

    /// `Matrix.postConcat(other)` → `this = other * this` (pre-multiply on the left).
    pub fn post_concat(&mut self, other: &Affine) {
        *self = Self::multiply(other, self);
    }

    /// `Matrix.setTranslate(dx, dy)`.
    pub fn set_translate(&mut self, dx: f32, dy: f32) {
        self.m = [1.0, 0.0, dx, 0.0, 1.0, dy, 0.0, 0.0, 1.0];
    }

    /// `Matrix.setScale(sx, sy)` (about the origin).
    pub fn set_scale(&mut self, sx: f32, sy: f32) {
        self.m = [sx, 0.0, 0.0, 0.0, sy, 0.0, 0.0, 0.0, 1.0];
    }

    /// `Matrix.setScale(sx, sy, px, py)` (about pivot `(px, py)`): translate(-p) then scale then
    /// translate(+p), pre-composed exactly as AOSP does.
    pub fn set_scale_pivot(&mut self, sx: f32, sy: f32, px: f32, py: f32) {
        // T(p) * S * T(-p), folded: scale, with translation = p - S*p.
        self.m = [sx, 0.0, px - sx * px, 0.0, sy, py - sy * py, 0.0, 0.0, 1.0];
    }

    /// `Matrix.setRotate(degrees)` (about the origin). Angle in degrees, AOSP/Skia convention
    /// (clockwise positive in screen space).
    pub fn set_rotate(&mut self, degrees: f32) {
        let (sin, cos) = degrees.to_radians().sin_cos();
        self.m = [cos, -sin, 0.0, sin, cos, 0.0, 0.0, 0.0, 1.0];
    }

    /// `Matrix.setRotate(degrees, px, py)` (about pivot `(px, py)`).
    pub fn set_rotate_pivot(&mut self, degrees: f32, px: f32, py: f32) {
        let (sin, cos) = degrees.to_radians().sin_cos();
        // T(p) * R * T(-p), folded.
        self.m = [
            cos,
            -sin,
            px - cos * px + sin * py,
            sin,
            cos,
            py - sin * px - cos * py,
            0.0,
            0.0,
            1.0,
        ];
    }

    /// `Matrix.mapPoints` for a single `(x, y)` point with the full perspective divide
    /// (AOSP `SkMatrix::mapPoints`): `[x', y', w] = M·[x, y, 1]`, returning `(x'/w, y'/w)` (or the
    /// unnormalized `(x', y')` when `w == 0`, matching Skia's guard).
    pub fn map_point(&self, x: f32, y: f32) -> (f32, f32) {
        let m = &self.m;
        let nx = m[0] * x + m[1] * y + m[2];
        let ny = m[3] * x + m[4] * y + m[5];
        let w = m[6] * x + m[7] * y + m[8];
        if w != 0.0 {
            (nx / w, ny / w)
        } else {
            (nx, ny)
        }
    }

    /// `true` iff this is exactly the identity matrix (`Matrix.isIdentity()`).
    pub fn is_identity(&self) -> bool {
        self.m == Self::IDENTITY.m
    }
}

/// A generational slot: the current generation plus the optional occupant.
struct Slot {
    generation: u32,
    value: Option<Affine>,
}

/// The slab + free list (same shape as the sibling registries).
#[derive(Default)]
struct Registry {
    slots: Vec<Slot>,
    free: Vec<u32>,
}

/// Pack a slot index + generation into a `jlong` handle (generation high, index low).
fn pack(index: u32, generation: u32) -> MatrixHandle {
    ((generation as u64) << 32 | index as u64) as i64
}

/// Unpack a `jlong` handle into (slot index, generation).
fn unpack(handle: MatrixHandle) -> (u32, u32) {
    let bits = handle as u64;
    ((bits & 0xFFFF_FFFF) as u32, (bits >> 32) as u32)
}

/// Lock the process-global registry, mapping a poisoned mutex to the typed
/// [`MatrixRegistryError::Poisoned`] (never a panic — AGENTS.md §2.8).
fn lock() -> Result<std::sync::MutexGuard<'static, Registry>, MatrixRegistryError> {
    MATRICES
        .get_or_init(|| Mutex::new(Registry::default()))
        .lock()
        .map_err(|_: PoisonError<_>| MatrixRegistryError::Poisoned)
}

/// Allocate a fresh matrix slot initialized to `value` and return its packed [`MatrixHandle`]
/// (`jlong`, generation ≥ 1, never the reserved `0`). Reuses a freed slot when available, else grows
/// the slab. Returns [`MatrixRegistryError::Poisoned`] only on a poisoned mutex — never panics.
pub fn allocate(value: Affine) -> Result<MatrixHandle, MatrixRegistryError> {
    let mut reg = lock()?;
    if let Some(index) = reg.free.pop() {
        let slot = &mut reg.slots[index as usize];
        slot.value = Some(value);
        return Ok(pack(index, slot.generation));
    }
    let index: u32 = reg
        .slots
        .len()
        .try_into()
        .map_err(|_| MatrixRegistryError::OutOfRange)?;
    reg.slots.push(Slot {
        generation: 1,
        value: Some(value),
    });
    Ok(pack(index, 1))
}

/// Read a copy of the [`Affine`] a `handle` refers to. The reserved `0` handle is the identity
/// sentinel and reads back [`Affine::IDENTITY`] (so `native_create(0)` can copy an "identity source"
/// without a real slot). Bounds-checks the slot index **and** verifies the handle's generation, so a
/// stale/out-of-range/fabricated non-zero handle returns `Err` and never dereferences out of bounds.
pub fn get(handle: MatrixHandle) -> Result<Affine, MatrixRegistryError> {
    if handle == 0 {
        return Ok(Affine::IDENTITY);
    }
    let (index, generation) = unpack(handle);
    let reg = lock()?;
    let slot = reg
        .slots
        .get(index as usize)
        .ok_or(MatrixRegistryError::OutOfRange)?;
    if slot.generation != generation {
        return Err(MatrixRegistryError::StaleHandle);
    }
    slot.value.ok_or(MatrixRegistryError::StaleHandle)
}

/// Look up the [`Affine`] for a `handle` and run `f` against it (mutable) under the registry lock.
/// Bounds-checks the slot index **and** verifies the handle's generation, so a stale/out-of-range/
/// fabricated handle returns `Err` and never dereferences out of bounds or aliases a different
/// matrix. The reserved `0` handle fails the check (live generations are ≥ 1) — the identity sentinel
/// has no mutable slot.
pub fn with_matrix<R>(
    handle: MatrixHandle,
    f: impl FnOnce(&mut Affine) -> R,
) -> Result<R, MatrixRegistryError> {
    let (index, generation) = unpack(handle);
    let mut reg = lock()?;
    let slot = reg
        .slots
        .get_mut(index as usize)
        .ok_or(MatrixRegistryError::OutOfRange)?;
    if slot.generation != generation {
        return Err(MatrixRegistryError::StaleHandle);
    }
    let value = slot
        .value
        .as_mut()
        .ok_or(MatrixRegistryError::StaleHandle)?;
    Ok(f(value))
}

/// Free the slot a `handle` refers to, bumping its generation so any other handle to it (or this one,
/// reused later) is rejected as [`MatrixRegistryError::StaleHandle`]. Validates the handle the same
/// way [`with_matrix`] does, so freeing an already-freed/stale/fabricated handle returns `Err`. The
/// reserved `0` identity sentinel has no slot to free and returns [`MatrixRegistryError::OutOfRange`].
pub fn free(handle: MatrixHandle) -> Result<(), MatrixRegistryError> {
    let (index, generation) = unpack(handle);
    let mut reg = lock()?;
    let slot = reg
        .slots
        .get_mut(index as usize)
        .ok_or(MatrixRegistryError::OutOfRange)?;
    if slot.generation != generation || slot.value.is_none() {
        return Err(MatrixRegistryError::StaleHandle);
    }
    slot.value = None;
    // Bump (saturating) so the freed handle and any copy become stale and can never alias a reuse.
    slot.generation = slot.generation.saturating_add(1);
    reg.free.push(index);
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    // 2026-06-05: soundness contract matches the sibling registries + the affine math is exact.
    // Fully in-harness (no VM, GPU-free).

    fn approx(a: f32, b: f32) {
        assert!((a - b).abs() < 1e-4, "expected {b}, got {a}");
    }

    #[test]
    fn allocate_returns_distinct_nonzero_handles() {
        let a = allocate(Affine::IDENTITY).expect("allocate a");
        let b = allocate(Affine::IDENTITY).expect("allocate b");
        assert_ne!(a, b, "distinct allocations must yield distinct handles");
        assert_ne!(a, 0, "a valid handle is never the reserved null 0");
        assert_ne!(b, 0, "a valid handle is never the reserved null 0");
        free(a).expect("free a");
        free(b).expect("free b");
    }

    #[test]
    fn null_handle_reads_identity_sentinel() {
        // native_create(0) copies an "identity source"; get(0) must read identity, not Err.
        assert_eq!(get(0), Ok(Affine::IDENTITY));
        // but it has no mutable slot and cannot be freed. The reserved null `0` unpacks to
        // (index 0, generation 0); whether slot 0 exists (allocated by another test sharing the
        // process-global slab) decides between OutOfRange (no slot) and StaleHandle (gen ≥ 1 ≠ 0).
        // Both are valid rejections — assert it is rejected, not the exact variant (no UB either way).
        assert!(matches!(
            with_matrix(0, |_| ()),
            Err(MatrixRegistryError::OutOfRange) | Err(MatrixRegistryError::StaleHandle)
        ));
        assert!(matches!(
            free(0),
            Err(MatrixRegistryError::OutOfRange) | Err(MatrixRegistryError::StaleHandle)
        ));
    }

    #[test]
    fn with_matrix_mutates_the_right_slot() {
        let a = allocate(Affine::IDENTITY).expect("allocate a");
        let b = allocate(Affine::IDENTITY).expect("allocate b");
        with_matrix(a, |m| m.set_translate(5.0, 7.0)).expect("with a");
        with_matrix(b, |m| m.set_scale(2.0, 3.0)).expect("with b");
        let va = get(a).expect("read a");
        let vb = get(b).expect("read b");
        approx(va.m[2], 5.0);
        approx(va.m[5], 7.0);
        approx(vb.m[0], 2.0);
        approx(vb.m[4], 3.0);
        free(a).expect("free a");
        free(b).expect("free b");
    }

    #[test]
    fn freed_handle_is_stale_and_does_not_alias_reused_slot() {
        let old = allocate(Affine::IDENTITY).expect("allocate old");
        with_matrix(old, |m| m.set_translate(1.0, 1.0)).expect("write old");
        free(old).expect("free old");

        let new = allocate(Affine::IDENTITY).expect("allocate new");
        assert_eq!(
            get(old),
            Err(MatrixRegistryError::StaleHandle),
            "a freed handle must be StaleHandle, never alias the reused slot"
        );
        assert!(
            get(new).expect("read new").is_identity(),
            "reused slot is fresh identity"
        );
        free(new).expect("free new");
    }

    #[test]
    fn out_of_range_and_fabricated_handles_return_err_not_panic() {
        let fabricated = pack(u32::MAX, 1);
        assert_eq!(
            with_matrix(fabricated, |_| ()),
            Err(MatrixRegistryError::OutOfRange),
            "a fabricated out-of-range index must be OutOfRange, never an out-of-bounds deref"
        );
        assert_eq!(get(fabricated), Err(MatrixRegistryError::OutOfRange));
        assert_eq!(free(fabricated), Err(MatrixRegistryError::OutOfRange));
    }

    #[test]
    fn double_free_is_rejected() {
        let h = allocate(Affine::IDENTITY).expect("allocate");
        free(h).expect("first free");
        assert_eq!(free(h), Err(MatrixRegistryError::StaleHandle));
    }

    #[test]
    fn pack_unpack_round_trips() {
        for &(index, generation) in &[(0u32, 1u32), (1, 1), (5, 42), (u32::MAX, u32::MAX), (3, 7)] {
            let handle = pack(index, generation);
            assert_eq!(unpack(handle), (index, generation));
        }
    }

    // --- exact affine math ------------------------------------------------------------------

    #[test]
    fn identity_maps_points_unchanged() {
        let id = Affine::IDENTITY;
        assert!(id.is_identity());
        let (x, y) = id.map_point(3.5, -2.0);
        approx(x, 3.5);
        approx(y, -2.0);
    }

    #[test]
    fn translate_maps_points() {
        let mut m = Affine::IDENTITY;
        m.set_translate(10.0, -4.0);
        let (x, y) = m.map_point(1.0, 1.0);
        approx(x, 11.0);
        approx(y, -3.0);
        assert!(!m.is_identity());
    }

    #[test]
    fn scale_maps_points() {
        let mut m = Affine::IDENTITY;
        m.set_scale(2.0, 3.0);
        let (x, y) = m.map_point(4.0, 5.0);
        approx(x, 8.0);
        approx(y, 15.0);
    }

    #[test]
    fn scale_about_pivot_fixes_the_pivot() {
        let mut m = Affine::IDENTITY;
        m.set_scale_pivot(2.0, 2.0, 100.0, 50.0);
        let (px, py) = m.map_point(100.0, 50.0);
        approx(px, 100.0); // pivot is a fixed point
        approx(py, 50.0);
        let (x, y) = m.map_point(101.0, 51.0);
        approx(x, 102.0);
        approx(y, 52.0);
    }

    #[test]
    fn rotate_90_about_origin() {
        let mut m = Affine::IDENTITY;
        m.set_rotate(90.0);
        // AOSP convention: (1,0) rotates to (0,1).
        let (x, y) = m.map_point(1.0, 0.0);
        approx(x, 0.0);
        approx(y, 1.0);
    }

    #[test]
    fn rotate_about_pivot_fixes_the_pivot() {
        let mut m = Affine::IDENTITY;
        m.set_rotate_pivot(37.0, 8.0, -3.0);
        let (px, py) = m.map_point(8.0, -3.0);
        approx(px, 8.0);
        approx(py, -3.0);
    }

    #[test]
    fn set_concat_is_a_times_b() {
        // a = translate(10,0); b = scale(2,2). setConcat(a,b) applied to a point first scales
        // then translates: p -> a*(b*p).
        let mut a = Affine::IDENTITY;
        a.set_translate(10.0, 0.0);
        let mut b = Affine::IDENTITY;
        b.set_scale(2.0, 2.0);
        let mut c = Affine::IDENTITY;
        c.set_concat(&a, &b);
        let (x, y) = c.map_point(3.0, 4.0);
        approx(x, 16.0); // 3*2 + 10
        approx(y, 8.0); // 4*2
    }

    #[test]
    fn pre_and_post_concat_differ_in_order() {
        let mut t = Affine::IDENTITY;
        t.set_translate(10.0, 0.0);
        let mut s = Affine::IDENTITY;
        s.set_scale(2.0, 2.0);

        // preConcat: this = T * S  → scale-then-translate
        let mut pre = t;
        pre.pre_concat(&s);
        let (px, _) = pre.map_point(3.0, 0.0);
        approx(px, 16.0); // 3*2 + 10

        // postConcat: this = S * T → translate-then-scale
        let mut post = t;
        post.post_concat(&s);
        let (qx, _) = post.map_point(3.0, 0.0);
        approx(qx, 26.0); // (3 + 10) * 2
    }

    #[test]
    fn reset_restores_identity() {
        let mut m = Affine::IDENTITY;
        m.set_scale(5.0, 5.0);
        assert!(!m.is_identity());
        m.reset();
        assert!(m.is_identity());
    }

    #[test]
    fn set_from_copies_or_resets() {
        let mut src = Affine::IDENTITY;
        src.set_translate(7.0, 9.0);
        let mut dst = Affine::IDENTITY;
        dst.set_from(Some(&src));
        approx(dst.m[2], 7.0);
        approx(dst.m[5], 9.0);
        dst.set_from(None);
        assert!(dst.is_identity());
    }
}
