use std::collections::BTreeSet;
use std::ffi::OsStr;
use std::fmt;
use std::fs;
use std::io;
use std::os::unix::fs::{DirBuilderExt as _, MetadataExt as _, PermissionsExt as _};
use std::path::{Path, PathBuf};
use std::time::{Duration, SystemTime};

pub const CLIENT_CACHE_CAP: u64 = 512 * 1024 * 1024;

const TRIM_INTERVAL: Duration = Duration::from_secs(24 * 60 * 60);

const TRIM_MARKER_SUFFIX: &str = ".trimmed";

const CONTENT_PROVIDER_PREFIX: &str = "ContentProvider_";

const OWNER_ONLY: u32 = 0o700;

const STAT_BLOCK_BYTES: u64 = 512;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Trim {
    NotDue,
    Done { freed_bytes: u64 },
}

#[derive(Debug)]
pub struct StorageError {
    action: &'static str,
    path: PathBuf,
    source: io::Error,
}

impl StorageError {
    fn new(action: &'static str, path: &Path, source: io::Error) -> Self {
        Self {
            action,
            path: path.to_path_buf(),
            source,
        }
    }
}

impl fmt::Display for StorageError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(
            f,
            "cannot {} {}: {}",
            self.action,
            self.path.display(),
            self.source
        )
    }
}

impl std::error::Error for StorageError {
    fn source(&self) -> Option<&(dyn std::error::Error + 'static)> {
        Some(&self.source)
    }
}

pub fn create_client_cache(dir: &Path) -> Result<(), StorageError> {
    fs::DirBuilder::new()
        .recursive(true)
        .mode(OWNER_ONLY)
        .create(dir)
        .map_err(|source| StorageError::new("create", dir, source))?;
    fs::set_permissions(dir, fs::Permissions::from_mode(OWNER_ONLY))
        .map_err(|source| StorageError::new("make owner-only", dir, source))
}

pub fn trim_client_cache(dir: &Path, cap: u64, now: SystemTime) -> Result<Trim, StorageError> {
    let marker = trim_marker(dir);
    if !trim_due(&marker, now)? {
        return Ok(Trim::NotDue);
    }
    remove_content_provider_dirs(dir)?;
    let freed_bytes = remove_oldest_files_over(dir, cap)?;
    fs::File::create(&marker)
        .and_then(|file| file.set_modified(now))
        .map_err(|source| StorageError::new("record the trim in", &marker, source))?;
    Ok(Trim::Done { freed_bytes })
}

fn trim_marker(dir: &Path) -> PathBuf {
    let mut marker = dir.as_os_str().to_owned();
    marker.push(TRIM_MARKER_SUFFIX);
    PathBuf::from(marker)
}

fn trim_due(marker: &Path, now: SystemTime) -> Result<bool, StorageError> {
    let trimmed_at = match fs::metadata(marker).and_then(|metadata| metadata.modified()) {
        Ok(trimmed_at) => trimmed_at,
        Err(error) if error.kind() == io::ErrorKind::NotFound => return Ok(true),
        Err(source) => return Err(StorageError::new("read the trim time of", marker, source)),
    };
    Ok(!now
        .duration_since(trimmed_at)
        .is_ok_and(|age| age < TRIM_INTERVAL))
}

fn remove_content_provider_dirs(dir: &Path) -> Result<(), StorageError> {
    for entry in fs::read_dir(dir).map_err(|source| StorageError::new("read", dir, source))? {
        let entry = entry.map_err(|source| StorageError::new("read", dir, source))?;
        if !is_content_provider_dir_name(&entry.file_name()) {
            continue;
        }
        let path = entry.path();
        let file_type = entry
            .file_type()
            .map_err(|source| StorageError::new("inspect", &path, source))?;
        if file_type.is_dir() {
            fs::remove_dir_all(&path)
                .map_err(|source| StorageError::new("remove", &path, source))?;
        }
    }
    Ok(())
}

fn is_content_provider_dir_name(name: &OsStr) -> bool {
    name.to_str()
        .and_then(|name| name.strip_prefix(CONTENT_PROVIDER_PREFIX))
        .is_some_and(|pid| !pid.is_empty() && pid.bytes().all(|byte| byte.is_ascii_digit()))
}

struct CachedFile {
    path: PathBuf,
    modified: SystemTime,
    allocated: u64,
}

fn remove_oldest_files_over(dir: &Path, cap: u64) -> Result<u64, StorageError> {
    let (mut allocated, mut files) = cached_files(dir)?;
    if allocated <= cap {
        return Ok(0);
    }
    let target = cap / 4 * 3;
    files.sort_by(|a, b| {
        a.modified
            .cmp(&b.modified)
            .then_with(|| a.path.cmp(&b.path))
    });
    let mut freed = 0;
    let mut emptied = BTreeSet::new();
    for file in files {
        if allocated <= target {
            break;
        }
        fs::remove_file(&file.path)
            .map_err(|source| StorageError::new("remove", &file.path, source))?;
        allocated = allocated.saturating_sub(file.allocated);
        freed += file.allocated;
        if let Some(parent) = file.path.parent() {
            emptied.insert(parent.to_path_buf());
        }
    }
    remove_emptied_dirs(dir, &emptied)?;
    Ok(freed)
}

fn cached_files(root: &Path) -> Result<(u64, Vec<CachedFile>), StorageError> {
    let mut allocated = 0;
    let mut files = Vec::new();
    let mut pending = vec![root.to_path_buf()];
    while let Some(dir) = pending.pop() {
        for entry in fs::read_dir(&dir).map_err(|source| StorageError::new("read", &dir, source))? {
            let entry = entry.map_err(|source| StorageError::new("read", &dir, source))?;
            let path = entry.path();
            let metadata = entry
                .metadata()
                .map_err(|source| StorageError::new("inspect", &path, source))?;
            let entry_allocated = metadata.blocks() * STAT_BLOCK_BYTES;
            allocated += entry_allocated;
            if metadata.is_dir() {
                pending.push(path);
            } else if metadata.is_file() {
                let modified = metadata
                    .modified()
                    .map_err(|source| StorageError::new("inspect", &path, source))?;
                files.push(CachedFile {
                    path,
                    modified,
                    allocated: entry_allocated,
                });
            }
        }
    }
    Ok((allocated, files))
}

fn remove_emptied_dirs(root: &Path, emptied: &BTreeSet<PathBuf>) -> Result<(), StorageError> {
    for dir in emptied.iter().rev() {
        let mut current = dir.as_path();
        while current != root && current.starts_with(root) {
            match fs::remove_dir(current) {
                Ok(()) => {}
                Err(error)
                    if matches!(
                        error.kind(),
                        io::ErrorKind::DirectoryNotEmpty | io::ErrorKind::NotFound
                    ) =>
                {
                    break;
                }
                Err(source) => return Err(StorageError::new("remove", current, source)),
            }
            let Some(parent) = current.parent() else {
                break;
            };
            current = parent;
        }
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    const KIB: u64 = 1024;

    struct Scratch(PathBuf);

    impl Scratch {
        fn new(tag: &str) -> Self {
            let root = std::env::temp_dir().join(format!(
                "eclipse-storage-{tag}-{}-{:?}",
                std::process::id(),
                std::thread::current().id()
            ));
            fs::remove_dir_all(&root).ok();
            fs::create_dir_all(&root).unwrap();
            Self(root)
        }

        fn cache(&self) -> PathBuf {
            self.0.join("client-cache")
        }
    }

    impl Drop for Scratch {
        fn drop(&mut self) {
            fs::remove_dir_all(&self.0).ok();
        }
    }

    fn incompressible(len: u64, seed: u64) -> Vec<u8> {
        let mut state = seed.wrapping_mul(0x9E37_79B9_7F4A_7C15) | 1;
        (0..len)
            .map(|_| {
                state ^= state << 13;
                state ^= state >> 7;
                state ^= state << 17;
                state.to_le_bytes()[0]
            })
            .collect()
    }

    fn write_aged(path: &Path, len: u64, modified: SystemTime) -> u64 {
        use std::io::Write as _;
        let seed = modified
            .duration_since(SystemTime::UNIX_EPOCH)
            .unwrap()
            .as_secs();
        fs::create_dir_all(path.parent().unwrap()).unwrap();
        let mut file = fs::File::create(path).unwrap();
        file.write_all(&incompressible(len, seed)).unwrap();
        file.sync_all().unwrap();
        file.set_modified(modified).unwrap();
        file.metadata().unwrap().blocks() * STAT_BLOCK_BYTES
    }

    fn first_launch() -> SystemTime {
        SystemTime::UNIX_EPOCH + Duration::from_secs(1_790_000_000)
    }

    #[test]
    fn trim_removes_oldest_files_down_to_three_quarters_of_the_cap() {
        let scratch = Scratch::new("cap");
        let cache = scratch.cache();
        create_client_cache(&cache).unwrap();
        fs::create_dir_all(cache.join("sounds")).unwrap();
        let names = [
            "rbx-storage/0a/oldest",
            "rbx-storage/0a/second",
            "http/third",
            "fourth",
            "rbx-storage/1b/fifth",
            "rbx-storage/1b/sixth",
            "http/seventh",
            "newest",
        ];
        let allocated: Vec<u64> = names
            .iter()
            .enumerate()
            .map(|(age, name)| {
                let modified = first_launch() + Duration::from_secs(60 * age as u64);
                write_aged(&cache.join(name), 256 * KIB, modified)
            })
            .collect();

        let trimmed = trim_client_cache(
            &cache,
            1536 * KIB,
            first_launch() + Duration::from_secs(3600),
        );

        assert_eq!(
            trimmed.unwrap(),
            Trim::Done {
                freed_bytes: allocated[..4].iter().sum()
            }
        );
        for name in &names[..4] {
            assert!(
                !cache.join(name).exists(),
                "{name} is among the four oldest"
            );
        }
        for name in &names[4..] {
            assert!(
                cache.join(name).is_file(),
                "{name} is among the four newest"
            );
        }
        assert!(
            !cache.join("rbx-storage/0a").exists(),
            "a directory the trim emptied is removed"
        );
        assert!(
            cache.join("http").is_dir(),
            "http still holds the seventh file"
        );
        assert!(
            cache.join("sounds").is_dir(),
            "a directory the client left empty is kept"
        );
        assert!(cache.is_dir());
    }

    #[test]
    fn a_cache_under_the_cap_keeps_every_file() {
        let scratch = Scratch::new("under");
        let cache = scratch.cache();
        create_client_cache(&cache).unwrap();
        write_aged(&cache.join("rbx-storage/0a/kept"), 64 * KIB, first_launch());

        let trimmed = trim_client_cache(&cache, 1536 * KIB, first_launch());

        assert_eq!(trimmed.unwrap(), Trim::Done { freed_bytes: 0 });
        assert!(cache.join("rbx-storage/0a/kept").is_file());
    }

    #[test]
    fn trim_runs_at_most_once_a_day() {
        let scratch = Scratch::new("daily");
        let cache = scratch.cache();
        create_client_cache(&cache).unwrap();
        let first = first_launch();
        assert_eq!(
            trim_client_cache(&cache, 768 * KIB, first).unwrap(),
            Trim::Done { freed_bytes: 0 }
        );
        for (age, name) in ["old", "middle", "new", "newest"].iter().enumerate() {
            let modified = first + Duration::from_secs(60 * age as u64);
            write_aged(&cache.join(name), 256 * KIB, modified);
        }

        let later_that_day = first + TRIM_INTERVAL - Duration::from_secs(1);
        assert_eq!(
            trim_client_cache(&cache, 768 * KIB, later_that_day).unwrap(),
            Trim::NotDue
        );
        assert!(cache.join("old").is_file());

        let next_day = first + TRIM_INTERVAL;
        assert!(matches!(
            trim_client_cache(&cache, 768 * KIB, next_day).unwrap(),
            Trim::Done { freed_bytes } if freed_bytes > 0
        ));
        assert!(!cache.join("old").exists());
        assert!(cache.join("newest").is_file());
        assert_eq!(
            trim_client_cache(&cache, 768 * KIB, next_day + Duration::from_secs(60)).unwrap(),
            Trim::NotDue
        );
    }

    #[test]
    fn a_trim_time_in_the_future_does_not_stop_trimming() {
        let scratch = Scratch::new("clock");
        let cache = scratch.cache();
        create_client_cache(&cache).unwrap();
        let future = first_launch() + 10 * TRIM_INTERVAL;
        trim_client_cache(&cache, 256 * KIB, future).unwrap();

        assert_eq!(
            trim_client_cache(&cache, 256 * KIB, first_launch()).unwrap(),
            Trim::Done { freed_bytes: 0 }
        );
    }

    #[test]
    fn stale_content_provider_directories_are_removed() {
        let scratch = Scratch::new("providers");
        let cache = scratch.cache();
        create_client_cache(&cache).unwrap();
        for dir in ["ContentProvider_74084", "ContentProvider_12"] {
            write_aged(&cache.join(dir).join("shared.png"), KIB, first_launch());
        }
        for kept in ["ContentProvider_", "ContentProvider_12x", "rbx-storage"] {
            fs::create_dir_all(cache.join(kept)).unwrap();
        }
        fs::write(
            cache.join("ContentProvider_99"),
            b"a file, not a provider directory",
        )
        .unwrap();

        trim_client_cache(&cache, CLIENT_CACHE_CAP, first_launch()).unwrap();

        assert!(!cache.join("ContentProvider_74084").exists());
        assert!(!cache.join("ContentProvider_12").exists());
        for kept in [
            "ContentProvider_",
            "ContentProvider_12x",
            "rbx-storage",
            "ContentProvider_99",
        ] {
            assert!(cache.join(kept).exists(), "{kept}");
        }
    }

    #[test]
    fn the_client_cache_is_created_owner_only() {
        let scratch = Scratch::new("mode");
        let cache = scratch.cache();
        fs::create_dir_all(&cache).unwrap();
        fs::set_permissions(&cache, fs::Permissions::from_mode(0o755)).unwrap();

        create_client_cache(&cache).unwrap();

        let mode = fs::metadata(&cache).unwrap().permissions().mode();
        assert_eq!(mode & 0o777, 0o700);
    }
}
