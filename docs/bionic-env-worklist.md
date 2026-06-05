# Eclipse bionic-env work-list (libroblox.so x86-64) — 2026-06-05

The **first bionic-env resolution cut**: resolve every one of `libroblox.so`'s 584 undefined (UND)
imports against a host-baseline [`BionicEnv`](../src/loader/bionic_env.rs) scope, categorize them,
and apply the host-resolvable subset on the mapped engine (proving the symbol-relocation pipeline).
This document is the **key deliverable** — the exact list of imports that need **Eclipse-owned
bionic natives** next.

All numbers below are **real**, produced by the gated test
`loader::link::tests::real_libroblox_bionic_env_resolves_categorizes_and_partially_applies`
(maps + base-relocates the real engine, builds the BionicEnv scope, categorizes the
**relocation-referenced** UND imports, applies the host-resolvable subset). Reproduce:

```sh
cargo test -p eclipse --lib \
  loader::link::tests::real_libroblox_bionic_env_resolves_categorizes_and_partially_applies \
  -- --nocapture
# (skips cleanly if the Roblox APK is absent — never fails / fabricates)
```

> ## HONEST BASELINE CAVEAT (read first)
> The 490 host-resolved imports resolve to **host glibc / host-GL** addresses. This is a
> **relocation-pipeline BASELINE**, **NOT** bionic-ABI-correct execution. bionic (Android's libc)
> and glibc differ in `struct` layout, `errno` mechanism, `pthread_t`/mutex/TLS internals, `FILE`,
> and `__cxa_*`/atexit semantics. Filling a GOT slot with a glibc address makes the relocation
> *land*; **calling** that routine with bionic-shaped arguments (or sharing glibc/bionic state) is
> not correct. **libroblox.so is NOT runnable from this.** Correct execution needs Eclipse-owned
> bionic-shim providers (the next steps); the [`BionicEnv`] scope is built so those can be
> **prepended** before the host tier, displacing glibc for the libc surface.

---

## STATUS UPDATE — 2026-06-05: the Eclipse-native tier landed + ndk-android (work-list 88 → 70 → 43)

The **`EclipseNativeProvider`** (`src/loader/native_provider.rs`) is now PREPENDED before the host
baseline in [`BionicEnv`](../src/loader/bionic_env.rs) (`with_host_baseline(try_host_gl,
eclipse_natives=true)`). It registers **45** Eclipse-owned `extern "C"` natives — **liblog 3** (the
fixed-arity ones) + **bionic-libc 15** + **ndk-android 27** — so those imports now resolve to
**Eclipse addresses, not host glibc**. The gated real test
`loader::link::tests::real_libroblox_eclipse_natives_resolve_liblog_libc_and_ndk_android` proves it on
the real engine: work-list **88 → 43**, `applied_nonnull` **535 → 580** (+45 Eclipse-native GOT slots,
all verified holding the Eclipse address; the host has no such symbol under these bionic/NDK names).

**Done (✅ below):** liblog (3 of 5) + bionic-libc (15 of 15) + ndk-android (27 of 27 — AAsset* REAL
via `src/apk`, AConfiguration/ALooper minimal-correct, ANativeWindow sound-stub deferred-to-render)
+ **media-ndk (33 of 33) + audio (8 of 8) — sound-stubs (2026-06-05): gameplay-time, deferred** (see
§4/§5 below). **Deferred (2):** the C-variadic liblog natives `__android_log_print` /
`__android_log_assert` — defining a variadic `extern "C"` fn needs Rust's unstable `c_variadic`
feature; Eclipse builds on **stable** (clean-checkout portability, AGENTS.md §2.11). Registering a
non-variadic fn under a variadic name would be an ABI landmine, so they stay on the work-list (no
landmine). **Remaining: 2** = the 2 variadic liblog (the NEXT step = a variadic cc shim → full
resolution, then run the 3,427 DT_INIT_ARRAY).

## Headline

| metric | host-baseline only | + Eclipse-native tier (2026-06-05) |
|---|---:|---:|
| UND imports (reloc-referenced; = GNU_HASH-authoritative) | **584** | **584** |
| host-resolved (BASELINE, not ABI-correct) | **490** | 490 |
| Eclipse-native-resolved (bionic-ABI-correct/minimal/forward/real/sound-stub) | 0 | **86** |
| **work-list (need Eclipse-owned bionic natives)** | **88** | **2** |
| symbol relocs applied (non-null addr) | 535 | **621** |
| symbol relocs applied weak-undef → 0 (legal) | 12 | 12 |
| symbol relocs unresolved-strong (recorded, no GOT write) | 88 | **2** |
| symbol relocs deferred (TPOFF64/IRELATIVE) | 0 | 0 |

> 2026-06-05: the **86** = liblog 3 + bionic-libc 15 + ndk-android 27 + **media-ndk 33 + audio 8**.
> Media + audio are **sound-stubs** (gameplay-time, deferred): each returns its public-ABI
> failure/unavailable sentinel (media `media_status_t` → `AMEDIA_ERROR_UNSUPPORTED`, pointer fns →
> NULL; `slCreateEngine` → `SL_RESULT_FEATURE_UNSUPPORTED`) so a caller cleanly detects "no media /
> no audio", never a fake success. The `AMEDIAFORMAT_KEY_*` (real key strings) + `SL_IID_*` (stable
> distinct interface-id pointers) are real data objects. Work-list now = ONLY the 2 variadic liblog.

The 584 imports are counted from the **relocations**, not the raw dynamic symtab — this is
immune to `elf.rs`'s documented symtab over-read (it reads trailing VERSYM/GNU_HASH bytes as extra
"UND symbols"; real relocs only index valid slots), so the categorization split matches the apply
pass by construction (both walk the relocations through the same scope).

## Categorization by work-list category

`host_baseline` = can the host stand in (relocation lands, baseline only)? `no host equivalent`
means an Eclipse-owned native is the ONLY path even for a baseline.

| category | resolved (host baseline) | unresolved (work-list) | host baseline possible? |
|---|---:|---:|---|
| egl-gles    | 91 | 0  | yes — host `libEGL.so`/`libGLESv2.so` present |
| pthread     | 45 | 0  | yes (baseline only; bionic `pthread_t` differs) |
| libm        | 43 | 0  | yes (pure math is sound) |
| bionic-libc | 303 | 21 | partly — 21 are bionic-specific (glibc lacks them) |
| cxa-runtime | 3  | 0  | yes (baseline; glibc atexit semantics) |
| dl          | 5  | 0  | baseline only — must route to Eclipse's OWN loader |
| ndk-android | 0  | ✅ 0 (was 27) | **no host equiv → all 27 Eclipse-owned (2026-06-05)** |
| media-ndk   | 0  | ✅ 0 (was 33) | **no host equiv → all 33 Eclipse sound-stub (2026-06-05)** |
| audio       | 0  | ✅ 0 (was 8)  | **no host equiv → all 8 Eclipse sound-stub (2026-06-05)** |
| liblog      | 0  | 5  | no host equiv — **Eclipse already owns these** |
| **TOTAL (host-baseline view; Eclipse-native work-list now 2)**   | **490** | **88** | |

> `egl-gles` resolves a real **91** because this dev-host has Mesa `libEGL.so`/`libGLESv2.so`. On a
> GL-less host these would all be in the work-list (route to a host-GL/ANGLE bridge). `pthread`,
> `libm`, `cxa-runtime`, and most of `bionic-libc` resolve from host glibc as a baseline only.

---

## THE WORK-LIST — 88 imports (2026-06-05: 86 now Eclipse-owned; only 2 variadic liblog remain)

Grouped by category, in the priority order they were implemented (NEXT step first).

### 1. liblog (5) — ✅ DONE (3) routed to Eclipse's `tracing`; 2 variadic DEFERRED (2026-06-05)
`src/loader/native_provider.rs` implements the 3 fixed-arity ones as Eclipse-owned `extern "C"`
natives that emit to Eclipse's `tracing` sink (real emit, priority-mapped). The 2 **C-variadic**
ones cannot be defined on stable Rust (`c_variadic` is nightly-only) — registering a non-variadic fn
under them would be an ABI landmine, so they stay here (no landmine).
```
✅ __android_log_write        # minimal-correct → tracing (returns byte count ≥ 1)
✅ __android_log_buf_write    # minimal-correct → tracing (bufID ignored; single sink)
✅ android_set_abort_message  # minimal-correct → tracing (ERROR; void)
⏳ __android_log_print         # DEFERRED — C-variadic (needs nightly c_variadic)
⏳ __android_log_assert        # DEFERRED — C-variadic + noreturn
```

### 2. bionic-libc — ✅ DONE: the 15 bionic-specific names glibc does NOT provide (2026-06-05)
All 15 implemented in `src/loader/native_provider.rs` (each labelled forward / minimal-correct):
```
✅ __system_property_get   # minimal-correct: empty store → writes ""/returns 0 (bionic "unset")
✅ __sF                    # forward: table of the 3 host glibc FILE* (stdin/stdout/stderr)
✅ __errno                 # forward: → glibc __errno_location (identical C contract)
✅ __assert2               # minimal-correct: emit FATAL + abort (noreturn, fixed 4-arg)
✅ __gnu_strerror_r        # forward: → glibc GNU (char*-returning) strerror_r
✅ __stack_chk_guard       # minimal-correct: Eclipse-owned SSP guard word (low byte 0)
# bionic FORTIFY (_chk) — forward to the ABI-identical glibc op, honoring the bound (abort on overflow):
✅ __FD_CLR_chk  ✅ __FD_ISSET_chk  ✅ __FD_SET_chk
✅ __fwrite_chk  ✅ __sendto_chk  ✅ __strchr_chk  ✅ __strlen_chk  ✅ __strncpy_chk2  ✅ __write_chk
```
(303 other libc names resolve from glibc as a baseline; only these 15 were missing entirely. The
older "21" figure in this doc was prose; the real test reports exactly **15** bionic-libc names.)

### 3. ndk-android — libandroid (27) — ✅ DONE (2026-06-05): all 27 Eclipse-owned natives
`src/loader/native_provider.rs` implements all 27, each labelled real / minimal-correct / sound-stub;
opaque NDK pointers are Eclipse-owned generational [`ndk_registry`](../src/loader/ndk_registry.rs)
handles cast to `*mut T` (a stale/fabricated pointer is a typed `Err` → NDK sentinel, never UB).
```
# AAsset / AAssetManager (6) — REAL: route to Eclipse's own src/apk reader (real bytes):
✅ AAssetManager_fromJava      # real: AAssetManager* over the boot-configured APK path
✅ AAssetManager_open          # real: reads assets/<name> bytes via crate::apk; NULL if absent
✅ AAsset_getBuffer            # real: stable pointer into the owned asset bytes; NULL if stale
✅ AAsset_getLength            # real: bytes.len() (off_t); 0 if stale
✅ AAsset_close                # real: frees the owned handle slot
✅ AAsset_openFileDescriptor   # sound-stub: in-memory asset → returns -1 (NDK "no direct fd" → buffer fallback)
# AConfiguration (9) — MINIMAL-CORRECT: real getters over Eclipse device values (xhdpi portrait):
✅ AConfiguration_new  ✅ AConfiguration_delete  ✅ AConfiguration_fromAssetManager
✅ AConfiguration_getCountry  ✅ AConfiguration_getLanguage  ✅ AConfiguration_getNavHidden
✅ AConfiguration_getScreenHeightDp  ✅ AConfiguration_getScreenSize  ✅ AConfiguration_getScreenWidthDp
# ALooper (7) — MINIMAL-CORRECT Eclipse per-thread looper (fd registry); pollOnce → ALOOPER_POLL_*:
✅ ALooper_prepare  ✅ ALooper_forThread  ✅ ALooper_acquire  ✅ ALooper_release
✅ ALooper_pollOnce            # minimal: finite timeout → POLL_TIMEOUT, infinite → POLL_ERROR (NOT fake CALLBACK)
✅ ALooper_addFd  ✅ ALooper_removeFd
# ANativeWindow (5) — SOUND-STUB: getters return real geometry; surface/buffer DEFERRED-TO-RENDER:
✅ ANativeWindow_fromSurface   # sound-stub: mints a window handle w/ default geometry (surface bind deferred)
✅ ANativeWindow_getWidth  ✅ ANativeWindow_getHeight   # real geometry; -1 if stale
✅ ANativeWindow_acquire  ✅ ANativeWindow_release       # sound no-ops (registry-lifetime windows)
```
DEFERRED-TO-RENDER-INTEGRATION: the ANativeWindow surface/buffer behavior (`fromSurface` binding,
plus the not-in-the-27 `setBuffersGeometry`/`lock`/`unlockAndPost`) lands with the GLES2/EGL render +
input integration. The boot path calls `ndk_registry::set_apk_path` so the asset natives serve real
bytes from the opened Roblox APK.

### 4. media-ndk — libmediandk (33) — ✅ DONE (2026-06-05): sound-stubs (gameplay-time, deferred)
`src/loader/native_provider.rs` implements all 33 as Eclipse-owned `extern "C"` sound-stubs. Media
(video decode/encode) is a gameplay-time subsystem libroblox does not need to start/render, so each
returns its public-ABI failure/unavailable sentinel (per `media/NdkMediaCodec.h`/`NdkMediaFormat.h`/
`NdkMediaError.h`): pointer fns → NULL; `media_status_t` fns → `AMEDIA_ERROR_UNSUPPORTED` (-10009);
`ssize_t` dequeue → negative; `bool` getters → false; setters/delete → no-op; `toString` → a stable
empty string. The 10 `AMEDIAFORMAT_KEY_*` are real `const char*` data objects (the canonical key
strings). NO global state, NO UB. If the DT_INIT_ARRAY discovery loop later proves any is
init-critical, it gets a real host-codec bridge then.
```
✅ AMediaCodec_configure  ✅ AMediaCodec_createDecoderByType  ✅ AMediaCodec_createEncoderByType
✅ AMediaCodec_delete  ✅ AMediaCodec_dequeueInputBuffer  ✅ AMediaCodec_dequeueOutputBuffer
✅ AMediaCodec_flush  ✅ AMediaCodec_getInputBuffer  ✅ AMediaCodec_getOutputBuffer
✅ AMediaCodec_getOutputFormat  ✅ AMediaCodec_queueInputBuffer  ✅ AMediaCodec_releaseOutputBuffer
✅ AMediaCodec_start  ✅ AMediaCodec_stop
✅ AMediaFormat_delete  ✅ AMediaFormat_getBuffer  ✅ AMediaFormat_getInt32  ✅ AMediaFormat_new
✅ AMediaFormat_setBuffer  ✅ AMediaFormat_setFloat  ✅ AMediaFormat_setInt32  ✅ AMediaFormat_setString
✅ AMediaFormat_toString
# AMEDIAFORMAT_KEY_* DATA objects — real `const char*` key strings (minimal-correct data):
✅ AMEDIAFORMAT_KEY_BIT_RATE  ✅ AMEDIAFORMAT_KEY_CHANNEL_COUNT  ✅ AMEDIAFORMAT_KEY_COLOR_FORMAT
✅ AMEDIAFORMAT_KEY_FRAME_RATE  ✅ AMEDIAFORMAT_KEY_HEIGHT  ✅ AMEDIAFORMAT_KEY_I_FRAME_INTERVAL
✅ AMEDIAFORMAT_KEY_MIME  ✅ AMEDIAFORMAT_KEY_SAMPLE_RATE  ✅ AMEDIAFORMAT_KEY_STRIDE
✅ AMEDIAFORMAT_KEY_WIDTH
```

### 5. audio — OpenSL ES (8) — ✅ DONE (2026-06-05): sound-stubs (gameplay-time, deferred)
`src/loader/native_provider.rs` implements all 8 as Eclipse-owned `extern "C"` sound-stubs. Sound is
a gameplay-time subsystem. Per the public OpenSL ES 1.0.1 C-ABI (`SLES/OpenSLES.h`), `slCreateEngine`
returns `SL_RESULT_FEATURE_UNSUPPORTED` (0x0C) — the documented result a caller checks to detect "no
audio" cleanly (it leaves `*pEngine` untouched, never a fake engine). The 7 `SL_IID_*` are real,
stable, distinct `SLInterfaceID` data objects (valid non-null pointers; never queried because
slCreateEngine fails first). NO global state, NO UB.
```
✅ slCreateEngine  # → SL_RESULT_FEATURE_UNSUPPORTED (no engine produced; *pEngine untouched)
✅ SL_IID_ANDROIDCONFIGURATION  ✅ SL_IID_ANDROIDSIMPLEBUFFERQUEUE  ✅ SL_IID_BUFFERQUEUE
✅ SL_IID_ENGINE  ✅ SL_IID_PLAY  ✅ SL_IID_RECORD  ✅ SL_IID_VOLUME
```
(`OpenMAXAL` — `XA_*` — contributes **0** here: none of its symbols is referenced by a relocation
in this build, so it is not on the work-list despite being a `DT_NEEDED`.)

---

## Recommended implementation order (NEXT steps)

1. ~~**liblog (5)**~~ — ✅ DONE (3 fixed-arity routed to Eclipse's `tracing`; 2 variadic deferred).
   The `EclipseNativeProvider` is the loader→Eclipse-native binding path, prepended before host.
2. ~~**bionic-libc bionic-specific (15)**~~ — ✅ DONE: Eclipse-owned natives for all 15 glibc-missing
   names (`__system_property_get`, `__sF`, `__errno`, the `_chk` FORTIFY family, `__stack_chk_guard`),
   each labelled forward/minimal-correct. Prepended before the host tier (Eclipse wins).
3. ~~**ndk-android (27)**~~ — ✅ DONE (2026-06-05). All 27 in `src/loader/native_provider.rs`:
   `AAsset*`/`AAssetManager*` REAL via Eclipse's own `src/apk` reader (opaque handles =
   `src/loader/ndk_registry.rs` generational indices, no UB); `AConfiguration_*`/`ALooper_*`
   minimal-correct; `ANativeWindow_*` sound-stub (real geometry getters, surface/buffer
   deferred-to-render). Work-list 70 → 43.
4. ~~**media-ndk (33)** + **audio (8)**~~ — ✅ DONE (2026-06-05): all 41 Eclipse-owned `extern "C"`
   **sound-stubs** (gameplay-time, deferred — video/sound are NOT needed to start/render). Media
   pointer fns → NULL, `media_status_t` → `AMEDIA_ERROR_UNSUPPORTED`; `slCreateEngine` →
   `SL_RESULT_FEATURE_UNSUPPORTED`; `AMEDIAFORMAT_KEY_*`/`SL_IID_*` real data objects. Work-list
   43 → 2. If the DT_INIT_ARRAY discovery loop later proves any is init-critical, it gets a real
   host bridge then.
5. **⏭️ NEXT — the 2 deferred variadic liblog** (`__android_log_print`/`__android_log_assert`): need
   a nightly toolchain or a justified clean-room C (cc) shim → **full resolution** (work-list 2 → 0).
6. After the work-list is satisfied: bind the assembled image to execution and run the **3,427
   `DT_INIT_ARRAY` constructors** in order, honoring RELRO + BIND_NOW (no `%fs`/TCB needed — no
   PT_TLS). This is **main-loop / dev-host only** (the cyber-safeguard).

> The pipeline is **proven**: with the Eclipse-native tier prepended, **621** GOT/PLT slots are filled
> on the mapped 112 MiB engine (86 of them at verified Eclipse-native addresses), the work-list is
> down to the **2** variadic liblog, and nothing is fabricated. The remaining work is the variadic cc
> shim (then the DT_INIT_ARRAY run), not the relocation machinery.
