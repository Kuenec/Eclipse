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

> ✅ **UPDATE 2026-06-04 — the libcore boot is IMPLEMENTED and the thesis is VALIDATED.**
> `runtime::boot()` `dlopen`s `libart.so` and calls `JNI_CreateJavaVM` (with `-Ximage` + the M0
> heap flags) to bring up a **libcore** ART VM — it returns `JNI_OK` from a bare, graphics-free
> Rust process (`eclipse run <apk>`, EXIT 0), with **no low_4gb exhaustion**. This proves the
> Step 3.5 thesis. **The "crux" below was disproven:** a bare `dlopen` IS enough for a libcore
> boot — `libart.so` is a host (glibc) build whose libcore native backends are host libs, and it
> pulls the translation linker (`NEEDED libdl_bio.so.0`) transitively, which self-initializes; no
> explicit `bionic_translation` setup is needed until the *app's* `libroblox.so` is loaded.
> Impl: `libloading` + `jni-sys`. Caveat: boot from the process **main thread** (the cargo-test
> harness aborts via `scoped_thread_state_change`). Remaining: reach Roblox `onCreate` (app
> classpath + Activity + `System.loadLibrary` + winit/ash). The recipe below stays accurate for
> paths/options; treat the "crux/recommended v1" as superseded by the simpler reality.

The `runtime` crate's planning layer is done (`runtime::BootPlan`, host-ISA detection,
`eclipse run`). The libcore `runtime::boot()` is done (above); the remaining work is driving ART
to Roblox's `onCreate`. The plan below is grounded in the M0 boot logs + the installed
`art_standalone` / `android_translation_layer` layout.

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

## onCreate JNI recipe (confirmed) — 2026-06-04

> **Status:** Spec'd and grounded for implementation. The libcore boot + Roblox classpath
> + native-lib path + the `!Send`/`!Sync` `Vm` handle are all DONE (§5 of `AGENTS.md`); this
> section pins the **next** increment: the JNI call sequence that drives ART from
> `JNI_CreateJavaVM` to Roblox's `onCreate`. Sources read 2026-06-04: ATL's
> `src/main-executable/main.c` (onCreate recipe) + `api-impl/android/{content,app}/*.java` +
> `api-impl-jni/handle_cache.c`; the `jni` crate API on **docs.rs/jni** (Context7 does not
> index `jni`); `~/eclipse-m0/framework-worklist.txt`.

### `jni` crate to add (verified against docs.rs/jni)

- **Pin `jni = "0.22"`** (current 0.22.4; pin to `0.22.*` for stability). It wraps the raw
  `jni-sys 0.4` already in the tree — compatible, same FFI layer. Add it alongside `jni-sys`
  (do **not** drop `jni-sys`: `boot()` still uses its raw invocation types).
- **Obtain the handle, don't re-create the VM.** `boot()` already holds the live VM as the
  `!Send`/`!Sync` `runtime::Vm` (raw `*mut JavaVM`). Wrap that pointer with
  `unsafe { jni::JavaVM::from_raw(ptr) }` — **safety contract:** the pointer must come from a
  live `JNI_CreateJavaVM` and support JNI ≥ 1.4 (it does; `from_raw` only null-checks).
- **Get an `Env` on the attached main thread.** There is **no** standalone `get_env()` in
  0.22. Use `vm.attach_current_thread(|env| { … })` — the callback receives `&mut Env<'_>`.
  The main thread is already JNI-attached after `JNI_CreateJavaVM`, so this is cheap (no real
  attach). `Env` is `!Send`/`!Sync` (tied to this thread) — matches the pinned-main-thread
  model (`Vm` is `!Send`/`!Sync`; boot ART on main, run winit's loop on main, call JNI from
  inside event-loop callbacks; never `AttachCurrentThread` a cross-thread env).
- **Call shapes:** `find_class("java/lang/String")` (slashed internal name, **not** dotted) →
  `JClass`; `call_static_method(class, name, sig, &[JValue])` and
  `call_method(obj, name, sig, &[JValue])` → `Result<JValueOwned>`. `JValue`/`JValueOwned` is
  an enum (`Object/Byte/Char/Short/Int/Long/Bool/Float/Double/Void`); unwrap a return via
  `.l()`/`.i()`/`.j()`/`.z()`/`.v()` etc. Errors are typed `Result` (`JavaException`,
  `MethodNotFound`, `NullPtr`) — no panics.

### The step-by-step recipe (from ATL `main.c` + `api-impl` sources)

Each row is a JNI call from the held `Env` on the main thread, in order. The host window is
passed as a **`long` / `jlong` (intptr_t)** — a raw handle, **not** an `android.view.Surface`
object. (ATL passes a `GtkWidget*`; Eclipse passes its winit window handle — exact handle
type is the one UNCONFIRMED below.)

| # | Java class (internal name) | Method | JNI descriptor | Arg types | Static/Instance | Purpose |
|---|---|---|---|---|---|---|
| 1 | `android/content/Context` | `createApplication` | `(J)Landroid/app/Application;` | `jlong native_window` | **static** | Build + init the `Application`, attach it to the window handle, set base context from the parsed manifest. Returns the `Application` used in step 3. |
| 2 | `android/content/ContentProvider` | `createContentProviders` | `()V` | none | **static** | Instantiate all manifest-declared `ContentProvider`s (same-process) and call their `onCreate`. |
| 3 | `android/app/Application` | `onCreate` | `()V` | none (on the obj from step 1) | **instance** | `Application.onCreate()` lifecycle — app's Java shell self-init before the Activity. |
| 4 | `android/app/Activity` | `createMainActivity` | `(Ljava/lang/String;JLjava/lang/String;)Landroid/app/Activity;` | `String className`, `jlong native_window`, `String uri` | **static** | Instantiate the launcher Activity (`className`, or `null` → auto-resolve manifest MAIN/LAUNCHER), create its Window on the window handle, build the Intent. |
| 5 | `android/app/Activity` | `onCreate` | `(Landroid/os/Bundle;)V` | `Bundle savedInstanceState` (`null` first launch) | **instance** | Activity `onCreate(Bundle)` — completes init; Roblox's shell sets up UI and calls `System.loadLibrary("roblox")`. |

- **Bootstrap class:** ATL does **not** hardcode one — it resolves the manifest MAIN/LAUNCHER
  intent-filter. For this APK that is `com.roblox.client.startup.ActivitySplash`; ATL's M0
  boots `-l com/roblox/client/ActivityNativeMain` to **bypass the splash**. Eclipse passes the
  chosen class to step 4 (or `null` to auto-resolve). `createMainActivity` is `private static`
  in the Java source but is invoked from native via `CallStaticObjectMethod` (JNI may call
  private methods).
- **`System.loadLibrary("roblox")`** fires from inside step 5; per the framework work-list it
  then loads `libroblox.so` via the bionic translation linker, which needs the NDK shims
  `libmediandk.so` / `libOpenMAXAL.so` (the separately-tracked native-shim increment).

### catch_unwind requirement (mandatory, §2.8 + CLAUDE.md)

Release keeps `panic = "abort"`, so a Rust panic unwinding into ART's C++ is UB. **Wrap every
Rust-side JNI/`extern "C"` callback body in `std::panic::catch_unwind`** (registered framework
native backends, and any closure run under `attach_current_thread`). `jni-rs` itself installs
`catch_unwind` on `EnvUnowned::with_env` and reports panics via its `Outcome`/`ErrorPolicy`
path — but the project's standing rule still applies: wrap defensively at every boundary; never
let a panic cross into C++.

### UNCONFIRMED — the implementer must resolve these at implement time

These are **not** yet proven (CLAUDE.md: known vs suspected). Resolve before/while coding,
primarily by reading the installed `api-impl.jar` (e.g. `javap -s` on the compiled classes)
and by a dev-host `eclipse run` — the cargo test harness aborts ART (worker-thread
`scoped_thread_state_change`), so the sequence is validated only from `main()`:

- **Whether step 5 (`Activity.onCreate`) is called directly from JNI or indirectly via the
  event loop.** ATL's `main.c` excerpt shows `createMainActivity` → `activity_start(activity)`
  (which drives `onStart`/`onResume` and input registration); the exact point `onCreate` fires
  may be inside `createMainActivity`/`internalCreateActivity` rather than a separate JNI call.
- **Looper/MessageQueue ordering:** whether `Looper.prepareMainLooper` / `Looper.loop` must run
  before or after this sequence (ATL calls `prepare_main_looper` early).
- **The exact window-handle type Eclipse passes as the `jlong`** (winit raw window handle vs a
  Vulkan/EGL surface vs raw Wayland/X11 id). ATL passes `GtkWidget*`; Eclipse's winit will
  differ — this is the framework/Surface design (component-map F), not a fixed value.
- **Whether `createMainActivity`'s compiled signature/visibility in `api-impl.jar` matches the
  source** (`javap -s` the jar to confirm descriptors before binding them).

## Non-GTK api-impl backing — design (2026-06-04)

> **Status:** Design, grounded for the next framework increment. Resolves the "framework
> frontier" crux (`AGENTS.md` §5 next-actions): the `jlong`-window/onCreate steps are gated on
> the backing for `api-impl.jar`'s `native` methods being **GTK-coupled**. This section pins the
> non-GTK replacement and the smallest first step. Companion to "onCreate JNI recipe (confirmed)"
> above. Sources read 2026-06-04: ATL's `api-impl/android/{content,view}/*.java` +
> `api-impl-jni/*.c` (the JNI/GTK backing) + `main-executable/main.c`; `readelf -d` on the
> installed `libtranslation_layer_main.so`; the winit 0.30 `raw-window-handle` API.

### Approach + rationale (why a native backing, not a Java fork)

Build **Eclipse's own non-GTK native backing** for `api-impl.jar`'s declared `native` methods —
a replacement for ATL's GTK-linked `libtranslation_layer_main.so` — and bind it into ART, keeping
ATL's `api-impl.jar` unchanged on the classpath. Evidence forcing this:

- **ATL binds natives by symbol name (no `RegisterNatives`).** There is no `JNI_OnLoad`/
  `RegisterNatives` anywhere in ATL; each native lazy-binds at its **first call** by the
  `Java_<class>_<method>` symbol name, from whatever `.so` is on `java.library.path`.
- **The GTK coupling is in the C backing, not the Java.** The installed
  `.../android_translation_layer/natives/libtranslation_layer_main.so` exports every `api-impl`
  native **and** is directly GTK-4/GDK/pango/webkitgtk-linked (`readelf -d` shows the GTK-family
  `NEEDED` entries). The moment ART resolves any of those natives, GTK loads and re-crowds
  low_4gb — exactly the Step 3.5 blocker winit avoids. `api-impl.jar` itself is **GTK-free**: it
  stores the `jlong` opaquely and only *declares* `native` methods.

So the durable, surgical fix (CLAUDE.md root-cause, smallest change) is to **supply Eclipse's own
non-GTK symbols for those exact `Java_*` names** and **remove ATL's GTK natives dir from
`java.library.path`** so Eclipse's symbols are the ones bound. Forking `api-impl.jar` is rejected
(large, non-surgical, duplicates a vendored artifact, and **no Java change is needed** to reach
onCreate); a hybrid Java-patch is rejected for the same reason.

### Minimal native-method contract

Two tiers of name-based `Java_*` symbols Eclipse defines (non-GTK) and binds via the `jni` crate's
`register_native_methods` (or by being the only matching symbol on `java.library.path` once ATL's
GTK natives dir is dropped):

**Tier A — reach `Application.onCreate` against a pure-Java APK.** Steps 1–3 of the confirmed
recipe (`Context.createApplication(J)` → `ContentProvider.createContentProviders()` →
`Application.onCreate()`) are **pure Java** — no native call; the `jlong` window is only stored as
a field. The only natives actually invoked up to `onCreate` are the **two** in `Context`'s static
initializer (runs at class-load):

| `Java_*` symbol | Descriptor | Eclipse (non-GTK) behavior |
|---|---|---|
| `android/content/Context.native_updateConfig` | `(Landroid/content/res/Configuration;)V` | Set `Configuration.screenWidthDp`/`screenHeightDp` from a winit `MonitorHandle` if a window exists, else safe constants. ATL queries GDK here — Eclipse does **not**. |
| `android/content/Context.native_get_apk_path` | `()Ljava/lang/String;` | Return the APK path as a `jstring`. Trivial, no GTK. |

**Tier B — present a surface / drive a real Activity.** Step 4 `Activity.createMainActivity(String,
J,String)` builds a `Window` (→ `Window.set_native_window` → `set_jobject`) and calls
`setTitle`/`setLayout`. Adds (all GTK-free, winit-backed):

| `Java_*` symbol | Eclipse (non-GTK) behavior |
|---|---|
| `android/view/Window.set_jobject` | Store the Java `Window` ref keyed by handle in a Rust-side map (replaces ATL's `g_object_set_data`). |
| `android/view/Window.set_title` | winit `Window::set_title`. |
| `android/view/Window.set_layout` | winit set size. |
| `android/view/Window.set_widget_as_root` | Attach view → surface (the real render binding; a stub initially). |
| `android/view/Window.take_input_queue` | winit input bridge. |
| `android/os/MessageQueue.nativeInit` / `nativePollOnce` | Looper/event-loop integration. |

`Activity.nativeStartActivity`/`nativeResumeActivity` are pure state (no GTK); they need no
Eclipse-specific work except dropping ATL's GTK window-close in `nativeFinish`.

### Render stack + window-handle mapping

Render surface is **ash/Vulkan-first, EGL fallback** (`docs/tech-selection.md` §C; the 2026-06-04
perf decision; `config.use_opengl=false` defaults to Vulkan). Eclipse does **not** render — it
provides the `libvulkan`/`libEGL` the engine links and forwards to the host ICD, translating WSI.

The `jlong` passed to `createApplication(J)`/`createMainActivity` is an **Eclipse-owned
`intptr_t`** — **not** a `GtkWidget*` and **not** raw-window-handle bytes. Keep a Rust-side
registry of Eclipse window objects and pass the registry handle (or `Box::into_raw` of an Eclipse
`WindowState`) as the `jlong`; Eclipse's own natives cast it back to the `WindowState`, which holds
the winit `Window` and later the `ash` `vk::SurfaceKHR`/EGL surface created from
`window.window_handle()`. Context7-confirmed (winit 0.30): `window.window_handle()?.as_raw()` →
`RawWindowHandle::Wayland(WaylandWindowHandle{ surface })` or `Xlib(XlibWindowHandle{ window, .. })`;
ash-window consumes the matching display+window handle to build the `VkSurface`. The engine never
sees the winit handle directly — it sees an Android `ANativeWindow`/`Surface` that Eclipse's WSI
shim backs with the winit surface. **For Tier A no surface is needed:** the `jlong` can be any
stable non-null Eclipse handle because steps 1–3 only store it.

### Smallest first implementation increment

Provide Eclipse's own non-GTK backing for **exactly the two Tier-A natives**, against the pure-Java
`demo_app.apk`, deferring all Window/Surface natives:

1. In `framework.rs`, after `from_raw` + attach, call `register_native_methods` to bind
   `android/content/Context` natives `native_updateConfig` (`(Landroid/content/res/Configuration;)V`)
   and `native_get_apk_path` (`()Ljava/lang/String;`) to two `extern "C"` Rust fns (both
   `catch_unwind`-wrapped, §2.8). Register **before** the `find_class` that triggers `Context`'s
   static initializer so the binding wins over lazy lookup.
2. In `runtime.rs` `library_path_option`, stop putting ATL's GTK-linked natives dir on
   `java.library.path` for this path (so `libtranslation_layer_main.so`/GTK is never `dlopen`ed);
   keep only the app-lib dir (none for demo_app).
3. Extend `drive_application_lifecycle` past `BridgeProven`: call step 1 `Context.createApplication(J)`
   with an Eclipse-owned non-null `jlong`, step 2 `ContentProvider.createContentProviders()`, step 3
   `Application.onCreate()` — all pure Java, no further natives.
4. Verify on the dev host: `cargo run -- run ~/eclipse-m0/atl_test_apks/demo_app.apk` boots ART,
   registers the 2 natives, runs `Context` static-init + steps 1–3, logs "Application.onCreate
   reached", opens the window, exits 0 — with **no GTK** in the process map (`/proc/self/maps`:
   no `libgtk-4`).

### UNCONFIRMED — resolve at implement time (dev-host `eclipse run` + `javap -s`)

- Whether `jni` 0.22.4 `register_native_methods` reliably **intercepts** ATL's name-based
  lazy-bound natives, and the registration **ordering** vs `Context`'s static init. If
  `RegisterNatives` needs the class already loaded (which triggers the static init that *calls*
  the natives), the symbols must instead be resolvable at first call — i.e. supply them as a real
  Eclipse `.so` on `java.library.path`. Validate which path works on the dev host.
- The **complete set** of natives `Context`'s static initializer + `PackageParser` transitively
  invoke for `demo_app.apk` beyond these two (enumerate by running until no `UnsatisfiedLinkError`;
  stub each GTK-free). Dropping ATL's natives dir could surface more framework natives than the two
  identified.
- The **compiled** `api-impl.jar` signatures/field names (`native_updateConfig`,
  `native_get_apk_path`, `Configuration.screenWidthDp`/`screenHeightDp`) — verify via `javap -s` on
  the installed jar, not the source (compiled jar could differ).
- Exact winit 0.30.x point release and whether `window_handle()` is used from the concrete
  `resumed()` API or the trait-object API in this tree.
- Tier B only: the precise winit `RawWindowHandle` variant → ash-window `VkSurface` (Wayland surface
  vs X11 XID) and how the engine's `ANativeWindow`/`vkCreateAndroidSurfaceKHR` maps onto it —
  deferred, not needed for `onCreate`.
- Whether a real (non-demo) Roblox APK reaches further natives before the bionic NDK-shim frontier
  (separate work-stream).

## Non-GTK Window/Surface backing — design (2026-06-05)

> **Status:** Design, grounded for the framework increment **after** the dev-host steps-1–3
> validation. Extends "Non-GTK api-impl backing — design (2026-06-04)" Tier B (steps 4–5) with the
> concrete handle/registry/surface model. Sources read 2026-06-05 (api-impl Java only — not the
> bionic linker C): ATL `api-impl/android/view/Window.java` (natives at L184–188; `set_native_window`
> L58–60; `set_widget_as_root(native_window, decorView.widget)` L78; `decorView = new FrameLayout(context)`
> L46) and `api-impl/android/view/View.java` (`public long widget; // pointer` L888; `widget =
> native_constructor(context, attrs)` L965, "will create a custom GtkWidget" L1166; the
> `native_constructor`/`native_addView`/`native_measure`/`native_layout`/`native_drawContent` cascade);
> the existing `framework.rs` Context-native pattern (`EnvUnowned::with_env` + `resolve::<LogErrorAndDefault>`
> + `register_native_methods`); winit 0.30 `raw-window-handle` re-exports (Context7).

### Owned-handle contract (the `jlong` meaning)

The `jlong` window handle passed to `createApplication`/`createMainActivity` is an **Eclipse-owned
registry index into a process-global generational slab — NOT `Box::into_raw` and NOT a raw pointer.**
ATL casts that `jlong` to `GtkWidget*`; Eclipse owns **both sides** of the `jlong`, so it is free to
define its own meaning. Why a registry index over `Box::into_raw(WindowState)`:

1. **Soundness / no UB.** A stale or fabricated `jlong` (the engine, or a buggy framework path) becomes
   a **bounds-checked slot lookup that returns `Err`**, never a wild deref / use-after-free.
   `Box::into_raw` would turn any wrong `jlong` into UB.
2. **`!Send`/`!Sync` respect.** `WindowState` owns the winit `Window` (`!Send` on some platforms) and
   later `ash` surfaces; the registry lives behind a thread-checking accessor so `WindowState` is only
   ever touched on the main thread that owns the VM and the winit loop.

Concrete shape (std-only, **no new dep**): `static WINDOWS: OnceLock<Mutex<…>>` over a small hand-rolled
`Vec<Option<WindowState>>` + freelist (or a `Slab`), keyed by a `u32` index packed into the low bits of
the `i64` `jlong` with a `u32` **generation** in the high bits, so a reused slot with a new generation
**rejects a stale handle**. Allocation returns the packed `i64`; each Window native unpacks
index+generation, locks the registry, validates the generation, and operates on the `WindowState`.
`jlong = 0` stays the reserved null sentinel (steps 1–3 store-only, never look up). For the **first**
increment `WindowState` holds **only metadata** (title/size + a `jni` `GlobalRef`); the live winit
`Window` is reached through the event-loop callback, **not** the registry, so no aliasing with the event
loop's `&mut` access arises yet (see risks).

### Per-native, non-GTK plan (Tier B — Window)

Behavior is Eclipse's own; the **contract** (which natives exist + their roles) is confirmed from
`Window.java`. ATL's analogue is listed for reference, not to reimplement (no GTK).

| `Java_*` native (descriptor) | ATL analogue | Eclipse (non-GTK) behavior |
|---|---|---|
| `Window.set_native_window(long)` | (pure Java) | **No Eclipse native.** `Window.java` L58–60 stores `this.native_window` then calls `set_jobject(native_window, this)`. |
| `Window.set_jobject(long, Window)` *(static)* | `g_object_set_data` weak ref | Look up the slot; store a `jni` `GlobalRef` (or `WeakGlobalRef`, mirroring ATL's `_WEAK_REF`, to avoid pinning the `Window` for GC) of `obj` in `WindowState.jobject` for later input/lifecycle dispatch back to Java. |
| `Window.set_title(long, String)` | `gtk_window_set_title` | `env.get_string` → winit `Window::set_title`; the `JString` releases via the `jni` crate's RAII. |
| `Window.set_layout(long, int w, int h)` | `gtk_window_set_default_size` (`w>0 && h>0`) | If `w>0 && h>0`, winit `Window::request_inner_size(PhysicalSize)` (winit 0.30 returns `Option` — **best-effort**, matching `set_default_size`'s advisory nature). |
| `Window.set_widget_as_root(long, long widget)` | `gtk_window_set_child` | **The render binding.** Attach the View tree's root surface/canvas as window content. **Deferred** to the full-rendering increment (initial stub records widget-as-root in `WindowState`). |
| `Window.take_input_queue(long, Callback, InputQueue)` | `gtk_event_controller_legacy` + `SetLongField native_ptr` | Create an `InputQueueState`, `SetLongField` the `InputQueue.native_ptr` to its registry handle, store `GlobalRef`s of callback+queue for later dispatch from the winit event loop. **Deferred** to the input increment. |

### Surface plan + render-stack decision

- **To reach `Activity.onCreate`, NO surface is needed.** Steps 1–3 only store the `jlong`; step 4
  `createMainActivity` builds the Window+DecorView and (per `Window.java`) calls
  `set_jobject`/`set_title`/`set_layout`/`set_widget_as_root`, but none require a live `VkSurface` to
  return — `set_widget_as_root` can **record** the root without presenting. The engine creates its own
  `VkInstance`/surface via the libvulkan shim **later** (onStart/onResume/first frame), not during
  `onCreate`. So the surface is **decoupled** from reaching `onCreate`.
- **Render stack = ash/Vulkan-first, EGL fallback** — settled, not re-litigated (`config.use_opengl`
  defaults `false` → Vulkan; AGENTS.md 2026-06-04 perf decision; `docs/tech-selection.md` §C). For the
  **first** Window increment **neither `ash` nor EGL is added** (smallest change).
- **Full-rendering path (later):** winit 0.30 re-exports `raw-window-handle`, so
  `window.window_handle()?.as_raw()` → `RawWindowHandle::Wayland{surface}` | `Xlib{window,..}` and
  `event_loop.display_handle()?.as_raw()` → the matching `RawDisplayHandle`; feed both to ash-window
  `create_surface(&entry,&instance,display,window,None)` → `vk::SurfaceKHR` stored in `WindowState`. The
  engine never sees the winit handle: Eclipse's libvulkan shim backs the Android
  `ANativeWindow`/`vkCreateAndroidSurfaceKHR` with the winit-derived host surface (WSI translation —
  component-map F, a separate later increment, gated on the engine actually loading).

### BIGGEST RISK — `set_widget_as_root` is not bindable in isolation

The View hierarchy is **entirely native-handle-backed** in ATL (`View.java` L888 `public long widget;
// pointer`, L965 `native_constructor`). Constructing the Window's `DecorView` (`new FrameLayout(context)`,
`Window.java` L46) calls `View.native_constructor`; `setContentView`/measure/layout/draw call
`native_addView`/`native_measure`/`native_layout`/`native_drawContent`. So reaching `set_widget_as_root`
**requires Eclipse non-GTK backings for the whole View/ViewGroup/FrameLayout `native_*` cascade too** —
a much larger Tier-B surface than the ~6 Window natives, and the real reason **steps 4–5 are the big
M2/M3 build, not a small one.** This must be acknowledged before estimating step 4 as small.

### Smallest first implementation increment

Build the owned-handle registry + bind the GTK-free **Window metadata** natives, deferring
`set_widget_as_root` presentation, `take_input_queue`, the View `native_*` cascade, and **all**
surface/`ash` work — **after** the dev-host steps-1–3 validation surfaces the next `UnsatisfiedLinkError`.
In-harness-compilable (registry + native fns compile and unit-test under `cargo test` with no
display/VM, exactly like the existing Context-native pattern):

1. New `src/framework/window_registry.rs` (or inline in `framework.rs`): the generational-slab registry
   (std-only), `allocate() -> i64`, `with_window(jlong, |&mut WindowState|) -> Result`; pack/unpack
   unit-tested (round-trip, **stale-generation rejection**, `jlong = 0` reserved).
2. Define `set_jobject`/`set_title`/`set_layout` as `extern "system"` `catch_unwind`-wrapped Rust fns
   following the existing `native_get_apk_path`/`native_update_config` pattern (`EnvUnowned::with_env` +
   `resolve::<LogErrorAndDefault>`), registered via `register_native_methods` on `android/view/Window`
   **before** `Window` is first used. `set_title`/`set_layout` operate on `WindowState` metadata for now
   (the live winit `Window` is threaded in by the event-loop increment).
3. A unit test pins the Window native names/descriptors against `Window.java` (mirroring
   `context_native_names_and_sigs_match_context_java`).

**Not in this increment:** driving step 4 end-to-end (blocked on the View `native_*` cascade), surface
creation, input. The dev-host discovery loop then surfaces the View natives as the next
`UnsatisfiedLinkError`s.

### UNCONFIRMED — resolve at implement time (dev-host `eclipse run` + `javap -s`)

- Whether `Activity.onCreate` fires directly from a JNI call or indirectly via
  `createMainActivity`/`internalCreateActivity`/`activity_start` or the Looper (ATL `main.c` shows
  `createMainActivity → activity_start`).
- The **complete** set of natives the View/ViewGroup/FrameLayout construction + `setContentView` cascade
  invokes for the real Roblox APK (enumerate by running until no `UnsatisfiedLinkError`). The Window
  natives are **necessary but not sufficient**.
- Whether `set_jobject` should store a `GlobalRef` or `WeakGlobalRef` (ATL uses `_WEAK_REF`; affects GC
  pinning of the Java `Window`), and how a slab-stored `jni 0.22` `GlobalRef` interacts with attach
  lifetimes.
- Whether `jni 0.22` `register_native_methods` reliably intercepts ATL's name-based lazy binding for
  `android/view/Window`, and whether ATL's GTK natives dir must be dropped from `java.library.path`
  (`library_path_option` also feeds libcore/other framework JNI backends — dropping it wholesale could
  break the boot; resolve which natives must come from ATL vs Eclipse on the dev host).
- Exact winit 0.30.x `request_inner_size` behavior and which handle API (concrete `Window` vs trait
  object) this tree uses in `resumed()`.
- The precise runtime `RawWindowHandle`/`RawDisplayHandle` variant (Wayland vs X11) and the ash-window
  `create_surface` signature for the pinned `ash`/rwh versions (which must match winit's rwh exactly) —
  deferred to the surface increment.
- Whether `jlong = 0` is the only null the framework passes, or if `createMainActivity` is ever called
  with a pre-existing non-null handle from `createApplication` (handle identity across steps 1→4).

## Sources

- [art_standalone (GitLab)](https://gitlab.com/android_translation_layer/art_standalone) — base `android-6.0.1_r46`, build outputs, patches
- [ATL Build.md (GitLab)](https://gitlab.com/android_translation_layer/android_translation_layer/-/blob/master/doc/Build.md) — build order, deps, wolfSSL 5.8.2
- [killerdevildog ATL fork (GitHub)](https://github.com/killerdevildog/android_translation_layer) — unified build of the whole chain
- [Configure ART — AOSP](https://source.android.com/docs/core/runtime/configure) · [ART JIT — AOSP](https://source.android.com/devices/tech/dalvik/jit-compiler.html) · [Boot image profiles — AOSP](https://source.android.com/docs/core/runtime/boot-image-profiles)
- [Android Runtime — Wikipedia](https://en.wikipedia.org/wiki/Android_Runtime) — AOT/JIT/interpreter hybrid, compiler filters
- [Building ART for native Linux (android-building group)](https://groups.google.com/g/android-building/c/ZZ-SXlkfKmY)
