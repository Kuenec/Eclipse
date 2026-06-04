# Eclipse — M0 Runbook (validate the foundation)

> **Purpose:** *Validation, not decision.* The architecture and component choices are
> locked (see [`component-map.md`](./component-map.md)). M0 proves the ATL runtime actually
> builds and that a current Roblox APK boots and renders on it, **before** we invest in the
> Rust launcher/integration layer (M1+). If M0 fails, we fix the foundation; if it passes,
> we build on it with confidence.
>
> Last updated **2026-06-04**. This is a manual runbook you run on a Linux box; it does not
> require any Eclipse code.

## Outcomes M0 must produce

1. A **working ATL stack** (`libart` + boot image + libcore jars + `android_translation_layer`) on the dev machine.
2. A yes/no on **"does the current Roblox APK boot to a rendered frame?"** with evidence.
3. The **framework work-list** — the exact `android.*` classes/methods Roblox calls that are
   missing/stubbed (this scopes M2).
4. Three measurements that tune later policy: **(a)** Roblox dex-vs-native split,
   **(b)** does **JIT** map here or must we use the interpreter, **(c)** **Vulkan vs GL**
   path and rough startup time.

## Prerequisites

- A **Linux x86_64** machine (Roblox Android ships x86_64; this is the first-class target).
- **Disk:** budget ~30–60 GB for the AOSP-derived ART build and caches.
- **Build deps** (from ATL's docs):
  - **Debian/Ubuntu:** `build-essential cmake meson ninja-build pkg-config git ant aapt autoconf libtool openjdk-21-jdk libgtk-4-dev libvulkan-dev libopenxr-dev libwayland-dev libportal-dev libsqlite3-dev libavcodec-dev libswscale-dev libdrm-dev libgudev-1.0-dev libwebkitgtk-6.0-dev libfontconfig-dev libasound2-dev libcap-dev libglib2.0-dev`
  - **Fedora:** the equivalents — `cmake meson ninja-build gcc-c++ git ant java-21-openjdk-devel pkgconf-pkg-config` plus `-devel` packages for `gtk4 vulkan-loader openxr libportal sqlite libavcodec libswscale libdrm libgudev webkitgtk6.0 fontconfig alsa-lib libcap glib2 wayland`.
  - **Arch:** ATL is in the AUR (`android_translation_layer` / `-git`) — may be faster than building.
  - **Alpine/postmarketOS:** ATL is packaged in `testing`.
- A **Roblox Android APK, x86_64, recent** — you obtain this (per project policy we do not
  redistribute it). Verify it contains `lib/x86_64/` and a `classes*.dex`.
- A **Vulkan-capable GPU** with a working ICD (Mesa or NVIDIA). Check: `vulkaninfo | head`.

## Step 1 — Build the ATL stack

Use the unified-CMake fork (builds the whole chain in order: wolfSSL → libunwind →
bionic_translation → art_standalone → android_translation_layer):

```bash
git clone https://github.com/killerdevildog/android_translation_layer.git
cd android_translation_layer
cmake -B build
cmake --build build         # long; this compiles the AOSP-derived ART
```

If the unified build fights you, fall back to the canonical sources and build in dependency
order:
- `https://gitlab.com/android_translation_layer/android_translation_layer` (see its `doc/Build.md`)
- `https://gitlab.com/android_translation_layer/art_standalone`
- `https://gitlab.com/android_translation_layer/bionic_translation`

**Pass criteria:** `build/install/` (or the meson install prefix) contains the
`android-translation-layer` binary, `libart`, a boot image, and the libcore jars.

## Step 2 — Smoke test with a trivial known-good APK

Confirm the runtime itself works before blaming Roblox. ATL's own test targets (e.g. the
Gravity Defied sample referenced in its docs) are ideal.

```bash
export ANDROID_LOG_TAGS="*:v"          # verbose logging
# On X11 only, force EGL so GTK doesn't pick GLX and break GLES:
export GDK_DEBUG=gl-essl

LD_LIBRARY_PATH="build/install/lib:." \
  ./build/install/bin/android-translation-layer /path/to/simple.apk \
  -l org/happysanta/gd/GDActivity
```

**Pass criteria:** the app window opens and renders a frame. If this fails, fix the stack
before touching Roblox.

## Step 3 — Boot the Roblox APK

```bash
# Inspect first:
unzip -l roblox.apk | grep -E 'lib/x86_64/|classes.*\.dex'

export ANDROID_LOG_TAGS="*:v"
export GDK_DEBUG=gl-essl                # X11 only
LD_LIBRARY_PATH="build/install/lib:." \
  ./build/install/bin/android-translation-layer roblox.apk \
  -l com/roblox/client/ActivityNativeMain \
  --sdk-int=33 2>&1 | tee roblox-boot.log
```

Notes:
- The launcher activity name (`com/roblox/client/ActivityNativeMain`) and the right
  `--sdk-int` may need adjustment — read them from the manifest:
  `axmldecoder`/`aapt dump badging roblox.apk` shows `launchable-activity` and `targetSdkVersion`.
- If Vulkan misbehaves, retry with the GL path (ATL/Mesa env) to isolate.
- If AOT/`dex2oat` chokes (page size, JIT), retry interpreter-leaning:
  `-X '-Xnoimage-dex2oat' -X '-Xusejit:false'` and clear `~/.cache/art/` between runs.

**Record, per attempt:** does it reach `onCreate`? show the login WebView? reach the home
screen? join an experience? render the 3D world? Capture screenshots + the full log.

## Step 4 — The four measurements

1. **Framework work-list (scopes M2).** Grep the log for the tells ATL documents:
   ```bash
   grep -E "Class .* not found|Method .* not found|UnsatisfiedLink|no implementation" roblox-boot.log \
     | sort -u > framework-worklist.txt
   ```
   Every unique entry is an `android.*` class/method we must implement.

2. **Dex-vs-native split (the ART-necessity confirmation).** Already concluded ART is
   required (see component-map §3); this quantifies it:
   ```bash
   mkdir rbx && cd rbx && unzip -q ../roblox.apk
   du -sh lib/x86_64/                      # native engine size
   ls -la classes*.dex                     # dex count + size
   # method count per dex (needs Android SDK build-tools' dexdump, optional):
   for d in classes*.dex; do echo "$d:"; dexdump -f "$d" 2>/dev/null | grep -c 'method_idx'; done
   ```

3. **JIT viability (sets compile policy).** From the boot log, note whether ART logs JIT
   compilation or falls back / errors on executable mmap. If JIT is blocked here, the
   policy is interpreter + AOT boot image (see `art-and-runtime.md` §4).

4. **Graphics path + startup.** Note Vulkan vs GL actually used, and rough time from launch
   to first frame, for the AOT/JIT/interpreter variants you tried.

## Pass/fail gate

| Result | Meaning | Next |
|---|---|---|
| Roblox renders a frame (even with rough edges) | **Foundation validated** | Proceed to **M1** (Rust launcher around this runtime). |
| Boots but stalls (missing classes/crash) | Foundation OK, framework gaps | Work the `framework-worklist.txt`; this *is* the M2 backlog. |
| Stack won't build / engine won't load `.so` | Foundation problem | Fix the bionic/ART build first; do not proceed. |

## What to hand off to M1

- The built ATL stack (note the exact commit/tag you built — we pin it).
- `roblox-boot.log`, screenshots, `framework-worklist.txt`, and the four measurements.
- The confirmed launcher activity + `--sdk-int` + graphics path that worked.

These become the fixtures and the work backlog for the Rust layer.
