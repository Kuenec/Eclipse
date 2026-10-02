use eclipse_webview::proto::{self, CookieExpiry, SameSite, StoredCookie};
use gtk4::glib;
use std::fmt;
use std::io::{Read as _, Write as _};
use std::os::unix::fs::OpenOptionsExt as _;
use std::path::{Path, PathBuf};
use webkit6::soup;

const SESSION_COOKIE_MAX_AGE: i32 = -1;

pub(crate) fn to_soup(cookie: &StoredCookie) -> Result<soup::Cookie, glib::BoolError> {
    if cookie.domain.is_empty() {
        return Err(glib::bool_error!("cookie {:?} has no domain", cookie.name));
    }
    let mut soup_cookie = soup::Cookie::new(
        &cookie.name,
        &cookie.value,
        &cookie.domain,
        &cookie.path,
        SESSION_COOKIE_MAX_AGE,
    );
    soup_cookie.set_secure(cookie.secure);
    soup_cookie.set_http_only(cookie.http_only);
    soup_cookie.set_same_site_policy(match cookie.same_site {
        SameSite::None => soup::SameSitePolicy::None,
        SameSite::Lax => soup::SameSitePolicy::Lax,
        SameSite::Strict => soup::SameSitePolicy::Strict,
    });
    if let CookieExpiry::At { epoch_s } = cookie.expiry {
        soup_cookie.set_expires(&glib::DateTime::from_unix_utc(epoch_s)?);
    }
    Ok(soup_cookie)
}

pub(crate) fn from_soup(cookie: &mut soup::Cookie) -> StoredCookie {
    StoredCookie {
        name: cookie.name().map(String::from).unwrap_or_default(),
        value: cookie.value().map(String::from).unwrap_or_default(),
        domain: cookie.domain().map(String::from).unwrap_or_default(),
        path: cookie.path().map(String::from).unwrap_or_default(),
        secure: cookie.is_secure(),
        http_only: cookie.is_http_only(),
        same_site: match cookie.same_site_policy() {
            soup::SameSitePolicy::Strict => SameSite::Strict,
            soup::SameSitePolicy::Lax => SameSite::Lax,
            _ => SameSite::None,
        },
        expiry: match cookie.expires() {
            Some(expires) => CookieExpiry::At {
                epoch_s: expires.to_unix(),
            },
            None => CookieExpiry::Session,
        },
    }
}

#[derive(Debug)]
pub(crate) enum JarError {
    Io(std::io::Error),
    Oversized,
    Codec(proto::ProtoError),
}

impl fmt::Display for JarError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Io(error) => write!(f, "{error}"),
            Self::Oversized => write!(f, "larger than {} bytes", proto::GLOBAL_FRAME_CAP),
            Self::Codec(error) => write!(f, "{error}"),
        }
    }
}

pub(crate) struct SessionJar {
    path: PathBuf,
}

impl SessionJar {
    pub(crate) fn new(path: PathBuf) -> Self {
        Self { path }
    }

    pub(crate) fn path(&self) -> &Path {
        &self.path
    }

    pub(crate) fn load(&self) -> Result<Vec<StoredCookie>, JarError> {
        let file = match std::fs::File::open(&self.path) {
            Ok(file) => file,
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => return Ok(Vec::new()),
            Err(error) => return Err(JarError::Io(error)),
        };
        let mut bytes = Vec::new();
        file.take(u64::from(proto::GLOBAL_FRAME_CAP) + 1)
            .read_to_end(&mut bytes)
            .map_err(JarError::Io)?;
        if bytes.len() > proto::GLOBAL_FRAME_CAP as usize {
            return Err(JarError::Oversized);
        }
        proto::decode_cookies(&bytes).map_err(JarError::Codec)
    }

    pub(crate) fn store(&self, cookies: &[StoredCookie]) -> Result<(), JarError> {
        let bytes = proto::encode_cookies(cookies).map_err(JarError::Codec)?;
        let staging = self.path.with_extension("tmp");
        let mut file = std::fs::OpenOptions::new()
            .write(true)
            .create(true)
            .truncate(true)
            .mode(0o600)
            .open(&staging)
            .map_err(JarError::Io)?;
        file.write_all(&bytes).map_err(JarError::Io)?;
        file.sync_all().map_err(JarError::Io)?;
        std::fs::rename(&staging, &self.path).map_err(JarError::Io)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::os::unix::fs::PermissionsExt as _;

    fn cookie(name: &str, expiry: CookieExpiry) -> StoredCookie {
        StoredCookie {
            name: name.to_string(),
            value: "v".to_string(),
            domain: ".roblox.com".to_string(),
            path: "/".to_string(),
            secure: true,
            http_only: true,
            same_site: SameSite::Lax,
            expiry,
        }
    }

    fn scratch(tag: &str) -> PathBuf {
        let dir =
            std::env::temp_dir().join(format!("eclipse-webview-jar-{}-{tag}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).expect("scratch dir");
        dir
    }

    #[test]
    fn cookies_keep_every_field_through_libsoup() {
        for original in [
            cookie("session", CookieExpiry::Session),
            cookie(
                "persistent",
                CookieExpiry::At {
                    epoch_s: 1_900_000_000,
                },
            ),
            StoredCookie {
                domain: "www.roblox.com".to_string(),
                secure: false,
                http_only: false,
                same_site: SameSite::Strict,
                ..cookie("host-only", CookieExpiry::Session)
            },
        ] {
            let mut soup_cookie = to_soup(&original).expect("soup cookie");
            assert_eq!(from_soup(&mut soup_cookie), original);
        }
    }

    #[test]
    fn a_cookie_without_a_domain_is_rejected_before_libsoup_sees_it() {
        let mut domainless = cookie("orphan", CookieExpiry::Session);
        domainless.domain.clear();
        assert!(to_soup(&domainless).is_err());
    }

    #[test]
    fn the_session_jar_round_trips_privately_and_starts_empty() {
        let dir = scratch("roundtrip");
        let jar = SessionJar::new(dir.join("session-cookies"));
        assert!(jar.load().expect("missing jar").is_empty());
        let cookies = vec![cookie("a", CookieExpiry::Session)];
        jar.store(&cookies).expect("store");
        assert_eq!(jar.load().expect("load"), cookies);
        let mode = std::fs::metadata(jar.path())
            .expect("metadata")
            .permissions()
            .mode();
        assert_eq!(mode & 0o777, 0o600);
        std::fs::remove_dir_all(&dir).expect("cleanup");
    }

    #[test]
    fn a_corrupt_session_jar_is_reported_not_trusted() {
        let dir = scratch("corrupt");
        let jar = SessionJar::new(dir.join("session-cookies"));
        std::fs::write(jar.path(), [5, 0, 1]).expect("write");
        assert!(matches!(jar.load(), Err(JarError::Codec(_))));
        std::fs::remove_dir_all(&dir).expect("cleanup");
    }
}
