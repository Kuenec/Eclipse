use std::fmt;

use serde::{Serialize, Serializer};

const GROUP_DIGITS: std::ops::RangeInclusive<usize> = 1..=4;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct PciId {
    pub vendor: u16,
    pub device: u16,
}

impl PciId {
    pub(crate) const ACCEPTED: &'static str =
        "`null` or a PCI ID \"vendor:device\" in hex, as `lspci -nn` shows it, such as \
         `\"10de:2f04\"`";

    #[must_use]
    pub fn parse(text: &str) -> Option<Self> {
        let (vendor, device) = text.split_once(':')?;
        Some(Self {
            vendor: hex_group(vendor)?,
            device: hex_group(device)?,
        })
    }

    pub(crate) fn from_json(value: &serde_json::Value) -> Option<Option<Self>> {
        match value {
            serde_json::Value::Null => Some(None),
            serde_json::Value::String(text) => Self::parse(text).map(Some),
            _ => None,
        }
    }
}

fn hex_group(text: &str) -> Option<u16> {
    if !GROUP_DIGITS.contains(&text.len()) || !text.bytes().all(|byte| byte.is_ascii_hexdigit()) {
        return None;
    }
    u16::from_str_radix(text, 16).ok()
}

impl fmt::Display for PciId {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "{:04x}:{:04x}", self.vendor, self.device)
    }
}

impl Serialize for PciId {
    fn serialize<S: Serializer>(&self, serializer: S) -> Result<S::Ok, S::Error> {
        serializer.collect_str(self)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_vendor_and_device_in_hex_parse_and_print_in_lspci_form() {
        let id = PciId::parse("10de:2f04").expect("a PCI ID");
        assert_eq!(
            id,
            PciId {
                vendor: 0x10de,
                device: 0x2f04
            }
        );
        assert_eq!(id.to_string(), "10de:2f04");
        assert_eq!(
            PciId::parse("1002:164E").map(|id| id.to_string()),
            Some("1002:164e".to_owned())
        );
        assert_eq!(
            PciId::parse("8086:416").map(|id| id.to_string()),
            Some("8086:0416".to_owned())
        );
        assert_eq!(
            serde_json::to_value(id).expect("serialize"),
            serde_json::json!("10de:2f04")
        );
    }

    #[test]
    fn only_two_groups_of_one_to_four_hex_digits_parse() {
        for text in [
            "10de",
            "zz:00",
            "10de:2f04:1",
            "",
            ":",
            "10de:",
            ":2f04",
            "10de0:2f04",
            "+10d:2f04",
            " 10de:2f04",
            "10de:2f04 ",
            "0x10de:2f04",
        ] {
            assert_eq!(PciId::parse(text), None, "{text:?}");
        }
    }

    #[test]
    fn json_null_means_no_device_and_other_json_types_are_rejected() {
        assert_eq!(PciId::from_json(&serde_json::Value::Null), Some(None));
        assert_eq!(
            PciId::from_json(&serde_json::json!("1002:164e")),
            Some(Some(PciId {
                vendor: 0x1002,
                device: 0x164e
            }))
        );
        for value in [
            serde_json::json!("amd"),
            serde_json::json!(4318),
            serde_json::json!(true),
            serde_json::json!(["10de:2f04"]),
        ] {
            assert_eq!(PciId::from_json(&value), None, "{value}");
        }
    }
}
