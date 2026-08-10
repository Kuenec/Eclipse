pub mod apk;
pub mod audio;
pub mod bionic;
pub mod config;
pub mod diagnostics;
pub mod egl_engine;
pub mod framework;
pub mod graphics;
pub mod input;
pub mod loader;
pub mod performance;
pub mod runtime;
pub mod services;
pub mod system_cursor;
pub mod webview;

pub const VERSION: &str = env!("CARGO_PKG_VERSION");
