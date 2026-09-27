use std::ffi::OsString;

use crate::loader::ndk_registry::ConfigurationLocale;

const POSIX_LOCALE_PRECEDENCE: [&str; 3] = ["LC_ALL", "LC_MESSAGES", "LANG"];

const LANGUAGE_PACK_BASE: u8 = b'a';

const REGION_PACK_BASE: u8 = b'0';

#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct HostLocale {
    language: String,
    region: Option<String>,
}

impl HostLocale {
    pub(crate) fn from_env(var: impl Fn(&str) -> Option<OsString>) -> Option<Self> {
        let value = |name: &str| {
            var(name)?
                .into_string()
                .ok()
                .filter(|value| !value.is_empty())
        };
        let posix_locales = || {
            POSIX_LOCALE_PRECEDENCE
                .iter()
                .filter_map(|&name| value(name))
        };
        let preferred = value("LANGUAGE").and_then(|list| {
            list.split(':')
                .next()
                .filter(|entry| !entry.is_empty())
                .map(str::to_owned)
        });
        let Some(preferred) = preferred else {
            return Self::parse(&posix_locales().next()?);
        };
        let mut locale = Self::parse(&preferred)?;
        if locale.region.is_none() {
            locale.region = posix_locales()
                .filter_map(|name| Self::parse(&name))
                .filter(|posix| posix.language == locale.language)
                .find_map(|posix| posix.region);
        }
        Some(locale)
    }

    fn parse(name: &str) -> Option<Self> {
        let name = name.split(['.', '@']).next().unwrap_or_default();
        let (language, region) = match name.split_once('_') {
            Some((language, region)) => (language, Some(region)),
            None => (name, None),
        };
        let language = parse_language(language)?;
        let region = match region {
            Some(region) => Some(parse_region(region)?),
            None => None,
        };
        Some(Self { language, region })
    }

    pub(crate) fn language_tag(&self) -> String {
        match &self.region {
            Some(region) => format!("{}-{region}", self.language),
            None => self.language.clone(),
        }
    }

    pub(crate) fn configuration_locale(&self) -> ConfigurationLocale {
        ConfigurationLocale {
            language: pack_subtag(&self.language, LANGUAGE_PACK_BASE),
            country: self
                .region
                .as_deref()
                .map_or([0; 2], |region| pack_subtag(region, REGION_PACK_BASE)),
        }
    }
}

fn parse_language(raw: &str) -> Option<String> {
    ((2..=3).contains(&raw.len()) && raw.bytes().all(|b| b.is_ascii_alphabetic()))
        .then(|| raw.to_ascii_lowercase())
}

fn parse_region(raw: &str) -> Option<String> {
    match raw.len() {
        2 if raw.bytes().all(|b| b.is_ascii_alphabetic()) => Some(raw.to_ascii_uppercase()),
        3 if raw.bytes().all(|b| b.is_ascii_digit()) => Some(raw.to_owned()),
        _ => None,
    }
}

fn pack_subtag(subtag: &str, base: u8) -> [u8; 2] {
    match *subtag.as_bytes() {
        [first, second] => [first, second],
        [first, second, third] => {
            let [first, second, third] =
                [first, second, third].map(|b| b.wrapping_sub(base) & 0x7f);
            [0x80 | (third << 2) | (second >> 3), (second << 5) | first]
        }
        _ => [0; 2],
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn from_pairs(pairs: &[(&str, &str)]) -> Option<HostLocale> {
        HostLocale::from_env(|name| {
            pairs
                .iter()
                .find(|(key, _)| *key == name)
                .map(|(_, value)| OsString::from(value))
        })
    }

    fn tag(pairs: &[(&str, &str)]) -> Option<String> {
        from_pairs(pairs).map(|locale| locale.language_tag())
    }

    #[test]
    fn posix_locale_names_become_bcp47_language_tags() {
        assert_eq!(
            tag(&[("LANG", "fr_FR.UTF-8@euro")]).as_deref(),
            Some("fr-FR")
        );
        assert_eq!(tag(&[("LANG", "de_DE")]).as_deref(), Some("de-DE"));
        assert_eq!(tag(&[("LANG", "pt")]).as_deref(), Some("pt"));
        assert_eq!(tag(&[("LANG", "sr_RS@latin")]).as_deref(), Some("sr-RS"));
        assert_eq!(tag(&[("LANG", "fil_PH.UTF-8")]).as_deref(), Some("fil-PH"));
        assert_eq!(tag(&[("LANG", "es_419")]).as_deref(), Some("es-419"));
        assert_eq!(tag(&[("LANG", "EN_us")]).as_deref(), Some("en-US"));
    }

    #[test]
    fn c_posix_empty_and_malformed_names_are_no_host_locale() {
        for value in [
            "C", "C.UTF-8", "POSIX", "", "english", "en_USA", "e1_US", "en_U1",
        ] {
            assert_eq!(tag(&[("LANG", value)]), None, "{value:?}");
        }
        assert_eq!(tag(&[]), None);
    }

    #[test]
    fn locale_variables_follow_desktop_precedence() {
        let all = [
            ("LANG", "de_DE.UTF-8"),
            ("LC_MESSAGES", "it_IT.UTF-8"),
            ("LC_ALL", "fr_FR.UTF-8"),
            ("LANGUAGE", "pt_BR:en"),
        ];
        assert_eq!(tag(&all).as_deref(), Some("pt-BR"));
        assert_eq!(tag(&all[..3]).as_deref(), Some("fr-FR"));
        assert_eq!(tag(&all[..2]).as_deref(), Some("it-IT"));
        assert_eq!(tag(&all[..1]).as_deref(), Some("de-DE"));
        assert_eq!(
            tag(&[("LANGUAGE", ""), ("LANG", "de_DE.UTF-8")]).as_deref(),
            Some("de-DE")
        );
        assert_eq!(tag(&[("LC_ALL", "C"), ("LANG", "de_DE.UTF-8")]), None);
    }

    #[test]
    fn a_language_list_entry_without_region_takes_the_matching_locale_region() {
        assert_eq!(
            tag(&[("LANGUAGE", "de:en"), ("LANG", "de_DE.UTF-8")]).as_deref(),
            Some("de-DE")
        );
        assert_eq!(
            tag(&[
                ("LANGUAGE", "de"),
                ("LC_MESSAGES", "de_AT.UTF-8"),
                ("LANG", "de_DE.UTF-8")
            ])
            .as_deref(),
            Some("de-AT")
        );
        assert_eq!(
            tag(&[
                ("LANGUAGE", "de"),
                ("LC_ALL", "en_US.UTF-8"),
                ("LANG", "de_CH.UTF-8")
            ])
            .as_deref(),
            Some("de-CH")
        );
        assert_eq!(
            tag(&[("LANGUAGE", "de"), ("LANG", "en_US.UTF-8")]).as_deref(),
            Some("de")
        );
        assert_eq!(
            tag(&[("LANGUAGE", "pt_PT:pt"), ("LANG", "pt_BR.UTF-8")]).as_deref(),
            Some("pt-PT")
        );
    }

    #[test]
    fn configuration_locale_packs_like_aosp_resource_config() {
        let fr = from_pairs(&[("LANG", "fr_FR.UTF-8")]).expect("fr_FR parses");
        assert_eq!(
            fr.configuration_locale(),
            ConfigurationLocale {
                language: *b"fr",
                country: *b"FR"
            }
        );
        let pt = from_pairs(&[("LANG", "pt")]).expect("pt parses");
        assert_eq!(pt.configuration_locale().country, [0; 2]);
        let fil = from_pairs(&[("LANG", "fil_PH")]).expect("fil_PH parses");
        assert_eq!(fil.configuration_locale().language, [0xad, 0x05]);
        let es = from_pairs(&[("LANG", "es_419")]).expect("es_419 parses");
        assert_eq!(es.configuration_locale().country, [0xa4, 0x24]);
    }
}
