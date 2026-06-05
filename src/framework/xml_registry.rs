//! Process-global generational-slab registry for Eclipse-owned parsed XML-asset blocks.
//!
//! 2026-06-05: AOSP's `AssetManager.openXmlBlockAsset(fileName)` opens a binary-XML asset, parses
//! it into a native `ResXMLTree`, and returns a `long` handle the framework wraps as an `XmlBlock`;
//! `XmlBlock.newParser()` then walks it as an `XmlResourceParser`. Eclipse's own (non-GTK) backing
//! must return a **real, non-zero** handle to a parsed document (the no-op `0` stub made
//! `openXmlBlockAsset` throw `FileNotFoundException`). This registry is that handle's meaning:
//! an **Eclipse-owned generational-slab index — NOT `Box::into_raw`, NOT a raw pointer** — exactly
//! the soundness pattern of [`window_registry`](super::window_registry). A stale/fabricated `jlong`
//! from Java is a bounds+generation-checked `Err`, never a wild dereference / use-after-free / UB.
//!
//! ## Handle layout
//! Identical to [`window_registry`](super::window_registry): a [`jlong`] packing a `u32` slot index
//! (low 32 bits) + a `u32` generation (high 32 bits). Generations start at 1, so a valid handle is
//! never `0` — `0` stays the reserved "no asset" sentinel (`openXmlAssetNative` returns `0` only on
//! a genuine failure, which the framework turns into `FileNotFoundException`).
//!
//! ## What a block holds
//! A [`XmlBlock`] owns the parsed [`XmlDocument`](crate::apk::axml::XmlDocument) (the immutable event
//! tree) plus a **parser cursor** (the current event position). AOSP separates the immutable
//! `XmlBlock` from a per-`newParser` cursor; for the launcher's single-pass manifest walk one cursor
//! per block is sufficient and avoids a second registry. If the framework opens multiple parsers on
//! one block this can split into block + parser registries — deferred until a run shows it is needed.
//!
//! ## Thread-safety
//! The slab lives behind a [`Mutex`] inside a [`OnceLock`] (process-global, std-only, no new dep),
//! same as [`window_registry`](super::window_registry); a poisoned lock surfaces as the typed
//! [`XmlRegistryError::Poisoned`] rather than a panic on the JNI path (AGENTS.md §2.8).

#![forbid(unsafe_code)]

use std::fmt;
use std::sync::{Mutex, OnceLock, PoisonError};

use crate::apk::axml::{XmlDocument, XmlEventKind};

/// Process-global slab of [`XmlBlock`], guarded by a [`Mutex`]. Initialized on first use.
static BLOCKS: OnceLock<Mutex<Registry>> = OnceLock::new();

/// An XML-block handle as it travels across JNI: a `jlong` (`i64`) packing the slot index (low 32
/// bits) and the slot's generation (high 32 bits). `0` is the reserved "no asset" sentinel.
pub type XmlBlockHandle = i64;

/// Errors from the XML-block registry. Every fallible path returns one of these instead of
/// panicking, so a stale/out-of-range/fabricated `jlong` from Java can never cause UB or unwind
/// across JNI.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum XmlRegistryError {
    /// The handle's slot index is outside the slab (fabricated handle, or the reserved `0`).
    OutOfRange,
    /// The slot exists but its generation does not match: the handle refers to a freed (and
    /// possibly reused) slot. Never aliases the new occupant.
    StaleHandle,
    /// The registry mutex was poisoned by a panic in another holder. Surfaced as an error (not a
    /// re-panic) so the JNI path stays panic-free (AGENTS.md §2.8).
    Poisoned,
}

impl fmt::Display for XmlRegistryError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::OutOfRange => f.write_str(
                "xml-block handle slot index is out of range (fabricated or null handle)",
            ),
            Self::StaleHandle => {
                f.write_str("xml-block handle refers to a freed slot (stale generation)")
            }
            Self::Poisoned => f.write_str("xml-block registry mutex was poisoned"),
        }
    }
}

impl std::error::Error for XmlRegistryError {}

/// A parsed XML asset block plus its parser cursor.
///
/// `doc` is the immutable parsed event tree; `cursor` is the index of the **next** event
/// [`next_event`](XmlBlock::next_event) will return (AOSP's `XmlBlock.Parser` advances one event per
/// `next()`). `cursor` starts at 0 (before the first event; the parser reports `START_DOCUMENT`
/// until the first `next()`). After a `next_event`, [`current_event`](XmlBlock::current_event)
/// reports the event just returned — the one the framework's `nativeGet*` accessors query.
#[derive(Debug)]
pub struct XmlBlock {
    /// The parsed binary-XML document (events + element/text/namespace tables).
    pub doc: XmlDocument,
    /// Index into `doc.events` of the next event to yield (the parser cursor).
    pub cursor: usize,
    /// Index into `doc.events` of the event most recently returned by [`next_event`], or `None`
    /// before the first `next_event` (the `START_DOCUMENT` state). The `nativeGet*` accessors read
    /// the element/text this points at.
    pub current: Option<usize>,
}

impl XmlBlock {
    /// The event most recently returned by [`next_event`] (the one the `nativeGet*` accessors query),
    /// or `None` in the pre-first-`next` `START_DOCUMENT` state / past the end.
    pub fn current_event(&self) -> Option<XmlEventKind> {
        self.current.and_then(|i| self.doc.events.get(i).copied())
    }

    /// Advance the cursor one step and return the event now under it, or `None` past the end.
    /// Records the returned event's index as [`current`](XmlBlock::current) so the `nativeGet*`
    /// accessors query the just-returned node.
    pub fn next_event(&mut self) -> Option<XmlEventKind> {
        if self.cursor < self.doc.events.len() {
            let ev = self.doc.events[self.cursor];
            self.current = Some(self.cursor);
            self.cursor = self.cursor.saturating_add(1);
            Some(ev)
        } else {
            self.current = None;
            None
        }
    }

    /// The [`XmlElement`](crate::apk::axml::XmlElement) of the current event when it is a
    /// start/end tag, else `None` (a text node or pre-first-`next` state). Used by the
    /// `nativeGetName`/`nativeGetNamespace`/`nativeGetAttribute*` accessors.
    pub fn current_element(&self) -> Option<&crate::apk::axml::XmlElement> {
        match self.current_event()? {
            XmlEventKind::StartTag(i) | XmlEventKind::EndTag(i) => self.doc.elements.get(i),
            _ => None,
        }
    }

    /// The [`XmlText`](crate::apk::axml::XmlText) of the current event when it is a text node, else
    /// `None`. Used by the `nativeGetText` accessor.
    pub fn current_text(&self) -> Option<&crate::apk::axml::XmlText> {
        match self.current_event()? {
            XmlEventKind::Text(i) => self.doc.texts.get(i),
            _ => None,
        }
    }

    /// The block's pooled string at `index`, or `None` if out of range. Backs
    /// `XmlBlock.nativeGetPooledString(state, idx)` — reached when a `TYPE_STRING` styled attribute's
    /// `TypedArray` cookie marks it XmlBlock-owned and `TypedArray.getString` calls
    /// `mXml.getPooledString(data)` with `data` = the source string-pool index.
    pub fn pooled_string(&self, index: usize) -> Option<&str> {
        self.doc.strings.get(index).map(String::as_str)
    }
}

/// A generational slot: the current generation plus the optional occupant.
struct Slot {
    generation: u32,
    block: Option<XmlBlock>,
}

/// The slab + free list (same shape as [`window_registry`](super::window_registry)).
#[derive(Default)]
struct Registry {
    slots: Vec<Slot>,
    free: Vec<u32>,
}

/// Pack a slot index + generation into a `jlong` handle (generation high, index low).
fn pack(index: u32, generation: u32) -> XmlBlockHandle {
    ((generation as u64) << 32 | index as u64) as i64
}

/// Unpack a `jlong` handle into (slot index, generation).
fn unpack(handle: XmlBlockHandle) -> (u32, u32) {
    let bits = handle as u64;
    ((bits & 0xFFFF_FFFF) as u32, (bits >> 32) as u32)
}

/// Lock the process-global registry, mapping a poisoned mutex to the typed
/// [`XmlRegistryError::Poisoned`] (never a panic — AGENTS.md §2.8).
fn lock() -> Result<std::sync::MutexGuard<'static, Registry>, XmlRegistryError> {
    BLOCKS
        .get_or_init(|| Mutex::new(Registry::default()))
        .lock()
        .map_err(|_: PoisonError<_>| XmlRegistryError::Poisoned)
}

/// Store a parsed [`XmlDocument`] as a new block (cursor at 0) and return its packed handle (≥ 1
/// generation, so never the reserved `0`). Returns [`XmlRegistryError::Poisoned`] only on a
/// poisoned mutex — never panics.
pub fn store(doc: XmlDocument) -> Result<XmlBlockHandle, XmlRegistryError> {
    let block = XmlBlock {
        doc,
        cursor: 0,
        current: None,
    };
    let mut reg = lock()?;
    if let Some(index) = reg.free.pop() {
        let slot = &mut reg.slots[index as usize];
        slot.block = Some(block);
        return Ok(pack(index, slot.generation));
    }
    let index: u32 = reg
        .slots
        .len()
        .try_into()
        .map_err(|_| XmlRegistryError::OutOfRange)?;
    reg.slots.push(Slot {
        generation: 1,
        block: Some(block),
    });
    Ok(pack(index, 1))
}

/// Look up the [`XmlBlock`] for a `handle` and run `f` against it (mutable, so the parser cursor can
/// advance) under the registry lock.
///
/// Bounds-checks the slot index **and** verifies the handle's generation, so a stale/out-of-range/
/// fabricated handle returns `Err` and never dereferences out of bounds or aliases a different
/// block. The reserved `0` handle fails the check (live generations are ≥ 1).
pub fn with_block<R>(
    handle: XmlBlockHandle,
    f: impl FnOnce(&mut XmlBlock) -> R,
) -> Result<R, XmlRegistryError> {
    let (index, generation) = unpack(handle);
    let mut reg = lock()?;
    let slot = reg
        .slots
        .get_mut(index as usize)
        .ok_or(XmlRegistryError::OutOfRange)?;
    if slot.generation != generation {
        return Err(XmlRegistryError::StaleHandle);
    }
    let block = slot.block.as_mut().ok_or(XmlRegistryError::StaleHandle)?;
    Ok(f(block))
}

/// Free the slot a `handle` refers to, bumping its generation so any other handle to it (or this
/// one, reused later) is rejected as [`XmlRegistryError::StaleHandle`]. Validates the handle the
/// same way [`with_block`] does, so freeing an already-freed/stale/fabricated handle returns `Err`.
pub fn free(handle: XmlBlockHandle) -> Result<(), XmlRegistryError> {
    let (index, generation) = unpack(handle);
    let mut reg = lock()?;
    let slot = reg
        .slots
        .get_mut(index as usize)
        .ok_or(XmlRegistryError::OutOfRange)?;
    if slot.generation != generation || slot.block.is_none() {
        return Err(XmlRegistryError::StaleHandle);
    }
    slot.block = None;
    // Bump (saturating) so the freed handle and any copy become stale and can never alias a reuse.
    slot.generation = slot.generation.saturating_add(1);
    reg.free.push(index);
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::apk::axml::XmlDocument;

    // Minimal empty document for handle-lifecycle tests (no VM/display needed — fully in-harness).
    fn empty_doc() -> XmlDocument {
        XmlDocument {
            events: Vec::new(),
            elements: Vec::new(),
            texts: Vec::new(),
            namespaces: Vec::new(),
            strings: Vec::new(),
        }
    }

    #[test]
    fn store_returns_distinct_nonzero_handles() {
        let a = store(empty_doc()).expect("store a");
        let b = store(empty_doc()).expect("store b");
        assert_ne!(a, b, "distinct stores must yield distinct handles");
        assert_ne!(a, 0, "a valid handle is never the reserved null 0");
        assert_ne!(b, 0, "a valid handle is never the reserved null 0");
        free(a).expect("free a");
        free(b).expect("free b");
    }

    #[test]
    fn freed_handle_is_stale_and_does_not_alias_reused_slot() {
        let old = store(empty_doc()).expect("store old");
        with_block(old, |b| b.cursor = 7).expect("mutate old");
        free(old).expect("free old");

        let new = store(empty_doc()).expect("store new");
        // The OLD handle must now be Stale — never read/write the NEW block's cursor.
        assert_eq!(
            with_block(old, |b| b.cursor),
            Err(XmlRegistryError::StaleHandle),
            "a freed handle must be StaleHandle, never alias the reused slot"
        );
        // The new block's cursor is fresh (0), unaffected by the stale lookup.
        assert_eq!(with_block(new, |b| b.cursor), Ok(0));
        free(new).expect("free new");
    }

    #[test]
    fn out_of_range_and_fabricated_handles_return_err_not_panic() {
        let fabricated = pack(u32::MAX, 1);
        assert_eq!(
            with_block(fabricated, |_| ()),
            Err(XmlRegistryError::OutOfRange),
            "a fabricated out-of-range index must be OutOfRange, never an out-of-bounds deref"
        );
        let null_lookup = with_block(0, |_| ());
        assert!(
            matches!(
                null_lookup,
                Err(XmlRegistryError::StaleHandle) | Err(XmlRegistryError::OutOfRange)
            ),
            "the reserved null handle 0 must be rejected, got {null_lookup:?}"
        );
        assert_eq!(free(fabricated), Err(XmlRegistryError::OutOfRange));
    }

    #[test]
    fn double_free_is_rejected() {
        let h = store(empty_doc()).expect("store");
        free(h).expect("first free");
        assert_eq!(free(h), Err(XmlRegistryError::StaleHandle));
    }

    #[test]
    fn cursor_advances_and_stops_at_end() {
        // A two-event doc: cursor walks both, then yields None and stays put.
        let doc = XmlDocument {
            events: vec![XmlEventKind::StartTag(0), XmlEventKind::EndTag(0)],
            elements: Vec::new(),
            texts: Vec::new(),
            namespaces: Vec::new(),
            strings: Vec::new(),
        };
        let h = store(doc).expect("store");
        assert_eq!(
            with_block(h, |b| b.next_event()),
            Ok(Some(XmlEventKind::StartTag(0)))
        );
        assert_eq!(
            with_block(h, |b| b.next_event()),
            Ok(Some(XmlEventKind::EndTag(0)))
        );
        assert_eq!(with_block(h, |b| b.next_event()), Ok(None));
        // Past the end stays None (saturating cursor — no overflow/panic).
        assert_eq!(with_block(h, |b| b.next_event()), Ok(None));
        free(h).expect("free");
    }

    #[test]
    fn pooled_string_returns_by_index_or_none() {
        // Backs XmlBlock.nativeGetPooledString: an in-range index returns its string; out-of-range
        // returns None (the native then yields null), never a panic.
        let doc = XmlDocument {
            events: Vec::new(),
            elements: Vec::new(),
            texts: Vec::new(),
            namespaces: Vec::new(),
            strings: vec!["zero".to_owned(), "Hello World!".to_owned()],
        };
        let h = store(doc).expect("store");
        assert_eq!(
            with_block(h, |b| b.pooled_string(0).map(str::to_owned)),
            Ok(Some("zero".to_owned()))
        );
        assert_eq!(
            with_block(h, |b| b.pooled_string(1).map(str::to_owned)),
            Ok(Some("Hello World!".to_owned()))
        );
        assert_eq!(with_block(h, |b| b.pooled_string(2).is_none()), Ok(true));
        free(h).expect("free");
    }
}
