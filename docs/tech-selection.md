# Eclipse — Technology & Library Selection (scoping)

> **Status:** Scoping / decision record. Last verified **2026-06-04**. No application code yet.
> Companion to [`sober-research.md`](./sober-research.md). The concrete dependency list
> lives in [`/Cargo.toml`](../Cargo.toml); this doc is the *why*.

## Selection principles (in priority order)

1. **Stability first.** Prefer mature, widely-used, actively-maintained components over
   new/clever ones. A crate that's been load-bearing for years beats a faster one written
   last month.
2. **Performance.** Within "stable," pick the most performant option and the lowest
   overhead path (this is a *forwarding/translation* layer — avoid extra abstraction
   layers between the game engine and the GPU/audio).
3. **Rust-first, but only when it's actually compatible and solid.** Per the rule:
   *if a Rust option exists, is compatible, and works well → take it.* Where the only
   credible option is C/C++/Java (ART, libcore), **reuse it via FFI** rather than ship a
   weak Rust reimplementation. We don't trade correctness for purity.
4. **Detect, don't assume (distro-agnostic).** Every host-facing choice must runtime-
   detect capability (Vulkan vs GL, Wayland vs X11, Pulse vs PipeWire) and fall back or
   fail with an actionable message — never assume a vendor/driver/distro.

A reminder that shapes every graphics/audio choice: **Eclipse is not a renderer.**
Roblox's native engine issues its *own* Vulkan/GLES and audio calls. Our job is to (a)
give it the Android libraries it dynamically links (`libvulkan.so`, `libEGL.so`,
`libGLESv2.so`, `libOpenSLES.so`, …), and (b) **forward/translate** those calls to the
host's real drivers plus a host window/surface. So we want **thin, low-level bindings**
(ash, khronos-egl, libpulse) — *not* high-level engines (wgpu, rodio).

---

## Master selection table

Legend — **Layer:** ✅ Rust (we write/own it) · 🔁 Rust binding to a host/system lib ·
♻️ Reused non-Rust via FFI (vendored). **Maturity:** how safe-to-depend-on today.

| Subsystem | Choice | Layer | Maturity | Why this over alternatives |
|---|---|---|---|---|
| **Dex VM (run Roblox's Java/Kotlin)** | **AOSP ART + libcore** (vendored, via `art_standalone`) | ♻️ | Production (it *is* Android) | No credible alternative; rewriting a dex VM in Rust is a multi-year correctness minefield. dex2jar→OpenJDK is fragile on obfuscated dex and lacks Android semantics. **Reuse.** |
| **Call into ART** | **`jni`** (jni-rs) | 🔁 | Mature, standard | The standard Rust JNI crate; drives `JNI_CreateJavaVM` + lifecycle. (Context7 didn't index it — verify current version at impl.) |
| **bionic `.so` loader/linker** | **`elf_loader` + `dlopen-rs`** as the base; *bridge to C `bionic_translation` linker first* | ✅ (target) / ♻️ (v1) | Base crates mature; bionic specifics are *our* work | `dlopen-rs`/`elf_loader` are pure-Rust dynamic linkers (faster than `ld.so`, `relocate_with` custom symbol resolution, x86_64/aarch64). They give a real Rust foundation. Gaps to fill: Android packed relocations (APS2 `DT_ANDROID_REL[A]`), bionic TLS layout, ctor ordering. **v1: FFI-wrap the proven C linker; rewrite onto `elf_loader` incrementally.** |
| **bionic↔glibc ABI shim** | **Rust shim** (`libc` + hand-written wrappers); reuse C `bionic_translation` until ported | ✅ (target) / ♻️ (v1) | Our code | The hard, security-critical core (stdio `FILE`, pthread, TLS, errno, dlfcn, C++ ABI). Start by reusing the proven GPLv3 C shim; port function-by-function behind a conformance test suite. |
| **ELF parsing** | **`object`** (and/or `goblin`) | ✅ | Very mature (used by rustc) | `object` is the reference ELF/Mach-O/PE reader; `goblin` is a lighter alt. For parsing headers/symbols/relocations of the APK's `.so`s. |
| **Vulkan (forward + WSI translate)** | **`ash`** (+ `ash-window`, `raw-window-handle`) | 🔁 | Mature, 180+ rev-deps, Vk 1.1–1.3 | De-facto low-level Vulkan binding; zero-overhead, full API. We intercept `vkCreateAndroidSurfaceKHR`→`vkCreateWaylandSurfaceKHR`/xlib and forward the rest. **Not wgpu/vulkano** (abstractions we don't want in a pass-through). |
| **EGL/GLES (forward)** | **`khronos-egl`** + thin GLES symbol forwarding | 🔁 | Stable | Mesa exposes GLES + EGL natively, so mostly forward; only `eglCreateWindowSurface(ANativeWindow)` needs window mapping. GL is the fallback path; Vulkan preferred. |
| **Window + display (the game surface)** | **`winit`** | 🔁 | Mature, pure-Rust | Gives a raw Wayland/X11 surface + `raw-window-handle` for `ash`, plus input. **Not GTK4 for the game window** — *"GTK's render pipeline is incompatible with Vulkan, preventing direct drawing"* (confirmed). GTK4 only considered for a separate settings UI. |
| **Buffer interop (gralloc/AHardwareBuffer)** | **`gbm` + `drm`** (DMA-BUF), as needed | 🔁 | Stable | For zero-copy buffer sharing if the engine uses `AHardwareBuffer`/external memory. Likely needed; scope confirmed in M0. |
| **Audio** | **`libpulse-binding`** (PulseAudio API) | 🔁 | Mature (PA 8.0+) | Works on **both** PulseAudio and PipeWire (via pipewire-pulse) → max compatibility, matches Sober's `--socket=pulseaudio`. `pipewire-rs`/native-Rust PW is still evolving — revisit later, don't bet v1 on it. Backs Android OpenSL ES/AAudio. |
| **Gamepad** | **`gilrs`** | 🔁 | Active (v0.11.2, updated 2026-05-30, 6.5M dl) | Unified gamepad abstraction; maps to Android controller events. Keyboard/mouse come from `winit`. |
| **HTTP + TLS (APK fetch/update)** | **`ureq` + `rustls`** | ✅/🔁 | Mature | Blocking, light, no async runtime needed for downloads; `rustls` is pure-Rust TLS (no OpenSSL system dep → portability). Use `reqwest` only if we later need async. |
| **APK (zip) reading** | **`zip`** | ✅ | Mature | Standard zip crate to open the APK container. |
| **AndroidManifest (binary XML)** | **own reader** (`src/apk/axml.rs`); `apk-info` if full ARSC needed | ✅ | Pure-Rust, we own it | 2026-06-04: initially `axmldecoder 0.3`, but it *panics* on hostile AXML (aborts under `panic=abort`, violating §2.8). Replaced with Eclipse's own total, pure-Rust reader (package id, launcher Activity, sdk levels, `largeHeap`; ~250 lines, no dep). `apk-info` is the heavier "full AXML+ARSC" option if we later need resources. |
| **Integrity / hashing** | **`sha2`** (+ `ring` for sig verify if we verify APK sig blocks) | ✅/🔁 | Mature | Verify downloaded artifacts; optionally validate APK Signature Scheme v2/v3. |
| **Config** | **`serde` + `serde_json`** | ✅ | Mature | Mirror Sober's `config.json` schema. |
| **CLI / subcommands** | **`clap`** | ✅ | Mature | `eclipse`, `eclipse config`, deep-link handler. |
| **Logging / diagnostics** | **`tracing` + `tracing-subscriber`** | ✅ | Mature | Structured, leveled diagnostics — directly serves the project's "improve observability before fixing" policy. |
| **XDG paths** | **`directories`** | ✅ | Mature | Portable data/config/cache dirs (no hardcoded `~/...`). |
| **D-Bus (portals, GameMode, secrets)** | **`zbus`** | ✅ | Mature, pure-Rust (no libdbus C dep) | Talk to Flatpak portals, Feral **GameMode** (`com.feralinteractive.GameMode`), and Secret Service for `use_libsecret`. |
| **Discord Rich Presence** | **`discord-rich-presence`** | ✅ | Maintained | Pure-Rust Discord IPC; matches Sober's `discord_rpc_enabled`. |
| **Allocator** | **`mimalloc`** (`libmimalloc-sys`) | 🔁 | Mature | Matches Sober (and Roblox) — strong perf for the allocation pattern; set as global allocator. Alt: `tikv-jemallocator`. |
| **Packaging** | **Flatpak** (freedesktop runtime) primary; **AppImage**/tarball secondary | — | Proven | Pins GTK/Mesa userspace/glibc so the host only provides kernel + GPU ICD + display socket (the portability guarantee). NVIDIA via the Flatpak `nvidia` runtime extension. |

---

## Notes on the load-bearing decisions

### A. ART + libcore stay reused (this is the honest line)
"Completely in Rust" applies to **every line Eclipse owns** — launcher, shims, bridges,
services. The **dex VM is the one piece we vendor**, the same way a Rust app vendors
`libvulkan` or SQLite. Source: GitLab `android_translation_layer/art_standalone`
(AOSP ART patched to build on host Linux + load JNI via the translation linker); built
most easily via the [killerdevildog unified-CMake fork](https://github.com/killerdevildog/android_translation_layer).
**Escape hatch:** if M0 shows Roblox's Java surface is thin, an apkenv-style "fake JVM"
could drop ART — *measure first* (open question #2 in the research doc).

### B. The bionic loader is where Rust-first pays off most — but stage it
`dlopen-rs`/`elf_loader` proves a pure-Rust dynamic linker is viable and *fast*. But
bionic `.so`s aren't vanilla ELF: **Android packed relocations (APS2)**, **bionic TLS
slot layout**, **`.init_array` ordering**, and **gnu-hash** details differ. Plan:
- **v1 (M1):** FFI-wrap the proven C `bionic_translation` linker — ship something correct.
- **target (M3):** reimplement on `elf_loader` with a custom symbol resolver pointing
  unresolved bionic symbols at our Rust shim, behind the ABI conformance test suite.
Do **not** rewrite this first; it's the highest-risk surface (§7 of the research doc).

### C. Graphics: thin forwarding, Vulkan-preferred, GL-fallback, detect at runtime
`ash` for Vulkan, `khronos-egl` for EGL/GLES, `winit` for the surface. We **translate WSI**
(Android surface → Wayland/X11) and **forward everything else** to the host ICD with
near-zero overhead — this is why the native path beats Wine's DXVK double-translation.
Runtime-detect Vulkan (Mesa/NVIDIA); fall back to GL; if neither, fail with a clear
message (mirrors Sober's `use_opengl` + 8-year-GPU guidance).

### D. Audio: target the PulseAudio API for reach, not the trendiest one
`libpulse-binding` runs on Pulse **and** PipeWire. Native-Rust PipeWire isn't ready for a
production bet in 2026; revisit when it stabilizes. Stability > novelty.

---

## What is explicitly *not* chosen (and why)

- **wgpu / vulkano** — high-level GPU abstractions. We forward the engine's own Vulkan;
  an abstraction layer would add overhead and impedance mismatch. Use `ash`.
- **GTK4 for the game window** — Vulkan-incompatible render pipeline; heavier. (May still
  use GTK4 *only* for an optional native settings dialog, isolated from the game surface.)
- **pipewire-rs / pipewire-native (for v1)** — API still changing; not a stability bet yet.
- **reqwest + tokio (for v1)** — async runtime is unnecessary weight for "download an APK."
- **Pure-Rust dex VM** — does not exist at production quality; non-starter for Roblox.
- **native-tls / OpenSSL** — drags a system C TLS dependency that hurts portability; use
  `rustls`.

---

## License posture (must decide for an *open* project)

- Rust crates above are permissive (**MIT/Apache-2.0**), except note **`gilrs` (MPL-2.0 — verify)**.
- **AOSP ART + libcore: Apache-2.0** → friendly to bundle.
- **ATL / `bionic_translation` / `art_standalone`: GPLv3+** → if we reuse their code
  (very likely for v1), Eclipse's distributed binary inherits **GPLv3** obligations.
  This is the opposite of Sober's closed-source stance and must be an intentional choice.
- **Action:** pick Eclipse's license deliberately once we know how much GPLv3 code we
  reuse vs replace. (Listed as open question #6 in the research doc.)

---

## Open items to validate in M0 (before locking the manifest)

1. Confirm `ash`/`khronos-egl`/`winit`/`gilrs`/`elf_loader` **current versions & MSRV**
   via Context7/crates.io at implementation time (pin exact versions then).
2. Measure Roblox's **dex vs native** split → decides ART-required vs fake-JVM-possible.
3. Confirm whether the engine needs **`AHardwareBuffer`/DMA-BUF** interop (→ `gbm`/`drm`).
4. Confirm **APK signature verification** scope (do we verify v2/v3, or trust the source?).
5. Verify **`gilrs` and any uncertain licenses**.
6. Decide **Eclipse's license** given GPLv3 reuse.

---

## Sources

- Vulkan: [ash (GitHub)](https://github.com/ash-rs/ash) · [ash reverse-deps](https://crates.io/crates/ash/reverse_dependencies) · Context7 `/ash-rs/ash`
- Windowing: [winit (docs.rs)](https://docs.rs/winit/latest/winit/) · [winit (GitHub)](https://github.com/rust-windowing/winit) · [rust-gamedev windowing/graphics interop tracker](https://github.com/rust-gamedev/wg/issues/26) · [GTK4+Vulkan window init notes](https://www.oreateai.com/blog/creating-rust-gtk4-windows-and-analyzing-wayland-subsurface-technology-a-guide-to-vulkan-rendering-window-initialization-linux-edition/db34a07612461d08eeb499fc627e9feb)
- APK/AXML: [axmldecoder (docs.rs)](https://docs.rs/axmldecoder) · [apk-info (crates.io)](https://crates.io/crates/apk-info) · [apk-info-axml](https://crates.io/crates/apk-info-axml/1.0.5) · [rusty-axml](https://crates.io/crates/rusty-axml)
- Loader: [elf_loader (GitHub)](https://github.com/afgTheCat/elf_loader) · [elf_loader (lib.rs)](https://lib.rs/crates/elf_loader) · [dlopen-rs (crates.io)](https://crates.io/crates/dlopen-rs) · [dlopen-rs (docs.rs)](https://docs.rs/dlopen-rs) · [memmap2](https://crates.io/crates/memmap2)
- Audio: [libpulse-binding (docs.rs)](https://docs.rs/libpulse-binding/latest/libpulse_binding/) · [libpulse-binding (lib.rs)](https://lib.rs/crates/libpulse-binding) · [PipeWire Rust efforts (Collabora, 2025)](https://www.collabora.com/news-and-blog/blog/2025/07/03/pipewire-workshop-2025-updates-video-transport-rust-bluetooth/) · [pipewire-native (docs.rs)](https://docs.rs/pipewire-native)
- Gamepad: [gilrs (docs.rs)](https://docs.rs/gilrs/latest/gilrs/) · [gilrs (GitHub mirror)](https://github.com/Arvamer/gilrs)
- D-Bus: [zbus](https://crates.io/crates/zbus) · Context7 `/z-galaxy/zbus`
- JNI: [jni (jni-rs, crates.io)](https://crates.io/crates/jni)
- ART source: [android_translation_layer/art_standalone (GitLab)](https://gitlab.com/android_translation_layer/art_standalone) · [killerdevildog ATL fork (GitHub)](https://github.com/killerdevildog/android_translation_layer)
