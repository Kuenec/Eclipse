pub mod apk;
pub mod audio;
pub mod bionic;
mod clipboard;
pub mod config;
pub mod diagnostics;
pub mod egl_engine;
mod font;
pub mod framework;
pub mod graphics;
mod host_locale;
pub mod input;
pub mod loader;
pub mod performance;
pub mod runtime;
pub mod services;
pub mod webview;

pub const VERSION: &str = env!("CARGO_PKG_VERSION");

pub const APP_ID: &str = "io.github.kuenec.Eclipse";
