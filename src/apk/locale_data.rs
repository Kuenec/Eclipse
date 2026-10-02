#![forbid(unsafe_code)]

use std::cmp::Ordering;
use std::collections::{HashMap, HashSet};
use std::sync::OnceLock;

const NO_REGION: [u8; 2] = [0, 0];
const LATIN: [u8; 4] = *b"Latn";
const ROOT: Locale = Locale {
    language: [0, 0],
    region: NO_REGION,
};
const ENGLISH: Locale = Locale {
    language: *b"en",
    region: NO_REGION,
};
const INTERNATIONAL_ENGLISH: Locale = Locale {
    language: *b"en",
    region: [0x84, 0x00],
};
const LATIN_AMERICAN_SPANISH: Locale = Locale {
    language: *b"es",
    region: [0xA4, 0x24],
};
const US_SPANISH: Locale = Locale {
    language: *b"es",
    region: *b"US",
};
const MEXICAN_SPANISH: Locale = Locale {
    language: *b"es",
    region: *b"MX",
};

const LIKELY_SCRIPTS: &[([u8; 4], &[&str])] = &[
    (
        *b"Arab",
        &[
            "ar", "az-IQ", "az-IR", "fa", "ha-CM", "ha-SD", "kk-AF", "kk-CN", "kk-IR", "kk-MN",
            "ks", "ku-LB", "ky-CN", "ms-CC", "pa-PK", "ps", "sd", "tg-PK", "ug", "ur", "uz-AF",
        ],
    ),
    (*b"Armn", &["hy"]),
    (*b"Avst", &["ae"]),
    (*b"Beng", &["as", "bn"]),
    (*b"Cans", &["cr", "iu", "oj"]),
    (
        *b"Cyrl",
        &[
            "ab", "av", "az-RU", "ba", "be", "bg", "ce", "cu", "cv", "kk", "kv", "ky", "mk", "mn",
            "os", "ru", "sr", "tg", "tt", "ug-KZ", "ug-MN", "uk", "uz-CN",
        ],
    ),
    (*b"Deva", &["hi", "mr", "ne", "sa", "sd-IN"]),
    (*b"Ethi", &["am", "ti"]),
    (*b"Geor", &["ka"]),
    (*b"Grek", &["el"]),
    (*b"Gujr", &["gu"]),
    (*b"Guru", &["pa"]),
    (*b"Hans", &["zh"]),
    (
        *b"Hant",
        &[
            "zh-AU", "zh-BN", "zh-GB", "zh-GF", "zh-HK", "zh-ID", "zh-MO", "zh-PA", "zh-PF",
            "zh-PH", "zh-SR", "zh-TH", "zh-TW", "zh-US", "zh-VN",
        ],
    ),
    (*b"Hebr", &["he", "iw", "ji", "yi"]),
    (*b"Jpan", &["ja"]),
    (*b"Khmr", &["km"]),
    (*b"Knda", &["kn"]),
    (*b"Kore", &["ko"]),
    (*b"Laoo", &["lo"]),
    (
        *b"Latn",
        &[
            "aa", "af", "ak", "an", "ay", "az", "bi", "bm", "br", "bs", "ca", "ch", "co", "cs",
            "cy", "da", "de", "ee", "en", "eo", "es", "et", "eu", "ff", "fi", "fj", "fo", "fr",
            "fy", "ga", "gd", "gl", "gn", "gv", "ha", "ho", "hr", "ht", "hu", "hz", "ia", "id",
            "ie", "ig", "ik", "in", "io", "is", "it", "jv", "jw", "kg", "ki", "kj", "kl", "kr",
            "ku", "kw", "ky-TR", "la", "lb", "lg", "li", "ln", "lt", "lu", "lv", "mg", "mh", "mi",
            "mo", "ms", "mt", "na", "nb", "nd", "ng", "nl", "nn", "no", "nr", "nv", "ny", "oc",
            "om", "pl", "pt", "qu", "rm", "rn", "ro", "rw", "sc", "se", "sg", "sk", "sl", "sm",
            "sn", "so", "sq", "sr-ME", "sr-RO", "sr-RU", "sr-TR", "ss", "st", "su", "sv", "sw",
            "tk", "tl", "tn", "to", "tr", "ts", "ty", "uz", "ve", "vi", "vo", "wa", "wo", "xh",
            "yo", "za", "zu",
        ],
    ),
    (*b"Mlym", &["ml"]),
    (*b"Mong", &["mn-CN"]),
    (*b"Mymr", &["my"]),
    (*b"Orya", &["or"]),
    (*b"Sinh", &["pi", "si"]),
    (*b"Taml", &["ta"]),
    (*b"Telu", &["te"]),
    (*b"Thaa", &["dv"]),
    (*b"Thai", &["th"]),
    (*b"Tibt", &["bo", "dz"]),
    (*b"Yiii", &["ii"]),
    (*b"~~~A", &["en-XA"]),
    (*b"~~~B", &["ar-XB"]),
];

const PARENTS: &[([u8; 4], &str, &[&str])] = &[
    (
        *b"Arab",
        "ar-015",
        &["ar-AE", "ar-DZ", "ar-EH", "ar-LY", "ar-MA", "ar-TN"],
    ),
    (*b"Hant", "zh-HK", &["zh-MO"]),
    (
        *b"Latn",
        "en-001",
        &[
            "en-150", "en-AG", "en-AI", "en-AU", "en-BB", "en-BM", "en-BS", "en-BW", "en-BZ",
            "en-CC", "en-CK", "en-CM", "en-CX", "en-CY", "en-DG", "en-DM", "en-ER", "en-FJ",
            "en-FK", "en-FM", "en-GB", "en-GD", "en-GG", "en-GH", "en-GI", "en-GM", "en-GY",
            "en-HK", "en-ID", "en-IE", "en-IL", "en-IM", "en-IN", "en-IO", "en-JE", "en-JM",
            "en-KE", "en-KI", "en-KN", "en-KY", "en-LC", "en-LR", "en-LS", "en-MG", "en-MO",
            "en-MS", "en-MT", "en-MU", "en-MV", "en-MW", "en-MY", "en-NA", "en-NF", "en-NG",
            "en-NR", "en-NU", "en-NZ", "en-PG", "en-PK", "en-PN", "en-PW", "en-RW", "en-SB",
            "en-SC", "en-SD", "en-SG", "en-SH", "en-SL", "en-SS", "en-SX", "en-SZ", "en-TC",
            "en-TK", "en-TO", "en-TT", "en-TV", "en-TZ", "en-UG", "en-VC", "en-VG", "en-VU",
            "en-WS", "en-ZA", "en-ZM", "en-ZW",
        ],
    ),
    (
        *b"Latn",
        "en-150",
        &[
            "en-AT", "en-BE", "en-CH", "en-CZ", "en-DE", "en-DK", "en-ES", "en-FI", "en-FR",
            "en-HU", "en-IT", "en-NL", "en-NO", "en-PL", "en-PT", "en-RO", "en-SE", "en-SI",
            "en-SK",
        ],
    ),
    (
        *b"Latn",
        "es-419",
        &[
            "es-AR", "es-BO", "es-BR", "es-BZ", "es-CL", "es-CO", "es-CR", "es-CU", "es-DO",
            "es-EC", "es-GT", "es-HN", "es-MX", "es-NI", "es-PA", "es-PE", "es-PR", "es-PY",
            "es-SV", "es-US", "es-UY", "es-VE",
        ],
    ),
    (
        *b"Latn",
        "pt-PT",
        &[
            "pt-AO", "pt-CH", "pt-CV", "pt-GQ", "pt-GW", "pt-LU", "pt-MO", "pt-MZ", "pt-ST",
            "pt-TL",
        ],
    ),
    (*b"~~~B", "ar-015", &["ar-XB"]),
];

const REPRESENTATIVE_LOCALES: &[([u8; 4], &[&str])] = &[
    (
        *b"Latn",
        &[
            "aa-ET", "af-ZA", "ak-GH", "an-ES", "ay-BO", "az-AZ", "bi-VU", "bm-ML", "br-FR",
            "bs-BA", "ca-ES", "ch-GU", "co-FR", "cs-CZ", "cy-GB", "da-DK", "de-DE", "ee-GH",
            "en-GB", "en-US", "es-ES", "es-MX", "es-US", "et-EE", "eu-ES", "ff-SN", "fi-FI",
            "fj-FJ", "fo-FO", "fr-FR", "fy-NL", "ga-IE", "gd-GB", "gl-ES", "gn-PY", "gv-IM",
            "ha-NG", "ho-PG", "hr-HR", "ht-HT", "hu-HU", "hz-NA", "id-ID", "ie-EE", "ig-NG",
            "ik-US", "in-ID", "is-IS", "it-IT", "jv-ID", "jw-ID", "kg-CD", "ki-KE", "kj-NA",
            "kl-GL", "kr-NG", "ku-TR", "kw-GB", "ky-TR", "la-VA", "lb-LU", "lg-UG", "li-NL",
            "ln-CD", "lt-LT", "lu-CD", "lv-LV", "mg-MG", "mh-MH", "mi-NZ", "mo-RO", "ms-MY",
            "mt-MT", "na-NR", "nb-NO", "nd-ZW", "ng-NA", "nl-NL", "nn-NO", "no-NO", "nr-ZA",
            "nv-US", "ny-MW", "oc-FR", "om-ET", "pl-PL", "pt-BR", "qu-PE", "rm-CH", "rn-BI",
            "ro-RO", "rw-RW", "sc-IT", "se-NO", "sg-CF", "sk-SK", "sl-SI", "sm-WS", "sn-ZW",
            "so-SO", "sq-AL", "ss-ZA", "st-ZA", "su-ID", "sv-SE", "sw-TZ", "tk-TM", "tl-PH",
            "tn-ZA", "to-TO", "tr-TR", "ts-ZA", "ty-PF", "uz-UZ", "ve-ZA", "vi-VN", "wa-BE",
            "wo-SN", "xh-ZA", "yo-NG", "za-CN", "zu-ZA",
        ],
    ),
    (
        *b"Cyrl",
        &[
            "ab-GE", "av-RU", "ba-RU", "be-BY", "bg-BG", "ce-RU", "cu-RU", "cv-RU", "kk-KZ",
            "kv-RU", "ky-KG", "mk-MK", "mn-MN", "os-GE", "ru-RU", "sr-RS", "tg-TJ", "tt-RU",
            "ug-KZ", "uk-UA",
        ],
    ),
    (*b"Avst", &["ae-IR"]),
    (*b"Ethi", &["am-ET", "ti-ET"]),
    (
        *b"Arab",
        &[
            "ar-EG", "az-IR", "fa-IR", "kk-CN", "ks-IN", "ku-IQ", "ky-CN", "pa-PK", "ps-AF",
            "sd-PK", "tg-PK", "ug-CN", "ur-PK", "uz-AF",
        ],
    ),
    (*b"Beng", &["as-IN", "bn-BD"]),
    (*b"Tibt", &["bo-CN", "dz-BT"]),
    (*b"Cans", &["cr-CA", "iu-CA", "oj-CA"]),
    (*b"Glag", &["cu-BG"]),
    (*b"Thaa", &["dv-MV"]),
    (*b"Grek", &["el-GR"]),
    (*b"Shaw", &["en-GB"]),
    (*b"Adlm", &["ff-GN"]),
    (*b"Gujr", &["gu-IN"]),
    (*b"Hebr", &["he-IL", "iw-IL", "ji-UA", "yi-UA"]),
    (*b"Deva", &["hi-IN", "mr-IN", "ne-NP", "sa-IN", "sd-IN"]),
    (*b"Armn", &["hy-AM"]),
    (*b"Yiii", &["ii-CN"]),
    (*b"Jpan", &["ja-JP"]),
    (*b"Geor", &["ka-GE"]),
    (*b"Khmr", &["km-KH"]),
    (*b"Knda", &["kn-IN"]),
    (*b"Kore", &["ko-KR"]),
    (*b"Yezi", &["ku-GE"]),
    (*b"Laoo", &["lo-LA"]),
    (*b"Mlym", &["ml-IN"]),
    (*b"Mong", &["mn-CN"]),
    (*b"Mymr", &["my-MM"]),
    (*b"Orya", &["or-IN"]),
    (*b"Guru", &["pa-IN"]),
    (*b"Sinh", &["pi-IN", "si-LK"]),
    (*b"Khoj", &["sd-IN"]),
    (*b"Sind", &["sd-IN"]),
    (*b"Taml", &["ta-IN"]),
    (*b"Telu", &["te-IN"]),
    (*b"Thai", &["th-TH"]),
    (*b"Bopo", &["zh-TW"]),
    (*b"Hanb", &["zh-TW"]),
    (*b"Hans", &["zh-CN"]),
    (*b"Hant", &["zh-TW"]),
];

#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash)]
struct Locale {
    language: [u8; 2],
    region: [u8; 2],
}

impl Locale {
    fn parse(tag: &str) -> Option<Self> {
        let (language, region) = tag.split_once('-').unwrap_or((tag, ""));
        Some(Self {
            language: pack_language(language)?,
            region: if region.is_empty() {
                NO_REGION
            } else {
                pack_region(region)?
            },
        })
    }

    fn of_table(tag: &str) -> Self {
        Self::parse(tag).unwrap_or_else(|| panic!("locale table tag {tag} is malformed"))
    }

    fn without_region(self) -> Self {
        Self {
            region: NO_REGION,
            ..self
        }
    }
}

struct Tables {
    likely_scripts: HashMap<Locale, [u8; 4]>,
    parents: HashMap<([u8; 4], Locale), Locale>,
    representatives: HashSet<([u8; 4], Locale)>,
}

fn tables() -> &'static Tables {
    static TABLES: OnceLock<Tables> = OnceLock::new();
    TABLES.get_or_init(|| Tables {
        likely_scripts: LIKELY_SCRIPTS
            .iter()
            .flat_map(|&(script, tags)| tags.iter().map(move |tag| (Locale::of_table(tag), script)))
            .collect(),
        parents: PARENTS
            .iter()
            .flat_map(|&(script, parent, children)| {
                children
                    .iter()
                    .map(move |child| ((script, Locale::of_table(child)), Locale::of_table(parent)))
            })
            .collect(),
        representatives: REPRESENTATIVE_LOCALES
            .iter()
            .flat_map(|&(script, tags)| tags.iter().map(move |tag| (script, Locale::of_table(tag))))
            .collect(),
    })
}

pub(super) fn pack_language(subtag: &str) -> Option<[u8; 2]> {
    pack_subtag(subtag, b'a', u8::is_ascii_lowercase)
}

pub(super) fn pack_region(subtag: &str) -> Option<[u8; 2]> {
    pack_subtag(subtag, b'0', u8::is_ascii_uppercase)
        .or_else(|| pack_subtag(subtag, b'0', u8::is_ascii_digit))
}

fn pack_subtag(subtag: &str, base: u8, accepts: fn(&u8) -> bool) -> Option<[u8; 2]> {
    let bytes = subtag.as_bytes();
    if !bytes.iter().all(accepts) {
        return None;
    }
    match *bytes {
        [first, second] => Some([first, second]),
        [first, second, third] => {
            let packed = |c: u8| (c - base) & 0x7F;
            Some([
                0x80 | (packed(third) << 2) | (packed(second) >> 3),
                (packed(second) << 5) | packed(first),
            ])
        }
        _ => None,
    }
}

pub(super) fn likely_script(language: [u8; 2], region: [u8; 2]) -> Option<[u8; 4]> {
    if language == [0, 0] {
        return None;
    }
    let locale = Locale { language, region };
    let likely_scripts = &tables().likely_scripts;
    likely_scripts
        .get(&locale)
        .or_else(|| likely_scripts.get(&locale.without_region()))
        .copied()
}

fn parent(locale: Locale, script: [u8; 4]) -> Option<Locale> {
    if locale.region == NO_REGION {
        return None;
    }
    let parent = tables()
        .parents
        .get(&(script, locale))
        .copied()
        .unwrap_or(locale.without_region());
    (parent != ROOT).then_some(parent)
}

fn ancestors(locale: Locale, script: [u8; 4]) -> impl Iterator<Item = Locale> {
    std::iter::successors(Some(locale), move |&ancestor| parent(ancestor, script))
}

fn is_special_spanish(locale: Locale) -> bool {
    locale == US_SPANISH || locale == MEXICAN_SPANISH
}

pub(super) fn compare_regions(
    left_region: [u8; 2],
    right_region: [u8; 2],
    requested_language: [u8; 2],
    requested_script: [u8; 4],
    requested_region: [u8; 2],
) -> Ordering {
    if left_region == right_region {
        return Ordering::Equal;
    }
    let in_requested_language = |region| Locale {
        language: requested_language,
        region,
    };
    let mut left = in_requested_language(left_region);
    let mut right = in_requested_language(right_region);
    let left_is_special_spanish = is_special_spanish(left);
    let right_is_special_spanish = is_special_spanish(right);
    if left_is_special_spanish && !right_is_special_spanish && right != LATIN_AMERICAN_SPANISH {
        left = LATIN_AMERICAN_SPANISH;
    } else if right_is_special_spanish && !left_is_special_spanish && left != LATIN_AMERICAN_SPANISH
    {
        right = LATIN_AMERICAN_SPANISH;
    }

    let mut request_ancestors = Vec::new();
    for ancestor in ancestors(in_requested_language(requested_region), requested_script) {
        if ancestor == left {
            return Ordering::Greater;
        }
        if ancestor == right {
            return Ordering::Less;
        }
        request_ancestors.push(ancestor);
    }

    let distance = |supported| {
        ancestors(supported, requested_script)
            .enumerate()
            .find_map(|(steps, ancestor)| {
                request_ancestors
                    .iter()
                    .position(|&requested| requested == ancestor)
                    .map(|index| steps + index)
            })
            .unwrap_or(usize::MAX)
    };
    let is_representative = |locale| {
        tables()
            .representatives
            .contains(&(requested_script, locale))
    };
    distance(right)
        .cmp(&distance(left))
        .then_with(|| is_representative(left).cmp(&is_representative(right)))
        .then_with(|| right.cmp(&left))
}

pub(super) fn is_close_to_us_english(region: [u8; 2]) -> bool {
    ancestors(
        Locale {
            language: ENGLISH.language,
            region,
        },
        LATIN,
    )
    .find(|&ancestor| ancestor == ENGLISH || ancestor == INTERNATIONAL_ENGLISH)
        == Some(ENGLISH)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn named_locales_pack_like_their_tags() {
        assert_eq!(Locale::parse("en"), Some(ENGLISH));
        assert_eq!(Locale::parse("en-001"), Some(INTERNATIONAL_ENGLISH));
        assert_eq!(Locale::parse("es-419"), Some(LATIN_AMERICAN_SPANISH));
        assert_eq!(Locale::parse("es-US"), Some(US_SPANISH));
        assert_eq!(Locale::parse("es-MX"), Some(MEXICAN_SPANISH));
    }

    #[test]
    fn the_tables_parse_and_every_ancestry_ends() {
        for &(script, child) in tables().parents.keys() {
            let ancestry: Vec<_> = ancestors(child, script).take(5).collect();
            assert!(ancestry.len() <= 4, "{ancestry:?} is too deep");
            assert_eq!(ancestry.last(), Some(&child.without_region()));
        }
    }

    #[test]
    fn regions_rank_by_their_distance_in_the_parent_tree() {
        let es = *b"es";
        let region = |tag| Locale::of_table(tag).region;
        assert_eq!(
            compare_regions(region("es-419"), NO_REGION, es, LATIN, region("es-MX")),
            Ordering::Greater
        );
        assert_eq!(
            compare_regions(region("es-US"), NO_REGION, es, LATIN, region("es-AR")),
            Ordering::Greater
        );
        assert_eq!(
            compare_regions(*b"AU", *b"GB", *b"en", LATIN, *b"NZ"),
            Ordering::Less
        );
        assert!(is_close_to_us_english(*b"CA"));
        assert!(!is_close_to_us_english(*b"GB"));
    }
}
