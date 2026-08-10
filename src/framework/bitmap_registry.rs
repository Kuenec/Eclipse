#![forbid(unsafe_code)]

use std::fmt;
use std::sync::{Mutex, OnceLock, PoisonError};

use jni::sys::jlong;

static BITMAPS: OnceLock<Mutex<Registry>> = OnceLock::new();

pub type BitmapHandle = jlong;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum BitmapRegistryError {
    OutOfRange,

    StaleHandle,

    Poisoned,
}

impl fmt::Display for BitmapRegistryError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::OutOfRange => {
                f.write_str("bitmap handle slot index is out of range (fabricated or null handle)")
            }
            Self::StaleHandle => {
                f.write_str("bitmap handle refers to a freed slot (stale generation)")
            }
            Self::Poisoned => f.write_str("bitmap registry mutex was poisoned"),
        }
    }
}

impl std::error::Error for BitmapRegistryError {}

#[derive(Debug, Default)]
pub struct BitmapState {
    pub width: i32,

    pub height: i32,

    pub bytes: Vec<u8>,
}

struct Slot {
    generation: u32,
    state: Option<BitmapState>,
}

#[derive(Default)]
struct Registry {
    slots: Vec<Slot>,
    free: Vec<u32>,
}

fn pack(index: u32, generation: u32) -> BitmapHandle {
    ((generation as u64) << 32 | index as u64) as i64
}

fn unpack(handle: BitmapHandle) -> (u32, u32) {
    let bits = handle as u64;
    ((bits & 0xFFFF_FFFF) as u32, (bits >> 32) as u32)
}

fn lock() -> Result<std::sync::MutexGuard<'static, Registry>, BitmapRegistryError> {
    BITMAPS
        .get_or_init(|| Mutex::new(Registry::default()))
        .lock()
        .map_err(|_: PoisonError<_>| BitmapRegistryError::Poisoned)
}

pub fn store(state: BitmapState) -> Result<BitmapHandle, BitmapRegistryError> {
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
        .map_err(|_| BitmapRegistryError::OutOfRange)?;
    reg.slots.push(Slot {
        generation: 1,
        state: Some(state),
    });
    Ok(pack(index, 1))
}

pub fn with_bitmap<R>(
    handle: BitmapHandle,
    f: impl FnOnce(&BitmapState) -> R,
) -> Result<R, BitmapRegistryError> {
    let (index, generation) = unpack(handle);
    let reg = lock()?;
    let slot = reg
        .slots
        .get(index as usize)
        .ok_or(BitmapRegistryError::OutOfRange)?;
    if slot.generation != generation {
        return Err(BitmapRegistryError::StaleHandle);
    }
    let state = slot
        .state
        .as_ref()
        .ok_or(BitmapRegistryError::StaleHandle)?;
    Ok(f(state))
}

pub fn free(handle: BitmapHandle) -> Result<(), BitmapRegistryError> {
    let (index, generation) = unpack(handle);
    let mut reg = lock()?;
    let slot = reg
        .slots
        .get_mut(index as usize)
        .ok_or(BitmapRegistryError::OutOfRange)?;
    if slot.generation != generation || slot.state.is_none() {
        return Err(BitmapRegistryError::StaleHandle);
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
    fn store_returns_distinct_nonzero_handles_and_round_trips_dimensions() {
        let a = store(BitmapState {
            width: 48,
            height: 24,
            bytes: vec![1, 2, 3],
        })
        .expect("store a");
        let b = store(BitmapState::default()).expect("store b");
        assert_ne!(a, b, "distinct allocations must yield distinct handles");
        assert_ne!(a, 0, "a valid handle is never the reserved null 0");
        let (w, h, len) = with_bitmap(a, |s| (s.width, s.height, s.bytes.len())).expect("read a");
        assert_eq!((w, h, len), (48, 24, 3), "recorded state round-trips");
        free(a).expect("free a");
        free(b).expect("free b");
    }

    #[test]
    fn freed_handle_is_stale_and_fabricated_handles_are_rejected() {
        let old = store(BitmapState::default()).expect("store old");
        free(old).expect("free old");
        let new = store(BitmapState::default()).expect("store new");
        assert_eq!(
            with_bitmap(old, |_| ()),
            Err(BitmapRegistryError::StaleHandle),
            "a freed handle must be StaleHandle, never alias the reused slot"
        );
        assert_eq!(
            with_bitmap(pack(u32::MAX, 1), |_| ()),
            Err(BitmapRegistryError::OutOfRange)
        );
        assert!(
            with_bitmap(0, |_| ()).is_err(),
            "the reserved null handle 0 must be rejected"
        );
        assert_eq!(free(old), Err(BitmapRegistryError::StaleHandle));
        free(new).expect("free new");
    }
}
