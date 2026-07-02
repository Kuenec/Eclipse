//! Process-global generational-slab registry for Eclipse-owned `android.graphics.Bitmap` texture
//! handles.
//!
//! 2026-07-01: ATL's `BitmapFactory.decodeStream` → `nativeDecodeStream(...)` returns a `long`
//! "texture" that `Bitmap(long)` wraps (`Bitmap.java` line 51: `native_get_width(texture)` /
//! `native_get_height(texture)`, then `BitmapDrawable` passes it around as the drawable
//! `paintable`). ATL backs it with a GTK `GdkTexture*`; Eclipse must NOT pull in GTK, so the handle
//! is an **Eclipse-owned generational-slab index into this slab — NOT `Box::into_raw`, NOT a raw
//! pointer**, exactly the soundness pattern of [`theme_registry`](super::theme_registry) /
//! [`view_registry`](super::view_registry). A stale/fabricated `jlong` from Java is a
//! bounds+generation-checked `Err`, never a wild dereference / use-after-free / UB.
//!
//! ## Scope (this increment — the headless-recording model)
//! [`BitmapState`] RECORDS the decoded image's dimensions plus its raw encoded bytes (the view tree
//! is recorded, not rasterized — Eclipse's framework is headless; the engine draws the screen).
//! This is what un-dams `Resources.loadDrawable`'s `.png` path once the styled-attribute
//! string-pool fix lets file-path drawables resolve: without a live handle, `new Bitmap(0)` →
//! `Bitmap.getTexture()` → the unbound GTK `native_create_texture` would abort inflation with an
//! `UnsatisfiedLinkError`. Pixel decode/raster is the deferred render build (AGENTS.md §5).
//!
//! ## Handle layout
//! Identical to the sibling registries: a [`jlong`] packing a `u32` slot index (low 32 bits) + a
//! `u32` generation (high 32 bits). Generations start at 1, so a valid handle is never `0` — `0`
//! stays the reserved "no bitmap" / null sentinel.

#![forbid(unsafe_code)]

use std::fmt;
use std::sync::{Mutex, OnceLock, PoisonError};

use jni::sys::jlong;

/// Process-global slab of [`BitmapState`], guarded by a [`Mutex`]. Initialized on first use.
static BITMAPS: OnceLock<Mutex<Registry>> = OnceLock::new();

/// A bitmap-registry handle as it travels across JNI (the Java `Bitmap.texture` /
/// `Drawable.paintable` `long`): a `jlong` packing the slot index (low 32 bits) and the slot's
/// generation (high 32 bits). `0` is the reserved "no bitmap" / null sentinel.
pub type BitmapHandle = jlong;

/// Errors from the bitmap registry. Every fallible path returns one of these instead of panicking,
/// so a stale/out-of-range/fabricated `jlong` from Java can never cause UB or unwind across JNI.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum BitmapRegistryError {
    /// The handle's slot index is outside the slab (fabricated handle, or the reserved `0`).
    OutOfRange,
    /// The slot exists but its generation does not match (a freed, possibly reused slot).
    StaleHandle,
    /// The registry mutex was poisoned by a panic in another holder (surfaced, not re-panicked).
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

/// One recorded bitmap: real dimensions (parsed from the encoded image's header) plus the retained
/// encoded source bytes (for the deferred raster pass; the headless framework never samples them).
#[derive(Debug, Default)]
pub struct BitmapState {
    /// Image width in pixels (`Bitmap.getWidth`); `0` when the encoding was unrecognized.
    pub width: i32,
    /// Image height in pixels (`Bitmap.getHeight`); `0` when the encoding was unrecognized.
    pub height: i32,
    /// The raw ENCODED image bytes as read from the stream (e.g. the PNG file), retained verbatim.
    pub bytes: Vec<u8>,
}

/// A generational slot: the current generation plus the optional occupant.
struct Slot {
    generation: u32,
    state: Option<BitmapState>,
}

/// The slab + free list (same shape as the sibling registries).
#[derive(Default)]
struct Registry {
    slots: Vec<Slot>,
    free: Vec<u32>,
}

/// Pack a slot index + generation into a `jlong` handle (generation high, index low).
fn pack(index: u32, generation: u32) -> BitmapHandle {
    ((generation as u64) << 32 | index as u64) as i64
}

/// Unpack a `jlong` handle into (slot index, generation).
fn unpack(handle: BitmapHandle) -> (u32, u32) {
    let bits = handle as u64;
    ((bits & 0xFFFF_FFFF) as u32, (bits >> 32) as u32)
}

/// Lock the process-global registry, mapping a poisoned mutex to the typed
/// [`BitmapRegistryError::Poisoned`] (never a panic — AGENTS.md §2.8).
fn lock() -> Result<std::sync::MutexGuard<'static, Registry>, BitmapRegistryError> {
    BITMAPS
        .get_or_init(|| Mutex::new(Registry::default()))
        .lock()
        .map_err(|_: PoisonError<_>| BitmapRegistryError::Poisoned)
}

/// Store a recorded bitmap and return its packed [`BitmapHandle`] (`jlong`, generation ≥ 1, never
/// the reserved `0`). Reuses a freed slot when one is available, otherwise grows the slab.
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

/// Look up the [`BitmapState`] for a `handle` and run `f` against it under the registry lock.
/// Bounds-checks the slot index **and** verifies the handle's generation, so a stale/out-of-range/
/// fabricated handle returns `Err` and never dereferences out of bounds or aliases another bitmap.
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

/// Free the slot a `handle` refers to, bumping its generation so any other handle to it (or this
/// one, reused later) is rejected as [`BitmapRegistryError::StaleHandle`].
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

    // 2026-07-01: same soundness contract as the sibling registries; fully in-harness (no VM).

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
