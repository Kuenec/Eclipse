//! # Eclipse
//!
//! Open-source, Rust, distro-agnostic runtime for running the **Android x86-64 build of
//! Roblox** natively on Linux — an open alternative to the closed-source Sober.
//!
//! ## Status (2026-06-04)
//! M0 (foundation validation) is **complete** and M1/M2 are underway. Implemented:
//! [`diagnostics`] (tracing), [`config`] (Sober-schema config.json), [`apk`] (open a local
//! APK, own total binary-AXML reader, native-ABI/engine detection, native-lib extraction,
//! streaming SHA-256), [`runtime`] (host-ISA detection, `BootPlan`, and **booting the
//! vendored ART VM** — `dlopen libart` + `JNI_CreateJavaVM` — with Roblox's Java on the
//! classpath), and [`graphics`] (the host window via `winit`, no GTK). `eclipse run <apk>`
//! boots the VM and opens the window. The remaining modules ([`audio`], [`input`],
//! [`services`], [`bionic`], [`framework`]) are stubs; driving the Activity to `onCreate` +
//! rendering (the `framework`) is the next phase. See `AGENTS.md` §5 for live state.
//!
//! ## Architecture (see `docs/` for the full picture)
//! Eclipse follows the Android-Translation-Layer approach: run Roblox's own native engine
//! `.so` directly on the Linux kernel, give it the Android environment it expects, and
//! **forward** its graphics/audio to the host. The dex VM (**AOSP ART**, vendored) runs
//! Roblox's Java/Kotlin shell off the gameplay hot path; everything Eclipse *owns* is Rust.
//!
//! - `docs/sober-research.md` — how Sober/ATL works
//! - `docs/component-map.md` — the authoritative component matrix (this crate mirrors it)
//! - `docs/art-and-runtime.md` — the vendored ART/runtime
//! - `docs/dependency-plan.md` — what each module will depend on
//! - `docs/m0-runbook.md` — the foundation-validation step that comes next
//!
//! ## Subsystem modules
//! Each module maps to a subsystem in `docs/component-map.md`; some are implemented (above),
//! the rest are documented stubs with a `TODO(Mn)` note marking the milestone that implements
//! them. See `AGENTS.md` for the project's engineering requirements.

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
pub mod runtime;
pub mod services;

/// Eclipse version (from Cargo).
pub const VERSION: &str = env!("CARGO_PKG_VERSION");
