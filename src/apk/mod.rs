#![forbid(unsafe_code)]

pub mod arsc;
pub mod axml;
pub mod fetch;

use std::collections::BTreeMap;
use std::fmt;
use std::fmt::Write as _;
use std::fs::File;
use std::io::{self, BufReader, Read};
use std::path::{Path, PathBuf};

use crc32fast::Hasher as Crc32;
use sha2::{Digest, Sha256};
use zip::{CompressionMethod, ZipArchive};

use axml::AxmlError;

const MANIFEST_ENTRY: &str = "AndroidManifest.xml";

const ENGINE_LIB: &str = "libroblox.so";

const TARGET_ABI: &str = "x86_64";

const READ_ENTRY_PREALLOC_CAP: u64 = 8 * 1024 * 1024;

const EXTRACTED_ENTRY_HASH_BUFFER_SIZE: usize = 64 * 1024;

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

pub struct Apk {
    path: PathBuf,
    archive: ZipArchive<BufReader<File>>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Manifest {
    pub package: String,

    pub launcher_activity: String,

    pub min_sdk: Option<u32>,

    pub target_sdk: Option<u32>,

    pub large_heap: bool,
}

#[derive(Debug, Clone, PartialEq, Eq, PartialOrd, Ord)]
pub struct NativeAbi {
    pub name: String,

    pub has_engine: bool,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct X8664Engine {
    pub entry: String,

    pub size: u64,

    pub stored: bool,
}

impl Apk {
    pub fn open(path: &Path) -> Result<Self, ApkError> {
        let file = File::open(path)?;
        let archive = ZipArchive::new(BufReader::new(file))?;
        Ok(Self {
            path: path.to_path_buf(),
            archive,
        })
    }

    pub fn manifest(&mut self) -> Result<Manifest, ApkError> {
        let bytes = self.read_entry(MANIFEST_ENTRY)?;
        let parsed = axml::read_manifest(&bytes)?;
        Ok(Manifest {
            package: parsed.package,
            launcher_activity: parsed.launcher_activity,
            min_sdk: parsed.min_sdk,
            target_sdk: parsed.target_sdk,
            large_heap: parsed.large_heap,
        })
    }

    pub fn native_abis(&self) -> Vec<NativeAbi> {
        let mut abis: BTreeMap<String, bool> = BTreeMap::new();
        for name in self.archive.file_names() {
            let Some(rest) = name.strip_prefix("lib/") else {
                continue;
            };
            let Some((abi, file)) = rest.split_once('/') else {
                continue;
            };
            if abi.is_empty() {
                continue;
            }
            let has_engine = abis.entry(abi.to_owned()).or_insert(false);
            if file == ENGINE_LIB {
                *has_engine = true;
            }
        }
        abis.into_iter()
            .map(|(name, has_engine)| NativeAbi { name, has_engine })
            .collect()
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

        if let Some(replacement) = crate::system_cursor::replacement_apk_entry(name) {
            return Ok(replacement.to_vec());
        }

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
        Ok(EntrySpan {
            data_start: entry.data_start(),
            uncompressed_size: entry.size(),
            stored: entry.compression() == CompressionMethod::Stored,
        })
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct EntrySpan {
    pub data_start: u64,

    pub uncompressed_size: u64,

    pub stored: bool,
}

pub fn verify_integrity(path: &Path, expected_hex: &str) -> Result<(), ApkError> {
    if expected_hex.len() != 64 || !expected_hex.bytes().all(|b| b.is_ascii_hexdigit()) {
        return Err(ApkError::InvalidDigest(expected_hex.to_owned()));
    }
    let actual = sha256_hex(path)?;
    if actual.eq_ignore_ascii_case(expected_hex) {
        Ok(())
    } else {
        Err(ApkError::Integrity {
            expected: expected_hex.to_ascii_lowercase(),
            actual,
        })
    }
}

fn sha256_hex(path: &Path) -> Result<String, ApkError> {
    let mut file = File::open(path)?;
    let mut hasher = Sha256::new();

    io::copy(&mut file, &mut hasher)?;
    let digest = hasher.finalize();
    let mut hex = String::with_capacity(64);
    for byte in digest {
        let _ = write!(hex, "{byte:02x}");
    }
    Ok(hex)
}

#[derive(Debug)]
pub enum ApkError {
    Io(io::Error),

    Zip(zip::result::ZipError),

    Axml(AxmlError),

    EntryMissing(String),

    EngineMissing,

    InvalidDigest(String),

    Integrity { expected: String, actual: String },
}

impl fmt::Display for ApkError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Io(e) => write!(f, "APK file I/O error: {e}"),
            Self::Zip(e) => write!(f, "APK zip error: {e}"),
            Self::Axml(e) => write!(f, "AndroidManifest.xml parse error: {e}"),
            Self::EntryMissing(name) => write!(f, "APK is missing required entry: {name}"),
            Self::EngineMissing => {
                write!(
                    f,
                    "APK has no x86_64 engine library (lib/{TARGET_ABI}/{ENGINE_LIB})"
                )
            }
            Self::InvalidDigest(s) => {
                write!(f, "expected digest is not 64 hex characters: {s:?}")
            }
            Self::Integrity { expected, actual } => {
                write!(
                    f,
                    "APK integrity check failed: expected {expected}, got {actual}"
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
            Self::EntryMissing(_)
            | Self::EngineMissing
            | Self::InvalidDigest(_)
            | Self::Integrity { .. } => None,
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
    fn native_abis_detect_x86_64_with_engine() {
        let bytes = build_apk(&[
            ("lib/x86_64/libroblox.so", b"engine"),
            ("lib/arm64-v8a/libroblox.so", b"engine"),
            ("lib/x86_64/libother.so", b"other"),
            ("lib/armeabi-v7a/libfoo.so", b"foo"),
            ("classes.dex", b"dex"),
        ]);
        let (apk, path) = open_apk(&bytes, "abis");
        let abis = apk.native_abis();
        std::fs::remove_file(&path).ok();

        let names: Vec<&str> = abis.iter().map(|a| a.name.as_str()).collect();
        assert_eq!(names, vec!["arm64-v8a", "armeabi-v7a", "x86_64"]);

        assert!(abis.iter().find(|a| a.name == "x86_64").unwrap().has_engine);
        assert!(
            abis.iter()
                .find(|a| a.name == "arm64-v8a")
                .unwrap()
                .has_engine
        );
        assert!(
            !abis
                .iter()
                .find(|a| a.name == "armeabi-v7a")
                .unwrap()
                .has_engine
        );
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
    fn native_abis_negative_case_no_x86_64() {
        let bytes = build_apk(&[("lib/arm64-v8a/libroblox.so", b"engine")]);
        let (mut apk, path) = open_apk(&bytes, "armonly");
        let abis = apk.native_abis();
        assert!(!abis.iter().any(|a| a.name == TARGET_ABI));

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
    fn verify_integrity_matches_correct_digest() {
        let path = temp_file("sha-ok", b"abc");
        let expected = "ba7816bf8f01cfea414140de5dae2223b00361a396177a9cb410ff61f20015ad";
        let result = verify_integrity(&path, expected);
        std::fs::remove_file(&path).ok();
        result.expect("digest must match");
    }

    #[test]
    fn verify_integrity_is_case_insensitive() {
        let path = temp_file("sha-case", b"abc");
        let expected = "BA7816BF8F01CFEA414140DE5DAE2223B00361A396177A9CB410FF61F20015AD";
        let result = verify_integrity(&path, expected);
        std::fs::remove_file(&path).ok();
        result.expect("uppercase digest must still match");
    }

    #[test]
    fn verify_integrity_rejects_wrong_digest() {
        let path = temp_file("sha-bad", b"abc");
        let wrong = "0000000000000000000000000000000000000000000000000000000000000000";
        let err = verify_integrity(&path, wrong).expect_err("must mismatch");
        std::fs::remove_file(&path).ok();
        match err {
            ApkError::Integrity { expected, actual } => {
                assert_eq!(expected, wrong);
                assert_eq!(
                    actual,
                    "ba7816bf8f01cfea414140de5dae2223b00361a396177a9cb410ff61f20015ad"
                );
            }
            other => panic!("expected Integrity, got {other:?}"),
        }
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
    fn verify_integrity_rejects_malformed_expected() {
        let path = temp_file("sha-malformed", b"abc");

        let err = verify_integrity(&path, "not-a-digest").expect_err("must reject");
        std::fs::remove_file(&path).ok();
        assert!(matches!(err, ApkError::InvalidDigest(_)), "got {err:?}");
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
                let _ = apk.native_abis();
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
}
