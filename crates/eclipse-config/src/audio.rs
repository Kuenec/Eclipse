use std::fmt;

use serde::de::{self, Deserializer};
use serde::{Deserialize, Serialize, Serializer};

const SYSTEM_DEFAULT: &str = "default";

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct DeviceName(String);

impl DeviceName {
    #[must_use]
    pub fn parse(name: &str) -> Option<Self> {
        let usable =
            !name.is_empty() && name != SYSTEM_DEFAULT && !name.chars().any(char::is_control);
        usable.then(|| Self(name.to_owned()))
    }

    #[must_use]
    pub fn as_str(&self) -> &str {
        &self.0
    }
}

impl fmt::Display for DeviceName {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(&self.0)
    }
}

impl Serialize for DeviceName {
    fn serialize<S: Serializer>(&self, serializer: S) -> Result<S::Ok, S::Error> {
        serializer.serialize_str(&self.0)
    }
}

impl<'de> Deserialize<'de> for DeviceName {
    fn deserialize<D: Deserializer<'de>>(deserializer: D) -> Result<Self, D::Error> {
        let name = String::deserialize(deserializer)?;
        Self::parse(&name).ok_or_else(|| {
            de::Error::invalid_value(de::Unexpected::Str(&name), &"a sound device name")
        })
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Default)]
pub enum AudioDevice {
    #[default]
    SystemDefault,
    Named(DeviceName),
}

impl AudioDevice {
    pub(crate) fn from_json(value: &serde_json::Value) -> Option<Self> {
        match value.as_str()? {
            SYSTEM_DEFAULT => Some(Self::SystemDefault),
            name => DeviceName::parse(name).map(Self::Named),
        }
    }
}

impl Serialize for AudioDevice {
    fn serialize<S: Serializer>(&self, serializer: S) -> Result<S::Ok, S::Error> {
        match self {
            Self::SystemDefault => serializer.serialize_str(SYSTEM_DEFAULT),
            Self::Named(name) => name.serialize(serializer),
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Direction {
    Output,
    Input,
}

impl Direction {
    pub const ALL: [Self; 2] = [Self::Output, Self::Input];
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct SoundDevice {
    pub name: DeviceName,
    pub description: String,
    pub monitor: bool,
}

#[derive(Debug, Clone, PartialEq, Eq, Default, Serialize, Deserialize)]
pub struct DeviceList {
    pub devices: Vec<SoundDevice>,
    pub system_default: Option<DeviceName>,
}

impl DeviceList {
    #[must_use]
    pub fn find(&self, name: &DeviceName) -> Option<&SoundDevice> {
        self.devices.iter().find(|device| device.name == *name)
    }

    #[must_use]
    pub fn default_device(&self) -> Option<&SoundDevice> {
        self.find(self.system_default.as_ref()?)
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Default, Serialize, Deserialize)]
pub struct SoundDevices {
    pub outputs: DeviceList,
    pub inputs: DeviceList,
}

impl SoundDevices {
    #[must_use]
    pub const fn of(&self, direction: Direction) -> &DeviceList {
        match direction {
            Direction::Output => &self.outputs,
            Direction::Input => &self.inputs,
        }
    }

    pub fn from_json(text: &str) -> Result<Self, String> {
        serde_json::from_str(text).map_err(|error| error.to_string())
    }

    #[must_use]
    pub fn to_json(&self) -> String {
        serde_json::to_string(self).expect("a device list has a JSON form")
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn named(name: &str) -> DeviceName {
        DeviceName::parse(name).expect("a device name")
    }

    #[test]
    fn default_is_the_system_default_and_any_other_text_names_a_device() {
        let quadcast = "alsa_input.usb-HP__Inc_HyperX_QuadCast_4111-00.analog-stereo";
        assert_eq!(
            AudioDevice::from_json(&"default".into()),
            Some(AudioDevice::SystemDefault)
        );
        assert_eq!(
            AudioDevice::from_json(&quadcast.into()),
            Some(AudioDevice::Named(named(quadcast)))
        );
        assert_eq!(
            serde_json::to_value(AudioDevice::Named(named(quadcast))).expect("serialize"),
            serde_json::json!(quadcast)
        );
        assert_eq!(
            serde_json::to_value(AudioDevice::SystemDefault).expect("serialize"),
            serde_json::json!("default")
        );
    }

    #[test]
    fn a_device_name_is_text_an_environment_variable_can_carry() {
        for refused in [
            serde_json::json!(""),
            serde_json::json!("bad\nname"),
            serde_json::json!("nul\0name"),
            serde_json::json!(null),
            serde_json::json!(3),
            serde_json::json!(["default"]),
        ] {
            assert_eq!(AudioDevice::from_json(&refused), None, "{refused}");
        }
        assert_eq!(DeviceName::parse("default"), None);
        assert_eq!(
            DeviceName::parse("Voice Changer Mic").map(|name| name.to_string()),
            Some("Voice Changer Mic".to_owned())
        );
    }

    #[test]
    fn a_device_list_round_trips_and_refuses_unusable_names() {
        let devices = SoundDevices {
            outputs: DeviceList {
                devices: vec![SoundDevice {
                    name: named("speakers"),
                    description: "Speakers".to_owned(),
                    monitor: false,
                }],
                system_default: Some(named("speakers")),
            },
            inputs: DeviceList::default(),
        };
        let json = devices.to_json();
        assert_eq!(SoundDevices::from_json(&json), Ok(devices.clone()));
        assert_eq!(
            devices.of(Direction::Output).default_device(),
            devices.outputs.devices.first()
        );
        assert_eq!(devices.of(Direction::Input).default_device(), None);

        let unusable = json.replace("\"speakers\"", "\"default\"");
        let error = SoundDevices::from_json(&unusable).expect_err("a reserved name");
        assert!(error.contains("a sound device name"), "{error}");
    }
}
