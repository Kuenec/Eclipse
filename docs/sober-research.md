# Sober — How It Works, and How to Rebuild It (Project Eclipse Research)

> **Status:** Research only. No code in this document is a commitment to an
> implementation. Last verified **2026-06-04**.
>
> **Goal of this doc:** Understand, in enough depth to re-implement, how
> [Sober](https://sober.vinegarhq.org/) runs Roblox on Linux, and lay out what an
> open-source, Rust-based, distro-agnostic alternative ("Eclipse") would need to do.

---

## 0. Evidence quality / how to read this

Sober itself is **closed-source**, so a lot of "how Sober works" cannot be read
directly from Sober's code. To stay honest (per project policy), every claim below is
tagged:

- **[CONFIRMED]** — stated by Sober/VinegarHQ docs, the Sober Flatpak manifest, or
  directly observable artifacts.
- **[OPEN-REF]** — not from Sober's own source, but from the **Android Translation
  Layer (ATL)** open-source stack, which uses the *same architecture* and is the best
  public reference for re-implementing the same approach.
- **[INFERRED]** — reasoned from how Android/Linux/the relevant libraries must behave;
  needs confirmation during a prototype.

When this doc says "Sober does X" without a tag, treat it as **[OPEN-REF/INFERRED]** —
i.e. "this is how the technique works; Sober almost certainly does the same, but verify."

> ⚠️ **Important caveat to resolve before building:** Public sources strongly imply
> Sober shares the ATL/bionic_translation architecture, but I did **not** find a primary
> source proving Sober literally forks ATL's code. Treat ATL as "the open reference
> implementation of the same idea," not as "Sober's source." This is the single biggest
> assumption in this document and should be validated early.

---

## 1. Executive summary

- Roblox does **not** ship a Linux client. Until 2024 the Linux community ran the
  **Windows** client under Wine (Grapejuice, then VinegarHQ's **Vinegar**). In early
  2024 Roblox's **Hyperion / "Byfron"** anti-cheat began blocking Wine, killing that
  path for the Player. **[CONFIRMED]**
- Roblox *does* ship an **Android** client, and crucially publishes an **x86-64
  Android** build (not just ARM). **[CONFIRMED]**
- Sober's insight: instead of emulating Android (Waydroid) or translating Windows
  (Wine), **run the Android x86-64 Roblox build natively on the Linux kernel** by
  providing a thin **Android-runtime-on-glibc compatibility layer**. No VM, no second
  kernel, no instruction emulation on x86-64 hosts. **[CONFIRMED]**
- The technique = **Android Translation Layer (ATL)**: reuse AOSP's **ART** (Dalvik/dex
  VM) + **libcore**, reuse the app's own native `.so` files, bridge Android's **bionic**
  libc to the host **glibc** (`bionic_translation`), and **re-implement the Android
  framework** (`android.*`) on top of desktop Linux (**GTK4**, Vulkan/GLES via EGL,
  PulseAudio/PipeWire). **[OPEN-REF]**
- Sober packages this as a **Flatpak** on the **GNOME 50 runtime**, ships prebuilt
  closed binaries (`sober`, `sober_services`, `libloader.so`, `libbadcpu.so`,
  `libmimalloc.so`), and adds Roblox-specific glue: APK fetch/update, FFlags, Discord
  Rich Presence, GameMode, HiDPI, controller/touch modes, OpenGL/Vulkan selection.
  **[CONFIRMED]**

The rest of this document is the long version, ending with a concrete plan and an honest
feasibility assessment for the Rust rewrite.

---

## 2. Context: the three historical approaches to "Roblox on Linux"

| Approach | What it does | Why it's worse than Sober | Status 2026 |
|---|---|---|---|
| **Wine** (Grapejuice, Vinegar) | Translates the **Windows** Roblox client's Win32/DirectX calls to Linux. | Hyperion/Byfron anti-cheat detects/blocks Wine for the **Player**. Heavy DXVK/d3d overhead. | **Player blocked** (~Mar 2024). Vinegar still useful for **Roblox Studio**. **[CONFIRMED]** |
| **Waydroid / full Android emulation** | Boots a complete Android (AOSP) userspace in a container (LXC) and runs the ARM/x86 APK inside it. | Boots an entire second OS: high RAM/disk, slow start, GPU passthrough is fragile (esp. NVIDIA). | Works but heavy; Sober reported better GPU/NVIDIA behavior. **[CONFIRMED]** |
| **Sober / ATL approach** | Runs the **Android x86-64** APK's code **directly on the Linux kernel** via a thin Android-API compatibility layer. | — (this is the current best path) | **Recommended path** since Aug 2024. **[CONFIRMED]** |

Key enabling fact: **Roblox publishes an x86-64 Android build.** On an x86-64 Linux PC
there is therefore **no CPU emulation** — the native engine (`libroblox`-style `.so`) is
already the right instruction set. Sober only has to satisfy the *Android* environment
those binaries expect, not the *ARM* instruction set. (On ARM64 Linux this stops being
free; Sober's ARM support is explicitly experimental/non-production. **[CONFIRMED]**)

---

## 3. The big idea: where do you "cut" the Android stack?

A normal Android app sits on this stack:

```
  App (your APK: dex bytecode + native .so)
  ─────────────────────────────────────────
  Android Java framework  (android.app.Activity, View, Surface, …)   ← android.jar
  ─────────────────────────────────────────
  ART (dex VM) + libcore (java.*, the JDK-ish classes)
  ─────────────────────────────────────────
  Native system services (SurfaceFlinger, AudioFlinger, InputFlinger, Binder, …)
  ─────────────────────────────────────────
  bionic libc / linker  +  HALs  +  Linux kernel (Android flavor)
```

Waydroid recreates **the whole stack** in a container. ATL/Sober instead makes a
**"surgical cut as close to the app as possible"** — between the app and the **Java
framework APIs** — and replaces everything *below the app's own code* with a
desktop-Linux-backed implementation: **[OPEN-REF]**

```
  App (unchanged APK: dex + native .so)
  ─────────────────────────────────────────  ← the cut
  Re-implemented android.* framework   (api-impl.jar)         ← OUR code (Java)
  JNI bridge  (libtranslation_layer_main.so)                  ← OUR code (C/Rust)
  bionic→glibc shim  (libandroid.so / bionic_translation)     ← OUR code (C/Rust)
  ─────────────────────────────────────────
  Reused AOSP: ART + libcore  (run the dex)                   ← REUSED, modified to use system libs
  ─────────────────────────────────────────
  Desktop Linux: GTK4, EGL/Vulkan/GLES, PulseAudio/PipeWire, glibc, Linux kernel
```

Consequence: you **keep the app's bytecode and native code unchanged**, you **reuse
ART+libcore** (because re-writing a production dex VM is insane), and you **write** the
parts that translate Android concepts → Linux concepts. That "written" part is where all
the work — and all of a Rust rewrite's surface area — lives.

---

## 4. Component-by-component deep dive

### 4.1 The Roblox Android APK (the input)

An APK is a ZIP containing, relevant parts: **[OPEN-REF/INFERRED for Roblox specifics]**

- `AndroidManifest.xml` (binary XML) — package id (`com.roblox.client`), the launcher
  **Activity**, permissions, `minSdkVersion`/`targetSdkVersion`, and which native lib is
  the NativeActivity entry (if any).
- `classes.dex` (one or more) — the **Dalvik bytecode** of the Java/Kotlin client. For
  Roblox this includes the bootstrapping/login/UI/in-game-menu code; community RE notes
  reference classes like `com/roblox/client/ActivityNativeMain`.
- `lib/x86_64/*.so` — the **native engine**: the actual game/render/physics/networking
  code compiled with the Android **NDK** against **bionic**. This is the performance-
  critical, already-x86-64 code.
- `resources.arsc`, `res/`, `assets/` — resources/assets loaded by `libandroidfw`.
- `META-INF/` — signing (v1) ; plus APK Signature Scheme v2/v3 blocks in the ZIP.

**Roblox is a hybrid app:** a Java/Kotlin shell (needs a real **ART**) that loads a large
**native engine** (`.so`, needs **bionic compatibility**). That hybrid nature is exactly
why a "fake JVM / native-only" shortcut (apkenv-style) is *not* sufficient for Roblox —
you need both halves. **[INFERRED — verify by inspecting a current APK's dex surface]**

> **Acquisition:** Sober has the user/app obtain a Roblox Android APK (historically from
> mirrors like APKMirror; later versions fetch a compatible build automatically from
> "trusted sources"). Sober does **not** redistribute the APK itself. **[CONFIRMED]**

### 4.2 `bionic_translation` — the libc/linker bridge (the hard core)

Android native `.so`s are linked against **bionic** (Android's libc/libm/libdl), **not**
glibc. Bionic and glibc are **ABI-incompatible**, so you cannot just `dlopen()` an
Android `.so` on a normal Linux system. This shim is the heart of the whole thing.
**[OPEN-REF, corroborated by `bionic_translation` and the older `android2gnulinux`/`apkenv`]**

What it has to solve:

1. **A bionic-aware dynamic linker.** glibc's `ld.so` won't load bionic `.so`s
   correctly. ATL/relatives use a **custom linker derived from AOSP's `linker`**
   (lineage traceable through `apkenv`). It resolves the app's `DT_NEEDED` against a set
   of **shim libraries** rather than the real Android system libs.
2. **Shimmed Android system libs.** The app expects `libc.so`, `libdl.so`, `libm.so`,
   `liblog.so`, `libandroid.so`, `libGLESv2.so`, `libEGL.so`, etc. The shim provides
   these names, where each exported symbol either:
   - **forwards** to glibc when semantics match (`malloc`, `memcpy`, most of libm), or
   - is **re-implemented** when bionic has it and glibc doesn't (or behaves differently).
3. **`bionic_`-prefixed core functions.** Functions whose **struct layouts or ABI
   differ** between bionic and glibc must be wrapped, not forwarded. Classic landmines:
   - **`FILE*` / stdio:** bionic's `FILE` is a different struct than glibc's — you cannot
     hand a glibc `FILE*` to bionic code. Needs `bionic_fopen`/`bionic_fread`/… wrappers.
   - **`pthread_*` & `pthread_t`:** different sizes/semantics; bionic folds pthread into
     libc. Thread creation, mutexes, TLS keys must be wrapped.
   - **TLS model:** bionic reserves specific **TLS slots** (and historically a fixed TLS
     layout / `__get_tls`). Native code may read TLS slots directly. The shim/linker must
     set up a bionic-compatible TLS area per thread. **[INFERRED — historically the
     nastiest part; verify against current bionic_translation]**
   - **`errno`:** bionic vs glibc differ in how/where `errno` lives.
   - **`setjmp`/`longjmp`, `jmp_buf` size**, signal structs, `dirent`, `stat`,
     `dl_iterate_phdr`, `__cxa_*` C++ ABI helpers, locale, `getauxval`.
   - **`dlopen`/`dlsym`/`dlclose` (libdl):** must route through the **custom linker**, so
     these are explicitly `bionic_`-prefixed and call into the shim's loader, not glibc's.
4. **Android-only syscalls/ioctls** invoked by native code: `ashmem`/`memfd` for shared
   memory, `ioctl`s for graphics buffers (`gralloc`/`ION`/DMA-BUF), `eventfd`/`timerfd`
   patterns, `__system_property_get` (the Android "properties" store), and **Binder**
   (`/dev/binder`) — which has **no Linux equivalent** and must be **emulated or stubbed**.

This component, written in C today, is **the most security- and correctness-sensitive
layer** and the one most worth doing carefully in a rewrite.

### 4.3 ART (the dex VM) + libcore — reused, not rewritten

To run the Java/Kotlin half you need to execute **dex bytecode**. ATL builds a
**standalone ART** (`art_standalone`) plus **libcore** (the `java.*` / `dalvik.*` runtime
classes), **modified to use system-provided libraries where possible** rather than
Android's. The launcher boots a Dalvik/ART VM with a classpath of *(framework jar + the
app's APK)* and then drives the Android lifecycle. **[OPEN-REF]**

Practical notes from the open stack:

- Build order in the ATL CMake fork is:
  `wolfSSL → libunwind → bionic_translation → art_standalone → android_translation_layer`.
  So **ART sits on top of the bionic shim** (ART itself uses bionic-style pieces),
  and the framework/JNI layer sits on top of ART. **[OPEN-REF]**
- ART normally **AOT-compiles** dex via `dex2oat` and caches `.oat`/`.art` images
  (ATL caches under `~/.cache/art/`). It can fall back to the **interpreter/JIT**
  (`-Xnoimage-dex2oat`, `-Xusejit:false`) — needed e.g. on non-4K page-size hosts
  (Apple Silicon). This matters for **distro portability** (page size, JIT, W^X). **[OPEN-REF]**
- Building ART "for host Linux" is a known, finicky AOSP exercise (host build via
  `buildbot-build.sh --host`); the canonical executables expect `/system/bin/linker`, so
  the standalone build has to be patched to not assume Android paths. **[CONFIRMED via AOSP docs]**

> **Rewrite reality check:** ART + libcore are **hundreds of thousands of lines of
> C++/Java**. They are **not** getting rewritten in Rust for v1 (or v5). A "completely in
> Rust" client must still **bundle/reuse prebuilt ART+libcore**. See §9.

### 4.4 The re-implemented Android framework (`api-impl.jar`) + JNI bridge

The app calls `android.*` framework APIs (e.g. `Activity`, `View`, `Surface`,
`AssetManager`, `MediaPlayer`, `ConnectivityManager`). In a real device these live in
`android.jar` backed by system services. ATL **re-implements** them: **[OPEN-REF]**

- **`api-impl.jar`** — Java implementations/stubs of the framework classes. Development
  is incremental: *stub a class so the app keeps launching → convert hot stubs into real
  implementations*. Crashes like `Class or Method was not found` literally tell you what
  to implement next.
- **`libtranslation_layer_main.so`** (`src/api-impl-jni/`) — the **JNI native side** of
  those framework classes. When a framework Java method needs to actually *do* something
  on Linux (draw a surface, play audio, read sensors), it calls down here in C.
- **`libandroid.so`** (`src/libandroid/`) — the shim presented to the *app's* native code
  for the Android **NDK** APIs (`ANativeActivity`, `ANativeWindow`, `AAssetManager`,
  `AInputQueue`, `ALooper`, `ASensorManager`, …). This is what a **NativeActivity**-style
  app talks to.

The framework is backed by **GTK4** for windowing/event loop (the app's main loop becomes
a **GLib main loop** after startup) and by the graphics/audio bridges below.

### 4.5 Graphics: Android `Surface`/`EGL`/`GLES`/`Vulkan` → desktop GPU

This is the second-hardest area after the libc bridge. **[OPEN-REF + CONFIRMED for Sober's user-facing options]**

- The app renders via **EGL + OpenGL ES** and/or **Vulkan**. The layer provides
  `libEGL.so`/`libGLESv2.so`/`libvulkan.so` shims that target the host's real GL/Vulkan
  (Mesa, NVIDIA). On X11, GTK may pick **GLX**, which breaks GLES assumptions — ATL forces
  EGL with `GDK_DEBUG=gl-essl`. **[OPEN-REF]**
- Android's **`Surface`/`ANativeWindow`/SurfaceFlinger** model (buffer queues, gralloc
  buffers) is mapped onto a **GTK4 surface / GL(ES) context**. Buffers that Android would
  pass as gralloc/`AHardwareBuffer` map to **DMA-BUF** on Linux (hence the `libdrm`,
  `libgbm` style deps). **[INFERRED]**
- **Vulkan is preferred** (best perf, cleanest mapping); **OpenGL is the fallback**.
  - Sober: Vulkan if your GPU is "from the last ~8 years," else auto-falls-back to GL;
    `use_opengl: true` forces GL. Needs **Mesa or NVIDIA** Vulkan; users verify via
    vulkan.gpuinfo.org. **[CONFIRMED]**
- This native-Vulkan path (vs Wine's DXVK Direct3D→Vulkan double translation) is a big
  reason Sober can **match or beat the Windows client** on the same hardware. **[CONFIRMED claim]**

### 4.6 Everything else the framework must bridge

- **Input:** Linux pointer/keyboard/gamepad events (via GTK/`libinput`/evdev) → Android
  `MotionEvent`/`KeyEvent`/`AInputEvent`. Sober adds **`touch_mode`** (off/on/fake-off)
  and **`allow_gamepad_permission`** for controllers. **[CONFIRMED for options]**
- **Audio:** Android `AudioTrack`/`OpenSL ES`/`AAudio` → **PulseAudio** (Sober's Flatpak
  declares `--socket=pulseaudio`; PipeWire's Pulse shim covers PipeWire systems). **[CONFIRMED socket]**
- **Fonts/text/WebView:** ATL pulls in **WebKitGTK 6.0** — used to back Android's
  **WebView** (Roblox login / web content) and contributes to text rendering. `libfontconfig` for fonts. **[OPEN-REF]**
- **Storage model:** ATL maps Android's per-app dirs to host paths:
  - app-private `/data/data/<pkg>` → `~/.local/share/android_translation_layer/<apk>_/`
  - shared `/storage/emulated/0` and `Android/obb/<pkg>/` under that same tree.
  Sober (Flatpak) keeps everything under `~/.var/app/org.vinegarhq.Sober/`. **[OPEN-REF + CONFIRMED]**
- **Networking:** Plain Linux sockets — but TLS matters. ATL builds **wolfSSL**; Roblox's
  native networking does its own TLS, so mostly this "just works" as native code over the
  host network (`--share=network`). **[OPEN-REF + CONFIRMED socket]**
- **Properties / `getprop`:** a fake Android property store (`__system_property_get`)
  returning sane values (build fingerprint, SDK int, etc.). `--sdk-int=NN` controls the
  reported API level. **[OPEN-REF]**
- **Binder/IPC:** stubbed/emulated; most Roblox-relevant calls avoid needing a real
  Binder, but anything that does must be faked. **[INFERRED — verify]**

---

## 5. The Sober-specific layer (what Sober adds on top of "ATL for Roblox")

These are **[CONFIRMED]** from Sober's Flatpak manifest, docs, and config schema.

### 5.1 Packaging & sandbox (Flatpak)

- **Runtime:** `org.gnome.Platform` **//50**; **SDK:** `org.gnome.Sdk`. (GNOME runtime
  gives GTK4 + Mesa + portals.) Command: `sober`.
- **`finish-args` (sandbox permissions):**
  - `--device=dri` — GPU / 3D.
  - `--share=network` — connectivity (required).
  - `--share=ipc` — X11 SHM performance.
  - `--socket=wayland` + `--socket=fallback-x11` — display (Wayland-first, X11 fallback).
  - `--socket=pulseaudio` — audio.
  - `--allow=devel` — `ptrace` (debugging/crash handling).
- **Shipped binaries** (prebuilt, closed; `buildsystem: simple` just installs them):
  - `sober` — the launcher/runtime executable.
  - `sober_services` — a helper/IPC/services process (Discord RPC, integrations, etc.).
  - `libloader.so` — **[INFERRED]** the bionic-aware loader/linker.
  - `libbadcpu.so` — **[INFERRED]** a CPU/feature shim (name suggests handling
    missing/old CPU features; recall Sober **requires SSE4.1/SSE4.2** — this lib likely
    relates to that gate or to "lie about"/trap unsupported features).
  - `libmimalloc.so` — Microsoft's **mimalloc** allocator (perf; also used by Roblox).
  - desktop file, icon, metadata, legal docs.
- **Source of binaries:** a versioned tarball, e.g.
  `https://sober.vinegarhq.org/artifacts/<date>_<commit>/<hash>/sober-binaries-unified.tar.zst`.
  **x86_64 only** in the manifest.
- **Why Flatpak-only:** the team wants a **reproducible runtime** (same GTK/Mesa/glibc
  everywhere) so bug reports are debuggable, and because closed binaries against a
  pinned runtime dodge the host-glibc/host-GTK variance problem. This is also their
  **distro-portability strategy** (see §8).

### 5.2 Configuration surface (what a user can tune)

`~/.var/app/org.vinegarhq.Sober/config/sober/config.json` (or right-click → Settings, or
`flatpak run org.vinegarhq.Sober config`). Confirmed options:

| Option | Meaning | Default |
|---|---|---|
| `use_opengl` | Force OpenGL instead of Vulkan | `false` |
| `graphics_optimization_mode` | `quality` / `balanced` / `performance` | `balanced` |
| `enable_gamemode` | Use Feral **GameMode** for perf | `true` |
| `enable_hidpi` | Scale window to display DPI | `false` |
| `discord_rpc_enabled` | Discord Rich Presence | `false` |
| `discord_rpc_show_join_button` | Allow Discord join | `false` |
| `server_location_indicator_enabled` | Show server-region popup on join | `false` |
| `close_on_leave` | Quit Sober when you leave a game | `true` |
| `touch_mode` | `off` / `on` / `fake-off` | `off` |
| `allow_gamepad_permission` | Controller support | `false` |
| `use_console_experience` | Console UI instead of desktop UI | `false` |
| `use_libsecret` | Store session via libsecret | `false` |
| `fflags` | Pass-through Roblox **Fast Flags** (engine config) | — |

`fflags` is significant: Roblox's engine is configured by thousands of **FFlags** (e.g.
the framerate cap lives behind an FFlag historically used to "unlock FPS"). Sober exposes
this so users can set engine flags the Android client wouldn't normally expose.

### 5.3 Hardware/runtime requirements (CONFIRMED)

- **CPU:** must support **SSE4.1 + SSE4.2** (check `/proc/cpuinfo`). (Native x86-64 code,
  no emulation.)
- **GPU:** Vulkan via **Mesa** or **NVIDIA** preferred; GL fallback.
- **Single instance only** — Roblox forbids concurrent sessions; multi-instance is
  deliberately disabled.
- **ARM64/VR:** experimental / not production; Quest VR not planned.
- **Roblox Studio:** out of scope — use **Vinegar** (Wine) for Studio.

### 5.4 Anti-cheat posture

- The Windows Player ships **Hyperion ("Byfron")**; the **Android** build's protection is
  different, which is why the Android route works where Wine was blocked. **[CONFIRMED]**
- VinegarHQ keeps Sober **closed-source specifically so the runtime is harder to
  weaponize into an exploit/cheat**, which would get the whole approach blocked. **[CONFIRMED]**
- They openly acknowledge Roblox **could block Sober at any time**; it's "unofficial
  research software." Any clone inherits this fragility. **[CONFIRMED]**

---

## 6. End-to-end execution flow (assembled picture)

```
1.  User launches `sober` (Flatpak).
2.  sober resolves config (config.json) + ensures a compatible Roblox APK is present
    (fetch/update from trusted source if needed).
3.  Runtime sets up the Android environment:
      - data dirs (/data/data, /storage/emulated/0 → host paths),
      - fake property store (build fingerprint, sdk int),
      - the bionic→glibc shim + custom linker (libloader/bionic_translation),
      - graphics backend selection (Vulkan, else GL; GDK_DEBUG=gl-essl on X11),
      - CPU feature gate (SSE4.1/4.2; libbadcpu).
4.  A GTK4 application + window is created; an ART/Dalvik VM boots with classpath =
    (re-implemented framework jar) : (Roblox APK).
5.  JNI libs register; framework reads AndroidManifest, builds the Application object,
    instantiates ContentProviders, launches the main Activity → onCreate().
6.  The Roblox client's Java/Kotlin code runs on ART; it System.loadLibrary()'s the
    native engine .so, which the custom linker loads against the bionic shim.
7.  Native engine initializes EGL/Vulkan against the host GPU; its Surface maps to the
    GTK4 window; input/audio bridges connect.
8.  Control hands off to the GLib main loop: input events → Android events, frames →
    GPU, audio → PulseAudio/PipeWire. Discord RPC / GameMode / etc. run alongside
    (sober_services).
9.  roblox:// / join deep-links route into the client; on leaving a game, close_on_leave
    optionally exits.
```

---

## 7. What's genuinely hard (risk map for a clone)

Ranked by difficulty/risk, highest first:

1. **bionic↔glibc ABI bridge + custom linker** (§4.2). TLS, stdio `FILE`, pthread, errno,
   C++ ABI, dlfcn routing. Subtle, crash-prone, security-sensitive. *Highest risk.*
2. **Reusing/building ART+libcore for host Linux** (§4.3). Big AOSP build, path
   assumptions, AOT vs interpreter, page-size/W^X portability. *High effort, mostly
   "integration" not "invention" — but unavoidable.*
3. **Graphics surface bridge** (§4.5). Android `Surface`/gralloc/`ANativeWindow` →
   GTK4 + EGL/Vulkan + DMA-BUF, on Mesa **and** NVIDIA, Wayland **and** X11.
4. **Framework breadth** (§4.4). Roblox touches a wide slice of `android.*`; you
   implement exactly enough, iteratively, driven by crash logs.
5. **Tracking Roblox** — Roblox ships frequent Android updates and can change FFlags,
   APIs, or anti-cheat. **Maintenance is forever.**
6. **Anti-cheat blocking** — out of your control; closed-source-ness is the only partial
   mitigation Sober uses.

---

## 8. Distro-portability strategy ("works on every distro")

Two viable strategies; **A is what Sober does and what I recommend for v1**:

- **A. Ship the runtime, not just the app (Flatpak / AppImage / containerized).**
  Bundle GTK4, Mesa userspace bits, glibc-compat, ART, the shims — pin everything against
  a known base (Sober pins **GNOME 50**). The host only provides the **kernel**, the
  **GPU kernel driver + ICD**, and the display socket. This sidesteps per-distro glibc/
  GTK/Mesa skew entirely. Flatpak also gives the **sandbox** and **portals** for free.
  - GPU is the one thing you can't fully bundle: you rely on the host's Vulkan/GL **ICD**
    (Mesa via the freedesktop GL extension; NVIDIA via the `nvidia` Flatpak runtime
    extension that Flatpak auto-matches to the host driver). Plan for both.
- **B. Native packages per distro.** Far more painful: every distro's glibc/GTK/Mesa
  version differs, ART's host build is fragile, and you'd re-fight portability constantly.
  Only worth it as a *secondary* target for packagers (AUR-style), as ATL has (Arch AUR,
  Alpine testing).

**Recommendation:** primary target = **Flatpak on the freedesktop or GNOME runtime**;
secondary = an **AppImage**/tarball for non-Flatpak systems; community native packages
optional. Either way, treat "the kernel + GPU ICD + Wayland/X11 socket" as the **only**
host contract. (Per project policy: detect GPU/Vulkan capability at runtime, fall back to
GL, fail with an actionable message if neither is present — never assume a vendor.)

---

## 9. Re-implementing in Rust ("Eclipse") — honest plan

The user wants **"completely in Rust."** Here is the honest engineering position, then a
staged plan.

### 9.1 What can be Rust vs what realistically can't (v1–v2)

| Layer | Rust-feasible? | Notes |
|---|---|---|
| Launcher / process orchestration / config / APK fetch+verify / deep-link handling | ✅ **Yes, ideal Rust** | This is normal systems code. Crates: `clap`, `serde`/`serde_json`, `reqwest`/`ureq` + `rustls`, `zip`, `sha2`, `directories`/`xdg`. |
| Discord RPC, GameMode toggling, services helper | ✅ Yes | `discord-rich-presence` crate; D-Bus via `zbus` for GameMode/portals. |
| Windowing / event loop glue | ✅ Yes | `gtk4-rs` (match Sober) **or** `winit` + `ash`/`wgpu`. GTK4 is closer to Sober's model + portals. |
| bionic→glibc shim + custom linker (§4.2) | ⚠️ **Partly** | Doable in Rust (a from-scratch ELF loader + ABI shims), but this is **the** highest-risk area; you may start by **wrapping/forking the existing C `bionic_translation`** and rewriting incrementally. Rust's `object`/`goblin` help parse ELF; the TLS/ABI glue needs `unsafe` + asm. |
| Android framework reimpl (`android.*`) | ⚠️ Mixed | The framework is **Java** (runs on ART) — that part stays Java. Its **JNI native backends** can be **Rust** (via `jni` crate) instead of C. |
| **ART (dex VM) + libcore** | ❌ **No (reuse)** | Re-writing a production dex VM + GC + JIT + the `java.*` library in Rust is a multi-year project on its own and a correctness minefield. **Bundle/reuse AOSP ART+libcore.** A pure-Rust dex interpreter exists only as toys; not viable for Roblox. |
| Graphics bridge | ⚠️ Yes-with-effort | Rust has excellent Vulkan (`ash`) / GL bindings; mapping Android Surface semantics is the hard part regardless of language. |

**Bottom line:** "**Eclipse is a Rust client that bundles a reused ART+libcore**" is
achievable and honest. "**100% Rust including the dex VM**" is not realistic for a
shippable Roblox client and shouldn't be promised. The *Rust* surface = launcher +
shims + JNI backends + graphics/input/audio bridges + the orchestration; the *reused*
surface = ART + libcore (and possibly the C `bionic_translation` until rewritten).

### 9.2 Staged milestones (prove the risky parts first)

1. **M0 — De-risk spike (do this before anything else).** Take the **open ATL stack**,
   build it, and **get *any* Android x86-64 app running** on your dev machine. Then try
   the **Roblox APK** unmodified. Goal: confirm the approach end-to-end and find where
   Roblox specifically breaks. *Success = a frame on screen, even if broken.*
2. **M1 — Rust launcher around the existing (C) runtime.** Re-implement Sober's
   *outer* shell in Rust: config, APK fetch/verify, env setup, GPU/Vulkan detection +
   GL fallback, Flatpak packaging. Keep ATL's C runtime underneath. Ship something that
   works.
3. **M2 — Rust-ify the JNI framework backends** (`jni` crate) and the
   integrations/services process. Replace `sober_services`-equivalent with Rust.
4. **M3 — Rust bionic shim + loader**, rewriting `bionic_translation`/`libloader`
   incrementally behind a test suite (per §10). Highest risk — do it last, with the most
   tests.
5. **M4 — Graphics/input/audio bridges in Rust** (`ash`, `gtk4`, `libinput`/evdev,
   PipeWire/Pulse).
6. Throughout: **reuse ART+libcore as a vendored, pinned dependency** with a documented
   build recipe.

### 9.3 Candidate crates / tooling (to validate via Context7 when implementing)

`ash` (Vulkan), `gtk4`/`gdk4`, `winit` (alt), `jni`, `object`/`goblin` (ELF),
`zip`/`apk-parser`-style, `rustls`+`reqwest`/`ureq`, `serde`/`serde_json`, `zbus` (D-Bus),
`discord-rich-presence`, `directories`. *None of the deep AOSP pieces are crates — they
are vendored C++/Java.* (Context7 wasn't applicable to ATL/Sober internals — they're not
documented libraries — but it should be used for each crate above at implementation time.)

---

## 10. Regression / verification approach (for the eventual build)

Tie tests to the confirmed risk areas, not to "it launched once":

- **ABI shim conformance tests:** for each `bionic_`-wrapped function, a tiny Android
  `.so` test fixture that exercises stdio `FILE`, pthread, TLS, errno, dlopen — run it
  through the loader and assert behavior matches a real device/AOSP reference. This is the
  guard that would catch the §4.2 class of bugs.
- **Capability/fallback tests:** force-no-Vulkan must fall back to GL; missing
  SSE4.2 must fail with a clear actionable error (never a silent crash); Wayland-absent
  must fall back to X11. (Directly mirrors the compatibility policy.)
- **Smoke test in CI:** boot the runtime headless (`ANDROID_LOG_TAGS=*:v`) against a
  trivial known-good APK and assert it reaches `onCreate` + first frame.
- **Distro matrix:** the Flatpak build is the portability guarantee; additionally smoke
  on Mesa **and** NVIDIA, Wayland **and** X11, in CI containers.

---

## 11. Open questions to resolve next (before/early in M0)

1. **Is Sober literally a fork of ATL, or an independent same-architecture
   implementation?** (Affects licensing and how much we can lean on ATL.) — *verify.*
2. **Exact Roblox dex surface:** which `android.*` classes does the current Roblox APK
   actually require? (Drives framework scope.) — *inspect a current APK.*
3. **Binder dependency:** does current Roblox need any real Binder IPC, or is stubbing
   enough? — *trace at runtime.*
4. **What `libbadcpu.so` really does** (CPU feature trapping vs gating) — *RE / confirm.*
5. **ART host-build recipe** that's reproducible and license-clean for redistribution.
6. **Licensing:** ATL is **GPLv3+**; AOSP is **Apache-2.0**; bundling ART+libcore +
   GPLv3 shims has obligations a closed clone avoided but an **open** project must honor.
   Decide Eclipse's license accordingly.

---

## 12. Sources

**Sober / VinegarHQ (primary):**
- [Sober official site](https://sober.vinegarhq.org/)
- [Sober docs — FAQ](https://vinegarhq.org/Sober/FAQ/index.html)
- [Sober docs — Configuration](https://vinegarhq.org/Sober/Configuration/index.html)
- [Sober docs — Installation](https://vinegarhq.org/Sober/Installation.html)
- [vinegarhq/sober (GitHub, issue tracker)](https://github.com/vinegarhq/sober)
- [vinegarhq/sober Discussion #767 — Android port](https://github.com/vinegarhq/sober/discussions/767)
- [vinegarhq/sober Issue #1221 — ARM64 support](https://github.com/vinegarhq/sober/issues/1221)
- [Install Sober — Flathub](https://flathub.org/en/apps/org.vinegarhq.Sober)
- [flathub/org.vinegarhq.Sober (Flatpak manifest repo)](https://github.com/flathub/org.vinegarhq.Sober)
- [VinegarHQ home](https://vinegarhq.org/) · [vinegarhq/vinegar (Studio via Wine)](https://github.com/vinegarhq/vinegar)
- [DeepWiki — vinegarhq/sober](https://deepwiki.com/vinegarhq/sober)

**Android Translation Layer (open reference):**
- [android_translation_layer (GitLab, canonical)](https://gitlab.com/android_translation_layer/android_translation_layer)
- [bionic_translation (GitLab)](https://gitlab.com/android_translation_layer/bionic_translation)
- [killerdevildog/android_translation_layer (GitHub fork w/ unified CMake + README detail)](https://github.com/killerdevildog/android_translation_layer)
- [NLnet — ATL project](https://nlnet.nl/project/ATL/)
- [Grokipedia — Android Translation Layer](https://grokipedia.com/page/android-translation-layer)
- [Hacker News — "ATL: A layer to run Android apps on Linux"](https://news.ycombinator.com/item?id=41966785)
- [Arch AUR — android_translation_layer](https://aur.archlinux.org/packages/android_translation_layer) · [-git](https://aur.archlinux.org/packages/android_translation_layer-git)
- [Alpine — android-translation-layer](https://pkgs.alpinelinux.org/package/edge/testing/armv7/android-translation-layer)

**bionic / ART / Android internals:**
- [Cloudef/android2gnulinux (bionic→glibc compat layer)](https://github.com/Cloudef/android2gnulinux)
- [Bionic (software) — Wikipedia](https://en.wikipedia.org/wiki/Bionic_(software))
- [platform/bionic — Google Git](https://android.googlesource.com/platform/bionic/)
- [Android Runtime (ART) — AOSP](https://source.android.com/docs/core/runtime)
- [ART modular system — AOSP](https://source.android.com/docs/core/ota/modular-system/art)
- [Building ART for native Linux (android-building group)](https://groups.google.com/g/android-building/c/ZZ-SXlkfKmY)
- [NativeActivity — Android Developers](https://developer.android.com/reference/android/app/NativeActivity)

**Context / history / guides:**
- [GamingOnLinux — Sober launch (Aug 2024)](https://www.gamingonlinux.com/2024/08/sober-is-a-new-way-to-play-roblox-on-linux-from-the-vinegar-team/)
- [Roblox DevForum — Sober announcement](https://devforum.roblox.com/t/sober-x86-64-roblox-build-without-hyperion-run-roblox-on-linux-natively-with-up-to-twice-the-performance-of-native-windows/3129236)
- [Roblox on Linux — Roblox Wiki (Fandom)](https://roblox.fandom.com/wiki/Roblox_on_Linux)
- [NixOS Wiki — Sober](https://wiki.nixos.org/wiki/Sober)
- [AxeeTech — Sober guide](https://axeetech.com/sober-roblox-linux-guide/) · [CyberPanel — Sober Linux](https://cyberpanel.net/blog/sober-linux)
- [The DUCK Project wiki — Sober + Bloxstrap](https://wiki.robotz.com/index.php/Linux_with_Sober_and_Bloxstraps)
