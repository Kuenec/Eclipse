#![forbid(unsafe_code)]

use std::collections::HashMap;
use std::fmt;
use std::sync::{Mutex, OnceLock, PoisonError};

use jni::sys::jlong;

static THEMES: OnceLock<Mutex<Registry>> = OnceLock::new();

pub type ThemeHandle = jlong;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ThemeRegistryError {
    OutOfRange,

    StaleHandle,

    Poisoned,
}

impl fmt::Display for ThemeRegistryError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::OutOfRange => {
                f.write_str("theme handle slot index is out of range (fabricated or null handle)")
            }
            Self::StaleHandle => {
                f.write_str("theme handle refers to a freed slot (stale generation)")
            }
            Self::Poisoned => f.write_str("theme registry mutex was poisoned"),
        }
    }
}

impl std::error::Error for ThemeRegistryError {}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct ThemeAttr {
    pub type_: u8,

    pub data: u32,

    pub source_package: u8,
}

#[derive(Debug, Default)]
pub struct ThemeState {
    pub styles: Vec<i32>,

    pub attrs: HashMap<i32, ThemeAttr>,
}

struct Slot {
    generation: u32,
    state: Option<ThemeState>,
}

#[derive(Default)]
struct Registry {
    slots: Vec<Slot>,
    free: Vec<u32>,
}

fn pack(index: u32, generation: u32) -> ThemeHandle {
    ((generation as u64) << 32 | index as u64) as i64
}

fn unpack(handle: ThemeHandle) -> (u32, u32) {
    let bits = handle as u64;
    ((bits & 0xFFFF_FFFF) as u32, (bits >> 32) as u32)
}

fn lock() -> Result<std::sync::MutexGuard<'static, Registry>, ThemeRegistryError> {
    THEMES
        .get_or_init(|| Mutex::new(Registry::default()))
        .lock()
        .map_err(|_: PoisonError<_>| ThemeRegistryError::Poisoned)
}

pub fn allocate() -> Result<ThemeHandle, ThemeRegistryError> {
    let mut reg = lock()?;
    if let Some(index) = reg.free.pop() {
        let slot = &mut reg.slots[index as usize];
        slot.state = Some(ThemeState::default());
        return Ok(pack(index, slot.generation));
    }
    let index: u32 = reg
        .slots
        .len()
        .try_into()
        .map_err(|_| ThemeRegistryError::OutOfRange)?;
    reg.slots.push(Slot {
        generation: 1,
        state: Some(ThemeState::default()),
    });
    Ok(pack(index, 1))
}

pub fn with_theme<R>(
    handle: ThemeHandle,
    f: impl FnOnce(&mut ThemeState) -> R,
) -> Result<R, ThemeRegistryError> {
    let (index, generation) = unpack(handle);
    let mut reg = lock()?;
    let slot = reg
        .slots
        .get_mut(index as usize)
        .ok_or(ThemeRegistryError::OutOfRange)?;
    if slot.generation != generation {
        return Err(ThemeRegistryError::StaleHandle);
    }
    let state = slot.state.as_mut().ok_or(ThemeRegistryError::StaleHandle)?;
    Ok(f(state))
}

pub fn free(handle: ThemeHandle) -> Result<(), ThemeRegistryError> {
    let (index, generation) = unpack(handle);
    let mut reg = lock()?;
    let slot = reg
        .slots
        .get_mut(index as usize)
        .ok_or(ThemeRegistryError::OutOfRange)?;
    if slot.generation != generation || slot.state.is_none() {
        return Err(ThemeRegistryError::StaleHandle);
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
    fn with_theme_mutates_the_right_slot() {
        let a = allocate().expect("allocate a");
        let b = allocate().expect("allocate b");
        with_theme(a, |s| s.styles.push(0x0101)).expect("with_theme a");
        let sa = with_theme(a, |s| s.styles.clone()).expect("read a");
        let sb = with_theme(b, |s| s.styles.clone()).expect("read b");
        assert_eq!(sa, vec![0x0101], "handle a addresses its own slot");
        assert!(sb.is_empty(), "handle b is unaffected by a's mutation");
        free(a).expect("free a");
        free(b).expect("free b");
    }

    #[test]
    fn freed_handle_is_stale_and_does_not_alias_reused_slot() {
        let old = allocate().expect("allocate old");
        with_theme(old, |s| s.styles.push(7)).expect("write old");
        free(old).expect("free old");

        let new = allocate().expect("allocate new");
        assert_eq!(
            with_theme(old, |s| s.styles.clone()),
            Err(ThemeRegistryError::StaleHandle),
            "a freed handle must be StaleHandle, never alias the reused slot"
        );
        assert_eq!(
            with_theme(new, |s| s.styles.clone()),
            Ok(Vec::new()),
            "the live handle's reused slot is fresh (empty styles)"
        );
        free(new).expect("free new");
    }

    #[test]
    fn out_of_range_and_fabricated_handles_return_err_not_panic() {
        let fabricated = pack(u32::MAX, 1);
        assert_eq!(
            with_theme(fabricated, |_| ()),
            Err(ThemeRegistryError::OutOfRange),
            "a fabricated out-of-range index must be OutOfRange, never an out-of-bounds deref"
        );
        let null_lookup = with_theme(0, |_| ());
        assert!(
            matches!(
                null_lookup,
                Err(ThemeRegistryError::StaleHandle) | Err(ThemeRegistryError::OutOfRange)
            ),
            "the reserved null handle 0 must be rejected, got {null_lookup:?}"
        );
        assert_eq!(free(fabricated), Err(ThemeRegistryError::OutOfRange));
    }

    #[test]
    fn double_free_is_rejected() {
        let h = allocate().expect("allocate");
        free(h).expect("first free");
        assert_eq!(free(h), Err(ThemeRegistryError::StaleHandle));
    }

    #[test]
    fn attrs_map_round_trips_and_copies_independently() {
        let src = allocate().expect("allocate src");
        with_theme(src, |t| {
            t.attrs.insert(
                0x7f01_0058,
                ThemeAttr {
                    type_: 0x12,
                    data: 0xffff_ffff,
                    source_package: 0x7f,
                },
            );
        })
        .expect("populate src");

        let got = with_theme(src, |t| t.attrs.get(&0x7f01_0058).copied())
            .expect("read src")
            .expect("attr present");
        assert_eq!(
            got,
            ThemeAttr {
                type_: 0x12,
                data: 0xffff_ffff,
                source_package: 0x7f,
            }
        );

        let dest = allocate().expect("allocate dest");
        let snapshot = with_theme(src, |t| t.attrs.clone()).expect("clone src attrs");
        with_theme(dest, |t| t.attrs = snapshot).expect("write dest attrs");
        with_theme(src, |t| {
            t.attrs.insert(
                0x7f01_0058,
                ThemeAttr {
                    type_: 0x10,
                    data: 0,
                    source_package: 0x7f,
                },
            );
        })
        .expect("mutate src");
        let dest_val = with_theme(dest, |t| t.attrs.get(&0x7f01_0058).copied())
            .expect("read dest")
            .expect("dest attr present");
        assert_eq!(
            dest_val,
            ThemeAttr {
                type_: 0x12,
                data: 0xffff_ffff,
                source_package: 0x7f,
            },
            "the copied map is independent of later src mutation"
        );

        free(src).expect("free src");
        free(dest).expect("free dest");
    }

    #[test]
    fn pack_unpack_round_trips() {
        for &(index, generation) in &[(0u32, 1u32), (1, 1), (5, 42), (u32::MAX, u32::MAX), (3, 7)] {
            let handle = pack(index, generation);
            assert_eq!(unpack(handle), (index, generation));
        }
    }
}
