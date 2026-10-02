#![forbid(unsafe_code)]

use std::cmp::Ordering;

use super::locale_data;

const MASK_LAYOUTDIR: u8 = 0xC0;
const MASK_SCREENSIZE: u8 = 0x0F;
const MASK_SCREENLONG: u8 = 0x30;
const SCREENSIZE_NORMAL: u8 = 0x02;
const MASK_UI_MODE_TYPE: u8 = 0x0F;
const MASK_UI_MODE_NIGHT: u8 = 0x30;
const MASK_SCREENROUND: u8 = 0x03;
const MASK_WIDE_COLOR_GAMUT: u8 = 0x03;
const MASK_HDR: u8 = 0x0C;
const MASK_KEYSHIDDEN: u8 = 0x03;
const MASK_NAVHIDDEN: u8 = 0x0C;
const KEYSHIDDEN_NO: u8 = 1;
const KEYSHIDDEN_SOFT: u8 = 3;
const DENSITY_MEDIUM: u16 = 160;
const DENSITY_ANY: u16 = 0xFFFE;
const NO_SCRIPT: [u8; 4] = [0; 4];

const ENGLISH: [u8; 2] = *b"en";
const UNITED_STATES: [u8; 2] = *b"US";
const TAGALOG: [u8; 2] = *b"tl";
const FILIPINO: [u8; 2] = [0xAD, 0x05];

#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct ResConfig {
    pub mcc: u16,
    pub mnc: u16,
    pub language: [u8; 2],
    pub country: [u8; 2],
    pub orientation: u8,
    pub touchscreen: u8,
    pub density: u16,
    pub keyboard: u8,
    pub navigation: u8,
    pub input_flags: u8,
    pub grammatical_inflection: u8,
    pub screen_width: u16,
    pub screen_height: u16,
    pub sdk_version: u16,
    pub minor_version: u16,
    pub screen_layout: u8,
    pub ui_mode: u8,
    pub smallest_screen_width_dp: u16,
    pub screen_width_dp: u16,
    pub screen_height_dp: u16,
    pub locale_script: [u8; 4],
    pub locale_variant: [u8; 8],
    pub screen_layout2: u8,
    pub color_mode: u8,
    pub locale_script_was_computed: bool,
}

impl ResConfig {
    pub fn parse(bytes: &[u8]) -> Self {
        let declared = bytes
            .get(..4)
            .and_then(|size| <[u8; 4]>::try_from(size).ok())
            .map_or(0, u32::from_le_bytes);
        let usable = usize::try_from(declared).map_or(bytes.len(), |size| size.min(bytes.len()));
        let bytes = &bytes[..usable];
        let u8_at = |offset: usize| bytes.get(offset).copied().unwrap_or(0);
        let u16_at = |offset: usize| u16::from_le_bytes([u8_at(offset), u8_at(offset + 1)]);
        let array_at = |offset: usize| -> [u8; 8] { std::array::from_fn(|i| u8_at(offset + i)) };
        let script = array_at(36);
        Self {
            mcc: u16_at(4),
            mnc: u16_at(6),
            language: [u8_at(8), u8_at(9)],
            country: [u8_at(10), u8_at(11)],
            orientation: u8_at(12),
            touchscreen: u8_at(13),
            density: u16_at(14),
            keyboard: u8_at(16),
            navigation: u8_at(17),
            input_flags: u8_at(18),
            grammatical_inflection: u8_at(19),
            screen_width: u16_at(20),
            screen_height: u16_at(22),
            sdk_version: u16_at(24),
            minor_version: u16_at(26),
            screen_layout: u8_at(28),
            ui_mode: u8_at(29),
            smallest_screen_width_dp: u16_at(30),
            screen_width_dp: u16_at(32),
            screen_height_dp: u16_at(34),
            locale_script: [script[0], script[1], script[2], script[3]],
            locale_variant: array_at(40),
            screen_layout2: u8_at(48),
            color_mode: u8_at(49),
            locale_script_was_computed: false,
        }
    }

    pub fn set_locale(&mut self, tag: &str) {
        self.language = [0, 0];
        self.country = [0, 0];
        self.locale_script = NO_SCRIPT;
        self.locale_variant = [0; 8];
        self.locale_script_was_computed = false;
        let mut subtags = tag.split(['-', '_']);
        let Some(language) = subtags.next().and_then(locale_data::pack_language) else {
            return;
        };
        self.language = language;
        for subtag in subtags {
            let bytes = subtag.as_bytes();
            match bytes.len() {
                2 | 3 => self.country = locale_data::pack_region(subtag).unwrap_or_default(),
                4 if subtag.is_ascii() && !bytes[0].is_ascii_digit() => {
                    self.locale_script = std::array::from_fn(|i| match i {
                        0 => bytes[i].to_ascii_uppercase(),
                        _ => bytes[i].to_ascii_lowercase(),
                    });
                }
                4..=8 if subtag.is_ascii() => {
                    for (slot, byte) in self.locale_variant.iter_mut().zip(bytes) {
                        *slot = byte.to_ascii_lowercase();
                    }
                }
                _ => break,
            }
        }
        self.locale_script_was_computed = self.locale_script == NO_SCRIPT;
        if self.locale_script_was_computed {
            self.locale_script =
                locale_data::likely_script(self.language, self.country).unwrap_or_default();
        }
    }

    fn has_locale(&self) -> bool {
        self.language != [0, 0] || self.country != [0, 0]
    }

    fn has_imsi(&self) -> bool {
        self.mcc != 0 || self.mnc != 0
    }

    fn has_screen_config(&self) -> bool {
        self.screen_layout != 0 || self.ui_mode != 0 || self.smallest_screen_width_dp != 0
    }

    fn has_screen_config2(&self) -> bool {
        self.screen_layout2 != 0 || self.color_mode != 0
    }

    fn has_screen_size_dp(&self) -> bool {
        self.screen_width_dp != 0 || self.screen_height_dp != 0
    }

    fn has_screen_type(&self) -> bool {
        self.orientation != 0 || self.touchscreen != 0 || self.density != 0
    }

    fn has_input(&self) -> bool {
        self.keyboard != 0 || self.navigation != 0 || self.input_flags != 0
    }

    fn has_screen_size(&self) -> bool {
        self.screen_width != 0 || self.screen_height != 0
    }

    fn has_version(&self) -> bool {
        self.sdk_version != 0 || self.minor_version != 0
    }

    pub fn matches(&self, settings: &Self) -> bool {
        if self.has_imsi() {
            if self.mcc != 0 && self.mcc != settings.mcc {
                return false;
            }
            if self.mnc != 0 && self.mnc != settings.mnc {
                return false;
            }
        }
        if self.has_locale() {
            if !languages_are_equivalent(self.language, settings.language) {
                return false;
            }
            let script = if settings.locale_script == NO_SCRIPT {
                None
            } else if self.locale_script == NO_SCRIPT && !self.locale_script_was_computed {
                locale_data::likely_script(self.language, self.country)
            } else {
                Some(self.locale_script)
            };
            match script {
                Some(script) => {
                    if script != settings.locale_script {
                        return false;
                    }
                }
                None => {
                    if self.country != [0, 0] && self.country != settings.country {
                        return false;
                    }
                }
            }
        }
        if self.grammatical_inflection != 0
            && self.grammatical_inflection != settings.grammatical_inflection
        {
            return false;
        }
        if self.has_screen_config() {
            let differs =
                |mask: u8, mine: u8, theirs: u8| mine & mask != 0 && mine & mask != theirs & mask;
            if differs(MASK_LAYOUTDIR, self.screen_layout, settings.screen_layout)
                || differs(MASK_SCREENLONG, self.screen_layout, settings.screen_layout)
                || differs(MASK_UI_MODE_TYPE, self.ui_mode, settings.ui_mode)
                || differs(MASK_UI_MODE_NIGHT, self.ui_mode, settings.ui_mode)
            {
                return false;
            }
            let screen_size = self.screen_layout & MASK_SCREENSIZE;
            if screen_size != 0 && screen_size > settings.screen_layout & MASK_SCREENSIZE {
                return false;
            }
            if self.smallest_screen_width_dp != 0
                && self.smallest_screen_width_dp > settings.smallest_screen_width_dp
            {
                return false;
            }
        }
        if self.has_screen_config2() {
            let differs =
                |mask: u8, mine: u8, theirs: u8| mine & mask != 0 && mine & mask != theirs & mask;
            if differs(
                MASK_SCREENROUND,
                self.screen_layout2,
                settings.screen_layout2,
            ) || differs(MASK_HDR, self.color_mode, settings.color_mode)
                || differs(MASK_WIDE_COLOR_GAMUT, self.color_mode, settings.color_mode)
            {
                return false;
            }
        }
        if self.has_screen_size_dp() {
            if self.screen_width_dp != 0 && self.screen_width_dp > settings.screen_width_dp {
                return false;
            }
            if self.screen_height_dp != 0 && self.screen_height_dp > settings.screen_height_dp {
                return false;
            }
        }
        if self.has_screen_type() {
            if self.orientation != 0 && self.orientation != settings.orientation {
                return false;
            }
            if self.touchscreen != 0 && self.touchscreen != settings.touchscreen {
                return false;
            }
        }
        if self.has_input() {
            let keys_hidden = self.input_flags & MASK_KEYSHIDDEN;
            let settings_keys_hidden = settings.input_flags & MASK_KEYSHIDDEN;
            if keys_hidden != 0
                && keys_hidden != settings_keys_hidden
                && !(keys_hidden == KEYSHIDDEN_NO && settings_keys_hidden == KEYSHIDDEN_SOFT)
            {
                return false;
            }
            let nav_hidden = self.input_flags & MASK_NAVHIDDEN;
            if nav_hidden != 0 && nav_hidden != settings.input_flags & MASK_NAVHIDDEN {
                return false;
            }
            if self.keyboard != 0 && self.keyboard != settings.keyboard {
                return false;
            }
            if self.navigation != 0 && self.navigation != settings.navigation {
                return false;
            }
        }
        if self.has_screen_size() {
            if self.screen_width != 0 && self.screen_width > settings.screen_width {
                return false;
            }
            if self.screen_height != 0 && self.screen_height > settings.screen_height {
                return false;
            }
        }
        if self.has_version() {
            if self.sdk_version != 0 && self.sdk_version > settings.sdk_version {
                return false;
            }
            if self.minor_version != 0 && self.minor_version != settings.minor_version {
                return false;
            }
        }
        true
    }

    pub fn is_better_than(&self, other: &Self, requested: &Self) -> bool {
        if self.has_imsi() || other.has_imsi() {
            if self.mcc != other.mcc && requested.mcc != 0 {
                return self.mcc != 0;
            }
            if self.mnc != other.mnc && requested.mnc != 0 {
                return self.mnc != 0;
            }
        }
        if requested.has_locale()
            && (self.has_locale() || other.has_locale())
            && self.is_locale_better_than(other, requested)
        {
            return true;
        }
        if (self.grammatical_inflection != 0 || other.grammatical_inflection != 0)
            && self.grammatical_inflection != other.grammatical_inflection
            && requested.grammatical_inflection != 0
        {
            return self.grammatical_inflection != 0;
        }
        if (self.screen_layout != 0 || other.screen_layout != 0)
            && (self.screen_layout ^ other.screen_layout) & MASK_LAYOUTDIR != 0
            && requested.screen_layout & MASK_LAYOUTDIR != 0
        {
            return self.screen_layout & MASK_LAYOUTDIR > other.screen_layout & MASK_LAYOUTDIR;
        }
        if self.smallest_screen_width_dp != other.smallest_screen_width_dp {
            return self.smallest_screen_width_dp > other.smallest_screen_width_dp;
        }
        if self.has_screen_size_dp() || other.has_screen_size_dp() {
            let delta = |config: &Self| {
                let mut delta = 0;
                if requested.screen_width_dp != 0 {
                    delta +=
                        i32::from(requested.screen_width_dp) - i32::from(config.screen_width_dp);
                }
                if requested.screen_height_dp != 0 {
                    delta +=
                        i32::from(requested.screen_height_dp) - i32::from(config.screen_height_dp);
                }
                delta
            };
            let (mine, theirs) = (delta(self), delta(other));
            if mine != theirs {
                return mine < theirs;
            }
        }
        if self.screen_layout != 0 || other.screen_layout != 0 {
            if (self.screen_layout ^ other.screen_layout) & MASK_SCREENSIZE != 0
                && requested.screen_layout & MASK_SCREENSIZE != 0
            {
                let mine = self.screen_layout & MASK_SCREENSIZE;
                let theirs = other.screen_layout & MASK_SCREENSIZE;
                let (mut fixed_mine, mut fixed_theirs) = (mine, theirs);
                if requested.screen_layout & MASK_SCREENSIZE >= SCREENSIZE_NORMAL {
                    if fixed_mine == 0 {
                        fixed_mine = SCREENSIZE_NORMAL;
                    }
                    if fixed_theirs == 0 {
                        fixed_theirs = SCREENSIZE_NORMAL;
                    }
                }
                if fixed_mine == fixed_theirs {
                    return mine != 0;
                }
                return fixed_mine > fixed_theirs;
            }
            if (self.screen_layout ^ other.screen_layout) & MASK_SCREENLONG != 0
                && requested.screen_layout & MASK_SCREENLONG != 0
            {
                return self.screen_layout & MASK_SCREENLONG != 0;
            }
        }
        if (self.screen_layout2 ^ other.screen_layout2) & MASK_SCREENROUND != 0
            && requested.screen_layout2 & MASK_SCREENROUND != 0
        {
            return self.screen_layout2 & MASK_SCREENROUND != 0;
        }
        if self.color_mode != 0 || other.color_mode != 0 {
            if (self.color_mode ^ other.color_mode) & MASK_WIDE_COLOR_GAMUT != 0
                && requested.color_mode & MASK_WIDE_COLOR_GAMUT != 0
            {
                return self.color_mode & MASK_WIDE_COLOR_GAMUT != 0;
            }
            if (self.color_mode ^ other.color_mode) & MASK_HDR != 0
                && requested.color_mode & MASK_HDR != 0
            {
                return self.color_mode & MASK_HDR != 0;
            }
        }
        if self.orientation != other.orientation && requested.orientation != 0 {
            return self.orientation != 0;
        }
        if self.ui_mode != 0 || other.ui_mode != 0 {
            if (self.ui_mode ^ other.ui_mode) & MASK_UI_MODE_TYPE != 0
                && requested.ui_mode & MASK_UI_MODE_TYPE != 0
            {
                return self.ui_mode & MASK_UI_MODE_TYPE != 0;
            }
            if (self.ui_mode ^ other.ui_mode) & MASK_UI_MODE_NIGHT != 0
                && requested.ui_mode & MASK_UI_MODE_NIGHT != 0
            {
                return self.ui_mode & MASK_UI_MODE_NIGHT != 0;
            }
        }
        if self.has_screen_type() || other.has_screen_type() {
            if self.density != other.density {
                return self.has_better_density_than(other, requested);
            }
            if self.touchscreen != other.touchscreen && requested.touchscreen != 0 {
                return self.touchscreen != 0;
            }
        }
        if self.has_input() || other.has_input() {
            let keys_hidden = self.input_flags & MASK_KEYSHIDDEN;
            let other_keys_hidden = other.input_flags & MASK_KEYSHIDDEN;
            let requested_keys_hidden = requested.input_flags & MASK_KEYSHIDDEN;
            if keys_hidden != other_keys_hidden && requested_keys_hidden != 0 {
                if keys_hidden == 0 {
                    return false;
                }
                if other_keys_hidden == 0 {
                    return true;
                }
                if requested_keys_hidden == keys_hidden {
                    return true;
                }
                if requested_keys_hidden == other_keys_hidden {
                    return false;
                }
            }
            let nav_hidden = self.input_flags & MASK_NAVHIDDEN;
            let other_nav_hidden = other.input_flags & MASK_NAVHIDDEN;
            if nav_hidden != other_nav_hidden && requested.input_flags & MASK_NAVHIDDEN != 0 {
                if nav_hidden == 0 {
                    return false;
                }
                if other_nav_hidden == 0 {
                    return true;
                }
            }
            if self.keyboard != other.keyboard && requested.keyboard != 0 {
                return self.keyboard != 0;
            }
            if self.navigation != other.navigation && requested.navigation != 0 {
                return self.navigation != 0;
            }
        }
        if self.has_screen_size() || other.has_screen_size() {
            let delta = |config: &Self| {
                let mut delta = 0;
                if requested.screen_width != 0 {
                    delta += i32::from(requested.screen_width) - i32::from(config.screen_width);
                }
                if requested.screen_height != 0 {
                    delta += i32::from(requested.screen_height) - i32::from(config.screen_height);
                }
                delta
            };
            let (mine, theirs) = (delta(self), delta(other));
            if mine != theirs {
                return mine < theirs;
            }
        }
        if self.has_version() || other.has_version() {
            if self.sdk_version != other.sdk_version && requested.sdk_version != 0 {
                return self.sdk_version > other.sdk_version;
            }
            if self.minor_version != other.minor_version && requested.minor_version != 0 {
                return self.minor_version != 0;
            }
        }
        false
    }

    fn has_better_density_than(&self, other: &Self, requested: &Self) -> bool {
        let mine = if self.density == 0 {
            DENSITY_MEDIUM
        } else {
            self.density
        };
        let theirs = if other.density == 0 {
            DENSITY_MEDIUM
        } else {
            other.density
        };
        if mine == DENSITY_ANY {
            return true;
        }
        if theirs == DENSITY_ANY {
            return false;
        }
        let wanted = match requested.density {
            0 | DENSITY_ANY => DENSITY_MEDIUM,
            density => density,
        };
        let (low, high, i_am_bigger) = if theirs > mine {
            (mine, theirs, false)
        } else {
            (theirs, mine, true)
        };
        if high == wanted {
            i_am_bigger
        } else if low >= wanted {
            !i_am_bigger
        } else {
            i_am_bigger
        }
    }

    fn is_locale_better_than(&self, other: &Self, requested: &Self) -> bool {
        if !languages_are_equivalent(self.language, other.language) {
            if requested.language == ENGLISH {
                if requested.country == UNITED_STATES {
                    return if self.language != [0, 0] {
                        self.country == [0, 0] || self.country == UNITED_STATES
                    } else {
                        !(other.country == [0, 0] || other.country == UNITED_STATES)
                    };
                }
                if locale_data::is_close_to_us_english(requested.country) {
                    return if self.language != [0, 0] {
                        locale_data::is_close_to_us_english(self.country)
                    } else {
                        !locale_data::is_close_to_us_english(other.country)
                    };
                }
            }
            return self.language != [0, 0];
        }
        match locale_data::compare_regions(
            self.country,
            other.country,
            requested.language,
            requested.locale_script,
            requested.country,
        ) {
            Ordering::Greater => return true,
            Ordering::Less => return false,
            Ordering::Equal => {}
        }
        let mine_matches = self.locale_variant == requested.locale_variant;
        let theirs_match = other.locale_variant == requested.locale_variant;
        if mine_matches != theirs_match {
            return mine_matches;
        }
        self.language == requested.language && other.language != requested.language
    }
}

fn languages_are_equivalent(a: [u8; 2], b: [u8; 2]) -> bool {
    a == b || (a == TAGALOG && b == FILIPINO) || (a == FILIPINO && b == TAGALOG)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn locale(tag: &str) -> ResConfig {
        let mut config = ResConfig::default();
        config.set_locale(tag);
        config
    }

    fn best<'a>(candidates: &'a [(&'a str, ResConfig)], requested: &ResConfig) -> &'a str {
        let mut chosen: Option<&(&str, ResConfig)> = None;
        for candidate in candidates {
            if !candidate.1.matches(requested) {
                continue;
            }
            if chosen.is_none_or(|best| candidate.1.is_better_than(&best.1, requested)) {
                chosen = Some(candidate);
            }
        }
        chosen.map_or("none", |chosen| chosen.0)
    }

    #[test]
    fn locale_tags_pack_like_aapt() {
        assert_eq!(locale("de-DE").language, *b"de");
        assert_eq!(locale("de-DE").country, *b"DE");
        assert_eq!(locale("de-").country, [0, 0]);
        assert_eq!(locale("fil-PH").language, FILIPINO);
        assert_eq!(locale("es-419").country, [0x80 | (9 << 2), (1 << 5) | 4]);
        assert_eq!(locale("").language, [0, 0]);
        assert_eq!(locale("-US"), ResConfig::default());
    }

    #[test]
    fn the_most_specific_matching_locale_wins() {
        let candidates = [
            ("default", ResConfig::default()),
            ("de", locale("de")),
            ("pt", locale("pt")),
            ("pt-BR", locale("pt-BR")),
            ("en-GB", locale("en-GB")),
        ];
        assert_eq!(best(&candidates, &locale("de-DE")), "de");
        assert_eq!(best(&candidates, &locale("pt-BR")), "pt-BR");
        assert_eq!(best(&candidates, &locale("pt-PT")), "pt");
        assert_eq!(best(&candidates, &locale("en-GB")), "en-GB");
        assert_eq!(best(&candidates, &locale("en-US")), "default");
        assert_eq!(best(&candidates, &locale("ja-JP")), "default");
        assert_eq!(best(&candidates, &ResConfig::default()), "default");
    }

    #[test]
    fn requests_carry_the_script_of_their_language_and_region() {
        let taiwan = locale("zh-TW");
        assert_eq!(taiwan.locale_script, *b"Hant");
        assert!(taiwan.locale_script_was_computed);
        assert_eq!(locale("zh-CN").locale_script, *b"Hans");
        assert_eq!(locale("sr-ME").locale_script, *b"Latn");
        assert_eq!(locale("de").locale_script, *b"Latn");
        let explicit = locale("zh-hant-TW");
        assert_eq!(explicit.locale_script, *b"Hant");
        assert!(!explicit.locale_script_was_computed);
        assert_eq!(explicit.country, *b"TW");
        assert_eq!(locale("de-DE-1901").locale_variant, *b"1901\0\0\0\0");
        assert_eq!(locale("tlh").locale_script, NO_SCRIPT);
    }

    #[test]
    fn latin_american_spanish_prefers_es_419_over_spain() {
        let candidates = [
            ("default", ResConfig::default()),
            ("es", locale("es")),
            ("es-419", locale("es-419")),
            ("es-US", locale("es-US")),
        ];
        assert_eq!(best(&candidates, &locale("es-MX")), "es-419");
        assert_eq!(best(&candidates, &locale("es-AR")), "es-419");
        assert_eq!(best(&candidates, &locale("es-US")), "es-US");
        assert_eq!(best(&candidates, &locale("es-ES")), "es");
        let without_419 = [("es", locale("es")), ("es-US", locale("es-US"))];
        assert_eq!(best(&without_419, &locale("es-CO")), "es-US");
    }

    #[test]
    fn chinese_requests_choose_resources_in_their_script() {
        let candidates = [
            ("default", ResConfig::default()),
            ("zh", locale("zh")),
            ("zh-CN", locale("zh-CN")),
            ("zh-HK", locale("zh-HK")),
            ("zh-TW", locale("zh-TW")),
        ];
        assert_eq!(best(&candidates, &locale("zh-TW")), "zh-TW");
        assert_eq!(best(&candidates, &locale("zh-MO")), "zh-HK");
        assert_eq!(best(&candidates, &locale("zh-CN")), "zh-CN");
        assert_eq!(best(&candidates, &locale("zh-SG")), "zh");
        assert_eq!(best(&candidates, &locale("zh-Hant-US")), "zh-TW");
        let simplified_only = [("default", ResConfig::default()), ("zh", locale("zh"))];
        assert_eq!(best(&simplified_only, &locale("zh-TW")), "default");
    }

    #[test]
    fn english_requests_follow_the_cldr_region_parents() {
        let candidates = [
            ("default", ResConfig::default()),
            ("en-AU", locale("en-AU")),
            ("en-CA", locale("en-CA")),
            ("en-GB", locale("en-GB")),
            ("en-IN", locale("en-IN")),
        ];
        assert_eq!(best(&candidates, &locale("en-NZ")), "en-GB");
        assert_eq!(best(&candidates, &locale("en-IE")), "en-GB");
        assert_eq!(best(&candidates, &locale("en-AU")), "en-AU");
        assert_eq!(best(&candidates, &locale("en-CA")), "en-CA");
        assert_eq!(best(&candidates, &locale("en-PH")), "en-CA");
        assert_eq!(best(&candidates, &locale("en-US")), "default");
        let british = [
            ("default", ResConfig::default()),
            ("en-GB", locale("en-GB")),
        ];
        assert_eq!(best(&british, &locale("en-PH")), "default");
        assert_eq!(best(&british, &locale("en-ZA")), "en-GB");
    }

    #[test]
    fn serbian_requests_choose_the_alphabet_of_their_region() {
        let candidates = [
            ("default", ResConfig::default()),
            ("sr", locale("sr")),
            ("sr-Latn", locale("sr-Latn")),
        ];
        assert_eq!(best(&candidates, &locale("sr-RS")), "sr");
        assert_eq!(best(&candidates, &locale("sr-Latn-RS")), "sr-Latn");
        assert_eq!(best(&candidates, &locale("sr-ME")), "sr-Latn");
    }

    #[test]
    fn qualifiers_above_the_device_do_not_match() {
        let sw500 = ResConfig {
            smallest_screen_width_dp: 500,
            ..ResConfig::default()
        };
        let v31 = ResConfig {
            sdk_version: 31,
            ..ResConfig::default()
        };
        let v28 = ResConfig {
            sdk_version: 28,
            ..ResConfig::default()
        };
        let night = ResConfig {
            ui_mode: 0x20,
            ..ResConfig::default()
        };
        let candidates = [
            ("default", ResConfig::default()),
            ("sw500", sw500),
            ("v31", v31),
            ("v28", v28),
            ("night", night),
        ];
        let phone = ResConfig {
            smallest_screen_width_dp: 360,
            sdk_version: 28,
            ..ResConfig::default()
        };
        assert_eq!(best(&candidates, &phone), "v28");
        let tablet = ResConfig {
            smallest_screen_width_dp: 600,
            sdk_version: 28,
            ..ResConfig::default()
        };
        assert_eq!(best(&candidates, &tablet), "sw500");
        let dark = ResConfig {
            ui_mode: 0x21,
            ..ResConfig::default()
        };
        assert_eq!(best(&candidates, &dark), "night");
    }

    #[test]
    fn density_buckets_prefer_the_closest_scaling_down() {
        let dpi = |density| ResConfig {
            density,
            ..ResConfig::default()
        };
        let candidates = [
            ("mdpi", dpi(160)),
            ("hdpi", dpi(240)),
            ("xhdpi", dpi(320)),
            ("xxhdpi", dpi(480)),
        ];
        assert_eq!(best(&candidates, &dpi(0)), "mdpi");
        assert_eq!(best(&candidates, &dpi(240)), "hdpi");
        assert_eq!(best(&candidates, &dpi(280)), "xhdpi");
        assert_eq!(best(&candidates, &dpi(640)), "xxhdpi");
        let with_any = [("xhdpi", dpi(320)), ("anydpi", dpi(DENSITY_ANY))];
        assert_eq!(best(&with_any, &dpi(320)), "anydpi");
    }

    #[test]
    fn parse_reads_the_aapt_layout_and_tolerates_short_configs() {
        let mut bytes = vec![0u8; 64];
        bytes[..4].copy_from_slice(&64u32.to_le_bytes());
        bytes[8..10].copy_from_slice(b"de");
        bytes[10..12].copy_from_slice(b"AT");
        bytes[14..16].copy_from_slice(&480u16.to_le_bytes());
        bytes[24..26].copy_from_slice(&28u16.to_le_bytes());
        bytes[29] = 0x20;
        bytes[30..32].copy_from_slice(&600u16.to_le_bytes());
        let config = ResConfig::parse(&bytes);
        assert_eq!(config.language, *b"de");
        assert_eq!(config.country, *b"AT");
        assert_eq!(config.density, 480);
        assert_eq!(config.sdk_version, 28);
        assert_eq!(config.ui_mode, 0x20);
        assert_eq!(config.smallest_screen_width_dp, 600);

        bytes[..4].copy_from_slice(&28u32.to_le_bytes());
        let short = ResConfig::parse(&bytes);
        assert_eq!(short.sdk_version, 28);
        assert_eq!(
            short.smallest_screen_width_dp, 0,
            "fields past size are unset"
        );
        assert_eq!(ResConfig::parse(&[]), ResConfig::default());
    }
}
