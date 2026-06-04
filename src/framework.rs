//! Android framework backends (component-map E · 🟢 Rust native side).
//!
//! The `android.*` framework is reimplemented as Java-on-ART (`api-impl.jar`); this module
//! is the **native (JNI) side** of those classes — the part ATL writes in C, we write in
//! Rust via the `jni` crate. It wires framework calls (Activity/View/Surface, AssetManager,
//! input, sensors, clipboard, properties/`getprop`, Binder stubs) to the graphics/audio/
//! input/services modules.
//!
//! The concrete work-list comes from M0: the missing classes/methods Roblox actually calls
//! (`docs/m0-runbook.md` step 4 → `framework-worklist.txt`).
//!
//! Planned deps: `jni`. TODO(M2): implement framework natives in priority order from M0.
