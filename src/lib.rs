pub mod apk;
pub mod audio;
pub mod bionic;
#[cfg(test)]
mod bounded_child;
pub mod client_log;
mod clipboard;
pub mod diagnostics;
pub mod egl_engine;
pub mod first_frame;
pub mod flatpak;
mod font;
pub mod framework;
pub mod gamepad;
pub mod graphics;
mod host_fonts;
mod host_locale;
mod host_time_zone;
pub mod https;
pub mod input;
pub mod links;
pub mod loader;
mod on_screen_keyboard;
pub mod performance;
pub mod portal;
pub mod runtime;
mod server_location;
pub mod services;
pub mod session;
pub mod status;
pub mod storage;
mod text_layout;
mod web_view_parent;
pub mod webview;

pub const VERSION: &str = env!("CARGO_PKG_VERSION");

pub const APP_ID: &str = "io.github.kuenec.Eclipse";

pub fn window_title(subject: &str) -> String {
    format!("Eclipse — {subject}")
}
