//! Process-global generational-slab registry for Eclipse-owned open asset streams.
//!
//! 2026-06-11: AOSP's `AssetManager.openAsset(fileName, accessMode)` opens an asset file and returns
//! a `long` handle that `readAsset`/`seekAsset`/`getAssetLength`/`getAssetRemainingLength`/
//! `destroyAsset` then operate on (the back end of `AssetManager.open(...)` → an `AssetInputStream`).
//! Eclipse's own (non-GTK) backing reads `assets/<fileName>` through its own `src/apk` reader and
//! returns a **real, non-zero** handle whose meaning is an entry in THIS registry — an **Eclipse-owned
//! generational-slab index, NOT `Box::into_raw`, NOT a raw pointer** — exactly the soundness pattern of
//! [`xml_registry`](super::xml_registry)/[`window_registry`](super::window_registry). A stale/
//! fabricated `jlong` from Java is a bounds+generation-checked `Err`, never a wild dereference / UB.
//!
//! ## Handle layout
//! Identical to [`xml_registry`](super::xml_registry): a [`jlong`] packing a `u32` slot index (low 32
//! bits) + a `u32` generation (high 32 bits). Generations start at 1, so a valid handle is never `0`
//! — `0` stays the reserved "no asset" sentinel (`openAsset` returns `0` only on a genuine failure,
//! which the framework turns into `FileNotFoundException`).
//!
//! ## What a stream holds
//! An [`AssetStream`] owns the asset's bytes (`Box<[u8]>`) plus a read cursor. The cursor supports the
//! sequential read + `lseek`-style seek the `AssetInputStream` needs. The whole asset is read into
//! memory up front (Android assets are small config/resource files; this avoids holding an open zip
//! reader across the JNI boundary — mirrors the XML-block path).
//!
//! ## Thread-safety
//! The slab lives behind a [`Mutex`] inside a [`OnceLock`] (process-global, std-only, no new dep),
//! same as [`xml_registry`](super::xml_registry); a poisoned lock surfaces as the typed
//! [`AssetRegistryError::Poisoned`] rather than a panic on the JNI path (AGENTS.md §2.8).

#![forbid(unsafe_code)]

use std::fmt;
use std::sync::{Mutex, OnceLock, PoisonError};

/// Process-global slab of [`AssetStream`], guarded by a [`Mutex`]. Initialized on first use.
static STREAMS: OnceLock<Mutex<Registry>> = OnceLock::new();

/// An asset-stream handle as it travels across JNI: a `jlong` (`i64`) packing the slot index (low 32
/// bits) and the slot's generation (high 32 bits). `0` is the reserved "no asset" sentinel.
pub type AssetHandle = i64;

/// Errors from the asset-stream registry. Every fallible path returns one of these instead of
/// panicking, so a stale/out-of-range/fabricated `jlong` from Java can never cause UB or unwind
/// across JNI.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum AssetRegistryError {
    /// The handle's slot index is outside the slab (fabricated handle, or the reserved `0`).
    OutOfRange,
    /// The slot exists but its generation does not match: the handle refers to a freed (and possibly
    /// reused) slot. Never aliases the new occupant.
    StaleHandle,
    /// The registry mutex was poisoned by a panic in another holder. Surfaced as an error (not a
    /// re-panic) so the JNI path stays panic-free (AGENTS.md §2.8).
    Poisoned,
}

impl fmt::Display for AssetRegistryError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::OutOfRange => {
                f.write_str("asset handle slot index is out of range (fabricated or null handle)")
            }
            Self::StaleHandle => {
                f.write_str("asset handle refers to a freed slot (stale generation)")
            }
            Self::Poisoned => f.write_str("asset registry mutex was poisoned"),
        }
    }
}

impl std::error::Error for AssetRegistryError {}

/// An open asset: its bytes plus a read cursor (the position of the next byte [`read`](AssetStream::read)
/// returns).
#[derive(Debug)]
pub struct AssetStream {
    data: Box<[u8]>,
    pos: usize,
}

impl AssetStream {
    /// The asset's total length in bytes (`getAssetLength`).
    #[must_use]
    pub fn len(&self) -> usize {
        self.data.len()
    }

    /// Whether the asset is empty (clippy companion to [`len`](AssetStream::len)).
    #[must_use]
    pub fn is_empty(&self) -> bool {
        self.data.is_empty()
    }

    /// Bytes from the cursor to the end (`getAssetRemainingLength`).
    #[must_use]
    pub fn remaining(&self) -> usize {
        self.data.len().saturating_sub(self.pos)
    }

    /// Copy up to `out.len()` bytes from the cursor into `out`, advance the cursor, and return how
    /// many were copied (`0` at EOF). Never reads out of bounds (bounded by [`remaining`](AssetStream::remaining)).
    pub fn read(&mut self, out: &mut [u8]) -> usize {
        let n = out.len().min(self.remaining());
        out[..n].copy_from_slice(&self.data[self.pos..self.pos + n]);
        self.pos = self.pos.saturating_add(n);
        n
    }

    /// `lseek`-style seek: `whence` 0 = SET, 1 = CUR, 2 = END. Returns the new cursor position, or
    /// `-1` for an unknown whence or an out-of-range result (never moves the cursor on error).
    pub fn seek(&mut self, offset: i64, whence: i32) -> i64 {
        let base: i64 = match whence {
            0 => 0,
            1 => self.pos as i64,
            2 => self.data.len() as i64,
            _ => return -1,
        };
        let Some(new) = base.checked_add(offset) else {
            return -1;
        };
        if new < 0 || (new as u64) > self.data.len() as u64 {
            return -1;
        }
        self.pos = new as usize;
        new
    }
}

/// A generational slot: the current generation plus the optional occupant.
struct Slot {
    generation: u32,
    stream: Option<AssetStream>,
}

/// The slab + free list (same shape as [`xml_registry`](super::xml_registry)).
#[derive(Default)]
struct Registry {
    slots: Vec<Slot>,
    free: Vec<u32>,
}

/// Pack a slot index + generation into a `jlong` handle (generation high, index low).
fn pack(index: u32, generation: u32) -> AssetHandle {
    ((generation as u64) << 32 | index as u64) as i64
}

/// Unpack a `jlong` handle into (slot index, generation).
fn unpack(handle: AssetHandle) -> (u32, u32) {
    let bits = handle as u64;
    ((bits & 0xFFFF_FFFF) as u32, (bits >> 32) as u32)
}

/// Lock the process-global registry, mapping a poisoned mutex to the typed
/// [`AssetRegistryError::Poisoned`] (never a panic — AGENTS.md §2.8).
fn lock() -> Result<std::sync::MutexGuard<'static, Registry>, AssetRegistryError> {
    STREAMS
        .get_or_init(|| Mutex::new(Registry::default()))
        .lock()
        .map_err(|_: PoisonError<_>| AssetRegistryError::Poisoned)
}

/// Store an asset's bytes as a new open stream (cursor at 0) and return its packed handle (≥ 1
/// generation, so never the reserved `0`). Returns [`AssetRegistryError::Poisoned`] only on a poisoned
/// mutex — never panics.
pub fn store(data: Vec<u8>) -> Result<AssetHandle, AssetRegistryError> {
    let stream = AssetStream {
        data: data.into_boxed_slice(),
        pos: 0,
    };
    let mut reg = lock()?;
    if let Some(index) = reg.free.pop() {
        let slot = &mut reg.slots[index as usize];
        slot.stream = Some(stream);
        return Ok(pack(index, slot.generation));
    }
    let index: u32 = reg
        .slots
        .len()
        .try_into()
        .map_err(|_| AssetRegistryError::OutOfRange)?;
    reg.slots.push(Slot {
        generation: 1,
        stream: Some(stream),
    });
    Ok(pack(index, 1))
}

/// Look up the [`AssetStream`] for `handle` and run `f` against it (mutable, so the cursor can
/// advance) under the registry lock.
///
/// Bounds-checks the slot index **and** verifies the handle's generation, so a stale/out-of-range/
/// fabricated handle returns `Err` and never dereferences out of bounds or aliases a different stream.
/// The reserved `0` handle fails the check (live generations are ≥ 1).
pub fn with_stream<R>(
    handle: AssetHandle,
    f: impl FnOnce(&mut AssetStream) -> R,
) -> Result<R, AssetRegistryError> {
    let (index, generation) = unpack(handle);
    let mut reg = lock()?;
    let slot = reg
        .slots
        .get_mut(index as usize)
        .ok_or(AssetRegistryError::OutOfRange)?;
    if slot.generation != generation {
        return Err(AssetRegistryError::StaleHandle);
    }
    let stream = slot
        .stream
        .as_mut()
        .ok_or(AssetRegistryError::StaleHandle)?;
    Ok(f(stream))
}

/// Free the slot `handle` refers to, bumping its generation so any other handle to it (or this one,
/// reused later) is rejected as [`AssetRegistryError::StaleHandle`]. Validates the handle the same way
/// [`with_stream`] does, so freeing an already-freed/stale/fabricated handle returns `Err`.
pub fn free(handle: AssetHandle) -> Result<(), AssetRegistryError> {
    let (index, generation) = unpack(handle);
    let mut reg = lock()?;
    let slot = reg
        .slots
        .get_mut(index as usize)
        .ok_or(AssetRegistryError::OutOfRange)?;
    if slot.generation != generation || slot.stream.is_none() {
        return Err(AssetRegistryError::StaleHandle);
    }
    slot.stream = None;
    // Bump (saturating) so the freed handle and any copy become stale and can never alias a reuse.
    slot.generation = slot.generation.saturating_add(1);
    reg.free.push(index);
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn store_returns_distinct_nonzero_handles_and_reads_sequentially() {
        let a = store(vec![1, 2, 3, 4, 5]).expect("store a");
        let b = store(vec![9]).expect("store b");
        assert_ne!(a, b);
        assert_ne!(a, 0, "a valid handle is never the reserved null 0");

        // Length + remaining.
        assert_eq!(with_stream(a, |s| s.len()), Ok(5));
        assert_eq!(with_stream(a, |s| s.remaining()), Ok(5));

        // Sequential read advances the cursor and is bounded at EOF.
        let mut buf = [0u8; 3];
        assert_eq!(with_stream(a, |s| s.read(&mut buf)), Ok(3));
        assert_eq!(buf, [1, 2, 3]);
        assert_eq!(with_stream(a, |s| s.remaining()), Ok(2));
        let mut buf2 = [0u8; 8];
        assert_eq!(with_stream(a, |s| s.read(&mut buf2)), Ok(2)); // only 2 left
        assert_eq!(&buf2[..2], &[4, 5]);
        assert_eq!(with_stream(a, |s| s.read(&mut buf2)), Ok(0)); // EOF

        free(a).expect("free a");
        free(b).expect("free b");
    }

    #[test]
    fn seek_set_cur_end_and_rejects_out_of_range() {
        let h = store(vec![0, 1, 2, 3, 4, 5, 6, 7, 8, 9]).expect("store");
        assert_eq!(with_stream(h, |s| s.seek(3, 0)), Ok(3)); // SET 3
        assert_eq!(with_stream(h, |s| s.remaining()), Ok(7));
        assert_eq!(with_stream(h, |s| s.seek(2, 1)), Ok(5)); // CUR +2 → 5
        assert_eq!(with_stream(h, |s| s.seek(0, 2)), Ok(10)); // END → len
        assert_eq!(with_stream(h, |s| s.seek(-1, 0)), Ok(-1)); // negative → error, no move
        assert_eq!(with_stream(h, |s| s.seek(1, 2)), Ok(-1)); // past end → error
        assert_eq!(with_stream(h, |s| s.seek(0, 99)), Ok(-1)); // unknown whence
                                                               // The cursor is unchanged by the rejected seeks (still at END = 10).
        assert_eq!(with_stream(h, |s| s.remaining()), Ok(0));
        free(h).expect("free");
    }

    #[test]
    fn freed_handle_is_stale_and_does_not_alias_reused_slot() {
        let old = store(vec![7, 7, 7]).expect("store old");
        free(old).expect("free old");
        let new = store(vec![0]).expect("store new");
        assert_eq!(
            with_stream(old, |s| s.len()),
            Err(AssetRegistryError::StaleHandle),
            "a freed handle must be StaleHandle, never alias the reused slot"
        );
        assert_eq!(with_stream(new, |s| s.len()), Ok(1));
        free(new).expect("free new");
    }

    #[test]
    fn out_of_range_fabricated_and_double_free_return_err_not_panic() {
        let fabricated = pack(u32::MAX, 1);
        assert_eq!(
            with_stream(fabricated, |_| ()),
            Err(AssetRegistryError::OutOfRange)
        );
        assert!(matches!(
            with_stream(0, |_| ()),
            Err(AssetRegistryError::StaleHandle) | Err(AssetRegistryError::OutOfRange)
        ));
        let h = store(vec![1]).expect("store");
        free(h).expect("first free");
        assert_eq!(free(h), Err(AssetRegistryError::StaleHandle));
    }
}
