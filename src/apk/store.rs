use std::fmt;
use std::fs::{self, File, OpenOptions};
use std::io::{self, BufReader, Read};
use std::path::{Path, PathBuf};
use std::time::{Duration, SystemTime, UNIX_EPOCH};

use eclipse_config::temp_file::{self, TempFile};
use ring::digest::SHA1_OUTPUT_LEN;
use serde::{Deserialize, Serialize};
use zip::result::ZipError;
use zip::ZipArchive;

use super::signature::SigningCertificateHistory;
use super::{
    is_bundle, Apk, ApkError, ApkSet, ApkSetError, ApkSetFiles, ApkSetPaths, SetIdentity,
    VersionCode, BASE_APK, MAX_APK_BYTES, NATIVE_SPLIT_APK, NATIVE_SPLIT_NAME, ROBLOX_PACKAGE,
};
use crate::status::StatusSink;

const STORE_DIR: &str = "roblox";
const CURRENT_FILE: &str = "current.json";
const LAST_CHECK_FILE: &str = "last-update-check.json";
const VERIFIED_FILE: &str = "verified.json";
const LOCK_FILE: &str = "install.lock";
const STAGING_DIR: &str = "incoming.partial";
const PARTIAL_SUFFIX: &str = ".partial";
const XAPK_NATIVE_SPLIT: &str = "config.x86_64.apk";

const UPDATE_INTERVAL: Duration = Duration::from_secs(6 * 60 * 60);
const FAILED_CHECK_RETRY: Duration = Duration::from_secs(30 * 60);
const VERIFIED_SCHEMA: u32 = 1;

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

pub struct Committed {
    pub set: ApkSet,

    pub leftover: Option<PruneError>,
}

pub enum UpdateOutcome {
    UpToDate {
        installed: InstalledVersion,
    },
    Updated {
        previous: Option<InstalledVersion>,
        committed: Box<Committed>,
    },
}

#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum CheckOutcome {
    #[default]
    Completed,

    Failed,
}

impl CheckOutcome {
    fn next_check_after(self) -> Duration {
        match self {
            Self::Completed => UPDATE_INTERVAL,
            Self::Failed => FAILED_CHECK_RETRY,
        }
    }
}

pub fn update_due(
    installed: Option<VersionCode>,
    last_check: Option<&UpdateCheck>,
    now: SystemTime,
) -> bool {
    installed.is_none()
        || last_check.is_none_or(|check| match now.duration_since(check.at) {
            Ok(elapsed) => elapsed >= check.outcome.next_check_after(),
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

    pub outcome: CheckOutcome,
}

#[derive(Serialize, Deserialize)]
struct LastCheck {
    checked_at_unix: u64,

    rejected: Option<Release>,

    #[serde(default)]
    outcome: CheckOutcome,
}

#[derive(Serialize, Deserialize)]
struct VerifiedRecord {
    schema: u32,

    files: SetIdentity,

    certificates: Vec<Vec<u8>>,
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

    pub fn root(&self) -> &Path {
        &self.root
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
        let Some((installed, paths)) = self.current_set()? else {
            return Ok(None);
        };
        let dir = self.version_dir(installed.version_code);
        let files = ApkSetFiles::open(paths)?;
        let identity = files.identity()?;
        if let Some(history) = recorded_verification(&dir, &identity) {
            return Ok(Some(files.assemble(history)?));
        }
        let set = files.verify()?;
        record_verification(&dir, identity, set.signing_certificate_history());
        Ok(Some(set))
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
                outcome: record.outcome,
            }))
    }

    pub fn record_check(&self, check: &UpdateCheck) -> Result<(), StoreError> {
        let checked_at_unix = check
            .at
            .duration_since(UNIX_EPOCH)
            .map_or(0, |since| since.as_secs());
        create_dir_all(&self.root)?;
        replace_json(
            &self.root,
            LAST_CHECK_FILE,
            &LastCheck {
                checked_at_unix,
                rejected: check.rejected,
                outcome: check.outcome,
            },
        )
    }

    pub fn install(
        &self,
        sources: &[PathBuf],
        status: &StatusSink,
    ) -> Result<Committed, StoreError> {
        let staging = self.begin(status)?;
        for source in sources {
            staging.add_source(source)?;
        }
        staging.commit(None)
    }

    pub fn begin(&self, status: &StatusSink) -> Result<Staging<'_>, StoreError> {
        create_dir_all(&self.root)?;
        let lock_path = self.root.join(LOCK_FILE);
        let io_error = |source| StoreError::Io {
            path: lock_path.clone(),
            source,
        };
        let lock = OpenOptions::new()
            .create(true)
            .truncate(false)
            .write(true)
            .open(&lock_path)
            .map_err(io_error)?;
        match lock.try_lock() {
            Ok(()) => {}
            Err(std::fs::TryLockError::WouldBlock) => {
                status.step("Waiting for another Eclipse install or update to finish…");
                lock.lock().map_err(io_error)?;
            }
            Err(std::fs::TryLockError::Error(source)) => return Err(io_error(source)),
        }
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

    fn activate(&self, installed: &InstalledVersion) -> Result<Option<PruneError>, StoreError> {
        let previous = match self.current() {
            Ok(previous) => previous,
            Err(StoreError::Corrupt { .. }) => None,
            Err(error) => return Err(error),
        };

        replace_json(&self.root, CURRENT_FILE, installed)?;
        let previous = match previous {
            Some(previous) if previous.version_code == installed.version_code => return Ok(None),
            previous => previous.map(|previous| previous.version_code),
        };
        Ok(self.prune(installed.version_code, previous).err())
    }

    fn prune(
        &self,
        installed: VersionCode,
        previous: Option<VersionCode>,
    ) -> Result<(), PruneError> {
        let prune_error = |path: &Path| {
            let path = path.to_path_buf();
            move |source| PruneError { path, source }
        };
        let entries = fs::read_dir(&self.root).map_err(prune_error(&self.root))?;
        for entry in entries {
            let entry = entry.map_err(prune_error(&self.root))?;
            let path = entry.path();
            let file_type = entry.file_type().map_err(prune_error(&path))?;
            let name = entry.file_name();
            let Some(name) = name.to_str() else {
                continue;
            };
            if file_type.is_dir() {
                let stale = name.ends_with(PARTIAL_SUFFIX)
                    || name.parse::<u32>().is_ok_and(|code| {
                        let code = VersionCode(code);
                        code != installed && Some(code) != previous
                    });
                if stale {
                    fs::remove_dir_all(&path).map_err(prune_error(&path))?;
                }
            } else if temp_file::is_abandoned(&entry).map_err(prune_error(&path))? {
                fs::remove_file(&path).map_err(prune_error(&path))?;
            }
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

    pub fn commit(mut self, expected: Option<VersionCode>) -> Result<Committed, StoreError> {
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

        let files = ApkSetFiles::open(ApkSetPaths::locate(&self.dir)?)?;
        let identity = files.identity()?;
        let staged = files.verify()?;
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
                let set = staged.relocated(ApkSetPaths::locate(&target)?)?;
                record_verification(&target, identity, set.signing_certificate_history());
                set
            }
        };
        sync_dir(&self.store.root)?;
        let leftover = self.store.activate(&installed)?;
        Ok(Committed { set, leftover })
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

fn recorded_verification(dir: &Path, identity: &SetIdentity) -> Option<SigningCertificateHistory> {
    let path = dir.join(VERIFIED_FILE);
    let bytes = match fs::read(&path) {
        Ok(bytes) => bytes,
        Err(error) if error.kind() == io::ErrorKind::NotFound => return None,
        Err(error) => {
            tracing::warn!(
                path = %path.display(),
                %error,
                "cannot read the record of the last signature check; verifying Roblox again"
            );
            return None;
        }
    };
    let record = serde_json::from_slice::<VerifiedRecord>(&bytes).ok()?;
    (record.schema == VERIFIED_SCHEMA && record.files == *identity)
        .then(|| SigningCertificateHistory::recorded(record.certificates))
}

fn record_verification(dir: &Path, files: SetIdentity, history: &SigningCertificateHistory) {
    if let Err(error) = temp_file::remove_abandoned(dir) {
        tracing::warn!(
            dir = %dir.display(),
            %error,
            "cannot remove temporary files left in the installed Roblox's directory"
        );
    }
    let record = VerifiedRecord {
        schema: VERIFIED_SCHEMA,
        files,
        certificates: history.certificates().to_vec(),
    };
    if let Err(error) = replace_json(dir, VERIFIED_FILE, &record) {
        tracing::warn!(
            %error,
            "cannot record the signature check; the next launch verifies Roblox again"
        );
    }
}

fn replace_json(dir: &Path, name: &str, record: &impl Serialize) -> Result<(), StoreError> {
    let mut json = serde_json::to_vec_pretty(record).expect("store records always serialize");
    json.push(b'\n');
    let target = dir.join(name);
    let mut temp = TempFile::create(dir, name).map_err(|source| StoreError::Io {
        path: dir.to_path_buf(),
        source,
    })?;
    temp.write_all(&json)
        .and_then(|()| temp.sync())
        .map_err(|source| StoreError::Io {
            path: temp.path().to_path_buf(),
            source,
        })?;
    let temp_path = temp.path().to_path_buf();
    temp.persist(&target)
        .map_err(|source| StoreError::Replace {
            temp: temp_path,
            path: target,
            source,
        })?;
    sync_dir(dir)
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

    Replace {
        temp: PathBuf,
        path: PathBuf,
        source: io::Error,
    },
}

#[derive(Debug)]
pub struct PruneError {
    path: PathBuf,
    source: io::Error,
}

impl fmt::Display for PruneError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(
            f,
            "Roblox was installed, but Eclipse could not remove the old Roblox files in {}: {}",
            self.path.display(),
            self.source
        )
    }
}

impl std::error::Error for PruneError {
    fn source(&self) -> Option<&(dyn std::error::Error + 'static)> {
        Some(&self.source)
    }
}

impl StoreError {
    pub fn is_unusable_install(&self) -> bool {
        matches!(self, Self::Corrupt { .. } | Self::MissingInstall { .. })
            || matches!(self, Self::Set(error) if !matches!(error, ApkSetError::Locate { .. }))
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
            Self::Replace { temp, path, source } => write!(
                f,
                "cannot move {} over {}: {source}",
                temp.display(),
                path.display()
            ),
        }
    }
}

impl std::error::Error for StoreError {
    fn source(&self) -> Option<&(dyn std::error::Error + 'static)> {
        match self {
            Self::Io { source, .. } | Self::Replace { source, .. } => Some(source),
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
    use std::io::Write as _;
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

        assert!(store.activate(&version(3055)).unwrap().is_none());
        assert_eq!(store.current().unwrap(), Some(version(3055)));
        assert_eq!(entries(&root), ["3053", "3055", CURRENT_FILE, "notes"]);

        fs::create_dir_all(root.join("3056")).unwrap();
        assert!(store.activate(&version(3056)).unwrap().is_none());
        assert_eq!(
            entries(&root),
            ["3053", "3055", "3056", CURRENT_FILE, "notes"]
        );

        fs::create_dir_all(root.join("3057")).unwrap();
        assert!(store.activate(&version(3057)).unwrap().is_none());
        assert_eq!(
            entries(&root),
            ["3053", "3056", "3057", CURRENT_FILE, "notes"]
        );

        fs::create_dir_all(root.join("3040")).unwrap();
        assert!(store.activate(&version(3057)).unwrap().is_none());
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
        let completed = |at| UpdateCheck {
            at,
            rejected: None,
            outcome: CheckOutcome::Completed,
        };
        assert!(update_due(installed, None, now));
        assert!(!update_due(
            installed,
            Some(&completed(now - Duration::from_secs(60))),
            now
        ));
        assert!(!update_due(
            installed,
            Some(&completed(now - UPDATE_INTERVAL + Duration::from_secs(1))),
            now
        ));
        assert!(update_due(
            installed,
            Some(&completed(now - UPDATE_INTERVAL)),
            now
        ));
        assert!(
            update_due(
                installed,
                Some(&completed(now + Duration::from_secs(60))),
                now
            ),
            "a check recorded in the future is treated as stale"
        );
        assert!(
            update_due(None, Some(&completed(now - Duration::from_secs(60))), now),
            "without a usable install Roblox is downloaded despite a recent check"
        );

        let root = temp_root("last-check");
        let store = Store::at(root.clone());
        assert_eq!(store.last_check().unwrap(), None);
        let checked = completed(now);
        store.record_check(&checked).unwrap();
        assert_eq!(store.last_check().unwrap(), Some(checked));
        assert_eq!(entries(&root), [LAST_CHECK_FILE]);

        let rejected = UpdateCheck {
            at: now,
            rejected: Some(Release {
                version_code: VersionCode(3170),
                base_sha1: [0x40; SHA1_OUTPUT_LEN],
            }),
            outcome: CheckOutcome::Completed,
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
            "a record from before failed checks were recorded is a completed check that \
             rejects nothing"
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

    #[test]
    fn a_failed_check_is_retried_after_thirty_minutes_and_keeps_its_rejection() {
        let now = UNIX_EPOCH + Duration::from_secs(1_800_000_000);
        let installed = Some(VersionCode(3170));
        let failed = |at| UpdateCheck {
            at,
            rejected: Some(Release {
                version_code: VersionCode(3171),
                base_sha1: [0x41; SHA1_OUTPUT_LEN],
            }),
            outcome: CheckOutcome::Failed,
        };
        assert!(!update_due(
            installed,
            Some(&failed(now - FAILED_CHECK_RETRY + Duration::from_secs(60))),
            now
        ));
        assert!(update_due(
            installed,
            Some(&failed(now - FAILED_CHECK_RETRY)),
            now
        ));

        let root = temp_root("failed-check");
        let store = Store::at(root.clone());
        store.record_check(&failed(now)).unwrap();
        assert_eq!(store.last_check().unwrap(), Some(failed(now)));
        fs::remove_dir_all(&root).ok();
    }

    #[test]
    fn concurrent_check_records_never_collide() {
        const WRITERS: usize = 8;
        const RECORDS: usize = 25;
        let root = temp_root("concurrent-checks");
        let barrier = std::sync::Barrier::new(WRITERS);
        let now = UNIX_EPOCH + Duration::from_secs(1_800_000_000);
        let failures: Vec<StoreError> = std::thread::scope(|scope| {
            let writers: Vec<_> = (0..WRITERS)
                .map(|writer| {
                    let (root, barrier) = (root.clone(), &barrier);
                    scope.spawn(move || {
                        let store = Store::at(root);
                        barrier.wait();
                        (0..RECORDS)
                            .filter_map(|_| {
                                store
                                    .record_check(&UpdateCheck {
                                        at: now,
                                        rejected: (writer % 2 == 1).then_some(Release {
                                            version_code: VersionCode(3170),
                                            base_sha1: [0x40; SHA1_OUTPUT_LEN],
                                        }),
                                        outcome: CheckOutcome::Completed,
                                    })
                                    .err()
                            })
                            .collect::<Vec<_>>()
                    })
                })
                .collect();
            writers
                .into_iter()
                .flat_map(|writer| writer.join().expect("writer thread"))
                .collect()
        });
        assert!(failures.is_empty(), "{failures:?}");
        assert!(Store::at(root.clone()).last_check().unwrap().is_some());
        assert_eq!(
            entries(&root),
            [LAST_CHECK_FILE],
            "no temporary file is left behind"
        );
        fs::remove_dir_all(&root).ok();
    }

    #[test]
    fn waiting_for_another_install_is_reported_before_blocking() {
        let root = temp_root("lock-wait");
        let store = Store::at(root.clone());
        let (updates, reported) = std::sync::mpsc::channel();
        let holder = store.begin(&StatusSink::terminal()).unwrap();
        std::thread::scope(|scope| {
            let waiter = scope.spawn(|| {
                let waiting = Store::at(root.clone());
                waiting.begin(&StatusSink::with_window(updates)).map(drop)
            });
            let update = reported
                .recv_timeout(Duration::from_secs(10))
                .expect("the wait is reported");
            assert_eq!(
                update,
                crate::status::StatusUpdate::Step(
                    "Waiting for another Eclipse install or update to finish…".to_owned()
                )
            );
            drop(holder);
            waiter
                .join()
                .expect("waiter thread")
                .expect("the lock is taken once it is released");
        });
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
    fn an_install_that_cannot_be_read_is_reported_instead_of_replaced() {
        use std::os::unix::fs::PermissionsExt as _;

        let root = temp_root("unreadable-install");
        let store = Store::at(root.clone());
        fs::write(
            root.join(CURRENT_FILE),
            serde_json::to_vec(&version(3056)).unwrap(),
        )
        .unwrap();
        let base = root.join("3056").join(BASE_APK);
        fs::create_dir_all(base.parent().unwrap()).unwrap();
        fs::write(&base, synthetic_apk(None, true)).unwrap();
        fs::set_permissions(&base, fs::Permissions::from_mode(0o000)).unwrap();

        let usable = usable_version(&store);
        fs::remove_dir_all(&root).ok();
        let err = usable.unwrap_err();
        assert!(
            matches!(&err, StoreError::Set(ApkSetError::Locate { path, .. }) if *path == base),
            "a read failure is an error, not a missing install: {err:?}"
        );
        assert!(!err.is_unusable_install());
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
        let staging = store.begin(&StatusSink::terminal()).unwrap();
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

        let staging = store.begin(&StatusSink::terminal()).unwrap();
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
        let err = store
            .install(std::slice::from_ref(&sources), &StatusSink::terminal())
            .err()
            .expect("unsigned APKs are refused");
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
            let staging = store.begin(&StatusSink::terminal()).unwrap();
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
            let staging = store.begin(&StatusSink::terminal()).unwrap();
            let err = staging.add_source(&sources.join(bundle)).unwrap_err();
            assert!(expected(&err), "{bundle}: {err:?}");
        }

        let staging = store.begin(&StatusSink::terminal()).unwrap();
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
        let committed = store
            .install(std::slice::from_ref(&sources), &StatusSink::terminal())
            .expect("install from a directory");
        assert!(committed.leftover.is_none());
        let installed = InstalledVersion::from(&committed.set);
        drop(committed);
        assert_eq!(store.current().unwrap(), Some(installed.clone()));
        let (current, installed_paths) = store.current_set().unwrap().unwrap();
        assert_eq!(current, installed);
        let set = ApkSet::open(installed_paths).expect("the installed set verifies");
        assert_eq!(set.version_code(), installed.version_code);
        drop(set);
        assert_eq!(usable_version(&store).unwrap(), Some(installed.clone()));

        let version_dir = root.join(installed.version_code.to_string());
        let staging = store.begin(&StatusSink::terminal()).unwrap();
        staging.add_source(&sources).unwrap();
        let kept = staging
            .commit(None)
            .expect("reinstall the same version")
            .set;
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
        let staging = store.begin(&StatusSink::terminal()).unwrap();
        staging.add_source(&bundle).unwrap();
        let mut repaired = staging.commit(None).expect("install from a bundle").set;
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

    fn official_sources(tag: &str) -> Option<PathBuf> {
        let Some(paths) = ApkSetPaths::from_env().expect("ECLIPSE_ROBLOX_APK must be usable")
        else {
            eprintln!("SKIP: set ECLIPSE_ROBLOX_APK to install the official Roblox APK set");
            return None;
        };
        let Some(split) = paths.native_split else {
            eprintln!("SKIP: ECLIPSE_ROBLOX_APK is a single APK, not a split set");
            return None;
        };
        let sources = temp_root(tag);
        fs::copy(&paths.base, sources.join(BASE_APK)).unwrap();
        fs::copy(&split, sources.join(NATIVE_SPLIT_APK)).unwrap();
        Some(sources)
    }

    fn history(set: &ApkSet) -> Vec<Vec<u8>> {
        set.signing_certificate_history().certificates().to_vec()
    }

    #[test]
    fn an_old_install_that_cannot_be_removed_does_not_fail_the_new_install() {
        use std::os::unix::fs::PermissionsExt as _;

        struct Cleanup<'a> {
            read_only: &'a Path,
            roots: [&'a Path; 2],
        }

        impl Drop for Cleanup<'_> {
            fn drop(&mut self) {
                fs::set_permissions(self.read_only, fs::Permissions::from_mode(0o755)).ok();
                for root in self.roots {
                    fs::remove_dir_all(root).ok();
                }
            }
        }

        let Some(sources) = official_sources("prune-leftover-sources") else {
            return;
        };
        let root = temp_root("prune-leftover");
        let locked = root.join("123").join("locked");
        let _cleanup = Cleanup {
            read_only: &locked,
            roots: [&root, &sources],
        };
        fs::create_dir_all(&locked).unwrap();
        fs::write(locked.join("file"), b"old").unwrap();

        let store = Store::at(root.clone());
        let staging = store.begin(&StatusSink::terminal()).unwrap();
        staging.add_source(&sources).unwrap();
        fs::set_permissions(&locked, fs::Permissions::from_mode(0o555)).unwrap();
        let committed = staging.commit(None).expect("the new install succeeds");
        let leftover = committed.leftover.expect("the old install is reported");
        assert_eq!(leftover.path, root.join("123"));
        assert_eq!(leftover.source.kind(), io::ErrorKind::PermissionDenied);
        assert_eq!(
            store.current().unwrap(),
            Some(InstalledVersion::from(&committed.set))
        );
    }

    #[test]
    fn recording_a_signature_check_removes_abandoned_temporaries_beside_it() {
        let dir = temp_root("version-temporaries");
        let file = crate::apk::FileIdentity {
            dev: 1,
            ino: 2,
            size: 3,
            mtime: 4,
            mtime_nsec: 5,
            ctime: 6,
            ctime_nsec: 7,
        };
        let identity = SetIdentity {
            base: file,
            native_split: Some(file),
        };
        let abandoned = dir.join(format!("{VERIFIED_FILE}.00000000000000aa.tmp"));
        let in_flight = dir.join(format!("{VERIFIED_FILE}.00000000000000bb.tmp"));
        fs::write(&abandoned, b"{").unwrap();
        fs::write(&in_flight, b"{").unwrap();
        File::options()
            .write(true)
            .open(&abandoned)
            .unwrap()
            .set_modified(SystemTime::now() - Duration::from_secs(2 * 60 * 60))
            .unwrap();

        record_verification(
            &dir,
            identity,
            &SigningCertificateHistory::recorded(vec![b"certificate".to_vec()]),
        );

        assert!(
            !abandoned.exists(),
            "a temporary left by a crash is removed"
        );
        assert!(in_flight.exists(), "another writer's temporary is kept");
        assert!(recorded_verification(&dir, &identity).is_some());
        fs::remove_dir_all(&dir).ok();
    }

    #[test]
    fn verified_installs_skip_rehashing_only_while_their_files_are_unchanged() {
        use std::os::unix::fs::FileExt as _;

        let Some(sources) = official_sources("verified-record-sources") else {
            return;
        };
        let root = temp_root("verified-record");
        let store = Store::at(root.clone());
        let installed = store
            .install(std::slice::from_ref(&sources), &StatusSink::terminal())
            .expect("the official set installs");
        let official = history(&installed.set);
        let dir = root.join(installed.set.version_code().to_string());
        drop(installed);
        let record_path = dir.join(VERIFIED_FILE);
        assert!(
            record_path.is_file(),
            "installing records the signature check"
        );

        let current = store.verified_current().unwrap().unwrap();
        assert_eq!(history(&current), official);
        drop(current);

        let record = |path: &Path| -> VerifiedRecord {
            serde_json::from_slice(&fs::read(path).unwrap()).unwrap()
        };
        let mut forged = record(&record_path);
        forged.certificates = vec![b"recorded".to_vec()];
        fs::write(&record_path, serde_json::to_vec(&forged).unwrap()).unwrap();
        assert_eq!(
            history(&store.verified_current().unwrap().unwrap()),
            [b"recorded".to_vec()],
            "an unchanged install is not hashed again"
        );

        forged.schema = VERIFIED_SCHEMA + 1;
        fs::write(&record_path, serde_json::to_vec(&forged).unwrap()).unwrap();
        assert_eq!(
            history(&store.verified_current().unwrap().unwrap()),
            official,
            "a record of another schema is ignored"
        );
        assert_eq!(record(&record_path).schema, VERIFIED_SCHEMA);

        let base = dir.join(BASE_APK);
        let copy = dir.join("base.copy");
        fs::copy(&base, &copy).unwrap();
        let modified = fs::metadata(&base).unwrap().modified().unwrap();
        File::options()
            .write(true)
            .open(&copy)
            .unwrap()
            .set_modified(modified)
            .unwrap();
        let before = record(&record_path).files;
        fs::rename(&copy, &base).unwrap();
        assert_eq!(
            history(&store.verified_current().unwrap().unwrap()),
            official,
            "an identical copy is verified again"
        );
        assert_ne!(
            record(&record_path).files,
            before,
            "the record follows the new file"
        );

        let file = File::options().read(true).write(true).open(&base).unwrap();
        let middle = file.metadata().unwrap().len() / 2;
        let mut byte = [0u8; 1];
        file.read_exact_at(&mut byte, middle).unwrap();
        file.write_all_at(&[byte[0] ^ 1], middle).unwrap();
        file.set_modified(modified).unwrap();
        drop(file);
        let err = store
            .verified_current()
            .err()
            .expect("a modified install fails the signature check");
        assert!(
            matches!(
                err,
                StoreError::Set(ApkSetError::Signature {
                    source: SignatureError::ContentDigestMismatch,
                    ..
                })
            ),
            "{err:?}"
        );
        fs::remove_dir_all(&root).ok();
        fs::remove_dir_all(&sources).ok();
    }
}
