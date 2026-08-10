use std::collections::BTreeMap;
use std::fmt;
use std::path::PathBuf;

use directories::ProjectDirs;
use serde::{Deserialize, Serialize};

pub const SETTINGS_SAVE_STATE_FLAG: &str = "FFlagGlobalBasicSettingsSaveStateReflection";

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, Default)]
#[serde(rename_all = "lowercase")]
pub enum GraphicsOptimizationMode {
    Quality,
    #[default]
    Balanced,
    Performance,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, Default)]
#[serde(rename_all = "kebab-case")]
pub enum TouchMode {
    #[default]
    Off,
    On,
    FakeOff,
}

impl TouchMode {
    #[must_use]
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::Off => "off",
            Self::On => "on",
            Self::FakeOff => "fake-off",
        }
    }
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(default)]
pub struct Config {
    pub use_opengl: bool,

    pub graphics_optimization_mode: GraphicsOptimizationMode,

    pub enable_gamemode: bool,

    pub enable_hidpi: bool,

    pub discord_rpc_enabled: bool,

    pub discord_rpc_show_join_button: bool,

    pub server_location_indicator_enabled: bool,

    pub close_on_leave: bool,

    pub touch_mode: TouchMode,

    pub allow_gamepad_permission: bool,

    pub use_console_experience: bool,

    pub use_libsecret: bool,

    pub fflags: BTreeMap<String, serde_json::Value>,

    pub apk_url: Option<String>,

    pub apk_sha256: Option<String>,

    pub auto_fetch_missing: bool,

    pub webview_helper_path: Option<String>,

    pub webview_allow_unsandboxed: bool,
}

impl Default for Config {
    fn default() -> Self {
        Self {
            use_opengl: false,
            graphics_optimization_mode: GraphicsOptimizationMode::default(),
            enable_gamemode: true,
            enable_hidpi: false,
            discord_rpc_enabled: false,
            discord_rpc_show_join_button: false,
            server_location_indicator_enabled: false,
            close_on_leave: true,
            touch_mode: TouchMode::default(),
            allow_gamepad_permission: false,
            use_console_experience: false,
            use_libsecret: false,
            fflags: BTreeMap::new(),
            apk_url: None,
            apk_sha256: None,
            auto_fetch_missing: false,
            webview_helper_path: None,
            webview_allow_unsandboxed: false,
        }
    }
}

impl Config {
    pub fn config_path() -> Result<PathBuf, ConfigError> {
        let dirs = ProjectDirs::from("", "", "eclipse").ok_or(ConfigError::NoConfigDir)?;
        Ok(dirs.config_dir().join("config.json"))
    }

    pub fn load() -> Result<Self, ConfigError> {
        let path = Self::config_path()?;
        match std::fs::read_to_string(&path) {
            Ok(text) => Ok(serde_json::from_str(&text)?),
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => Ok(Self::default()),
            Err(e) => Err(ConfigError::Io(e)),
        }
    }

    pub fn save(&self) -> Result<(), ConfigError> {
        let path = Self::config_path()?;
        if let Some(parent) = path.parent() {
            std::fs::create_dir_all(parent)?;
        }
        std::fs::write(&path, self.to_json_pretty()?)?;
        Ok(())
    }

    pub fn to_json_pretty(&self) -> Result<String, ConfigError> {
        Ok(serde_json::to_string_pretty(self)?)
    }

    pub fn roblox_client_app_settings(&self) -> BTreeMap<String, serde_json::Value> {
        let mut settings = self.fflags.clone();
        settings
            .entry(SETTINGS_SAVE_STATE_FLAG.to_string())
            .or_insert(serde_json::Value::Bool(true));
        settings
    }
}

#[derive(Debug)]
pub enum ConfigError {
    NoConfigDir,

    Io(std::io::Error),

    Json(serde_json::Error),
}

impl fmt::Display for ConfigError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::NoConfigDir => {
                f.write_str("could not determine a config directory (is $HOME set?)")
            }
            Self::Io(e) => write!(f, "config file I/O error: {e}"),
            Self::Json(e) => write!(f, "config JSON error: {e}"),
        }
    }
}

impl std::error::Error for ConfigError {
    fn source(&self) -> Option<&(dyn std::error::Error + 'static)> {
        match self {
            Self::NoConfigDir => None,
            Self::Io(e) => Some(e),
            Self::Json(e) => Some(e),
        }
    }
}

impl From<std::io::Error> for ConfigError {
    fn from(e: std::io::Error) -> Self {
        Self::Io(e)
    }
}

impl From<serde_json::Error> for ConfigError {
    fn from(e: serde_json::Error) -> Self {
        Self::Json(e)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn default_round_trips_through_json() {
        let cfg = Config::default();
        let json = cfg.to_json_pretty().expect("serialize");
        let back: Config = serde_json::from_str(&json).expect("deserialize");
        assert_eq!(cfg, back);
    }

    #[test]
    fn partial_config_fills_missing_from_defaults() {
        let cfg: Config = serde_json::from_str(r#"{"use_opengl": true}"#).expect("parse");
        assert!(cfg.use_opengl);
        assert!(cfg.enable_gamemode);
        assert!(cfg.close_on_leave);
        assert_eq!(
            cfg.graphics_optimization_mode,
            GraphicsOptimizationMode::Balanced
        );
        assert_eq!(cfg.touch_mode, TouchMode::Off);

        assert!(!cfg.webview_allow_unsandboxed);
    }

    #[test]
    fn unknown_keys_are_ignored() {
        let cfg: Config =
            serde_json::from_str(r#"{"some_future_key": 42}"#).expect("parse with extra key");
        assert_eq!(cfg, Config::default());
    }

    #[test]
    fn enum_values_use_sober_spelling() {
        let cfg: Config = serde_json::from_str(
            r#"{"graphics_optimization_mode": "performance", "touch_mode": "fake-off"}"#,
        )
        .expect("parse");
        assert_eq!(
            cfg.graphics_optimization_mode,
            GraphicsOptimizationMode::Performance
        );
        assert_eq!(cfg.touch_mode, TouchMode::FakeOff);
        assert_eq!(TouchMode::Off.as_str(), "off");
        assert_eq!(TouchMode::On.as_str(), "on");
        assert_eq!(TouchMode::FakeOff.as_str(), "fake-off");
    }

    #[test]
    fn client_app_settings_enable_persistence_and_preserve_user_fflags() {
        let mut cfg = Config::default();
        cfg.fflags.insert(
            "DFIntExample".to_string(),
            serde_json::Value::Number(42.into()),
        );
        let settings = cfg.roblox_client_app_settings();
        assert_eq!(
            settings.get(SETTINGS_SAVE_STATE_FLAG),
            Some(&serde_json::Value::Bool(true))
        );
        assert_eq!(settings.get("DFIntExample"), Some(&serde_json::json!(42)));

        cfg.fflags.insert(
            SETTINGS_SAVE_STATE_FLAG.to_string(),
            serde_json::Value::Bool(false),
        );
        assert_eq!(
            cfg.roblox_client_app_settings()
                .get(SETTINGS_SAVE_STATE_FLAG),
            Some(&serde_json::Value::Bool(false)),
            "an explicit user override must win"
        );
    }

    #[test]
    fn config_path_lives_under_eclipse_dir() {
        if let Ok(path) = Config::config_path() {
            assert!(path.ends_with("eclipse/config.json"), "got {path:?}");
        }
    }
}
