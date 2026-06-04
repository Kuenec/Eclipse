//! Android runtime: ART boot, lifecycle & storage mapping (component-map C/I · 🟢 + 🔴 ART).
//!
//! Boots the **vendored AOSP ART** (`dlopen` `libart`, `JNI_CreateJavaVM` with the boot
//! image + bootclasspath + classpath = our framework jar : Roblox APK), registers the
//! framework native backends, parses the manifest, and drives the Activity lifecycle
//! (`onCreate` …). Also maps Android paths (`/data/data`, `/storage/emulated/0`, OBB) onto
//! host dirs.
//!
//! ART is **unavoidable** for Roblox (see component-map §3) and sits **off the gameplay hot
//! path**, so it costs no FPS. Compile policy: AOT libcore boot image, JIT app dex,
//! interpreter fallback (detect JIT viability at runtime).
//!
//! Planned deps: `jni` (drives ART's JNI Invocation API), `rustix`/std for path mapping.
//! Vendored (FFI, not cargo): ART + libcore (`art_standalone`). TODO(M1): boot a VM, reach `onCreate`.
