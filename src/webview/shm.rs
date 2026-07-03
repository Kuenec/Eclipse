//! memfd frame-buffer lifecycle for the helper's BGRA frame transport.
//!
//! 2026-07-03: one memfd per buffer GENERATION (view x size) holding `slot_count` slots
//! back-to-back. The helper creates it sealed (`F_SEAL_SHRINK|F_SEAL_GROW|F_SEAL_SEAL` —
//! write access stays open; only the SIZE is immutable) so the consumer can never SIGBUS via
//! truncation; the consumer maps it read-only AFTER verifying size and seals
//! (detect-don't-assume — never map an unsealed or missized fd). The fd crosses the process
//! boundary via [`super::fdpass`].
//!
//! `unsafe` is confined to the `memfd`/`fcntl`/`mmap` FFI; this file is shared into the
//! helper crate by `#[path]`, so it uses `libc` only.

use std::os::fd::{AsRawFd, BorrowedFd, FromRawFd, OwnedFd};

/// Typed shm error (payload-free).
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ShmError {
    /// `width * height * 4 * slots` does not fit the protocol's u32 `slot_bytes` field.
    TooLarge,
    /// Zero-sized dimensions or slot count.
    ZeroSized,
    /// `memfd_create` failed.
    Create(std::io::ErrorKind),
    /// `ftruncate` failed.
    Truncate(std::io::ErrorKind),
    /// `F_ADD_SEALS`/`F_GET_SEALS` failed.
    SealCtl(std::io::ErrorKind),
    /// `fstat` failed.
    Stat(std::io::ErrorKind),
    /// The fd's size does not match the announced geometry.
    WrongSize { actual: u64, expected: u64 },
    /// The fd is missing `F_SEAL_SHRINK` — mapping it could SIGBUS via truncation.
    NotSealed { seals: i32 },
    /// `mmap` failed.
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

/// Bytes of one tightly-packed BGRA slot (`4 * width * height`), validated against the
/// protocol's u32 field width.
pub fn slot_bytes_for(width: u16, height: u16) -> Result<u32, ShmError> {
    if width == 0 || height == 0 {
        return Err(ShmError::ZeroSized);
    }
    let bytes = 4u64 * u64::from(width) * u64::from(height);
    u32::try_from(bytes).map_err(|_| ShmError::TooLarge)
}

/// Helper role: create the sealed frame memfd for one buffer generation. Returns the fd and
/// the per-slot byte count. The fd is `MFD_CLOEXEC` (it must never leak into CEF's forked
/// subprocesses) and size-sealed (`SHRINK|GROW|SEAL`); the helper keeps write access through
/// ordinary `pwrite`/mappings — only resizing is forbidden.
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

    // SAFETY: memfd_create takes a NUL-terminated name and flag bits; it creates a new fd or
    // returns -1. The name is a static CStr literal.
    let raw = unsafe {
        libc::memfd_create(
            c"eclipse-webview-frames".as_ptr(),
            libc::MFD_CLOEXEC | libc::MFD_ALLOW_SEALING,
        )
    };
    if raw < 0 {
        return Err(ShmError::Create(last_os_error()));
    }
    // SAFETY: memfd_create just returned this fd; we take sole ownership.
    let fd = unsafe { OwnedFd::from_raw_fd(raw) };

    // SAFETY: ftruncate on our own fresh memfd with a validated non-negative length.
    if unsafe { libc::ftruncate(fd.as_raw_fd(), total_off) } != 0 {
        return Err(ShmError::Truncate(last_os_error()));
    }
    // SAFETY: F_ADD_SEALS on our own MFD_ALLOW_SEALING memfd. SHRINK|GROW freeze the size
    // (the consumer's SIGBUS guard); SEAL freezes the seal set itself. F_SEAL_WRITE is
    // deliberately NOT added — the helper writes frames through this fd for its lifetime.
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

/// A read-only mapping of a whole frame memfd (all slots of one generation).
///
/// 2026-07-03 aliasing contract: the helper writes other slots of this mapping concurrently.
/// A slot's bytes may only be read between the `FrameReady` that published it and the
/// consumer's own `FrameAck` for it — in that window the helper never writes that slot (the
/// [`super::slots::SlotTracker`] invariant), so the returned slice is stable.
#[derive(Debug)]
pub struct FrameMapping {
    ptr: std::ptr::NonNull<u8>,
    len: usize,
}

impl FrameMapping {
    /// Total mapped length in bytes (all slots).
    pub fn len(&self) -> usize {
        self.len
    }

    pub fn is_empty(&self) -> bool {
        self.len == 0
    }

    /// Bounds-checked view of `[offset, offset + len)`. See the struct-level aliasing
    /// contract for WHEN reading it is sound.
    pub fn slice(&self, offset: usize, len: usize) -> Option<&[u8]> {
        let end = offset.checked_add(len)?;
        if end > self.len {
            return None;
        }
        // SAFETY: the mapping is PROT_READ, MAP_SHARED, `self.len` bytes long and lives until
        // Drop; offset+len was bounds-checked above. Publish/ack flow control (see the
        // struct docs) guarantees the helper is not writing this range while the caller
        // holds the slice.
        Some(unsafe { std::slice::from_raw_parts(self.ptr.as_ptr().add(offset), len) })
    }
}

impl Drop for FrameMapping {
    fn drop(&mut self) {
        // SAFETY: (ptr, len) is exactly the live mapping mmap returned; unmapping it once in
        // Drop ends its lifetime (no slices outlive self per the borrow on `slice`).
        unsafe {
            libc::munmap(self.ptr.as_ptr().cast(), self.len);
        }
    }
}

/// Consumer role: map a received frame memfd read-only — AFTER verifying it. `expected_len`
/// is `slot_bytes * slot_count` from the validated `FrameBufferNew` announcement.
///
/// Detect-don't-assume: the fd's size must equal `expected_len` exactly and its seal set
/// must include `F_SEAL_SHRINK`, otherwise a hostile/buggy peer could shrink the file and
/// turn later reads into SIGBUS. Both checks yield typed errors, never a mapping.
pub fn map_frame_buffer(fd: BorrowedFd<'_>, expected_len: usize) -> Result<FrameMapping, ShmError> {
    if expected_len == 0 {
        return Err(ShmError::ZeroSized);
    }

    // SAFETY: fstat writes into the zeroed stat buffer on success; fd is live (BorrowedFd).
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

    // SAFETY: F_GET_SEALS reads the seal bits of the fd; no memory is touched.
    let seals = unsafe { libc::fcntl(fd.as_raw_fd(), libc::F_GET_SEALS) };
    if seals < 0 {
        return Err(ShmError::SealCtl(last_os_error()));
    }
    if seals & libc::F_SEAL_SHRINK == 0 {
        return Err(ShmError::NotSealed { seals });
    }

    // SAFETY: length validated nonzero and equal to the fd's sealed size; PROT_READ +
    // MAP_SHARED over a shrink-sealed memfd cannot SIGBUS. We check MAP_FAILED before use.
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
    let ptr = std::ptr::NonNull::new(ptr.cast::<u8>()).ok_or(ShmError::Map(
        // mmap never returns NULL for a successful fixed-offset file mapping; treat it as a
        // mapping failure rather than panicking (total, panic=abort root).
        std::io::ErrorKind::Other,
    ))?;
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
        // 2026-07-03: pins the detect-don't-assume mapping guard (the SIGBUS-via-truncation
        // class). memfd needs no display — plain `cargo test`.

        // An UNSEALED memfd of the right size must be refused.
        // SAFETY: plain memfd_create; ownership taken immediately below.
        let raw = unsafe {
            libc::memfd_create(c"eclipse-webview-test-unsealed".as_ptr(), libc::MFD_CLOEXEC)
        };
        assert!(raw >= 0, "memfd_create failed");
        // SAFETY: fresh fd from memfd_create.
        let unsealed = unsafe { OwnedFd::from_raw_fd(raw) };
        let expected = 4usize * 4 * 4 * 2;
        // SAFETY: ftruncate on our own memfd.
        assert_eq!(
            unsafe { libc::ftruncate(unsealed.as_raw_fd(), expected as i64) },
            0
        );
        match map_frame_buffer(unsealed.as_fd(), expected) {
            Err(ShmError::NotSealed { .. }) => {}
            other => panic!("unsealed memfd must be refused, got {other:?}"),
        }

        // A SEALED memfd with the WRONG size must be refused.
        let (sealed, slot_bytes) = create_sealed_frame_memfd(4, 4, 2).expect("create");
        let total = slot_bytes as usize * 2;
        match map_frame_buffer(sealed.as_fd(), total + 1) {
            Err(ShmError::WrongSize { actual, expected }) => {
                assert_eq!(actual, total as u64);
                assert_eq!(expected, total as u64 + 1);
            }
            other => panic!("missized memfd must be refused, got {other:?}"),
        }

        // The correctly sealed, exact-size one maps — and serves the helper-written bytes.
        let payload: Vec<u8> = (0..total).map(|i| (i % 251) as u8).collect();
        let file = File::from(sealed.try_clone().expect("dup"));
        file.write_at(&payload, 0).expect("write");
        let mapping = map_frame_buffer(sealed.as_fd(), total).expect("map");
        assert_eq!(mapping.len(), total);
        assert_eq!(mapping.slice(0, total).expect("slice"), payload.as_slice());
        // Bounds discipline: out-of-range views are None, never UB.
        assert!(mapping.slice(total, 1).is_none());
        assert!(mapping.slice(usize::MAX, 2).is_none());
    }
}
