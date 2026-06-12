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

- **2026-06-12 (main-Looper pump) — 🚀 Roblox now boots PAST the splash into DEEP engine init (Mimalloc · RbxStorage/
  SQLite WAL · AndroidGLView · HTTP/network · telemetry) and the engine SIGSEGV is now REPRODUCIBLE + ROOT-CAUSED. ⇐ START
  HERE NEXT SESSION (frontier = libroblox static-TLS for engine-spawned threads — a `thread_local` DTOR runs at thread
  exit on a never-constructed/zero object; see the gdb root-cause + `src/loader/tls.rs` below).** Implemented the **Android main-`Looper`
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
  SIGSEGVs):** the faulting frame is a libroblox **C++ `thread_local` DESTRUCTOR** (fn @ `+0x2779bb0`) running from
  **`__call_tls_dtors`** (libc) — i.e. a thread is EXITING — on an **all-zero object** (`rbx=valid` but `rbx+0x3e0..+0x420`
  all 0, so `[rbx+0x408]=NULL` → write to `NULL+0x58`). The exiting thread is one of several ART-attached engine threads
  named "Main" (siblings: `RBX Worker A–P`, `[vkcf]/[vkrt]/[vkps]`, `HttpClient`, …). **The deep cause:** Eclipse loads
  `libroblox.so` via its OWN loader (not glibc `dlopen`), so **glibc never initializes libroblox's static-TLS block for
  newly-spawned engine threads** — their `thread_local`s stay zero, yet glibc's `__call_tls_dtors` still runs the
  registered destructor at thread exit → null deref. (`__cxa_thread_atexit_impl` is a WEAK libroblox import left on the
  host glibc baseline — `bionic_pthread.rs:1773` "ABI-identical"; the gap is the static-TLS *template init for new
  threads*, the loader's `tls.rs` domain — NOT a JNI/bionic-return null as first guessed.) The pump only EXPOSED it by
  advancing Roblox far enough to spawn+exit those threads. **NEXT: make libroblox's static-TLS block be allocated +
  template-initialized (and/or its `thread_local` ctors run, or destructors safely skipped) for engine-spawned threads**
  — `src/loader/tls.rs` + the thread-create path. **Also fixed a `panic = "abort"` regression the pump exposed:** Eclipse routes the engine's
  `android.util.Log`/`liblog` firehose + its own native diagnostics through `tracing`, emitted from ART/bionic WORKER
  threads; `tracing-subscriber`'s default `fmt` layer formats via a `thread_local! BUF` (`fmt_layer.rs:1022 BUF.with`),
  and a worker logging during its TLS teardown hit `LocalKey::with` on a destroyed TLS → AccessError → **process abort**.
  Replaced it with a teardown-safe `diagnostics::PanicSafeStderr` layer that formats into a function-LOCAL buffer (zero
  thread-locals; same RFC3339+level+target+fields format, no ANSI). Gate: **517 unit + 4 integration + 2 doctests**
  (+1 `nativePollOnce` yield-table test), fmt/clippy `-D warnings`/release all 0-warning. Durability: overlay still needs
  `ECLIPSE_ANDROID_FRAMEWORK_DIR`; pump + logging fix are in-binary. Detail: §6 (2026-06-12 main-Looper pump).
  *(Superseded entry below — its "engine SIGSEGV resolved / 6-6 clean" held only while the splash stall hid the fault.)*
- **2026-06-12 (live-validated) — ⚠️ SUPERSEDED: "engine SIGSEGV resolved; boot STABLE to window (6/6 clean)" — the
  fault was merely UNREACHED behind the splash stall; the main-Looper pump above reaches it every run.**
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
  ART/libcore networking-internal miss, NOT an Eclipse gap — wolfSSL-backed okhttp sockets do connect/read); the benign
  `framework-res.apk` dex2oat "no dex files" + `ClassLoaderContext`/duplicate-class warnings; Canvas `nDrawColor` draw
  cascade still disabled (GskCanvas-backed, view quads + text still render). Gate: **516 unit + 4 integration + 2
  doctests**, fmt/clippy `-D warnings`/release all 0-warning. Durability: overlay output is a cache artifact (rebuild via
  `tools/framework-overlay/patch-framework.sh`; `eclipse run` needs `ECLIPSE_ANDROID_FRAMEWORK_DIR`). Detail: §6
  (2026-06-12 engine-SIGSEGV-resolved).
- **2026-06-12 — EARLY-FAULT TAP IMPLEMENTED (gate-green: 516 unit + 4 integration (self-skip path, displays unset)
  + 2 doctests = 522 passed, 0 failed; STILL UNCOMMITTED with the rest of the held signal-ABI work — owner hold on
  all post-2026-06-11-morning work). ⇐ START HERE NEXT SESSION (frontier = OWNER live validation on the dev-host
  MAIN LOOP):** run `ECLIPSE_ANDROID_FRAMEWORK_DIR=$HOME/.cache/eclipse/framework-patched cargo run -- run <APK>`
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
  re-entry latch — the two 2026-06-12 review-fix entries in §6). Detail + carried review notes: §6
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
