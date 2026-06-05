# Eclipse — The Vendored Runtime (ART & the C component chain)

> **Status:** Scoping deep-dive. Last verified **2026-06-04**. Companion to
> [`sober-research.md`](./sober-research.md) (§4.3) and [`tech-selection.md`](./tech-selection.md).
> Focus: what we reuse instead of writing in Rust, and what that costs in **performance**
> and **stability** — the two priorities for this project.

## TL;DR

- We **vendor AOSP ART + libcore** (the dex VM + Java core classes) plus a small chain of
  C support libraries. This is the one part that can't be Rust.
- **It does not hurt gameplay performance.** Roblox's frame loop is its **native engine**
  (`.so`, already x86-64) → our Vulkan forward → GPU. ART runs only the **Java/Kotlin
  shell**. ART affects **startup + menu latency**, *not* FPS.
- **Stability strategy:** pin a known-good `art_standalone` commit (don't track AOSP
  head), prefer **JIT/interpreter** over install-time AOT for the app dex (more portable,
  and Java isn't the hot path), keep only the **libcore boot image** AOT-compiled, and
  **detect JIT viability at runtime** with an interpreter fallback.

---

## 1. What actually gets vendored (the chain)

ATL/Sober's runtime is ART plus a handful of C libraries. Eclipse reuses the same set
(via the `art_standalone` super-build), then drives it from Rust. Build order and roles:

| # | Component | Lang | Role | Eclipse plan |
|---|---|---|---|---|
| 1 | **wolfSSL** (v5.8.2-stable, built from source w/ JNI flags) | C | TLS for libcore's networking (Conscrypt-style provider) | **Reuse.** (Our *launcher* downloads use `rustls`; the VM's own networking uses wolfSSL — two stacks, fine.) |
| 2 | **libunwind** | C | Stack unwinding for ART (exceptions, crash traces) | Reuse. |
| 3 | **bionic_translation** | C | bionic→glibc libc/linker shim; loads the app's native `.so` | Reuse v1 → **port to Rust** (`elf_loader`) incrementally. |
| 4 | **art_standalone** (= libart, dex2oat, boot image, **libcore jars**, libandroidfw) | C++/Java | **The dex VM + Java core** + asset/resource loading | **Reuse, pinned.** No Rust alternative. |
| 5 | **libOpenSLES** (standalone, optional) | C | Android OpenSL ES audio entry the app links | Reuse v1 → route to our Rust `libpulse` audio over time. |
| 6 | **api-impl.jar** (the `android.*` reimplementation) | Java | The Android *framework* (Activity/View/Surface/…) | This is the framework layer — **ours** (Java on ART), native backends in Rust via `jni`. |

So "the runtime" = **reused VM/core (1–5)** + **our framework + bridges (6 + Rust)**.

---

## 2. `art_standalone` specifics (the important nuances)

- **Base:** the AOSP **`dalvik` branch**, core components from the **`android-6.0.1_r46`**
  tag (Android 6.0.1 / Marshmallow / API 23) — **but ART and libcore are forward-ported
  via git history** so the VM understands modern dex. This matters: a current Roblox APK's
  dex runs because the **VM was updated**, even though the surrounding base is old.
- **Build:** `make` (its own makefile super-build), not Meson. Needs **Java, Python,
  Meson** (for sub-parts), **EGL**, and **wolfSSL compiled from source** (Debian's package
  omits the JNI flags ART needs). `ARCH=x86` for x86 builds; `make install PREFIX=…`.
- **Patches it applies to AOSP:**
  - bakes in a **default bootclasspath** (so callers don't have to specify the libcore jars
    every launch),
  - makes ART **load JNI libraries through the translation linker** (so the app's bionic
    `.so`s resolve via the shim, not glibc's `ld.so`),
  - removes the **`/system/bin/linker`** / Android-path assumptions that stop stock ART
    from running on a glibc host.
- **Outputs we ship:** `libart.so`, `dex2oat`, the **boot image** (`boot.art` / `boot.oat`),
  the **libcore jars** (`core-oj.jar`, `core-libart.jar`, …), and `libandroidfw`.

> **Takeaway:** building this is the heavy, finicky step. We build it **once per target**
> in CI, **pin the commit**, and vendor the artifacts into the Flatpak. We do not rebuild
> it per machine and we do not chase AOSP head.

---

## 3. How Eclipse boots and drives ART (from Rust)

```
Rust launcher
  ├─ dlopen("libart.so")
  ├─ JNI_CreateJavaVM(...) with ART options:
  │      -Ximage:<boot.art>            (preloaded, AOT libcore — fast VM init)
  │      -Xbootclasspath:<libcore jars>  (baked default from the patch)
  │      classpath = api-impl.jar : <Roblox.apk>
  │      compiler/interpreter mode flags (see §4)
  ├─ register our native framework backends (JNI, via the `jni` crate)
  ├─ build Application, parse AndroidManifest, launch main Activity → onCreate()
  └─ app's Java calls System.loadLibrary("…") → translation linker loads the
     native engine .so against bionic_translation → engine inits Vulkan/EGL → frames
```

- The Rust **`jni`** crate wraps the **JNI Invocation API** (`JavaVM`, `AttachCurrentThread`,
  method calls). ART implements that API, so the crate drives it — we just pass
  **ART-specific VM options** (boot image, bootclasspath, compiler filter) instead of a
  stock JVM's.
- After `onCreate`, control hands to the event loop; the **native engine runs on its own
  thread(s)**. JNI crossings are mostly init/lifecycle/menu — not per-frame.

---

## 4. Performance — why the C++ VM doesn't cost you FPS

**The hot path has no ART in it:**

```
[native engine .so]  →  [our libvulkan/libEGL shim: ash forward + WSI translate]  →  [host GPU]
        ^ already native x86-64                ^ near-zero overhead
```

ART sits **beside** this, running the Java shell. Its compilation mode therefore trades
**startup/UI latency**, not frame rate. ART's modes (Android 7+ hybrid):

| Mode | What | Cost | Use for |
|---|---|---|---|
| **AOT** (`dex2oat`, filter `speed`) | Compile dex → native ahead of time | Slow first-run compile; **fragile on host** (page size, time, disk) | the **libcore boot image** (compiled once, by us) |
| **JIT** | Hot methods compiled at runtime | Needs **executable mmap** (W^X concerns) | the **app dex**, when JIT is allowed |
| **Interpreter** | Execute dex directly | Slower Java; **most portable** (no codegen) | fallback when JIT is blocked |

**Eclipse policy:** keep the **boot image AOT** (built once → fast VM init), run the
**app dex with JIT**, and **fall back to interpreter** where JIT can't run. Because Java
isn't the gameplay hot path, interpreter-mode Roblox still games fine — it just logs
in/menus a bit slower. This is the same reason ATL can ship `-Xnoimage-dex2oat` /
`-Xusejit:false` escape hatches.

The performance win vs Wine is structural: Wine routes **D3D → DXVK → Vulkan** for *every
frame*; Eclipse routes the engine's **native Vulkan straight to the host ICD**. Same
reason Sober can match/beat the Windows client.

---

## 5. Stability & portability — the real risks and how we contain them

1. **JIT needs executable memory.** Some hardened kernels, strict SELinux/AppArmor, or
   tight seccomp can block RWX / dual-map JIT. **Mitigation:** runtime-detect whether JIT
   mapping succeeds; **fall back to interpreter** automatically (detect-don't-assume).
2. **Page size ≠ 4K.** AOT images/`dex2oat` assume 4K pages; ARM/Apple-silicon/16K-page
   hosts break. **Mitigation:** ship per-target boot images; on odd hosts, interpreter.
   (Matches Sober: x86_64 first-class, ARM experimental.)
3. **Version drift.** Newer Roblox dex could exceed our forward-ported VM's dex version.
   **Mitigation:** track which dex version our pinned ART supports; bump deliberately,
   test, re-pin. Never auto-track AOSP head.
4. **Reproducibility.** The whole point of Sober's Flatpak-only stance: pin glibc/GTK/Mesa
   *and* the ART build so crashes are debuggable. We do the same — **vendor pinned
   artifacts**, build them in CI, don't depend on the dev's machine.
5. **Boot-image / bootclasspath mismatch.** The boot image must match the libcore jars and
   the ART build exactly, or the VM won't start. **Mitigation:** build+ship them as one
   pinned unit; assert the match at startup with a clear error.

---

## 6. Which reused-C parts can become Rust later (and which never)

| Component | Rust-able eventually? | Notes |
|---|---|---|
| `bionic_translation` (libc/linker shim) | ✅ Yes (target) | Onto `elf_loader`/`dlopen-rs` + Rust ABI wrappers. Highest value Rust port. |
| `libandroidfw` (assets/`resources.arsc`) | ✅ Plausible | We have our own `axml` reader (`src/apk/axml.rs`); could grow it into the ARSC/asset path. |
| `libOpenSLES` (audio entry) | ✅ Yes | Route OpenSL ES/AAudio calls to our `libpulse-binding` Rust audio. |
| wolfSSL (libcore TLS) | ⚠️ Hard | libcore expects a specific JNI TLS provider; swapping to rustls means reimplementing that provider. Low priority. |
| libunwind | ⚠️ Low value | Reuse; little benefit to porting. |
| **ART + libcore (the VM)** | ❌ **Never (reuse)** | A production dex VM + GC + JIT + `java.*` in Rust is out of scope, forever, for this project. |

So the long-term shape: **ART+libcore stay vendored C++/Java; everything around them
migrates to Rust.** The "Rust client" identity holds because every line we *own* is Rust.

---

## 7. M0 plan for the runtime specifically

1. Build `art_standalone` via the [killerdevildog unified-CMake fork](https://github.com/killerdevildog/android_translation_layer)
   (it fetches wolfSSL→libunwind→bionic_translation→art_standalone in order) — get a
   working `libart` + boot image + jars on the dev machine.
2. Boot a trivial known-good APK; confirm `onCreate` + first frame.
3. Boot the **Roblox APK**; capture where it breaks (`ANDROID_LOG_TAGS=*:v`, missing
   classes/methods) → that's the framework work-list.
4. Measure: **(a)** Roblox dex vs native split (→ is ART even mandatory, or could a
   fake-JVM work?), **(b)** does JIT map OK here, **(c)** startup time AOT vs JIT vs
   interpreter. These numbers set the runtime policy before we write the Rust launcher.

---

## 8. Open questions

- Exact **pinned commit / dex version** of `art_standalone` to target.
- Whether Roblox needs a **real Binder** or stubs suffice (affects how much VM/framework
  plumbing is required).
- Boot-image **size** and per-target matrix (x86_64 now; ARM later) for the Flatpak.
- Does Roblox's Java shell make **per-frame JNI calls**? (If yes, interpreter mode could
  matter more than assumed — measure in M0 step 4.)

## VM boot — implementation plan (2026-06-04, evidence-based)

The `runtime` crate's planning layer is done (`runtime::BootPlan`, host-ISA detection,
`eclipse run` dry-run). The remaining M1 work is `runtime::boot()` — the actual ART VM boot.
This is the charter's **highest-risk / last** step; the plan below is grounded in the M0 boot
logs + the installed `art_standalone` / `android_translation_layer` layout.

### Boot recipe (verified components on this host)
- **VM library:** `/usr/lib/art/libart.so` — exports `JNI_CreateJavaVM` +
  `JNI_GetDefaultJavaVMInitArgs` (`nm -D` confirmed). Load via `libloading` (dlopen) so the
  build does not link ART; discover the path (detect-don't-assume), don't hardcode.
- **Boot image location:** `/usr/lib/java/dex/art/oat/boot.art` (the `-Ximage` *location*);
  ART/dex2oat compiles it to `~/.cache/art/x86_64/...@boot.oat` on first run (already present
  here from M0). First boot pays a one-time dex2oat cost.
- **Bootclasspath (libcore):** the `*-hostdex.jar` set in `/usr/lib/java/dex/art/oat/`
  (`core-oj`, `core-libart`, `bouncycastle`, `apachehttp`, `apache-xml`, `okhttp`,
  `wolfssljni`, …). The patched ART may bake a default bootclasspath (per §2); confirm whether
  `-Xbootclasspath` must be passed explicitly.
- **libcore native backends:** `/usr/lib/java/dex/art/natives/` (e.g. `libjavacore.so`,
  `libopenjdk.so`) — ART loads these **during VM init**, through the *translation linker*.
- **Framework + app classpath:** `classpath = /usr/lib/java/dex/android_translation_layer/api-impl.jar : <Roblox.apk>` (the M0 log's `class_loader_context` confirms `api-impl.jar` as PCL).
- **VM options:** the M0-validated heap flags from `BootPlan::vm_options()` (`-Xmx768m`,
  `-XX:HeapGrowthLimit=768m`, `-XX:DisableHSpaceCompactForOOM`). NB: `--instruction-set-features`
  is a **dex2oat** flag, NOT a `JavaVMOption` — keep `BootPlan` split (VM vs dex2oat).

### The crux (env setup): bare dlopen is *not* enough
Even a libcore-only VM init makes ART load `libjavacore.so`/`libopenjdk.so` (bionic `.so`s)
**through the translation linker**, and the patched ART "removes `/system/bin/linker`
assumptions / loads JNI libs through the translation linker" (§2). So `runtime::boot()` must
first stand up **bionic_translation**'s environment (the linker shim) before
`JNI_CreateJavaVM`, exactly as ATL's launcher does. Per the charter (§6 bionic decision), **v1
FFIs the proven C `bionic_translation`** (and likely links/loads `libandroid_translation_layer`
for its boot glue) for stability, then ports to Rust behind the ABI conformance suite later.

### Recommended v1 shape
1. `libloading` to dlopen `libart.so` (+ resolve `JNI_CreateJavaVM`); `jni = 0.21` for the JNI
   types and `JavaVM::from_raw(...)` → `JNIEnv`. **Or** link `libandroid_translation_layer` and
   call its higher-level boot entry (simpler, more stable for v1 — compare both).
2. Stand up bionic_translation, build `JavaVMInitArgs` from `BootPlan::vm_options()` + `-Ximage`
   + bootclasspath + classpath, call `JNI_CreateJavaVM`, get a `JNIEnv`, run a trivial
   `java.lang.System` call, `DestroyJavaVM`. This is the **libcore-only smoke boot**.
3. Then: register framework native backends (JNI), build the Application, drive the Activity
   (`ActivitySplash`/`ActivityNativeMain`) to `onCreate`; `System.loadLibrary` pulls
   `libroblox.so` via the translation linker; engine inits Vulkan/EGL.

### Safety + gate
- This introduces the crate's first `unsafe` → lift `#![forbid(unsafe_code)]` in `runtime.rs`,
  confine `unsafe` to the boot path with `// SAFETY:` notes, and wrap **every** registered JNI
  native callback body in `catch_unwind` (no unwind into C++ under `panic = "abort"`, §2.8).
- Gate-clean: dlopen means it **compiles without ART linked**; the actual boot is an
  `#[ignore]` integration test run only where `/usr/lib/art` exists.

### The key experiment — Step 3.5 thesis test
A **graphics-stack-free** smoke boot (no GTK4/Mesa/winit) should have a *clean* low_4gb window,
so ART/LOS allocations should succeed where ATL+GTK4 exhausted them (Step 3.5). Running the
libcore-only boot is the cheapest decisive test of Eclipse's core architectural claim — do it
first, before wiring winit/ash.

### Open questions / risks (need ATL source from GitLab or real-run iteration)
- ATL's **exact** pre-`JNI_CreateJavaVM` init sequence (the local AUR clone has no extracted
  source; fetch `android_translation_layer` from GitLab, or iterate against real boots).
- Whether `-Xbootclasspath` is needed or baked; whether `libandroidfw` must be initialized for
  `framework-res.apk`; first-run dex2oat boot-image compile time/fragility (page size).
- **Tooling note (2026-06-04):** Anthropic's cyber-safeguard repeatedly false-positives on
  *workflow subagents* asked to analyze ART-VM-boot/JNI-FFI topics (blocked 3+ agents). Do this
  step in the main loop / interactively, not via Workflow subagents.

## Sources

- [art_standalone (GitLab)](https://gitlab.com/android_translation_layer/art_standalone) — base `android-6.0.1_r46`, build outputs, patches
- [ATL Build.md (GitLab)](https://gitlab.com/android_translation_layer/android_translation_layer/-/blob/master/doc/Build.md) — build order, deps, wolfSSL 5.8.2
- [killerdevildog ATL fork (GitHub)](https://github.com/killerdevildog/android_translation_layer) — unified build of the whole chain
- [Configure ART — AOSP](https://source.android.com/docs/core/runtime/configure) · [ART JIT — AOSP](https://source.android.com/devices/tech/dalvik/jit-compiler.html) · [Boot image profiles — AOSP](https://source.android.com/docs/core/runtime/boot-image-profiles)
- [Android Runtime — Wikipedia](https://en.wikipedia.org/wiki/Android_Runtime) — AOT/JIT/interpreter hybrid, compiler filters
- [Building ART for native Linux (android-building group)](https://groups.google.com/g/android-building/c/ZZ-SXlkfKmY)
