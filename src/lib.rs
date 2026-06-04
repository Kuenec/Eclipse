//! # Eclipse
//!
//! Open-source, Rust, distro-agnostic runtime for running the **Android x86-64 build of
//! Roblox** natively on Linux — an open alternative to the closed-source Sober.
//!
//! ## Status
//! Research/scoping is **complete** and the architecture + component set are **locked**.
//! This crate is currently a **skeleton**: each module below documents a locked subsystem
//! and its chosen library, but implementation has not begun. The first build step is the
//! manual foundation-validation in `docs/m0-runbook.md` (M0), which runs *before* wiring
//! up these modules.
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
//! Each module below is a documented stub mapping to a locked subsystem in
//! `docs/component-map.md`; a `TODO(Mn)` note marks the milestone that implements it.
//! See `AGENTS.md` for the project's engineering requirements.

pub mod apk;
pub mod audio;
pub mod bionic;
pub mod config;
pub mod diagnostics;
pub mod framework;
pub mod graphics;
pub mod input;
pub mod runtime;
pub mod services;

/// Eclipse version (from Cargo).
pub const VERSION: &str = env!("CARGO_PKG_VERSION");
