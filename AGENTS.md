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

- **2026-06-11 (evening) — 🎉🎉 FULL LIFECYCLE MILESTONE: recipe steps 1–7 (CREATED → RESUMED) drive on the REAL
  Roblox v2.721.1108 — `ActivitySplash` runs its startup, the host window + Vulkan swapchain open, and the ENGINE's
  own native startup executes (`JNIRobloxSettings` → crashpad init). ⇐ START HERE NEXT SESSION (frontier = the
  bionic SIGNAL ABI: a SIGSEGV inside libroblox dies in a GARBAGE signal handler).**
  This session corrected TWO wrong framings from the prior entry with dex/core evidence (detail: §6 evening entry):
  (1) the "Background process detected" check is **`ActivityManager.getRunningAppProcesses()`** (dex `yj.s.b`: an entry
  with `importance==100` whose `pkgList` contains the package) — NOT `getProcessName`; (2) the `System.exit(10)` came
  from **ATL's vendored-libcore `Thread.java` default uncaught-exception handler** (exits 10 on ANY thread's uncaught
  exception) — so **"worker-thread gaps are non-blocking" is FALSE**; each uncaught worker exception is process-fatal.
  **Fixed this session:** (a) `~/.cache/eclipse` was WIPED (overlay + its script lost) → the patch tooling is now
  **IN-REPO `tools/framework-overlay/`** (script + patched ATL sources + compile-only stubs; README documents the
  multidex first-dex-wins mechanism); overlay now patches **Build + NetworkRequest + ActivityManager (foreground
  RunningAppProcessInfo) + PowerManager (`isDeviceIdleMode` — the actual exit-10 killer)**; (b) in-binary natives
  bound: `View.nativeSetFullscreen (JZ)V`, `TextView.native_setTextColor (I)V` (instance, NO widget param),
  `Path.native_reset (JJ)V` (frees both registry slots; the splash spinner calls it per frame) + name-sig pin tests.
  **NEW FRONTIER (core-dump evidence, `coredumpctl` 455287):** during `InitHelper.getAllAppSettings` → engine JNI, a
  SIGSEGV is raised inside `libroblox.so` (≈ base+0x1f28eff) and the kernel-invoked handler address is GARBAGE/unmapped
  (gdb: `#1 <signal handler called>`, `rdi=0xb`) → double-fault SIGSEGV death. Crashpad had JUST registered its
  first-chance handler; Eclipse's native provider does NOT intercept `sigaction` (only Eclipse's own `init_run.rs`
  crash hook uses host sigaction), so bionic `sigaction` from engine code falls through to HOST GLIBC whose
  `struct sigaction` layout differs from bionic LP64 (handler@0/mask@8(128B)/flags@136 vs flags@0/handler@8/mask@16(8B))
  → scrambled registration. NEXT: bionic-correct signal surface in `native_provider` (sigaction/sigaction64,
  sigprocmask, sigaltstack; mind ART's sigchain), then find what raises the FIRST fault. Worker gaps still open:
  `Log.println_native` (benign, caught), WorkManager "non-main process" framing, `java.time` BootstrapMethodError,
  Firebase StreamCorruptedException. Gate: **506 unit + 4 integration + 2 doctests**, fmt/clippy `-D warnings`/release
  all 0-warning. Prior state (SQLite A+B, APK auto-fetch) is committed at `f886fcf`/`13de7ec`; durability caveat now
  reduced to: the overlay output still lives in the cache and `run` still needs `ECLIPSE_ANDROID_FRAMEWORK_DIR`
  (auto-provision from inside Eclipse remains open).

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
