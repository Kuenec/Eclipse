//! Process-global generational-slab registry for Eclipse-owned `Resources.Theme` native handles.
//!
//! 2026-06-05: AOSP's `AssetManager.newTheme()` creates a native theme object and returns its `long`
//! handle (`Resources$Theme` stores it; `applyStyle`/`resolveAttributes`/`AssetManager.releaseTheme`
//! consume it). ATL backs this in C; Eclipse must NOT pull in GTK (AGENTS.md §5 Step 3.5), so a
//! theme's `long` handle is an **Eclipse-owned generational-slab index into this slab — NOT
//! `Box::into_raw`, NOT a raw pointer**, exactly the soundness pattern of
//! [`window_registry`](super::window_registry) / [`view_registry`](super::view_registry) /
//! [`xml_registry`](super::xml_registry). A stale/fabricated `jlong` from Java is a
//! bounds+generation-checked `Err`, never a wild dereference / use-after-free / UB.
//!
//! ## Handle layout
//! Identical to the sibling registries: a [`jlong`] packing a `u32` slot index (low 32 bits) + a
//! `u32` generation (high 32 bits). Generations start at 1, so a valid handle is never `0` — `0`
//! stays the reserved "no theme" / null sentinel.
//!
//! ## Scope (this increment)
//! [`ThemeState`] records the **non-GTK theme attribute set** the theme natives accumulate (resource
//! ids → resolved style data, applied via `applyStyle`). It is a minimal placeholder until a theme
//! native actually needs to read it back; recording themes soundly is what lets the View constructors
//! (`obtainStyledAttributes` → `getTheme` → `newTheme`) proceed past theme creation without GTK.

#![forbid(unsafe_code)]

use std::collections::HashMap;
use std::fmt;
use std::sync::{Mutex, OnceLock, PoisonError};

use jni::sys::jlong;

/// Process-global slab of [`ThemeState`], guarded by a [`Mutex`]. Initialized on first use.
static THEMES: OnceLock<Mutex<Registry>> = OnceLock::new();

/// A theme-registry handle as it travels across JNI: a `jlong` (`i64`) packing the slot index (low
/// 32 bits) and the slot's generation (high 32 bits). `0` is the reserved "no theme" / null sentinel.
pub type ThemeHandle = jlong;

/// Errors from the theme registry. Every fallible path returns one of these instead of panicking, so
/// a stale/out-of-range/fabricated `jlong` from Java can never cause UB or unwind across JNI.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ThemeRegistryError {
    /// The handle's slot index is outside the slab (fabricated handle, or the reserved `0`).
    OutOfRange,
    /// The slot exists but its generation does not match: the handle refers to a freed (and
    /// possibly reused) slot. Never aliases the new occupant.
    StaleHandle,
    /// The registry mutex was poisoned by a panic in another holder. Surfaced as an error (not a
    /// re-panic) so the JNI path stays panic-free (AGENTS.md §2.8).
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

/// One resolved theme attribute value: the `Res_value` type + data a theme attribute is set to.
///
/// 2026-06-05: a theme is, after style + parent-chain merge, a map of attribute resource id →
/// this value. `obtainStyledAttributes(int[])` (the no-parser path) looks each requested attr id up
/// here. `type_` is the `Res_value.dataType` (== `TypedValue.TYPE_*`); `data` is the raw 32-bit
/// payload (a referenced resource id for `TYPE_REFERENCE`, an attribute id for `TYPE_ATTRIBUTE`).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct ThemeAttr {
    /// `Res_value.dataType` (== `TypedValue.TYPE_*`).
    pub type_: u8,
    /// `Res_value.data` (raw 32-bit payload).
    pub data: u32,
    /// The package id of the `resources.arsc` table the style bag defining this value lives in
    /// (`0x01` = the framework table, anything else = the app table).
    ///
    /// 2026-07-01: for a `TYPE_STRING` value, `data` is an index into THAT table's GLOBAL value
    /// string pool — NOT the XmlBlock pool — so `framework`'s `TypedEntry` must carry the matching
    /// positive ARSC asset cookie (the challenge-boot "Resource is not a Drawable … \"<null>\""
    /// root cause was routing these to the XmlBlock pool via cookie `-1`).
    pub source_package: u8,
}

/// Per-theme state held in a registry slot: the non-GTK theme attribute set.
///
/// 2026-06-05: `attrs` is the theme's MERGED attribute map (attribute resource id → [`ThemeAttr`]),
/// built by `framework`'s `applyThemeStyle` from the applied style's bag + its parent chain in
/// `resources.arsc` (child overrides parent). `obtainStyledAttributes(int[])` resolves each requested
/// attribute from it. `styles` records the applied style resource ids in application order (kept for
/// diagnostics + `force` semantics; a freshly created theme has both empty).
#[derive(Debug, Default)]
pub struct ThemeState {
    /// Applied style resource ids (`Resources.Theme.applyStyle`), in application order.
    pub styles: Vec<i32>,
    /// The merged theme attribute map: attribute resource id → resolved value.
    pub attrs: HashMap<i32, ThemeAttr>,
}

/// A generational slot: the current generation plus the optional occupant.
struct Slot {
    generation: u32,
    state: Option<ThemeState>,
}

/// The slab + free list (same shape as the sibling registries).
#[derive(Default)]
struct Registry {
    slots: Vec<Slot>,
    free: Vec<u32>,
}

/// Pack a slot index + generation into a `jlong` handle (generation high, index low).
fn pack(index: u32, generation: u32) -> ThemeHandle {
    ((generation as u64) << 32 | index as u64) as i64
}

/// Unpack a `jlong` handle into (slot index, generation).
fn unpack(handle: ThemeHandle) -> (u32, u32) {
    let bits = handle as u64;
    ((bits & 0xFFFF_FFFF) as u32, (bits >> 32) as u32)
}

/// Lock the process-global registry, mapping a poisoned mutex to the typed
/// [`ThemeRegistryError::Poisoned`] (never a panic — AGENTS.md §2.8).
fn lock() -> Result<std::sync::MutexGuard<'static, Registry>, ThemeRegistryError> {
    THEMES
        .get_or_init(|| Mutex::new(Registry::default()))
        .lock()
        .map_err(|_: PoisonError<_>| ThemeRegistryError::Poisoned)
}

/// Allocate a fresh theme slot and return its packed [`ThemeHandle`] (`jlong`, generation ≥ 1, never
/// the reserved `0`). Reuses a freed slot when one is available, otherwise grows the slab. Returns
/// [`ThemeRegistryError::Poisoned`] only on a poisoned mutex — never panics.
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

/// Look up the [`ThemeState`] for a `handle` and run `f` against it (mutable) under the registry
/// lock. Bounds-checks the slot index **and** verifies the handle's generation, so a stale/
/// out-of-range/fabricated handle returns `Err` and never dereferences out of bounds or aliases a
/// different theme. The reserved `0` handle fails the check (live generations are ≥ 1).
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

/// Free the slot a `handle` refers to, bumping its generation so any other handle to it (or this one,
/// reused later) is rejected as [`ThemeRegistryError::StaleHandle`]. Validates the handle the same
/// way [`with_theme`] does, so freeing an already-freed/stale/fabricated handle returns `Err`.
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
        // 2026-06-05: the merged theme attribute map is what obtainStyledAttributes(int[]) reads;
        // copyTheme must copy it (not just `styles`). Guard the round-trip + the copy independence.
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

        // Read it back.
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

        // Copy into a dest theme (as copyTheme does), then mutate src and confirm dest is unaffected.
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
