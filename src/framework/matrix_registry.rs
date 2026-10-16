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

impl Affine {
    pub const IDENTITY: Self = Self {
        m: [1.0, 0.0, 0.0, 0.0, 1.0, 0.0, 0.0, 0.0, 1.0],
    };
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
            free(0),
            Err(MatrixRegistryError::OutOfRange) | Err(MatrixRegistryError::StaleHandle)
        ));
    }

    #[test]
    fn freed_handle_is_stale_and_does_not_alias_reused_slot() {
        let old = allocate(Affine {
            m: [2.0, 0.0, 5.0, 0.0, 3.0, 7.0, 0.0, 0.0, 1.0],
        })
        .expect("allocate old");
        free(old).expect("free old");

        let new = allocate(Affine::IDENTITY).expect("allocate new");
        assert_eq!(
            get(old),
            Err(MatrixRegistryError::StaleHandle),
            "a freed handle must be StaleHandle, never alias the reused slot"
        );
        assert_eq!(
            get(new),
            Ok(Affine::IDENTITY),
            "reused slot is fresh identity"
        );
        free(new).expect("free new");
    }

    #[test]
    fn out_of_range_and_fabricated_handles_return_err_not_panic() {
        let fabricated = pack(u32::MAX, 1);
        assert_eq!(
            get(fabricated),
            Err(MatrixRegistryError::OutOfRange),
            "a fabricated out-of-range index must be OutOfRange, never an out-of-bounds deref"
        );
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
}
