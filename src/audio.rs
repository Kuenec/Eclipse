//! Audio (component-map G · 🟡 thin binding — purity ceiling).
//!
//! Backs the engine's Android audio (OpenSL ES / AAudio) with the host audio server via the
//! **PulseAudio API**, which also works on PipeWire (pipewire-pulse) → maximum reach.
//!
//! Note: there is **no pure-Rust audio path on Linux** — even `cpal` links ALSA (C) — so a
//! thin binding is as pure as physically possible here. Revisit native-Rust PipeWire only
//! when it stabilizes.
//!
//! Planned dep: `libpulse-binding`. TODO(M4): OpenSL ES / AAudio → Pulse stream bridge.
