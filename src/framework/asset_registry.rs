#![forbid(unsafe_code)]

use std::fmt;
use std::sync::{Mutex, OnceLock, PoisonError};

static STREAMS: OnceLock<Mutex<Registry>> = OnceLock::new();

pub type AssetHandle = i64;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum AssetRegistryError {
    OutOfRange,

    StaleHandle,

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

#[derive(Debug)]
pub struct AssetStream {
    data: Box<[u8]>,
    pos: usize,
}

impl AssetStream {
    #[must_use]
    pub fn len(&self) -> usize {
        self.data.len()
    }

    #[must_use]
    pub fn is_empty(&self) -> bool {
        self.data.is_empty()
    }

    #[must_use]
    pub fn remaining(&self) -> usize {
        self.data.len().saturating_sub(self.pos)
    }

    pub fn read(&mut self, out: &mut [u8]) -> usize {
        let n = out.len().min(self.remaining());
        out[..n].copy_from_slice(&self.data[self.pos..self.pos + n]);
        self.pos = self.pos.saturating_add(n);
        n
    }

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

struct Slot {
    generation: u32,
    stream: Option<AssetStream>,
}

#[derive(Default)]
struct Registry {
    slots: Vec<Slot>,
    free: Vec<u32>,
}

fn pack(index: u32, generation: u32) -> AssetHandle {
    ((generation as u64) << 32 | index as u64) as i64
}

fn unpack(handle: AssetHandle) -> (u32, u32) {
    let bits = handle as u64;
    ((bits & 0xFFFF_FFFF) as u32, (bits >> 32) as u32)
}

fn lock() -> Result<std::sync::MutexGuard<'static, Registry>, AssetRegistryError> {
    STREAMS
        .get_or_init(|| Mutex::new(Registry::default()))
        .lock()
        .map_err(|_: PoisonError<_>| AssetRegistryError::Poisoned)
}

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

        assert_eq!(with_stream(a, |s| s.len()), Ok(5));
        assert_eq!(with_stream(a, |s| s.remaining()), Ok(5));

        let mut buf = [0u8; 3];
        assert_eq!(with_stream(a, |s| s.read(&mut buf)), Ok(3));
        assert_eq!(buf, [1, 2, 3]);
        assert_eq!(with_stream(a, |s| s.remaining()), Ok(2));
        let mut buf2 = [0u8; 8];
        assert_eq!(with_stream(a, |s| s.read(&mut buf2)), Ok(2));
        assert_eq!(&buf2[..2], &[4, 5]);
        assert_eq!(with_stream(a, |s| s.read(&mut buf2)), Ok(0));

        free(a).expect("free a");
        free(b).expect("free b");
    }

    #[test]
    fn seek_set_cur_end_and_rejects_out_of_range() {
        let h = store(vec![0, 1, 2, 3, 4, 5, 6, 7, 8, 9]).expect("store");
        assert_eq!(with_stream(h, |s| s.seek(3, 0)), Ok(3));
        assert_eq!(with_stream(h, |s| s.remaining()), Ok(7));
        assert_eq!(with_stream(h, |s| s.seek(2, 1)), Ok(5));
        assert_eq!(with_stream(h, |s| s.seek(0, 2)), Ok(10));
        assert_eq!(with_stream(h, |s| s.seek(-1, 0)), Ok(-1));
        assert_eq!(with_stream(h, |s| s.seek(1, 2)), Ok(-1));
        assert_eq!(with_stream(h, |s| s.seek(0, 99)), Ok(-1));

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
