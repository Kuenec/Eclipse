mod document;
mod load;
pub mod shell;
pub mod temp_file;

use std::collections::BTreeMap;
use std::fmt;
use std::path::PathBuf;

use serde::{Serialize, Serializer};

pub use document::Position;
pub use load::{load, load_from, Loaded, Problem};

const SOBER_FAKE_OFF: &str = "fake_off";

const LINK_LAUNCHES: &str = "browser";

#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum GraphicsOptimizationMode {
    Quality,
    #[default]
    Balanced,
    Performance,
}

impl GraphicsOptimizationMode {
    const ALL: [Self; 3] = [Self::Quality, Self::Balanced, Self::Performance];

    #[must_use]
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::Quality => "quality",
            Self::Balanced => "balanced",
            Self::Performance => "performance",
        }
    }

    fn from_name(name: &str) -> Option<Self> {
        Self::ALL.into_iter().find(|mode| mode.as_str() == name)
    }
}

impl Serialize for GraphicsOptimizationMode {
    fn serialize<S: Serializer>(&self, serializer: S) -> Result<S::Ok, S::Error> {
        serializer.serialize_str(self.as_str())
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum TouchMode {
    #[default]
    Off,
    On,
    FakeOff,
}

impl TouchMode {
    const ALL: [Self; 3] = [Self::Off, Self::On, Self::FakeOff];

    #[must_use]
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::Off => "off",
            Self::On => "on",
            Self::FakeOff => "fake-off",
        }
    }

    fn from_name(name: &str) -> Option<Self> {
        if name == SOBER_FAKE_OFF {
            return Some(Self::FakeOff);
        }
        Self::ALL.into_iter().find(|mode| mode.as_str() == name)
    }
}

impl Serialize for TouchMode {
    fn serialize<S: Serializer>(&self, serializer: S) -> Result<S::Ok, S::Error> {
        serializer.serialize_str(self.as_str())
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum CloseOnLeave {
    Never,
    #[default]
    LinkLaunches,
    Always,
}

impl CloseOnLeave {
    const ALL: [Self; 3] = [Self::Never, Self::LinkLaunches, Self::Always];

    const fn json_form(self) -> &'static str {
        match self {
            Self::Never => "false",
            Self::LinkLaunches => "\"browser\"",
            Self::Always => "true",
        }
    }

    fn from_json(value: &serde_json::Value) -> Option<Self> {
        match value {
            serde_json::Value::Bool(false) => Some(Self::Never),
            serde_json::Value::Bool(true) => Some(Self::Always),
            serde_json::Value::String(name) if name == LINK_LAUNCHES => Some(Self::LinkLaunches),
            _ => None,
        }
    }
}

impl Serialize for CloseOnLeave {
    fn serialize<S: Serializer>(&self, serializer: S) -> Result<S::Ok, S::Error> {
        match self {
            Self::Never => serializer.serialize_bool(false),
            Self::LinkLaunches => serializer.serialize_str(LINK_LAUNCHES),
            Self::Always => serializer.serialize_bool(true),
        }
    }
}

#[derive(Debug, Clone, PartialEq, Serialize)]
pub struct Config {
    pub graphics_optimization_mode: GraphicsOptimizationMode,

    pub touch_mode: TouchMode,

    pub enable_gamemode: bool,

    pub roblox_auto_update: bool,

    pub close_on_leave: CloseOnLeave,

    pub server_location_indicator_enabled: bool,

    pub fflags: BTreeMap<String, serde_json::Value>,

    pub webview_helper_path: Option<PathBuf>,
}

impl Default for Config {
    fn default() -> Self {
        Self {
            graphics_optimization_mode: GraphicsOptimizationMode::default(),
            touch_mode: TouchMode::default(),
            enable_gamemode: true,
            roblox_auto_update: true,
            close_on_leave: CloseOnLeave::default(),
            server_location_indicator_enabled: false,
            fflags: BTreeMap::new(),
            webview_helper_path: None,
        }
    }
}

impl Config {
    fn set_value(&mut self, key: Key, value: serde_json::Value) -> Result<(), String> {
        let reason = |error: serde_json::Error| document::reason(&error);
        match key {
            Key::Setting(key) => match key.setting(&value)? {
                Setting::TouchMode(mode) => self.touch_mode = mode,
                Setting::GraphicsOptimizationMode(mode) => self.graphics_optimization_mode = mode,
                Setting::EnableGamemode(enabled) => self.enable_gamemode = enabled,
                Setting::RobloxAutoUpdate(enabled) => self.roblox_auto_update = enabled,
                Setting::CloseOnLeave(policy) => self.close_on_leave = policy,
                Setting::ServerLocationIndicatorEnabled(enabled) => {
                    self.server_location_indicator_enabled = enabled;
                }
            },
            Key::FileOnly(FileOnlyKey::Fflags) => {
                self.fflags = serde_json::from_value(value).map_err(reason)?;
            }
            Key::FileOnly(FileOnlyKey::WebviewHelperPath) => {
                self.webview_helper_path = serde_json::from_value::<Option<PathBuf>>(value)
                    .map_err(reason)?
                    .filter(|path| !path.as_os_str().is_empty());
            }
        }
        Ok(())
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
#[serde(untagged)]
pub enum Setting {
    TouchMode(TouchMode),
    GraphicsOptimizationMode(GraphicsOptimizationMode),
    EnableGamemode(bool),
    RobloxAutoUpdate(bool),
    CloseOnLeave(CloseOnLeave),
    ServerLocationIndicatorEnabled(bool),
}

impl Setting {
    #[must_use]
    pub const fn key(self) -> SettingKey {
        match self {
            Self::TouchMode(_) => SettingKey::TouchMode,
            Self::GraphicsOptimizationMode(_) => SettingKey::GraphicsOptimizationMode,
            Self::EnableGamemode(_) => SettingKey::EnableGamemode,
            Self::RobloxAutoUpdate(_) => SettingKey::RobloxAutoUpdate,
            Self::CloseOnLeave(_) => SettingKey::CloseOnLeave,
            Self::ServerLocationIndicatorEnabled(_) => SettingKey::ServerLocationIndicatorEnabled,
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum SettingKey {
    TouchMode,
    GraphicsOptimizationMode,
    EnableGamemode,
    RobloxAutoUpdate,
    CloseOnLeave,
    ServerLocationIndicatorEnabled,
}

impl SettingKey {
    const ALL: [Self; 6] = [
        Self::TouchMode,
        Self::GraphicsOptimizationMode,
        Self::EnableGamemode,
        Self::RobloxAutoUpdate,
        Self::CloseOnLeave,
        Self::ServerLocationIndicatorEnabled,
    ];

    #[must_use]
    pub const fn name(self) -> &'static str {
        match self {
            Self::TouchMode => "touch_mode",
            Self::GraphicsOptimizationMode => "graphics_optimization_mode",
            Self::EnableGamemode => "enable_gamemode",
            Self::RobloxAutoUpdate => "roblox_auto_update",
            Self::CloseOnLeave => "close_on_leave",
            Self::ServerLocationIndicatorEnabled => "server_location_indicator_enabled",
        }
    }

    pub fn from_name(name: &str) -> Result<Self, SettingError> {
        match Key::from_name(name) {
            Some(Key::Setting(key)) => Ok(key),
            Some(Key::FileOnly(key)) => Err(SettingError::FileOnly(key.name())),
            None => Err(SettingError::UnknownKey(name.to_owned())),
        }
    }

    pub fn parse(self, value: serde_json::Value) -> Result<Setting, SettingError> {
        self.setting(&value)
            .map_err(|message| SettingError::Invalid { key: self, message })
    }

    fn setting(self, value: &serde_json::Value) -> Result<Setting, String> {
        let name = value.as_str();
        let setting = match self {
            Self::TouchMode => name.and_then(TouchMode::from_name).map(Setting::TouchMode),
            Self::GraphicsOptimizationMode => name
                .and_then(GraphicsOptimizationMode::from_name)
                .map(Setting::GraphicsOptimizationMode),
            Self::EnableGamemode => value.as_bool().map(Setting::EnableGamemode),
            Self::RobloxAutoUpdate => value.as_bool().map(Setting::RobloxAutoUpdate),
            Self::CloseOnLeave => CloseOnLeave::from_json(value).map(Setting::CloseOnLeave),
            Self::ServerLocationIndicatorEnabled => {
                value.as_bool().map(Setting::ServerLocationIndicatorEnabled)
            }
        };
        setting.ok_or_else(|| format!("expected one of {}", self.accepted_values()))
    }

    fn accepted_values(self) -> String {
        let names: &[&str] = match self {
            Self::TouchMode => &TouchMode::ALL.map(TouchMode::as_str),
            Self::GraphicsOptimizationMode => {
                &GraphicsOptimizationMode::ALL.map(GraphicsOptimizationMode::as_str)
            }
            Self::EnableGamemode
            | Self::RobloxAutoUpdate
            | Self::ServerLocationIndicatorEnabled => &["true", "false"],
            Self::CloseOnLeave => &CloseOnLeave::ALL.map(CloseOnLeave::json_form),
        };
        let quoted: Vec<String> = names.iter().map(|name| format!("`{name}`")).collect();
        quoted.join(", ")
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum FileOnlyKey {
    Fflags,
    WebviewHelperPath,
}

impl FileOnlyKey {
    const ALL: [Self; 2] = [Self::Fflags, Self::WebviewHelperPath];

    const fn name(self) -> &'static str {
        match self {
            Self::Fflags => "fflags",
            Self::WebviewHelperPath => "webview_helper_path",
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Key {
    Setting(SettingKey),
    FileOnly(FileOnlyKey),
}

impl Key {
    fn from_name(name: &str) -> Option<Self> {
        let settings = SettingKey::ALL.into_iter().map(Self::Setting);
        let file_only = FileOnlyKey::ALL.into_iter().map(Self::FileOnly);
        settings.chain(file_only).find(|key| key.name() == name)
    }

    const fn name(self) -> &'static str {
        match self {
            Self::Setting(key) => key.name(),
            Self::FileOnly(key) => key.name(),
        }
    }

    fn default_json(self) -> String {
        let defaults =
            serde_json::to_value(Config::default()).expect("the default config has a JSON form");
        defaults[self.name()].to_string()
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum SettingError {
    UnknownKey(String),

    FileOnly(&'static str),

    Invalid { key: SettingKey, message: String },
}

impl fmt::Display for SettingError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::UnknownKey(name) => {
                write!(f, "`{name}` is not a setting; the settings are ")?;
                for (index, key) in SettingKey::ALL.into_iter().enumerate() {
                    let separator = if index == 0 { "" } else { ", " };
                    write!(f, "{separator}`{}`", key.name())?;
                }
                Ok(())
            }
            Self::FileOnly(key) => write!(f, "`{key}` is edited by hand in config.json"),
            Self::Invalid { key, message } => write!(f, "{}: {message}", key.name()),
        }
    }
}

impl std::error::Error for SettingError {}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn sober_written_fake_off_parses_and_is_written_as_fake_off() {
        assert_eq!(TouchMode::Off.as_str(), "off");
        assert_eq!(TouchMode::On.as_str(), "on");
        assert_eq!(TouchMode::FakeOff.as_str(), "fake-off");
        assert_eq!(
            SettingKey::TouchMode.parse("fake_off".into()),
            Ok(Setting::TouchMode(TouchMode::FakeOff))
        );
        assert_eq!(
            serde_json::to_string(&Setting::TouchMode(TouchMode::FakeOff)).expect("serialize"),
            r#""fake-off""#
        );
    }

    #[test]
    fn file_only_keys_cannot_be_set_by_tools() {
        assert_eq!(
            SettingKey::from_name("fflags"),
            Err(SettingError::FileOnly("fflags"))
        );
        assert_eq!(
            SettingKey::from_name("webview_helper_path"),
            Err(SettingError::FileOnly("webview_helper_path"))
        );
        assert_eq!(
            SettingKey::from_name("use_opengl"),
            Err(SettingError::UnknownKey("use_opengl".to_owned()))
        );
        assert_eq!(
            SettingError::UnknownKey("use_opengl".to_owned()).to_string(),
            "`use_opengl` is not a setting; the settings are `touch_mode`, \
             `graphics_optimization_mode`, `enable_gamemode`, `roblox_auto_update`, \
             `close_on_leave`, `server_location_indicator_enabled`"
        );
    }

    #[test]
    fn every_setting_key_round_trips_through_its_name() {
        for key in SettingKey::ALL {
            assert_eq!(SettingKey::from_name(key.name()), Ok(key));
        }
        let setting = SettingKey::GraphicsOptimizationMode
            .parse("performance".into())
            .expect("valid value");
        assert_eq!(
            setting,
            Setting::GraphicsOptimizationMode(GraphicsOptimizationMode::Performance)
        );
        assert_eq!(setting.key(), SettingKey::GraphicsOptimizationMode);
        assert_eq!(
            SettingKey::TouchMode
                .parse("on".into())
                .expect("valid value")
                .key(),
            SettingKey::TouchMode
        );
    }

    #[test]
    fn an_invalid_value_names_its_key_and_the_accepted_values() {
        let error = SettingKey::TouchMode
            .parse("sideways".into())
            .expect_err("not a touch mode");
        let SettingError::Invalid { key, ref message } = error else {
            panic!("expected Invalid, got {error:?}");
        };
        assert_eq!(key, SettingKey::TouchMode);
        for accepted in ["`off`", "`on`", "`fake-off`"] {
            assert!(message.contains(accepted), "{message}");
        }
        assert!(error.to_string().starts_with("touch_mode: "), "{error}");
        assert_eq!(
            SettingKey::GraphicsOptimizationMode.parse(3.into()),
            Err(SettingError::Invalid {
                key: SettingKey::GraphicsOptimizationMode,
                message: "expected one of `quality`, `balanced`, `performance`".to_owned(),
            })
        );
    }

    #[test]
    fn only_a_string_names_a_setting_value() {
        for value in [
            serde_json::json!(5),
            serde_json::json!(true),
            serde_json::json!(null),
            serde_json::json!(["on"]),
            serde_json::json!({"on": null}),
        ] {
            assert_eq!(
                SettingKey::TouchMode.parse(value.clone()),
                Err(SettingError::Invalid {
                    key: SettingKey::TouchMode,
                    message: "expected one of `off`, `on`, `fake-off`".to_owned(),
                }),
                "{value}"
            );
        }
    }

    #[test]
    fn enable_gamemode_takes_only_a_json_boolean() {
        for enabled in [true, false] {
            let setting = SettingKey::EnableGamemode
                .parse(enabled.into())
                .expect("a boolean");
            assert_eq!(setting, Setting::EnableGamemode(enabled));
            assert_eq!(setting.key(), SettingKey::EnableGamemode);
            assert_eq!(
                serde_json::to_string(&setting).expect("serialize"),
                enabled.to_string()
            );
        }
        for value in [
            serde_json::json!("false"),
            serde_json::json!(0),
            serde_json::json!(null),
        ] {
            assert_eq!(
                SettingKey::EnableGamemode.parse(value.clone()),
                Err(SettingError::Invalid {
                    key: SettingKey::EnableGamemode,
                    message: "expected one of `true`, `false`".to_owned(),
                }),
                "{value}"
            );
        }
    }

    #[test]
    fn roblox_auto_update_takes_only_a_json_boolean() {
        for enabled in [true, false] {
            let setting = SettingKey::RobloxAutoUpdate
                .parse(enabled.into())
                .expect("a boolean");
            assert_eq!(setting, Setting::RobloxAutoUpdate(enabled));
            assert_eq!(setting.key(), SettingKey::RobloxAutoUpdate);
            assert_eq!(
                serde_json::to_string(&setting).expect("serialize"),
                enabled.to_string()
            );
        }
        for value in [
            serde_json::json!("false"),
            serde_json::json!(0),
            serde_json::json!(null),
        ] {
            assert_eq!(
                SettingKey::RobloxAutoUpdate.parse(value.clone()),
                Err(SettingError::Invalid {
                    key: SettingKey::RobloxAutoUpdate,
                    message: "expected one of `true`, `false`".to_owned(),
                }),
                "{value}"
            );
        }
    }

    #[test]
    fn server_location_indicator_enabled_takes_only_a_json_boolean() {
        let key = SettingKey::ServerLocationIndicatorEnabled;
        for enabled in [true, false] {
            let setting = key.parse(enabled.into()).expect("a boolean");
            assert_eq!(setting, Setting::ServerLocationIndicatorEnabled(enabled));
            assert_eq!(setting.key(), key);
            assert_eq!(
                serde_json::to_string(&setting).expect("serialize"),
                enabled.to_string()
            );
        }
        assert_eq!(
            key.parse(serde_json::json!("true")),
            Err(SettingError::Invalid {
                key,
                message: "expected one of `true`, `false`".to_owned(),
            })
        );
    }

    #[test]
    fn close_on_leave_takes_sober_booleans_and_browser() {
        for (json, policy) in [
            (serde_json::json!(false), CloseOnLeave::Never),
            (serde_json::json!("browser"), CloseOnLeave::LinkLaunches),
            (serde_json::json!(true), CloseOnLeave::Always),
        ] {
            let setting = SettingKey::CloseOnLeave
                .parse(json.clone())
                .expect("a close_on_leave value");
            assert_eq!(setting, Setting::CloseOnLeave(policy));
            assert_eq!(setting.key(), SettingKey::CloseOnLeave);
            assert_eq!(serde_json::to_value(setting).expect("serialize"), json);
            assert_eq!(json.to_string(), policy.json_form());
        }
        for value in [
            serde_json::json!("always"),
            serde_json::json!("true"),
            serde_json::json!(1),
            serde_json::json!(null),
        ] {
            assert_eq!(
                SettingKey::CloseOnLeave.parse(value.clone()),
                Err(SettingError::Invalid {
                    key: SettingKey::CloseOnLeave,
                    message: "expected one of `false`, `\"browser\"`, `true`".to_owned(),
                }),
                "{value}"
            );
        }
    }

    #[test]
    fn every_schema_key_is_a_setting_or_file_only() {
        let written = serde_json::to_value(Config::default()).expect("serialize");
        let mut schema: Vec<&str> = written
            .as_object()
            .expect("an object")
            .keys()
            .map(String::as_str)
            .collect();
        let mut known: Vec<&str> = SettingKey::ALL
            .into_iter()
            .map(Key::Setting)
            .chain(FileOnlyKey::ALL.into_iter().map(Key::FileOnly))
            .map(Key::name)
            .collect();
        schema.sort_unstable();
        known.sort_unstable();
        assert_eq!(schema, known);
        for name in known {
            assert_eq!(Key::from_name(name).map(Key::name), Some(name));
        }
    }

    #[test]
    fn defaults_are_shown_in_their_json_form() {
        let default_json = |name| {
            Key::from_name(name)
                .map(Key::default_json)
                .expect("a schema key")
        };
        assert_eq!(default_json("touch_mode"), r#""off""#);
        assert_eq!(default_json("graphics_optimization_mode"), r#""balanced""#);
        assert_eq!(default_json("enable_gamemode"), "true");
        assert_eq!(default_json("roblox_auto_update"), "true");
        assert_eq!(default_json("close_on_leave"), r#""browser""#);
        assert_eq!(default_json("server_location_indicator_enabled"), "false");
        assert_eq!(default_json("fflags"), "{}");
        assert_eq!(default_json("webview_helper_path"), "null");
    }
}
