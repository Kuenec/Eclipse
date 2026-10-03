use std::collections::{BTreeSet, HashSet};
use std::ffi::OsStr;
use std::fmt;
use std::fs;
use std::io;
use std::os::unix::fs::{DirBuilderExt as _, MetadataExt as _, PermissionsExt as _};
use std::path::{Path, PathBuf};
use std::time::{Duration, SystemTime};

use serde::ser::SerializeStruct as _;
use serde::{Serialize, Serializer};

use crate::apk::store::{InstalledVersion, Store, StoreError, VersionRole, VersionState};
use crate::runtime::{NativeLibRoot, RuntimeError, StoreArtCode};
use crate::status::{mebibytes, StatusSink};
use crate::webview::client::{ClientError, Storage as WebviewStorage};

const MIB: u64 = 1024 * 1024;

pub const CLIENT_CACHE_CAP: u64 = 512 * MIB;

const LISTED_APP_DATA_BYTES: u64 = 10 * MIB;

const REPORT_SCHEMA: u32 = 1;

const ANDROID_FILES_DIR: &str = "files";

const ASSETS_DIR: &str = "assets";

const TRIM_INTERVAL: Duration = Duration::from_secs(24 * 60 * 60);

const TRIM_MARKER_SUFFIX: &str = ".trimmed";

const CONTENT_PROVIDER_PREFIX: &str = "ContentProvider_";

const OWNER_ONLY: u32 = 0o700;

const STAT_BLOCK_BYTES: u64 = 512;

const FLATPAK_ID_ENV: &str = "FLATPAK_ID";

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Trim {
    NotDue,
    Done { freed_bytes: u64 },
}

#[derive(Debug)]
pub enum StorageError {
    Io {
        action: &'static str,
        path: PathBuf,
        source: io::Error,
    },

    Logs(io::Error),

    Store(StoreError),

    Runtime(RuntimeError),

    Webview(ClientError),

    NoCacheDir,

    StoreNotAbsolute(PathBuf),
}

impl StorageError {
    fn new(action: &'static str, path: &Path, source: io::Error) -> Self {
        Self::Io {
            action,
            path: path.to_path_buf(),
            source,
        }
    }
}

impl fmt::Display for StorageError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Io {
                action,
                path,
                source,
            } => write!(f, "cannot {action} {}: {source}", path.display()),
            Self::Logs(error) => write!(f, "{error}"),
            Self::Store(error) => write!(f, "{error}"),
            Self::Runtime(error) => write!(f, "{error}"),
            Self::Webview(error) => write!(f, "{error}"),
            Self::NoCacheDir => {
                f.write_str("cannot resolve the cache directory; set XDG_CACHE_HOME or HOME")
            }
            Self::StoreNotAbsolute(path) => write!(
                f,
                "the Roblox store {} is not an absolute path",
                path.display()
            ),
        }
    }
}

impl std::error::Error for StorageError {
    fn source(&self) -> Option<&(dyn std::error::Error + 'static)> {
        match self {
            Self::Io { source, .. } | Self::Logs(source) => Some(source),
            Self::Store(error) => Some(error),
            Self::Runtime(error) => Some(error),
            Self::Webview(error) => Some(error),
            Self::NoCacheDir | Self::StoreNotAbsolute(_) => None,
        }
    }
}

impl From<StoreError> for StorageError {
    fn from(error: StoreError) -> Self {
        Self::Store(error)
    }
}

impl From<RuntimeError> for StorageError {
    fn from(error: RuntimeError) -> Self {
        Self::Runtime(error)
    }
}

pub fn extracted_assets_dir(app_data: &Path) -> PathBuf {
    app_data.join(ANDROID_FILES_DIR).join(ASSETS_DIR)
}

pub struct StorageLayout {
    pub store: Store,

    pub app_data: PathBuf,

    pub native_libs: PathBuf,

    pub art_cache: PathBuf,

    pub client_cache: PathBuf,

    pub webview: WebviewStorage,

    pub private_cache_root: Option<PathBuf>,
}

impl StorageLayout {
    pub fn resolve(app_data: PathBuf) -> Result<Self, StorageError> {
        let (NativeLibRoot::Cache(native_libs) | NativeLibRoot::Override(native_libs)) =
            crate::runtime::native_lib_root()?;
        let private_cache_root = match std::env::var_os(FLATPAK_ID_ENV) {
            Some(_) => Some(
                directories::BaseDirs::new()
                    .ok_or(StorageError::NoCacheDir)?
                    .cache_dir()
                    .to_path_buf(),
            ),
            None => None,
        };
        Ok(Self {
            store: Store::open()?,
            app_data,
            native_libs,
            art_cache: crate::runtime::dalvik_cache_dir().ok_or(StorageError::NoCacheDir)?,
            client_cache: crate::runtime::client_cache_dir()?.path().to_path_buf(),
            webview: crate::webview::client::storage_dirs().map_err(StorageError::Webview)?,
            private_cache_root,
        })
    }

    fn log_dir(&self) -> PathBuf {
        crate::diagnostics::log_dir(&self.app_data)
    }

    fn older_logs(&self) -> Result<Vec<PathBuf>, StorageError> {
        crate::diagnostics::older_run_logs(&self.log_dir()).map_err(StorageError::Logs)
    }

    pub fn report(&self) -> Result<Report, StorageError> {
        let mut tally = Tally::default();
        let mut entries = Vec::new();
        let versions = self.store.versions()?;
        for stored in &versions {
            entries.push(Entry {
                category: Category::RobloxVersion {
                    version: stored.version.clone(),
                    role: stored.role,
                    starts: Starts(stored.state),
                },
                bytes: tally.tree(&stored.dir, &[])?,
                path: stored.dir.clone(),
            });
        }
        let version_dirs: Vec<&Path> = versions.iter().map(|stored| stored.dir.as_path()).collect();
        let store = self.store.root();
        entries.push(Entry {
            category: Category::StoreRecords,
            bytes: tally.tree(store, &version_dirs)?,
            path: store.to_path_buf(),
        });
        let assets = extracted_assets_dir(&self.app_data);
        for (category, path) in [
            (Category::NativeLibs, &self.native_libs),
            (Category::Assets, &assets),
        ] {
            entries.push(Entry {
                category,
                bytes: tally.tree(path, &[])?,
                path: path.clone(),
            });
        }
        let art_code = self.art_code()?;
        let art_code: Vec<&Path> = art_code.iter().map(PathBuf::as_path).collect();
        entries.push(Entry {
            category: Category::ArtCode,
            bytes: tally.paths(&art_code)?,
            path: self.art_cache.clone(),
        });
        let cef_profile = &self.webview.cef_profile;
        let cef_present = cef_profile
            .try_exists()
            .map_err(|source| StorageError::new("inspect", cef_profile, source))?;
        let caches = [
            (
                Category::ClientCache {
                    cap_bytes: CLIENT_CACHE_CAP,
                },
                &self.client_cache,
            ),
            (Category::WebviewCache, &self.webview.cache),
            (Category::WebviewData, &self.webview.data),
        ];
        let cef = cef_present.then_some((Category::CefProfile, cef_profile));
        for (category, path) in caches.into_iter().chain(cef) {
            entries.push(Entry {
                category,
                bytes: tally.tree(path, &[])?,
                path: path.clone(),
            });
        }
        if let Some(cache_root) = &self.private_cache_root {
            let counted: Vec<&Path> = [
                self.native_libs.as_path(),
                &self.client_cache,
                &self.webview.cache,
            ]
            .into_iter()
            .chain(art_code.iter().copied())
            .collect();
            entries.push(Entry {
                category: Category::OtherCache,
                bytes: tally.tree(cache_root, &counted)?,
                path: cache_root.clone(),
            });
        }
        let log_dir = self.log_dir();
        let (bytes, largest) = tally.app_data(
            &self.app_data,
            &[&assets, &log_dir, &self.webview.data, cef_profile],
        )?;
        entries.push(Entry {
            category: Category::AppData { largest },
            bytes,
            path: self.app_data.clone(),
        });
        let older_logs = self.older_logs()?;
        let older_logs: Vec<&Path> = older_logs.iter().map(PathBuf::as_path).collect();
        entries.push(Entry {
            category: Category::Log,
            bytes: tally.tree(&log_dir, &older_logs)?,
            path: log_dir.clone(),
        });
        entries.push(Entry {
            category: Category::OlderLogs,
            bytes: tally.paths(&older_logs)?,
            path: log_dir,
        });
        Ok(Report::new(entries))
    }

    fn art_code(&self) -> Result<Vec<PathBuf>, StorageError> {
        let store = self.store.root();
        let names = StoreArtCode::in_store(store)
            .ok_or_else(|| StorageError::StoreNotAbsolute(store.to_path_buf()))?;
        Ok(children(&self.art_cache)?
            .into_iter()
            .filter(|path| {
                path.file_name()
                    .and_then(|name| names.version_of(name))
                    .is_some()
            })
            .collect())
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Starts(pub VersionState);

impl Serialize for Starts {
    fn serialize<S: Serializer>(&self, serializer: S) -> Result<S::Ok, S::Error> {
        let (state, failed_starts) = match self.0 {
            VersionState::Unproven { failed_starts } => ("unproven", failed_starts),
            VersionState::Proven => ("proven", 0),
            VersionState::Played => ("played", 0),
            VersionState::BothFailed => ("both_failed", 0),
        };
        let mut fields = serializer.serialize_struct("Starts", 2)?;
        fields.serialize_field("state", state)?;
        fields.serialize_field("failed_starts", &failed_starts)?;
        fields.end()
    }
}

impl fmt::Display for Starts {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self.0 {
            VersionState::Unproven { failed_starts: 0 } => f.write_str("not started yet"),
            VersionState::Unproven { failed_starts: 1 } => f.write_str("failed to start once"),
            VersionState::Unproven { failed_starts } => {
                write!(f, "failed to start {failed_starts} times")
            }
            VersionState::Proven => f.write_str("has started"),
            VersionState::Played => f.write_str("has been played"),
            VersionState::BothFailed => {
                f.write_str("failed to start, as did the version kept before it")
            }
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct LargeEntry {
    pub path: PathBuf,

    pub bytes: u64,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
#[serde(tag = "kind", rename_all = "snake_case")]
pub enum Category {
    RobloxVersion {
        #[serde(flatten)]
        version: InstalledVersion,

        role: VersionRole,

        #[serde(flatten)]
        starts: Starts,
    },

    StoreRecords,

    NativeLibs,

    Assets,

    ArtCode,

    ClientCache {
        cap_bytes: u64,
    },

    WebviewCache,

    WebviewData,

    CefProfile,

    OtherCache,

    AppData {
        largest: Vec<LargeEntry>,
    },

    Log,

    OlderLogs,
}

impl Category {
    pub fn cleanable(&self) -> bool {
        match self {
            Self::RobloxVersion { role, .. } => *role == VersionRole::Unkept,
            Self::ClientCache { .. } | Self::WebviewCache | Self::OlderLogs => true,
            Self::StoreRecords
            | Self::NativeLibs
            | Self::Assets
            | Self::ArtCode
            | Self::WebviewData
            | Self::CefProfile
            | Self::OtherCache
            | Self::AppData { .. }
            | Self::Log => false,
        }
    }
}

impl fmt::Display for Category {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::RobloxVersion {
                version,
                role,
                starts,
            } => {
                let role = match role {
                    VersionRole::Current => "current",
                    VersionRole::Fallback => "kept to go back to",
                    VersionRole::Failed => "kept while the older one is tried",
                    VersionRole::Unkept => "no longer kept",
                };
                match &version.version_name {
                    Some(name) => write!(f, "Roblox {name}, {role}, {starts}"),
                    None => write!(
                        f,
                        "Roblox versionCode {}, {role}, {starts}",
                        version.version_code
                    ),
                }
            }
            Self::StoreRecords => f.write_str("Roblox store records"),
            Self::NativeLibs => f.write_str("Roblox native libraries"),
            Self::Assets => f.write_str("Roblox assets copied from the APK"),
            Self::ArtCode => f.write_str("Roblox code compiled by ART"),
            Self::ClientCache { cap_bytes } => {
                write!(f, "Roblox's cache, trimmed above {} MiB", cap_bytes / MIB)
            }
            Self::WebviewCache => f.write_str("WebView cache"),
            Self::WebviewData => f.write_str("WebView data, with the login"),
            Self::CefProfile => f.write_str("WebView profile of Eclipse 0.1.4 and older"),
            Self::OtherCache => f.write_str("GPU shader caches and other cached files"),
            Self::AppData { .. } => f.write_str("Roblox app data"),
            Self::Log => f.write_str("Log of the last run"),
            Self::OlderLogs => f.write_str("Logs of older runs"),
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Entry {
    pub category: Category,

    pub bytes: u64,

    pub path: PathBuf,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Report {
    pub entries: Vec<Entry>,

    pub total_bytes: u64,
}

#[derive(Serialize)]
struct JsonReport<'a> {
    schema: u32,

    total_bytes: u64,

    entries: Vec<JsonEntry<'a>>,
}

#[derive(Serialize)]
struct JsonEntry<'a> {
    #[serde(flatten)]
    category: &'a Category,

    bytes: u64,

    cleanable: bool,

    path: &'a Path,
}

impl Report {
    fn new(entries: Vec<Entry>) -> Self {
        let total_bytes = entries.iter().map(|entry| entry.bytes).sum();
        Self {
            entries,
            total_bytes,
        }
    }

    pub fn to_json(&self) -> Result<String, serde_json::Error> {
        serde_json::to_string(&JsonReport {
            schema: REPORT_SCHEMA,
            total_bytes: self.total_bytes,
            entries: self
                .entries
                .iter()
                .map(|entry| JsonEntry {
                    category: &entry.category,
                    bytes: entry.bytes,
                    cleanable: entry.category.cleanable(),
                    path: &entry.path,
                })
                .collect(),
        })
    }
}

impl fmt::Display for Report {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        let base = shared_parent(self.entries.iter().map(|entry| entry.path.as_path()));
        let mut rows = Vec::new();
        for entry in &self.entries {
            let mark = if entry.category.cleanable() { " *" } else { "" };
            let path = base
                .as_deref()
                .and_then(|base| entry.path.strip_prefix(base).ok())
                .unwrap_or(&entry.path);
            rows.push((entry.bytes, format!("{}{mark}", entry.category), Some(path)));
            if let Category::AppData { largest } = &entry.category {
                for part in largest {
                    let name = part.path.strip_prefix(&entry.path).unwrap_or(&part.path);
                    rows.push((part.bytes, format!("  {}", name.display()), None));
                }
            }
        }
        let width = rows
            .iter()
            .map(|(_, what, _)| what.chars().count())
            .max()
            .unwrap_or(0);
        if let Some(base) = &base {
            writeln!(f, "Files under {}/", base.display())?;
        }
        writeln!(f, "{:>9}  {:<width$}  Where", "MiB", "What")?;
        for (bytes, what, path) in rows {
            match path {
                Some(path) => writeln!(
                    f,
                    "{:>9.1}  {what:<width$}  {}",
                    mebibytes(bytes),
                    path.display()
                )?,
                None => writeln!(f, "{:>9.1}  {what}", mebibytes(bytes))?,
            }
        }
        writeln!(f, "{:>9.1}  Total", mebibytes(self.total_bytes))?;
        writeln!(f, "* emptied by `eclipse storage --clean`")
    }
}

fn shared_parent<'a>(mut paths: impl Iterator<Item = &'a Path>) -> Option<PathBuf> {
    let mut shared = paths.next()?.parent()?.to_path_buf();
    for path in paths {
        let parent = path.parent()?;
        while !parent.starts_with(&shared) {
            if !shared.pop() {
                return None;
            }
        }
    }
    shared.parent().is_some().then_some(shared)
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Cleaned {
    ClientCache,

    WebviewCache,

    OlderLogs,

    UnkeptVersions,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Freed {
    pub cleaned: Cleaned,

    pub bytes: u64,
}

impl fmt::Display for Freed {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        let from = match self.cleaned {
            Cleaned::ClientCache => "from Roblox's cache",
            Cleaned::WebviewCache => "from the WebView cache",
            Cleaned::OlderLogs => "of logs from older runs",
            Cleaned::UnkeptVersions => "of Roblox versions and downloads Eclipse no longer keeps",
        };
        write!(f, "freed {:.1} MiB {from}", mebibytes(self.bytes))
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Cleanup {
    pub freed: Vec<Freed>,
}

impl fmt::Display for Cleanup {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        for freed in &self.freed {
            writeln!(f, "{freed}")?;
        }
        let total: u64 = self.freed.iter().map(|freed| freed.bytes).sum();
        writeln!(f, "freed {:.1} MiB in all", mebibytes(total))
    }
}

pub fn clean(layout: &StorageLayout, status: &StatusSink) -> Result<Cleanup, StorageError> {
    let install = layout.store.lock_install(status)?;
    let older_logs = layout.older_logs()?;
    let older_logs: Vec<&Path> = older_logs.iter().map(PathBuf::as_path).collect();
    let freed = vec![
        Freed {
            cleaned: Cleaned::ClientCache,
            bytes: freeing(&[&layout.client_cache], || empty_dir(&layout.client_cache))?,
        },
        Freed {
            cleaned: Cleaned::WebviewCache,
            bytes: freeing(&[&layout.webview.cache], || {
                empty_dir(&layout.webview.cache)
            })?,
        },
        Freed {
            cleaned: Cleaned::OlderLogs,
            bytes: freeing(&older_logs, || remove_files(&older_logs))?,
        },
        Freed {
            cleaned: Cleaned::UnkeptVersions,
            bytes: freeing(&[layout.store.root()], || Ok(install.prune()?))?,
        },
    ];
    Ok(Cleanup { freed })
}

fn freeing(
    paths: &[&Path],
    remove: impl FnOnce() -> Result<(), StorageError>,
) -> Result<u64, StorageError> {
    let before = Tally::default().paths(paths)?;
    remove()?;
    Ok(before.saturating_sub(Tally::default().paths(paths)?))
}

fn empty_dir(dir: &Path) -> Result<(), StorageError> {
    for path in children(dir)? {
        let file_type = fs::symlink_metadata(&path)
            .map_err(|source| StorageError::new("inspect", &path, source))?
            .file_type();
        let removed = if file_type.is_dir() {
            fs::remove_dir_all(&path)
        } else {
            fs::remove_file(&path)
        };
        removed.map_err(|source| StorageError::new("remove", &path, source))?;
    }
    Ok(())
}

fn remove_files(paths: &[&Path]) -> Result<(), StorageError> {
    for path in paths {
        match fs::remove_file(path) {
            Ok(()) => {}
            Err(error) if error.kind() == io::ErrorKind::NotFound => {}
            Err(source) => return Err(StorageError::new("remove", path, source)),
        }
    }
    Ok(())
}

fn children(dir: &Path) -> Result<Vec<PathBuf>, StorageError> {
    let entries = match fs::read_dir(dir) {
        Ok(entries) => entries,
        Err(error) if error.kind() == io::ErrorKind::NotFound => return Ok(Vec::new()),
        Err(source) => return Err(StorageError::new("read", dir, source)),
    };
    entries
        .map(|entry| {
            entry
                .map(|entry| entry.path())
                .map_err(|source| StorageError::new("read", dir, source))
        })
        .collect()
}

struct Node {
    bytes: u64,
    is_dir: bool,
}

#[derive(Default)]
struct Tally {
    counted_links: HashSet<(u64, u64)>,
}

impl Tally {
    fn node(&mut self, path: &Path) -> Result<Option<Node>, StorageError> {
        let metadata = match fs::symlink_metadata(path) {
            Ok(metadata) => metadata,
            Err(error) if error.kind() == io::ErrorKind::NotFound => return Ok(None),
            Err(source) => return Err(StorageError::new("inspect", path, source)),
        };
        let is_dir = metadata.is_dir();
        let counted = !is_dir
            && metadata.nlink() > 1
            && !self.counted_links.insert((metadata.dev(), metadata.ino()));
        let bytes = if counted {
            0
        } else {
            metadata.blocks() * STAT_BLOCK_BYTES
        };
        Ok(Some(Node { bytes, is_dir }))
    }

    fn tree(&mut self, root: &Path, skip: &[&Path]) -> Result<u64, StorageError> {
        let mut bytes = 0;
        let mut pending = vec![root.to_path_buf()];
        while let Some(path) = pending.pop() {
            if skip.contains(&path.as_path()) {
                continue;
            }
            let Some(node) = self.node(&path)? else {
                continue;
            };
            bytes += node.bytes;
            if node.is_dir {
                pending.extend(children(&path)?);
            }
        }
        Ok(bytes)
    }

    fn paths(&mut self, paths: &[&Path]) -> Result<u64, StorageError> {
        let mut bytes = 0;
        for path in paths {
            bytes += self.tree(path, &[])?;
        }
        Ok(bytes)
    }

    fn app_data(
        &mut self,
        root: &Path,
        skip: &[&Path],
    ) -> Result<(u64, Vec<LargeEntry>), StorageError> {
        let files = root.join(ANDROID_FILES_DIR);
        let mut bytes = 0;
        let mut parts = Vec::new();
        let mut containers = vec![root.to_path_buf()];
        while let Some(container) = containers.pop() {
            let Some(node) = self.node(&container)? else {
                continue;
            };
            bytes += node.bytes;
            if !node.is_dir {
                continue;
            }
            for path in children(&container)? {
                if skip.contains(&path.as_path()) {
                    continue;
                }
                if path == files {
                    containers.push(path);
                    continue;
                }
                let part = self.tree(&path, skip)?;
                bytes += part;
                parts.push(LargeEntry { path, bytes: part });
            }
        }
        parts.retain(|part| part.bytes >= LISTED_APP_DATA_BYTES);
        parts.sort_by(|a, b| b.bytes.cmp(&a.bytes).then_with(|| a.path.cmp(&b.path)));
        Ok((bytes, parts))
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
    use crate::apk::VersionCode;
    use std::collections::BTreeMap;

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

    const OLDER_RUN: &str = "eclipse-20261003T080000.000Z";

    const LAST_RUN: &str = "eclipse-20261003T090000.000Z";

    fn layout_in(scratch: &Scratch) -> StorageLayout {
        let data = scratch.0.join("data/eclipse");
        let cache = scratch.0.join("cache/eclipse");
        let app_data = data.join("app-data");
        StorageLayout {
            store: Store::at(data.join("roblox")),
            native_libs: cache.join("native-libs"),
            art_cache: scratch.0.join("cache/art/x86_64"),
            client_cache: cache.join("client-cache"),
            webview: WebviewStorage {
                data: app_data.join("webview"),
                cache: cache.join("webview"),
                cef_profile: app_data.join("webview-cef"),
            },
            app_data,
            private_cache_root: Some(scratch.0.join("cache")),
        }
    }

    fn cache_root(layout: &StorageLayout) -> &Path {
        layout.private_cache_root.as_deref().unwrap()
    }

    fn write(path: &Path, len: u64) {
        fs::create_dir_all(path.parent().unwrap()).unwrap();
        fs::write(path, incompressible(len, len)).unwrap();
    }

    fn art_code_file(layout: &StorageLayout) -> PathBuf {
        let mut name = crate::runtime::dalvik_cache_stem(layout.store.root()).unwrap();
        name.push("@3170@base.apk@classes.dex");
        layout.art_cache.join(name)
    }

    fn older_logs(layout: &StorageLayout) -> Vec<PathBuf> {
        let logs = crate::diagnostics::log_dir(&layout.app_data);
        ["log", "tail.log"]
            .map(|suffix| logs.join(format!("{OLDER_RUN}.{suffix}")))
            .into()
    }

    fn large_app_data(layout: &StorageLayout) -> PathBuf {
        layout
            .app_data
            .join(ANDROID_FILES_DIR)
            .join("ota_rbxm_decompressed_cache")
    }

    fn populate(scratch: &Scratch, layout: &StorageLayout) {
        let store = layout.store.root();
        fs::create_dir_all(store).unwrap();
        fs::write(
            store.join("current.json"),
            br#"{"version_code":3170,"version_name":"2.740.931"}"#,
        )
        .unwrap();
        fs::write(
            store.join("last-update-check.json"),
            br#"{"checked_at_unix":1790000000,"outcome":"completed"}"#,
        )
        .unwrap();
        for lock in ["install.lock", "state.lock"] {
            fs::write(store.join(lock), b"").unwrap();
        }
        write(&store.join("3170/base.apk"), 96 * KIB);
        write(&store.join("3170/split_config.x86_64.apk"), 48 * KIB);
        write(&store.join("3056/base.apk"), 80 * KIB);
        write(&store.join("incoming.partial/base.apk"), 40 * KIB);
        write(&layout.native_libs.join("3170/libroblox.so"), 120 * KIB);
        write(
            &extracted_assets_dir(&layout.app_data).join("shaders/shaders_vulkan.pack"),
            64 * KIB,
        );
        write(&art_code_file(layout), 24 * KIB);
        write(
            &layout.art_cache.join("system@framework@boot.art"),
            32 * KIB,
        );
        create_client_cache(&layout.client_cache).unwrap();
        let blob = layout.client_cache.join("rbx-storage/0a/blob");
        write(&blob, 56 * KIB);
        fs::create_dir_all(layout.client_cache.join("http")).unwrap();
        fs::hard_link(&blob, layout.client_cache.join("http/blob")).unwrap();
        let account = scratch.0.join("data/eclipse/google-play.json");
        write(&account, 4 * KIB);
        std::os::unix::fs::symlink(&account, layout.client_cache.join("account")).unwrap();
        write(&scratch.0.join("config/eclipse/config.json"), 3 * KIB);
        write(
            &layout.webview.cache.join("WebKitCache/Blobs/one"),
            12 * KIB,
        );
        write(&layout.webview.data.join("cookies"), 4 * KIB);
        write(&layout.webview.cef_profile.join("Default/Cookies"), 8 * KIB);
        write(
            &cache_root(layout).join("nvidia/GLCache/0a/shader.bin"),
            20 * KIB,
        );
        write(
            &cache_root(layout).join("mesa_shader_cache_db/index"),
            4 * KIB,
        );
        write(
            &large_app_data(layout).join("decompressed"),
            LISTED_APP_DATA_BYTES + KIB,
        );
        write(
            &layout.app_data.join("files/appData/LocalStorage/one"),
            16 * KIB,
        );
        write(&layout.app_data.join("shared_prefs/prefs.xml"), 4 * KIB);
        for (path, len) in older_logs(layout).iter().zip([6 * KIB, 3 * KIB]) {
            write(path, len);
        }
        let logs = crate::diagnostics::log_dir(&layout.app_data);
        write(&logs.join(format!("{LAST_RUN}.log")), 5 * KIB);
        std::os::unix::fs::symlink(format!("{LAST_RUN}.log"), logs.join("eclipse.log")).unwrap();
    }

    fn du(paths: &[&Path]) -> u64 {
        let output = std::process::Command::new("du")
            .args(["-s", "-B1", "-c"])
            .args(paths)
            .output()
            .unwrap();
        assert!(
            output.status.success(),
            "{}",
            String::from_utf8_lossy(&output.stderr)
        );
        String::from_utf8(output.stdout)
            .unwrap()
            .lines()
            .last()
            .and_then(|total| total.split('\t').next())
            .unwrap()
            .parse()
            .unwrap()
    }

    #[test]
    fn totals_match_du_for_every_category() {
        let scratch = Scratch::new("du");
        let layout = layout_in(&scratch);
        populate(&scratch, &layout);

        let report = layout.report().unwrap();

        let store = layout.store.root();
        let (current, unkept) = (store.join("3170"), store.join("3056"));
        let assets = extracted_assets_dir(&layout.app_data);
        let art = art_code_file(&layout);
        let logs = crate::diagnostics::log_dir(&layout.app_data);
        let older = older_logs(&layout);
        let older: Vec<&Path> = older.iter().map(PathBuf::as_path).collect();
        let webview = &layout.webview;
        let expected = [
            du(&[&current]),
            du(&[&unkept]),
            du(&[store]) - du(&[&current]) - du(&[&unkept]),
            du(&[&layout.native_libs]),
            du(&[&assets]),
            du(&[&art]),
            du(&[&layout.client_cache]),
            du(&[&webview.cache]),
            du(&[&webview.data]),
            du(&[&webview.cef_profile]),
            du(&[cache_root(&layout)])
                - du(&[&layout.native_libs])
                - du(&[&layout.client_cache])
                - du(&[&webview.cache])
                - du(&[&art]),
            du(&[&layout.app_data])
                - du(&[&assets])
                - du(&[&logs])
                - du(&[&webview.data])
                - du(&[&webview.cef_profile]),
            du(&[&logs]) - du(&older),
            du(&older),
        ];
        let measured: Vec<u64> = report.entries.iter().map(|entry| entry.bytes).collect();
        assert_eq!(measured, expected);
        assert_eq!(
            report.total_bytes,
            du(&[store, &layout.app_data, cache_root(&layout)])
        );
        let roles: Vec<(VersionCode, VersionRole)> = report
            .entries
            .iter()
            .filter_map(|entry| match &entry.category {
                Category::RobloxVersion { version, role, .. } => {
                    Some((version.version_code, *role))
                }
                _ => None,
            })
            .collect();
        assert_eq!(
            roles,
            [
                (VersionCode(3170), VersionRole::Current),
                (VersionCode(3056), VersionRole::Unkept)
            ]
        );
        let large = large_app_data(&layout);
        assert_eq!(
            report.entries[11].category,
            Category::AppData {
                largest: vec![LargeEntry {
                    bytes: du(&[&large]),
                    path: large,
                }]
            }
        );
    }

    fn snapshot(root: &Path) -> BTreeMap<PathBuf, Vec<u8>> {
        let mut snapshot = BTreeMap::new();
        let mut pending = vec![root.to_path_buf()];
        while let Some(path) = pending.pop() {
            let file_type = fs::symlink_metadata(&path).unwrap().file_type();
            let content = if file_type.is_dir() {
                pending.extend(children(&path).unwrap());
                b"directory".to_vec()
            } else if file_type.is_symlink() {
                fs::read_link(&path)
                    .unwrap()
                    .into_os_string()
                    .into_encoded_bytes()
            } else {
                fs::read(&path).unwrap()
            };
            snapshot.insert(path, content);
        }
        snapshot
    }

    #[test]
    fn clean_removes_only_the_allowlisted_caches() {
        let scratch = Scratch::new("clean");
        let layout = layout_in(&scratch);
        populate(&scratch, &layout);
        let report_before = layout.report().unwrap();
        let before = snapshot(&scratch.0);

        let cleanup = clean(&layout, &StatusSink::terminal()).unwrap();

        let logs = crate::diagnostics::log_dir(&layout.app_data);
        let store = layout.store.root();
        let emptied = |dir: &Path, path: &Path| path.starts_with(dir) && path != dir;
        let removed = |path: &Path| {
            emptied(&layout.client_cache, path)
                || emptied(&layout.webview.cache, path)
                || (path.parent() == Some(logs.as_path())
                    && path
                        .file_name()
                        .unwrap()
                        .to_str()
                        .unwrap()
                        .starts_with(OLDER_RUN))
                || path.starts_with(store.join("3056"))
                || path.starts_with(store.join("incoming.partial"))
        };
        let kept: BTreeMap<PathBuf, Vec<u8>> = before
            .iter()
            .filter(|(path, _)| !removed(path))
            .map(|(path, content)| (path.clone(), content.clone()))
            .collect();
        assert_eq!(snapshot(&scratch.0), kept);
        for gone in [
            layout.client_cache.join("http/blob"),
            layout.client_cache.join("account"),
            layout.webview.cache.join("WebKitCache"),
            older_logs(&layout)[0].clone(),
            store.join("3056"),
            store.join("incoming.partial"),
        ] {
            assert!(before.contains_key(&gone), "{}", gone.display());
        }
        let mode = fs::metadata(&layout.client_cache)
            .unwrap()
            .permissions()
            .mode();
        assert_eq!(mode & 0o777, 0o700);
        assert_eq!(
            cleanup
                .freed
                .iter()
                .map(|freed| freed.cleaned)
                .collect::<Vec<_>>(),
            [
                Cleaned::ClientCache,
                Cleaned::WebviewCache,
                Cleaned::OlderLogs,
                Cleaned::UnkeptVersions
            ]
        );
        assert!(
            cleanup.freed.iter().all(|freed| freed.bytes > 0),
            "{cleanup}"
        );
        let freed: u64 = cleanup.freed.iter().map(|freed| freed.bytes).sum();
        assert_eq!(
            freed,
            report_before.total_bytes - layout.report().unwrap().total_bytes
        );
    }

    fn sorted_keys(object: &serde_json::Map<String, serde_json::Value>) -> Vec<&str> {
        let mut keys: Vec<&str> = object.keys().map(String::as_str).collect();
        keys.sort_unstable();
        keys
    }

    #[test]
    fn storage_json_keeps_its_schema() {
        let scratch = Scratch::new("json");
        let layout = layout_in(&scratch);
        populate(&scratch, &layout);

        let json: serde_json::Value =
            serde_json::from_str(&layout.report().unwrap().to_json().unwrap()).unwrap();

        assert_eq!(
            sorted_keys(json.as_object().unwrap()),
            ["entries", "schema", "total_bytes"]
        );
        assert_eq!(json["schema"], 1);
        assert!(json["total_bytes"].is_u64());
        let mut kinds = Vec::new();
        for entry in json["entries"].as_array().unwrap() {
            let kind = entry["kind"].as_str().unwrap();
            assert!(entry["bytes"].is_u64(), "{kind}");
            assert!(entry["cleanable"].is_boolean(), "{kind}");
            assert!(entry["path"].is_string(), "{kind}");
            let details: &[&str] = match kind {
                "roblox_version" => {
                    assert!(entry["version_code"].is_u64());
                    assert!(entry["version_name"].is_string() || entry["version_name"].is_null());
                    assert!(["current", "fallback", "failed", "unkept"]
                        .contains(&entry["role"].as_str().unwrap()));
                    assert!(["unproven", "proven", "played", "both_failed"]
                        .contains(&entry["state"].as_str().unwrap()));
                    assert!(entry["failed_starts"].is_u64());
                    &[
                        "failed_starts",
                        "role",
                        "state",
                        "version_code",
                        "version_name",
                    ]
                }
                "client_cache" => {
                    assert_eq!(entry["cap_bytes"], CLIENT_CACHE_CAP);
                    &["cap_bytes"]
                }
                "app_data" => {
                    for part in entry["largest"].as_array().unwrap() {
                        assert_eq!(sorted_keys(part.as_object().unwrap()), ["bytes", "path"]);
                        assert!(part["bytes"].is_u64() && part["path"].is_string());
                    }
                    &["largest"]
                }
                _ => &[],
            };
            let mut expected: Vec<&str> = ["bytes", "cleanable", "kind", "path"]
                .into_iter()
                .chain(details.iter().copied())
                .collect();
            expected.sort_unstable();
            assert_eq!(sorted_keys(entry.as_object().unwrap()), expected, "{kind}");
            kinds.push(kind.to_owned());
        }
        assert_eq!(
            kinds,
            [
                "roblox_version",
                "roblox_version",
                "store_records",
                "native_libs",
                "assets",
                "art_code",
                "client_cache",
                "webview_cache",
                "webview_data",
                "cef_profile",
                "other_cache",
                "app_data",
                "log",
                "older_logs"
            ]
        );
    }

    #[test]
    fn the_table_aligns_mebibytes_and_marks_what_clean_empties() {
        let report = Report::new(vec![
            Entry {
                category: Category::RobloxVersion {
                    version: InstalledVersion {
                        version_code: VersionCode(3170),
                        version_name: Some("2.740.931".to_owned()),
                    },
                    role: VersionRole::Current,
                    starts: Starts(VersionState::Played),
                },
                bytes: 152_199_168,
                path: PathBuf::from("/v/app/data/eclipse/roblox/3170"),
            },
            Entry {
                category: Category::RobloxVersion {
                    version: InstalledVersion {
                        version_code: VersionCode(3100),
                        version_name: None,
                    },
                    role: VersionRole::Unkept,
                    starts: Starts(VersionState::Proven),
                },
                bytes: 1_048_576,
                path: PathBuf::from("/v/app/data/eclipse/roblox/3100"),
            },
            Entry {
                category: Category::ClientCache {
                    cap_bytes: CLIENT_CACHE_CAP,
                },
                bytes: 60_188_672,
                path: PathBuf::from("/v/app/cache/eclipse/client-cache"),
            },
            Entry {
                category: Category::AppData {
                    largest: vec![LargeEntry {
                        path: PathBuf::from("/v/app/data/eclipse/app-data/files/appData"),
                        bytes: 61_656_064,
                    }],
                },
                bytes: 241_172_480,
                path: PathBuf::from("/v/app/data/eclipse/app-data"),
            },
        ]);

        assert_eq!(
            report.to_string(),
            "Files under /v/app/
      MiB  What                                                    Where
    145.1  Roblox 2.740.931, current, has been played              data/eclipse/roblox/3170
      1.0  Roblox versionCode 3100, no longer kept, has started *  data/eclipse/roblox/3100
     57.4  Roblox's cache, trimmed above 512 MiB *                 cache/eclipse/client-cache
    230.0  Roblox app data                                         data/eclipse/app-data
     58.8    files/appData
    433.5  Total
* emptied by `eclipse storage --clean`
"
        );
    }

    #[test]
    fn paths_are_shown_under_the_deepest_directory_they_share() {
        let shared = |paths: &[&str]| shared_parent(paths.iter().map(Path::new));
        assert_eq!(
            shared(&[
                "/v/app/data/roblox",
                "/v/app/data/roblox/3170",
                "/v/app/cache"
            ]),
            Some(PathBuf::from("/v/app"))
        );
        assert_eq!(
            shared(&["/v/app/data/roblox/3170"]),
            Some(PathBuf::from("/v/app/data/roblox"))
        );
        assert_eq!(shared(&["/d/roblox/3170", "/c/client-cache"]), None);
        assert_eq!(shared(&["/v/app", "/v/cache"]), Some(PathBuf::from("/v")));
        assert_eq!(shared(&["/v", "/w/cache"]), None);
        assert_eq!(shared(&[]), None);
    }
}
