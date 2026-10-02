use std::ffi::OsString;
use std::io::Read;
use std::path::{Path, PathBuf};

const LOCALTIME: &str = "/etc/localtime";

const TIMEZONE_FILE: &str = "/etc/timezone";

const DEFAULT_ZONEINFO_DIR: &str = "/usr/share/zoneinfo";

const ZONEINFO_MARKER: &str = "zoneinfo/";

const ZONEINFO_VARIANTS: [&str; 2] = ["posix/", "right/"];

const ANDROID_ZONE_ID_MAX_LEN: usize = 39;

const TZIF_MAGIC: [u8; 4] = *b"TZif";

const EMPTY_TZ_ZONE_ID: &str = "UTC";

#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct HostTimeZone(String);

impl HostTimeZone {
    pub(crate) fn detect() -> Option<Self> {
        let zoneinfo_dir = std::env::var_os("TZDIR")
            .filter(|dir| !dir.is_empty())
            .map_or_else(|| PathBuf::from(DEFAULT_ZONEINFO_DIR), PathBuf::from);
        Self::from_sources(
            &zoneinfo_dir,
            std::env::var_os("TZ"),
            std::fs::read_link(LOCALTIME).ok(),
            std::fs::read_to_string(TIMEZONE_FILE).ok(),
        )
    }

    pub(crate) fn from_sources(
        zoneinfo_dir: &Path,
        tz: Option<OsString>,
        localtime_target: Option<PathBuf>,
        timezone_file: Option<String>,
    ) -> Option<Self> {
        tz.and_then(|value| value.into_string().ok())
            .and_then(|value| Self::from_tz(&value, zoneinfo_dir))
            .or_else(|| {
                localtime_target.and_then(|target| Self::from_zoneinfo_path(&target, zoneinfo_dir))
            })
            .or_else(|| {
                timezone_file.and_then(|content| Self::installed(content.trim(), zoneinfo_dir))
            })
    }

    fn from_tz(value: &str, zoneinfo_dir: &Path) -> Option<Self> {
        let value = value.strip_prefix(':').unwrap_or(value);
        if value.is_empty() {
            Some(Self(EMPTY_TZ_ZONE_ID.to_owned()))
        } else if value.starts_with('/') {
            Self::from_zoneinfo_path(Path::new(value), zoneinfo_dir)
        } else {
            Self::installed(value, zoneinfo_dir)
        }
    }

    fn from_zoneinfo_path(path: &Path, zoneinfo_dir: &Path) -> Option<Self> {
        let (_, id) = path.to_str()?.rsplit_once(ZONEINFO_MARKER)?;
        let id = ZONEINFO_VARIANTS
            .iter()
            .find_map(|variant| id.strip_prefix(variant))
            .unwrap_or(id);
        Self::installed(id, zoneinfo_dir)
    }

    fn installed(id: &str, zoneinfo_dir: &Path) -> Option<Self> {
        let valid = id.len() <= ANDROID_ZONE_ID_MAX_LEN
            && id.split('/').all(|part| !part.is_empty())
            && id
                .bytes()
                .all(|b| b.is_ascii_alphanumeric() || matches!(b, b'/' | b'_' | b'+' | b'-'))
            && is_tzif(&zoneinfo_dir.join(id));
        valid.then(|| Self(id.to_owned()))
    }

    pub(crate) fn id(&self) -> &str {
        &self.0
    }
}

fn is_tzif(path: &Path) -> bool {
    let mut magic = [0; TZIF_MAGIC.len()];
    std::fs::File::open(path)
        .and_then(|mut file| file.read_exact(&mut magic))
        .is_ok_and(|()| magic == TZIF_MAGIC)
}

#[cfg(test)]
pub(crate) mod tests {
    use super::*;

    pub(crate) fn zoneinfo_with(tag: &str, zones: &[&str]) -> PathBuf {
        let dir =
            std::env::temp_dir().join(format!("eclipse-zoneinfo-{tag}-{}", std::process::id()));
        std::fs::remove_dir_all(&dir).ok();
        std::fs::create_dir_all(&dir).expect("create the zoneinfo directory");
        for zone in zones {
            let file = dir.join(zone);
            std::fs::create_dir_all(file.parent().expect("a zone file has a parent"))
                .expect("create the zone's directory");
            std::fs::write(&file, b"TZif2").expect("write the zone file");
        }
        dir
    }

    fn detect(
        zoneinfo_dir: &Path,
        tz: Option<&str>,
        localtime_target: Option<&str>,
        timezone_file: Option<&str>,
    ) -> Option<String> {
        HostTimeZone::from_sources(
            zoneinfo_dir,
            tz.map(OsString::from),
            localtime_target.map(PathBuf::from),
            timezone_file.map(str::to_owned),
        )
        .map(|zone| zone.id().to_owned())
    }

    #[test]
    fn localtime_symlinks_name_the_zone_below_zoneinfo() {
        let zoneinfo = zoneinfo_with(
            "localtime",
            &[
                "America/Chicago",
                "Europe/Paris",
                "Asia/Kolkata",
                "Australia/Lord_Howe",
                "Etc/GMT+5",
                "UTC",
            ],
        );
        for (target, expected) in [
            ("../usr/share/zoneinfo/America/Chicago", "America/Chicago"),
            ("/usr/share/zoneinfo/Europe/Paris", "Europe/Paris"),
            ("/etc/zoneinfo/Asia/Kolkata", "Asia/Kolkata"),
            (
                "/usr/share/zoneinfo/posix/Australia/Lord_Howe",
                "Australia/Lord_Howe",
            ),
            ("/usr/share/zoneinfo/right/Etc/GMT+5", "Etc/GMT+5"),
            ("/usr/share/zoneinfo/UTC", "UTC"),
        ] {
            assert_eq!(
                detect(&zoneinfo, None, Some(target), None).as_deref(),
                Some(expected),
                "{target}"
            );
        }
        std::fs::remove_dir_all(&zoneinfo).ok();
    }

    #[test]
    fn tz_takes_precedence_over_the_system_zone() {
        let zoneinfo = zoneinfo_with(
            "tz",
            &[
                "Europe/Paris",
                "America/Los_Angeles",
                "Asia/Tokyo",
                "America/Sao_Paulo",
            ],
        );
        let system = Some("/usr/share/zoneinfo/Europe/Paris");
        for (tz, expected) in [
            ("America/Los_Angeles", "America/Los_Angeles"),
            (":Asia/Tokyo", "Asia/Tokyo"),
            ("/usr/share/zoneinfo/America/Sao_Paulo", "America/Sao_Paulo"),
        ] {
            assert_eq!(
                detect(&zoneinfo, Some(tz), system, None).as_deref(),
                Some(expected),
                "{tz}"
            );
        }
        std::fs::remove_dir_all(&zoneinfo).ok();
    }

    #[test]
    fn an_empty_tz_selects_utc_as_glibc_does() {
        let zoneinfo = zoneinfo_with("empty-tz", &["Europe/Paris"]);
        let system = Some("/usr/share/zoneinfo/Europe/Paris");
        for tz in ["", ":"] {
            assert_eq!(
                detect(&zoneinfo, Some(tz), system, None).as_deref(),
                Some("UTC"),
                "{tz:?}"
            );
        }
        std::fs::remove_dir_all(&zoneinfo).ok();
    }

    #[test]
    fn tz_values_that_name_no_installed_zone_fall_back_to_the_system_zone() {
        let zoneinfo = zoneinfo_with("unusable-tz", &["Europe/Paris", "America/Chicago"]);
        std::fs::write(zoneinfo.join("leapseconds"), b"#\tLeap\t2016\tDec\t31")
            .expect("write a non-TZif zoneinfo file");
        let system = Some("/usr/share/zoneinfo/Europe/Paris");
        for tz in [
            "JST-9",
            "EST5",
            "CET-1CEST,M3.5.0,M10.5.0/3",
            "<+03>-3",
            "../etc/passwd",
            "Asia/Tokyo",
            "/usr/share/zoneinfo/Asia/Tokyo",
            "America",
            "leapseconds",
        ] {
            assert_eq!(
                detect(&zoneinfo, Some(tz), system, None).as_deref(),
                Some("Europe/Paris"),
                "{tz:?}"
            );
        }
        std::fs::remove_dir_all(&zoneinfo).ok();
    }

    #[test]
    fn the_timezone_file_is_trimmed_when_localtime_names_no_installed_zone() {
        let zoneinfo = zoneinfo_with("timezone-file", &["America/Chicago", "Europe/Berlin"]);
        for (localtime_target, timezone_file, expected) in [
            (None, "America/Chicago\n", "America/Chicago"),
            (
                Some("/etc/localtime.real"),
                " Europe/Berlin \n",
                "Europe/Berlin",
            ),
            (
                Some("/usr/share/zoneinfo/Europe/Paris"),
                "Europe/Berlin\n",
                "Europe/Berlin",
            ),
        ] {
            assert_eq!(
                detect(&zoneinfo, None, localtime_target, Some(timezone_file)).as_deref(),
                Some(expected),
                "{localtime_target:?}"
            );
        }
        std::fs::remove_dir_all(&zoneinfo).ok();
    }

    #[test]
    fn no_usable_source_means_no_host_time_zone() {
        let zoneinfo = zoneinfo_with("none", &["Europe/Paris"]);
        assert_eq!(detect(&zoneinfo, None, None, None), None);
        assert_eq!(detect(&zoneinfo, None, None, Some("\n")), None);
        assert_eq!(
            detect(
                &zoneinfo,
                None,
                Some("/usr/share/zoneinfo/"),
                Some("Not a zone!")
            ),
            None
        );
        assert_eq!(detect(&zoneinfo, None, None, Some("Asia/Tokyo")), None);
        assert_eq!(
            detect(
                &zoneinfo,
                Some("America/An_Id_Longer_Than_Android_Allows_X"),
                None,
                None
            ),
            None
        );
        std::fs::remove_dir_all(&zoneinfo).ok();
    }
}
