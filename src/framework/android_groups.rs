use std::ops::RangeInclusive;

use jni::sys::jint;

const ANDROID_IDS: [(&str, u32); 87] = [
    ("root", 0),
    ("daemon", 1),
    ("bin", 2),
    ("system", 1000),
    ("radio", 1001),
    ("bluetooth", 1002),
    ("graphics", 1003),
    ("input", 1004),
    ("audio", 1005),
    ("camera", 1006),
    ("log", 1007),
    ("compass", 1008),
    ("mount", 1009),
    ("wifi", 1010),
    ("adb", 1011),
    ("install", 1012),
    ("media", 1013),
    ("dhcp", 1014),
    ("sdcard_rw", 1015),
    ("vpn", 1016),
    ("keystore", 1017),
    ("usb", 1018),
    ("drm", 1019),
    ("mdnsr", 1020),
    ("gps", 1021),
    ("media_rw", 1023),
    ("mtp", 1024),
    ("drmrpc", 1026),
    ("nfc", 1027),
    ("sdcard_r", 1028),
    ("clat", 1029),
    ("loop_radio", 1030),
    ("mediadrm", 1031),
    ("package_info", 1032),
    ("sdcard_pics", 1033),
    ("sdcard_av", 1034),
    ("sdcard_all", 1035),
    ("logd", 1036),
    ("shared_relro", 1037),
    ("dbus", 1038),
    ("tlsdate", 1039),
    ("mediaex", 1040),
    ("audioserver", 1041),
    ("metrics_coll", 1042),
    ("metricsd", 1043),
    ("webserv", 1044),
    ("debuggerd", 1045),
    ("mediacodec", 1046),
    ("cameraserver", 1047),
    ("firewall", 1048),
    ("trunks", 1049),
    ("nvram", 1050),
    ("dns", 1051),
    ("dns_tether", 1052),
    ("webview_zygote", 1053),
    ("vehicle_network", 1054),
    ("media_audio", 1055),
    ("media_video", 1056),
    ("media_image", 1057),
    ("tombstoned", 1058),
    ("media_obb", 1059),
    ("ese", 1060),
    ("ota_update", 1061),
    ("automotive_evs", 1062),
    ("lowpan", 1063),
    ("hsm", 1064),
    ("reserved_disk", 1065),
    ("statsd", 1066),
    ("incidentd", 1067),
    ("secure_element", 1068),
    ("shell", 2000),
    ("cache", 2001),
    ("diag", 2002),
    ("net_bt_admin", 3001),
    ("net_bt", 3002),
    ("inet", 3003),
    ("net_raw", 3004),
    ("net_admin", 3005),
    ("net_bw_stats", 3006),
    ("net_bw_acct", 3007),
    ("readproc", 3009),
    ("wakelock", 3010),
    ("uhid", 3011),
    ("everybody", 9997),
    ("misc", 9998),
    ("nobody", 9999),
    ("overflowuid", 65534),
];

const OEM_RANGES: [RangeInclusive<u32>; 2] = [2900..=2999, 5000..=5999];
const APP_START: u32 = 10000;
const CACHE_GID_START: u64 = 20000;
const SHARED_GID_START: u64 = 50000;
const SHARED_GID_END: u64 = 59999;
const ISOLATED_START: u64 = 99000;
const OVERFLOW_ID: u32 = 65534;
const USER_OFFSET: u32 = 100000;
const MAX_USER: u64 = 1000;
const PER_USER_RANGES: [RangeInclusive<u32>; 6] = [
    10000..=19999,
    20000..=29999,
    30000..=39999,
    40000..=49999,
    50000..=59999,
    99000..=99999,
];
const SCANF_NUMBER_WIDTH: usize = 512;
const MUTF8_NUL: [u8; 2] = [0xc0, 0x80];
const NOT_FOUND: jint = -1;

pub(super) fn gid_for_name(mutf8_name: &[u8]) -> jint {
    if mutf8_name.is_empty() {
        return NOT_FOUND;
    }
    if mutf8_name.iter().all(u8::is_ascii_digit) {
        return atoi(mutf8_name);
    }
    let c_string_end = mutf8_name
        .windows(MUTF8_NUL.len())
        .position(|pair| pair == MUTF8_NUL)
        .unwrap_or(mutf8_name.len());
    group_id(&mutf8_name[..c_string_end]).map_or(NOT_FOUND, |gid| gid as jint)
}

fn atoi(digits: &[u8]) -> jint {
    let long = decimal(digits)
        .and_then(|value| i64::try_from(value).ok())
        .unwrap_or(i64::MAX);
    long as jint
}

fn decimal(digits: &[u8]) -> Option<u64> {
    digits.iter().try_fold(0u64, |value, digit| {
        value.checked_mul(10)?.checked_add(u64::from(digit - b'0'))
    })
}

fn group_id(name: &[u8]) -> Option<u32> {
    android_id(name)
        .or_else(|| oem_id(name))
        .or_else(|| app_group_id(name))
}

fn android_id(name: &[u8]) -> Option<u32> {
    ANDROID_IDS
        .iter()
        .find(|(id_name, _)| id_name.as_bytes() == name)
        .map(|&(_, id)| id)
}

fn oem_id(name: &[u8]) -> Option<u32> {
    let id = scanf_unsigned(name.strip_prefix(b"oem_")?)?;
    OEM_RANGES
        .iter()
        .any(|range| range.contains(&id))
        .then_some(id)
}

fn scanf_unsigned(text: &[u8]) -> Option<u32> {
    let skipped = text.iter().take_while(|&&byte| is_c_space(byte)).count();
    let number = &text[skipped..];
    let sign = usize::from(matches!(number.first(), Some(b'+' | b'-')));
    let digits = number[sign..]
        .iter()
        .take(SCANF_NUMBER_WIDTH - sign)
        .take_while(|byte| byte.is_ascii_digit())
        .count();
    if digits == 0 {
        return None;
    }
    let magnitude = decimal(&number[sign..sign + digits]);
    let value = match (number.first(), magnitude) {
        (_, None) => u64::MAX,
        (Some(b'-'), Some(magnitude)) => magnitude.wrapping_neg(),
        (_, Some(magnitude)) => magnitude,
    };
    Some(value as u32)
}

fn is_c_space(byte: u8) -> bool {
    byte == b' ' || (b'\t'..=b'\r').contains(&byte)
}

fn leading_decimal(text: &[u8]) -> (u64, &[u8]) {
    let digits = text.iter().take_while(|byte| byte.is_ascii_digit()).count();
    let value = decimal(&text[..digits]).unwrap_or(u64::MAX);
    (value, &text[digits..])
}

fn app_group_id(name: &[u8]) -> Option<u32> {
    let (user, shared, rest) = match name {
        [b'a', b'l', b'l', rest @ ..] => (0, true, rest),
        [b'u', digit, ..] if digit.is_ascii_digit() => {
            let (user, rest) = leading_decimal(&name[1..]);
            (user, false, rest)
        }
        _ => return None,
    };
    let suffix = rest
        .strip_prefix(b"_")
        .filter(|suffix| !suffix.is_empty())?;
    let (app, rest) = match suffix {
        [b'a', digit, ..] if digit.is_ascii_digit() => {
            let (number, rest) = leading_decimal(&suffix[1..]);
            if shared {
                let app = number.wrapping_add(SHARED_GID_START);
                if app > SHARED_GID_END {
                    return None;
                }
                (app, rest)
            } else if rest == b"_cache" {
                (number.wrapping_add(CACHE_GID_START), &[][..])
            } else {
                (number.wrapping_add(u64::from(APP_START)), rest)
            }
        }
        [b'i', digit, ..] if digit.is_ascii_digit() => {
            let (number, rest) = leading_decimal(&suffix[1..]);
            (number.wrapping_add(ISOLATED_START), rest)
        }
        _ => (u64::from(android_id(suffix)?), &[][..]),
    };
    if !rest.is_empty() || user > MAX_USER || app >= u64::from(USER_OFFSET) {
        return None;
    }
    let gid = u32::try_from(app + user * u64::from(USER_OFFSET)).ok()?;
    (gid >= APP_START && is_valid_app_id(gid)).then_some(gid)
}

fn is_valid_app_id(id: u32) -> bool {
    let app = id % USER_OFFSET;
    app != OVERFLOW_ID
        && (app < APP_START || PER_USER_RANGES.iter().any(|range| range.contains(&app)))
}

#[cfg(test)]
mod tests {
    use super::gid_for_name;

    fn gid(name: &str) -> i32 {
        gid_for_name(name.as_bytes())
    }

    #[test]
    fn empty_and_numeric_names_follow_the_jni_parsing() {
        assert_eq!(gid(""), -1);
        assert_eq!(gid("0"), 0);
        assert_eq!(gid("3003"), 3003);
        assert_eq!(gid("007"), 7);
        assert_eq!(gid("4294967296"), 0);
        assert_eq!(gid("2147483648"), i32::MIN);
        assert_eq!(gid("99999999999999999999"), -1);
        assert_eq!(gid("-5"), -1);
        assert_eq!(gid(" 5"), -1);
    }

    #[test]
    fn android_id_names_resolve_with_the_generators_name_fixups() {
        assert_eq!(gid("root"), 0);
        assert_eq!(gid("system"), 1000);
        assert_eq!(gid("inet"), 3003);
        assert_eq!(gid("mediacodec"), 1046);
        assert_eq!(gid("media_codec"), -1);
        assert_eq!(gid("unused1"), -1);
        assert_eq!(gid("overflowuid"), 65534);
        assert_eq!(gid("app"), -1);
        assert_eq!(gid("Inet"), -1);
    }

    #[test]
    fn oem_names_resolve_inside_the_reserved_ranges_like_sscanf() {
        assert_eq!(gid("oem_2900"), 2900);
        assert_eq!(gid("oem_5999"), 5999);
        assert_eq!(gid("oem_3000"), -1);
        assert_eq!(gid("oem_ \t+2901trailing"), 2901);
        assert_eq!(gid("oem_4294970197"), 2901);
        assert_eq!(gid("oem_-4294964395"), 2901);
        assert_eq!(gid(&format!("oem_{}", "9".repeat(30))), -1);
        assert_eq!(gid(&format!("oem_{}2901", "0".repeat(508))), 2901);
        assert_eq!(gid(&format!("oem_{}2901", "0".repeat(509))), -1);
        assert_eq!(gid("oem_"), -1);
        assert_eq!(gid("oem_-"), -1);
    }

    #[test]
    fn per_user_app_names_resolve_to_their_app_ids() {
        assert_eq!(gid("u0_a12"), 10012);
        assert_eq!(gid("u0_a12_cache"), 20012);
        assert_eq!(gid("u0_a12_cachex"), -1);
        assert_eq!(gid("all_a5"), 50005);
        assert_eq!(gid("all_a10000"), -1);
        assert_eq!(gid("u2_i3"), 299003);
        assert_eq!(gid("u1_system"), 101000);
        assert_eq!(gid("u0_system"), -1);
        assert_eq!(gid("u1000_a1"), 100010001);
        assert_eq!(gid("u1001_a1"), -1);
        assert_eq!(gid("u0_a55534"), -1);
        assert_eq!(gid("u0_a89999"), 99999);
        assert_eq!(gid("u0_a90000"), -1);
        assert_eq!(gid("u0_a49999"), 59999);
        assert_eq!(gid("u0_a50000"), -1);
        assert_eq!(gid("u0_"), -1);
        assert_eq!(gid("u0_b1"), -1);
        assert_eq!(gid("ux_a1"), -1);
    }

    #[test]
    fn a_name_ends_at_its_first_nul_character() {
        assert_eq!(gid_for_name(b"u0_a1\xc0\x80x"), 10001);
        assert_eq!(gid_for_name(b"1\xc0\x802"), -1);
        assert_eq!(gid_for_name(b"\xc0\x80"), -1);
        assert_eq!(gid_for_name("inet\u{e9}".as_bytes()), -1);
    }
}
