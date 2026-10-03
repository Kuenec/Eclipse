#![forbid(unsafe_code)]

pub mod apkcombo;
pub mod arsc;
pub mod axml;
pub mod cache;
mod file_reader;
mod intent_filter;
mod locale_data;
pub mod play;
pub mod res_config;
pub mod signature;
pub mod store;

use std::collections::HashSet;
use std::fmt;
use std::fs::{File, Metadata, OpenOptions};
use std::io::{self, Read};
use std::os::fd::AsRawFd;
use std::os::unix::ffi::OsStrExt as _;
use std::os::unix::fs::MetadataExt as _;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::Arc;
use std::time::Instant;

use crc32fast::Hasher as Crc32;
use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};
use zip::{CompressionMethod, ZipArchive};

use axml::AxmlError;
use file_reader::ApkFileReader;
use intent_filter::{ViewHandler, ViewUri};
use signature::{SignatureError, SigningCertificateHistory};

use crate::status::{StatusSink, WINDOW_PROGRESS_INTERVAL};

const MANIFEST_ENTRY: &str = "AndroidManifest.xml";

pub const ENGINE_LIB: &str = "libroblox.so";

pub const TARGET_ABI: &str = "x86_64";

pub const ROBLOX_PACKAGE: &str = "com.roblox.client";

pub const BASE_APK: &str = "base.apk";

pub const NATIVE_SPLIT_APK: &str = "split_config.x86_64.apk";

pub const DEV_APK_ENV: &str = "ECLIPSE_ROBLOX_APK";

const NATIVE_SPLIT_NAME: &str = "config.x86_64";

const MAX_APK_BYTES: u64 = 1024 * 1024 * 1024;

const MAX_MANIFEST_BYTES: u64 = 4 * 1024 * 1024;

const BUNDLE_EXTENSIONS: [&str; 3] = ["apks", "xapk", "apkm"];

const READ_ENTRY_PREALLOC_CAP: u64 = 8 * 1024 * 1024;

const EXTRACTED_ENTRY_HASH_BUFFER_SIZE: usize = 64 * 1024;

const EXTRACTION_PREFIX: &str = ".eclipse-extract.";

const EXTRACTION_LOCK: &str = ".eclipse-extract.lock";

const EXTRACTION_STAMP: &str = ".eclipse-extract.stamp";

const EXTRACTION_TEMP_SUFFIX: &str = ".partial";

const SYNC_WORKERS: usize = 16;

fn decode_modified_utf8(data: &[u8]) -> Option<String> {
    if let Ok(text) = std::str::from_utf8(data) {
        return Some(text.to_owned());
    }
    let mut units = Vec::with_capacity(data.len());
    let mut rest = data;
    while let Some((&lead, tail)) = rest.split_first() {
        let (continuation_len, initial, minimum) = match lead {
            0x00..=0x7F => (0, u32::from(lead), 0),
            0xC0..=0xDF => (1, u32::from(lead & 0x1F), 0x80),
            0xE0..=0xEF => (2, u32::from(lead & 0x0F), 0x800),
            0xF0..=0xF7 => (3, u32::from(lead & 0x07), 0x1_0000),
            _ => return None,
        };
        let (continuation, next) = tail.split_at_checked(continuation_len)?;
        let mut code = initial;
        for &byte in continuation {
            if byte & 0xC0 != 0x80 {
                return None;
            }
            code = (code << 6) | u32::from(byte & 0x3F);
        }
        let encoded_nul = continuation_len == 1 && code == 0;
        if code < minimum && !encoded_nul {
            return None;
        }
        match char::from_u32(code) {
            Some(character) => units.extend_from_slice(character.encode_utf16(&mut [0; 2])),
            None => units.push(u16::try_from(code).ok()?),
        }
        rest = next;
    }
    Some(String::from_utf16_lossy(&units))
}

fn io_context(error: io::Error, operation: &str, path: &Path) -> io::Error {
    io::Error::new(
        error.kind(),
        format!("{operation} {}: {error}", path.display()),
    )
}

fn extracted_entry_matches(path: &Path, size: u64, crc32: u32) -> io::Result<bool> {
    let mut file = match File::open(path) {
        Ok(file) => file,
        Err(error) if error.kind() == io::ErrorKind::NotFound => return Ok(false),
        Err(error) => return Err(io_context(error, "open", path)),
    };
    let metadata = file
        .metadata()
        .map_err(|error| io_context(error, "stat", path))?;
    if metadata.len() != size {
        return Ok(false);
    }

    let mut hasher = Crc32::new();
    let mut buffer = [0_u8; EXTRACTED_ENTRY_HASH_BUFFER_SIZE];
    loop {
        let read = file
            .read(&mut buffer)
            .map_err(|error| io_context(error, "read", path))?;
        if read == 0 {
            break;
        }
        hasher.update(&buffer[..read]);
    }
    Ok(hasher.finalize() == crc32)
}

struct PlannedEntry {
    name: String,
    dest: PathBuf,
    size: u64,
    crc32: u32,
}

struct ExtractionProgress<'a> {
    status: &'a StatusSink,
    done: u64,
    total: u64,
    last_update: Instant,
}

impl<'a> ExtractionProgress<'a> {
    fn start(status: &'a StatusSink, planned: &[PlannedEntry]) -> Self {
        let total = planned
            .iter()
            .fold(0_u64, |total, entry| total.saturating_add(entry.size));
        status.extraction(0, total);
        Self {
            status,
            done: 0,
            total,
            last_update: Instant::now(),
        }
    }

    fn advance(&mut self, bytes: u64) {
        self.done = self.done.saturating_add(bytes);
        let now = Instant::now();
        if now.duration_since(self.last_update) >= WINDOW_PROGRESS_INTERVAL
            || self.done == self.total
        {
            self.last_update = now;
            self.status.extraction(self.done, self.total);
        }
    }
}

fn sync_extracted(files: &[&Path]) -> io::Result<()> {
    let next = AtomicUsize::new(0);
    let sync_remaining = || -> io::Result<()> {
        while let Some(path) = files.get(next.fetch_add(1, Ordering::Relaxed)) {
            File::open(path)
                .and_then(|opened| opened.sync_all())
                .map_err(|error| io_context(error, "sync", path))?;
        }
        Ok(())
    };
    std::thread::scope(|scope| {
        let helpers = (1..SYNC_WORKERS.min(files.len()))
            .map(|_| {
                std::thread::Builder::new()
                    .name("eclipse-sync".to_owned())
                    .spawn_scoped(scope, sync_remaining)
                    .map_err(|error| {
                        io::Error::new(
                            error.kind(),
                            format!("start a thread to sync extracted files: {error}"),
                        )
                    })
            })
            .collect::<io::Result<Vec<_>>>()?;
        let synced = sync_remaining();
        for helper in helpers {
            helper
                .join()
                .unwrap_or_else(|panic| std::panic::resume_unwind(panic))?;
        }
        synced
    })
}

fn lock_extraction_dir(dir: &Path) -> io::Result<File> {
    let path = dir.join(EXTRACTION_LOCK);
    let context = |error| io_context(error, "lock", &path);
    let lock = OpenOptions::new()
        .create(true)
        .truncate(false)
        .write(true)
        .open(&path)
        .map_err(context)?;
    lock.lock().map_err(context)?;
    Ok(lock)
}

fn extraction_digest(
    source_path: &Path,
    source: &Metadata,
    planned: &[PlannedEntry],
) -> io::Result<Option<Vec<u8>>> {
    let mut digest = Sha256::new();
    digest.update(source_path.as_os_str().as_bytes());
    digest.update([0]);
    FileIdentity::of(source).hash_into(&mut digest);
    for entry in planned {
        let extracted = match std::fs::metadata(&entry.dest) {
            Ok(extracted) => extracted,
            Err(error)
                if matches!(
                    error.kind(),
                    io::ErrorKind::NotFound | io::ErrorKind::NotADirectory
                ) =>
            {
                return Ok(None)
            }
            Err(error) => return Err(io_context(error, "stat", &entry.dest)),
        };
        if !extracted.is_file() || extracted.len() != entry.size {
            return Ok(None);
        }
        digest.update(entry.name.as_bytes());
        digest.update([0]);
        digest.update(entry.size.to_le_bytes());
        digest.update(entry.crc32.to_le_bytes());
        FileIdentity::of(&extracted).hash_into(&mut digest);
    }
    Ok(Some(digest.finalize().to_vec()))
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
struct FileIdentity {
    dev: u64,
    ino: u64,
    size: u64,
    mtime: i64,
    mtime_nsec: i64,
    ctime: i64,
    ctime_nsec: i64,
}

impl FileIdentity {
    fn of(metadata: &Metadata) -> Self {
        Self {
            dev: metadata.dev(),
            ino: metadata.ino(),
            size: metadata.size(),
            mtime: metadata.mtime(),
            mtime_nsec: metadata.mtime_nsec(),
            ctime: metadata.ctime(),
            ctime_nsec: metadata.ctime_nsec(),
        }
    }

    fn hash_into(&self, digest: &mut Sha256) {
        for field in [self.dev, self.ino, self.size] {
            digest.update(field.to_le_bytes());
        }
        for field in [self.mtime, self.mtime_nsec, self.ctime, self.ctime_nsec] {
            digest.update(field.to_le_bytes());
        }
    }
}

fn read_stamp(path: &Path) -> io::Result<Option<Vec<u8>>> {
    match std::fs::read(path) {
        Ok(stamp) => Ok(Some(stamp)),
        Err(error) if error.kind() == io::ErrorKind::NotFound => Ok(None),
        Err(error) => Err(io_context(error, "read", path)),
    }
}

fn remove_stale_temporaries(dir: &Path) -> io::Result<()> {
    let list_error = |error| io_context(error, "list", dir);
    for entry in std::fs::read_dir(dir).map_err(list_error)? {
        let entry = entry.map_err(list_error)?;
        let name = entry.file_name();
        let stale = name.to_str().is_some_and(|name| {
            name.starts_with(EXTRACTION_PREFIX) && name.ends_with(EXTRACTION_TEMP_SUFFIX)
        });
        if stale {
            let path = entry.path();
            std::fs::remove_file(&path)
                .map_err(|error| io_context(error, "remove stale", &path))?;
        }
    }
    Ok(())
}

#[derive(Clone, Copy)]
enum UnplannedEntries {
    Keep,
    Remove,
}

fn planned_subdirectories<'a>(dir: &Path, planned: &'a [PlannedEntry]) -> HashSet<&'a Path> {
    planned
        .iter()
        .flat_map(|entry| {
            entry
                .dest
                .ancestors()
                .skip(1)
                .take_while(|ancestor| *ancestor != dir)
        })
        .collect()
}

fn extraction_directories<'a>(dest_dir: &'a Path, planned: &'a [PlannedEntry]) -> Vec<&'a Path> {
    planned_subdirectories(dest_dir, planned)
        .into_iter()
        .chain([dest_dir])
        .collect()
}

fn remove_unplanned_entries(dir: &Path, planned: &[PlannedEntry]) -> io::Result<()> {
    let bookkeeping = [dir.join(EXTRACTION_LOCK), dir.join(EXTRACTION_STAMP)];
    let files: HashSet<&Path> = planned
        .iter()
        .map(|entry| entry.dest.as_path())
        .chain(bookkeeping.iter().map(PathBuf::as_path))
        .collect();
    remove_unplanned_in(dir, &files, &planned_subdirectories(dir, planned))
}

fn remove_unplanned_in(
    dir: &Path,
    files: &HashSet<&Path>,
    directories: &HashSet<&Path>,
) -> io::Result<()> {
    let list_error = |error| io_context(error, "list", dir);
    for entry in std::fs::read_dir(dir).map_err(list_error)? {
        let entry = entry.map_err(list_error)?;
        let path = entry.path();
        let remove_error = |error| io_context(error, "remove stale", &path);
        let file_type = entry
            .file_type()
            .map_err(|error| io_context(error, "stat", &path))?;
        if file_type.is_dir() && directories.contains(path.as_path()) {
            remove_unplanned_in(&path, files, directories)?;
        } else if file_type.is_dir() {
            std::fs::remove_dir_all(&path).map_err(remove_error)?;
        } else if !files.contains(path.as_path()) {
            std::fs::remove_file(&path).map_err(remove_error)?;
        }
    }
    Ok(())
}

#[derive(Clone)]
pub struct Apk {
    path: PathBuf,
    file: Arc<File>,
    archive: ZipArchive<ApkFileReader>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Manifest {
    pub package: String,

    pub launcher_activity: String,

    pub min_sdk: Option<u32>,

    pub target_sdk: Option<u32>,

    pub large_heap: bool,

    pub(crate) view_handlers: Vec<ViewHandler>,
}

impl Manifest {
    pub fn resolve_view_activity(&self, uri: &str) -> Option<&str> {
        let uri = ViewUri::parse(uri)?;
        self.view_handlers
            .iter()
            .find(|handler| handler.accepts(&uri))
            .map(|handler| handler.activity.as_str())
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash, Serialize, Deserialize)]
#[serde(transparent)]
pub struct VersionCode(pub u32);

impl fmt::Display for VersionCode {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "{}", self.0)
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct PackageInfo {
    pub package: String,

    pub version_code: Option<VersionCode>,

    pub version_name: Option<String>,

    pub split: Option<String>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct X8664Engine {
    pub entry: String,

    pub size: u64,

    pub stored: bool,
}

impl Apk {
    pub fn open(path: &Path) -> Result<Self, ApkError> {
        Self::from_file(path, File::open(path)?)
    }

    fn from_file(path: &Path, file: File) -> Result<Self, ApkError> {
        let file = Arc::new(file);
        let archive = ZipArchive::new(ApkFileReader::new(Arc::clone(&file)))?;
        Ok(Self {
            path: path.to_path_buf(),
            file,
            archive,
        })
    }

    pub fn manifest(&mut self) -> Result<Manifest, ApkError> {
        let bytes = self.manifest_bytes()?;
        let parsed = axml::read_manifest(&bytes)?;
        Ok(Manifest {
            package: parsed.package,
            launcher_activity: parsed.launcher_activity.ok_or(AxmlError::NoLauncher)?,
            min_sdk: parsed.min_sdk,
            target_sdk: parsed.target_sdk,
            large_heap: parsed.large_heap,
            view_handlers: parsed.view_handlers,
        })
    }

    pub fn package_info(&mut self) -> Result<PackageInfo, ApkError> {
        let bytes = self.manifest_bytes()?;
        let parsed = axml::read_manifest(&bytes)?;
        Ok(PackageInfo {
            package: parsed.package,
            version_code: parsed.version_code.map(VersionCode),
            version_name: parsed.version_name,
            split: parsed.split,
        })
    }

    pub fn native_lib_filenames(&self, abi: &str) -> Vec<String> {
        let prefix = format!("lib/{abi}/");
        let mut names: Vec<String> = self
            .archive
            .file_names()
            .filter_map(|n| n.strip_prefix(&prefix))
            .filter(|rest| !rest.is_empty() && !rest.contains('/') && rest.ends_with(".so"))
            .map(str::to_owned)
            .collect();
        names.sort();
        names
    }

    pub fn x86_64_engine(&mut self) -> Result<X8664Engine, ApkError> {
        let entry = format!("lib/{TARGET_ABI}/{ENGINE_LIB}");
        let file = match self.archive.by_name(&entry) {
            Ok(f) => f,
            Err(zip::result::ZipError::FileNotFound) => {
                return Err(ApkError::EngineMissing);
            }
            Err(e) => return Err(ApkError::Zip(e)),
        };
        Ok(X8664Engine {
            stored: file.compression() == CompressionMethod::Stored,
            size: file.size(),
            entry,
        })
    }

    pub fn path(&self) -> &Path {
        &self.path
    }

    pub fn file(&self) -> &Arc<File> {
        &self.file
    }

    pub fn extract_native_libs(
        &mut self,
        abi: &str,
        dest_dir: &Path,
        status: &StatusSink,
    ) -> Result<usize, ApkError> {
        let prefix = format!("lib/{abi}/");

        let names: Vec<String> = self
            .archive
            .file_names()
            .filter(|n| n.starts_with(&prefix) && n.ends_with(".so"))
            .map(str::to_owned)
            .collect();
        let mut planned = Vec::with_capacity(names.len());
        for name in names {
            let base = name.rsplit('/').next().unwrap_or(name.as_str());
            let dest = dest_dir.join(base);
            let entry = self.archive.by_name(&name)?;
            let (size, crc32) = (entry.size(), entry.crc32());
            planned.push(PlannedEntry {
                name,
                dest,
                size,
                crc32,
            });
        }
        self.extract_planned(dest_dir, &planned, UnplannedEntries::Keep, status)
    }

    pub fn extract_assets(
        &mut self,
        dest_dir: &Path,
        status: &StatusSink,
    ) -> Result<usize, ApkError> {
        const PREFIX: &str = "assets/";

        let names: Vec<String> = self
            .archive
            .file_names()
            .filter(|n| n.starts_with(PREFIX) && !n.ends_with('/'))
            .map(str::to_owned)
            .collect();
        let mut planned = Vec::with_capacity(names.len());
        for name in names {
            let entry = self.archive.by_name(&name)?;

            let Some(safe) = entry.enclosed_name() else {
                continue;
            };
            let Ok(rel) = safe.strip_prefix(PREFIX) else {
                continue;
            };
            if rel.as_os_str().is_empty() {
                continue;
            }
            let bookkeeping = rel.parent() == Some(Path::new(""))
                && rel
                    .to_str()
                    .is_some_and(|file_name| file_name.starts_with(EXTRACTION_PREFIX));
            if bookkeeping {
                return Err(io::Error::new(
                    io::ErrorKind::InvalidData,
                    format!(
                        "APK asset {name} would overwrite Eclipse's extraction bookkeeping in {}",
                        dest_dir.display()
                    ),
                )
                .into());
            }
            let dest = dest_dir.join(rel);
            let (size, crc32) = (entry.size(), entry.crc32());
            planned.push(PlannedEntry {
                name,
                dest,
                size,
                crc32,
            });
        }
        self.extract_planned(dest_dir, &planned, UnplannedEntries::Remove, status)
    }

    fn extract_planned(
        &mut self,
        dest_dir: &Path,
        planned: &[PlannedEntry],
        unplanned: UnplannedEntries,
        status: &StatusSink,
    ) -> Result<usize, ApkError> {
        std::fs::create_dir_all(dest_dir).map_err(|error| io_context(error, "create", dest_dir))?;
        let _lock = lock_extraction_dir(dest_dir)?;
        let source = self
            .file
            .metadata()
            .map_err(|error| io_context(error, "stat", &self.path))?;
        let stamp_path = dest_dir.join(EXTRACTION_STAMP);
        let current = extraction_digest(&self.path, &source, planned)?;
        if current.is_some() && current == read_stamp(&stamp_path)? {
            return Ok(0);
        }

        match unplanned {
            UnplannedEntries::Keep => remove_stale_temporaries(dest_dir)?,
            UnplannedEntries::Remove => remove_unplanned_entries(dest_dir, planned)?,
        }
        let temporary = dest_dir.join(format!(
            "{EXTRACTION_PREFIX}{}{EXTRACTION_TEMP_SUFFIX}",
            std::process::id()
        ));
        let mut progress = ExtractionProgress::start(status, planned);
        let mut written = 0;
        for planned_entry in planned {
            if !extracted_entry_matches(
                &planned_entry.dest,
                planned_entry.size,
                planned_entry.crc32,
            )? {
                self.write_entry(planned_entry, &temporary)?;
                written += 1;
            }
            progress.advance(planned_entry.size);
        }
        let files: Vec<&Path> = planned.iter().map(|entry| entry.dest.as_path()).collect();
        sync_extracted(&files)?;
        sync_extracted(&extraction_directories(dest_dir, planned))?;

        if let Some(stamp) = extraction_digest(&self.path, &source, planned)? {
            std::fs::write(&temporary, stamp)
                .map_err(|error| io_context(error, "write", &temporary))?;
            std::fs::rename(&temporary, &stamp_path).map_err(|error| {
                io_context(
                    error,
                    &format!("rename {} to", temporary.display()),
                    &stamp_path,
                )
            })?;
        }
        Ok(written)
    }

    fn write_entry(&mut self, planned: &PlannedEntry, temporary: &Path) -> Result<(), ApkError> {
        if let Some(parent) = planned.dest.parent() {
            std::fs::create_dir_all(parent).map_err(|error| io_context(error, "create", parent))?;
        }
        let mut entry = self.archive.by_name(&planned.name)?;
        let mut out =
            File::create(temporary).map_err(|error| io_context(error, "create", temporary))?;
        io::copy(&mut entry, &mut out).map_err(|error| {
            io_context(error, &format!("extract {} to", planned.name), temporary)
        })?;
        drop(out);
        std::fs::rename(temporary, &planned.dest).map_err(|error| {
            io_context(
                error,
                &format!("rename {} to", temporary.display()),
                &planned.dest,
            )
        })?;
        Ok(())
    }

    fn manifest_bytes(&mut self) -> Result<Vec<u8>, ApkError> {
        let entry = match self.archive.by_name(MANIFEST_ENTRY) {
            Ok(entry) => entry,
            Err(zip::result::ZipError::FileNotFound) => {
                return Err(ApkError::EntryMissing(MANIFEST_ENTRY.to_owned()));
            }
            Err(error) => return Err(ApkError::Zip(error)),
        };
        let declared = entry.size();
        if declared > MAX_MANIFEST_BYTES {
            return Err(ApkError::ManifestTooLarge(declared));
        }
        let mut bytes = Vec::with_capacity(declared as usize);
        entry.take(declared + 1).read_to_end(&mut bytes)?;
        if bytes.len() as u64 > declared {
            return Err(ApkError::ManifestLongerThanDeclared(declared));
        }
        Ok(bytes)
    }

    pub fn read_entry(&mut self, name: &str) -> Result<Vec<u8>, ApkError> {
        let mut entry = match self.archive.by_name(name) {
            Ok(e) => e,
            Err(zip::result::ZipError::FileNotFound) => {
                return Err(ApkError::EntryMissing(name.to_owned()));
            }
            Err(e) => return Err(ApkError::Zip(e)),
        };

        let cap = entry.size().min(READ_ENTRY_PREALLOC_CAP) as usize;
        let mut buf = Vec::with_capacity(cap);
        entry.read_to_end(&mut buf)?;
        Ok(buf)
    }

    pub fn entry_span(&mut self, name: &str) -> Result<EntrySpan, ApkError> {
        let entry = match self.archive.by_name(name) {
            Ok(e) => e,
            Err(zip::result::ZipError::FileNotFound) => {
                return Err(ApkError::EntryMissing(name.to_owned()));
            }
            Err(e) => return Err(ApkError::Zip(e)),
        };
        let data_start = entry
            .data_start()
            .ok_or_else(|| ApkError::EntryOffsetUnknown(name.to_owned()))?;
        Ok(EntrySpan {
            data_start,
            uncompressed_size: entry.size(),
            stored: entry.compression() == CompressionMethod::Stored,
        })
    }
}

pub fn reopen(file: &File) -> io::Result<File> {
    let link = Path::new("/proc/self/fd").join(file.as_raw_fd().to_string());
    File::open(&link).map_err(|error| {
        io::Error::new(
            error.kind(),
            format!("reopen APK through {}: {error}", link.display()),
        )
    })
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct EntrySpan {
    pub data_start: u64,

    pub uncompressed_size: u64,

    pub stored: bool,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ApkSetPaths {
    pub base: PathBuf,

    pub native_split: Option<PathBuf>,
}

impl ApkSetPaths {
    pub fn locate(path: &Path) -> Result<Self, ApkSetError> {
        let metadata = std::fs::metadata(path).map_err(|source| ApkSetError::Locate {
            path: path.to_path_buf(),
            source,
        })?;
        if !metadata.is_dir() {
            if is_bundle(path) {
                return Err(ApkSetError::Bundle(path.to_path_buf()));
            }
            return Ok(Self {
                base: path.to_path_buf(),
                native_split: None,
            });
        }
        let base = path.join(BASE_APK);
        if !regular_file_exists(&base)? {
            return Err(ApkSetError::MissingBase(path.to_path_buf()));
        }
        let split = path.join(NATIVE_SPLIT_APK);
        let native_split = regular_file_exists(&split)?.then_some(split);
        Ok(Self { base, native_split })
    }

    pub fn from_env() -> Result<Option<Self>, ApkSetError> {
        std::env::var_os(DEV_APK_ENV)
            .filter(|value| !value.is_empty())
            .map(|value| Self::locate(Path::new(&value)))
            .transpose()
    }

    pub fn native_libs(&self) -> &Path {
        self.native_split.as_deref().unwrap_or(&self.base)
    }
}

fn is_bundle(path: &Path) -> bool {
    path.extension()
        .and_then(|extension| extension.to_str())
        .is_some_and(|extension| {
            BUNDLE_EXTENSIONS
                .iter()
                .any(|bundle| extension.eq_ignore_ascii_case(bundle))
        })
}

fn regular_file_exists(path: &Path) -> Result<bool, ApkSetError> {
    match std::fs::metadata(path) {
        Ok(metadata) => Ok(metadata.is_file()),
        Err(error) if error.kind() == io::ErrorKind::NotFound => Ok(false),
        Err(source) => Err(ApkSetError::Locate {
            path: path.to_path_buf(),
            source,
        }),
    }
}

pub struct ApkSet {
    paths: ApkSetPaths,
    base: Apk,
    native_split: Option<Apk>,
    manifest: Manifest,
    version_code: VersionCode,
    version_name: Option<String>,
    signing_certificate_history: SigningCertificateHistory,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
struct SetIdentity {
    base: FileIdentity,
    native_split: Option<FileIdentity>,
}

struct ApkSetFiles {
    base: Member,
    native_split: Option<Member>,
}

struct Member {
    path: PathBuf,
    file: File,
}

impl Member {
    fn open(path: PathBuf) -> Result<Self, ApkSetError> {
        match File::open(&path) {
            Ok(file) => Ok(Self { path, file }),
            Err(source) => Err(ApkSetError::Locate { path, source }),
        }
    }

    fn identity(&self) -> Result<FileIdentity, ApkSetError> {
        self.file
            .metadata()
            .map(|metadata| FileIdentity::of(&metadata))
            .map_err(|source| ApkSetError::Locate {
                path: self.path.clone(),
                source,
            })
    }

    fn signature_error(&self, source: SignatureError) -> ApkSetError {
        let path = self.path.clone();
        match source {
            SignatureError::Io(source) => ApkSetError::Locate { path, source },
            source => ApkSetError::Signature { path, source },
        }
    }

    fn into_apk(self) -> Result<Apk, ApkSetError> {
        Apk::from_file(&self.path, self.file).map_err(|source| ApkSetError::Open {
            path: self.path,
            source,
        })
    }
}

impl ApkSetFiles {
    fn open(paths: ApkSetPaths) -> Result<Self, ApkSetError> {
        Ok(Self {
            base: Member::open(paths.base)?,
            native_split: paths.native_split.map(Member::open).transpose()?,
        })
    }

    fn identity(&self) -> Result<SetIdentity, ApkSetError> {
        Ok(SetIdentity {
            base: self.base.identity()?,
            native_split: self
                .native_split
                .as_ref()
                .map(Member::identity)
                .transpose()?,
        })
    }

    fn verify(self) -> Result<ApkSet, ApkSetError> {
        let signing_certificate_history = signature::verify_roblox_signing_history(&self.base.file)
            .map_err(|source| self.base.signature_error(source))?;
        if let Some(split) = &self.native_split {
            signature::verify_roblox_signature(&split.file)
                .map_err(|source| split.signature_error(source))?;
        }
        self.assemble(signing_certificate_history)
    }

    fn assemble(
        self,
        signing_certificate_history: SigningCertificateHistory,
    ) -> Result<ApkSet, ApkSetError> {
        let paths = ApkSetPaths {
            base: self.base.path.clone(),
            native_split: self.native_split.as_ref().map(|split| split.path.clone()),
        };
        let mut base = self.base.into_apk()?;
        let base_info = member_info(&mut base, &paths.base)?;
        if let Some(split) = base_info.split {
            return Err(ApkSetError::BaseIsSplit {
                path: paths.base.clone(),
                split,
            });
        }
        let version_code = base_info
            .version_code
            .ok_or_else(|| ApkSetError::MissingVersionCode(paths.base.clone()))?;
        let manifest = base.manifest().map_err(|source| ApkSetError::Open {
            path: paths.base.clone(),
            source,
        })?;

        let native_split = match self.native_split {
            None => None,
            Some(member) => {
                let path = member.path.clone();
                let mut split = member.into_apk()?;
                let info = member_info(&mut split, &path)?;
                if info.split.as_deref() != Some(NATIVE_SPLIT_NAME) {
                    return Err(ApkSetError::NotNativeSplit {
                        path,
                        split: info.split,
                    });
                }
                if info.version_code != Some(version_code) {
                    return Err(ApkSetError::VersionMismatch {
                        path,
                        base: version_code,
                        split: info.version_code,
                    });
                }
                Some(split)
            }
        };

        let mut set = ApkSet {
            paths,
            base,
            native_split,
            manifest,
            version_code,
            version_name: base_info.version_name,
            signing_certificate_history,
        };
        match set.native_libs_mut().x86_64_engine() {
            Ok(_) => Ok(set),
            Err(ApkError::EngineMissing) => Err(ApkSetError::EngineMissing {
                path: set.native_libs_path().to_path_buf(),
                has_native_split: set.paths.native_split.is_some(),
            }),
            Err(source) => Err(ApkSetError::Open {
                path: set.native_libs_path().to_path_buf(),
                source,
            }),
        }
    }
}

impl ApkSet {
    pub fn open(paths: ApkSetPaths) -> Result<Self, ApkSetError> {
        ApkSetFiles::open(paths)?.verify()
    }

    pub fn base_path(&self) -> &Path {
        &self.paths.base
    }

    pub fn native_libs_path(&self) -> &Path {
        self.paths.native_libs()
    }

    pub fn manifest(&self) -> &Manifest {
        &self.manifest
    }

    pub fn version_code(&self) -> VersionCode {
        self.version_code
    }

    pub fn version_name(&self) -> Option<&str> {
        self.version_name.as_deref()
    }

    pub fn signing_certificate_history(&self) -> &SigningCertificateHistory {
        &self.signing_certificate_history
    }

    pub fn base_mut(&mut self) -> &mut Apk {
        &mut self.base
    }

    pub fn native_libs(&self) -> &Apk {
        self.native_split.as_ref().unwrap_or(&self.base)
    }

    pub fn native_libs_mut(&mut self) -> &mut Apk {
        self.native_split.as_mut().unwrap_or(&mut self.base)
    }

    fn relocated(mut self, paths: ApkSetPaths) -> Result<Self, ApkSetError> {
        claim_moved_member(&mut self.base, &paths.base)?;
        match (self.native_split.as_mut(), paths.native_split.as_deref()) {
            (Some(split), Some(path)) => claim_moved_member(split, path)?,
            (None, None) => {}
            (Some(_), None) => {
                return Err(ApkSetError::Replaced(
                    paths.base.with_file_name(NATIVE_SPLIT_APK),
                ))
            }
            (None, Some(path)) => return Err(ApkSetError::Replaced(path.to_path_buf())),
        }
        self.paths = paths;
        Ok(self)
    }
}

fn claim_moved_member(apk: &mut Apk, path: &Path) -> Result<(), ApkSetError> {
    let moved = match std::fs::metadata(path) {
        Ok(moved) => moved,
        Err(error) if error.kind() == io::ErrorKind::NotFound => {
            return Err(ApkSetError::Replaced(path.to_path_buf()))
        }
        Err(source) => {
            return Err(ApkSetError::Locate {
                path: path.to_path_buf(),
                source,
            })
        }
    };
    let verified = apk.file.metadata().map_err(|source| ApkSetError::Locate {
        path: apk.path.clone(),
        source,
    })?;
    if (moved.dev(), moved.ino()) != (verified.dev(), verified.ino()) {
        return Err(ApkSetError::Replaced(path.to_path_buf()));
    }
    apk.path = path.to_path_buf();
    Ok(())
}

fn member_info(apk: &mut Apk, path: &Path) -> Result<PackageInfo, ApkSetError> {
    let info = apk.package_info().map_err(|source| ApkSetError::Open {
        path: path.to_path_buf(),
        source,
    })?;
    if info.package != ROBLOX_PACKAGE {
        return Err(ApkSetError::WrongPackage {
            path: path.to_path_buf(),
            package: info.package,
        });
    }
    Ok(info)
}

#[derive(Debug)]
pub enum ApkError {
    Io(io::Error),

    Zip(zip::result::ZipError),

    Axml(AxmlError),

    EntryMissing(String),

    EntryOffsetUnknown(String),

    EngineMissing,

    ManifestTooLarge(u64),

    ManifestLongerThanDeclared(u64),
}

impl fmt::Display for ApkError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Io(e) => write!(f, "APK file I/O error: {e}"),
            Self::Zip(e) => write!(f, "APK zip error: {e}"),
            Self::Axml(e) => write!(f, "AndroidManifest.xml parse error: {e}"),
            Self::EntryMissing(name) => write!(f, "APK is missing required entry: {name}"),
            Self::EntryOffsetUnknown(name) => {
                write!(f, "APK entry {name} has no resolved data offset")
            }
            Self::EngineMissing => {
                write!(
                    f,
                    "APK has no x86_64 engine library (lib/{TARGET_ABI}/{ENGINE_LIB})"
                )
            }
            Self::ManifestTooLarge(declared) => write!(
                f,
                "{MANIFEST_ENTRY} declares {declared} bytes, more than the {MAX_MANIFEST_BYTES} \
                 bytes Eclipse reads from an APK"
            ),
            Self::ManifestLongerThanDeclared(declared) => write!(
                f,
                "{MANIFEST_ENTRY} inflates past the {declared} bytes its zip entry declares"
            ),
        }
    }
}

impl std::error::Error for ApkError {
    fn source(&self) -> Option<&(dyn std::error::Error + 'static)> {
        match self {
            Self::Io(e) => Some(e),
            Self::Zip(e) => Some(e),
            Self::Axml(e) => Some(e),
            Self::EntryMissing(_)
            | Self::EntryOffsetUnknown(_)
            | Self::EngineMissing
            | Self::ManifestTooLarge(_)
            | Self::ManifestLongerThanDeclared(_) => None,
        }
    }
}

impl From<io::Error> for ApkError {
    fn from(e: io::Error) -> Self {
        Self::Io(e)
    }
}

impl From<zip::result::ZipError> for ApkError {
    fn from(e: zip::result::ZipError) -> Self {
        Self::Zip(e)
    }
}

impl From<AxmlError> for ApkError {
    fn from(e: AxmlError) -> Self {
        Self::Axml(e)
    }
}

#[derive(Debug)]
pub enum ApkSetError {
    Locate {
        path: PathBuf,
        source: io::Error,
    },

    MissingBase(PathBuf),

    Bundle(PathBuf),

    Open {
        path: PathBuf,
        source: ApkError,
    },

    Signature {
        path: PathBuf,
        source: SignatureError,
    },

    WrongPackage {
        path: PathBuf,
        package: String,
    },

    BaseIsSplit {
        path: PathBuf,
        split: String,
    },

    NotNativeSplit {
        path: PathBuf,
        split: Option<String>,
    },

    MissingVersionCode(PathBuf),

    VersionMismatch {
        path: PathBuf,
        base: VersionCode,
        split: Option<VersionCode>,
    },

    EngineMissing {
        path: PathBuf,
        has_native_split: bool,
    },

    Replaced(PathBuf),
}

impl fmt::Display for ApkSetError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Locate { path, source } => write!(f, "cannot read {}: {source}", path.display()),
            Self::MissingBase(dir) => write!(
                f,
                "{} has no {BASE_APK}; pass an APK file or a directory holding {BASE_APK} and \
                 {NATIVE_SPLIT_APK}",
                dir.display()
            ),
            Self::Bundle(path) => write!(
                f,
                "{} is an app bundle; install it with `eclipse install {}`, then start Roblox \
                 with `eclipse run`",
                path.display(),
                eclipse_config::shell::word(&path.to_string_lossy())
            ),
            Self::Open { path, source } => write!(f, "{}: {source}", path.display()),
            Self::Signature { path, source } => write!(
                f,
                "{} is not the official, unmodified Roblox client ({source}); Eclipse only runs \
                 APKs signed by Roblox Corporation, so download Roblox with `eclipse update` \
                 (APKCombo) or `eclipse update --play` (Google Play), or `eclipse install` \
                 Roblox's own release files",
                path.display()
            ),
            Self::WrongPackage { path, package } => write!(
                f,
                "{} is the app {package}, not {ROBLOX_PACKAGE}",
                path.display()
            ),
            Self::BaseIsSplit { path, split } => write!(
                f,
                "{} is the split APK {split}, not a base APK; pass the directory that holds \
                 {BASE_APK} and {NATIVE_SPLIT_APK}",
                path.display()
            ),
            Self::NotNativeSplit { path, split } => match split {
                Some(split) => write!(
                    f,
                    "{} is the split {split}, not the {NATIVE_SPLIT_NAME} split",
                    path.display()
                ),
                None => write!(
                    f,
                    "{} is a base APK, not the {NATIVE_SPLIT_NAME} split",
                    path.display()
                ),
            },
            Self::MissingVersionCode(path) => {
                write!(f, "{} declares no versionCode", path.display())
            }
            Self::VersionMismatch { path, base, split } => {
                let split = split.map_or_else(|| "no versionCode".to_owned(), |v| v.to_string());
                write!(
                    f,
                    "{} has versionCode {split} but {BASE_APK} has {base}; both files must come \
                     from the same Roblox release",
                    path.display()
                )
            }
            Self::EngineMissing {
                path,
                has_native_split: true,
            } => write!(f, "{} has no lib/{TARGET_ABI}/{ENGINE_LIB}", path.display()),
            Self::EngineMissing {
                path,
                has_native_split: false,
            } => write!(
                f,
                "{} has no lib/{TARGET_ABI}/{ENGINE_LIB}; Roblox ships its x86_64 code in \
                 {NATIVE_SPLIT_APK}, so put {BASE_APK} and {NATIVE_SPLIT_APK} in one directory \
                 and pass that directory",
                path.display()
            ),
            Self::Replaced(path) => write!(
                f,
                "{} is not the Roblox APK file that passed the signature check; it was replaced \
                 while Eclipse installed it",
                path.display()
            ),
        }
    }
}

impl std::error::Error for ApkSetError {
    fn source(&self) -> Option<&(dyn std::error::Error + 'static)> {
        match self {
            Self::Locate { source, .. } => Some(source),
            Self::Open { source, .. } => Some(source),
            Self::Signature { source, .. } => Some(source),
            Self::MissingBase(_)
            | Self::Bundle(_)
            | Self::WrongPackage { .. }
            | Self::BaseIsSplit { .. }
            | Self::NotNativeSplit { .. }
            | Self::MissingVersionCode(_)
            | Self::VersionMismatch { .. }
            | Self::EngineMissing { .. }
            | Self::Replaced(_) => None,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::io::{Cursor, Write};
    use zip::write::SimpleFileOptions;
    use zip::ZipWriter;

    const FIXTURE_MANIFEST: &[u8] = include_bytes!("../../tests/fixtures/AndroidManifest-min.bin");

    const FIXTURE_PANIC: &[u8] = include_bytes!("../../tests/fixtures/AndroidManifest-panic.bin");

    const FIXTURE_ABSENT: &[u8] = include_bytes!("../../tests/fixtures/AndroidManifest-absent.bin");

    const FIXTURE_UTF16: &[u8] = include_bytes!("../../tests/fixtures/AndroidManifest-utf16.bin");

    fn build_apk(entries: &[(&str, &[u8])]) -> Vec<u8> {
        let methoded: Vec<(&str, &[u8], CompressionMethod)> = entries
            .iter()
            .map(|(n, b)| (*n, *b, CompressionMethod::Stored))
            .collect();
        build_apk_methods(&methoded)
    }

    fn build_apk_methods(entries: &[(&str, &[u8], CompressionMethod)]) -> Vec<u8> {
        let mut writer = ZipWriter::new(Cursor::new(Vec::new()));
        for (name, bytes, method) in entries {
            let opts = SimpleFileOptions::default().compression_method(*method);
            writer.start_file(*name, opts).expect("start_file");
            writer.write_all(bytes).expect("write_all");
        }
        writer.finish().expect("finish").into_inner()
    }

    fn temp_file(tag: &str, bytes: &[u8]) -> PathBuf {
        let mut path = std::env::temp_dir();
        path.push(format!(
            "eclipse-apk-test-{tag}-{:?}.tmp",
            std::thread::current().id()
        ));
        std::fs::write(&path, bytes).expect("write temp file");
        path
    }

    fn open_apk(bytes: &[u8], tag: &str) -> (Apk, PathBuf) {
        let path = temp_file(tag, bytes);
        let apk = Apk::open(&path).expect("open apk");
        (apk, path)
    }

    #[test]
    fn modified_utf8_decodes_utf8_cesu8_pairs_and_encoded_nul() {
        assert_eq!(decode_modified_utf8(b"plain").as_deref(), Some("plain"));
        assert_eq!(
            decode_modified_utf8("Jump back in \u{1F504}".as_bytes()).as_deref(),
            Some("Jump back in \u{1F504}")
        );
        assert_eq!(
            decode_modified_utf8(b"Popular right now \xED\xA0\xBD\xED\xB4\xA5").as_deref(),
            Some("Popular right now \u{1F525}")
        );
        assert_eq!(decode_modified_utf8(&[0xC0, 0x80]).as_deref(), Some("\0"));
        assert_eq!(
            decode_modified_utf8(&[0xED, 0xA0, 0xBD, b'x']).as_deref(),
            Some("\u{FFFD}x")
        );
    }

    #[test]
    fn modified_utf8_rejects_bad_leads_truncation_and_out_of_range_code_points() {
        assert_eq!(decode_modified_utf8(&[0xFF]), None);
        assert_eq!(decode_modified_utf8(&[0x80]), None);
        assert_eq!(decode_modified_utf8(&[0xE2, 0x82]), None);
        assert_eq!(decode_modified_utf8(&[0xE2, 0x28, 0xA1]), None);
        assert_eq!(decode_modified_utf8(&[0xF4, 0x90, 0x80, 0x80]), None);
    }

    #[test]
    fn modified_utf8_rejects_overlong_forms_other_than_the_encoded_nul() {
        assert_eq!(decode_modified_utf8(&[0xC0, 0xAF]), None);
        assert_eq!(decode_modified_utf8(&[0xC1, 0x81]), None);
        assert_eq!(decode_modified_utf8(&[0xE0, 0x80, 0xAF]), None);
        assert_eq!(decode_modified_utf8(&[0xE0, 0x80, 0x80]), None);
        assert_eq!(decode_modified_utf8(&[0xF0, 0x8F, 0xBF, 0xBF]), None);
        assert_eq!(
            decode_modified_utf8(&[b'a', 0xC0, 0x80]).as_deref(),
            Some("a\0")
        );
    }

    #[test]
    fn manifest_parses_fields_and_resolves_launcher() {
        let bytes = build_apk(&[(MANIFEST_ENTRY, FIXTURE_MANIFEST)]);
        let (mut apk, path) = open_apk(&bytes, "manifest");
        let manifest = apk.manifest().expect("parse manifest");
        std::fs::remove_file(&path).ok();

        assert_eq!(manifest.package, "com.example.app");

        assert_eq!(manifest.launcher_activity, ".SplashActivity");
        assert_eq!(manifest.min_sdk, Some(26));
        assert_eq!(manifest.target_sdk, Some(35));
        assert!(manifest.large_heap);
    }

    #[test]
    fn manifest_defaults_when_uses_sdk_and_large_heap_absent() {
        let bytes = build_apk(&[(MANIFEST_ENTRY, FIXTURE_ABSENT)]);
        let (mut apk, path) = open_apk(&bytes, "manifest-absent");
        let manifest = apk.manifest().expect("parse manifest");
        std::fs::remove_file(&path).ok();

        assert_eq!(manifest.package, "com.example.app");
        assert_eq!(manifest.launcher_activity, ".SplashActivity");
        assert_eq!(manifest.min_sdk, None);
        assert_eq!(manifest.target_sdk, None);
        assert!(!manifest.large_heap);
    }

    #[test]
    fn manifest_reads_deflate_compressed_entry() {
        let bytes = build_apk_methods(&[(
            MANIFEST_ENTRY,
            FIXTURE_MANIFEST,
            CompressionMethod::Deflated,
        )]);
        let (mut apk, path) = open_apk(&bytes, "manifest-deflate");
        let manifest = apk.manifest().expect("parse deflated manifest");
        std::fs::remove_file(&path).ok();
        assert_eq!(manifest.package, "com.example.app");
        assert_eq!(manifest.launcher_activity, ".SplashActivity");
    }

    fn android<'a>(name: &'a str, value: &'a str) -> axml::fixture::Attribute<'a> {
        axml::fixture::Attribute {
            android: true,
            name,
            value: axml::fixture::Value::Str(value),
        }
    }

    fn component_with_filter(
        document: &mut axml::fixture::Document,
        (tag, name): (&str, &str),
        action: &str,
        categories: &[&str],
        data: &[&[axml::fixture::Attribute<'_>]],
    ) {
        document.start(tag, &[android("name", name)]);
        document.start("intent-filter", &[]);
        document.start("action", &[android("name", action)]);
        document.end("action");
        for category in categories {
            document.start("category", &[android("name", category)]);
            document.end("category");
        }
        for attributes in data {
            document.start("data", attributes);
            document.end("data");
        }
        document.end("intent-filter");
        document.end(tag);
    }

    #[test]
    fn view_links_resolve_to_the_first_activity_whose_default_view_filter_matches() {
        const VIEW: &str = "android.intent.action.VIEW";
        const DEFAULT: &str = "android.intent.category.DEFAULT";
        const BROWSABLE: &str = "android.intent.category.BROWSABLE";
        let mut document = axml::fixture::Document::default();
        document.start(
            "manifest",
            &[axml::fixture::Attribute {
                android: false,
                name: "package",
                value: axml::fixture::Value::Str(ROBLOX_PACKAGE),
            }],
        );
        document.start("application", &[]);
        component_with_filter(
            &mut document,
            ("activity", "com.example.Splash"),
            "android.intent.action.MAIN",
            &["android.intent.category.LAUNCHER", DEFAULT],
            &[],
        );
        component_with_filter(
            &mut document,
            ("service", "com.example.Service"),
            VIEW,
            &[DEFAULT],
            &[&[android("scheme", "roblox")]],
        );
        component_with_filter(
            &mut document,
            ("activity", "com.example.NoDefault"),
            VIEW,
            &[BROWSABLE],
            &[&[android("scheme", "roblox")]],
        );
        component_with_filter(
            &mut document,
            ("activity", "com.example.Games"),
            VIEW,
            &[DEFAULT, BROWSABLE],
            &[&[
                android("scheme", "https"),
                android("host", "www.roblox.com"),
                android("pathPattern", "/games/..*"),
            ]],
        );
        component_with_filter(
            &mut document,
            ("activity", "com.example.Share"),
            VIEW,
            &[DEFAULT],
            &[&[
                android("scheme", "https"),
                android("host", "www.roblox.com"),
                android("pathPrefix", "/share"),
            ]],
        );
        component_with_filter(
            &mut document,
            ("activity", "com.example.Home"),
            VIEW,
            &[DEFAULT],
            &[&[
                android("scheme", "https"),
                android("host", "roblox.com"),
                android("path", "/home"),
            ]],
        );
        component_with_filter(
            &mut document,
            ("activity", "com.example.Scheme"),
            VIEW,
            &[DEFAULT],
            &[
                &[android("scheme", "roblox")],
                &[android("scheme", "robloxmobile")],
            ],
        );
        component_with_filter(
            &mut document,
            ("activity", "com.example.Global"),
            VIEW,
            &[DEFAULT],
            &[&[
                android("scheme", "robloxglobal"),
                android("pathPrefix", "/only"),
            ]],
        );
        document.end("application");
        document.end("manifest");

        let bytes = build_apk(&[(MANIFEST_ENTRY, &document.finish())]);
        let (mut apk, path) = open_apk(&bytes, "view-filters");
        let manifest = apk.manifest().expect("parse manifest");
        std::fs::remove_file(&path).ok();

        assert_eq!(manifest.launcher_activity, "com.example.Splash");
        let cases = [
            ("roblox://placeId=1818", Some("com.example.Scheme")),
            ("robloxmobile://placeId=1818", Some("com.example.Scheme")),
            ("https://www.roblox.com/games/1", Some("com.example.Games")),
            (
                "https://WWW.Roblox.com/games/1818/Slug?x=/share",
                Some("com.example.Games"),
            ),
            ("https://www.roblox.com/games/", None),
            (
                "https://www.roblox.com/share?code=1&type=Server",
                Some("com.example.Share"),
            ),
            ("https://roblox.com/share", None),
            ("https://roblox.com/home", Some("com.example.Home")),
            ("https://roblox.com/home/1", None),
            ("http://www.roblox.com/games/1", None),
            ("robloxglobal://any/path", Some("com.example.Global")),
            ("roblox-player:1+launchmode:play", None),
            ("placeId=1818", None),
        ];
        for (uri, activity) in cases {
            assert_eq!(manifest.resolve_view_activity(uri), activity, "{uri}");
        }
    }

    #[test]
    fn manifest_missing_entry_is_typed_error() {
        let bytes = build_apk(&[("lib/x86_64/libroblox.so", b"stub")]);
        let (mut apk, path) = open_apk(&bytes, "nomanifest");
        let err = apk.manifest().expect_err("should be missing");
        std::fs::remove_file(&path).ok();
        match err {
            ApkError::EntryMissing(name) => assert_eq!(name, MANIFEST_ENTRY),
            other => panic!("expected EntryMissing, got {other:?}"),
        }
    }

    #[test]
    fn garbage_manifest_is_typed_error() {
        let bytes = build_apk(&[(MANIFEST_ENTRY, b"this is not binary xml at all")]);
        let (mut apk, path) = open_apk(&bytes, "garbage");
        let err = apk.manifest().expect_err("garbage must fail");
        std::fs::remove_file(&path).ok();
        assert!(matches!(err, ApkError::Axml(_)), "got {err:?}");
    }

    #[test]
    fn panic_fixture_returns_typed_error_not_panic() {
        let bytes = build_apk(&[(MANIFEST_ENTRY, FIXTURE_PANIC)]);
        let (mut apk, path) = open_apk(&bytes, "panic");
        let err = apk
            .manifest()
            .expect_err("panic fixture must be a typed error");
        std::fs::remove_file(&path).ok();
        assert!(matches!(err, ApkError::Axml(_)), "got {err:?}");
    }

    #[test]
    fn reader_is_total_under_truncation_and_mutation() {
        for base in [FIXTURE_MANIFEST, FIXTURE_UTF16] {
            for len in 0..=base.len() {
                let _ = axml::read_manifest(&base[..len]);
            }

            let stride = 7;
            for off in (0..base.len()).step_by(stride) {
                for &val in &[0x00u8, 0x7F, 0xFF] {
                    let mut buf = base.to_vec();
                    buf[off] = val;
                    let _ = axml::read_manifest(&buf);
                }
            }
        }
    }

    #[test]
    fn parse_document_walks_manifest_events_and_attributes() {
        for base in [FIXTURE_MANIFEST, FIXTURE_UTF16] {
            let doc = axml::parse_document(base).expect("parse_document on a valid manifest");

            let manifest_el = doc
                .elements
                .iter()
                .find(|e| e.name.as_deref() == Some("manifest"))
                .expect("manifest element present");
            let pkg = manifest_el
                .attributes
                .iter()
                .find(|a| a.name.as_deref() == Some("package"))
                .expect("package attribute present");
            assert_eq!(pkg.value_string.as_deref(), Some("com.example.app"));

            assert!(
                doc.elements
                    .iter()
                    .any(|e| e.name.as_deref() == Some("activity")),
                "activity element must appear in the event walk"
            );

            let starts = doc
                .events
                .iter()
                .filter(|e| matches!(e, axml::XmlEventKind::StartTag(_)))
                .count();
            let ends = doc
                .events
                .iter()
                .filter(|e| matches!(e, axml::XmlEventKind::EndTag(_)))
                .count();
            assert_eq!(
                starts, ends,
                "start/end tags must balance in a well-formed manifest"
            );
            assert!(
                starts >= 2,
                "manifest has at least <manifest> and <application>"
            );
        }
    }

    #[test]
    fn parse_document_is_total_on_garbage() {
        assert!(axml::parse_document(b"not binary xml at all").is_err());
        assert!(axml::parse_document(&[]).is_err());
    }

    #[test]
    fn manifest_parses_utf16_string_pool() {
        let utf16 = build_apk(&[(MANIFEST_ENTRY, FIXTURE_UTF16)]);
        let (mut apk16, p16) = open_apk(&utf16, "manifest-utf16");
        let m16 = apk16.manifest().expect("parse utf16 manifest");
        std::fs::remove_file(&p16).ok();

        assert_eq!(m16.package, "com.example.app");
        assert_eq!(m16.launcher_activity, ".SplashActivity");
        assert_eq!(m16.min_sdk, Some(26));
        assert_eq!(m16.target_sdk, Some(35));
        assert!(m16.large_heap);

        let utf8 = build_apk(&[(MANIFEST_ENTRY, FIXTURE_MANIFEST)]);
        let (mut apk8, p8) = open_apk(&utf8, "manifest-utf8-xcheck");
        let m8 = apk8.manifest().expect("parse utf8 manifest");
        std::fs::remove_file(&p8).ok();
        assert_eq!(m16, m8);
    }

    #[test]
    fn native_lib_filenames_lists_flat_so_files_for_the_abi_sorted() {
        let bytes = build_apk(&[
            ("lib/x86_64/libroblox.so", b"engine"),
            ("lib/x86_64/libzstd-jni-1.5.7-6.so", b"zstd"),
            ("lib/x86_64/libeigen_blas.so", b"blas"),
            ("lib/x86_64/notashared.txt", b"txt"),
            ("lib/x86_64/nested/deep.so", b"nested"),
            ("lib/arm64-v8a/libroblox.so", b"arm"),
            ("classes.dex", b"dex"),
        ]);
        let (apk, path) = open_apk(&bytes, "lib-filenames");
        let names = apk.native_lib_filenames("x86_64");
        std::fs::remove_file(&path).ok();
        assert_eq!(
            names,
            vec![
                "libeigen_blas.so".to_string(),
                "libroblox.so".to_string(),
                "libzstd-jni-1.5.7-6.so".to_string(),
            ]
        );

        let java_only = build_apk(&[(MANIFEST_ENTRY, FIXTURE_MANIFEST), ("classes.dex", b"dex")]);
        let (apk2, p2) = open_apk(&java_only, "lib-filenames-empty");
        assert!(apk2.native_lib_filenames("x86_64").is_empty());
        std::fs::remove_file(&p2).ok();
    }

    #[test]
    fn x86_64_engine_is_missing_from_an_arm_only_apk() {
        let bytes = build_apk(&[("lib/arm64-v8a/libroblox.so", b"engine")]);
        let (mut apk, path) = open_apk(&bytes, "armonly");
        let err = apk.x86_64_engine().expect_err("no x86_64 engine");
        std::fs::remove_file(&path).ok();
        assert!(matches!(err, ApkError::EngineMissing), "got {err:?}");
    }

    #[test]
    fn x86_64_engine_reports_stored_and_size() {
        let payload = b"libroblox-engine-bytes";
        let bytes = build_apk(&[("lib/x86_64/libroblox.so", payload)]);
        let (mut apk, path) = open_apk(&bytes, "engine");
        let engine = apk.x86_64_engine().expect("engine present");
        std::fs::remove_file(&path).ok();
        assert_eq!(engine.entry, "lib/x86_64/libroblox.so");
        assert_eq!(engine.size, payload.len() as u64);
        assert!(engine.stored);
    }

    #[test]
    fn x86_64_engine_reports_not_stored_when_deflated() {
        let payload = b"libroblox-engine-bytes-deflated";
        let bytes = build_apk_methods(&[(
            "lib/x86_64/libroblox.so",
            payload,
            CompressionMethod::Deflated,
        )]);
        let (mut apk, path) = open_apk(&bytes, "engine-deflated");
        let engine = apk.x86_64_engine().expect("engine present");
        std::fs::remove_file(&path).ok();
        assert_eq!(engine.size, payload.len() as u64);
        assert!(!engine.stored);
    }

    #[test]
    fn entry_span_reports_stored_offset_size_and_rejects_absent() {
        const PAYLOAD: &[u8] = b"profile-bytes-0123456789";
        let bytes = build_apk_methods(&[
            (
                "assets/dexopt/baseline.prof",
                PAYLOAD,
                CompressionMethod::Stored,
            ),
            (
                "assets/compressed.bin",
                PAYLOAD,
                CompressionMethod::Deflated,
            ),
        ]);
        let (mut apk, path) = open_apk(&bytes, "entry-span");

        let span = apk
            .entry_span("assets/dexopt/baseline.prof")
            .expect("stored span");
        assert!(span.stored);
        assert_eq!(span.uncompressed_size, PAYLOAD.len() as u64);
        let start = usize::try_from(span.data_start).expect("offset fits usize");
        assert_eq!(
            &bytes[start..start + PAYLOAD.len()],
            PAYLOAD,
            "the bytes at data_start must BE the Stored asset"
        );

        let span = apk
            .entry_span("assets/compressed.bin")
            .expect("deflated span");
        assert!(!span.stored, "a Deflated entry must report stored == false");
        assert_eq!(span.uncompressed_size, PAYLOAD.len() as u64);

        let err = apk.entry_span("assets/absent.bin").unwrap_err();
        assert!(matches!(err, ApkError::EntryMissing(_)), "got {err:?}");
        std::fs::remove_file(&path).ok();
    }

    #[test]
    fn extract_native_libs_extracts_matching_abi_only_and_is_idempotent() {
        let bytes = build_apk(&[
            ("lib/x86_64/libroblox.so", b"ENGINE-BYTES"),
            ("lib/x86_64/libother.so", b"OTHER"),
            ("lib/arm64-v8a/libfoo.so", b"ARM-ONLY"),
            ("classes.dex", b"dex"),
        ]);
        let (mut apk, apk_path) = open_apk(&bytes, "extract");
        let dir = std::env::temp_dir().join(format!(
            "eclipse-extract-test-{:?}",
            std::thread::current().id()
        ));
        std::fs::remove_dir_all(&dir).ok();

        let extracted = apk
            .extract_native_libs("x86_64", &dir, &StatusSink::terminal())
            .expect("extract");
        assert_eq!(extracted, 2, "two x86_64 libraries written");
        assert_eq!(
            std::fs::read(dir.join("libroblox.so")).unwrap(),
            b"ENGINE-BYTES"
        );
        assert_eq!(std::fs::read(dir.join("libother.so")).unwrap(), b"OTHER");
        assert!(
            !dir.join("libfoo.so").exists(),
            "wrong-ABI lib must not extract"
        );
        assert!(
            !dir.join("classes.dex").exists(),
            "non-.so must not extract"
        );

        let again = apk
            .extract_native_libs("x86_64", &dir, &StatusSink::terminal())
            .expect("re-extract");
        assert_eq!(again, 0, "an unchanged extraction writes nothing");

        std::fs::remove_dir_all(&dir).ok();
        std::fs::remove_file(&apk_path).ok();
    }

    #[test]
    fn extract_native_libs_replaces_a_changed_same_size_entry_after_an_apk_upgrade() {
        let old_bytes = build_apk(&[("lib/x86_64/libsame.so", b"OLD-LIB")]);
        let new_bytes = build_apk(&[("lib/x86_64/libsame.so", b"NEW-LIB")]);
        assert_eq!(b"OLD-LIB".len(), b"NEW-LIB".len());
        let (mut old_apk, old_path) = open_apk(&old_bytes, "extract-upgrade-old");
        let (mut new_apk, new_path) = open_apk(&new_bytes, "extract-upgrade-new");
        let dir = std::env::temp_dir().join(format!(
            "eclipse-extract-upgrade-test-{:?}",
            std::thread::current().id()
        ));
        std::fs::remove_dir_all(&dir).ok();

        old_apk
            .extract_native_libs("x86_64", &dir, &StatusSink::terminal())
            .expect("extract old APK");
        new_apk
            .extract_native_libs("x86_64", &dir, &StatusSink::terminal())
            .expect("extract upgraded APK");
        assert_eq!(
            std::fs::read(dir.join("libsame.so")).unwrap(),
            b"NEW-LIB",
            "same-size old library must not survive an APK upgrade"
        );

        std::fs::remove_dir_all(&dir).ok();
        std::fs::remove_file(&old_path).ok();
        std::fs::remove_file(&new_path).ok();
    }

    #[test]
    fn extraction_syncs_the_destination_and_every_directory_below_it_once() {
        let dest = Path::new("/extracted/assets");
        let planned: Vec<PlannedEntry> = ["top.bin", "a/one.bin", "a/b/two.bin", "a/b/three.bin"]
            .into_iter()
            .map(|name| PlannedEntry {
                name: format!("assets/{name}"),
                dest: dest.join(name),
                size: 1,
                crc32: 0,
            })
            .collect();

        let mut directories = extraction_directories(dest, &planned);
        directories.sort_unstable();

        assert_eq!(
            directories,
            [
                Path::new("/extracted/assets"),
                Path::new("/extracted/assets/a"),
                Path::new("/extracted/assets/a/b"),
            ]
        );
    }

    #[test]
    fn extract_assets_strips_prefix_preserves_subpaths_skips_non_assets_and_is_idempotent() {
        let bytes = build_apk(&[
            ("assets/shaders/shaders_glsles3.pack", b"GLSLES3-PACK"),
            ("assets/baz.txt", b"BAZ"),
            ("lib/x86_64/libroblox.so", b"ENGINE"),
            ("classes.dex", b"dex"),
        ]);
        let (mut apk, apk_path) = open_apk(&bytes, "extract-assets");
        let dir = std::env::temp_dir().join(format!(
            "eclipse-extract-assets-test-{:?}",
            std::thread::current().id()
        ));
        std::fs::remove_dir_all(&dir).ok();

        let count = apk
            .extract_assets(&dir, &StatusSink::terminal())
            .expect("extract assets");
        assert_eq!(count, 2, "two asset files written");
        assert_eq!(
            std::fs::read(dir.join("shaders/shaders_glsles3.pack")).unwrap(),
            b"GLSLES3-PACK",
            "nested asset lands at <dest>/shaders/… (prefix stripped, sub-path preserved)"
        );
        assert_eq!(std::fs::read(dir.join("baz.txt")).unwrap(), b"BAZ");
        assert!(
            !dir.join("libroblox.so").exists() && !dir.join("x86_64").exists(),
            "non-asset entry must not be extracted"
        );
        assert!(
            !dir.join("classes.dex").exists(),
            "non-asset entry must not be extracted"
        );

        let again = apk
            .extract_assets(&dir, &StatusSink::terminal())
            .expect("re-extract assets");
        assert_eq!(again, 0, "idempotent re-extract writes 0 files");

        std::fs::remove_dir_all(&dir).ok();
        std::fs::remove_file(&apk_path).ok();
    }

    #[test]
    fn extraction_reports_monotonic_progress_ending_at_the_total() {
        use crate::status::{Progress, StatusUpdate};

        let bytes = build_apk(&[
            ("assets/a.bin", &[1; 300]),
            ("assets/dir/b.bin", &[2; 500]),
            ("assets/c.bin", &[3; 700]),
        ]);
        let (mut apk, apk_path) = open_apk(&bytes, "extract-progress");
        let dir = std::env::temp_dir().join(format!(
            "eclipse-extract-progress-test-{:?}",
            std::thread::current().id()
        ));
        std::fs::remove_dir_all(&dir).ok();
        let extract = |apk: &mut Apk| {
            let (updates, shown) = std::sync::mpsc::channel();
            let written = apk
                .extract_assets(&dir, &StatusSink::with_window(updates))
                .expect("extract assets");
            let reported: Vec<(u64, u64)> = shown
                .try_iter()
                .map(|update| match update {
                    StatusUpdate::Progress(Progress::Extraction { done, total }) => (done, total),
                    other => panic!("extraction reports only its progress: {other:?}"),
                })
                .collect();
            (written, reported)
        };

        let (written, reported) = extract(&mut apk);
        assert_eq!(written, 3);
        assert_eq!(reported.first(), Some(&(0, 1500)));
        assert_eq!(reported.last(), Some(&(1500, 1500)));
        assert!(
            reported.windows(2).all(|pair| pair[0].0 <= pair[1].0),
            "{reported:?}"
        );
        assert!(
            reported.iter().all(|&(_, total)| total == 1500),
            "{reported:?}"
        );

        std::fs::remove_file(dir.join(EXTRACTION_STAMP)).unwrap();
        let (written, reported) = extract(&mut apk);
        assert_eq!(written, 0, "unchanged files are checked, not rewritten");
        assert_eq!(
            reported.last(),
            Some(&(1500, 1500)),
            "files that are already extracted count as prepared"
        );

        assert_eq!(extract(&mut apk), (0, Vec::new()));

        std::fs::remove_dir_all(&dir).ok();
        std::fs::remove_file(&apk_path).ok();
    }

    #[test]
    fn extract_assets_replaces_a_changed_same_size_entry_after_an_apk_upgrade() {
        let old_bytes = build_apk(&[(
            "assets/ExtraContent/models/UniversalApp/UniversalApp_checksum",
            b"OLD-CHECKSUM",
        )]);
        let new_bytes = build_apk(&[(
            "assets/ExtraContent/models/UniversalApp/UniversalApp_checksum",
            b"NEW-CHECKSUM",
        )]);
        assert_eq!(b"OLD-CHECKSUM".len(), b"NEW-CHECKSUM".len());
        let (mut old_apk, old_path) = open_apk(&old_bytes, "asset-upgrade-old");
        let (mut new_apk, new_path) = open_apk(&new_bytes, "asset-upgrade-new");
        let dir = std::env::temp_dir().join(format!(
            "eclipse-asset-upgrade-test-{:?}",
            std::thread::current().id()
        ));
        std::fs::remove_dir_all(&dir).ok();

        assert_eq!(
            old_apk
                .extract_assets(&dir, &StatusSink::terminal())
                .expect("extract old APK"),
            1
        );
        assert_eq!(
            new_apk
                .extract_assets(&dir, &StatusSink::terminal())
                .expect("extract upgraded APK"),
            1,
            "same-size changed asset must be rewritten"
        );
        assert_eq!(
            std::fs::read(dir.join("ExtraContent/models/UniversalApp/UniversalApp_checksum"))
                .unwrap(),
            b"NEW-CHECKSUM",
            "same-size old checksum must not survive an APK upgrade"
        );

        std::fs::remove_dir_all(&dir).ok();
        std::fs::remove_file(&old_path).ok();
        std::fs::remove_file(&new_path).ok();
    }

    fn extracted_tree(dir: &Path) -> Vec<String> {
        let mut tree = Vec::new();
        let mut pending = vec![dir.to_path_buf()];
        while let Some(next) = pending.pop() {
            for entry in std::fs::read_dir(&next).unwrap() {
                let entry = entry.unwrap();
                let path = entry.path();
                let mut name = path.strip_prefix(dir).unwrap().display().to_string();
                if entry.file_type().unwrap().is_dir() {
                    name.push('/');
                    pending.push(path);
                }
                tree.push(name);
            }
        }
        tree.sort();
        tree
    }

    #[test]
    fn an_apk_upgrade_removes_dropped_assets_and_survives_file_directory_swaps() {
        let old_bytes = build_apk(&[
            ("assets/a/x", b"OLD-X"),
            ("assets/b", b"OLD-B"),
            ("assets/old.bin", b"OLD"),
        ]);
        let new_bytes = build_apk(&[("assets/a", b"NEW-A"), ("assets/b/y", b"NEW-Y")]);
        let (mut old_apk, old_path) = open_apk(&old_bytes, "asset-prune-old");
        let (mut new_apk, new_path) = open_apk(&new_bytes, "asset-prune-new");
        let dir = std::env::temp_dir().join(format!(
            "eclipse-asset-prune-test-{:?}",
            std::thread::current().id()
        ));
        std::fs::remove_dir_all(&dir).ok();
        std::fs::create_dir_all(&dir).unwrap();
        std::fs::write(dir.join(".eclipse-extract.4242.partial"), b"stale").unwrap();
        let bookkeeping = [".eclipse-extract.lock", ".eclipse-extract.stamp"];

        assert_eq!(
            old_apk
                .extract_assets(&dir, &StatusSink::terminal())
                .expect("extract old APK"),
            3
        );
        assert_eq!(
            new_apk
                .extract_assets(&dir, &StatusSink::terminal())
                .expect("extract upgraded APK"),
            2
        );
        let mut upgraded = vec!["a", "b/", "b/y"];
        upgraded.extend(bookkeeping);
        upgraded.sort();
        assert_eq!(extracted_tree(&dir), upgraded);
        assert_eq!(std::fs::read(dir.join("a")).unwrap(), b"NEW-A");

        assert_eq!(
            old_apk
                .extract_assets(&dir, &StatusSink::terminal())
                .expect("extract downgraded APK"),
            3
        );
        let mut downgraded = vec!["a/", "a/x", "b", "old.bin"];
        downgraded.extend(bookkeeping);
        downgraded.sort();
        assert_eq!(extracted_tree(&dir), downgraded);
        assert_eq!(std::fs::read(dir.join("b")).unwrap(), b"OLD-B");

        std::fs::remove_dir_all(&dir).ok();
        std::fs::remove_file(&old_path).ok();
        std::fs::remove_file(&new_path).ok();
    }

    fn noisy_payload(len: usize, seed: u32) -> Vec<u8> {
        let mut state = seed;
        (0..len)
            .map(|_| {
                state = state.wrapping_mul(1_664_525).wrapping_add(1_013_904_223);
                (state >> 24) as u8
            })
            .collect()
    }

    #[test]
    fn concurrent_launches_extract_into_the_same_directories_without_failing() {
        let engine = noisy_payload(4 * 1024 * 1024, 7);
        let asset = noisy_payload(256 * 1024, 11);
        let bytes = build_apk(&[
            ("lib/x86_64/libbig.so", &engine),
            ("assets/content/big.bin", &asset),
            ("assets/small.txt", b"SMALL"),
        ]);
        let apk_path = temp_file("concurrent-extract", &bytes);
        let root = std::env::temp_dir().join(format!(
            "eclipse-concurrent-extract-{:?}",
            std::thread::current().id()
        ));
        for round in 0..8 {
            std::fs::remove_dir_all(&root).ok();
            let libs = root.join("libs");
            let assets = root.join("assets");
            let barrier = std::sync::Barrier::new(2);
            let results: Vec<Result<(), ApkError>> = std::thread::scope(|scope| {
                let workers: Vec<_> = (0..2)
                    .map(|_| {
                        let mut apk = Apk::open(&apk_path).expect("open apk");
                        let (libs, assets, barrier) = (&libs, &assets, &barrier);
                        scope.spawn(move || {
                            barrier.wait();
                            apk.extract_native_libs("x86_64", libs, &StatusSink::terminal())?;
                            apk.extract_assets(assets, &StatusSink::terminal())
                                .map(drop)
                        })
                    })
                    .collect();
                workers
                    .into_iter()
                    .map(|worker| worker.join().expect("extraction thread"))
                    .collect()
            });
            for result in results {
                assert!(result.is_ok(), "round {round}: {result:?}");
            }
            assert_eq!(std::fs::read(libs.join("libbig.so")).unwrap(), engine);
            assert_eq!(
                std::fs::read(assets.join("content/big.bin")).unwrap(),
                asset
            );
            assert_eq!(std::fs::read(assets.join("small.txt")).unwrap(), b"SMALL");
        }
        std::fs::remove_dir_all(&root).ok();
        std::fs::remove_file(&apk_path).ok();
    }

    #[test]
    fn extraction_stamps_are_trusted_only_while_the_files_metadata_is_unchanged() {
        use std::os::unix::fs::PermissionsExt as _;

        let bytes = build_apk(&[
            ("lib/x86_64/libroblox.so", b"ENGINE-BYTES"),
            ("assets/content/fonts/a.ttf", b"FONT"),
        ]);
        let (mut apk, apk_path) = open_apk(&bytes, "extract-stamp");
        let root = std::env::temp_dir().join(format!(
            "eclipse-extract-stamp-test-{:?}",
            std::thread::current().id()
        ));
        std::fs::remove_dir_all(&root).ok();
        let (libs, assets) = (root.join("libs"), root.join("assets"));
        apk.extract_native_libs("x86_64", &libs, &StatusSink::terminal())
            .expect("extract libs");
        assert_eq!(
            apk.extract_assets(&assets, &StatusSink::terminal())
                .expect("extract assets"),
            1
        );
        assert_eq!(
            apk.extract_native_libs("x86_64", &libs, &StatusSink::terminal())
                .expect("stamped libs"),
            0
        );
        assert_eq!(
            apk.extract_assets(&assets, &StatusSink::terminal())
                .expect("stamped assets"),
            0
        );

        let unreadable = std::fs::Permissions::from_mode(0o000);
        std::fs::set_permissions(libs.join("libroblox.so"), unreadable.clone()).unwrap();
        std::fs::set_permissions(assets.join("content/fonts/a.ttf"), unreadable).unwrap();
        let again = apk.extract_native_libs("x86_64", &libs, &StatusSink::terminal());
        let assets_again = apk.extract_assets(&assets, &StatusSink::terminal());
        let readable = std::fs::Permissions::from_mode(0o644);
        std::fs::set_permissions(libs.join("libroblox.so"), readable.clone()).unwrap();
        std::fs::set_permissions(assets.join("content/fonts/a.ttf"), readable).unwrap();
        for result in [again, assets_again] {
            assert!(
                matches!(&result, Err(ApkError::Io(error)) if error.kind() == io::ErrorKind::PermissionDenied),
                "a file whose metadata changed after the stamp is read again: {result:?}"
            );
        }

        std::fs::remove_dir_all(&root).ok();
        std::fs::remove_file(&apk_path).ok();
    }

    #[test]
    fn a_file_replaced_after_the_stamp_is_checked_and_rewritten() {
        let bytes = build_apk(&[
            ("lib/x86_64/libroblox.so", b"ENGINE-BYTES"),
            ("assets/a.bin", b"ASSET"),
        ]);
        let (mut apk, apk_path) = open_apk(&bytes, "extract-replaced");
        let root = std::env::temp_dir().join(format!(
            "eclipse-extract-replaced-test-{:?}",
            std::thread::current().id()
        ));
        std::fs::remove_dir_all(&root).ok();
        let (libs, assets) = (root.join("libs"), root.join("assets"));
        apk.extract_native_libs("x86_64", &libs, &StatusSink::terminal())
            .expect("extract libs");
        apk.extract_assets(&assets, &StatusSink::terminal())
            .expect("extract assets");

        for (path, bytes) in [
            (libs.join("libroblox.so"), b"OTHER-ENGINE".as_slice()),
            (assets.join("a.bin"), b"OTHER".as_slice()),
        ] {
            let replacement = path.with_extension("replacement");
            std::fs::write(&replacement, bytes).unwrap();
            std::fs::rename(&replacement, &path).unwrap();
        }
        std::fs::write(libs.join(".eclipse-extract.4242.partial"), b"stale").unwrap();

        assert_eq!(
            apk.extract_native_libs("x86_64", &libs, &StatusSink::terminal())
                .expect("re-extract libs"),
            1
        );
        assert_eq!(
            apk.extract_assets(&assets, &StatusSink::terminal())
                .expect("re-extract assets"),
            1
        );
        assert_eq!(
            std::fs::read(libs.join("libroblox.so")).unwrap(),
            b"ENGINE-BYTES"
        );
        assert_eq!(std::fs::read(assets.join("a.bin")).unwrap(), b"ASSET");
        assert!(
            !libs.join(".eclipse-extract.4242.partial").exists(),
            "a temporary file left by a killed extraction is removed"
        );

        std::fs::remove_dir_all(&root).ok();
        std::fs::remove_file(&apk_path).ok();
    }

    #[test]
    fn every_extracted_file_is_synced_and_a_failure_names_its_file() {
        let dir = std::env::temp_dir().join(format!(
            "eclipse-extract-sync-test-{:?}",
            std::thread::current().id()
        ));
        std::fs::remove_dir_all(&dir).ok();
        std::fs::create_dir_all(&dir).unwrap();
        let mut files: Vec<PathBuf> = (0..40)
            .map(|index| {
                let path = dir.join(format!("file-{index}"));
                std::fs::write(&path, index.to_string()).unwrap();
                path
            })
            .collect();
        let borrowed: Vec<&Path> = files.iter().map(PathBuf::as_path).collect();
        sync_extracted(&borrowed).expect("existing files sync");

        let missing = dir.join("missing");
        files.insert(37, missing.clone());
        let borrowed: Vec<&Path> = files.iter().map(PathBuf::as_path).collect();
        let error = sync_extracted(&borrowed).expect_err("a missing file cannot be synced");
        std::fs::remove_dir_all(&dir).ok();
        assert_eq!(error.kind(), io::ErrorKind::NotFound);
        assert!(
            error
                .to_string()
                .starts_with(&format!("sync {}: ", missing.display())),
            "{error}"
        );
    }

    #[test]
    fn assets_that_collide_with_extraction_bookkeeping_are_refused() {
        let bytes = build_apk(&[("assets/.eclipse-extract.stamp", b"not a stamp")]);
        let (mut apk, apk_path) = open_apk(&bytes, "extract-collision");
        let dir = std::env::temp_dir().join(format!(
            "eclipse-extract-collision-test-{:?}",
            std::thread::current().id()
        ));
        std::fs::remove_dir_all(&dir).ok();
        let error = apk
            .extract_assets(&dir, &StatusSink::terminal())
            .expect_err("bookkeeping name");
        assert!(
            error.to_string().contains(".eclipse-extract.stamp"),
            "{error}"
        );
        std::fs::remove_dir_all(&dir).ok();
        std::fs::remove_file(&apk_path).ok();
    }

    #[test]
    fn extraction_errors_name_the_failed_operation_and_path() {
        use std::os::unix::fs::PermissionsExt as _;

        let bytes = build_apk(&[("assets/content/a.bin", b"ASSET")]);
        let (mut apk, apk_path) = open_apk(&bytes, "extract-error-context");
        let dir = std::env::temp_dir().join(format!(
            "eclipse-extract-error-context-test-{:?}",
            std::thread::current().id()
        ));
        std::fs::remove_dir_all(&dir).ok();
        let unreadable = dir.join("content/a.bin");
        std::fs::create_dir_all(unreadable.parent().unwrap()).unwrap();
        std::fs::write(&unreadable, b"OTHER").unwrap();
        std::fs::set_permissions(&unreadable, std::fs::Permissions::from_mode(0o000)).unwrap();

        let error = apk
            .extract_assets(&dir, &StatusSink::terminal())
            .expect_err("the extracted asset cannot be read");
        std::fs::remove_dir_all(&dir).ok();
        std::fs::remove_file(&apk_path).ok();
        let expected = format!("open {}: ", unreadable.display());
        assert!(error.to_string().contains(&expected), "{error}");
    }

    #[test]
    fn open_missing_file_is_io_error() {
        let mut path = std::env::temp_dir();
        path.push(format!(
            "eclipse-apk-test-nonexistent-{:?}.tmp",
            std::thread::current().id()
        ));
        std::fs::remove_file(&path).ok();

        let err = Apk::open(&path).err().expect("missing file must fail");
        assert!(matches!(err, ApkError::Io(_)), "got {err:?}");
    }

    #[test]
    fn open_non_zip_file_is_zip_error() {
        let path = temp_file("notzip", b"this is plainly not a zip archive");
        let err = Apk::open(&path).err().expect("non-zip must fail");
        std::fs::remove_file(&path).ok();
        assert!(matches!(err, ApkError::Zip(_)), "got {err:?}");
    }

    #[test]
    fn open_truncated_zip_at_every_boundary_is_typed_error_never_panic() {
        let bytes = build_apk(&[
            (MANIFEST_ENTRY, FIXTURE_MANIFEST),
            ("lib/x86_64/libroblox.so", b"engine-bytes-payload"),
            ("classes.dex", b"dex-bytes"),
        ]);
        for len in 0..=bytes.len() {
            let path = temp_file(&format!("trunc-{len}"), &bytes[..len]);

            if let Ok(mut apk) = Apk::open(&path) {
                let _ = apk.read_entry(MANIFEST_ENTRY);
                let _ = apk.x86_64_engine();
                let _ = apk.native_lib_filenames(TARGET_ABI);
            }
            std::fs::remove_file(&path).ok();
        }
    }

    #[test]
    fn open_corrupted_central_directory_is_typed_zip_error() {
        let bytes = build_apk(&[
            (MANIFEST_ENTRY, FIXTURE_MANIFEST),
            ("classes.dex", b"dex-bytes"),
        ]);

        let start = bytes.len().saturating_sub(128);
        for off in (start..bytes.len()).step_by(3) {
            for &val in &[0x00u8, 0xFF] {
                let mut buf = bytes.clone();
                buf[off] = val;
                let path = temp_file(&format!("cd-corrupt-{off}-{val}"), &buf);
                if let Ok(mut apk) = Apk::open(&path) {
                    let _ = apk.read_entry(MANIFEST_ENTRY);
                }
                std::fs::remove_file(&path).ok();
            }
        }
    }

    #[test]
    fn read_entry_prealloc_cap_bounds_upfront_allocation() {
        assert_eq!(READ_ENTRY_PREALLOC_CAP, 8 * 1024 * 1024);

        let payload: &[u8] = b"a-small-manifest-class-entry";
        let cap = (payload.len() as u64).min(READ_ENTRY_PREALLOC_CAP) as usize;
        assert_eq!(cap, payload.len(), "small entry: cap is the true size");
        let bytes = build_apk(&[("res.bin", payload)]);
        let (mut apk, path) = open_apk(&bytes, "prealloc-cap");
        let got = apk.read_entry("res.bin").expect("read small entry");
        std::fs::remove_file(&path).ok();
        assert_eq!(got, payload, "cap must never truncate the real bytes");
    }

    #[test]
    fn package_info_rejects_a_manifest_that_inflates_past_the_cap() {
        let bomb = vec![0u8; (MAX_MANIFEST_BYTES + 1) as usize];
        let bytes = build_apk_methods(&[(MANIFEST_ENTRY, &bomb, CompressionMethod::Deflated)]);
        assert!(bytes.len() < 64 * 1024, "the fixture stays small on disk");
        let (mut apk, path) = open_apk(&bytes, "manifest-bomb");
        let info = apk.package_info();
        let manifest = apk.manifest();
        std::fs::remove_file(&path).ok();
        assert!(
            matches!(info, Err(ApkError::ManifestTooLarge(size)) if size == MAX_MANIFEST_BYTES + 1),
            "got {info:?}"
        );
        assert!(
            matches!(manifest, Err(ApkError::ManifestTooLarge(_))),
            "got {manifest:?}"
        );
    }

    #[test]
    fn package_info_rejects_a_manifest_longer_than_its_declared_size() {
        const DECLARED: u32 = 16;
        let payload = vec![0u8; 8 * 1024];
        let mut bytes =
            build_apk_methods(&[(MANIFEST_ENTRY, &payload, CompressionMethod::Deflated)]);
        bytes[22..26].copy_from_slice(&DECLARED.to_le_bytes());
        let central = bytes
            .windows(4)
            .position(|window| window == b"PK\x01\x02")
            .expect("a central directory header");
        bytes[central + 24..central + 28].copy_from_slice(&DECLARED.to_le_bytes());
        let (mut apk, path) = open_apk(&bytes, "manifest-understated");
        let info = apk.package_info();
        std::fs::remove_file(&path).ok();
        assert!(
            matches!(info, Err(ApkError::ManifestLongerThanDeclared(16))),
            "got {info:?}"
        );
    }

    #[test]
    fn read_entry_missing_is_typed_error_not_panic() {
        let bytes = build_apk(&[(MANIFEST_ENTRY, FIXTURE_MANIFEST)]);
        let (mut apk, path) = open_apk(&bytes, "read-missing");
        let err = apk
            .read_entry("definitely/not/here.bin")
            .expect_err("absent entry");
        std::fs::remove_file(&path).ok();
        match err {
            ApkError::EntryMissing(name) => assert_eq!(name, "definitely/not/here.bin"),
            other => panic!("expected EntryMissing, got {other:?}"),
        }
    }

    #[test]
    fn read_entry_deflated_is_bounded_and_roundtrips() {
        let payload = vec![0x41u8; 256 * 1024];
        let bytes = build_apk_methods(&[("big.bin", &payload, CompressionMethod::Deflated)]);

        assert!(
            bytes.len() < payload.len() / 2,
            "repetitive payload should compress well in the fixture"
        );
        let (mut apk, path) = open_apk(&bytes, "deflate-bounded");
        let got = apk.read_entry("big.bin").expect("read deflated entry");
        std::fs::remove_file(&path).ok();
        assert_eq!(
            got.len(),
            payload.len(),
            "decompressed length is the declared size"
        );
        assert_eq!(got, payload, "deflated bytes round-trip exactly");
    }

    #[test]
    fn empty_file_open_is_typed_error() {
        let path = temp_file("empty", b"");
        let err = Apk::open(&path).err().expect("empty file must fail");
        std::fs::remove_file(&path).ok();
        assert!(
            matches!(err, ApkError::Zip(_) | ApkError::Io(_)),
            "got {err:?}"
        );
    }

    fn open_unsigned_set(paths: ApkSetPaths) -> Result<ApkSet, ApkSetError> {
        ApkSetFiles::open(paths)?.assemble(SigningCertificateHistory::unverified(vec![
            b"test certificate".to_vec(),
        ]))
    }

    fn roblox_manifest(version_code: u32, split: Option<&str>) -> Vec<u8> {
        axml::fixture::Manifest {
            package: ROBLOX_PACKAGE,
            version_code: Some(version_code),
            version_name: split.is_none().then_some("2.737.1584"),
            split,
            launcher: split
                .is_none()
                .then_some("com.roblox.client.startup.ActivitySplash"),
        }
        .encode()
    }

    fn temp_set_dir(tag: &str) -> PathBuf {
        let dir = std::env::temp_dir().join(format!(
            "eclipse-apk-set-test-{tag}-{:?}",
            std::thread::current().id()
        ));
        std::fs::remove_dir_all(&dir).ok();
        std::fs::create_dir_all(&dir).expect("create temp set dir");
        dir
    }

    fn write_set(tag: &str, base: &[u8], split: Option<&[u8]>) -> (PathBuf, ApkSetPaths) {
        let dir = temp_set_dir(tag);
        std::fs::write(dir.join(BASE_APK), base).expect("write base.apk");
        if let Some(split) = split {
            std::fs::write(dir.join(NATIVE_SPLIT_APK), split).expect("write split");
        }
        let paths = ApkSetPaths::locate(&dir).expect("locate synthetic set");
        (dir, paths)
    }

    #[test]
    fn package_info_reads_version_name_and_split() {
        let bytes = build_apk(&[(MANIFEST_ENTRY, &roblox_manifest(3056, None))]);
        let (mut apk, path) = open_apk(&bytes, "package-info-base");
        let info = apk.package_info().expect("base package info");
        std::fs::remove_file(&path).ok();
        assert_eq!(info.package, ROBLOX_PACKAGE);
        assert_eq!(info.version_code, Some(VersionCode(3056)));
        assert_eq!(info.version_name.as_deref(), Some("2.737.1584"));
        assert_eq!(info.split, None);
    }

    #[test]
    fn split_manifest_has_package_info_but_no_launcher_manifest() {
        let bytes = build_apk(&[(
            MANIFEST_ENTRY,
            &roblox_manifest(3056, Some(NATIVE_SPLIT_NAME)),
        )]);
        let (mut apk, path) = open_apk(&bytes, "package-info-split");
        let info = apk.package_info().expect("split package info");
        let manifest = apk.manifest();
        std::fs::remove_file(&path).ok();
        assert_eq!(info.split.as_deref(), Some(NATIVE_SPLIT_NAME));
        assert_eq!(info.version_code, Some(VersionCode(3056)));
        assert!(
            matches!(manifest, Err(ApkError::Axml(AxmlError::NoLauncher))),
            "got {manifest:?}"
        );
    }

    #[test]
    fn locate_accepts_a_file_or_a_directory_with_an_optional_split() {
        let dir = temp_set_dir("locate");
        let file = dir.join("roblox.apk");
        std::fs::write(&file, b"apk").unwrap();
        assert_eq!(
            ApkSetPaths::locate(&file).unwrap(),
            ApkSetPaths {
                base: file.clone(),
                native_split: None
            }
        );

        let err = ApkSetPaths::locate(&dir).expect_err("directory without base.apk");
        assert!(matches!(err, ApkSetError::MissingBase(_)), "got {err:?}");

        std::fs::write(dir.join(BASE_APK), b"base").unwrap();
        let base_only = ApkSetPaths::locate(&dir).unwrap();
        assert_eq!(base_only.native_split, None);
        assert_eq!(base_only.native_libs(), dir.join(BASE_APK));

        std::fs::write(dir.join(NATIVE_SPLIT_APK), b"split").unwrap();
        let with_split = ApkSetPaths::locate(&dir).unwrap();
        assert_eq!(with_split.base, dir.join(BASE_APK));
        assert_eq!(with_split.native_libs(), dir.join(NATIVE_SPLIT_APK));

        let err = ApkSetPaths::locate(&dir.join("absent")).expect_err("missing path");
        assert!(matches!(err, ApkSetError::Locate { .. }), "got {err:?}");

        let bundle = dir.join("Roblox.XAPK");
        std::fs::write(&bundle, b"bundle").unwrap();
        let err = ApkSetPaths::locate(&bundle).expect_err("a bundle is not an APK");
        assert!(
            matches!(&err, ApkSetError::Bundle(path) if *path == bundle),
            "got {err:?}"
        );
        assert!(err.to_string().contains("`eclipse install "), "{err}");
        std::fs::remove_dir_all(&dir).ok();
    }

    #[test]
    fn the_bundle_install_command_quotes_paths_for_the_shell() {
        let bundle = PathBuf::from("/home/u/My Games/Roblox's copy.xapk");
        let message = ApkSetError::Bundle(bundle).to_string();
        assert!(
            message.contains(r"`eclipse install '/home/u/My Games/Roblox'\''s copy.xapk'`"),
            "{message}"
        );
        let plain = ApkSetError::Bundle(PathBuf::from("/home/u/Roblox_2.740.931.xapk"));
        assert!(
            plain
                .to_string()
                .contains("`eclipse install /home/u/Roblox_2.740.931.xapk`"),
            "{plain}"
        );
    }

    #[test]
    fn unsigned_sets_are_refused_before_their_contents_are_trusted() {
        let base = build_apk(&[
            (MANIFEST_ENTRY, &roblox_manifest(3056, None)),
            ("lib/x86_64/libroblox.so", b"engine"),
        ]);
        let (dir, paths) = write_set("unsigned", &base, None);
        let err = ApkSet::open(paths).err().expect("unsigned APK must fail");
        std::fs::remove_dir_all(&dir).ok();
        assert!(
            matches!(
                err,
                ApkSetError::Signature {
                    source: SignatureError::MissingV2Signature,
                    ..
                }
            ),
            "got {err:?}"
        );
        let text = err.to_string();
        assert!(text.contains("signed by Roblox Corporation"), "{text}");
        assert!(text.contains("`eclipse update --play`"), "{text}");
        assert!(
            !text.contains("from Google Play with `eclipse update`"),
            "{text}"
        );

        let missing = temp_set_dir("missing-member");
        let err = ApkSet::open(ApkSetPaths {
            base: missing.join(BASE_APK),
            native_split: None,
        })
        .err()
        .expect("a missing APK cannot be opened");
        std::fs::remove_dir_all(&missing).ok();
        assert!(matches!(err, ApkSetError::Locate { .. }), "got {err:?}");
    }

    #[test]
    fn apks_that_cannot_be_read_are_not_called_unofficial() {
        let dir = temp_set_dir("unreadable-member");
        let missing = dir.join(BASE_APK);
        let unopened = ApkSet::open(ApkSetPaths {
            base: missing.clone(),
            native_split: None,
        });
        let write_only = dir.join("write-only.apk");
        std::fs::write(&write_only, b"apk").unwrap();
        let file = OpenOptions::new().write(true).open(&write_only).unwrap();
        let unread = ApkSetFiles {
            base: Member {
                path: write_only.clone(),
                file,
            },
            native_split: None,
        }
        .verify();
        std::fs::remove_dir_all(&dir).ok();
        for (base, opened) in [(missing, unopened), (write_only, unread)] {
            let err = opened
                .err()
                .expect("an APK that cannot be read cannot be opened");
            assert!(
                matches!(&err, ApkSetError::Locate { path, .. } if *path == base),
                "got {err:?}"
            );
            assert!(!err.to_string().contains("not the official"), "{err}");
        }
    }

    #[test]
    fn split_set_takes_manifest_from_base_and_native_libs_from_split() {
        let base = build_apk(&[
            (MANIFEST_ENTRY, &roblox_manifest(3056, None)),
            ("classes.dex", b"dex"),
        ]);
        let split = build_apk(&[
            (
                MANIFEST_ENTRY,
                &roblox_manifest(3056, Some(NATIVE_SPLIT_NAME)),
            ),
            ("lib/x86_64/libroblox.so", b"engine"),
        ]);
        let (dir, paths) = write_set("split-set", &base, Some(&split));
        let mut set = open_unsigned_set(paths).expect("consistent split set");
        assert_eq!(set.version_code(), VersionCode(3056));
        assert_eq!(set.version_name(), Some("2.737.1584"));
        assert_eq!(
            set.manifest().launcher_activity,
            "com.roblox.client.startup.ActivitySplash"
        );
        assert_eq!(set.base_path(), dir.join(BASE_APK));
        assert_eq!(set.native_libs_path(), dir.join(NATIVE_SPLIT_APK));
        assert_eq!(
            set.native_libs_mut().native_lib_filenames(TARGET_ABI),
            vec![ENGINE_LIB.to_string()]
        );
        assert!(set.base_mut().native_lib_filenames(TARGET_ABI).is_empty());
        drop(set);
        std::fs::remove_dir_all(&dir).ok();
    }

    #[test]
    fn a_verified_set_follows_its_own_files_to_a_new_directory_only() {
        let base = build_apk(&[
            (MANIFEST_ENTRY, &roblox_manifest(3056, None)),
            ("classes.dex", b"dex"),
        ]);
        let split = build_apk(&[
            (
                MANIFEST_ENTRY,
                &roblox_manifest(3056, Some(NATIVE_SPLIT_NAME)),
            ),
            ("lib/x86_64/libroblox.so", b"engine"),
        ]);
        let (staged, paths) = write_set("relocate-staged", &base, Some(&split));
        let moved = staged.with_file_name(format!(
            "eclipse-apk-set-test-relocate-moved-{:?}",
            std::thread::current().id()
        ));
        std::fs::remove_dir_all(&moved).ok();
        let set = open_unsigned_set(paths).expect("consistent split set");
        std::fs::rename(&staged, &moved).unwrap();
        let set = set
            .relocated(ApkSetPaths::locate(&moved).unwrap())
            .expect("the verified files moved together");
        assert_eq!(set.base_path(), moved.join(BASE_APK));
        assert_eq!(set.native_libs_path(), moved.join(NATIVE_SPLIT_APK));
        assert_eq!(set.base.path(), moved.join(BASE_APK));

        let (copies, copy_paths) = write_set("relocate-copies", &base, Some(&split));
        let err = set
            .relocated(copy_paths)
            .err()
            .expect("copies are other files");
        assert!(
            matches!(&err, ApkSetError::Replaced(path) if *path == copies.join(BASE_APK)),
            "{err:?}"
        );

        let set = open_unsigned_set(ApkSetPaths::locate(&moved).unwrap()).unwrap();
        let base_only = temp_set_dir("relocate-base-only");
        std::fs::hard_link(moved.join(BASE_APK), base_only.join(BASE_APK)).unwrap();
        let err = set
            .relocated(ApkSetPaths::locate(&base_only).unwrap())
            .err()
            .expect("the verified split is missing");
        assert!(
            matches!(&err, ApkSetError::Replaced(path) if *path == base_only.join(NATIVE_SPLIT_APK)),
            "{err:?}"
        );
        assert!(err.to_string().contains("signature check"), "{err}");

        for dir in [moved, copies, base_only] {
            std::fs::remove_dir_all(dir).ok();
        }
    }

    #[test]
    fn universal_base_provides_its_own_native_libs() {
        let base = build_apk(&[
            (MANIFEST_ENTRY, &roblox_manifest(3056, None)),
            ("lib/x86_64/libroblox.so", b"engine"),
        ]);
        let (dir, paths) = write_set("universal", &base, None);
        let set = open_unsigned_set(paths).expect("universal APK");
        assert_eq!(set.native_libs_path(), dir.join(BASE_APK));
        drop(set);
        std::fs::remove_dir_all(&dir).ok();
    }

    #[test]
    fn inconsistent_sets_are_typed_errors() {
        let engine = ("lib/x86_64/libroblox.so", b"engine".as_slice());
        let base = build_apk(&[(MANIFEST_ENTRY, &roblox_manifest(3056, None))]);

        let other_app = axml::fixture::Manifest {
            package: "com.example.app",
            version_code: Some(3056),
            version_name: None,
            split: None,
            launcher: Some(".Main"),
        }
        .encode();
        let (dir, paths) = write_set(
            "wrong-package",
            &build_apk(&[(MANIFEST_ENTRY, &other_app), engine]),
            None,
        );
        let err = open_unsigned_set(paths).err().unwrap();
        assert!(matches!(err, ApkSetError::WrongPackage { .. }), "{err:?}");
        std::fs::remove_dir_all(&dir).ok();

        let split_as_base = build_apk(&[
            (
                MANIFEST_ENTRY,
                &roblox_manifest(3056, Some(NATIVE_SPLIT_NAME)),
            ),
            engine,
        ]);
        let (dir, paths) = write_set("split-as-base", &split_as_base, None);
        let err = open_unsigned_set(paths).err().unwrap();
        assert!(matches!(err, ApkSetError::BaseIsSplit { .. }), "{err:?}");
        std::fs::remove_dir_all(&dir).ok();

        let density_split = build_apk(&[
            (
                MANIFEST_ENTRY,
                &roblox_manifest(3056, Some("config.xxhdpi")),
            ),
            engine,
        ]);
        let (dir, paths) = write_set("density-split", &base, Some(&density_split));
        let err = open_unsigned_set(paths).err().unwrap();
        assert!(matches!(err, ApkSetError::NotNativeSplit { .. }), "{err:?}");
        std::fs::remove_dir_all(&dir).ok();

        let older_split = build_apk(&[
            (
                MANIFEST_ENTRY,
                &roblox_manifest(3055, Some(NATIVE_SPLIT_NAME)),
            ),
            engine,
        ]);
        let (dir, paths) = write_set("version-mismatch", &base, Some(&older_split));
        let err = open_unsigned_set(paths).err().unwrap();
        assert!(
            matches!(
                err,
                ApkSetError::VersionMismatch {
                    base: VersionCode(3056),
                    split: Some(VersionCode(3055)),
                    ..
                }
            ),
            "{err:?}"
        );
        std::fs::remove_dir_all(&dir).ok();

        let empty_split = build_apk(&[(
            MANIFEST_ENTRY,
            &roblox_manifest(3056, Some(NATIVE_SPLIT_NAME)),
        )]);
        let (dir, paths) = write_set("split-without-engine", &base, Some(&empty_split));
        let err = open_unsigned_set(paths).err().unwrap();
        assert!(
            matches!(
                err,
                ApkSetError::EngineMissing {
                    has_native_split: true,
                    ..
                }
            ),
            "{err:?}"
        );
        std::fs::remove_dir_all(&dir).ok();

        let (dir, paths) = write_set("base-without-split", &base, None);
        let err = open_unsigned_set(paths).err().unwrap();
        assert!(
            matches!(
                err,
                ApkSetError::EngineMissing {
                    has_native_split: false,
                    ..
                }
            ),
            "{err:?}"
        );
        assert!(err.to_string().contains(NATIVE_SPLIT_APK), "{err}");
        std::fs::remove_dir_all(&dir).ok();

        let unversioned = axml::fixture::Manifest {
            package: ROBLOX_PACKAGE,
            version_code: None,
            version_name: None,
            split: None,
            launcher: Some(".Main"),
        }
        .encode();
        let (dir, paths) = write_set(
            "unversioned",
            &build_apk(&[(MANIFEST_ENTRY, &unversioned), engine]),
            None,
        );
        let err = open_unsigned_set(paths).err().unwrap();
        assert!(matches!(err, ApkSetError::MissingVersionCode(_)), "{err:?}");
        std::fs::remove_dir_all(&dir).ok();
    }

    #[test]
    fn official_roblox_set_opens_and_its_base_alone_is_refused() {
        let Some(paths) = ApkSetPaths::from_env().expect("ECLIPSE_ROBLOX_APK must be usable")
        else {
            eprintln!("SKIP: set ECLIPSE_ROBLOX_APK to verify the official Roblox APK set");
            return;
        };

        let mut set = ApkSet::open(paths.clone()).expect("the official Roblox set verifies");
        assert_eq!(set.manifest().package, ROBLOX_PACKAGE);
        assert_eq!(
            set.signing_certificate_history(),
            &signature::verify_roblox_signing_history(&File::open(&paths.base).unwrap())
                .expect("the official base verifies")
        );
        assert!(set.version_code().0 > 0);
        assert!(set
            .native_libs_mut()
            .native_lib_filenames(TARGET_ABI)
            .iter()
            .any(|name| name == ENGINE_LIB));

        if paths.native_split.is_some() {
            let err = ApkSet::open(ApkSetPaths {
                base: paths.base.clone(),
                native_split: None,
            })
            .err()
            .expect("the base alone lacks the engine");
            assert!(
                matches!(
                    err,
                    ApkSetError::EngineMissing {
                        has_native_split: false,
                        ..
                    }
                ),
                "{err:?}"
            );
        }
    }
}
