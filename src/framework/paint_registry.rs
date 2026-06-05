//! Process-global generational-slab registry for Eclipse-owned `android.graphics.Paint` handles.
//!
//! 2026-06-05: AOSP's `Paint.native_create()` creates a native paint object and returns its `long`
//! handle (`Paint.mNativePaint`); the `native_set*` configurators (color, text size, …) and the
//! `Canvas` drawing natives consume it. ATL backs this in C against GTK/Cairo; Eclipse must NOT pull
//! in GTK (AGENTS.md §5 Step 3.5), so a Paint's `long` handle is an **Eclipse-owned generational-slab
//! index into this slab — NOT `Box::into_raw`, NOT a raw pointer**, exactly the soundness pattern of
//! the sibling registries ([`window_registry`](super::window_registry) etc.). A stale/fabricated
//! `jlong` from Java is a bounds+generation-checked `Err`, never a wild dereference / UB.
//!
//! ## Handle layout
//! Identical to the sibling registries: a [`jlong`] packing a `u32` slot index (low 32 bits) + a
//! `u32` generation (high 32 bits). Generations start at 1, so a valid handle is never `0` (the
//! reserved "no paint" / null sentinel).
//!
//! ## Scope (this increment)
//! [`PaintState`] records the **drawing configuration** the Paint setters set (color, text size). It
//! does NO drawing and holds no GTK/Cairo context — the real ash/Vulkan text/shape rendering is the
//! deferred big build (AGENTS.md §5). Recording the config soundly is what lets the View hierarchy's
//! `TextPaint`/`Paint` construct during `setContentView` without GTK.

#![forbid(unsafe_code)]

use std::fmt;
use std::sync::{Mutex, OnceLock, PoisonError};

use jni::sys::jlong;

/// Process-global slab of [`PaintState`], guarded by a [`Mutex`]. Initialized on first use.
static PAINTS: OnceLock<Mutex<Registry>> = OnceLock::new();

/// A paint-registry handle as it travels across JNI: a `jlong` packing the slot index (low 32 bits)
/// and the slot's generation (high 32 bits). `0` is the reserved "no paint" / null sentinel.
pub type PaintHandle = jlong;

/// Errors from the paint registry. Every fallible path returns one of these instead of panicking, so
/// a stale/out-of-range/fabricated `jlong` from Java can never cause UB or unwind across JNI.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum PaintRegistryError {
    /// The handle's slot index is outside the slab (fabricated handle, or the reserved `0`).
    OutOfRange,
    /// The slot exists but its generation does not match: the handle refers to a freed (and possibly
    /// reused) slot. Never aliases the new occupant.
    StaleHandle,
    /// The registry mutex was poisoned by a panic in another holder. Surfaced as an error (not a
    /// re-panic) so the JNI path stays panic-free (AGENTS.md §2.8).
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

/// Per-paint state held in a registry slot: the non-GTK drawing configuration.
///
/// 2026-06-05: minimal by design — `color` (ARGB, `Paint.native_setColor`) and `text_size`
/// (`Paint.native_setTextSize`). Holds no GTK/Cairo context and performs no drawing (deferred). More
/// fields (typeface, stroke, flags) are added when their setters are bound.
#[derive(Debug, Default)]
pub struct PaintState {
    /// ARGB color (`Paint.setColor`); 0 (transparent black) until set.
    pub color: i32,
    /// Text size in pixels (`Paint.setTextSize`); 0.0 until set.
    pub text_size: f32,
}

/// A generational slot: the current generation plus the optional occupant.
struct Slot {
    generation: u32,
    state: Option<PaintState>,
}

/// The slab + free list (same shape as the sibling registries).
#[derive(Default)]
struct Registry {
    slots: Vec<Slot>,
    free: Vec<u32>,
}

/// Pack a slot index + generation into a `jlong` handle (generation high, index low).
fn pack(index: u32, generation: u32) -> PaintHandle {
    ((generation as u64) << 32 | index as u64) as i64
}

/// Unpack a `jlong` handle into (slot index, generation).
fn unpack(handle: PaintHandle) -> (u32, u32) {
    let bits = handle as u64;
    ((bits & 0xFFFF_FFFF) as u32, (bits >> 32) as u32)
}

/// Lock the process-global registry, mapping a poisoned mutex to the typed
/// [`PaintRegistryError::Poisoned`] (never a panic — AGENTS.md §2.8).
fn lock() -> Result<std::sync::MutexGuard<'static, Registry>, PaintRegistryError> {
    PAINTS
        .get_or_init(|| Mutex::new(Registry::default()))
        .lock()
        .map_err(|_: PoisonError<_>| PaintRegistryError::Poisoned)
}

/// Allocate a fresh paint slot and return its packed [`PaintHandle`] (`jlong`, generation ≥ 1, never
/// the reserved `0`). Reuses a freed slot when available, else grows the slab. Returns
/// [`PaintRegistryError::Poisoned`] only on a poisoned mutex — never panics.
pub fn allocate() -> Result<PaintHandle, PaintRegistryError> {
    let mut reg = lock()?;
    if let Some(index) = reg.free.pop() {
        let slot = &mut reg.slots[index as usize];
        slot.state = Some(PaintState::default());
        return Ok(pack(index, slot.generation));
    }
    let index: u32 = reg
        .slots
        .len()
        .try_into()
        .map_err(|_| PaintRegistryError::OutOfRange)?;
    reg.slots.push(Slot {
        generation: 1,
        state: Some(PaintState::default()),
    });
    Ok(pack(index, 1))
}

/// Look up the [`PaintState`] for a `handle` and run `f` against it (mutable) under the registry
/// lock. Bounds-checks the slot index **and** verifies the handle's generation, so a stale/
/// out-of-range/fabricated handle returns `Err` and never dereferences out of bounds or aliases a
/// different paint. The reserved `0` handle fails the check (live generations are ≥ 1).
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

/// Free the slot a `handle` refers to, bumping its generation so any other handle to it (or this one,
/// reused later) is rejected as [`PaintRegistryError::StaleHandle`]. Validates the handle the same
/// way [`with_paint`] does, so freeing an already-freed/stale/fabricated handle returns `Err`.
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
    // Bump (saturating) so the freed handle and any copy become stale and can never alias a reuse.
    slot.generation = slot.generation.saturating_add(1);
    reg.free.push(index);
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    // 2026-06-05: same soundness contract as the sibling registries; fully in-harness (no VM).

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
            Ok(0),
            "the live handle's reused slot is fresh (default color 0)"
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
    fn pack_unpack_round_trips() {
        for &(index, generation) in &[(0u32, 1u32), (1, 1), (5, 42), (u32::MAX, u32::MAX), (3, 7)] {
            let handle = pack(index, generation);
            assert_eq!(unpack(handle), (index, generation));
        }
    }
}
