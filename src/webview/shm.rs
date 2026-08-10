use std::os::fd::{AsRawFd, BorrowedFd, FromRawFd, OwnedFd};

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ShmError {
    TooLarge,

    ZeroSized,

    Create(std::io::ErrorKind),

    Truncate(std::io::ErrorKind),

    SealCtl(std::io::ErrorKind),

    Stat(std::io::ErrorKind),

    WrongSize { actual: u64, expected: u64 },

    NotSealed { seals: i32 },

    Map(std::io::ErrorKind),
}

impl std::fmt::Display for ShmError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::TooLarge => write!(f, "frame geometry exceeds the u32 slot_bytes field"),
            Self::ZeroSized => write!(f, "frame geometry has a zero dimension or slot count"),
            Self::Create(kind) => write!(f, "memfd_create failed: {kind}"),
            Self::Truncate(kind) => write!(f, "ftruncate failed: {kind}"),
            Self::SealCtl(kind) => write!(f, "memfd seal fcntl failed: {kind}"),
            Self::Stat(kind) => write!(f, "fstat failed: {kind}"),
            Self::WrongSize { actual, expected } => {
                write!(f, "memfd size {actual} != expected {expected}")
            }
            Self::NotSealed { seals } => write!(
                f,
                "memfd seals 0x{seals:X} lack F_SEAL_SHRINK — refusing to map (SIGBUS risk)"
            ),
            Self::Map(kind) => write!(f, "mmap failed: {kind}"),
        }
    }
}

impl std::error::Error for ShmError {}

fn last_os_error() -> std::io::ErrorKind {
    std::io::Error::last_os_error().kind()
}

pub fn slot_bytes_for(width: u16, height: u16) -> Result<u32, ShmError> {
    if width == 0 || height == 0 {
        return Err(ShmError::ZeroSized);
    }
    let bytes = 4u64 * u64::from(width) * u64::from(height);
    u32::try_from(bytes).map_err(|_| ShmError::TooLarge)
}

pub fn create_sealed_frame_memfd(
    width: u16,
    height: u16,
    slots: u8,
) -> Result<(OwnedFd, u32), ShmError> {
    if slots == 0 {
        return Err(ShmError::ZeroSized);
    }
    let slot_bytes = slot_bytes_for(width, height)?;
    let total = u64::from(slot_bytes) * u64::from(slots);
    let total_off = i64::try_from(total).map_err(|_| ShmError::TooLarge)?;

    let raw = unsafe {
        libc::memfd_create(
            c"eclipse-webview-frames".as_ptr(),
            libc::MFD_CLOEXEC | libc::MFD_ALLOW_SEALING,
        )
    };
    if raw < 0 {
        return Err(ShmError::Create(last_os_error()));
    }

    let fd = unsafe { OwnedFd::from_raw_fd(raw) };

    if unsafe { libc::ftruncate(fd.as_raw_fd(), total_off) } != 0 {
        return Err(ShmError::Truncate(last_os_error()));
    }

    if unsafe {
        libc::fcntl(
            fd.as_raw_fd(),
            libc::F_ADD_SEALS,
            libc::F_SEAL_SHRINK | libc::F_SEAL_GROW | libc::F_SEAL_SEAL,
        )
    } != 0
    {
        return Err(ShmError::SealCtl(last_os_error()));
    }
    Ok((fd, slot_bytes))
}

#[derive(Debug)]
pub struct FrameMapping {
    ptr: std::ptr::NonNull<u8>,
    len: usize,
}

impl FrameMapping {
    pub fn len(&self) -> usize {
        self.len
    }

    pub fn is_empty(&self) -> bool {
        self.len == 0
    }

    pub fn slice(&self, offset: usize, len: usize) -> Option<&[u8]> {
        let end = offset.checked_add(len)?;
        if end > self.len {
            return None;
        }

        Some(unsafe { std::slice::from_raw_parts(self.ptr.as_ptr().add(offset), len) })
    }
}

impl Drop for FrameMapping {
    fn drop(&mut self) {
        unsafe {
            libc::munmap(self.ptr.as_ptr().cast(), self.len);
        }
    }
}

pub fn map_frame_buffer(fd: BorrowedFd<'_>, expected_len: usize) -> Result<FrameMapping, ShmError> {
    if expected_len == 0 {
        return Err(ShmError::ZeroSized);
    }

    let mut st: libc::stat = unsafe { std::mem::zeroed() };
    if unsafe { libc::fstat(fd.as_raw_fd(), &mut st) } != 0 {
        return Err(ShmError::Stat(last_os_error()));
    }
    let actual = u64::try_from(st.st_size).unwrap_or(0);
    if actual != expected_len as u64 {
        return Err(ShmError::WrongSize {
            actual,
            expected: expected_len as u64,
        });
    }

    let seals = unsafe { libc::fcntl(fd.as_raw_fd(), libc::F_GET_SEALS) };
    if seals < 0 {
        return Err(ShmError::SealCtl(last_os_error()));
    }
    if seals & libc::F_SEAL_SHRINK == 0 {
        return Err(ShmError::NotSealed { seals });
    }

    let ptr = unsafe {
        libc::mmap(
            std::ptr::null_mut(),
            expected_len,
            libc::PROT_READ,
            libc::MAP_SHARED,
            fd.as_raw_fd(),
            0,
        )
    };
    if ptr == libc::MAP_FAILED {
        return Err(ShmError::Map(last_os_error()));
    }
    let ptr =
        std::ptr::NonNull::new(ptr.cast::<u8>()).ok_or(ShmError::Map(std::io::ErrorKind::Other))?;
    Ok(FrameMapping {
        ptr,
        len: expected_len,
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::fs::File;
    use std::os::fd::AsFd;
    use std::os::unix::fs::FileExt;

    #[test]
    fn map_frame_buffer_rejects_unsealed_or_missized_memfd() {
        let raw = unsafe {
            libc::memfd_create(c"eclipse-webview-test-unsealed".as_ptr(), libc::MFD_CLOEXEC)
        };
        assert!(raw >= 0, "memfd_create failed");

        let unsealed = unsafe { OwnedFd::from_raw_fd(raw) };
        let expected = 4usize * 4 * 4 * 2;

        assert_eq!(
            unsafe { libc::ftruncate(unsealed.as_raw_fd(), expected as i64) },
            0
        );
        match map_frame_buffer(unsealed.as_fd(), expected) {
            Err(ShmError::NotSealed { .. }) => {}
            other => panic!("unsealed memfd must be refused, got {other:?}"),
        }

        let (sealed, slot_bytes) = create_sealed_frame_memfd(4, 4, 2).expect("create");
        let total = slot_bytes as usize * 2;
        match map_frame_buffer(sealed.as_fd(), total + 1) {
            Err(ShmError::WrongSize { actual, expected }) => {
                assert_eq!(actual, total as u64);
                assert_eq!(expected, total as u64 + 1);
            }
            other => panic!("missized memfd must be refused, got {other:?}"),
        }

        let payload: Vec<u8> = (0..total).map(|i| (i % 251) as u8).collect();
        let file = File::from(sealed.try_clone().expect("dup"));
        file.write_at(&payload, 0).expect("write");
        let mapping = map_frame_buffer(sealed.as_fd(), total).expect("map");
        assert_eq!(mapping.len(), total);
        assert_eq!(mapping.slice(0, total).expect("slice"), payload.as_slice());

        assert!(mapping.slice(total, 1).is_none());
        assert!(mapping.slice(usize::MAX, 2).is_none());
    }
}
