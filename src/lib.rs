pub mod apk;
pub mod audio;
pub mod bionic;
#[cfg(test)]
mod bounded_child;
pub mod client_log;
mod clipboard;
pub mod diagnostics;
pub mod egl_engine;
mod font;
pub mod framework;
pub mod graphics;
mod host_fonts;
mod host_locale;
mod host_time_zone;
pub mod input;
pub mod loader;
pub mod performance;
pub mod runtime;
pub mod services;
pub mod status;
mod text_layout;
pub mod webview;

pub const VERSION: &str = env!("CARGO_PKG_VERSION");

pub const APP_ID: &str = "io.github.kuenec.Eclipse";

pub fn window_title(subject: &str) -> String {
    format!("Eclipse — {subject}")
}
