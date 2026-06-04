//! Input (component-map H · 🟢 + 🟡 gamepad).
//!
//! Maps host input to Android events: keyboard/mouse come from `winit` (the graphics
//! window) → Android `MotionEvent`/`KeyEvent`; gamepad via `gilrs`. Also implements Sober's
//! `touch_mode` (off/on/fake-off).
//!
//! Purity note: `gilrs` pulls `libudev` (C); the pure-Rust target is `evdev` (read
//! `/dev/input` directly + inotify hotplug) if the small C dep ever matters.
//!
//! Planned deps: `gilrs` (→ `evdev` target); kbd/mouse via `winit` in `graphics`.
//! TODO(M4): event translation + touch_mode.
