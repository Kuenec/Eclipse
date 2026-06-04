//! APK acquisition, parsing & verification (component-map B · 🟢 pure Rust).
//!
//! Fetches a compatible Roblox Android x86-64 APK, verifies its integrity, opens the zip,
//! reads the binary `AndroidManifest.xml` (package id, launcher Activity, sdk levels,
//! native-lib name), and locates `lib/x86_64/*.so` + `classes*.dex`.
//!
//! Planned deps: `ureq` + `rustls` (download), `zip` (container), `axmldecoder` (manifest),
//! `sha2` (integrity); optionally `apk-info` (full ARSC) / `ring` (signature v2/v3).
//! TODO(M1): manifest reader + integrity check. Never redistribute the APK (project policy).
