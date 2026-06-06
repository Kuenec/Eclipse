//! Process-global generational-slab registries for Eclipse-owned **NDK opaque handles**.
//!
//! 2026-06-05: the NDK `libandroid` C-ABI hands the engine opaque pointers — `AAssetManager*`,
//! `AAsset*`, `AConfiguration*`, `ALooper*`, `ANativeWindow*` — that it later passes back to the
//! getters/closers. Eclipse owns **both** sides of these pointers (it implements every libandroid
//! native in [`super::native_provider`]), so it defines their meaning: each "pointer" is an
//! **Eclipse-owned generational registry handle**, not `Box::into_raw` / a raw heap pointer.
//!
//! Why a registry index instead of `Box::into_raw(state)` (the same soundness argument as
//! [`crate::framework::window_registry`], dated 2026-06-05): a **stale or fabricated** handle (a
//! double-close, a wrong cast, a buggy engine path) becomes a **bounds-checked + generation-checked
//! lookup that returns `Err`**, which the native turns into the NDK error sentinel (NULL / negative)
//! — never a wild dereference, use-after-free, or UB. `Box::into_raw` would turn any wrong pointer
//! into instant UB across the C ABI.
//!
//! ## Handle layout (matches `window_registry`)
//! A handle packs a `u32` **slot index** (low 32 bits) and a `u32` **generation** (high 32 bits)
//! into a `u64`, which the native casts to the opaque `*mut T` it returns to C. Freeing a slot bumps
//! its generation, so an old handle to a later-reused slot fails the generation check. Generations
//! start at 1, so a live handle's high bits are non-zero — `0` (a C `NULL`) can never be a valid
//! handle, keeping it reserved as the null sentinel.
//!
//! ## Thread-safety
//! Each slab lives behind a [`Mutex`] in a [`OnceLock`] (process-global, std-only, no new dep). A
//! poisoned lock surfaces as [`NdkRegistryError::Poisoned`] (not a re-panic), so the C-ABI native
//! path stays panic-free (AGENTS.md §2.8 — never unwind across FFI).
//!
//! ## Pointer-stability note for `AAsset_getBuffer` (dated 2026-06-05)
//! [`AssetState`] owns the asset bytes in a `Box<[u8]>`. `AAsset_getBuffer` must return a
//! `const void*` valid until `AAsset_close`. A `Box<[u8]>`'s contents have a **stable heap address**
//! that does not move while the box lives (it is read-only after open — never re-allocated), and the
//! slot is not freed until close, so the returned pointer stays valid for the asset's lifetime. The
//! native that hands the pointer out documents this in its `// SAFETY:` note.

#![forbid(unsafe_code)]

use std::fmt;
use std::path::PathBuf;
use std::sync::{Mutex, OnceLock, PoisonError};

/// A packed NDK opaque-handle value: the `u64` a native casts to/from the opaque `*mut T`. The low
/// 32 bits are the slot index, the high 32 bits the generation (see module docs).
pub type NdkHandle = u64;

/// Errors from an NDK handle registry. Every fallible lookup returns one of these so a
/// stale/out-of-range/fabricated opaque pointer from the engine can never cause UB or unwind across
/// the C ABI — the native maps the `Err` to the NDK error sentinel (NULL / negative).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum NdkRegistryError {
    /// The handle's slot index is outside the slab (fabricated handle, or the reserved `0`/NULL).
    OutOfRange,
    /// The slot exists but its generation does not match: the handle refers to a freed (possibly
    /// reused) slot. The key soundness rejection — never aliases the new occupant.
    StaleHandle,
    /// The registry mutex was poisoned by a panic in another holder. Surfaced as an error (not a
    /// re-panic) so the C-ABI path stays panic-free (AGENTS.md §2.8).
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

// =================================================================================================
// Generic generational slab — one instance per NDK handle type.
// =================================================================================================

/// A generational slot: the current generation plus the optional occupant. A live slot is
/// `Some(state)`; a freed slot is `None` but keeps (and on free, bumps) its generation so stale
/// handles to it are rejected.
struct Slot<T> {
    generation: u32,
    state: Option<T>,
}

/// A process-global generational slab of `T`, guarded by a [`Mutex`] in a [`OnceLock`].
///
/// One static instance backs each NDK handle type (see [`asset_managers`], [`assets`], etc.). The
/// API mirrors [`crate::framework::window_registry`]: [`Self::insert`] allocates a slot and returns
/// a packed [`NdkHandle`]; [`Self::with`] runs a closure against the occupant under the lock with a
/// bounds + generation check; [`Self::remove`] frees a slot and bumps its generation.
pub struct Slab<T> {
    inner: OnceLock<Mutex<Registry<T>>>,
}

struct Registry<T> {
    slots: Vec<Slot<T>>,
    free: Vec<u32>,
}

// Manual `Default` (not derived): the derive would add a spurious `T: Default` bound, but a registry
// of any `T` is always default-constructible (two empty `Vec`s). 2026-06-05.
impl<T> Default for Registry<T> {
    fn default() -> Self {
        Self {
            slots: Vec::new(),
            free: Vec::new(),
        }
    }
}

/// Pack a slot index + generation into a handle (generation high, index low).
fn pack(index: u32, generation: u32) -> NdkHandle {
    (generation as u64) << 32 | index as u64
}

/// Unpack a handle into (slot index, generation).
fn unpack(handle: NdkHandle) -> (u32, u32) {
    ((handle & 0xFFFF_FFFF) as u32, (handle >> 32) as u32)
}

impl<T> Default for Slab<T> {
    fn default() -> Self {
        Self::new()
    }
}

impl<T> Slab<T> {
    /// A new, empty slab (const so it can back a `static`).
    pub const fn new() -> Self {
        Self {
            inner: OnceLock::new(),
        }
    }

    /// Lock the slab, mapping a poisoned mutex to the typed [`NdkRegistryError::Poisoned`].
    fn lock(&self) -> Result<std::sync::MutexGuard<'_, Registry<T>>, NdkRegistryError> {
        self.inner
            .get_or_init(|| Mutex::new(Registry::default()))
            .lock()
            .map_err(|_: PoisonError<_>| NdkRegistryError::Poisoned)
    }

    /// Insert `state` into a fresh (or reused) slot and return its packed [`NdkHandle`]. The handle's
    /// generation is ≥ 1, so it is never `0` (the reserved NULL). Returns
    /// [`NdkRegistryError::Poisoned`] only if the mutex was poisoned — never panics.
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

    /// Run `f` against the occupant for `handle` under the lock. Bounds-checks the slot index **and**
    /// verifies the generation, so a stale (freed/reused), out-of-range, or fabricated handle (incl.
    /// the reserved `0`/NULL) returns `Err` and never dereferences out of bounds or aliases a
    /// different occupant.
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

    /// Free the slot `handle` refers to, bumping its generation so any other copy of it (or this one,
    /// reused later) is rejected as [`NdkRegistryError::StaleHandle`]. Validates the handle exactly
    /// as [`Self::with`] does, so freeing an already-freed/stale/fabricated handle returns `Err`
    /// rather than corrupting the free list.
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
        // Saturating so a (practically unreachable) 2^32 reuse cycle never wraps onto a live value.
        slot.generation = slot.generation.saturating_add(1);
        reg.free.push(index);
        Ok(())
    }
}

// =================================================================================================
// Per-type state held in each slab + the process-global slab statics.
// =================================================================================================

/// State behind an `AAssetManager*` handle: the APK the asset manager reads from. Cloned per
/// `AAssetManager_fromJava` call (cheap — just a `PathBuf`); `AAssetManager_open` re-opens the APK
/// and reads the named `assets/<name>` zip entry through Eclipse's own `src/apk` reader.
#[derive(Debug, Clone)]
pub struct AssetManagerState {
    /// The on-disk APK path Eclipse's `src/apk` reader opens to serve `AAssetManager_open`.
    pub apk_path: PathBuf,
}

/// State behind an `AAsset*` handle: the entire asset contents (read from the APK at open) plus a
/// read cursor. `AAsset_getBuffer` returns a pointer into `bytes` (stable for the asset's lifetime,
/// see the module docs); `AAsset_getLength` returns `bytes.len()`.
#[derive(Debug)]
pub struct AssetState {
    /// The asset's full uncompressed contents, owned for the asset's lifetime. `Box<[u8]>` so its
    /// heap address is stable (never re-allocated) while the slot lives — required for the
    /// `AAsset_getBuffer` pointer-stability contract.
    pub bytes: Box<[u8]>,
    /// The read cursor (bytes consumed). Reserved for a future `AAsset_read`/`AAsset_seek`; the
    /// current 27-symbol cut serves reads via `getBuffer`+`getLength`.
    pub cursor: usize,
}

/// State behind an `AConfiguration*` handle: Eclipse's minimal-correct device configuration values.
/// Real getters read these back. Defaults are sane desktop-Linux values (mdpi, the window geometry
/// in dp, portrait) until a real device-config source is wired.
#[derive(Debug, Clone)]
pub struct ConfigurationState {
    /// Display density in dpi (`AConfiguration_getDensity`); `ACONFIGURATION_DENSITY_MEDIUM` = 160.
    pub density: i32,
    /// Available screen width in dp (`AConfiguration_getScreenWidthDp`).
    pub screen_width_dp: i32,
    /// Available screen height in dp (`AConfiguration_getScreenHeightDp`).
    pub screen_height_dp: i32,
    /// Screen-size bucket (`AConfiguration_getScreenSize`); `ACONFIGURATION_SCREENSIZE_NORMAL` = 2.
    pub screen_size: i32,
    /// Orientation (`AConfiguration_getOrientation`); `ACONFIGURATION_ORIENTATION_PORT` = 1.
    pub orientation: i32,
    /// Nav-keys-hidden state (`AConfiguration_getNavHidden`); `ACONFIGURATION_NAVHIDDEN_YES` = 2
    /// (a touchscreen desktop has no hardware nav keys).
    pub nav_hidden: i32,
    /// BCP-47 language (2 chars, `AConfiguration_getLanguage` fills a `char[2]`). `"en"` default.
    pub language: [u8; 2],
    /// ISO-3166 country (2 chars, `AConfiguration_getCountry` fills a `char[2]`). `"US"` default.
    pub country: [u8; 2],
}

/// State behind an `ALooper*` handle: a minimal Eclipse per-thread looper. Holds only the registered
/// fd identifiers (so `addFd`/`removeFd` are bookkeeping-correct); the real epoll/event wiring is
/// deferred (the looper has no event source until the render/input integration). See the looper
/// natives' docs for the documented poll sentinels.
#[derive(Debug, Default)]
pub struct LooperState {
    /// Registered `(fd, ident)` pairs from `ALooper_addFd`, removed by `ALooper_removeFd`. Tracked so
    /// add/remove return contract-correct values; not yet polled (no event source).
    pub fds: Vec<(i32, i32)>,
}

/// State behind an `ANativeWindow*` handle: the window geometry the getters return. Sound for the
/// getters (real values); buffer/surface ops are deferred to the render integration.
#[derive(Debug, Clone, Copy)]
pub struct NativeWindowState {
    /// Window width in pixels (`ANativeWindow_getWidth`).
    pub width: i32,
    /// Window height in pixels (`ANativeWindow_getHeight`).
    pub height: i32,
    /// Pixel format (`ANativeWindow_getFormat`); `AHARDWAREBUFFER_FORMAT_R8G8B8A8_UNORM` = 1.
    pub format: i32,
}

/// The process-global APK path the NDK asset natives serve from.
///
/// `AAssetManager_fromJava(JNIEnv*, jobject)` receives a Java `AssetManager` Eclipse owns the
/// backing of; it cannot derive the APK from the opaque JNI args without the (cyber-safeguarded)
/// framework asset code, so the boot path **configures the APK path here** (the same APK Eclipse
/// already opened to stage `libroblox.so`). When unset, `AAssetManager_fromJava` returns NULL — a
/// sound "no asset source" answer, never a fake manager. Set once; idempotent.
static APK_PATH: OnceLock<PathBuf> = OnceLock::new();

/// Configure the APK path the NDK asset natives serve. Idempotent: the first set wins; later calls
/// with the same or a different path leave the first value (returns whether this call set it). The
/// boot path calls this with the opened Roblox APK before the engine's init runs.
pub fn set_apk_path(path: PathBuf) -> bool {
    APK_PATH.set(path).is_ok()
}

/// The configured APK path, or `None` if the boot path has not set one (then asset natives report no
/// source rather than fabricating one).
pub fn apk_path() -> Option<&'static PathBuf> {
    APK_PATH.get()
}

/// The real, live geometry of Eclipse's engine render window, in physical pixels.
///
/// 2026-06-05: `ANativeWindow_fromSurface` mints a window handle whose geometry the engine reads via
/// `ANativeWindow_getWidth/Height/Format` to size its EGL framebuffer. The window is opened by `winit`
/// only after the event loop is `resumed`, *after* the engine-init path; so the run/test path
/// **publishes the live window's geometry here** (the same window the engine's EGL surface presents
/// to — see [`crate::egl_engine`]). When unset, the natives fall back to a documented portrait
/// default (a sound geometry, never a crash). Updated on resize so the engine can re-query.
static ENGINE_WINDOW_GEOMETRY: Mutex<Option<(i32, i32)>> = Mutex::new(None);

/// Publish Eclipse's live window geometry (physical pixels) for the `ANativeWindow_*` geometry
/// natives. Called by the run/test path when the window is created and on each resize. Clamped to
/// ≥ 1×1 (a zero dimension is not a valid surface size). A poisoned lock is ignored (best-effort
/// publish; the natives then read the last good / default value) so this never panics.
pub fn set_engine_window_geometry(width: i32, height: i32) {
    if let Ok(mut g) = ENGINE_WINDOW_GEOMETRY.lock() {
        *g = Some((width.max(1), height.max(1)));
    }
}

/// The published engine-window geometry `(width, height)` in physical pixels, or `None` if the run/
/// test path has not opened a window yet (then the geometry natives use their documented default).
pub fn engine_window_geometry() -> Option<(i32, i32)> {
    ENGINE_WINDOW_GEOMETRY.lock().ok().and_then(|g| *g)
}

/// The process-global `AAssetManager*` slab.
pub fn asset_managers() -> &'static Slab<AssetManagerState> {
    static S: Slab<AssetManagerState> = Slab::new();
    &S
}

/// The process-global `AAsset*` slab.
pub fn assets() -> &'static Slab<AssetState> {
    static S: Slab<AssetState> = Slab::new();
    &S
}

/// The process-global `AConfiguration*` slab.
pub fn configurations() -> &'static Slab<ConfigurationState> {
    static S: Slab<ConfigurationState> = Slab::new();
    &S
}

/// The process-global `ALooper*` slab.
pub fn loopers() -> &'static Slab<LooperState> {
    static S: Slab<LooperState> = Slab::new();
    &S
}

/// The process-global `ANativeWindow*` slab.
pub fn native_windows() -> &'static Slab<NativeWindowState> {
    static S: Slab<NativeWindowState> = Slab::new();
    &S
}

#[cfg(test)]
mod tests {
    use super::*;

    // The slabs are process-global; tests share each one and are written order-independent (each
    // allocates its own handles, never asserts absolute slot indices). They prove the soundness
    // contract: distinct non-NULL handles, correct-slot access, and the key property that a freed
    // handle becomes Stale after the slot is reused — never UB / cross-talk.

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
        let old = s.insert(LooperState::default()).expect("insert old");
        s.with(old, |l| l.fds.push((7, 1))).expect("use old");
        s.remove(old).expect("remove old");
        // Reuse pops the freed slot with a bumped generation.
        let new = s.insert(LooperState::default()).expect("insert new");
        // The OLD handle must now be Stale — never reading/writing the NEW occupant.
        assert_eq!(
            s.with(old, |l| l.fds.len()),
            Err(NdkRegistryError::StaleHandle),
            "a freed handle must be StaleHandle, never alias the reused slot"
        );
        assert_eq!(
            s.with(new, |l| l.fds.len()),
            Ok(0),
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
        // The reserved NULL handle (0) is never a live generation (live ≥ 1) → rejected.
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

    #[test]
    fn asset_bytes_have_stable_address_across_with_calls() {
        // Pointer-stability contract for AAsset_getBuffer: the bytes' address must not change between
        // lookups while the slot lives (the Box<[u8]> is never re-allocated).
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
