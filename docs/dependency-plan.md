# Eclipse — Dependency Plan (planned, not yet wired)

> The buildable `/Cargo.toml` has **no deps yet** (the skeleton compiles with just rustc).
> This is the plan for what each subsystem will pull in, mapped to [`component-map.md`](./component-map.md).
> Add a dep to `Cargo.toml` only when building that subsystem, after verifying the version
> with `cargo add`. Priorities: **stability > pure-Rust > no bloat.**
> 🟢 pure Rust · 🟡 thin binding to unavoidable host C · 🔴 vendored non-Rust (FFI).
> Last updated **2026-06-04**.

## Planned `[dependencies]` (by subsystem)

```toml
# --- Orchestration / launcher (src/main.rs, config, diagnostics) -------------
clap = { version = "4", features = ["derive"] }       # 🟢 CLI: run / config / uri handler
serde = { version = "1", features = ["derive"] }      # 🟢 config.json (mirror Sober schema)
serde_json = "1"                                       # 🟢
directories = "6"                                      # 🟢 portable XDG dirs (no hardcoded paths)
tracing = "0.1"                                        # 🟢 structured diagnostics
tracing-subscriber = { version = "0.3", features = ["env-filter"] }  # 🟢
rustix = "0.38"                                        # 🟢 flock (single-instance), fs/syscalls

# --- APK fetch / parse / verify (src/apk.rs) ---------------------------------
ureq = "2"                                             # 🟢 blocking HTTP, no async runtime
rustls = "0.23"                                        # 🟢 pure-Rust TLS (no system OpenSSL)
zip = { version = "2", default-features = false, features = ["deflate"] }  # 🟢 APK container (WIRED M1)
# binary AndroidManifest.xml: Eclipse OWNS the reader (src/apk/axml.rs) — NO dep. 2026-06-04:
# dropped `axmldecoder 0.3` — it *panics* on hostile AXML (aborts under panic=abort); our reader
# is total (typed errors, never panics) + pure-Rust-we-own (§2.1/2.5/2.8).
sha2 = "0.10"                                          # 🟢 artifact integrity (WIRED M1)
# apk-info = "*"        # 🟢 OPTIONAL: full AXML+ARSC if we need resources
# ring = "0.17"         # 🟢/🟡 OPTIONAL: APK signature v2/v3 verification

# --- Bind to vendored ART (src/runtime.rs) -----------------------------------
libloading = "0.9"                                     # 🟢 WIRED M1: dlopen /usr/lib/art/libart.so (🔴 ART) — no link-time ART dep
jni-sys = "0.4"                                        # 🟢 WIRED M1: raw JNI invocation types for JNI_CreateJavaVM (libcore boot proven)
# jni = "0.22"   # 🟢 full safe JNIEnv wrappers — DEFERRED to the framework-lifecycle work (driving onCreate)

# --- bionic loader / shim (src/bionic.rs) ------------------------------------
object = "0.36"                                        # 🟢 ELF parsing (rustc-grade)
elf_loader = "0.13"                                    # 🟢 pure-Rust ELF load+relocate (target)
dlopen-rs = "0.4"                                      # 🟢 pure-Rust dynamic linker
memmap2 = "0.9"                                        # 🟢 map .so segments
libc = "0.2"                                           # 🟢 glibc-side shim targets
# v1 may instead FFI the proven C `bionic_translation` (🔴) for stability, then port.

# --- Graphics: forward, don't render (src/graphics.rs) -----------------------
ash = "0.38"                                           # 🟡 low-level Vulkan (forward + WSI translate)
ash-window = "0.13"                                    # 🟡 VkSurface from window handle
raw-window-handle = "0.6"                              # 🟢 handle interchange
khronos-egl = { version = "6", features = ["dynamic"] }# 🟡 EGL for the GLES fallback path
winit = "0.30"                                         # 🟢 window + Wayland/X11 + kbd/mouse
# gbm / drm   # 🟡 OPTIONAL: DMA-BUF buffer interop (gralloc/AHardwareBuffer) — confirm M0

# --- Audio (src/audio.rs) ----------------------------------------------------
libpulse-binding = "2"                                 # 🟡 PulseAudio API (runs on Pulse AND PipeWire)
# (No pure-Rust audio exists on Linux — even cpal links ALSA-C. This is the purity ceiling.)

# --- Input (src/input.rs) ----------------------------------------------------
gilrs = "0.11"                                         # 🟡 gamepad (libudev-C; MPL-2.0 — verify)
# evdev = "0.12"        # 🟢 PURE-RUST target: /dev/input + inotify hotplug

# --- Desktop integration / services (src/services.rs) ------------------------
zbus = "4"                                             # 🟢 D-Bus: Feral GameMode, low-level portal/secret
ashpd = "0.9"                                          # 🟢 XDG portals (notifications, etc.)
discord-rich-presence = "0.2"                          # 🟢 Discord RPC

# --- Allocator ---------------------------------------------------------------
# Use the SYSTEM allocator by default (zero dep, leanest). Add only on profiling evidence:
# mimalloc = { version = "0.1", optional = true }      # 🟡
```

## Vendored non-Rust (NOT cargo deps — built/linked by the build system)

| Component | Source | License | Notes |
|---|---|---|---|
| AOSP **ART + libcore** | `gitlab.com/android_translation_layer/art_standalone` | Apache-2.0 | The dex VM. Pinned. The only forever-🔴. |
| **bionic_translation** (C) | `gitlab.com/android_translation_layer/bionic_translation` | GPLv3+ | libc/linker shim. Port to Rust over time. |
| **wolfSSL** 5.8.2 | source build w/ JNI flags | GPLv2+/commercial | libcore TLS provider. |
| **libunwind**, **libandroidfw**, **libOpenSLES** | via the ATL super-build | various | Reuse v1; migrate peripheral ones to Rust later. |
| Easiest unified build | `github.com/killerdevildog/android_translation_layer` | — | Builds the whole chain in order. |
