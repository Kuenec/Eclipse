# Eclipse — Dependency Plan (planned, not yet wired)

> The buildable `/Cargo.toml` has **no deps yet** (the skeleton compiles with just rustc).
> This is the plan for what each subsystem will pull in, mapped to [`component-map.md`](./component-map.md).
> Add a dep to `Cargo.toml` only when building that subsystem, after verifying the version
> with `cargo add`. Priorities: **stability > pure-Rust > no bloat.**
> 🟢 pure Rust · 🟡 thin binding to unavoidable host C · 🔴 vendored non-Rust (FFI).
> Last updated **2026-07-03**.

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
ureq = { version = "3.3", default-features = false, features = ["rustls"] }  # 🟢 WIRED 2026-06-11: blocking HTTP, no async, opt-in APK auto-fetch (src/apk/fetch.rs). Eclipse never hosts/hard-codes a Roblox source — user-configured URL only.
# rustls 0.23 comes transitively via `ureq`'s `rustls` feature (pure-Rust TLS, no system OpenSSL) — not a direct dep.
zip = { version = "2", default-features = false, features = ["deflate"] }  # 🟢 APK container (WIRED M1)
# binary AndroidManifest.xml: Eclipse OWNS the reader (src/apk/axml.rs) — NO dep. 2026-06-04:
# dropped `axmldecoder 0.3` — it *panics* on hostile AXML (aborts under panic=abort); our reader
# is total (typed errors, never panics) + pure-Rust-we-own (§2.1/2.5/2.8).
sha2 = "0.10"                                          # 🟢 artifact integrity (WIRED M1)
crc32fast = "1"                                        # 🟢 WIRED 2026-07-17: stream-check existing extracted APK entries against ZIP CRC32; already transitive via zip (zero new crates), closes same-size cross-version cache reuse
httpdate = "1.0.3"                                     # 🟢 WIRED 2026-07-17: parse CookieManager Set-Cookie `Expires` HTTP dates before the structured CEF handoff; one tiny pure-Rust/forbid-unsafe crate, prevents persistent cookies being mislabeled as session cookies
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
ab_glyph = "0.2"                                       # 🟢 WIRED 2026-06-05: pure-Rust glyph rasterizer for the View-tree TEXT pass (R8 glyph atlas)
tiny-skia = { version = "0.12", default-features = false, features = ["std", "simd"] }  # 🟢 WIRED 2026-06-05: pure-Rust software 2D rasterizer (Skia subset, no C) — fills android.graphics.Path's REAL geometry into an RGBA pixmap for the vector-drawable pass; png-format dropped (no PNG, raw RGBA → GPU)
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

# --- Framework: SQLite (src/framework/sqlite.rs) — WIRED 2026-06-11 ----------
# 🟡 The engine behind android.database.sqlite.SQLiteConnection's natives (ATL declares the full AOSP
# native surface but backs it in its GTK lib Eclipse doesn't load → Roblox's onCreate DB open is an
# UnsatisfiedLinkError; Eclipse binds them itself against the RAW FFI). `bundled` compiles the vendored
# SQLite amalgamation via `cc` (no system libsqlite3, deterministic, distro-portable). No pure-Rust
# SQLite is production-grade (stability > purity), so a thin C binding is the accepted shape — the one new
# C black box, same rationale as cpal→ALSA. Raw -sys (not rusqlite) — the JNI contract IS the C API.
libsqlite3-sys = { version = "0.38.1", features = ["bundled"] }  # 🟡 (+vcpkg, Windows-only build helper)

# --- WebView engine: challenge-only CEF helper (eclipse-webview) — OWNER DECISION (a) 2026-07-03
# (docs/web-engine-plan.md; deps added at MILESTONE 1/2 of that plan — deliberately NOT wired yet) --
# 🟡/🔴 The engine behind android.webkit.WebView for the LOGIN CHALLENGE only. It runs in an
# Eclipse-owned OUT-OF-PROCESS `eclipse-webview` Rust helper binary — zero engine bytes are ever
# mapped into the ART main process (the recorded low_4gb/no-GTK mechanism bans ANY in-process
# engine) — lazily spawned on the first challenge load-drive, killed after completion/timeout, so
# the main binary, its test gate, and the gameplay hot path carry zero cost. `cef` is
# machine-generated bindings over CEF's STABLE C API, loaded via libloading (regenerable in-house
# if upstream stalls); libcef itself is the SECOND vendored non-Rust black box, accepted under the
# libsqlite3-sys precedent (no pure-Rust web engine is production-grade — the anti-bot widget
# vendor targets Chromium/Android-WebView; stability > purity). IPC = owned std-only Unix socket +
# memfd BGRA frames (NO tokio/async runtime). Runner-up recorded: `servo` (half-yearly LTS pin) in
# the identical helper shape, gated on its own servoshell real-challenge-URL spike — triggered only
# if the vendor's scoring refuses CEF-shaped clients at plan-M6 or the owner rejects the footprint.
# 2026-07-03 (plan M2): WIRED in `crates/eclipse-webview` ONLY — the workspace-DETACHED helper
# crate (empty [workspace] table, the libm-shim/spike pattern); the root `eclipse` Cargo.toml
# NEVER gains it (root Cargo.lock verified clean of cef*). Exact pin `=149.3.0` → CEF 149.0.6 /
# Chromium 149.0.7827.201 `linux64_minimal`, archive SHA1 d46ec0d5723771bd1c9678c429e1cdb1f1ef0a72
# (download-cef-verified, M1 record). Transitive sys layer: `cef-dll-sys` with the CEF_PATH build
# contract — it fails ACTIONABLY when CEF_PATH is unset (no silent fallback); the dev-host
# instance is the M1 dist at crates/eclipse-webview-spike/cef-dist/linux-x86_64 (REUSED, never
# re-downloaded; any `export-cef-dir` output dir is equally valid). Supply chain is MOZJS-free:
# machine-generated bindings over CEF's STABLE C API, regenerable in-house if tauri-apps stalls.
# Strip/prune plan executed at package time (plan M5): libcef.so 1,375,259,784 → 256,322,688 B,
# locales → en-US; 336 MB measured (M1). Chromium third-party license-attribution obligation:
# ship CREDITS.html / the aggregate with the payload. Accepted under the libsqlite3-sys/ART
# precedent: no pure-Rust web engine is production-grade — stability > purity (AGENTS.md §3).
cef = "=149.3.0"       # 🟡/🔴 crates/eclipse-webview ONLY — never a root-crate dep
# 2026-07-03: `download-cef`/`export-cef-dir` is a DEV/PACKAGING-TIME tool only (`cargo install
# export-cef-dir`; pinned + SHA1-verified fetch) — never a [dependencies] entry of any shipped
# crate. Helper co-dependency: `libc = "0.2"` (memfd/SCM_RIGHTS FFI). The helper's protocol/
# redaction/fd-pass/shm/slot code is the ROOT crate's src/webview/* shared by #[path] includes
# (crates/eclipse-webview/src/shared.rs) — one canonical source, no duplicate implementation.

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
| **CEF `libcef`** (Chromium 149 prebuilt `linux64_minimal`) | `download-cef`, pinned + SHA1-verified | BSD-3 + Chromium third-party aggregate (must ship) | 2026-07-03 owner decision (a): the challenge-WebView engine. Helper-process ONLY — never mapped into the ART process. Stripped + locale-pruned at package time. The second 🔴 after ART (`docs/web-engine-plan.md`). M2 (2026-07-03): `cef` dep wired in the detached `crates/eclipse-webview` helper; root crate untouched (verified: root Cargo.lock diff empty of cef*). M5 (2026-07-10): packaging lives at `tools/webview-dist/package-webview.sh` (in-repo tool, the patch-framework.sh precedent) — DUAL-digest pins (the M1 SHA1 + a SHA256 of the verified on-disk tarball, both re-checked at package time independent of the CDN index), EVERY shipped CEF byte extracted from the verified tarball itself (the extracted dist is only the helper's build input, its libcef.so digest-checked against the tarball; sources never mutated), libcef stripped to the output, explicit SHIP_MEMBERS, the en-US locale prune per this row's recorded decision executed, CREDITS.html AND CEF's own LICENSE.txt shipped, helper `RUNPATH=$ORIGIN` readelf-verified, guarded OUT wipe. The script's pins + the helper's `cef = "=149.3.0"` + this row are ONE artifact pair (release bumps move all three together); the measured payload size printed by the script is the plan §7 #5 sign-off evidence. |
