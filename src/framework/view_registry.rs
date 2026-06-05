//! Process-global generational-slab registry for Eclipse-owned `android.view.View` handles.
//!
//! 2026-06-05: AOSP's `View`/`ViewGroup` hierarchy is fully native-handle-backed — each Java `View`
//! constructs a native peer and stores its `long` handle (`View.java` native constructor /
//! `mNativePtr`), and `ViewGroup`/`setContentView`/`Window.setContentView` wire those handles into a
//! tree. ATL backs these in C against GTK widgets (`GtkWidget*`). Eclipse must NOT pull in GTK (it
//! re-crowds the low_4gb window — AGENTS.md §5 Step 3.5), so a View's `long` handle is an
//! **Eclipse-owned generational-slab index into this slab — NOT `Box::into_raw`, NOT a raw pointer**,
//! exactly the soundness pattern of [`window_registry`](super::window_registry) and
//! [`xml_registry`](super::xml_registry). A stale/fabricated `jlong` from Java is a
//! bounds+generation-checked `Err`, never a wild dereference / use-after-free / UB.
//!
//! ## Handle layout
//! Identical to [`window_registry`](super::window_registry): a [`jlong`] packing a `u32` slot index
//! (low 32 bits) + a `u32` generation (high 32 bits). Generations start at 1, so a valid handle is
//! never `0` — `0` stays the reserved "no view" / null sentinel.
//!
//! ## Scope (this increment)
//! [`ViewState`] records the **non-GTK view-tree metadata** step 4/5 (`createMainActivity` →
//! `Activity.onCreate` → `setContentView` → the `View`/`ViewGroup` native cascade) sets: the view's
//! Java class name, an optional text/title, and its child view handles (the tree relationships). It
//! deliberately holds **no** GTK widget and performs **no** layout/measure/draw — the real
//! ash/Vulkan surface + rendering is the deferred big build (AGENTS.md §5). Recording the tree shape
//! soundly is what lets the lifecycle proceed past the View constructors without GTK.
//!
//! ## Thread-safety
//! The slab lives behind a [`Mutex`] inside a [`OnceLock`] (process-global, std-only, no new dep),
//! same as [`window_registry`](super::window_registry); a poisoned lock surfaces as the typed
//! [`ViewRegistryError::Poisoned`] rather than a panic on the JNI path (AGENTS.md §2.8).

#![forbid(unsafe_code)]

use std::fmt;
use std::sync::{Mutex, OnceLock, PoisonError};

use jni::sys::jlong;

/// Process-global slab of [`ViewState`], guarded by a [`Mutex`]. Initialized on first use.
static VIEWS: OnceLock<Mutex<Registry>> = OnceLock::new();

/// A view-registry handle as it travels across JNI: a `jlong` (`i64`) packing the slot index (low 32
/// bits) and the slot's generation (high 32 bits). `0` is the reserved "no view" / null sentinel.
pub type ViewHandle = jlong;

/// Errors from the view registry. Every fallible path returns one of these instead of panicking, so
/// a stale/out-of-range/fabricated `jlong` from Java can never cause UB or unwind across JNI.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ViewRegistryError {
    /// The handle's slot index is outside the slab (fabricated handle, or the reserved `0`).
    OutOfRange,
    /// The slot exists but its generation does not match: the handle refers to a freed (and
    /// possibly reused) slot. Never aliases the new occupant.
    StaleHandle,
    /// The registry mutex was poisoned by a panic in another holder. Surfaced as an error (not a
    /// re-panic) so the JNI path stays panic-free (AGENTS.md §2.8).
    Poisoned,
}

impl fmt::Display for ViewRegistryError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::OutOfRange => {
                f.write_str("view handle slot index is out of range (fabricated or null handle)")
            }
            Self::StaleHandle => {
                f.write_str("view handle refers to a freed slot (stale generation)")
            }
            Self::Poisoned => f.write_str("view registry mutex was poisoned"),
        }
    }
}

impl std::error::Error for ViewRegistryError {}

/// Per-view state held in a registry slot: the non-GTK view-tree metadata the View natives set.
///
/// 2026-06-05: minimal by design — records what is needed to track the view hierarchy's shape
/// without GTK, layout, or draw. The view's `class_name` (which `View`/`ViewGroup`/`FrameLayout`/
/// `TextView` subclass constructed it), an optional `text` (`TextView.setText`/`Window.setTitle`),
/// and `children` (the ordered child view handles a `ViewGroup.addView` wires in). The real winit
/// `Window`/Vulkan surface association is a later, deferred step.
#[derive(Debug, Default)]
pub struct ViewState {
    /// The Java class name that constructed this view (e.g. `android.widget.FrameLayout`), recorded
    /// by the view's native constructor for diagnostics + tree shape. Empty until a constructor sets it.
    pub class_name: String,
    /// Optional text/title set on this view (`TextView` text, or a container title). `None` until set.
    pub text: Option<String>,
    /// Ordered child view handles wired into this view by `ViewGroup.addView` (the tree edges).
    /// Stored as handles (not references) so the slab stays a flat `Vec` with no internal aliasing.
    pub children: Vec<ViewHandle>,
}

/// A generational slot: the current generation plus the optional occupant.
struct Slot {
    generation: u32,
    state: Option<ViewState>,
}

/// The slab + free list (same shape as [`window_registry`](super::window_registry)).
#[derive(Default)]
struct Registry {
    slots: Vec<Slot>,
    free: Vec<u32>,
}

/// Pack a slot index + generation into a `jlong` handle (generation high, index low).
fn pack(index: u32, generation: u32) -> ViewHandle {
    ((generation as u64) << 32 | index as u64) as i64
}

/// Unpack a `jlong` handle into (slot index, generation).
fn unpack(handle: ViewHandle) -> (u32, u32) {
    let bits = handle as u64;
    ((bits & 0xFFFF_FFFF) as u32, (bits >> 32) as u32)
}

/// Lock the process-global registry, mapping a poisoned mutex to the typed
/// [`ViewRegistryError::Poisoned`] (never a panic — AGENTS.md §2.8).
fn lock() -> Result<std::sync::MutexGuard<'static, Registry>, ViewRegistryError> {
    VIEWS
        .get_or_init(|| Mutex::new(Registry::default()))
        .lock()
        .map_err(|_: PoisonError<_>| ViewRegistryError::Poisoned)
}

/// Allocate a fresh view slot with the given `class_name` and return its packed [`ViewHandle`]
/// (`jlong`, generation ≥ 1, never the reserved `0`).
///
/// Reuses a freed slot when one is available (its generation was already bumped on [`free`]),
/// otherwise grows the slab. Returns [`ViewRegistryError::Poisoned`] only on a poisoned mutex —
/// never panics.
pub fn allocate(class_name: &str) -> Result<ViewHandle, ViewRegistryError> {
    let state = ViewState {
        class_name: class_name.to_owned(),
        text: None,
        children: Vec::new(),
    };
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
        .map_err(|_| ViewRegistryError::OutOfRange)?;
    reg.slots.push(Slot {
        generation: 1,
        state: Some(state),
    });
    Ok(pack(index, 1))
}

/// Look up the [`ViewState`] for a `handle` and run `f` against it (mutable, so a constructor/setter
/// can record metadata) under the registry lock.
///
/// Bounds-checks the slot index **and** verifies the handle's generation, so a stale/out-of-range/
/// fabricated handle returns `Err` and never dereferences out of bounds or aliases a different view.
/// The reserved `0` handle fails the check (live generations are ≥ 1).
pub fn with_view<R>(
    handle: ViewHandle,
    f: impl FnOnce(&mut ViewState) -> R,
) -> Result<R, ViewRegistryError> {
    let (index, generation) = unpack(handle);
    let mut reg = lock()?;
    let slot = reg
        .slots
        .get_mut(index as usize)
        .ok_or(ViewRegistryError::OutOfRange)?;
    if slot.generation != generation {
        return Err(ViewRegistryError::StaleHandle);
    }
    let state = slot.state.as_mut().ok_or(ViewRegistryError::StaleHandle)?;
    Ok(f(state))
}

/// Free the slot a `handle` refers to, bumping its generation so any other handle to it (or this one,
/// reused later) is rejected as [`ViewRegistryError::StaleHandle`]. Validates the handle the same way
/// [`with_view`] does, so freeing an already-freed/stale/fabricated handle returns `Err`.
pub fn free(handle: ViewHandle) -> Result<(), ViewRegistryError> {
    let (index, generation) = unpack(handle);
    let mut reg = lock()?;
    let slot = reg
        .slots
        .get_mut(index as usize)
        .ok_or(ViewRegistryError::OutOfRange)?;
    if slot.generation != generation || slot.state.is_none() {
        return Err(ViewRegistryError::StaleHandle);
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

    // 2026-06-05: these tests run fully in-harness — no VM, no display. They prove the same soundness
    // contract window_registry/xml_registry do, for the View slab: distinct handles, correct-slot
    // mutation, and (the key property) that a freed handle becomes Stale after the slot is reused with
    // a bumped generation, so a stale/fabricated `jlong` from Java is an `Err`, never UB / cross-talk.
    // The registry is process-global; tests are order-independent (own handles, no absolute indices).

    #[test]
    fn allocate_returns_distinct_nonzero_handles() {
        let a = allocate("android.widget.FrameLayout").expect("allocate a");
        let b = allocate("android.widget.TextView").expect("allocate b");
        assert_ne!(a, b, "distinct allocations must yield distinct handles");
        assert_ne!(a, 0, "a valid handle is never the reserved null 0");
        assert_ne!(b, 0, "a valid handle is never the reserved null 0");
        free(a).expect("free a");
        free(b).expect("free b");
    }

    #[test]
    fn with_view_mutates_the_right_slot() {
        let a = allocate("A").expect("allocate a");
        let b = allocate("B").expect("allocate b");
        with_view(a, |s| s.text = Some("text-a".to_owned())).expect("with_view a");
        with_view(b, |s| s.children.push(a)).expect("with_view b");
        let ta = with_view(a, |s| s.text.clone()).expect("read a");
        let cb = with_view(b, |s| s.children.clone()).expect("read b");
        assert_eq!(
            ta.as_deref(),
            Some("text-a"),
            "handle a addresses its own slot"
        );
        assert_eq!(cb, vec![a], "handle b addresses its own slot");
        free(a).expect("free a");
        free(b).expect("free b");
    }

    #[test]
    fn freed_handle_is_stale_and_does_not_alias_reused_slot() {
        // The key soundness property. Allocate, mutate, free, re-allocate; the freed handle must NOT
        // see the new occupant — it must be rejected as StaleHandle.
        let old = allocate("old").expect("allocate old");
        with_view(old, |s| s.text = Some("old".to_owned())).expect("write old");
        free(old).expect("free old");

        let new = allocate("new").expect("allocate new");
        assert_eq!(
            with_view(old, |s| s.text.clone()),
            Err(ViewRegistryError::StaleHandle),
            "a freed handle must be StaleHandle, never alias the reused slot"
        );
        // The live handle still works and is unaffected by the stale lookup.
        assert_eq!(
            with_view(new, |s| s.class_name.clone()),
            Ok("new".to_owned()),
            "the live handle must still address the reused slot"
        );
        free(new).expect("free new");
    }

    #[test]
    fn out_of_range_and_fabricated_handles_return_err_not_panic() {
        let fabricated = pack(u32::MAX, 1);
        assert_eq!(
            with_view(fabricated, |_| ()),
            Err(ViewRegistryError::OutOfRange),
            "a fabricated out-of-range index must be OutOfRange, never an out-of-bounds deref"
        );
        let null_lookup = with_view(0, |_| ());
        assert!(
            matches!(
                null_lookup,
                Err(ViewRegistryError::StaleHandle) | Err(ViewRegistryError::OutOfRange)
            ),
            "the reserved null handle 0 must be rejected, got {null_lookup:?}"
        );
        assert_eq!(free(fabricated), Err(ViewRegistryError::OutOfRange));
    }

    #[test]
    fn double_free_is_rejected() {
        let h = allocate("x").expect("allocate");
        free(h).expect("first free");
        // The same handle is now stale (generation bumped); a second free returns StaleHandle, not a
        // panic or free-list corruption.
        assert_eq!(free(h), Err(ViewRegistryError::StaleHandle));
    }

    #[test]
    fn pack_unpack_round_trips() {
        for &(index, generation) in &[(0u32, 1u32), (1, 1), (5, 42), (u32::MAX, u32::MAX), (3, 7)] {
            let handle = pack(index, generation);
            assert_eq!(unpack(handle), (index, generation));
        }
    }
}
