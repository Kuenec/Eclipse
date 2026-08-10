use std::fmt;
use std::fs::File;
use std::io::{self, Read, Write};
use std::path::{Path, PathBuf};

use sha2::{Digest, Sha256};

const VERSION_ORACLE_URL: &str =
    "https://clientsettingscdn.roblox.com/v2/client-version/WindowsPlayer";

const USER_AGENT: &str = "Mozilla/5.0 (X11; Linux x86_64; rv:128.0) Gecko/20100101 Firefox/128.0";

const MAX_APK_BYTES: u64 = 512 * 1024 * 1024;

#[derive(Debug)]
pub enum FetchError {
    NoSource,

    Http(String),

    Status(u16, String),

    Io(io::Error),

    ShaMismatch { expected: String, got: String },

    TooLarge,

    BadOracle(String),
}

impl fmt::Display for FetchError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::NoSource => write!(
                f,
                "no APK source configured (set config `apk_url` or ECLIPSE_APK_URL, or pass an APK path)"
            ),
            Self::Http(e) => write!(f, "HTTP request failed: {e}"),
            Self::Status(code, url) => write!(f, "download returned HTTP {code} for {url}"),
            Self::Io(e) => write!(f, "I/O error during fetch: {e}"),
            Self::ShaMismatch { expected, got } => {
                write!(f, "APK SHA-256 mismatch: expected {expected}, got {got}")
            }
            Self::TooLarge => write!(f, "download exceeded the {MAX_APK_BYTES}-byte cap"),
            Self::BadOracle(e) => write!(f, "version oracle response was malformed: {e}"),
        }
    }
}

impl std::error::Error for FetchError {}

impl From<io::Error> for FetchError {
    fn from(e: io::Error) -> Self {
        Self::Io(e)
    }
}

pub fn apk_cache_dir() -> Result<PathBuf, FetchError> {
    let dir = if let Some(over) = std::env::var_os("ECLIPSE_APK_CACHE_DIR") {
        PathBuf::from(over)
    } else {
        directories::ProjectDirs::from("", "", "eclipse")
            .map(|p| p.cache_dir().join("apks"))
            .ok_or_else(|| {
                FetchError::Io(io::Error::new(
                    io::ErrorKind::NotFound,
                    "no XDG cache dir (set ECLIPSE_APK_CACHE_DIR)",
                ))
            })?
    };
    std::fs::create_dir_all(&dir)?;
    Ok(dir)
}

pub fn latest_roblox_version() -> Result<String, FetchError> {
    let mut resp = ureq::get(VERSION_ORACLE_URL)
        .header("User-Agent", USER_AGENT)
        .call()
        .map_err(|e| FetchError::Http(e.to_string()))?;
    let status = resp.status().as_u16();
    if !(200..300).contains(&status) {
        return Err(FetchError::Status(status, VERSION_ORACLE_URL.to_string()));
    }
    let body = resp
        .body_mut()
        .read_to_string()
        .map_err(|e| FetchError::BadOracle(e.to_string()))?;
    let json: serde_json::Value =
        serde_json::from_str(&body).map_err(|e| FetchError::BadOracle(e.to_string()))?;
    json.get("version")
        .and_then(serde_json::Value::as_str)
        .map(str::to_owned)
        .ok_or_else(|| FetchError::BadOracle("missing `version` field".to_string()))
}

pub fn fetch_apk(url: &str, expected_sha256: Option<&str>) -> Result<PathBuf, FetchError> {
    let dir = apk_cache_dir()?;
    let dest = dir.join(cache_filename(url));

    if dest.exists() {
        match expected_sha256 {
            Some(exp) if file_sha256(&dest)?.eq_ignore_ascii_case(exp) => return Ok(dest),
            Some(_) => {}
            None => return Ok(dest),
        }
    }

    let tmp = dest.with_extension("partial");
    let got = download_to_file(url, &tmp)?;
    if let Some(exp) = expected_sha256 {
        if !got.eq_ignore_ascii_case(exp) {
            let _ = std::fs::remove_file(&tmp);
            return Err(FetchError::ShaMismatch {
                expected: exp.to_string(),
                got,
            });
        }
    }
    std::fs::rename(&tmp, &dest)?;
    Ok(dest)
}

fn download_to_file(url: &str, tmp: &Path) -> Result<String, FetchError> {
    let mut resp = ureq::get(url)
        .header("User-Agent", USER_AGENT)
        .call()
        .map_err(|e| FetchError::Http(e.to_string()))?;
    let status = resp.status().as_u16();
    if !(200..300).contains(&status) {
        return Err(FetchError::Status(status, url.to_string()));
    }

    let mut reader = resp.body_mut().as_reader().take(MAX_APK_BYTES + 1);
    let mut file = File::create(tmp)?;
    let mut hasher = Sha256::new();
    let mut buf = vec![0u8; 1 << 16];
    let mut total: u64 = 0;
    loop {
        let n = reader.read(&mut buf)?;
        if n == 0 {
            break;
        }
        total += n as u64;
        if total > MAX_APK_BYTES {
            drop(file);
            let _ = std::fs::remove_file(tmp);
            return Err(FetchError::TooLarge);
        }
        file.write_all(&buf[..n])?;
        hasher.update(&buf[..n]);
    }
    file.sync_all()?;
    Ok(hex(&hasher.finalize()))
}

fn file_sha256(path: &Path) -> Result<String, FetchError> {
    let mut file = File::open(path)?;
    let mut hasher = Sha256::new();
    let mut buf = vec![0u8; 1 << 16];
    loop {
        let n = file.read(&mut buf)?;
        if n == 0 {
            break;
        }
        hasher.update(&buf[..n]);
    }
    Ok(hex(&hasher.finalize()))
}

fn cache_filename(url: &str) -> String {
    let last = url
        .rsplit('/')
        .next()
        .unwrap_or("")
        .split(['?', '#'])
        .next()
        .unwrap_or("");
    let cleaned: String = last
        .chars()
        .filter(|c| c.is_ascii_alphanumeric() || matches!(c, '.' | '-' | '_'))
        .collect();
    if cleaned.is_empty() || cleaned == "." || cleaned == ".." {
        "roblox.apk".to_string()
    } else {
        cleaned
    }
}

fn hex(bytes: &[u8]) -> String {
    use fmt::Write;
    let mut s = String::with_capacity(bytes.len() * 2);
    for b in bytes {
        let _ = write!(s, "{b:02x}");
    }
    s
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn cache_filename_is_safe_and_derived() {
        assert_eq!(
            cache_filename("https://example.com/dl/roblox-2.721.1108.apk?token=abc"),
            "roblox-2.721.1108.apk"
        );
        assert_eq!(
            cache_filename("https://d.apkpure.com/b/XAPK/com.roblox.client?versionCode=1"),
            "com.roblox.client"
        );

        assert_eq!(cache_filename("http://x/../../etc/passwd"), "passwd");
        assert_eq!(cache_filename("http://x/"), "roblox.apk");
        assert_eq!(cache_filename("http://x/.."), "roblox.apk");
    }

    #[test]
    fn hex_is_lowercase_padded() {
        assert_eq!(hex(&[0x00, 0x0f, 0xa0, 0xff]), "000fa0ff");
    }
}
