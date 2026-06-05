//! APK parsing & verification (component-map B · 🟢 pure Rust).
//!
//! Opens a *local* Roblox Android APK (a zip), reads the binary `AndroidManifest.xml`
//! (package id, launcher Activity, sdk levels, `largeHeap`), enumerates the native ABIs
//! present and locates the x86-64 engine (`lib/x86_64/libroblox.so`), and verifies file
//! integrity with a streaming SHA-256.
//!
//! Network acquisition (fetch from a backend service), APK-signature (v2/v3) verification,
//! and full ARSC/resource decoding are intentionally **deferred** — see
//! `docs/dependency-plan.md`. This module never downloads or redistributes the APK
//! (project policy).
//!
//! ## Manifest field reliability (verified 2026-06-04 against Roblox v2.724.735)
//! `package`, the launcher activity, `android:minSdkVersion`, `android:targetSdkVersion`
//! and `android:largeHeap` all read cleanly from the binary manifest via Eclipse's own
//! [`axml`] reader (2026-06-04: replaced the panic-prone `axmldecoder` dependency — see
//! [`axml`]'s module docs). `min_sdk`/`target_sdk` are exposed as `Option<u32>` because
//! `<uses-sdk>` (or the attribute) may legitimately be absent in some APKs — we never
//! fabricate a value. The launcher is **resolved from the manifest** (the
//! `<activity>`/`<activity-alias>` whose `<intent-filter>` carries `action MAIN` +
//! `category LAUNCHER`), never hardcoded: for v2.724.735 that is
//! `com.roblox.client.startup.ActivitySplash`, *not* `ActivityNativeMain` (which has no
//! intent-filter). Measured `largeHeap` was `false`.

#![forbid(unsafe_code)]

mod axml;

use std::collections::BTreeMap;
use std::fmt;
use std::fmt::Write as _;
use std::fs::File;
use std::io::{self, BufReader, Read};
use std::path::{Path, PathBuf};

use sha2::{Digest, Sha256};
use zip::{CompressionMethod, ZipArchive};

use axml::AxmlError;

/// The binary manifest entry name, fixed by the Android APK format.
const MANIFEST_ENTRY: &str = "AndroidManifest.xml";
/// The native engine library Eclipse must run (the Roblox C++ engine).
const ENGINE_LIB: &str = "libroblox.so";
/// The ABI directory Eclipse targets (Android x86-64).
const TARGET_ABI: &str = "x86_64";
/// Upper bound on the speculative buffer pre-allocation in [`Apk::read_entry`], so an
/// attacker-controlled uncompressed-size field cannot force a large allocation up front.
/// Sized generously above any real manifest-class entry (8 MiB).
const READ_ENTRY_PREALLOC_CAP: u64 = 8 * 1024 * 1024;

/// An opened local APK (zip container) ready for parsing.
///
/// Holds the buffered zip archive plus the source path (used for the streaming integrity
/// hash, which re-reads the file rather than buffering it in memory).
pub struct Apk {
    path: PathBuf,
    archive: ZipArchive<BufReader<File>>,
}

/// Fields read from a binary `AndroidManifest.xml`.
///
/// `min_sdk`/`target_sdk` are `Option` because the manifest may omit `<uses-sdk>` — an
/// absent value is reported as `None`, never invented. `large_heap` defaults to `false`
/// (the Android default when `android:largeHeap` is absent).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Manifest {
    /// Application package id (root `<manifest>` `package` attribute).
    pub package: String,
    /// Fully-qualified launcher Activity class (resolved from the MAIN/LAUNCHER filter).
    pub launcher_activity: String,
    /// `android:minSdkVersion`, if declared.
    pub min_sdk: Option<u32>,
    /// `android:targetSdkVersion`, if declared.
    pub target_sdk: Option<u32>,
    /// `android:largeHeap` on `<application>` (`false` when absent).
    pub large_heap: bool,
}

/// A native ABI directory present under `lib/` in the APK.
#[derive(Debug, Clone, PartialEq, Eq, PartialOrd, Ord)]
pub struct NativeAbi {
    /// The ABI name (e.g. `x86_64`, `arm64-v8a`).
    pub name: String,
    /// Whether this ABI ships the Roblox engine library (`libroblox.so`).
    pub has_engine: bool,
}

/// The located x86-64 engine library inside the APK.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct X8664Engine {
    /// The full zip entry path (`lib/x86_64/libroblox.so`).
    pub entry: String,
    /// Uncompressed size in bytes.
    pub size: u64,
    /// `true` when stored uncompressed — required so ART can `mmap` it directly
    /// (2026-06-04: Roblox ships `libroblox.so` Stored for exactly this reason).
    pub stored: bool,
}

impl Apk {
    /// Open a local APK file for reading.
    ///
    /// `std::fs::File` is `Read + Seek`, which `ZipArchive` requires; the `BufReader`
    /// cuts syscalls during the central-directory scan.
    pub fn open(path: &Path) -> Result<Self, ApkError> {
        let file = File::open(path)?;
        let archive = ZipArchive::new(BufReader::new(file))?;
        Ok(Self {
            path: path.to_path_buf(),
            archive,
        })
    }

    /// Parse the binary `AndroidManifest.xml`.
    ///
    /// 2026-06-04: parsing is delegated to Eclipse's own [`axml`] reader, which is *total* —
    /// it returns a typed [`AxmlError`] for every malformed/hostile manifest and never panics
    /// (the previous `axmldecoder` dependency panicked on adversarial AXML, which aborts under
    /// the release `panic = "abort"` profile; see [`axml`]'s module docs). The manifest entry
    /// is read into memory first because the reader operates on a byte slice.
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

    /// List the native ABIs present under `lib/<abi>/`, flagging which carry the engine.
    ///
    /// ABIs are discovered by parsing entry names (detect-don't-assume — no fixed ABI set
    /// is assumed). `file_names()` borrows immutably, so this needs only `&self`.
    pub fn native_abis(&self) -> Vec<NativeAbi> {
        // BTreeMap keeps the output sorted+deduped while allocating each ABI name once.
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

    /// Locate the x86-64 engine library and report whether it is stored uncompressed.
    ///
    /// Returns [`ApkError::EngineMissing`] if `lib/x86_64/libroblox.so` is not present —
    /// an APK without it cannot run on Eclipse's target ABI.
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

    /// The path this APK was opened from.
    pub fn path(&self) -> &Path {
        &self.path
    }

    /// Read a named zip entry fully into memory.
    fn read_entry(&mut self, name: &str) -> Result<Vec<u8>, ApkError> {
        let mut entry = match self.archive.by_name(name) {
            Ok(e) => e,
            Err(zip::result::ZipError::FileNotFound) => {
                return Err(ApkError::EntryMissing(name.to_owned()));
            }
            Err(e) => return Err(ApkError::Zip(e)),
        };
        // size() is the uncompressed length from the (untrusted) central directory; cap the
        // speculative pre-allocation so a hostile entry declaring a huge size can't trigger a
        // large allocation before any bytes are read. The Vec still grows if the real data is
        // larger; manifest-class entries are well under this bound (2026-06-04).
        let cap = entry.size().min(READ_ENTRY_PREALLOC_CAP) as usize;
        let mut buf = Vec::with_capacity(cap);
        entry.read_to_end(&mut buf)?;
        Ok(buf)
    }
}

/// Verify a file's integrity against an expected SHA-256 digest (lowercase hex).
///
/// The comparison is case-insensitive (callers may pass an upper- or mixed-case digest).
/// Hashing streams the file through `io::copy` in fixed chunks, so memory stays constant
/// regardless of the APK size (~215 MB for Roblox). Returns
/// [`ApkError::InvalidDigest`] if `expected_hex` is not 64 hex characters, and
/// [`ApkError::Integrity`] on a mismatch.
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

/// Stream a file through SHA-256 and return the lowercase-hex digest.
fn sha256_hex(path: &Path) -> Result<String, ApkError> {
    let mut file = File::open(path)?;
    let mut hasher = Sha256::new();
    // io::copy streams the file through the hasher (Sha256: io::Write via sha2's `std`
    // feature); it never loads the whole file into memory.
    io::copy(&mut file, &mut hasher)?;
    let digest = hasher.finalize();
    let mut hex = String::with_capacity(64);
    for byte in digest {
        // Writing a formatted byte into a String is infallible.
        let _ = write!(hex, "{byte:02x}");
    }
    Ok(hex)
}

/// Errors from opening, parsing, or verifying an APK.
#[derive(Debug)]
pub enum ApkError {
    /// Opening or reading the APK file failed.
    Io(io::Error),
    /// The zip container could not be read.
    Zip(zip::result::ZipError),
    /// The binary `AndroidManifest.xml` could not be parsed. Carries Eclipse's own typed
    /// [`AxmlError`] (2026-06-04: the reader is total — every malformed/hostile manifest,
    /// including the missing-`<manifest>`/`package`/launcher cases, is an [`AxmlError`]
    /// variant, never a panic).
    Axml(AxmlError),
    /// A required zip entry was absent.
    EntryMissing(String),
    /// The x86-64 engine library (`lib/x86_64/libroblox.so`) was absent.
    EngineMissing,
    /// The expected digest was not a 64-character hex string.
    InvalidDigest(String),
    /// The file's SHA-256 did not match the expected digest.
    Integrity {
        /// The expected digest (lowercase hex).
        expected: String,
        /// The computed digest (lowercase hex).
        actual: String,
    },
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

    /// A minimal, checked-in binary `AndroidManifest.xml` (see `tests/fixtures/README.md`
    /// for provenance), so the manifest tests need no APK and no network. The bytes encode:
    /// package=com.example.app, `<uses-sdk>` min=26 target=35,
    /// `<application android:largeHeap="true">`, and a launcher activity `.SplashActivity`
    /// reached via a MAIN/LAUNCHER intent-filter, plus a second activity (`.MainActivity`)
    /// with no filter (negative case for the launcher resolver).
    const FIXTURE_MANIFEST: &[u8] = include_bytes!("../../tests/fixtures/AndroidManifest-min.bin");

    /// A structurally valid AXML whose single attribute carried an invalid resource
    /// value-type byte that made the old `axmldecoder` 0.3 dependency *panic* (not `Err`).
    /// 2026-06-04: it is the root-cause regression input — Eclipse's own [`axml`] reader must
    /// return a typed `ApkError::Axml(..)` for it instead of panicking/aborting.
    const FIXTURE_PANIC: &[u8] = include_bytes!("../../tests/fixtures/AndroidManifest-panic.bin");

    /// A valid AXML manifest with no `<uses-sdk>` and no `android:largeHeap` (see
    /// `tests/fixtures/README.md`), to pin the documented defaults: `min_sdk`/`target_sdk`
    /// are `None` (never fabricated) and `large_heap` is `false` when those are absent.
    const FIXTURE_ABSENT: &[u8] = include_bytes!("../../tests/fixtures/AndroidManifest-absent.bin");

    /// The same logical manifest as [`FIXTURE_MANIFEST`] but with a **UTF-16** string pool
    /// (UTF8_FLAG cleared), so the `decode_utf16` path — the one the *real* Roblox manifest
    /// uses — is exercised on both good input (here) and adversarial input (the totality
    /// fuzz). The other fixtures use a UTF-8 pool, so without this the production decoder
    /// would never be driven by the regression guard (2026-06-04).
    const FIXTURE_UTF16: &[u8] = include_bytes!("../../tests/fixtures/AndroidManifest-utf16.bin");

    /// Build an in-memory APK (zip) containing the given entries (name, bytes), all Stored.
    fn build_apk(entries: &[(&str, &[u8])]) -> Vec<u8> {
        let methoded: Vec<(&str, &[u8], CompressionMethod)> = entries
            .iter()
            .map(|(n, b)| (*n, *b, CompressionMethod::Stored))
            .collect();
        build_apk_methods(&methoded)
    }

    /// Build an in-memory APK with a per-entry compression method. Lets tests exercise both
    /// the real APK's mix (Deflated AndroidManifest.xml, Stored libroblox.so) and the Stored
    /// path that `build_apk` covers.
    fn build_apk_methods(entries: &[(&str, &[u8], CompressionMethod)]) -> Vec<u8> {
        let mut writer = ZipWriter::new(Cursor::new(Vec::new()));
        for (name, bytes, method) in entries {
            let opts = SimpleFileOptions::default().compression_method(*method);
            writer.start_file(*name, opts).expect("start_file");
            writer.write_all(bytes).expect("write_all");
        }
        writer.finish().expect("finish").into_inner()
    }

    /// Write bytes to a unique temp file and return its path (caller removes it).
    fn temp_file(tag: &str, bytes: &[u8]) -> PathBuf {
        // Unique per call via tag + thread id, under the OS temp dir (no hardcoded path).
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
        // Launcher is resolved from the MAIN/LAUNCHER filter, not the first activity.
        assert_eq!(manifest.launcher_activity, ".SplashActivity");
        assert_eq!(manifest.min_sdk, Some(26));
        assert_eq!(manifest.target_sdk, Some(35));
        assert!(manifest.large_heap);
    }

    #[test]
    fn manifest_defaults_when_uses_sdk_and_large_heap_absent() {
        // Guards the documented contract: absent <uses-sdk> => sdk None (never fabricated),
        // absent android:largeHeap => large_heap false. The real Roblox manifest measured
        // large_heap == false, so this is the production-relevant case.
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
        // Regression guard: the real Roblox AndroidManifest.xml is Deflate-compressed (only
        // libroblox.so is Stored). With the zip crate's `deflate` feature disabled this fails
        // with ZipError::UnsupportedArchive; this test pins that the feature stays enabled.
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
        // An APK with no AndroidManifest.xml must surface EntryMissing, not panic.
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
        // Non-AXML bytes are rejected by the reader as a typed ApkError::Axml(_), not a panic.
        let bytes = build_apk(&[(MANIFEST_ENTRY, b"this is not binary xml at all")]);
        let (mut apk, path) = open_apk(&bytes, "garbage");
        let err = apk.manifest().expect_err("garbage must fail");
        std::fs::remove_file(&path).ok();
        assert!(matches!(err, ApkError::Axml(_)), "got {err:?}");
    }

    #[test]
    fn panic_fixture_returns_typed_error_not_panic() {
        // ROOT-CAUSE regression guard (2026-06-04): this exact fixture made the old
        // axmldecoder 0.3 dependency *panic* internally (invalid resource value-type byte),
        // which aborts the process under the release `panic = "abort"` profile. Eclipse's own
        // total reader must instead return a typed ApkError::Axml(..) — proving the root cause
        // (a library that panics on hostile AXML) is fixed. The test reaching this assertion
        // at all (rather than the harness reporting a panic) is the proof of totality.
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
        // TOTALITY guard for the confirmed root cause (a parser that panics on malformed AXML
        // aborts under panic=abort). Starting from a known-good manifest, this exhaustively:
        //   (a) truncates it at *every* length 0..=len, and
        //   (b) flips bytes at a strided set of offsets to 0x00 / 0x7F / 0xFF,
        // and calls the reader on each input, requiring it to return a Result (Ok or Err)
        // without panicking. Because the test process completes, every one of these thousands
        // of adversarial inputs was handled without a panic — that is the operational meaning
        // of "total" for a #[forbid(unsafe_code)] reader: no input drives it to abort.
        // Drive BOTH string-pool encodings: the UTF-8 fixture and the UTF-16 fixture (the
        // encoding the real Roblox manifest uses), so the decode_utf16 path is fuzzed too.
        for base in [FIXTURE_MANIFEST, FIXTURE_UTF16] {
            // (a) Truncation at every prefix length.
            for len in 0..=base.len() {
                // Must not panic; we only require *a* Result, not a particular variant.
                let _ = axml::read_manifest(&base[..len]);
            }

            // (b) Byte mutation across a stride, each over three boundary values.
            let stride = 7; // coprime-ish with struct sizes so it hits varied fields cheaply
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
    fn manifest_parses_utf16_string_pool() {
        // The real Roblox manifest uses a UTF-16 string pool (aapt2/bundletool), which drives
        // axml::decode_utf16 — a path the UTF-8 fixtures never reach. FIXTURE_UTF16 is the same
        // logical manifest as FIXTURE_MANIFEST with UTF8_FLAG cleared, so it must parse to the
        // identical fields.
        let utf16 = build_apk(&[(MANIFEST_ENTRY, FIXTURE_UTF16)]);
        let (mut apk16, p16) = open_apk(&utf16, "manifest-utf16");
        let m16 = apk16.manifest().expect("parse utf16 manifest");
        std::fs::remove_file(&p16).ok();

        assert_eq!(m16.package, "com.example.app");
        assert_eq!(m16.launcher_activity, ".SplashActivity");
        assert_eq!(m16.min_sdk, Some(26));
        assert_eq!(m16.target_sdk, Some(35));
        assert!(m16.large_heap);

        // Cross-check: the UTF-16 and UTF-8 encodings of the same logical manifest must yield
        // identical results — proves the decoder, not just the fixture.
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
            // armeabi-v7a ships a native lib but NOT the engine (has_engine false branch).
            ("lib/armeabi-v7a/libfoo.so", b"foo"),
            ("classes.dex", b"dex"),
        ]);
        let (apk, path) = open_apk(&bytes, "abis");
        let abis = apk.native_abis();
        std::fs::remove_file(&path).ok();

        let names: Vec<&str> = abis.iter().map(|a| a.name.as_str()).collect();
        assert_eq!(names, vec!["arm64-v8a", "armeabi-v7a", "x86_64"]); // sorted, deduped
                                                                       // Both flag states are pinned: engine ABIs true, the engine-less one false.
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
    fn native_abis_negative_case_no_x86_64() {
        // Negative case: an arm-only APK has no x86_64 ABI and no x86_64 engine.
        let bytes = build_apk(&[("lib/arm64-v8a/libroblox.so", b"engine")]);
        let (mut apk, path) = open_apk(&bytes, "armonly");
        let abis = apk.native_abis();
        assert!(!abis.iter().any(|a| a.name == TARGET_ABI));
        // And the engine lookup reports it missing, as a typed error.
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
        assert!(engine.stored); // fixture writes Stored
    }

    #[test]
    fn x86_64_engine_reports_not_stored_when_deflated() {
        // Guards the mmap-readiness invariant in the other direction: a Deflated engine must
        // report stored == false (ART cannot mmap it), while size still reports the
        // uncompressed length. Without this, an inverted comparison would pass the suite.
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
    fn verify_integrity_matches_correct_digest() {
        // SHA-256 of "abc" (RFC test vector) — a fixed, machine-independent regression guard.
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
        // A non-existent path must surface a typed ApkError::Io, never panic.
        let mut path = std::env::temp_dir();
        path.push(format!(
            "eclipse-apk-test-nonexistent-{:?}.tmp",
            std::thread::current().id()
        ));
        std::fs::remove_file(&path).ok(); // ensure it does not exist
                                          // Apk has no Debug impl (it holds a ZipArchive), so inspect the Err via .err().
        let err = Apk::open(&path).err().expect("missing file must fail");
        assert!(matches!(err, ApkError::Io(_)), "got {err:?}");
    }

    #[test]
    fn open_non_zip_file_is_zip_error() {
        // A real file that is not a zip must surface a typed ApkError::Zip, never panic.
        let path = temp_file("notzip", b"this is plainly not a zip archive");
        let err = Apk::open(&path).err().expect("non-zip must fail");
        std::fs::remove_file(&path).ok();
        assert!(matches!(err, ApkError::Zip(_)), "got {err:?}");
    }

    #[test]
    fn verify_integrity_rejects_malformed_expected() {
        let path = temp_file("sha-malformed", b"abc");
        // Too short and non-hex must be rejected before hashing.
        let err = verify_integrity(&path, "not-a-digest").expect_err("must reject");
        std::fs::remove_file(&path).ok();
        assert!(matches!(err, ApkError::InvalidDigest(_)), "got {err:?}");
    }
}
