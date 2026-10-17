use std::fs::File;
use std::io;
use std::ops::Deref;
use std::path::Path;
use std::ptr::NonNull;

use rustix::mm::{mmap, munmap, MapFlags, ProtFlags};

pub(crate) struct MappedFile {
    mapping: NonNull<u8>,
    mapping_len: usize,
    lead: usize,
}

unsafe impl Send for MappedFile {}
unsafe impl Sync for MappedFile {}

impl MappedFile {
    pub(crate) fn open(path: &Path) -> io::Result<Self> {
        let file = File::open(path)?;
        let len = file.metadata()?.len();
        if len == 0 {
            return Err(io::Error::new(
                io::ErrorKind::InvalidData,
                "the file is empty",
            ));
        }
        Self::range(&file, 0, len)
    }

    pub(crate) fn range(file: &File, offset: u64, len: u64) -> io::Result<Self> {
        let file_len = file.metadata()?.len();
        if offset.checked_add(len).is_none_or(|end| end > file_len) {
            return Err(io::Error::new(
                io::ErrorKind::UnexpectedEof,
                format!(
                    "{len} bytes at offset {offset} run past the end of the {file_len}-byte file"
                ),
            ));
        }
        if len == 0 {
            return Err(io::Error::new(
                io::ErrorKind::InvalidData,
                "the range to map is empty",
            ));
        }
        let lead = offset % rustix::param::page_size() as u64;
        let mapping_len = usize::try_from(lead + len).map_err(|_| {
            io::Error::new(
                io::ErrorKind::FileTooLarge,
                "the range does not fit in the address space",
            )
        })?;
        let mapping = unsafe {
            mmap(
                std::ptr::null_mut(),
                mapping_len,
                ProtFlags::READ,
                MapFlags::PRIVATE,
                file,
                offset - lead,
            )
        }?;
        let mapping = NonNull::new(mapping.cast::<u8>())
            .ok_or_else(|| io::Error::other("mmap returned a null mapping"))?;
        Ok(Self {
            mapping,
            mapping_len,
            lead: lead as usize,
        })
    }
}

impl Deref for MappedFile {
    type Target = [u8];

    fn deref(&self) -> &[u8] {
        unsafe {
            std::slice::from_raw_parts(
                self.mapping.as_ptr().add(self.lead),
                self.mapping_len - self.lead,
            )
        }
    }
}

impl Drop for MappedFile {
    fn drop(&mut self) {
        let _ = unsafe { munmap(self.mapping.as_ptr().cast(), self.mapping_len) };
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::loader::link::tests::temp_dir;

    #[test]
    fn a_mapped_file_reads_back_exactly_its_contents() {
        let dir = temp_dir("mapped-contents");
        let path = dir.join("contents.bin");
        let contents: Vec<u8> = (0..=255u8).cycle().take(3 * 4096 + 17).collect();
        std::fs::write(&path, &contents).expect("write the fixture");
        let mapped = MappedFile::open(&path).expect("map the fixture");
        assert_eq!(&*mapped, contents.as_slice());
        std::fs::remove_dir_all(dir).expect("remove the fixture");
    }

    #[test]
    fn a_range_at_an_unaligned_offset_maps_exactly_its_bytes() {
        let dir = temp_dir("mapped-range");
        let path = dir.join("contents.bin");
        let contents: Vec<u8> = (0..=250u8).cycle().take(5 * 4096 + 3).collect();
        std::fs::write(&path, &contents).expect("write the fixture");
        let file = File::open(&path).expect("open the fixture");
        let (offset, len) = (4096 + 13, 2 * 4096 + 7);
        let mapped = MappedFile::range(&file, offset, len).expect("map the range");
        assert_eq!(
            &*mapped,
            &contents[offset as usize..(offset + len) as usize]
        );
        std::fs::remove_dir_all(dir).expect("remove the fixture");
    }

    #[test]
    fn an_empty_file_is_refused_instead_of_mapped() {
        let dir = temp_dir("mapped-empty");
        let path = dir.join("empty.bin");
        std::fs::write(&path, []).expect("write the fixture");
        let Err(error) = MappedFile::open(&path) else {
            panic!("an empty file must not map");
        };
        assert_eq!(error.kind(), io::ErrorKind::InvalidData);
        std::fs::remove_dir_all(dir).expect("remove the fixture");
    }
}
