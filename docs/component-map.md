# Eclipse — Complete Component Map (under final priorities)

> **Status:** Authoritative decision matrix. Last verified **2026-06-04**.
> Supersedes the *priority ordering* in [`tech-selection.md`](./tech-selection.md)
> (the individual library rationale there still applies). See also
> [`sober-research.md`](./sober-research.md) and [`art-and-runtime.md`](./art-and-runtime.md).

## The priority order (yours, applied literally)

1. **Stability** — correctness and not-breaking beats everything. A proven component wins
   over a purer-but-shakier one.
2. **Purely-Rust** — *for every line we own.* Where the **host owns** the component (GPU
   driver, audio server, the dex VM), "pure Rust" is physically impossible, so the goal
   becomes **the thinnest possible Rust binding**. Purity never overrides #1.
3. **Minimal overhead / no bloat** — fewest dependencies, no abstraction layers we don't
   use, leanest runtime.

### What "purely-Rust" can and cannot mean (two hard ecosystem facts)

- **No pure-Rust dex VM exists.** All Rust JVMs (`rjvm`, `rusty-jvm`, `ristretto`) are
  hobby/educational, run *JVM* (not dex) bytecode, and implement no Android framework.
  They'd require dex→class translation **+** framework **+** native-loading semantics —
  the **least stable** path. ⇒ ART stays vendored (see §3).
- **No pure-Rust audio on Linux.** Even `cpal` links **ALSA (C)** under its Pulse/PipeWire
  features. ⇒ audio is always a thin C binding (see map).

So Eclipse is **"100% Rust for everything we author, thin bindings where the host owns it,
and exactly one big vendored black box (ART)."**

---

## Purity tiers (used in the map)

- 🟢 **Pure Rust** — no C linked by us; we own it end-to-end.
- 🟡 **Thin binding** — a Rust crate wrapping a host-owned C lib that *cannot* be Rust
  (GPU/audio/udev). The thinnest layer possible; the C is the host's, not our bloat.
- 🔴 **Vendored non-Rust** — large reused C/C++/Java we ship (ART, and C bits we port later).

---

## Master map — everything Eclipse needs

### A. Launcher / process
| Need | Pick | Tier | Notes (stability / overhead / purity) |
|---|---|---|---|
| CLI / subcommands | `clap` | 🟢 | Standard, lean with `derive` only. |
| Config (`config.json`) | `serde` + `serde_json` | 🟢 | Mirror Sober's schema. |
| XDG paths | `directories` | 🟢 | No hardcoded paths (portability policy). |
| Diagnostics/logging | `tracing` + `tracing-subscriber` | 🟢 | Backbone for the "observe-before-fix" policy; lean. |
| Deep links (`roblox://`) | own code + `clap` | 🟢 | Register a `.desktop` URI handler. |
| Single-instance lock | own code (flock) + `rustix` | 🟢 | Roblox forbids concurrent sessions. |
| Crash/backtrace | `std::backtrace` (+ reuse `libunwind` for ART frames) | 🟢/🔴 | Rust side pure; ART unwinding uses its vendored libunwind. |

### B. APK handling
| Need | Pick | Tier | Notes |
|---|---|---|---|
| HTTP fetch/update | `ureq` | 🟢 | Blocking, no async runtime → no `tokio` bloat. |
| TLS (our downloads) | `rustls` | 🟢 | Pure-Rust TLS; no system OpenSSL. |
| Zip container | `zip` | 🟢 | Open the APK. |
| Binary manifest (AXML) | **own reader** (`src/apk/axml.rs`) | 🟢 | 2026-06-04: dropped `axmldecoder` (it panicked on hostile AXML → abort); our total pure-Rust reader replaces it. `apk-info` only if we need full ARSC. |
| Integrity hash | `sha2` | 🟢 | Verify artifacts. |
| APK signature v2/v3 (optional) | `ring` or pure-Rust `rsa`+`sha2` | 🟢/🟡 | Only if we verify sigs vs trust source — decide M0. |

### C. Dex VM (run Roblox's Java/Kotlin) — the one black box
| Need | Pick | Tier | Notes |
|---|---|---|---|
| Execute dex | **AOSP ART + libcore** (pinned `art_standalone`) | 🔴 | No stable alternative (see §3). Off the gameplay hot path (see `art-and-runtime.md`). |
| Drive the VM | `jni` (jni-rs) | 🟢 | Pure-Rust crate calling ART's JNI Invocation API. |

### D. Native code loading (bionic) — the flagship Rust port
| Need | Pick | Tier | Notes |
|---|---|---|---|
| Load/relocate bionic `.so` | `elf_loader` + `dlopen-rs` (**target**); FFI `bionic_translation` (**v1**) | 🟢 / 🔴 | Pure-Rust loader is viable & *faster than ld.so*; v1 bridges proven C for stability, then we port. **Highest-value purity win.** |
| ELF parsing | `object` (or `goblin`) | 🟢 | rustc-grade. |
| mmap segments | `memmap2` | 🟢 | — |
| bionic↔glibc ABI shim | own Rust + `libc` (**target**); reuse C (**v1**) | 🟢 / 🔴 | The hard, security-critical core (TLS, stdio, pthread, errno, dlfcn). Port behind a conformance suite. |

### E. Android framework (`android.*`)
| Need | Pick | Tier | Notes |
|---|---|---|---|
| Framework classes | `api-impl.jar` (Java on ART) — **ours** | 🔴(Java) | Has to be Java (runs on ART); incremental stub→impl. |
| Framework native backends | own Rust via `jni` | 🟢 | The C `api-impl-jni` becomes **Rust**. |

### F. Graphics (we forward, we don't render)
| Need | Pick | Tier | Notes |
|---|---|---|---|
| Vulkan forward + WSI translate | `ash` (+`ash-window`, `raw-window-handle`) | 🟡 | Binds the host Vulkan **loader** (host-owned). Thinnest path; Vulkan-preferred. |
| EGL/GLES fallback | `khronos-egl` + thin GLES forward | 🟡 | Binds host EGL/Mesa. Fallback for old GPUs. |
| Window + surface + kbd/mouse | `winit` | 🟢 | Pure Rust; Wayland+X11; raw handle for `ash`. **Not GTK4** (Vulkan-incompatible + heavy). |
| Buffer interop (gralloc/AHardwareBuffer) | `gbm`+`drm` if needed | 🟡 | DMA-BUF zero-copy; confirm need M0. |

### G. Audio
| Need | Pick | Tier | Notes |
|---|---|---|---|
| OpenSL ES/AAudio backend | `libpulse-binding` | 🟡 | **No pure-Rust option exists** (cpal still needs ALSA-C). libpulse runs on Pulse *and* PipeWire → max reach, thinnest. Revisit `pipewire-native` when it matures. |

### H. Input
| Need | Pick | Tier | Notes |
|---|---|---|---|
| Keyboard/mouse | `winit` | 🟢 | From the window. |
| Gamepad | `evdev` (**target, pure Rust**) or `gilrs` (**convenient**) | 🟢 / 🟡 | `gilrs` pulls `libudev` (C) for hotplug; pure-Rust `evdev` + inotify is purer, slightly more work. Minor subsystem — start `gilrs`, port if it matters. |
| Touch mode | own code | 🟢 | Sober's `touch_mode` semantics. |

### I. Storage / filesystem
| Need | Pick | Tier | Notes |
|---|---|---|---|
| `/data/data`, `/storage/emulated/0`, OBB mapping | own Rust + `rustix`/`std::fs` | 🟢 | Map Android paths → host dirs (Flatpak `~/.var/app/...`). |
| Asset/resource access (`resources.arsc`) | reuse `libandroidfw` (**v1**) → own Rust (**target**) | 🔴 / 🟢 | Grow our own `axml` reader (`src/apk/axml.rs`) into the ARSC/asset path over time. |

### J. Networking
| Need | Pick | Tier | Notes |
|---|---|---|---|
| Sockets | Linux kernel | — | Native; the engine does its own. |
| VM/libcore TLS | reuse `wolfSSL` | 🔴 | libcore's JNI expects this provider; porting to rustls = reimplementing the provider (low priority). |
| Launcher TLS | `rustls` | 🟢 | (Two TLS stacks coexist; fine.) |

### K. System-service emulation
| Need | Pick | Tier | Notes |
|---|---|---|---|
| Property store (`getprop`) | own Rust | 🟢 | Return build fingerprint, `sdk_int`. |
| Binder/IPC | own Rust stub/emulate | 🟢 | Most Roblox paths avoid real Binder — confirm M0. |
| Looper/Handler, sensors, clipboard | own Rust (in framework backends) | 🟢 | Implement what Roblox touches. |
| Notifications | `ashpd` (portals) | 🟢 | Pure-Rust XDG portal client. |
| Secrets (`use_libsecret`) | `oo7` or `zbus`→Secret Service | 🟢 | Pure Rust. |

### L. Desktop integration / services
| Need | Pick | Tier | Notes |
|---|---|---|---|
| D-Bus (GameMode, portals) | `zbus` (+`ashpd`) | 🟢 | Pure Rust, **no libdbus C dep**. |
| Discord Rich Presence | `discord-rich-presence` | 🟢 | Pure Rust. |
| Packaging | **Flatpak on `org.freedesktop.Platform`** | — | **Leaner than Sober's GNOME runtime** — we use `winit`, not GTK, so we don't need the GNOME platform. Real no-bloat win. AppImage/tarball secondary. |

### M. Memory
| Need | Pick | Tier | Notes |
|---|---|---|---|
| Allocator | **system allocator** (v1); `mimalloc` only if profiled | 🟢 / 🟡 | Drop the default `mimalloc` dep → leanest, zero extra dep (no-bloat #3). Roblox's engine brings its own allocator anyway. Add `mimalloc` later *only* if our Rust-side allocation shows up in profiles. |

### N. Challenge WebView engine (login challenge only — 2026-07-03 owner decision (a))

> Added 2026-07-03 (plan M2 of `docs/web-engine-plan.md`); the 2026-06-04 matrix above is unchanged.

| Need | Pick | Tier | Notes |
|---|---|---|---|
| Render the web challenge | CEF/Chromium 149 via the `cef` crate in the OUT-OF-PROCESS `crates/eclipse-webview` helper | 🔴 (libcef, the second vendored black box) / 🟢 (every helper line is Rust) | Zero engine bytes in the ART process; lazily spawned, killed after completion. |
| IPC protocol + frame transport | own std-only Unix-socket protocol + memfd BGRA, `src/webview/` (shared into the helper by `#[path]`) | 🟢 | No tokio; SCM_RIGHTS/memfd confined `unsafe`. |
| URL redaction contract | `src/webview/redact.rs` (moved from `framework.rs`) | 🟢 | Scheme+host only, both processes. |
| Host capability probes (DT_NEEDED / sandbox / display / GPU) | `src/webview/hostprobe.rs` (pre-spawn, own `loader::elf` + owned x86-64-row-filtered ld.so.cache reader) + helper `engine.rs`/`main.rs` (live userns usability probe — create + in-namespace capability use, Chromium-predicate SUID stat, ozone table, render-node scan) | 🟢 | 2026-07-10 (plan M5). Detect-don't-assume: advisory before spawn, authoritative only on measured failure; every unavailable capability → an actionable error, never a crash/silent fallback. |
| Ship packaging (pinned CEF fetch, strip/prune, `$ORIGIN` layout) | `tools/webview-dist/package-webview.sh` (+ README with the Flatpak sketch) | — (tool) | 2026-07-10 (plan M5). Dual-digest pin verification, every shipped CEF byte extracted from the verified tarball, explicit SHIP_MEMBERS, en-US locale prune (owner decision), CREDITS.html + CEF LICENSE.txt shipped, readelf RUNPATH check, guarded OUT wipe, no-display smoke. |

---

## Purity scorecard

- 🟢 **Pure Rust (everything we author):** launcher, config, logging, APK fetch/parse,
  bionic loader+shim *(target)*, framework native backends, window/input, storage mapping,
  service emulation, portals/secrets, D-Bus, Discord, allocator(default). **This is the
  bulk of Eclipse's own code.**
- 🟡 **Thin unavoidable bindings (host-owned C):** `ash`/`khronos-egl` (GPU loader),
  `libpulse-binding` (audio server), `gilrs`/`gbm`/`drm` (udev/DRM). Cannot be Rust —
  the driver/server is the host's.
- 🔴 **Vendored black boxes:** **ART + libcore** (permanent), and *temporarily* the C
  `bionic_translation`, `libandroidfw`, `libOpenSLES`, `wolfSSL`, `libunwind` — most of
  which we **migrate to Rust over time** (see `art-and-runtime.md` §6). ART is the only
  forever-🔴.

**Net:** every line Eclipse owns is Rust; the irreducible non-Rust is (1) the GPU/audio
host libs everyone binds, and (2) ART. That's as pure as a stable Roblox-on-Linux client
can physically be in 2026.

---

## §3. "A different ART, if possible" — investigated

| Option | Stability (#1) | Purely-Rust (#2) | Verdict |
|---|---|---|---|
| **Reuse AOSP ART + libcore** | ✅ Production (it *is* Android) | ❌ C++/Java | **Chosen.** Stability-optimal; off the gameplay hot path. |
| **Pure-Rust dex VM** (write one) | ❌ Doesn't exist beyond toys; years to stabilize | ✅ | Rejected — directly violates #1. |
| **Rust JVM** (`rusty-jvm`/`ristretto`) **+ dex→class** | ❌ Hobby VM **×** lossy dex2jar **×** no framework | ⚠️ mostly | Rejected — fragility stack; violates #1 hard. |
| **Fake-JVM / native-only** (apkenv-style, **in Rust**) | ⚠️ *Only if Roblox is native-heavy*; apkenv ran simple games (Angry Birds), not big commercial clients | ✅ (no VM, no `jni`, all Rust) | **Conditional** — the *only* legitimately-more-Rust path. **Gate on M0 measurement.** Likely insufficient for full Roblox, but may drive the engine via a custom Rust bootstrap. |

**Decision (RESOLVED 2026-06-04 — reuse ART; fake-JVM ruled out for Roblox):** evidence
settles this without M0. Roblox uses a **custom Java Activity** (`com.roblox.client.ActivityNativeMain`,
the GameActivity/native-app-glue pattern) that owns the Activity lifecycle, the render
Surface, and the input event queue; the native engine **polls events from it and calls
back via JNI**, so it cannot run without that Java layer. Roblox is also a full Java/Kotlin
app shell (login WebView, account, IAP). The fake-JVM precedent (apkenv) only ever ran
*simple* native games. And Sober/ATL — who would gladly drop ART for less overhead — ship a
**full standalone ART** anyway. ⇒ **ART is unavoidable for Roblox.** This was the last open
technical lever; #2 (pure-Rust) is now maximized to the physical limit. It costs nothing
real: ART is off the gameplay hot path (no FPS cost), Apache-2.0, and every line we own
stays Rust.

---

## Changes vs the earlier scoping (what this re-ranking moved)

1. **Allocator:** `mimalloc` → **system allocator by default** (no-bloat; add mimalloc only on evidence).
2. **Flatpak runtime:** GNOME → **`org.freedesktop.Platform`** (we dropped GTK, so the GNOME runtime is needless weight).
3. **Gamepad:** flagged **pure-Rust `evdev`** as the purity target over `gilrs` (libudev-C).
4. **bionic loader:** elevated to the **#1 Rust-port priority** (feasible + high purity value).
5. **ART:** confirmed reuse, with **fake-JVM-in-Rust** as the measured ART-free candidate.
6. **Audio:** confirmed `libpulse-binding` is the *ceiling* of purity (no pure-Rust audio exists on Linux).

---

## Sources (new this round)

- Rust JVMs: [rjvm — "a tiny JVM… learning project"](https://github.com/andreabergia/rjvm) · [author's writeup (toy, no threads/JIT)](https://andreabergia.com/blog/2023/07/i-have-written-a-jvm-in-rust/) · [rusty-jvm](https://github.com/hextriclosan/rusty-jvm) · [ristretto_classfile via rustc_codegen_jvm](https://github.com/IntegralPilot/rustc_codegen_jvm)
- Rust dex (parse-only): [SUPERAndroidAnalyzer/dalvik (pure-Rust dex parser)](https://github.com/SUPERAndroidAnalyzer/dalvik) · [dexparser](https://lib.rs/crates/dexparser) · [Dalvik bytecode — AOSP](https://source.android.com/docs/core/runtime/dalvik-bytecode)
- Fake-JVM precedent: [thp/apkenv (runs native APKs sans Dalvik; simple games only)](https://github.com/thp/apkenv) · [apkenv README](https://github.com/thp/apkenv/blob/master/README)
- Audio purity: [RustAudio/cpal](https://github.com/RustAudio/cpal) · [cpal #259 PulseAudio](https://github.com/RustAudio/cpal/issues/259) · [cpal #554 PipeWire](https://github.com/RustAudio/cpal/issues/554) — ALSA-C always required on Linux
