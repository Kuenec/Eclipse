#![forbid(unsafe_code)]

use std::fmt;
use std::sync::{Mutex, OnceLock, PoisonError};

use crate::apk::axml::{XmlDocument, XmlEventKind};

static BLOCKS: OnceLock<Mutex<Registry>> = OnceLock::new();

pub type XmlBlockHandle = i64;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum XmlRegistryError {
    OutOfRange,

    StaleHandle,

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

#[derive(Debug)]
pub struct XmlBlock {
    pub doc: XmlDocument,

    pub cursor: usize,

    pub current: Option<usize>,
}

impl XmlBlock {
    pub fn current_event(&self) -> Option<XmlEventKind> {
        self.current.and_then(|i| self.doc.events.get(i).copied())
    }

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

    pub fn current_element(&self) -> Option<&crate::apk::axml::XmlElement> {
        match self.current_event()? {
            XmlEventKind::StartTag(i) | XmlEventKind::EndTag(i) => self.doc.elements.get(i),
            _ => None,
        }
    }

    pub fn current_text(&self) -> Option<&crate::apk::axml::XmlText> {
        match self.current_event()? {
            XmlEventKind::Text(i) => self.doc.texts.get(i),
            _ => None,
        }
    }

    pub fn pooled_string(&self, index: usize) -> Option<&str> {
        self.doc.strings.get(index).map(String::as_str)
    }
}

struct Slot {
    generation: u32,
    block: Option<XmlBlock>,
}

#[derive(Default)]
struct Registry {
    slots: Vec<Slot>,
    free: Vec<u32>,
}

fn pack(index: u32, generation: u32) -> XmlBlockHandle {
    ((generation as u64) << 32 | index as u64) as i64
}

fn unpack(handle: XmlBlockHandle) -> (u32, u32) {
    let bits = handle as u64;
    ((bits & 0xFFFF_FFFF) as u32, (bits >> 32) as u32)
}

fn lock() -> Result<std::sync::MutexGuard<'static, Registry>, XmlRegistryError> {
    BLOCKS
        .get_or_init(|| Mutex::new(Registry::default()))
        .lock()
        .map_err(|_: PoisonError<_>| XmlRegistryError::Poisoned)
}

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

    slot.generation = slot.generation.saturating_add(1);
    reg.free.push(index);
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::apk::axml::XmlDocument;

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

        assert_eq!(
            with_block(old, |b| b.cursor),
            Err(XmlRegistryError::StaleHandle),
            "a freed handle must be StaleHandle, never alias the reused slot"
        );

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

        assert_eq!(with_block(h, |b| b.next_event()), Ok(None));
        free(h).expect("free");
    }

    #[test]
    fn pooled_string_returns_by_index_or_none() {
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
