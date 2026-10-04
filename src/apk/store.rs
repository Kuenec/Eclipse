use std::collections::BTreeMap;
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
const STATE_FILE: &str = "state.json";
const STATE_LOCK_FILE: &str = "state.lock";
const ATTEMPT_FILE: &str = "attempt.json";
const STAGING_DIR: &str = "incoming.partial";
const PARTIAL_SUFFIX: &str = ".partial";
const XAPK_NATIVE_SPLIT: &str = "config.x86_64.apk";

const UPDATE_INTERVAL: Duration = Duration::from_secs(6 * 60 * 60);
const FAILED_CHECK_RETRY: Duration = Duration::from_secs(30 * 60);
const VERIFIED_SCHEMA: u32 = 1;
const STATE_SCHEMA: u32 = 1;
const FAILED_STARTS_BEFORE_FALLBACK: u8 = 2;
const KEPT_FILE_REJECTIONS: usize = 8;

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

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct UpdateCheck {
    pub at: SystemTime,

    pub outcome: CheckOutcome,
}

#[derive(Serialize, Deserialize)]
struct LastCheck {
    checked_at_unix: u64,

    #[serde(default)]
    outcome: CheckOutcome,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub enum DeclaredFile {
    #[serde(rename = "apkcombo_base_sha1")]
    ApkComboBaseSha1([u8; SHA1_OUTPUT_LEN]),
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Candidate {
    pub version: VersionCode,

    pub file: DeclaredFile,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum CheckMode {
    Scheduled,

    Explicit,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum LeftBecause {
    FailedToStart,

    RolledBack,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Rejection {
    FailedVerification,

    Left(LeftBecause),
}

#[derive(Debug, PartialEq, Eq)]
pub enum Plan {
    UpToDate(InstalledVersion),

    Older(InstalledVersion),

    Rejected(Rejection),

    Download,
}

#[derive(Debug, Default, PartialEq, Eq)]
pub struct Rejections {
    left: BTreeMap<VersionCode, LeftBecause>,

    files: Vec<DeclaredFile>,
}

impl Rejections {
    pub fn skipped_versions(&self) -> impl Iterator<Item = (VersionCode, LeftBecause)> + '_ {
        self.left
            .iter()
            .map(|(&version, &because)| (version, because))
    }
}

pub fn plan(
    candidate: Candidate,
    verified: Option<&InstalledVersion>,
    recorded: Option<&InstalledVersion>,
    rejections: &Rejections,
    mode: CheckMode,
) -> Plan {
    if let Some(installed) =
        verified.filter(|installed| installed.version_code >= candidate.version)
    {
        return Plan::UpToDate(installed.clone());
    }
    if let Some(installed) = recorded.filter(|installed| installed.version_code > candidate.version)
    {
        return Plan::Older(installed.clone());
    }
    match mode {
        CheckMode::Explicit => Plan::Download,
        CheckMode::Scheduled => match rejections.left.get(&candidate.version) {
            Some(&because) => Plan::Rejected(Rejection::Left(because)),
            None if rejections.files.contains(&candidate.file) => {
                Plan::Rejected(Rejection::FailedVerification)
            }
            None => Plan::Download,
        },
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum VersionState {
    Unproven { failed_starts: u8 },

    Proven,

    Played,

    BothFailed,
}

impl VersionState {
    fn started(self) -> bool {
        matches!(self, Self::Proven | Self::Played)
    }
}

const UNPROVEN: VersionState = VersionState::Unproven { failed_starts: 0 };

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Proof {
    FirstFrame,

    NormalClose,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum VersionRole {
    Current,

    Fallback,

    Failed,

    Unkept,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct StoredVersion {
    pub version: InstalledVersion,

    pub dir: PathBuf,

    pub role: VersionRole,

    pub state: VersionState,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
struct RejectedVersion {
    #[serde(flatten)]
    version: InstalledVersion,

    eclipse_version: String,

    because: LeftBecause,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
struct State {
    schema: u32,

    versions: BTreeMap<VersionCode, VersionState>,

    fallback_from: Option<InstalledVersion>,

    rejected_versions: Vec<RejectedVersion>,

    rejected_files: Vec<DeclaredFile>,
}

impl State {
    fn new() -> Self {
        Self {
            schema: STATE_SCHEMA,
            versions: BTreeMap::new(),
            fallback_from: None,
            rejected_versions: Vec::new(),
            rejected_files: Vec::new(),
        }
    }

    fn version(&self, version: VersionCode) -> VersionState {
        self.versions.get(&version).copied().unwrap_or(UNPROVEN)
    }

    fn fallback_target(&self, current: VersionCode) -> Option<VersionCode> {
        let rejected = |version: VersionCode| {
            self.rejected_versions
                .iter()
                .any(|rejection| rejection.version.version_code == version)
        };
        self.versions
            .iter()
            .filter(|(version, state)| {
                **version != current && state.started() && !rejected(**version)
            })
            .max_by_key(|(version, state)| (**state == VersionState::Played, **version))
            .map(|(version, _)| *version)
    }

    fn kept(&self, current: VersionCode) -> BTreeMap<VersionCode, VersionRole> {
        let mut kept = BTreeMap::new();
        if self.version(current) != VersionState::Played {
            kept.extend(
                self.fallback_target(current)
                    .map(|fallback| (fallback, VersionRole::Fallback)),
            );
        }
        kept.extend(
            self.fallback_from
                .as_ref()
                .map(|failed| (failed.version_code, VersionRole::Failed)),
        );
        kept.insert(current, VersionRole::Current);
        kept
    }
}

#[derive(Serialize, Deserialize)]
struct AttemptRecord {
    version_code: VersionCode,
}

pub struct Attempt {
    store: Store,
}

impl Attempt {
    pub fn closed_by_user(self) -> Result<(), StoreError> {
        let _state = self.store.lock_state()?;
        self.store.remove_attempt()
    }
}

#[derive(Debug, PartialEq, Eq)]
pub struct FellBack {
    pub failed: InstalledVersion,

    pub using: InstalledVersion,
}

impl fmt::Display for FellBack {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(
            f,
            "Roblox {} failed to start twice, so Eclipse started the version it kept, {}. Roblox \
             may soon require the newer version; Eclipse tries newer releases as they come out.",
            self.failed, self.using
        )
    }
}

#[derive(Debug, PartialEq, Eq)]
pub struct BothFailed {
    pub newer: InstalledVersion,

    pub older: InstalledVersion,
}

impl fmt::Display for BothFailed {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(
            f,
            "Roblox {} and the kept {} both failed to start, which points at the graphics driver \
             or Eclipse rather than Roblox. Eclipse stays on {}; see the log.",
            self.newer, self.older, self.newer
        )
    }
}

#[derive(Debug, PartialEq, Eq)]
pub struct RolledBack {
    pub from: InstalledVersion,

    pub to: InstalledVersion,
}

impl fmt::Display for RolledBack {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(
            f,
            "went back from Roblox {} to {}; automatic updates skip {}",
            self.from, self.to, self.from
        )
    }
}

#[derive(Debug, PartialEq, Eq)]
pub enum Rollback {
    RolledBack(RolledBack),

    NotInstalled,

    NothingKept,
}

#[derive(Serialize, Deserialize)]
struct VerifiedRecord {
    schema: u32,

    files: SetIdentity,

    certificates: Vec<Vec<u8>>,
}

#[derive(Clone)]
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
                outcome: check.outcome,
            },
        )
    }

    pub fn install(&self, sources: &[PathBuf], status: &StatusSink) -> Result<ApkSet, StoreError> {
        let staging = self.begin(status)?;
        for source in sources {
            staging.add_source(source)?;
        }
        staging.commit(None)
    }

    pub fn begin(&self, status: &StatusSink) -> Result<Staging<'_>, StoreError> {
        let lock = self.wait_for_install_lock(status)?;
        let dir = self.root.join(STAGING_DIR);
        remove_dir_if_present(&dir)?;
        create_dir_all(&dir)?;
        Ok(Staging {
            store: self,
            dir,
            _lock: lock,
        })
    }

    pub fn lock_install(&self, status: &StatusSink) -> Result<InstallLock<'_>, StoreError> {
        Ok(InstallLock {
            store: self,
            _lock: self.wait_for_install_lock(status)?,
        })
    }

    pub fn versions(&self) -> Result<Vec<StoredVersion>, StoreError> {
        let entries = match fs::read_dir(&self.root) {
            Ok(entries) => entries,
            Err(error) if error.kind() == io::ErrorKind::NotFound => return Ok(Vec::new()),
            Err(source) => {
                return Err(StoreError::Io {
                    path: self.root.clone(),
                    source,
                })
            }
        };
        let current = self.current()?;
        let state = self.read_state()?;
        let kept = current
            .as_ref()
            .map(|current| state.kept(current.version_code))
            .unwrap_or_default();
        let mut versions = Vec::new();
        for entry in entries {
            let entry = entry.map_err(|source| StoreError::Io {
                path: self.root.clone(),
                source,
            })?;
            let Some(code) = entry
                .file_name()
                .to_str()
                .and_then(|name| name.parse().ok())
                .map(VersionCode)
            else {
                continue;
            };
            let dir = entry.path();
            let file_type = entry.file_type().map_err(|source| StoreError::Io {
                path: dir.clone(),
                source,
            })?;
            if !file_type.is_dir() {
                continue;
            }
            let role = kept.get(&code).copied().unwrap_or(VersionRole::Unkept);
            let version_name = match role {
                VersionRole::Current => current
                    .as_ref()
                    .and_then(|current| current.version_name.clone()),
                VersionRole::Fallback | VersionRole::Failed => self
                    .installed_version(code)?
                    .and_then(|installed| installed.version_name),
                VersionRole::Unkept => None,
            };
            versions.push(StoredVersion {
                version: InstalledVersion {
                    version_code: code,
                    version_name,
                },
                dir,
                role,
                state: state.version(code),
            });
        }
        versions.sort_by_key(|stored| std::cmp::Reverse(stored.version.version_code));
        Ok(versions)
    }

    pub fn settle_launch(&self) -> Result<Option<BothFailed>, StoreError> {
        let _state = self.lock_state()?;
        let Some(attempted) = self.take_attempt()? else {
            return Ok(None);
        };
        let mut state = self.read_state()?;
        let current = self.readable_current()?;
        let failed_fallback = state.fallback_from.clone().zip(
            current
                .clone()
                .filter(|current| current.version_code == attempted),
        );
        let Some((newer, older)) = failed_fallback else {
            if let VersionState::Unproven { failed_starts } = state.version(attempted) {
                state.versions.insert(
                    attempted,
                    VersionState::Unproven {
                        failed_starts: failed_starts.saturating_add(1),
                    },
                );
            }
            return self
                .write_state(state, current.map(|current| current.version_code))
                .map(|()| None);
        };
        state
            .rejected_versions
            .retain(|rejection| rejection.version.version_code != newer.version_code);
        state
            .versions
            .insert(newer.version_code, VersionState::BothFailed);
        state.fallback_from = None;
        self.write_state(state, Some(newer.version_code))?;
        replace_json(&self.root, CURRENT_FILE, &newer)?;
        Ok(Some(BothFailed { newer, older }))
    }

    pub fn fall_back_if_failing(
        &self,
        eclipse_version: &str,
    ) -> Result<Option<FellBack>, StoreError> {
        let _state = self.lock_state()?;
        let Some(failed) = self.readable_current()? else {
            return Ok(None);
        };
        let state = self.read_state()?;
        let failing = matches!(
            state.version(failed.version_code),
            VersionState::Unproven { failed_starts } if failed_starts >= FAILED_STARTS_BEFORE_FALLBACK
        );
        if !failing {
            return Ok(None);
        }
        let Some(using) = self.kept_proven(&state, failed.version_code)? else {
            return Ok(None);
        };
        self.switch_to_kept(
            state,
            &failed,
            &using,
            eclipse_version,
            LeftBecause::FailedToStart,
        )?;
        Ok(Some(FellBack { failed, using }))
    }

    pub fn roll_back(&self, eclipse_version: &str) -> Result<Rollback, StoreError> {
        let _state = self.lock_state()?;
        let Some(from) = self.current()? else {
            return Ok(Rollback::NotInstalled);
        };
        let state = self.read_state()?;
        let Some(to) = self.kept_proven(&state, from.version_code)? else {
            return Ok(Rollback::NothingKept);
        };
        self.switch_to_kept(state, &from, &to, eclipse_version, LeftBecause::RolledBack)?;
        Ok(Rollback::RolledBack(RolledBack { from, to }))
    }

    fn switch_to_kept(
        &self,
        mut state: State,
        left: &InstalledVersion,
        kept: &InstalledVersion,
        eclipse_version: &str,
        because: LeftBecause,
    ) -> Result<(), StoreError> {
        state
            .rejected_versions
            .retain(|rejection| rejection.version.version_code != left.version_code);
        state.rejected_versions.push(RejectedVersion {
            version: left.clone(),
            eclipse_version: eclipse_version.to_owned(),
            because,
        });
        state.fallback_from = match because {
            LeftBecause::FailedToStart => Some(left.clone()),
            LeftBecause::RolledBack => None,
        };
        self.write_state(state, Some(kept.version_code))?;
        replace_json(&self.root, CURRENT_FILE, kept)
    }

    pub fn proof_needed(&self, version: VersionCode) -> Result<Option<Proof>, StoreError> {
        let state = self.read_state()?;
        if state.fallback_from.is_some() {
            return Ok(Some(Proof::FirstFrame));
        }
        Ok(match state.version(version) {
            VersionState::Unproven { .. } | VersionState::BothFailed => Some(Proof::FirstFrame),
            VersionState::Proven => Some(Proof::NormalClose),
            VersionState::Played => None,
        })
    }

    pub fn begin_attempt(&self, version: VersionCode) -> Result<Attempt, StoreError> {
        let _state = self.lock_state()?;
        replace_json(
            &self.root,
            ATTEMPT_FILE,
            &AttemptRecord {
                version_code: version,
            },
        )?;
        Ok(Attempt {
            store: self.clone(),
        })
    }

    pub fn record_first_frame(&self, version: VersionCode) -> Result<(), StoreError> {
        self.record_start(version, VersionState::Proven)
    }

    pub fn record_normal_close(&self, version: VersionCode) -> Result<(), StoreError> {
        self.record_start(version, VersionState::Played)
    }

    fn record_start(&self, version: VersionCode, reached: VersionState) -> Result<(), StoreError> {
        let _state = self.lock_state()?;
        let proven = self.read_state().and_then(|mut state| {
            if state.version(version) != VersionState::Played {
                state.versions.insert(version, reached);
            }
            let current = self.readable_current()?.map(|current| current.version_code);
            if current == Some(version) {
                state.fallback_from = None;
            }
            self.write_state(state, current)
        });
        let removed = self.remove_attempt();
        proven.and(removed)
    }

    pub fn rejections(&self, eclipse_version: &str) -> Result<Rejections, StoreError> {
        let state = self.read_state()?;
        Ok(Rejections {
            left: state
                .rejected_versions
                .iter()
                .filter(|rejection| rejection.eclipse_version == eclipse_version)
                .map(|rejection| (rejection.version.version_code, rejection.because))
                .collect(),
            files: state.rejected_files,
        })
    }

    pub fn reject_file(&self, file: DeclaredFile) -> Result<(), StoreError> {
        let _state = self.lock_state()?;
        let mut state = self.read_state()?;
        state.rejected_files.retain(|rejected| *rejected != file);
        state.rejected_files.push(file);
        let excess = state
            .rejected_files
            .len()
            .saturating_sub(KEPT_FILE_REJECTIONS);
        state.rejected_files.drain(..excess);
        let current = self.readable_current()?.map(|current| current.version_code);
        self.write_state(state, current)
    }

    fn version_dir(&self, version: VersionCode) -> PathBuf {
        self.root.join(version.to_string())
    }

    fn readable_current(&self) -> Result<Option<InstalledVersion>, StoreError> {
        match self.current() {
            Err(StoreError::Corrupt { .. }) => Ok(None),
            current => current,
        }
    }

    fn kept_proven(
        &self,
        state: &State,
        current: VersionCode,
    ) -> Result<Option<InstalledVersion>, StoreError> {
        match state.fallback_target(current) {
            Some(version) => self.installed_version(version),
            None => Ok(None),
        }
    }

    fn installed_version(
        &self,
        version: VersionCode,
    ) -> Result<Option<InstalledVersion>, StoreError> {
        let paths = match ApkSetPaths::locate(&self.version_dir(version)) {
            Ok(paths) => paths,
            Err(ApkSetError::MissingBase(_)) => return Ok(None),
            Err(ApkSetError::Locate { source, .. }) if source.kind() == io::ErrorKind::NotFound => {
                return Ok(None)
            }
            Err(error) => return Err(error.into()),
        };
        let info = Apk::open(&paths.base)
            .and_then(|mut apk| apk.package_info())
            .map_err(|source| StoreError::Apk {
                path: paths.base.clone(),
                source,
            })?;
        Ok(Some(InstalledVersion {
            version_code: version,
            version_name: info.version_name,
        }))
    }

    fn open_lock(&self, name: &str) -> Result<File, StoreError> {
        create_dir_all(&self.root)?;
        let path = self.root.join(name);
        OpenOptions::new()
            .create(true)
            .truncate(false)
            .write(true)
            .open(&path)
            .map_err(|source| StoreError::Io { path, source })
    }

    fn wait_for_install_lock(&self, status: &StatusSink) -> Result<File, StoreError> {
        let lock = self.open_lock(LOCK_FILE)?;
        let io_error = |source| StoreError::Io {
            path: self.root.join(LOCK_FILE),
            source,
        };
        match lock.try_lock() {
            Ok(()) => Ok(lock),
            Err(std::fs::TryLockError::WouldBlock) => {
                status.step("Waiting for another Eclipse install or update to finish…");
                lock.lock().map_err(io_error)?;
                Ok(lock)
            }
            Err(std::fs::TryLockError::Error(source)) => Err(io_error(source)),
        }
    }

    fn lock_state(&self) -> Result<File, StoreError> {
        let lock = self.open_lock(STATE_LOCK_FILE)?;
        lock.lock().map_err(|source| StoreError::Io {
            path: self.root.join(STATE_LOCK_FILE),
            source,
        })?;
        Ok(lock)
    }

    fn read_state(&self) -> Result<State, StoreError> {
        let path = self.root.join(STATE_FILE);
        let bytes = match fs::read(&path) {
            Ok(bytes) => bytes,
            Err(error) if error.kind() == io::ErrorKind::NotFound => return Ok(State::new()),
            Err(source) => return Err(StoreError::Io { path, source }),
        };
        match serde_json::from_slice::<State>(&bytes) {
            Ok(state) if state.schema == STATE_SCHEMA => Ok(state),
            Ok(state) => {
                tracing::warn!(
                    path = %path.display(),
                    schema = state.schema,
                    "the record of Roblox starts has another schema; Eclipse starts a new one"
                );
                Ok(State::new())
            }
            Err(error) => {
                tracing::warn!(
                    path = %path.display(),
                    %error,
                    "cannot read the record of Roblox starts; Eclipse starts a new one"
                );
                Ok(State::new())
            }
        }
    }

    fn write_state(
        &self,
        mut state: State,
        current: Option<VersionCode>,
    ) -> Result<(), StoreError> {
        let mut retained = BTreeMap::new();
        for (version, version_state) in std::mem::take(&mut state.versions) {
            if path_exists(&self.version_dir(version))? {
                retained.insert(version, version_state);
            }
        }
        state.versions = retained;
        if let Some(current) = current {
            state
                .rejected_versions
                .retain(|rejection| rejection.version.version_code > current);
        }
        state.schema = STATE_SCHEMA;
        replace_json(&self.root, STATE_FILE, &state)
    }

    fn take_attempt(&self) -> Result<Option<VersionCode>, StoreError> {
        let path = self.root.join(ATTEMPT_FILE);
        let bytes = match fs::read(&path) {
            Ok(bytes) => bytes,
            Err(error) if error.kind() == io::ErrorKind::NotFound => return Ok(None),
            Err(source) => return Err(StoreError::Io { path, source }),
        };
        self.remove_attempt()?;
        match serde_json::from_slice::<AttemptRecord>(&bytes) {
            Ok(record) => Ok(Some(record.version_code)),
            Err(error) => {
                tracing::warn!(
                    path = %path.display(),
                    %error,
                    "cannot read the record of the last Roblox start, so it is not counted"
                );
                Ok(None)
            }
        }
    }

    fn remove_attempt(&self) -> Result<(), StoreError> {
        let path = self.root.join(ATTEMPT_FILE);
        match fs::remove_file(&path) {
            Ok(()) => sync_dir(&self.root),
            Err(error) if error.kind() == io::ErrorKind::NotFound => Ok(()),
            Err(source) => Err(StoreError::Io { path, source }),
        }
    }

    fn activate(&self, installed: &InstalledVersion) -> Result<(), StoreError> {
        let _state = self.lock_state()?;
        let previous = self
            .readable_current()?
            .map(|previous| previous.version_code);
        if previous == Some(installed.version_code) {
            return replace_json(&self.root, CURRENT_FILE, installed);
        }
        let state = self.read_state()?;
        let mut started = state.clone();
        started
            .rejected_versions
            .retain(|rejection| rejection.version.version_code != installed.version_code);
        if !started.version(installed.version_code).started() {
            started.versions.remove(&installed.version_code);
        }
        started.fallback_from = None;
        if started != state {
            self.write_state(started, Some(installed.version_code))?;
        }
        replace_json(&self.root, CURRENT_FILE, installed)
    }

    pub fn prune(&self) -> Result<(), StoreError> {
        let install = self.open_lock(LOCK_FILE)?;
        match install.try_lock() {
            Ok(()) => {}
            Err(std::fs::TryLockError::WouldBlock) => {
                tracing::info!(
                    "another Eclipse is installing Roblox, so the versions Eclipse no longer \
                     keeps are removed later"
                );
                return Ok(());
            }
            Err(std::fs::TryLockError::Error(source)) => {
                return Err(StoreError::Io {
                    path: self.root.join(LOCK_FILE),
                    source,
                })
            }
        }
        self.remove_unkept()
    }

    fn remove_unkept(&self) -> Result<(), StoreError> {
        let _state = self.lock_state()?;
        let Some(current) = self.current()? else {
            return Ok(());
        };
        let mut state = self.read_state()?;
        let kept = state.kept(current.version_code);
        let io_error = |path: &Path| {
            let path = path.to_path_buf();
            move |source| StoreError::Io { path, source }
        };
        for entry in fs::read_dir(&self.root).map_err(io_error(&self.root))? {
            let entry = entry.map_err(io_error(&self.root))?;
            let path = entry.path();
            let file_type = entry.file_type().map_err(io_error(&path))?;
            let name = entry.file_name();
            let Some(name) = name.to_str() else {
                continue;
            };
            if file_type.is_dir() {
                let unkept = name.ends_with(PARTIAL_SUFFIX)
                    || name
                        .parse::<u32>()
                        .is_ok_and(|code| !kept.contains_key(&VersionCode(code)));
                if unkept {
                    fs::remove_dir_all(&path).map_err(io_error(&path))?;
                }
            } else if temp_file::is_abandoned(&entry).map_err(io_error(&path))? {
                fs::remove_file(&path).map_err(io_error(&path))?;
            }
        }
        let recorded = state.versions.len();
        state
            .versions
            .retain(|version, _| kept.contains_key(version));
        if state.versions.len() == recorded {
            return Ok(());
        }
        self.write_state(state, Some(current.version_code))
    }
}

pub struct InstallLock<'a> {
    store: &'a Store,
    _lock: File,
}

impl InstallLock<'_> {
    pub fn prune(&self) -> Result<(), StoreError> {
        self.store.remove_unkept()
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
            .filter(|name| name != LOCK_FILE && name != STATE_LOCK_FILE)
            .collect();
        names.sort();
        names
    }

    #[test]
    fn activation_switches_current_and_pruning_removes_what_is_not_kept() {
        let root = temp_root("activate");
        let store = Store::at(root.clone());
        store.prune().unwrap();
        assert!(
            entries(&root).is_empty(),
            "nothing installed, nothing pruned"
        );
        for dir in ["3054", "3055", "3056", "3057.partial", "notes"] {
            fs::create_dir_all(root.join(dir)).unwrap();
        }
        fs::write(root.join("3053"), b"a file, not an install").unwrap();
        let abandoned = format!("{CURRENT_FILE}.00000000000000aa.tmp");
        fs::write(root.join(&abandoned), b"{").unwrap();
        File::options()
            .write(true)
            .open(root.join(&abandoned))
            .unwrap()
            .set_modified(SystemTime::now() - Duration::from_secs(2 * 60 * 60))
            .unwrap();

        store.activate(&version(3055)).unwrap();
        assert_eq!(store.current().unwrap(), Some(version(3055)));
        assert_eq!(
            entries(&root),
            [
                "3053",
                "3054",
                "3055",
                "3056",
                "3057.partial",
                CURRENT_FILE,
                &abandoned,
                "notes"
            ],
            "activation removes nothing"
        );

        store.prune().unwrap();
        assert_eq!(entries(&root), ["3053", "3055", CURRENT_FILE, "notes"]);

        fs::create_dir_all(root.join("3056")).unwrap();
        store.activate(&version(3056)).unwrap();
        store.prune().unwrap();
        assert_eq!(entries(&root), ["3053", "3056", CURRENT_FILE, "notes"]);

        fs::create_dir_all(root.join("3040")).unwrap();
        store.activate(&version(3056)).unwrap();
        assert!(
            root.join("3040").exists(),
            "re-activating the current version must not prune"
        );
        assert_eq!(store.current().unwrap(), Some(version(3056)));
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

        fs::write(
            root.join(LAST_CHECK_FILE),
            b"{\"checked_at_unix\": 1800000000}",
        )
        .unwrap();
        assert_eq!(
            store.last_check().unwrap(),
            Some(checked),
            "a record from before failed checks were recorded is a completed check"
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
    fn a_failed_check_is_retried_after_thirty_minutes() {
        let now = UNIX_EPOCH + Duration::from_secs(1_800_000_000);
        let installed = Some(VersionCode(3170));
        let failed = |at| UpdateCheck {
            at,
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
                                        outcome: if writer % 2 == 1 {
                                            CheckOutcome::Failed
                                        } else {
                                            CheckOutcome::Completed
                                        },
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
        let installed = InstalledVersion::from(&committed);
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
        let staging = store.begin(&StatusSink::terminal()).unwrap();
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
    fn a_version_that_cannot_be_removed_is_named_and_the_kept_one_is_untouched() {
        use std::os::unix::fs::PermissionsExt as _;

        struct Cleanup<'a> {
            read_only: &'a Path,
            root: &'a Path,
        }

        impl Drop for Cleanup<'_> {
            fn drop(&mut self) {
                fs::set_permissions(self.read_only, fs::Permissions::from_mode(0o755)).ok();
                fs::remove_dir_all(self.root).ok();
            }
        }

        let root = temp_root("prune-leftover");
        let locked = root.join("123").join("locked");
        let _cleanup = Cleanup {
            read_only: &locked,
            root: &root,
        };
        let store = Store::at(root.clone());
        install_fixture(&root, 3170);
        store.activate(&version(3170)).unwrap();
        fs::create_dir_all(&locked).unwrap();
        fs::write(locked.join("file"), b"old").unwrap();
        fs::set_permissions(&locked, fs::Permissions::from_mode(0o555)).unwrap();

        let err = store.prune().unwrap_err();
        assert!(
            matches!(
                &err,
                StoreError::Io { path, source }
                    if *path == root.join("123")
                        && source.kind() == io::ErrorKind::PermissionDenied
            ),
            "{err:?}"
        );
        assert_eq!(store.current().unwrap(), Some(version(3170)));
        assert!(root.join("3170").join(BASE_APK).is_file());
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
        let official = history(&installed);
        let dir = root.join(installed.version_code().to_string());
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

    const ABORT_CHILD: &str = "ECLIPSE_TEST_STORE_ABORT_CHILD";

    fn install_fixture(root: &Path, code: u32) {
        let name = version(code).version_name.unwrap();
        let manifest = axml::fixture::Manifest {
            package: ROBLOX_PACKAGE,
            version_code: Some(code),
            version_name: Some(&name),
            split: None,
            launcher: Some(".Main"),
        }
        .encode();
        let dir = root.join(code.to_string());
        fs::create_dir_all(&dir).unwrap();
        fs::write(
            dir.join(BASE_APK),
            zip_of(&[("AndroidManifest.xml", &manifest)], stored()),
        )
        .unwrap();
    }

    fn proven_then_updated(tag: &str) -> (PathBuf, Store) {
        let root = temp_root(tag);
        let store = Store::at(root.clone());
        install_fixture(&root, 3170);
        store.activate(&version(3170)).unwrap();
        play(&store, 3170);
        install_fixture(&root, 3212);
        store.activate(&version(3212)).unwrap();
        (root, store)
    }

    fn show_first_frame(store: &Store, code: u32) {
        store.begin_attempt(VersionCode(code)).unwrap();
        store.record_first_frame(VersionCode(code)).unwrap();
    }

    fn play(store: &Store, code: u32) {
        show_first_frame(store, code);
        store.record_normal_close(VersionCode(code)).unwrap();
    }

    fn fail_start(store: &Store, code: u32) -> Option<BothFailed> {
        store.begin_attempt(VersionCode(code)).unwrap();
        store.settle_launch().unwrap()
    }

    fn fall_back(store: &Store) -> Option<FellBack> {
        store.fall_back_if_failing(crate::VERSION).unwrap()
    }

    fn state_of(store: &Store, code: u32) -> VersionState {
        store.read_state().unwrap().version(VersionCode(code))
    }

    fn candidate(code: u32, digest: u8) -> Candidate {
        Candidate {
            version: VersionCode(code),
            file: DeclaredFile::ApkComboBaseSha1([digest; SHA1_OUTPUT_LEN]),
        }
    }

    #[test]
    fn an_unproven_version_falls_back_after_two_failed_starts() {
        let (root, store) = proven_then_updated("fall-back");
        assert_eq!(fail_start(&store, 3212), None);
        assert_eq!(fall_back(&store), None, "one failed start is not enough");
        assert_eq!(
            state_of(&store, 3212),
            VersionState::Unproven { failed_starts: 1 }
        );
        assert_eq!(fail_start(&store, 3212), None);
        assert_eq!(
            fall_back(&store),
            Some(FellBack {
                failed: version(3212),
                using: version(3170),
            })
        );
        assert_eq!(store.current().unwrap(), Some(version(3170)));
        assert_eq!(
            store.read_state().unwrap().rejected_versions,
            [RejectedVersion {
                version: version(3212),
                eclipse_version: crate::VERSION.to_owned(),
                because: LeftBecause::FailedToStart,
            }]
        );
        let rejections = store.rejections(crate::VERSION).unwrap();
        assert_eq!(
            rejections.left,
            BTreeMap::from([(VersionCode(3212), LeftBecause::FailedToStart)])
        );
        assert_eq!(
            rejections.skipped_versions().collect::<Vec<_>>(),
            [(VersionCode(3212), LeftBecause::FailedToStart)]
        );
        assert!(root.join("3212").join(BASE_APK).is_file());
        assert!(!root.join(ATTEMPT_FILE).exists());
        fs::remove_dir_all(&root).ok();
    }

    #[test]
    fn a_user_close_before_the_first_frame_does_not_count() {
        let (root, store) = proven_then_updated("user-close");
        for _ in 0..3 {
            let attempt = store.begin_attempt(VersionCode(3212)).unwrap();
            assert!(root.join(ATTEMPT_FILE).is_file());
            attempt.closed_by_user().unwrap();
            assert_eq!(store.settle_launch().unwrap(), None);
        }
        assert_eq!(state_of(&store, 3212), UNPROVEN);
        assert_eq!(fall_back(&store), None);
        assert_eq!(store.current().unwrap(), Some(version(3212)));
        fs::remove_dir_all(&root).ok();
    }

    #[test]
    fn a_first_frame_proves_the_version_and_ends_the_fallback_probe() {
        let (root, store) = proven_then_updated("first-frame");
        store.begin_attempt(VersionCode(3212)).unwrap();
        store.record_first_frame(VersionCode(3212)).unwrap();
        assert!(!root.join(ATTEMPT_FILE).exists());
        assert_eq!(state_of(&store, 3212), VersionState::Proven);
        assert_eq!(store.settle_launch().unwrap(), None);
        assert_eq!(state_of(&store, 3212), VersionState::Proven);

        let (root, store) = proven_then_updated("fallback-probe");
        fail_start(&store, 3212);
        fail_start(&store, 3212);
        assert!(fall_back(&store).is_some());
        assert_eq!(
            store.proof_needed(VersionCode(3170)).unwrap(),
            Some(Proof::FirstFrame),
            "the kept version proves itself again while the fallback is probed"
        );
        store.begin_attempt(VersionCode(3170)).unwrap();
        store.record_first_frame(VersionCode(3170)).unwrap();
        let state = store.read_state().unwrap();
        assert_eq!(state.fallback_from, None);
        assert_eq!(
            state.version(VersionCode(3170)),
            VersionState::Played,
            "a first frame never forgets that the version was played"
        );
        assert_eq!(store.proof_needed(VersionCode(3170)).unwrap(), None);
        assert_eq!(store.settle_launch().unwrap(), None);
        assert_eq!(store.current().unwrap(), Some(version(3170)));
        fs::remove_dir_all(&root).ok();
    }

    #[test]
    fn a_failed_fallback_lifts_the_rejection_and_stops_switching() {
        let (root, store) = proven_then_updated("both-failed");
        fail_start(&store, 3212);
        fail_start(&store, 3212);
        assert!(fall_back(&store).is_some());
        assert_eq!(
            fail_start(&store, 3170),
            Some(BothFailed {
                newer: version(3212),
                older: version(3170),
            })
        );
        assert_eq!(store.current().unwrap(), Some(version(3212)));
        assert_eq!(
            store.rejections(crate::VERSION).unwrap(),
            Rejections::default()
        );
        assert_eq!(state_of(&store, 3212), VersionState::BothFailed);
        assert_eq!(state_of(&store, 3170), VersionState::Played);
        for _ in 0..2 {
            assert_eq!(fail_start(&store, 3212), None);
            assert_eq!(fall_back(&store), None);
        }
        assert_eq!(store.current().unwrap(), Some(version(3212)));
        assert_eq!(state_of(&store, 3212), VersionState::BothFailed);
        fs::remove_dir_all(&root).ok();
    }

    #[test]
    fn a_first_install_is_never_rejected() {
        let root = temp_root("first-install");
        let store = Store::at(root.clone());
        install_fixture(&root, 3212);
        store.activate(&version(3212)).unwrap();
        for _ in 0..3 {
            assert_eq!(fail_start(&store, 3212), None);
            assert_eq!(fall_back(&store), None);
        }
        assert_eq!(
            state_of(&store, 3212),
            VersionState::Unproven { failed_starts: 3 }
        );
        assert_eq!(
            store.rejections(crate::VERSION).unwrap(),
            Rejections::default()
        );
        assert_eq!(store.current().unwrap(), Some(version(3212)));
        fs::remove_dir_all(&root).ok();
    }

    #[test]
    fn crash_rejections_from_another_eclipse_version_are_ignored() {
        let (root, store) = proven_then_updated("other-eclipse");
        fail_start(&store, 3212);
        fail_start(&store, 3212);
        assert!(store.fall_back_if_failing("0.1.4").unwrap().is_some());
        let plan_with = |eclipse_version| {
            plan(
                candidate(3212, 0x40),
                Some(&version(3170)),
                Some(&version(3170)),
                &store.rejections(eclipse_version).unwrap(),
                CheckMode::Scheduled,
            )
        };
        assert_eq!(
            plan_with("0.1.4"),
            Plan::Rejected(Rejection::Left(LeftBecause::FailedToStart))
        );
        assert_eq!(plan_with("0.1.5"), Plan::Download);
        fs::remove_dir_all(&root).ok();
    }

    #[test]
    fn proven_versions_never_record_attempts() {
        let (root, store) = proven_then_updated("proven");
        let proof = |code| store.proof_needed(VersionCode(code)).unwrap();
        assert_eq!(proof(3170), None);
        assert_eq!(proof(3212), Some(Proof::FirstFrame));
        show_first_frame(&store, 3212);
        assert_eq!(proof(3212), Some(Proof::NormalClose));
        store.record_normal_close(VersionCode(3212)).unwrap();
        assert_eq!(proof(3212), None);
        fs::remove_dir_all(&root).ok();
    }

    fn versions_on_disk(root: &Path) -> Vec<u32> {
        let mut versions: Vec<u32> = entries(root)
            .iter()
            .filter_map(|name| name.parse().ok())
            .collect();
        versions.sort_unstable();
        versions
    }

    #[test]
    fn retention_keeps_the_newest_proven_version_while_current_is_unproven() {
        let (root, store) = proven_then_updated("keeps-proven");
        assert_eq!(fail_start(&store, 3212), None);
        store.prune().unwrap();
        assert_eq!(versions_on_disk(&root), [3170, 3212]);

        install_fixture(&root, 3250);
        store.activate(&version(3250)).unwrap();
        store.prune().unwrap();
        assert_eq!(
            versions_on_disk(&root),
            [3170, 3250],
            "the last version that started survives two updates without a launch"
        );
        let state = store.read_state().unwrap();
        assert_eq!(
            state.versions,
            BTreeMap::from([(VersionCode(3170), VersionState::Played)]),
            "a removed version leaves no state behind"
        );
        fs::remove_dir_all(&root).ok();
    }

    #[test]
    fn retention_keeps_the_previous_version_until_the_new_one_closes_normally() {
        let (root, store) = proven_then_updated("keeps-current");
        store.prune().unwrap();
        assert_eq!(versions_on_disk(&root), [3170, 3212]);
        show_first_frame(&store, 3212);
        store.prune().unwrap();
        assert_eq!(
            versions_on_disk(&root),
            [3170, 3212],
            "a version that may crash after its first frame keeps the one before it"
        );
        store.record_normal_close(VersionCode(3212)).unwrap();
        store.prune().unwrap();
        assert_eq!(versions_on_disk(&root), [3212]);
        assert_eq!(
            store.read_state().unwrap().versions,
            BTreeMap::from([(VersionCode(3212), VersionState::Played)])
        );
        fs::remove_dir_all(&root).ok();
    }

    #[test]
    fn the_kept_version_is_the_newest_one_that_was_played() {
        let (root, store) = proven_then_updated("keeps-played");
        show_first_frame(&store, 3212);
        install_fixture(&root, 3250);
        store.activate(&version(3250)).unwrap();
        store.prune().unwrap();
        assert_eq!(
            versions_on_disk(&root),
            [3170, 3250],
            "a version that showed a frame but never closed normally is not kept"
        );
        fail_start(&store, 3250);
        fail_start(&store, 3250);
        assert_eq!(
            fall_back(&store),
            Some(FellBack {
                failed: version(3250),
                using: version(3170),
            })
        );
        fs::remove_dir_all(&root).ok();
    }

    #[test]
    fn rollback_returns_to_the_kept_version_and_scheduled_checks_skip_the_one_left() {
        let (root, store) = proven_then_updated("rollback");
        show_first_frame(&store, 3212);
        assert_eq!(
            store.roll_back(crate::VERSION).unwrap(),
            Rollback::RolledBack(RolledBack {
                from: version(3212),
                to: version(3170),
            })
        );
        assert_eq!(store.current().unwrap(), Some(version(3170)));
        assert_eq!(
            plan(
                candidate(3212, 0x40),
                Some(&version(3170)),
                Some(&version(3170)),
                &store.rejections(crate::VERSION).unwrap(),
                CheckMode::Scheduled,
            ),
            Plan::Rejected(Rejection::Left(LeftBecause::RolledBack))
        );
        assert_eq!(
            store.roll_back(crate::VERSION).unwrap(),
            Rollback::NothingKept,
            "the version left behind is never gone back to"
        );
        assert_eq!(store.current().unwrap(), Some(version(3170)));
        assert_eq!(store.proof_needed(VersionCode(3170)).unwrap(), None);
        store.prune().unwrap();
        assert_eq!(
            versions_on_disk(&root),
            [3170],
            "nothing switches back to the version left behind, so it is not kept"
        );
        assert_eq!(
            store.rejections(crate::VERSION).unwrap().left,
            BTreeMap::from([(VersionCode(3212), LeftBecause::RolledBack)]),
            "the version left behind stays skipped after its files are gone"
        );
        fs::remove_dir_all(&root).ok();
    }

    #[test]
    fn a_failed_start_after_a_rollback_stays_on_the_chosen_version() {
        let (root, store) = proven_then_updated("rollback-failed-start");
        show_first_frame(&store, 3212);
        assert!(matches!(
            store.roll_back(crate::VERSION).unwrap(),
            Rollback::RolledBack(_)
        ));
        assert_eq!(fail_start(&store, 3170), None);
        assert_eq!(fall_back(&store), None);
        assert_eq!(store.current().unwrap(), Some(version(3170)));
        assert_eq!(state_of(&store, 3170), VersionState::Played);
        assert_eq!(
            store.rejections(crate::VERSION).unwrap().left,
            BTreeMap::from([(VersionCode(3212), LeftBecause::RolledBack)])
        );
        fs::remove_dir_all(&root).ok();
    }

    #[test]
    fn rollback_needs_an_installed_and_a_kept_version() {
        let root = temp_root("rollback-nothing");
        let store = Store::at(root.clone());
        assert_eq!(
            store.roll_back(crate::VERSION).unwrap(),
            Rollback::NotInstalled
        );
        fs::remove_dir_all(&root).ok();

        let (root, store) = proven_then_updated("rollback-played");
        play(&store, 3212);
        store.prune().unwrap();
        assert_eq!(
            store.roll_back(crate::VERSION).unwrap(),
            Rollback::NothingKept
        );
        assert_eq!(store.current().unwrap(), Some(version(3212)));
        assert_eq!(
            store.rejections(crate::VERSION).unwrap(),
            Rejections::default()
        );
        fs::remove_dir_all(&root).ok();
    }

    #[test]
    fn the_rejected_version_is_kept_only_during_the_fallback_probe() {
        let (root, store) = proven_then_updated("probe-retention");
        fail_start(&store, 3212);
        fail_start(&store, 3212);
        assert!(fall_back(&store).is_some());
        store.prune().unwrap();
        assert_eq!(
            versions_on_disk(&root),
            [3170, 3212],
            "the failed version stays while the kept one is probed"
        );
        show_first_frame(&store, 3170);
        store.prune().unwrap();
        assert_eq!(versions_on_disk(&root), [3170]);
        assert_eq!(
            store.rejections(crate::VERSION).unwrap().left,
            BTreeMap::from([(VersionCode(3212), LeftBecause::FailedToStart)]),
            "the failed version stays rejected after its files are gone"
        );
        fs::remove_dir_all(&root).ok();

        let (root, store) = proven_then_updated("both-failed-retention");
        fail_start(&store, 3212);
        fail_start(&store, 3212);
        assert!(fall_back(&store).is_some());
        assert!(fail_start(&store, 3170).is_some());
        store.prune().unwrap();
        assert_eq!(
            versions_on_disk(&root),
            [3170, 3212],
            "after both failed, the proven version is kept until the newer one starts"
        );
        show_first_frame(&store, 3212);
        store.prune().unwrap();
        assert_eq!(
            versions_on_disk(&root),
            [3170, 3212],
            "after both failed, a first frame alone does not drop the version that played"
        );
        store.record_normal_close(VersionCode(3212)).unwrap();
        store.prune().unwrap();
        assert_eq!(versions_on_disk(&root), [3212]);
        fs::remove_dir_all(&root).ok();
    }

    #[test]
    fn pruning_waits_for_another_install_to_finish() {
        let (root, store) = proven_then_updated("prune-waits-install");
        play(&store, 3212);
        let installing = store.begin(&StatusSink::terminal()).unwrap();
        store.prune().unwrap();
        assert_eq!(versions_on_disk(&root), [3170, 3212]);
        drop(installing);
        store.prune().unwrap();
        assert_eq!(versions_on_disk(&root), [3212]);
        fs::remove_dir_all(&root).ok();
    }

    #[test]
    fn a_held_install_lock_prunes_what_a_waiting_prune_skips() {
        let (root, store) = proven_then_updated("prune-holding-install");
        play(&store, 3212);
        let install = store.lock_install(&StatusSink::terminal()).unwrap();
        store.prune().unwrap();
        assert_eq!(versions_on_disk(&root), [3170, 3212]);
        install.prune().unwrap();
        assert_eq!(versions_on_disk(&root), [3212]);
        fs::remove_dir_all(&root).ok();
    }

    fn listed(store: &Store) -> Vec<(u32, Option<String>, VersionRole, VersionState)> {
        store
            .versions()
            .unwrap()
            .into_iter()
            .map(|stored| {
                assert_eq!(
                    stored.dir,
                    store.root().join(stored.version.version_code.to_string())
                );
                (
                    stored.version.version_code.0,
                    stored.version.version_name,
                    stored.role,
                    stored.state,
                )
            })
            .collect()
    }

    #[test]
    fn stored_versions_carry_the_role_retention_gives_them() {
        let empty = temp_root("stored-missing");
        assert_eq!(listed(&Store::at(empty.join(STORE_DIR))), []);
        fs::remove_dir_all(&empty).ok();

        let (root, store) = proven_then_updated("stored-roles");
        fs::create_dir_all(root.join("3000")).unwrap();
        fs::write(root.join("3100"), b"not a version directory").unwrap();
        fs::create_dir_all(root.join(STAGING_DIR)).unwrap();
        let name = |code: u32| version(code).version_name;
        assert_eq!(
            listed(&store),
            [
                (3212, name(3212), VersionRole::Current, UNPROVEN),
                (
                    3170,
                    name(3170),
                    VersionRole::Fallback,
                    VersionState::Played
                ),
                (3000, None, VersionRole::Unkept, UNPROVEN),
            ]
        );

        fail_start(&store, 3212);
        fail_start(&store, 3212);
        assert!(fall_back(&store).is_some());
        assert_eq!(
            listed(&store),
            [
                (
                    3212,
                    name(3212),
                    VersionRole::Failed,
                    VersionState::Unproven { failed_starts: 2 }
                ),
                (3170, name(3170), VersionRole::Current, VersionState::Played),
                (3000, None, VersionRole::Unkept, UNPROVEN),
            ]
        );
        fs::remove_dir_all(&root).ok();
    }

    #[test]
    fn only_a_newer_release_is_downloaded_and_roblox_is_never_downgraded() {
        let offered = candidate(3170, 0x40);
        let none = Rejections::default();
        let plan_for = |verified: Option<u32>, recorded: Option<u32>| {
            plan(
                offered,
                verified.map(version).as_ref(),
                recorded.map(version).as_ref(),
                &none,
                CheckMode::Scheduled,
            )
        };
        assert_eq!(plan_for(None, None), Plan::Download);
        assert_eq!(plan_for(Some(3056), Some(3056)), Plan::Download);
        for code in [3170, 3200] {
            assert_eq!(
                plan_for(Some(code), Some(code)),
                Plan::UpToDate(version(code))
            );
        }
        assert_eq!(
            plan_for(None, Some(3170)),
            Plan::Download,
            "a damaged install of the offered release is repaired"
        );
        assert_eq!(plan_for(None, Some(3200)), Plan::Older(version(3200)));
    }

    #[test]
    fn scheduled_checks_skip_rejections_and_explicit_ones_clear_them() {
        let offered = candidate(3212, 0x40);
        let plan_with = |rejections: &Rejections, installed: u32, mode| {
            plan(
                offered,
                Some(&version(installed)),
                Some(&version(installed)),
                rejections,
                mode,
            )
        };
        let left = |because| Rejections {
            left: BTreeMap::from([(VersionCode(3212), because)]),
            files: Vec::new(),
        };
        let crashed = left(LeftBecause::FailedToStart);
        let rolled_back = left(LeftBecause::RolledBack);
        let file = Rejections {
            left: BTreeMap::new(),
            files: vec![offered.file],
        };
        assert_eq!(
            plan_with(&crashed, 3170, CheckMode::Scheduled),
            Plan::Rejected(Rejection::Left(LeftBecause::FailedToStart))
        );
        assert_eq!(
            plan_with(&rolled_back, 3170, CheckMode::Scheduled),
            Plan::Rejected(Rejection::Left(LeftBecause::RolledBack))
        );
        assert_eq!(
            plan_with(&file, 3170, CheckMode::Scheduled),
            Plan::Rejected(Rejection::FailedVerification)
        );
        for rejections in [&crashed, &rolled_back, &file] {
            assert_eq!(
                plan_with(rejections, 3170, CheckMode::Explicit),
                Plan::Download
            );
            assert_eq!(
                plan_with(rejections, 3212, CheckMode::Scheduled),
                Plan::UpToDate(version(3212)),
                "an installed release is up to date whatever was rejected"
            );
        }
        let unrelated = Rejections {
            left: BTreeMap::from([(VersionCode(3120), LeftBecause::FailedToStart)]),
            files: vec![candidate(3212, 0x41).file],
        };
        assert_eq!(
            plan_with(&unrelated, 3170, CheckMode::Scheduled),
            Plan::Download
        );

        let (root, store) = proven_then_updated("explicit-clears");
        fail_start(&store, 3212);
        fail_start(&store, 3212);
        assert!(fall_back(&store).is_some());
        store.activate(&version(3212)).unwrap();
        assert_eq!(
            store.rejections(crate::VERSION).unwrap(),
            Rejections::default()
        );
        assert_eq!(state_of(&store, 3212), UNPROVEN);
        assert_eq!(store.read_state().unwrap().fallback_from, None);
        assert_eq!(store.current().unwrap(), Some(version(3212)));
        fs::remove_dir_all(&root).ok();
    }

    #[test]
    fn file_rejections_keep_the_newest_eight() {
        let root = temp_root("file-rejections");
        let store = Store::at(root.clone());
        for digest in 0..10 {
            store
                .reject_file(DeclaredFile::ApkComboBaseSha1([digest; SHA1_OUTPUT_LEN]))
                .unwrap();
        }
        store
            .reject_file(DeclaredFile::ApkComboBaseSha1([5; SHA1_OUTPUT_LEN]))
            .unwrap();
        let kept: Vec<DeclaredFile> = [2, 3, 4, 6, 7, 8, 9, 5]
            .into_iter()
            .map(|digest| DeclaredFile::ApkComboBaseSha1([digest; SHA1_OUTPUT_LEN]))
            .collect();
        assert_eq!(store.rejections(crate::VERSION).unwrap().files, kept);
        fs::remove_dir_all(&root).ok();
    }

    #[test]
    fn old_last_check_files_with_a_rejected_field_still_parse() {
        let root = temp_root("old-last-check");
        let store = Store::at(root.clone());
        let at = UNIX_EPOCH + Duration::from_secs(1_800_000_000);
        for rejected in [
            serde_json::json!({"version_code": 3171, "base_sha1": vec![0x41u8; 20]}),
            serde_json::Value::Null,
        ] {
            let record = serde_json::json!({
                "checked_at_unix": 1_800_000_000u64,
                "rejected": rejected,
                "outcome": "failed",
            });
            fs::write(
                root.join(LAST_CHECK_FILE),
                serde_json::to_vec(&record).unwrap(),
            )
            .unwrap();
            assert_eq!(
                store.last_check().unwrap(),
                Some(UpdateCheck {
                    at,
                    outcome: CheckOutcome::Failed,
                })
            );
        }
        fs::remove_dir_all(&root).ok();
    }

    #[test]
    fn a_process_that_aborts_after_boot_counts_as_a_failed_start() {
        if let Some(root) = std::env::var_os(ABORT_CHILD) {
            rustix::process::set_dumpable_behavior(rustix::process::DumpableBehavior::NotDumpable)
                .unwrap();
            let _attempt = Store::at(PathBuf::from(root))
                .begin_attempt(VersionCode(3212))
                .unwrap();
            std::process::abort();
        }
        use std::os::unix::process::ExitStatusExt as _;

        let (root, store) = proven_then_updated("abort");
        let child = crate::bounded_child::output(
            std::process::Command::new(std::env::current_exe().unwrap())
                .args([
                    "--exact",
                    "apk::store::tests::a_process_that_aborts_after_boot_counts_as_a_failed_start",
                ])
                .env(ABORT_CHILD, &root),
            Duration::from_secs(60),
        );
        assert_eq!(
            child.status.signal(),
            Some(libc::SIGABRT),
            "{}",
            String::from_utf8_lossy(&child.stderr)
        );
        assert_eq!(store.settle_launch().unwrap(), None);
        assert_eq!(
            state_of(&store, 3212),
            VersionState::Unproven { failed_starts: 1 }
        );
        assert!(!root.join(ATTEMPT_FILE).exists());
        fs::remove_dir_all(&root).ok();
    }
}
