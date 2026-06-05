//! Process-global generational-slab registry for Eclipse-owned window handles.
//!
//! 2026-06-05: the `jlong` window handle Eclipse passes to the launcher lifecycle natives
//! (`createApplication`/`createMainActivity`) is an **Eclipse-owned registry index into this
//! generational slab — NOT `Box::into_raw`, NOT a raw pointer** (`docs/art-and-runtime.md`
//! "Non-GTK Window/Surface backing — design"). ATL casts that `jlong` to a `GtkWidget*`; Eclipse
//! owns both sides of the `jlong`, so it defines its own meaning. A registry index is chosen over
//! `Box::into_raw(WindowState)` for **soundness**: a stale or fabricated `jlong` (from a buggy
//! framework path or the engine) becomes a **bounds-checked + generation-checked lookup that
//! returns `Err`**, never a wild dereference / use-after-free / UB. `Box::into_raw` would turn any
//! wrong `jlong` into UB.
//!
//! ## Handle layout
//! A handle is an [`i64`] (`jlong`) packing a `u32` **slot index** in the low 32 bits and a `u32`
//! **generation** in the high 32 bits. Freeing a slot bumps its generation, so an old handle to a
//! later-reused slot fails the generation check ([`WindowRegistryError::StaleHandle`]) instead of
//! aliasing the new occupant. Generations start at 1, so a freshly allocated slot's handle always
//! has non-zero high bits — `0` can never be a valid handle, keeping `jlong = 0` reserved as the
//! null sentinel that steps 1–3 store but never look up.
//!
//! ## Thread-safety
//! The slab lives behind a [`Mutex`] inside a [`OnceLock`] (process-global, std-only, no new dep).
//! [`WindowState`] is intended to be touched only on the VM/winit main thread; the `Mutex` makes
//! the registry itself safe to call from anywhere, and a poisoned lock is surfaced as the typed
//! [`WindowRegistryError::Poisoned`] rather than a panic on the JNI path (AGENTS.md §2.8).
//!
//! ## Scope (this increment)
//! [`WindowState`] holds **only** what is needed before a winit `Window` exists at allocation time
//! (`createApplication` runs after boot but **before** the winit window is created). It is a
//! minimal placeholder: a `title` and a documented `jobject` slot. It deliberately holds **no**
//! winit `Window` — none exists yet — which avoids the aliasing hazard the design flagged (the live
//! window is reached through the event-loop callback, not the registry). Associating the real winit
//! window + the dereferencing Window natives is a later, dev-host-gated step.

#![forbid(unsafe_code)]

use std::fmt;
use std::sync::{Mutex, OnceLock, PoisonError};

/// Process-global slab of [`WindowState`], guarded by a [`Mutex`]. Initialized on first use.
static WINDOWS: OnceLock<Mutex<Registry>> = OnceLock::new();

/// A window-registry handle as it travels across JNI: a `jlong` (`i64`) packing the slot index
/// (low 32 bits) and the slot's generation (high 32 bits). See the module docs for the layout and
/// why `0` is reserved.
pub type WindowHandle = i64;

/// Errors from the window registry. Every fallible path returns one of these instead of panicking,
/// so a stale/out-of-range/fabricated `jlong` from Java can never cause UB or unwind across JNI.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum WindowRegistryError {
    /// The handle's slot index is outside the slab (fabricated handle, or the reserved `0`/null).
    OutOfRange,
    /// The slot exists but its current generation does not match the handle's: the handle refers to
    /// a freed (and possibly reused) slot. The key soundness rejection — never aliases the new
    /// occupant.
    StaleHandle,
    /// The registry mutex was poisoned by a panic in another holder. Surfaced as an error (not a
    /// re-panic) so the JNI path stays panic-free (AGENTS.md §2.8).
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

/// Per-window state held in a registry slot.
///
/// 2026-06-05: minimal by design — `createApplication` allocates a slot **before** the winit window
/// exists, so this holds NO winit `Window` (that would alias the event loop's `&mut` access and
/// there is nothing to store yet). It carries only metadata the early lifecycle natives set:
/// the window `title` (default empty) and a placeholder `jobject` slot for the Java `Window`
/// back-reference a later increment will populate (kept as a documented `()`-typed `Option` so no
/// `jni` `GlobalRef` lifetime is taken before the design for it is settled).
#[derive(Debug, Default)]
pub struct WindowState {
    /// Window title (`Window.set_title` stores this; applied to the real winit window when associated).
    pub title: String,
    /// 2026-06-05: whether the Java `android.view.Window` back-reference has been set (`Window.set_jobject`).
    /// The real `GlobalRef`/`WeakGlobalRef` for input/lifecycle dispatch is a later increment; held as
    /// `Option<()>` so this increment takes no `GlobalRef`/attach lifetime — the unit just records
    /// "set or not". Replaced with the real ref type when input dispatch is wired.
    pub jobject: Option<()>,
    /// 2026-06-05: the content-root view handle (`Window.set_widget_as_root`), an index into the
    /// [`view_registry`](super::view_registry) slab — the root of this window's view tree, or `None`
    /// until set. Stored as a handle (not a reference), so no cross-registry aliasing.
    pub root_view: Option<i64>,
}

/// A generational slot: the current generation plus the optional occupant. A live slot is
/// `Some(state)`; a freed slot is `None` but keeps (and on free, bumped) its generation so stale
/// handles to it are rejected.
struct Slot {
    /// Current generation. Starts at 1 (so the first handle has non-zero high bits — `0` stays the
    /// reserved null), bumped on every [`free`]. A handle is valid only if its generation equals
    /// this.
    generation: u32,
    /// The occupant, or `None` if the slot is free.
    state: Option<WindowState>,
}

/// The slab + free list. `slots[i].generation` is authoritative for slot `i`; `free` holds the
/// indices of currently-free slots for O(1) reuse.
#[derive(Default)]
struct Registry {
    slots: Vec<Slot>,
    free: Vec<u32>,
}

/// Pack a slot index + generation into a `jlong` handle (generation high, index low).
fn pack(index: u32, generation: u32) -> WindowHandle {
    ((generation as u64) << 32 | index as u64) as i64
}

/// Unpack a `jlong` handle into (slot index, generation).
fn unpack(handle: WindowHandle) -> (u32, u32) {
    let bits = handle as u64;
    ((bits & 0xFFFF_FFFF) as u32, (bits >> 32) as u32)
}

/// Lock the process-global registry, mapping a poisoned mutex to the typed
/// [`WindowRegistryError::Poisoned`] (never a panic — AGENTS.md §2.8).
fn lock() -> Result<std::sync::MutexGuard<'static, Registry>, WindowRegistryError> {
    WINDOWS
        .get_or_init(|| Mutex::new(Registry::default()))
        .lock()
        .map_err(|_: PoisonError<_>| WindowRegistryError::Poisoned)
}

/// Allocate a fresh registry slot and return its packed [`WindowHandle`] (`jlong`).
///
/// Reuses a freed slot when one is available (its generation was already bumped on [`free`]),
/// otherwise grows the slab. The returned handle's generation is ≥ 1, so it is never `0` (the
/// reserved null). Returns [`WindowRegistryError::Poisoned`] only if the registry mutex was
/// poisoned — never panics.
pub fn allocate() -> Result<WindowHandle, WindowRegistryError> {
    let mut reg = lock()?;
    if let Some(index) = reg.free.pop() {
        let slot = &mut reg.slots[index as usize];
        slot.state = Some(WindowState::default());
        return Ok(pack(index, slot.generation));
    }
    let index: u32 = reg
        .slots
        .len()
        .try_into()
        .map_err(|_| WindowRegistryError::OutOfRange)?;
    reg.slots.push(Slot {
        generation: 1,
        state: Some(WindowState::default()),
    });
    Ok(pack(index, 1))
}

/// Look up the [`WindowState`] for a `handle` and run `f` against it under the registry lock.
///
/// Bounds-checks the slot index **and** verifies the handle's generation matches the slot's current
/// generation, so a stale (freed/reused), out-of-range, or fabricated handle returns `Err` and
/// never dereferences out of bounds or aliases a different window. The reserved `0`/null handle
/// fails the generation check (slot 0's live generation is ≥ 1) and returns
/// [`WindowRegistryError::StaleHandle`] for an existing slot 0, or
/// [`WindowRegistryError::OutOfRange`] when slot 0 does not exist.
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

/// Free the slot a `handle` refers to, bumping its generation so any other handle to it (or this
/// one, reused later) is rejected as [`WindowRegistryError::StaleHandle`].
///
/// Validates the handle the same way [`with_window`] does before freeing, so freeing an
/// already-freed, stale, or fabricated handle returns `Err` rather than corrupting the free list.
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
    // Bump the generation so the just-freed handle (and any copy of it) is now stale. Saturating so
    // a (practically unreachable) 2^32 reuse cycle never wraps a generation back onto a live value.
    slot.generation = slot.generation.saturating_add(1);
    reg.free.push(index);
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    // 2026-06-05: these tests run fully in-harness — no VM, no display. They prove the soundness
    // contract of the owned-handle registry: distinct handles, correct-slot mutation, and (the key
    // property) that a freed handle becomes Stale after the slot is reused with a bumped generation,
    // so a stale/fabricated `jlong` from Java is an `Err`, never UB / cross-talk.
    //
    // The registry is process-global, so tests share one slab. They are written to be
    // order-independent: each allocates its own handles and never asserts absolute slot indices.

    #[test]
    fn pack_unpack_round_trips() {
        for &(index, generation) in &[(0u32, 1u32), (1, 1), (5, 42), (u32::MAX, u32::MAX), (3, 7)] {
            let handle = pack(index, generation);
            assert_eq!(unpack(handle), (index, generation));
        }
    }

    #[test]
    fn allocate_returns_distinct_nonzero_handles() {
        let a = allocate().expect("allocate a");
        let b = allocate().expect("allocate b");
        assert_ne!(a, b, "distinct allocations must yield distinct handles");
        // Generations start at 1, so a valid handle's high bits are non-zero ⇒ never the null 0.
        assert_ne!(a, 0, "a valid handle is never the reserved null 0");
        assert_ne!(b, 0, "a valid handle is never the reserved null 0");
        free(a).expect("free a");
        free(b).expect("free b");
    }

    #[test]
    fn with_window_mutates_the_right_slot() {
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
        // The key soundness property. Allocate, write, free, re-allocate; the freed handle must NOT
        // see the new occupant — it must be rejected as StaleHandle.
        let old = allocate().expect("allocate old");
        with_window(old, |s| s.title = "old".to_owned()).expect("write old");
        free(old).expect("free old");

        // Reuse: the registry pops the just-freed slot but with a bumped generation.
        let new = allocate().expect("allocate new");
        with_window(new, |s| s.title = "new".to_owned()).expect("write new");

        // The OLD handle must now be Stale — not silently reading/writing the NEW window's state.
        assert_eq!(
            with_window(old, |s| s.title.clone()),
            Err(WindowRegistryError::StaleHandle),
            "a freed handle must be StaleHandle, never alias the reused slot"
        );
        // The new handle still works and is unaffected by the stale lookup.
        assert_eq!(
            with_window(new, |s| s.title.clone()),
            Ok("new".to_owned()),
            "the live handle must still address the reused slot"
        );
        free(new).expect("free new");
    }

    #[test]
    fn out_of_range_and_fabricated_handles_return_err_not_panic() {
        // A handle whose slot index is far past the slab is OutOfRange (bounds-checked, no panic).
        let fabricated = pack(u32::MAX, 1);
        assert_eq!(
            with_window(fabricated, |_| ()),
            Err(WindowRegistryError::OutOfRange),
            "a fabricated out-of-range index must be OutOfRange, never an out-of-bounds deref"
        );
        // The reserved null handle (0) — slot index 0, generation 0 — is never a live generation
        // (live generations are ≥ 1), so it is rejected: Stale if slot 0 exists, OutOfRange if not.
        // Either way it is an Err and never aliases a real window.
        let null_lookup = with_window(0, |_| ());
        assert!(
            matches!(
                null_lookup,
                Err(WindowRegistryError::StaleHandle) | Err(WindowRegistryError::OutOfRange)
            ),
            "the reserved null handle 0 must be rejected, got {null_lookup:?}"
        );
        // free of a fabricated handle is likewise an error, not a panic or free-list corruption.
        assert_eq!(free(fabricated), Err(WindowRegistryError::OutOfRange));
    }

    #[test]
    fn double_free_is_rejected() {
        let h = allocate().expect("allocate");
        free(h).expect("first free");
        // The same handle is now stale (generation bumped); a second free must not corrupt the
        // free list or panic — it returns StaleHandle.
        assert_eq!(free(h), Err(WindowRegistryError::StaleHandle));
    }
}
