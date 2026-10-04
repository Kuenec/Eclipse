use std::fmt;

pub const INFO_PATH: &str = "/.flatpak-info";

pub fn info_value<'a>(info: &'a str, group: &str, key: &str) -> Option<&'a str> {
    let mut in_group = false;
    for line in info.lines().map(str::trim) {
        if let Some(name) = line.strip_prefix('[') {
            in_group = name.strip_suffix(']') == Some(group);
        } else if in_group {
            match line.split_once('=') {
                Some((name, value)) if name.trim() == key => {
                    return Some(value.trim()).filter(|value| !value.is_empty());
                }
                _ => {}
            }
        }
    }
    None
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord)]
pub struct Version {
    major: u32,
    minor: u32,
    micro: u32,
}

impl Version {
    #[must_use]
    pub const fn new(major: u32, minor: u32, micro: u32) -> Self {
        Self {
            major,
            minor,
            micro,
        }
    }

    pub fn of_instance(info: &str) -> Option<Self> {
        let text = info_value(info, "Instance", "flatpak-version")?;
        let mut parts = text.splitn(3, '.').map(str::parse::<u32>);
        let major = parts.next()?.ok()?;
        let minor = parts.next()?.ok()?;
        let micro = parts.next().unwrap_or(Ok(0)).ok()?;
        Some(Self::new(major, minor, micro))
    }
}

impl fmt::Display for Version {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "{}.{}.{}", self.major, self.minor, self.micro)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    const INFO: &str = "[Application]\n\
                        name=io.github.kuenec.Eclipse\n\
                        runtime=runtime/org.gnome.Platform/x86_64/51\n\
                        \n\
                        [Instance]\n\
                        instance-id=1234\n\
                        branch = stable\n\
                        arch=x86_64\n\
                        app-commit=0123456789abcdef\n\
                        runtime-commit=fedcba9876543210\n\
                        flatpak-version=1.16.1\n";

    #[test]
    fn values_are_read_from_their_own_group() {
        for (group, key, value) in [
            ("Application", "name", Some("io.github.kuenec.Eclipse")),
            (
                "Application",
                "runtime",
                Some("runtime/org.gnome.Platform/x86_64/51"),
            ),
            ("Instance", "branch", Some("stable")),
            ("Instance", "arch", Some("x86_64")),
            ("Instance", "app-commit", Some("0123456789abcdef")),
            ("Instance", "runtime-commit", Some("fedcba9876543210")),
            ("Instance", "flatpak-version", Some("1.16.1")),
            ("Instance", "name", None),
            ("Application", "branch", None),
        ] {
            assert_eq!(info_value(INFO, group, key), value, "{group} {key}");
        }
        assert_eq!(
            info_value("[Application]\nname=\n", "Application", "name"),
            None
        );
    }

    #[test]
    fn the_running_flatpak_version_is_parsed_and_ordered() {
        assert_eq!(Version::of_instance(INFO), Some(Version::new(1, 16, 1)));
        let version =
            |text: &str| Version::of_instance(&format!("[Instance]\nflatpak-version={text}\n"));
        assert_eq!(version("1.14.10"), Some(Version::new(1, 14, 10)));
        assert_eq!(version("1.17"), Some(Version::new(1, 17, 0)));
        assert_eq!(version("1.16.0.1"), None);
        assert_eq!(version("1.x.2"), None);
        assert_eq!(version("2"), None);
        assert_eq!(
            Version::of_instance("[Application]\nflatpak-version=1.16.1\n"),
            None
        );
        assert!(Version::new(1, 15, 99) < Version::new(1, 16, 0));
        assert_eq!(Version::new(1, 14, 10).to_string(), "1.14.10");
    }
}
