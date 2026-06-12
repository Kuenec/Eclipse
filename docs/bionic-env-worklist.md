# Eclipse bionic-env work-list (libroblox.so x86-64) — 2026-06-05

> ## ✅ COMPLETE (2026-06-05) — work-list 0; FULL resolution of all 584 libroblox imports
> The final 2 entries (the C-variadic liblog natives `__android_log_print` / `__android_log_assert`)
> are now DEFINED by a clean-room C shim (`src/loader/liblog_shim.c`, compiled by `build.rs` via the
> `cc` build-dependency) that formats varargs with `vsnprintf` into a bounded buffer and forwards to
> the Eclipse `extern "C"` sink `eclipse_liblog_emit` → `tracing`. The `EclipseNativeProvider` now
> registers **88** natives; the gated REAL test
> `loader::link::tests::real_libroblox_eclipse_natives_fully_resolve_all_imports` proves on the real
> engine: **work-list 88 → 0**, all 88 imports resolve to Eclipse addresses (the 2 variadic liblog to
> the shim), `applied_nonnull = 623` (621 + the 2 shim slots), `unresolved_strong = 0`, 88 GOT slots
> verified holding the Eclipse addresses, no panic/leak. **NEXT (the only remaining engine-load
> step): bind the relocated + fully-resolved image to execution and run the 3,427 `DT_INIT_ARRAY`
> constructors in an isolated harness (honoring RELRO + BIND_NOW; no `%fs`/TCB — no PT_TLS),
> main-loop / dev-host only.**

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

**Done (✅ below):** liblog (5 of 5 — 3 fixed-arity Rust + the 2 C-variadic via the clean-room C
shim, 2026-06-05) + bionic-libc (15 of 15) + ndk-android (27 of 27 — AAsset* REAL via `src/apk`,
AConfiguration/ALooper minimal-correct, ANativeWindow sound-stub deferred-to-render) + **media-ndk
(33 of 33) + audio (8 of 8) — sound-stubs (2026-06-05): gameplay-time, deferred** (see §4/§5 below).
**The 2 C-variadic liblog natives `__android_log_print` / `__android_log_assert`** are now DEFINED by
`src/loader/liblog_shim.c` (compiled by `build.rs` via the `cc` build-dep — the standard, justified
varargs bridge): each `vsnprintf`s its varargs into a bounded buffer and forwards to the Eclipse
`extern "C"` sink `eclipse_liblog_emit`. Rust DECLARES the variadic externs + takes their addresses
on stable (variadic *declarations* are stable; only *definitions* need nightly `c_variadic`), so no
ABI landmine and no nightly toolchain. **Remaining: 0** — FULL resolution of all 584 imports. The
NEXT step is to run the 3,427 DT_INIT_ARRAY constructors in an isolated harness.

## Headline

| metric | host-baseline only | + Eclipse-native tier (2026-06-05, FULL) |
|---|---:|---:|
| UND imports (reloc-referenced; = GNU_HASH-authoritative) | **584** | **584** |
| host-resolved (BASELINE, not ABI-correct) | **490** | 490 |
| Eclipse-native-resolved (bionic-ABI-correct/minimal/forward/real/sound-stub/C-shim) | 0 | **88** |
| **work-list (need Eclipse-owned bionic natives)** | **88** | **0** |
| symbol relocs applied (non-null addr) | 535 | **623** |
| symbol relocs applied weak-undef → 0 (legal) | 12 | 12 |
| symbol relocs unresolved-strong (recorded, no GOT write) | 88 | **0** |
| symbol relocs deferred (TPOFF64/IRELATIVE) | 0 | 0 |

> 2026-06-05: the **88** = liblog 5 (3 fixed-arity Rust + 2 C-variadic shim) + bionic-libc 15 +
> ndk-android 27 + **media-ndk 33 + audio 8**. Media + audio are **sound-stubs** (gameplay-time,
> deferred): each returns its public-ABI failure/unavailable sentinel (media `media_status_t` →
> `AMEDIA_ERROR_UNSUPPORTED`, pointer fns → NULL; `slCreateEngine` → `SL_RESULT_FEATURE_UNSUPPORTED`)
> so a caller cleanly detects "no media / no audio", never a fake success. The `AMEDIAFORMAT_KEY_*`
> (real key strings) + `SL_IID_*` (stable distinct interface-id pointers) are real data objects.
> **Work-list now = 0 — FULL resolution of all 584 imports** (the variadic cc shim closed the last 2).

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
| **TOTAL (host-baseline view; Eclipse-native work-list now 0 — FULL)**   | **490** | **88** | |

> `egl-gles` resolves a real **91** because this dev-host has Mesa `libEGL.so`/`libGLESv2.so`. On a
> GL-less host these would all be in the work-list (route to a host-GL/ANGLE bridge). `pthread`,
> `libm`, `cxa-runtime`, and most of `bionic-libc` resolve from host glibc as a baseline only.

---

## THE WORK-LIST — 88 imports (2026-06-05: ✅ ALL 88 now Eclipse-owned — work-list 0, FULL resolution)

Grouped by category, in the priority order they were implemented (NEXT step first).

### 1. liblog (5) — ✅ DONE — all 5 (3 fixed-arity Rust + 2 variadic C-shim) (2026-06-05)
`src/loader/native_provider.rs` implements the 3 fixed-arity ones as Eclipse-owned `extern "C"`
natives that emit to Eclipse's `tracing` sink (real emit, priority-mapped). The 2 **C-variadic**
ones are now DEFINED by the clean-room C shim `src/loader/liblog_shim.c` (compiled by `build.rs` via
the `cc` build-dep): each formats its varargs with `vsnprintf` into a bounded stack buffer and
forwards the finished line to the Eclipse `extern "C"` sink `eclipse_liblog_emit` → the same
`tracing` sink (`__android_log_assert` emits FATAL then `abort()`, noreturn). Rust declares the
variadic externs + takes their addresses on stable; no ABI landmine.
```
✅ __android_log_write        # minimal-correct → tracing (returns byte count ≥ 1)
✅ __android_log_buf_write    # minimal-correct → tracing (bufID ignored; single sink)
✅ android_set_abort_message  # minimal-correct → tracing (ERROR; void)
✅ __android_log_print         # C-shim → vsnprintf → eclipse_liblog_emit (returns byte count > 0)
✅ __android_log_assert        # C-shim → vsnprintf → eclipse_liblog_emit (FATAL) → abort() (noreturn)
```

### 2. bionic-libc — ✅ DONE: the 15 bionic-specific names glibc does NOT provide (2026-06-05)
All 15 implemented in `src/loader/native_provider.rs` (each labelled forward / minimal-correct):
```
✅ __system_property_get   # minimal-correct: empty store → writes ""/returns 0 (bionic "unset")
✅ __sF                    # 2026-06-12: bionic-SHAPED 3x152-byte backing (array of struct __sFILE,
                           # public NDK LP64 ABI) — &__sF[i] are Eclipse-owned sentinels remapped to
                           # the host streams by 25 translating stdio natives (fputs/fflush/fwrite/
                           # fprintf/…). The original 24-byte host-FILE*-pointer table was the root
                           # cause of core 782252 (crashpad's fputs(&__sF[2]) → SIGSEGV in its own
                           # crash logging): bionic consumers take the slot's ADDRESS, not its value.
✅ __errno                 # forward: → glibc __errno_location (identical C contract)
✅ __assert2               # minimal-correct: emit FATAL + abort (noreturn, fixed 4-arg)
✅ __gnu_strerror_r        # forward: → glibc GNU (char*-returning) strerror_r
✅ __stack_chk_guard       # minimal-correct: Eclipse-owned SSP guard word (low byte 0)
# bionic FORTIFY (_chk) — forward to the ABI-identical glibc op, honoring the bound (abort on overflow):
✅ __FD_CLR_chk  ✅ __FD_ISSET_chk  ✅ __FD_SET_chk
✅ __fwrite_chk  ✅ __sendto_chk  ✅ __strchr_chk  ✅ __strlen_chk  ✅ __strncpy_chk2  ✅ __write_chk
```
(303 other libc names resolve from glibc as a baseline; only these 15 were missing entirely. The
older "21" figure in this doc was prose; the real test reports exactly **15** bionic-libc names.
2026-06-12: 25 of the baseline names — every FILE*-consuming stdio import of the five `__sF`
importers: clearerr fclose feof ferror fflush fgets fileno fputc fputs fputwc fread __fread_chk
fseek fseeko ftell ftello fwrite getc getwc setvbuf ungetc ungetwc + fprintf/fscanf/vfprintf
(C shim) — are now Eclipse-owned translating natives, because a bionic `&__sF[i]` stream sentinel
must be remapped to the host glibc stream before glibc stdio may dereference it; `__fread_chk`
additionally had a bionic-vs-glibc argument-order mismatch.)

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

### 5. audio — OpenSL ES (8) — ✅ DONE (2026-06-05): REAL OpenSL ES → host audio (cpal)
**2026-06-05 UPDATE — audio is now REAL (was a sound-stub).** `src/loader/opensl.rs` implements
`slCreateEngine` as a WORKING `SLObjectItf` engine whose Eclipse-owned `#[repr(C)]` vtables (the
public OpenSL ES 1.0.1 slot order) drive `Realize`/`GetInterface` → `SLEngineItf`;
`CreateOutputMix` + `CreateAudioPlayer` (an `SLDataLocator_AndroidSimpleBufferQueue` +
`SLDataFormat_PCM` source → an output-mix sink) → a player exposing `SLPlayItf` +
`SLAndroidSimpleBufferQueueItf` whose `Enqueue` decodes 8/16-bit-LE PCM → `f32` and feeds a **cpal**
host output stream (real sound; the bq-callback fires per finished buffer). On a host with no audio
device the engine still constructs and accepts Enqueues (no sound) — a clean "no device" posture,
never a fake. The 7 `SL_IID_*` stay real, stable, distinct `SLInterfaceID` data objects, now
**consumed** by `GetInterface` (matched via `native_provider::sl_iid_index`). Only these 8 symbols are
imported (everything else flows through the vtables) → no dead natives. Validate with
`eclipse __audio-test` (drives the real path; SKIPs cleanly with no device).
```
✅ slCreateEngine  # REAL: returns a working SLObjectItf engine (src/loader/opensl.rs → cpal)
✅ SL_IID_ANDROIDCONFIGURATION  ✅ SL_IID_ANDROIDSIMPLEBUFFERQUEUE  ✅ SL_IID_BUFFERQUEUE
✅ SL_IID_ENGINE  ✅ SL_IID_PLAY  ✅ SL_IID_RECORD  ✅ SL_IID_VOLUME  # consumed by GetInterface
```
(`OpenMAXAL` — `XA_*` — contributes **0** here: none of its symbols is referenced by a relocation
in this build, so it is not on the work-list despite being a `DT_NEEDED`.)

---

## Recommended implementation order (NEXT steps)

1. ~~**liblog (5)**~~ — ✅ DONE (3 fixed-arity routed to Eclipse's `tracing`; the 2 C-variadic now
   DEFINED by the clean-room C shim `src/loader/liblog_shim.c` via the `cc` build-dep — 2026-06-05).
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
5. ~~**the 2 variadic liblog** (`__android_log_print`/`__android_log_assert`)~~ — ✅ DONE (2026-06-05):
   the clean-room C (cc) shim `src/loader/liblog_shim.c` DEFINES both (vsnprintf → `eclipse_liblog_emit`),
   bringing the work-list 2 → 0 → **full resolution** of all 584 imports.
6. **⏭️ NEXT — bind + run the constructors:** bind the relocated + fully-resolved image to execution
   and run the **3,427 `DT_INIT_ARRAY` constructors** in order in an isolated harness, honoring
   RELRO + BIND_NOW (no `%fs`/TCB needed — no PT_TLS). This is **main-loop / dev-host only** (the
   cyber-safeguard).

> The pipeline is **proven and CLOSED**: with the Eclipse-native tier prepended, **623** GOT/PLT slots
> are filled on the mapped 112 MiB engine (88 of them at verified Eclipse-native addresses, incl. the 2
> variadic liblog C-shim), the work-list is **0** (FULL resolution), and nothing is fabricated. The
> remaining work is binding + the DT_INIT_ARRAY run, not the relocation machinery.
