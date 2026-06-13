# AGENTS.md — Eclipse persistent charter & working state

> **READ THIS FIRST, EVERY SESSION. UPDATE IT, EVERY SESSION.**
> This file is the durable source of truth for Eclipse. It survives context
> compaction/summarization. At the start of a session, read it. Whenever you make a
> meaningful change or decision, update the **Living State** (§5) and append to the
> **Decisions Log** (§6) with a `YYYY-MM-DD` date. The harness memory index (`MEMORY.md`)
> points here so you are reminded to.

---

## 0. Authority & precedence

1. **`CLAUDE.md` — ALWAYS follow it.** It is the global engineering policy (root-cause
   fixes, no workarounds, research/Context7 before changes, surgical edits, regression
   protection, completion standard). It wins on all general engineering questions.
2. **This file** — project-specific requirements + living state, layered on top of CLAUDE.md.
3. **`docs/`** — the locked technical plan (architecture, component choices, ART, M0).

> ⚠️ **OS note:** CLAUDE.md's "Compatibility Requirements" section is written for **Windows**.
> **Eclipse targets LINUX (all distros).** Apply that section's *intent* — broad
> compatibility, **detect capabilities don't assume them**, no vendor/path/hardware
> assumptions, graceful fallback, actionable errors — to: Linux distros, Wayland **and**
> X11, GPUs (Mesa **and** NVIDIA), Vulkan **and** GL, Pulse **and** PipeWire, CPU feature
> levels (SSE4.1/4.2), and page sizes (4K vs 16K). Not Windows builds.

---

## 1. What Eclipse is

An **open-source, Rust, distro-agnostic** runtime that runs the **Android x86-64 build of
Roblox** natively on Linux — an open alternative to the closed-source Sober. It uses the
Android-Translation-Layer approach: run Roblox's own native engine `.so` on the Linux
kernel, give it the Android environment it expects, **forward** its Vulkan/audio to the
host, and run its Java/Kotlin shell on a **vendored AOSP ART** (off the gameplay hot path).

---

## 2. Non-negotiable engineering requirements

Each requirement names **how it is enforced** (not just stated).

1. **Purely Rust — for every line we own.** Thin Rust *bindings* are allowed only where the
   **host owns** the component (GPU loader, audio server) or it's physically impossible to
   be Rust. The **only** vendored non-Rust black box is **ART + libcore** (the dex VM,
   proven unavoidable — see `docs/component-map.md` §3). *Enforcement:* any new non-Rust
   dependency or FFI surface must be justified against §3 priorities and **logged in §6**.
2. **0 compiler warnings or errors.** *Enforcement:* before any commit/handoff,
   `cargo build` **and** `cargo clippy --all-targets --all-features -- -D warnings` **and**
   `cargo fmt --check` **and** `cargo test` must all pass clean. Backed by the `[lints]`
   table in `Cargo.toml`. Never silence a warning with `#[allow]` to "make it pass" —
   fix the cause (CLAUDE.md). An `#[allow]` is acceptable only with a one-line dated
   justification comment.
3. **Minimal `unsafe`.** Prefer safe Rust. `unsafe` is confined to where it's unavoidable
   (FFI/JNI, the bionic loader/shim, raw Vulkan). *Enforcement:* `unsafe_op_in_unsafe_fn`
   is denied; **every `unsafe` block carries a `// SAFETY:` comment** documenting the
   invariant it relies on; modules that need no `unsafe` declare `#![forbid(unsafe_code)]`.
   No `unsafe` for convenience or micro-optimization without a measured, logged reason.
4. **Optimized for performance & speed.** *Enforcement:* release profile uses fat LTO,
   `codegen-units=1`, `panic=abort`, `strip`. **Hot paths** (the Vulkan/audio forwarding
   thunks, per-frame work, per-event work, FFI/JNI crossings) must be **allocation-free,
   lock-free where possible, and zero-cost**. The forwarding layer must not add measurable
   per-call overhead. Optimize with **evidence** (benchmark/profile before & after) — do
   not micro-optimize blindly (balance with CLAUDE.md "Simplicity First").
5. **No bloat.** Minimize dependencies; **prefer `std`**; **no async runtime** (`tokio`)
   unless a subsystem genuinely requires it. *Enforcement:* every new dep is justified vs
   stability/pure-Rust/no-bloat and recorded in `docs/dependency-plan.md`; periodically
   audit with `cargo tree` (transitive deps) and `cargo bloat` (binary size). Keep the
   public API surface tight (`unreachable_pub`).
6. **As few allocations as possible.** Borrow over own (`&str` over `String`, slices over
   `Vec`); reuse buffers; no heap allocation per frame / per input event / per FFI call;
   avoid hidden `clone()`/`collect()` in loops. *Enforcement:* clippy lints + targeted
   allocation profiling (e.g. heaptrack / a counting allocator in tests) on hot paths.
7. **Stability first / root-cause only.** No workarounds, symptom-hiding, error
   suppression, or feature-disabling (CLAUDE.md core principle). Diagnose before fixing.
8. **No panics in library/hot-path code.** Return typed `Result`s with context; `unwrap`/
   `expect`/`panic!` only in `main`/setup (with a clear message) or tests. Never let a
   panic unwind across an FFI/JNI boundary.
9. **Detect, don't assume (distro-agnostic).** Runtime-detect Vulkan/GL, Wayland/X11,
   Pulse/PipeWire, CPU features, page size; fall back gracefully; fail with an actionable
   message. No hardcoded paths/usernames/vendors (CLAUDE.md "Build & Environment Portability").
10. **Regression protection tied to root causes** (CLAUDE.md): the ABI-shim conformance
    suite, capability/fallback tests, and a CI smoke boot are the primary guards. No
    unnecessary new scripts.
11. **Reproducible & pinned.** Pin the MSRV (`rust-version` in `Cargo.toml`), pin the
    vendored ART commit, commit `Cargo.lock`. Builds must work from a clean checkout on any
    machine (no dev-machine assumptions).
12. **Documentation discipline:** module-level docs on every subsystem; `// SAFETY:` on
    unsafe; dated comments (`YYYY-MM-DD`) for non-obvious behavior/assumptions (CLAUDE.md).

---

## 3. Priorities (use these to break ties)

1. **Stability**  2. **Purely-Rust** (per §2.1)  3. **Minimal overhead / performance / no
bloat** (§2.4–2.6). Lower numbers win. #2 and #3 never override #1. The one place this bit:
ART stays vendored (stability beats purity). Everywhere else the three priorities agree.

---

## 4. Session workflow & quality gate

**At session start:** read `CLAUDE.md` (already in context), **this file**, `MEMORY.md`,
and the relevant `docs/`. Reconcile the Living State (§5) with the actual repo before acting.

**Before changing code:** follow CLAUDE.md — read the relevant code, use **Context7** for
any external library/API, state assumptions, define success criteria.

**Quality gate — run before declaring any work done and before any commit:**
```bash
cargo fmt --all
cargo build --all-targets
cargo clippy --all-targets --all-features -- -D warnings
cargo test
# and for shipped artifacts:
cargo build --release
```
All must be clean (0 warnings/errors). Report the actual results — never claim done unverified.

**After meaningful changes:** update §5 (Living State) and append a dated entry to §6
(Decisions Log). Keep "Next actions" current.

**Outward actions:** commit & push are **authorized** to the repo in §8 (as Kuenec). Still
never commit a Roblox APK or vendored ART artifacts (`.gitignore` guards), and confirm
before any history-rewriting/force operation.

---

## 5. Living State  *(UPDATE EACH SESSION)*

- **2026-06-13 — 🖼️ RENDER PHASE 6 SHIPPED: VULKAN WSI TRANSLATION (Android→Wayland) via a tier-0 `dlsym` interposer.
  ⇐ START HERE NEXT SESSION.** After Phase 5 un-gated Vulkan (API 28), Mode 6 failed `Unable to create Vulkan instance`
  because the engine requests the Android-only `VK_KHR_android_surface` instance extension, absent from the host Linux
  Vulkan ICD. KEY MECHANISM (proven, settles the Phase-5 plan's open question): the engine **`dlopen`s `libvulkan.so` at
  RUNTIME and `dlsym`s the loader commands by name** — `vk*` are NOT `DT_NEEDED`/UND imports, so the tier-0 `vk*` natives
  are NEVER consulted for it (diagnostic: zero shim hits until `dlsym` was intercepted). But `dlsym` IS a UND import the
  engine resolves through Eclipse's scope. FIX (`src/loader/vulkan_wsi.rs` + a tier-0 `dlsym` registration in
  `native_provider`): `eclipse_dlsym` returns Eclipse's WSI-translating shims for the three Vulkan-loader entry points the
  engine looks up by name (`vkGetInstanceProcAddr` is load-bearing — the engine reaches the rest THROUGH it), forwarding
  every other symbol unchanged to the host `dlsym`. The shims: `eclipse_vk_create_instance` swaps
  `VK_KHR_android_surface`→`VK_KHR_wayland_surface` in the enabled-extension list (order/pNext/appinfo/layers preserved)
  and forwards to the host `vkCreateInstance`; `eclipse_vk_create_android_surface_khr` builds a
  `VkWaylandSurfaceCreateInfoKHR` from Eclipse's own winit `wl_display` (`ndk_registry::wsi_display`) + `wl_surface`
  (`ndk_registry::wsi_wl_surface`, newly published by `graphics.rs::resumed`) and forwards to the host
  `vkCreateWaylandSurfaceKHR` (the engine's `ANativeWindow` is ignored — Eclipse owns the real WSI). LIVE BOOT
  (`/tmp/eclipse-vkdlsym.log`, EXIT=124): **✅ Vulkan instance + surface + device now CREATE** — the engine resolves
  hundreds of `vk*` through Eclipse's `vkGetInstanceProcAddr` and progresses to `RenderView created[1]`; `Mode 6` no
  longer fails on instance creation. Gate clean (566 unit incl. `vulkan_wsi` swap + `wsi_wl_surface` round-trip tests +
  4 integ + 2 doctest; 134-base count test updated). **NEW SINGLE COMMON RENDER BLOCKER (both paths now hit it):**
  `Mode 6 failed: Error opening shader pack vulkan_mobile` AND `Mode 4 failed: Error opening shader pack glsles3` →
  `RenderView is NULL`. So the shader-pack open is the COMMON final blocker for BOTH Vulkan and GLES3 (NOT path-specific
  as Phase-5 §5 framed it). EVIDENCE (Phase-5 forensics, still valid): the engine reads the pack DIRECTLY from the APK
  (its own zip reader — `lseek` to the CRC-valid STORED entry; it ignores the Phase-4 extracted FS tree; corrupting the
  FS copy was byte-identical) and rejects the valid `RBXS` bytes POST-READ. Since it is now common to BOTH packs/APIs,
  the failure is in the engine's COMMON pack open/read/parse, not an API/GL/Vulkan-surface step. GRANULAR SYSCALL TRACE
  DONE (attach-late read/pread/lseek, 6.3M lines): the engine reads each pack DIRECTLY from the APK —
  `lseek(apkfd,<data_off>); read(apkfd,"RBXS\v\0\1\0…default\0…",8192)=8192` (FULL read, NO short/failed reads → Eclipse
  file IO is sound) — i.e. it reads the `RBXS` header + variant table (`default` = first shader variant) and **rejects
  during EARLY PARSE: it never reads past the first 8 KB** (exactly ONE data-offset seek + ONE RBXS read per pack across
  ~70 retry frames). Both packs are RBXS format version `0x0b`. (It also re-scans the whole zip central directory every
  retry — ~1.3M reads — a perf artifact of the per-frame retry, NOT the reject cause; it does find+read the entry.)
  CONCLUSION: the reject is INSIDE the engine's proprietary RBXS parse / variant-selection on a valid+complete+CRC-correct
  +same-build pack. **PROVISIONING/VERSION-SKEW RULED OUT (2026-06-13):** all prior boots used the SUSPECT
  `roblox-2.721.1108.apk` (a ~254 MB all-arches *universal* merge). Re-booted the KNOWN-CONSISTENT
  `$HOME/eclipse-m0/apk/v2.724.735/roblox-2.724.735-merged.apk` (Eclipse's OWN default path — base.apk + ONLY the
  `config.x86_64` split of the SAME build code 2460, so `lib/x86_64/libroblox.so` + `assets/shaders/*.pack` cannot
  version-skew) → the engine rejects BOTH packs IDENTICALLY (`Error opening shader pack vulkan_mobile` + `glsles3` →
  `RenderView is NULL`). So the engine rejects its OWN internally-consistent matching packs ⇒ the cause is
  **ECLIPSE-ENVIRONMENTAL, NOT the APK** (a real Eclipse bug, first-party-fixable). USE THE 2.724.735 DEFAULT APK GOING
  FORWARD (latest released is 2.725.1142 / 2026-06-11, but 2.724.735/code-2460 is the consistent base+x86_64 set already
  on disk + the repo default). DEVICE-IDENTITY HYPOTHESIS TESTED + DISPROVEN (2026-06-13): a multi-agent
  env-investigation workflow ranked the Java `Build.*` device identity (the SDK_INT-style JNI channel) as the top
  suspect (the prior "ruled out" probe used the wrong NATIVE property store). Tested it on the REAL channel: edited
  vendored ATL `Build.java` `MANUFACTURER "HTC"→"Google"` + `MODEL getString(...)→"Pixel 7 Pro"`, rebuilt the overlay,
  re-booted. The engine DID pick it up (`Vulkan Android Device: Google Pixel 7 Pro`, `Excluded 'Google Pixel 7
  Pro:NVIDIA GeForce RTX 5070' - disabling SuperHQ`) — but the shader pack STILL rejects IDENTICALLY (`Error opening
  shader pack` both modes → `RenderView is NULL`). So device identity is NOT the cause (matches the synthesis's own
  counter-argument: a real low-end phone gets a degraded profile yet opens these packs). [The Build.java Pixel edit is a
  gitignored local overlay experiment, harmless, can be reverted to "HTC".] **STRONGEST REMAINING CONCRETE LEAD —
  `caps.videoMemory = 67108864` (64 MiB):** the engine logs (on the Vulkan path, set BEFORE both shader failures)
  `VULKAN unifiedMemory = false, device memory = 12820938752, host memory = 24901687296, setting caps.videoMemory =
  67108864` — it enumerates 11.9 GB device memory but caps videoMemory to 64 MiB BECAUSE `unifiedMemory = false`. NO real
  Android device is discrete — they are all INTEGRATED/UNIFIED-memory GPUs — so the engine's `unifiedMemory=false`
  (discrete-GPU) branch is Roblox-on-Android-untested territory that pins videoMemory to a 64 MiB fallback. This value is
  COMMON to both modes and set BEFORE both shader-open failures (a Roblox shader/pipeline pool likely sized off
  videoMemory → too small at 64 MiB → "Error opening shader pack"). This is the clearest "Eclipse presents what no
  Android device would" environmental cause. NEXT PROBE (carries RISK): intercept `vkGetPhysicalDeviceMemoryProperties`
  (+ `...2`) in `src/loader/vulkan_wsi.rs` (via the Phase-6 `vkGetInstanceProcAddr` shim) to present an INTEGRATED/UNIFIED
  heap layout (a large `DEVICE_LOCAL|HOST_VISIBLE|HOST_COHERENT` heap) so the engine sees `unifiedMemory=true` + a large
  videoMemory like a real Android iGPU — BUT this also changes the memory types the engine ALLOCATES from, so it can
  break allocation on the discrete NVIDIA (host-visible ≠ all of VRAM); test carefully and watch for allocation
  failures. If that's untenable or doesn't fix it, the remaining cause is the RBXS format-revision/parse internals
  (RE of `libroblox` — OFF-POLICY; do NOT). Also unproven-but-cheap: force `use_opengl`-equivalent + try FFlags. Detail:
  §6 (2026-06-13 render Phase 6 — Vulkan WSI translation).
- **2026-06-13 — 🖼️ RENDER PHASE 5 SHIPPED: GUEST API LEVEL (`-DBuild.VERSION.SDK_INT`). [Superseded as START-HERE by Phase 6.]**
  Owner live boot proved the engine reaches render init but **no graphics mode succeeds → `RenderView is NULL` → no
  frames**. A multi-agent first-party forensics + an `strace`/`LD_PRELOAD`/magic-flip probe campaign (orchestrator,
  dev-host) established the chain and **corrected the Phase 4 hypothesis below** (Phase 4 was the WRONG layer — see its
  ⚠️). CONFIRMED ROOT CAUSE: ATL's `android.os.Build$VERSION` static initializer
  (`vendor/atl/src/api-impl/android/os/Build.java:111`) defaults `SDK_INT` to **23** when the JVM property
  `Build.VERSION.SDK_INT` is unset, and `runtime.rs::BootPlan::vm_options()` was heap-only — it never passed that `-D`,
  so `BootPlan.sdk_int` (correctly 35 from manifest `targetSdk`) never reached ATL. The engine reads
  `Build.VERSION.SDK_INT` over JNI as its device API level (`[FLog::Graphics] Android API 23`); at 23 (< 24) it
  hard-rejects **Mode 6 (Vulkan)** "Android version is too old to activate Vulkan" and drops onto the **Mode 4 (GLES3)**
  path. (Serving `ro.build.version.sdk` via `__system_property_get` was the WRONG channel — proven: it changed nothing.)
  FIX (1 line, `src/runtime.rs::vm_options()`): `opts.push(format!("-DBuild.VERSION.SDK_INT={}", self.sdk_int.min(28)))`.
  **The `.min(28)` clamp is load-bearing and must stay < 29:** ATL's `Activity.registerActivityLifecycleCallbacks`
  (`vendor/atl/src/api-impl/android/app/Activity.java:614`) is an empty no-op `{}`, so at `SDK_INT >= 29` androidx
  `ReportFragment` switches to the `registerActivityLifecycleCallbacks`/`onActivityPostCreated` path and the create-phase
  `ON_CREATE` dispatch (the `onPostCreate`→`Fragment.onActivityCreated` overlay path) is dropped → the
  `IllegalStateException` boot blocker (§6 2026-06-13) returns BEFORE render init. `RESOURCES_SDK_INT` auto-follows
  `SDK_INT` in ATL when its own prop is unset (no mismatch). Regression guard:
  `runtime.rs::tests::vm_options_propagate_clamped_sdk_int` (asserts `=28` for `targetSdk=35`, rejects 23/35, propagates
  a sub-28 target verbatim). Gate clean (563 unit + 4 integ + 2 doctest, fmt/clippy 0-warn, release built). OWNER LIVE
  BOOT (`/tmp/eclipse-sdk28.log`, EXIT=124): **(A) ✅ `Android API 28`** (was 23) — engine now ATTEMPTS Vulkan, loading
  `VK_KHR_surface` + `VK_KHR_android_surface`; **(B) ✅ NO `IllegalStateException`**, `ActivityNativeMain` reaches
  `onResume`/RESUMED (clamp-28 preserved the lifecycle); **(C) ⏳ still no frames** — the engine reads its shader pack
  **directly from the APK** (strace: `lseek` to the CRC-valid STORED `assets/shaders/shaders_glsles3.pack` local-header
  67686916 + data 67686984; it NEVER opens the extracted FS tree, and corrupting the FS copy's magic was byte-identical).
  **NEXT GATE = (a) VULKAN — gate (b) GLES3 device-profile was RULED OUT by probe (this session):** **(a) Vulkan
  (reference path — Sober uses it; the engine PREFERS it, Mode 6 is tried first):** `Mode 6 failed: Unable to create
  Vulkan instance` because the engine requests the Android-only **`VK_KHR_android_surface`** instance extension, which the
  host Linux Vulkan ICD lacks (it has `VK_KHR_wayland_surface`/`VK_KHR_xcb_surface`). Eclipse must add a Vulkan-surface
  translation seam (tier-0, parallel to the Phase-3 `eglGetDisplay` connection-match): intercept the engine's
  `vkCreateInstance` to swap `VK_KHR_android_surface`→`VK_KHR_wayland_surface` in `ppEnabledExtensionNames` (and have
  `vkEnumerateInstanceExtensionProperties` advertise `android_surface` so the engine requests it), then intercept
  `vkCreateAndroidSurfaceKHR(instance,{ANativeWindow})`→`vkCreateWaylandSurfaceKHR(instance,{wl_display,wl_surface})`
  using winit's `wl_display`+`wl_surface` from `ndk_registry` WSI (the same handles Phase 1/3 publish). The engine routes
  `vk*` to host `libvulkan` via `bionic_env` tier 1 (like `egl*`/`gl*`); Eclipse intercepts only the 2-3 surface calls at
  tier 0. This is the high-confidence path and warrants a focused forensics+design workflow. **(b) GLES3 device-profile —
  RULED OUT as the render blocker (probe evidence):** serving the 3 native device keys the engine actually reads
  (captured live via `__system_property_get`: `ro.product.model`/`ro.hardware`/`ro.soc.manufacturer`) with sane values
  (`Pixel 7 Pro`/`cheetah`/`Qualcomm`) changed NEITHER the `HTC unknown` profile NOR the shader-pack open — because the
  "HTC unknown" identity is sourced from Java `Build.MANUFACTURER`/`Build.MODEL` over JNI (the SAME channel as SDK_INT),
  not the native property store, AND the `Error opening shader pack glsles3` is independent of the device profile (it is
  the Vulkan-fallback path the engine does not robustly use on this config). So the empty native property store is NOT
  the render blocker; populating it (and/or the Java `Build.*` device identity) is at most a separate User-Agent/quality
  correctness nicety, NOT on the render critical path. Detail: §6 (2026-06-13 render Phase 5 — guest API level).
- **2026-06-13 — ⚠️ RENDER PHASE 4 (BUNDLED-ASSET PROVISIONING) — CORRECTED BY PHASE 5: it was the WRONG layer.** The
  `Apk::extract_assets` wiring (extract APK `assets/` → app-data `files/assets/`) is harmless and still ships, BUT the
  Phase 5 strace probe PROVED the engine reads its shader packs/content **directly from the APK** (its own zip reader,
  `openat` of the .apk + `lseek` to the stored entry), NOT from the extracted FS tree — so this extraction does NOT fix
  the shader-pack open and the "FS content root" theory in this entry is SUPERSEDED. Original Phase 4 detail (kept for
  history): Render Phase 3
  (commit `c5681bc`) fixed the EGL connection-match: the engine now creates its EGL CONTEXT + 800×600 window surface
  successfully (live boot logged `[FLog::Graphics]` "Initialized EGL context … with renderbuffer 800x600",
  `eglSwapInterval(1)`, GL extensions + framebuffer caps enumerated — NO more `eglCreateWindowSurface` 3003). But the
  NEXT render blocker appeared: `[FLog::SurfaceController] Mode 4 failed: Error opening shader pack glsles3
  (<app_data_dir>/files/assets/content/../shaders/shaders_glsles3.pack)` then `RenderView is NULL` → no frames. CONFIRMED
  ROOT CAUSE (owner live boot, commit `c5681bc`, EXIT=124): the engine reads its shader packs (and fonts/content) from
  the FILESYSTEM under its content root `app_data_dir/files/assets/shaders/shaders_glsles3.pack` (the logged
  `content/../shaders/` normalises to `files/assets/shaders/`), but Eclipse extracted ONLY `lib/x86_64/*.so` from the APK
  and NEVER the `assets/` tree, so `files/assets/` held only empty `android/content/ExtraContent` dirs and no `shaders/`
  → shader-pack open fails → `RenderView` NULL. The APK bundles `assets/shaders/shaders_glsles3.pack` (~9.6 MB) +
  `shaders_vulkan_mobile.pack` (~20 MB); the full `assets/` tree is ~105 MB (shaders/ ExtraContent/ content/ android/
  fonts/ ssl/ shared_compression_dictionaries/ com/ + PublicSuffixDatabase.list, dexopt/). FIX (3 surgical edits
  mirroring `extract_native_libs`): **(A)** `src/apk/mod.rs`: `pub fn extract_assets(&mut self, dest_dir: &Path) ->
  Result<usize, ApkError>` — collect entry names under the `assets/` prefix excluding directory entries (immutable
  borrow), then stream each via `by_name` (mutable borrow), strip the `assets/` prefix (so `assets/shaders/x.pack` →
  `dest_dir/shaders/x.pack`), create parent dirs, idempotent size-skip, atomic temp(`.partial`)+fsync+rename; `zip` 2.x
  `enclosed_name()` path-traversal safety (rejects NUL/`..`/absolute — added because the assets/ tree is nested, unlike
  the flat lib/<abi>/ extractor). Returns the count written this call; typed `ApkError`, never panics. **(B)**
  `src/framework.rs`: raised `fn app_data_dir()` → `pub fn app_data_dir()` (dated note) so the boot flow derives the
  extraction dest from the SAME source of truth `native_get_app_data_dir` returns — the path can never drift. **(C)**
  `src/main.rs::run_apk`: after the `extract_native_libs` block, `assets_dir = framework::app_data_dir()/files/assets`
  (an actionable `io::Error` when no XDG/home base resolves), prints the progress line, `apk.extract_assets(&assets_dir)?`
  — FATAL via `?` (a missing shader pack means no rendering). **OWNER LIVE-VALIDATION — START HERE NEXT SESSION (dev-host
  MAIN LOOP, EXIT=124 clean):** if `~/.cache/eclipse` was wiped or the overlay touched, rebuild the overlay FIRST with
  `tools/framework-overlay/patch-framework.sh` (`export ECLIPSE_ANDROID_FRAMEWORK_DIR=$HOME/.cache/eclipse/framework-patched`;
  `vendor/toolchain/smali/` must hold the smali 2.5.2 jars), then `cargo run -- run <APK>` on the process MAIN thread (NOT
  `cargo test` — ART aborts off-main-thread). NOTE: the FIRST boot now extracts ~105 MB of APK assets to
  `app-data/files/assets` (a few seconds; idempotent after — the second boot rewrites 0 files). Look for, in order:
  (1) the new progress lines `# Extracting Roblox bundled assets (assets/ → files/assets/) to <…>/files/assets…` then
  `extracted <n> asset file(s)` with `n` large on first boot, `n==0` on a second boot; (2) on the filesystem,
  `<app_data_dir>/files/assets/shaders/shaders_glsles3.pack` present (~9.6 MB) at the exact path the engine logged it
  could not open; (3) in the engine FLog, the prior `[FLog::SurfaceController] Mode 4 failed: Error opening shader pack
  glsles3 (…)` must be GONE — the Mode 4 shader-pack open must now SUCCEED; (4) `RenderView is NULL` must be GONE
  (`RenderView` non-NULL); (5) THE FIRST ENGINE FRAME renders in Eclipse's window (engine content — the landing UI — the
  Phase 3 EGL connection-match already created the 800×600 context + window surface). IF `RenderView` is still NULL or a
  DIFFERENT asset/shader error appears: capture the EXACT `[FLog::SurfaceController]`/`[FLog::Graphics]` line for the next
  iteration, and confirm the extracted file exists at `<app_data_dir>/files/assets/shaders/` with non-zero size; if
  `ECLIPSE_APP_DATA_DIR` is set, confirm both the extraction dest and the engine's `native_get_app_data_dir` resolve to
  that same root (they share `framework::app_data_dir()`). NOTE: the CDN 401/403 asset errors are login-gated (separate,
  expected without auth) and do NOT block the bundled shader/UI render. Record the owner laptop log path (e.g.
  `/tmp/eclipse-assets-extract-validate.log`; runtime log-observation only — do NOT RE the APK/libroblox). REGRESSION:
  `apk::tests::extract_assets_strips_prefix_preserves_subpaths_skips_non_assets_and_is_idempotent` pins prefix-strip +
  nested sub-path preservation + non-asset skip + idempotent re-extract (count 0). Gate (only the 3 work files changed —
  `src/apk/mod.rs`, `src/framework.rs`, `src/main.rs`): `cargo fmt --all -- --check` clean, `cargo build --all-targets`
  0 warn, `cargo clippy --all-targets --all-features -- -D warnings` 0 warn (forced recheck via `touch` of the 3 files),
  `cargo test` **568 passed, 0 failed (562 unit + 0 main + 4 integration, 0 SKIP + 2 doctests)** (+1 vs Phase 3's 567:
  the new `extract_assets` regression test), `cargo build --release` clean (artifact 8,945,096 bytes, grew from Phase 3's
  8,939,368 by the asset-extraction wiring). RUNTIME CORRECTNESS (does the engine render its first frame) is confirmed
  ONLY by this live boot. Detail: §6 (2026-06-13 render Phase 4 — bundled-asset provisioning entry).
- **2026-06-13 — 🖼️ RENDER PHASE 3 SHIPPED: EGL DISPLAY CONNECTION-MATCH (tier-0 `eglGetDisplay` shim).** [Superseded as
  the START-HERE marker by the RENDER PHASE 4 entry above — Phase 3's EGL connection-match HOLDS (the engine's EGL
  context + window surface now create successfully, no more 3003), but revealed the NEXT blocker: the missing on-disk
  shader pack that Phase 4 fixes.] Phase 2.1 (commit `6a75944`) freed the `wl_surface`
  before dispatch (dropped Eclipse's `VulkanRenderer` strictly first), so the engine now creates its EGL CONTEXT
  successfully — but its `eglCreateWindowSurface` STILL failed `[FLog::SurfaceController] Mode 4 failed: Error creating
  context: eglCreateWindowSurface 3003` (`EGL_BAD_ALLOC`). ROOT CAUSE (independent of Phase 2.1's two-owners fix): the
  engine resolves its `egl*` symbols through `bionic_env` tier 1 (host `libEGL.so`) — Eclipse's tier-0
  `EclipseNativeProvider` previously registered ZERO `egl*`/`gl*` names. So the engine's
  `eglGetDisplay(EGL_DEFAULT_DISPLAY=0=NULL)` ran in HOST Mesa, which per the Khronos
  `EGL_KHR_platform_wayland`/`EGL_EXT_platform_wayland` registry text (Context7, verified 2026-06-13: "When
  `EGL_DEFAULT_DISPLAY` is used, EGL connects to the default Wayland socket, similar to `wl_display_connect(3)`") opens
  Mesa's OWN `wl_display` via `wl_display_connect(NULL)` — a DIFFERENT connection object than the one `winit` opened for
  Eclipse's window. But the `ANativeWindow*` Eclipse hands the engine (`current_wsi_window` via
  `eclipse_anativewindow_fromsurface`) wraps a `wl_egl_window*` on `winit`'s `wl_surface`; `eglCreateWindowSurface`
  requires the EGLDisplay's `wl_display` and the `wl_egl_window`'s `wl_surface` on the SAME connection → crossing them =
  `EGL_BAD_ALLOC` 3003 (matches the live log exactly: `eglCreateContext` SUCCEEDS — display/config fine — but
  `eglCreateWindowSurface` 3003). Eclipse's own `__gl-test-anw` AVOIDS this because `egl_engine` builds its EGLDisplay
  from the `winit` `RawDisplayHandle::Wayland` `wl_display` (the SAME connection as its `wl_egl_window`). FIX (3 surgical
  edits mirroring the existing WSI-window plumbing): **(A)** `ndk_registry`: a `WSI_DISPLAY: Mutex<Option<usize>>` +
  `set_wsi_display(Option<usize>)`/`wsi_display() -> Option<usize>` pair next to `register_wsi_window`/
  `current_wsi_window` — stores the winit `wl_display*` as a `usize` VALUE (the module is `#![forbid(unsafe_code)]`),
  best-effort/poison-safe (§2.8). **(B)** `native_provider`: a pure JVM-free helper
  `resolve_egl_display_target(display_id, wsi)` (`EGL_DEFAULT_DISPLAY=0` + Wayland → winit `wl_display`; else
  pass-through), a cached `host_egl_get_display()` (`OnceLock<Option<usize>>` doing its OWN
  `dlopen("libEGL.so", RTLD_NOW|RTLD_LOCAL)` + `dlsym("eglGetDisplay")` so the shim NEVER re-enters the engine's
  relocated symbol — no recursion; `None` → `EGL_NO_DISPLAY`, clean failure), and the tier-0 native
  `eclipse_egl_get_display` delegating to the host fn with the remapped target; registered `"eglGetDisplay"` in
  `with_bionic_natives` before the ANativeWindow block (wins over host `libEGL` by `resolve`'s first-strong-match).
  **(C)** `graphics::resumed`: right after the Phase 1 WSI-publish block, match `window.display_handle().as_raw()` →
  `RawDisplayHandle::Wayland(d)` → `set_wsi_display(Some(d.display.as_ptr() as usize))`, else/`Err` → `set_wsi_display(None)`
  (the SAME `wl_display` pointer `egl_engine` uses for `__gl-test-anw`). **OWNER LIVE-VALIDATION — START HERE NEXT
  SESSION (dev-host MAIN LOOP, EXIT=124 clean):** if `~/.cache/eclipse` was wiped or the overlay touched, rebuild the
  overlay FIRST with `tools/framework-overlay/patch-framework.sh` (`export
  ECLIPSE_ANDROID_FRAMEWORK_DIR=$HOME/.cache/eclipse/framework-patched`; `vendor/toolchain/smali/` must hold the smali
  2.5.2 jars), then `cargo run -- run <APK>` on the process MAIN thread (NOT `cargo test` — ART aborts off-main-thread).
  Look for, in order: (1) NO more `[FLog::SurfaceController] Mode 4 failed: ... eglCreateWindowSurface 3003`
  (`EGL_BAD_ALLOC`) — the engine's `eglCreateWindowSurface` must now SUCCEED on Eclipse's window (the engine's EGLDisplay
  now shares the `wl_egl_window`'s winit `wl_display` connection); (2) the engine's GLES2 context goes current; (3) THE
  FIRST ENGINE FRAME renders in Eclipse's window (engine content, NOT Eclipse's clear loop; Phase 1 logged
  width=800 height=600). DISCRIMINATE `eglGetDisplay` vs `eglGetPlatformDisplay` (the one unproven leg): confirm via the
  SurfaceController/EGL log whether the tier-0 `eclipse_egl_get_display` fired AND took the Wayland branch (proving the
  engine calls `eglGetDisplay` AND the tier-0 override beat host `libEGL`). IF `eglCreateWindowSurface` STILL returns
  3003 with `eglGetDisplay` intercepted: if `eclipse_egl_get_display` never fired, the engine took the platform-display
  path → add `eglGetPlatformDisplay`/`eglGetPlatformDisplayEXT` interceptions (same `EGL_DEFAULT_DISPLAY` → winit
  `wl_display` mapping, `platform == EGL_PLATFORM_WAYLAND_KHR/EXT 0x31D8`) — NOT shipped now because the engine's actual
  display-acquisition symbol cannot be pinned without RE of `libroblox` (out of scope; Eclipse's first-party docs
  enumerate only `eglGetError` among the 91 EGL/GLES imports). If still 3003 or a different EGL error, capture the exact
  SurfaceController/EGL lines + whether the engine used `eglGetDisplay` vs `eglGetPlatformDisplay` for the next iteration
  (runtime-only, log-observation, do NOT RE the APK/libroblox). Record the owner laptop log path
  (e.g. `/tmp/eclipse-egldisplay-validate.log`). REGRESSION: confirm `__gl-test` / `__gl-test-anw` still pass — they
  build their EGLDisplay via `egl_engine` (the winit `wl_display` directly), NOT via the engine's resolved symbol, so
  they must be UNAFFECTED by the new tier-0 native. RUNTIME CORRECTNESS (does the engine render) is confirmed ONLY by
  this live boot. Gate (only the 3 work files changed — `src/graphics.rs`, `src/loader/native_provider.rs`,
  `src/loader/ndk_registry.rs`): `cargo fmt --all -- --check` clean, `cargo build --all-targets` 0 warn, `cargo clippy
  --all-targets --all-features -- -D warnings` 0 warn (forced recheck via `touch` of the 3 files), `cargo test` **567
  passed, 0 failed (561 unit + 0 main + 4 integration, 0 SKIP + 2 doctests)** (+2 vs Phase 2.1's 565: the two new unit
  tests), `cargo build --release` clean (artifact 8,939,368 bytes, grew from Phase 2.1's 8,937,896 by the EGL
  interception). Regression guards: `resolve_egl_display_target_maps_default_display_to_winit_wayland_only` pins the
  confirmed-root-cause mapping (EGL_DEFAULT_DISPLAY+Wayland → winit `wl_display`; non-default & X11 pass through);
  `wsi_display_round_trips_set_and_get` pins the registry round-trip; the
  `with_bionic_natives_registers_the_three_implemented_categories` count assertion (129→130) + name list now cover
  `eglGetDisplay`. The host-EGL dlopen/dlsym delegation has no JVM-free seam (FFI to host Mesa libEGL) so its live
  tier-0 win is owner-dev-host-boot validated. Detail: §6 (2026-06-13 render Phase 3 — EGL display connection-match entry).
- **2026-06-13 — 🖼️ RENDER PHASE 2.1 SHIPPED: DROP-BEFORE-DISPATCH handoff ordering fix.** [Superseded as the
  START-HERE marker by the RENDER PHASE 3 entry above — Phase 2.1's drop-before-dispatch HOLDS (it freed the
  `wl_surface` so the engine's EGL CONTEXT now creates successfully), but revealed the remaining/independent
  `eglCreateWindowSurface` 3003 cross-connection cause that Phase 3 fixes.] Phase 2 (commit `ae20ef5`) dispatched `surfaceCreated` FIRST and
  dropped Eclipse's `VulkanRenderer` only on the NEXT `about_to_wait` tick (gated on `engine_claimed_surface`, set inside
  `fromSurface`). The owner live boot (EXIT=124 clean) proved the dispatch + handoff fire end-to-end and the engine
  subscribes (engine `surfaceCreated` ran — `MainScreenController`/`AppShellFragment` `surfaceCreated`, "Start the lua
  app"), BUT the engine's EGL surface then FAILED: `[FLog::SurfaceController] Mode 4 failed: Error creating context:
  eglCreateWindowSurface 3003` (`EGL_BAD_ALLOC`) at t=.908349, and ONLY AFTER that did Eclipse log its renderer release at
  t=.927677 — ~19 ms too late. ROOT CAUSE: the engine ran `eglCreateWindowSurface` over its `wl_egl_window` on the same
  `wl_surface` while Eclipse's `VkSurfaceKHR`/`VkSwapchainKHR` STILL owned it → two owners of one `wl_surface` →
  `EGL_BAD_ALLOC`. FIX (minimal surgical REORDER, not a redesign): drop the renderer STRICTLY BEFORE dispatching
  `surfaceCreated`, gated on a readiness probe. What changed: **(A)** `framework::engine_surface_callback_ready(vm) ->
  Result<bool>` — factored out of the dispatch self-gate; locates the `RBXSurfaceView` peer via
  `find_by_class(com.roblox.client.RBXSurfaceView)` and returns `Ok(true)` iff the peer exists AND its `SurfaceView`
  `mCallbacks` `ArrayList` is non-empty (engine's `AndroidGLView` `SurfaceHolder.Callback` registered); it dispatches
  NOTHING. The load-bearing `mCallbacks` read (`get_field` `Ljava/util/ArrayList;` → `size()I`) now lives in ONE shared
  inner helper `surface_callbacks_size`, consumed by BOTH the probe and `surface_lifecycle`'s self-gate (one source of
  truth). Same JNI discipline (null-guarded `JavaVM::from_raw` / `attach_current_thread` / `catch_unwind` / `checked`).
  **(B)** `graphics::about_to_wait`: replaced the dispatch-then-(next-tick)-drop logic with a single `handed_off` gate —
  `if !handed_off && engine_window.is_some()`, evaluate `engine_surface_callback_ready(vm)`; on `Ok(true)`: FIRST
  `self.renderer = None` (its `Drop` runs `device_wait_idle` → `destroy_swapchain` → `destroy_surface`, RELEASING the
  `wl_surface`), THEN `dispatch_surface_lifecycle(vm, w, h)` (`(w,h)` from `engine_window_geometry().unwrap_or((1,1))`)
  so the engine creates its EGL surface on the now-FREE `wl_surface`, THEN `handed_off = true` + `ControlFlow::Poll` + one
  drop-before-dispatch handoff info log. `Ok(false)` retries next tick (never blanked early); `Err` warns + retries.
  **(C)** removed the `surface_dispatched` field and folded the old `engine_claimed_surface()`-gated separate drop block
  into the single `handed_off` gate. KEPT `set_engine_claimed_surface`/`engine_claimed_surface` and its set inside
  `eclipse_anativewindow_fromsurface` — now a confirmation-only one-shot log (`else if handed_off && engine_claimed_surface()`),
  NOT the drop trigger. **OWNER LIVE-VALIDATION — START HERE NEXT SESSION (dev-host MAIN LOOP, EXIT=124 clean):** if
  `~/.cache/eclipse` was wiped or the overlay touched, rebuild the overlay FIRST with `tools/framework-overlay/patch-framework.sh`
  (`export ECLIPSE_ANDROID_FRAMEWORK_DIR=$HOME/.cache/eclipse/framework-patched`; `vendor/toolchain/smali/` must hold the
  smali 2.5.2 jars), then `cargo run -- run <APK>` on the process MAIN thread (NOT `cargo test` — ART aborts off-main-thread).
  Look for, in order: (1) the SINGLE handoff log `Eclipse released its Vulkan renderer then dispatched the SurfaceView
  lifecycle (surfaceCreated + surfaceChanged); present-loop handoff (drop-before-dispatch)` — it MUST appear BEFORE the
  engine's `eglCreateWindowSurface`, not ~19 ms after; (2) the engine's `eglCreateWindowSurface` SUCCEEDING — the
  `[FLog::SurfaceController] Mode 4 failed: ... eglCreateWindowSurface 3003` (`EGL_BAD_ALLOC`) from `ae20ef5` must be GONE
  (the `wl_surface` is now free before the engine takes it); (3) THE FIRST ENGINE FRAME visible in the window (engine
  content, NOT Eclipse's clear-and-present); (4) a separate one-shot confirmation log `engine claimed the surface
  (ANativeWindow_fromSurface returned Eclipse's WSI window)` — correlation-only, fires at most once. If
  `eglCreateWindowSurface` STILL errors (3003 or other) or the window stays blank, capture the exact SurfaceController/EGL
  log lines and the timing for the next iteration (runtime-only, log-observation, do NOT RE the APK/libroblox). If
  `engine_surface_callback_ready` never returns true (no handoff log), the `RBXSurfaceView` `mCallbacks` list never became
  non-empty — confirm the engine's `AndroidGLView` `getHolder().addCallback` ran via the `View.native_constructor` debug
  log for `com.roblox.client.RBXSurfaceView`. RUNTIME CORRECTNESS (does the engine render) is confirmed ONLY by this live
  boot. Gate (only the 3 work files changed — `src/framework.rs`, `src/graphics.rs`, `src/loader/ndk_registry.rs`):
  `cargo fmt --all -- --check` clean, `cargo build --all-targets` 0 warn, `cargo clippy --all-targets --all-features -- -D
  warnings` 0 warn (forced recheck via `touch` of the 3 files), `cargo test` **565 passed, 0 failed (559 unit + 0 main + 4
  integration, 0 SKIP + 2 doctests)** — same count as Phase 2 (a field was removed, no test was), `cargo build --release`
  clean (artifact 8,937,896 bytes, grew from Phase 2's 8,926,184 by the extracted fn + reorder + confirmation log).
  Regression guards: the `view_native_names_sigs_and_class_match_view_java` pin (`ARRAY_LIST_SIG`/`RBX_SURFACE_VIEW_CLASS`/
  `WINDOW_FORMAT_RGBA_8888`) now covers BOTH the probe and the dispatch (both go through `surface_callbacks_size`); the
  Phase 2 `find_by_class` + `engine_claimed_surface` pins stay green; `engine_surface_callback_ready` has no JVM-free seam
  (JNI field read) so its runtime behavior is owner-live-boot-validated. Detail: §6 (2026-06-13 render Phase 2.1 entry).
- **2026-06-13 — 🖼️ RENDER PHASE 2 SHIPPED: self-gated `surfaceCreated`/`surfaceChanged` dispatch to the engine's
  `RBXSurfaceView` + renderer-DROP present-loop handoff triggered when the engine pulls the surface. [Superseded by the
  RENDER PHASE 2.1 entry above — the dispatch + handoff this entry SHIPPED proved correct end-to-end in the owner live boot
  (engine subscribed + `surfaceCreated` ran), but revealed a handoff ORDERING bug (renderer dropped ~19 ms too late →
  `EGL_BAD_ALLOC` 3003); Phase 2.1 reorders the drop strictly before the dispatch. The Phase 2 4-step mechanism below still
  describes the dispatch/probe/flag plumbing, but the `surface_dispatched` field and the `engine_claimed_surface`-gated
  drop it describes are replaced by Phase 2.1's single `handed_off` gate.]** Phase 1 published Eclipse's real WSI window as the engine
  `ANativeWindow*`, but the engine does NOT call `fromSurface`/`surfaceCreated` on its own (Phase 1 live-confirmed ZERO
  such log lines). Phase 2 drives the engine to pull that surface and renders into it, and corrects the prior attempt's
  abort (going merely QUIESCENT left TWO owners of one `wl_surface`) by RELEASING the renderer instead. What it does, in 4
  steps: **(1)** `view_registry::find_by_class(name) -> Option<ViewHandle>` scans the slab for the live entry whose
  recorded `class_name` matches and returns its handle (used to locate the `RBXSurfaceView` peer). **(2)**
  `framework::dispatch_surface_lifecycle(vm, w, h) -> Result<bool>` (modeled on `dispatch_touch_to_view`: null-guarded
  `JavaVM::from_raw`, `attach_current_thread`, `catch_unwind`, inner `surface_lifecycle`): locates the peer via
  `find_by_class("com.roblox.client.RBXSurfaceView")` (None → `Ok(false)`); SELF-GATES by reading the `SurfaceView`
  `mCallbacks` `ArrayList` field (`Ljava/util/ArrayList;`) via JNI `get_field` then `size()I` — returns `Ok(false)` while
  empty (the engine has not subscribed its `AndroidGLView` `SurfaceHolder.Callback` yet, retry next tick, never blank the
  window prematurely); when non-empty, JNI-dispatches private `surfaceCreated()V` THEN `surfaceChanged(III)V`
  (`WINDOW_FORMAT_RGBA_8888=1`, w, h) per the AOSP contract and returns `Ok(true)`. Every JNI call routes through
  `checked` (a thrown exception is described + cleared + returned typed, never left pending). **(3)**
  `native_provider::eclipse_anativewindow_fromsurface` sets `ndk_registry::set_engine_claimed_surface(true)` in the
  REAL-WSI-pointer branch ONLY (NOT the geometry-only fallback) — the engine actually pulling the surface is the handoff
  trigger. **(4)** `graphics.rs`: `GameWindow` gains `surface_dispatched`/`handed_off` (both false). In `about_to_wait`,
  after `pump_main_looper`: if `!surface_dispatched && engine_window.is_some()`, call `dispatch_surface_lifecycle` with
  geometry from `engine_window_geometry().unwrap_or((1,1))` (`Ok(true)` sets the flag + logs; `Ok(false)` retries; `Err`
  warns + retries); separately, if `!handed_off && engine_claimed_surface()`, set `self.renderer = None` to DROP the
  `VulkanRenderer` (its `Drop` runs `device_wait_idle` → `destroy_swapchain` → `destroy_surface`, truly RELEASING the
  `wl_surface`/`VkSurfaceKHR`), set `handed_off`, switch to `ControlFlow::Poll`, and log the handoff. The
  `RedrawRequested` `Some(renderer)` guard then stops Eclipse drawing/re-arming `request_redraw`; the main `Looper` keeps
  pumping so the engine runs on. **OWNER LIVE-VALIDATION — START HERE NEXT SESSION (dev-host MAIN LOOP, EXIT=124 clean):**
  if `~/.cache/eclipse` was wiped or the overlay touched, rebuild the overlay FIRST with
  `tools/framework-overlay/patch-framework.sh` (`export ECLIPSE_ANDROID_FRAMEWORK_DIR=$HOME/.cache/eclipse/framework-patched`;
  `vendor/toolchain/smali/` must hold the smali 2.5.2 jars), then `cargo run -- run <APK>` on the process MAIN thread (NOT
  `cargo test` — ART aborts off-main-thread). Look for, in order: (1) the dispatch log `engine SurfaceView lifecycle
  dispatched (surfaceCreated + surfaceChanged); engine should now pull Eclipse's ANativeWindow width=<w> height=<h>` —
  fires exactly once, only once the engine's `AndroidGLView` callback registered (`mCallbacks` non-empty); before that
  `about_to_wait` silently retries each tick (the window is never blanked early); (2) the engine then calling
  `ANativeWindow_fromSurface` (sets `engine_claimed_surface`) and `eglCreateWindowSurface`; (3) the handoff log `engine
  claimed the surface; Eclipse released its Vulkan renderer (present-loop handoff)` — fires exactly once; (4) THE FIRST
  ENGINE FRAME in the window (engine frames, NOT Eclipse's clear-and-present). Watch for: NO `engine SurfaceView lifecycle
  dispatch failed` warn (a described+cleared JNI/Java exception) and NO double-dispatch (both flags are one-shot). If the
  dispatch logs but the engine does NOT then call `fromSurface` (no handoff log), the gate read of `mCallbacks` fired but
  the engine's callback path differs — capture that exact log as the next forensics signal (log-observation only; do NOT
  RE the APK/libroblox). If dispatch never fires, the `RBXSurfaceView` peer may not be captured under
  `com.roblox.client.RBXSurfaceView` — confirm via the `View.native_constructor` debug log for that class. If the engine
  renders but the handoff has a runtime issue (timing/race/blank), capture the exact log and describe for the next
  iteration. RUNTIME CORRECTNESS (does the engine render) is confirmed ONLY by this live boot. Gate (only the 5 work files
  changed — `src/framework.rs`, `src/framework/view_registry.rs`, `src/graphics.rs`, `src/loader/native_provider.rs`,
  `src/loader/ndk_registry.rs`): `cargo fmt --all -- --check` clean, `cargo build --all-targets` 0 warn, `cargo clippy
  --all-targets --all-features -- -D warnings` 0 warn (forced recheck via `touch` of all 5 files), `cargo test` **559 unit
  (+2: `find_by_class_locates_the_right_handle_and_is_none_for_absent_class` + `engine_claimed_surface_round_trips_set_and_get`)
  + 0 main-bin + 4 integration (0 SKIP) + 2 doctests = 565 passed, 0 failed**, `cargo build --release` clean (artifact
  8,926,184 bytes, grew from 8,913,704 by the Phase 2 dispatch + handoff wiring). Regression guards: the two new pins +
  the `view_native_names_sigs_and_class_match_view_java` pin extended with `ARRAY_LIST_SIG`/`RBX_SURFACE_VIEW_CLASS`/
  `WINDOW_FORMAT_RGBA_8888` (a transcription drift in any load-bearing string/value fails CI instead of silently no-op-ing
  or `NoSuchMethod`-ing the live boot). Detail: §6 (2026-06-13 render Phase 2 entry).
- **2026-06-13 — 🖼️ RENDER PHASE 1 DONE: Eclipse's REAL winit-window WSI handle is published as the engine's
  `ANativeWindow*` — `ANativeWindow_fromSurface` now returns Eclipse's actual window (live-confirmed, EXIT=124 clean) —
  but the engine does NOT yet render because it does not call `fromSurface`/`surfaceCreated` on its own. [START-HERE
  marker moved 2026-06-13 to the RENDER PHASE 2 entry at the TOP of §5 — Phase 1's WSI publish HOLDS (live-confirmed); the
  Phase 2 dispatch + handoff this entry flagged as NEXT TASK is now SHIPPED by that top entry and awaits OWNER live
  validation.]** Phase 1 (the safe first increment of the
  render-integration plan; production-side mirror of the proven `__gl-test-anw` harness) wires Eclipse's real WSI
  window into the engine ANativeWindow path: `GameWindow` gains an `engine_window: Option<egl_engine::EngineNativeWindow>`
  drop-guard; `resumed`, right after creating the winit window, reads its window/display handle + `inner_size`, calls
  `ndk_registry::set_engine_window_geometry`, and builds `EngineNativeWindow::new(window_handle, geometry)` (whose ctor
  runs `register_wsi_window`, so `ndk_registry::current_wsi_window()` becomes Eclipse's real window and
  `native_provider::eclipse_anativewindow_fromsurface` returns the real WSI handle instead of the geometry-only
  fallback; its `Drop` unregisters). `WindowEvent::Resized` re-publishes via a `publish_engine_window_geometry` helper
  (`set_engine_window_geometry` + idempotent `register_wsi_window`) so `ANativeWindow_getWidth`/`getHeight` track
  resizes. Both the no-handle and unsupported-display arms are non-fatal (warn; the window still opens and the
  geometry-only fallback stands). **OWNER LIVE-VALIDATION (current tree, dev-host main loop, EXIT=124 clean):** the boot
  logs `engine ANativeWindow published (real WSI handle); ANativeWindow_fromSurface now returns Eclipse's window
  width=800 height=600` and stays clean to APP_READY / DataModel-load. **CRUCIAL OBSERVATION (carried into the Phase 2
  plan):** with the window published but NO `surfaceCreated` dispatch, the engine does NOT call `ANativeWindow_fromSurface`
  / `surfaceCreated` on its own (ZERO such log lines) — confirming Phase 2's `surfaceCreated`/`surfaceChanged` dispatch
  to the engine's `AndroidGLView` `SurfaceHolder.Callback` is REQUIRED for the engine to pull the surface and render.
  **NEXT TASK = RENDER PHASE 2 (do NOT bundle into this commit; evidence-backed below):** (a) JNI-dispatch the
  `RBXSurfaceView` `SurfaceHolder.Callback.surfaceCreated()` then `surfaceChanged(format, w, h)` to the engine's
  `AndroidGLView` callback — SELF-GATED: only dispatch once the engine has actually registered its callback (the
  `SurfaceView` `mCallbacks` list is non-empty, read via JNI), retrying each main-loop tick until then so the window is
  never blanked prematurely; `format = WINDOW_FORMAT_RGBA_8888`, `w`/`h` = published geometry; capture the `RBXSurfaceView`
  peer as a Global ref in `view_native_constructor` (find it by the concrete class name `com.roblox.client.RBXSurfaceView`).
  (b) PRESENT-LOOP HANDOFF (the CORRECT design — the prior attempt's blocking bug): when the engine has CLAIMED the
  surface, Eclipse must `self.renderer.take()` to DROP the `VulkanRenderer` (its `Drop` does `device_wait_idle` +
  `destroy_swapchain` + `destroy_surface`, truly RELEASING the `wl_surface`/`VkSurfaceKHR`) — going merely quiescent
  leaves TWO owners of one surface; trigger the `take()` off the engine actually claiming the surface (set a flag inside
  `eclipse_anativewindow_fromsurface` when it returns the real WSI pointer) so Eclipse holds the surface until the engine
  genuinely takes it, then releases. Keep pumping the main `Looper`. Detail + the prior abort's lesson: §6 (2026-06-13
  render Phase 1 entry). Gate (ONLY `src/graphics.rs` changed): `cargo fmt --all -- --check` clean, `cargo build
  --all-targets` 0 warn, `cargo clippy --all-targets --all-features -- -D warnings` 0 warn (forced recheck via
  `touch src/graphics.rs`), `cargo test` **557 unit + 0 main-bin + 4 integration (0 SKIP) + 2 doctests = 563 passed, 0
  failed** (+1 unit: the new pin `graphics::tests::publish_engine_window_geometry_registers_real_wsi_mapping`), `cargo
  build --release` clean (artifact 8,913,704 bytes, grew from 8,911,112 by the Phase 1 WSI-publish wiring). Regression
  guard: the new order-independent pin fails if `register_wsi_window` is dropped from `publish_engine_window_geometry`.
- **2026-06-13 — 🟢 ROBLOX BOOTS TO APP_READY (Startup/Landing) — `ActivityManager$MemoryInfo` is now `Parcelable`
  (`writeToParcel`/`describeContents`) — OWNER LIVE-VALIDATED (EXIT=124 clean): `writeToParcel` resolves,
  `ActivityNativeMain` gets PAST `onResume` ENTIRELY, the app reaches RESUMED + a running main `Looper` pump, the engine
  loads its DataModel (`rbxasset://places/Mobile.rbxl`) and reaches APP_READY.** Root cause: Roblox calls
  `android.app.ActivityManager$MemoryInfo.writeToParcel(Landroid/os/Parcel;I)V` in `ActivityNativeMain.onResume` startup,
  but the patched (`javac`) `MemoryInfo` (a verbatim ATL copy + the `RunningAppProcessInfo` patch) did NOT declare it →
  `NoSuchMethodError`. AOSP's `MemoryInfo` IS `Parcelable`. **Fix (javac path, NOT smali — depends on ATL's stock
  `Parcel` write-API surface):** `tools/framework-overlay/src/android/app/ActivityManager.java` — `MemoryInfo` now
  `implements android.os.Parcelable`, with `describeContents()` returning `0` and `writeToParcel(Parcel,int)` writing its
  4 fields via the stock Parcel write-API (`dest.writeLong` on `availMem`/`totalMem`/`threshold`; `dest.writeInt` on
  `lowMemory` as `1`/`0`); ATL's installed `Parcel` was verified to provide `writeLong(J)V`/`writeInt(I)V` so the calls
  resolve at runtime. The compile-only stub `tools/framework-overlay/stubs/android/os/Parcel.java` was extended with
  `writeLong(long)`/`writeInt(int)` so the patched `MemoryInfo` compiles (the stub is NEVER dexed; the real `Parcel` is
  used at runtime). `MemoryInfo` is staged into `classes.dex` by the existing `android/app/ActivityManager*.class` javac
  glob — no script change. **[START-HERE marker moved 2026-06-13 to the RENDER PHASE 1 (WSI publish) entry at the TOP of
  §5 — APP_READY holds; the render frontier flagged below (NEW FRONTIER (b)) is now advanced by RENDER PHASE 1.]** Boot
  recipe unchanged (= OWNER live validation on the dev-host MAIN LOOP): rebuild the
  overlay FIRST with `tools/framework-overlay/patch-framework.sh` if `~/.cache/eclipse` was wiped or the overlay was
  touched (`export ECLIPSE_ANDROID_FRAMEWORK_DIR=$HOME/.cache/eclipse/framework-patched`, `vendor/toolchain/smali/` must
  hold the smali 2.5.2 jars), then `cargo run -- run <APK>` on the process main thread. MILESTONE REACHED (owner-validated,
  current tree): with `MemoryInfo.writeToParcel`, `ActivityNativeMain` gets PAST `onResume`; the app reaches RESUMED +
  running main `Looper` pump, the engine loads its DataModel (`rbxasset://places/Mobile.rbxl`) and reaches APP_READY
  (Startup/Landing) — Roblox boots to the landing/app-ready stage; the boot is EXIT=124 clean. NEW FRONTIER (next tasks):
  (a) the now-TOLERATED running-loop framework gaps the pump survives (non-fatal in the pump today): bind
  `View.nativeIsAttachedToWindow` → `boolean` (return-driven; the activity view IS attached, so return `true`),
  `View.getWindowVisibleDisplayFrame(Rect)` (fill the `Rect` with the window frame), and `android.app.Dialog.nativeInit`
  → `long` (Dialog peer) — so UI/dialog messages stop throwing in the pump; (b) the STANDING render frontier — wire the
  engine `AndroidGLView` surface to Eclipse's window (Eclipse currently runs its own Vulkan clear-and-present loop while
  the engine renders to its own surface) so rendered frames appear; (c) login/auth (`apis.roblox.com` 403s —
  environmental, needs real credentials). Capture the running-loop gaps one at a time by log observation only.** Gate
  (overlay javac-path + compile-only stub only; NO Rust changed): overlay build clean (exit 0; `classes.dex` 18832B
  [grew from 18656B for the larger `Parcelable` `MemoryInfo`], `classes2.dex` 60968B UNCHANGED, `classes3.dex` 2498192B
  UNCHANGED); baksmali of `classes.dex` confirms `MemoryInfo` `.implements Landroid/os/Parcelable;` with
  `describeContents()I` returning 0 and `writeToParcel(Landroid/os/Parcel;I)V` invoking `Parcel->writeLong(J)V` ×3 +
  `Parcel->writeInt(I)V` ×1; `classes2.dex` still EXACTLY the 7 smali classes (smali path untouched). `cargo fmt --all --
  check` / `build --all-targets` (0 warn) / `clippy --all-targets --all-features -D warnings` (0 warn) / `build
  --release` (8,911,112-byte artifact) all 0-warning; `cargo test` **556 unit + 0 main-bin + 4 integration (0 SKIP) + 2
  doctests = 562 passed, 0 failed** (no Rust changed → no test delta; overlay regression protection is the build-time
  anchor/glob/stub-exclusion guards inside `patch-framework.sh`). Detail: §6 (2026-06-13 `MemoryInfo` `Parcelable`/
  `writeToParcel` + APP_READY entry).
- **2026-06-13 — 🎬 `ActivityNativeMain` IS NOW FULLY RESUMED — `Display.getMode()`/`Display$Mode` + `Vibrator.cancel()`
  OVERLAY PATCHES — OWNER LIVE-VALIDATED (EXIT=124 clean): `getMode` resolves, `onCreate`→`onPostCreate`→`onStart`→`onResume`
  ALL fire, `createGlAppsFrame` succeeds.** Roblox hits two more INSTALLED-framework gaps in `ActivityNativeMain.onResume`
  startup that ATL omits: (1) `android.view.Display.getMode()Landroid/view/Display$Mode;` — `NoSuchMethodError`; ATL's
  installed `Display` omits BOTH the method AND the `Mode` nested class. (2) `android.os.Vibrator.cancel()V` — Roblox
  calls it on a `Timer` thread (caught by Roblox's own handler = non-fatal noise) but ATL's `Vibrator`
  (`hasVibrator`/`vibrate` only) omits it. Both are **framework-overlay** patches (NOT Rust — `RegisterNatives` cannot
  add a Java *method* or a nested *type*), extending the existing step-4b smali pipeline. Same drift-proof approach as
  the `View`/`Display`/`Activity`/`Fragment` patches: **baksmali the AUTHORITATIVE installed classes**, anchor-guarded
  inserts (exact-count==1), an "already declares" drift guard, and post-insert `grep -qF` back-checks that fail the
  build loudly. **`Display.getMode()`** is anchored after the UNIQUE `getWidth()I`; it constructs a new
  `android.view.Display$Mode` from the installed `Display`'s `window_width:I`/`window_height:I` statics (the same
  `public static` fields the pre-existing `getWidth`/`getHeight` read) + `60.0f` (`const/high16 0x42700000`, consistent
  with `getWidth`/`getHeight`/`getRefreshRate`). The nested class is a NEW committed source
  `tools/framework-overlay/smali/android/view/Display$Mode.smali` (public static final, accessFlags 0x19; fields
  `mModeId`/`mWidth`/`mHeight`/`mRefreshRate`; ctor `(IIIF)V`; getters `getModeId`/`getPhysicalWidth`/
  `getPhysicalHeight`/`getRefreshRate`), assembled alongside `View`+`Display`+`Activity`+`Fragment`+`Vibrator`.
  **`Vibrator.cancel()`** is anchored after the UNIQUE `vibrate(J)V`; a `return-void` no-op faithful to Eclipse's
  no-vibration-device backing. Overlay layout grows to 7 smali classes in `classes2.dex`: `View` +
  `View$OnCapturedPointerListener` + `Display` + `Display$Mode` + `Activity` + `Fragment` + `Vibrator`; first-dex-wins.
  Working-tree changes: `tools/framework-overlay/patch-framework.sh` (the `Display.getMode` + `Vibrator.cancel` blocks
  + the assemble `cp` lines + the header comment) and the NEW committed `Display$Mode.smali`; the vendored smali jars
  stay in git-ignored `vendor/toolchain/smali/`. **[START-HERE marker moved 2026-06-13 to the `MemoryInfo`
  `Parcelable`/`writeToParcel` + APP_READY entry at the TOP of §5 — OWNER LIVE-VALIDATED that this `getMode`/
  `Vibrator.cancel` resume work holds: `ActivityNativeMain` is FULLY RESUMED. The NEW FRONTIER this entry flagged
  (`MemoryInfo.writeToParcel`) is now FIXED by that top entry, which carries the live frontier forward to APP_READY +
  the tolerated running-loop gaps.]** MILESTONE REACHED (owner-validated):
  `ActivityNativeMain` is FULLY RESUMED — `onCreate`→`onPostCreate`→`onStart`→`onResume` all fire,
  `createGlAppsFrame` succeeds, the lifecycle-ordering fix + `getMode` hold; the boot is EXIT=124 clean and advances
  PAST `getMode`. Gate (no Rust changed —
  smali-overlay + build-script only): overlay build clean (exit 0; `classes.dex` 18656B, `classes2.dex` 60968B [grew
  from 59704B Activity+Fragment by the added `Display.getMode` + `Display$Mode` + `Vibrator.cancel`], `classes3.dex`
  2498192B; `classes2.dex` verified via baksmali `list classes` to define EXACTLY `Activity` + `Fragment` + `Vibrator`
  + `Display` + `Display$Mode` + `View` + `View$OnCapturedPointerListener` — 7 classes, no strays); `cargo fmt --all --
  check` / `build --all-targets` (0 warn) / `clippy --all-targets --all-features -D warnings` (0 warn) / `build
  --release` (8,911,112-byte artifact) all 0-warning; `cargo test` **556 unit + 0 main-bin + 4 integration (0 SKIP) + 2
  doctests = 562 passed, 0 failed** (overlay regression protection is the build-time anchor/already-declares/grep
  guards inside `patch-framework.sh`). Detail: §6 (2026-06-13 `Display.getMode`/`Display$Mode` + `Vibrator.cancel`
  overlay entry).
- **2026-06-13 — 🔁 androidx LIFECYCLE-ORDERING FIX — `ON_CREATE` now dispatched during the activity's CREATE phase,
  BEFORE `onStart`.** Owner's live boot of `b480bd0` (EXIT=124 clean) advanced `ActivityNativeMain` PAST `onCreate`
  (`createGlAppsFrame` succeeds) into `onStart`, which then threw `IllegalStateException: LifecycleOwner
  ActivityNativeMain is attempting to register while current state is STARTED — must call register before STARTED`:
  `MediaPickerProtocolV2.onCreate` (a `DefaultLifecycleObserver`) calls `registerForActivityResult` whose
  `ActivityResultRegistry.register` guard throws because the activity's androidx `LifecycleRegistry` had already
  reached STARTED before `ON_CREATE` was dispatched to observers. **Root cause (confirmed first-party):**
  `ActivityNativeMain` extends androidx `ComponentActivity`; its `LifecycleRegistry` must receive `ON_CREATE` during
  the create phase. At ATL's `Build.VERSION.SDK_INT == 23` (ATL `Build.java` defaults to 23 when the property is
  unset; `runtime.rs` `vm_options()` pushes no `-DBuild.VERSION.SDK_INT`), androidx's `ReportFragment` dispatches
  `ON_CREATE` from its `android.app.Fragment.onActivityCreated(Bundle)` override. ATL dispatched NO create-phase
  fragment hook (installed `Activity.onCreate` only loops `fragment.onCreate()`; `Activity.onPostCreate` was a
  Slog-only no-op; the base `Fragment` had no `onActivityCreated`), and Eclipse's `drive_lifecycle` called `onCreate`
  → `onStart` back-to-back with nothing between — so the FIRST event the registry saw was `ReportFragment.onStart` →
  `handleLifecycleEvent(ON_START)`, which advanced `mState` to STARTED and back-filled `ON_CREATE` to lagging
  observers while already STARTED → the throw. **Fix (durable, NOT suppression — `fixLocation=both`, matching AOSP's
  `performCreate` → `onPostCreate` ordering):** (A) framework overlay (`tools/framework-overlay/patch-framework.sh`,
  step-4b smali pipeline now also shadows the INSTALLED `android.app.Activity` + `android.app.Fragment` into
  `classes2.dex`): base `Fragment` gets the AOSP no-op `onActivityCreated(Bundle)` hook (so androidx `ReportFragment`'s
  `@Override` resolves + is invoked), and the installed `Activity.onPostCreate` (was a Slog-only no-op) now iterates
  `fragments` calling `Fragment.onActivityCreated(savedInstanceState)` — the create-phase dispatch AOSP runs and ATL
  omitted. (B) Eclipse Rust (`src/framework.rs`): a new `STEP_ACTIVITY_ON_POST_CREATE` recipe step + a
  `call_activity_on_post_create` helper (null `Bundle`, `(Landroid/os/Bundle;)V`, routed through `checked`), driven
  BETWEEN `onCreate` and `onStart` in BOTH up-lifecycle drivers — `drive_lifecycle` (step 5 → 5b → 6 → 7) AND the
  static `activity_native_start_activity` (the splash→main `nativeStartActivity` handoff). ATL/Eclipse has no
  `performCreate` to invoke `onPostCreate`, so the driver must; `onPostCreate` (not `onCreate`) is the dispatch site
  because the androidx `ReportFragment` is injected during `ComponentActivity.onCreate`'s super-chain — it is present
  in `fragments` only after the whole `onCreate` chain returns, and still before `onStart`. Net: `ON_CREATE` reaches
  observers while the registry is at CREATED → `registerForActivityResult` legitimately sees CREATED and passes its
  guard; NO catch/ignore of the `IllegalStateException` anywhere. Same-pattern audit: both up-lifecycle drivers fixed;
  `nativeResumeActivity` (drives only `onResume` on an already-created/started instance) correctly left unpatched;
  `recreate()` routes through `nativeStartActivity` so it is covered. Regression guards: a new JVM-free source-order
  pin `lifecycle_drivers_call_on_post_create_between_on_create_and_on_start` (`include_str!` asserts `onCreate` <
  `onPostCreate` < `onStart` < `onResume` in both drivers — ART cannot run under `cargo test`), `STEP_ACTIVITY_ON_POST_CREATE`
  class/method/descriptor + call-site literal asserts in the existing recipe-pin cluster, and build-time overlay
  guards in `patch-framework.sh` (exact-count==1 anchors + a `perl -0777` pristine-body guard + post-insert `grep -qF`
  back-checks that fail the build loudly if the `Fragment.onActivityCreated` hook or the `Activity.onPostCreate`
  dispatch is reverted / the installed-class shape drifts). **[START-HERE marker moved 2026-06-13 to the
  `Display.getMode`/`Display$Mode` + `Vibrator.cancel` overlay entry at the TOP of §5 — OWNER LIVE-VALIDATED that this
  `onPostCreate` create-phase dispatch holds: `ActivityNativeMain.onStart` no longer throws the `IllegalStateException`
  (`register while STARTED`), `registerForActivityResult` sees CREATED, and the boot advances PAST `onStart` into
  `onResume`. The NEXT FRONTIER this entry flagged (further `onStart`/`onResume` work) is now the
  `Display.getMode`/`Vibrator.cancel` gaps fixed by that top entry, which carries the live frontier forward.]** RESIDUAL RISK to watch (note, not a blocker): the create-phase
  dispatch reaches androidx's `ReportFragment.onActivityCreated` only if that fragment is actually in
  `activity.fragments` via the framework `android.app.FragmentManager` at `onPostCreate` time; if the bundled androidx
  routes its `ReportFragment` through a support FragmentManager instead, the named fallback is ATL's no-op
  `Activity.registerActivityLifecycleCallbacks` (overlay) feeding the API-29+ `LifecycleCallbacks.onActivityPostCreated`
  path — diagnose via log observation only.** Gate (only `src/framework.rs` + `tools/framework-overlay/patch-framework.sh`
  changed): `cargo fmt --all -- --check` / `build --all-targets` (0 warn) / `clippy --all-targets --all-features -D
  warnings` (0 warn) / `build --release` (8,911,112-byte artifact) all 0-warning; `cargo test` **556 unit + 0 main-bin
  + 4 integration (0 SKIP) + 2 doctests = 562 passed, 0 failed** (+1 unit: the new source-order pin). Overlay build
  clean (exit 0; `classes.dex` 18656B, `classes2.dex` 59704B [grew from 43580B by the added Activity+Fragment
  lifecycle smali], `classes3.dex` 2498192B; `classes2.dex` verified via baksmali `list classes` to define EXACTLY
  `Activity` + `Fragment` + `Display` + `View` + `View$OnCapturedPointerListener`). Detail: §6 (2026-06-13 androidx
  lifecycle-ordering / `onPostCreate` create-phase dispatch entry).
- **2026-06-13 — 📺 `android.view.Display.getSupportedRefreshRates()[F` OVERLAY PATCH — OWNER LIVE-VALIDATED
  (EXIT=124 clean): the method resolves, `ActivityNativeMain` now COMPLETES `onCreate` (`createGlAppsFrame`
  succeeds) and ENTERS `onStart`.** Roblox calls `Display.getSupportedRefreshRates()[F` in `Activity.onStart`
  (framerate setup) but ATL's INSTALLED `Display` omits it → `NoSuchMethodError`. This is a **framework-overlay**
  patch (NOT Rust — `RegisterNatives` cannot add a Java *method*), extending the existing step-4b smali pipeline.
  Same drift-proof approach as the `View` pointer-capture patch: **baksmali the AUTHORITATIVE installed `Display`**,
  insert (anchor-guarded after the UNIQUE `getRefreshRate()F` method, exact-count==1 guard like `Build.java`) a
  `getSupportedRefreshRates()[F` returning `float[]{60.0f}` — the `const/high16 0x42700000` IEEE-754 bit pattern of
  60.0f, matching ATL's `getRefreshRate()` which HARDCODES 60.0f, so the reported set is faithful to the installed
  `Display` — then reassemble `View` + `View$OnCapturedPointerListener` + `Display` TOGETHER into `classes2.dex`
  (post-insert `grep -qF` back-check; `cp` of `Display.smali` into the `smali-view` dir). Overlay layout stays
  3-dex: `classes.dex` (javac-patched) + `classes2.dex` (smali `View` + `View$OnCapturedPointerListener` +
  `Display`, defines EXACTLY those 3 classes) + `classes3.dex` (stock); first-dex-wins. Working-tree change is
  confined to `tools/framework-overlay/patch-framework.sh` (+15/-3: the Display anchor guard + perl insert +
  post-insert grep guard + the `cp` into `smali-view`, plus two header-comment updates); no new committed files;
  the vendored smali jars stay in git-ignored `vendor/toolchain/smali/`. **[START-HERE marker moved 2026-06-13 to the
  androidx lifecycle-ordering entry at the TOP of §5 — `getSupportedRefreshRates` resolves and `ActivityNativeMain`
  COMPLETES `onCreate` (`createGlAppsFrame` succeeds) and ADVANCES to `onStart`; the androidx lifecycle-ORDERING bug
  this entry flagged as the NEXT FRONTIER (`IllegalStateException: LifecycleOwner ActivityNativeMain is attempting to
  register while current state is STARTED` — `MediaPickerProtocolV2.onCreate` calls `registerForActivityResult` while
  the `LifecycleRegistry` is already STARTED because `ON_CREATE` was never dispatched during the create phase) is now
  FIXED by that entry's `onPostCreate` → `Fragment.onActivityCreated` create-phase dispatch.]** Gate (no Rust changed — smali-overlay + build-script
  only): overlay build clean (exit 0; `classes.dex` 18656B, `classes2.dex` 43580B [grew from 42288B pointer-capture-
  only by the added Display method], `classes3.dex` 2498192B; `classes2.dex` verified to define EXACTLY `View` +
  `View$OnCapturedPointerListener` + `Display`); `cargo fmt --all -- --check`/`build --all-targets`/`clippy
  --all-targets --all-features -D warnings`/`build --release` (8,910,152-byte artifact) all 0-warning; `cargo test`
  **555 unit + 0 main-bin + 4 integration (0 SKIP) + 2 doctests = 561 passed, 0 failed**. Detail: §6 (2026-06-13
  Display.getSupportedRefreshRates overlay entry).
- **2026-06-13 — 👆 `android.view.View` TOUCH/LONG-CLICK LISTENER NATIVES BOUND (record-the-listener, non-GTK).**
  Owner's live boot of the pointer-capture overlay (commit `8cf570c`, EXIT=124 clean) advanced
  `ActivityNativeMain.onCreate` → `d1` PAST pointer-capture into d1's input setup and hit
  `No implementation found for void android.view.View.nativeSetOnTouchListener(long)` (at `ActivityNativeMain.d1`,
  `View.setOnTouchListener`). `register_view_natives` bound `nativeSetOnClickListener` but not its touch sibling.
  `View.java` confirms `setOnTouchListener` (line 1151) and `setOnLongClickListener` (line 1444) each call a
  `(long widget)` native (`nativeSetOnTouchListener` line 1155 / `nativeSetOnLongClickListener` line 1448, both
  `protected native void` → `(J)V`) then store the listener in a View Java field (`on_touch_listener` 1153 /
  `on_long_click_listener` 1446) — the EXACT `nativeSetOnClickListener` shape. Fix (pure-Rust `RegisterNatives`,
  NOT an overlay change): bound BOTH on `android/view/View` via the existing per-method best-effort registrar,
  both pointing at one shared headless handler `view_set_input_listener` (validates the `view_registry` handle via
  `with_view(widget, |_| ())`, then no-ops — listener lives Java-side, the engine/input path dispatches). It
  deliberately does NOT flip the `clickable` flag (that gates only the click hit-test; touch/long-click are
  distinct and engine-dispatched). Both natives are `void`, so nothing branches on a return — the validated no-op
  is honest. `nativeRequestFocus` left unbound per the owner-validated `<requestFocus/>` headless-consume decision.
  Regression pin: the existing `view_native_names_sigs_and_class_match_view_java` extended with name+`(J)V`
  descriptor asserts for both new natives (tied to View.java 1155/1448 + the live UnsatisfiedLinkError). **[START-HERE
  marker moved 2026-06-13 to the `Display.getSupportedRefreshRates` overlay entry at the TOP of §5 — the owner's live
  boot validated PAST `View.nativeSetOnTouchListener` (these natives bound) all the way through `onCreate`
  (`createGlAppsFrame` succeeds) into `onStart`, where the NEW frontier is the androidx lifecycle-ORDERING bug, not a
  missing native.]** Gate (only `src/framework.rs` changed, +87/-2): `cargo fmt --all -- --check`
  / `build --all-targets` (0 warn) / `clippy --all-targets --all-features -D warnings` (0 warn) / `build --release`
  (8,910,152-byte artifact) all clean; `cargo test` **555 unit + 0 main-bin + 4 integration (0 SKIP) + 2 doctests
  = 561 passed, 0 failed**. Detail: §6 (2026-06-13 View touch/long-click listener natives entry).
- **2026-06-13 — 🖱️ `android.view.View` POINTER-CAPTURE OVERLAY PATCH — OWNER LIVE-VALIDATED (EXIT=124 clean):
  `setOnCapturedPointerListener` resolves, boot advanced PAST pointer-capture.** Roblox's `ActivityNativeMain.d1`
  references `android.view.View$OnCapturedPointerListener` and calls `View.setOnCapturedPointerListener(listener)` —
  AOSP's API-26 pointer-capture API that ATL's INSTALLED `View` omits (without it the boot aborts with
  `NoClassDefFoundError`/`NoSuchMethodError`). This is a **framework-overlay** patch (NOT Rust — `RegisterNatives`
  cannot add a Java *method* or a nested *type*). Adding the setter needs the whole `View` class, and the repo's
  vendored `View.java` has **DRIFTED** from the installed jar (e.g. `setBackgroundColor(int)` is `native` in vendored
  but plain-Java installed), so recompiling vendored re-breaks it. Fix (`patch-framework.sh` step 4b): **baksmali the
  AUTHORITATIVE installed `View`**, insert ONLY the backing field (`mCapturedPointerListener`) + the setter
  (`setOnCapturedPointerListener` — a pure-Java field record, headless: Eclipse's engine owns pointer input) + the
  nested interface's MemberClasses entry, each behind an exact-count anchor guard (mirrors the `Build.java` anchor
  guard; field+setter inserts are back-checked by `grep -qF || fail`), then reassemble (smali) ONLY `View` + the
  committed `View$OnCapturedPointerListener.smali` nested interface. Overlay layout is now **3-dex**: `classes.dex`
  (javac-patched) + `classes2.dex` (smali `View` + `View$OnCapturedPointerListener`, defines EXACTLY those 2 classes) +
  `classes3.dex` (stock); first-dex-wins resolves `View` and the nested interface from `classes2.dex`. The smali
  toolchain (baksmali/smali 2.5.2) is vendored at `vendor/toolchain/smali/` (git-ignored local toolchain, exactly like
  the JDK; env-overridable `BAKSMALI_JAR`/`SMALI_JAR`). **[START-HERE marker moved 2026-06-13 to the View touch/long-click
  listener entry at the TOP of §5 — the owner's live boot validated PAST pointer-capture to the
  `View.nativeSetOnTouchListener` gap, which is now closed by that entry's `RegisterNatives` binding.]** (OWNER live
  validation on the dev-host MAIN LOOP, DONE for this patch — `tools/framework-overlay/patch-framework.sh` reproduces the 3-dex
  overlay (`classes2.dex` defines EXACTLY `View` + `View$OnCapturedPointerListener`) and the live boot is EXIT=124
  clean: `setOnCapturedPointerListener` resolves, `setBackgroundColor` is intact, and `ActivityNativeMain.onCreate`
  advanced PAST pointer-capture). NEXT GAP = the NEW frontier the live boot revealed: `View.nativeSetOnTouchListener` —
  a `View` native sibling of `nativeSetOnClickListener` (which Eclipse already binds in `register_view_natives`); bind
  the touch sibling there as a quick Rust `RegisterNatives` binding (record-the-listener, like the other listener
  natives — NOT an overlay change). REMINDER: the overlay is a CACHE artifact — if `~/.cache/eclipse` was wiped and the
  boot errors `Android framework not found`, rebuild it FIRST with `tools/framework-overlay/patch-framework.sh`
  (`export ECLIPSE_ANDROID_FRAMEWORK_DIR=$HOME/.cache/eclipse/framework-patched`), and `vendor/toolchain/smali/` must
  hold the smali 2.5.2 jars (git-ignored, like the JDK — see `vendor/toolchain/smali/SOURCE.txt`).** This pointer-capture
  patch is LIVE-PROVEN (boot advanced past pointer-capture to the `nativeSetOnTouchListener` gap). Gate (no Rust changed
  — Java/smali-overlay + build-script only): overlay build clean (exit 0; `classes.dex` 18656B, `classes2.dex` 42288B,
  `classes3.dex` 2498192B; `classes2.dex` verified to define EXACTLY `View` + `View$OnCapturedPointerListener`);
  `cargo fmt --all -- --check`/`build --all-targets`/`clippy -D warnings`/`build --release` (8,907,880-byte artifact)
  all 0-warning; `cargo test` **555 unit + 0 main-bin + 4 integration (0 SKIP) + 2 doctests = 561 passed, 0 failed**.
  Detail: §6 (2026-06-13 View pointer-capture overlay entry).
- **2026-06-13 — 🩹 LayoutInflater `<requestFocus/>` OVERLAY PATCH — LIVE-PROVEN: inflation advanced PAST `<requestFocus/>`.**
  ATL's vendored `LayoutInflater.rInflate` stubbed the standard AOSP `<requestFocus/>` layout tag with `throw new
  Exception("<requestFocus /> not supported atm")`, which aborted `ActivityNativeMain.onCreate`'s content-view inflation.
  Fix is a **framework-overlay** Java patch (NOT Rust — no `RegisterNatives` can add a Java method body): a committed
  patched copy `tools/framework-overlay/src/android/view/LayoutInflater.java`, byte-identical to the vendored original
  EXCEPT the `<requestFocus/>` branch now calls a new private `parseRequestFocus(parser, parent)` → `consumeChildElements(parser)`
  (the canonical AOSP frameworks/base parse-and-consume idiom — a genuine depth-guarded consume of the tag so inflation
  continues, NOT error suppression). It deliberately OMITS `View.requestFocus()`: Eclipse is headless and binds no
  `nativeRequestFocus` (it would `UnsatisfiedLinkError`); the engine owns input focus, so consuming the tag is the
  load-bearing behavior. Shadows the stock class via the overlay's `classes.dex` (multidex first-dex-wins). New
  compile-only stubs under `tools/framework-overlay/stubs/` (`android/view/{View,ViewGroup,ContextThemeWrapper}`,
  `android/content/res/{TypedArray,XmlResourceParser,Resources}`, `android/util/{AttributeSet,Slog,Xml}`,
  `com/android/internal/R`, `org/xmlpull/v1/{XmlPullParser,XmlPullParserException,XmlPullParserFactory}`) + an extended
  `android/content/Context.java` stub (concrete `getResources`/`obtainStyledAttributes`/`getSystemService`) let `javac`
  compile the patched source WITHOUT ATL's full source tree; the staging glob (`android/view/LayoutInflater*.class`)
  dexes ONLY the 3 LayoutInflater classes, so NO stub ever reaches the dex (verified: `classes.dex` defines exactly 17
  classes, 0 stub classes). `patch-framework.sh` wires LayoutInflater into the javac list + staging glob and adds a
  build-time regression guard (lines 63-70) that fails the build if the `<requestFocus/>` fix is ever reverted (mirrors
  the Build.java anchor guard). Overlay build clean (exit 0; `classes.dex` 10508 → **18656** bytes; `classes2.dex`
  2498192 bytes). **[START-HERE marker moved 2026-06-13 to the View pointer-capture entry at the TOP of §5 — the owner's
  live boot validated PAST `<requestFocus/>` to the `NoClassDefFoundError android.view.View$OnCapturedPointerListener`
  gap, which is now resolved by that entry's overlay patch.]** (this entry's now-superseded NEXT-GAP plan said to "add
  the nested type `View$OnCapturedPointerListener` to the overlay's `classes.dex` WITHOUT shadowing the large `View`
  class (ship the nested interface alone, do NOT re-dex a whole patched `View`)" — the shipped fix deliberately did the
  OPPOSITE and re-dexed a whole baksmali-patched installed `View`, because Roblox ALSO CALLS `setOnCapturedPointerListener`,
  a *method* the nested interface alone cannot provide; see the §5 top entry + the 2026-06-13 §6 View pointer-capture
  entry for why the whole-View smali approach was required. REMINDER: the overlay is a CACHE artifact — if
  `~/.cache/eclipse` was wiped and the boot errors `Android framework not found`, rebuild it with
  `tools/framework-overlay/patch-framework.sh` FIRST
  (`export ECLIPSE_ANDROID_FRAMEWORK_DIR=$HOME/.cache/eclipse/framework-patched`).) This `<requestFocus/>` patch is
  LIVE-PROVEN (inflation advanced past the tag). Gate (no Rust changed — Java-overlay + build-script only): overlay
  build clean; `cargo fmt --all -- --check`/`build --all-targets`/`clippy -D warnings`/`build --release` (8,907,880-byte
  artifact) all 0-warning; `cargo test` **555 unit + 0 main-bin + 4 integration (0 SKIP) + 2 doctests = 561 passed, 0
  failed**. Detail: §6 (2026-06-13 LayoutInflater `<requestFocus/>` overlay entry).
- **2026-06-13 — ⌨️ `EditText` LISTENER NATIVES BOUND (record-the-listener) + the 58a50f6 atomic-RegisterNatives
  ABORT CLASS root-cause-fixed across the View/widget per-class registrations.** Owner live boot of `16db9eb` (clean,
  pure log observation) PROVED the 58a50f6 regression fix landed: `register_view_natives` registers cleanly, the boot
  reaches `ActivityNativeMain.onCreate` past `ProgressBar.native_setIndeterminate`, and the NEXT unbound native — while
  `LayoutInflater` inflates the content view via `RbxKeyboard` → `AppCompatEditText.<init>` → `EditText.addTextChangedListener`
  — is `No implementation found for void android.widget.EditText.native_addTextChangedListener(long, android.text.TextWatcher)`.
  **(A)** Bound the three `EditText` listener natives ON `android/widget/EditText` (all first-party-verified `protected
  native`: `native_addTextChangedListener` `EditText.java:26`, `native_removeTextChangedListener` `:27`,
  `native_setOnEditorActionListener` `:28`) with HONEST record-the-listener semantics — Eclipse's vendored
  `addTextChangedListener`/`setOnEditorActionListener` (`EditText.java:52`/`:57`) pass the listener straight to the
  native with NO Java field, so the native MUST retain it or it is collected on return: `add` retains a `new_global_ref`
  TextWatcher on the `view_registry` peer (`ViewState.text_watchers: Vec<Global>`), `remove` drops the `IsSameObject`-
  matching retained watcher, `setOnEditorActionListener` retains/replaces (null clears) the editor-action listener
  (`ViewState.editor_action_listener: Option<Global>`); each `Global` releases its ref on `Drop` (slot `free`d /
  replaced). Null listener ignored; stale/fabricated handle is a typed `Err` (logged, never UB). The editor-action sig
  is the nested `(JLandroid/widget/TextView$OnEditorActionListener;)V` (`OnEditorActionListener` is `public static
  interface` in `TextView.java:287`, reached via EditText's TextView supertype). **Actually DISPATCHING
  `TextWatcher.onTextChanged`/`onEditorAction` on real input is a FUTURE input-integration step — no input occurs during
  boot, so retaining the listener is the complete correct behavior now.** **(B)** ROOT-CAUSE-CLASS FIX: the 58a50f6 boot
  break was JNI `RegisterNatives` aborting an ENTIRE per-class `NativeMethod` array atomically when one entry
  (`setBackgroundColor(I)V`) is plain Java in the shipped dex — taking `native_constructor`/`native_destructor`/
  `native_get_window` down with it. NEW `register_class_natives_best_effort(env, class, &[NativeBinding])` (modeled on
  the existing `register_asset_stream_natives` per-native precedent, but at LOUD `tracing::warn!` not debug) binds each
  method via a single-element `RegisterNatives` slice; a method the shipped dex does not declare native is skipped
  (exception cleared) with a per-method WARN naming class+method+sig, degrading the fatal whole-class abort into a
  deferred call-time `UnsatisfiedLinkError` on ONLY that method — the discovery signal the project already relies on.
  `find_class` failure still propagates via `?` (a genuine class-load failure is not masked). Converted EXACTLY the
  View/widget per-class registrars affected by this class of bug: `register_view_natives`, `register_view_group_natives`,
  `register_text_view_natives`, `register_image_view_natives`, `register_image_button_natives`,
  `register_surface_view_natives`, `register_view_subclass_constructor_natives`, `register_widget_property_setter_natives`
  (all 8 widget classes); unrelated registrars (Paint/Canvas/Window/Activity/asset-stream/ViewTreeObserver) left atomic
  per "do not rewrite unrelated registration code." Regression guard: NEW
  `register_class_natives_best_effort_skips_unbindable_method_and_continues` drives the pure `fold_best_effort` core
  (JVM-free, since ART can't run in-harness) with a 3-entry set whose middle entry fails — asserts all 3 are visited in
  order (no short-circuit) and `bound == 2`, the smallest check that would have caught the 58a50f6 atomic abort; the
  existing `widget_property_setter_names_sigs_and_classes_match_overlay` pin extended with the three EditText listener
  name/sig pins so a transcription drift re-introducing the boot-block fails CI. **(SUPERSEDED as the START-HERE marker
  by the 2026-06-13 LayoutInflater `<requestFocus/>` entry above — the owner live boot validated past EditText to the
  `<requestFocus/>` gap, then past that too; left here for the lineage.)** With the three EditText listener natives bound, expect
  `RbxKeyboard`/`AppCompatEditText` construction to no longer trip `UnsatisfiedLinkError` on `native_addTextChangedListener`
  and `ActivityNativeMain`'s content-view inflation to proceed PAST EditText/RbxKeyboard; confirm the View/widget
  registrations log the normal `(best-effort)` info lines and NO per-method WARN (a WARN names a genuinely-non-native
  shipped method to investigate next). Capture the NEXT unbound native one at a time (pure log observation, no binary
  inspection; expected: whether `native_removeTextChangedListener`/`native_setOnEditorActionListener` are also reached
  on this ctor path, then a return-driving getter like `EditText.native_getText`/`SeekBar.native_getProgress`, an
  `isChecked()`/`setChecked()` pair, another listener registration, or the next class on the inflate→attach→surface
  path). The standing next FRONTIER remains the SCOPED surface-to-engine render wiring from `2194f02`'s §6 plan: wire
  `EngineNativeWindow::new` + `register_wsi_window`/`set_engine_window_geometry` into `graphics.rs::run_windowed` +
  present-loop ownership handoff + JNI-dispatch `SurfaceView.surfaceCreated()`/`surfaceChanged()` once the WSI surface
  is live — designed AFTER the live boot reveals the post-layout call chain (libroblox-internal RUNTIME behavior, NOT
  first-party-determinable, NOT to be obtained by reverse-engineering libroblox.so).** Gate: **555 unit + 0 main-bin + 4
  integration (live milestone subprocesses, 0 SKIP, exact success markers) + 2 doctests = 561 passed, 0 failed** (+3
  unit: two `view_registry` listener-retention tests + one `fold_best_effort` skip-and-continue test), fmt/clippy
  `-D warnings`/release (8,907,880-byte artifact) all 0-warning. Detail: §6 (2026-06-13 EditText-listener /
  best-effort-registration entry).
- **2026-06-13 — 🩹 58a50f6 REGRESSION FIXED: the two speculative base-`android.view.View` setters that 58a50f6 added
  to `register_view_natives` are REMOVED — `View.setBackgroundColor(I)V` and `View.native_keep_screen_on(JZ)V`.** Root
  cause (PROVEN by the owner's live boot of 58a50f6): `RegisterNatives` is ATOMIC over its whole `NativeMethod` array
  and rejected the very first new entry — ART logged `jni_internal.cc: Failed to register non-native method
  android.view.View.setBackgroundColor(I)V as native` → `No such method: no native method "Landroid/view/View;.
  setBackgroundColor(I)V"`. In the SHIPPED framework `setBackgroundColor(int)` is a PLAIN Java method (the vendored
  `View.java:1284` `public native void setBackgroundColor(int)` is demonstrably out of sync with the installed dex), so
  the atomic registration FAILED ENTIRELY and took the lifecycle-critical View natives
  (`native_constructor`/`native_destructor`/`native_get_window`) down with it — the lifecycle aborted and the process
  faulted during teardown. `native_keep_screen_on` was never reached (RegisterNatives stopped at the first bad entry),
  so its shipped native-ness is unverified and the out-of-sync vendored source cannot prove it either; both are left
  OUT until a live boot proves the shipped framework declares them native. Neither was needed for progress — the boot
  reached `ActivityNativeMain.onCreate` WITHOUT them before 58a50f6. FIX (surgical, src/framework.rs only, 25 ins / 126
  del): dropped both consts (name+sig), both fn bodies (`view_set_background_color_no_handle`, `view_keep_screen_on`),
  both `NativeMethod` array entries, the `info!` log mentions, and the two pin-test assertions; added a dated guard
  comment in the consts cluster, in `register_view_natives`, and in the pin test so neither is reintroduced without a
  live-boot proof. The kept pre-existing `native_setBackgroundColor(JI)V` `(JI)V` binding and ALL 58a50f6 widget-class
  setters/constructors are intact and untouched. Regression guard: the existing
  `widget_property_setter_names_sigs_and_classes_match_overlay` pin test now carries the dated NOTE that these two are
  intentionally unbound; the dropped assertions + the in-code dated comments are the smallest guard tied to the proven
  root cause (a future reintroduction must again pass an atomic RegisterNatives the shipped dex rejects). **[START-HERE
  marker moved 2026-06-13 to the EditText-listener / best-effort-registration entry at the TOP of §5 — the owner's live
  boot of `16db9eb` CONFIRMED this fix landed (`register_view_natives` registers cleanly, the boot reaches
  `ActivityNativeMain.onCreate` past `ProgressBar.native_setIndeterminate`), and the next unbound native it surfaced —
  `EditText.native_addTextChangedListener` — is now bound; and the atomic-RegisterNatives mechanism this entry root-caused
  is now hardened class-wide via per-method best-effort registration.]** (the plan was = OWNER live validation on the
  dev-host MAIN LOOP: `./target/release/eclipse run <APK>` with
  `ECLIPSE_ANDROID_FRAMEWORK_DIR=$HOME/.cache/eclipse/framework-patched`): with the two bad entries gone, expect
  `register_view_natives` to register CLEANLY again — NO `NoSuchMethod`/`Failed to register non-native method` on
  `android.view.View`, the View natives (`native_constructor`/`native_destructor`/`native_get_window`) bound — and the
  boot to reach `ActivityNativeMain.onCreate` with the widget property setters working (`ProgressBar.native_setIndeterminate`
  bound, no `UnsatisfiedLinkError`). Then capture the NEXT unbound native one at a time (pure log observation, no binary
  inspection; expected from the leftUnbound set: a return-driving getter like `SeekBar.native_getProgress`/
  `EditText.native_getText`, an `isChecked()`/`setChecked()` pair, a listener registration, or the next class on the
  inflate→attach→surface path). The standing next FRONTIER is the SCOPED surface-to-engine render wiring from
  `2194f02`'s §6 plan: wire `EngineNativeWindow::new` + `register_wsi_window`/`set_engine_window_geometry` into
  `graphics.rs::run_windowed` + present-loop ownership handoff + JNI-dispatch `SurfaceView.surfaceCreated()`/
  `surfaceChanged()` once the WSI surface is live — designed AFTER the live boot reveals the post-layout call chain
  (libroblox-internal RUNTIME behavior, NOT first-party-determinable, NOT to be obtained by reverse-engineering
  libroblox.so).** Gate: **552 unit + 4 integration (live milestone subprocesses, 0 SKIP) + 2 doctests = 558 passed,
  0 failed**, fmt/clippy `-D warnings`/release (8,897,128-byte artifact) all 0-warning. Detail: §6 (2026-06-13
  58a50f6-regression entry).
- **2026-06-13 — 🎚️ INFLATABLE `android.widget.*` PROPERTY SETTERS BOUND (one pass) — the per-widget setter
  `UnsatisfiedLinkError` churn after construction is closed; `ProgressBar.native_setIndeterminate` (the trigger) and
  the rest of the widget-set property setters resolve.** Following the View-subclass `native_constructor` batch
  (entry below), the layout now constructs the widgets and the NEXT trip is each widget's PROPERTY SETTERS. Bound in
  one pass on each setter's OWN declaring class (ART resolves natives per declaring class), with honest no-GTK
  record-or-no-op semantics: TEXT setters RECORD (renderer-consumed) — `Button.native_setText`/`EditText.native_setText`/
  `CheckBox.native_setText` (`(JLjava/lang/String;)V`) and `RadioButton.setText(Ljava/lang/CharSequence;)V` (records
  `this.widget` text via `CharSequence.toString()`); ScrollView REUSES the class-agnostic
  `view_group_native_add_view`/`view_group_native_remove_view` (records real tree edges); validated-handle NO-OPs
  where NO bound native getter reads the value back and the renderer draws no such chrome (so the Java caller depends
  on no native effect — mirrors the existing `ImageView.native_setScaleType`/`View.nativeSetFullscreen`/
  `native_setBackgroundDrawable` no-ops): `ProgressBar.native_setIndeterminate(Z)V`, `ProgressBar.native_setProgress(JF)V`,
  `SeekBar.native_setProgress(JF)V`, `SeekBar.native_setMax(JI)V`, `Spinner.native_setAdapter(JLandroid/widget/SpinnerAdapter;)V`,
  `Button.native_setCompoundDrawables(JJ)V`. Also added two base-`android.view.View` setters into
  `register_view_natives`: `setBackgroundColor(I)V` (RECORDS ARGB via `view_registry::set_background_color`,
  renderer-consumed; verified `View.java:1284`) and the STATIC `native_keep_screen_on(JZ)V` (validated no-op — no host
  screen-wake, no native getter; `View.java:1982`). PROMOTED the 8 widget class-name literals to `pub const`
  (`BUTTON_CLASS`..`SCROLL_VIEW_CLASS`) as one source of truth reused by both `VIEW_SUBCLASS_CONSTRUCTOR_CLASSES` and
  the new `register_widget_property_setter_natives`, wired into `drive_lifecycle` right after
  `register_view_subclass_constructor_natives` (before step 4 / LayoutInflater). DELIBERATELY LEFT UNBOUND + flagged
  (return value drives Java control flow → never no-op'd per policy; surfaces loudly at boot as the next discovery
  signal): the return-driving GETTERS `SeekBar.native_getProgress(J)I`, `EditText.native_getText(J)Ljava/lang/String;`,
  `Button.getText()Ljava/lang/CharSequence;`; the COUPLED stateful `CheckBox.isChecked()Z`/`setChecked(Z)V` and
  `RadioButton.isChecked()Z`/`setChecked(Z)V` pairs left FULLY unbound (no consumed `view_registry` field backs
  `checked`; no-op'ing `setChecked` while `isChecked` reads it would be a silent wrong answer — bind both together with
  a real `checked` field when evidence requires); and all listener registrations (RBX-bytecode/owner-run-data-gated).
  Regression guard: NEW `widget_property_setter_names_sigs_and_classes_match_overlay` (mirrors
  `view_subclass_constructor_classes_are_slashed_internal_names`) pins the exact slashed class internal names +
  method name/JNI descriptors for every newly bound setter (incl. the two base-View setters), so a dropped class or a
  transcribed-wrong name/sig — which re-introduces the one-per-boot `UnsatisfiedLinkError` — fails CI. **[START-HERE
  marker moved 2026-06-13 to the regression-fix entry at the TOP of §5 — note (a) below was PROVEN by the owner's live
  boot of 58a50f6: the two NEW base-View setters DID break `register_view_natives` (atomic RegisterNatives rejected the
  shipped `setBackgroundColor(I)V` as non-native), so both were removed; note (b)'s pre-existing `(JI)V` binding
  remains untouched and is still the standing separate-cleanup flag.]** (the plan was = OWNER live validation on the
  dev-host MAIN LOOP: `./target/release/eclipse run <APK>` with
  `ECLIPSE_ANDROID_FRAMEWORK_DIR=$HOME/.cache/eclipse/framework-patched`): confirm `ProgressBar.native_setIndeterminate`
  no longer trips `UnsatisfiedLinkError` and `ActivityNativeMain`'s `LayoutInflater` builds the FULL content view /
  `onCreate` proceeds further toward RESUMED, then capture the NEXT unbound native one at a time (expected from the
  leftUnbound set: a return-driving getter like `SeekBar.native_getProgress`/`EditText.native_getText`, an
  `isChecked()`/`setChecked()` pair, a listener registration, or the next class on the inflate→attach→surface path —
  pure log observation, no binary inspection). The standing next FRONTIER is the SCOPED surface-to-engine render
  wiring from `2194f02`'s §6 plan: wire `EngineNativeWindow::new` + `register_wsi_window`/`set_engine_window_geometry`
  into `graphics.rs::run_windowed` + present-loop ownership handoff + JNI-dispatch `SurfaceView.surfaceCreated()`/
  `surfaceChanged()` once the WSI surface is live — designed AFTER the live boot reveals the post-layout call chain
  (libroblox-internal RUNTIME behavior, NOT first-party-determinable, NOT to be obtained by reverse-engineering
  libroblox.so). The NDK/EGL half is de-risked (`gl_test_anw_binds_real_wsi_handle` green). Two OWNER-confirm notes
  from the read: (a) the two NEW base-View setters join the EXISTING all-or-nothing `register_view_natives`
  RegisterNatives array — confirm it still registers cleanly (no `NoSuchMethodError` on `android.view.View`) under the
  installed stock dex; (b) the pre-existing `native_setBackgroundColor(JI)V` binding targets a method the current
  vendored `View.java` no longer declares (only the `(I)V` form at line 1284 exists) — pre-existing, NOT regressed
  here, flagged for a separate cleanup if a boot log shows a `NoSuchMethodError`/`No implementation` on it.)
  Gate: **552 unit + 4 integration (live milestone subprocesses, 0 SKIP) + 2 doctests = 558 passed, 0 failed**
  (+1 unit: the pin test), fmt/clippy `-D warnings`/release all 0-warning. Detail: §6 (2026-06-13 widget
  property-setter entry). [SUPERSEDED in part 2026-06-13 by the regression-fix entry above: the two base-View setters
  this entry added were removed; the gate count is unchanged because the fix dropped assertions inside the existing pin
  test, not a whole test.]
- **2026-06-13 — 🧩 INFLATABLE `android.widget.*` VIEW-SUBCLASS `native_constructor` BATCH BOUND (8 classes, one pass)
  — the one-class-per-boot `UnsatisfiedLinkError` churn at `LayoutInflater.inflate` is closed for the widget set.**
  Owner live validation of the SurfaceView bind (`/tmp/eclipse-surfaceview-validate.log`, EXIT=124 clean) proved
  `RBXSurfaceView` constructs and `ActivityNativeMain`'s `LayoutInflater` proceeds further into the content view,
  then tripped the NEXT View subclass in the same layout: `No implementation found for long
  android.widget.ProgressBar.native_constructor(android.content.Context, android.util.AttributeSet)` at
  `LayoutInflater.inflate` → `ActivityNativeMain.onCreate`. ROOT CAUSE (first-party, same mechanism as SurfaceView):
  ART resolves natives PER DECLARING CLASS, and `android.view.View`'s `native_constructor(Context, AttributeSet)J`
  (`View.java:1166`) is RE-declared VERBATIM by every concrete inflatable `android.widget.*` subclass the vendored
  ATL ships, so the `register_view_natives` base binding does NOT satisfy them — each must bind the shared
  class-agnostic `view_native_constructor` on its OWN class before step 4. FIX (this commit): instead of one fn per
  class, a NEW `const VIEW_SUBCLASS_CONSTRUCTOR_CLASSES` (the 8 slashed names) + a single
  `register_view_subclass_constructor_natives(env)` helper loops `find_class → NativeMethod::from_raw_parts(
  VIEW_NATIVE_CONSTRUCTOR_NAME/_SIG, view_native_constructor) → register_native_methods` (the exact
  `register_surface_view_natives` recipe — minimal because all 8 bind the IDENTICAL shared body, which records the
  receiver's concrete class via `view_class_name` + `view_registry::allocate`, handle ≥ 1), wired into
  `drive_lifecycle` right after `register_surface_view_natives` (before step 4 / LayoutInflater). THE 8 BOUND
  (each verified first-party to declare the 2-arg `native_constructor` DIRECTLY against
  `vendor/atl/src/api-impl/android/widget/`): `Button` (`Button.java:39`), `EditText` (`:24`), `ProgressBar` (`:49`),
  `CheckBox` (`:19`), `RadioButton` (`:17`), `SeekBar` (`:17`), `Spinner` (`:26`), `ScrollView` (`:18`). EXCLUDED
  (first-party-verified): `CompoundButton` is `public abstract class` (`CompoundButton.java:9`) — not
  LayoutInflater-instantiable; its concrete leaves CheckBox/RadioButton re-declare the native and ARE bound.
  `PopupWindow` (`PopupWindow.java:177`) declares ZERO-ARG `native_constructor()J` and is NOT a View — wrong arity +
  wrong type for the shared body. The abstract parents `AbsSeekBar`/`AbsSpinner`/`AdapterView`/`ViewGroup` do NOT
  declare `native_constructor` (the layout containers inherit View's, already bound) — no binding needed. ONLY
  `native_constructor` is bound per class; each class's extra natives (e.g. `ProgressBar.native_setProgress`, SeekBar/
  Spinner extras) stay UNBOUND on purpose so the next real layout/draw trip surfaces them one at a time (the
  deliberate loud discovery signal, exactly as `register_surface_view_natives` omits `native_createSnapshot`/
  `native_postSnapshot`). `View.native_destructor(long)` (`View.java:1168`) is declared on View and NOT re-declared by
  any widget, so the existing `register_view_natives` binding covers destruction for all 8 by inheritance. RECORDED,
  deliberately NOT bound (out of the `android.widget.*` scope, not yet surfaced by evidence): `android.webkit.WebView`
  (`vendor/atl/src/api-impl/android/webkit/WebView.java`) is the ONE remaining concrete class that re-declares the
  exact `(Context, AttributeSet)J` `native_constructor` and is currently unbound anywhere in `framework.rs` — if a
  future layout inflates a WebView it will trip `No implementation found for long
  android.webkit.WebView.native_constructor(...)`; it shares the exact signature so it can join
  `VIEW_SUBCLASS_CONSTRUCTOR_CLASSES` (+ the pin test) when/if it surfaces. AUDIT (full overlay grep,
  `native long native_constructor(Context` across `widget`/`view`/`webkit`): exactly 15 declarers = 5 already bound
  (View, SurfaceView, TextView, ImageView, ImageButton) + 8 newly bound + 1 abstract-excluded (CompoundButton) + 1
  recorded-unbound (WebView); PopupWindow's zero-arg form is correctly outside this set. Regression guard:
  `view_subclass_constructor_classes_are_slashed_internal_names` (mirrors `surface_view_class_is_slashed_internal_name`)
  pins the EXACT ordered 8-name set so a dropped/reordered class — which re-introduces the one-per-boot
  `UnsatisfiedLinkError` — fails CI, and asserts CompoundButton/PopupWindow stay OUT. **[START-HERE marker moved
  2026-06-13 to the widget property-setter entry above — the per-widget extra natives this entry predicted as the next
  trip surfaced: `ProgressBar.native_setIndeterminate` opened the discovery, and the whole inflatable widget set's
  property setters are now bound in one pass]** (the plan was = OWNER live validation on the dev-host MAIN LOOP:
  `./target/release/eclipse run <APK>` with `ECLIPSE_ANDROID_FRAMEWORK_DIR=$HOME/.cache/eclipse/framework-patched`: the
  empirical bind→boot→next-gap loop — expect `ActivityNativeMain`'s `LayoutInflater` to now build its FULL content view
  WITHOUT tripping per-widget `native_constructor` (ProgressBar + the rest of the batch construct; a `view_registry`
  peer is allocated per inflated widget recording its concrete class), then surface the NEXT unbound native one at a
  time — either a per-widget extra native (e.g. `ProgressBar.native_setProgress`), the recorded `WebView.native_constructor`
  if a WebView is in the layout, or the next class on the inflate→attach→surface path. Capture that exact next-native ART
  stack (pure log observation, no binary inspection). The next FRONTIER once the layout completes is the SCOPED
  surface-to-engine wiring from `2194f02`'s §6 plan: wire `EngineNativeWindow::new` + `register_wsi_window`/
  `set_engine_window_geometry` into `graphics.rs::run_windowed` (today ZERO there, so production
  `ANativeWindow_fromSurface` always hits the geometry-only fallback), resolve present-loop ownership handoff, and
  JNI-dispatch `SurfaceView.surfaceCreated()`/`surfaceChanged()` once the WSI surface is live — designed AFTER the
  live boot reveals the post-layout call chain (in particular whether/when libroblox's reflection-registered
  `AndroidGLView` SurfaceHolder.Callback fires and on which thread; all libroblox-internal RUNTIME behavior, NOT
  first-party-determinable, NOT to be obtained by reverse-engineering libroblox.so — capture that next-native/
  AndroidGLView trace from the boot log). The NDK/EGL half is de-risked (`gl_test_anw_binds_real_wsi_handle` green).)**
  Gate: **551 unit + 4 integration (live milestone subprocesses, 0 SKIP) + 2 doctests = 557 passed, 0 failed**
  (+1 unit: the pin test), fmt/clippy `-D warnings`/release all 0-warning. Detail: §6 (2026-06-13 View-subclass
  constructor-batch entry).
- **2026-06-13 — 🎬 `SurfaceView.native_constructor` + `View.native_destructor` BOUND — Roblox's GL render
  surface (`com.roblox.client.RBXSurfaceView`) now CONSTRUCTS; LayoutInflater can complete ActivityNativeMain's
  content view.** Owner live validation of `native_get_window` (`/tmp/eclipse-getwindow-validate.log`, EXIT=124
  clean) proved the boot got PAST `native_get_window` and into `ActivityNativeMain.onCreate` → `d1()` →
  `LayoutInflater.inflate`, which constructs `RBXSurfaceView` (extends `android.view.SurfaceView` — THE engine's GL
  render surface) and died on two coupled unbound natives, BOTH FIXED THIS COMMIT: (1) `No implementation found for
  long android.view.SurfaceView.native_constructor(Context, AttributeSet)`; then (2) on the FinalizerDaemon cleaning
  up the half-built View, `No implementation found for void android.view.View.native_destructor(long)` (after which
  Roblox's watchdog logged `RBXCRASH-HangDetected` — the main-thread onCreate failure tripped its hang detector).
  ROOT CAUSE (first-party-confirmed): SurfaceView `@Override`-re-declares `native_constructor` (vendored
  `SurfaceView.java:40`, `(Context, AttributeSet)J`) and ART resolves natives PER DECLARING CLASS, so the existing
  View-class binding did NOT satisfy it; and `View.native_destructor(long)` (`View.java:1168`, called from
  `View.finalize` `View.java:1679`) was genuinely never bound on `android/view/View` (zero src references). FIX
  (both root causes, both land together — binding only one leaves the other's `UnsatisfiedLinkError`): a NEW
  `register_surface_view_natives` binds the EXISTING class-agnostic `view_native_constructor` on a NEW
  `SURFACE_VIEW_CLASS = "android/view/SurfaceView"` (records the receiver's actual class
  `com.roblox.client.RBXSurfaceView` in `view_registry`, allocates a real generational slab handle ≥ 1 so
  `View.widget` is non-zero and SurfaceView's `mSurface.widget`/`Surface.isValid()` hold), wired into
  `drive_lifecycle` right after `register_image_button_natives` (before step 4, so it is bound before
  `LayoutInflater` runs); and a NEW `view_native_destructor` (consts `VIEW_NATIVE_DESTRUCTOR_NAME`/`_SIG`) added to
  the EXISTING `register_view_natives` method table on `android/view/View` (declared on View, NOT overridden, so one
  binding covers every View subclass incl. SurfaceView by inheritance) — frees the peer via the bounds+generation-
  checked `view_registry::free`, and MUST tolerate `widget == 0`/stale/fabricated gracefully (the failed-construct
  path leaves `View.widget` at the `long` default 0, then the finalizer calls `native_destructor(0)`): it logs +
  ignores any `Err`, NEVER throwing on the FinalizerDaemon thread. Deliberately NOT bound (discovery signal stays
  loud, per policy): `SurfaceView.native_createSnapshot`/`native_postSnapshot` (off the EGL render path — the
  `lockCanvas` software-blit path Roblox doesn't use), `View.native_measure`/`native_layout`/`native_queueAllocate`
  (whether they fire is RBXSurfaceView-bytecode/owner-run-data-gated). SURFACE WIRING DEFERRED (not a punt — the
  NDK/EGL HALF is already proven): the SurfaceHolder surface → host `ANativeWindow` render-integration is scoped to
  the NEXT workflow; `eclipse_anativewindow_fromsurface` already IGNORES its jobject arg and returns the real
  process-global WSI window (`ndk_registry::current_wsi_window`), and `gl_test_anw_binds_real_wsi_handle` is green —
  the remaining work is `graphics.rs::run_windowed` WSI publish (`EngineNativeWindow::new` +
  `register_wsi_window`/`set_engine_window_geometry`, today ZERO in graphics.rs/main.rs so production
  `ANativeWindow_fromSurface` always hits the geometry-only fallback) + present-loop ownership handoff + JNI-
  dispatching SurfaceView's private `surfaceCreated()`/`surfaceChanged()` once the WSI surface is live (no Java
  caller in vendored ATL; Eclipse must drive them — precedent: `View.layoutInternal` JNI-called from native). **[START-HERE
  marker moved 2026-06-13 to the View-subclass `native_constructor` batch entry above — the per-class
  one-at-a-time SurfaceView bind exposed `ProgressBar` next, and the whole `android.widget.*` inflatable
  set is now bound in one pass.]** (the plan was = OWNER live validation on the dev-host MAIN LOOP: `./target/release/eclipse run <APK>`
  with `ECLIPSE_ANDROID_FRAMEWORK_DIR=$HOME/.cache/eclipse/framework-patched`): the empirical bind→boot→next-gap
  loop — expect `RBXSurfaceView` to now CONSTRUCT (NO `UnsatisfiedLinkError` on `SurfaceView.native_constructor`; a
  `view_registry` peer naming `com.roblox.client.RBXSurfaceView` is allocated and LayoutInflater completes the
  content view), and NO finalizer-thread `UnsatisfiedLinkError` on `View.native_destructor` (the failed-construct
  finalizer path is gone because the constructor now succeeds; confirm from the log that `native_destructor` is no
  longer reached with `widget == 0`, and that real RBXSurfaceView destructor calls pass a valid handle `free()`
  accepts — pure log observation, no binary inspection). Then EITHER the next unbound framework native surfaces on
  the inflate→attach→surface-available path in the same one-per-boot discovery way (capture its exact ART stack — in
  particular whether/when libroblox, via its reflection-registered `AndroidGLView` SurfaceHolder.Callback, calls
  `SurfaceView.getHolder().addCallback(...)`, what triggers the private `surfaceCreated()`/`surfaceChanged()` and on
  which thread, and whether the engine's GL/EGL targets THIS RBXSurfaceView via `getHolder().getSurface()` →
  `ANativeWindow_fromSurface` vs a separate AndroidGLView surface — all libroblox-internal RUNTIME behavior, NOT
  first-party-determinable, NOT to be obtained by reverse-engineering libroblox.so), OR the view tree completes and
  the engine's SurfaceHolder.Callback / AndroidGLView surface path begins — at which point the RENDER-INTEGRATION
  frontier is ACTIVE and success looks like: the engine's surfaceCreated path fires, EGL context creation succeeds,
  and the FIRST engine frames appear in the winit window (the prize). The NDK/EGL path is de-risked
  (`gl_test_anw_binds_real_wsi_handle` green); the next workflow is the graphics.rs WSI publish + present-loop
  handoff + the surfaceCreated/surfaceChanged JNI-dispatch described above, designed AFTER the owner boot reveals
  the post-construction call chain.)** Gate: **550 unit + 4 integration (live milestone subprocesses, 0 SKIP) + 2
  doctests = 556 passed, 0 failed**, fmt/clippy `-D warnings`/release all 0-warning. Detail: §6 (2026-06-13
  SurfaceView/native_destructor entry).
- **2026-06-13 — 🪟 `View.native_get_window` BOUND — the last evidence-pinned blocker on the
  ActivityNativeMain.onCreate view-tree path is closed (the EXIT=124 boot's only remaining gap).** Owner live
  validation of the exit-10 fix chain (`/tmp/eclipse-exit10-validate.log`, EXIT=124 clean, NO coredump, ZERO
  native faults) proved the boot indefinitely stable through the splash→ActivityNativeMain transition and real
  Roblox HTTPS flag-fetch, with ONE remaining blocker: `ActivityNativeMain.onCreate` → `d1()` →
  `View.getViewTreeObserver()` threw `UnsatisfiedLinkError: No implementation found for android.view.Window
  android.view.View.native_get_window(long)` (ATL's `getViewTreeObserver` calls the instance native
  `native_get_window(widget)` to obtain the Window that owns the view tree's `ViewTreeObserver`). ROOT CAUSE
  (first-party): two coupled gaps — (1) `View.native_get_window` (declared `(J)Landroid/view/Window;` instance,
  vendored ATL `View.java:1244`) was unbound; (2) Eclipse held NO real Window object to return —
  `window_registry::WindowState.jobject` was a presence-only `Option<()>` and `Window.set_jobject` only recorded
  `Some(())`. FIX (this commit, both root causes): `window_registry` now CAPTURES the real Java Window — `jobject`
  is `Option<Global<JObject<'static>>>` (mirrors the proven `view_registry` `Global`+`Send`+Drop-releases-the-ref
  triple), with `set_jobject`/`with_jobject` accessors and a lock-free `ACTIVE_WINDOW` `AtomicI64` +
  `active_window()` (mirrors `view_registry::ACTIVE_ROOT`) published by `allocate`/cleared by `free`. `framework.rs`
  binds `View.native_get_window` in `register_view_natives` (validates the view `widget` handle, maps any view to
  the single process-shared window via `active_window()`, returns a fresh frame-local `new_local_ref` of the
  captured Window Global — never the Global raw — or JNI null on no-capture/stale → ATL's contract-valid
  floating-observer fallback, `View.java:1252`); `Window.set_jobject` now does `env.new_global_ref(&window)` +
  `window_registry::set_jobject` (the one place the Window object flows into Eclipse — Window.java:188, called from
  `set_native_window` AFTER `this.native_window` is set, so the captured object always has a valid `native_window`
  field). Also bound the immediate next native on the same view-tree path (code-path-proven, not speculative):
  `ViewTreeObserver.native_set_have_global_layout_listeners(Z)V` (instance no-op recording the flag — Eclipse has
  no host layout signal; ViewTreeObserver.java:1049, called from `addOnGlobalLayoutListener` right after
  `getViewTreeObserver`) via the new `register_view_tree_observer_natives` wired into `drive_lifecycle` after
  `register_view_natives`. Deliberately NOT bound (discovery signal stays loud, per policy):
  `View.native_getMatrix`, `View.native_getGlobalVisibleRect` — not on the captured path. [START-HERE marker moved
  2026-06-13 to the SurfaceView entry above — this owner live validation HAPPENED: `native_get_window` is BOUND and
  working (`/tmp/eclipse-getwindow-validate.log`, EXIT=124 clean), `ActivityNativeMain.onCreate` got PAST it and
  proceeded into `d1()` → `LayoutInflater.inflate` constructing `com.roblox.client.RBXSurfaceView` (extends
  android.view.SurfaceView) — the next discovery trip the prediction here named ("the view tree completes and the
  engine's `AndroidGLView`/ANativeWindow surface path begins"); the new gap was the unbound SurfaceView
  `native_constructor` + View `native_destructor`, now bound — see the entry above]. Gate: **548 unit + 4
  integration (live milestone subprocesses, 0 SKIP) + 2 doctests = 554 passed, 0 failed**, fmt/clippy `-D warnings`/
  release all 0-warning. Detail: §6 (2026-06-13 native_get_window entry).
- **2026-06-13 — 🏁 NATIVE-CRASH LADDER CLIMBED + EXIT=10 ROOT-CAUSED + FIXED (owner live validation of `54153e1`,
  `/tmp/eclipse-1223806-validate.log`, EXIT=10, NO coredump): ZERO SIGSEGV/SIGABRT this boot — the whole 4-core fix
  chain (782252 `__sF` → 866509 apkenv+altstack → 947663 thread-exit ordering → 1223806 dl_iterate_phdr/dladdr +
  Vibrator) is fully RUN-PROVEN.** The engine now catches its own C++ exceptions (DNS HttpError lines logged and
  SURVIVED — previously the first throw was a 61k-iteration std::terminate death); ~6 s of deep work (Vulkan
  swapchain active, ENGINE-logged mimalloc options, WorkManager/JobScheduler). The death was JAVA-LEVEL and clean
  (`System.exit called, status: 10`), root-caused to four framework-native gaps + the §6-reserved resolver-ABI gap,
  ALL FIXED THIS COMMIT: (1) **`android.app.Activity` natives** — the splash's `finish()` was the NORMAL
  splash→main transition (dex-proven: `startActivity(ActivityNativeMain)` BEFORE `finish()`); the unbound STATIC
  `nativeStartActivity` threw `UnsatisfiedLinkError` out of `Looper.loop` AFTER the message was dequeued, so the
  transition was irrecoverably consumed — **ActivityNativeMain (the engine-hosting activity) never reached
  onCreate**; the unbound INSTANCE `nativeFinish` (thrown 2×, dex-proven double-post) lost the splash
  down-lifecycle. Bound 5 of the artifact-verified 7 declared natives (`register_activity_natives`):
  `nativeStartActivity` drives the already-constructed Activity through exactly recipe steps 5–7 (factored shared
  helpers, same `checked()` discipline); `nativeFinish` validates the shared `window_registry` handle and drives
  onPause→onStop→onDestroy ONCE (NEVER frees the process-shared handle, NEVER closes the host window);
  `nativeResumeActivity` minimal dex-proven contract (no live tracked instance → false = the create fall-through);
  `isInMultiWindowMode` false; `isTaskRoot` tracked. `nativeOpenURI` + `nativeFileChooser` RECORDED-not-bound
  (host-action design pending; unbound stays the loud discovery signal). (2) **`AssetManager.openAssetFd`** — the
  ACTUAL exit(10): androidx.profileinstaller's worker hit the unbound native, and under the vendored libcore EVERY
  uncaught worker exception is process-fatal (`hacky_uncaught_exception_handler` → `System.exit(10)`); now a REAL
  fd implementation (fresh fd + `data_start` offset + uncompressed length for Stored entries via the new
  `Apk::entry_span`; negative return → Java's designed, CAUGHT FileNotFoundException for absent/compressed
  entries) — covers `openNonAssetFd` by construction. (3) **`Process.getElapsedCpuTime`** —
  CLOCK_PROCESS_CPUTIME_ID → ms (4 caught misses this boot; boot-long telemetry loss; latent process-fatal class).
  (4) **registration ORDERING** — new `register_engine_preload_natives` (Log + Process) called in `run_apk` BEFORE
  `preload_app_native_libs`, closing the JNI_OnLoad-time `println_native` miss behind the boot-long `process
  timestamps will be inaccurate` WARN (drive_lifecycle's block stays, idempotent re-registration). (5) the
  **engine-DNS NXDOMAIN** — the §6-reserved resolver-ABI suspicion CONFIRMED Eclipse-side and FIXED: bionic-shaped
  `getaddrinfo`/`freeaddrinfo`/`gai_strerror`/`getnameinfo` translating natives in `native_provider.rs` (bionic
  `addrinfo` tail is BSD order `ai_canonname`@24/`ai_addr`@32 — SWAPPED vs glibc — plus AI_/EAI_/NI_ divergences;
  deep-copied bionic-shaped chains, bionic-positive EAI codes; the `eclipse.netdb` trace = the reserved attribution
  diagnostic). Counts: provider **129 base / 187 total** (+4). The main-Looper pump's exception contract was ruled
  CORRECT — deliberately NO change (the loud error pair stays the regression signal that surfaced all of this).
  [START-HERE marker moved 2026-06-13 to the `native_get_window` entry above — this owner live validation
  HAPPENED: `/tmp/eclipse-exit10-validate.log`, EXIT=124 (clean timeout), NO coredump, ZERO native faults: the
  whole exit-10 fix chain is RUN-PROVEN — splash→ActivityNativeMain transition works, the boot is indefinitely
  stable, real Roblox HTTPS flag-fetch works; the SINGLE remaining blocker was ActivityNativeMain.onCreate
  throwing `UnsatisfiedLinkError` on the unbound `android.view.View.native_get_window(long)` (the next discovery
  trip the exit-10 entry predicted on the engine-surface/View path), now bound — see the entry above] (the plan
  was = OWNER live validation on the dev-host MAIN LOOP: `./target/release/eclipse run <APK>` with
  `ECLIPSE_ANDROID_FRAMEWORK_DIR=$HOME/.cache/eclipse/framework-patched` — expected: (a) `launchMainActivity`
  followed by NO UnsatisfiedLinkError — the factored steps 5–7 drive **ActivityNativeMain**, whose onCreate is
  the next discovery surface (expect NEW unbound-native trips on the engine surface/View path); (b) splash
  down-lifecycle driven ONCE (the second `finish()` a guarded no-op), host window staying up; (c) NO
  `main Looper pump failed` pairs; (d) at +5 s NO `hacky_uncaught_exception_handler`/`System.exit(10)` — on the
  merged APK profileinstaller reads the real Stored `baseline.prof` fd, on 2.721 it logs its caught
  FileNotFoundException path; (e) the `process timestamps will be inaccurate` WARN GONE; (f) the engine
  HttpResponse for `ecsv2.roblox.com` RESOLVING (no DnsResolve; the `eclipse.netdb` trace records the engine's
  ACTUAL ai_flags/family — closing the last unobserved resolver detail; watch Roblox's EAI_AGAIN retry
  classification now that EAI codes are bionic-shaped). Once the Java lifecycle holds, the standing next frontier
  is the RENDER-INTEGRATION build — wire the window's `ANativeWindow` into the engine's `AndroidGLView`/EGL path.
  If a CLEAR_TOP relaunch trips nativeResumeActivity's live-instance branch, capture the intent flags before
  deepening its semantics (onNewIntent delivery is NOT evidence-pinned). On ANY new silent SI_KERNEL/addr=0 kill:
  capture the fresh core FIRST. KEEP core 1223806 + `~/.cache/eclipse-forensics/core1223806` + `/tmp/core1223806-*`
  + `/tmp/t1stack.bin` until this validation passes.) Unproven/recorded items deliberately NOT coded:
  nativeOpenURI/nativeFileChooser host-action design; the `AssetManager.destroy()` stub → dictionary `readAsset`
  IOException regression vs 06-11; the tzdata/java.time ART env gap; the ~7.4 s `ActivitySplash.onCreate` stall +
  `Resource is not a Drawable` WARNs (profile first); the benign ClientAppSettings.json FileNotFound reads; ART
  attach-time 32 KiB guard-less altstacks on engine threads (866509 open item). Gate: **544 unit + 4 integration
  (live milestone subprocesses, 0 SKIP) + 2 doctests = 550 passed, 0 failed**, fmt/clippy `-D warnings`/release all
  0-warning. Detail: §6 (2026-06-13 exit-10 entry).
- **2026-06-12 — ✅ CORE 1223806 ROOT-CAUSED + FIXED (owner live validation of `ddabcd7`,
  `/tmp/eclipse-947663-validate.log`, EXIT=139 → core 1223806 124.7 M — the 947663 thread-exit fix is RUN-PROVEN
  (recurrence discriminator CLEAN); this death was a NEW, SILENT mechanism):** libroblox's statically-linked libc++abi
  unwinder resolves FDEs via its `dl_iterate_phdr@LIBC` import, which Eclipse never provided — host glibc's walk is
  BLIND to Eclipse's anonymously-mmapped images, so EVERY C++ throw in libroblox was uncatchable: phase-1 unwind → no
  FDE → `std::terminate` → Roblox's terminate handler re-raises to classify → a 3-frame cycle repeated **61,497×**
  consuming 12.2 MB of stack, entered on the engine HTTP worker's DNS-failure error path. The actual KILL was a kernel
  `force_sigsegv()` (NT_SIGINFO `si_code=128` SI_KERNEL, `si_addr=0x0`): signal-frame setup targeted a
  registered-but-UNWRITABLE `SA_ONSTACK` altstack (owner UNPROVEN — kernel-only state; lead suspect = an
  engine-registered altstack later decommitted, e.g. a mimalloc PROT_NONE purge), handler reset to SIG_DFL, ZERO
  handler bytes ran — the complete explanation of why BOTH reporters (tap + crashpad) were silent. THIRD mechanism:
  `android.os.Vibrator.native_constructor()` `UnsatisfiedLinkError` escaped `Looper.loop` 17 ms before death — the
  main Looper pump was permanently dead (splash init unreachable that boot regardless). FIXES: (1) Eclipse-owned
  bionic `dl_iterate_phdr` + same-class `dladdr` (NEW `src/loader/module_registry.rs`; every loader-mapped image
  registered in `engine.rs` BEFORE any engine instruction, Drop-symmetric unregistration; one fix covers all 4
  importing engine libs); (2) `android.os.Vibrator` FULL declared native set bound in `src/framework.rs`
  (`native_constructor()I` → −1 documented no-vibration-device constant, `native_vibrate(IJ)V` logged no-op —
  intentional capability handling; NO catch-and-continue in the pump); (3) sigaltstack OBSERVABILITY: Eclipse-owned
  translating native (layout-identical forward; `src/loader/sigaltstack_shim.c` captures the caller) logs
  tid/ss_sp/ss_size/ss_flags + caller-module attribution into a 64-entry ring — coverage = bionic-import-routed calls
  ONLY (an EMPTY ring for a dying tid implicates a host-side registrant, most likely ART's attach-time altstack — the
  866509 open item — not an attribution bug). Counts: provider **125 base / 183 total** (+3). ALSO: the engine curl
  `ecsv2.roblox.com` NXDOMAIN is NOT environmental (the host resolves it; the SAME process's Java/okhttp path
  succeeded 7 s earlier) — RECLASSIFIED as a suspected Eclipse resolver-ABI gap (libroblox imports
  getaddrinfo/freeaddrinfo/gai_strerror/getnameinfo, Eclipse provides none; bionic vs glibc `addrinfo` field order /
  `AI_*` / `EAI_*` diverge) — diagnostics-first, a future blocker once gameplay traffic runs on the engine path.
  STRAWTOGRASP/`SocketImpl.delegate` CLOSED as benign-by-design (wolfssljni dual-shape probe; never a death marker).
  [START-HERE marker moved 2026-06-13 to the exit-10 entry above — this owner live validation HAPPENED:
  `/tmp/eclipse-1223806-validate.log`, EXIT=10, NO coredump, ZERO SIGSEGV/SIGABRT — the dl_iterate_phdr fix
  run-proven (the engine's DNS HttpError throws were CAUGHT and the boot SURVIVED them; no terminate loop), the
  Vibrator fix held (the pump stayed alive past `InitHelper`), ~6 s of deep work (swapchain active, engine-logged
  mimalloc options, WorkManager/JobScheduler); the death was JAVA-level `System.exit(10)` — see the 2026-06-13
  entry above] (the plan was = OWNER live validation on the dev-host MAIN LOOP: `./target/release/eclipse run <APK>`
  with `ECLIPSE_ANDROID_FRAMEWORK_DIR=$HOME/.cache/eclipse/framework-patched` — expect: the DNS-failure throw becomes
  a CAUGHT, logged HttpError with a retry (`numberOfTimesRetried` increments; NO 3-frame terminate loop); the main
  Looper pump SURVIVES past `InitHelper` (the Vibrator no-vibration-device registration line prints; no `main Looper
  pump failed`); `eclipse.sigaltstack` attribution lines name every engine altstack registrant; tap/crashpad still
  book-keep first-chance ART signal-11s. If an engine throw STILL terminates → suspect the unwinder's secondary
  lookups (dladdr-based / `__gnu_Unwind`) — capture the core, do not iterate blind. On any recurrence of a silent
  SI_KERNEL/addr=0 kill: capture the fresh core FIRST — the ring + core together name the stack owner (empty ring for
  the dying tid = host-side/ART registrant). KEEP core 1223806 + `~/.cache/eclipse-forensics/core1223806` +
  `/tmp/core1223806-*` + `/tmp/t1stack.bin` until this validation passes; core 947663 + its scratch are RELEASED
  (discriminator clean). After validation: the render-integration build — wire the window's `ANativeWindow` into the
  engine's `AndroidGLView`/EGL path; the dormant pre-load surfaces (libimage_processing_util_jni 5 /
  librenderscript-toolkit 3) remain the recorded design item.)** Untriaged non-blocking boot observations recorded,
  evidence-first (§6): the ~7.4 s `ActivitySplash.onCreate` stall + 4× `Resource is not a Drawable` WARNs; the
  `AssetManager.destroy()` STUB → dictionary `readAsset` IOException regression vs 06-11; the `Couldn't find any
  tzdata file!` ART env gap; `CrashLibFileHelper` nativeLibraryDir miss. Gate: **535 unit + 4 integration (live
  milestone subprocesses, 0 SKIP) + 2 doctests = 541 passed, 0 failed**, fmt/clippy `-D warnings`/release all
  0-warning. Detail: §6 (2026-06-12 core-1223806 entry).
- **2026-06-12 — ✅ CORE 947663 ROOT-CAUSED + FIXED (the engine-thread-exit MIMALLOC fault — Yoshi's open question
  ANSWERED): Eclipse INVERTED bionic's thread-exit destructor order.** `thread_trampoline`/`eclipse_pthread_exit`
  ran the pthread-KEY destructors while `__cxa_thread_atexit_impl` stayed on host glibc (cxa finalizers only later,
  in `__call_tls_dtors`) — public AOSP bionic `pthread_exit.cpp` runs `__cxa_thread_finalize()` FIRST, then
  `pthread_key_clean_all()`. Under Eclipse the engine's key dtors mi_free'd + abandoned the dying thread's 128-slot
  TLS-registry block (key1 = `_mi_thread_done`, disasm-matched to public mimalloc), a newborn sibling RECLAIMED and
  re-stamped it, THEN the late cxa finalizer walked the stale obj → `movq $0x0,0x58(%r12)` through NULL+0x58 (the
  deterministic `libroblox+0x2779cc4` MAPERR addr=0x58). The per-thread body was NEVER half-initialized — it was
  freed-then-reclaimed before its finalizer ran (hypothesis (b) ordering WON; (a) lazy-init and (c) TSD corruption
  disproven by core 947663). FIX: Eclipse-owned `__cxa_thread_atexit_impl` (per-thread LIFO, re-entrancy-safe
  drain) run BEFORE the key sweep on BOTH exit paths; the forced-unwind path typed `extern "C-unwind"` (glibc
  `pthread_exit` unwinds — nounwind Rust frames aborted; pinned against the installed rustc 1.96.0 std
  personality). ALSO FIXED: `pthread_atfork` — the 2ceca8a "pre-load resolves clean / apkenv never entered" claim
  was FALSIFIED by this validation (log line 60: unresolved 2→1, naming exactly `pthread_atfork`; nothing
  boot-mapped defines it) — now a native forwarding to link-time `libc::pthread_atfork`, AND the masking link.rs
  allowlist entry REMOVED so the boot-path pin fails closed; and `ANativeWindow_getFormat` (libsurface_util_jni
  pre-loads 1/1). Counts: pthread 51→53, ndk 27→28, provider 122 base / 180 total. [START-HERE marker moved
  2026-06-12 to the core-1223806 entry above — this owner live validation HAPPENED:
  `/tmp/eclipse-947663-validate.log`, EXIT=139 → fresh core 1223806, a NEW mechanism; the recurrence discriminator
  came back CLEAN on every axis (zero `libroblox+0x2779cc4` frames at this base, zero MAPERR addr=0x58, zero
  `__call_tls_dtors`/`pthread_exit`/cxa frames across all 76 threads), the old death milestone was crossed at
  t=8.769 with ~0.32 s of deep multi-thread work beyond it — the cxa-before-keys fix HELD; retention condition
  satisfied: core 947663 + `~/.cache/eclipse-forensics/core947663` RELEASED] (the plan was = OWNER live validation
  on the dev-host MAIN LOOP: `./target/release/eclipse run <APK>` with
  `ECLIPSE_ANDROID_FRAMEWORK_DIR=$HOME/.cache/eclipse/framework-patched` — expect: the libbacktrace-native pre-load
  WARNING GONE (`unresolved_strong=0`) + `Runtime.nativeLoad: already pre-loaded` for backtrace-native (the apkenv
  delegation closed for the last boot-path lib); libsurface_util_jni pre-loading clean; NO early-fault tap capture
  at `libroblox+0x2779cc4` / MAPERR addr=0x58 — if the mimalloc fix holds, the engine's burst-created pool threads
  exit cleanly and the boot survives past the Vulkan swapchain to the next wall — then the render-integration
  build: wire the window's `ANativeWindow` into the engine's `AndroidGLView`/EGL path. KEEP core 947663 + the
  scratch copy at `~/.cache/eclipse-forensics/core947663` until that boot: same rip + a foreign live-sibling TID
  stamp in the faulted obj again = a second reuse path; an all-zero obj with NO foreign TID = the purge/decommit
  variant — capture the new core before touching code.)** Remaining pre-load surfaces after that (dormant today):
  libimage_processing_util_jni (5) + librenderscript-toolkit (3) → one shared pin-or-copy AndroidBitmap/
  ANativeWindow-CPU-buffer design (8 imports, recorded design work); libtrampoline is NOT JNI-loadable (dynsym
  all-UND — triage-only). Open items recorded, not coded: D2 (key dtors never run on non-Eclipse-created threads),
  D4 (engine-side forced-unwind exposure — the ordering half IS fixed), the cross-allocator vasprintf/realpath +
  mallinfo-shape hazard notes (`bionic_env.rs`). Gate: **527 unit + 4 integration + 2 doctests = 533 passed, 0
  failed**, fmt/clippy `-D warnings`/release all 0-warning. Detail: §6 (2026-06-12 core-947663 entry).
- **2026-06-12 — ✅ CORE 866509 ROOT-CAUSED + FIXED (the first-ever-swapchain boot's death, owner live validation of
  the `__sF` fix): (1) `libbacktrace-native.so`'s Eclipse pre-load failed on exactly 2 missing natives
  (`__android_log_vprint` + `__umask_chk`), so its `System.loadLibrary` DELEGATED into the apkenv shim linker, which
  died writing through its never-initialized `_r_debug_ptr` (SIGSEGV, fault addr 0x18, rip in `libdl_bio.so.0.0.1` —
  NOT libroblox/ART) — both natives now provided (liblog C-shim + Rust FORTIFY umask; provider 121 base / 177 total),
  the pre-load resolves clean and the apkenv delegation is never entered [CORRECTED 2026-06-12 — live validation
  FALSIFIED this clause: unresolved went 2→1, `pthread_atfork` remained (nothing boot-mapped defines it; glibc's is a
  compat-versioned WEAK `@GLIBC_2.2.5` dlsym cannot see) — now an Eclipse pthread native and the masking link.rs
  allowlist entry removed; see the core-947663 entry above]; (2) the fatal-signal handler chain
  (~79.2 KiB measured) overflowed ART's heap-backed, guard-less 32 KiB main-thread altstack, zero-filling live heap
  (the `malloc(): unaligned tcache chunk detected` SIGABRT that destroyed the crash report mid-backtrace) — the main
  thread now gets an Eclipse-owned guard-paged 256 KiB mmap'd altstack right after `JNI_CreateJavaVM`
  (`install_guarded_altstack`, wired in `runtime::boot`). [START-HERE marker moved 2026-06-12 to the core-947663
  entry above — this owner live validation HAPPENED: `/tmp/eclipse-866509-validate.log`, EXIT=139 → core 947663;
  the altstack + named-warning fixes run-proven, but the libbacktrace-native WARNING did NOT clear (2→1:
  `pthread_atfork`)] (the plan was = OWNER live validation of
  these fixes on the dev-host MAIN LOOP: `./target/release/eclipse run <APK>` with
  `ECLIPSE_ANDROID_FRAMEWORK_DIR=$HOME/.cache/eclipse/framework-patched` — expect: the
  `pre-load of libbacktrace-native.so failed` WARNING GONE; the new boot line `main-thread alternate signal stack:
  Eclipse guard-paged 256 KiB …`; the two remaining pre-load WARNINGs (libimage_processing_util_jni 5 /
  librenderscript-toolkit 3) now print symbol NAMES (run-proving the static enumeration — the warning previously
  printed only a count); the boot should reach the Vulkan swapchain again and advance to the NEXT wall — most
  probably the engine-thread-exit MIMALLOC fault (pump entry below: `_mi_thread_done` on a partially-init per-thread
  heap; collaborator's diagnostics ongoing, no fix proposed yet per the evidence standard) — then the
  render-integration build: wire the window's `ANativeWindow` into the engine's `AndroidGLView`/EGL path).**
  Validation context: the `__sF` fix IS run-proven (crashpad's in-handler logging reached stderr intact; the 8.46 s
  first-chance ART signal-11 was book-kept and the boot SURVIVED it, crashpad-first) and the boot reached the
  FIRST-EVER `Vulkan surface + swapchain initialized; clear-and-present loop active` on this machine (B8G8R8A8_SRGB
  800×600) before the fatal fired ~0.14 s later. Recorded-only (no code, deliberate): the 8 NDK natives behind the
  two remaining same-pattern pre-load failures (need a real ANativeWindow CPU-buffer + jnigraphics surface — design
  work, not fall-through stubs); ART-attached ENGINE threads still receive ART's guard-less 32 KiB heap altstack at
  attach (vendored `thread_linux.cc` overwrites any pre-installed stack — open work item; a vendor-build-side
  mitigation of `kHostAltSigStackSize`/guard page is the candidate that closes the class); the ART-first
  fault-manager-ordering item stays monitored-only (today's boot SURVIVED the first-chance signal-11 with
  crashpad-first, and the fatal PC was native — ordering-orthogonal; zero ordering-attributable failures on this
  tree). Gate: **524 unit + 4 integration + 2 doctests = 530 passed, 0 failed**, fmt/clippy `-D warnings`/release
  all 0-warning. Detail: §6 (2026-06-12 core-866509 entry).
- **2026-06-12 — ✅ `__sF` SHAPE MISMATCH CONFIRMED (core 782252) AND ROOT-CAUSE-FIXED: Eclipse provided the bionic
  data symbol `__sF` as a 24-byte table of 3 glibc `FILE*` POINTERS, but bionic's public ABI makes `__sF` an array of
  3 × 152-byte STRUCTS whose ADDRESSES are the streams — now a bionic-shaped 456-byte sentinel backing + 25
  translating stdio natives. [START-HERE marker moved 2026-06-12 to the core-866509 entry above — this owner live
  validation HAPPENED: `/tmp/eclipse-sf-validate.log`, EXIT=134 → core 866509, the `__sF` fix run-proven] (the plan
  was = OWNER live validation of the MERGED tree on the dev-host
  MAIN LOOP: `./target/release/eclipse run <APK>` with
  `ECLIPSE_ANDROID_FRAMEWORK_DIR=$HOME/.cache/eclipse/framework-patched` — expect crashpad's in-handler logging to
  SURVIVE now (no more silent EXIT=139 through the `__sF` fputs fault); with the main-Looper pump merged (entry
  below) the boot reaches DEEP engine init and deterministically hits the root-caused thread-exit-dtor SIGSEGV — so
  the next CODE frontier is that per-thread cleanup fault (entry below), then the render-integration build — wire
  the window's `ANativeWindow` into the engine's `AndroidGLView`/EGL path).** Context: the "ENGINE SIGSEGV RESOLVED,
  6/6 stable" entry below was MACHINE-SPECIFIC (and independently superseded by the pump entry below) — today an
  owner boot on this machine (cachyos x86-64) died, EXIT=139 at ~8 s (`/tmp/eclipse-render-check.log`, systemd core
  782252). Core forensics confirmed the long-suspected (UNVERIFIED until now) `__sF` hypothesis end to
  end: crashpad's bionic-compiled logger computes `stderr = &__sF[2]` = base+0x130 (LP64 `sizeof(struct __sFILE)` =
  152 per public AOSP NDK headers), which landed 280 bytes PAST Eclipse's 24-byte pointer table inside unrelated Rust
  statics → glibc `fputs` read `fp->_lock` = `0xff` and faulted at `si_addr=0x107` (the historic "rdi=0xff invalid
  string pointer" was a misattributed reloaded register — the string was VALID). The FIRST fault (the one crashpad was
  logging) turned out to be NOT an engine bug: a routine ART implicit-null-check SIGSEGV in AOT boot-classpath code
  (`boot-wolfssljni-hostdex.oat`, si_addr=0x0, in the machine/network-dependent wolfssljni/okhttp `getAllAppSettings`
  path — which reconciles the collaborator's 6/6 with today's repro) that ART's fault manager is designed to recover
  into a Java NPE; it killed the boot only because crashpad sat AHEAD of ART's fault manager in the chain and then
  crashpad's own logging died on `__sF`. COMPANION WORK ITEM (flagged, deliberately NOT in this diff): ART-first
  fault-manager ordering in the `eclipse_sigaction`/tap chain so routine managed-NPE fixups are recovered before
  crashpad classifies them as real crashes — without it, a boot that hits that NPE path still dies (now cleanly
  logged through the fixed stdio). Fix shipped: `SF_FILE_STRIDE=152`-pinned 3×152 backing,
  `eclipse_sf_translate_stream` sentinel remap, 22 Rust + 3 C-shim (`src/loader/stdio_shim.c`:
  fprintf/fscanf/vfprintf) translating natives covering every FILE*-consuming stdio import of the five `__sF`
  importers, plus the `__fread_chk` bionic-vs-glibc ARGUMENT-ORDER fix (same-pattern catch). Gate: **519 unit + 4
  integration (live milestone subprocesses, exact SUCCESS markers) + 2 doctests = 525 passed, 0 failed**, fmt/clippy
  `-D warnings`/release all 0-warning. Detail: §6 (2026-06-12 `__sF` root-cause entry).
- **2026-06-12 (main-Looper pump) — 🚀 Roblox now boots PAST the splash into DEEP engine init (Mimalloc · RbxStorage/
  SQLite WAL · AndroidGLView · HTTP/network · telemetry) and the engine SIGSEGV is now REPRODUCIBLE + ROOT-CAUSED.
  [START-HERE marker consolidated into the `__sF` entry above at the 2026-06-12 merge] (frontier = **MIMALLOC's
  per-thread-heap cleanup** (`_mi_thread_done` via `__cxa_thread_atexit_impl`) faults at engine-thread EXIT on a
  PARTIALLY-initialized mimalloc heap in `[anon:mimalloc]` — header valid, per-CPU body all-zero; NOT static TLS, and the
  thread IS Eclipse-`pthread_create`'d — see the gdb root-cause below).** Implemented the **Android main-`Looper`
  pump**: Eclipse drives the lifecycle then hands the main thread to winit, so it never ran `Looper.loop()` — main-thread
  `Handler.post` continuations + `SurfaceHolder` callbacks queued but never dispatched, stalling Roblox at `ActivitySplash`
  RESUMED. Fix (`src/framework.rs` + `src/graphics.rs`): bind `MessageQueue.nativePollOnce(JI)Z`/`nativeWake(J)V`
  (ATL's `next()` yields via `nativePollOnce`; ours is NON-BLOCKING — `false` for `timeout==0` to pull a ready message,
  `true` otherwise to yield), add `framework::pump_main_looper` (drives `Looper.loop()` once), and call it from a new
  winit `GameWindow::about_to_wait` hook (fires each frame via the renderer's self-driven `request_redraw`). **Result
  (live, `/tmp/r*.log`):** the pump dispatches main-thread work (`main Looper pump active`) and Roblox advances FAR — it
  initializes mimalloc, opens its `rbx-storage.db` (SQLite, "recovered 59 frames from WAL"), inits `AndroidGLView`,
  runs telemetry, and makes real HTTP requests (`ecsv2.roblox.com` — host internet works; networking is NOT the blocker).
  **This corrects the prior entry's claim that the engine SIGSEGV was "resolved": it was never resolved — the splash
  stall just prevented REACHING the faulting code.** With the pump, Roblox reaches it every run; the team's early-fault
  tap now captures it deterministically (`signal 11 MAPERR addr=0x58 rip=libroblox+0x2779cc4`). **ROOT-CAUSED via gdb
  (catchpoint conditioned on the faulting instruction bytes `movq $0,0x58(%r12)` to skip ART's implicit-null-check
  SIGSEGVs):** the faulting frame is a libroblox destructor (fn @ `+0x2779bb0`) run by **`__call_tls_dtors`** (libc) — i.e.
  a thread is EXITING and glibc is running a destructor libroblox registered via **`__cxa_thread_atexit_impl`** (a WEAK
  libroblox import left on the host glibc baseline, `bionic_pthread.rs:1773`). The destructor walks a per-thread/per-CPU
  heap structure (`rbx` = a linked list of nodes; it reads `[rbx+0x408]` → a node whose sub-pointer is NULL, then writes
  `[NULL+0x58]`; the surrounding code masks `sched_getcpu()&0xf` to index a 16-slot per-CPU array, stride `0x4a140`, with
  `lock cmpxchg`). The exiting thread is one of several ART-attached engine threads named "Main" (siblings: `RBX Worker
  A–P`, `[vkcf]/[vkrt]/[vkps]`, `HttpClient`, …). **CORRECTION (verified, do not repeat the wrong guess):** this is **NOT a
  static-TLS gap** — `readelf` confirms libroblox has **no `PT_TLS`, 0 TLS symbols, 0 `R_X86_64_TPOFF64`** (the team's
  "libroblox has no PT_TLS" holds). So the bug is a libroblox per-thread CLEANUP structure left with a null node at the
  point an engine thread exits — a thread-lifecycle / per-thread-state issue, exposed (not caused) by the pump advancing
  Roblox far enough to spawn+exit these native threads. **Candidate (c) RULED OUT (verified):** a temporary TID trace in
  `nativePollOnce` (since reverted) showed **0 worker threads ever run `Looper.loop()`** across pump-active runs — the
  non-blocking `nativePollOnce` only ever runs on the main/winit thread, so the engine fault is **independent of the pump's
  yield**; the pump merely advances Roblox so its NATIVE engine threads spawn+exit. **IDENTIFIED (gdb, `[anon:mimalloc]`):
  it is MIMALLOC's per-thread-heap cleanup.** The faulting `obj` (`rbx`) lives in an `[anon:mimalloc]` 1 GB arena mapping;
  it is a mimalloc per-thread heap / `mi_tld_t` whose **header is valid** (a `0x500`-sized descriptor at `rbx-0x80..rbx-0x10`
  with intra-arena self-pointers + arena pointers) but whose **per-CPU/segment body (`rbx`+) is ALL-ZERO** — i.e. a
  PARTIALLY-initialized heap. The destructor (`_mi_thread_done`-style, registered via `__cxa_thread_atexit_impl`, run by
  glibc `__call_tls_dtors`) walks the per-CPU body (`sched_getcpu()&0xf` → 16 slots, stride `0x4a140`, `lock cmpxchg`) and
  dereferences a zero block pointer → write to `NULL+0x58`. **Candidate (a) RULED OUT:** the faulting thread (e.g. tid
  325442) IS Eclipse-created — it appears in the `ECLIPSE_TRACE_THREADS=1` `pthread_create child running tid=` log (one of
  ~24 thread-pool threads spawned in a burst). So it is NOT a registration-skip; it's a mimalloc per-thread heap left
  half-initialized when the thread exits. **NEXT (open):** why is the mimalloc per-thread body zero at exit on these
  burst-created pool threads? — likely a mimalloc lazy-thread-init vs thread-done ORDERING/coordination issue under
  Eclipse's threading (the thread registered `_mi_thread_done` but exited before/without the per-CPU body init completing),
  OR a destructor-ordering interaction between Eclipse's pthread-key destructors (`bionic_pthread.rs::run_tls_destructors`)
  and glibc's `__call_tls_dtors`. Approach: read mimalloc's `_mi_heap_init`/`_mi_thread_done` (open source) to see what
  zeroes/gates the per-CPU body, and check whether Eclipse's thread trampoline runs the thread-start hook mimalloc's lazy
  init expects. **Also fixed a `panic = "abort"` regression the pump exposed:** Eclipse routes the engine's
  `android.util.Log`/`liblog` firehose + its own native diagnostics through `tracing`, emitted from ART/bionic WORKER
  threads; `tracing-subscriber`'s default `fmt` layer formats via a `thread_local! BUF` (`fmt_layer.rs:1022 BUF.with`),
  and a worker logging during its TLS teardown hit `LocalKey::with` on a destroyed TLS → AccessError → **process abort**.
  Replaced it with a teardown-safe `diagnostics::PanicSafeStderr` layer that formats into a function-LOCAL buffer (zero
  thread-locals; same RFC3339+level+target+fields format, no ANSI). Gate: **517 unit + 4 integration + 2 doctests**
  (+1 `nativePollOnce` yield-table test), fmt/clippy `-D warnings`/release all 0-warning. Durability: overlay still needs
  `ECLIPSE_ANDROID_FRAMEWORK_DIR`; pump + logging fix are in-binary. Detail: §6 (2026-06-12 main-Looper pump).
  *(Superseded entry below — its "engine SIGSEGV resolved / 6-6 clean" held only while the splash stall hid the fault.)*
- **2026-06-12 (live-validated) — ⚠️ SUPERSEDED: "engine SIGSEGV resolved; boot STABLE to window (6/6 clean)" — the
  fault was merely UNREACHED behind the splash stall; the main-Looper pump above reaches it every run. [Also
  MACHINE-SPECIFIC: the same day it DID die pre-pump on the owner's machine — core 782252, the `__sF` entry above.]**
  Owner live-validation on the dev-host MAIN LOOP (`./target/release/eclipse run <APK>` with
  `ECLIPSE_ANDROID_FRAMEWORK_DIR=$HOME/.cache/eclipse/framework-patched`): the signal-ABI work (now COMMITTED + merged —
  origin/main `1b56e99`) made the engine's crashpad-era SIGSEGV stop reproducing entirely. Across 6 consecutive runs
  (`/tmp/tap{1..5}.log` warm + `/tmp/cold1.log` cold) the boot drives lifecycle 1–7 → `ActivitySplash` RESUMED → winit
  window `Eclipse — com.roblox.client` → **Vulkan swapchain present loop** (B8G8R8A8_SRGB, 800×600, 3 images) every time:
  `EXIT=124` (clean 30–40 s timeout), zero `Fatal signal 11`, zero `corrupted double-linked list`, **the early-fault tap
  never fires** (no engine SIGSEGV left to trace — the team's prior "validate the tap captures the engine fault"
  START-HERE is now MOOT; the tap stays as a dated diagnostic floor). The earlier transient instability (1/3 SIGABRT
  heap-corruption right after the merge) did not recur once the release binary was rebuilt from the merged tree + the
  asset caches warmed; cold-start is also clean, so it was a first-run asset-unpack race, not a standing bug. **What the
  engine reaches:** FLog crashpad init + `Roblox files folder`/`cache folder` + `AndroidGLView nativeInitClientSettings`
  + `FlagCache Deferring … post TTI`, then it idles — because once `graphics::run_windowed` takes the main thread for
  Eclipse's OWN clear-and-present loop (the recorded `view_registry` quads — white bg + blue UI rects, owner
  screenshot-confirmed), the engine's `AndroidGLView` has **no host surface wired to it**, so Roblox's real GL UI never
  reaches the screen. **NEXT FRONTIER (the big render-integration build):** hand Eclipse's window's `ANativeWindow` to
  the engine's `AndroidGLView`/EGL path (the `__gl-test-anw` diagnostic already proves engine-GLES2-on-Eclipse's-window
  works — integration test `gl_test_anw_binds_real_wsi_handle` is green; the boot just doesn't WIRE it yet). **Secondary
  observations (not blocking the window):** ART logs `STRAWTOGRASP: GetFieldID(SocketImpl.delegate) returning NULL` (an
  ART/libcore networking-internal miss, NOT an Eclipse gap — wolfSSL-backed okhttp sockets do connect/read) [CLOSED
  2026-06-12: benign-by-design, not a "miss" at all — the sole caller is wolfssljni's dual-shape `setFd` probe for
  OpenJDK-13+ `DelegatingSocketImpl`, which NULL-checks the fid and ExceptionClears the EXPECTED `NoSuchFieldError`;
  never a death marker — see §6 core-1223806 entry]; the benign
  `framework-res.apk` dex2oat "no dex files" + `ClassLoaderContext`/duplicate-class warnings; Canvas `nDrawColor` draw
  cascade still disabled (GskCanvas-backed, view quads + text still render). Gate: **516 unit + 4 integration + 2
  doctests**, fmt/clippy `-D warnings`/release all 0-warning. Durability: overlay output is a cache artifact (rebuild via
  `tools/framework-overlay/patch-framework.sh`; `eclipse run` needs `ECLIPSE_ANDROID_FRAMEWORK_DIR`). Detail: §6
  (2026-06-12 engine-SIGSEGV-resolved).
- **2026-06-12 — EARLY-FAULT TAP IMPLEMENTED (gate-green: 516 unit + 4 integration (self-skip path, displays unset)
  + 2 doctests = 522 passed, 0 failed; since committed with the signal-ABI work — origin/main `1b56e99`). [START-HERE
  marker retired 2026-06-12 — the owner live validation happened; see the entries above.] (frontier WAS = OWNER live
  validation on the dev-host MAIN LOOP):** run `ECLIPSE_ANDROID_FRAMEWORK_DIR=$HOME/.cache/eclipse/framework-patched cargo run -- run <APK>`
  (rebuild the overlay FIRST via `tools/framework-overlay/patch-framework.sh` if `~/.cache/eclipse` was wiped — the
  overlay is a cache artifact) and capture the tap's dump of the ORIGINAL engine SIGSEGV at ~`libroblox+0x1f28xxx`:
  expect the `*** ECLIPSE EARLY-FAULT TAP: signal 11 … (libroblox+0x…)` block on stderr BEFORE crashpad's
  `Run book keeping for signal 11`, then the boot dying exactly as today (the tap masks nothing — it is a
  diagnostic, not a fix). Copy that evidence into §6 SAME-SESSION, and only THEN evaluate the (UNVERIFIED) `__sF`
  hypothesis for crashpad's `fputs` second fault. **What landed (NEXT item (1) of the entry below):** a
  kernel-first `SA_SIGINFO|SA_ONSTACK` SIGSEGV tap installed under a `std::sync::Once` as the FIRST statement of
  `engine::load_app_native_lib` (after ART, before any engine instruction); it dumps
  `si_signo`/`si_code`/`si_addr` + RIP/RSP/RBP/ERR + a bounded RBP stack walk (engine-PC-filtered once
  `publish_engine_text_range` arms it after the libroblox map) via ONE async-signal-safe `write(2)`, then CHAINS to
  whatever crashpad registered through the `eclipse_sigaction` seam (alloc-free claim-once pool slots; tid-scoped
  re-entry latch; chain seed before kernel install — the three 2026-06-12 review-fix entries in §6). Detail + carried review notes: §6
  (2026-06-12 early-fault tap base entry).
- **2026-06-11 (late) — 🎉 BIONIC SIGNAL ABI DONE: crashpad's first-chance SIGSEGV handler now actually RUNS on the real
  Roblox v2.721.1108 boot — the engine raises SIGSEGV inside `libroblox.so` and Roblox's own logger emits
  `"Run book keeping for signal 11"` (it did not before — delivery double-faulted into a garbage address). The
  underlying engine SIGSEGV is STILL there; crashpad now hits a SECOND fault INSIDE its own logging path.
  (Frontier = the engine's original `libroblox+0x1f28xxx` fault, plus the crashpad-internal fputs crash that hides
  it — the START-HERE marker moved to the 2026-06-12 entry above: the early-fault tap from NEXT item (1) is now
  implemented; live validation is the owner's.)** **Done this session (gate-green, NOT committed — owner's session instruction):**
  `src/loader/native_provider.rs` now provides the **bionic signal ABI as 6 translating natives** —
  `sigaction`/`sigemptyset`/`sigaddset`/`sigfillset`/`sigprocmask`/`pthread_sigmask`. Mechanism: a
  `#[repr(C)] BionicSigaction { sa_flags@0, handler@8, sa_mask@16, sa_restorer@24 }` (32B) and `BionicSigsetT = u64`
  match the AOSP LP64 layout (`bits/signal_types.h`); each native translates bionic ↔ glibc fields and FORWARDS to
  glibc with the glibc-shaped struct. `SA_RESTORER` is stripped both directions (glibc supplies its own `__restore_rt`;
  leaking it back would corrupt re-registration). Provider total **88 → 94 base** (`150` with pthread+sysconf); the
  registration list and count assertion in `with_bionic_natives_registers_the_three_implemented_categories` are
  updated; the stale "ABI-identical" claim for `pthread_sigmask` in `bionic_pthread.rs` `PTHREAD_NATIVE_COUNT` is
  corrected (sigset width was 8 vs 128 — non-null `oldset` would have written 128 bytes through an 8-byte set).
  **5 new tests (the regression guards):** `bionic_sigaction_layout_matches_lp64` (pins offsets/sizes so a drift can't
  re-introduce the scramble), `bionic_sigset_ops_match_the_bionic_contract` (sigempty/fill/add bound checks),
  `bionic_sigset_translation_round_trips` (widen/narrow lossless, kernel `sigismember` cross-check),
  `bionic_sigaction_registers_a_live_handler_and_round_trips_oldact` (REGISTERS a SIGURG handler through the bionic
  path, `raise(SIGURG)`, asserts the kernel DELIVERED it — this is the smallest reliable check that would have
  failed on the prior fall-through), `bionic_sigprocmask_translates_both_directions` (cross-checks the host mask
  via glibc after a bionic SIG_BLOCK). **LIVE-BOOT EVIDENCE (`/tmp/eclipse-sig{1,2}.log`, `coredumpctl` 482294):**
  lifecycle 1–7 still drives (`Activity resumed: recipe steps 1–7 driven`); the engine native startup runs (FLog
  crashpad init + `Roblox files folder`/`cache folder` + `initialized crashpad, plug in the sfirst chance exception
  handler`); the engine then SIGSEGVs at the same site as before — but now the line **`FLog::CrashReportLog Run
  book keeping for signal 11`** is emitted (it was IMPOSSIBLE before — delivery faulted at a garbage handler address,
  proving the bionic-ABI fix is load-bearing). Crashpad's signal handler then crashes inside `fputs` (glibc + 0x93fc5,
  rdi=`0xff`=invalid string ptr) — a SECOND ABI gap inside crashpad's own logging path (likely a bionic/glibc stdio
  `__sF`/FILE layout disagreement or a pre-existing engine bug that the previously-broken signal delivery had hidden).
  **NEXT (root-cause, in order):** (1) trace the *original* engine SIGSEGV with a tiny logging early-fault handler
  (a verbatim signal-context dump before forwarding to crashpad) — the engine fault is the actual bug; (2) determine
  whether crashpad's `fputs` call goes through our `__sF` provision and fix that path if so. Other gaps unchanged
  (the §8-class teardown SIGSEGV, `Log.println_native` benign worker error, WorkManager non-main framing,
  `java.time` BootstrapMethodError, Firebase StreamCorruptedException). Gate: **511 unit + 4 integration + 2 doctests**
  (+5 signal-ABI), fmt/clippy `-D warnings`/release all 0-warning. Durability caveat unchanged (overlay output is
  cache, `eclipse run` still needs `ECLIPSE_ANDROID_FRAMEWORK_DIR`). Detail: §6 (2026-06-11 late signal-ABI).

---

## 6. Decisions Log  *(append-only, dated)*

> **2026-06-11 trim:** the granular session-by-session narrative from 2026-06-04/05 (the M0
> AUR-install path, the M1 launcher build-out, the engine-load tower, the 27 + 33 + 8 NDK
> tiers, the four engine-milestone capstones, the adversarial-robustness passes for loader
> and `apk`) is preserved in git history (HEAD `f886fcf`). Only the still-active
> foundational decisions and the recent (2026-06-11) decisions establishing today's state
> are kept inline.

### Foundational (still in force)

- **2026-06-04** — Priorities locked: **1) Stability 2) Purely-Rust 3) No-bloat/perf.**
- **2026-06-04** — **ART + libcore is unavoidable for Roblox** (custom Java Activity
  `com.roblox.client.ActivityNativeMain` tightly coupled to the native engine via JNI;
  full Java/Kotlin shell; apkenv-style fake-JVM only ran simple games; Sober/ATL ship full
  ART). Accepted: off the gameplay hot path, Apache-2.0, every line we own stays Rust.
  The fake-JVM "more-Rust" path is **closed**.
- **2026-06-04** — Two settled ecosystem facts: no pure-Rust dex VM exists (only toys); no
  pure-Rust audio on Linux exists (cpal still links ALSA-C). So ART is vendored and
  `libpulse-binding` is the audio purity ceiling.
- **2026-06-04** — Architecture = ATL approach (confirmed state-of-the-art 2026). Graphics
  forward via `ash`/`khronos-egl` + `winit` (not GTK4: Vulkan-incompatible + heavier).
  Allocator = system default (drop `mimalloc` unless profiled). Flatpak target =
  `org.freedesktop.Platform` (we don't use GTK, so GNOME runtime is needless weight).
- **2026-06-04** — bionic loader is the **#1 Rust-port priority** (`elf_loader`/`dlopen-rs`
  base); v1 may FFI the proven C `bionic_translation` for stability, then port behind an ABI
  conformance suite (do it **last**, not first — highest risk). *(Superseded in practice:
  the from-scratch Rust loader at `src/loader/{elf,reloc,map,resolve,tls,link}.rs` is now
  the in-use path; `bionic_translation` not linked.)*
- **2026-06-04** — Strategic/external risk (Roblox blocking, open-source detection) is **not
  a concern** — user has a Roblox-engineer relationship. No open technical levers remain.

### Recent (current technical state — 2026-06-11)

- **2026-06-11 — 🎉 THE #1 WALL IS CROSSED: ART `Runtime.nativeLoad` now CONSULTS Eclipse's pre-load registry;
  `System.loadLibrary("zstd-jni")` skips the apkenv linker → the boot advances PAST the SIGSEGV into Roblox's full
  content-provider init (Firebase/MLKit/OkHttp).** This is the documented `docs/libroblox-init-run.md` §10/§11
  "registry consult" — the engine-load wall the §9–§11 capstone reserved for main-loop / owner authorization. Done
  in the main loop with explicit owner authorization (NOT a subagent). **Root cause it fixes:** ART's
  `Runtime.nativeLoad(path, loader, caller)` → `art::JavaVMExt::LoadNativeLibrary` → `bionic_dlopen` (the apkenv shim
  linker), which ABORTS on the engine JNI libs' modern relocs / a NULL deref in its dependency-graph walk. Eclipse
  already PRE-LOADS those libs through its own Rust loader, but ART did not consult that → it re-loaded via apkenv →
  SIGSEGV. **Mechanism (root-cause, robust):** bind Eclipse's own `Runtime.nativeLoad` via `RegisterNatives` (best-
  effort, like Canvas — a libcore-signature mismatch logs + leaves ART's original in place). Per call: (1) derive
  the soname from the resolved path; if Eclipse pre-loaded it (`engine::is_preloaded`, the registry's public consult)
  → return `null` = SUCCESS, **apkenv never entered**; (2) else → **DELEGATE** to ART's REAL
  `JavaVMExt::LoadNativeLibrary` (exported `T` by libart.so, `dlsym`'d at runtime since libart is RTLD_GLOBAL) so
  every OTHER library loads through ART's normal path UNCHANGED — handle in `libraries_`, `JNI_OnLoad`, `Java_*`
  discovery. Delegation goes through a small **clean-room C++ shim** (`src/loader/native_load_shim.cpp`, compiled by
  `build.rs` via `cc` `.cpp(true)` — the established shim pattern) that builds the `std::string` args with the host
  libstdc++ (correct ABI by construction) and calls the dlsym'd member function as a free fn with explicit `this`
  (Itanium ABI for a non-virtual member; ART upcasts `JavaVM*`→`JavaVMExt*`, same address). The interception runs on
  whatever thread issues `loadLibrary` (Roblox loads zstd on its `AppStartupTaskM` thread) — all thread-safe (the
  registry is `Mutex`-guarded; delegation re-enters ART with the calling thread's `JNIEnv`, which ART locks). With no
  pre-loaded lib (pure-Java APK) it is a pure passthrough → no-op. **VALIDATED on the dev host (the user's
  v2.721.1108 APK, `/tmp/eclipse-fix.log`):** `registered Eclipse's Runtime.nativeLoad interception` → `Runtime.
  nativeLoad: already pre-loaded by Eclipse's Rust loader — reporting success (apkenv skipped)
  soname="libzstd-jni-1.5.7-6.so"`; the **`unknown reloc type 18` / apkenv `bionic_dlopen` SIGSEGV is GONE**;
  **`libwolfssljni` loads via the delegation** (`I/wolfssljni: Unable to set FIPS callback` — cert verification
  passes); and **Roblox's `Application.onCreate` runs its full startup** — `roblox.config setBaseUrl →
  www.roblox.com`, then `androidx.startup.InitializationProvider` creates `ShellConfigurationContentProvider` /
  `AppAssetsContentProvider` / `FileProvider` / **`MlKitInitProvider`** / **`FirebaseInitProvider`** + `OkHttp Cache
  initialized`. **NEW frontiers revealed (all NORMAL, post-wall):** (a) **the pre-loaded lazy-native discovery gap**
  — `com.github.luben.zstd.ZstdInputStreamNoFinalizer.recommendedDInSize()` `UnsatisfiedLinkError`: because Eclipse
  pre-loaded zstd through its OWN mmap loader (not a dlopen handle in ART's `libraries_`), ART's `Java_*` discovery
  can't dlsym zstd's natives (the predicted gap — its 148 `Java_*` need registering with ART, e.g. enumerate +
  `RegisterNatives` from the loaded image, or hand ART a usable handle); (b) **`AssetManager.getResourcePackageName(int)`**
  missing framework native (Firebase's `Resources.getResourcePackageName` — a benign discovery-loop bind; Eclipse's
  `arsc` already has `package_name`); (c) the trailing **exit-teardown SIGSEGV** is the §8-class artifact on the
  ERROR path (the lifecycle returns `FrameworkError::Jni` from the missing native, `main` tears down with the
  engine's background workers + finalizer threads still live → fault at exit; it does NOT occur once the lifecycle
  succeeds + enters the event loop). **Gate:** fmt --all --check / build --all-targets / clippy (-D warnings) / test
  (**487 unit + 4 integration + 2 doctests**, +3: `runtime_native_load_name_sig_and_class_match_art`,
  `soname_from_load_path_returns_the_basename`, `engine::is_preloaded_reflects_registration`) / release — all
  0-warning/0-error. **`unsafe`:** confined to the dlsym + the one FFI call to the shim (dated `// SAFETY:`); the
  shim has NO build-time libart dependency (the fn is a `void*` param); reloc/elf/resolve stay
  `#![forbid(unsafe_code)]`. **Cyber-safeguard:** this IS the gated native-load-interception region — done in the
  MAIN LOOP with explicit owner authorization (the sanctioned path; `docs/libroblox-init-run.md` §10/§11 "MAIN-LOOP
  ONLY ... human one-time edit"). Read ART's `art/runtime/jni/java_vm_ext.cc` (`LoadNativeLibrary` dedup-by-path +
  `bionic_dlopen` site) to confirm the delegation target + the already-loaded contract; the C++ shim is clean-room
  from the PUBLIC non-virtual member-function Itanium ABI + the public `nativeLoad`/`LoadNativeLibrary` signatures.
  NOT a subagent. Files: `src/framework.rs` (the `runtime_native_load` native + `register_runtime_native_load_natives`
  + the shim FFI decl + `art_load_native_library_fn`/`soname_from_load_path` helpers + 2 tests + wired into
  `drive_lifecycle` before step 1), `src/loader/native_load_shim.cpp` (new), `build.rs` (compile it), `src/loader/
  engine.rs` (`pub fn is_preloaded` + index the pre-load by file name + 1 test). **NEXT (post-wall discovery loop):**
  register the pre-loaded lazy-native libs' `Java_*` with ART (close the zstd discovery gap) — e.g. enumerate the
  loaded image's `Java_*` exports and `RegisterNatives` them, or give ART a dlsym-able handle; bind
  `AssetManager.getResourcePackageName(int)`; then continue driving `onCreate`. Honest status: **Roblox does not
  render yet** — but the engine-load apkenv wall (the #1 frontier since 2026-06-05) is DONE, and the boot now reaches
  Roblox's content-provider initialization. Full prior analysis: [`docs/libroblox-init-run.md`](docs/libroblox-init-run.md)
  §9–§11.
- **2026-06-11 (post-wall discovery loop → SQLite) — bound the resource + asset-stream framework natives Roblox's
  `Application.onCreate` needs; the lifecycle advanced step 2 → step 3 and now reads the full compression dictionary +
  creates ALL content providers, stopping at SQLite.** After the nativeLoad wall (entry above), the real
  v2.721.1108 run surfaced the next `android.*` natives in turn (each benign — bound non-GTK against Eclipse's own
  `apk`/`arsc`/`src/apk`, NOT ATL's GTK C; discovery-loop branch ii):
  (1) **`AssetManager.getResourcePackageName(int)`** — Firebase's `Resources.getResourcePackageName`; backed by
  `arsc::package_name` (the id's high-byte package). (2) **`AssetManager.getResourceIdentifier(String name, String
  defType, String defPackage)`** — Firebase's `Resources.getIdentifier` (the REVERSE of `getResourceName`); needed a
  NEW **`arsc::ResTable::find_resource_id`** (total, bounded reverse lookup name→id: scans the matching package's type
  chunks for the type name, then its entries for the key name, the entry-id loop bounded by the 16-bit entry space;
  `#![forbid(unsafe_code)]` preserved). With both, Firebase's `onCreate` cleared its resource lookups → **the
  lifecycle advanced step 2 (`createContentProviders`) → step 3 (`Application.onCreate`)**. (3) The **asset-stream
  subsystem** (Roblox reads its zstd compression dictionary from `assets/`): new
  **`src/framework/asset_registry.rs`** (`#![forbid(unsafe_code)]`, jlong = generational-slab index holding the
  asset's `Box<[u8]>` + a read cursor with `read`/`seek`/`len`/`remaining`, mirroring `xml_registry`; stale/
  fabricated handle → typed `Err`, never UB) + 6 natives `openAsset(Ljava/lang/String;I)J` (reads `assets/<name>`
  via the `src/apk` reader → a handle) and the read cycle `readAsset`/`seekAsset`/`getAssetLength`/
  `getAssetRemainingLength`/`destroyAsset`. **TWO real bugs found+fixed on the live run** (not in tests): (a) ATL
  passes `openAsset` the **FULL `assets/…` path** (not a stock-AOSP relative one) → `read_asset_bytes` was
  double-prefixing `assets/assets/…` → "not found"; fixed to accept both forms; (b) ATL's **`readAsset` signature is
  `(J[BJJ)I`** (long off/len — confirmed from the ART vtable dump `readAsset(long, byte[], long, long)`), NOT the
  classic `(J[BII)I`; fixed the descriptor + the native's off/len params. The read cycle is registered **PER-NATIVE
  best-effort** (`register_asset_stream_natives` — ATL declares only some; a grouped bind failed as a whole on the
  wrong-sig `readAsset`), and `readAsset` is **bounded to `b[]`'s real `len`** (`JPrimitiveArray::len`) so a
  `set_region` overflow can't leave a pending `ArrayIndexOutOfBounds` JNI exception (which ATL's `readAsset_internal`
  surfaced as `IOException` → step 3 fail). **REAL RESULT (v2.721.1108):** `Application.onCreate` reads the 1.2 MB
  `shared_compression_dictionaries/7390….dict` **fully** (148 × 8192-byte `readAsset`s, returned=8192 each) and
  creates ShellConfig/AppAssets/MlKit/Firebase/**MobileAds** content providers. **NEW frontier = SQLite:** step 3
  now stops at `android.database.sqlite.SQLiteConnection.nativeOpen(String,int,String,boolean,boolean)` (Roblox opens
  a DB during onCreate) — a LARGE subsystem (the full `SQLiteConnection`/`SQLiteStatement`/`CursorWindow` native
  surface, ~25 natives, needs a REAL SQLite backing; ATL's C SQLite natives aren't loaded since Eclipse doesn't load
  ATL's GTK-linked `libtranslation_layer_main.so`). The pre-loaded-lib `Java_*` discovery gap (`zstd
  recommendedDInSize`, on a background startup-task thread) + the exit-teardown SIGSEGV (§8 class, error path) remain.
  **Cyber-safeguard NOT tripped** (every native from the benign ART `No implementation found` line / vtable dump +
  Eclipse's own `apk`/`arsc`/`src/apk`; no ATL asset/Resources C source, no SQLite/linker source, no native-load
  region beyond the already-authorized nativeLoad). Gate clean: fmt/build/clippy `-D warnings`/test (**496 unit + 4
  integration + 2 doctests**, +9: 4 `asset_registry` + 1 `arsc::find_resource_id` round-trip + the resource/asset
  name-sig pins)/release all 0-warning. Files: `src/framework.rs` (the 8 natives + helpers + `register_asset_stream_natives`
  + tests + wiring), `src/framework/asset_registry.rs` (new), `src/apk/arsc.rs` (`find_resource_id` + test). **NEXT =
  the SQLite subsystem** (a real SQLite-backed `SQLiteConnection`/`CursorWindow` native surface), then the zstd
  `Java_*` registration, then the rest of `onCreate` → step 4 `Activity.createMainActivity` → the engine render path.

### 2026-06-11 — Evidence corrected the frontier: the MAIN-THREAD step-3 blocker was a missing Java FIELD (`Build.SUPPORTED_64_BIT_ABIS`), not SQLite — fixed via a patched ATL `api-impl.jar`; new frontier = the pre-loaded-lib `Java_*` discovery gap

A fresh live `eclipse run` (real v2.721.1108 APK, dev host, `/tmp/eclipse-sqlite-blocker.log`, EXIT=139) proved the
prior "frontier = SQLite" framing was incomplete. `onCreate` (step 3) actually aborted on the MAIN thread with
`java.lang.NoSuchFieldError: No static field SUPPORTED_64_BIT_ABIS of type [Ljava/lang/String; in class
Landroid/os/Build;` (at `com.roblox.client.RobloxApplication.onCreate(Unknown Source:307)`). The `SQLiteConnection.nativeOpen`
failure is REAL but on a **background WorkManager thread** (`androidx.work.impl.utils.ForceStopRunnable`) — it does NOT
abort the main-thread lifecycle, so SQLite alone would not advance `eclipse run`.

**Root cause (decisive evidence):** ATL's `api-impl.jar` `android.os.Build` declares `SUPPORTED_ABIS`/`CPU_ABI`/`CPU_ABI2`
but OMITS `SUPPORTED_64_BIT_ABIS` and `SUPPORTED_32_BIT_ABIS` (confirmed by `strings` on the extracted `classes.dex`).
This is the **first framework gap that is a missing Java FIELD, not a missing native method** — Eclipse's `RegisterNatives`
mechanism (which binds *methods*) CANNOT add a field, and ART cannot add a field to a loaded class. The only root-cause
fix is at the framework-jar level. Owner chose (AskUserQuestion) "fix Build first via a patched `api-impl.jar` pointed at
by `ECLIPSE_ANDROID_FRAMEWORK_DIR` (no root)".

**Fix (dev-host framework artifact, NOT Eclipse repo code — like vendored ART):** add the two AOSP-standard fields to
ATL's `src/api-impl/android/os/Build.java`
(`SUPPORTED_{32,64}_BIT_ABIS = SystemProperties.get("ro.product.cpu.abilist{32,64}", "x86"/"x86_64").split(",")` — reads
the property with a correct x86_64 fallback, since ATL's `SystemProperties` seeds abi/abi2/abilist but not abilist32/64).
Tooling available is only `javac`+`dx`+`jar` (no ant/smali/decompiler), so the mechanism is a **MULTIDEX jar**: compile the
patched `Build.java` against a compile-only `SystemProperties` stub, dex ONLY the `Build*` classes, and ship
`[classes.dex = patched Build (wins — "first dex wins" in ART's `DexPathList`) | classes2.dex = the original whole
api-impl]`. Reproducible script: `~/.cache/eclipse/build-field-patch/patch-build-field.sh` (env-overridable `ATL_SRC`/
`ORIG_FW`/`OUT`, no hardcoded user paths); output framework dir `~/.cache/eclipse/framework-patched/` (patched
`api-impl.jar` + symlinked `framework-res.apk`/`natives/`). The patched `Build.java` is Apache-2.0 (vendorable).

**VERIFIED result (re-run with `ECLIPSE_ANDROID_FRAMEWORK_DIR=…/framework-patched`, `/tmp/eclipse-buildpatch.log`):** the
`SUPPORTED_64_BIT_ABIS` `NoSuchFieldError` is GONE (0 occurrences) and `onCreate` advanced from source line 307 → **319**.

**NEW main-thread frontier (root cause, evidence-based) = the pre-loaded-lib `Java_*` discovery gap.** step 3 now aborts on
`UnsatisfiedLinkError: No implementation found for void com.roblox.client.JNIAAssetManagerSetup.initNative(android.content.res.AssetManager)`
(at `RobloxApplication.onCreate(319)`), and a parallel cascade of the SAME cause —
`com.roblox.universalapp.logging.JNILoggingProtocol.nativeGetTimestamp/nativeLogEvent`, `zstd…recommendedDInSize`. These are
all **libroblox `Java_*` natives ART cannot find**: Eclipse pre-loads libroblox via its own mmap loader (not an ART `dlopen`
handle in `libraries_`), so ART's lazy `dlsym(handle,"Java_…")` resolution has nothing to dlsym. **Fixing this one gap
unblocks a whole CLASS of natives at once** (far higher impact than SQLite). Recommended fix (in-scope, pure-Rust, main-loop):
after a pre-loaded lib's `JNI_OnLoad`, enumerate its exported `Java_*` symbols (Eclipse's `elf`/`resolve` already hold
name→addr), demangle to (class, method), reflect the Java class's declared native methods for the JNI signature, and
`RegisterNatives` each against the live ART VM. **Deterministic core landed this session** (gate-green): `src/loader/jni_mangle.rs`
reverses the JNI name-mangling (`Java_<class>_<method>[__sig]` → `(class, method)`, escape-aware `_1`/`_2`/`_3`/`_0XXXX`,
nested-class `$`, overload long form), pure + total (`#![forbid(unsafe_code)]`), 8 unit tests incl. the 3 real blocking
symbols — following the project's build-the-tested-core-first methodology (like `reloc.rs`/`elf.rs`). The remaining LIVE wiring
(enumerate exports → demangle → reflect the declared method for the full JNI signature → `register_native_methods` to the
export address) must be built carefully against the **redesigned `jni` 0.22.4 API** (`Env`/`NativeMethod`/`JNIStr`,
runtime-string signatures + reflection — study the crate first per CLAUDE.md) — main-loop only (native-load/ART region).
**SQLite remains a real (parallel, worker-thread) frontier** — tackle after the discovery gap. Gate: **504 unit + 4
integration + 2 doctests**, fmt/clippy `-D warnings`/release all 0-warning. Done MAIN-LOOP (dev-host); **not committed**
(owner's session instruction: no git commit/push unless asked).

### 2026-06-11 — The `Java_*` discovery gap CLOSED (general reflection RegisterNatives); lifecycle advanced step 3 → step 5

Building on the demangler core, the LIVE half landed and is VALIDATED. `src/loader/jni_register.rs::register_all_preloaded_natives`
(+ `engine::LoadedEngine::java_native_exports`, wired in `load_app_native_lib` after `JNI_OnLoad`) enumerates ALL of a
pre-loaded lib's exported `Java_*` symbols, [`jni_mangle::demangle`]s each to `(class, method)`, groups by class, reflects
each declaring class's declared `native` methods via JNI (`getDeclaredMethods` → per method `getModifiers`/`getName`/
`getReturnType`/`getParameterTypes` → build the JNI descriptor) for the full signature, and `register_native_methods` to
the export address (what ART's broken `dlsym` would have returned). Engineered against the **redesigned `jni` 0.22.4 API**
(`Env`/`NativeMethod::from_raw_parts` with runtime `JNIString`s, `cast_local`, `JObjectArray::len`/`get_element`,
`JString::mutf8_chars`); local references are scoped **per-class and per-method via `with_local_frame`** so reflecting
libroblox's ~499 natives never overflows the local-ref table. Every step is **best-effort** (class not loadable / no
matching reflected sig / JNI throw → clear + log + skip) — strictly additive, never regresses the boot. A small curated
table (`register_preloaded_natives`) is kept as a fallback used only if reflection binds nothing.

**REAL v2.721.1108 RESULT (`ECLIPSE_ANDROID_FRAMEWORK_DIR=…/framework-patched`, `/tmp/eclipse-bulk.log`, EXIT=139 teardown):**
`libroblox.so: bound 466 Java_* native(s) across 60 class(es)`, `libzstd-jni…: bound 144 … across 10 class(es)`. The
lifecycle advanced from the long-standing step-3 `Application.onCreate` wall **through step 3 ✅ → step 4
`Activity.createMainActivity` ✅ → step 5 `Activity.onCreate`**, i.e. Roblox's REAL launcher `com.roblox.client.startup.ActivitySplash.onCreate`
now runs. The next main-thread blocker is an **ATL framework METHOD gap** (the Build-field category, NOT a native):
`NoSuchMethodError android.net.NetworkRequest$Builder.addCapability(int)` (the `com.birbit.android.jobqueue` connectivity
monitor in `ActivitySplash.onCreate`) — fix by adding the method to ATL's `NetworkRequest` in the patched-`api-impl.jar`
overlay (extend `patch-build-field.sh`), exactly as the Build field. Remaining (parallel/worker or deeper): SQLite
`nativeOpen`, the `java.time.DateTimeFormatter` `BootstrapMethodError` (libcore invokedynamic), the Firebase measurement
`StreamCorruptedException` (Binder/Parcel). **Durability caveat:** the framework-jar patches apply only via
`ECLIPSE_ANDROID_FRAMEWORK_DIR` — a bare `./eclipse run` uses the stock `api-impl.jar` and the field/method gaps reappear;
an Eclipse-side auto-provision of the overlay (like the libm shim) is an open improvement. Files: `src/loader/jni_register.rs`
(+`register_all_preloaded_natives` + reflection helpers), `src/loader/jni_mangle.rs` (new), `src/loader/engine.rs`
(`resolve_export` + `java_native_exports` + wiring), `src/loader.rs` (2 mods). Gate clean: **504 unit + 4 integration + 2
doctests**, fmt/clippy `-D warnings`/release all 0-warning. Done MAIN-LOOP (dev-host); **not committed** (owner's session
instruction).

### 2026-06-11 — SQLite subsystem Phase A: `SQLiteConnection` natives (libsqlite3-backed) — DB open + statement lifecycle + executes WORK

**New dependency (policy-logged):** `libsqlite3-sys = { version = "0.38.1", features = ["bundled"] }` (+ `vcpkg`, a
Windows-only build helper — 2 new crates total). `bundled` compiles the vendored SQLite amalgamation via the `cc` crate
(already a build-dep) and links it statically: NO system `libsqlite3`, deterministic version, distro-portable
(detect-don't-assume §9). No pure-Rust SQLite is production-grade (stability §3.1), so a thin binding to the gold-standard C
engine is the accepted shape — the one new C black box, same rationale as cpal→ALSA / the `cc` shims. Eclipse binds the
natives against the RAW `libsqlite3-sys` FFI (the JNI contract IS a thin C-API surface; `sqlite3*`/`sqlite3_stmt*` round-trip
as the jlong handles), not the higher-level `rusqlite` (leaner: no rusqlite/smallvec/fallible-iterator). Recorded in
`Cargo.toml` (per-dep comment) + `docs/dependency-plan.md`.

**Module `src/framework/sqlite.rs`:** a generational-slab registry of the raw `sqlite3*`/`sqlite3_stmt*` (jlong = packed
slab index+generation, NOT a raw pointer — a stale/fabricated handle is a checked throw, never UB; raw ptrs wrapped in a
`SendPtr` whose `unsafe impl Send` is justified by SQLITE_THREADSAFE=1 + the registry Mutex) + **26 `SQLiteConnection`
natives** bound via `RegisterNatives` in `framework::sqlite::register_natives` (wired into `drive_application_lifecycle` after
`register_connectivity_natives`). UTF-8 SQLite entry points (`open_v2`/`prepare_v2`/`bind_text`/`column_text`) — functionally
identical to AOSP's UTF-16 ones, simpler. Each native is `with_env`-guarded (no panic across JNI) and throws
`android.database.sqlite.SQLiteException` (via `throw_new`) on any SQLite error. `nativeOpen` maps Android's `openFlags`
(`CREATE_IF_NECESSARY`/`OPEN_READONLY`) to SQLite flags exactly as AOSP does, sets a 2.5 s busy timeout, and registers the
`"LOCALIZED"`/`"UNICODE"` collations (a byte-lexicographic comparator via `sqlite3_create_collation_v2`) so
`SQLiteDatabase.setLocale`'s `REINDEX LOCALIZED` resolves (it threw "unable to identify the object to be reindexed" until the
collations existed). `nativeRegisterLocalizedCollators` re-registers them on the connection. The rarely-hit
`nativeExecuteForBlobFileDescriptor` returns -1 (no ashmem fd path); `nativeRegisterCustomFunction`/`nativeCancel`/
`nativeResetCancel`/`nativeGetDbLookaside` are sound no-ops/neutral values.

**VALIDATED (real v2.721.1108, `ECLIPSE_ANDROID_FRAMEWORK_DIR=…/framework-patched`):** `nativeOpen` succeeds —
`androidx.work.workdb` and `db_default_job_manager` open, PRAGMAs / prepared statements / non-cursor executes run, and
`setLocale` succeeds (0 `REINDEX LOCALIZED` errors). The lifecycle stays at step 5 (`ActivitySplash.onCreate`) on the **next
blocker = `nativeExecuteForCursorWindow`** (the first row-returning SELECT). **Phase B note (do this next):** ATL declares it
`nativeExecuteForCursorWindow(long conn, long stmt, android.database.CursorWindow window, int startPos, int requiredPos,
boolean countAllRows) → long` — the window is a **Java object, NOT a `long` windowPtr** (ATL's `CursorWindow` is pure-Java,
no native buffer). Implement it by looping `sqlite3_step` and FILLING the Java window via JNI callbacks
(`window.clear()`/`setNumColumns(n)`/`allocRow()`/`putLong`/`putString`/`putDouble`/`putNull`/`putBlob(value,row,col)` — verify
the exact method names/sigs in ATL's `api-impl/android/database/CursorWindow.java`), then return
`(jlong(startPos) << 32) | totalRows`. No `#[repr(C,packed)]` FieldSlot buffer is needed. Gate: **504 unit + 4 integration +
2 doctests**, all 0-warning. Done MAIN-LOOP (dev-host); **not committed** (owner's session instruction).

### 2026-06-11 — SQLite Phase B (`nativeExecuteForCursorWindow`) done → `Application.onCreate` COMPLETES; + opt-in APK auto-fetch

**Phase B implemented** exactly as the prior entry predicted: `native_execute_for_cursor_window` in `src/framework/sqlite.rs`
loops `sqlite3_step` and fills ATL's **pure-Java** `CursorWindow` (`ArrayList<Object[]>`) via JNI callbacks — `clear` /
`setNumColumns(colCount)` / `setStartPosition(startPos)` / per row `allocRow()` + `put{Long,Double,String,Blob,Null}(value,
ABSROW, col)` (ATL's `putX` does `row - startPos` internally; row is ABSOLUTE), per-row `with_local_frame` to free transient
`JString`/`JByteArray` refs — returns `(startPos<<32)|totalRows`. Registered (24 natives now). **REAL boot result
(`/tmp/eclipse-cursor.log`):** the full `SQLiteOpenHelper.getWritableDatabase` path runs — `nativeOpen` opens
`androidx.work.workdb` + `db_default_job_manager`, PRAGMAs/binds/executes/cursor-queries succeed, `setLocale`'s
`REINDEX LOCALIZED` resolves (collations registered) — and **`Application.onCreate` completes with NO crash** ("recipe steps
1–3 driven"). The process then ends via **`System.exit(10)` after `RobloxApplication.onCreate:411` logs "Background process
detected"** — Roblox's OWN main-vs-background process self-check failing (Eclipse's process isn't seen as the main
`com.roblox.client` process). NEXT: make ATL's `Application.getProcessName()` (→ `ActivityThread.currentProcessName()`) report
`com.roblox.client`, then drive step 4 onward. (`PowerManager.isDeviceIdleMode()` etc. are worker-thread gaps.)

**Opt-in APK auto-fetch (owner-requested):** `src/apk/fetch.rs` + config (`apk_url`/`apk_sha256`/`auto_fetch_missing`) + CLI
(`eclipse fetch`; `run` auto-fetches when no APK + configured). Deps: `ureq` 3.3 (`default-features=false` + `rustls`) → pure-Rust
TLS (no OpenSSL), blocking (no async); SHA-256 via `sha2`. `latest_roblox_version()` hits the OFFICIAL `clientsettings`
WindowsPlayer oracle (Android has NO official APK endpoint — Google-Play-only; verified live `0.725.0.7251138` ≈ Android
2.725.x). `fetch_apk(url, sha)` streams to the XDG cache (`ECLIPSE_APK_CACHE_DIR` override), SHA-256-verifies, crash-safe
`.partial`→rename, idempotent. **POLICY (deliberate, Sober precedent): Eclipse NEVER hosts or hard-codes a Roblox source** —
auto-fetch is opt-in, from a URL the USER configures; the bright line is no redistribution (README + this file updated).
Research recorded mirror options (APKPure `d.apkpure.com/b/XAPK/com.roblox.client?versionCode=…`, APKMirror UA
`APKUpdater-v3.0.3`) but they are Cloudflare-fragile/ToS-gray, so deliberately NOT hard-coded; the user points `apk_url` at
their chosen source. Follow-ups: optional mirror auto-resolve, XAPK-split→merged-APK assembly, APK signing-cert pinning
(the load-bearing trust control). Gate: **506 unit + 4 integration + 2 doctests** (+2 `fetch`), all 0-warning. Done MAIN-LOOP
(dev-host); **not committed** (owner's session instruction). *(Committed next session: `13de7ec` + `f886fcf`.)*

### 2026-06-11 (late) — Bionic signal ABI in `native_provider` (6 translating natives); crashpad's first-chance handler now actually RUNS on the real Roblox boot

**Root cause confirmed before fixing (CLAUDE.md "Root-Cause Diagnosis"):** the prior session's framing — engine
`sigaction` falls through to host glibc with an incompatible struct layout — was **PROVEN** with two pieces of
evidence before any code change. (1) **Static side, `readelf --dyn-syms -W`:** `libroblox.so` imports
`sigaction`/`sigemptyset`/`sigaddset`/`sigfillset`/`pthread_sigmask` UND from `LIBC` (and `libbacktrace-native.so` adds
`sigprocmask`/`sigaltstack`) — none of these were registered in the Eclipse provider, so all six resolved to host
glibc. (2) **Dynamic side, `coredumpctl info 455287` + gdb:** the kernel-invoked handler from the prior crash dump
was `0x00007fbc_08000804`. The low 32 bits `0x08000804` decompose exactly as `SA_ONSTACK (0x08000000) |
SA_EXPOSE_TAGBITS (0x800) | SA_SIGINFO (0x4)` — crashpad's first-chance flags. That means glibc read its `sa_handler`
from offset 0 of the caller's bionic struct, which is where bionic keeps `sa_flags`. ABI confirmed (AOSP
`bits/signal_types.h` LP64): bionic `struct sigaction = { int sa_flags; union handler; sigset_t sa_mask;
void(*sa_restorer)(); }` (32 bytes: flags@0, handler@8, mask@16, restorer@24); glibc x86-64 is
`{ union handler; 128-byte sa_mask; int sa_flags; sa_restorer }` (152 bytes: handler@0, mask@8, flags@136). The
mask size disagreement is also a real corruption source — glibc's `sigfillset` and a `*_sigmask(oldset)` would
WRITE 128 bytes through an 8-byte bionic set. Decisive evidence in hand, the fix moved to implementation.

**Mechanism (the fix, pure-Rust, AGENTS.md §2.1 + CLAUDE.md "Compatibility Requirements"):** new section in
`src/loader/native_provider.rs` between `eclipse_system_property_get` and the `__stack_chk_guard`/`__sF` data
objects. A `#[repr(C)] BionicSigaction` + `type BionicSigsetT = u64` pin the bionic LP64 layout (pin-tested below);
`glibc_sigset_from_bionic`/`bionic_sigset_from_glibc` widen/narrow the kernel's first sigset word (the kernel's
`rt_sigaction`/`rt_sigprocmask` actually consume 8 bytes — signals 1–64 — and bionic represents exactly that, so the
remaining 120 bytes of glibc's set are always 0 in practice). The six translating natives:
`eclipse_sigaction` builds a glibc-shaped `sigaction` from the bionic input, forwards to glibc, translates `oldact`
back; `eclipse_sigemptyset`/`eclipse_sigfillset` write EXACTLY one 8-byte word (an EINVAL on null); `eclipse_sigaddset`
bounds `signum` to 1..=64 with EINVAL outside; `eclipse_sigprocmask`/`eclipse_pthread_sigmask` widen `set`, forward,
narrow `oldset`. `SA_RESTORER` is stripped from both directions on the wire (glibc supplies its own restorer; leaking
that pointer to a bionic-ABI caller that later re-registers it would be unsound). `sigaltstack` stays on the host
baseline — bionic and glibc `stack_t` are layout-identical on x86-64 (verified: `{ void* ss_sp; int ss_flags;
size_t ss_size }` in both). Registered in `with_bionic_natives` (provider count 88 → 94 base, 144 → 150 total);
the presence-list test gains the 6 new names; the count assertion + its breakdown comment are updated; the stale
"`pthread_sigmask` is ABI-identical, stays on host" comment in `bionic_pthread.rs` `PTHREAD_NATIVE_COUNT` is
corrected with a back-reference to this section (the count is unchanged because the natives live in
`native_provider`, not `bionic_pthread`).

**Regression guards (CLAUDE.md "Regression Protection" — the smallest reliable checks):** **5 new tests** in the
existing `native_provider::tests` module, in the established style (no new file, no script). (1)
`bionic_sigaction_layout_matches_lp64` pins the struct offsets/size + the sigset width disagreement — a future
refactor that broke the layout would re-introduce the scramble; this test catches it before the live boot does.
(2) `bionic_sigset_ops_match_the_bionic_contract` verifies sigempty/fill/add exactly clear/fill the one word and
that out-of-range signums + null sets return EINVAL. (3) `bionic_sigset_translation_round_trips` widens a multi-bit
bionic set, cross-checks with glibc's `sigismember` (proves the widened set actually represents the right bits to
the kernel), then narrows back losslessly. (4) `bionic_sigaction_registers_a_live_handler_and_round_trips_oldact`
is the smallest end-to-end check that would have failed on the prior fall-through: register a `SA_SIGINFO` handler
through the bionic path, `raise(SIGURG)` (its default disposition is IGNORE so a broken registration cannot kill
the test process), and assert the kernel actually delivered the signal to our handler — then restore the saved
`oldact` and re-query it, proving the chain-to-previous pattern (which crashpad uses for first-chance handlers)
round-trips. (5) `bionic_sigprocmask_translates_both_directions` blocks a signal through the bionic path and
queries the resulting host thread mask through glibc directly to cross-check the translation against the kernel's
view. All 5 pass; existing tests unchanged.

**Live-boot evidence the fix works (`/tmp/eclipse-sig{1,2}.log`):** lifecycle still drives all the way through
`Activity resumed: recipe steps 1–7 driven (launcher Activity = com.roblox.client.startup.ActivitySplash)` on the
real v2.721.1108 APK; engine native startup runs and emits the same `FLog::CrashReportLog` lines as before
(`Roblox files folder` / `cache folder` / `initialized crashpad, plug in the sfirst chance exception handler`).
Then the engine SIGSEGVs at the same site as the prior dump — and the **NEW** log line appears:
`FLog::CrashReportLog Run book keeping for signal 11`. That line is the first thing Roblox's `CrashReporter::HandleSignal`
emits on entry from its signal handler. Before this fix, it was IMPOSSIBLE to reach (the kernel jumped to a
bionic-`sa_flags`-value-read-as-pointer, double-faulted, and killed the process with no log). That this line now
appears is direct positive proof the bionic-ABI fix delivered the signal correctly to crashpad's registered handler.

**NEW frontier (HONEST: the original engine fault is still there; a SECOND ABI gap now hides behind crashpad's
own logging path; coredump 482294 + gdb):** the process dies inside crashpad's signal handler at
`fputs` (glibc + 0x93fc5) ← `libroblox + 0x278fe42` ← `libroblox + 0x278fc8e` ← `libroblox + 0x2792fe5` ←
`libroblox + 0x2792930` ← signal trampoline. Register state at the fault: `rdi = 0xff` (the `fputs` `s` argument
— invalid pointer; `gdb: Cannot access memory at address 0xff`). This is a different failure mode than the prior
double-fault delivery: crashpad's handler IS running, but its first formatted message tries to `fputs` a string
whose pointer is `0xff`. Hypothesis (UNVERIFIED — to be confirmed BEFORE fixing, per CLAUDE.md): either (a) crashpad
uses `stdin`/`stdout`/`stderr` via the bionic `__sF` macro and our `__sF` table layout disagrees with crashpad's
expected indexing (bionic FILE struct is small, so `&__sF[1]` is `__sF_base + sizeof(struct __sFILE)` — our table
is an array of 3 `*const FILE` pointers, so the same expression yields the wrong byte offset); or (b) the original
engine bug corrupts a TLS/global that crashpad's logger reads, and the `0xff` is downstream of that. Diagnostic
plan: install an **early-fault tap** (a minimal `SA_SIGINFO` handler registered through `eclipse_sigaction` BEFORE
the engine's first call — it logs `siginfo_t.si_signo`/`si_code`/`si_addr` and a bounded stack walk, then either
chains to the previously-saved handler or `abort()`s with the dump). That isolates the engine fault from the
crashpad logging path so we can root-cause them independently.

**Cyber-safeguard NOT tripped:** the bionic signal ABI is public AOSP NDK header material (`sys/signal.h`,
`bits/signal_types.h`) + a public-ABI translation to glibc; no AOSP/ATL/linker source modification. Done MAIN-LOOP
on the dev host. **Gate:** fmt --all --check / build --all-targets / clippy `-D warnings` /
test (**511 unit + 4 integration + 2 doctests**, +5: the signal-ABI tests above) / release — all 0-warning/0-error.
**`unsafe`:** confined to the C-ABI native bodies and the kernel-side `sigaction`/`sigprocmask`/`pthread_sigmask`
forwards (each `// SAFETY:`-dated); the translation helpers operate over `&` and `&mut`; `reloc.rs`/`elf.rs`/
`resolve.rs` stay `#![forbid(unsafe_code)]`. Files: `src/loader/native_provider.rs` (the new section + the 5 tests
+ the count/list edits), `src/loader/bionic_pthread.rs` (the stale-comment correction). **NOT committed** — owner
explicitly held all post-2026-06-11-morning work uncommitted; bionic-signal sits on top of the in-repo
framework-overlay work that was just committed in `29d8dcd`/`8beef79`/`f9c7ef4` and pushed.

### 2026-06-11 (evening) — Two prior framings corrected by evidence; lifecycle 1–7 MILESTONE on real Roblox; new frontier = the bionic signal ABI

**Context:** `~/.cache/eclipse` had been WIPED between sessions — the patched-framework overlay AND its out-of-tree build
script were gone (exactly the durability risk §5 carried). Root-cause durability fix: the patch tooling now lives
**in-repo at `tools/framework-overlay/`** (`patch-framework.sh` + committed patched ATL sources + compile-only stubs +
README). `Build.java` is GENERATED from the vendored ATL source by anchor-insert (drift-guarded: the script fails if the
`SUPPORTED_ABIS` anchor count ≠ 1); `NetworkRequest`/`ActivityManager`/`PowerManager` are committed patched copies (verbatim
ATL Apache-2.0 + `ECLIPSE PATCH` markers). Mechanism unchanged: multidex first-dex-wins (`classes.dex` = patched classes,
`classes2.dex` = stock api-impl). Tools: env-overridable `JAVAC`/`JAR` (repo `vendor/toolchain/jdk-*` first), `DX`,
`ATL_SRC`/`ORIG_FW`/`OUT`; actionable failure when missing; output stays a cache artifact.

**Framing correction #1 — the "Background process detected" mechanism (dex evidence, androguard over `classes2.dex`):**
`RobloxApplication.onCreate` computes `v5 = yj.s.a(ctx).equals("foreground")`; `yj.s.b(ctx)` scans
`ActivityManager.getRunningAppProcesses()` for an entry with `importance == 100` (IMPORTANCE_FOREGROUND) whose
`pkgList` contains the package name. It does NOT call `Application.getProcessName()` (the prior frontier hypothesis — ATL's
already returns the package name). ATL's `RunningAppProcessInfo` left `importance` = 0 and had NO `pkgList` field → scan
matches nothing → "background". **Fix:** overlay-patched `ActivityManager` (importance=IMPORTANCE_FOREGROUND=100,
`pkgList=[pkg]`, AOSP no-arg ctor). **VERIFIED:** the log line is GONE.

**Framing correction #2 — the `System.exit(10)` source:** NO dex code exits 10 (xref sweep: `restartApp` exits 0, the
Kotlin crash handler exits 1). The exit comes from **ATL's vendored-libcore `Thread.java`
`hacky_uncaught_exception_handler`** — the DEFAULT uncaught-exception handler, `System.exit(10)` on ANY thread's uncaught
exception (mirrors AOSP's KillApplicationHandler). So the prior "worker-thread gaps are non-blocking the main thread"
framing is FALSE — every uncaught worker exception is process-fatal. The actual killer that run:
`NoSuchMethodError PowerManager.isDeviceIdleMode()` (jobqueue's job-manager thread). **Fix:** overlay-patched
`PowerManager` (`isDeviceIdleMode() → false`; a desktop host is never in Doze). **VERIFIED:** exit-10 GONE.

**Discovery loop (in-binary RegisterNatives, each from the ART No-implementation-found line):**
`View.nativeSetFullscreen (JZ)V` (instance; `setSystemUiVisibility` from `ActivitySplash.onCreate` — no system bars on the
host window → validate+no-op), `TextView.native_setTextColor (I)V` (instance, NO widget param — ATL reads `this.widget`;
splash `LoadingBar`→`AppCompatTextView` inflation; renderer uses fixed `TEXT_COLOR` → validate+no-op, follow-up like
visibility/alpha), `Path.native_reset (JJ)V` (static; `Path.reset()` per splash-spinner animation frame; frees BOTH
path_registry slots, `0` = absent, stale handle logged+ignored). Name-sig pin asserts added to the existing View/TextView/
Path pin tests (the established regression guard for transcription errors).

**MILESTONE (run `/tmp/eclipse-fg4.log`, then deeper `/tmp/eclipse-fg5.log`):** `Activity.onCreate reached: recipe steps
1–5` → `Activity resumed: recipe steps 1–7 driven` on `com.roblox.client.startup.ActivitySplash` — the FIRST full
CREATED→RESUMED lifecycle on the real APK. Host winit window + Vulkan swapchain active (fg4 sat stable 170s). fg5 went
deeper: `ActivitySplash.d1 startup` → `InitHelper.getAllAppSettings` → **the ENGINE's native startup runs**
(`rbx.JNIRobloxSettings` sets the Guac policy file, FLog reports files/cache folders, **crashpad initializes**).

**NEW frontier (core-dump evidence, `coredumpctl info/dump 455287` + gdb):** immediately after crashpad's "plug in the
first chance exception handler", a SIGSEGV is raised INSIDE `libroblox.so` (frames ≈ base+0x1f28db0/+0x2334xxx, main
thread) and the kernel-invoked signal handler address is GARBAGE/unmapped (`#0 0x7fbc08000804`, `#1 <signal handler
called>`, `rdi=0xb`=SIGSEGV) → the handler delivery itself faults → process death (EXIT=139), no crash report. Suspected
root cause (to be CONFIRMED next session before fixing): engine `sigaction` calls fall through Eclipse's native provider
to HOST GLIBC, whose `struct sigaction` layout differs from bionic LP64 (glibc: handler@0, 128-byte mask@8, flags@136;
bionic: flags@0, handler@8, 8-byte mask@16, restorer@24) → scrambled handler registration. Work item: a bionic-correct
signal surface in `native_provider` (`sigaction`/`sigaction64`, `sigprocmask`, `sigaltstack`; coordinate with ART's
sigchain + Eclipse's own `init_run.rs` crash hook), then identify the FIRST fault's cause (could be crashpad probing or a
real engine-init failure the broken handler hid). Remaining non-fatal: `Log.println_native` (caught, early), WorkManager
"non-main process", `java.time` BootstrapMethodError, Firebase StreamCorruptedException.

**Gate:** fmt/build/clippy `-D warnings`/test (**506 unit + 4 integration + 2 doctests**)/release — all 0-warning.
**Cyber-safeguard:** natives from benign ART error lines + ATL Java sources only; the signal-ABI analysis is core-dump
forensics of Eclipse's OWN process (no linker/ART source modification); done MAIN-LOOP on the dev host.

### 2026-06-12 — Review fix: the early-fault tap's chain slot is now alloc-free (static claim-once pool — no heap in handler context)

Blocking review finding on the (uncommitted) early-fault-tap work: `tap_chain_register` published chain-slot values via
`Box::leak(Box::new(..))` — but that seam is reachable INSIDE the fault-handler chain (crashpad's documented not-handled
flow, `Signals::RestoreHandlerAndReraiseSignalOnReturn`, re-registers the saved previous action via `sigaction` from
WITHIN its handler → engine PLT → `eclipse_sigaction` → the tapped-signal seam), so the publish could `malloc` while the
interrupted context is arbitrary engine code — possibly mid-`malloc`; glibc's arena lock is not reentrant → deadlock or
allocator corruption instead of dump+death, in exactly the restore-and-reraise flow the tap exists to diagnose. **Fix
(`src/loader/native_provider.rs`):** `TAP_CHAIN` is now backed by `TAP_CHAIN_POOL` — 8 static claim-once
`UnsafeCell<BionicSigaction>` cells claimed by a grow-only `AtomicUsize` cursor, each fully written BEFORE being
published through the existing `AtomicPtr` (Release) — preserving the never-free/no-tearing property with zero
allocation; on exhaustion (unreachable in the real 3-claim flow: install seed + crashpad register + crashpad restore)
the slot keeps its last occupant with one async-signal-safe `write(2)` note. `install_early_fault_tap`'s seed uses the
same pool — NO `Box::leak` remains anywhere in the signal path (audited; the file's remaining `Box::new` are test
fixtures; `bionic_pthread.rs`'s lazy mutex/once `Box::leak` init is a separate, pre-existing, unbounded-count pattern —
noted, not changed here). **Regression guards:** new
`tap_chain_pool_publishes_in_place_and_keeps_last_occupant_on_exhaustion` (a LOCAL pool/cursor/slot triple via the
`tap_chain_publish` parametrization, so exhaustion testing cannot poison the process-global pool) + an in-pool pointer
assertion on the REAL statics inside `early_fault_tap_intercepts_registration_and_chains` (a reintroduced heap publish
fails it). Gate: **515 unit + 4 integration + 2 doctests**, fmt/clippy `-D warnings`/release all 0-warning. NOT
committed (owner hold on post-2026-06-11-morning work).

### 2026-06-12 — Review fix: the early-fault tap's re-entry latch is now tid-scoped (a cross-thread fault is concurrency, not recursion)

Second blocking review finding on the (uncommitted) early-fault-tap work: `TAP_IN_HANDLER` was a process-global
`AtomicBool`, held from handler entry until AFTER the chained handler returned. The tap is kernel-first for EVERY
delivery of the tapped signal (the engine-PC filter gates only the dump), and SIGSEGV is blocked only on the handling
thread — so a second thread faulting in that window (routine on x86-64: ART delivers managed NPE/StackOverflow fixups
via SIGSEGV) saw `swap()==true`, was misclassified as re-entry, and took the bail path: `tap_restore_default` installed
SIG_DFL PROCESS-WIDE and the re-executed fault killed the process — two overlapping recoverable faults became spurious
whole-process death with the tap+crashpad chain stripped from the kernel slot. **Fix (`src/loader/native_provider.rs`):**
`TAP_HANDLER_TID` (`AtomicI64`, owner tid via raw `SYS_gettid` — async-signal-safe, glibc-version-independent) +
`tap_entry_claim(latch, tid)` (one CAS, pure over the latch): CAS 0→tid = `Latched` (release on exit); failure with
owner==tid = `SameThreadReentry` (the existing SIG_DFL bail — recursive tap fault / sigchain re-front cycle, preserved);
failure with owner!=tid = `Unlatched` → PROCEED without the latch (all per-fault handler state is stack-local,
`TAP_CHAIN` reads are Acquire loads of immutable cells, the dump is one `write(2)` — at worst two dumps interleave on
fd 2; never release the owner's claim). **Regression guards:** `tap_entry_claim_is_tid_scoped_not_process_global`
(local-latch pure-fn pins: different-tid proceeds, same-tid bails, Unlatched never disturbs the owner) + a new
cross-thread phase (f) in `early_fault_tap_intercepts_registration_and_chains` (a second thread parks INSIDE the
chained handler holding the latch; a delivery on the test thread must still chain and the kernel slot must stay the
tap — verified to FAIL under the old global-bool semantics: entries 1≠2). Same-pattern audit: `init_run.rs::crash_handler`
has no latch (it `_exit`s, never returns — not the pattern); no other signal-handler latch in `src/`. Gate: **516 unit
(+1) + 2 integration (GL SKIP path, displays unset) + 2 doctests** clean, fmt/clippy `-D warnings`/release 0-warning
(the 2 live-boot integration tests excluded per the subagent live-boot rule — dev-host main-loop validation covers
them). NOT committed (owner hold on post-2026-06-11-morning work).

### 2026-06-12 — Early-fault tap IMPLEMENTED (the base entry the two review-fix entries above build on; appended at close-out, §6 is append-only)

**What it is:** the diagnostic the 2026-06-11 (late) entry planned as NEXT item (1): a minimal, Eclipse-owned,
**kernel-first** `SA_SIGINFO|SA_ONSTACK` SIGSEGV handler that dumps the ORIGINAL engine fault's verbatim signal
context BEFORE crashpad's handler runs, then chains to whatever crashpad registered — isolating the engine's
`libroblox+0x1f28xxx` fault from crashpad's own crashing `fputs` logging path so the two can be root-caused
independently. Lives entirely in the bionic signal-ABI section of `src/loader/native_provider.rs`: statics
`TAPPED_SIGNAL` (AtomicI32, doubles as the seam gate), `TAP_CHAIN` (AtomicPtr<BionicSigaction> over the claim-once
pool), `TAP_IN_HANDLER`/`TAP_HANDLER_TID` (re-entry latch), `ENGINE_RANGE_BASE/SPAN`; handler stack:
`tap_restore_default` (raw SIG_DFL reinstall), `tap_read_u64` (`SYS_process_vm_readv` self-probe, `Some` only on
ret==8), `tap_stack_walk` (RIP then RBP frame chain — accepts iff fp≠0, 8-aligned, fp>rsp, next>fp, step<1 MiB;
32-entry cap; standalone-testable), `tap_write_addr` (hex + `(libroblox+0x…)` annotation), and
`early_fault_tap_handler` (errno save/restore, null-checked siginfo/ucontext reads, engine-PC filter —
dump-everything until the range is published, the detect-don't-assume choice — fixed 2048-byte buffer formatted
with the `init_run` `write_bytes`/`write_dec`/`write_hex` helpers promoted to `pub(super)`, ONE raw `write(2)`,
zero panic/alloc/stdio/lock paths, dated `// SAFETY:` on every unsafe block). Local `SEGV_MAPERR=1`/`SEGV_ACCERR=2`
consts pin kernel UAPI (absent from libc 0.2.186 for linux-gnu — verified in the pinned registry source).

**Where it registers and why that is provably early enough:** `install_early_fault_tap(SIGSEGV)` runs under a
`std::sync::Once` as the **FIRST statement of `engine::load_app_native_lib`** (install failure logs a warning and
never aborts the boot) — i.e. AFTER ART is up (ART's sigchain installed first; the tap fronts it) and BEFORE
`map_resolve_app_lib`/`run_init_array`, so NO engine instruction — constructor or later — can execute before the
tap holds the kernel slot. Immediately after the libroblox map resolves (before constructors),
`publish_engine_text_range(base, span)` arms the engine-PC filter.

**Chaining design (the tap stays kernel-first by construction):** a seam at the top of `eclipse_sigaction`
Acquire-loads `TAPPED_SIGNAL`; the tapped signal routes to `tap_chain_register` — oldact ← the current slot
occupant (or zeroed SIG_DFL), act → a restorer-stripped copy into a claim-once pool cell, Release-published;
returns 0, so crashpad's registration "succeeds" while the kernel slot never changes. Every other signal is
byte-identical forward-to-glibc. The factored `bionic_action_from_glibc` helper is the ONLY glibc→bionic action
translation (oldact path + install seed share it — they cannot drift). Handler chain dispatch: null→SIG_DFL;
SIG_DFL→reinstall+return (the re-executed fault preserves the original siginfo); SIG_IGN→return; else 3-arg
`sa_sigaction` iff the slot carries SA_SIGINFO, else 1-arg `sa_handler`. Deliberately NOT flag/mask-exact (the
slot's `sa_mask` is not applied around the chained call; SA_RESETHAND/SA_NODEFER not emulated) — immaterial for
crashpad's proven flags, documented in the handler. Live test evidence: a kernel-delivered SIGWINCH produced the
real dump then chained to the test handler — kernel→tap→chain proven end-to-end.

**Regression guards (3 new tests, in the existing `native_provider::tests` module; the 2 review-fix tests above
add 2 more):** `early_fault_tap_intercepts_registration_and_chains` (live SIGWINCH end-to-end: kernel slot IS the
tap, the seam round-trips oldact with `sa_restorer==0`, `raise` → kernel→tap→chained handler with the dump on test
stderr, crashpad-style restore reverts the slot, full cleanup restores the raw pre-test state),
`tap_stack_walk_bounds_and_validates` (synthetic frame chains: termination, alignment/ordering/1-MiB-step
rejection, the 32-cap; live `SYS_process_vm_readv` mapped/unmapped probe), `tap_si_code_consts_match_kernel_uapi`
(pins SEGV_MAPERR/ACCERR + the `bionic_action_from_glibc` anti-drift contract: SA_RESTORER stripped, handler/mask
carried, restorer forced to 0). Companion edits: `src/loader/engine.rs` (the Once install + the
`publish_engine_text_range` call) and `src/loader/init_run.rs` (the three write helpers promoted `pub(super)`,
visibility-only). No new provider symbol — the 150-count registration test is unchanged.

**Gate (close-out re-verified 2026-06-12, displays/APK env unset so `tests/engine_milestones.rs` took its
documented self-skip path):** fmt --all / build --all-targets / clippy `-D warnings` / test / release — **516 unit
+ 4 integration (self-skip) + 2 doctests = 522 passed, 0 failed**, all 0-warning. (The base tap work landed at 514
unit; the two review fixes above added 2.) **NOT committed — owner hold on all post-2026-06-11-morning work; the
tree stays dirty on purpose.** Live validation is the OWNER's next step (dev-host main loop — see §5).

**Carried review notes (non-blocking, recorded not acted on):** (a) MINOR seed-window:
`install_early_fault_tap` installs the tap kernel-first BEFORE seeding `TAP_CHAIN`; in that sub-microsecond
once-per-boot window a concurrent tapped-signal delivery on another thread would see a null chain → SIG_DFL →
spurious death of a recoverable fault. Fix shape if ever needed: query current action → seed → install (→ optional
re-seed from the returned oldact). (b) Dump-everything noise window: the tap installs before the multi-second
libroblox map, so routine ART SIGSEGV fixups in that window dump in full; if the noise pollutes the evidence log,
move the Once to just before `publish_engine_text_range` (still before `run_init_array`). (c)
`publish_engine_text_range` publishes the FULL mapped image span (map.rs `span()`), not just PF_X text — name
overstates precision, harmless over-inclusion for an RIP filter. (d) The 2048-byte buffer can truncate the deepest
frames in the 16-hex-digit worst case (~2.1 KB); bounded loss only (header/registers never lost) — bump to 4096 if
guaranteed-complete 32-frame dumps are wanted. (e) Test phase (f) asserts `entries_while_parked == 2`; a real
tty-resize SIGWINCH during an interactive run could flake it — relax to `>= 2` if ever observed. (f) Pool
exhaustion in `tap_chain_register` returns 0 with the slot keeping its last occupant (one `write(2)` note;
unreachable in the real ~3-claim flow). (g) A SIG_IGN chain occupant for a synchronous fault would loop
dump+re-execute (mirrors kernel semantics; unreachable with crashpad — treat SIG_IGN like SIG_DFL for fault-class
signals if ever hit). (h) bionic also exports `sigaction64`/`sigprocmask64`; libroblox/libbacktrace import only
the six provided names today — route any future imports to the same translating natives (identical LP64 layout on
x86-64) and extend the presence-list test. (i) The handler comment justifying the skipped `sa_mask` emulation
cites crashpad's flags word, which does not encode the mask — reword to own the tradeoff directly when next
touching the file. (j) `eclipse_sigaction`'s doc header still describes the pure forward-to-glibc contract; the
tapped-signal seam exception is documented only in the dated body comment — optionally add one doc line.

### 2026-06-12 — Review fix: carried note (a) CLOSED — the early-fault tap seeds the chain slot BEFORE the kernel install (seed-window race eliminated)

Third review fix on the (uncommitted) early-fault-tap work: `install_early_fault_tap` registered the tap kernel-first
BEFORE seeding `TAP_CHAIN` — in that once-per-boot sub-microsecond window a tapped-signal delivery on another thread
(routine: ART's implicit-NPE SIGSEGV fixups) entered the handler with a null chain → `tap_restore_default` installed
SIG_DFL process-wide → a recoverable fault killed the process. **New order (`src/loader/native_provider.rs`):**
(1) query the current kernel action (`sigaction(signum, NULL, &queried)` — an invalid signum fails before anything is
seeded or installed); (2) seed the chain slot from it (`tap_chain_store(bionic_action_from_glibc(&queried))`);
(3) install the tap; (4) re-seed from the install's returned oldact ONLY if it differs from the queried action (a
re-registration raced the query→install window; `BionicSigaction` now derives `PartialEq`/`Eq` for the comparison).
The handler can therefore never observe a null chain from a real boot (that branch stays as a dated defensive floor).
Ordering invariants preserved by construction: seed (Release) → install → `TAPPED_SIGNAL` gate (Release) LAST. Pool
budget: the real flow still claims 3 cells (query seed + crashpad register + crashpad restore), 4 only in the raced
re-seed case — the `TAP_CHAIN_POOL_LEN`/`tap_chain_register` comments are updated and the section-header + install
doc comments rewritten so no stale comment describes install-then-seed. **Regression guard (deterministic, no
concurrency test):** `early_fault_tap_intercepts_registration_and_chains` gains phase (a2) —
`install_early_fault_tap(SIGKILL)` (queryable but never replaceable, kernel EINVAL; nothing is ever raised) must fail
with the pool cursor advanced by exactly 1 and `TAP_CHAIN` published while the gate stays closed — verified to FAIL
under the old order (assertion "the chain seed is claimed BEFORE the kernel install") — plus an exactly-one-cell pin
on the quiescent real install (no spurious re-seed). Same-pattern audit: `init_run.rs::install_crash_handler` is not
the pattern (its handler reads always-valid atomics and `_exit`s — no install-before-seed dependency); no other
handler installer in `src/`. **Gate (re-run in full):** fmt --all / build --all-targets / clippy `-D warnings` /
test (**516 unit + 4 integration (self-skip, displays/APK unset) + 2 doctests = 522 passed, 0 failed**) / release —
all 0-warning/0-error. NOT committed (the Push-phase agent owns commit/push).
- **2026-06-12 (engine-SIGSEGV-resolved — live owner validation on the dev-host main loop)** — ✅ The signal-ABI work
  (committed: origin/main `1b56e99`) **resolved the engine's crashpad-era SIGSEGV**: it no longer reproduces. Validation:
  6 consecutive `./target/release/eclipse run <APK>` boots (`ECLIPSE_ANDROID_FRAMEWORK_DIR=~/.cache/eclipse/
  framework-patched`) — 5 warm (`/tmp/tap{1..5}.log`) + 1 COLD (`/tmp/cold1.log`, `~/.local/share/eclipse/app-data` +
  `/tmp/atl_cache/com.roblox.client` wiped) — all `EXIT=124` (clean timeout), all drive lifecycle 1–7 → `ActivitySplash`
  RESUMED → winit window → **Vulkan swapchain present loop**, with **zero** `Fatal signal 11`, **zero** `corrupted
  double-linked list`, and the early-fault tap **never firing** (no engine SIGSEGV remains to trace → the team's prior
  "capture the engine fault via the tap" START-HERE is retired; the tap stays as a dated diagnostic floor). A transient
  1/3 SIGABRT heap-corruption seen immediately after the merge did NOT recur after rebuilding the release binary from
  the merged tree + a warmed asset cache, and cold-start is clean too → it was a first-run asset-unpack race, not a
  standing bug. The engine reaches `AndroidGLView nativeInitClientSettings` + `FlagCache … post TTI` then idles: once
  `graphics::run_windowed` claims the main thread for Eclipse's own clear-and-present loop (recorded `view_registry`
  quads), the engine's `AndroidGLView` has **no host surface wired to it**, so Roblox's real GL UI never reaches the
  screen. **This reframes the §5 START-HERE to the render-integration build**: hand Eclipse's window `ANativeWindow` to
  the engine's `AndroidGLView`/EGL (the `__gl-test-anw` diagnostic + green `gl_test_anw_binds_real_wsi_handle` already
  prove engine-GLES2-on-Eclipse's-window works; the boot just doesn't WIRE it). Doc-only change (Living State §5 + this
  entry); no code touched, gate unchanged (**516 unit + 4 integration + 2 doctests**, fmt/clippy `-D warnings`/release
  all 0-warning). Committed + pushed (owner authorized git this session; no co-author).
- **2026-06-12 (main-Looper pump → deep engine init; engine NULL-deref captured)** — 🚀 **Bound the Android main
  `Looper` to the winit loop so Roblox boots PAST the splash.** Root cause of the post-RESUMED stall (proven by a SIGQUIT
  thread dump: main thread parked in winit with NO managed frames, i.e. not in `Looper.loop()`; Roblox made 0 network
  calls though the host CDN returns HTTP 200): Eclipse drives lifecycle 1–7 then enters `graphics::run_windowed`, so the
  main thread never pumps the Android main `Looper` — `Handler.post` continuations and (on Android) `SurfaceHolder`
  callbacks dispatch on the main thread via `Looper.loop()`, which never runs. Fix: bind `MessageQueue.nativePollOnce`/
  `nativeWake` (ATL's patched `next()` is `if (nativePollOnce(mPtr,t)) return null;`; ours is NON-BLOCKING per
  `main_looper_poll_should_yield` — pull on `timeout==0`, yield otherwise so the driven `Looper.loop()` returns instead of
  blocking the winit thread); add `framework::pump_main_looper` (drive `Looper.loop()` once, re-entrancy-guarded) called
  from a new winit `GameWindow::about_to_wait` (ticks every frame via the renderer's `request_redraw`). **LIVE RESULT:**
  Roblox advances from the splash plateau into mimalloc init, `rbx-storage.db` SQLite (WAL recovery), `AndroidGLView`,
  telemetry, and real `ecsv2.roblox.com` HTTP — then reliably hits the engine's pre-existing SIGSEGV (the team's
  early-fault tap now captures it every run: `signal 11 MAPERR addr=0x58 rip=libroblox+0x2779cc4 err=0x6` = a USER WRITE
  through NULL+0x58 in the engine; some Eclipse-provided pointer is 0). **Same-pattern note:** the pump surfaced that the
  `tracing-subscriber` default `fmt` layer ABORTS under `panic="abort"` when a teardown-state worker thread logs
  (`fmt_layer.rs:1022 BUF.with()` on a destroyed thread-local → AccessError); replaced it with the zero-thread-local
  `diagnostics::PanicSafeStderr` layer (same RFC3339+level+target+fields format). A first attempt that kept a worker-
  reachable `thread_local!` poll counter reproduced the abort — removed; `nativePollOnce` is now TLS-free, the main-thread
  guard uses `try_with`. **Regression guards:** `main_looper_poll_yield_table_matches_atl_next_contract` pins the
  non-blocking yield decision (the whole pump correctness) + name/sig asserts for the two new natives. Gate (full):
  **517 unit + 4 integration + 2 doctests**, fmt --all / clippy `-D warnings` / release all 0-warning. **Known follow-ups
  the pump exposed (now the frontier):** the engine NULL+0x58 write (disassemble `libroblox+0x2779cc4`, trace the null
  source); worker `HandlerThread` loopers currently exit-immediately on the yield (none load-bearing yet; back with a
  real blocking wait if one appears). Committed + pushed (owner authorized git this session; no co-author).

### 2026-06-12 — `__sF` shape mismatch CONFIRMED (core 782252) and root-cause-FIXED: bionic array-of-structs vs Eclipse's 3-pointer table; 25 translating stdio natives; the "engine SIGSEGV resolved" claim was machine-specific

**Correction first (append-only — the 2026-06-12 engine-SIGSEGV-resolved entry above stands as written for ITS
machine):** that 6/6-stable validation was performed on the collaborator's machine. TODAY on the owner's machine
(cachyos x86-64) a live boot of v2.721.1108 died again — EXIT=139 at ~8 s (`/tmp/eclipse-render-check.log`),
crashpad's `Run book keeping for signal 11` present, the early-fault tap silent, systemd core 782252 captured
(116 MB, present). The crash is machine/network-state dependent (see "the first fault" below), which reconciles the
6/6 there with the reproduction here. The §5 "resolved" framing is corrected to machine-specific.

**Confirmed mechanism (gdb over core 782252, crashing thread LWP 782400 — the SECOND fault, the one that killed the
process):** Eclipse provided the bionic data symbol `__sF` as `SfTable([*mut libc::FILE; 3])` — 24 bytes of glibc
`FILE*` POINTER VALUES (registered FIRST-priority in `EclipseNativeProvider`; `reloc.rs` writes the table's address
verbatim into each importer's `R_X86_64_GLOB_DAT` slot). Bionic's PUBLIC ABI (AOSP NDK `stdio.h` +
`bits/struct_file.h`) declares `extern FILE __sF[]` — an array of STRUCTS, LP64 `sizeof(struct __sFILE)` = 152,
`stderr = &__sF[2]` — consumers compute an ADDRESS (`base + i*152`) and never LOAD the slots. Deref chain verified
end to end in the core: libroblox's bionic-compiled crashpad logger at `libroblox+0x278fe2d` runs
`mov $0x130,%r14d; add GOT(__sF),%r14; mov %r14,%rsi; call fputs` — GOT slot `libroblox+0x701c738` held
`0x557292051138`, whose memory IS Eclipse's table (`[_IO_2_1_stdin_, _IO_2_1_stdout_, _IO_2_1_stderr_]`,
gdb-resolved) — so the computed `FILE*` = `0x557292051268` = table+0x130, **280 bytes past the 24-byte table inside
unrelated Rust statics** in eclipse's `.data`. glibc `fputs` (the STRING arg was VALID — the 93-byte
`crashpad_client_linux.cc:337 … Handle a real crash` line, strlen `0x5d` in rax/rdx) read `fp->_lock` at fp+0x88 =
`0x5572920512f0` → `0xff`, then faulted at `fputs+53` `mov 0x8(%rdi),%rax`: `si_signo=11`, `si_code=1` SEGV_MAPERR,
`si_addr=0x107` = 0xff+8 — exact. SIGSEGV was blocked in-handler → kernel force-kill → EXIT=139. The historic
"rdi=0xff invalid string pointer" reading (2026-06-11 late entry) was a MISATTRIBUTED reloaded register (`fputs`
moves s→rbp / fp→rbx at +6/+9 and reloads rdi from `fp->_lock` at +30). Prior art: the SAME mechanism was gdb-proven
2026-06-05 at exit-time `fflush(&__sF[i])` (`init_run.rs`, then sidestepped with `_exit(0)`); the provision comment's
premise ("a relocated reference to `__sF[i]` yields a usable host stdio stream") was false — bionic consumers take
the address, they never read the pointer value Eclipse stored there.

**What the FIRST fault turned out to be (recovered rt_sigframe on the sigaltstack — the fault crashpad was logging):**
NOT an engine bug and NOT in libroblox: `si_addr=0x0`, RIP `0x62de3e1e` inside the executable mapping of
`~/.cache/art/x86_64/…boot-wolfssljni-hostdex.oat` — ART AOT boot-classpath code, the canonical ART implicit
null-check (`mov (%rsi),%edi` + vtable dispatch, no compiler-emitted null test, rsi=0) in the wolfssljni/okhttp
managed networking path of the just-started `getAllAppSettings` HTTPS fetch (machine/network-dependent). On Android,
libsigchain keeps ART's fault manager FIRST and converts this into a recoverable Java `NullPointerException`; here
the tap's chain slot held crashpad (registered through the `eclipse_sigaction` seam ~1.5 ms earlier), so crashpad ran
AHEAD of ART's fault manager, classified a routine fixup as "a real crash", and then its own logging died on `__sF`.
The faulting PC being OUTSIDE the published libroblox image is exactly why the early-fault tap's engine-PC filter
correctly stayed silent. One inferential step remains (that ART's fault manager would have claimed this exact PC —
strongly supported by the implicit-check codegen, not run-proven in this core); the Java-level origin of the null
(the long-noted `GetFieldID(SocketImpl.delegate)` NULL is a plausible upstream) is unproven, a separate follow-up.
**COMPANION WORK ITEM (distinct mechanism, deliberately NOT in this diff):** restore Android-equivalent ordering in
the `eclipse_sigaction`/tap chain so ART's fault manager sees synchronous faults BEFORE crashpad's first-chance
handler — without it, even with `__sF` fixed, a boot that hits a routine managed-NPE fixup racing crashpad's
registration still dies (now cleanly logged through the fixed stdio).

**The fix (root-cause, one place — `src/loader/native_provider.rs` + a small C shim):** (1) `__sF` is now a
bionic-ABI-shaped backing: `SF_FILE_STRIDE=152` (named const pinning the public LP64 `sizeof(struct __sFILE)`) ×
`SF_ENTRY_COUNT=3` = `SF_BACKING_LEN=456` bytes, `#[repr(C, align(8))]` over a zero-initialized writable
`UnsafeCell<[u8; 456]>` — so `&__sF[0]`/`&__sF[1]`/`&__sF[2]` = base+0x000/+0x098/+0x130 are deterministic
Eclipse-owned SENTINELS that can never alias unrelated statics. (2) `eclipse_sf_translate_stream` exact-matches the
three sentinel addresses → host glibc stdin/stdout/stderr; every other `FILE*` (incl. null) passes through unchanged
(fopen-returned streams are real glibc streams). (3) 25 stdio names now Eclipse-owned translating natives (provider
94 → 119 base, 175 total): 22 in Rust — clearerr fclose feof ferror fflush fgets fileno fputc fputs fputwc fread
`__fread_chk` fseek fseeko ftell ftello fwrite getc getwc setvbuf ungetc ungetwc (getwc/fputwc/ungetwc via direct
glibc extern decls; libc 0.2.186 lacks them for linux-gnu) — + fprintf/fscanf/vfprintf via the NEW clean-room
`src/loader/stdio_shim.c` (C-variadic + va_list, the established `liblog_shim.c`/`build.rs` pattern: remap the
stream, forward to glibc vfprintf/vfscanf). (4) The existing `eclipse_fwrite_chk` gained the sentinel remap. (5)
Disproven comments rewritten with dated 2026-06-12 notes: the `native_provider.rs` `__sF` doc (now records the
core-782252 proof), `init_run.rs`'s `_exit(0)` reason #2 (historical note; the `_exit(0)` STAYS — reason #1, live
engine worker threads, still holds), plus `docs/bionic-env-worklist.md` and `docs/libroblox-init-run.md` brackets.

**Same-pattern audit (static, data-only readelf over the v2.721.1108 x86-64 libs):** exactly FIVE libs import `__sF`
— libroblox.so (GOT `0x701c738`, APS2-packed relocs, slot proven by the core), libbacktrace-native.so (`0x55d688`),
libeigen_blas.so (`0x401c0`), librenderscript-toolkit.so (`0x61cb8`), libzstd-jni-1.5.7-6.so (`0xb3fd0`) — all
`R_X86_64_GLOB_DAT` addend 0 against the same base; ONE provider-side fix covers all five by construction. Every
OTHER Eclipse-provided data object re-audited shape-correct (`__stack_chk_guard`, 10× `AMEDIAFORMAT_KEY_*`, 7×
`SL_IID_*`) — `__sF` was the ONLY address-consumed array-of-structs. Fall-through UND OBJECT imports across all 11
libs (stdin/stdout/stderr — bionic API-23+ `FILE*` pointer objects, glibc shape-identical — environ, optarg, optind,
daylight, timezone, tzname, in6addr_*) verified shape-identical and deliberately NOT intercepted. The exhaustive
FILE*-consuming FUNC sweep of the five importers found ONE additional same-class instance, FIXED: `__fread_chk` fell
through to glibc whose argument order DIFFERS (glibc `(ptr, ptrlen, size, n, stream)` vs bionic
`(buf, size, count, stream, buf_size)`) — now an Eclipse native with the bionic order + bound + remap; the other
`_chk` imports verified shape-identical. Review re-verified the 25-name set is enumeration-complete for v2.721.1108
AND v2.724.735.

**Regression pins (existing `native_provider::tests` module — no new files, no scripts):**
`sf_backing_is_bionic_shaped_three_structs` (THE pin that would have caught the bug: stride 152, backing 456 B,
registered `__sF` addr == backing addr, every `&__sF[i]`+152 falls INSIDE the Eclipse-owned object — the old 24-byte
table fails it), `sf_sentinels_translate_to_host_streams` (sentinels → glibc stdin/stdout/stderr;
`eclipse_fileno`(sentinels) == 0/1/2; interior+null pass-through; THE killing call shape `fputs(msg, &__sF[2])` →
host stderr succeeds), `sf_stdio_natives_round_trip_a_real_stream` (pass-through branch over a real `tmpfile()`
through the natives incl. the C-shim fprintf/fscanf), `fread_chk_uses_the_bionic_argument_order_and_honors_the_bound`;
the disproven-shape `sf_table_points_at_three_host_streams` REMOVED; the provider presence/count test updated
(94 → 119 base, the 25 names listed).

**Gate (run end-to-end twice after the last edit; genuine rebuilds forced via `cargo clean -p eclipse`):** fmt --all /
build --all-targets / clippy `-D warnings` / test / release — **519 unit + 4 integration + 2 doctests = 525 passed,
0 failed**, all 0-warning. The 4 `tests/engine_milestones.rs` tests ran their LIVE milestone subprocesses (APK +
display present on this machine) and matched their exact SUCCESS markers — loader constructor run, GL render, ANW
WSI bind, looper input.

**Carried review notes (non-blocking, recorded not acted on):** (a) the 25 stdio natives are pinned by PRESENCE only
— add address pins (`p.resolve("fputs").addr == eclipse_fputs as …`, mirroring the existing `__sF` address pin) so a
re-introduced glibc fall-through cannot pass the suite; (b) the 25-name set is enumeration-complete for
v2.721.1108/v2.724.735 ONLY — a future APK importing another FILE*-consumer (putc, freopen, getline, `__fgets_chk` —
itself argument-order-mismatched, the `__fread_chk` class) would silently fall through and crash on a sentinel; guard
shape: extend the self-skipping real-APK link tests (`src/loader/link.rs`) to assert every FILE*-consuming stdio
import of a `__sF`-importing lib resolves to the Eclipse provider tier, and re-run the readelf enumeration on every
APK version bump; (c) `eclipse_fread_chk` aborts on size×count mul-overflow where bionic delegates to fread
(EOVERFLOW) — copies the 2026-06-05 `eclipse_fwrite_chk` posture, practically unreachable; (d) the `SfBacking` doc's
"no importer pokes fields (readelf audit)" overclaims what readelf proves — the real basis is the opaque
`__private[]` public NDK ABI + no `__srget`/`__swbuf`/`__isthreaded` imports + the writable zeroed backing as the
floor; reword when next touching the file; (e) crashpad's in-handler logging now reaches glibc `fputs`, which takes
the per-FILE lock (not async-signal-safe) — faithful to bionic-on-Android semantics, but a fault interrupting a
holder of the host stderr lock could deadlock the handler; the tap's `write(2)`-only dump remains the
async-signal-safe floor; (f) test nit: `eclipse_fileno(f) > 2` assumes fds 0–2 occupied (deterministic under
cargo test).

### 2026-06-12 — Core 866509 root-caused (the first-ever-swapchain boot's death): the apkenv delegation re-opened by a 2-import pre-load gap (FIXED) + the fatal-handler chain overflowing ART's 32 KiB heap altstack (FIXED, main thread); 8 NDK natives + engine-thread altstack recorded-only

**Context (owner live validation of the merged tree at `2ac0c9d`, `/tmp/eclipse-sf-validate.log`, EXIT=134):** the
`__sF` fix is RUN-PROVEN — crashpad's in-handler logging reached stderr intact, the routine first-chance ART
signal-11 at engine-clock 8.46 s (tid 866658, the 782252-class wolfssljni/okhttp implicit-null-check path) was
book-kept by crashpad running AHEAD of ART's fault manager and **the boot SURVIVED it**, then reached the
FIRST-EVER `Vulkan surface + swapchain initialized; clear-and-present loop active` on this machine (B8G8R8A8_SRGB
800×600) — and died ~0.14 s later. systemd core 866509 (SIGABRT, 119.7 M) held the whole story; forensics via the
established-safe `coredumpctl`/`gdb -batch` method (minimal call-site disassembly only).

**Mechanism 1 — CONFIRMED, boot-blocking, the fatal SIGSEGV (Eclipse-side root cause = the loader provision gap;
FIXED):** rip `0x7f54dbdcbc80` = `/usr/lib/libdl_bio.so.0.0.1+0x9c80` = `apkenv_find_library+3856` (the host
apkenv/bionic_translation shim linker, pkg r107.026ea254-1 — NOT libroblox/ART/host-graphics), disasm
`movl $0x1,0x18(%rax)` (`r_debug->r_state = RT_ADD`, the post-load debugger rendezvous) through the shim's
never-initialized BSS global `_r_debug_ptr` = NULL (`nm -D`: `11650 B _r_debug_ptr`; gdb by name: `0x0`). Raw
rt_sigframe mcontext `TRAPNO=0xe ERR=0x6 CR2=0x18` — a user-mode WRITE to NULL+0x18 (corrects the initial "read"
framing); registers byte-identical to the in-handler report at log ~line 708. It fired while apkenv recursed into
DT_NEEDED `"libm.so"` (rbx soinfo name + r12 string both `libm.so`, r12 inside libbacktrace-native's image); the
full trigger chain is attested in the stack: eclipse `nativeLoad` delegation frames ← `art::JavaVMExt::
LoadNativeLibrary` (libart+0x41b805) ← `bionic_dlopen` ← `apkenv_find_library` ×2. WHY the delegation was reached:
Eclipse's pre-load of `libbacktrace-native.so` failed at boot with exactly 2 unresolved strong imports (log lines
58–59: `unresolved_strong=2` + a count-only WARNING), so when rbx.backtrace's
`System.loadLibrary("backtrace-native")` was dispatched on the Main thread by the main-Looper pump,
`runtime_native_load` could not report it pre-loaded and delegated — the established "#1 wall" registry-consult
design intent says that delegation must never carry an Eclipse-preloadable lib. The 2 symbols were statically
identified (data-only readelf/nm set-arithmetic: UND set minus host libc/libm/libz exports minus every provider
registration = exactly `{__android_log_vprint, __umask_chk}`; the same method reproduces the boot log's 2/5/3
unresolved counts for all three failing libs; UND sets byte-identical across v2.721.1108 and v2.724.735). The
early-fault tap behaved correctly by design: frame #16 (eclipse+0x295b6b) proves it ran and CHAINED, and its 0 dump
banners are the engine-PC filter correctly excluding a libdl_bio rip (libroblox itself pre-loaded fine: 3388 ctors,
`JNI_OnLoad 0x10006`). The apkenv `_r_debug_ptr` defect itself is host-package-internal — not durably fixable by
Eclipse, and the architecture already treats apkenv as the dead-end the Rust loader exists to replace. **Fix:**
(a) `__android_log_vprint` DEFINED in `src/loader/liblog_shim.c` (`va_list` — the established clean-room C-shim
pattern; bounded vsnprintf → `eclipse_liblog_emit`, same return contract as `__android_log_print`);
(b) `__umask_chk` as a Rust translating native in `native_provider.rs` (bionic FORTIFY: abort on `mode & ~0o777`,
else glibc `umask` — public contract); provider liblog 5→6, bionic-libc 15→16 = **121 base / 177 total**;
(c) surgical observability fix tied to this root cause: `EngineLoadError::UnresolvedImports` now carries the
sorted/deduped NAMES and the pre-load WARNING prints them (the 2-symbol identification had to be reconstructed
offline; the named warning run-proves it on the owner's next boot).

**Mechanism 2 — CONFIRMED, the amplifier not the killer (Eclipse-integration-owned; FIXED on the main thread):**
the `malloc(): unaligned tcache chunk detected` SIGABRT was NOT pre-existing corruption and NOT the 25 new stdio
natives (the prime suspects — exonerated by line-by-line audit AND by the proven alternative planter): vendored
ART's `Thread::SetUpAlternateSignalStack` (`thread_linux.cc`) registers each attaching thread's altstack as a
32 KiB glibc-HEAP buffer (`operator new[]`, libart+0x174b04 `call _Znam`), no guard page, live malloc arena below —
and Eclipse deliberately installed no sigaltstack (the old `native_provider.rs:1806` stance, justified by "the
known fault is not a stack overflow" — DISPROVEN by this core). The fatal chain (tap → libsigchain → ART
`HandleUnexpectedSignalCommon` → `DumpNativeStack` → `BacktraceMap::Create` → vendored libunwind, whose maps-parser
frame alone is 76,816 B) consumed ~79.2 KiB: thread rsp bottomed 51,888 B BELOW `ss_sp` (frame-22 ucontext
`uc_stack: ss_sp=0x5603b6c5a050 ss_size=0x8000`), zero-filling live heap. The heapslice's maximal zero runs match
frame #12's bounds exactly, interrupted only by libunwind's strdup'd 62-char `framework-res.apk` path (tcache
bin 3); the zeroed free chunk's next field gave `REVEAL_PTR(0)` = addr>>12 = `0x5603b6c51` — the one unaligned
`entries[3]` — and the next bin-3 strdup hit glibc 2.43 `malloc.c:5341 misaligned_mem` → abort. Yoshi's earlier
transient `corrupted double-linked list` is the same class (consistent, not run-proven — no core retained; with
the fix, any recurrence presents as a clean guard-page SIGSEGV at a PROT_NONE address — itself the diagnostic).
**Fix:** `install_guarded_altstack()` — mmap PROT_NONE then mprotect-RW, 256 KiB usable (3× the measured ~80 KiB
`ALTSTACK_CHAIN_BUDGET`) over one PROT_NONE guard page — wired into `runtime::boot` immediately AFTER
`JNI_CreateJavaVM` (verified against the vendored ART source: `Thread::Init` overwrites any pre-installed stack
unconditionally → install after; `TearDownAlternateSignalStack` `delete[]`s the CURRENT `ss_sp` but never runs on
the main thread — `Vm` has no `Drop`, `DestroyJavaVM` is never called; the displaced 32 KiB ART buffer leaks once
by design — freeing a foreign `operator new[]` allocation would be unsound). Install failure is a non-fatal
WARNING (boot proceeds on ART's stack = the pre-fix state). The disproven no-sigaltstack comment is rewritten with
the dated core-866509 note.

**Recorded-only (deliberate, per the no-workarounds policy — no code):** (a) the 8 NDK natives behind the two
same-pattern pre-load failures — `libimage_processing_util_jni.so` needs `{ANativeWindow_lock,
ANativeWindow_setBuffersGeometry, ANativeWindow_unlockAndPost, AndroidBitmap_lockPixels,
AndroidBitmap_unlockPixels}` (5) and `librenderscript-toolkit.so` needs `{AndroidBitmap_getInfo,
AndroidBitmap_lockPixels, AndroidBitmap_unlockPixels}` (3) — counts match the boot log's 5/3 exactly; no
`System.loadLibrary` of either is on the current boot path, and real implementations need the ANativeWindow
CPU-buffer + jnigraphics surface (design work, not fall-through stubs); extend the link.rs pin's lib list when
they land. (b) ART-attached ENGINE threads still receive ART's guard-less 32 KiB heap altstack at attach
(`SetUpAlternateSignalStack` overwrites any pre-installed stack; a trampoline install would be clobbered AND
risks `TearDown`'s foreign-`delete[]`) — the exact core-866509 corruption stays reachable on an engine-thread
fatal fault; not fixable Eclipse-side without modifying vendored ART, but Eclipse BUILDS vendored ART locally, so
a vendor-build-side mitigation (`kHostAltSigStackSize` bump / guard-paged variant in `thread_linux.cc`) is the
candidate follow-up that closes the class — explicit open work item. (c) The ART-first fault-manager-ordering
companion item stays monitored-only: today's evidence cuts against urgency (the boot SURVIVED the first-chance
signal-11 with crashpad-first post-`__sF`-fix, and the fatal PC was native — apkenv — which ART's fault manager
cannot claim under ANY ordering; SignalChain correctly fell through to ART's unexpected-signal dump). Graduation
condition: a boot dying where crashpad classifies a MANAGED-PC fault (.oat/JIT mapping) that ART would have
converted to a Java NPE — capture that core first.

**Same-pattern audits:** (provider-gap class) the readelf/nm set-arithmetic above over BOTH APK versions —
libbacktrace-native = exactly the 2 (fixed); the 5/3 recorded-only; libzstd-jni's only residuals are 4 WEAK
`ZSTD_trace_*` (legal weak-undef→0, not strong). (altstack-overflow class) grep of `SA_ONSTACK`/`sigaltstack`
across `src/`: the tap (`SA_ONSTACK` — now guard-paged on the main thread); `init_run.rs::crash_handler` has no
`SA_ONSTACK` and `_exit`s (shallow, not the pattern); the engine's bionic `sigaltstack` import deliberately stays
host-baseline (`stack_t` layout-identical on x86-64; engine-registered stacks are the engine's own).

**Regression pins (existing style — no new files, no scripts):** `native_provider::tests` —
`umask_chk_forwards_a_valid_mode_and_round_trips` (new),
`guarded_altstack_installs_eclipse_region_with_a_prot_none_guard_page` (new — active-stack identity via
`sigaltstack(NULL,&ss)`, `ss_size >= 2×` the documented chain budget, PROT_NONE guard probed via the tap's
`process_vm_readv` self-probe; NOTE: it pins the INSTALLER's geometry/protection/registration — the live boot
WIRING is evidenced by the new `main-thread alternate signal stack: Eclipse guard-paged …` boot line, per
dev-host-runbook practice), the presence/count test updated (121 base; both new names),
`provider_resolves_registered_and_rejects_unregistered` extended; `engine::tests` —
`unresolved_imports_error_names_the_symbols` (new — the Display must NAME every unresolved import);
`link::tests` — `real_boot_path_loadlibrary_libs_fully_resolve` (new, self-skipping real-APK: every boot-path
`System.loadLibrary` lib must pre-load with its unresolved set CONFINED to the documented boot-only RTLD_GLOBAL
surface — 8 zlib names via libart's NEEDED libz + `pthread_atfork`, host-dlsym-invisible because glibc's is
compat-versioned `@GLIBC_2.2.5`; ANY other name = re-opened apkenv delegation; it ran LIVE against v2.724.735 and
it CAUGHT the pre-fix state during development). One discovery recorded: the boot resolves 9 extra
libbacktrace-native imports only via libart's RTLD_GLOBAL NEEDED surface (reloc arithmetic cross-checks exactly:
boot 11301+2 vs test 11294+9 = the same 11303 with the 2 new natives) — hence the confined-allowlist form instead
of a raw `==0` (dlopen'ing libz RTLD_GLOBAL in-test would leak zlib names into RTLD_DEFAULT and perturb the
sibling 88-work-list count tests). Doc reconciliation: `docs/bionic-env-worklist.md` gains a dated scope note
(its 5/15 counts are libroblox's own work-list, still correct; the provider's 6/16 additions are
libbacktrace-native's).

**Gate (run on this exact tree; genuine rebuilds forced via `cargo clean -p eclipse` per gate precedent):**
fmt --all (+ `--check`) / build --all-targets / clippy `-D warnings` / test (displays + `ECLIPSE_ROBLOX_APK`
unset; the 2 display-gated milestone tests took the documented SKIP path, the 2 APK-gated ones ran their live
milestone subprocesses against the default `$HOME/eclipse-m0` APK — this host's documented norm) / release —
**524 unit + 4 integration + 2 doctests = 530 passed, 0 failed**, all 0-warning. No live ART/bionic boot was run
in the workflow (dev-host boundary respected); the live validation of the named-imports warning + altstack boot
line is the owner's next-session START-HERE.

**Carried non-blocking notes (recorded, not acted on):** (a) the engine-thread altstack exposure above — recorded
as an explicit open work item, not only a code comment; (b) the guarded-altstack test comment slightly overclaims
("ever becomes the active one again on a thread Eclipse owns") — it cannot detect the `runtime::boot` wiring being
dropped, ART re-overwriting on a re-attach, or the engine displacing the stack via its host-baseline `sigaltstack`
import; reword toward the installer-properties claim when next touching the file (live wiring = the boot log
line); (c) mechanism-3 state moved under the collaborator's same-day entries while this work was in flight: it is
now identified as MIMALLOC's `_mi_thread_done` on a partially-initialized per-thread heap, candidate (a)
registration-skip RULED OUT (the thread IS Eclipse-`pthread_create`'d) — the verdict's (a)-vs-(b) gdb plan is
superseded by his narrower open question (why the per-CPU body is zero at exit; mimalloc lazy-init vs
`__call_tls_dtors`/pthread-key destructor ordering under Eclipse's trampoline); with mechanism 1 fixed, that fault
is the most probable next wall, and the guard-paged altstack now protects its diagnosability (the same deep ART
dump chain would have re-corrupted the heap mid-backtrace exactly as core 866509 did); (d) why the collaborator's
`/tmp/r*.log` boots survived the rbx.backtrace `System.loadLibrary` path is unestablished (likely config/flag
dependent — `No symbolication ID provided` printed immediately before today's fatal block); (e)
`eclipse_umask_chk`'s abort-on-invalid-mode branch is pinned by presence, not exercised (process-fatal by
contract); (f) the one-time displaced 32 KiB ART main-thread altstack buffer leak is by design and documented at
the wiring site.

---

### 2026-06-12 — Core 947663 root-caused: Eclipse inverted bionic's thread-exit destructor order — the engine's mimalloc key-dtors freed+abandoned the per-thread registry block and a sibling RECLAIMED it before the late cxa finalizer walked it (FIXED: Eclipse-owned `__cxa_thread_atexit_impl`, cxa-before-keys on both exit paths); + `pthread_atfork` provided and the masking allowlist entry removed (corrects this log's prior "resolves clean" claim); + `ANativeWindow_getFormat`

**Context (owner live validation of `2ceca8a`, `/tmp/eclipse-866509-validate.log`, EXIT=139 → fresh core 947663,
118.6 M):** the 866509 fixes are LIVE-PROVEN — the guard-paged altstack banner printed (log line 34: `main-thread
alternate signal stack: Eclipse guard-paged 256 KiB @ 0x7f6e4e7c0000`), there was NO malloc-corruption abort (the
complete deep libunwind/ART dump chain finished intact — the fix held under a real fatal fault), and the
named-imports WARNING run-proved itself (line 60). The boot reached the Vulkan swapchain (line 550), then ~0.17 s
later the early-fault tap made its FIRST REAL CAPTURE (line 613): `*** ECLIPSE EARLY-FAULT TAP: signal 11 code 1
(MAPERR) addr=0x58 ***` — thread 947876 "Main" (ART-named), rip=`0x7f6e000b5cc4` = libroblox+`0x2779cc4` (base
`0x7f6dfd93c000`), rax=0 r12=0 — byte-identical to core 947663's frame-17 sigframe and to Yoshi's earlier capture
at a different base. Forensics on Eclipse's own core via the established-safe `coredumpctl`/`gdb -batch` method,
matched against PUBLIC mimalloc sources (minimal call-site disassembly only).

**Mechanism 1 — CONFIRMED Eclipse-side (hypothesis (b) destructor ORDERING; boot-blocking; FIXED). Yoshi's open
question — "why is the per-thread body zero at exit?" — is ANSWERED: it was never half-initialized; it was
freed-then-reclaimed before its finalizer ran.** Chain: Eclipse's `thread_trampoline` and `eclipse_pthread_exit`
ran `run_thread_key_destructors()` BEFORE glibc `start_thread`'s `__call_tls_dtors`, while
`__cxa_thread_atexit_impl` was deliberately left on host glibc (the "stays on the host baseline (ABI-identical)"
comment — true of the SIGNATURE, false of the ORDERING semantics) — public AOSP bionic `pthread_exit.cpp` runs
`__cxa_thread_finalize()` FIRST, then `pthread_key_clean_all()`; Eclipse ran the two destructor classes in the
inverted order, and stock mimalloc's thread-exit hook IS a pthread-key dtor (public `src/init.c`). Under Eclipse:
key0's dtor (libroblox+`0x2c18fb0`, a deferred-free cache) mi_frees the dying thread's Roblox 128-slot
TLS-registry block and key1's dtor (libroblox+`0x232813f` = `_mi_thread_done`, disasm-matched field-for-field to
public microsoft/mimalloc `init.c`, v3 with reclaim ON) abandons the thread's pages; mimalloc page reclaim hands
the freed block to newborn sibling LWP 947882, which zeroes it, stamps its TID at obj+0x14, and parks
mid-registration on the registry per-CPU futex (`0x7f6e42dc660c`, the recycled page base on its stack ×3); THEN
glibc `__call_tls_dtors` runs the engine's cxa finalizer (libroblox+`0x2779bb0`, registered exactly once, list
intact) on the stale obj: `[obj+0x408]`=NULL → `movq $0x0,0x58(%r12)` = write through NULL+0x58 → SIGSEGV. Key
core evidence: faulted obj `0x7f6dc1df00a0` is all-zero EXCEPT live sibling 947882's TID at +0x14; healthy parked
sibling 947875 shows the initialized shape (node+0x58 → obj back-pointer) AND its key0 deferred-free item[4] IS
its own registry obj — wiring the free to the key sweep; the crashed thread's TLS_VALUES slots are all-zero (both
key dtors ran before `__call_tls_dtors`). Alternatives disproven: (a) failed-lazy-init — the healthy sibling shape
+ the foreign TID stamp prove reuse, not non-init; (c) TSD corruption — TLS generations coherent, no key reuse,
the `__cxa` registration exists exactly once in the intact `tls_dtor_list` (mangled pointer demangles to
`0x7f6e000b5bb0`; 28 sibling registrations process-wide; zero direct call sites to the finalizer in the whole text
image); `gettid`/`sched_getcpu` natives verified correct; libroblox still has no PT_TLS (static-TLS stays ruled
out). Roblox's exit design is VALID under bionic's contract — Eclipse broke the contract. **Fix:** Eclipse-owned
`eclipse_cxa_thread_atexit_impl` in `bionic_pthread.rs` — per-thread LIFO `CXA_THREAD_DTORS` (RefCell<Vec>),
loop-drain re-entrancy-safe (pop releases the borrow before each call; a dtor may legally register more),
`dso_handle` accepted+ignored (engine libs are never unloaded, dated), `try_with` teardown-safe registration with
a documented fallback (forward to host glibc's `__cxa_thread_atexit_impl` via dlsym; bounded leak if even that is
absent) — and `run_cxa_thread_dtors()` drains it BEFORE `run_thread_key_destructors()` in BOTH
`thread_trampoline` (return-from-start) and `eclipse_pthread_exit`, restoring bionic's order on both exit modes.
Non-Eclipse-created threads drain via `CxaThreadDtorList::Drop` in glibc's `__call_tls_dtors` phase — today's
glibc semantics preserved where Eclipse does not own the thread (dated comment). The disproven "ABI-identical"
claims rewritten with the dated core-947663 finding (PTHREAD_NATIVE_COUNT doc; `docs/libroblox-init-run.md` §8
dated bracket). **Necessary intersect discovered by the new pthread_exit-path pin aborting (then verified against
the installed rustc 1.96.0 std personality source — an IP missing from the LSDA call-site table yields
`EHAction::Terminate` even under `_UA_FORCE_UNWIND`):** glibc `pthread_exit` force-unwinds, and Eclipse's frames
were nounwind `extern "C"` — the three Eclipse crossing points (`eclipse_pthread_exit` + its host transmute,
`SpawnArgs::start` + the widening transmute, `thread_trampoline` + the narrowing transmute) are now
`extern "C-unwind"` (machine-ABI identical to "C"), and the trampoline early-drops its `Box<SpawnArgs>` before
`start()` so the frame is a plain-old-frame (RFC 2945). This is the load-bearing ORDERING half of D4; the
engine-side unwind exposure remains a recorded open item.

**Mechanism 2 — CONFIRMED + the post-hoc BLOCKING finding (FIXED both): the `pthread_atfork` provider gap kept
libbacktrace-native's pre-load failing — and the 2ceca8a regression guard ALLOWLISTED the failure.** Run-proven:
log line 59–60 verbatim — `applied_nonnull=11302 weak_zero=0 unresolved_strong=1` + `WARNING: pre-load of
libbacktrace-native.so failed (continuing): 1 strong import(s) unresolved (work-list non-empty): pthread_atfork` —
unresolved went 2→1, NOT 2→0. **This CORRECTS the core-866509 entries above (append-only): the claim "the pre-load
resolves clean and the apkenv delegation is never entered" is FALSIFIED, and the reloc cross-check arithmetic
(boot 11301+2 vs test 11294+9, total 11303 in both) actually proves the old boot's real unresolved set was
`{__umask_chk, pthread_atfork}` — `__android_log_vprint` was resolved by `/usr/lib/art/liblog.so`'s RTLD_GLOBAL
export, which the offline set-arithmetic did not model.** Root cause of the residual: glibc exports
`pthread_atfork` only as a compat-versioned WEAK symbol `@GLIBC_2.2.5` (no default version — `dlsym(RTLD_DEFAULT)`
fails, empirically proven), NO boot-mapped lib defines it (nm scan over all 79 old-core libs — the link.rs comment
"an ART-side lib exports pthread_atfork" is DISPROVEN), and new links get it from `libc_nonshared.a` (a
binary-LOCAL `W pthread_atfork` wrapper → `U __register_atfork@@GLIBC_2.3.2` — never in any dlsym surface). Worse,
the guard built to pin this class allow-listed the exact name (`boot_global_resolvable` in
`real_boot_path_loadlibrary_libs_fully_resolve`), green-lighting the failure. This boot the delegation provably
never ran — three independent proofs: zero `is not a prelinked library` warnings (the old log had them right after
the SAME `b.<init>` line), no `Runtime.nativeLoad` for backtrace-native, and core 947663's NT_FILE has zero
file-backed native-libs mappings (vs core 866509's apkenv fingerprint) — most consistent reading:
`System.loadLibrary` lost the ~ms race to mechanism 1. But `libdl_bio.so.0.0.1` is resident every boot (5 segments
in core 947663) and rbx.backtrace's loadLibrary IS on the boot path, so with mechanism 1 fixed this was the next
fatal (the core-866509 NULL-`_r_debug_ptr` precedent). **Fix:** `eclipse_pthread_atfork` in `bionic_pthread.rs`
forwarding to the link-time `libc::pthread_atfork` (probe-verified on this host: a libc-0.2.186 program compiles,
links, and runs — the PRELOAD-forensics premise "libc 0.2.186 lacks it" is disproven; portable to musl hosts
unlike a glibc-internal `__register_atfork` extern). Bionic-contract signature (3 × `Option<unsafe extern "C"
fn()>`, NULL handlers allowed); NO Eclipse-side handler list — the in-process fork IS glibc's, so glibc's own
atfork list runs the handlers at exactly the right points. AND `pthread_atfork` REMOVED from the allowlist (now
exactly the 8 zlib names, re-verified sound: resolved at boot via libart's RTLD_GLOBAL NEEDED libz) + the
disproven comment rewritten — the boot-path test is now the fail-closed pin.

**Mechanism 3 — the residual pre-load inventory (run-proven, all four DORMANT this boot — no later loadLibrary/
class references in the log) + the post-hoc 2ceca8a review outcome.** [1] `libimage_processing_util_jni.so` — 5
unresolved (`ANativeWindow_lock`/`setBuffersGeometry`/`unlockAndPost` + `AndroidBitmap_lockPixels`/
`unlockPixels`, line 88); [2] `librenderscript-toolkit.so` — 3 unresolved (`AndroidBitmap_getInfo`/`lockPixels`/
`unlockPixels`, line 93): one shared pin-or-copy AndroidBitmap/ANativeWindow-CPU-buffer design covers 8 of the 10
outstanding imports (`lockPixels` must yield a stable pixel pointer until `unlockPixels`; no bitmap registry
exists in `src/framework/`) — recorded-only design work per the no-fall-through-stubs policy, blocked on the
render-integration build. [3] `libsurface_util_jni.so` — 1 unresolved (`ANativeWindow_getFormat`, line 98): FIXED
NOW — `eclipse_anativewindow_getformat` in `native_provider.rs`, the exact sibling of getWidth/getHeight (WSI map
first → `WINDOW_FORMAT_RGBA_8888`, then the `ndk_registry::native_windows()` slab `.format`, −1 for stale/
fabricated handles per the NDK negative-error contract), registered (ndk 27→28) and `libsurface_util_jni.so`
added to the boot-path pin's staged lib list (run-proven in-test: `applied_nonnull=9, unresolved_strong=0` — 1/1
closed). Deliberate deviation from the verdict's letter, recorded: getFormat is NOT added to the libroblox
27-name ndk pin loop — it is NOT a libroblox import (the assertion would be false); the genuine pins are the
boot-path lib test + the provider presence/count test (the standing "extend the link.rs pin's lib list when they
land" intent). [4] `libtrampoline.so` — 1 unresolved (`__libc_init`, line 103): NOT a JNI lib (dynsym = 8 entries
ALL UND — zero exports, no `JNI_OnLoad`/`Java_*`) — an exec-style stub a Java `loadLibrary` can never serve; a
`__libc_init` native would be a fall-through stub for an exec-only code path — triage-only. **Post-hoc 2ceca8a
review (the lost lenses now covered): verdict FAIL — 1 blocking + 2 minor, ALL repaired in this pass.** Blocking =
the allowlist masking (mechanism 2 above). Minor (a): `EngineLoadError::UnresolvedImports` Display printed the
RELOC count as "import(s)" (a 1-symbol/2-reloc gap would print `2 strong import(s) unresolved: __foo` — the exact
triage ambiguity 2ceca8a existed to remove) → now `{names.len()} strong import(s) unresolved ({n} reloc(s)):
{names}`, wording pinned by `unresolved_imports_error_names_the_symbols` with a deliberately mismatched
3-relocs/2-names fixture; the same-pattern sweep found ONE more instance — `init_run.rs`'s diagnostic-harness
warning — fixed identically. Minor (b): `umask_chk_forwards_a_valid_mode_and_round_trips` gained the dated
invariant comment (sole umask(2) toucher in the test binary; grep before adding another — a second toucher
introduces a flake). Review notes verified-CLEAN (no action needed): "ART never re-attaches main" TRUE on every
current path (zero `DetachCurrentThread`/`DestroyJavaVM` sites; `Vm` has no Drop); provider-count math;
UnresolvedImports plumbing (sole construction → Display → boot log); BTreeSet sortedness; the
`__android_log_vprint` va_list C/Rust boundary; all policy items (dated comments/SAFETY/surgical diff). Optional
extras deliberately SKIPPED (surgical-changes policy): the `__umask_chk` abort-branch FATAL liblog line; the
`jni_register.rs` scoped-attach cross-reference comments.

**Same-pattern audits:** (thread-local `.with`-aborts-under-`panic=abort` class — the fixed tracing-subscriber BUF
precedent) `TLS_VALUES` had 3 sites, all converted to `try_with` with defined dated fallbacks (getspecific → NULL,
setspecific → EINVAL, key-sweep → return); the new `CXA_THREAD_DTORS` uses `try_with` by construction;
`framework.rs`'s main-thread guard already did; no other engine-reachable `thread_local!` `.with` in the loader.
(destructor-order class) the only two thread-exit paths Eclipse owns both drain cxa-before-keys now; non-Eclipse
threads documented (D2/Drop comments). (disproven-premise-allowlist class) the remaining 8 zlib names re-verified
sound; no other name remains. (single-import pre-load class) the four dormant libs triaged above.
(reloc-count-labelled-as-imports class) grep over unresolved-printing sites found exactly ONE more instance
(`init_run.rs:189`) — fixed; `link.rs`'s test eprintln prints explicitly-labelled stats fields (not the pattern);
`main.rs` prints the fixed Display. (stale-comment sweep for the disproven claims) PTHREAD_NATIVE_COUNT doc +
link.rs `:2123` comment rewritten, `docs/libroblox-init-run.md` §8 dated bracket added; `bionic_env.rs`'s
CxaRuntime classification already said "baseline only" — left untouched.

**Regression pins (existing style — no new files, no scripts):** `bionic_pthread::tests` —
`cxa_dtors_run_before_key_dtors_and_lifo_on_return_from_start` + `cxa_dtors_run_before_key_dtors_on_pthread_exit_path`
(NEW — a real thread through `eclipse_pthread_create`, one key dtor + cxa dtors registered through the Eclipse
natives; pin cxa-before-key on BOTH exit paths, LIFO within the cxa list, loop-drain of a mid-drain
re-registration, null-func rejection, and the join retval round-trip; BOTH verified to FAIL under the pre-fix
inverted drain order — `got last-cxa=4 KEY=1` / `got CXA=2 KEY=1` — and the exit-path test exercises the real
glibc forced unwind through the now C-unwind-typed frames), `pthread_atfork_registers_handlers_including_null`
(NEW — pins the link-time `libc::pthread_atfork` binding without forking), the word-count test auto-tracking
PTHREAD_NATIVE_COUNT 51→53; `link::tests` — `real_boot_path_loadlibrary_libs_fully_resolve` (UPDATED — THE
mechanism-2 pin: `pthread_atfork` removed from the allowlist = fails closed on any future fall-through;
`libsurface_util_jni.so` staged; ran LIVE against the real APK this session: libbacktrace-native unresolved =
exactly the 8 zlib names, libsurface_util_jni 0, libzstd-jni 0); `native_provider::tests` — presence/count
updated (180 total = 122 base + 53 pthread + 5 sysconf; the three new names present), the two ANativeWindow tests
extended (slab-default format / stale → −1 / registered-WSI → RGBA_8888); `engine::tests` —
`unresolved_imports_error_names_the_symbols` (UPDATED — the wording pin).

**Gate (run on this exact tree; genuine rebuilds forced via `cargo clean -p eclipse` / `--release` per gate
precedent; logs `/tmp/eclipse-gate-{build,clippy,test,release}.log`):** fmt --all (+ `--check`) / build
--all-targets / clippy `-D warnings` / test / release — **527 unit + 4 integration + 2 doctests = 533 passed, 0
failed**, 0 SKIP (all 4 milestone tests ran their live milestone subprocesses against the default APK + displays —
this host's documented norm; none boots ART), all 0-warning. No live ART/bionic boot was run in the workflow
(dev-host boundary respected); the live validation expectations are the §5 START-HERE.

**Carried non-blocking notes (recorded, not acted on):** (a) the link.rs boot-path-test comment says "the two
remaining same-pattern pre-load failures" — UNDERCOUNT: the validation log named a third, `libtrampoline`
(`__libc_init`); its disposition is recorded HERE (not-JNI-loadable, triage-only) — name it in that sentence when
next touching the file. (b) Narrow bionic divergence: a cxa dtor registered DURING the key sweep on an
Eclipse-created thread runs LATE via the Drop drain (bionic LEAKS such a registration) — triage any future fault
in that window against this; matching bionic exactly needs a post-finalize flag, only with evidence. (c) Failure
ergonomics of the two ordering pins: a failing assert inside the spawned start fns presents as a SIGABRT of the
test process (panic through the C-unwind trampoline into glibc — "failed to initiate panic"), not a clean failed
assertion; the static ticket atomics in the core identify which assertion tripped. (d) `docs/bionic-env-worklist.md`
staleness: the cxa-runtime row ("baseline; glibc atexit semantics") and the dated scope note ("grown 2 natives …
121 base / 177 total") now lag — correct to the Eclipse-tier `__cxa_thread_atexit_impl` + 122 base / 180 total /
5 beyond-work-list natives on next touch (dated-bracket style). (e) Pre-existing: the `register_natives` section
header says "rwlock (6)" but registers 5 (the 53 arithmetic is right) — fix the header on next touch. (f)
Non-Eclipse-thread interleaving: batching engine cxa dtors into one Drop entry changes the global LIFO
interleaving vs other same-thread registrants (engine-internal LIFO preserved) — a candidate for any future
ART-attached-thread teardown fault, alongside D2. (g) The dtor call sites in both sweeps are plain `extern "C"`
(nounwind) — a dtor-initiated `pthread_exit` would terminate there (pathological under bionic too); decide at D4
design time. (h) The foreign-thread Drop-drain path has no pin (the two new tests cover the Eclipse exit paths);
a cheap deterministic pin exists if it ever matters (`std::thread::spawn` + register via the native + join +
assert-ran). (i) Recorded-only hazard notes now live in code: `bionic_env.rs` — the cross-allocator class
(vasprintf/realpath(..,NULL)/strdup-family return glibc-heap blocks; consistent today, must move TOGETHER with any
future Eclipse malloc/free displacement) and the bionic-mallinfo 80 B vs glibc 40 B shape note; D2 + D4 dated
open-item comments sit at the sweep/exit sites in `bionic_pthread.rs`.

---

### 2026-06-12 — Core 1223806 root-caused (the SILENT death past the 947663 wall): missing bionic `dl_iterate_phdr` made every libroblox C++ throw uncatchable (std::terminate re-raise loop ×61,497) and the kill was a kernel `force_sigsegv()` onto an unwritable altstack — zero handler bytes ran (why BOTH reporters were silent); + Vibrator `UnsatisfiedLinkError` killed the main Looper pump; the 947663 fix RUN-PROVEN clean

**Context (owner live validation of `ddabcd7`, `/tmp/eclipse-947663-validate.log`, EXIT=139 → fresh core 1223806,
124.7 M, PID 1223806):** the boot got FURTHER than ever — guard-paged altstack banner (line 34), libbacktrace-native
pre-load fully CLEAN (`unresolved_strong=0`, 2 ctors, `JNI_OnLoad 0x10004`) with line 689's `Runtime.nativeLoad:
already pre-loaded by Eclipse's Rust loader — reporting success (apkenv skipped)` (ddabcd7 mechanisms 2+3
run-proven), the early ART first-chance signal-11 at 8.469 s book-kept and SURVIVED, Vulkan swapchain active
(line 626), then ~0.3 s of REAL deep work (WorkManager workers SUCCESS, JobScheduler, real curl HTTP, analytics
EventUploadJob) — and then a SILENT SIGSEGV: no tap banner, no crashpad `Fatal signal`, the log just ends.
Forensics on Eclipse's own core via the established-safe `coredumpctl`/`gdb -batch`/`eu-readelf` method (minimal
disassembly; scratch artifacts `/tmp/core1223806-*` + the 12.8 MB dying-thread stack image `/tmp/t1stack.bin`).

**947663 recurrence check — CLEAN (the cxa-before-keys fix HELD).** The §5 discriminator applied to core 1223806:
zero frames at `libroblox+0x2779cc4`, zero MAPERR addr=0x58, zero `__call_tls_dtors`/`pthread_exit`/cxa frames
across all 76 threads (grep = 0 hits over `/tmp/core1223806-{allbt,eustack}.txt`); the old death milestone (the
`b.<init>`/RbxStorage-DONE pair where core 866509's boot died at engine t=8.670) passed at t=8.769 with ~0.32 s of
dense multi-thread work beyond it and the tap never firing. The whole 06-12 fix chain (`__sF` → apkenv+altstack →
thread-exit ordering+pthread_atfork) is now fully live-validated; this boot's death is a NEW mechanism. Retention:
the 947663 keep-condition is satisfied — its core + `~/.cache/eclipse-forensics/core947663` are RELEASED; core
1223806 + `~/.cache/eclipse-forensics/core1223806` + `/tmp/core1223806-*` + `/tmp/t1stack.bin` take the
keep-until-validated slot.

**Mechanism 1 — CONFIRMED Eclipse-side, boot-blocking, FIXED: no bionic `dl_iterate_phdr` ⇒ FDE blindness ⇒ every
C++ exception thrown in libroblox is process-fatal.** libroblox's statically-linked libc++abi unwinder resolves
FDEs via its `dl_iterate_phdr@LIBC` import (`nm -D`: `U dl_iterate_phdr@LIBC`, plus `U dladdr@LIBC`,
`U sigaltstack@LIBC` — re-verified); Eclipse provided no such native (grep over `src/`: zero hits), so the import
fell through to HOST GLIBC, whose walk covers only glibc's own link map — Eclipse's anonymously-mmapped libroblox
is invisible (the in-stack callback cursor shows ~0x4f host modules visited, libroblox absent; frame #0 of the
dying thread is glibc `dl_iterate_phdr`). On the engine HTTP worker's DNS-failure error path (LWP 1226978 =
0x12b8e2 = the tid of the final engine log line `HttpResponse error:2 HttpError:DnsResolve
https://ecsv2.roblox.com/timespent/pbe`, log:728) the boot's first C++ throw entered phase-1 unwind → no FDE for
any libroblox PC → `std::terminate` → Roblox's terminate handler re-raises to classify → unwind fails again: a
3-frame cycle (`libroblox+0x2bfc7a8`/`+0x2bfc7c6`/`+0x6a9c881`, disassembly-matched field-for-field to libc++abi
`std::terminate`/`__terminate`, r12 = `kOurExceptionClass` `CLNGC++\0`) repeated **61,497 times** spanning
12.20 MB of the 24.4 MB stack at 208 B/iter — deterministically fatal by stack exhaustion even absent mechanism 2.
The consequence is GENERAL, not DNS-specific: under Eclipse, ANY engine throw (which Roblox uses routinely for
recoverable errors) was process-fatal; on real Android Roblox's own error handling catches the DNS-failure
exception. **Fix:** NEW `src/loader/module_registry.rs` — `BionicDlPhdrInfo` pins the 8-field bionic LP64
`dl_phdr_info` (verified against PUBLIC bionic `libc/include/link.h` via aosp-mirror BEFORE coding: identical to
glibc field-for-field, tail 4 fields API-30+, the size arg versions the struct); `ModuleRecord::for_image` derives
`dlpi_phdr` the bionic way (PT_PHDR p_vaddr first, else PT_LOAD file-range-containment translation; an uncovered
table is a typed Err, never a fabricated pointer) plus a sorted defined-dynsym table; `eclipse_dl_iterate_phdr`
walks Eclipse-mapped modules FIRST (full 64-byte size arg, adds/subs counters, tls_modid 0 — engine libs have no
PT_TLS) then delegates to host glibc with the caller's callback unchanged (typed extern decl, no transmute),
honoring the first-nonzero-rc stop; `eclipse_dladdr` (same-class, fixed together) resolves Eclipse-module
addresses (bionic containment rule: defined, `st_value <= a < st_value+st_size`, zero-size never matches) with
host `libc::dladdr` fallback for host PCs; `describe_address` is the attribution helper mechanism 2's ring
consumes. Wiring (`src/loader/engine.rs`): `map_resolve_app_lib` step 5 registers EVERY object of the kept-alive
`LoadedImageSet` BEFORE returning — i.e. before any engine instruction — and a NEW `impl Drop for LoadedEngine`
unregisters by load base (body drops before the field drops munmap the set), so the dedup-skip and error paths can
never leave records pointing at unmapped memory. Registered in `native_provider.rs` (new "bionic link-map
introspection (2)" section, dated core-1223806 rationale).

**Mechanism 2 — CONFIRMED (the silent kill; NOT a proven Eclipse logic defect — the Eclipse-side gap was
OBSERVABILITY, which is FIXED; the altstack OWNER stays a bounded unknown):** a signal arrived for the
terminate-looping thread mid-iteration at `dl_iterate_phdr+18`; the kernel's signal-frame setup targeted the
thread's registered `SA_ONSTACK` altstack, the WRITE FAULTED (stack unwritable), and the kernel `force_sigsegv()`'d:
NT_SIGINFO `si_signo=11, si_code=128 (SI_KERNEL), si_addr=0x0`, handler reset to SIG_DFL, instant whole-process
kill BEFORE any handler byte executed. That fully explains the total silence — tap, crashpad, and ART's chain never
received control (and the tap's engine-PC filter would have suppressed its banner anyway: RIP was in libc.so.6 —
answering the carried tap-filter question: extending dump-everything would NOT have changed this outcome; no
in-process observability can make a force_sigsegv self-report — the coredump IS the report for this class; a
bounded one-line record for non-engine-PC faults stays optional, not a fix). Proven by elimination: the interrupted
instruction (`mov %rdi,0x10(%rsp)`) wrote to a stack VMA the core proves RW (memsz==filesz, 24.4 MB) with 12.2 MB
headroom and pristine zeros below rsp (the kernel never targeted the writable current stack); all 76 threads show
uniform `sighold <3,10,13>` with empty sigpend — nobody mid-handler; a real memory fault would carry SEGV_MAPERR +
an address. BOUNDED UNKNOWNS (kernel-only state, not in the core): the inbound signal's identity (consumed at the
failed delivery; timeout-SIGTERM ruled out by timing/EXIT=139; no thread in raise/abort/tgkill) and WHO registered
the unwritable altstack — libroblox imports `sigaltstack@LIBC`, which Eclipse deliberately left host-baseline, so
Eclipse had ZERO attribution; lead suspect = an engine-registered altstack whose backing was later decommitted
(e.g. mimalloc `mprotect(PROT_NONE)` purge); ART's 32 KiB heap altstack would have been writable. **Fix
(observability only, per the evidence standard — no logic fix without a proven owner):** the host-baseline
`sigaltstack` is replaced by an Eclipse-owned translating native: NEW clean-room `src/loader/sigaltstack_shim.c`
(the established `liblog_shim.c` pattern — stable Rust has no spelling for `__builtin_return_address(0)`) captures
the caller and tail-calls `eclipse_sigaltstack_record` (`native_provider.rs`): a pure layout-identical forward to
glibc (bionic/glibc `stack_t` identical on x86-64), then a tracing log + 64-entry `AltstackRegistration` ring
(tid/ss_sp/ss_size/ss_flags/caller + caller module via the Eclipse module table then host-dladdr fallback; pure
queries record nothing; kernel rejections logged); accessors `recent_altstack_registrations`/
`altstack_registration_total`; provider signal natives 6→7. The stale "sigaltstack … stays on the host baseline"
comments are REWRITTEN with the dated core-1223806 finding (repo-wide grep: zero stale instances remain).
COVERAGE BOUNDARY (important for the next forensics): the ring sees only bionic-IMPORT-routed registrations —
Eclipse's own `install_guarded_altstack` calls `libc::sigaltstack` directly, and vendored ART's attach-time
altstack (the 866509 open item; this core's dying thread was ART-named "Main") goes straight to glibc — so an
EMPTY ring for a dying tid is itself the discriminator implicating a host-side/ART registrant, NOT evidence that
no registration happened, and NOT an attribution bug.

**Mechanism 3 — CONFIRMED Eclipse-side, boot-blocking, FIXED: `android.os.Vibrator.native_constructor()`
`UnsatisfiedLinkError` escaped `Looper.loop` — the main Looper pump was permanently dead 17 ms before the crash.**
InitHelper's `AsyncTask.onPostExecute` → `ContextImpl.getSystemService` constructed `Vibrator`; its unbound native
threw out of `Looper.loop` (log:691 `java_vm_ext.cc:1130 No implementation found for int
android.os.Vibrator.native_constructor()`; log:709–710 `framework lifecycle step failed step="pump Looper.loop"` +
`main Looper pump failed` at 22:31:28.993) — from that point no main-thread Handler continuation was ever
dispatched again, so splash init was unreachable that boot regardless of the crash. The loud pump failure is
CORRECT behavior (it surfaced the gap); the defect was the missing framework native — the established
discovery-loop class. **Fix (`src/framework.rs`):** the vendored ATL `Vibrator.java` read FIRST — its FULL declared
native list is exactly two: `native_constructor()I` + `native_vibrate(IJ)V` (both instance; independently verified
against the ACTUAL boot artifact `framework-patched/api-impl.jar` classes2.dex). `vibrator_native_constructor`
returns −1 = the documented no-vibration-device constant (ATL's own C backing returns −1 when `/dev/input` has no
motor; the Java class gates on `fd != -1` itself) — intentional capability handling on a desktop host, dated, NOT a
fall-through stub; `vibrator_native_vibrate` is a logged no-op (bound so NO path through the class can ever
re-surface an `UnsatisfiedLinkError` out of `Looper.loop`). `register_vibrator_natives` is wired into
`drive_lifecycle` after `register_connectivity_natives`, before step 1. Deliberately NO catch-and-continue in
`pump_main_looper` — the loud failure stays the regression signal for this class.

**STRAWTOGRASP / `SocketImpl.delegate` NULL-jfieldID hypothesis — CONTRADICTED, CLOSED (no fix at any layer;
supersedes the §5 "ART/libcore networking-internal miss" note, now bracketed):** ruled out by source + artifact +
log + core. The print is Eclipse's prior instrumentation in vendored ART `GetFieldID`
(`art/runtime/jni/jni_internal.cc:1649`); the SOLE `delegate` caller in the entire vendored tree is wolfssljni's
`Java_com_wolfssl_WolfSSLSession_setFd` (`com_wolfssl_WolfSSLSession.c:286–311`) — an upstream dual-shape
portability probe for OpenJDK-13+ `DelegatingSocketImpl` that NULL-checks the fid, ExceptionClears the EXPECTED
`NoSuchFieldError` ("we expect it to happen"), and falls back to `Socket.impl`; the NULL fid is never dereferenced
and the fallback fd lookups exist in the vendored libcore. Class shape: `java.net.SocketImpl` has exactly one
definer (boot-classpath `core-oj-hostdex.jar`, androguard-proven, no `delegate` anywhere in the hierarchy); the
framework overlay + APK multidex define ZERO `java.net` classes — no shadowing, no dex-order contest. The line
occurred 4× this log (3 followed by hundreds of healthy lines) and in the prior 6/6-clean boots; the dying thread
has zero JNI/wolfssl frames — occurrence #4 merely timestamps the EventUploadJob's TLS setup in flight on a
parallel thread at kill time. Never re-chase this line as a death marker.

**Engine curl DNS NXDOMAIN — RECLASSIFIED: NOT environmental; suspected Eclipse resolver-ABI gap (UNPROVEN,
diagnostics-first, deliberately NO code per the `__sF` discipline):** the brief's "host has no network/DNS for it"
framing is CONTRADICTED — the host resolves `ecsv2.roblox.com` (live `getent` → 128.116.95.3 via
systemd-resolved) and the SAME PROCESS completed a real Roblox HTTPS round-trip on the Java/okhttp/wolfSSL path
7 s before the engine curl failed (`Network won the race`/`Network payload stored` log:566/568 vs the engine
`Could not resolve host` at log:727; telemetry-only this boot — settings loaded via the Java path, boot progress
was not network-blocked). Suspected mechanism: libroblox imports `getaddrinfo`/`freeaddrinfo`/`gai_strerror`/
`getnameinfo@LIBC` (nm-verified) and Eclipse provides none, so the bionic-compiled resolver caller runs against
glibc — and bionic vs glibc publicly diverge on `struct addrinfo` field order (`ai_canonname`/`ai_addr` swapped),
`AI_*` flag values (bionic `AI_ADDRCONFIG` 0x400 == glibc `AI_NUMERICSERV`), and `EAI_*` signs (bionic positive,
glibc negative) — the proven `__sF`/`__fread_chk`/sigaction shape-mismatch class on a confirmed import surface.
UNPROVEN links: whether libroblox's curl actually calls `getaddrinfo` for this request (vs a bundled resolver such
as c-ares) and which divergence yields the symptom. Next-session diagnostics: re-verify the bionic `netdb.h`
divergences from public AOSP; minimal nm/strings scan for a bundled resolver (`ares_*`); if the getaddrinfo path
is confirmed, bionic-shaped translating `getaddrinfo`/`freeaddrinfo` (+ `EAI`/`AI` translation; `getnameinfo` is
struct-free) is the root-cause fix AND doubles as the attribution diagnostic. This WILL matter once gameplay
traffic runs on the engine path.

**Untriaged non-blocking boot observations (recorded, evidence-first — measure before binding anything):** the
~7.4 s `ActivitySplash.onCreate` stall bracketed by 4× `Resource is not a Drawable (color or path)` WARNs
(string/file-path drawable gap — profile where the time goes first); the `AssetManager.destroy()` STUB →
compression-dictionary `readAsset` IOException REGRESSION vs 06-11 (it worked fully then — diff the
`asset_registry` lifecycle around `destroy()`); the `Couldn't find any tzdata file!` ART env gap (sibling of the
known `java.time` BootstrapMethodError — likely ANDROID_TZDATA/ART env wiring); `CrashLibFileHelper`'s
nativeLibraryDir miss (Java-side symbolication config — matters once crash reporting is exercised).

**Same-pattern audits:** (module-introspection-blindness class) `nm -D` over ALL 12 cached engine libs:
`dl_iterate_phdr` is imported by libroblox, libbacktrace-native, libeigen_blas AND librenderscript-toolkit — ONE
provider-side fix covers all four by construction (the `__sF` precedent shape); `dladdr` libroblox-only; zero
`dlvsym`/`android_dlopen_ext`/`dl_unwind_find_exidx` imports anywhere. The remaining dl-family imports
(dlopen/dlsym/dlclose/dlerror) are the LOADING class, not PC/module introspection — the pre-existing recorded
host-baseline dlfcn gap (`bionic_env.rs`'s `Dl` classification now carries a dated note separating the fixed
introspection pair from the open loading gap). One adjacent path recorded, not changed: `init_run.rs`'s
`__run-libroblox-init` diagnostic harness maps libroblox via its own Linker pipeline WITHOUT registry records — a
throw there still hits the blind walk (diagnostic-only, `_exit(0)`s, and the 3,427 ctors are run-proven
non-throwing); register-or-document when next touched (carried note (a)). (stale-comment class) repo-wide grep:
zero remaining "stays on the host baseline" sigaltstack claims; `install_guarded_altstack`'s core-866509 comments
verified still accurate (they describe the INSTALL, not the import routing). (getSystemService ctor-natives class)
all 27 service classes ATL's `ContextImpl.getSystemService` constructs were audited: Vibrator is the ONLY one
whose CONSTRUCTOR invokes a native; six others (WindowManagerImpl, ClipboardManager, SensorManager,
ConnectivityManager, NotificationManager, LocationManager) declare method-level natives only, which surface as
their own discovery-loop lines if ever reached — bind on run evidence, no speculative pre-binding.

**Regression pins (existing style — no scripts; the new module's tests live in it):** `module_registry::tests` —
`dl_phdr_info_layout_matches_glibc` (THE ABI pin: `BionicDlPhdrInfo` size 64 + all 8 field offsets vs
`libc::dl_phdr_info` — a drift scrambles the engine unwinder's view),
`for_image_derives_phdr_addr_via_pt_phdr_then_pt_load` (PT_PHDR-wins / PT_LOAD-containment / uncovered-is-Err),
`dladdr_lookup_finds_containing_module_and_symbol` (bionic containment incl. zero-size-never-matches),
`eclipse_dladdr_falls_back_to_host_for_host_pcs`, `describe_address_names_module_plus_offset`; `link::tests` —
`module_registry_enumerates_loader_mapped_and_host_modules` (NEW, the required collecting-callback pin: a REAL
Linker-mapped fixture module enumerated with correct base/name/phnum + a DEREFERENCED mapped phdr entry + the full
API-30+ size arg AND ≥1 host module via the glibc delegation; rc-stop contract) and
`real_libroblox_eclipse_natives_fully_resolve_all_imports` EXTENDED with the fail-closed host-shadowed pin —
`dl_iterate_phdr`/`dladdr`/`sigaltstack` must resolve through the full Eclipse scope to the EXACT Eclipse-provider
addresses, with `host_only.resolve(name).is_some()` proving each pin load-bearing (these names were never on the
88 work-list — the 947663 no-allowlisting lesson; ran LIVE against real libroblox: 88-baseline / 0-work-list /
623-applied unchanged); `native_provider::tests` — `sigaltstack_native_forwards_and_records_caller_attribution`
(registers through the C shim, kernel round-trip via `sigaltstack(NULL,&ss)`, attribution names this tid + a
shim-captured caller resolved to a module, pure query records nothing, full save/restore hygiene) + the
presence/count test UPDATED (125 base + 53 pthread + 5 sysconf = 183; the 3 new names listed); `framework::tests`
— `vibrator_native_names_sigs_and_class_match_vibrator_java` (class/name/sig pins for BOTH declared natives = the
full-list count pin, matched against the exact ART-reported line at log:691).

**Gate (run on this exact tree; genuine rebuilds forced via `cargo clean -p eclipse` / `--release` per gate
precedent; logs `/tmp/gate-{build,clippy,test,release}.log`):** fmt --all (+ `--check`) / build --all-targets /
clippy `-D warnings` / test / release — **535 unit + 4 integration + 2 doctests = 541 passed, 0 failed**, 0 SKIP
(the 4 milestone tests ran their LIVE milestone subprocesses against the default APK + displays — this host's
documented norm; none boots ART; the constructor milestone matched `ALL 3427/3427 constructors completed without a
crash`), all 0-warning. No live ART/bionic boot was run in the workflow (dev-host boundary respected); the live
validation expectations are the §5 START-HERE.

**Docs ledger:** `docs/bionic-env-worklist.md` brought current in this commit (closes the 947663 entry's carried
note (d)): the scope note now records the beyond-list growth through `pthread_atfork`/`ANativeWindow_getFormat`
(122/180) to the 3 new natives (**125 base / 183 total**), and the stale cxa-runtime / dl table rows carry dated
brackets (`__cxa_thread_atexit_impl` Eclipse-tier since core 947663; `dl_iterate_phdr`/`dladdr` Eclipse-owned
since core 1223806; dlopen-family = the open loading gap).

**Carried non-blocking review notes (recorded, not acted on):** (a) the `init_run.rs` harness registry gap above —
register `ModuleRecord`s there (the image lives until `_exit`) or add a dated deliberate-skip comment when next
touched. (b) `module_registry.rs`'s MODULES doc overclaims std `RwLock` recursive-read safety (std disclaims it: a
queued writer can deadlock a re-entrant read); exposure is theoretical (no realistic unwinder callback re-enters;
write windows are boot-time pushes) — correct the comment (or walk a snapshot) when next touched; the same lock is
reachable from crashpad-style in-handler unwinds (bionic/glibc loader-lock-equivalent semantics, not worse — check
this lock first in any future hung-crash-dump triage). (c) `dladdr_lookup`'s `s.value + s.size` is an unchecked
u64 add over APK-supplied dynsym fields — a crafted symbol panics in DEBUG builds across the FFI boundary (release
wraps fail-safe); switch to `checked_add` when next touched. (d) `for_image` trusts PT_PHDR's `p_vaddr` without
span-containment validation (bionic-identical trust; the real artifact is well-formed — PT_PHDR at 0x40 inside the
first R E PT_LOAD); a one-line guard or docstring softening when next touched. (e) Eclipse-modules-first walk
order: callbacks no longer see the main executable as entry 0 (glibc/bionic report it first) — order-independent
for unwinders; document or flip to host-first only on evidence of a consumer that cares. (f) Two
`dlpi_adds`/`dlpi_subs` counter domains (Eclipse vs glibc) ⇒ LLVM libunwind's FrameHeaderCache flushes on every
Eclipse→host boundary crossing — perf-only, the code comment already owns "at worst flushes a cache". (g)
`eclipse_sigaltstack_record`'s FAILURE path logs via tracing before returning, which can clobber errno for a
caller checking it after −1 — save/restore errno around the log when next touched. (h) the record fn is NOT
async-signal-safe (Mutex/tracing) — documented assumption: sigaltstack is thread-setup-only on every observed
engine path (crashpad re-registers ACTIONS in-handler, never stacks); if a future core shows a thread parked in
this lock during signal handling, move to an atomics ring (the tap pool pattern). (i) Vibrator: both natives
register in ONE grouped `?`-propagated bind; `native_vibrate (IJ)V` is dex-verified this session but not yet
run-proven — if the owner's live validation ever surfaces a NoSuchMethodError on the grouped bind, split
per-native (ctor mandatory, vibrate best-effort — the 06-11 `readAsset` precedent). (j) the fail-closed three-name
pin lives in the APK-gated `real_*` test, so APK-less checkouts are guarded only by the presence/count test —
convention-consistent; hoist an ungated scope-priority test only if it ever matters. (k) the
`#[allow(clippy::type_complexity)]` justification comment lacks the §2.2 dated format — date it on next touch.

### 2026-06-13 — EXIT=10 root-caused (the first ZERO-native-fault boot — the native-crash ladder is climbed): unbound `Activity.nativeStartActivity`/`nativeFinish` consumed the splash→main Looper messages, and unbound `AssetManager.openAssetFd` turned profileinstaller's benign no-profile path into a process-fatal worker `UnsatisfiedLinkError` → `System.exit(10)`; + `Process.getElapsedCpuTime`, Log/Process registration ordering, and the reserved netdb resolver-ABI gap CONFIRMED + FIXED

**Context (owner live validation of `54153e1`, `/tmp/eclipse-1223806-validate.log`, EXIT=10, NO coredump,
~19:13 CDT 2026-06-12):** ZERO SIGSEGV/SIGABRT — the 06-12 4-core fix chain (782252 `__sF` → 866509
apkenv+altstack → 947663 cxa-before-keys → 1223806 dl_iterate_phdr/dladdr + Vibrator) is fully RUN-PROVEN this
boot: the engine CAUGHT its own C++ exceptions (DNS HttpError throws logged and survived — previously the first
throw was the 61,497-iteration std::terminate death), ~6 s of deep work (swapchain active log:561, ENGINE-logged
mimalloc options log:590, WorkManager/JobScheduler running), and the death was Java-level and clean
(`OpenjdkJvm.cc:314] System.exit called, status: 10`, log:757). Forensics were LOG + DEX + SOURCE based (no
coredump existed): androguard over the ACTUAL boot artifact `~/.cache/eclipse/framework-patched/api-impl.jar`
(classes.dex + classes2.dex), `nm`/`readelf` import enumeration, vendored-ART/libcore source, host headers. No
live ART/bionic boot was run in the workflow (dev-host boundary respected).

**Mechanism 1 — CONFIRMED Eclipse-side, boot-blocking, FIXED: `android.app.Activity.nativeStartActivity` unbound
⇒ the splash→ActivityNativeMain handoff irrecoverably consumed.** The splash's `finish()` was the NORMAL
NEW_STARTUP transition, not an error bail — dex-proven order in `com.roblox.client.startup.ActivitySplash`:
`s()` → `Z0(false)` → `Context.startActivity(ActivityNativeMain intent)` FIRST, then `overridePendingTransition`,
then `finish()` (twice — Z0's and s()'s; log:708–711 corroborates). ATL's `Context.startActivity` posts
`Context$6` to the main Looper; Eclipse's pump dispatched it; `Context$6.run` successfully constructed
ActivityNativeMain via `internalCreateActivity` (its Window wraps the SAME process-shared `window_registry`
handle as `Application.native_window`) and then hit the unbound STATIC `nativeStartActivity(Activity)` →
`UnsatisfiedLinkError` (an Error — escapes Context$6's Exception-typed catch) escaped `Looper.loop`
(log:713–721); the pump's `checked()` described+cleared it, but `Looper.loop` had already DEQUEUED the message,
so the transition was consumed: the engine-hosting activity never reached onCreate — the boot dead-ended on a
stranded splash even absent the exit(10). Eclipse bound ZERO of Activity's declared natives (grep-proven; the
ACTIVITY_CLASS was used only by recipe steps 4–7). Artifact re-enumeration this session (androguard over
api-impl.jar classes2.dex): EXACTLY 7 declared natives — `nativeFinish (J)V` 0x102, `nativeOpenURI` 0x109,
`nativeResumeActivity (Ljava/lang/Class;Landroid/content/Intent;)Z` 0x109, `nativeStartActivity` 0x109,
`isInMultiWindowMode ()Z` 0x101, `isTaskRoot ()Z` 0x101, `nativeFileChooser (ILjava/lang/String;Ljava/lang/
String;I)V` 0x101; the prior survey's 8th entry `moveTaskToBack` is FALSIFIED (not native). No api-impl Java
drives a started activity's up-lifecycle — the native owns it (xref-swept); Eclipse's exact equivalent is its
existing recipe steps 5–7. **Fix (`src/framework.rs`):** new `register_activity_natives` (one RegisterNatives on
ACTIVITY_CLASS, wired into drive_lifecycle's registration block before `register_view_natives`); drive_lifecycle's
inline steps 5–7 factored into shared helpers (`call_activity_on_create`/`on_start`/`on_resume` +
`drive_activity_down_lifecycle`), labels/log placement unchanged; `nativeStartActivity` drives the passed,
already-constructed Activity through exactly steps 5–7 via the helpers, exceptions described+cleared per
`checked()`, never left pending across the native return; a new `TRACKED_ACTIVITIES` tracker (creation-order
Global refs + finished flag; drive_lifecycle tracks the step-4 launcher activity) backs
`nativeResumeActivity`/`isTaskRoot`/the finish dedupe.

**Mechanism 2 — CONFIRMED Eclipse-side, boot-blocking, FIXED: `Activity.nativeFinish` unbound ⇒ splash
down-lifecycle lost (thrown 2× by design of the Java side).** ATL's `Activity.finish()` posts `Activity$2`;
its `run` guards on `window.native_window != 0`, calls the unbound INSTANCE `nativeFinish(window.native_window)`
and zeroes the field only AFTER the native returns — so the first `UnsatisfiedLinkError` skipped the zeroing and
the second queued `Activity$2` threw identically (log:722–741, two identical stacks + pump error pairs; the pump
then ticked clean ~5.9 s — no spin, no permanent death). The jlong IS the Eclipse `window_registry` handle shared
process-wide. **Fix:** `nativeFinish (J)V` bound: validates the jlong via `window_registry::with_window`
(generation-checked; stale handle logs and returns), drives the finishing instance onPause→onStop→onDestroy via
the same `checked()` discipline, gated by `mark_activity_finished_once` (the dex-proven double-post means a
same-boot double call is reachable), and MUST NOT free the handle or close the host window — the handle is shared
with ActivityNativeMain's Window (Java zeroes only the finishing activity's field).

**Mechanism 3 — CONFIRMED Eclipse-side, boot-blocking, FIXED: `AssetManager.openAssetFd` unbound — the ACTUAL
EXIT=10 trigger.** ~5.1 s after the Looper failures, pool-20-thread-1 ran androidx.profileinstaller →
`AssetManager.openFd("dexopt/baseline.prof")` → `openFd_internal` → the unbound `openAssetFd
(Ljava/lang/String;I[J[J)I` → `UnsatisfiedLinkError` (log:742–757). profileinstaller's designed no-profile path
catches only the IOException a real openAssetFd raises; the unbound native converted that benign path into an
uncaught `java.lang.Error` on a worker → the vendored-libcore default UncaughtExceptionHandler
(`Thread.java:1832–1839` `hacky_uncaught_exception_handler`, verified verbatim — banner + printStackTrace +
`System.exit(10)`) fired on the same tid. Exit 10 is ATL/vendored-libcore semantics (no Roblox dex exits 10 —
xref-swept). INDEPENDENT of mechanisms 1–2: fixing only the Activity natives still exits 10 at +5 s; fixing only
this still strands the boot on a dead splash. Re-confirms the recorded rule: under the vendored libcore EVERY
uncaught worker exception is process-fatal. **Fix (REAL implementation, per policy and the AOSP contract):**
`openAssetFd` registered alongside the existing AssetManager bindings (5→6, per-native best-effort); per call
`asset_fd_for` resolves the APK entry; exists AND Stored → a FRESH fd on the APK file (`File::open` →
`into_raw_fd`, ownership transfers to Java per the ParcelFileDescriptor wrap — never a shared/duped cached fd),
`outOffsets[0]`=`data_start()`, `outLengths[0]`=uncompressed size written BEFORE returning the fd; absent →
negative (Java throws the designed, CAUGHT FileNotFoundException); compressed → negative (AOSP's own openAssetFd
refuses compressed assets). Every failure path is an explicit −1 with exceptions described+cleared and the
un-transferred fd closed (jint's default 0 is a VALID fd — stdin — so the body never error-propagates to
`LogErrorAndDefault`). Covers `openNonAssetFd` (shared `openFd_internal`). New small general accessor
`Apk::entry_span(name) → {data_start, uncompressed_size, stored}` in `src/apk/mod.rs` (zip 2.x `data_start()`,
verified non-panicking-after-`by_name` in the vendored crate source; absent → typed `ApkError::EntryMissing`),
generalizing the X8664Engine-only stored flag. APK content evidence: 2.721.1108 has NO `dexopt/baseline.prof`
(absent → caught FNF, like real Android); the 2.724.735-merged APK has both `.prof`/`.profm` Stored — the real fd
path is servable.

**Mechanism 4 — CONFIRMED Eclipse-side, not boot-blocking, FIXED: `Process.getElapsedCpuTime` unbound.** 4
consecutive caught misses on a worker (log:634–637, no stack/no UEH banner — Roblox telemetry caught it); the
engine warned `process timestamps will be inaccurate` (log:50) — boot-long telemetry data loss, and per the
worker-fatal rule any future uncaught call site is exit-10. **Fix:** new `register_process_natives` on
`android/os/Process` binding `getElapsedCpuTime ()J` STATIC — `clock_gettime(CLOCK_PROCESS_CPUTIME_ID)` →
saturating ms, 0 on clock failure (the AOSP `android_util_Process.cpp` contract). The other 23 declared Process
natives stay discovery-loop items (Simplicity First — no run evidence).

**Mechanism 5 — CONFIRMED Eclipse-side, not boot-blocking, FIXED: `Log.println_native` registered too LATE
(ordering, not a missing native).** `run_apk` calls `preload_app_native_libs` (where libroblox's JNI_OnLoad runs)
BEFORE `drive_application_lifecycle`, and `register_log_natives`'s only call site was inside drive_lifecycle — so
the engine's one JNI_OnLoad-time Log call (LoggingProtocol init) missed (log:49–51), compounding mechanism 4 on
the same machinery. **Fix:** new `pub fn register_engine_preload_natives(&Vm)` (attaches + registers Log +
Process, catch_unwind-guarded) called in `run_apk` BEFORE `preload_app_native_libs`; drive_lifecycle keeps both
registrations (RegisterNatives re-registration of identical fnPtrs is spec-legal and idempotent) so neither path
can regress the other. Audit: only Log (observed) and Process (same code path) have engine-JNI_OnLoad-time-caller
evidence; the entry point grows on run evidence only.

**Mechanism 6 — main-Looper pump exception contract RULED CORRECT — deliberately NO change.** Log-proven this
boot: each tick drives `Looper.loop()` once via `checked()` (describe+clear+Err); graphics logs and keeps
ticking; exactly 3 error pairs bracketing the 3 throws ~1.4 ms apart, then clean ticks for ~5.9 s — the pump
neither spins nor dies. The cost (a throwing message is irrecoverably lost — already dequeued) is inherent; on
real Android the same escape kills the whole app. NO catch-and-continue/retry/replay — that would mask exactly
the missing-native class this boot surfaced (three natives found BECAUSE the failures were loud). The loud error
pair remains the regression signal; binding the natives is the root-cause fix.

**Mechanism 7 — the engine resolver-ABI gap CONFIRMED Eclipse-side + FIXED (closes the 06-12 entry's reserved
diagnostics):** host DNS healthy (getent resolves `ecsv2.roblox.com`) and the SAME process's Java/okhttp/wolfSSL
path completed real Roblox HTTPS round-trips in all 4 validation logs, while the engine curl path failed 2/2
times ever reached (zero successes in any log). libroblox imports plain POSIX
`getaddrinfo`/`freeaddrinfo`/`gai_strerror`/`getnameinfo`/`gethostbyname@LIBC` (nm re-verified; NO
`android_getaddrinfofornet`/`android_res_*` — netd ruled out); its curl uses the threaded getaddrinfo resolver
(exact failf string present; zero `ares_*` — the c-ares UNPROVEN link closed). Eclipse provided none → the
HostDlsymProvider glibc fall-through (the proven `__sF`/dl_iterate_phdr class). Deterministic public-ABI break,
headers pinned both sides: bionic `addrinfo` tail = BSD order `ai_canonname`@24/`ai_addr`@32 (public AOSP
netdb.h) vs glibc `ai_addr`@24/`ai_canonname`@32 (host `/usr/include/netdb.h:565–575`) — SWAPPED, so the
bionic-compiled walker reads glibc's canonname slot (NULL) as `ai_addr` → zero usable addresses →
CURLE_COULDNT_RESOLVE_HOST, exactly the logged non-crashing failure. Over-determined by AI_* value aliasing
(bionic ADDRCONFIG 0x400 == glibc NUMERICSERV; bionic 0x200/0x800 invalid to glibc → EAI_BADFLAGS), the EAI_*
sign flip (bionic positive vs glibc negative — also breaks Roblox's own EAI_AGAIN retry classification), and the
NI_* scramble — deterministic failure under EVERY flag combination, matching 0-successes-ever. Residual unknown:
the exact ai_flags at the curl call site (which divergence fires first) — the fix's own `eclipse.netdb` trace
closes that observation. **Fix (`src/loader/native_provider.rs`, the sigaction/`__sF` translating-native
pattern, new "bionic netdb resolver ABI (4)" section):** `eclipse_getaddrinfo` (AI_* translated BY NAME — struct
head 0–16 identical; host call; deep-copy into single-malloc-block bionic-shaped nodes; glibc-negative EAI →
bionic-positive; EAI_MEMORY unwind on alloc failure; errno save/restore around the failure trace; tracing target
`eclipse.netdb` records node/service/ai_flags/family + outcome — the reserved attribution diagnostic);
`eclipse_freeaddrinfo` (frees Eclipse's own chain ONLY, never forwarded to glibc); `eclipse_gai_strerror` (static
table keyed by bionic-positive codes); `eclipse_getnameinfo` (sockaddr passthrough — layouts identical on Linux
x86-64 — with NI_*/EAI translation). `gethostbyname` stays host-baseline (hostent field order identical — dated
record-only comment). Importer audit: exactly 2 of 12 cached engine libs import the family (libroblox,
libbacktrace-native) — one provider-side fix covers both. Counts: provider **125→129 base / 183→187 total**;
`docs/bionic-env-worklist.md` scope-note chain extended and the bionic-libc category row carries the dated
netdb bracket. Not boot-blocking this boot (caught, telemetry-only) but it gates ALL engine-path content —
gameplay traffic, RUPP/TURN transport.

**Fixed vs recorded-only:** FIXED = mechanisms 1–5 + 7 above. RECORDED, deliberately NOT bound:
`nativeOpenURI (Ljava/lang/String;)V` 0x109 + `nativeFileChooser (ILjava/lang/String;Ljava/lang/String;I)V`
0x101 (not tripped this boot; both need real host-action design — a detected URI opener / file dialog — and a
no-op would be a workaround; unbound they surface loudly through the pump signal — the next discovery items);
the other 23 `android/os/Process` declared natives (no run evidence); the prior entry's untriaged items carry
unchanged (AssetManager.destroy() → readAsset IOException regression, tzdata gap, splash onCreate stall +
Drawable WARNs, benign ClientAppSettings.json FNF reads, ART attach-time engine-thread altstacks).

**Same-pattern audits:** (full-declared-list discipline) Activity re-enumerated from the ACTUAL boot artifact —
exactly 7, `moveTaskToBack` falsified; no path through the transition dispatch (Context$6/Activity$2) remains
unbound. (worker-fatal UnsatisfiedLinkError class) Process (24 declared) + AssetManager enumerated; openAssetFd
was the only unbound fd-path native (openNonAssetFd shares openFd_internal); the existing
openAsset/readAsset/seekAsset/getAssetLength/destroyAsset bindings re-confirmed against the artifact.
(registration-ordering class) drive_lifecycle's full registration block swept — only Log + Process have
engine-JNI_OnLoad-time evidence. (host-shadowed fall-through class) the 4 netdb names joined the fail-closed
link.rs pin exactly like dl_iterate_phdr/dladdr/sigaltstack; gethostbyname/inet_ntop/inet_pton audited
shape-identical (record-only). (stale-comment class) provider doc-comment "122 symbols total"/"signal-ABI 6"
corrected; register_asset_stream_natives doc updated; repo grep for stale "provides none"/"resolver-ABI gap"
claims in src/ → zero. (pump) NO changes — mechanism 6 ruling.

**Regression pins (existing style — no scripts):** `framework::tests` —
`activity_native_names_sigs_and_class_match_api_impl_dex` (class + all 5 bound name/sig pins vs the artifact
7-list + the two exact ART-reported gaps; records the 2 unbound),
`process_native_name_sig_and_class_match_api_impl_dex`,
`engine_preload_natives_entry_point_exists_and_covers_log_and_process` (compile-shape pin; the live signal = the
WARN disappearing), `asset_manager_stream_native_names_and_sigs_are_the_classic_aosp_set` EXTENDED (openAssetFd),
`asset_fd_for_serves_stored_entries_and_refuses_absent_and_compressed` (zip fixture: the (fd,offset,length)
triple read back THROUGH the returned fd; absent→EntryMissing; compressed→Compressed); `apk::tests` —
`entry_span_reports_stored_offset_size_and_rejects_absent` (data_start bytes ARE the Stored asset);
`native_provider::tests` — `bionic_addrinfo_layout_is_bsd_order_and_differs_from_glibc` (THE ABI pin: all 8
bionic offsets + size 48 AND the glibc swap proven via `libc::addrinfo` offsets),
`bionic_ai_ni_eai_translation_tables_match_both_headers` (incl. the 0x400 aliasing hazard),
`bionic_getaddrinfo_returns_bionic_shaped_nodes_and_positive_eai` (offline live lookup: 127.0.0.1 numeric-host →
non-NULL `ai_addr`@32 + deep-copied canonname@24; invalid name → bionic-positive EAI_NONAME=8; free round-trip),
`bionic_getnameinfo_translates_flags_and_returns_numeric_host` (raw bits would mean NAMEREQD — proves the
translation load-bearing), count test → 129/187 with the 4 names; `link::tests` —
`real_libroblox_eclipse_natives_fully_resolve_all_imports` EXTENDED to 7 fail-closed host-shadowed pins (ran
LIVE against real libroblox this session: 88-baseline/0-work-list/623-applied unchanged, all 7 pins green).

**Gate (this exact tree):** fmt --all (+ `--check`) / build --all-targets / clippy `-D warnings` / test /
release — **544 unit + 4 integration + 2 doctests = 550 passed, 0 failed, 0 SKIP** (the 4 milestone tests ran
their live milestone subprocesses — this host's documented norm, none boots ART;
`run_libroblox_init_runs_all_3427_constructors` green WITH the new netdb provider entries in the resolution
scope), all 0-warning. No live ART/bionic boot in the workflow; the live validation expectations are the §5
START-HERE.

**Carried non-blocking review notes (recorded, not acted on):** (a) `activity_native_finish` skips the
down-lifecycle + finished-marking when handle validation fails — but dex proves handle 0 is a LEGITIMATE shape
(`Activity$3`/`recreate()` calls `nativeFinish(0)` before `nativeStartActivity(new)`): under Eclipse a recreate()
would skip the old instance's down-lifecycle and leave it "live" in the tracker; when next touched, run the
dedupe+down-lifecycle regardless of handle validity (warn only for a NONZERO invalid handle) and update the
contract comment to name both Activity$2 and Activity$3 caller shapes. (b) `asset_fd_for` unconditionally
prepends `assets/`, but `openNonAssetFd` passes RAW zip paths (e.g. `res/raw/…`) through the same native — those
would mis-root to `assets/res/…` and always miss; no observed caller today (profileinstaller goes through
openFd); try the literal name first when next touched. (c) `asset_manager_open_asset_fd` resolves via
`LogErrorAndDefault`, whose caught-PANIC path returns jint::default()=0 — a VALID fd (stdin); the body's Err
half is closed (explicit −1 everywhere) and release is panic=abort, so exposure is dev-build panics only; if a
panic-capable call is ever added, restructure so the resolved default is −1 (local ErrorPolicy/newtype). (d)
`TRACKED_ACTIVITIES` is append-only — finished entries keep their Global refs for the process lifetime (pins the
destroyed Activity graph against GC); bounded today (2 activities; the dedupe gate needs the tombstone); the doc
clause "released when the entry is dropped" is slightly misleading (entries never drop) — prune-with-tombstone
if activity churn ever becomes real. (e) `eclipse_getaddrinfo` forwards NULL hints straight to glibc, whose
documented GNU default is `AI_V4MAPPED|AI_ADDRCONFIG` vs bionic's zero-flags NULL-hints behavior — pass an
explicit zeroed hints struct on NULL when next touched (curl always passes non-null today; host-config-dependent
silent divergence). (f) `engine_preload_natives_entry_point_exists_and_covers_log_and_process` is a
compile-shape pin only — it cannot catch run_apk dropping the pre-preload CALL; the live WARN line is the real
ordering signal. (g) evidence-pinned-minimal lifecycle contracts, revisit on run evidence: `nativeResumeActivity`
drives only onResume (a stopped-but-unfinished instance would skip onRestart/onStart), drops the Intent (no
onNewIntent — deliberately unimplemented, not evidence-pinned), and returns true even if onResume threw (false
would make Java construct a DUPLICATE activity); `nativeStartActivity` resumes the new activity BEFORE the
finishing splash's onPause (Android pauses old-first) — unobservable for the current single-stack boot. (h)
comment-wording: the openAssetFd constants comment says "vendored-libcore default uncaught handler" where the
Process section names `hacky_uncaught_exception_handler` — the SAME handler (vendored libcore
`Thread.java:1832–1839`); align the wording on next touch so nobody hunts for two handlers. (i) the netdb live
fix is necessarily unvalidated until the next owner boot — on any remaining DnsResolve, the `eclipse.netdb`
trace line (the engine's actual resolver arguments) is the first thing to read.

- **2026-06-13 (View.native_get_window → view-tree path) — 🪟 Bound `android.view.View.native_get_window` (+ the
  next-on-path `ViewTreeObserver.native_set_have_global_layout_listeners`) and CAPTURED the real Java `Window`
  object in `window_registry`, closing the last evidence-pinned blocker on `ActivityNativeMain.onCreate`.**
  *Confirmed root cause (first-party evidence — the owner exit-10 boot stack, `/tmp/eclipse-exit10-validate.log`,
  EXIT=124 clean):* the EXIT=124 boot was indefinitely stable (splash→ActivityNativeMain transition, real Roblox
  HTTPS flag-fetch, zero native faults) with ONE remaining gap — `ActivityNativeMain.onCreate` →
  `com.roblox.client.ActivityNativeMain.d1()` → `android.view.View.getViewTreeObserver()` threw
  `UnsatisfiedLinkError: No implementation found for android.view.Window android.view.View.native_get_window(long)
  (tried Java_android_view_View_native_1get_1window and ...__J)`. ATL's `getViewTreeObserver()` calls the instance
  native `native_get_window(widget)` to obtain the Window that owns the view tree's `ViewTreeObserver`. Two coupled
  gaps made this fail: (1) the native was undeclared/unbound; (2) Eclipse held NO real Window object to return —
  `window_registry::WindowState.jobject` was a presence-only `Option<()>` and `Window.set_jobject` only recorded
  `Some(())` (the documented placeholder slot). *Fix (both root causes, smallest necessary change):*
  **`src/framework/window_registry.rs`** — `jobject` is now `Option<Global<JObject<'static>>>` (a captured JNI
  global ref; `Global` is `Send`, lives soundly in the process-global slab, and its `Drop` releases the ref on
  `free`/replacement — mirrors the proven `view_registry::ViewState.jobject` triple), with `set_jobject(handle,
  Global)` / `with_jobject(handle, f)` accessors (handle bounds+generation validated, so a stale handle is a typed
  `Err`, never UB) and a lock-free `ACTIVE_WINDOW: AtomicI64` + `active_window()` (mirrors
  `view_registry::ACTIVE_ROOT`) published by `allocate()` / cleared by `free()` via `compare_exchange` (only when it
  still names the freed handle — preserves the one-window-per-launch invariant under a superseding allocate).
  **`src/framework.rs`** — bound `View.native_get_window` (`(J)Landroid/view/Window;`, instance) in
  `register_view_natives`: it validates-and-logs the view `widget` handle (a bad handle is non-fatal — the window is
  the shared one), maps any view to the single live window via `active_window()`, and returns a fresh frame-local
  `env.new_local_ref` of the captured Window Global via `with_jobject` (NEVER the Global raw — the established
  in-tree pattern); on no-capture/stale/error it returns JNI null, which is contract-valid (ATL builds a floating
  observer, `View.java:1252`). Rewrote `Window.set_jobject` (the one place the Window object flows into Eclipse —
  `Window.java:188`, called from `set_native_window` AFTER `this.native_window` is populated, `Window.java:58-60`)
  to `env.new_global_ref(&window)` + `window_registry::set_jobject` — so the captured object always has a valid
  `native_window` field, which `ViewTreeObserver(window)` reads (`ViewTreeObserver.java:305-308`). Also bound the
  immediate next native on the SAME view-tree path (code-path-proven, not speculative — `getViewTreeObserver` →
  `addOnGlobalLayoutListener` crosses the listener count 0→1, `ViewTreeObserver.java:344`):
  `ViewTreeObserver.native_set_have_global_layout_listeners(Z)V`, an instance no-op that records the flag (Eclipse
  has no host layout signal to gate `onGlobalLayout` — mirrors `nativeSetFullscreen`/`native_setVisibility`), via
  the new `register_view_tree_observer_natives` wired into `drive_lifecycle` right after `register_view_natives`.
  *Recorded-only (deliberately NOT bound — per the discovery-signal policy, unbound stays the loud signal):*
  `View.native_getMatrix` (`View.java:1756`) and `View.native_getGlobalVisibleRect` (`View.java:2066`) — not on the
  captured `getViewTreeObserver` path; `Window.getInsetsController` is pure Java (`Window.java:180`), needs no
  native. *Same-pattern audit:* grepped the crate for the presence-only `Option<()>` / `jobject = Some(())` pattern
  — the only `window_registry::WindowState.jobject` writer was `Window.set_jobject` (fixed); the separate
  `NativeWindowState` (loader `ndk_registry`/`native_provider`) is the engine `ANativeWindow` registry, unrelated
  and untouched; the field-type change compiles clean across every `window_registry::` consumer. *Regression
  guards (tied to the confirmed root cause):* extended the View pin-test with `VIEW_NATIVE_GET_WINDOW_NAME` ==
  `native_get_window` / `_SIG` == `(J)Landroid/view/Window;` (a transcription drift fails in-harness instead of
  re-producing the exact runtime `UnsatisfiedLinkError` ART named); a new ViewTreeObserver pin-test for the class +
  `native_set_have_global_layout_listeners` / `(Z)V`; and three `window_registry` unit tests (no-capture →
  `Ok(None)` = the null/floating-observer path, stale handle → `Err`, `active_window()` allocate/free tracking, and
  freeing a superseded window does NOT clear the newer active) — these would have caught the `Option<()>` bug
  (`set_jobject` silently dropping the ref). Also fixed the now-stale `WindowState` struct-level doc comment that
  still described the old `()`-typed placeholder (CLAUDE.md "Comments and Documentation"). *Verification (full gate,
  clean working tree, no machine-specific assumptions):* `cargo fmt --all --check` CLEAN; `cargo build
  --all-targets` 0; `cargo clippy --all-targets --all-features -- -D warnings` 0 warnings; `cargo test` **548 unit +
  0 (main) + 4 integration (`tests/engine_milestones.rs`, 0 SKIP — APK+display present, exact success markers
  required, so the binding did NOT regress the engine-milestone paths) + 2 doctests = 554 passed, 0 failed**;
  `cargo build --release` clean (stripped PIE x86-64, 8868552 bytes). Unit count 544→548 (+3 window_registry, +1
  ViewTreeObserver pin-test). *Context7:* had no `jni`-rs index (only `napi`-rs, unrelated); the authoritative jni
  0.21.1 behavior (`new_local_ref(global.as_obj()) -> JObject<'local>`, `with_jobject` borrow) was verified against
  the in-tree usage being mirrored (`view_registry`, `framework.rs:7457` `nativeResumeActivity`), which
  compile/test-pass. NOTE: `Cargo.toml` comments say jni 0.22 but `Cargo.lock` pins 0.21.1 (pre-existing, out of
  scope). *Did NOT live-boot ART (no `cargo run` / `__*` subcommands) and did NOT inspect any third-party binary —
  first-party only.* **OWNER-RUN DATA NEEDED (dev-host live boot, prohibited here):** (1) confirm the log shows
  `Window.set_jobject: captured Java Window object` BEFORE the `native_get_window` call in ActivityNativeMain's
  onCreate (the timing the non-null/non-floating path depends on — inferred sound from `Activity.java:81-82`
  ordering, but observe it); (2) capture the NEXT native on the view-tree path after this binding (predicted:
  `ViewTreeObserver.native_set_have_global_layout_listeners`, now bound → should PASS, then the genuine next trip),
  or the view tree completing and the engine's `AndroidGLView` surface path beginning (the render-integration
  frontier). *Files:* `src/framework.rs`, `src/framework/window_registry.rs`. *No subagent live boot.*

### 2026-06-13 — `SurfaceView.native_constructor` + `View.native_destructor` bound — Roblox's `RBXSurfaceView` GL render surface now constructs (LayoutInflater can complete ActivityNativeMain's content view); the engine SurfaceHolder/ANativeWindow render-integration is the scoped next-workflow step

*Confirmed root cause (first-party + live-evidence — owner boot of `95f964c`, `/tmp/eclipse-getwindow-validate.log`,
EXIT=124 clean):* with `native_get_window` bound (entry above), `ActivityNativeMain.onCreate` proceeded into
`com.roblox.client.ActivityNativeMain.d1()` → `android.view.LayoutInflater.inflate`, which constructs
`com.roblox.client.RBXSurfaceView` (extends `android.view.SurfaceView` — THE engine's GL render surface) and died on
two coupled unbound natives:
1. `No implementation found for long android.view.SurfaceView.native_constructor(android.content.Context,
   android.util.AttributeSet)` (`SurfaceView.native_constructor` → `View.<init>` → `LayoutInflater.createView` →
   `ActivityNativeMain.d1` → `onCreate` → `Activity.nativeStartActivity`).
2. Then, on the ART FinalizerDaemon cleaning up the half-built View, `No implementation found for void
   android.view.View.native_destructor(long)` (`View.native_destructor` → `View.finalize` → `FinalizerDaemon`).

After the exception Roblox's watchdog logged `Simulate crash with reason: RBXCRASH-HangDetected` — the main-thread
onCreate failure tripped its hang detector. *Mechanism (first-party-verified):* (a) SurfaceView
`@Override`-re-declares `native_constructor(Context, AttributeSet) -> long` (vendored `SurfaceView.java:40`,
returns a native peer handle that `View.java:965` stores in the `long widget` field) and ART resolves natives PER
DECLARING CLASS — so the existing View-class binding did NOT satisfy SurfaceView; it needs its OWN `RegisterNatives`
on `android/view/SurfaceView` (same per-class pattern already handled for TextView/ImageView/ImageButton). (b)
`View.native_destructor(long widget)` (`View.java:1168`, called from `View.finalize` `View.java:1679` to free that
peer) was declared on View, NOT overridden by any subclass, and genuinely never bound (zero `view_native_destructor`
/ `VIEW_NATIVE_DESTRUCTOR` references in src; the `register_view_natives` table had no destructor entry).

*Fix (both root causes — both land together; binding only one leaves the other's `UnsatisfiedLinkError`,
`src/framework.rs`):*
- NEW `pub const SURFACE_VIEW_CLASS = jni_str!("android/view/SurfaceView")` + NEW `register_surface_view_natives(env)`
  (an exact mirror of `register_image_view_natives`/`register_image_button_natives`): `find_class(SURFACE_VIEW_CLASS)`
  + one `NativeMethod::from_raw_parts(VIEW_NATIVE_CONSTRUCTOR_NAME, VIEW_NATIVE_CONSTRUCTOR_SIG,
  view_native_constructor)`. No new constructor body — it reuses the EXISTING class-agnostic
  `view_native_constructor`, which reads the receiver's concrete class via `getClass().getName()` (so it records
  `com.roblox.client.RBXSurfaceView` in `view_registry`) and allocates a real generational slab handle ≥ 1 (never the
  reserved 0), making `View.widget` non-zero so SurfaceView copies it into `mSurface.widget`
  (`SurfaceView.java:18,24`) and `Surface.isValid()` holds. Wired into `drive_lifecycle` right after
  `register_image_button_natives` (before step 4), so it is bound before `LayoutInflater` runs — the SAME ordering
  proven for the other per-class View-subclass registrations.
- NEW consts `VIEW_NATIVE_DESTRUCTOR_NAME = jni_str!("native_destructor")` / `VIEW_NATIVE_DESTRUCTOR_SIG =
  jni_str!("(J)V")` (dated, citing `View.java:1168`/finalize:1679) + NEW `extern "system" fn view_native_destructor`
  added to the EXISTING `register_view_natives` method table on `android/view/View` (declared on View and not
  overridden → one binding covers SurfaceView and every View subclass by inheritance; no new register fn for the
  destructor). It calls the bounds+generation-checked `view_registry::free(widget)` inside
  `env.with_env(...).resolve::<LogErrorAndDefault>()` and on `Err` logs + ignores — NEVER throwing. CRITICAL coupling:
  it MUST tolerate `widget == 0` and any stale/fabricated handle, because in the live crash `native_constructor`
  THREW so `widget = native_constructor(...)` (`View.java:965`) never assigned and `widget` stayed the `long` default
  0, then the finalizer ran `native_destructor(0)`. `view_registry::free(0)` safely returns `Err` (0 → index0/gen0;
  live generations are ≥ 1 → `StaleHandle`/`OutOfRange`), and `with_env` is `catch_unwind`-guarded — so a fault on the
  FinalizerDaemon thread cannot re-produce the live boot's second `UnsatisfiedLinkError`-shaped failure. This is also
  the FIRST runtime caller of `view_registry::free` (before this, View slots leaked for the process lifetime).

*Surface wiring — DELIBERATELY DEFERRED to the next workflow (not a punt; first-party-grounded):* the engine's
NDK/EGL HALF is already complete and proven — `eclipse_anativewindow_fromsurface` (`native_provider.rs`) IGNORES its
jobject arg and returns `ndk_registry::current_wsi_window()` (the process-global real WSI `EGLNativeWindowType`),
`EngineNativeWindow::new` mints+registers it and `EngineGlSurface::from_ndk_window` renders over it (`egl_engine.rs`),
and the integration test `gl_test_anw_binds_real_wsi_handle` is green. So the Java Surface peer is NOT how the engine
reaches the window — binding `native_constructor` does NOT block on any Surface→ANW plumbing. The wiring is genuinely
large (three production-path facts none yet coded — `register_wsi_window`/`EngineNativeWindow`/
`set_engine_window_geometry` exist ONLY in `egl_engine.rs`, ZERO in `graphics.rs`/`main.rs`, so production
`ANativeWindow_fromSurface` always hits the geometry-only fallback; present-loop ownership handoff between
run_windowed's Vulkan loop and the engine; and the Java trigger — SurfaceView's `surfaceCreated()`/`surfaceChanged()`
are PRIVATE with no Java caller in vendored ATL, so Eclipse must JNI-dispatch them once its winit WSI surface is live,
precedent `View.layoutInternal`). The exact post-construction call chain is libroblox-internal RUNTIME behavior,
observable ONLY on the live boot and NOT to be obtained by reverse-engineering libroblox.so — binding
constructor+destructor IS the one-native-per-boot step that surfaces it.

*Recorded-only (deliberately NOT bound — per the discovery-signal policy, unbound stays the loud signal):*
`SurfaceView.native_createSnapshot()J` / `native_postSnapshot(long, long)V` (`SurfaceView.java:42-43`) — off the
Roblox EGL render path (reached only via `lockCanvas`/`unlockCanvasAndPost`, the GskCanvas software-blit path Roblox
doesn't use); `SurfaceView.surfaceCreated()` / `surfaceChanged(int,int,int)` (PRIVATE, `SurfaceView.java:27-37`) —
the surface-lifecycle DELIVERY methods Eclipse must JNI-dispatch in the next workflow, not a native to bind;
`View.native_measure(JII)V` / `native_layout(JIIII)V` / `native_queueAllocate(J)V` — on the inflate→layout path but
whether they fire is RBXSurfaceView-bytecode (third-party) / owner-run-data-gated, bind reactively if a next boot
surfaces them; production WSI wiring into `graphics.rs::run_windowed` `resumed` + present-loop handoff (the
next-workflow render build, scoped above).

*Same-pattern audit:* `native_destructor` is declared ONLY on View (grep of vendored `View.java` — once at :1168, no
subclass override), so the single `android/view/View` binding covers RBXSurfaceView/SurfaceView/TextView/ImageView/
ImageButton by inheritance — no equivalent-instance gap. `native_constructor` is `@Override`-re-declared by SurfaceView
(`SurfaceView.java:40`) and ART resolves per declaring class — the same per-class pattern already handled for
View/TextView/ImageView/ImageButton, so SurfaceView was the one missing per-class binding; the recordedOnly natives
have zero src references (verified left unbound as loud discovery signals). No surface→ANativeWindow wiring was added;
the proven `egl_engine`/`__gl-test-anw`/`ndk_registry` path is untouched. The diff's asymmetry (per-class bind for the
constructor, View-only bind for the destructor) is exactly the correct consequence of these two facts.

*Regression guards (tied to the confirmed root cause):* NEW `surface_view_class_is_slashed_internal_name`
(`src/framework.rs`) pins `SURFACE_VIEW_CLASS == "android/view/SurfaceView"` so `register_surface_view_natives`'
`find_class` cannot drift into a boot-time `NoClassDefFoundError`; EXTENDED
`view_native_names_sigs_and_class_match_view_java` to pin `VIEW_NATIVE_DESTRUCTOR_NAME == "native_destructor"` /
`VIEW_NATIVE_DESTRUCTOR_SIG == "(J)V"` (a descriptor drift would re-produce the exact runtime `UnsatisfiedLinkError`
on the finalizer thread instead of failing in-harness); NEW `view_registry::tests::
surface_view_peer_round_trips_and_destructor_tolerates_null` (`src/framework/view_registry.rs`) proves a real
`allocate("com.roblox.client.RBXSurfaceView")` peer frees cleanly AND `free(0)` (the failed-construct finalizer path)
returns `Err`, never panics — the two exact properties `view_native_destructor` relies on (the existing
`out_of_range_and_fabricated_handles_return_err_not_panic` / `double_free_is_rejected` already cover the broader
stale/double-free soundness). As with every prior View native, the binding-PRESENCE guard (a missing
`RegisterNatives` only surfaces under a live ART boot, which can't run in-harness) is the documented owner dev-host
live boot.

*Verification (full gate, clean working tree, no machine-specific assumptions):* `cargo fmt --all --check` CLEAN;
`cargo build --all-targets` 0 warnings; `cargo clippy --all-targets --all-features -- -D warnings` 0 warnings;
`cargo test` **550 unit + 0 (main) + 4 integration (`tests/engine_milestones.rs`, 0 SKIP — APK+display present, exact
success markers required, including `gl_test_anw_binds_real_wsi_handle` and the 3427-constructor init) + 2 doctests =
556 passed, 0 failed**; `cargo build --release` clean (stripped PIE x86-64). Unit count 548→550 (+1
`surface_view_class_is_slashed_internal_name`, +1 `surface_view_peer_round_trips_and_destructor_tolerates_null`). *Did
NOT live-boot ART (no `cargo run` / `__*` subcommands) and did NOT inspect any third-party binary — first-party only.*

*OWNER-RUN DATA NEEDED (dev-host live boot, prohibited here — `./target/release/eclipse run <APK>` with
`ECLIPSE_ANDROID_FRAMEWORK_DIR=$HOME/.cache/eclipse/framework-patched`):* (1) confirm `RBXSurfaceView` `<init>` gets
PAST `native_constructor` (NO `UnsatisfiedLinkError`) and a `view_registry` peer naming
`com.roblox.client.RBXSurfaceView` is logged, and LayoutInflater completes the content view; (2) confirm NO
finalizer-thread `UnsatisfiedLinkError` on `View.native_destructor`, and (pure log observation) that
`native_destructor` is no longer reached with `widget == 0` (because the constructor now succeeds) and that real
RBXSurfaceView destructor calls pass a valid handle `free()` accepts; (3) capture the NEXT unbound-native ART stack on
the inflate→attach→surface-available path — specifically whether/when libroblox (via its reflection-registered
`AndroidGLView` SurfaceHolder.Callback) calls `SurfaceView.getHolder().addCallback(...)`, what triggers the private
`surfaceCreated()`/`surfaceChanged()` and on which thread, and whether the engine's GL/EGL targets THIS RBXSurfaceView
(`getHolder().getSurface()` → `ANativeWindow_fromSurface`) vs a separate AndroidGLView surface — all libroblox-internal
RUNTIME behavior, NOT first-party-determinable and NOT to be obtained by reverse-engineering libroblox.so. Once that
post-construction call chain is observed, the render-integration frontier is ACTIVE and the next-workflow build is the
`graphics.rs::run_windowed` WSI publish + present-loop ownership handoff + JNI-dispatch of
`surfaceCreated()`/`surfaceChanged()` on top of the already-green ANW path; success looks like EGL context creation
succeeding and the FIRST engine frames in the winit window. *Files:* `src/framework.rs`,
`src/framework/view_registry.rs`. *No subagent live boot.*

### 2026-06-13 — Inflatable `android.widget.*` View-subclass `native_constructor` batch bound in one pass (Button/EditText/ProgressBar/CheckBox/RadioButton/SeekBar/Spinner/ScrollView) — closes the one-class-per-boot `LayoutInflater.inflate` `UnsatisfiedLinkError` churn for the widget set

*Confirmed root cause (first-party + live-evidence — owner boot of `2194f02`, `/tmp/eclipse-surfaceview-validate.log`,
EXIT=124 clean):* with `SurfaceView.native_constructor` + `View.native_destructor` bound (entry above),
`RBXSurfaceView` constructed and `ActivityNativeMain`'s `LayoutInflater` proceeded further into the content view, then
tripped the NEXT View subclass in the same layout: `No implementation found for long
android.widget.ProgressBar.native_constructor(android.content.Context, android.util.AttributeSet)` at
`android.widget.ProgressBar.native_constructor(Native Method)` → `android.view.LayoutInflater.inflate` →
`com.roblox.client.ActivityNativeMain.onCreate` → `android.app.Activity.nativeStartActivity`. *Mechanism
(first-party-verified, IDENTICAL to the SurfaceView root cause):* ART resolves natives PER DECLARING/receiver class,
and `android.view.View`'s `native_constructor(Context, AttributeSet) -> long` (vendored `View.java:1166`, the peer
handle `View.java:965` stores into the `long widget` field) is RE-declared VERBATIM by every concrete inflatable
`android.widget.*` subclass — so the `register_view_natives` base binding does NOT satisfy them; each subclass the
layout inflates needs the shared class-agnostic `view_native_constructor` registered on its OWN class before step 4 /
`LayoutInflater`, or `inflate` throws. Eclipse had been binding these one at a time (View, then SurfaceView, …); the
layout clearly contained more (ProgressBar surfaced, others to follow), so binding them one-per-boot is slow — this
binds the whole `android.widget.*` set the overlay declares `native_constructor` on, in one pass.

*Fix (one shared helper, not eight near-duplicate functions, `src/framework.rs`):* NEW
`const VIEW_SUBCLASS_CONSTRUCTOR_CLASSES: &[&JNIStr]` (the 8 slashed internal names below) + NEW
`register_view_subclass_constructor_natives(env)` that loops the slice: `find_class(class_name)` →
`[NativeMethod::from_raw_parts(VIEW_NATIVE_CONSTRUCTOR_NAME, VIEW_NATIVE_CONSTRUCTOR_SIG, view_native_constructor)]` →
`unsafe env.register_native_methods` — the EXACT `register_surface_view_natives` recipe (`src/framework.rs`), with the
same `// SAFETY:` discipline. One shared helper (not per-class fns) is the minimal/non-boilerplate match to the
codebase's own design, because all 8 bind the IDENTICAL shared body: `view_native_constructor` (`src/framework.rs`) is
fully class-agnostic — it reads the receiver's concrete class via `getClass().getName()` (`view_class_name`) and
allocates a real `view_registry` generational-slab peer (handle ≥ 1), so each subclass records its OWN concrete class
(e.g. `android.widget.ProgressBar`). Wired into `drive_lifecycle` right after `register_surface_view_natives(env)?`
(before step 4 / LayoutInflater) — the SAME ordering proven for every other per-class View-subclass registration.

*The 8 bound* (each verified first-party to declare the 2-arg `native_constructor(Context, AttributeSet)J` DIRECTLY on
itself against `vendor/atl/src/api-impl/android/widget/`, so `RegisterNatives` finds the method — a class that merely
inherited it would `NoSuchMethodError`): `android/widget/Button` (`Button.java:39`), `android/widget/EditText`
(`EditText.java:24`), `android/widget/ProgressBar` (`ProgressBar.java:49`), `android/widget/CheckBox`
(`CheckBox.java:19`), `android/widget/RadioButton` (`RadioButton.java:17`), `android/widget/SeekBar`
(`SeekBar.java:17`), `android/widget/Spinner` (`Spinner.java:26`), `android/widget/ScrollView` (`ScrollView.java:18`).
All 8 share the exact `VIEW_NATIVE_CONSTRUCTOR_SIG = (Landroid/content/Context;Landroid/util/AttributeSet;)J`
(`src/framework.rs:4322-4324`).

*Excluded (first-party-verified, intentional):* `android/widget/CompoundButton` is `public abstract class`
(`CompoundButton.java:9`) — LayoutInflater cannot instantiate it; its concrete leaves CheckBox/RadioButton re-declare
the native and ARE in the set. `android/widget/PopupWindow` declares ZERO-ARG `native_constructor()J`
(`PopupWindow.java:177`) and is NOT a View — pointing it at the shared `(Context, AttributeSet)J` body would be wrong
arity AND wrong type; it gets its own distinct body if/when it traps. The abstract layout parents
`AbsSeekBar`/`AbsSpinner`/`AdapterView`/`ViewGroup` do NOT declare `native_constructor` (the containers inherit View's,
already bound by `register_view_natives`) — no binding needed. ONLY `native_constructor` is bound per class; each
class's extra natives (e.g. `ProgressBar.native_setProgress`, SeekBar/Spinner extras) stay UNBOUND on purpose so the
next real layout/draw trip surfaces them one at a time — the deliberate loud discovery signal, exactly as
`register_surface_view_natives` omits `native_createSnapshot`/`native_postSnapshot`. `View.native_destructor(long)`
(`View.java:1168`) is declared on View and re-declared by NONE of these, so the existing `register_view_natives`
binding covers destruction for all 8 by inheritance — no per-class destructor binding.

*Same-pattern audit (full overlay grep, `native long native_constructor(Context` across `widget`/`view`/`webkit`):*
exactly 15 declarers of the `(Context, AttributeSet)J` form: 5 already bound (View, view/SurfaceView, widget/TextView,
widget/ImageView, widget/ImageButton) + 8 newly bound (above) + 1 abstract-excluded (widget/CompoundButton) + 1
recorded-unbound (webkit/WebView). PopupWindow's zero-arg form is correctly outside this set. RECORDED, deliberately
NOT bound (out of the `android.widget.*` scope of this pass, not yet surfaced by evidence): `android.webkit.WebView`
(`vendor/atl/src/api-impl/android/webkit/WebView.java`) is the ONE remaining concrete class that re-declares the exact
`(Context, AttributeSet)J` `native_constructor` and is currently unbound anywhere in `framework.rs` — if a future
layout inflates a WebView it will trip `No implementation found for long android.webkit.WebView.native_constructor(...)`;
it shares the exact signature so it can join `VIEW_SUBCLASS_CONSTRUCTOR_CLASSES` (+ the pin test) when/if it surfaces.
Leaving it unbound keeps it a loud discovery signal, consistent with the per-class extra-natives policy above. The
`ViewGroup` layout containers (LinearLayout/FrameLayout/RelativeLayout/ViewGroup) do NOT re-declare `native_constructor`
— they inherit View's, already bound — so they are correctly absent.

*Regression guard (tied to the confirmed root cause):* NEW `view_subclass_constructor_classes_are_slashed_internal_names`
(`src/framework.rs`, mirrors `surface_view_class_is_slashed_internal_name`) pins the EXACT ordered 8-name set
(`assert_eq!` on the full Vec, so a DROPPED or reordered class — which re-introduces the one-per-boot
`UnsatisfiedLinkError` this pass fixes — fails the test) and asserts CompoundButton/PopupWindow stay OUT of the set
(abstract / wrong-arity-non-View). Host-independent pure-const test. As with every prior View native, the
binding-PRESENCE guard (a missing `RegisterNatives` only surfaces under a live ART boot, which can't run in-harness) is
the documented owner dev-host live boot. Run: `cargo test view_subclass_constructor_classes_are_slashed_internal_names`
(1 passed).

*Verification (full gate, clean working tree, no machine-specific assumptions):* `cargo fmt --all` CLEAN;
`cargo build --all-targets` 0 warnings; `cargo clippy --all-targets --all-features -- -D warnings` 0 warnings (confirmed
real, not stale cache — forced a fresh recompile of the eclipse crate, 2.45s); `cargo test` **551 unit + 0 (main) + 4
integration (`tests/engine_milestones.rs`, 0 SKIP — APK+display present, exact success markers required) + 2 doctests =
557 passed, 0 failed**; `cargo build --release` clean (stripped PIE x86-64, artifact 8,870,408 bytes). Unit count
550→551 (+1, the pin test). *Did NOT live-boot ART (no `cargo run` / `__*` subcommands) and did NOT inspect any
third-party binary — first-party only.*

*OWNER-RUN DATA NEEDED (dev-host live boot, prohibited here — `./target/release/eclipse run <APK>` with
`ECLIPSE_ANDROID_FRAMEWORK_DIR=$HOME/.cache/eclipse/framework-patched`):* (1) confirm `ActivityNativeMain`'s
`LayoutInflater` builds its FULL content view WITHOUT tripping per-widget `native_constructor` (ProgressBar + the rest
of the batch construct; a `view_registry` peer is allocated per inflated widget recording its concrete class); (2)
capture the NEXT unbound-native ART stack — either a per-widget extra native (e.g. `ProgressBar.native_setProgress`),
the recorded `WebView.native_constructor` if the layout inflates a WebView, or the next class on the
inflate→attach→surface path; (3) watch for any `UnsatisfiedLinkError` naming a `com.roblox.*` class on
`native_constructor` (an RBX* custom view that `@Override`-re-declares it in the dex would need its own binding on the
app class name — an app-bytecode fact, NOT first-party-determinable; report the exact class, do not dexdump the APK).
Once the layout completes, the render-integration frontier is the SCOPED surface-to-engine wiring from `2194f02`'s §6
plan: `EngineNativeWindow::new` + `register_wsi_window`/`set_engine_window_geometry` into `graphics.rs::run_windowed`
(today ZERO there, so production `ANativeWindow_fromSurface` always hits the geometry-only fallback), present-loop
ownership handoff, and JNI-dispatch of `SurfaceView.surfaceCreated()`/`surfaceChanged()` once the WSI surface is live —
designed AFTER the live boot reveals the post-layout call chain (in particular whether/when libroblox's
reflection-registered `AndroidGLView` SurfaceHolder.Callback fires and on which thread; all libroblox-internal RUNTIME
behavior, NOT first-party-determinable, NOT to be obtained by reverse-engineering libroblox.so — capture that
next-native/AndroidGLView trace from the boot log). The NDK/EGL half is de-risked (`gl_test_anw_binds_real_wsi_handle`
green). *Files:* `src/framework.rs`. *No subagent live boot.*

---

### 2026-06-13 — Inflatable `android.widget.*` property setters bound in one pass (Button/EditText/ProgressBar/CheckBox/RadioButton/SeekBar/Spinner/ScrollView + two base `android.view.View` setters) — closes the per-widget setter `UnsatisfiedLinkError` churn after the construction batch; control-flow getters and the coupled isChecked/setChecked pairs deliberately left unbound

*Confirmed root cause (first-party + the prior live-evidence chain):* with the inflatable View-subclass
`native_constructor` batch bound (entry above), `ActivityNativeMain`'s `LayoutInflater` constructs the widgets, and
the NEXT one-per-boot `UnsatisfiedLinkError` trip is each widget's PROPERTY SETTER native — `ProgressBar.native_setIndeterminate(boolean)`
was named as THE trigger. *Mechanism (identical to the constructor batch):* ART resolves natives PER DECLARING class,
so each widget's setter must be registered on its OWN class. Binding them one-per-boot is slow; this binds the
inflatable-widget-set's property setters in one pass.

*Fix (`src/framework.rs`):* NEW `register_widget_property_setter_natives(env)` registers each setter on its declaring
class, wired into `drive_lifecycle` right after `register_view_subclass_constructor_natives(env)?` (before step 4 /
LayoutInflater). Honest no-GTK record-or-no-op semantics (the project model: the framework RECORDS the view tree into
`view_registry` and the graphics pass draws view quads from it; real game frames come from the engine GL surface, not
these `android.widget` views): (1) TEXT setters RECORD on the peer (renderer-consumed) — `Button.native_setText`,
`EditText.native_setText`, `CheckBox.native_setText` (each `(JLjava/lang/String;)V`) and
`RadioButton.setText(Ljava/lang/CharSequence;)V` (records `this.widget` via `CharSequence.toString()`, resolved with
`?` so a thrown Java exception is described+cleared at the boundary). (2) ScrollView REUSES the already-class-agnostic
`view_group_native_add_view`/`view_group_native_remove_view` — records real tree edges. (3) Validated-handle NO-OPs
where the decisive check holds — NO bound native getter reads the value back AND the renderer draws no such chrome, so
the Java caller depends on no native effect (mirrors the existing `ImageView.native_setScaleType`/`View.nativeSetFullscreen`/
`native_setBackgroundDrawable` no-ops): `ProgressBar.native_setIndeterminate(Z)V` (the trigger; `isIndeterminate()`
reads a Java field), `ProgressBar.native_setProgress(JF)V`, `SeekBar.native_setProgress(JF)V`, `SeekBar.native_setMax(JI)V`,
`Spinner.native_setAdapter(JLandroid/widget/SpinnerAdapter;)V` (`getAdapter()` returns the Java adapter),
`Button.native_setCompoundDrawables(JJ)V` (drawable draw deferred). (4) Two base `android.view.View` setters added to
the EXISTING `register_view_natives` array: `setBackgroundColor(I)V` RECORDS ARGB via `view_registry::set_background_color`
(renderer-consumed; verified `View.java:1284`), and the STATIC `native_keep_screen_on(JZ)V` is a validated no-op (no
host screen-wake, no native getter; verified `View.java:1982`). One small refactor traceable to the change: the 8
widget class-name literals promoted to `pub const` (`BUTTON_CLASS`..`SCROLL_VIEW_CLASS`) as a single source of truth
reused by both `VIEW_SUBCLASS_CONSTRUCTOR_CLASSES` and the new registrar (mirrors the `SURFACE_VIEW_CLASS` precedent).
Every body runs inside `EnvUnowned::with_env` (catch_unwind, AGENTS.md §2.8) and resolves via `LogErrorAndDefault`.

*Deliberately LEFT UNBOUND + flagged (per policy — a native whose RETURN value drives Java control flow is NEVER
no-op'd; it stays the loud discovery signal):* the return-driving GETTERS `SeekBar.native_getProgress(J)I`
(`SeekBar.getProgress()`), `EditText.native_getText(J)Ljava/lang/String;` (`getText()`/`getEditableText()`),
`Button.getText()Ljava/lang/CharSequence;`. The COUPLED stateful `CheckBox.isChecked()Z`/`setChecked(Z)V` and
`RadioButton.isChecked()Z`/`setChecked(Z)V` pairs are left FULLY unbound (not silent no-op setters): no consumed
`view_registry` field backs a `checked` boolean, so no-op'ing `setChecked` while `isChecked` reads it would be a
silent wrong answer — binding them later means adding a `checked` field + the `isChecked` reader together. All listener
registrations (Button/CheckBox/RadioButton/View `*OnClickListener`, `setOnCheckedChangeListener`,
`setOnSeekBarChangeListener`, Spinner `setOnItemSelectedListener`, EditText text/editor-action listeners, View
touch/long-click/focus) are NOT property setters and whether they fire is RBX-bytecode/owner-run-data-gated — kept as
the deliberate per-class discovery signal. View getters/queries with return-driven flow (`getWidth`/`getHeight`,
`nativeIsFocused`, `nativeIsAttachedToWindow`, `native_getMatrix`, `native_getGlobalVisibleRect`) and the deferred
layout/draw/CSS natives (`native_measure`/`native_layout`/`native_drawBackground`/etc.) stay out of the
property-setter scope, consistent with §5's deferred-layout note.

*Same-pattern audit:* the audit confirmed the honest-semantics decision per setter against the project's
record-or-no-op model — `view_registry::ViewState` carries only `class_name`/`text`/`children`/`layout`/`clickable`/
`jobject`/`background_color`, and the renderer draws `RenderNode` from those, so text + background-color + tree-edge
setters RECORD (real fidelity) and progress/indeterminate/max/adapter/compound-drawable no-op (no backing field, no
chrome). This mirrors the existing View setter bindings (`nativeSetFullscreen`, `native_setVisibility`,
`native_setTextColor`, `native_setBackgroundColor`).

*Regression guard (tied to the confirmed root cause):* NEW `widget_property_setter_names_sigs_and_classes_match_overlay`
(`src/framework.rs`, mirrors `view_subclass_constructor_classes_are_slashed_internal_names`) pins the exact slashed
class internal names + method name/JNI descriptors for every newly bound setter (incl. the two base-View setters), so a
dropped class or a transcribed-wrong name/sig — the failure modes that re-introduce the one-per-boot
`UnsatisfiedLinkError` — fails CI. As with every prior View native, the binding-PRESENCE / per-class WIRING guard (a
missing or misrouted `RegisterNatives` only surfaces under a live ART boot, which can't run in-harness — ART aborts
off the main thread) is the documented owner dev-host live boot. Run:
`cargo test widget_property_setter_names_sigs_and_classes_match_overlay` (1 passed).

*Verification (full gate, clean working tree, no machine-specific assumptions):* `cargo fmt --all` CLEAN;
`cargo build --all-targets` 0 warnings (exit 0); `cargo clippy --all-targets --all-features -- -D warnings` 0 warnings
(exit 0); `cargo test` **552 unit + 0 (main) + 4 integration (`tests/engine_milestones.rs`, 0 SKIP — APK+display
present, exact success markers required) + 2 doctests = 558 passed, 0 failed**; `cargo build --release` clean
(artifact `/home/kue/Projects/Eclipse/target/release/eclipse`, 8,901,224 bytes). Unit count 551→552 (+1, the pin
test). *Did NOT live-boot ART (no `cargo run` / `__*` subcommands) and did NOT inspect any third-party binary —
first-party only against the vendored `vendor/atl/src/api-impl/android/` source + the public Android widget API.*

*OWNER-RUN DATA NEEDED (dev-host live boot, prohibited here — `./target/release/eclipse run <APK>` with
`ECLIPSE_ANDROID_FRAMEWORK_DIR=$HOME/.cache/eclipse/framework-patched`):* (1) confirm `ProgressBar.native_setIndeterminate`
no longer trips `UnsatisfiedLinkError` and `ActivityNativeMain`'s `LayoutInflater` builds the FULL content view /
`onCreate` proceeds further toward RESUMED; (2) capture the NEXT unbound-native ART stack one at a time (a return-driving
getter like `SeekBar.native_getProgress`/`EditText.native_getText`, an `isChecked()`/`setChecked()` pair, a listener
registration, or the next class on the inflate→attach→surface path — pure log observation, no binary inspection); (3)
confirm `register_view_natives` still registers cleanly (no `NoSuchMethodError` on `android.view.View`) now that
`setBackgroundColor(I)V` + `native_keep_screen_on(JZ)V` joined that all-or-nothing array under the installed stock
dex. *Pre-existing wart flagged, NOT regressed here, out of scope:* the pre-existing `native_setBackgroundColor(JI)V`
binding in `register_view_natives` targets a method the current vendored `View.java` no longer declares (only the
`(I)V` form at `View.java:1284` exists); the boot demonstrably already passes `register_view_natives` (it reaches the
widget classes), but if a live log ever shows a `NoSuchMethodError`/`No implementation` on `native_setBackgroundColor`,
reconcile that dead binding to the shipped overlay in a separate cleanup pass. *Files:* `src/framework.rs`. *No
subagent live boot.*

---

- **2026-06-13 — 🩹 58a50f6 REGRESSION FIXED — the two speculative base-`android.view.View` setters added by 58a50f6
  are removed from `register_view_natives`.** This resolves the prior (2026-06-13 widget property-setter) entry's
  *OWNER-RUN DATA NEEDED (3)* — "confirm `register_view_natives` still registers cleanly now that `setBackgroundColor(I)V`
  + `native_keep_screen_on(JZ)V` joined that all-or-nothing array under the installed stock dex." **It did NOT.** ROOT
  CAUSE (PROVEN by the owner's live boot of 58a50f6, NOT a subagent — pure log observation): JNI `RegisterNatives` is
  ATOMIC over its entire `NativeMethod` array; ART validates each entry against the class's declared methods FIRST and
  aborts the whole array on the first mismatch. The very first new entry, `View.setBackgroundColor(I)V`, is a PLAIN
  Java method in the SHIPPED framework (not native), so ART logged `jni_internal.cc: Failed to register non-native
  method android.view.View.setBackgroundColor(I)V as native` → `No such method: no native method "Landroid/view/View;.
  setBackgroundColor(I)V"` and rejected the ENTIRE `register_view_natives` array. That took the lifecycle-critical
  base-View natives down with it — `native_constructor`/`native_destructor`/`native_get_window` never registered — so
  the lifecycle drive aborted and the process faulted during teardown. (The vendored `View.java:1284` `public native
  void setBackgroundColor(int)` is demonstrably out of sync with the installed dex; the source cannot be trusted to
  decide the shipped native/non-native split. `native_keep_screen_on(JZ)V` was never reached — RegisterNatives stopped
  at the first bad entry — so its shipped native-ness is likewise unverified.) FIX (surgical, `src/framework.rs` only,
  25 insertions / 126 deletions): removed the two consts (`VIEW_SET_BACKGROUND_COLOR_NO_HANDLE_NAME`/`_SIG`,
  `VIEW_KEEP_SCREEN_ON_NAME`/`_SIG`), the two fn bodies (`view_set_background_color_no_handle`, `view_keep_screen_on`),
  the two `NativeMethod::from_raw_parts` array entries in `register_view_natives`, their mentions in the `info!`
  registration log, and the two assertions in the `widget_property_setter_names_sigs_and_classes_match_overlay` pin
  test. Added a dated `2026-06-13` guard comment in three places (the consts cluster, the `register_view_natives`
  array, and the pin test) recording the live-log evidence so neither setter is reintroduced without a live boot
  proving the shipped framework declares it native. NOT TOUCHED (intact): the pre-existing `native_setBackgroundColor(JI)V`
  `(JI)V` binding (still the separate-cleanup flag from the prior entry) and ALL 58a50f6 widget-class
  constructors/property setters. WHY THIS IS THE ROOT-CAUSE FIX, not a workaround: the boot reached
  `ActivityNativeMain.onCreate` WITHOUT either setter before 58a50f6, so neither is required for progress; the durable
  correct state is to bind ONLY methods the shipped framework actually declares native, and the atomic-RegisterNatives
  contract makes a single non-native entry fatal to the whole class — so the speculative entries were the defect, and
  removing them restores correct registration. REGRESSION GUARD: the existing pin test
  `widget_property_setter_names_sigs_and_classes_match_overlay` now carries the dated NOTE that these two are
  intentionally unbound; reintroducing either would have to re-add an array entry that the shipped dex's atomic
  RegisterNatives rejects (caught at the next live boot) — and the in-code dated comments document the live evidence
  for any future reader. GATE (re-run on the merged working tree): `cargo fmt --all -- --check` clean; `cargo build
  --all-targets` 0 warnings; `cargo clippy --all-targets --all-features -- -D warnings` 0 warnings;
  `cargo test` **552 unit + 0 main-bin + 4 integration (`tests/engine_milestones.rs`, 0 SKIP, exact success markers) +
  2 doctests = 558 passed, 0 failed** (unit count unchanged at 552 — the fix dropped assertions inside the existing pin
  test, not a whole test; `widget_property_setter_names_sigs_and_classes_match_overlay` 1 passed); `cargo build
  --release` clean (artifact `/home/kue/Projects/Eclipse/target/release/eclipse`, 8,897,128 bytes). *Did NOT live-boot
  ART (no `cargo run`/`__*` subcommands) and did NOT inspect any binary — the regression mechanism is the owner's
  already-captured live log of 58a50f6; the fix is first-party source + the green gate.* *Files:* `src/framework.rs`,
  `AGENTS.md`. *No subagent live boot.*

---

### 2026-06-13 — `EditText` listener natives bound (record-the-listener; dispatch-on-real-input is future work) + the 58a50f6 atomic-`RegisterNatives` abort class hardened to per-method best-effort across the View/widget per-class registrations

*Confirmed root cause / live evidence (owner dev-host boot of `16db9eb`, clean run, pure log observation — NOT a
subagent):* the 58a50f6 regression fix landed — `register_view_natives` registers cleanly and the boot reaches
`ActivityNativeMain.onCreate` PAST `ProgressBar.native_setIndeterminate`. The NEXT unbound native, while `LayoutInflater`
inflates the content view via `com.roblox.client.RbxKeyboard.<init>` → `androidx.appcompat.widget.AppCompatEditText.<init>`
→ `EditText.addTextChangedListener`: `No implementation found for void
android.widget.EditText.native_addTextChangedListener(long, android.text.TextWatcher)` (at `Activity.nativeStartActivity`
→ `ActivityNativeMain.onCreate` → `LayoutInflater`). The `UnsatisfiedLinkError` confirms the native IS declared native
in the shipped framework but unbound.

*Fix (A) — `src/framework.rs` + `src/framework/view_registry.rs`, EditText listener natives, record-the-listener:* bound
the three listener natives ON their declaring class `android/widget/EditText`, each first-party-verified `protected
native` in the vendored overlay: `native_addTextChangedListener` (`EditText.java:26`, `(JLandroid/text/TextWatcher;)V`),
`native_removeTextChangedListener` (`:27`, same sig), `native_setOnEditorActionListener` (`:28`,
`(JLandroid/widget/TextView$OnEditorActionListener;)V` — `OnEditorActionListener` is a `public static interface` in
`TextView.java:287`, reached through EditText's TextView supertype). HONEST record-the-listener semantics: Eclipse's
vendored `EditText.addTextChangedListener`/`setOnEditorActionListener` (`EditText.java:52`/`:57`) pass the listener
straight to the native with NO Java field, so a plain local arg would be GC'd the moment the native returns — the native
therefore RETAINS it. `edit_text_add_text_changed_listener` stores a `env.new_global_ref(watcher)` on the
`view_registry` peer (NEW `ViewState.text_watchers: Vec<Global<JObject<'static>>>`);
`edit_text_remove_text_changed_listener` drops the `IsSameObject`-matching retained watcher (releasing its global ref;
an `IsSameObject` JNI failure conservatively KEEPS the watcher — a safe false-negative);
`edit_text_set_on_editor_action_listener` retains/replaces (the old `Global`'s `Drop` releases its ref) the editor-action
listener (NEW `ViewState.editor_action_listener: Option<Global<JObject<'static>>>`), `null` clears it. Each `Global`
releases its ref on `Drop` (slot `free`d or listener replaced/cleared). Null listener ignored; a stale/fabricated handle
is a typed `Err` (logged + ignored, never UB) via the bounds+generation-checked `view_registry::{add_text_watcher,
retain_text_watchers, set_editor_action_listener}` helpers (each validates the handle exactly like `with_view`). Every
body runs inside `EnvUnowned::with_env` (catch_unwind, §2.8) and resolves via `LogErrorAndDefault`. **Actually
DISPATCHING `TextWatcher.onTextChanged`/`onEditorAction` on real input is a FUTURE input-integration step — no input
occurs during boot, so retaining the listener (so a future input-dispatch path can invoke the held object) is the
complete correct behavior NOW.** This is documented in-code (`view_registry.rs:172-186`, each native's docstring).

*Fix (B) — `src/framework.rs`, the 58a50f6 root-cause CLASS fix + regression guard:* the 58a50f6 boot break was JNI
`RegisterNatives` aborting an ENTIRE per-class `NativeMethod` array ATOMICALLY when one entry (`setBackgroundColor(I)V`)
is plain Java in the shipped dex — ART validates every entry against the class's declared methods first and rejects the
whole array on the first mismatch, taking the lifecycle-critical `native_constructor`/`native_destructor`/
`native_get_window` down with it. NEW `register_class_natives_best_effort(env, class_name, &[NativeBinding])` (where
`type NativeBinding = (&'static JNIStr, &'static JNIStr, *mut c_void)`) resolves `find_class` ONCE (a genuine class-load
failure still propagates via `?`, never masked), then binds each method INDEPENDENTLY via a single-element
`RegisterNatives` slice (`std::slice::from_ref(&method)`); a method the shipped dex does not declare native makes the
single-method `register_native_methods` throw — the pending exception is cleared and the entry skipped with a LOUD
per-method `tracing::warn!` naming class+method+sig. This faithfully mirrors the EXISTING per-native best-effort
precedent in `register_asset_stream_natives` (the readAsset/openAssetFd/… loop), but at WARN not the precedent's debug:
it must NEVER silently mask a genuinely-needed native — it only degrades the fatal whole-class abort into a deferred
call-time `UnsatisfiedLinkError` on ONLY the bad method, the same loud discovery signal the project already relies on.
The skip-and-continue control flow is split into a pure `fold_best_effort(bindings, step) -> u32` core (no JVM) so it is
unit-testable in-harness. CONVERTED exactly the View/widget per-class registrars affected by this class of bug from
atomic-array `RegisterNatives` to per-method best-effort: `register_view_natives` (the function 58a50f6 actually broke),
`register_view_group_natives`, `register_text_view_natives`, `register_image_view_natives`, `register_image_button_natives`,
`register_surface_view_natives`, `register_view_subclass_constructor_natives`, and `register_widget_property_setter_natives`
(all 8 inflatable widget classes). Did NOT touch unrelated registrars (Paint/Matrix/Path/Canvas/Drawable/Window/Activity/
asset-stream stay atomic per "do not rewrite unrelated registration code"; `register_view_tree_observer_natives` sits at
the edge of the chosen scope and was left atomic — flagged below).

*Same-pattern audit:* the audit boundary is the LayoutInflater-critical View/widget per-class `RegisterNatives` family —
exactly the registrations that can be reached during step-4 inflation and so are exposed to the 58a50f6 atomic-abort
mechanism (an entry the shipped dex disagrees with taking lifecycle-critical siblings down with it). All eight are
converted. `register_view_tree_observer_natives` (reached on the same `getViewTreeObserver` path, commit `95f964c`)
still uses atomic `register_native_methods`; it is at the edge of scope and left atomic per "do not rewrite unrelated
registration code" — if a future single-bad-entry there ever aborts its class, convert it the same way. The non-View
registrars (Paint/Canvas/Window/Activity/asset-stream) were correctly left atomic.

*Regression guard (tied to the confirmed root cause):* NEW `register_class_natives_best_effort_skips_unbindable_method_and_continues`
(`src/framework.rs`) drives the pure `fold_best_effort` core (JVM-free — ART can't run in `cargo test`, it aborts off
the main thread) with a 3-entry binding set whose MIDDLE entry fails: it asserts all 3 entries are visited IN ORDER (no
short-circuit) and `bound == 2` — the smallest check that would have caught the 58a50f6 atomic abort, and a future
reintroduction of an early `return Err`/`?` on a per-entry failure fails it. (Coverage boundary, stated for transparency:
this pins the no-short-circuit LOOP invariant — the exact thing 58a50f6 violated — but tests the extracted `step`
closure, not the real `register_class_natives_best_effort` match-arm body; the match-arm mirrors the proven asset-stream
precedent. This is the best achievable without a JVM.) The existing `widget_property_setter_names_sigs_and_classes_match_overlay`
pin is EXTENDED with name/sig pins for the three EditText listener natives (add/remove share `(JLandroid/text/TextWatcher;)V`;
editor-action `(JLandroid/widget/TextView$OnEditorActionListener;)V`) so a transcription drift re-introducing the
boot-blocking `UnsatisfiedLinkError` fails CI. NEW `view_registry` tests
`listener_retention_counts_start_empty_and_clear_is_a_noop_on_empty` and
`listener_retention_helpers_reject_stale_and_fabricated_handles` pin the handle-validation + count/clear bookkeeping of
the listener-retention helpers (a real `Global` needs a live VM — that path is validated on the owner dev-host run).
Run: `cargo test register_class_natives_best_effort_skips_unbindable_method_and_continues` (1 passed),
`cargo test listener_retention` (2 passed), `cargo test widget_property_setter_names_sigs_and_classes_match_overlay`
(1 passed). As with every View native, the binding-PRESENCE / per-class WIRING guard only surfaces under a live ART
boot (which can't run in-harness) — the documented owner dev-host live boot.

*Verification (full gate, clean working tree, no machine-specific assumptions):* `cargo fmt --all -- --check` CLEAN;
`cargo build --all-targets` 0 warnings (exit 0); `cargo clippy --all-targets --all-features -- -D warnings` 0 warnings
(exit 0); `cargo test` **555 unit + 0 (main) + 4 integration (`tests/engine_milestones.rs`, 0 SKIP — APK+display
present, exact success markers required) + 2 doctests = 561 passed, 0 failed** (unit 552→555, +3: two `view_registry`
listener-retention tests + one `fold_best_effort` skip-and-continue test); `cargo build --release` clean (artifact
`/home/kue/Projects/Eclipse/target/release/eclipse`, 8,907,880 bytes). *Did NOT live-boot ART (no `cargo run`/`__*`
subcommands) and did NOT inspect any third-party binary — first-party only against the vendored
`vendor/atl/src/api-impl/android/widget/EditText.java` + `TextView.java` + the public Android API.*

*OWNER-RUN DATA NEEDED (dev-host live boot, prohibited here — `./target/release/eclipse run <APK>` with
`ECLIPSE_ANDROID_FRAMEWORK_DIR=$HOME/.cache/eclipse/framework-patched`; NOTE: the framework overlay is a CACHE artifact
wiped periodically — if the boot errors `Android framework not found`, rebuild it with
`tools/framework-overlay/patch-framework.sh` FIRST):* (1) confirm `EditText.native_addTextChangedListener` no longer
trips `UnsatisfiedLinkError` on the `RbxKeyboard`/`AppCompatEditText` construction path and `LayoutInflater` proceeds
past it; (2) confirm the View/widget registrations log the normal `(best-effort)` info lines and NO per-method WARN (a
WARN names a genuinely-non-native shipped method to investigate next, e.g. confirming which — if any — of these EditText
listeners the shipped dex declares plain Java); (3) capture the NEXT unbound native one at a time (whether
`native_removeTextChangedListener`/`native_setOnEditorActionListener` are also reached on this ctor path, then likely a
return-driving getter such as `EditText.native_getText`/`SeekBar.native_getProgress`, an `isChecked()`/`setChecked()`
pair, another listener registration, or the next class on the inflate→attach→surface path — pure log observation, no
binary inspection). *Files:* `src/framework.rs`, `src/framework/view_registry.rs`, `AGENTS.md`. *No subagent live boot.*

---

### 2026-06-13 — LayoutInflater `<requestFocus/>` framework-overlay patch (ATL stubbed the standard tag; AOSP parse-and-consume, headless so `requestFocus()` is skipped) — LIVE-PROVEN: inflation advanced past the tag

*Confirmed root cause / live evidence (owner dev-host boot, commit `521ba34` tree + this overlay — clean log observation, NOT a subagent):* `ActivityNativeMain.onCreate`'s content-view `LayoutInflater.inflate` aborted with `<requestFocus /> not supported atm`. The vendored ATL `LayoutInflater.rInflate` (`vendor/atl/src/api-impl/android/view/LayoutInflater.java`) stubs the standard AOSP `<requestFocus/>` layout tag with `throw new Exception("<requestFocus /> not supported atm")` (the AOSP `parseRequestFocus` call is commented out right below it). Roblox's content layout contains a `<requestFocus/>` element, so inflation hits the throw and dies.

*Why an overlay patch (not a Rust `RegisterNatives` fix):* this is a pure-Java method-BODY gap — no native method is involved, so Eclipse's `RegisterNatives` mechanism (which binds native methods) cannot fix it. The patch goes through the framework overlay (`tools/framework-overlay/`, multidex first-dex-wins: patched classes in `classes.dex` shadow the stock ATL `classes2.dex`), exactly the lane established for `Build`/`NetworkRequest`/`ActivityManager`/`PowerManager`.

*Fix:* NEW committed patched copy `tools/framework-overlay/src/android/view/LayoutInflater.java` (Apache-2.0), byte-identical to the vendored original EXCEPT: (1) header comment, (2) the `<requestFocus/>` branch in `rInflate` now calls a new `private parseRequestFocus(parser, parent)` instead of throwing, (3) two new private methods `parseRequestFocus(XmlPullParser, View)` → `consumeChildElements(XmlPullParser)`. `consumeChildElements` is the canonical AOSP frameworks/base idiom (and identical to `rInflate`'s own depth-guard loop): it advances the parser to the current element's matching `END_TAG` and stops there, so `rInflate`'s next `parser.next()` resumes at the next SIBLING (no skipped siblings), always advances via `parser.next()`, and terminates on `END_DOCUMENT` (no infinite loop) — a GENUINE consume of the (empty) tag, not error suppression. `parseRequestFocus` DELIBERATELY OMITS `View.requestFocus()`: Eclipse is headless (no GTK) and binds NO `nativeRequestFocus` native (`View.requestFocus()` bottoms out in `private native void nativeRequestFocus(long, int)`, whose only impl is ATL's GTK `gtk_widget_grab_focus`, which Eclipse never loads); calling it would trade one inflation abort for an `UnsatisfiedLinkError`. The engine owns input focus headlessly — consuming the tag so inflation continues is the load-bearing, correct behavior. `ECLIPSE PATCH 2026-06-13` markers carry the root-cause + headless-focus reasoning in-code.

*Compile-only stub set (NEVER dexed):* the patched `LayoutInflater` references View/ViewGroup/Context/Resources/TypedArray/XmlPullParser etc., but ATL ships dex (not classfiles) so it cannot be a `javac` classpath. NEW compile-only stubs under `tools/framework-overlay/stubs/`: `android/view/{View,ViewGroup,ContextThemeWrapper}`, `android/content/res/{TypedArray,XmlResourceParser,Resources}`, `android/util/{AttributeSet,Slog,Xml}`, `com/android/internal/R`, `org/xmlpull/v1/{XmlPullParser,XmlPullParserException,XmlPullParserFactory}`; plus an EXTENDED `tools/framework-overlay/stubs/android/content/Context.java` (added concrete `getResources`/`obtainStyledAttributes`/`getSystemService` — `getSystemService` concrete, NOT abstract, because `android/app/Application` extends `Context` as a non-abstract class and an abstract method would break the pre-existing Application stub compile). The `XmlPullParser` stub constants (`START_TAG=2`/`END_TAG=3`/`END_DOCUMENT=1`) are the canonical `org.xmlpull.v1` values, compile-inlined into the patched bytecode; the stub is never dexed, and the original ATL `rInflate`/`parseInclude` already reference these same constants at runtime today, so the values are runtime-proven by the existing inflate path. The staging glob in `patch-framework.sh` selects ONLY `android/view/LayoutInflater*.class`, so NONE of the co-compiled stubs reach the dex.

*`patch-framework.sh` wiring + regression guard:* added `LayoutInflater.java` to the javac list and the staging glob (so the 3 LayoutInflater classes — `LayoutInflater`, `$Factory`, `$Factory2` — go into `classes.dex`); updated the header comment's patched-class list. NEW build-time regression guard (lines 63-70, mirroring the existing Build.java anchor guard): the build FAILS loudly if `parseRequestFocus(parser, parent);` is absent OR if the old `<requestFocus /> not supported atm` throw is still present — directly tied to the confirmed fix, so a silent revert cannot ship. Verified effective: a simulated revert to the old throw trips the guard and exits 1; both checks pass on the real file.

*Same-pattern audit:* searched the vendored ATL `LayoutInflater` for other `not supported atm`-style inflation throws — the `<include/>` branch throws only on a genuine malformed root, `<merge/>` is handled, and no other standard layout tag is stubbed with a "not supported" throw on the live content-view path. The `<requestFocus/>` throw was the single instance of this class on the inflation path; this patch covers it. The next inflation gap is a DIFFERENT class (missing nested type, below), not another stubbed-tag throw.

*Verification (overlay build + full cargo gate; no Rust source changed — Java-overlay + build-script only):* `tools/framework-overlay/patch-framework.sh` ran clean (exit 0, prints `OK: patched framework overlay installed`) — `classes.dex` 10508 → **18656** bytes, `classes2.dex` 2498192 bytes (matches the owner's live-validated growth); only the benign javac unchecked-operations NOTE (NetworkRequest generics). Parsed `classes.dex`'s `class_defs` table directly (no `dexdump` available): it DEFINES exactly 17 classes — the 3 LayoutInflater classes plus the pre-existing patched `Build*`/`NetworkRequest*`/`ActivityManager*`/`PowerManager*` — and ZERO stub classes (View/ViewGroup/Context/TypedArray/XmlPullParser/… appear only as referenced type descriptors, never as `class_defs`, so they resolve from `classes2.dex`/real AOSP via first-dex-wins). Cargo gate: `cargo fmt --all -- --check` CLEAN; `cargo build --all-targets` 0 warnings (exit 0); `cargo clippy --all-targets --all-features -- -D warnings` 0 warnings (exit 0); `cargo test` **555 unit + 0 (main) + 4 integration (`tests/engine_milestones.rs`, 0 SKIP) + 2 doctests = 561 passed, 0 failed**; `cargo build --release` clean (artifact `/home/kue/Projects/Eclipse/target/release/eclipse`, 8,907,880 bytes — matches §5). *No live ART boot in this workflow; the dev-host live boot was the OWNER's.*

*OWNER LIVE-VALIDATION (already done — commit `521ba34` tree + this overlay):* the live boot no longer throws `<requestFocus /> not supported atm`; `ActivityNativeMain.onCreate` inflation advanced PAST `<requestFocus/>` to a NEW, DIFFERENT gap — `NoClassDefFoundError android.view.View$OnCapturedPointerListener` at `ActivityNativeMain.d1` (a newer Android nested interface ATL's vendored `View` lacks). That is the NEXT frontier (a separate future item, NOT part of this patch): add the nested type `View$OnCapturedPointerListener` to the overlay's `classes.dex` WITHOUT shadowing the large `View` class (ship the nested interface alone; do not re-dex a whole patched `View`). REMINDER: the overlay is a CACHE artifact under `~/.cache/eclipse` — if it was wiped and the boot errors `Android framework not found`, run `tools/framework-overlay/patch-framework.sh` FIRST. *Files:* `tools/framework-overlay/src/android/view/LayoutInflater.java` (NEW), `tools/framework-overlay/stubs/**` (NEW stub set + extended `Context.java`), `tools/framework-overlay/patch-framework.sh`, `AGENTS.md`.

---

### 2026-06-13 — `android.view.View` pointer-capture overlay patch (baksmali the installed View; 3-dex; headless setter) — OWNER LIVE-VALIDATED (EXIT=124 clean)

*Root cause:* Roblox's `ActivityNativeMain.d1` references the nested interface `android.view.View$OnCapturedPointerListener` and calls `View.setOnCapturedPointerListener(listener)` — AOSP's API-26 pointer-capture API. ATL's INSTALLED `View` omits both, so the boot aborts with `NoClassDefFoundError`/`NoSuchMethodError` (the NEXT-frontier gap the 2026-06-13 LayoutInflater entry's live boot revealed). `RegisterNatives` cannot add a Java *method* or a nested *type*, so this is a framework-overlay patch.

*Why NOT recompile the vendored View:* adding the *method* needs the whole `View` class. The repo's vendored `View.java` has DRIFTED from the installed jar — e.g. `setBackgroundColor(int)` is `native` in vendored but plain-Java installed — so recompiling vendored re-introduces a wrong `View` (the same drift that the 16db9eb regression already proved breaks `register_view_natives`). This SUPERSEDES the LayoutInflater entry's planned "ship the nested interface alone, do NOT re-dex a whole patched View" fix path: the nested interface alone resolves the `NoClassDefFoundError` but NOT the `setOnCapturedPointerListener(listener)` *method* call, so the whole-`View` approach is required — but it must be the AUTHORITATIVE installed `View`, not the drifted vendored one.

*Fix (`patch-framework.sh` step 4b):* baksmali-disassemble the INSTALLED framework's `View`, then insert exactly three things behind exact-count anchor guards (mirroring the `Build.java` anchor: anchor count != 1 fails loud — installed-View drift never silently guessed): (i) backing field `mCapturedPointerListener:Landroid/view/View$OnCapturedPointerListener;` after the `on_touch_listener` field; (ii) the setter `setOnCapturedPointerListener(...)V` after `setOnClickListener`'s `.end method` — a pure-Java field record (`iput-object`, `return-void`): HEADLESS, because Eclipse's engine owns pointer input, so recording the listener is the complete correct behavior; (iii) the nested class's MemberClasses annotation entry (reflection completeness). Inserts (i) and (ii) are back-checked by `grep -qF … || fail`. Then reassemble (smali) ONLY the patched `View` + the committed nested interface `smali/android/view/View$OnCapturedPointerListener.smali` (modeled byte-for-byte on the installed `View$OnClickListener`: `public static interface abstract`, `accessFlags 0x609`, EnclosingClass/InnerClass annotations, one abstract `onCapturedPointer(View, MotionEvent)Z`) into `classes2.dex`.

*3-dex overlay layout (first-dex-wins):* `classes.dex` (javac-patched Build*/NetworkRequest*/ActivityManager*/PowerManager*/LayoutInflater*) + `classes2.dex` (smali `View` + `View$OnCapturedPointerListener`, defining EXACTLY those 2 classes) + `classes3.dex` (stock whole api-impl). ART's `DexPathList` resolves `View` and the nested interface from `classes2.dex` ahead of `classes3.dex`'s stock `View` (which still lacks the nested interface), and everything else from `classes3.dex`.

*Vendored smali toolchain:* baksmali/smali 2.5.2 run via the vendored JDK's `java`, vendored at `vendor/toolchain/smali/{baksmali,smali}-2.5.2.jar` (+ `SOURCE.txt`). `vendor/` is git-ignored (local toolchain, exactly like the vendored JDK) — confirmed via `git check-ignore`; the jars are NOT committed. Env-overridable `BAKSMALI_JAR`/`SMALI_JAR`/`JAVA`; missing tools fail with an actionable error (no silent fallback), satisfying CLAUDE.md "Build and Environment Portability" (the generator survives a cache wipe; the overlay output stays a cache artifact under `~/.cache/eclipse`).

*Same-pattern audit:* the `setOnCapturedPointerListener` setter is the only pointer-capture method Roblox's `d1` needs; the nested `OnCapturedPointerListener` is the only nested type required for it. The anchor-guard + back-check discipline matches the existing `Build.java` anchor and the new step-4b guards; the headless record-the-listener semantics match the existing `EditText`/`View` listener-record precedents (the engine owns input). The `setBackgroundColor` drift this patch sidesteps is the same drift class the 16db9eb §6 entry root-caused for `register_view_natives` — here it is avoided structurally by patching the installed (not vendored) `View`, and the live boot confirms `setBackgroundColor` stays intact.

*Regression protection:* the build-time anchor guards in `patch-framework.sh` step 4b are the regression guard tied to the confirmed root cause — if a future ATL build drifts the `on_touch_listener`/`setOnClickListener`/`DeclaredOnClickListener` anchors so an insert no longer applies, the field/setter back-checks (`grep -qF … || fail`) fail the build loudly (consistent with the `Build.java` and LayoutInflater guards). *Known low-impact asymmetry (reviewer note, NOT blocking):* the (iii) MemberClasses insert has no post-insert back-check — it is reflection-completeness only (non-load-bearing: ART resolves the type from its own `class_def` and the committed nested interface carries its own EnclosingClass/InnerClass annotations; the setter does not depend on it), so a future-drift silent no-op would not break the validated boot. Optional symmetric hardening (a `grep -qF … View$OnCapturedPointerListener … || fail` after the (iii) substitution) is recorded as a future cleanup, not done here to keep the change surgical and because the entry is non-load-bearing.

*Verification (overlay build; no Rust source changed — smali/Java-overlay + build-script only):* `tools/framework-overlay/patch-framework.sh` reproduced clean (exit 0, `OK: patched framework overlay installed`) — `classes.dex` **18656** B, `classes2.dex` **42288** B, `classes3.dex` **2498192** B; only the benign pre-existing LayoutInflater javac unchecked-operations NOTE. The overlay gate confirmed `classes2.dex` defines EXACTLY `View` + `View$OnCapturedPointerListener` (custom DEX `class_defs` reader), `classes.dex` 17 defs (no `View`, no stub classes), `classes3.dex` stock 1548 defs (defines `View` but NOT the nested interface, so first-dex-wins resolves both from `classes2.dex`), and a re-baksmali of the assembled `classes2.dex` confirmed all three inserts landed (field, `iput-object` setter, MemberClasses registration). Cargo gate (re-run on this tree by the gate agent; no Rust changed): `cargo fmt --all -- --check` CLEAN; `cargo build --all-targets` 0 warnings; `cargo clippy --all-targets --all-features -- -D warnings` 0 warnings; `cargo test` **555 unit + 0 (main) + 4 integration (`tests/engine_milestones.rs`, 0 SKIP) + 2 doctests = 561 passed, 0 failed**; `cargo build --release` clean (artifact 8,907,880 bytes). *No live ART boot in this workflow; the dev-host live boot was the OWNER's.*

*OWNER LIVE-VALIDATION (already done, current tree):* `patch-framework.sh` reproduces the 3-dex overlay; the live boot is EXIT=124 clean (no crash) — `setOnCapturedPointerListener` resolves, `setBackgroundColor` is intact, and `ActivityNativeMain.onCreate` advanced PAST pointer-capture to a NEW gap: `View.nativeSetOnTouchListener` (a `View` native sibling of `nativeSetOnClickListener`, which Eclipse already binds in `register_view_natives`). That is the NEXT frontier — a quick Rust `RegisterNatives` binding in `register_view_natives` (record-the-listener), NOT an overlay change, and NOT part of this patch. *Files:* `tools/framework-overlay/patch-framework.sh`, `tools/framework-overlay/smali/android/view/View$OnCapturedPointerListener.smali` (NEW), `tools/framework-overlay/README.md`, `AGENTS.md`; vendored (git-ignored, NOT committed): `vendor/toolchain/smali/{baksmali,smali}-2.5.2.jar` + `SOURCE.txt`.

---

### 2026-06-13 — `android.view.View` touch/long-click listener natives bound (record-the-listener, headless) — closes the `nativeSetOnTouchListener` gap the pointer-capture live boot revealed

*Root cause (evidence):* the owner's dev-host live boot of the pointer-capture overlay (commit `8cf570c`, EXIT=124 clean — see the entry above) advanced `ActivityNativeMain.onCreate` → `d1` PAST pointer-capture into d1's input setup, where it hit `No implementation found for void android.view.View.nativeSetOnTouchListener(long)` (`at android.view.View.nativeSetOnTouchListener(Native Method) … at android.view.View.setOnTouchListener(…) … at com.roblox.client.ActivityNativeMain.d1`). `register_view_natives` bound `nativeSetOnClickListener` but NOT its `nativeSetOnTouchListener` sibling (confirmed in the boot log's registration line — no touch sibling).

*Source-of-truth check:* `vendor/atl/src/api-impl/android/view/View.java` — `setOnTouchListener` (line 1151) calls `nativeSetOnTouchListener(widget)` then stores `on_touch_listener` (1153); `setOnLongClickListener` (1444) calls `nativeSetOnLongClickListener(widget)` then stores `on_long_click_listener` (1446). Both natives are `protected native void …(long widget)` → instance descriptor `(J)V` (lines 1155/1448) — the EXACT `setOnClickListener`/`nativeSetOnClickListener` shape (line 1158/1161). Both return `void`, so nothing branches on a return value. Same-pattern audit of all 11 `setOn*Listener` methods in `View.java`: only THREE call a native (`click` already bound, `touch` + `long-click` now bound); the other 8 (`Key`/`Hover`/`FocusChange`/`GenericMotion`/`CreateContextMenu`/`ApplyWindowInsets`/`Drag`/`SystemUiVisibilityChange`) have empty `{}` Java bodies that call no native, so they cannot trip `UnsatisfiedLinkError`. Neither native is re-declared on any other ATL Java class (grep confirms only `View.java`), so the single `View` registration is full coverage.

*Fix (pure-Rust `RegisterNatives`, NOT an overlay change):* bound both `nativeSetOnTouchListener` and `nativeSetOnLongClickListener` on `android/view/View`, both pointing at one shared headless handler `view_set_input_listener(EnvUnowned, JObject, jlong)`. It validates the `view_registry` handle via `view_registry::with_view(widget, |_| ())` (bounds+generation-checked; stale/fabricated handle → typed `Err`, logged + ignored, never UB) and headless no-ops — the listener object lives Java-side and the engine/input path dispatches to it (Eclipse is headless; no GTK signal wiring). Mirrors the existing `image_button_set_on_click_listener` exception discipline exactly (`EnvUnowned::with_env` + `resolve::<LogErrorAndDefault>`). It DELIBERATELY does NOT flip `view_registry`'s `clickable` flag — that flag gates only the click hit-test; touch/long-click are distinct in Android and dispatched by the engine input path, the documented follow-up. One shared handler since neither native carries the listener object in its signature (only the handle), so their backing is identical. Bound through the existing per-method best-effort registrar (`register_class_natives_best_effort`), so a shipped-dex sig drift degrades to a deferred per-method `UnsatisfiedLinkError` (loud discovery), never the 58a50f6 atomic whole-class abort. `nativeSetOnLongClickListener` is INFERRED (the live boot surfaced only `nativeSetOnTouchListener`), bound proactively as the same-pattern sibling reached on the same view-setup path — low-risk because the best-effort registrar skips+WARNs if the shipped class disagrees.

*Left unbound (deliberate):* `nativeRequestFocus(long,int)` (`View.java:1212`, `(JI)V`) — the 2026-06-13 LayoutInflater `<requestFocus/>` overlay entry documents the owner-validated decision to consume `<requestFocus/>` headlessly and NOT bind `nativeRequestFocus` (the engine owns input focus headlessly; `requestFocus()` is skipped, so the native is not reached). Binding it would contradict that established decision. Return-driving `View` getters (`isFocused`/`getWidth`/`getMatrix`/…) remain unbound as discovery signals per the standing policy — out of scope for a listener-setter no-op.

*Regression protection:* extended the existing pin test `framework::tests::view_native_names_sigs_and_class_match_view_java` with name + `(J)V` descriptor assertions for both new natives (`nativeSetOnTouchListener`, `nativeSetOnLongClickListener`), tied to `View.java` lines 1155/1448 and the live-boot `UnsatisfiedLinkError`. This is the deterministic regression pin (JVM-free — ART cannot run under `cargo test`): a transcription drift in either const re-produces the exact boot-time `UnsatisfiedLinkError` instead, but the pin fails it in-harness first. Fits the project's existing framework pin-test style; no new script.

*Verification (only `src/framework.rs` changed, +87/-2):* `cargo fmt --all -- --check` CLEAN; `cargo build --all-targets` (forced rebuild of the changed crate) 0 warnings; `cargo clippy --all-targets --all-features -- -D warnings` 0 warnings; `cargo test` **555 unit + 0 (main) + 4 integration (`tests/engine_milestones.rs`, 0 SKIP) + 2 doctests = 561 passed, 0 failed** (includes the extended pin test); `cargo build --release` clean (artifact 8,910,152 bytes). No live ART boot in this workflow (off-main-thread + cyber-safeguard); the dev-host live boot is the OWNER's next step.

*Context7:* not used — no external library/API surface changed; this is internal JNI `RegisterNatives` against vendored `View.java` and the project's own `jni`-crate usage already established by the adjacent `nativeSetOnClickListener` binding. *Gate (owner next step):* rebuild the overlay with `tools/framework-overlay/patch-framework.sh` if `~/.cache/eclipse` was wiped, then `cargo run -- run <APK>` on the process main thread — expect d1's input setup to get PAST `View.nativeSetOnTouchListener`; capture the next gap one at a time (pure log observation). Standing next frontier: the scoped surface-to-engine render wiring (2194f02 §6 plan) once `onCreate` reaches RESUMED.

---

### 2026-06-13 — `android.view.Display.getSupportedRefreshRates()[F` framework-overlay patch (baksmali the installed Display; returns `{60.0f}` matching `getRefreshRate`; 3-dex with View) — OWNER LIVE-VALIDATED (EXIT=124 clean): `ActivityNativeMain` completes `onCreate` and enters `onStart`

*Root cause (evidence):* with the pointer-capture + touch-listener gaps closed (entries above), the owner's dev-host live boot advanced `ActivityNativeMain` through `onCreate` (`createGlAppsFrame` succeeds) and into `Activity.onStart`, where Roblox's framerate setup calls `Display.getSupportedRefreshRates()[F`. ATL's INSTALLED `Display` omits that AOSP method → `NoSuchMethodError`. Adding a *method* cannot be done by `RegisterNatives` (JNI binds natives, not Java method bodies/signatures), so this is a **framework-overlay** patch, extending the existing step-4b smali pipeline.

*Source-of-truth check:* the installed `Display` (baksmali-disassembled from the AUTHORITATIVE `$ORIG_FW` jar by step 4b) defines `getRefreshRate()F` exactly once, and that method HARDCODES `const/high16 v0, 0x42700000` (= IEEE-754 60.0f). It does NOT define `getSupportedRefreshRates()[F`. The repo's vendored `View.java`/`Display` sources have drifted from the installed jar (the documented reason the View patch baksmali's the installed class rather than recompiling vendored), so the same drift-proof approach is required for `Display`.

*Fix (framework-overlay, NOT Rust):* in `patch-framework.sh` step 4b, after the existing baksmali of the installed framework, anchor-guard on the UNIQUE `getRefreshRate()F` method (exact-count `== 1` guard, mirroring the `Build.java` / View anchors — aborts loudly if the installed `Display` drifted), then `perl -0pi` insert (after that method's `.end method`, `/s` non-greedy so it cannot over-match a `getRefreshRateXyz` lookalike) a `public getSupportedRefreshRates()[F` that builds a 1-element `float[]` and stores `const/high16 0x42700000` (60.0f) — the SAME constant `getRefreshRate()` returns, so the advertised refresh-rate set is FAITHFUL to the installed `Display`, not a fabricated value. A post-insert `grep -qF 'getSupportedRefreshRates()[F' || fail` back-checks the insert (drift guard). The patched `Display.smali` is then `cp`'d into the existing `smali-view` staging dir so the smali assembler emits `View` + `View$OnCapturedPointerListener` + `Display` TOGETHER into `classes2.dex`; first-dex-wins resolves all three from `classes2.dex` (stock `classes3.dex` still defines `Display`/`View` but is shadowed). Layout stays 3-dex; only `classes2.dex` grew (42288 → 43580 B) by the one added method. Smali source 4.0+ uses `.locals 3` (= `.registers 4`: 3 locals + `p0`/this); the inserted bytecode round-trips smali→dex→baksmali as valid (`new-array [F` size 1; `const/high16 0x42700000`; `aput`; `return-object`).

*Same-pattern audit:* `getSupportedRefreshRates` is the ONLY refresh-rate method Roblox's `onStart` framerate setup is known to call that the installed `Display` omits; `getRefreshRate()F` already exists (the anchor). No other installed `android.*` class needs a sibling refresh-rate method for this boot. The patch is confined to `Display` in the same smali assembly already used for `View`; no other overlay class or Rust native is touched.

*Regression protection:* the build-time anchor + back-check guards in `patch-framework.sh` step 4b ARE the regression guard tied to the confirmed root cause — the `getRefreshRate()F` exact-count `== 1` guard fails the build loudly if a future ATL `Display` drifts the anchor (renamed/duplicated/absent), and the post-insert `grep -qF 'getSupportedRefreshRates()[F' || fail` fails the build if the insert silently no-ops. Consistent with the `Build.java`, LayoutInflater, and View guards; no new script. (A reviewer-noted optional hardening — a pre-insert `grep -qF … && fail` asserting the installed `Display` does NOT already ship the method — was left out as low-value: smali `assemble` rejects a duplicate method loudly regardless, and the current installed `Display` omits it; recorded as future cleanup to keep the change surgical.)

*Verification (overlay build; no Rust source changed — smali-overlay + build-script only; working-tree change confined to `tools/framework-overlay/patch-framework.sh`, +15/-3):* `tools/framework-overlay/patch-framework.sh` reproduced clean (exit 0, `OK: patched framework overlay installed`) — `classes.dex` **18656** B, `classes2.dex` **43580** B (grew from 42288 B pointer-capture-only by the added Display method), `classes3.dex` **2498192** B; only the benign pre-existing LayoutInflater javac unchecked-operations NOTE. The overlay gate confirmed (baksmali `list classes`) that `classes2.dex` defines EXACTLY `Landroid/view/Display;` + `Landroid/view/View$OnCapturedPointerListener;` + `Landroid/view/View;` — no stray classes — and a re-baksmali confirmed `Display.getSupportedRefreshRates()[F` (with `const/high16 0x42700000` = 60.0f) and `View.setOnCapturedPointerListener` both landed. Cargo gate (re-run on this tree; no Rust changed): `cargo fmt --all -- --check` CLEAN; `cargo build --all-targets` 0 warnings; `cargo clippy --all-targets --all-features -- -D warnings` 0 warnings; `cargo test` **555 unit + 0 (main) + 4 integration (`tests/engine_milestones.rs`, 0 SKIP) + 2 doctests = 561 passed, 0 failed**; `cargo build --release` clean (artifact 8,910,152 bytes). *No live ART boot in this workflow (off-main-thread + cyber-safeguard); the dev-host live boot was the OWNER's.*

*Context7:* not used — no external library/API surface changed; this extends the project's own established baksmali/smali step-4b overlay pipeline (`smali`/`baksmali` 2.5.2, already vendored) against the AUTHORITATIVE installed `Display` dex.

*OWNER LIVE-VALIDATION (already done, current tree):* `patch-framework.sh` reproduces the 3-dex overlay (`classes2.dex` defines EXACTLY `View` + `View$OnCapturedPointerListener` + `Display`); the live boot is EXIT=124 clean — `getSupportedRefreshRates` resolves, `ActivityNativeMain` COMPLETES `onCreate` (`createGlAppsFrame` succeeds) and ADVANCES to `onStart`. **MILESTONE:** this is the first boot to complete `onCreate` and enter `onStart`. **NEW FRONTIER (the next investigation, NOT part of this patch):** an androidx lifecycle-ORDERING bug — `IllegalStateException: LifecycleOwner ActivityNativeMain is attempting to register while current state is STARTED — must call register before STARTED`, thrown when `MediaPickerProtocolV2.onCreate` (a lifecycle observer) calls `registerForActivityResult` during the `onStart` dispatch. I.e. the activity's androidx `LifecycleRegistry` reached STARTED before `ON_CREATE` was dispatched to observers; investigate how Eclipse's `drive_lifecycle` drives `ActivityNativeMain`'s steps vs how ATL's `Activity` dispatches androidx lifecycle events. *Files:* `tools/framework-overlay/patch-framework.sh`, `AGENTS.md`; vendored (git-ignored, NOT committed): `vendor/toolchain/smali/{baksmali,smali}-2.5.2.jar` + `SOURCE.txt`.

---

### 2026-06-13 — 🔁 androidx lifecycle-ordering fix: dispatch `ON_CREATE` during the activity's CREATE phase (`onPostCreate` → `Fragment.onActivityCreated`), before `onStart`

*Symptom (owner live boot of `b480bd0`, EXIT=124 clean):* `ActivityNativeMain` completes `onCreate` (`createGlAppsFrame` succeeds), then Eclipse drives `onStart`, which throws `java.lang.IllegalStateException: LifecycleOwner com.roblox.client.ActivityNativeMain@… is attempting to register while current state is STARTED. LifecycleOwners must call register before they are STARTED.` The stack is `ActivityResultRegistry.register` ← `MediaPickerProtocolV2.onCreate` (a `DefaultLifecycleObserver`'s `ON_CREATE` callback) ← `LifecycleRegistry` sync/dispatch ← a `ReportFragment`-style `ON_START` driver (`k0.onStart`, a `Fragment` method) ← `ComponentActivity.onStart` super ← `ActivityNativeMain.onStart` ← Eclipse's `nativeStartActivity`.

*Root cause (confirmed first-party — Eclipse-side ordering + missing ATL create-phase dispatch):* `ActivityNativeMain` extends androidx `ComponentActivity`, whose `LifecycleRegistry` must receive `Lifecycle.Event.ON_CREATE` during the create phase (AOSP: `performCreate` → `onCreate` → `dispatchActivityPostCreated` / `ReportFragment`, all BEFORE `onStart`). At ATL's `Build.VERSION.SDK_INT == 23` (ATL `Build.java` reads `System.getProperty("Build.VERSION.SDK_INT")` and DEFAULTS TO 23 when unset; `runtime.rs` `vm_options()` pushes only `-Xmx`/`-XX` heap opts and never `-DBuild.VERSION.SDK_INT`, so `BootPlan.sdk_int=35` does NOT reach ATL's Java `Build.VERSION.SDK_INT`), androidx's `ReportFragment.injectIfNeededIn` takes the pre-API-29 framework-`Fragment` path: it dispatches `ON_CREATE` from its `android.app.Fragment.onActivityCreated(Bundle)` override (matching the live trace's `k0.onStart` being a `Fragment` method). But (1) ATL dispatched NO create-phase fragment hook — installed `Activity.onCreate` only loops `fragment.onCreate()`, installed `Activity.onPostCreate` was a Slog-only no-op, and the base `android.app.Fragment` had no `onActivityCreated` at all; and (2) Eclipse's `drive_lifecycle` (and the static `activity_native_start_activity`) called `onCreate` → `onStart` back-to-back with nothing between. So the FIRST event the registry ever saw was `ReportFragment.onStart` → `handleLifecycleEvent(ON_START)`, which advanced `mState` to STARTED and then back-filled the skipped `ON_CREATE` to lagging observers while `currentState` was already STARTED → `MediaPickerProtocolV2`'s `ON_CREATE` callback called `registerForActivityResult`, whose `ActivityResultRegistry.register` guard (`lifecycle.currentState.isAtLeast(STARTED)` → throw) fired. (`ActivitySplash` survives the same late ordering only because it registers no `ON_CREATE` observer that calls `registerForActivityResult` — APK-confirmable, not first-party.)

*Fix (durable, NOT suppression — `fixLocation=both`, restoring AOSP's `onCreate` → `onPostCreate` → `onStart` ordering; the registry legitimately reaches CREATED first, so `register` passes its guard — no `catch`/ignore of the `IllegalStateException` anywhere):*
- **(A) Framework overlay** (`tools/framework-overlay/patch-framework.sh`, extending the established step-4b baksmali pipeline to ALSO shadow the INSTALLED `android.app.Activity` + `android.app.Fragment` into `classes2.dex`): insert the AOSP base no-op `Fragment.onActivityCreated(Landroid/os/Bundle;)V` hook (so androidx `ReportFragment`'s `@Override` resolves + is invoked), and replace the no-op `Activity.onPostCreate(Bundle)` body with a `fragments`-loop dispatching `Fragment.onActivityCreated(savedInstanceState)` — the create-phase hook ATL omitted. (`FragmentTransaction.add` already populates `activity.fragments`, verified in the installed smali, so the injected `ReportFragment` is in the loop.)
- **(B) Eclipse Rust** (`src/framework.rs`): a new `STEP_ACTIVITY_ON_POST_CREATE` recipe-step const (`android/app/Activity` · `onPostCreate` · `(Landroid/os/Bundle;)V`) + a `call_activity_on_post_create` helper (null `Bundle`, routed through `checked`), wired BETWEEN `call_activity_on_create` and `call_activity_on_start` in BOTH up-lifecycle drivers — `drive_lifecycle` (step 5 → 5b → 6 → 7) AND the static `activity_native_start_activity` (the splash→main `nativeStartActivity` handoff). ATL/Eclipse has no `performCreate` to invoke `onPostCreate`, so the driver must. `onPostCreate` (not `onCreate`) is the dispatch site: the androidx `ReportFragment` is injected during `ComponentActivity.onCreate`'s super-chain, so it is present in `fragments` only after the whole `onCreate` chain returns — and still before `onStart`.

*Why this is the correct mechanism, not a workaround:* the registry reaches CREATED during the create phase exactly as on real Android; `registerForActivityResult` then sees `state == CREATED` and passes its own guard. Nothing catches or ignores the exception; the exception simply never arises because the precondition it guards is now satisfied in the right order.

*Same-pattern audit:* grepped every Activity up-lifecycle call site in `src/framework.rs`. Exactly two drivers run the create→start cascade — `drive_lifecycle` (steps 5/5b/6/7) and the static `activity_native_start_activity` (splash→main `nativeStartActivity`) — and BOTH now drive `onPostCreate` between `onCreate` and `onStart`; `android.app.Activity.recreate()` routes through `nativeStartActivity`, so it is covered. `nativeResumeActivity` intentionally drives ONLY `onResume` (it resumes an already-created/started instance; re-dispatching the create phase there would re-fire `ON_CREATE` to an already-CREATED registry) — correctly left unpatched. `Application.onCreate` (step 3, `()V`) and the down-lifecycle `onPause`/`onStop`/`onDestroy` helpers are unrelated. On the ATL side the create-phase dispatch was missing in TWO places (base `Fragment` had no `onActivityCreated`; `Activity.onCreate`/`onPostCreate` never called it) — both fixed in the overlay.

*Regression protection (tied to the confirmed root cause; no new script):* (1) a NEW JVM-free source-order pin `framework::tests::lifecycle_drivers_call_on_post_create_between_on_create_and_on_start` — `include_str!("framework.rs")` then asserts the helper order `onCreate` < `onPostCreate` < `onStart` < `onResume` in BOTH drivers (catches a reverted/mis-ordered `onPostCreate` insert; ART cannot run under `cargo test`); (2) `STEP_ACTIVITY_ON_POST_CREATE` class/method/descriptor asserts added to `recipe_descriptors_match_confirmed_spec` and `onPostCreate` call-site-literal asserts to `call_site_literals_match_recipe_constants`; (3) build-time overlay guards in `patch-framework.sh` (the established mechanism, like the Build.java/Display/View anchors) — exact-count `== 1` anchors on the installed `Fragment.onCreate` / `Activity.onPostCreate` signatures, a `perl -0777` pristine-no-op-body guard (`grep -F` cannot multi-line match), and post-insert `grep -qF` back-checks asserting the `Fragment.onActivityCreated` hook + the `Activity.onPostCreate` dispatch landed — the build fails loudly if either is reverted or the installed-class shape drifts.

*SDK_INT resolved from first-party sources (no live boot needed for the diagnostic):* the verdict's open SDK_INT probe is answered by code — ATL `Build.java` defaults `Build.VERSION.SDK_INT` to 23 when the property is unset, and `runtime.rs` `vm_options()` pushes no `-DBuild.VERSION.SDK_INT` (no native `System.setProperty` for it either), so ATL's `Build.VERSION.SDK_INT == 23` (< 29) at boot. That is why the load-bearing dispatch is `Fragment.onActivityCreated` (the pre-API-29 `ReportFragment` path), NOT the API-29+ `registerActivityLifecycleCallbacks` → `onActivityPostCreated` path (which ATL's `ActivityLifecycleCallbacks` interface lacks anyway), and why the Rust `onPostCreate` step (B) is REQUIRED, not optional: the dispatch must run after the full `onCreate` super-chain injects the `ReportFragment` and before `onStart`.

*Verification (this tree):* `cargo fmt --all -- --check` clean; `cargo build --all-targets` 0 warnings; `cargo clippy --all-targets --all-features -- -D warnings` 0 warnings; `cargo test` **556 unit + 0 main-bin + 4 integration (`tests/engine_milestones.rs`, 0 SKIP) + 2 doctests = 562 passed, 0 failed**; `cargo build --release` clean (artifact 8,911,112 bytes). Overlay: `tools/framework-overlay/patch-framework.sh` exits 0 (`OK: patched framework overlay installed`) — `classes.dex` 18656 B, `classes2.dex` 59704 B (grew from 43580 B by the added Activity+Fragment lifecycle smali), `classes3.dex` 2498192 B; baksmali `list classes` on the shipped `classes2.dex` confirms it defines EXACTLY `Landroid/app/Activity;` + `Landroid/app/Fragment;` + `Landroid/view/Display;` + `Landroid/view/View$OnCapturedPointerListener;` + `Landroid/view/View;`, and a re-disassemble confirms `Activity.onPostCreate` invokes `Fragment->onActivityCreated(Landroid/os/Bundle;)V` and `Fragment.onActivityCreated(Bundle)` round-tripped through smali assembly. *No live ART boot in this workflow (off-main-thread + cyber-safeguard preclude it); the dev-host live boot is the OWNER's (§5 START-HERE).*

*Context7:* `/androidx/androidx` consulted — confirmed `ReportFragment` extends `android.app.Fragment`, declares `onActivityCreated(Bundle)`, and that `injectIfNeededIn` uses the framework-`Fragment` path pre-API-29 (the `LifecycleCallbacks`/`onActivityPostCreated` path was added on API 29+).

*Files:* `src/framework.rs` (recipe step + helper + driver wiring + pins), `tools/framework-overlay/patch-framework.sh` (Activity/Fragment shadow + guards), `AGENTS.md`; overlay output is a `~/.cache` artifact regenerated by the in-repo script (not committed); vendored smali toolchain stays git-ignored under `vendor/toolchain/smali/`.

*Residual risk (note for next session, not a blocker):* the create-phase dispatch reaches androidx's `ReportFragment.onActivityCreated` only if that fragment is actually in `activity.fragments` via the framework `android.app.FragmentManager` at `onPostCreate` time. If the bundled androidx routes its `ReportFragment` through a support FragmentManager instead (which would not feed `activity.fragments`), the named fallback is making ATL's no-op `Activity.registerActivityLifecycleCallbacks` (overlay) store the callback and feeding the API-29+ `LifecycleCallbacks.onActivityPostCreated` path — to be diagnosed via the owner's live boot log only (no binary inspection).

---

- **2026-06-13 — 🎬 `Display.getMode()`/`Display$Mode` + `Vibrator.cancel()` OVERLAY PATCHES — `ActivityNativeMain` IS NOW FULLY RESUMED (OWNER LIVE-VALIDATED, EXIT=124 clean).**

  *Symptom / evidence:* with the androidx lifecycle-ordering fix in place, the owner's live boot advanced `ActivityNativeMain` PAST `onStart` into `onResume`, where Roblox hits two more gaps in ATL's INSTALLED `android.*` framework during the resume startup: (1) `android.view.Display.getMode()Landroid/view/Display$Mode;` → `NoSuchMethodError` — ATL's installed `Display` omits BOTH the `getMode()` method AND the `Mode` nested class; (2) `android.os.Vibrator.cancel()V` → called on a `Timer` thread (caught by Roblox's own handler, so non-fatal noise) but ATL's `Vibrator` (`hasVibrator`/`vibrate` only) omits it.

  *Root cause:* purely missing Java-level surface on the installed framework classes — `RegisterNatives` cannot add a Java *method* or a nested *type*, so this is a **framework-overlay** fix, not a Rust one. It extends the existing step-4b smali pipeline that already shadows the installed `View`/`Display`/`Activity`/`Fragment`.

  *Fix (faithful to the installed classes, drift-proof — mirrors the `View`/`Display`/`Activity`/`Fragment` anchor pattern):* in `tools/framework-overlay/patch-framework.sh`, (A) `Display.getMode()` is inserted into the baksmali-disassembled AUTHORITATIVE installed `Display`, anchored after the UNIQUE `getWidth()I` method (exact-count==1 guard + an "already declares `getMode`" drift guard + post-insert `grep -qF` back-check). It constructs a new `android.view.Display$Mode` from the installed `Display`'s `window_width:I`/`window_height:I` `public static` statics (the SAME fields the pre-existing `getWidth`/`getHeight` read) + `60.0f` (`const/high16 0x42700000`, the same constant ATL's `getRefreshRate()` hardcodes — so the reported mode is faithful to the installed `Display`, not fabricated). The synthetic mode uses `modeId == 0` (Eclipse advertises a single mode; owner-validated that the native `getMode` caller accepts it). (B) The nested class is a NEW committed source `tools/framework-overlay/smali/android/view/Display$Mode.smali` — public static final (`accessFlags 0x19`), `EnclosingClass`/`InnerClass` annotations naming `android/view/Display`, fields `mModeId:I`/`mWidth:I`/`mHeight:I`/`mRefreshRate:F`, constructor `<init>(IIIF)V` (`.registers 5`, matching `getMode`'s `invoke-direct {v0,v1,v2,v3,v4}`), and AOSP getters `getModeId()I`/`getPhysicalWidth()I`/`getPhysicalHeight()I`/`getRefreshRate()F`. (C) `Vibrator.cancel()` is inserted into the disassembled installed `Vibrator`, anchored after the UNIQUE `vibrate(J)V` (same exact-count + already-declares + grep-back-check guards); a `return-void` no-op faithful to Eclipse's no-vibration-device backing. (D) `Display$Mode.smali` and the patched `Vibrator.smali` are `cp`'d into the smali assembly dir (with a new `android/os` subdir) and assembled together with `View`+`View$OnCapturedPointerListener`+`Display`+`Activity`+`Fragment` into `classes2.dex`. The header comment was updated; the line-226 closing echo's class list is a cosmetic log string left untouched (out of scope for this surgical patch).

  *Same-pattern audit:* every other ATL framework gap fixed by this pipeline (`View`, `Display.getSupportedRefreshRates`, `Activity.onPostCreate`, `Fragment.onActivityCreated`) already uses the identical anchor-guarded baksmali/smali insert; the two new inserts add nothing novel structurally. No equivalent unguarded inserts elsewhere.

  *Regression protection:* the build-time guards inside `patch-framework.sh` — for each insert, an exact-count==1 anchor check, an "already declares" drift guard, and a post-insert `grep -qF` back-check — fail the build loudly if the installed-class shape drifts or the insert is reverted. `smali assemble` succeeding is itself a structural validity check (it rejects bad registers/types/descriptors). This is the same regression mechanism as the prior overlay entries; no new test script was warranted (ART cannot run under `cargo test`).

  *Milestone (owner live-validated, current tree):* `ActivityNativeMain` is now FULLY RESUMED — `onCreate`→`onPostCreate`→`onStart`→`onResume` ALL fire, `createGlAppsFrame` succeeds, the lifecycle-ordering fix + `getMode` hold; EXIT=124 clean. The boot advances PAST `getMode` to a SEPARATE next gap.

  *New frontier (next task):* a series of framework-completeness gaps in `onResume` startup; the IMMEDIATE one is `android.app.ActivityManager$MemoryInfo.writeToParcel(Landroid/os/Parcel;I)V` (`NoSuchMethodError`) — the patched-`javac` `MemoryInfo` must implement `Parcelable` + `writeToParcel`, a **javac-overlay edit to `tools/framework-overlay/src/android/app/ActivityManager.java`, NOT smali** (depends on ATL's stock `Parcel` write-API surface). Capture the next gap one at a time by log observation only. STANDING FRONTIER once the resume gaps clear is the surface-to-engine render wiring.

  *Verification (this tree):* `tools/framework-overlay/patch-framework.sh` exits 0 (`OK: patched framework overlay installed`); 3-dex `api-impl.jar` — `classes.dex` 18656 B, `classes2.dex` 60968 B (grew from 59704 B Activity+Fragment by the added `Display.getMode` + `Display$Mode` + `Vibrator.cancel`), `classes3.dex` 2498192 B; baksmali `list classes` on the shipped `classes2.dex` confirms it defines EXACTLY the 7 classes `Landroid/app/Activity;` + `Landroid/app/Fragment;` + `Landroid/os/Vibrator;` + `Landroid/view/Display;` + `Landroid/view/Display$Mode;` + `Landroid/view/View;` + `Landroid/view/View$OnCapturedPointerListener;` — no strays. `cargo fmt --all -- --check` clean; `cargo build --all-targets` 0 warnings; `cargo clippy --all-targets --all-features -- -D warnings` 0 warnings; `cargo test` **556 unit + 0 main-bin + 4 integration (`tests/engine_milestones.rs`, 0 SKIP) + 2 doctests = 562 passed, 0 failed**; `cargo build --release` clean (artifact 8,911,112 bytes). *No live ART boot in this workflow (off-main-thread + cyber-safeguard preclude it); the dev-host live boot is the OWNER's (§5 START-HERE) and is the source of the EXIT=124-clean resume milestone above.*

  *Files:* `tools/framework-overlay/patch-framework.sh` (the `Display.getMode` + `Vibrator.cancel` insert blocks + the assemble `cp`/`mkdir` lines + the header comment), NEW `tools/framework-overlay/smali/android/view/Display$Mode.smali`, `AGENTS.md`; overlay output is a `~/.cache` artifact regenerated by the in-repo script (not committed); vendored smali toolchain stays git-ignored under `vendor/toolchain/smali/`.

---

### 2026-06-13 — 🟢 `ActivityManager$MemoryInfo` made `Parcelable` (`writeToParcel`/`describeContents`) — Roblox BOOTS TO APP_READY (Startup/Landing), `ActivityNativeMain` gets PAST `onResume` (OWNER LIVE-VALIDATED, EXIT=124 clean)

  *Symptom / evidence:* with the `Display.getMode`/`Vibrator.cancel` resume gaps fixed and `ActivityNativeMain` FULLY RESUMED, the owner's live boot threw `NoSuchMethodError` on `android.app.ActivityManager$MemoryInfo.writeToParcel(Landroid/os/Parcel;I)V` in `ActivityNativeMain.onResume` startup. The overlay's patched (`javac`) `MemoryInfo` is a verbatim ATL copy (+ the `RunningAppProcessInfo` importance/pkgList patch) and declared only the 4 plain fields (`availMem`/`totalMem`/`threshold`/`lowMemory`) — it did NOT implement `Parcelable`, so the call had no target.

  *Root cause:* purely missing Java-level surface on the overlay's patched `MemoryInfo`. AOSP's `ActivityManager.MemoryInfo` IS `Parcelable` (declares `describeContents()` + `writeToParcel(Parcel,int)` + a `CREATOR`); ATL's copy dropped the Parcelable surface, and Roblox's resume path parcels a `MemoryInfo`. `RegisterNatives` cannot add a Java *method* or an `implements` clause, so this is a **framework-overlay** fix — and specifically the **javac path**, because `MemoryInfo` is the javac-patched `android/app/ActivityManager` class (NOT one of the smali-shadowed installed classes).

  *Fix (javac path, faithful to AOSP, minimal):* `tools/framework-overlay/src/android/app/ActivityManager.java` — `public static class MemoryInfo` now `implements android.os.Parcelable`, with `describeContents()` returning `0` (no FDs) and `writeToParcel(android.os.Parcel dest, int flags)` writing its 4 declared fields via the stock Parcel write-API: `dest.writeLong(availMem)`, `dest.writeLong(totalMem)`, `dest.writeLong(threshold)`, `dest.writeInt(lowMemory ? 1 : 0)`. ATL's installed `Parcel` was verified to provide `writeLong(J)V` + `writeInt(I)V`, so the invoke-virtuals resolve at runtime. So the patched source compiles against the compile-only stub tree (`api-impl.jar` ships dex, not classfiles, so it can't be a javac classpath), `tools/framework-overlay/stubs/android/os/Parcel.java` was extended with `writeLong(long)` + `writeInt(int)` no-op shells. The pre-existing `stubs/android/os/Parcelable.java` interface already declares the `describeContents()`/`writeToParcel(Parcel,int)` pair, so no stub change there. Both stubs are **compile-only and NEVER dexed** — the stage glob (`patch-framework.sh`) copies only `Build*`/`PowerManager*`/`NetworkRequest*`/`ActivityManager*`/`LayoutInflater*` classes into `classes.dex`; `android/os/Parcel.class`/`Parcelable.class` are excluded, so the REAL ATL `Parcel`/`Parcelable` are used at runtime. `MemoryInfo` is staged into `classes.dex` by the existing `android/app/ActivityManager*.class` glob — **no `patch-framework.sh` change needed**.

  *Same-pattern audit:* the only other `Parcelable` nested class in the overlay's `ActivityManager.java` is `RunningServiceInfo`, which already declares the `describeContents()`/`writeToParcel` pair — so this patch brings `MemoryInfo` in line with the in-file precedent. No other `MemoryInfo` definition exists anywhere in the overlay (the smali path is untouched by this javac-only patch; `classes2.dex` stays exactly the 7 installed-class shadows). Like the sibling `RunningServiceInfo`, `MemoryInfo` omits the read-side `CREATOR` — intentional and correct for the confirmed failure (Roblox only calls `writeToParcel` in the validated boot); adding `CREATOR` would be speculative scope (Simplicity First). Flagged for a future reader: if a later boot shows Roblox reading a `MemoryInfo` back (`CREATOR.createFromParcel`), add `CREATOR` + a matching read path then, mirroring the write field order (`availMem`/`totalMem`/`threshold` long, then `lowMemory` int). (Pre-existing, untouched: `getMemoryInfo(MemoryInfo outInfo)` does a no-op local reassignment `outInfo = new MemoryInfo();` that never populates the caller's object — inherited verbatim ATL behavior, out of this patch's scope.)

  *Regression protection:* the build-time guards inside `patch-framework.sh` (the dex-entry/byte-size and class-set checks) plus the `javac` compile itself — `writeToParcel` referencing `Parcel.writeLong`/`writeInt` will not compile if the stub regresses, and the build fails loudly. No new test script was warranted (ART cannot run under `cargo test`, and the failure is a Java-level method-resolution gap the Rust unit/integration suite cannot exercise); the verifiable invariant is the rebuilt `classes.dex` containing `MemoryInfo .implements Landroid/os/Parcelable;` with the exact `writeToParcel(Landroid/os/Parcel;I)V` signature, checked via baksmali in verification below.

  *Milestone (owner live-validated, current tree):* with `MemoryInfo.writeToParcel`, `writeToParcel` resolves and `ActivityNativeMain` gets PAST `onResume` ENTIRELY. The app reaches RESUMED, the main `Looper` pump runs, the engine loads its DataModel (`rbxasset://places/Mobile.rbxl`) and reaches **APP_READY (Startup/Landing)** — Roblox boots to the landing/app-ready stage; the boot is EXIT=124 clean.

  *New frontier (next tasks):* (a) the now-TOLERATED running-loop framework gaps the pump survives (non-fatal in the running pump today, but they throw): `View.nativeIsAttachedToWindow` → `boolean` (return-driven; the activity view IS attached, so return `true`), `View.getWindowVisibleDisplayFrame(Rect)` (fill the `Rect` with the window frame), and `android.app.Dialog.nativeInit` → `long` (Dialog peer) — bind these so UI/dialog messages stop throwing in the pump; (b) the STANDING render frontier — wire the engine `AndroidGLView` surface to Eclipse's window (Eclipse currently runs its own Vulkan clear-and-present loop while the engine renders to its own surface) so rendered frames appear; (c) login/auth (`apis.roblox.com` 403s — environmental, needs real credentials). Capture the running-loop gaps one at a time by log observation only.

  *Verification (this tree):* `tools/framework-overlay/patch-framework.sh` exits 0 (`OK: patched framework overlay installed`); 3-dex `api-impl.jar` — `classes.dex` 18832 B (grew from 18656 B, consistent with the larger `Parcelable` `MemoryInfo`), `classes2.dex` 60968 B UNCHANGED, `classes3.dex` 2498192 B UNCHANGED; baksmali of the produced `classes.dex` confirms `android/app/ActivityManager$MemoryInfo` `.implements Landroid/os/Parcelable;` with `describeContents()I` returning 0 and `writeToParcel(Landroid/os/Parcel;I)V` invoking `Parcel->writeLong(J)V` ×3 (`availMem`/`totalMem`/`threshold`) + `Parcel->writeInt(I)V` ×1 (`lowMemory` 1/0) — the exact stock-Parcel write-API surface and the exact signature of the `NoSuchMethodError` target; `classes2.dex` baksmali `list classes` still defines EXACTLY the 7 smali classes (`Activity`/`Fragment`/`Vibrator`/`Display`/`Display$Mode`/`View`/`View$OnCapturedPointerListener`) — smali path untouched. `cargo fmt --all -- --check` clean; `cargo build --all-targets` 0 warnings; `cargo clippy --all-targets --all-features -- -D warnings` 0 warnings; `cargo test` **556 unit + 0 main-bin + 4 integration (`tests/engine_milestones.rs`, 0 SKIP) + 2 doctests = 562 passed, 0 failed** (no Rust changed → no test delta); `cargo build --release` clean (artifact 8,911,112 bytes). *No live ART boot in this workflow (off-main-thread + cyber-safeguard preclude it); the dev-host live boot is the OWNER's (§5 START-HERE) and is the source of the EXIT=124-clean APP_READY milestone above.*

  *Files:* `tools/framework-overlay/src/android/app/ActivityManager.java` (`MemoryInfo implements Parcelable` + `describeContents`/`writeToParcel`), `tools/framework-overlay/stubs/android/os/Parcel.java` (compile-only `writeLong(long)`/`writeInt(int)` shells — never dexed), `AGENTS.md`; overlay output is a `~/.cache` artifact regenerated by the in-repo script (not committed).

---

### 2026-06-13 — 🖼️ Render Phase 1: publish Eclipse's REAL winit-window WSI handle as the engine `ANativeWindow*` (production-side mirror of the proven `__gl-test-anw` harness); the evidence-backed Phase 2 plan + the prior abort's lesson

  *Context / goal:* APP_READY holds (entry above), and the standing frontier is the surface-to-engine render wiring — Eclipse runs its own Vulkan clear/present loop while the engine renders to its own surface, so no engine frames appear in Eclipse's window. The render-integration plan is split into safe increments. This is **Render Phase 1 (WSI publish)** — the first increment — done in `src/graphics.rs` only. Phase 2 (`surfaceCreated`/`surfaceChanged` dispatch + present-loop handoff) is DELIBERATELY NOT in this change; it is the next task (plan below).

  *What Phase 1 does (faithful to the proven `__gl-test-anw` harness — `egl_engine::GlAnwTestApp::resumed`/`render_engine_style`):* `GameWindow` gains an `engine_window: Option<crate::egl_engine::EngineNativeWindow>` field — a drop-guard. In `GameWindow::resumed`, right after the winit window is created, Eclipse reads the window/display handle (`window.window_handle()` → `as_raw()`) + `inner_size()`, computes `WindowGeometry::from_physical`, calls `ndk_registry::set_engine_window_geometry`, then builds `EngineNativeWindow::new(window_handle, geometry)` and stores it. `EngineNativeWindow::new` internally `register_wsi_window`s the REAL WSI pointer (`egl_engine.rs:433`); its `Drop` `unregister_wsi_window`s (`egl_engine.rs:468`). So `ndk_registry::current_wsi_window()` becomes Eclipse's real window, and `native_provider::eclipse_anativewindow_fromsurface` (`native_provider.rs:3878`) returns the real WSI handle instead of the geometry-only fallback — exactly what the green `egl_engine` `gl_test_anw_binds_real_wsi_handle` harness asserts, now wired in production. `WindowEvent::Resized` re-publishes via a new free helper `publish_engine_window_geometry(wsi_ptr, w, h)` (`set_engine_window_geometry` + idempotent `register_wsi_window` on the same pointer) so `ANativeWindow_getWidth`/`getHeight` track resizes. Both the no-raw-handle arm and the unsupported-display arm are NON-FATAL (warn + leave `engine_window = None`; the window still opens and the geometry-only fallback stands), matching the adjacent Vulkan non-fatal pattern.

  *Why this is the correct first increment, not a workaround:* it publishes the real surface handle the engine needs to render into Eclipse's window — the root the render frontier requires — without yet forcing the handoff. The pointer registered by `EngineNativeWindow::new` (`self.native_window`) is the exact pointer `as_native_window()` returns, so the production `Resized` re-publish hits the same WSI entry; `register_wsi_window` is idempotent on the pointer (`ndk_registry.rs:353-365`), so the resize updates the existing geometry, not a duplicate. The X11 XID / Wayland `wl_egl_window` pointer round-trips identically (`as usize`) between `new()` registration and the resize-arm `as_native_window()`, so the registry key is stable.

  *Same-pattern audit:* the only non-test callers of `register_wsi_window` / `EngineNativeWindow::new` / `set_engine_window_geometry` outside `egl_engine.rs` / `ndk_registry.rs` are this new `graphics.rs` code (the `native_provider.rs` matches at lines 6267/6293/6326 are inside the `#[cfg(test)]` module at 4394) — confirming this is the FIRST production WSI publish; no other production render path needed the same wiring.

  *Regression protection (tied to the root, no new script):* a new unit test `graphics::tests::publish_engine_window_geometry_registers_real_wsi_mapping` pins that the helper registers the real WSI mapping. It uses a unique fabricated pointer (`0xECC1_0613`) and asserts only THAT pointer's `wsi_window_geometry(ptr)` (NOT the order-dependent process-global `current_wsi_window`), so it is order-independent vs the `ndk_registry` WSI tests sharing the binary; it defensively `unregister_wsi_window`s before/between phases, and would FAIL if `register_wsi_window` were dropped from `publish_engine_window_geometry`. (The WSI publish in `resumed` needs a real `RawWindowHandle`, so the resize re-publish was factored into the testable free helper; the live WSI publish itself is covered by the owner's dev-host boot.)

  *OWNER LIVE-VALIDATION (already done, current tree, dev-host main loop):* the live boot logs `engine ANativeWindow published (real WSI handle); ANativeWindow_fromSurface now returns Eclipse's window width=800 height=600` and stays EXIT=124 clean to APP_READY / DataModel-load. **CRUCIAL OBSERVATION:** with the window published but no `surfaceCreated` dispatch, the engine does NOT call `ANativeWindow_fromSurface` / `surfaceCreated` on its own (ZERO such log lines) — direct evidence that the engine will not pull the surface until told to. This is why Phase 2's `surfaceCreated`/`surfaceChanged` dispatch is REQUIRED.

  *NEXT TASK = Render Phase 2 (evidence-backed plan; NOT in this commit):* (a) **JNI-dispatch the surface lifecycle** — call the `RBXSurfaceView` `SurfaceHolder.Callback.surfaceCreated()` then `surfaceChanged(format, w, h)` into the engine's `AndroidGLView` callback. SELF-GATED: dispatch only once the engine has actually registered its callback (read the `SurfaceView` `mCallbacks` list via JNI and check it is non-empty), retrying each main-loop tick until then, so the window is never blanked prematurely. `format = WINDOW_FORMAT_RGBA_8888`; `w`/`h` = the published geometry. Capture the `RBXSurfaceView` peer as a Global ref in `view_native_constructor`, found by the concrete class name `com.roblox.client.RBXSurfaceView`. (b) **PRESENT-LOOP HANDOFF — the CORRECT design (and the prior attempt's blocking bug):** when the engine has CLAIMED the surface, Eclipse must `self.renderer.take()` to DROP the `VulkanRenderer` — its `Drop` does `device_wait_idle` + `destroy_swapchain` + `destroy_surface`, truly RELEASING the `wl_surface`/`VkSurfaceKHR`. Going merely QUIESCENT (the prior abort's mistake) leaves TWO owners of one surface, which deadlocks/blocks. Trigger the `take()` off the engine actually claiming the surface — set a flag inside `eclipse_anativewindow_fromsurface` when it returns the real WSI pointer — so Eclipse holds the surface until the engine genuinely takes it, then releases. Keep pumping the main `Looper` throughout.

  *Verification (this tree; ONLY `src/graphics.rs` changed):* `cargo fmt --all -- --check` clean; `cargo build --all-targets` 0 warnings; `cargo clippy --all-targets --all-features -- -D warnings` 0 warnings (forced recheck via `touch src/graphics.rs` to defeat incremental caching; re-checked the eclipse crate clean); `cargo test` **557 unit + 0 main-bin + 4 integration (`tests/engine_milestones.rs`, 0 SKIP) + 2 doctests = 563 passed, 0 failed** (+1 unit: the new pin); `cargo build --release` clean (artifact 8,913,704 bytes, grew from 8,911,112 by the Phase 1 WSI-publish wiring). *No live ART boot in this workflow (off-main-thread + cyber-safeguard preclude it); the EXIT=124-clean WSI-publish validation is the OWNER's dev-host boot (§5 START-HERE).*

  *Context7:* not used — no external library/API surface changed; this is the production-side wiring of Eclipse's own already-proven `egl_engine` / `ndk_registry` WSI APIs (the same ones the `__gl-test-anw` harness exercises), against the project's own `winit` `window_handle()`/`raw-window-handle` integration already imported in `graphics.rs`.

  *Files:* `src/graphics.rs` (the `engine_window` field, the `resumed` WSI publish, the `Resized` re-publish + `publish_engine_window_geometry` helper, and the new pin test), `AGENTS.md`.

---

### 2026-06-13 — 🖼️ Render Phase 2: self-gated `surfaceCreated`/`surfaceChanged` dispatch into the engine's `RBXSurfaceView` + a renderer-DROP present-loop handoff triggered when the engine pulls the surface (RELEASE, not quiesce — the prior abort's lesson)

  *Context / goal:* Phase 1 (entry above) published Eclipse's real winit-window WSI handle as the engine `ANativeWindow*` and LIVE-CONFIRMED the crucial observation: with the window published but no `surfaceCreated` dispatch, the engine does NOT call `ANativeWindow_fromSurface`/`surfaceCreated` on its own (ZERO such log lines). So the engine will not pull the surface until told to. Phase 2 is the next increment of the render-integration plan: drive the engine to pull Eclipse's published surface and render into it, then hand the surface off cleanly. Confined to the 5 owned files; no overlay, no external API surface.

  *Recorded forensics this builds on (first-party — Eclipse's own source + log observation, NOT RE):* the vendored `vendor/atl/src/api-impl/android/view/SurfaceView.java` has `final ArrayList<SurfaceHolder.Callback> mCallbacks` (line 13; descriptor `Ljava/util/ArrayList;`), and `private void surfaceCreated()` (line 33, `()V`) / `private void surfaceChanged(int format, int width, int height)` (line 27, `(III)V`) — both PRIVATE, each fanning the lifecycle out to every subscribed callback, with NO Java caller anywhere in ATL. The engine's `AndroidGLView` subscribes via `getHolder().addCallback(...)`, which appends to `mCallbacks` (line 61-66), so `mCallbacks` becomes non-empty once it has registered. `view_native_constructor` captures the `RBXSurfaceView` peer as a `Global<JObject>` (`set_jobject`) and records its concrete class via `getClass().getName()` (`view_class_name`), i.e. the dot-form FQN `com.roblox.client.RBXSurfaceView`. A JNI `call_method` bypasses Java private-access checks.

  *What it does (the 4 spec steps):* **(1)** `view_registry::find_by_class(name) -> Option<ViewHandle>` (next to `active_root`): scans `reg.slots` under the registry lock, returns `pack(index, generation)` of the first LIVE (occupied) slot whose `state.class_name == name`, else `None`. **(2)** `framework::dispatch_surface_lifecycle(vm, w, h) -> Result<bool, FrameworkError>`, modeled exactly on `dispatch_touch_to_view`: null-guarded `JavaVM::from_raw` (SAFETY mirrors the touch path), `attach_current_thread`, `catch_unwind(AssertUnwindSafe(...))` over an inner `surface_lifecycle`. `surface_lifecycle`: (a) `find_by_class("com.roblox.client.RBXSurfaceView")` → `None` returns `Ok(false)`; (b) SELF-GATE via `with_jobject` — `get_field(surface_view, "mCallbacks", FieldSignature::from_raw_parts("Ljava/util/ArrayList;", JavaType::Object)).l()` then `call_method(callbacks, "size", "()I").i()`; `size <= 0` returns `Ok(false)` (engine has not subscribed yet — do NOT dispatch into an empty list, retry next tick so the window is never blanked prematurely); (c) when non-empty, `call_method surfaceCreated ()V` THEN `call_method surfaceChanged (III)V` with `[WINDOW_FORMAT_RGBA_8888, width, height]` (surfaceCreated BEFORE surfaceChanged per the AOSP `SurfaceHolder.Callback` contract), returns `Ok(true)`. Every JNI call routes through `checked` (a thrown Java exception is `exception_describe` + `exception_clear`ed and returned as typed `FrameworkError::Jni`, never left pending). The `with_jobject` result is flattened exactly like `touch_view`: `Ok(Some(inner)) => inner` (propagating a JNI `Err` as `Err`), `Ok(None) => Ok(false)`, `Err(registry) => Ok(false)` (logged debug). **(3)** `native_provider::eclipse_anativewindow_fromsurface` calls `ndk_registry::set_engine_claimed_surface(true)` inside the `current_wsi_window().is_some()` REAL-WSI-pointer branch ONLY (NOT the geometry-only fallback) — the engine actually pulling the real surface is the handoff trigger; backed by a new lock-free `static ENGINE_CLAIMED_SURFACE: AtomicBool` + `set_engine_claimed_surface`/`engine_claimed_surface` (Release/Acquire) in `ndk_registry`. **(4)** `graphics.rs`: `GameWindow` gains `surface_dispatched`/`handed_off` (both `false`). In `about_to_wait`, after the unconditional `pump_main_looper`: if `!surface_dispatched && engine_window.is_some()`, call `dispatch_surface_lifecycle(vm, w, h)` with `(w,h)` from `engine_window_geometry().unwrap_or((1,1))` — `Ok(true)` sets the flag + logs, `Ok(false)` retries, `Err` warns + retries; separately, if `!handed_off && engine_claimed_surface()`, set `self.renderer = None` to DROP the `VulkanRenderer`, set `handed_off`, `set_control_flow(ControlFlow::Poll)`, log the handoff.

  *Why DROP, not quiesce (the prior abort's lesson — and why this is a root-cause design, not a workaround):* the engine renders into the SAME `wl_surface`/`VkSurfaceKHR` once it claims Eclipse's published `ANativeWindow`. Two producers (Eclipse's Vulkan present loop + the engine's EGL window surface) must NOT share one `wl_surface` — that is the resource conflict. The prior attempt went merely QUIESCENT (stopped drawing but kept the `VulkanRenderer` alive), leaving TWO live owners of the one surface, which blocked. The correct fix is to RELEASE: `self.renderer = None` runs `VulkanRenderer::Drop` (`device_wait_idle` → `destroy_swapchain` → `destroy_surface(self.surface)` — verified at `graphics.rs` Drop), so the engine's own EGL window surface owns the `wl_surface` alone. `self.window` (the `wl_surface` owner) and `self.engine_window` (the `wl_egl_window` built on it in `resumed`) are NOT dropped — only the renderer is nulled — so after handoff the engine's surface still has its backing window. The `RedrawRequested` `Some(renderer)` guard then stops Eclipse drawing/re-arming `request_redraw`; `ControlFlow::Poll` keeps `about_to_wait` ticking so the main `Looper` keeps pumping and the engine runs on.

  *Same-pattern audit:* `surfaceCreated`/`surfaceChanged` are dispatched ONLY from the new `surface_lifecycle` (confirmed: SurfaceView.java has them private with no Java caller, so this native dispatch is the sole driver — no competing/duplicate path). `set_engine_claimed_surface(true)` is in exactly one production place (the real-WSI branch of `fromSurface`); the geometry-only fallback never sets it. `self.renderer = None` appears once, only in the handoff arm; there is no other renderer-drop and no quiescent early-return that keeps the renderer alive (the prior abort's bug is not present). `find_by_class` is the only new slab-scan-by-class helper. Phase 1's WSI-publish callers are unchanged and already audited.

  *Regression protection (tied to the root, no new script):* (1) `view_registry::tests::find_by_class_locates_the_right_handle_and_is_none_for_absent_class` — two live slab entries in a unique `eclipse.test.FindByClass*` namespace (so a parallel test sharing the process-global slab cannot alias the result), asserts the right handle per class, `None` for an absent class, and `None` after the match is freed (proves class discrimination + liveness filtering). (2) `ndk_registry::tests::engine_claimed_surface_round_trips_set_and_get` — set/get the flag, serialized under `WSI_TEST_LOCK`, restored to the boot-initial `false` so the live boot's first-tick read is unaffected. (3) `framework::tests::view_native_names_sigs_and_class_match_view_java` extended with `ARRAY_LIST_SIG == "Ljava/util/ArrayList;"`, `RBX_SURFACE_VIEW_CLASS == "com.roblox.client.RBXSurfaceView"`, `WINDOW_FORMAT_RGBA_8888 == 1` — a transcription drift in any load-bearing string/value (the `mCallbacks` descriptor, the peer class, or the format) fails CI instead of silently no-op-ing (empty `mCallbacks` read) or `NoSuchMethod`-ing the live boot. The surface dispatch and the render itself are owner-live-boot-validated (ART cannot run under `cargo test`).

  *Verification (this tree; the 5 work files):* `cargo fmt --all -- --check` clean; `cargo build --all-targets` 0 warnings; `cargo clippy --all-targets --all-features -- -D warnings` 0 warnings (forced recheck via `touch` of all 5 changed files); `cargo test` **559 unit (+2: the two new pins) + 0 main-bin + 4 integration (`tests/engine_milestones.rs`, 0 SKIP) + 2 doctests = 565 passed, 0 failed**; `cargo build --release` clean (artifact 8,926,184 bytes, grew from 8,913,704 by the Phase 2 dispatch + handoff wiring). *No live ART boot in this workflow (off-main-thread + cyber-safeguard preclude it); RUNTIME CORRECTNESS — the engine actually rendering into Eclipse's window — is confirmed ONLY by the OWNER's dev-host MAIN-LOOP boot (§5 START-HERE): the surfaceCreated/Changed dispatch log → the engine calling `fromSurface`/`eglCreateWindowSurface` → the renderer-released handoff log → THE FIRST ENGINE FRAME. If the dispatch logs but no handoff follows, capture that as the next forensics signal (log-observation only; do NOT RE the APK/libroblox).*

  *Context7:* not used — the `jni` crate here is 0.22.4 with a project-specific `Env`/`Global` API (NOT upstream `JNIEnv`/`GlobalRef`); the authoritative source is the project's own code (the `dispatch_touch_to_view` model, the `get_field` + `FieldSignature::from_raw_parts(_, JavaType::Object)` precedent at `framework.rs`, `checked`'s exception discipline, `with_jobject`'s `Result<Option<R>>` contract) plus the vendored `SurfaceView.java`. `winit` 0.30's `ActiveEventLoop::set_control_flow(ControlFlow::Poll)` and the `jni`-0.22.4 `JValueOwned` accessors (`.l()`/`.i()`/`.v()`) were confirmed by reading the vendored crate sources, not from memory.

  *Files:* `src/framework/view_registry.rs` (`find_by_class` + its test), `src/loader/ndk_registry.rs` (`ENGINE_CLAIMED_SURFACE` + set/get + test), `src/loader/native_provider.rs` (set the flag in the real-WSI branch of `fromSurface`), `src/framework.rs` (`dispatch_surface_lifecycle`/`surface_lifecycle` + the `ARRAY_LIST_SIG`/`RBX_SURFACE_VIEW_CLASS`/`WINDOW_FORMAT_RGBA_8888` consts + the extended name/sig pin), `src/graphics.rs` (the `surface_dispatched`/`handed_off` fields + the `about_to_wait` dispatch + renderer-drop handoff), `AGENTS.md`.

---

### 2026-06-13 — 🖼️ Render Phase 2.1: DROP-BEFORE-DISPATCH — release the `wl_surface` (drop `VulkanRenderer`) STRICTLY BEFORE dispatching `surfaceCreated`, fixing the `EGL_BAD_ALLOC` (3003) the owner Phase 2 live boot revealed

  *Evidence (owner live boot, commit `ae20ef5`, EXIT=124 clean — the success marker):* the Phase 2 dispatch + handoff fired end to end and the engine subscribed — the log shows the engine `surfaceCreated` callback ran (`MainScreenController`/`AppShellFragment` `surfaceCreated`, "Start the lua app"), then the engine attempted its EGL surface and FAILED: `[FLog::SurfaceController] Mode 4 failed: Error creating context: eglCreateWindowSurface 3003` (`EGL_BAD_ALLOC`) at t=.908349, and ONLY AFTER that did Eclipse log `engine claimed the surface; Eclipse released its Vulkan renderer (present-loop handoff)` at t=.927677 — ~19 ms too late.

  *Confirmed root cause (timing + ownership, from the live boot above — first-party log observation, NOT RE):* Phase 2 dispatched `surfaceCreated` FIRST; the engine then created its EGL window surface (`eglCreateWindowSurface` over its `wl_egl_window` on the SAME `wl_surface`) while Eclipse's `VkSurfaceKHR`/`VkSwapchainKHR` STILL owned that `wl_surface` — two owners of one `wl_surface` → `EGL_BAD_ALLOC`. Eclipse dropped its renderer only on the NEXT `about_to_wait` tick (gated on `engine_claimed_surface`, which `eclipse_anativewindow_fromsurface` sets), i.e. AFTER the engine had already tried. The surface must be FREE before the engine creates its EGL surface, i.e. BEFORE Eclipse dispatches `surfaceCreated`.

  *The fix (minimal surgical REORDER — not a redesign; root-cause, not a workaround):* drop the renderer STRICTLY BEFORE dispatching `surfaceCreated`. **(A)** Factored the load-bearing `mCallbacks` read into ONE shared inner helper `surface_callbacks_size(env, surface_view) -> Result<jint>` (the `FieldSignature::from_raw_parts(ARRAY_LIST_SIG, JavaType::Object)` `get_field` then `size()I`, with the existing SAFETY comment + `checked` exception discipline) so there is a single source of truth for the field read; `surface_lifecycle`'s self-gate now calls it (`if surface_callbacks_size(env, sv)? <= 0 { return Ok(false) }`). Added `pub fn engine_surface_callback_ready(vm) -> Result<bool>` (null-guarded `JavaVM::from_raw` + `attach_current_thread` + `catch_unwind`, same discipline as `dispatch_surface_lifecycle`) delegating to a private `surface_callback_ready(env)` that does `find_by_class(RBX_SURFACE_VIEW_CLASS)` → `with_jobject` → `surface_callbacks_size > 0`; it dispatches NOTHING. `Ok(false)` when no peer / empty list / no recorded jobject; `Err` on VM/JNI error. **(B)** Reordered `graphics::about_to_wait` to a single `handed_off` gate: `if !handed_off && engine_window.is_some()`, evaluate `engine_surface_callback_ready(vm)`; on `Ok(true)`: (1) `self.renderer = None` — `VulkanRenderer::Drop` runs `device_wait_idle` → `destroy_swapchain` → `destroy_surface`, RELEASING the `wl_surface`/`VkSurfaceKHR`; (2) `dispatch_surface_lifecycle(vm, w, h)` (`(w,h)` from `engine_window_geometry().unwrap_or((1,1))`) so the engine creates its EGL window surface over the now-FREE `wl_surface`; (3) `handed_off = true` + `set_control_flow(ControlFlow::Poll)` + one drop-before-dispatch handoff info log. `Ok(false)` retries next tick (never blanked early); `Err` warns + retries. The main `Looper` keeps pumping. **(C)** Removed the now-redundant `surface_dispatched` field (the single `handed_off` gate covers both the dispatch and the drop). KEPT `set_engine_claimed_surface`/`engine_claimed_surface` and its set inside `eclipse_anativewindow_fromsurface` (the real-WSI branch) — repurposed to a one-shot confirmation log (`else if handed_off && engine_claimed_surface()`), NOT the drop trigger.

  *Why this is the correct order (not the inverse):* the engine renders into the SAME `wl_surface` once it claims Eclipse's published `ANativeWindow`. Two producers must never share one `wl_surface`. `engine_surface_callback_ready` (non-empty `mCallbacks`) confirms the engine has subscribed its `SurfaceHolder.Callback`, i.e. it is ABOUT to be told `surfaceCreated` and WILL `eglCreateWindowSurface`. Releasing Eclipse's renderer in the same tick, before the dispatch, guarantees the engine's EGL surface is the sole owner of the `wl_surface` when it is created. `ControlFlow::Poll` is set in the same tick as the drop, so there is no gap where the renderer is gone but the loop waits without a pending redraw.

  *Same-pattern audit:* after the edit, `surface_dispatched` has ZERO references across `src/`, `tests/`, `docs/` (the field, initializer, and its comments were the only users — removed cleanly). The `mCallbacks` ArrayList field read now exists in exactly ONE place (`surface_callbacks_size`), consumed by both `surface_lifecycle` and `surface_callback_ready` (verified: `get_field(.., "mCallbacks", ..)` / `FieldSignature::from_raw_parts(ARRAY_LIST_SIG, ..)` appears only inside that helper). `self.renderer = None` appears once (the handoff arm) — no other renderer-drop, no quiescent early-return that keeps the renderer alive (the prior abort's two-owners bug AND the Phase 2 dispatch-then-late-drop bug are both gone). `dispatch_surface_lifecycle` is called from exactly one production site (the reordered handoff arm). `set_engine_claimed_surface(true)` remains in exactly one production place (the real-WSI branch of `fromSurface`), now a correlation signal only. The handoff is still gated on `engine_window.is_some()` so the no-WSI / geometry-only fallback never blanks the window.

  *Regression protection (tied to the root, no new script):* `engine_surface_callback_ready` and `surface_callbacks_size` are entirely JNI/JVM-bound (`find_by_class` + `attach_current_thread` + a live JNI field/method read) with NO JVM-free seam to unit-test under `cargo` (ART aborts off the main thread), so their runtime behavior is OWNER-live-boot-validated — exactly as `dispatch_surface_lifecycle` is. Their non-JNI inputs (the const strings/values) are pinned by the existing `framework::tests::view_native_names_sigs_and_class_match_view_java` (`ARRAY_LIST_SIG == "Ljava/util/ArrayList;"`, `RBX_SURFACE_VIEW_CLASS == "com.roblox.client.RBXSurfaceView"`, `WINDOW_FORMAT_RGBA_8888 == 1`), which now covers BOTH the probe and the dispatch self-gate because both route through `surface_callbacks_size` — a transcription drift in the field descriptor / peer class / format fails CI instead of silently no-op-ing the live boot. The Phase 2 pins `view_registry::tests::find_by_class_locates_the_right_handle_and_is_none_for_absent_class` and `ndk_registry::tests::engine_claimed_surface_round_trips_set_and_get` stay green (both helpers are still used). No test was added (no new JVM-free seam) and none was removed.

  *Verification (this tree; the 3 work files — `src/framework.rs`, `src/graphics.rs`, `src/loader/ndk_registry.rs`):* `cargo fmt --all -- --check` clean; `cargo build --all-targets` 0 warnings; `cargo clippy --all-targets --all-features -- -D warnings` 0 warnings (forced recheck via `touch` of the 3 files); `cargo test` **565 passed, 0 failed (559 unit + 0 main + 4 integration `tests/engine_milestones.rs` 0 SKIP + 2 doctests)** — same count as Phase 2; `cargo build --release` clean (artifact 8,937,896 bytes, grew from Phase 2's 8,926,184 by the extracted fn + reorder + confirmation log). *No live ART boot in this workflow (off-main-thread + cyber-safeguard preclude it); RUNTIME CORRECTNESS — the engine's `eglCreateWindowSurface` SUCCEEDING (no more 3003) and THE FIRST ENGINE FRAME — is confirmed ONLY by the OWNER's dev-host MAIN-LOOP boot (§5 START-HERE). If `eglCreateWindowSurface` still errors or the window stays blank, capture the exact SurfaceController/EGL log lines + timing (log-observation only; do NOT RE the APK/libroblox).*

  *Comment hygiene:* `ndk_registry.rs`'s `ENGINE_CLAIMED_SURFACE` / `engine_claimed_surface()` doc comments (which described it as the renderer-drop trigger) were updated to the Phase 2.1 confirmation-only role per the comment-dating + stale-comment policy (dated 2026-06-13).

  *Context7:* not used — this change touches no external API surface or version-sensitive behavior; it reorders two existing internal calls and extracts one function. The `winit` `ApplicationHandler`/`ControlFlow::Poll` usage and the `jni`-0.22.4 `FieldSignature::from_raw_parts`/`call_method`/`get_field` patterns are unchanged from the already-green Phase 2 code (the project's own source is the authority for the project-specific `Env`/`Global` API).

  *Files:* `src/framework.rs` (`surface_callbacks_size` shared helper + `engine_surface_callback_ready`/`surface_callback_ready` + rewired `surface_lifecycle` self-gate), `src/graphics.rs` (removed `surface_dispatched`; single `handed_off` drop-before-dispatch gate + confirmation-only `engine_claimed_surface` log), `src/loader/ndk_registry.rs` (Phase 2.1 doc-comment hygiene on the claim signal), `AGENTS.md`.

---

### 2026-06-13 — 🖼️ Render Phase 3: EGL DISPLAY CONNECTION-MATCH — tier-0 `eglGetDisplay` remaps `EGL_DEFAULT_DISPLAY` to Eclipse's winit `wl_display`, fixing the engine's `eglCreateWindowSurface` `EGL_BAD_ALLOC` (3003)

  *Confirmed root cause (evidence, not speculation):* Phase 2.1 (commit `6a75944`) freed the `wl_surface` before dispatch (dropped Eclipse's `VulkanRenderer` strictly first), so the engine's EGL CONTEXT now creates successfully — `eglCreateContext` SUCCEEDS — but `eglCreateWindowSurface` STILL failed `[FLog::SurfaceController] Mode 4 failed: Error creating context: eglCreateWindowSurface 3003` (`EGL_BAD_ALLOC`). This is an INDEPENDENT cause from Phase 2.1's two-owners-of-one-`wl_surface`: the engine resolves its `egl*` symbols through `bionic_env` tier 1 (host `libEGL.so`, opened at `bionic_env.rs:516-524`) because Eclipse's tier-0 `EclipseNativeProvider` previously registered ZERO `egl*`/`gl*` names (verified by grep of `p.register("egl`/`gl` in `native_provider.rs` — empty; only the 6 ANativeWindow + bionic/libc natives existed). So the engine's `eglGetDisplay(EGL_DEFAULT_DISPLAY=0=NULL)` ran in HOST Mesa, which per the Khronos `EGL_KHR_platform_wayland` / `EGL_EXT_platform_wayland` registry text (Context7, verified 2026-06-13: "When `EGL_DEFAULT_DISPLAY` is used, EGL connects to the default Wayland socket, similar to `wl_display_connect(3)`" / "If `EGL_DEFAULT_DISPLAY` is used, EGL creates a new `wl_display` structure by connecting to the default Wayland socket") opens Mesa's OWN `wl_display` via `wl_display_connect(NULL)` — a DIFFERENT connection object than the one `winit` opened for Eclipse's window. The `ANativeWindow*` Eclipse hands the engine (`ndk_registry::current_wsi_window` via `eclipse_anativewindow_fromsurface`) wraps a `wl_egl_window*` on `winit`'s `wl_surface`; `eglCreateWindowSurface` requires the EGLDisplay's `wl_display` and the `wl_egl_window`'s `wl_surface` to be on the SAME connection — crossing connections is `EGL_BAD_ALLOC` 3003 (matches the live log EXACTLY: `eglCreateContext` succeeds — display/config valid — but `eglCreateWindowSurface` 3003). Eclipse's own `__gl-test-anw` AVOIDS this because `egl_engine.rs:251-263` builds its EGLDisplay from the `winit` `RawDisplayHandle::Wayland` `wl_display` (`d.display.as_ptr()`) — the SAME connection as its `wl_egl_window`. The constant `EGL_DEFAULT_DISPLAY == 0 == NULL` is verified in vendored `khronos-egl-6.0.0/src/lib.rs` (`DEFAULT_DISPLAY` line 1486; `NativeDisplayType`/`EGLDisplay = *mut c_void`, lines 236/232).

  *Fix (connection-MATCHING, not a workaround — 3 surgical edits mirroring the existing WSI-window plumbing):* register an Eclipse-OWNED `eglGetDisplay` at loader tier 0, which wins over host `libEGL` by `resolve.rs`'s first-strong-match (`resolve.rs:240-241`), and map `EGL_DEFAULT_DISPLAY` to the registered winit `wl_display` before delegating to the HOST `eglGetDisplay` so the engine's EGLDisplay shares the `wl_egl_window`'s connection. **(A)** `src/loader/ndk_registry.rs` (`#![forbid(unsafe_code)]` — store the pointer VALUE as `usize`, exactly like `WSI_WINDOWS`): `static WSI_DISPLAY: Mutex<Option<usize>>` + `set_wsi_display(Option<usize>)` (best-effort, poison→ignore, never panics per §2.8) + `wsi_display() -> Option<usize>` (poisoned→`None`), next to `register_wsi_window`/`current_wsi_window`; dated doc comment: it is the winit `wl_display` connection the engine's EGLDisplay must match (`None` on X11/other, where the XID is server-scoped so cross-connection is fine). **(B)** `src/loader/native_provider.rs`: a pure JVM-free helper `resolve_egl_display_target(display_id, wsi) -> usize` (`display_id==0` + Wayland → winit `wl_display`; else pass-through — written `wsi.unwrap_or(0)` to satisfy `clippy::manual_unwrap_or_default`); a cached `host_egl_get_display()` (`OnceLock<Option<usize>>` doing its OWN `dlopen("libEGL.so", RTLD_NOW|RTLD_LOCAL)` + `dlsym("eglGetDisplay")`, `RTLD_LOCAL` + process-lifetime, never `dlclose`, mirroring `DlopenLibProvider` — `None` on null handle/sym → `EGL_NO_DISPLAY`, a clean failure, never UB; using Eclipse's own handle keeps the lookup out of the engine's symbol scope so the shim NEVER re-enters the engine's relocated `eglGetDisplay` — no recursion); and the tier-0 native `unsafe extern "C" fn eclipse_egl_get_display(display_id: *mut c_void) -> *mut c_void` delegating to the host fn with the remapped target (does NOT dereference `display_id`); registered `p.register("eglGetDisplay", …)` in `with_bionic_natives` before the ANativeWindow block. Dated 2026-06-13 SAFETY comments on both `unsafe` blocks (the `dlopen`/`dlsym` FFI; the host-fn transmute+call). Used `c"…"` C-string literals (stable since Rust 1.77) to avoid a `CString` import. **(C)** `src/graphics.rs` (`HasDisplayHandle` already imported; added `RawDisplayHandle` to the `raw_window_handle` import): in `GameWindow::resumed`, right after the Phase 1 WSI-publish block (before `self.window = Some(window)`), match `window.display_handle().as_raw()` → `RawDisplayHandle::Wayland(d)` → `set_wsi_display(Some(d.display.as_ptr() as usize))`, else/`Err` → `set_wsi_display(None)`. The `wl_display` pointer is the SAME one `egl_engine.rs:252` uses, so the engine's remapped EGLDisplay lands on winit's connection — identical to `__gl-test-anw`. Non-fatal, dated 2026-06-13 comment matching the adjacent Phase 1 pattern.

  *Same-pattern audit:* grep of `eglGetDisplay`/`eglGetPlatformDisplay`/`register("egl`/`register("gl` across `src/` confirms: (1) BEFORE this change tier-0 `EclipseNativeProvider` registered ZERO `egl*`/`gl*` names (the engine's `egl*` resolved only through `bionic_env` tier 1 host `libEGL.so`) — so this is the single, correct interception point; (2) `egl_engine.rs` calls host `eglGetDisplay` DIRECTLY via the `khronos-egl` `EglInstance` built from the winit `RawDisplayHandle::Wayland` `wl_display` (`egl_engine.rs:251-263`), NOT through the engine's resolved symbol scope — so `__gl-test`/`__gl-test-anw` (which build their EGLDisplay from that same `wl_display`, already matching their `wl_egl_window`) are UNAFFECTED by the new tier-0 native (their integration tests stay green); (3) NO `eglGetPlatformDisplay`/`eglGetPlatformDisplayEXT` is registered anywhere — consistent with the CONDITIONAL classification below (live-probe-gated). The connection-mismatch is the same CLASS of bug as Phase 2.1's two-owners-of-one-`wl_surface` (both `EGL_BAD_ALLOC` 3003) — Phase 2.1 fixed the `VkSurfaceKHR` co-ownership, this fixes the independent `wl_display` cross-connection; no other instance of cross-connection EGL display acquisition exists in Eclipse-owned code.

  *Platform-display follow-up (deferred, live-probe-gated — Simplicity First):* `eglGetPlatformDisplay` / `eglGetPlatformDisplayEXT` (EGL 1.5 / `EGL_EXT_platform_base`) are NOT intercepted. The engine's actual display-acquisition symbol cannot be pinned without RE of `libroblox` (out of scope; Eclipse's first-party docs enumerate only `eglGetError` among the 91 EGL/GLES imports — the display-acquisition symbol name is not in Eclipse's own sources). The live-log evidence (`eglCreateContext` succeeds, only `eglCreateWindowSurface` fails 3003) is consistent with the engine using plain `eglGetDisplay`. Add the platform-display interceptions ONLY if the OWNER live-probe shows the engine took the platform-display path (`eclipse_egl_get_display` never fired, OR 3003 persists with `eglGetDisplay` intercepted): same `EGL_DEFAULT_DISPLAY`→winit `wl_display` mapping on the `(platform, native_display, attribs)` signature, remap `native_display==NULL(0)` when `platform == EGL_PLATFORM_WAYLAND_KHR/EXT (0x31D8)` and `wsi_display()` is `Some`, else delegate unchanged.

  *Regression protection (tied to the root, no new script):* `native_provider::tests::resolve_egl_display_target_maps_default_display_to_winit_wayland_only` is the confirmed-root-cause guard — the pure JVM-free decision: (a) `display_id=0 + wsi=Some(0x5000_1000)` → `0x5000_1000` (the EXACT bug: `EGL_DEFAULT_DISPLAY` on Wayland remaps to the registered winit `wl_display`); (b) `display_id=0 + wsi=None` → `0` (X11/no-Wayland pass-through, preserves X11/NVIDIA); (c) `display_id=0xABCD + wsi=Some(…)` → `0xABCD` (a non-default display is never rewritten); (d) `display_id=0xABCD + wsi=None` → `0xABCD`. `ndk_registry::tests::wsi_display_round_trips_set_and_get` pins the registry round-trip (serialized under the existing `WSI_TEST_LOCK`, RESTORED to `None` at the end so it does not leak into other tests, mirroring `engine_claimed_surface_round_trips_set_and_get`). `native_provider::tests::with_bionic_natives_registers_the_three_implemented_categories` was updated (count 129→130 + descriptive comment) and `"eglGetDisplay"` added to its name-presence list, so a dropped registration or transcription drift fails CI. The host-EGL `dlopen`/`dlsym` delegation + the live tier-0 win over host `libEGL` have NO JVM-free seam (FFI to host Mesa libEGL), so they are OWNER-dev-host-boot validated (same posture as the other render natives); the unit tests pin the decision logic + the registry round-trip.

  *Verification (this tree; the 3 work files — `src/graphics.rs`, `src/loader/native_provider.rs`, `src/loader/ndk_registry.rs`):* `cargo fmt --all -- --check` clean; `cargo build --all-targets` 0 warnings; `cargo clippy --all-targets --all-features -- -D warnings` 0 warnings (forced recheck via `touch` of the 3 files); `cargo test` **567 passed, 0 failed (561 unit + 0 main + 4 integration `tests/engine_milestones.rs` 0 SKIP + 2 doctests)** — +2 vs Phase 2.1's 565 (the two new unit tests); `cargo build --release` clean (artifact 8,939,368 bytes, grew from Phase 2.1's 8,937,896 by the EGL interception). *No live ART boot in this workflow (off-main-thread + cyber-safeguard preclude it); RUNTIME CORRECTNESS — the engine's `eglCreateWindowSurface` SUCCEEDING (no more 3003) and THE FIRST ENGINE FRAME — is confirmed ONLY by the OWNER's dev-host MAIN-LOOP boot (§5 START-HERE; record the laptop log path e.g. `/tmp/eclipse-egldisplay-validate.log`). If `eglCreateWindowSurface` still errors, capture the exact SurfaceController/EGL log lines + whether the engine used `eglGetDisplay` vs `eglGetPlatformDisplay` (log-observation only; do NOT RE the APK/libroblox).*

  *Context7:* Khronos EGL Registry consulted (verified 2026-06-13) — `EGL_KHR_platform_wayland` / `EGL_EXT_platform_wayland` confirm the `EGL_DEFAULT_DISPLAY` → `wl_display_connect(NULL)` (own default-socket connection) semantics that ARE the cross-connection root cause; `khronos-egl-6.0.0` vendored source confirms `DEFAULT_DISPLAY=0` and `EGLDisplay`/`NativeDisplayType = *mut c_void` for the ABI of the tier-0 native and the host-fn transmute.

  *Files:* `src/loader/ndk_registry.rs` (`WSI_DISPLAY` + `set_wsi_display`/`wsi_display` + round-trip test), `src/loader/native_provider.rs` (`resolve_egl_display_target` + `host_egl_get_display` + `eclipse_egl_get_display` + registration + count/name test updates + mapping test), `src/graphics.rs` (`RawDisplayHandle` import + `set_wsi_display` from `resumed`), `AGENTS.md`.

---

### 2026-06-13 — 🖼️ Render Phase 4: BUNDLED-ASSET PROVISIONING — extract the APK's `assets/` tree to the engine content root (`app-data/files/assets`) so the engine can open its shader packs from the filesystem (the missing pack was `RenderView is NULL`)

  *Confirmed root cause (evidence, not speculation — owner live boot, commit `c5681bc`, EXIT=124):* Render Phase 3 fixed the EGL connection-match — the engine now creates its EGL CONTEXT + 800×600 window surface successfully (`[FLog::Graphics]` "Initialized EGL context … with renderbuffer 800x600", `eglSwapInterval(1)`, GL extensions + framebuffer caps enumerated; NO more `eglCreateWindowSurface` 3003). The NEXT render blocker then surfaced: `[FLog::SurfaceController] Mode 4 failed: Error opening shader pack glsles3 (<app_data_dir>/files/assets/content/../shaders/shaders_glsles3.pack)` followed by `RenderView is NULL` → no frames. The engine reads its shader packs (and fonts/content) from the FILESYSTEM under its content root `<app_data_dir>/files/assets/shaders/shaders_glsles3.pack` (the logged `content/../shaders/` normalises to `files/assets/shaders/`) — NOT through the JNI `AssetManager`. But Eclipse extracted ONLY `lib/x86_64/*.so` from the APK (`extract_native_libs`) and never the `assets/` tree, so `files/assets/` held only empty `android/content/ExtraContent` dirs and no `shaders/` → the shader-pack open fails → `RenderView` NULL. The APK bundles `assets/shaders/shaders_glsles3.pack` (~9.6 MB) + `shaders_vulkan_mobile.pack` (~20 MB); the full `assets/` tree is ~105 MB (shaders/ ExtraContent/ content/ android/ fonts/ ssl/ shared_compression_dictionaries/ com/ + PublicSuffixDatabase.list, dexopt/).

  *Fix (provision the bundled assets on disk — root-cause, not a workaround; 3 surgical edits mirroring `extract_native_libs`):* materialise the APK's `assets/` tree at the engine content root before boot. **(A)** `src/apk/mod.rs`: `pub fn extract_assets(&mut self, dest_dir: &Path) -> Result<usize, ApkError>` — the SAME two-phase borrow as `extract_native_libs` (collect entry names under the `assets/` prefix excluding directory entries via the immutable `file_names()`, then stream each through `by_name` with the mutable borrow — constant memory), strips the leading `assets/` component so `assets/shaders/x.pack` → `dest_dir/shaders/x.pack`, creates parent dirs (the assets/ tree is nested, unlike the flat `lib/<abi>/` layout), idempotent size-skip (a dest already at the entry's uncompressed size is left untouched — repeat boots don't rewrite ~105 MB), and atomic temp(`.partial`)+fsync+rename writes (a kill mid-copy leaves only a `.partial`, never a same-size-but-corrupt dest the skip would accept). Adds `zip` 2.x `enclosed_name()` path-traversal safety (rejects NUL bytes / `..` traversal / absolute names — the recommended safe-extraction check; the nested tree needs it where the flat lib extractor flattened to basename and did not). Returns the count written this call; typed `ApkError`, never panics (matters under release `panic = "abort"`). **(B)** `src/framework.rs`: raised `fn app_data_dir()` → `pub fn app_data_dir()` with a dated 2026-06-13 doc note, so `main.rs` derives the extraction dest from the SAME source of truth `native_get_app_data_dir` returns — the extraction path can never drift from what the engine reads (`ECLIPSE_APP_DATA_DIR` override else `directories` `ProjectDirs` `eclipse` data_dir + `app-data`). Visibility-only change; `native_get_app_data_dir` still calls it unchanged. **(C)** `src/main.rs::run_apk`: after the `extract_native_libs` block, `assets_dir = framework::app_data_dir()/files/assets` (an actionable `io::Error` when no XDG/home base resolves and `ECLIPSE_APP_DATA_DIR` is unset — never a silent skip), prints `# Extracting Roblox bundled assets (assets/ → files/assets/) to <…>…` then `apk.extract_assets(&assets_dir)?` (FATAL via `?` — a missing shader pack means no rendering) and `extracted <n> asset file(s)`.

  *Same-pattern audit:* grepped `src/` for `extract_native_libs` / `extract_assets` / `files/assets` / `"assets"` usages. The only on-disk asset/lib extraction path is the APK→filesystem flow in `main.rs::run_apk`; `extract_native_libs` (`lib/<abi>/*.so`, flat) was the model and `extract_assets` is the new sibling for the nested `assets/` tree — no other code materialises APK assets to disk. The JNI `AssetManager` path (`framework.rs` `read_asset_bytes`) serves assets straight from the APK zip and is a DIFFERENT, unaffected mechanism (the engine's shader pack is read by the engine's OWN C++ file IO from the filesystem — exactly why on-disk extraction is required). `extract_assets` mirrors `extract_native_libs`'s idempotency + atomic temp+rename so the same truncated-mid-copy class of bug cannot recur, and adds `enclosed_name()` path-traversal safety the nested tree needs. No equivalent flawed instance elsewhere.

  *Regression protection (tied to the root, no new script):* `apk::tests::extract_assets_strips_prefix_preserves_subpaths_skips_non_assets_and_is_idempotent` (mirrors the model `extract_native_libs_extracts_matching_abi_only_and_is_idempotent`): builds an in-memory APK with `assets/shaders/shaders_glsles3.pack` + `assets/baz.txt` + a non-asset `lib/x86_64/libroblox.so` + `classes.dex`, extracts to a per-thread tempdir (portable, no hardcoded paths), asserts count==2; the nested asset lands at `dest/shaders/shaders_glsles3.pack` with correct bytes (prefix stripped, sub-path preserved); `baz.txt` at the dest root; the non-asset entries are NOT extracted; and a second extract returns 0 (idempotent — proves repeat boots don't rewrite the ~105 MB tree). A drift that broke prefix-stripping, leaked a non-asset, or lost idempotency fails CI. The shader-pack open + `RenderView` non-NULL + the first engine frame are OWNER-live-boot-validated (ART cannot run under `cargo test`).

  *Verification (this tree; the 3 work files — `src/apk/mod.rs`, `src/framework.rs`, `src/main.rs`):* `cargo fmt --all -- --check` clean; `cargo build --all-targets` 0 warnings (forced recheck via `touch` of the 3 files); `cargo clippy --all-targets --all-features -- -D warnings` 0 warnings; `cargo test` **568 passed, 0 failed (562 unit (+1: the new `extract_assets` test) + 0 main + 4 integration `tests/engine_milestones.rs` 0 SKIP + 2 doctests)** — +1 vs Phase 3's 567; `cargo build --release` clean (artifact 8,945,096 bytes, grew from Phase 3's 8,939,368 by the asset-extraction wiring). *No live ART boot in this workflow (off-main-thread + cyber-safeguard preclude it); RUNTIME CORRECTNESS — the engine's shader-pack open SUCCEEDING (no more `[FLog::SurfaceController] Mode 4 failed: Error opening shader pack`), `RenderView` non-NULL, and THE FIRST ENGINE FRAME — is confirmed ONLY by the OWNER's dev-host MAIN-LOOP boot (§5 START-HERE; record the laptop log path e.g. `/tmp/eclipse-assets-extract-validate.log`). The first boot extracts ~105 MB (a few seconds; idempotent after). If `RenderView` is still NULL or a different asset/shader error appears, capture the exact SurfaceController/Graphics line + confirm the extracted file's path & size (log-observation only; do NOT RE the APK/libroblox). The CDN 401/403 asset errors are login-gated, separate, and do NOT block the bundled shader/UI render.*

  *Context7:* `zip` 2.x (`/zip-rs/zip2`) consulted (verified 2026-06-13) — confirmed `enclosed_name()` is the recommended path-exploit-resistant safe-extraction API (rejects NUL bytes / `..` traversal / absolute paths) and is NOT deprecated (the deprecated method is `sanitized_name`); trailing-`/` detects directory entries. The project's own `extract_native_libs` (+ its idempotent/atomic test) is the authoritative model for the two-phase borrow, size-skip idempotency, and temp+fsync+rename atomicity.

  *Files:* `src/apk/mod.rs` (`extract_assets` + its regression test), `src/framework.rs` (`app_data_dir` raised to `pub` with a dated note — single source of truth for the content root), `src/main.rs` (`run_apk` asset-extraction step after `extract_native_libs`), `AGENTS.md`.

### 2026-06-13 — 🖼️ Render Phase 5: GUEST DEVICE API LEVEL — propagate `-DBuild.VERSION.SDK_INT` to ART so the engine stops misreading API 23 (un-gates Vulkan); corrects Phase 4 (the engine reads shaders from the APK, not the FS)

  *Confirmed root cause (multi-agent first-party forensics + orchestrator `strace`/`LD_PRELOAD`/magic-flip probes; owner live boots):* after Phase 3/4 the engine reached render init but `RenderView is NULL` — no graphics mode came up. The engine tries modes in order: **Mode 6 (Vulkan)** then **Mode 4 (GLES3)**. Mode 6 failed `Android version is too old to activate Vulkan` and Mode 4 failed `Error opening shader pack glsles3`. The probe campaign established: (1) the engine logs `[FLog::Graphics] Android API 23`, but the manifest `targetSdk=35` and Eclipse's `BootPlan.sdk_int=35`; (2) `strace` proved the engine reads `shaders_glsles3.pack` **directly from the APK** (`openat` of the .apk + `lseek` to the STORED entry's local-header 67686916 / data 67686984) — the entry is CRC-valid (`unzip -t` OK, central==local==computed `0x4f49dbf7`), the bytes are valid `RBXS`; (3) the engine NEVER `openat`s the Phase-4 extracted FS tree, and corrupting the extracted FS copy's magic was byte-identical → **Phase 4 (FS extraction) was the wrong layer; it does not feed the shader read** (this corrects the Phase 4 §6 entry above + §5). The load-bearing API channel is JNI `android.os.Build$VERSION.SDK_INT`: ATL's `vendor/atl/src/api-impl/android/os/Build.java:111` does `SDK_INT = (System.getProperty("Build.VERSION.SDK_INT") != null) ? parseInt : 23` — it falls back to **23** when the property is unset, and `runtime.rs::BootPlan::vm_options()` was heap-only (it never passed that `-D`), so `sdk_int=35` never reached ATL. API 23 < 24 ⇒ the engine hard-rejects Vulkan and drops to the GLES3 path. (Serving `ro.build.version.sdk` via bionic `__system_property_get` was the WRONG channel — proven inert by an inline probe.)

  *Fix (1 line — root-cause, smallest change):* `src/runtime.rs::vm_options()` now pushes `format!("-DBuild.VERSION.SDK_INT={}", self.sdk_int.min(28))` (capacity bumped 3→4). **The `.min(28)` clamp is load-bearing and MUST stay < 29:** ATL's `Activity.registerActivityLifecycleCallbacks` (`vendor/atl/src/api-impl/android/app/Activity.java:614`) is an empty no-op `{}`, so at `SDK_INT >= 29` androidx `ReportFragment.injectIfNeededIn` switches from the pre-API-29 `android.app.Fragment.onActivityCreated` path (which the 2026-06-13 lifecycle-ordering overlay services) to `registerActivityLifecycleCallbacks`/`onActivityPostCreated` → the callback is dropped → `ON_CREATE` never dispatches → the `IllegalStateException` boot blocker (§6 androidx lifecycle-ordering entry) returns BEFORE render init. 28 (Android 9) clears the Vulkan API-24 gate while staying below the androidx-29 switch. ATL sets `RESOURCES_SDK_INT` to the same value when its own property is unset (no mismatch crash), so one option suffices.

  *Same-pattern audit:* `vm_options()` is the single Java-system-property choke point (grep of `src/`+`tools/` for `setProperty`/`-DBuild`/`SDK_INT` found only this site and ATL's reader). The NDK device-API path is not load-bearing (`AConfiguration_getSdkVersion`/`android_get_device_api_level` are not imported strong — the engine loaded — and the JNI value is what it printed). VRAM `67108864` (64 MiB) and `renderbuffer 800x600` are engine-side values (the engine queries the desktop `GL_NVX_gpu_memory_info` extension the host GLES context lacks and falls back to its own floor; 800×600 is Eclipse's window size) — NOT Eclipse-settable and NOT touched.

  *Regression protection (tied to the root, no new script):* `runtime.rs::tests::vm_options_propagate_clamped_sdk_int` — asserts a `targetSdk=35` plan yields `-DBuild.VERSION.SDK_INT=28`, never `=23` (the bug) or `=35` (the API-29 regression), and that a sub-28 target (`Some(21)`) is propagated verbatim (we cap, not floor). A drift that drops the property, un-clamps to ≥29, or hardcodes 23 fails CI.

  *Verification (this tree):* `cargo fmt --all` clean; `cargo build --all-targets` 0 warn; `cargo clippy --all-targets --all-features -- -D warnings` 0 warn; `cargo test` **563 unit + 4 integration (0 SKIP) + 2 doctests, 0 failed**; `cargo build --release` clean. OWNER LIVE BOOT (`/tmp/eclipse-sdk28.log`, EXIT=124): **(A) ✅** `Android API 28` (was 23) — the engine now ATTEMPTS Vulkan (loads `VK_KHR_surface` + `VK_KHR_android_surface`); **(B) ✅** NO `IllegalStateException`, `ActivityNativeMain` reaches `onResume`/RESUMED (clamp-28 preserved the lifecycle); **(C) ⏳** still no frames.

  *Two next gates revealed (either unblocks render — pick next session):* **(a) Vulkan (reference path; Sober uses it):** `Mode 6 failed: Unable to create Vulkan instance` because the engine requests the Android-only `VK_KHR_android_surface` instance extension, absent from the host Linux Vulkan ICD. Eclipse needs a Vulkan-surface translation seam (parallel to the EGL connection-match): intercept `vkCreateInstance` to swap `VK_KHR_android_surface`→`VK_KHR_wayland_surface` (+ `vkEnumerateInstanceExtensionProperties` filtering), and `vkCreateAndroidSurfaceKHR`→`vkCreateWaylandSurfaceKHR` on winit's `wl_display`+`wl_surface`. **(b) GLES3 (EGL already wired by Phase 3):** `Error opening shader pack glsles3` is a POST-READ rejection of valid bytes; forensics' medium-confidence hypothesis is the empty `ro.product.*`/`ro.build.*` store collapsing the engine into a bogus "HTC unknown" low-end profile (`Excluded 'HTC unknown:…RTX 5070' - disabling SuperHQ shaders`, `GLES MT shader loading is disabled`, 64 MiB VRAM floor) that gates the pack. NEXT PROBE: populate sane `ro.product.*`/`ro.build.*` in `native_provider.rs::eclipse_system_property_get` (currently empty for every key) and re-boot (confirm-by-fix).

  *Files:* `src/runtime.rs` (`vm_options()` SDK_INT push + `vm_options_propagate_clamped_sdk_int` test), `AGENTS.md`. Forensics: workflow `eclipse-shader-render-forensics`.

---

### 2026-06-13 — 🖼️ Render Phase 6: VULKAN WSI TRANSLATION (Android→Wayland) — a tier-0 `dlsym` interposer routes the engine's runtime-dlopen'd libvulkan lookups to Eclipse's `vkCreateInstance` (ext-swap) + `vkCreateAndroidSurfaceKHR`→wayland shims; Mode-6 "Unable to create Vulkan instance" FIXED

  *Confirmed root cause (orchestrator live boots + syscall/`dlsym` diagnostics):* with API 28 (Phase 5) the engine attempts Vulkan Mode 6 and requests instance extensions `VK_KHR_surface` + `VK_KHR_get_physical_device_properties2` + `VK_KHR_android_surface`; `vkCreateInstance` returns `VK_ERROR_EXTENSION_NOT_PRESENT` (`Mode 6 failed: Unable to create Vulkan instance`) because the host Linux Vulkan ICD has `VK_KHR_wayland_surface`, not the Android-only `VK_KHR_android_surface`. CRITICAL MECHANISM (this settles the Phase-5 open question of how the engine resolves `vk*`): the engine **`dlopen`s `/usr/lib/libvulkan.so.1` at RUNTIME and `dlsym`s the loader commands by name** — `vk*` are NOT `DT_NEEDED`/UND imports, so Eclipse's tier-0 `vk*` natives are NEVER consulted for the engine (PROVEN: a first registration of tier-0 `vkCreateInstance`/`vkGetInstanceProcAddr`/`vkCreateAndroidSurfaceKHR` got ZERO shim hits — the engine bypassed them entirely, unlike `eglGetDisplay` which IS a UND import). `dlsym` and `dlopen`, however, ARE UND imports the engine resolves through Eclipse's scope.

  *Fix (the interception point is `dlsym`, not the `vk*` symbols):* a new `src/loader/vulkan_wsi.rs` + a tier-0 `dlsym` registration. `eclipse_dlsym` (registered at tier 0 in `native_provider`, winning over the host `dlsym` by `resolve`'s first-strong-match) returns Eclipse's shims for exactly the three Vulkan-loader entry points the engine looks up by name — `vkGetInstanceProcAddr` (load-bearing: the engine reaches `vkCreateInstance` + everything else THROUGH it), `vkCreateInstance`, `vkCreateAndroidSurfaceKHR` — and forwards every other symbol UNCHANGED to the host `dlsym` (a faithful pass-through). The shims (all `extern "system"`, ash 0.38 `vk::` types): **`eclipse_vk_get_instance_proc_addr`** returns the two create-shims by name, else forwards to the host `vkGetInstanceProcAddr` (`ash::Entry::load().static_fn()`); **`eclipse_vk_create_instance`** copies the requested extension list swapping `VK_KHR_android_surface`→`VK_KHR_wayland_surface` (order + pNext + app_info + layers + flags preserved), then forwards to the host `vkCreateInstance` (`entry.fp_v1_0().create_instance`); **`eclipse_vk_create_android_surface_khr`** builds a `VkWaylandSurfaceCreateInfoKHR` from Eclipse's winit `wl_display` (`ndk_registry::wsi_display`) + `wl_surface` (`ndk_registry::wsi_wl_surface`, newly published in `graphics.rs::resumed` from `RawWindowHandle::Wayland`) and forwards to the host `vkCreateWaylandSurfaceKHR` (resolved via the host `vkGetInstanceProcAddr` on the instance) — the engine's `ANativeWindow`-bound create-info is ignored since Eclipse owns the real WSI handles; returns `VK_ERROR_INITIALIZATION_FAILED` when the host loader or WSI handles are absent (clean failure, never UB). The three vk* tier-0 native registrations from the first cut are KEPT (harmless — they would serve any future lib that DID UND-import vk*).

  *Same-pattern audit:* the only Android-WSI extension the engine requests that the host lacks is `VK_KHR_android_surface` (→ `vkCreateAndroidSurfaceKHR`); the EGL path's analogous Android→Wayland mismatch was already handled by the Phase-3 `eglGetDisplay` connection-match. No other Android-only WSI symbol is in the engine's Vulkan path (it resolves the rest — swapchain/device/queue — through the host unchanged via the forwarding `vkGetInstanceProcAddr`). `dlsym` interposition is scoped to the three names; all else is verbatim host `dlsym`.

  *Regression protection:* `loader::vulkan_wsi::tests::swap_android_for_wayland_surface_replaces_only_android_and_preserves_order` + `..._is_identity_without_android` (pure extension-rewrite core — the one part testable without a live ICD); `ndk_registry` `wsi_wl_surface` round-trip; the `native_provider` registration-count test updated to 134 base (+1 `dlsym`). The live forwarding path (host `vkCreateInstance`/`vkCreateWaylandSurfaceKHR`) is inherently a dev-host live-boot concern (needs a real ICD; cannot run under `cargo test`) and is owner-validated below.

  *Verification (this tree):* `cargo fmt`/`build --all-targets` 0-warn/`clippy -D warnings` 0-warn/`cargo test` **566 unit + 4 integ (0 SKIP) + 2 doctest, 0 failed** (+3 vs Phase 5)/`cargo build --release` clean. OWNER LIVE BOOT (`/tmp/eclipse-vkdlsym.log`, EXIT=124): the engine's `dlsym("vkGetInstanceProcAddr")` is intercepted, `eclipse_vk_create_instance` runs with the host loader present, the engine then resolves hundreds of `vk*` through Eclipse's `vkGetInstanceProcAddr` and **progresses past Vulkan instance + surface + device creation to `RenderView created[1]`** — `Mode 6` no longer fails on instance creation.

  *New single common render blocker (next session):* Mode 6 now fails `Error opening shader pack vulkan_mobile` and Mode 4 fails `Error opening shader pack glsles3` → `RenderView is NULL`. The shader-pack open is the COMMON final blocker for BOTH render paths. The engine reads the pack DIRECTLY from the APK (CRC-valid STORED `RBXS` bytes; ignores the Phase-4 FS tree — corrupting the FS copy is byte-identical) and rejects it POST-READ. Being common to both packs/APIs, it is in the engine's COMMON pack open/read/parse (not an API/GL/Vulkan step). NEXT: granular attach-late `strace` (read/pread/lseek) of the `vulkan_mobile` pack read to see how far the read gets + the rejection point; weigh provisioning (merged-APK engine/shader version skew — same-version + intact CRC argues against) vs a common decompress/parse step.

  *Files:* `src/loader/vulkan_wsi.rs` (NEW — shims + `eclipse_dlsym` interposer + tests), `src/loader.rs` (`pub mod vulkan_wsi`), `src/loader/ndk_registry.rs` (`set_wsi_wl_surface`/`wsi_wl_surface` + test), `src/loader/native_provider.rs` (tier-0 `dlsym` + 3 `vk*` registrations, count test), `src/graphics.rs` (publish `wl_surface` in `resumed`), `AGENTS.md`. Implement+review: workflow `eclipse-vulkan-wsi-translation`.

---

## 7. Doc index

| File | Purpose |
|---|---|
| `CLAUDE.md` | Global engineering policy (authoritative; always followed). |
| `README.md` | Project front door. |
| `docs/sober-research.md` | How Sober/ATL works (full technical writeup). |
| `docs/component-map.md` | **Authoritative** component matrix (the code mirrors it). |
| `docs/tech-selection.md` | Library selection rationale. |
| `docs/art-and-runtime.md` | Vendored ART/runtime: build, performance, stability. |
| `docs/dependency-plan.md` | What each subsystem will depend on. |
| `docs/m0-runbook.md` | The next step: validate the foundation. |
| `docs/bionic-loader-plan.md` | Build-ready bionic NDK-soname-shim spec (DEFERRED, main-loop). |
| `docs/bionic-loader-strategy.md` | Bionic-loader v1 strategy: the modern-relocation wall + chosen path. |
| `docs/dev-host-runbook.md` | Dev-host execution steps the cargo-test harness can't run. |
| `docs/project-state-2026-06-05.md` | Session capstone: verified demo lifecycle+render; full state + engine-load roadmap. |
| `docs/libroblox-characterization.md` | REAL x86-64 `libroblox.so` + APK native-lib intel (DT_NEEDED, reloc histogram, APS2 gap, bionic-env surface). |
| `tools/framework-overlay/README.md` | In-repo patched-ATL-framework overlay builder (Build/NetworkRequest/ActivityManager/PowerManager; multidex first-dex-wins). |

---

## 8. Version control & commits

- **Remote:** <https://github.com/Kuenec/Eclipse> — push here.
- **Identity:** commit & push as **Kuenec** `<Kuenec44@gmail.com>` (set in repo git config).
- **NEVER co-author commits.** Do **not** add a `Co-Authored-By` trailer or any second
  author. (This explicitly overrides any default co-author behavior.)
- **Commit messages:** short and concise — state *what was done*, nothing more. Imperative,
  one line where it fits.
- **Never commit:** Roblox APKs or vendored ART/ATL artifacts (`.gitignore` guards
  `/vendor`, `/build`, `*.apk`).
- **Push policy (owner-authorized 2026-06-05):** commit to `main` **and** push to
  `origin/main` after each green-gate commit. **Never force-push. Never push a red or
  un-gated tree** (the §4 quality gate must be clean first). Still confirm before any
  destructive/history-rewriting action (force-push, rebase of shared history, etc.).
