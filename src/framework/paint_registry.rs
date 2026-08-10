#![forbid(unsafe_code)]

use std::fmt;
use std::sync::{Mutex, OnceLock, PoisonError};

use jni::sys::jlong;

static PAINTS: OnceLock<Mutex<Registry>> = OnceLock::new();

pub type PaintHandle = jlong;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum PaintRegistryError {
    OutOfRange,

    StaleHandle,

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

#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum PaintStyle {
    #[default]
    Fill,

    Stroke,

    FillAndStroke,
}

impl PaintStyle {
    pub fn from_ordinal(ordinal: i32) -> Self {
        match ordinal {
            1 => Self::Stroke,
            2 => Self::FillAndStroke,
            _ => Self::Fill,
        }
    }

    pub fn ordinal(self) -> i32 {
        match self {
            Self::Fill => 0,
            Self::Stroke => 1,
            Self::FillAndStroke => 2,
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum StrokeCap {
    #[default]
    Butt,

    Round,

    Square,
}

impl StrokeCap {
    pub fn from_ordinal(ordinal: i32) -> Self {
        match ordinal {
            1 => Self::Round,
            2 => Self::Square,
            _ => Self::Butt,
        }
    }

    pub fn ordinal(self) -> i32 {
        match self {
            Self::Butt => 0,
            Self::Round => 1,
            Self::Square => 2,
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum StrokeJoin {
    #[default]
    Miter,

    Round,

    Bevel,
}

impl StrokeJoin {
    pub fn from_ordinal(ordinal: i32) -> Self {
        match ordinal {
            1 => Self::Round,
            2 => Self::Bevel,
            _ => Self::Miter,
        }
    }

    pub fn ordinal(self) -> i32 {
        match self {
            Self::Miter => 0,
            Self::Round => 1,
            Self::Bevel => 2,
        }
    }
}

#[derive(Debug, Clone)]
pub struct PaintState {
    pub color: i32,

    pub text_size: f32,

    pub stroke_width: f32,

    pub style: PaintStyle,

    pub stroke_cap: StrokeCap,

    pub stroke_join: StrokeJoin,
}

impl Default for PaintState {
    fn default() -> Self {
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

struct Slot {
    generation: u32,
    state: Option<PaintState>,
}

#[derive(Default)]
struct Registry {
    slots: Vec<Slot>,
    free: Vec<u32>,
}

fn pack(index: u32, generation: u32) -> PaintHandle {
    ((generation as u64) << 32 | index as u64) as i64
}

fn unpack(handle: PaintHandle) -> (u32, u32) {
    let bits = handle as u64;
    ((bits & 0xFFFF_FFFF) as u32, (bits >> 32) as u32)
}

fn lock() -> Result<std::sync::MutexGuard<'static, Registry>, PaintRegistryError> {
    PAINTS
        .get_or_init(|| Mutex::new(Registry::default()))
        .lock()
        .map_err(|_: PoisonError<_>| PaintRegistryError::Poisoned)
}

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

pub fn allocate() -> Result<PaintHandle, PaintRegistryError> {
    let mut reg = lock()?;
    allocate_state(&mut reg, PaintState::default())
}

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

    slot.generation = slot.generation.saturating_add(1);
    reg.free.push(index);
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

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
        assert_eq!(PaintStyle::from_ordinal(0), PaintStyle::Fill);
        assert_eq!(PaintStyle::from_ordinal(1), PaintStyle::Stroke);
        assert_eq!(PaintStyle::from_ordinal(2), PaintStyle::FillAndStroke);
        assert_eq!(PaintStyle::from_ordinal(99), PaintStyle::Fill);
        assert_eq!(PaintStyle::from_ordinal(-1), PaintStyle::Fill);
        assert_eq!(PaintStyle::default(), PaintStyle::Fill);
    }

    #[test]
    fn style_cap_join_ordinals_round_trip_and_stay_in_java_values_range() {
        for ordinal in 0..=2 {
            assert_eq!(PaintStyle::from_ordinal(ordinal).ordinal(), ordinal);
            assert_eq!(StrokeCap::from_ordinal(ordinal).ordinal(), ordinal);
            assert_eq!(StrokeJoin::from_ordinal(ordinal).ordinal(), ordinal);
        }

        assert_eq!(StrokeCap::from_ordinal(0), StrokeCap::Butt);
        assert_eq!(StrokeCap::from_ordinal(1), StrokeCap::Round);
        assert_eq!(StrokeCap::from_ordinal(2), StrokeCap::Square);
        assert_eq!(StrokeJoin::from_ordinal(0), StrokeJoin::Miter);
        assert_eq!(StrokeJoin::from_ordinal(1), StrokeJoin::Round);
        assert_eq!(StrokeJoin::from_ordinal(2), StrokeJoin::Bevel);

        assert_eq!(StrokeCap::from_ordinal(99).ordinal(), 0);
        assert_eq!(StrokeCap::from_ordinal(-1).ordinal(), 0);
        assert_eq!(StrokeJoin::from_ordinal(99).ordinal(), 0);
        assert_eq!(StrokeJoin::from_ordinal(-1).ordinal(), 0);
    }

    #[test]
    fn fresh_paint_defaults_are_aosp_reference_values() {
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
