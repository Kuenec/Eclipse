use std::ffi::{c_int, CStr, CString};
use std::num::NonZeroU32;
use std::os::unix::ffi::OsStrExt as _;
use std::path::Path;
use std::time::{SystemTime, UNIX_EPOCH};

use aes::cipher::{Array, BlockCipherDecrypt as _, KeyInit as _};
use aes::Aes128;
use libsqlite3_sys as ffi;
use sha2::{Digest as _, Sha256};

use super::proto::{CookieExpiry, SameSite, StoredCookie};

pub(super) const COOKIE_DATABASE: &str = "Default/Cookies";

const SCHEMA_VERSION: i64 = 24;

const SCHEMA_VERSION_QUERY: &CStr = c"SELECT value FROM meta WHERE key = 'version'";

const COOKIE_QUERY: &CStr = c"SELECT host_key, name, value, encrypted_value, path, expires_utc, \
    is_secure, is_httponly, is_persistent, samesite FROM cookies WHERE top_frame_site_key = ''";

const SECONDS_FROM_1601_TO_UNIX_EPOCH: i64 = 11_644_473_600;

const MICROSECONDS_PER_SECOND: i64 = 1_000_000;

const OBFUSCATION_PREFIX: &[u8] = b"v10";

const KEYRING_PREFIX: &[u8] = b"v11";

const OBFUSCATION_PASSWORD: &[u8] = b"peanuts";

const OBFUSCATION_SALT: &[u8] = b"saltysalt";

const AES_BLOCK: usize = 16;

const OBFUSCATION_IV: [u8; AES_BLOCK] = [b' '; AES_BLOCK];

#[derive(Debug, Clone, PartialEq, Eq)]
pub(super) enum CefProfileError {
    Database {
        operation: &'static str,
        message: String,
    },

    UnsupportedVersion(i64),

    KeyringEncrypted,

    Cookie {
        host: String,
        name: String,
        problem: &'static str,
    },
}

impl std::fmt::Display for CefProfileError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::Database { operation, message } => {
                write!(f, "cannot {operation} the CEF cookie database: {message}")
            }
            Self::UnsupportedVersion(version) => write!(
                f,
                "the CEF cookie database has schema version {version}, and Eclipse reads only \
                 version {SCHEMA_VERSION}"
            ),
            Self::KeyringEncrypted => f.write_str(
                "the CEF WebView encrypted its cookies with a key kept in the desktop keyring \
                 (v11), which Eclipse cannot read",
            ),
            Self::Cookie {
                host,
                name,
                problem,
            } => write!(f, "cookie {name} for {host} {problem}"),
        }
    }
}

impl std::error::Error for CefProfileError {}

pub(super) fn read_cookies(
    profile: &Path,
    now: SystemTime,
) -> Result<Vec<StoredCookie>, CefProfileError> {
    let path = profile.join(COOKIE_DATABASE);
    match path.try_exists() {
        Ok(true) => {}
        Ok(false) => return Ok(Vec::new()),
        Err(error) => {
            return Err(CefProfileError::Database {
                operation: "find",
                message: error.to_string(),
            })
        }
    }
    let database = Database::open(&path)?;
    let version = database.schema_version()?;
    if version != SCHEMA_VERSION {
        return Err(CefProfileError::UnsupportedVersion(version));
    }
    let now_unix_s = now.duration_since(UNIX_EPOCH).map_or(0, |since| {
        i64::try_from(since.as_secs()).unwrap_or(i64::MAX)
    });
    let cipher = obfuscation_cipher();
    let mut rows = database.prepare(COOKIE_QUERY)?;
    let mut cookies = Vec::new();
    while let Some(row) = rows.next_row()? {
        let row = CookieRow {
            host: row.text(0)?,
            name: row.text(1)?,
            plain_value: row.bytes(2)?,
            encrypted_value: row.bytes(3)?,
            path: row.text(4)?,
            expires_utc: row.int(5)?,
            secure: row.int(6)? != 0,
            http_only: row.int(7)? != 0,
            persistent: row.int(8)? != 0,
            same_site: row.int(9)?,
        };
        if let Some(cookie) = row.into_cookie(&cipher, now_unix_s)? {
            cookies.push(cookie);
        }
    }
    Ok(cookies)
}

struct CookieRow {
    host: String,
    name: String,
    plain_value: Vec<u8>,
    encrypted_value: Vec<u8>,
    path: String,
    expires_utc: i64,
    secure: bool,
    http_only: bool,
    persistent: bool,
    same_site: i64,
}

impl CookieRow {
    fn into_cookie(
        self,
        cipher: &Aes128,
        now_unix_s: i64,
    ) -> Result<Option<StoredCookie>, CefProfileError> {
        let expiry = if self.persistent {
            let epoch_s = self.expires_utc.div_euclid(MICROSECONDS_PER_SECOND)
                - SECONDS_FROM_1601_TO_UNIX_EPOCH;
            if epoch_s <= now_unix_s {
                return Ok(None);
            }
            CookieExpiry::At { epoch_s }
        } else {
            CookieExpiry::Session
        };
        let same_site = match self.same_site {
            -1 | 1 => SameSite::Lax,
            0 => SameSite::None,
            2 => SameSite::Strict,
            _ => return Err(self.problem("has an unknown SameSite value")),
        };
        let Ok(value) = String::from_utf8(self.value(cipher)?) else {
            return Err(self.problem("has a value that is not UTF-8"));
        };
        Ok(Some(StoredCookie {
            name: self.name,
            value,
            domain: self.host,
            path: self.path,
            secure: self.secure,
            http_only: self.http_only,
            same_site,
            expiry,
        }))
    }

    fn value(&self, cipher: &Aes128) -> Result<Vec<u8>, CefProfileError> {
        if self.encrypted_value.is_empty() {
            return Ok(self.plain_value.clone());
        }
        match self
            .encrypted_value
            .split_at_checked(OBFUSCATION_PREFIX.len())
        {
            Some((OBFUSCATION_PREFIX, ciphertext)) => {
                let plaintext =
                    decrypt_cbc(cipher, ciphertext).map_err(|problem| self.problem(problem))?;
                let digest = Sha256::digest(self.host.as_bytes());
                plaintext
                    .strip_prefix(digest.as_slice())
                    .map(<[u8]>::to_vec)
                    .ok_or_else(|| self.problem("does not start with the digest of its host"))
            }
            Some((KEYRING_PREFIX, _)) => Err(CefProfileError::KeyringEncrypted),
            _ => Err(self.problem("is encrypted with an unknown scheme")),
        }
    }

    fn problem(&self, problem: &'static str) -> CefProfileError {
        CefProfileError::Cookie {
            host: self.host.clone(),
            name: self.name.clone(),
            problem,
        }
    }
}

fn obfuscation_cipher() -> Aes128 {
    let mut key = [0u8; AES_BLOCK];
    ring::pbkdf2::derive(
        ring::pbkdf2::PBKDF2_HMAC_SHA1,
        NonZeroU32::MIN,
        OBFUSCATION_SALT,
        OBFUSCATION_PASSWORD,
        &mut key,
    );
    Aes128::new(&Array::from(key))
}

fn decrypt_cbc(cipher: &Aes128, ciphertext: &[u8]) -> Result<Vec<u8>, &'static str> {
    let (blocks, rest) = ciphertext.as_chunks::<AES_BLOCK>();
    if blocks.is_empty() || !rest.is_empty() {
        return Err("has a ciphertext that is not a whole number of AES blocks");
    }
    let mut plaintext = Vec::with_capacity(ciphertext.len());
    let mut previous = OBFUSCATION_IV;
    for block in blocks {
        let mut decrypted = Array::from(*block);
        cipher.decrypt_block(&mut decrypted);
        plaintext.extend(
            decrypted
                .iter()
                .zip(previous)
                .map(|(byte, mask)| byte ^ mask),
        );
        previous = *block;
    }
    let padding = plaintext.last().map_or(0, |&last| usize::from(last));
    let unpadded = plaintext.len().saturating_sub(padding);
    if !(1..=AES_BLOCK).contains(&padding)
        || plaintext[unpadded..]
            .iter()
            .any(|&byte| usize::from(byte) != padding)
    {
        return Err("has invalid AES padding");
    }
    plaintext.truncate(unpadded);
    Ok(plaintext)
}

struct Database(*mut ffi::sqlite3);

impl Database {
    fn open(path: &Path) -> Result<Self, CefProfileError> {
        let Ok(c_path) = CString::new(path.as_os_str().as_bytes()) else {
            return Err(CefProfileError::Database {
                operation: "open",
                message: format!("{} contains a NUL byte", path.display()),
            });
        };
        let mut handle = std::ptr::null_mut();
        let rc = unsafe {
            ffi::sqlite3_open_v2(
                c_path.as_ptr(),
                &mut handle,
                ffi::SQLITE_OPEN_READWRITE,
                std::ptr::null(),
            )
        };
        let database = Self(handle);
        if rc != ffi::SQLITE_OK {
            return Err(database.error("open"));
        }
        Ok(database)
    }

    fn error(&self, operation: &'static str) -> CefProfileError {
        let message = if self.0.is_null() {
            "SQLite could not allocate a connection".to_string()
        } else {
            unsafe { CStr::from_ptr(ffi::sqlite3_errmsg(self.0)) }
                .to_string_lossy()
                .into_owned()
        };
        CefProfileError::Database { operation, message }
    }

    fn prepare(&self, sql: &CStr) -> Result<Statement<'_>, CefProfileError> {
        let mut handle = std::ptr::null_mut();
        let rc = unsafe {
            ffi::sqlite3_prepare_v2(self.0, sql.as_ptr(), -1, &mut handle, std::ptr::null_mut())
        };
        let statement = Statement {
            database: self,
            handle,
        };
        if rc != ffi::SQLITE_OK {
            return Err(self.error("query"));
        }
        Ok(statement)
    }

    fn schema_version(&self) -> Result<i64, CefProfileError> {
        let mut rows = self.prepare(SCHEMA_VERSION_QUERY)?;
        let Some(row) = rows.next_row()? else {
            return Err(CefProfileError::Database {
                operation: "read the schema version of",
                message: "the meta table has no version".to_string(),
            });
        };
        row.int(0)
    }
}

impl Drop for Database {
    fn drop(&mut self) {
        unsafe { ffi::sqlite3_close(self.0) };
    }
}

struct Statement<'db> {
    database: &'db Database,
    handle: *mut ffi::sqlite3_stmt,
}

impl<'db> Statement<'db> {
    fn next_row(&mut self) -> Result<Option<Row<'_, 'db>>, CefProfileError> {
        match unsafe { ffi::sqlite3_step(self.handle) } {
            ffi::SQLITE_ROW => Ok(Some(Row { statement: self })),
            ffi::SQLITE_DONE => Ok(None),
            _ => Err(self.database.error("read")),
        }
    }
}

impl Drop for Statement<'_> {
    fn drop(&mut self) {
        unsafe { ffi::sqlite3_finalize(self.handle) };
    }
}

struct Row<'stmt, 'db> {
    statement: &'stmt Statement<'db>,
}

impl Row<'_, '_> {
    fn column(&self, column: c_int) -> Result<c_int, CefProfileError> {
        let count = unsafe { ffi::sqlite3_column_count(self.statement.handle) };
        if !(0..count).contains(&column) {
            return Err(CefProfileError::Database {
                operation: "read",
                message: format!("the query has no column {column}"),
            });
        }
        Ok(column)
    }

    fn int(&self, column: c_int) -> Result<i64, CefProfileError> {
        let column = self.column(column)?;
        Ok(unsafe { ffi::sqlite3_column_int64(self.statement.handle, column) })
    }

    fn bytes(&self, column: c_int) -> Result<Vec<u8>, CefProfileError> {
        let column = self.column(column)?;
        let handle = self.statement.handle;
        let data = unsafe { ffi::sqlite3_column_blob(handle, column) };
        let len = unsafe { ffi::sqlite3_column_bytes(handle, column) };
        let len = usize::try_from(len).map_err(|_| self.statement.database.error("read"))?;
        if len == 0 {
            return Ok(Vec::new());
        }
        if data.is_null() {
            return Err(self.statement.database.error("read"));
        }
        Ok(unsafe { std::slice::from_raw_parts(data.cast::<u8>(), len) }.to_vec())
    }

    fn text(&self, column: c_int) -> Result<String, CefProfileError> {
        String::from_utf8(self.bytes(column)?).map_err(|_| CefProfileError::Database {
            operation: "read",
            message: format!("column {column} holds text that is not UTF-8"),
        })
    }
}

#[cfg(test)]
pub(super) fn install_fixture(profile: &Path) {
    let database = profile.join(COOKIE_DATABASE);
    std::fs::create_dir_all(database.parent().expect("the database has a directory"))
        .expect("create the profile");
    std::fs::copy(
        Path::new(env!("CARGO_MANIFEST_DIR")).join("tests/fixtures/cef-cookies-v10.sqlite"),
        &database,
    )
    .expect("copy the fixture");
}

#[cfg(test)]
pub(super) fn fixture_written_at() -> SystemTime {
    UNIX_EPOCH + std::time::Duration::from_secs(1_790_958_000)
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::path::PathBuf;
    use std::time::Duration;

    fn scratch(tag: &str) -> PathBuf {
        let dir =
            std::env::temp_dir().join(format!("eclipse-cef-profile-{}-{tag}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).expect("create the scratch dir");
        dir
    }

    fn edit(profile: &Path, sql: &CStr) {
        let database = Database::open(&profile.join(COOKIE_DATABASE)).expect("open the copy");
        let mut statement = database.prepare(sql).expect("prepare the edit");
        assert!(statement.next_row().expect("run the edit").is_none());
    }

    fn at(unix_s: u64) -> SystemTime {
        UNIX_EPOCH + Duration::from_secs(unix_s)
    }

    fn sorted(mut cookies: Vec<StoredCookie>) -> Vec<StoredCookie> {
        cookies.sort_by(|a, b| (&a.domain, &a.name).cmp(&(&b.domain, &b.name)));
        cookies
    }

    fn names(cookies: Vec<StoredCookie>) -> Vec<String> {
        sorted(cookies)
            .into_iter()
            .map(|cookie| cookie.name)
            .collect()
    }

    fn roblox(name: &str, value: &str, expiry: CookieExpiry) -> StoredCookie {
        StoredCookie {
            name: name.to_string(),
            value: value.to_string(),
            domain: ".roblox.com".to_string(),
            path: "/".to_string(),
            secure: true,
            http_only: true,
            same_site: SameSite::Lax,
            expiry,
        }
    }

    fn loopback(name: &str, expiry: CookieExpiry) -> StoredCookie {
        StoredCookie {
            domain: "127.0.0.1".to_string(),
            secure: false,
            http_only: false,
            ..roblox(name, "1", expiry)
        }
    }

    fn expires(epoch_s: i64) -> CookieExpiry {
        CookieExpiry::At { epoch_s }
    }

    #[test]
    fn a_cef_profile_yields_every_cookie_the_old_helper_stored() {
        let profile = scratch("all");
        install_fixture(&profile);
        let cookies = read_cookies(&profile, fixture_written_at()).expect("read the profile");
        let loopback_expiry = expires(1_791_044_405);
        assert_eq!(
            sorted(cookies),
            vec![
                roblox(
                    ".RBXIDCHECK",
                    "synthetic-idcheck-1f2e3d",
                    expires(1_822_494_004)
                ),
                roblox(
                    ".ROBLOSECURITY",
                    "synthetic-roblosecurity-for-the-eclipse-cef-migration-test",
                    expires(1_793_550_004)
                ),
                StoredCookie {
                    secure: false,
                    http_only: false,
                    ..roblox("GuestData", "UserID=-1234567", expires(1_822_494_004))
                },
                roblox(
                    "RBXEventTrackerV2",
                    "CreateDate=10/2/2026 9:00:00 AM&rbxid=&browserid=1730000000001",
                    expires(1_825_518_005)
                ),
                roblox(
                    "RBXSessionTracker",
                    "sessionid=synthetic-session",
                    CookieExpiry::Session
                ),
                loopback("http_host_only", loopback_expiry),
                loopback("http_lax", loopback_expiry),
                StoredCookie {
                    http_only: true,
                    ..loopback("http_session", CookieExpiry::Session)
                },
                StoredCookie {
                    path: "/sub".to_string(),
                    same_site: SameSite::Strict,
                    ..loopback("http_strict", loopback_expiry)
                },
                loopback("js_persistent", loopback_expiry),
                loopback("js_session", CookieExpiry::Session),
                StoredCookie {
                    domain: "www.roblox.com".to_string(),
                    http_only: false,
                    ..roblox("rbx-ip2", "1", expires(1_790_961_604))
                },
            ]
        );
        let _ = std::fs::remove_dir_all(&profile);
    }

    #[test]
    fn expired_cef_cookies_stay_behind() {
        let profile = scratch("expired");
        install_fixture(&profile);
        let cookies = read_cookies(&profile, at(1_791_000_000)).expect("read the profile");
        assert_eq!(cookies.len(), 11);
        assert!(!names(cookies).contains(&"rbx-ip2".to_string()));
        let cookies = read_cookies(&profile, at(1_900_000_000)).expect("read the profile");
        assert_eq!(
            names(cookies),
            ["RBXSessionTracker", "http_session", "js_session"]
        );
        let _ = std::fs::remove_dir_all(&profile);
    }

    #[test]
    fn a_keyring_encrypted_cef_profile_is_refused() {
        let profile = scratch("keyring");
        install_fixture(&profile);
        edit(
            &profile,
            c"UPDATE cookies SET encrypted_value = X'76313100112233445566778899AABBCCDDEEFF'",
        );
        assert_eq!(
            read_cookies(&profile, fixture_written_at()),
            Err(CefProfileError::KeyringEncrypted)
        );
        let _ = std::fs::remove_dir_all(&profile);
    }

    #[test]
    fn another_cef_schema_version_is_refused() {
        let profile = scratch("schema");
        install_fixture(&profile);
        edit(
            &profile,
            c"UPDATE meta SET value = '25' WHERE key = 'version'",
        );
        assert_eq!(
            read_cookies(&profile, fixture_written_at()),
            Err(CefProfileError::UnsupportedVersion(25))
        );
        let _ = std::fs::remove_dir_all(&profile);
    }

    #[test]
    fn a_cef_profile_without_a_cookie_database_has_nothing_to_migrate() {
        let profile = scratch("empty");
        assert_eq!(read_cookies(&profile, fixture_written_at()), Ok(Vec::new()));
        let _ = std::fs::remove_dir_all(&profile);
    }

    #[test]
    fn a_damaged_cef_cookie_database_is_reported() {
        let profile = scratch("damaged");
        std::fs::create_dir_all(profile.join("Default")).expect("create the profile");
        std::fs::write(profile.join(COOKIE_DATABASE), [0x5Au8; 4096]).expect("write junk");
        assert!(matches!(
            read_cookies(&profile, fixture_written_at()),
            Err(CefProfileError::Database { .. })
        ));
        let _ = std::fs::remove_dir_all(&profile);
    }

    #[test]
    fn a_cookie_moved_to_another_host_fails_its_digest_check() {
        let profile = scratch("digest");
        install_fixture(&profile);
        edit(
            &profile,
            c"UPDATE cookies SET host_key = 'www.example.com' WHERE name = 'rbx-ip2'",
        );
        assert_eq!(
            read_cookies(&profile, fixture_written_at()),
            Err(CefProfileError::Cookie {
                host: "www.example.com".to_string(),
                name: "rbx-ip2".to_string(),
                problem: "does not start with the digest of its host",
            })
        );
        let _ = std::fs::remove_dir_all(&profile);
    }

    #[test]
    fn partitioned_cookies_stay_behind_and_plain_values_are_read_as_stored() {
        let profile = scratch("partitioned");
        install_fixture(&profile);
        edit(
            &profile,
            c"UPDATE cookies SET top_frame_site_key = 'https://example.com' WHERE name = 'GuestData'",
        );
        edit(
            &profile,
            c"UPDATE cookies SET value = 'plain', encrypted_value = X'', samesite = 0 \
              WHERE name = 'js_session'",
        );
        let cookies = sorted(read_cookies(&profile, fixture_written_at()).expect("read"));
        assert_eq!(cookies.len(), 11);
        assert!(cookies.iter().all(|cookie| cookie.name != "GuestData"));
        assert_eq!(
            cookies.iter().find(|cookie| cookie.name == "js_session"),
            Some(&StoredCookie {
                value: "plain".to_string(),
                same_site: SameSite::None,
                ..loopback("js_session", CookieExpiry::Session)
            })
        );
        let _ = std::fs::remove_dir_all(&profile);
    }
}
