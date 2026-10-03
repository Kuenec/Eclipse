use std::collections::{HashMap, HashSet};
use std::os::fd::BorrowedFd;
use std::sync::Arc;

const PLANES: usize = 4;

const KNOWN_FLAGS: u32 = 0b111;

const PAIRS_CHECKED_SINCE: u32 = 4;

const FORMAT_ENTRY_SIZE: usize = 16;

const FORMAT_TABLE_LIMIT: usize = 1024 * 1024;

const ADVERTISED_LIMIT: usize = 64 * 1024;

#[derive(Clone, Copy)]
struct Plane {
    offset: u32,
    stride: u32,
    file_size: Option<u64>,
}

#[derive(Default)]
struct Params {
    planes: [Option<Plane>; PLANES],
    modifier: Option<u64>,
    used: bool,
    size: Option<(i32, i32)>,
}

pub(super) struct Creation {
    pub(super) version: u32,
    pub(super) width: i32,
    pub(super) height: i32,
    pub(super) format: u32,
    pub(super) flags: u32,
}

#[derive(Default)]
pub(super) struct Dmabuf {
    params: HashMap<u32, Params>,
    tables: HashMap<u32, Arc<[(u32, u64)]>>,
    advertised: HashSet<(u32, u64)>,
}

impl Dmabuf {
    pub(super) fn add(
        &mut self,
        params: u32,
        file: BorrowedFd<'_>,
        plane: u32,
        (offset, stride): (u32, u32),
        modifier: u64,
    ) -> Result<(), &'static str> {
        let state = self.params.entry(params).or_default();
        if state.used {
            return Err("a plane added after the buffer was created");
        }
        let slot = usize::try_from(plane)
            .ok()
            .and_then(|plane| state.planes.get_mut(plane))
            .ok_or("a plane index above 3")?;
        if slot.is_some() {
            return Err("a plane added twice");
        }
        if state.modifier.is_some_and(|known| known != modifier) {
            return Err("planes with different modifiers");
        }
        *slot = Some(Plane {
            offset,
            stride,
            file_size: rustix::fs::seek(file, rustix::fs::SeekFrom::End(0)).ok(),
        });
        state.modifier = Some(modifier);
        Ok(())
    }

    pub(super) fn create(&mut self, params: u32, creation: &Creation) -> Result<(), &'static str> {
        let state = self.params.entry(params).or_default();
        if state.used {
            return Err("a second buffer from one set of planes");
        }
        state.used = true;
        let planes: Vec<Plane> = state.planes.iter().map_while(|plane| *plane).collect();
        if planes.is_empty() {
            return Err("a buffer without plane 0");
        }
        if state.planes.iter().flatten().count() != planes.len() {
            return Err("a gap between planes");
        }
        if creation.flags & !KNOWN_FLAGS != 0 {
            return Err("unknown buffer flags");
        }
        if creation.width < 1 || creation.height < 1 {
            return Err("a buffer smaller than one pixel");
        }
        let height = u64::from(creation.height.unsigned_abs());
        for (index, plane) in planes.iter().enumerate() {
            let (offset, stride) = (u64::from(plane.offset), u64::from(plane.stride));
            let end = offset + stride * height;
            if offset + stride > u64::from(u32::MAX) || end > u64::from(u32::MAX) {
                return Err("a plane whose size overflows");
            }
            let Some(file_size) = plane.file_size else {
                continue;
            };
            if offset > file_size || offset + stride > file_size || stride == 0 {
                return Err("a plane offset or stride outside its file");
            }
            if index == 0 && end > file_size {
                return Err("plane 0 is larger than its file");
            }
        }
        let modifier = state.modifier.unwrap_or_default();
        if creation.version >= PAIRS_CHECKED_SINCE
            && !self.advertised.contains(&(creation.format, modifier))
        {
            return Err("a format and modifier the compositor did not offer");
        }
        state.size = Some((creation.width, creation.height));
        Ok(())
    }

    pub(super) fn requested_size(&self, params: u32) -> Option<(i32, i32)> {
        self.params.get(&params)?.size
    }

    pub(super) fn forget(&mut self, id: u32) {
        self.params.remove(&id);
        self.tables.remove(&id);
    }

    pub(super) fn format_table(&mut self, feedback: u32, file: BorrowedFd<'_>, size: u32) {
        let table = read_format_table(file, size).unwrap_or_default();
        let shared = self
            .tables
            .values()
            .find(|known| ***known == *table)
            .cloned()
            .unwrap_or_else(|| table.into());
        self.tables.insert(feedback, shared);
    }

    pub(super) fn tranche_formats(&mut self, feedback: u32, indices: &[u8]) {
        let Some(table) = self.tables.get(&feedback) else {
            return;
        };
        for index in indices.as_chunks::<2>().0 {
            if self.advertised.len() >= ADVERTISED_LIMIT {
                return;
            }
            if let Some(pair) = table.get(usize::from(u16::from_ne_bytes(*index))) {
                self.advertised.insert(*pair);
            }
        }
    }
}

fn read_format_table(file: BorrowedFd<'_>, size: u32) -> Option<Vec<(u32, u64)>> {
    let size = usize::try_from(size)
        .ok()
        .filter(|size| *size <= FORMAT_TABLE_LIMIT)?;
    let mut bytes = vec![0u8; size];
    let mut filled = 0;
    while filled < size {
        let offset = u64::try_from(filled).ok()?;
        match rustix::io::pread(file, &mut bytes[filled..], offset) {
            Ok(0) | Err(_) => return None,
            Ok(read) => filled += read,
        }
    }
    let (entries, _) = bytes.as_chunks::<FORMAT_ENTRY_SIZE>();
    Some(
        entries
            .iter()
            .map(|&[f0, f1, f2, f3, _, _, _, _, modifier @ ..]| {
                (
                    u32::from_ne_bytes([f0, f1, f2, f3]),
                    u64::from_ne_bytes(modifier),
                )
            })
            .collect(),
    )
}
