use std::collections::BTreeMap;
use std::fmt;
use std::path::PathBuf;

use directories::ProjectDirs;
use serde::{Deserialize, Serialize};

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

    pub webview_helper_path: Option<String>,
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
            webview_helper_path: None,
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
        let text = match std::fs::read_to_string(&path) {
            Ok(text) => text,
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => return Ok(Self::default()),
            Err(source) => return Err(ConfigError::Read { path, source }),
        };
        serde_json::from_str(&text).map_err(|source| ConfigError::Parse { path, source })
    }

    pub fn save(&self) -> Result<(), ConfigError> {
        let path = Self::config_path()?;
        let write_error = |source| ConfigError::Write {
            path: path.clone(),
            source,
        };
        if let Some(parent) = path.parent() {
            std::fs::create_dir_all(parent).map_err(write_error)?;
        }
        std::fs::write(&path, self.to_json_pretty()?).map_err(write_error)?;
        Ok(())
    }

    pub fn to_json_pretty(&self) -> Result<String, ConfigError> {
        Ok(serde_json::to_string_pretty(self)?)
    }

    pub fn client_app_settings_json(&self) -> Result<Vec<u8>, ConfigError> {
        let mut fflags: BTreeMap<&str, serde_json::Value> =
            BTreeMap::from([(MAXIMUM_FRAME_RATE_ROW_FLAG, serde_json::Value::from("True"))]);
        fflags.extend(
            self.fflags
                .iter()
                .map(|(name, value)| (name.as_str(), value.clone())),
        );
        let mut json = serde_json::to_vec_pretty(&fflags)?;
        json.push(b'\n');
        Ok(json)
    }
}

const MAXIMUM_FRAME_RATE_ROW_FLAG: &str = "FFlagGameBasicSettingsFramerateCap5";

#[derive(Debug)]
pub enum ConfigError {
    NoConfigDir,

    Read {
        path: PathBuf,
        source: std::io::Error,
    },

    Parse {
        path: PathBuf,
        source: serde_json::Error,
    },

    Write {
        path: PathBuf,
        source: std::io::Error,
    },

    Json(serde_json::Error),
}

impl fmt::Display for ConfigError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::NoConfigDir => {
                f.write_str("could not determine a config directory (is $HOME set?)")
            }
            Self::Read { path, source } => write!(f, "cannot read {}: {source}", path.display()),
            Self::Parse { path, source } => write!(
                f,
                "{} is not valid Eclipse settings JSON: {source}",
                path.display()
            ),
            Self::Write { path, source } => {
                write!(f, "cannot write {}: {source}", path.display())
            }
            Self::Json(e) => write!(f, "config JSON error: {e}"),
        }
    }
}

impl std::error::Error for ConfigError {
    fn source(&self) -> Option<&(dyn std::error::Error + 'static)> {
        match self {
            Self::NoConfigDir => None,
            Self::Read { source, .. } | Self::Write { source, .. } => Some(source),
            Self::Parse { source, .. } => Some(source),
            Self::Json(e) => Some(e),
        }
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

    fn client_app_settings(cfg: &Config) -> serde_json::Value {
        serde_json::from_slice(&cfg.client_app_settings_json().unwrap()).unwrap()
    }

    #[test]
    fn client_app_settings_enable_only_the_maximum_frame_rate_row_by_default() {
        assert_eq!(
            client_app_settings(&Config::default()),
            serde_json::json!({"FFlagGameBasicSettingsFramerateCap5": "True"})
        );
    }

    #[test]
    fn client_app_settings_add_the_user_fflags_to_the_default() {
        let mut cfg = Config::default();
        cfg.fflags.insert(
            "DFIntExample".to_string(),
            serde_json::Value::Number(42.into()),
        );
        assert_eq!(
            client_app_settings(&cfg),
            serde_json::json!({
                "DFIntExample": 42,
                "FFlagGameBasicSettingsFramerateCap5": "True",
            })
        );
    }

    #[test]
    fn user_fflags_override_the_default_flag() {
        let cfg: Config =
            serde_json::from_str(r#"{"fflags": {"FFlagGameBasicSettingsFramerateCap5": "False"}}"#)
                .expect("parse");
        assert_eq!(
            client_app_settings(&cfg),
            serde_json::json!({"FFlagGameBasicSettingsFramerateCap5": "False"})
        );
    }

    #[test]
    fn config_path_lives_under_eclipse_dir() {
        if let Ok(path) = Config::config_path() {
            assert!(path.ends_with("eclipse/config.json"), "got {path:?}");
        }
    }
}
