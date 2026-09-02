#![forbid(unsafe_code)]

use std::fmt;
use std::sync::{Mutex, OnceLock, PoisonError};

use jni::sys::jlong;

static PATHS: OnceLock<Mutex<Registry>> = OnceLock::new();

pub type PathHandle = jlong;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum PathRegistryError {
    OutOfRange,

    StaleHandle,

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

#[derive(Debug, Clone, Copy, PartialEq)]
pub enum Verb {
    MoveTo,

    LineTo,

    QuadTo,

    CubicTo,

    Close,
}

impl Verb {
    pub const fn point_count(self) -> usize {
        match self {
            Self::MoveTo | Self::LineTo => 1,
            Self::QuadTo => 2,
            Self::CubicTo => 3,
            Self::Close => 0,
        }
    }
}

#[derive(Debug, Default, Clone, PartialEq)]
pub struct PathGeometry {
    pub verbs: Vec<Verb>,

    pub points: Vec<f32>,
}

impl PathGeometry {
    pub fn move_to(&mut self, x: f32, y: f32) {
        self.verbs.push(Verb::MoveTo);
        self.points.push(x);
        self.points.push(y);
    }

    pub fn line_to(&mut self, x: f32, y: f32) {
        self.verbs.push(Verb::LineTo);
        self.points.push(x);
        self.points.push(y);
    }

    pub fn quad_to(&mut self, cx: f32, cy: f32, x: f32, y: f32) {
        self.verbs.push(Verb::QuadTo);
        self.points.extend_from_slice(&[cx, cy, x, y]);
    }

    #[allow(clippy::too_many_arguments)]
    pub fn cubic_to(&mut self, c1x: f32, c1y: f32, c2x: f32, c2y: f32, x: f32, y: f32) {
        self.verbs.push(Verb::CubicTo);
        self.points.extend_from_slice(&[c1x, c1y, c2x, c2y, x, y]);
    }

    pub fn close(&mut self) {
        self.verbs.push(Verb::Close);
    }

    pub fn reset(&mut self) {
        self.verbs.clear();
        self.points.clear();
    }

    pub fn is_empty(&self) -> bool {
        self.verbs.is_empty()
    }

    pub fn bounds(&self) -> Option<(f32, f32, f32, f32)> {
        let mut it = self.points.as_chunks::<2>().0.iter();
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

struct Slot {
    generation: u32,
    geometry: Option<PathGeometry>,
}

#[derive(Default)]
struct Registry {
    slots: Vec<Slot>,
    free: Vec<u32>,
}

fn pack(index: u32, generation: u32) -> PathHandle {
    ((generation as u64) << 32 | index as u64) as i64
}

fn unpack(handle: PathHandle) -> (u32, u32) {
    let bits = handle as u64;
    ((bits & 0xFFFF_FFFF) as u32, (bits >> 32) as u32)
}

fn lock() -> Result<std::sync::MutexGuard<'static, Registry>, PathRegistryError> {
    PATHS
        .get_or_init(|| Mutex::new(Registry::default()))
        .lock()
        .map_err(|_: PoisonError<_>| PathRegistryError::Poisoned)
}

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

    slot.generation = slot.generation.saturating_add(1);
    reg.free.push(index);
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

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

        assert_eq!(g.points.len(), 14);
        assert_eq!(&g.points[0..4], &[10.0, 20.0, 30.0, 40.0]);
        assert_eq!(&g.points[4..8], &[50.0, 60.0, 70.0, 80.0]);
        assert_eq!(&g.points[8..14], &[90.0, 100.0, 110.0, 120.0, 130.0, 140.0]);
        free(h).expect("free");
    }

    #[test]
    fn verb_point_counts_match_the_flat_buffer() {
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
