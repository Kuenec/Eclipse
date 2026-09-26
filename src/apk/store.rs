use std::fmt;
use std::fs::{self, File, OpenOptions};
use std::io::{self, BufReader, Read, Write};
use std::path::{Path, PathBuf};
use std::time::{Duration, SystemTime, UNIX_EPOCH};

use ring::digest::SHA1_OUTPUT_LEN;
use serde::{Deserialize, Serialize};
use zip::result::ZipError;
use zip::ZipArchive;

use super::{
    Apk, ApkError, ApkSet, ApkSetError, ApkSetPaths, VersionCode, BASE_APK, MAX_APK_BYTES,
    NATIVE_SPLIT_APK, NATIVE_SPLIT_NAME, ROBLOX_PACKAGE,
};

const STORE_DIR: &str = "roblox";
const CURRENT_FILE: &str = "current.json";
const LAST_CHECK_FILE: &str = "last-update-check.json";
const TEMP_SUFFIX: &str = ".tmp";
const LOCK_FILE: &str = "install.lock";
const STAGING_DIR: &str = "incoming.partial";
const PARTIAL_SUFFIX: &str = ".partial";
const BUNDLE_EXTENSIONS: [&str; 3] = ["apks", "xapk", "apkm"];
const XAPK_NATIVE_SPLIT: &str = "config.x86_64.apk";

const UPDATE_INTERVAL: Duration = Duration::from_secs(6 * 60 * 60);

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct InstalledVersion {
    pub version_code: VersionCode,

    pub version_name: Option<String>,
}

impl From<&ApkSet> for InstalledVersion {
    fn from(set: &ApkSet) -> Self {
        Self {
            version_code: set.version_code(),
            version_name: set.version_name().map(str::to_owned),
        }
    }
}

impl fmt::Display for InstalledVersion {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match &self.version_name {
            Some(name) => write!(f, "{name} (versionCode {})", self.version_code),
            None => write!(f, "versionCode {}", self.version_code),
        }
    }
}

pub enum UpdateOutcome {
    UpToDate {
        installed: InstalledVersion,
    },
    Updated {
        previous: Option<InstalledVersion>,
        set: Box<ApkSet>,
    },
}

pub fn update_due(
    installed: Option<VersionCode>,
    last_check: Option<SystemTime>,
    now: SystemTime,
) -> bool {
    installed.is_none()
        || last_check.is_none_or(|last| match now.duration_since(last) {
            Ok(elapsed) => elapsed >= UPDATE_INTERVAL,
            Err(_) => true,
        })
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub struct Release {
    pub version_code: VersionCode,

    pub base_sha1: [u8; SHA1_OUTPUT_LEN],
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct UpdateCheck {
    pub at: SystemTime,

    pub rejected: Option<Release>,
}

#[derive(Serialize, Deserialize)]
struct LastCheck {
    checked_at_unix: u64,

    rejected: Option<Release>,
}

pub struct Store {
    root: PathBuf,
}

impl Store {
    pub fn open() -> Result<Self, StoreError> {
        let dirs =
            directories::ProjectDirs::from("", "", "eclipse").ok_or(StoreError::NoDataDir)?;
        Ok(Self::at(dirs.data_dir().join(STORE_DIR)))
    }

    pub fn at(root: PathBuf) -> Self {
        Self { root }
    }

    pub fn current(&self) -> Result<Option<InstalledVersion>, StoreError> {
        let path = self.root.join(CURRENT_FILE);
        let bytes = match fs::read(&path) {
            Ok(bytes) => bytes,
            Err(error) if error.kind() == io::ErrorKind::NotFound => return Ok(None),
            Err(source) => return Err(StoreError::Io { path, source }),
        };
        serde_json::from_slice(&bytes)
            .map(Some)
            .map_err(|source| StoreError::Corrupt { path, source })
    }

    pub fn current_set(&self) -> Result<Option<(InstalledVersion, ApkSetPaths)>, StoreError> {
        let Some(current) = self.current()? else {
            return Ok(None);
        };
        let dir = self.version_dir(current.version_code);
        match ApkSetPaths::locate(&dir) {
            Ok(paths) => Ok(Some((current, paths))),
            Err(ApkSetError::MissingBase(_)) => Err(StoreError::MissingInstall {
                installed: current,
                dir,
            }),
            Err(ApkSetError::Locate { source, .. }) if source.kind() == io::ErrorKind::NotFound => {
                Err(StoreError::MissingInstall {
                    installed: current,
                    dir,
                })
            }
            Err(error) => Err(error.into()),
        }
    }

    pub fn verified_current(&self) -> Result<Option<ApkSet>, StoreError> {
        match self.current_set()? {
            Some((_, paths)) => Ok(Some(ApkSet::open(paths)?)),
            None => Ok(None),
        }
    }

    pub fn usable_current(&self) -> Result<Option<ApkSet>, StoreError> {
        match self.verified_current() {
            Err(error) if error.is_unusable_install() => Ok(None),
            current => current,
        }
    }

    pub fn last_check(&self) -> Result<Option<UpdateCheck>, StoreError> {
        let path = self.root.join(LAST_CHECK_FILE);
        let bytes = match fs::read(&path) {
            Ok(bytes) => bytes,
            Err(error) if error.kind() == io::ErrorKind::NotFound => return Ok(None),
            Err(source) => return Err(StoreError::Io { path, source }),
        };
        let Ok(record) = serde_json::from_slice::<LastCheck>(&bytes) else {
            return Ok(None);
        };
        Ok(UNIX_EPOCH
            .checked_add(Duration::from_secs(record.checked_at_unix))
            .map(|at| UpdateCheck {
                at,
                rejected: record.rejected,
            }))
    }

    pub fn record_check(&self, check: &UpdateCheck) -> Result<(), StoreError> {
        let checked_at_unix = check
            .at
            .duration_since(UNIX_EPOCH)
            .map_or(0, |since| since.as_secs());
        self.replace_json(
            LAST_CHECK_FILE,
            &LastCheck {
                checked_at_unix,
                rejected: check.rejected,
            },
        )
    }

    pub fn install(&self, sources: &[PathBuf]) -> Result<InstalledVersion, StoreError> {
        let staging = self.begin()?;
        for source in sources {
            staging.add_source(source)?;
        }
        staging.commit(None).map(|set| InstalledVersion::from(&set))
    }

    pub fn begin(&self) -> Result<Staging<'_>, StoreError> {
        create_dir_all(&self.root)?;
        let lock_path = self.root.join(LOCK_FILE);
        let lock = OpenOptions::new()
            .create(true)
            .truncate(false)
            .write(true)
            .open(&lock_path)
            .map_err(|source| StoreError::Io {
                path: lock_path.clone(),
                source,
            })?;
        lock.lock().map_err(|source| StoreError::Io {
            path: lock_path,
            source,
        })?;
        let dir = self.root.join(STAGING_DIR);
        remove_dir_if_present(&dir)?;
        create_dir_all(&dir)?;
        Ok(Staging {
            store: self,
            dir,
            _lock: lock,
        })
    }

    fn version_dir(&self, version: VersionCode) -> PathBuf {
        self.root.join(version.to_string())
    }

    fn activate(&self, installed: &InstalledVersion) -> Result<(), StoreError> {
        let previous = match self.current() {
            Ok(previous) => previous,
            Err(StoreError::Corrupt { .. }) => None,
            Err(error) => return Err(error),
        };

        self.replace_json(CURRENT_FILE, installed)?;
        match previous {
            Some(previous) if previous.version_code == installed.version_code => Ok(()),
            previous => self.prune(installed, previous.map(|p| p.version_code)),
        }
    }

    fn replace_json(&self, name: &str, record: &impl Serialize) -> Result<(), StoreError> {
        let temp = self.root.join(format!("{name}{TEMP_SUFFIX}"));
        let mut json = serde_json::to_vec_pretty(record).expect("store records always serialize");
        json.push(b'\n');
        write_synced(&temp, &json)?;
        let path = self.root.join(name);
        fs::rename(&temp, &path).map_err(|source| StoreError::Io { path, source })?;
        sync_dir(&self.root)
    }

    fn prune(
        &self,
        installed: &InstalledVersion,
        previous: Option<VersionCode>,
    ) -> Result<(), StoreError> {
        let entries = fs::read_dir(&self.root).map_err(|source| StoreError::Io {
            path: self.root.clone(),
            source,
        })?;
        for entry in entries {
            let entry = entry.map_err(|source| StoreError::Io {
                path: self.root.clone(),
                source,
            })?;
            let file_type = entry.file_type().map_err(|source| StoreError::Io {
                path: entry.path(),
                source,
            })?;
            if !file_type.is_dir() {
                continue;
            }
            let name = entry.file_name();
            let Some(name) = name.to_str() else {
                continue;
            };
            let stale = name.ends_with(PARTIAL_SUFFIX)
                || name.parse::<u32>().is_ok_and(|code| {
                    let code = VersionCode(code);
                    code != installed.version_code && Some(code) != previous
                });
            if !stale {
                continue;
            }
            let path = entry.path();
            fs::remove_dir_all(&path).map_err(|source| StoreError::Prune {
                installed: installed.clone(),
                path,
                source,
            })?;
        }
        Ok(())
    }
}

pub struct Staging<'a> {
    store: &'a Store,
    dir: PathBuf,
    _lock: File,
}

impl Staging<'_> {
    pub fn dir(&self) -> &Path {
        &self.dir
    }

    pub fn add_source(&self, source: &Path) -> Result<(), StoreError> {
        let metadata = fs::metadata(source).map_err(|error| StoreError::Io {
            path: source.to_path_buf(),
            source: error,
        })?;
        if metadata.is_dir() {
            let paths = ApkSetPaths::locate(source)?;
            self.copy_in(&paths.base, StagedFile::Base)?;
            if let Some(split) = &paths.native_split {
                self.copy_in(split, StagedFile::NativeSplit)?;
            }
            return Ok(());
        }
        if is_bundle(source) {
            return self.extract_bundle(source);
        }
        let role = classify_apk(source)?;
        self.copy_in(source, role)
    }

    pub fn commit(mut self, expected: Option<VersionCode>) -> Result<ApkSet, StoreError> {
        if !path_exists(&self.dir.join(BASE_APK))? {
            return Err(StoreError::NoBaseGiven);
        }
        if !path_exists(&self.dir.join(NATIVE_SPLIT_APK))? && !staged_base_has_engine(&self.dir)? {
            return Err(StoreError::NoNativeSplitGiven);
        }
        let partial = self.store.root.join(format!(
            "{}{PARTIAL_SUFFIX}",
            staged_version_code(&self.dir)?
        ));
        remove_dir_if_present(&partial)?;
        rename(&self.dir, &partial)?;
        self.dir = partial;

        let paths = ApkSetPaths::locate(&self.dir)?;
        let staged = ApkSet::open(paths)?;
        let installed = InstalledVersion::from(&staged);
        if let Some(expected) = expected {
            if expected != installed.version_code {
                return Err(StoreError::UnexpectedVersion {
                    expected,
                    found: installed.version_code,
                });
            }
        }

        let target = self.store.version_dir(installed.version_code);
        let existing = if path_exists(&target)? {
            ApkSetPaths::locate(&target).and_then(ApkSet::open).ok()
        } else {
            None
        };
        let set = match existing {
            Some(existing) => {
                drop(staged);
                remove_dir_if_present(&self.dir)?;
                existing
            }
            None => {
                remove_dir_if_present(&target)?;
                rename(&self.dir, &target)?;
                staged.relocated(ApkSetPaths::locate(&target)?)?
            }
        };
        sync_dir(&self.store.root)?;
        self.store.activate(&installed)?;
        Ok(set)
    }

    fn copy_in(&self, source: &Path, role: StagedFile) -> Result<(), StoreError> {
        let mut input = File::open(source).map_err(|error| StoreError::Io {
            path: source.to_path_buf(),
            source: error,
        })?;
        self.write_staged(role, source, &mut input)
    }

    fn write_staged(
        &self,
        role: StagedFile,
        origin: &Path,
        input: &mut impl Read,
    ) -> Result<(), StoreError> {
        let dest = self.dir.join(role.file_name());
        let mut output = match OpenOptions::new().write(true).create_new(true).open(&dest) {
            Ok(file) => file,
            Err(error) if error.kind() == io::ErrorKind::AlreadyExists => {
                return Err(StoreError::Duplicate {
                    path: origin.to_path_buf(),
                    file: role.file_name(),
                });
            }
            Err(source) => return Err(StoreError::Io { path: dest, source }),
        };
        let io_error = |source| StoreError::Io {
            path: origin.to_path_buf(),
            source,
        };
        let copied = io::copy(&mut input.take(MAX_APK_BYTES + 1), &mut output).map_err(io_error)?;
        if copied > MAX_APK_BYTES {
            return Err(StoreError::TooLarge {
                path: origin.to_path_buf(),
            });
        }
        output
            .sync_all()
            .map_err(|source| StoreError::Io { path: dest, source })
    }

    fn extract_bundle(&self, bundle: &Path) -> Result<(), StoreError> {
        let file = File::open(bundle).map_err(|source| StoreError::Io {
            path: bundle.to_path_buf(),
            source,
        })?;
        let mut archive = ZipArchive::new(BufReader::new(file)).map_err(|source| {
            StoreError::UnreadableBundle {
                path: bundle.to_path_buf(),
                source,
            }
        })?;
        let members: Vec<(String, StagedFile)> = archive
            .file_names()
            .filter_map(|name| bundle_member_role(name).map(|role| (name.to_owned(), role)))
            .collect();
        if !members.iter().any(|(_, role)| *role == StagedFile::Base) {
            return Err(StoreError::BundleMissingBase(bundle.to_path_buf()));
        }
        for (name, role) in &members {
            let mut entry = archive.by_name(name).map_err(|source| match source {
                ZipError::UnsupportedArchive(message) if message == ZipError::PASSWORD_REQUIRED => {
                    StoreError::EncryptedBundle(bundle.to_path_buf())
                }
                source => StoreError::UnreadableBundle {
                    path: bundle.to_path_buf(),
                    source,
                },
            })?;
            self.write_staged(*role, bundle, &mut entry)?;
        }
        Ok(())
    }
}

impl Drop for Staging<'_> {
    fn drop(&mut self) {
        let _ = fs::remove_dir_all(&self.dir);
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum StagedFile {
    Base,
    NativeSplit,
}

impl StagedFile {
    fn file_name(self) -> &'static str {
        match self {
            Self::Base => BASE_APK,
            Self::NativeSplit => NATIVE_SPLIT_APK,
        }
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

fn bundle_member_role(name: &str) -> Option<StagedFile> {
    let file_name = name.rsplit('/').next().unwrap_or(name);
    if file_name == BASE_APK || file_name == format!("{ROBLOX_PACKAGE}.apk") {
        Some(StagedFile::Base)
    } else if file_name == NATIVE_SPLIT_APK || file_name == XAPK_NATIVE_SPLIT {
        Some(StagedFile::NativeSplit)
    } else {
        None
    }
}

fn classify_apk(path: &Path) -> Result<StagedFile, StoreError> {
    let apk_error = |source| StoreError::Apk {
        path: path.to_path_buf(),
        source,
    };
    let info = Apk::open(path)
        .map_err(apk_error)?
        .package_info()
        .map_err(apk_error)?;
    match info.split.as_deref() {
        None => Ok(StagedFile::Base),
        Some(NATIVE_SPLIT_NAME) => Ok(StagedFile::NativeSplit),
        Some(split) => Err(StoreError::UnneededSplit {
            path: path.to_path_buf(),
            split: split.to_owned(),
        }),
    }
}

fn staged_version_code(dir: &Path) -> Result<VersionCode, StoreError> {
    let path = dir.join(BASE_APK);
    let apk_error = |source| StoreError::Apk {
        path: path.clone(),
        source,
    };
    Apk::open(&path)
        .map_err(apk_error)?
        .package_info()
        .map_err(apk_error)?
        .version_code
        .ok_or_else(|| ApkSetError::MissingVersionCode(path.clone()).into())
}

fn staged_base_has_engine(dir: &Path) -> Result<bool, StoreError> {
    let path = dir.join(BASE_APK);
    let apk_error = |source| StoreError::Apk {
        path: path.clone(),
        source,
    };
    match Apk::open(&path).map_err(apk_error)?.x86_64_engine() {
        Ok(_) => Ok(true),
        Err(ApkError::EngineMissing) => Ok(false),
        Err(source) => Err(apk_error(source)),
    }
}

fn path_exists(path: &Path) -> Result<bool, StoreError> {
    match fs::symlink_metadata(path) {
        Ok(_) => Ok(true),
        Err(error) if error.kind() == io::ErrorKind::NotFound => Ok(false),
        Err(source) => Err(StoreError::Io {
            path: path.to_path_buf(),
            source,
        }),
    }
}

fn create_dir_all(path: &Path) -> Result<(), StoreError> {
    fs::create_dir_all(path).map_err(|source| StoreError::Io {
        path: path.to_path_buf(),
        source,
    })
}

fn remove_dir_if_present(path: &Path) -> Result<(), StoreError> {
    match fs::remove_dir_all(path) {
        Ok(()) => Ok(()),
        Err(error) if error.kind() == io::ErrorKind::NotFound => Ok(()),
        Err(source) => Err(StoreError::Io {
            path: path.to_path_buf(),
            source,
        }),
    }
}

fn rename(from: &Path, to: &Path) -> Result<(), StoreError> {
    fs::rename(from, to).map_err(|source| StoreError::Io {
        path: to.to_path_buf(),
        source,
    })
}

fn write_synced(path: &Path, bytes: &[u8]) -> Result<(), StoreError> {
    let io_error = |source| StoreError::Io {
        path: path.to_path_buf(),
        source,
    };
    let mut file = File::create(path).map_err(io_error)?;
    file.write_all(bytes).map_err(io_error)?;
    file.sync_all().map_err(io_error)
}

fn sync_dir(path: &Path) -> Result<(), StoreError> {
    File::open(path)
        .and_then(|dir| dir.sync_all())
        .map_err(|source| StoreError::Io {
            path: path.to_path_buf(),
            source,
        })
}

#[derive(Debug)]
pub enum StoreError {
    NoDataDir,

    Io {
        path: PathBuf,
        source: io::Error,
    },

    Corrupt {
        path: PathBuf,
        source: serde_json::Error,
    },

    Set(ApkSetError),

    MissingInstall {
        installed: InstalledVersion,
        dir: PathBuf,
    },

    Apk {
        path: PathBuf,
        source: ApkError,
    },

    UnneededSplit {
        path: PathBuf,
        split: String,
    },

    Duplicate {
        path: PathBuf,
        file: &'static str,
    },

    NoBaseGiven,

    NoNativeSplitGiven,

    TooLarge {
        path: PathBuf,
    },

    UnreadableBundle {
        path: PathBuf,
        source: ZipError,
    },

    EncryptedBundle(PathBuf),

    BundleMissingBase(PathBuf),

    UnexpectedVersion {
        expected: VersionCode,
        found: VersionCode,
    },

    Prune {
        installed: InstalledVersion,
        path: PathBuf,
        source: io::Error,
    },
}

impl StoreError {
    pub fn is_unusable_install(&self) -> bool {
        matches!(
            self,
            Self::Corrupt { .. } | Self::MissingInstall { .. } | Self::Set(_)
        )
    }
}

impl fmt::Display for StoreError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::NoDataDir => {
                f.write_str("cannot determine Eclipse's data directory; set HOME or XDG_DATA_HOME")
            }
            Self::Io { path, source } => write!(f, "{}: {source}", path.display()),
            Self::Corrupt { path, source } => write!(
                f,
                "{} is not a valid Eclipse install record ({source}); reinstall with `eclipse \
                 install` or `eclipse update`",
                path.display()
            ),
            Self::Set(error) => error.fmt(f),
            Self::MissingInstall { installed, dir } => write!(
                f,
                "the installed Roblox {installed} is missing from {}; reinstall it with \
                 `eclipse update` or `eclipse install`",
                dir.display()
            ),
            Self::Apk { path, source } => write!(f, "{}: {source}", path.display()),
            Self::UnneededSplit { path, split } => write!(
                f,
                "{} is the split {split}, which Eclipse does not use; pass only {BASE_APK} and \
                 {NATIVE_SPLIT_APK}",
                path.display()
            ),
            Self::Duplicate { path, file } => write!(
                f,
                "{} is a second {file}; pass exactly one {BASE_APK} and at most one \
                 {NATIVE_SPLIT_APK}",
                path.display()
            ),
            Self::NoBaseGiven => write!(
                f,
                "none of the given files is a Roblox base APK; pass {BASE_APK} together with \
                 {NATIVE_SPLIT_APK}"
            ),
            Self::NoNativeSplitGiven => write!(
                f,
                "none of the given files is {NATIVE_SPLIT_APK} and {BASE_APK} has no x86_64 \
                 code; pass {NATIVE_SPLIT_APK} too (a bundle without it has no x86_64 build of \
                 Roblox)"
            ),
            Self::TooLarge { path } => write!(
                f,
                "{} holds an APK larger than {MAX_APK_BYTES} bytes",
                path.display()
            ),
            Self::UnreadableBundle { path, source } => write!(
                f,
                "cannot read {} as a zip bundle ({source}); encrypted bundles such as newer \
                 .apkm files are not supported, so pass the plain {BASE_APK} and \
                 {NATIVE_SPLIT_APK} instead",
                path.display()
            ),
            Self::EncryptedBundle(path) => write!(
                f,
                "{} is encrypted; pass the plain {BASE_APK} and {NATIVE_SPLIT_APK} instead",
                path.display()
            ),
            Self::BundleMissingBase(path) => {
                write!(f, "{} contains no {BASE_APK}", path.display())
            }
            Self::UnexpectedVersion { expected, found } => write!(
                f,
                "expected Roblox versionCode {expected} but received {found}"
            ),
            Self::Prune {
                installed,
                path,
                source,
            } => write!(
                f,
                "installed Roblox {installed}, but could not remove the old install {}: {source}",
                path.display()
            ),
        }
    }
}

impl std::error::Error for StoreError {
    fn source(&self) -> Option<&(dyn std::error::Error + 'static)> {
        match self {
            Self::Io { source, .. } | Self::Prune { source, .. } => Some(source),
            Self::Corrupt { source, .. } => Some(source),
            Self::Set(error) => Some(error),
            Self::Apk { source, .. } => Some(source),
            Self::UnreadableBundle { source, .. } => Some(source),
            Self::NoDataDir
            | Self::MissingInstall { .. }
            | Self::UnneededSplit { .. }
            | Self::Duplicate { .. }
            | Self::NoBaseGiven
            | Self::NoNativeSplitGiven
            | Self::TooLarge { .. }
            | Self::EncryptedBundle(_)
            | Self::BundleMissingBase(_)
            | Self::UnexpectedVersion { .. } => None,
        }
    }
}

impl From<ApkSetError> for StoreError {
    fn from(error: ApkSetError) -> Self {
        Self::Set(error)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::apk::axml;
    use crate::apk::signature::SignatureError;
    use zip::unstable::write::FileOptionsExt as _;
    use zip::write::SimpleFileOptions;
    use zip::{CompressionMethod, ZipWriter};

    type ErrorCheck = fn(&StoreError) -> bool;

    fn temp_root(tag: &str) -> PathBuf {
        let root = std::env::temp_dir().join(format!(
            "eclipse-store-test-{tag}-{:?}",
            std::thread::current().id()
        ));
        fs::remove_dir_all(&root).ok();
        fs::create_dir_all(&root).expect("create temp store root");
        root
    }

    fn zip_of(entries: &[(&str, &[u8])], options: SimpleFileOptions) -> Vec<u8> {
        let mut writer = ZipWriter::new(io::Cursor::new(Vec::new()));
        for (name, bytes) in entries {
            writer.start_file(*name, options).expect("start entry");
            writer.write_all(bytes).expect("write entry");
        }
        writer.finish().expect("finish zip").into_inner()
    }

    fn stored() -> SimpleFileOptions {
        SimpleFileOptions::default().compression_method(CompressionMethod::Stored)
    }

    fn synthetic_apk(split: Option<&str>, engine: bool) -> Vec<u8> {
        let manifest = axml::fixture::Manifest {
            package: ROBLOX_PACKAGE,
            version_code: Some(3056),
            version_name: None,
            split,
            launcher: split.is_none().then_some(".Main"),
        }
        .encode();
        let mut entries: Vec<(&str, &[u8])> = vec![("AndroidManifest.xml", &manifest)];
        if engine {
            entries.push(("lib/x86_64/libroblox.so", b"engine"));
        }
        zip_of(&entries, stored())
    }

    fn version(code: u32) -> InstalledVersion {
        InstalledVersion {
            version_code: VersionCode(code),
            version_name: Some(format!("2.{code}.0")),
        }
    }

    fn entries(root: &Path) -> Vec<String> {
        let mut names: Vec<String> = fs::read_dir(root)
            .expect("list store root")
            .map(|entry| {
                entry
                    .expect("entry")
                    .file_name()
                    .into_string()
                    .expect("utf-8")
            })
            .filter(|name| name != LOCK_FILE)
            .collect();
        names.sort();
        names
    }

    #[test]
    fn activation_switches_current_keeps_the_previous_version_and_prunes_the_rest() {
        let root = temp_root("activate");
        let store = Store::at(root.clone());
        for dir in ["3054", "3055", "3056", "3057.partial", "notes"] {
            fs::create_dir_all(root.join(dir)).unwrap();
        }
        fs::write(root.join("3053"), b"a file, not an install").unwrap();

        store.activate(&version(3055)).unwrap();
        assert_eq!(store.current().unwrap(), Some(version(3055)));
        assert_eq!(entries(&root), ["3053", "3055", CURRENT_FILE, "notes"]);

        fs::create_dir_all(root.join("3056")).unwrap();
        store.activate(&version(3056)).unwrap();
        assert_eq!(
            entries(&root),
            ["3053", "3055", "3056", CURRENT_FILE, "notes"]
        );

        fs::create_dir_all(root.join("3057")).unwrap();
        store.activate(&version(3057)).unwrap();
        assert_eq!(
            entries(&root),
            ["3053", "3056", "3057", CURRENT_FILE, "notes"]
        );

        fs::create_dir_all(root.join("3040")).unwrap();
        store.activate(&version(3057)).unwrap();
        assert!(
            root.join("3040").exists(),
            "re-activating the current version must not prune"
        );
        assert_eq!(store.current().unwrap(), Some(version(3057)));
        fs::remove_dir_all(&root).ok();
    }

    #[test]
    fn install_record_is_absent_readable_or_a_typed_corruption_error() {
        let root = temp_root("current");
        let store = Store::at(root.clone());
        assert_eq!(store.current().unwrap(), None);
        assert!(store.current_set().unwrap().is_none());

        fs::write(root.join(CURRENT_FILE), b"{\"version_code\": 3056}").unwrap();
        assert_eq!(
            store.current().unwrap(),
            Some(InstalledVersion {
                version_code: VersionCode(3056),
                version_name: None
            })
        );

        let err = store.current_set().unwrap_err();
        assert!(
            matches!(err, StoreError::MissingInstall { .. }),
            "a record whose install directory is gone: {err:?}"
        );
        fs::create_dir_all(root.join("3056")).unwrap();
        let err = store.current_set().unwrap_err();
        assert!(
            matches!(err, StoreError::MissingInstall { .. }),
            "an install directory without base.apk: {err:?}"
        );
        assert!(err.to_string().contains("eclipse update"), "{err}");

        fs::write(root.join(CURRENT_FILE), b"not json").unwrap();
        let err = store.current().unwrap_err();
        assert!(matches!(err, StoreError::Corrupt { .. }), "{err:?}");
        assert!(err.to_string().contains("eclipse install"), "{err}");
        fs::remove_dir_all(&root).ok();
    }

    #[test]
    fn update_checks_are_due_every_six_hours_once_roblox_is_installed() {
        let now = UNIX_EPOCH + Duration::from_secs(1_800_000_000);
        let installed = Some(VersionCode(3170));
        assert!(update_due(installed, None, now));
        assert!(!update_due(
            installed,
            Some(now - Duration::from_secs(60)),
            now
        ));
        assert!(!update_due(
            installed,
            Some(now - UPDATE_INTERVAL + Duration::from_secs(1)),
            now
        ));
        assert!(update_due(installed, Some(now - UPDATE_INTERVAL), now));
        assert!(
            update_due(installed, Some(now + Duration::from_secs(60)), now),
            "a check recorded in the future is treated as stale"
        );
        assert!(
            update_due(None, Some(now - Duration::from_secs(60)), now),
            "without a usable install Roblox is downloaded despite a recent check"
        );

        let root = temp_root("last-check");
        let store = Store::at(root.clone());
        assert_eq!(store.last_check().unwrap(), None);
        let checked = UpdateCheck {
            at: now,
            rejected: None,
        };
        store.record_check(&checked).unwrap();
        assert_eq!(store.last_check().unwrap(), Some(checked));
        assert_eq!(entries(&root), [LAST_CHECK_FILE]);

        let rejected = UpdateCheck {
            at: now,
            rejected: Some(Release {
                version_code: VersionCode(3170),
                base_sha1: [0x40; SHA1_OUTPUT_LEN],
            }),
        };
        store.record_check(&rejected).unwrap();
        assert_eq!(store.last_check().unwrap(), Some(rejected));
        fs::write(
            root.join(LAST_CHECK_FILE),
            b"{\"checked_at_unix\": 1800000000}",
        )
        .unwrap();
        assert_eq!(
            store.last_check().unwrap(),
            Some(checked),
            "a record without a rejected release rejects nothing"
        );

        let record = root.join(LAST_CHECK_FILE);
        for damaged in [&b"{"[..], b"{\"checked_at_unix\": 18446744073709551615}"] {
            fs::write(&record, damaged).unwrap();
            assert_eq!(
                store.last_check().unwrap(),
                None,
                "a damaged last-check record makes the update due"
            );
        }
        store.record_check(&checked).unwrap();
        assert_eq!(store.last_check().unwrap(), Some(checked));

        fs::remove_file(&record).unwrap();
        fs::create_dir(&record).unwrap();
        let err = store.last_check().unwrap_err();
        assert!(
            matches!(err, StoreError::Io { .. }),
            "an unreadable last-check record is an error: {err:?}"
        );
        fs::remove_dir_all(&root).ok();
    }

    fn usable_version(store: &Store) -> Result<Option<InstalledVersion>, StoreError> {
        store
            .usable_current()
            .map(|set| set.as_ref().map(InstalledVersion::from))
    }

    #[test]
    fn only_an_install_that_still_verifies_is_usable() {
        let root = temp_root("usable");
        let store = Store::at(root.clone());
        assert_eq!(usable_version(&store).unwrap(), None, "nothing installed");

        fs::write(root.join(CURRENT_FILE), b"not json").unwrap();
        assert_eq!(
            usable_version(&store).unwrap(),
            None,
            "a corrupt install record"
        );

        fs::write(
            root.join(CURRENT_FILE),
            serde_json::to_vec(&version(3056)).unwrap(),
        )
        .unwrap();
        assert_eq!(
            usable_version(&store).unwrap(),
            None,
            "a record whose install directory is gone"
        );

        let dir = root.join("3056");
        fs::create_dir_all(&dir).unwrap();
        fs::write(dir.join(BASE_APK), synthetic_apk(None, false)).unwrap();
        fs::write(
            dir.join(NATIVE_SPLIT_APK),
            synthetic_apk(Some(NATIVE_SPLIT_NAME), true),
        )
        .unwrap();
        assert_eq!(
            usable_version(&store).unwrap(),
            None,
            "an install that fails the Roblox signature check"
        );
        assert!(
            matches!(
                store.verified_current(),
                Err(StoreError::Set(ApkSetError::Signature {
                    source: SignatureError::MissingV2Signature,
                    ..
                }))
            ),
            "launching reports why the install failed verification"
        );

        fs::remove_file(root.join(CURRENT_FILE)).unwrap();
        fs::create_dir(root.join(CURRENT_FILE)).unwrap();
        let err = usable_version(&store).unwrap_err();
        assert!(
            matches!(err, StoreError::Io { .. }),
            "an unreadable install record is an error: {err:?}"
        );
        fs::remove_dir_all(&root).ok();
    }

    #[test]
    fn loose_apks_are_staged_by_their_manifest_role() {
        let root = temp_root("classify");
        let sources = temp_root("classify-sources");
        let base = synthetic_apk(None, false);
        let split = synthetic_apk(Some(NATIVE_SPLIT_NAME), true);
        fs::write(sources.join("roblox-base.apk"), &base).unwrap();
        fs::write(sources.join("roblox-x86.apk"), &split).unwrap();
        fs::write(
            sources.join("density.apk"),
            synthetic_apk(Some("config.xxhdpi"), false),
        )
        .unwrap();

        let store = Store::at(root.clone());
        let staging = store.begin().unwrap();
        staging.add_source(&sources.join("roblox-x86.apk")).unwrap();
        staging
            .add_source(&sources.join("roblox-base.apk"))
            .unwrap();
        assert_eq!(fs::read(staging.dir().join(BASE_APK)).unwrap(), base);
        assert_eq!(
            fs::read(staging.dir().join(NATIVE_SPLIT_APK)).unwrap(),
            split
        );

        let err = staging
            .add_source(&sources.join("roblox-base.apk"))
            .unwrap_err();
        assert!(matches!(err, StoreError::Duplicate { .. }), "{err:?}");
        let err = staging
            .add_source(&sources.join("density.apk"))
            .unwrap_err();
        assert!(matches!(err, StoreError::UnneededSplit { .. }), "{err:?}");
        drop(staging);
        assert!(
            !root.join(STAGING_DIR).exists(),
            "an abandoned staging directory is removed"
        );

        let staging = store.begin().unwrap();
        staging.add_source(&sources.join("roblox-x86.apk")).unwrap();
        let err = staging.commit(None).err().expect("no base APK was given");
        assert!(matches!(err, StoreError::NoBaseGiven), "{err:?}");
        fs::remove_dir_all(&root).ok();
        fs::remove_dir_all(&sources).ok();
    }

    #[test]
    fn unsigned_candidates_are_rejected_without_touching_the_install() {
        let root = temp_root("unsigned");
        let sources = temp_root("unsigned-sources");
        fs::write(sources.join(BASE_APK), synthetic_apk(None, false)).unwrap();
        fs::write(
            sources.join(NATIVE_SPLIT_APK),
            synthetic_apk(Some(NATIVE_SPLIT_NAME), true),
        )
        .unwrap();

        let store = Store::at(root.clone());
        let err = store.install(std::slice::from_ref(&sources)).unwrap_err();
        assert!(
            matches!(
                err,
                StoreError::Set(ApkSetError::Signature {
                    source: SignatureError::MissingV2Signature,
                    ..
                })
            ),
            "{err:?}"
        );
        assert_eq!(store.current().unwrap(), None);
        assert!(entries(&root).is_empty(), "{:?}", entries(&root));
        fs::remove_dir_all(&root).ok();
        fs::remove_dir_all(&sources).ok();
    }

    #[test]
    fn bundles_yield_only_base_and_the_x86_64_split() {
        let root = temp_root("bundle");
        let sources = temp_root("bundle-sources");
        let base = synthetic_apk(None, false);
        let split = synthetic_apk(Some(NATIVE_SPLIT_NAME), true);
        let density = synthetic_apk(Some("config.xxhdpi"), false);
        fs::write(
            sources.join("roblox.apkm"),
            zip_of(
                &[
                    ("info.json", b"{}"),
                    (BASE_APK, &base),
                    ("split_config.xxhdpi.apk", &density),
                    (NATIVE_SPLIT_APK, &split),
                ],
                stored(),
            ),
        )
        .unwrap();
        fs::write(
            sources.join("roblox.XAPK"),
            zip_of(
                &[
                    ("manifest.json", b"{}"),
                    ("com.roblox.client.apk", &base),
                    ("config.x86_64.apk", &split),
                ],
                stored(),
            ),
        )
        .unwrap();

        let store = Store::at(root.clone());
        for bundle in ["roblox.apkm", "roblox.XAPK"] {
            let staging = store.begin().unwrap();
            staging.add_source(&sources.join(bundle)).unwrap();
            let mut staged = entries(staging.dir());
            staged.sort();
            assert_eq!(staged, [BASE_APK, NATIVE_SPLIT_APK], "{bundle}");
            assert_eq!(fs::read(staging.dir().join(BASE_APK)).unwrap(), base);
            assert_eq!(
                fs::read(staging.dir().join(NATIVE_SPLIT_APK)).unwrap(),
                split
            );
        }
        fs::remove_dir_all(&root).ok();
        fs::remove_dir_all(&sources).ok();
    }

    #[test]
    fn unusable_bundles_are_typed_errors() {
        let root = temp_root("bad-bundles");
        let sources = temp_root("bad-bundle-sources");
        let base = synthetic_apk(None, false);
        let split = synthetic_apk(Some(NATIVE_SPLIT_NAME), true);
        fs::write(sources.join("garbage.apkm"), b"APKM encrypted payload").unwrap();
        fs::write(
            sources.join("no-base.apks"),
            zip_of(&[(NATIVE_SPLIT_APK, &split)], stored()),
        )
        .unwrap();
        fs::write(
            sources.join("arm-only.apks"),
            zip_of(
                &[(BASE_APK, &base), ("split_config.arm64_v8a.apk", &split)],
                stored(),
            ),
        )
        .unwrap();
        fs::write(
            sources.join("locked.apks"),
            zip_of(
                &[(BASE_APK, &base)],
                stored().with_deprecated_encryption(b"secret").unwrap(),
            ),
        )
        .unwrap();

        let store = Store::at(root.clone());
        let expectations: [(&str, ErrorCheck); 3] = [
            ("garbage.apkm", |error| {
                matches!(error, StoreError::UnreadableBundle { .. })
            }),
            ("no-base.apks", |error| {
                matches!(error, StoreError::BundleMissingBase(_))
            }),
            ("locked.apks", |error| {
                matches!(error, StoreError::EncryptedBundle(_))
            }),
        ];
        for (bundle, expected) in expectations {
            let staging = store.begin().unwrap();
            let err = staging.add_source(&sources.join(bundle)).unwrap_err();
            assert!(expected(&err), "{bundle}: {err:?}");
        }

        let staging = store.begin().unwrap();
        staging.add_source(&sources.join("arm-only.apks")).unwrap();
        let err = staging
            .commit(None)
            .err()
            .expect("no x86_64 split was given");
        assert!(matches!(err, StoreError::NoNativeSplitGiven), "{err:?}");
        fs::remove_dir_all(&root).ok();
        fs::remove_dir_all(&sources).ok();
    }

    #[test]
    fn official_set_installs_from_a_directory_and_from_a_bundle() {
        let Some(paths) = ApkSetPaths::from_env().expect("ECLIPSE_ROBLOX_APK must be usable")
        else {
            eprintln!("SKIP: set ECLIPSE_ROBLOX_APK to install the official Roblox APK set");
            return;
        };
        let Some(split) = paths.native_split.clone() else {
            eprintln!("SKIP: ECLIPSE_ROBLOX_APK is a single APK, not a split set");
            return;
        };
        let root = temp_root("official");
        let sources = temp_root("official-sources");
        fs::copy(&paths.base, sources.join(BASE_APK)).unwrap();
        fs::copy(&split, sources.join(NATIVE_SPLIT_APK)).unwrap();

        let store = Store::at(root.clone());
        let installed = store
            .install(std::slice::from_ref(&sources))
            .expect("install from a directory");
        assert_eq!(store.current().unwrap(), Some(installed.clone()));
        let (current, installed_paths) = store.current_set().unwrap().unwrap();
        assert_eq!(current, installed);
        let set = ApkSet::open(installed_paths).expect("the installed set verifies");
        assert_eq!(set.version_code(), installed.version_code);
        drop(set);
        assert_eq!(usable_version(&store).unwrap(), Some(installed.clone()));

        let version_dir = root.join(installed.version_code.to_string());
        let staging = store.begin().unwrap();
        staging.add_source(&sources).unwrap();
        let kept = staging.commit(None).expect("reinstall the same version");
        assert_eq!(kept.base_path(), version_dir.join(BASE_APK));
        assert_eq!(kept.native_libs_path(), version_dir.join(NATIVE_SPLIT_APK));
        drop(kept);

        fs::remove_file(version_dir.join(NATIVE_SPLIT_APK)).unwrap();
        assert!(store.current_set().unwrap().is_some());
        assert_eq!(
            usable_version(&store).unwrap(),
            None,
            "an install that lost its x86_64 split is not usable"
        );

        let bundle = sources.join("roblox.apks");
        let mut writer = ZipWriter::new(File::create(&bundle).unwrap());
        for (name, path) in [(BASE_APK, &paths.base), (NATIVE_SPLIT_APK, &split)] {
            writer.start_file(name, stored()).unwrap();
            io::copy(&mut File::open(path).unwrap(), &mut writer).unwrap();
        }
        writer.finish().unwrap();
        let staging = store.begin().unwrap();
        staging.add_source(&bundle).unwrap();
        let mut repaired = staging.commit(None).expect("install from a bundle");
        assert_eq!(InstalledVersion::from(&repaired), installed);
        assert_eq!(repaired.base_path(), version_dir.join(BASE_APK));
        assert_eq!(
            repaired.native_libs_path(),
            version_dir.join(NATIVE_SPLIT_APK)
        );
        assert!(repaired
            .native_libs_mut()
            .native_lib_filenames(crate::apk::TARGET_ABI)
            .iter()
            .any(|name| name == crate::apk::ENGINE_LIB));
        drop(repaired);
        assert_eq!(
            usable_version(&store).unwrap(),
            Some(installed.clone()),
            "reinstalling repairs the damaged install"
        );
        assert_eq!(
            entries(&root),
            [installed.version_code.to_string(), CURRENT_FILE.to_owned()]
        );
        fs::remove_dir_all(&root).ok();
        fs::remove_dir_all(&sources).ok();
    }
}
