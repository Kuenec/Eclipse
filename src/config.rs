//! Configuration (component-map A · 🟢 pure Rust).
//!
//! Loads/saves `config.json` from the XDG config dir, mirroring Sober's schema
//! (see `docs/sober-research.md` §5.2). Field names and defaults match Sober so existing
//! user expectations carry over; `fflags` is an open pass-through of Roblox Fast Flags.
//!
//! On Linux the file lives at `$XDG_CONFIG_HOME/eclipse/config.json`
//! (`~/.config/eclipse/config.json` by default) — resolved via `directories`, never a
//! hardcoded path.

use std::collections::BTreeMap;
use std::fmt;
use std::path::PathBuf;

use directories::ProjectDirs;
use serde::{Deserialize, Serialize};

/// `graphics_optimization_mode` — Sober's quality/perf trade-off knob.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, Default)]
#[serde(rename_all = "lowercase")]
pub enum GraphicsOptimizationMode {
    Quality,
    #[default]
    Balanced,
    Performance,
}

/// `touch_mode` — how Roblox's touch input layer is presented.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, Default)]
#[serde(rename_all = "kebab-case")]
pub enum TouchMode {
    #[default]
    Off,
    On,
    FakeOff,
}

/// Eclipse configuration, mirroring Sober's `config.json` (docs/sober-research.md §5.2).
///
/// `#[serde(default)]` makes every field optional in the file: a partial `config.json`
/// (or a first run with no file) fills the rest from [`Config::default`]. Unknown keys are
/// ignored (forward-compatible with newer/older schemas).
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(default)]
pub struct Config {
    /// Force OpenGL instead of Vulkan.
    pub use_opengl: bool,
    /// Quality/balanced/performance trade-off.
    pub graphics_optimization_mode: GraphicsOptimizationMode,
    /// Use Feral GameMode for performance.
    pub enable_gamemode: bool,
    /// Scale the window to display DPI.
    pub enable_hidpi: bool,
    /// Discord Rich Presence.
    pub discord_rpc_enabled: bool,
    /// Allow the Discord "join" button.
    pub discord_rpc_show_join_button: bool,
    /// Show the server-region popup on join.
    pub server_location_indicator_enabled: bool,
    /// Quit Eclipse when you leave a game.
    pub close_on_leave: bool,
    /// Touch input presentation.
    pub touch_mode: TouchMode,
    /// Controller support.
    pub allow_gamepad_permission: bool,
    /// Console UI instead of desktop UI.
    pub use_console_experience: bool,
    /// Store the session via libsecret.
    pub use_libsecret: bool,
    /// Pass-through Roblox Fast Flags (engine config). Open-ended; values may be bool,
    /// number, or string, so they are kept as raw JSON. `BTreeMap` keeps `save` output
    /// stable (sorted keys).
    pub fflags: BTreeMap<String, serde_json::Value>,
}

impl Default for Config {
    fn default() -> Self {
        // Defaults match Sober (docs/sober-research.md §5.2): most are `false`, but
        // `enable_gamemode` and `close_on_leave` default to `true`.
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
        }
    }
}

impl Config {
    /// Resolve the config file path (`$XDG_CONFIG_HOME/eclipse/config.json`).
    ///
    /// Errors with [`ConfigError::NoConfigDir`] only when no home/config base directory can
    /// be determined (e.g. `$HOME` unset) — an actionable failure, not a silent fallback.
    pub fn config_path() -> Result<PathBuf, ConfigError> {
        let dirs = ProjectDirs::from("", "", "eclipse").ok_or(ConfigError::NoConfigDir)?;
        Ok(dirs.config_dir().join("config.json"))
    }

    /// Load the config, or return defaults when the file does not exist yet (first run is
    /// not an error). I/O and parse failures are surfaced so a corrupt file is never
    /// silently reset.
    pub fn load() -> Result<Self, ConfigError> {
        let path = Self::config_path()?;
        match std::fs::read_to_string(&path) {
            Ok(text) => Ok(serde_json::from_str(&text)?),
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => Ok(Self::default()),
            Err(e) => Err(ConfigError::Io(e)),
        }
    }

    /// Write the config as pretty JSON, creating the config directory if needed.
    pub fn save(&self) -> Result<(), ConfigError> {
        let path = Self::config_path()?;
        if let Some(parent) = path.parent() {
            std::fs::create_dir_all(parent)?;
        }
        std::fs::write(&path, self.to_json_pretty()?)?;
        Ok(())
    }

    /// Serialize to pretty JSON (used by `save` and the `eclipse config` command).
    pub fn to_json_pretty(&self) -> Result<String, ConfigError> {
        Ok(serde_json::to_string_pretty(self)?)
    }
}

/// Errors from loading/saving configuration.
#[derive(Debug)]
pub enum ConfigError {
    /// No home/config base directory could be determined.
    NoConfigDir,
    /// Reading or writing the config file failed.
    Io(std::io::Error),
    /// Parsing or serializing the config JSON failed.
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
        // Only one field present; the rest must come from defaults (the `#[serde(default)]`
        // contract that lets partial / first-run config files work).
        let cfg: Config = serde_json::from_str(r#"{"use_opengl": true}"#).expect("parse");
        assert!(cfg.use_opengl);
        assert!(cfg.enable_gamemode); // default true, not the bool default of false
        assert!(cfg.close_on_leave); // default true
        assert_eq!(
            cfg.graphics_optimization_mode,
            GraphicsOptimizationMode::Balanced
        );
        assert_eq!(cfg.touch_mode, TouchMode::Off);
    }

    #[test]
    fn unknown_keys_are_ignored() {
        // Forward/backward compatibility: a key Eclipse does not model must not break load.
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
    }

    #[test]
    fn config_path_lives_under_eclipse_dir() {
        // Skip rather than fail in an environment with no resolvable home dir.
        if let Ok(path) = Config::config_path() {
            assert!(path.ends_with("eclipse/config.json"), "got {path:?}");
        }
    }
}
