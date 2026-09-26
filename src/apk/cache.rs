use std::fs::Metadata;
use std::os::unix::fs::MetadataExt;
use std::path::Path;
use std::sync::{Mutex, PoisonError};

use super::{Apk, ApkError};

pub static APP_APK: ApkCache = ApkCache::new();

pub static FRAMEWORK_RES_APK: ApkCache = ApkCache::new();

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
struct FileIdentity {
    device: u64,
    inode: u64,
    len: u64,
    modified_seconds: i64,
    modified_nanos: i64,
}

impl FileIdentity {
    fn of(metadata: &Metadata) -> Self {
        Self {
            device: metadata.dev(),
            inode: metadata.ino(),
            len: metadata.size(),
            modified_seconds: metadata.mtime(),
            modified_nanos: metadata.mtime_nsec(),
        }
    }
}

struct OpenedApk {
    identity: FileIdentity,
    apk: Apk,
}

pub struct ApkCache {
    opened: Mutex<Option<OpenedApk>>,
}

impl ApkCache {
    const fn new() -> Self {
        Self {
            opened: Mutex::new(None),
        }
    }

    pub fn open(&self, path: &Path) -> Result<Apk, ApkError> {
        let identity = FileIdentity::of(&std::fs::metadata(path)?);
        let mut opened = self.opened.lock().unwrap_or_else(PoisonError::into_inner);
        if let Some(cached) = opened
            .as_ref()
            .filter(|cached| cached.identity == identity && cached.apk.path() == path)
        {
            return Ok(cached.apk.clone());
        }
        let apk = Apk::open(path)?;
        *opened = Some(OpenedApk {
            identity,
            apk: apk.clone(),
        });
        Ok(apk)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::io::{Cursor, Write};
    use std::path::PathBuf;
    use zip::write::SimpleFileOptions;
    use zip::{CompressionMethod, ZipWriter};

    fn write_apk(tag: &str, entries: &[(&str, &[u8], CompressionMethod)]) -> PathBuf {
        let mut writer = ZipWriter::new(Cursor::new(Vec::new()));
        for (name, bytes, method) in entries {
            let options = SimpleFileOptions::default().compression_method(*method);
            writer.start_file(*name, options).expect("start_file");
            writer.write_all(bytes).expect("write_all");
        }
        let bytes = writer.finish().expect("finish").into_inner();
        let path = std::env::temp_dir().join(format!(
            "eclipse-apk-cache-{tag}-{}-{:?}.apk",
            std::process::id(),
            std::thread::current().id()
        ));
        std::fs::write(&path, bytes).expect("write apk");
        path
    }

    #[test]
    fn cached_reads_match_fresh_reads_for_stored_and_deflated_entries() {
        let big: Vec<u8> = (0..300_000u32).map(|i| (i * 7 % 253) as u8).collect();
        let path = write_apk(
            "identical",
            &[
                (
                    "res/layout/a.xml",
                    b"layout-bytes",
                    CompressionMethod::Deflated,
                ),
                ("assets/big.bin", &big, CompressionMethod::Stored),
                ("assets/big.deflated", &big, CompressionMethod::Deflated),
            ],
        );
        let cache = ApkCache::new();
        let mut fresh = Apk::open(&path).expect("fresh open");
        for _ in 0..3 {
            let mut cached = cache.open(&path).expect("cached open");
            for name in ["res/layout/a.xml", "assets/big.bin", "assets/big.deflated"] {
                assert_eq!(
                    cached.read_entry(name).expect("cached read"),
                    fresh.read_entry(name).expect("fresh read"),
                    "{name}"
                );
            }
        }
        std::fs::remove_file(&path).ok();
    }

    #[test]
    fn a_different_path_or_a_replaced_file_is_never_served_from_the_cache() {
        let old = write_apk(
            "old",
            &[("assets/version.txt", b"old", CompressionMethod::Stored)],
        );
        let new = write_apk(
            "new",
            &[("assets/version.txt", b"new!", CompressionMethod::Stored)],
        );
        let cache = ApkCache::new();
        let read = |path: &Path| {
            cache
                .open(path)
                .expect("open")
                .read_entry("assets/version.txt")
                .expect("read")
        };
        assert_eq!(read(&old), b"old");
        assert_eq!(read(&new), b"new!");
        assert_eq!(read(&old), b"old");

        std::fs::rename(&new, &old).expect("install the update over the old path");
        assert_eq!(
            read(&old),
            b"new!",
            "a replaced file at the same path is reopened"
        );
        std::fs::remove_file(&old).ok();
    }

    #[test]
    fn reopening_a_cached_apk_file_reaches_the_indexed_file_with_its_own_offset() {
        use std::io::{Read, Seek, SeekFrom};

        let installed = write_apk(
            "reopen-installed",
            &[("assets/a.bin", b"installed", CompressionMethod::Stored)],
        );
        let update = write_apk(
            "reopen-update",
            &[
                ("assets/pad.bin", &[0u8; 4096], CompressionMethod::Stored),
                ("assets/a.bin", b"updated!!", CompressionMethod::Stored),
            ],
        );
        let cache = ApkCache::new();
        let mut apk = cache.open(&installed).expect("open");
        let span = apk.entry_span("assets/a.bin").expect("span");
        std::fs::rename(&update, &installed).expect("install the update over the path");

        let mut reopened = crate::apk::reopen(apk.file()).expect("reopen");
        reopened
            .seek(SeekFrom::Start(span.data_start))
            .expect("seek to the entry");
        let mut got = [0u8; 9];
        reopened.read_exact(&mut got).expect("read the entry");
        assert_eq!(&got, b"installed");
        assert_eq!(
            (&**apk.file()).stream_position().expect("shared position"),
            0,
            "the reopened descriptor does not move the cached file's offset"
        );
        std::fs::remove_file(&installed).ok();
    }

    #[test]
    fn clones_from_the_cache_read_concurrently() {
        let entries: Vec<(String, Vec<u8>)> = (0..16u8)
            .map(|i| (format!("assets/{i}.bin"), vec![i; 50_000 + usize::from(i)]))
            .collect();
        let zip_entries: Vec<(&str, &[u8], CompressionMethod)> = entries
            .iter()
            .map(|(name, bytes)| (name.as_str(), bytes.as_slice(), CompressionMethod::Deflated))
            .collect();
        let path = write_apk("concurrent", &zip_entries);
        let cache = ApkCache::new();
        std::thread::scope(|scope| {
            for (name, bytes) in &entries {
                let cache = &cache;
                let path = &path;
                scope.spawn(move || {
                    for _ in 0..4 {
                        let mut apk = cache.open(path).expect("open");
                        assert_eq!(&apk.read_entry(name).expect("read"), bytes, "{name}");
                    }
                });
            }
        });
        std::fs::remove_file(&path).ok();
    }
}
