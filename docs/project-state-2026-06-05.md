# Eclipse — Project State Capstone (2026-06-05)

> **Purpose.** A single, faithful, dated consolidation of where Eclipse stands after the long
> autonomous session that drove it from a bare ART boot to a complete, faithfully-rendering
> Android app lifecycle. This is the pickup point for the next session.
>
> **Authority.** This doc summarizes; it does not override. `CLAUDE.md` (global engineering
> policy) and `AGENTS.md` (project charter + §5 Living State + §6 Decisions Log) remain the
> sources of truth. Cross-references: [`docs/bionic-loader-plan.md`](bionic-loader-plan.md),
> [`docs/bionic-loader-strategy.md`](bionic-loader-strategy.md),
> [`docs/art-and-runtime.md`](art-and-runtime.md),
> [`docs/dev-host-runbook.md`](dev-host-runbook.md).
>
> **Faithful-reporting note.** Section (a) "demo" is **re-verified at HEAD `16cb2e2` on
> 2026-06-05** by this capstone (gate + a live `eclipse run` of the demo APK). Section (a)
> "Roblox" is marked **previously-verified** — it is the recorded evidence from §6 of AGENTS.md
> from earlier in the session, **not** re-run here.

---

## (a) What works now

### Verified at HEAD `16cb2e2` (re-run by this capstone, 2026-06-05)

**Quality gate — all clean:**

- `cargo fmt --all --check` — clean.
- `cargo build --all-targets` — clean.
- `cargo clippy --all-targets --all-features -- -D warnings` — clean (0 warnings).
- `cargo test` — **131 unit tests + 2 compile_fail doctests pass; 0 failed.**
- `cargo build --release` — clean (`panic=abort` / fat-LTO release profile).

**The demo APK boots all the way to a faithfully-rendered, RESUMED Android activity.**
`timeout 60 cargo run --release -- run ~/eclipse-m0/atl_test_apks/demo_app.apk`
(`/tmp/eclipse-capstone.log`, EXIT=124 = the 60 s present loop ran the full duration — the
expected success outcome). Observed end-to-end, in order:

1. **ART VM boots** with the demo's Java on the classpath (`ART VM booted … ✓`), from a pure-Rust,
   graphics-stack-free process (no GTK) — the Step 3.5 thesis (a non-GTK host keeps the low_4gb
   window clear for ART) holding in practice.
2. **Application lifecycle, steps 1–3:** `Context.createApplication` → `createContentProviders` →
   `Application.onCreate` (`Application.onCreate reached: recipe steps 1–3 driven`).
3. **Launcher Activity created (steps 4–5):** the demo's own Java overrides run and log
   `- onCreate - yay!`, `- setContentView - yay!`, `- onContentChanged - yay!`; the View hierarchy
   inflates; `findViewById(0x7f030000).setText(…)` succeeds (no NPE). Reported as
   `Activity.onCreate reached: recipe steps 1–5 driven` for `com.example.demo_application.MainActivity`.
4. **Started + resumed (steps 6–7):** `- onStart - yay!`, `- onResume - yay!`, then
   `Activity resumed: recipe steps 1–7 driven` and `framework lifecycle driven: ActivityResumed ✓`.
   **The framework Activity lifecycle reaches CREATED → STARTED → RESUMED.**
5. **Measure/layout pass** (debug log, `RUST_LOG=eclipse::graphics=debug`): the real recorded tree
   `FrameLayout(M×M) → LinearLayout(M×M) → 2×TextView(W×W)` resolves to rects
   FrameLayout (0,0,800×600), LinearLayout (0,0,800×600), TextView#1 (0,0,180.5×28),
   TextView#2 (0,28,204.3×28) — both layouts fill the window; the two WRAP TextViews size to their
   glyph-measured text and stack vertically.
6. **Vulkan view + text render:** a system font is discovered portably
   (`/usr/share/fonts/noto/NotoSans-Regular.ttf`) and an R8 glyph atlas built (95 glyphs); the
   winit window stands up a real surface + swapchain (`Vulkan surface + swapchain initialized …
   format=B8G8R8A8_SRGB extent=800x600 images=3`); and (at `RUST_LOG=eclipse::graphics=trace`) each
   frame logs `drawing recorded View tree into the swapchain views=4 quads=4 glyphs=31` — the
   4 recorded views drawn as 4 depth-colored quads with 31 rasterized text glyphs composited on top.
7. **Zero errors:** across the runs, grep count for `VK_ERROR | panic | Exception | draw-failed |
   UnsatisfiedLink | abort | validation` = **0**.

### Previously verified (recorded in AGENTS.md §6; NOT re-run in this capstone)

**The real Roblox APK reaches its own `RobloxApplication.onCreate` + startup tasks.** Running the
merged v2.724.735 APK (`~/eclipse-m0/apk/v2.724.735/roblox-2.724.735-merged.apk`) earlier in the
session: ART booted with Roblox on the classpath; `PackageParser.parsePackage` walked Roblox's real
manifest; certificates were collected and WolfSSL loaded; step 1 instantiated Roblox's own
`com.roblox.client.RobloxApplication`; and after binding `SystemClock.elapsedRealtime`, Roblox's
`Application.onCreate` **ran its own startup** — `roblox.config` (`setBaseUrl → www.roblox.com`),
`AppStartupTaskManager` tasks, `androidx.startup.InitializationProvider`. The run then advanced into
the engine-load (bionic-linker) track — see (c). Faithful run log of record: `/tmp/eclipse-roblox.log`.

---

## (b) Architecture — the Eclipse-owned subsystems built

A faithful inventory of what exists in the tree (no code here). All Eclipse-owned modules are
pure-Rust; the only vendored non-Rust black box remains ART + libcore (per the charter).

**Runtime / ART boot (`src/runtime.rs`).**
- Host-CPUID ISA detection emitting dex2oat's `--instruction-set-features` in ART's canonical
  token order (the M0 Step-4 fix for ATL's hardcoded baseline ISA).
- `BootPlan` deriving the boot args from the parsed manifest + config, split at the type level into
  VM options (`-X*` → `JNI_CreateJavaVM`) vs dex2oat options.
- `boot()` = `dlopen` the vendored `libart.so` **`RTLD_NOW | RTLD_GLOBAL`** (so libart's NEEDED
  `liblog.so` / `__android_log_print` is process-global, which WolfSSL's glibc-dlopen fallback
  needs) + `JNI_CreateJavaVM`, returning an owned, `!Send`/`!Sync` `Vm` handle that pins the VM to
  the booting (main) thread. The classpath loads the app's own Java
  (`api-impl.jar : apk : framework-res.apk`).
- Portable native-lib extraction to an XDG cache dir + `-Djava.library.path` wiring.
- **Bionic provisioning:** `whitelist_bionic_library_path` (calls libdl_bio's
  `dl_parse_library_path` from the global scope so `System.loadLibrary` resolves the extracted
  `.so`s) and `provision_bionic_sonames` (symlinks each run-confirmed bare Android soname — e.g.
  `libm.so` → the host's real-ELF `libm.so.6`, found portably via `cc -print-file-name` with an
  ELF-magic check that rejects the host's bare GNU-ld linker-script trap).

**APK / AXML / ARSC readers (`src/apk/`).**
- `mod.rs` — opens the APK zip (`zip 2`, `deflate`, no default features), reads the binary
  manifest, detects native ABIs + the x86_64 engine, streams a SHA-256 integrity hash, extracts
  native libs (atomic temp+rename), exposes `read_entry`.
- `axml.rs` — Eclipse's own **total** pure-Rust binary-XML reader (`#![forbid(unsafe_code)]`): the
  5-field `read_manifest` path plus a general event-walk `parse_document` → `XmlDocument`
  (elements/attributes with raw `Res_value` + resolved strings/text/namespaces), including
  `RES_XML_RESOURCE_MAP_TYPE` decode so attribute `name_resource` is populated. Bounds-checked,
  never panics (totality fuzz over UTF-8 + UTF-16 pools).
- `arsc.rs` — self-contained, total `resources.arsc` (ResTable) reader: `resource_value`,
  `resolve`, `value_string`, `package_name`, multi-package selection by id high byte.

**JNI framework natives (`src/framework.rs` + `src/framework/*`).** Eclipse's own **non-GTK** Rust
backing for the `android.*` natives the lifecycle reaches, bound via `RegisterNatives` (which wins
over ATL's name-based lazy binding) before `Context.<clinit>`. Each native is `extern "system"`,
runs under `EnvUnowned::with_env` (`catch_unwind`-wrapped) with a neutral default on error, typed
`FrameworkError`, no unwrap, no panic crossing into ART (`panic=abort`). Bound surfaces include:
- **Context** (`native_get_apk_path`, `native_updateConfig`), **Log** (`println_native` → `tracing`),
  **Environment** (`native_get_app_data_dir` → portable XDG path), **SystemClock**
  (`elapsedRealtime` → monotonic `Instant`).
- **AssetManager** — `init`, `native_setApkAssets`, `setConfiguration`, `openXmlAssetNative` (real:
  reads + parses the named APK entry into the XML registry), `retrieveAttributes` (real XML-attribute
  extraction into off-heap TypedArray buffers via bounds-proven writes), `applyStyle` (resolves XML
  attrs from its parser arg), `newTheme`/`applyThemeStyle`/`copyTheme`, `getResourceName`/
  `loadResourceValue` (route through app + framework ARSC by package id).
- **XmlBlock** parser natives over the parsed tree (`nativeCreateParseState`/`nativeNext`/`nativeGetName`/
  `nativeGetAttributeIndex`/`nativeGetAttributeStringValue`/`nativeGetPooledString`/`nativeGetLineNumber`/
  `nativeDestroyParseState`/`nativeDestroy`).
- **View** (`native_constructor`, `native_setPadding`, `native_setLayoutParams` [records
  LayoutParams], `native_requestLayout`), **ViewGroup** (`native_addView` records the real
  parent→child edge), **TextView** (`native_constructor`, `native_setText`), **Window**
  (`set_jobject`/`set_title`/`set_layout`/`set_widget_as_root`), **Paint** (`native_create`).
- The **TypedArray window layout** is the standard AOSP API-29+ one (stride 7: TYPE@0, DATA@1,
  ASSET_COOKIE@2, RESOURCE_ID@3), found empirically + pinned by tests; `android:id` (a REFERENCE)
  lands in RESOURCE_ID@3 so `findViewById`/`setId`/`getId` work in pure Java.
- **Sound generational-slab registries** (`window_registry`, `xml_registry`, `view_registry`,
  `theme_registry`, `paint_registry`), each `#![forbid(unsafe_code)]`: the `jlong` handle is a slab
  *index* packing slot + generation — **never a raw pointer**; a stale/out-of-range/fabricated/
  double-freed handle is a typed `Err`, never UB; 5–6 soundness tests each.

**Lifecycle driver (`src/framework.rs::drive_lifecycle` / `drive_application_lifecycle`).** Drives
the recipe steps 1–7 on the held `Vm` / JNI-attached main thread, each call through a `checked()`
helper that describes + clears any pending Java exception and surfaces a typed error: step 1
`Context.createApplication(window)` → 2 `ContentProvider.createContentProviders()` → 3
`Application.onCreate()` → 4 `Activity.createMainActivity(class, window, null)` → 5
`Activity.onCreate(null Bundle)` → 6 `Activity.onStart()` → 7 `Activity.onResume()`.

**Window + graphics (`src/graphics.rs`, `shaders/`).** `winit 0.30` host window (Wayland/X11,
**no GTK**). `VulkanRenderer` (`ash 0.38` with `loaded` → runtime `dlopen` of `libvulkan.so`, no
link-time dep; `ash-window 0.13`; `raw-window-handle 0.6`): instance with discovered surface
extensions → `VkSurfaceKHR` from the window's raw handle → physical device + graphics/present queue
→ swapchain (BGRA8_SRGB, FIFO, min+1 images, recreate on resize). A **faithful measure+layout pass**
(MeasureSpec resolution, top-down measure/layout, vertical LinearLayout stacking + trivial weight,
FrameLayout gravity with the `gravity=-1` UNSPECIFIED guard, padding insets) runs once over the
view-registry snapshot each frame. A **colored-quad pipeline** (embedded SPIR-V, no build-time
shader compiler) draws each laid-out view; a **textured-glyph pipeline** (R8 atlas via `ab_glyph`,
combined-image-sampler + push-constant color) composites each TextView's text on top. Init failure
is non-fatal (typed warning, window stays open). All handles freed in `Drop` after
`device_wait_idle`; every `unsafe` carries a `// SAFETY:` note.

**Diagnostics + config (`src/diagnostics.rs`, `src/config.rs`).** `tracing` + env-filter
subscriber; the full Sober `config.json` schema via `serde`/`serde_json` + portable XDG path
(`directories`), typed `ConfigError`, no panics.

---

## (c) The two remaining tracks (and why blocked for autonomous subagents)

### 1. ENGINE-LOAD — the bionic-linker relocation wall (the #1 frontier)

This is the gate between "Roblox's Java shell runs" and "the native engine loads + renders."

- **The wall (from the faithful Roblox run, not inference):** with the app-lib search path
  whitelisted (`dl_parse_library_path`) and bare host sonames provisioned (`libm.so` →
  `libm.so.6`), the apkenv-era C shim linker (`libdl_bio.so.0`) now **finds + opens** the libs but
  **cannot relocate** them — `unknown reloc type 18 → failed to link libm.so`. Type 18 on x86-64 =
  `R_X86_64_TPOFF64` (TLS thread-pointer offset, used for per-thread `errno`); the host `libm.so.6`
  also carries `RELR`-compressed relatives + `BIND_NOW`. These are pervasive modern-toolchain
  defaults, so provisioning host libs is necessary-but-insufficient — **the limitation is the
  linker, not the libs**. `libm.so` is a `DT_NEEDED` of both zstd-jni and `libroblox.so`, so this
  wall is **upstream** of the NDK soname shims (`libmediandk.so` / `libOpenMAXAL.so` / `liblog.so`)
  in [`docs/bionic-loader-plan.md`](bionic-loader-plan.md).
- **Decided v1 strategy — HYBRID** (see [`docs/bionic-loader-strategy.md`](bionic-loader-strategy.md)):
  minimally **extend** the proven C `bionic_translation` linker for `R_X86_64_TPOFF64` / `RELR` /
  `BIND_NOW` to unblock now (charter-sanctioned v1 FFI), and keep the from-scratch Rust bionic
  loader as the durable do-LAST replacement behind an ABI conformance suite (the C extension doubles
  as that conformance spec). A reloc-clean shim for `libm`/errno was assessed **infeasible** (TLS is
  semantic, not cosmetic); a newer-AOSP-linker swap imports a glibc-vs-bionic TLS-interop project at
  linker scope with worse charter fit.
- **Smallest first step:** a throwaway probe that `bionic_dlopen`s the already-provisioned `libm.so`
  in isolation (no ART/engine), reproduces `reloc type 18` in seconds, then proves the fix on ONE
  reloc (`R_X86_64_TPOFF64` → `libm.so` links + `errno==EDOM` per-thread); RELR/BIND_NOW are then
  incremental. Concrete steps are in [`docs/dev-host-runbook.md`](dev-host-runbook.md) Section B.
- **Why subagents can't do this:** dynamic-linker source work trips the Anthropic cyber-safeguard on
  workflow subagents. The probe + extension are **main-loop / dev-host only**. (They also need a
  main-thread `cargo run`, which the cargo-test harness can't provide — ART aborts on worker threads.)

### 2. FRAMEWORK BREADTH — more layouts / widgets / the real surface + input

The framework is broad enough to run the demo and reach Roblox's `Application.onCreate`, but is
intentionally minimal. Documented out-of-scope today: RelativeLayout / ConstraintLayout, exact
multi-pass weight, baseline alignment, scrolling, **LinearLayout `orientation` detection** (the
field is not threaded through any native, so a LinearLayout defaults to vertical), real input
(`take_input_queue` deferred), and wiring the eventual engine surface into this window's swapchain
via WSI translation.

- **Constraint:** the cyber-safeguard has escalated to trip on Android-internals source (content/res
  Java, api-impl-jni C, bionic). Further native binds must therefore be **empirical / signature-only**
  — driven by the dev-host discovery loop (each missing native named by ART's `No implementation
  found` line, cross-checked only against allowed view/widget Java), not by reading the denylisted
  framework internals. This is how the entire step-4/5 cascade and the AssetManager/XmlBlock surface
  were already bound.

---

## Pickup for the next session

- The headline milestone (demo: boot → lifecycle CREATED→STARTED→RESUMED → faithful Vulkan
  view+text render, zero VK errors) is **verified at HEAD `16cb2e2`**.
- The single next frontier is **engine-load: extend the bionic shim linker for
  `R_X86_64_TPOFF64`/`RELR`/`BIND_NOW`**, starting with the throwaway TLS-reloc probe — **main-loop /
  dev-host only** (cyber-safeguard blocks subagents on linker source).
