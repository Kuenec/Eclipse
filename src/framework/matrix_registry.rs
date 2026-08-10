#![forbid(unsafe_code)]

use std::fmt;
use std::sync::{Mutex, OnceLock, PoisonError};

use jni::sys::jlong;

static MATRICES: OnceLock<Mutex<Registry>> = OnceLock::new();

pub type MatrixHandle = jlong;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum MatrixRegistryError {
    OutOfRange,

    StaleHandle,

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

#[derive(Debug, Clone, Copy, PartialEq)]
pub struct Affine {
    pub m: [f32; 9],
}

impl Default for Affine {
    fn default() -> Self {
        Self::IDENTITY
    }
}

impl Affine {
    pub const IDENTITY: Self = Self {
        m: [1.0, 0.0, 0.0, 0.0, 1.0, 0.0, 0.0, 0.0, 1.0],
    };

    pub fn reset(&mut self) {
        *self = Self::IDENTITY;
    }

    pub fn set_from(&mut self, src: Option<&Affine>) {
        match src {
            Some(s) => self.m = s.m,
            None => self.reset(),
        }
    }

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

    pub fn set_concat(&mut self, a: &Affine, b: &Affine) {
        *self = Self::multiply(a, b);
    }

    pub fn pre_concat(&mut self, other: &Affine) {
        *self = Self::multiply(self, other);
    }

    pub fn post_concat(&mut self, other: &Affine) {
        *self = Self::multiply(other, self);
    }

    pub fn set_translate(&mut self, dx: f32, dy: f32) {
        self.m = [1.0, 0.0, dx, 0.0, 1.0, dy, 0.0, 0.0, 1.0];
    }

    pub fn set_scale(&mut self, sx: f32, sy: f32) {
        self.m = [sx, 0.0, 0.0, 0.0, sy, 0.0, 0.0, 0.0, 1.0];
    }

    pub fn set_scale_pivot(&mut self, sx: f32, sy: f32, px: f32, py: f32) {
        self.m = [sx, 0.0, px - sx * px, 0.0, sy, py - sy * py, 0.0, 0.0, 1.0];
    }

    pub fn set_rotate(&mut self, degrees: f32) {
        let (sin, cos) = degrees.to_radians().sin_cos();
        self.m = [cos, -sin, 0.0, sin, cos, 0.0, 0.0, 0.0, 1.0];
    }

    pub fn set_rotate_pivot(&mut self, degrees: f32, px: f32, py: f32) {
        let (sin, cos) = degrees.to_radians().sin_cos();

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

    pub fn is_identity(&self) -> bool {
        self.m == Self::IDENTITY.m
    }
}

struct Slot {
    generation: u32,
    value: Option<Affine>,
}

#[derive(Default)]
struct Registry {
    slots: Vec<Slot>,
    free: Vec<u32>,
}

fn pack(index: u32, generation: u32) -> MatrixHandle {
    ((generation as u64) << 32 | index as u64) as i64
}

fn unpack(handle: MatrixHandle) -> (u32, u32) {
    let bits = handle as u64;
    ((bits & 0xFFFF_FFFF) as u32, (bits >> 32) as u32)
}

fn lock() -> Result<std::sync::MutexGuard<'static, Registry>, MatrixRegistryError> {
    MATRICES
        .get_or_init(|| Mutex::new(Registry::default()))
        .lock()
        .map_err(|_: PoisonError<_>| MatrixRegistryError::Poisoned)
}

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

    slot.generation = slot.generation.saturating_add(1);
    reg.free.push(index);
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

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
        assert_eq!(get(0), Ok(Affine::IDENTITY));

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
        approx(px, 100.0);
        approx(py, 50.0);
        let (x, y) = m.map_point(101.0, 51.0);
        approx(x, 102.0);
        approx(y, 52.0);
    }

    #[test]
    fn rotate_90_about_origin() {
        let mut m = Affine::IDENTITY;
        m.set_rotate(90.0);

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
        let mut a = Affine::IDENTITY;
        a.set_translate(10.0, 0.0);
        let mut b = Affine::IDENTITY;
        b.set_scale(2.0, 2.0);
        let mut c = Affine::IDENTITY;
        c.set_concat(&a, &b);
        let (x, y) = c.map_point(3.0, 4.0);
        approx(x, 16.0);
        approx(y, 8.0);
    }

    #[test]
    fn pre_and_post_concat_differ_in_order() {
        let mut t = Affine::IDENTITY;
        t.set_translate(10.0, 0.0);
        let mut s = Affine::IDENTITY;
        s.set_scale(2.0, 2.0);

        let mut pre = t;
        pre.pre_concat(&s);
        let (px, _) = pre.map_point(3.0, 0.0);
        approx(px, 16.0);

        let mut post = t;
        post.post_concat(&s);
        let (qx, _) = post.map_point(3.0, 0.0);
        approx(qx, 26.0);
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
