//! Configuration (component-map A · 🟢 pure Rust).
//!
//! Loads/saves `config.json` (mirroring Sober's schema: `use_opengl`,
//! `graphics_optimization_mode`, `enable_gamemode`, `discord_rpc_enabled`, `fflags`,
//! `touch_mode`, …) from the XDG config dir.
//!
//! Planned deps: `serde` + `serde_json`, `directories`.
//! TODO(M1): define the `Config` struct + load/default/save.
