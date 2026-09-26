#![forbid(unsafe_code)]

pub mod arsc;
pub mod axml;
pub mod cache;
mod file_reader;
pub mod play;
pub mod signature;
pub mod store;

use std::fmt;
use std::fs::File;
use std::io::{self, Read};
use std::os::fd::AsRawFd;
use std::path::{Path, PathBuf};
use std::sync::Arc;

use crc32fast::Hasher as Crc32;
use serde::{Deserialize, Serialize};
use zip::{CompressionMethod, ZipArchive};

use axml::AxmlError;
use file_reader::ApkFileReader;
use signature::{SignatureError, SigningCertificateHistory};

const MANIFEST_ENTRY: &str = "AndroidManifest.xml";

pub const ENGINE_LIB: &str = "libroblox.so";

pub const TARGET_ABI: &str = "x86_64";

pub const ROBLOX_PACKAGE: &str = "com.roblox.client";

pub const BASE_APK: &str = "base.apk";

pub const NATIVE_SPLIT_APK: &str = "split_config.x86_64.apk";

pub const DEV_APK_ENV: &str = "ECLIPSE_ROBLOX_APK";

const NATIVE_SPLIT_NAME: &str = "config.x86_64";

const MAX_APK_BYTES: u64 = 1024 * 1024 * 1024;

const READ_ENTRY_PREALLOC_CAP: u64 = 8 * 1024 * 1024;

const EXTRACTED_ENTRY_HASH_BUFFER_SIZE: usize = 64 * 1024;

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

fn extracted_entry_matches(path: &Path, size: u64, crc32: u32) -> io::Result<bool> {
    let mut file = match File::open(path) {
        Ok(file) => file,
        Err(error) if error.kind() == io::ErrorKind::NotFound => return Ok(false),
        Err(error) => return Err(error),
    };
    if file.metadata()?.len() != size {
        return Ok(false);
    }

    let mut hasher = Crc32::new();
    let mut buffer = [0_u8; EXTRACTED_ENTRY_HASH_BUFFER_SIZE];
    loop {
        let read = file.read(&mut buffer)?;
        if read == 0 {
            break;
        }
        hasher.update(&buffer[..read]);
    }
    Ok(hasher.finalize() == crc32)
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
        let file = Arc::new(File::open(path)?);
        let archive = ZipArchive::new(ApkFileReader::new(Arc::clone(&file)))?;
        Ok(Self {
            path: path.to_path_buf(),
            file,
            archive,
        })
    }

    pub fn manifest(&mut self) -> Result<Manifest, ApkError> {
        let bytes = self.read_entry(MANIFEST_ENTRY)?;
        let parsed = axml::read_manifest(&bytes)?;
        Ok(Manifest {
            package: parsed.package,
            launcher_activity: parsed.launcher_activity.ok_or(AxmlError::NoLauncher)?,
            min_sdk: parsed.min_sdk,
            target_sdk: parsed.target_sdk,
            large_heap: parsed.large_heap,
        })
    }

    pub fn package_info(&mut self) -> Result<PackageInfo, ApkError> {
        let bytes = self.read_entry(MANIFEST_ENTRY)?;
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
    ) -> Result<Vec<PathBuf>, ApkError> {
        let prefix = format!("lib/{abi}/");

        let names: Vec<String> = self
            .archive
            .file_names()
            .filter(|n| n.starts_with(&prefix) && n.ends_with(".so"))
            .map(str::to_owned)
            .collect();
        std::fs::create_dir_all(dest_dir)?;
        let mut extracted = Vec::with_capacity(names.len());
        for name in names {
            let base = name.rsplit('/').next().unwrap_or(name.as_str());
            let dest = dest_dir.join(base);
            let mut entry = self.archive.by_name(&name)?;

            if extracted_entry_matches(&dest, entry.size(), entry.crc32())? {
                extracted.push(dest);
                continue;
            }

            let tmp = dest_dir.join(format!("{base}.partial"));
            let mut out = File::create(&tmp)?;
            io::copy(&mut entry, &mut out)?;
            out.sync_all()?;
            drop(out);
            std::fs::rename(&tmp, &dest)?;
            extracted.push(dest);
        }
        Ok(extracted)
    }

    pub fn extract_assets(&mut self, dest_dir: &Path) -> Result<usize, ApkError> {
        const PREFIX: &str = "assets/";

        let names: Vec<String> = self
            .archive
            .file_names()
            .filter(|n| n.starts_with(PREFIX) && !n.ends_with('/'))
            .map(str::to_owned)
            .collect();
        std::fs::create_dir_all(dest_dir)?;
        let mut written = 0usize;
        for name in names {
            let mut entry = self.archive.by_name(&name)?;

            let Some(safe) = entry.enclosed_name() else {
                continue;
            };
            let Ok(rel) = safe.strip_prefix(PREFIX) else {
                continue;
            };
            if rel.as_os_str().is_empty() {
                continue;
            }
            let dest = dest_dir.join(rel);

            if extracted_entry_matches(&dest, entry.size(), entry.crc32())? {
                continue;
            }

            if let Some(parent) = dest.parent() {
                std::fs::create_dir_all(parent)?;
            }

            let file_name = dest.file_name().unwrap_or(rel.as_os_str());
            let tmp = dest.with_file_name(format!("{}.partial", file_name.to_string_lossy()));
            let mut out = File::create(&tmp)?;
            io::copy(&mut entry, &mut out)?;
            out.sync_all()?;
            drop(out);
            std::fs::rename(&tmp, &dest)?;
            written += 1;
        }
        Ok(written)
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

impl ApkSet {
    pub fn open(paths: ApkSetPaths) -> Result<Self, ApkSetError> {
        let signing_certificate_history = signature::verify_roblox_signing_history(&paths.base)
            .map_err(|source| ApkSetError::Signature {
                path: paths.base.clone(),
                source,
            })?;
        if let Some(split) = &paths.native_split {
            verify_signature(split)?;
        }
        Self::open_verified(paths, signing_certificate_history)
    }

    fn open_verified(
        paths: ApkSetPaths,
        signing_certificate_history: SigningCertificateHistory,
    ) -> Result<Self, ApkSetError> {
        let mut base = open_member(&paths.base)?;
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

        let native_split = match &paths.native_split {
            None => None,
            Some(path) => {
                let mut split = open_member(path)?;
                let info = member_info(&mut split, path)?;
                if info.split.as_deref() != Some(NATIVE_SPLIT_NAME) {
                    return Err(ApkSetError::NotNativeSplit {
                        path: path.clone(),
                        split: info.split,
                    });
                }
                if info.version_code != Some(version_code) {
                    return Err(ApkSetError::VersionMismatch {
                        path: path.clone(),
                        base: version_code,
                        split: info.version_code,
                    });
                }
                Some(split)
            }
        };

        let mut set = Self {
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
}

fn verify_signature(path: &Path) -> Result<(), ApkSetError> {
    signature::verify_roblox_signature(path).map_err(|source| ApkSetError::Signature {
        path: path.to_path_buf(),
        source,
    })
}

fn open_member(path: &Path) -> Result<Apk, ApkSetError> {
    Apk::open(path).map_err(|source| ApkSetError::Open {
        path: path.to_path_buf(),
        source,
    })
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
        }
    }
}

impl std::error::Error for ApkError {
    fn source(&self) -> Option<&(dyn std::error::Error + 'static)> {
        match self {
            Self::Io(e) => Some(e),
            Self::Zip(e) => Some(e),
            Self::Axml(e) => Some(e),
            Self::EntryMissing(_) | Self::EntryOffsetUnknown(_) | Self::EngineMissing => None,
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
            Self::Open { path, source } => write!(f, "{}: {source}", path.display()),
            Self::Signature { path, source } => write!(
                f,
                "{} is not the official, unmodified Roblox client ({source}); Eclipse only runs \
                 APKs signed by Roblox Corporation, so get them from Google Play with `eclipse \
                 update` or `eclipse install` the files Google Play delivered",
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
            | Self::WrongPackage { .. }
            | Self::BaseIsSplit { .. }
            | Self::NotNativeSplit { .. }
            | Self::MissingVersionCode(_)
            | Self::VersionMismatch { .. }
            | Self::EngineMissing { .. } => None,
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

        let extracted = apk.extract_native_libs("x86_64", &dir).expect("extract");
        let mut names: Vec<String> = extracted
            .iter()
            .map(|p| p.file_name().unwrap().to_string_lossy().into_owned())
            .collect();
        names.sort();
        assert_eq!(names, vec!["libother.so", "libroblox.so"]);
        assert_eq!(
            std::fs::read(dir.join("libroblox.so")).unwrap(),
            b"ENGINE-BYTES"
        );
        assert!(
            !dir.join("libfoo.so").exists(),
            "wrong-ABI lib must not extract"
        );
        assert!(
            !dir.join("classes.dex").exists(),
            "non-.so must not extract"
        );

        let again = apk.extract_native_libs("x86_64", &dir).expect("re-extract");
        assert_eq!(again.len(), 2);

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
            .extract_native_libs("x86_64", &dir)
            .expect("extract old APK");
        new_apk
            .extract_native_libs("x86_64", &dir)
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

        let count = apk.extract_assets(&dir).expect("extract assets");
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

        let again = apk.extract_assets(&dir).expect("re-extract assets");
        assert_eq!(again, 0, "idempotent re-extract writes 0 files");

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

        assert_eq!(old_apk.extract_assets(&dir).expect("extract old APK"), 1);
        assert_eq!(
            new_apk.extract_assets(&dir).expect("extract upgraded APK"),
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
        ApkSet::open_verified(
            paths,
            SigningCertificateHistory::unverified(vec![b"test certificate".to_vec()]),
        )
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
        std::fs::remove_dir_all(&dir).ok();
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
        assert!(err.to_string().contains("signed by Roblox Corporation"));
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
            &signature::verify_roblox_signing_history(&paths.base)
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
