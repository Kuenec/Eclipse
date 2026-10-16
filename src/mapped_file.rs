use std::fs::File;
use std::io;
use std::ops::Deref;
use std::path::Path;
use std::ptr::NonNull;

use rustix::mm::{mmap, munmap, MapFlags, ProtFlags};

pub(crate) struct MappedFile {
    base: NonNull<u8>,
    len: usize,
}

unsafe impl Send for MappedFile {}
unsafe impl Sync for MappedFile {}

impl MappedFile {
    pub(crate) fn open(path: &Path) -> io::Result<Self> {
        let file = File::open(path)?;
        let len = usize::try_from(file.metadata()?.len()).map_err(|_| {
            io::Error::new(
                io::ErrorKind::FileTooLarge,
                "the file does not fit in the address space",
            )
        })?;
        if len == 0 {
            return Err(io::Error::new(
                io::ErrorKind::InvalidData,
                "the file is empty",
            ));
        }
        let mapping = unsafe {
            mmap(
                std::ptr::null_mut(),
                len,
                ProtFlags::READ,
                MapFlags::PRIVATE,
                &file,
                0,
            )
        }?;
        let base = NonNull::new(mapping.cast::<u8>())
            .ok_or_else(|| io::Error::other("mmap returned a null mapping"))?;
        Ok(Self { base, len })
    }
}

impl Deref for MappedFile {
    type Target = [u8];

    fn deref(&self) -> &[u8] {
        unsafe { std::slice::from_raw_parts(self.base.as_ptr(), self.len) }
    }
}

impl Drop for MappedFile {
    fn drop(&mut self) {
        let _ = unsafe { munmap(self.base.as_ptr().cast(), self.len) };
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
