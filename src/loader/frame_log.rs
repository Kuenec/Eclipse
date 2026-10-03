use std::fs::OpenOptions;
use std::io::ErrorKind;
use std::num::NonZeroU64;
use std::os::unix::fs::OpenOptionsExt as _;
use std::path::{Path, PathBuf};
use std::ptr::NonNull;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::OnceLock;

use rustix::fs::{fallocate, FallocateFlags};
use rustix::io::Errno;
use rustix::mm::{mmap, munmap, MapFlags, ProtFlags};
use rustix::time::{clock_gettime, ClockId};

const _: () = assert!(cfg!(target_endian = "little"));

const ENV: &str = "ECLIPSE_FRAMETIME_LOG";
const MAGIC: [u8; 8] = *b"ECLFRAME";
const CAPACITY: NonZeroU64 = NonZeroU64::new(1 << 18).unwrap();
const HEADER_BYTES: usize = 64;
const ENTRY_BYTES: usize = 16;
const WORD_BYTES: usize = 8;
const CAPACITY_OFFSET: usize = 8;
const WRITTEN_OFFSET: usize = 16;
const FILE_MODE: u32 = 0o600;
const NANOS_PER_SECOND: u64 = 1_000_000_000;

static ARMED: OnceLock<Option<FrameLog>> = OnceLock::new();

#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord)]
pub(crate) struct MonotonicNs(u64);

impl MonotonicNs {
    pub(crate) fn now() -> Self {
        let now = clock_gettime(ClockId::Monotonic);
        Self(now.tv_sec as u64 * NANOS_PER_SECOND + now.tv_nsec as u64)
    }

    fn nanos_since(self, earlier: Self) -> u32 {
        u32::try_from(self.0.saturating_sub(earlier.0)).unwrap_or(u32::MAX)
    }
}

#[derive(Debug, Clone, Copy)]
pub(crate) struct HostCall {
    pub(crate) started: MonotonicNs,
    pub(crate) ended: MonotonicNs,
}

#[derive(Debug)]
pub enum FrameLogError {
    RelativePath(PathBuf),

    Exists(PathBuf),

    Io {
        operation: &'static str,
        path: PathBuf,
        source: std::io::Error,
    },

    LeftBehind {
        operation: &'static str,
        path: PathBuf,
        source: std::io::Error,
        removal: std::io::Error,
    },
}

impl std::fmt::Display for FrameLogError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::RelativePath(path) => {
                write!(f, "{ENV} must be an absolute path, not {}", path.display())
            }
            Self::Exists(path) => write!(
                f,
                "cannot create the frame-time log {}: the file already exists, and Eclipse \
                 writes each frame-time log to a new file",
                path.display()
            ),
            Self::Io {
                operation,
                path,
                source,
            } => write!(
                f,
                "cannot {operation} the frame-time log {}: {source}",
                path.display()
            ),
            Self::LeftBehind {
                operation,
                path,
                source,
                removal,
            } => write!(
                f,
                "cannot {operation} the frame-time log {}: {source}, and cannot remove the \
                 unusable file: {removal}",
                path.display()
            ),
        }
    }
}

impl std::error::Error for FrameLogError {}

pub fn arm_from_env() -> Result<(), FrameLogError> {
    let mut outcome = Ok(());
    ARMED.get_or_init(|| {
        let path = PathBuf::from(std::env::var_os(ENV)?);
        match FrameLog::create(&path, CAPACITY) {
            Ok(log) => Some(log),
            Err(error) => {
                outcome = Err(error);
                None
            }
        }
    });
    outcome
}

pub(crate) fn armed() -> Option<&'static FrameLog> {
    ARMED.get().and_then(Option::as_ref)
}

pub(crate) struct FrameLog {
    mapping: NonNull<u8>,
    len: usize,
    capacity: NonZeroU64,
}

unsafe impl Send for FrameLog {}
unsafe impl Sync for FrameLog {}

impl FrameLog {
    pub(crate) fn create(path: &Path, capacity: NonZeroU64) -> Result<Self, FrameLogError> {
        if !path.is_absolute() {
            return Err(FrameLogError::RelativePath(path.to_path_buf()));
        }
        let failed = |operation, source| FrameLogError::Io {
            operation,
            path: path.to_path_buf(),
            source,
        };
        let file = OpenOptions::new()
            .read(true)
            .write(true)
            .create_new(true)
            .mode(FILE_MODE)
            .open(path)
            .map_err(|source| match source.kind() {
                ErrorKind::AlreadyExists => FrameLogError::Exists(path.to_path_buf()),
                _ => failed("create", source),
            })?;
        let abandon = |operation, errno: Errno| {
            let source = std::io::Error::from(errno);
            match std::fs::remove_file(path) {
                Ok(()) => failed(operation, source),
                Err(removal) => FrameLogError::LeftBehind {
                    operation,
                    path: path.to_path_buf(),
                    source,
                    removal,
                },
            }
        };
        let len = HEADER_BYTES + ENTRY_BYTES * capacity.get() as usize;
        fallocate(&file, FallocateFlags::empty(), 0, len as u64)
            .map_err(|errno| abandon("allocate", errno))?;
        let mapping = unsafe {
            mmap(
                std::ptr::null_mut(),
                len,
                ProtFlags::READ | ProtFlags::WRITE,
                MapFlags::SHARED,
                &file,
                0,
            )
        }
        .and_then(|mapping| NonNull::new(mapping.cast::<u8>()).ok_or(Errno::NOMEM))
        .map_err(|errno| abandon("map", errno))?;
        let log = Self {
            mapping,
            len,
            capacity,
        };
        for page in (0..len).step_by(rustix::param::page_size()) {
            log.word(page).store(0, Ordering::Relaxed);
        }
        log.word(0)
            .store(u64::from_le_bytes(MAGIC), Ordering::Relaxed);
        log.word(CAPACITY_OFFSET)
            .store(capacity.get(), Ordering::Relaxed);
        Ok(log)
    }

    pub(crate) fn record(&self, entered: MonotonicNs, host: HostCall) {
        let index = self.word(WRITTEN_OFFSET).fetch_add(1, Ordering::Relaxed);
        let slot = HEADER_BYTES + ENTRY_BYTES * (index % self.capacity) as usize;
        let seam = host.started.nanos_since(entered);
        let driver = host.ended.nanos_since(host.started);
        self.word(slot).store(entered.0, Ordering::Relaxed);
        self.word(slot + WORD_BYTES).store(
            u64::from(seam) | (u64::from(driver) << 32),
            Ordering::Relaxed,
        );
    }

    fn word(&self, offset: usize) -> &AtomicU64 {
        assert!(offset.is_multiple_of(WORD_BYTES) && offset + WORD_BYTES <= self.len);
        unsafe { &*self.mapping.as_ptr().add(offset).cast::<AtomicU64>() }
    }
}

impl Drop for FrameLog {
    fn drop(&mut self) {
        if let Err(errno) = unsafe { munmap(self.mapping.as_ptr().cast(), self.len) } {
            panic!("cannot unmap the frame-time log: {errno}");
        }
    }
}

#[cfg(test)]
pub(crate) mod tests {
    use super::*;
    use crate::loader::link::tests::temp_dir;
    use std::os::unix::fs::PermissionsExt as _;

    #[derive(Debug, Clone, Copy, PartialEq, Eq)]
    pub(crate) struct Entry {
        pub(crate) entered: u64,
        pub(crate) seam: u32,
        pub(crate) driver: u32,
    }

    pub(crate) struct Ring {
        pub(crate) capacity: u64,
        pub(crate) written: u64,
        pub(crate) entries: Vec<Entry>,
    }

    fn u64_at(bytes: &[u8], at: usize) -> u64 {
        u64::from_le_bytes(bytes[at..at + 8].try_into().unwrap())
    }

    fn u32_at(bytes: &[u8], at: usize) -> u32 {
        u32::from_le_bytes(bytes[at..at + 4].try_into().unwrap())
    }

    pub(crate) fn read_ring(path: &Path) -> Ring {
        let bytes = std::fs::read(path).expect("read the frame-time log");
        assert_eq!(&bytes[..8], b"ECLFRAME");
        assert!(bytes[24..64].iter().all(|&byte| byte == 0));
        let capacity = u64_at(&bytes, 8);
        assert_eq!(bytes.len() as u64, 64 + 16 * capacity);
        let entries = bytes[64..]
            .as_chunks::<16>()
            .0
            .iter()
            .map(|entry| Entry {
                entered: u64_at(entry, 0),
                seam: u32_at(entry, 8),
                driver: u32_at(entry, 12),
            })
            .collect();
        Ring {
            capacity,
            written: u64_at(&bytes, 16),
            entries,
        }
    }

    pub(crate) fn nanos(time: MonotonicNs) -> u64 {
        time.0
    }

    fn capacity(slots: u64) -> NonZeroU64 {
        NonZeroU64::new(slots).unwrap()
    }

    fn present(entered: u64, seam: u64, driver: u64) -> (MonotonicNs, HostCall) {
        let started = entered + seam;
        (
            MonotonicNs(entered),
            HostCall {
                started: MonotonicNs(started),
                ended: MonotonicNs(started + driver),
            },
        )
    }

    #[test]
    fn a_full_ring_overwrites_its_oldest_entries_in_place() {
        let dir = temp_dir("frame-log-wrap");
        let path = dir.join("frames.bin");
        let log = FrameLog::create(&path, capacity(4)).unwrap();
        let entry = |i: u64| Entry {
            entered: 1_000 * (i + 1),
            seam: 10 + i as u32,
            driver: 100 + i as u32,
        };

        for i in 0..6 {
            let Entry {
                entered,
                seam,
                driver,
            } = entry(i);
            let (entered, host) = present(entered, seam.into(), driver.into());
            log.record(entered, host);
        }
        let ring = read_ring(&path);
        let mode = std::fs::metadata(&path).unwrap().permissions().mode();
        drop(log);
        std::fs::remove_dir_all(&dir).unwrap();

        assert_eq!((ring.capacity, ring.written), (4, 6));
        assert_eq!(
            ring.entries,
            [entry(4), entry(5), entry(2), entry(3)],
            "the fifth and sixth presents replace the first two, and the others stay in place"
        );
        assert_eq!(mode & 0o777, 0o600, "only the user can read the log");
    }

    #[test]
    fn durations_beyond_the_u32_range_saturate() {
        let dir = temp_dir("frame-log-saturate");
        let path = dir.join("frames.bin");
        let log = FrameLog::create(&path, capacity(2)).unwrap();

        let (entered, host) = present(7, u64::from(u32::MAX) - 1, 5 * NANOS_PER_SECOND);
        log.record(entered, host);
        let (entered, host) = present(9, u64::from(u32::MAX) + 1, u64::from(u32::MAX));
        log.record(entered, host);
        let ring = read_ring(&path);
        drop(log);
        std::fs::remove_dir_all(&dir).unwrap();

        assert_eq!(
            ring.entries,
            [
                Entry {
                    entered: 7,
                    seam: u32::MAX - 1,
                    driver: u32::MAX,
                },
                Entry {
                    entered: 9,
                    seam: u32::MAX,
                    driver: u32::MAX,
                },
            ]
        );
    }

    #[test]
    fn a_log_path_that_cannot_hold_a_new_log_fails_with_an_error_naming_it() {
        let dir = temp_dir("frame-log-paths");
        let existing = dir.join("frames.bin");
        std::fs::write(&existing, b"another run").unwrap();
        let missing_parent = dir.join("missing").join("frames.bin");
        let relative = PathBuf::from("frames.bin");

        let errors = [&relative, &existing, &missing_parent].map(|path| {
            FrameLog::create(path, capacity(4))
                .err()
                .expect("the log is refused")
        });
        let kept = std::fs::read(&existing).unwrap();
        std::fs::remove_dir_all(&dir).unwrap();

        assert!(matches!(errors[0], FrameLogError::RelativePath(_)));
        assert!(matches!(errors[1], FrameLogError::Exists(_)));
        assert!(matches!(
            errors[2],
            FrameLogError::Io {
                operation: "create",
                ..
            }
        ));
        for (error, path) in errors.iter().zip([&relative, &existing, &missing_parent]) {
            assert!(
                error.to_string().contains(&path.display().to_string()),
                "{error}"
            );
        }
        assert_eq!(
            kept, b"another run",
            "an existing file is never truncated, because a live Eclipse may have it mapped"
        );
    }

    #[test]
    fn a_log_that_cannot_be_allocated_fails_and_leaves_no_file_behind() {
        let dir = temp_dir("frame-log-too-large");
        let path = dir.join("frames.bin");

        let error = FrameLog::create(&path, capacity(1 << 59))
            .err()
            .expect("the kernel refuses to allocate a file longer than i64::MAX bytes");
        let left_behind = path.exists();
        std::fs::remove_dir_all(&dir).unwrap();

        assert!(
            matches!(
                error,
                FrameLogError::Io {
                    operation: "allocate",
                    ..
                }
            ),
            "{error}"
        );
        assert!(
            error.to_string().contains(&path.display().to_string()),
            "{error}"
        );
        assert!(
            !left_behind,
            "a later run with the same path must not fail because the file exists"
        );
    }
}
