//! Desktop integration & services (component-map K/L · 🟢 pure Rust).
//!
//! The side features that run alongside the game: Discord Rich Presence, Feral GameMode
//! toggling, XDG Desktop Portals (notifications), Secret Service (`use_libsecret`), and the
//! general D-Bus plumbing. Mirrors Sober's `discord_rpc_enabled`, `enable_gamemode`, etc.
//!
//! Planned deps: `zbus` (GameMode/secrets/low-level D-Bus), `ashpd` (portals),
//! `discord-rich-presence`. All pure Rust.
//! TODO(M2): Discord RPC + GameMode first (highest user-visible value).
