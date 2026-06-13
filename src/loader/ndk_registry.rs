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

/// State behind an `ALooper*` handle: a real, wakeable Eclipse per-thread looper. Owns a wake
/// `eventfd` + the registered `(fd, ident, events)` poll set; `ALooper_pollOnce` does a genuine
/// `poll(2)` over them. See [`super::looper::Looper`] for the event model. (2026-06-05: upgraded from
/// the prior bookkeeping-only fd list — the looper now actually blocks/wakes on its fds.)
#[derive(Debug)]
pub struct LooperState {
    /// The real fd-backed looper this handle drives. `ALooper_addFd`/`removeFd`/`pollOnce` operate on
    /// it; `ALooper_prepare` constructs it (an `eventfd` is allocated then).
    pub looper: super::looper::Looper,
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

/// The engine render WSI bind: a map from the **real WSI native-window pointer** (a Wayland
/// `wl_egl_window*` / an X11 XID, as a `usize`) to that window's geometry.
///
/// 2026-06-05 — why this exists: Roblox's native engine creates its OWN EGL surface by calling host
/// `eglCreateWindowSurface(display, config, (EGLNativeWindowType)<the `ANativeWindow*` it got from
/// `ANativeWindow_fromSurface`>, …)`. For that surface to land on Eclipse's window, the
/// `ANativeWindow*` Eclipse hands the engine must BE the real WSI handle host EGL accepts. So
/// `ANativeWindow_fromSurface` returns that real WSI pointer (owned by [`crate::egl_engine::
/// EngineNativeWindow`], which lives on the main thread alongside the `winit` window), and this map
/// lets the geometry natives (`ANativeWindow_getWidth`/`getHeight`), which take that pointer back,
/// answer with the real size — a bounds-safe table lookup (unknown pointer → `None` → the NDK `-1`
/// sentinel), never a dereference of an engine-supplied pointer. Only the pointer **value** + the
/// geometry (both `Copy`) cross into this process-global table; the `EngineNativeWindow` itself never
/// leaves its owning thread.
static WSI_WINDOWS: Mutex<Vec<(usize, (i32, i32))>> = Mutex::new(Vec::new());

/// Register a real WSI native-window pointer + its geometry as the backing for the engine's
/// `ANativeWindow*`. Idempotent on the pointer (re-registering updates the geometry). A poisoned
/// lock is ignored (best-effort; never panics — AGENTS.md §2.8).
pub fn register_wsi_window(native_window: usize, width: i32, height: i32) {
    if native_window == 0 {
        return; // a NULL pointer is never a valid WSI window (reserved as the ANativeWindow NULL).
    }
    if let Ok(mut v) = WSI_WINDOWS.lock() {
        let geo = (width.max(1), height.max(1));
        if let Some(entry) = v.iter_mut().find(|(p, _)| *p == native_window) {
            entry.1 = geo;
        } else {
            v.push((native_window, geo));
        }
    }
}

/// Remove a WSI native-window pointer registered by [`register_wsi_window`] (called when its owning
/// [`crate::egl_engine::EngineNativeWindow`] is torn down). A poisoned lock / unknown pointer is a
/// no-op; never panics.
pub fn unregister_wsi_window(native_window: usize) {
    if let Ok(mut v) = WSI_WINDOWS.lock() {
        v.retain(|(p, _)| *p != native_window);
    }
}

/// The geometry registered for a WSI native-window pointer, or `None` if it is not a known WSI
/// window (a fabricated/stale `ANativeWindow*` → `None` → the geometry native returns the NDK `-1`
/// sentinel, never a dereference). Poisoned lock → `None`.
pub fn wsi_window_geometry(native_window: usize) -> Option<(i32, i32)> {
    WSI_WINDOWS
        .lock()
        .ok()
        .and_then(|v| v.iter().find(|(p, _)| *p == native_window).map(|(_, g)| *g))
}

/// The real WSI native-window pointer Eclipse currently exposes (the most recently registered — there
/// is one Eclipse window), or `None` if the render path has not built an
/// [`crate::egl_engine::EngineNativeWindow`] yet. `ANativeWindow_fromSurface` returns this so the
/// `ANativeWindow*` the engine gets IS the real WSI handle host EGL accepts; when `None` the native
/// falls back to a sound geometry-only handle (the window does not exist yet at that point).
pub fn current_wsi_window() -> Option<usize> {
    WSI_WINDOWS
        .lock()
        .ok()
        .and_then(|v| v.last().map(|(p, _)| *p))
}

/// 2026-06-13 — the winit Wayland `wl_display*` connection (as a `usize`) the engine's EGLDisplay must
/// MATCH, or `None` on X11/other.
///
/// Why this exists: Roblox's engine resolves `egl*` through the host `libEGL.so` (bionic_env tier 1)
/// and calls `eglGetDisplay(EGL_DEFAULT_DISPLAY)`. Per the Khronos `EGL_KHR_platform_wayland` /
/// `EGL_EXT_platform_wayland` registry text, on Wayland `EGL_DEFAULT_DISPLAY` makes EGL open Mesa's
/// OWN `wl_display` by `wl_display_connect(NULL)` — a DIFFERENT connection than the one `winit` opened
/// for Eclipse's window. The `ANativeWindow*` Eclipse hands the engine ([`current_wsi_window`]) wraps a
/// `wl_egl_window*` on `winit`'s `wl_surface`; `eglCreateWindowSurface` requires the EGLDisplay's
/// `wl_display` and the `wl_egl_window`'s `wl_surface` to be on the SAME connection — crossing them is
/// `EGL_BAD_ALLOC` (3003). Eclipse intercepts `eglGetDisplay` (tier-0 native) and remaps
/// `EGL_DEFAULT_DISPLAY` to THIS pointer so the engine's EGLDisplay lands on `winit`'s connection —
/// identical to what `egl_engine` does for `__gl-test-anw`. `None` on X11/other: an X11 window XID is
/// server-scoped (not connection-scoped the same way), so passing `EGL_DEFAULT_DISPLAY` through is
/// correct there. Stores only the pointer VALUE as `usize` (this module is `#![forbid(unsafe_code)]`),
/// exactly like [`WSI_WINDOWS`] stores the native-window pointer.
static WSI_DISPLAY: Mutex<Option<usize>> = Mutex::new(None);

/// Record the winit Wayland `wl_display*` (as a `usize`), or `None` on X11/other (see [`WSI_DISPLAY`]).
/// Called by `graphics::run_windowed`'s `resumed` once it has the winit window's display handle. A
/// poisoned lock is ignored (best-effort; never panics — AGENTS.md §2.8).
pub fn set_wsi_display(display: Option<usize>) {
    if let Ok(mut d) = WSI_DISPLAY.lock() {
        *d = display;
    }
}

/// The registered winit Wayland `wl_display*` (as a `usize`), or `None` on X11/other or before
/// `resumed` registered it (see [`WSI_DISPLAY`]). The tier-0 `eglGetDisplay` native reads this to
/// remap `EGL_DEFAULT_DISPLAY` to winit's connection. Poisoned lock → `None`.
pub fn wsi_display() -> Option<usize> {
    WSI_DISPLAY.lock().ok().and_then(|d| *d)
}

/// 2026-06-13 — render Phase 2.1 present-loop handoff CONFIRMATION signal: set `true` the first time the
/// engine actually PULLS the real WSI surface, i.e. `ANativeWindow_fromSurface`
/// ([`super::native_provider::eclipse_anativewindow_fromsurface`]) returns the real WSI pointer (its
/// `current_wsi_window().is_some()` branch — NOT the geometry-only fallback). This is NO LONGER the
/// renderer-drop trigger (Phase 2 used it as one and released the `wl_surface` ~19 ms too late — after
/// the engine's `eglCreateWindowSurface`, causing two owners of one `wl_surface` → `EGL_BAD_ALLOC`
/// 3003). The drop now happens in `graphics::about_to_wait` gated on
/// [`crate::framework::engine_surface_callback_ready`] (non-empty `mCallbacks`), STRICTLY BEFORE
/// `dispatch_surface_lifecycle`. The winit loop reads [`engine_claimed_surface`] only for a one-shot
/// confirmation log correlating the handoff with the engine genuinely claiming the surface. Lock-free
/// (a single atomic), so the render native and the winit loop never contend a mutex on this hot signal.
static ENGINE_CLAIMED_SURFACE: std::sync::atomic::AtomicBool =
    std::sync::atomic::AtomicBool::new(false);

/// Record that the engine has pulled the real WSI surface (see [`ENGINE_CLAIMED_SURFACE`]). Called by
/// `eclipse_anativewindow_fromsurface` when it returns the real WSI pointer.
pub fn set_engine_claimed_surface(claimed: bool) {
    ENGINE_CLAIMED_SURFACE.store(claimed, std::sync::atomic::Ordering::Release);
}

/// Whether the engine has pulled the real WSI surface yet (see [`ENGINE_CLAIMED_SURFACE`]). 2026-06-13
/// — confirmation signal only: the winit loop reads this AFTER the handoff to log, once, that the engine
/// genuinely claimed the surface. It is NOT the renderer-drop trigger (that is
/// [`crate::framework::engine_surface_callback_ready`], evaluated before `dispatch_surface_lifecycle`).
pub fn engine_claimed_surface() -> bool {
    ENGINE_CLAIMED_SURFACE.load(std::sync::atomic::Ordering::Acquire)
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

/// Process-global list of every prepared looper's wake handle, so the winit input feed can wake a
/// parked `ALooper_pollOnce` **without** taking the [`loopers`] slab mutex (which a parked poll could
/// be holding-snapshot-of — see [`super::looper`] lock discipline).
///
/// 2026-06-05: Roblox's engine threads each `ALooper_prepare` a looper and park in `pollOnce` on their
/// own input source. A host input event is a liveness signal that the engine's loopers should re-check
/// their sources, so the engine-path winit feed [`wake_all_loopers`] every registered waker. The
/// `Waker` is a cheap `Arc<eventfd>` clone; waking is lock-free and best-effort.
static LOOPER_WAKERS: Mutex<Vec<super::looper::Waker>> = Mutex::new(Vec::new());

/// Register a prepared looper's [`super::looper::Waker`] so [`wake_all_loopers`] can wake it. Called by
/// `ALooper_prepare` after it builds the real looper. Idempotent per looper is not required (each
/// `prepare` of a new looper adds one); the list is process-lifetime (loopers are not freed).
pub fn register_looper_waker(waker: super::looper::Waker) {
    if let Ok(mut wakers) = LOOPER_WAKERS.lock() {
        wakers.push(waker);
    }
}

/// Wake every registered looper (the engine-path winit input feed calls this on a host input event so
/// a parked `ALooper_pollOnce` returns `ALOOPER_POLL_WAKE` and the engine thread re-checks its input).
/// Lock-free per waker (each is an `Arc<eventfd>` write); returns the number of loopers woken.
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
        let make = || LooperState {
            looper: super::super::looper::Looper::new().expect("eventfd"),
        };
        let old = s.insert(make()).expect("insert old");
        s.with(old, |l| l.looper.add_fd(7, 1, 1)).expect("use old");
        s.remove(old).expect("remove old");
        // Reuse pops the freed slot with a bumped generation.
        let new = s.insert(make()).expect("insert new");
        // The OLD handle must now be Stale — never reading/writing the NEW occupant.
        assert_eq!(
            s.with(old, |l| l.looper.remove_fd(7)),
            Err(NdkRegistryError::StaleHandle),
            "a freed handle must be StaleHandle, never alias the reused slot"
        );
        // The live handle addresses the reused (freshly-constructed) slot — its looper has no fds.
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

    // Serializes the WSI-window-registry tests (the registry is process-global). 2026-06-05.
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
        // Re-registering the same pointer updates the geometry (a resize), not a duplicate.
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
        // A NULL pointer is reserved as the ANativeWindow NULL → never registered.
        register_wsi_window(0, 100, 100);
        assert_eq!(
            wsi_window_geometry(0),
            None,
            "NULL is never a valid WSI window"
        );
        // An unknown (fabricated) pointer resolves to None, never a wild read.
        assert_eq!(
            wsi_window_geometry(0xDEAD_BEEF),
            None,
            "an unknown pointer is None"
        );
        // Geometry is clamped to ≥ 1×1 (a zero dimension is not a valid surface size).
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
        // 2026-06-13: the render Phase 2 present-loop handoff signal. set_engine_claimed_surface
        // toggles the flag engine_claimed_surface reads. Serialized under WSI_TEST_LOCK and restored
        // to the boot-initial `false` at the end so it does not leak into other tests in this binary.
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
        // Restore the boot-initial state so the live boot's first-tick read still sees `false`.
        set_engine_claimed_surface(false);
        assert!(!engine_claimed_surface(), "the flag clears back to false");
    }

    #[test]
    fn wsi_display_round_trips_set_and_get() {
        // 2026-06-13: the winit wl_display connection the engine's EGLDisplay must match. set_wsi_display
        // round-trips through wsi_display; Some(ptr) on Wayland, None on X11/other. Serialized under
        // WSI_TEST_LOCK and RESTORED to None at the end so it does not leak the registered display into
        // other tests in this binary (same discipline as engine_claimed_surface_round_trips_set_and_get).
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
