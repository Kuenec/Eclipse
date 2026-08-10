#![forbid(unsafe_code)]

use std::fmt;
use std::sync::atomic::{AtomicI64, Ordering};
use std::sync::{Mutex, OnceLock, PoisonError};

use jni::objects::JObject;
use jni::refs::Global;

static WINDOWS: OnceLock<Mutex<Registry>> = OnceLock::new();

static ACTIVE_WINDOW: AtomicI64 = AtomicI64::new(0);

pub type WindowHandle = i64;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum WindowRegistryError {
    OutOfRange,

    StaleHandle,

    Poisoned,
}

impl fmt::Display for WindowRegistryError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::OutOfRange => {
                f.write_str("window handle slot index is out of range (fabricated or null handle)")
            }
            Self::StaleHandle => {
                f.write_str("window handle refers to a freed slot (stale generation)")
            }
            Self::Poisoned => f.write_str("window registry mutex was poisoned"),
        }
    }
}

impl std::error::Error for WindowRegistryError {}

#[derive(Debug, Default)]
pub struct WindowState {
    pub title: String,

    pub jobject: Option<Global<JObject<'static>>>,

    pub root_view: Option<i64>,
}

struct Slot {
    generation: u32,

    state: Option<WindowState>,
}

#[derive(Default)]
struct Registry {
    slots: Vec<Slot>,
    free: Vec<u32>,
}

fn pack(index: u32, generation: u32) -> WindowHandle {
    ((generation as u64) << 32 | index as u64) as i64
}

fn unpack(handle: WindowHandle) -> (u32, u32) {
    let bits = handle as u64;
    ((bits & 0xFFFF_FFFF) as u32, (bits >> 32) as u32)
}

fn lock() -> Result<std::sync::MutexGuard<'static, Registry>, WindowRegistryError> {
    WINDOWS
        .get_or_init(|| Mutex::new(Registry::default()))
        .lock()
        .map_err(|_: PoisonError<_>| WindowRegistryError::Poisoned)
}

pub fn allocate() -> Result<WindowHandle, WindowRegistryError> {
    let mut reg = lock()?;
    let handle = if let Some(index) = reg.free.pop() {
        let slot = &mut reg.slots[index as usize];
        slot.state = Some(WindowState::default());
        pack(index, slot.generation)
    } else {
        let index: u32 = reg
            .slots
            .len()
            .try_into()
            .map_err(|_| WindowRegistryError::OutOfRange)?;
        reg.slots.push(Slot {
            generation: 1,
            state: Some(WindowState::default()),
        });
        pack(index, 1)
    };

    ACTIVE_WINDOW.store(handle, Ordering::Release);
    Ok(handle)
}

pub fn with_window<R>(
    handle: WindowHandle,
    f: impl FnOnce(&mut WindowState) -> R,
) -> Result<R, WindowRegistryError> {
    let (index, generation) = unpack(handle);
    let mut reg = lock()?;
    let slot = reg
        .slots
        .get_mut(index as usize)
        .ok_or(WindowRegistryError::OutOfRange)?;
    if slot.generation != generation {
        return Err(WindowRegistryError::StaleHandle);
    }
    let state = slot
        .state
        .as_mut()
        .ok_or(WindowRegistryError::StaleHandle)?;
    Ok(f(state))
}

pub fn free(handle: WindowHandle) -> Result<(), WindowRegistryError> {
    let (index, generation) = unpack(handle);
    let mut reg = lock()?;
    let slot = reg
        .slots
        .get_mut(index as usize)
        .ok_or(WindowRegistryError::OutOfRange)?;
    if slot.generation != generation || slot.state.is_none() {
        return Err(WindowRegistryError::StaleHandle);
    }
    slot.state = None;

    slot.generation = slot.generation.saturating_add(1);
    reg.free.push(index);

    let _ = ACTIVE_WINDOW.compare_exchange(handle, 0, Ordering::AcqRel, Ordering::Acquire);
    Ok(())
}

pub fn set_jobject(
    handle: WindowHandle,
    jobject: Global<JObject<'static>>,
) -> Result<(), WindowRegistryError> {
    with_window(handle, move |w| w.jobject = Some(jobject))
}

pub fn with_jobject<R>(
    handle: WindowHandle,
    f: impl FnOnce(&Global<JObject<'static>>) -> R,
) -> Result<Option<R>, WindowRegistryError> {
    with_window(handle, |w| w.jobject.as_ref().map(f))
}

pub fn active_window() -> WindowHandle {
    ACTIVE_WINDOW.load(Ordering::Acquire)
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::Mutex;

    static TEST_LOCK: Mutex<()> = Mutex::new(());

    #[test]
    fn pack_unpack_round_trips() {
        for &(index, generation) in &[(0u32, 1u32), (1, 1), (5, 42), (u32::MAX, u32::MAX), (3, 7)] {
            let handle = pack(index, generation);
            assert_eq!(unpack(handle), (index, generation));
        }
    }

    #[test]
    fn allocate_returns_distinct_nonzero_handles() {
        let _guard = TEST_LOCK.lock().unwrap_or_else(|e| e.into_inner());
        let a = allocate().expect("allocate a");
        let b = allocate().expect("allocate b");
        assert_ne!(a, b, "distinct allocations must yield distinct handles");

        assert_ne!(a, 0, "a valid handle is never the reserved null 0");
        assert_ne!(b, 0, "a valid handle is never the reserved null 0");
        free(a).expect("free a");
        free(b).expect("free b");
    }

    #[test]
    fn with_window_mutates_the_right_slot() {
        let _guard = TEST_LOCK.lock().unwrap_or_else(|e| e.into_inner());
        let a = allocate().expect("allocate a");
        let b = allocate().expect("allocate b");
        with_window(a, |s| s.title = "window-a".to_owned()).expect("with_window a");
        with_window(b, |s| s.title = "window-b".to_owned()).expect("with_window b");
        let ta = with_window(a, |s| s.title.clone()).expect("read a");
        let tb = with_window(b, |s| s.title.clone()).expect("read b");
        assert_eq!(ta, "window-a", "handle a must address its own slot");
        assert_eq!(tb, "window-b", "handle b must address its own slot");
        free(a).expect("free a");
        free(b).expect("free b");
    }

    #[test]
    fn freed_handle_is_stale_and_does_not_alias_reused_slot() {
        let _guard = TEST_LOCK.lock().unwrap_or_else(|e| e.into_inner());

        let old = allocate().expect("allocate old");
        with_window(old, |s| s.title = "old".to_owned()).expect("write old");
        free(old).expect("free old");

        let new = allocate().expect("allocate new");
        with_window(new, |s| s.title = "new".to_owned()).expect("write new");

        assert_eq!(
            with_window(old, |s| s.title.clone()),
            Err(WindowRegistryError::StaleHandle),
            "a freed handle must be StaleHandle, never alias the reused slot"
        );

        assert_eq!(
            with_window(new, |s| s.title.clone()),
            Ok("new".to_owned()),
            "the live handle must still address the reused slot"
        );
        free(new).expect("free new");
    }

    #[test]
    fn out_of_range_and_fabricated_handles_return_err_not_panic() {
        let fabricated = pack(u32::MAX, 1);
        assert_eq!(
            with_window(fabricated, |_| ()),
            Err(WindowRegistryError::OutOfRange),
            "a fabricated out-of-range index must be OutOfRange, never an out-of-bounds deref"
        );

        let null_lookup = with_window(0, |_| ());
        assert!(
            matches!(
                null_lookup,
                Err(WindowRegistryError::StaleHandle) | Err(WindowRegistryError::OutOfRange)
            ),
            "the reserved null handle 0 must be rejected, got {null_lookup:?}"
        );

        assert_eq!(free(fabricated), Err(WindowRegistryError::OutOfRange));
    }

    #[test]
    fn double_free_is_rejected() {
        let _guard = TEST_LOCK.lock().unwrap_or_else(|e| e.into_inner());
        let h = allocate().expect("allocate");
        free(h).expect("first free");

        assert_eq!(free(h), Err(WindowRegistryError::StaleHandle));
    }

    #[test]
    fn with_jobject_is_none_without_a_captured_object_and_err_when_stale() {
        let _guard = TEST_LOCK.lock().unwrap_or_else(|e| e.into_inner());
        let h = allocate().expect("alloc");

        assert_eq!(with_jobject(h, |_| 1i32), Ok(None));
        free(h).expect("free");

        assert_eq!(
            with_jobject(h, |_| 1i32),
            Err(WindowRegistryError::StaleHandle)
        );
    }

    #[test]
    fn active_window_tracks_allocate_and_clears_on_free() {
        let _guard = TEST_LOCK.lock().unwrap_or_else(|e| e.into_inner());
        let h = allocate().expect("alloc");

        assert_eq!(
            active_window(),
            h,
            "allocate must publish the active window"
        );
        free(h).expect("free");

        assert_eq!(active_window(), 0, "free must clear the active window");
    }

    #[test]
    fn freeing_a_superseded_window_does_not_clear_a_newer_active() {
        let _guard = TEST_LOCK.lock().unwrap_or_else(|e| e.into_inner());

        let old = allocate().expect("alloc old");
        let new = allocate().expect("alloc new");
        assert_eq!(
            active_window(),
            new,
            "the newer allocate is the active window"
        );
        free(old).expect("free old");
        assert_eq!(
            active_window(),
            new,
            "freeing a superseded window must not clear the newer active window"
        );
        free(new).expect("free new");
        assert_eq!(active_window(), 0, "freeing the live window clears it");
    }
}
