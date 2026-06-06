# Eclipse — Engine-load capstone & handoff (2026-06-05)

> **Purpose.** The single, faithful, dated capstone for the engine-load track: what the
> from-scratch Rust bionic loader + bionic environment actually do today, the ONE wall that
> still stands between "the engine inits + registers JNI" and "the engine runs + renders," and
> the runbook for the moment that wall is cleared.
>
> **Authority.** This doc summarizes; it does not override. `CLAUDE.md` (global engineering
> policy) and `AGENTS.md` (project charter + §5 Living State + §6 Decisions Log) remain the
> sources of truth. It supersedes the engine-load sections of the earlier
> [`docs/project-state-2026-06-05.md`](project-state-2026-06-05.md) and
> [`docs/bionic-loader-strategy.md`](bionic-loader-strategy.md), both of which were written
> *before* the Rust loader was built and describe the older "extend the apkenv C linker" plan
> that the Rust loader replaced. Track-level detail:
> [`docs/libroblox-init-run.md`](libroblox-init-run.md) (§9–§11),
> [`docs/libroblox-characterization.md`](libroblox-characterization.md),
> [`docs/bionic-env-worklist.md`](bionic-env-worklist.md).
>
> **Faithful-reporting note.** **Roblox does NOT render yet.** The load + init + I/O foundation
> below is complete and verified; the engine actually *running* and *rendering* is the remaining
> work, gated on the one wall in section (b). Every number below is a real verified value (gate +
> the gated regression guards, re-run for this capstone), not an estimate.

---

## Verified state (re-run for this capstone, 2026-06-05)

- **HEAD:** `1d4228e` (`loader: real OpenSL ES engine -> host audio (cpal) …`).
- **Quality gate — all clean:**
  - `cargo fmt --all --check` — clean.
  - `cargo build --all-targets` — clean.
  - `cargo clippy --all-targets --all-features -- -D warnings` — clean (0 warnings).
  - `cargo test` — **420 unit + 4 integration + 2 compile-fail doctests pass; 0 failed.**
  - `cargo build --release` — clean (`panic=abort` / fat-LTO release profile).
- **The four engine-load regression guards** (`tests/engine_milestones.rs`,
  `cargo test --release --test engine_milestones`) — **all 4 RAN + PASSED** on this dev host
  (Roblox APK + Wayland display both present, so none skipped). Each asserts the harness's exact
  REAL success marker (not a trivially-passing check) and a success exit status, and each SKIPS
  CLEANLY when its precondition is absent (no APK / no display), so the suite never fails
  spuriously headless/CI.

---

## (a) What works — the load pipeline and the verified milestones

### The from-scratch Rust bionic loader (pure-Rust cores; the only vendored black box stays ART)

Eclipse owns a complete, unit-tested, x86-64 ELF dynamic-loader pipeline that takes the **real**
`lib/x86_64/libroblox.so` (~111 MiB, the actual engine) from bytes to running code. The four
relocation/loader cores are `#![forbid(unsafe_code)]`; `unsafe` is confined to the syscall/FFI
seams (mmap/mprotect, one `dlsym`, the foreign-call jumps), each with a dated `// SAFETY:`.

- **`src/loader/elf.rs`** — a total, bounds-checked `ET_DYN` decoder that produces exactly the
  applier's inputs (relocations, dynsyms, `DynInfo`, `PT_LOAD`/`PT_TLS`/`PT_GNU_RELRO` layout). It
  decodes libroblox's **Android packed (`APS2`) relocations** at `DT_ANDROID_RELA*`, so
  `relocations()` returns **all 527,843** of the engine's relocs (an exact match to `llvm-readelf`).
- **`src/loader/reloc.rs`** — applies every reloc type an Eclipse-loaded `.so` needs
  (`RELATIVE`/`GLOB_DAT`/`JUMP_SLOT`/`64`/`TPOFF64`/`RELR`) over a safe `&mut [u8]`.
- **`src/loader/map.rs`** — reserves one contiguous region, places the `PT_LOAD` segments, applies
  base + symbol + TLS relocs, honors `PT_GNU_RELRO` (`mprotect` read-only after reloc), `BIND_NOW`
  (eager `JUMP_SLOT`).
- **`src/loader/resolve.rs` / `tls.rs` / `link.rs`** — the System-V symbol-resolution scope, the
  variant-II static-TLS math, and the dependency-graph linker.

Verified end-to-end on the real engine (gated `link.rs` real-libroblox tests; skip cleanly if the
APK is absent): map 3 `PT_LOAD`, apply **527,208 `R_X86_64_RELATIVE`**, harden 1 RELRO region, and
— with the bionic environment below prepended — resolve **all 584 UND imports** to Eclipse/host
addresses (`unresolved_strong = 0`).

### The Eclipse-owned bionic environment (584-import work-list CLOSED to 0)

`src/loader/bionic_env.rs` + `native_provider.rs` + the per-category tiers resolve every libroblox
import. Full work-list (now COMPLETE): [`docs/bionic-env-worklist.md`](bionic-env-worklist.md).

- **liblog (5)** → Eclipse's `tracing` sink (the 2 C-variadic entries via a clean-room `cc` shim).
- **bionic-libc (15)** — `_FORTIFY` `_chk` family, `__errno`, `__sF`, `__system_property_get`,
  `__stack_chk_guard`, `__assert2` (forward to the ABI-identical glibc op / minimal-correct).
- **bionic `sysconf` tier (5)** — `sysconf`/`getauxval`/`sched_getcpu`/`getpagesize`/`sysinfo`,
  bionic-ABI-correct (the bionic `_SC_*` constant values differ from glibc — the root cause of the
  allocator-bootstrap abort, fixed).
- **bionic pthread + thread lifecycle (51)** — futex-backed mutex/cond/rwlock/sem, `pthread_once`,
  TLS keys, and the full TID-based thread lifecycle (`pthread_create`/join/detach/setname/…).
- **ndk-android (27)** — `AAsset*`/`AAssetManager*` route to Eclipse's own `src/apk` reader;
  `AConfiguration*` minimal-correct; `ALooper*` a real fd-backed looper; `ANativeWindow*` WSI-bound.
- **EGL/GLES2 (91)** → host Mesa (libroblox is a GLES2/EGL render path — **0 Vulkan**).
- **OpenSL ES audio (8)** → a real OpenSL ES 1.0.1 engine bridged to host audio (cpal).
- **media-ndk (33)** → sound-stubs (gameplay-time, deferred, public-NDK sentinels).

> **Honest baseline caveat (preserved from the source docs).** The host-resolved glibc/host-GL
> addresses are a relocation-pipeline baseline, not a guarantee of bionic-ABI-correct execution
> everywhere; where bionic and glibc diverge in a way the engine's init path depends on (sysconf,
> pthread/TLS identity), Eclipse owns a bionic-correct native PREPENDED before the host tier. Those
> divergences were each found evidence-first and fixed (see the run track below).

### The init + JNI milestones (proven in the LIVE ART runtime)

`src/loader/engine.rs` integrates the pipeline into the live `eclipse run` path (persistent form —
the image stays mapped for the process lifetime so the engine's workers keep running). On the real
Roblox APK, deterministically, in the live process (evidence:
[`docs/libroblox-init-run.md`](libroblox-init-run.md) §8–§9):

1. **libroblox maps + relocates + RELRO-hardens** in the live runtime (527,208 RELATIVE + 623
   symbol relocs, `unresolved_strong = 0`).
2. **All 3,427 `DT_INIT_ARRAY` constructors run** in order, deterministically, EXIT=0 (the engine
   even emits its own liblog warnings through Eclipse's liblog natives). Getting here required three
   evidence-first root-cause fixes: the bionic-vs-glibc `sysconf` constant mismatch (allocator
   bootstrap), a mixed-`pthread_t`-ABI worker-thread crash, and a `pthread_create` child-TID
   use-after-free.
3. **`JNI_OnLoad` runs against Eclipse's REAL ART `JavaVM` and returns `JNI_VERSION_1_6`** — the
   engine's native methods are now registered against the running ART VM.
4. **The framework lifecycle then drives Roblox's OWN `Application.onCreate`** — real Roblox Java
   runs (`roblox.config setBaseUrl → www.roblox.com`, `rbx.baseurl`), reaching
   `androidx.startup.InitializationProvider`.

### The engine I/O surfaces (built + validated in isolation, drive-ready)

These are the surfaces the engine lights up once it reaches a frame. Each is validated standalone
via a hidden harness subcommand and protected by a gated regression guard in
`tests/engine_milestones.rs`. Harness commands and the markers they assert:

| Surface | Harness | Asserted REAL marker |
|---|---|---|
| **init** | `eclipse __run-libroblox-init` | EXIT=0 + `ALL 3427/3427 constructors completed without a crash` |
| **render (EGL/GLES2 on Eclipse's window)** | `eclipse __gl-test` | `EGL+GLES2 OK:` + `0 GL errors, all swaps succeeded` |
| **render (ANativeWindow WSI bind)** | `eclipse __gl-test-anw` | `ANativeWindow* is the real WSI handle = true` + `0 GL errors, all swaps succeeded` |
| **input (real fd-backed ALooper)** | `eclipse __input-test` | `input path OK:` + `pollOnce returned ident 11` + `parked pollOnce returned ALOOPER_POLL_WAKE` |
| **audio (OpenSL ES → cpal)** | `eclipse __audio-test` | a real PCM tone plays end-to-end through the public OpenSL vtables (0 SL errors) |

- **Render** (`src/egl_engine.rs`): an EGL display + GLES2 context + on-screen window surface on
  Eclipse's existing winit window, using host EGL/GLESv2; `ANativeWindow_fromSurface` returns the
  **real host-EGL WSI native window** (Wayland `wl_egl_window*` / X11 XID), so the engine's OWN
  `eglCreateWindowSurface(ANativeWindow)` presents to Eclipse's window (validated: 0 GL errors,
  swaps succeed). The Java-view Vulkan path (`src/graphics.rs`) is untouched (no regression).
- **Input** (`src/loader/looper.rs`): a genuine `poll(2)`-backed, wakeable `ALooper`; a winit input
  event wakes a parked `pollOnce`. Binary evidence: libroblox is **not** a NativeActivity — it
  receives input via JNI-push, so an `AInputQueue` surface would be dead code (intentionally absent).
- **Audio** (`src/loader/opensl.rs`): a working OpenSL ES 1.0.1 engine → cpal host output (real tone
  end-to-end; clean "no device" posture when no audio device exists).

---

## (b) The one wall — the native-load routing step

This is the single render-critical blocker. It is **not** in Eclipse's loader and **not** in
libroblox — both are correct (libroblox loads, inits 3427/3427, and `JNI_OnLoad`s cleanly).

### The concept

During `Application.onCreate`, `androidx.startup` calls `System.loadLibrary("zstd-jni")`. That call
goes through ART's `Runtime.nativeLoad`, which re-enters the vendored apkenv-era shim linker —
**ART does not consult Eclipse's pre-loaded-lib registry.** Eclipse already pre-loads the app's full
`lib/x86_64/*.so` set through its own Rust loader (`zstd-jni` relocates cleanly, work-list 0), but
that pre-load is **inert**: `loadLibrary` independently re-loads the soname through apkenv, whose
own dependency-graph walk then faults. So pre-loading is correct and necessary but does nothing
until the routing step is wired.

The remaining step, at a high level, is to make ART's `Runtime.nativeLoad` **consult Eclipse's
pre-loaded-lib registry first** and report a pre-loaded soname as already-loaded (a success result)
— short-circuiting the apkenv re-load for libraries Eclipse's own loader already mapped + relocated.

### Why it is the blocker

`onCreate` cannot get past `androidx.startup` until that `System.loadLibrary` succeeds. Every
downstream milestone (the engine's frame loop, and therefore the already-built render/input/audio
surfaces) is behind it. The pre-load half is done and proven; the consult half is all that remains
on the critical path.

### The precise file / seam (NOT the interception code)

- **The registry to consult:** the process-global pre-loaded-lib registry maintained by
  `src/loader/engine.rs` (`load_app_native_lib` records each loaded soname; the mappings are kept
  alive for the process lifetime). A `nativeLoad` for a soname present there is a hit.
- **The seam to re-route:** ART's `java.lang.Runtime.nativeLoad` — re-`RegisterNatives` it (the same
  mechanism `src/framework.rs` already uses to win over ATL's lazy binding) so it checks the
  engine.rs registry and returns success for a hit, falling through to the existing path otherwise.

> **This step trips Anthropic's cyber-safeguard** (the native-load / `nativeLoad` / apkenv-linker
> region). It is therefore **NOT** doable by a workflow subagent — it needs the **Cyber Verification
> Program** or a one-time **human edit** in the main loop. The interception code is deliberately not
> written here; only the concept + the file/seam are named. (It also needs a main-thread `cargo run`
> on the dev host — ART aborts on worker threads, which the cargo-test harness can't avoid.)

---

## (c) The runbook — once the wall is cleared

When `Runtime.nativeLoad` consults the engine.rs registry (so `System.loadLibrary("zstd-jni")`
short-circuits to the already-pre-loaded, cleanly-relocated copy):

1. **`onCreate` should pass `androidx.startup`** — the `zstd-jni` load returns success instead of
   re-entering apkenv and faulting; the `AppStartupTaskM` SIGSEGV is gone.
2. **The boot should reach the engine's frame loop.** libroblox is already inited + JNI-registered;
   once its startup tasks complete, the engine drives its render/input path.
3. **The already-built I/O surfaces light up:**
   - **Render** — the engine's own `eglCreateWindowSurface(ANativeWindow*)` presents to Eclipse's
     window via the WSI bind (validated 0-GL-error in isolation; lands the moment a frame is drawn).
   - **Input** — the real fd-backed `ALooper` + winit feed delivers liveness wakes; the engine's
     JNI-push input path drives gameplay input.
   - **Audio** — the engine's `slCreateEngine` drives the real OpenSL ES → cpal output.

### What to watch for next (the likely next frontier)

The likely next frontier is **not** the render path (built + validated) — it is the
**host-baseline bionic libc ABI-mismatch class**, the same class as the already-fixed `sysconf`
constant mismatch and the pthread/TLS-identity fixes. As the engine runs further past
`androidx.startup`, more of its imports that currently bind to the host glibc baseline may hit a
point where bionic and glibc diverge in a way the engine depends on. Each such case must be:

- **diagnosed evidence-first** (env-gated trace + gdb/objdump on the mapped image, the method that
  found every prior fix — no brute-forcing), then
- **fixed by an Eclipse-owned, bionic-ABI-correct native PREPENDED before the host tier in
  `BionicEnv`** (the established pattern; the host baseline is displaced, never relied on where it is
  wrong).

This is steady, mechanical, evidence-first work — not another conceptual wall.

---

## Honest status

- **Does Roblox render yet?** **No.**
- **The exact remaining gate:** the native-load routing step in section (b) — ART's
  `Runtime.nativeLoad` must consult Eclipse's pre-loaded-lib registry so
  `androidx.startup`'s `System.loadLibrary("zstd-jni")` succeeds instead of re-entering the apkenv
  linker. It trips the cyber-safeguard → needs the Cyber Verification Program or a human one-time
  edit (main-loop / dev-host only).
- **What IS complete:** the load + init + I/O foundation. The real engine maps, relocates (527,843
  relocs incl. APS2), resolves all 584 imports, runs all 3,427 `DT_INIT_ARRAY` constructors
  deterministically, and `JNI_OnLoad`s against the real ART VM; the render (EGL/ANativeWindow WSI),
  input (real ALooper), and audio (OpenSL → cpal) surfaces are built and validated in isolation; the
  threading UAF is fixed; and four gated regression guards protect the milestones.
- **What remains:** the engine actually running + rendering — gated entirely on the one wall above.

---

## Reproduce (dev host)

```sh
# Gate (benign; no binary RE)
cargo fmt --all --check
cargo build --all-targets
cargo clippy --all-targets --all-features -- -D warnings
cargo test
cargo build --release

# The four engine-load regression guards (skip cleanly without the APK / a display)
cargo test --release --test engine_milestones -- --nocapture

# The I/O surfaces in isolation (dev host with a display / audio device)
./target/release/eclipse __gl-test
./target/release/eclipse __gl-test-anw
./target/release/eclipse __input-test
./target/release/eclipse __audio-test
```

The native-load routing step itself (section (b)) is **main-loop / dev-host only** — it is inside
the cyber-safeguard boundary and must be done via the Cyber Verification Program or a human edit.
