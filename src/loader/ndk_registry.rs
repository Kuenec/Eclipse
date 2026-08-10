#![forbid(unsafe_code)]

use std::fmt;
use std::path::PathBuf;
use std::sync::atomic::{AtomicI32, AtomicU64, AtomicU8, AtomicUsize, Ordering};
use std::sync::{Mutex, OnceLock, PoisonError};

pub type NdkHandle = u64;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum NdkRegistryError {
    OutOfRange,

    StaleHandle,

    Poisoned,
}

impl fmt::Display for NdkRegistryError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::OutOfRange => {
                f.write_str("NDK handle slot index is out of range (fabricated or NULL handle)")
            }
            Self::StaleHandle => {
                f.write_str("NDK handle refers to a freed slot (stale generation)")
            }
            Self::Poisoned => f.write_str("NDK registry mutex was poisoned"),
        }
    }
}

impl std::error::Error for NdkRegistryError {}

struct Slot<T> {
    generation: u32,
    state: Option<T>,
}

pub struct Slab<T> {
    inner: OnceLock<Mutex<Registry<T>>>,
}

struct Registry<T> {
    slots: Vec<Slot<T>>,
    free: Vec<u32>,
}

impl<T> Default for Registry<T> {
    fn default() -> Self {
        Self {
            slots: Vec::new(),
            free: Vec::new(),
        }
    }
}

fn pack(index: u32, generation: u32) -> NdkHandle {
    (generation as u64) << 32 | index as u64
}

fn unpack(handle: NdkHandle) -> (u32, u32) {
    ((handle & 0xFFFF_FFFF) as u32, (handle >> 32) as u32)
}

impl<T> Default for Slab<T> {
    fn default() -> Self {
        Self::new()
    }
}

impl<T> Slab<T> {
    pub const fn new() -> Self {
        Self {
            inner: OnceLock::new(),
        }
    }

    fn lock(&self) -> Result<std::sync::MutexGuard<'_, Registry<T>>, NdkRegistryError> {
        self.inner
            .get_or_init(|| Mutex::new(Registry::default()))
            .lock()
            .map_err(|_: PoisonError<_>| NdkRegistryError::Poisoned)
    }

    pub fn insert(&self, state: T) -> Result<NdkHandle, NdkRegistryError> {
        let mut reg = self.lock()?;
        if let Some(index) = reg.free.pop() {
            let slot = &mut reg.slots[index as usize];
            slot.state = Some(state);
            return Ok(pack(index, slot.generation));
        }
        let index: u32 = reg
            .slots
            .len()
            .try_into()
            .map_err(|_| NdkRegistryError::OutOfRange)?;
        reg.slots.push(Slot {
            generation: 1,
            state: Some(state),
        });
        Ok(pack(index, 1))
    }

    pub fn with<R>(
        &self,
        handle: NdkHandle,
        f: impl FnOnce(&mut T) -> R,
    ) -> Result<R, NdkRegistryError> {
        let (index, generation) = unpack(handle);
        let mut reg = self.lock()?;
        let slot = reg
            .slots
            .get_mut(index as usize)
            .ok_or(NdkRegistryError::OutOfRange)?;
        if slot.generation != generation {
            return Err(NdkRegistryError::StaleHandle);
        }
        let state = slot.state.as_mut().ok_or(NdkRegistryError::StaleHandle)?;
        Ok(f(state))
    }

    pub fn remove(&self, handle: NdkHandle) -> Result<(), NdkRegistryError> {
        let (index, generation) = unpack(handle);
        let mut reg = self.lock()?;
        let slot = reg
            .slots
            .get_mut(index as usize)
            .ok_or(NdkRegistryError::OutOfRange)?;
        if slot.generation != generation || slot.state.is_none() {
            return Err(NdkRegistryError::StaleHandle);
        }
        slot.state = None;

        slot.generation = slot.generation.saturating_add(1);
        reg.free.push(index);
        Ok(())
    }
}

#[derive(Debug, Clone)]
pub struct AssetManagerState {
    pub apk_path: PathBuf,
}

#[derive(Debug)]
pub struct AssetState {
    pub bytes: Box<[u8]>,

    pub cursor: usize,
}

#[derive(Debug, Clone, Copy)]
pub struct ConfigurationState {
    pub density: i32,

    pub screen_width_dp: i32,

    pub screen_height_dp: i32,

    pub screen_size: i32,

    pub orientation: i32,

    pub nav_hidden: i32,

    pub language: [u8; 2],

    pub country: [u8; 2],
}

#[derive(Debug)]
pub struct LooperState {
    pub looper: super::looper::Looper,
}

#[derive(Debug, Clone, Copy)]
pub struct NativeWindowState {
    pub width: i32,

    pub height: i32,

    pub format: i32,
}

static APK_PATH: OnceLock<PathBuf> = OnceLock::new();

pub fn set_apk_path(path: PathBuf) -> bool {
    APK_PATH.set(path).is_ok()
}

pub fn apk_path() -> Option<&'static PathBuf> {
    APK_PATH.get()
}

static ENGINE_WINDOW_GEOMETRY: AtomicU64 = AtomicU64::new(0);

fn pack_geometry(width: i32, height: i32) -> u64 {
    (u64::from(width.max(1) as u32) << 32) | u64::from(height.max(1) as u32)
}

fn unpack_geometry(value: u64) -> Option<(i32, i32)> {
    if value == 0 {
        return None;
    }
    Some(((value >> 32) as u32 as i32, value as u32 as i32))
}

pub fn set_engine_window_geometry(width: i32, height: i32) {
    let geometry = pack_geometry(width, height);
    ENGINE_WINDOW_GEOMETRY.store(geometry, Ordering::Release);
    if FALLBACK_NATIVE_WINDOW_GEOMETRY.load(Ordering::Acquire) != 0 {
        FALLBACK_NATIVE_WINDOW_GEOMETRY.store(geometry, Ordering::Release);
    }
}

pub fn engine_window_geometry() -> Option<(i32, i32)> {
    unpack_geometry(ENGINE_WINDOW_GEOMETRY.load(Ordering::Acquire))
}

static WSI_WINDOW: AtomicUsize = AtomicUsize::new(0);
static WSI_WINDOW_GEOMETRY: AtomicU64 = AtomicU64::new(0);

pub fn register_wsi_window(native_window: usize, width: i32, height: i32) {
    if native_window == 0 {
        return;
    }
    WSI_WINDOW_GEOMETRY.store(pack_geometry(width, height), Ordering::Release);
    WSI_WINDOW.store(native_window, Ordering::Release);
}

pub fn unregister_wsi_window(native_window: usize) {
    let _ = WSI_WINDOW.compare_exchange(native_window, 0, Ordering::AcqRel, Ordering::Acquire);
}

pub fn wsi_window_geometry(native_window: usize) -> Option<(i32, i32)> {
    if native_window == 0 || WSI_WINDOW.load(Ordering::Acquire) != native_window {
        return None;
    }
    unpack_geometry(WSI_WINDOW_GEOMETRY.load(Ordering::Acquire))
}

pub fn current_wsi_window() -> Option<usize> {
    match WSI_WINDOW.load(Ordering::Acquire) {
        0 => None,
        native_window => Some(native_window),
    }
}

static FALLBACK_NATIVE_WINDOW_TOKEN: AtomicU8 = AtomicU8::new(0);
static FALLBACK_NATIVE_WINDOW_GEOMETRY: AtomicU64 = AtomicU64::new(0);
static FALLBACK_NATIVE_WINDOW_FORMAT: AtomicI32 = AtomicI32::new(0);

pub fn register_fallback_native_window(state: NativeWindowState) -> usize {
    FALLBACK_NATIVE_WINDOW_GEOMETRY
        .store(pack_geometry(state.width, state.height), Ordering::Release);
    FALLBACK_NATIVE_WINDOW_FORMAT.store(state.format, Ordering::Release);
    std::ptr::addr_of!(FALLBACK_NATIVE_WINDOW_TOKEN) as usize
}

pub fn fallback_native_window_state(native_window: usize) -> Option<NativeWindowState> {
    if native_window != std::ptr::addr_of!(FALLBACK_NATIVE_WINDOW_TOKEN) as usize {
        return None;
    }
    let (width, height) = unpack_geometry(FALLBACK_NATIVE_WINDOW_GEOMETRY.load(Ordering::Acquire))?;
    Some(NativeWindowState {
        width,
        height,
        format: FALLBACK_NATIVE_WINDOW_FORMAT.load(Ordering::Acquire),
    })
}

static WSI_DISPLAY: Mutex<Option<usize>> = Mutex::new(None);

pub fn set_wsi_display(display: Option<usize>) {
    if let Ok(mut d) = WSI_DISPLAY.lock() {
        *d = display;
    }
}

pub fn wsi_display() -> Option<usize> {
    WSI_DISPLAY.lock().ok().and_then(|d| *d)
}

static WSI_WL_SURFACE: Mutex<Option<usize>> = Mutex::new(None);

pub fn set_wsi_wl_surface(surface: Option<usize>) {
    if let Ok(mut s) = WSI_WL_SURFACE.lock() {
        *s = surface;
    }
}

pub fn wsi_wl_surface() -> Option<usize> {
    WSI_WL_SURFACE.lock().ok().and_then(|s| *s)
}

static ENGINE_CLAIMED_SURFACE: std::sync::atomic::AtomicBool =
    std::sync::atomic::AtomicBool::new(false);

pub fn set_engine_claimed_surface(claimed: bool) {
    ENGINE_CLAIMED_SURFACE.store(claimed, std::sync::atomic::Ordering::Release);
}

pub fn engine_claimed_surface() -> bool {
    ENGINE_CLAIMED_SURFACE.load(std::sync::atomic::Ordering::Acquire)
}

pub fn asset_managers() -> &'static Slab<AssetManagerState> {
    static S: Slab<AssetManagerState> = Slab::new();
    &S
}

pub fn assets() -> &'static Slab<AssetState> {
    static S: Slab<AssetState> = Slab::new();
    &S
}

pub fn configurations() -> &'static Slab<ConfigurationState> {
    static S: Slab<ConfigurationState> = Slab::new();
    &S
}

pub fn loopers() -> &'static Slab<LooperState> {
    static S: Slab<LooperState> = Slab::new();
    &S
}

static LOOPER_WAKERS: Mutex<Vec<super::looper::Waker>> = Mutex::new(Vec::new());

pub fn register_looper_waker(waker: super::looper::Waker) {
    if let Ok(mut wakers) = LOOPER_WAKERS.lock() {
        wakers.push(waker);
    }
}

pub fn wake_all_loopers() -> usize {
    match LOOPER_WAKERS.lock() {
        Ok(wakers) => {
            for w in wakers.iter() {
                w.wake();
            }
            wakers.len()
        }
        Err(_) => 0,
    }
}

pub fn native_windows() -> &'static Slab<NativeWindowState> {
    static S: Slab<NativeWindowState> = Slab::new();
    &S
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn pack_unpack_round_trips() {
        for &(i, g) in &[(0u32, 1u32), (1, 1), (5, 42), (u32::MAX, u32::MAX), (3, 7)] {
            assert_eq!(unpack(pack(i, g)), (i, g));
        }
    }

    #[test]
    fn insert_returns_distinct_nonzero_handles_and_with_addresses_own_slot() {
        let s = native_windows();
        let a = s
            .insert(NativeWindowState {
                width: 100,
                height: 200,
                format: 1,
            })
            .expect("insert a");
        let b = s
            .insert(NativeWindowState {
                width: 300,
                height: 400,
                format: 1,
            })
            .expect("insert b");
        assert_ne!(a, b, "distinct inserts yield distinct handles");
        assert_ne!(a, 0, "a valid handle is never the reserved NULL 0");
        assert_ne!(b, 0, "a valid handle is never the reserved NULL 0");
        assert_eq!(s.with(a, |w| w.width).unwrap(), 100, "a addresses its slot");
        assert_eq!(s.with(b, |w| w.width).unwrap(), 300, "b addresses its slot");
        s.remove(a).expect("remove a");
        s.remove(b).expect("remove b");
    }

    #[test]
    fn freed_handle_is_stale_and_does_not_alias_reused_slot() {
        let s = loopers();
        let make = || LooperState {
            looper: super::super::looper::Looper::new().expect("eventfd"),
        };
        let old = s.insert(make()).expect("insert old");
        s.with(old, |l| l.looper.add_fd(7, 1, 1)).expect("use old");
        s.remove(old).expect("remove old");

        let new = s.insert(make()).expect("insert new");

        assert_eq!(
            s.with(old, |l| l.looper.remove_fd(7)),
            Err(NdkRegistryError::StaleHandle),
            "a freed handle must be StaleHandle, never alias the reused slot"
        );

        assert_eq!(
            s.with(new, |l| l.looper.remove_fd(7)),
            Ok(false),
            "the live handle addresses the reused (cleared) slot"
        );
        s.remove(new).expect("remove new");
    }

    #[test]
    fn out_of_range_fabricated_and_null_handles_return_err_not_panic() {
        let s = configurations();
        let fabricated = pack(u32::MAX, 1);
        assert_eq!(
            s.with(fabricated, |_| ()),
            Err(NdkRegistryError::OutOfRange),
            "a fabricated out-of-range index is OutOfRange, never an OOB deref"
        );

        let null_lookup = s.with(0u64, |_| ());
        assert!(
            matches!(
                null_lookup,
                Err(NdkRegistryError::StaleHandle) | Err(NdkRegistryError::OutOfRange)
            ),
            "the reserved NULL handle 0 must be rejected, got {null_lookup:?}"
        );
        assert_eq!(s.remove(fabricated), Err(NdkRegistryError::OutOfRange));
    }

    #[test]
    fn double_free_is_rejected() {
        let s = assets();
        let h = s
            .insert(AssetState {
                bytes: Box::from(&b"hi"[..]),
                cursor: 0,
            })
            .expect("insert");
        s.remove(h).expect("first free");
        assert_eq!(
            s.remove(h),
            Err(NdkRegistryError::StaleHandle),
            "a second free of the same handle is StaleHandle, not corruption"
        );
    }

    static WSI_TEST_LOCK: Mutex<()> = Mutex::new(());

    #[test]
    fn wsi_window_register_lookup_unregister_round_trips() {
        let _g = WSI_TEST_LOCK.lock().unwrap_or_else(|e| e.into_inner());
        let p: usize = 0x5000_1000;
        register_wsi_window(p, 1280, 720);
        assert_eq!(
            wsi_window_geometry(p),
            Some((1280, 720)),
            "a registered WSI pointer resolves to its geometry"
        );
        assert_eq!(
            current_wsi_window(),
            Some(p),
            "the registered WSI pointer is the current one"
        );

        register_wsi_window(p, 800, 600);
        assert_eq!(
            wsi_window_geometry(p),
            Some((800, 600)),
            "re-register updates geometry"
        );
        unregister_wsi_window(p);
        assert_eq!(
            wsi_window_geometry(p),
            None,
            "an unregistered WSI pointer is unknown → the getter returns the NDK -1 sentinel"
        );
    }

    #[test]
    fn wsi_window_rejects_null_and_unknown_pointers() {
        let _g = WSI_TEST_LOCK.lock().unwrap_or_else(|e| e.into_inner());

        register_wsi_window(0, 100, 100);
        assert_eq!(
            wsi_window_geometry(0),
            None,
            "NULL is never a valid WSI window"
        );

        assert_eq!(
            wsi_window_geometry(0xDEAD_BEEF),
            None,
            "an unknown pointer is None"
        );

        let p: usize = 0x5000_2000;
        register_wsi_window(p, 0, 0);
        assert_eq!(
            wsi_window_geometry(p),
            Some((1, 1)),
            "zero geometry clamps to 1×1"
        );
        unregister_wsi_window(p);
    }

    #[test]
    fn engine_claimed_surface_round_trips_set_and_get() {
        let _g = WSI_TEST_LOCK.lock().unwrap_or_else(|e| e.into_inner());
        set_engine_claimed_surface(false);
        assert!(
            !engine_claimed_surface(),
            "the flag starts (and is restorable to) false"
        );
        set_engine_claimed_surface(true);
        assert!(
            engine_claimed_surface(),
            "set_engine_claimed_surface(true) is observed by engine_claimed_surface()"
        );

        set_engine_claimed_surface(false);
        assert!(!engine_claimed_surface(), "the flag clears back to false");
    }

    #[test]
    fn wsi_display_round_trips_set_and_get() {
        let _g = WSI_TEST_LOCK.lock().unwrap_or_else(|e| e.into_inner());
        let p: usize = 0x5000_1000;
        set_wsi_display(Some(p));
        assert_eq!(
            wsi_display(),
            Some(p),
            "a registered Wayland wl_display round-trips through wsi_display"
        );
        set_wsi_display(None);
        assert_eq!(
            wsi_display(),
            None,
            "clearing to None (X11/other) is observed by wsi_display"
        );
    }

    #[test]
    fn wsi_wl_surface_round_trips_set_and_get() {
        let _g = WSI_TEST_LOCK.lock().unwrap_or_else(|e| e.into_inner());
        let p: usize = 0x6000_2000;
        set_wsi_wl_surface(Some(p));
        assert_eq!(
            wsi_wl_surface(),
            Some(p),
            "a registered Wayland wl_surface round-trips through wsi_wl_surface"
        );
        set_wsi_wl_surface(None);
        assert_eq!(
            wsi_wl_surface(),
            None,
            "clearing to None (X11/other) is observed by wsi_wl_surface"
        );
    }

    #[test]
    fn asset_bytes_have_stable_address_across_with_calls() {
        let s = assets();
        let h = s
            .insert(AssetState {
                bytes: Box::from(&b"stable-bytes"[..]),
                cursor: 0,
            })
            .expect("insert");
        let p1 = s.with(h, |a| a.bytes.as_ptr() as usize).unwrap();
        let p2 = s.with(h, |a| a.bytes.as_ptr() as usize).unwrap();
        assert_eq!(
            p1, p2,
            "the asset buffer address is stable for its lifetime"
        );
        s.remove(h).expect("remove");
    }
}
