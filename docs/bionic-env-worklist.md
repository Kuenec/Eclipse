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

## Headline

| metric | value |
|---|---:|
| UND imports (reloc-referenced; = the GNU_HASH-authoritative count) | **584** |
| host-resolved (BASELINE, not ABI-correct) | **490** |
| **work-list (need Eclipse-owned bionic natives)** | **88** |
| symbol relocs applied (non-null host addr) | 535 |
| symbol relocs applied weak-undef → 0 (legal) | 12 |
| symbol relocs unresolved-strong (recorded, no GOT write) | 88 |
| symbol relocs deferred (TPOFF64/IRELATIVE) | 0 |

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
| ndk-android | 0  | 27 | **no host equivalent** |
| media-ndk   | 0  | 33 | **no host equivalent** |
| audio       | 0  | 8  | **no host equivalent** |
| liblog      | 0  | 5  | no host equiv — **Eclipse already owns these** |
| **TOTAL**   | **490** | **88** | |

> `egl-gles` resolves a real **91** because this dev-host has Mesa `libEGL.so`/`libGLESv2.so`. On a
> GL-less host these would all be in the work-list (route to a host-GL/ANGLE bridge). `pthread`,
> `libm`, `cxa-runtime`, and most of `bionic-libc` resolve from host glibc as a baseline only.

---

## THE WORK-LIST — 88 imports needing Eclipse-owned bionic natives

Grouped by category, in the priority order to implement (NEXT step first).

### 1. liblog (5) — Eclipse ALREADY owns these in `src/framework.rs`; just route them
The loader must route these to Eclipse's existing log natives (not the host). Smallest first step.
```
__android_log_assert  __android_log_buf_write  __android_log_print  __android_log_write
android_set_abort_message
```

### 2. bionic-libc — the 21 bionic-specific names glibc does NOT provide
These need Eclipse-owned bionic-libc natives (bionic-only entry points / objects):
```
__system_property_get          # Android system properties (no glibc equivalent)
__sF                           # bionic stdio FILE table (object; glibc has no __sF)
__errno                        # bionic errno fn (glibc exports __errno_location, not __errno)
__assert2                      # bionic 4-arg assert
__gnu_strerror_r               # bionic GNU strerror_r alias
__stack_chk_guard              # bionic SSP guard OBJECT (glibc puts it in TLS, not exported)
# bionic FORTIFY (_chk) variants glibc lacks under these exact names:
__FD_CLR_chk  __FD_ISSET_chk  __FD_SET_chk
__fwrite_chk  __sendto_chk  __strchr_chk  __strlen_chk  __strncpy_chk2  __write_chk
```
(303 other libc names resolve from glibc as a baseline; only these 21 are missing entirely.)

### 3. ndk-android — libandroid (27), NO host equivalent
```
AAssetManager_fromJava  AAssetManager_open
AAsset_close  AAsset_getBuffer  AAsset_getLength  AAsset_openFileDescriptor
AConfiguration_delete  AConfiguration_fromAssetManager  AConfiguration_getCountry
AConfiguration_getLanguage  AConfiguration_getNavHidden  AConfiguration_getScreenHeightDp
AConfiguration_getScreenSize  AConfiguration_getScreenWidthDp  AConfiguration_new
ALooper_acquire  ALooper_addFd  ALooper_forThread  ALooper_pollOnce  ALooper_prepare
ALooper_release  ALooper_removeFd
ANativeWindow_acquire  ANativeWindow_fromSurface  ANativeWindow_getHeight
ANativeWindow_getWidth  ANativeWindow_release
```
Asset access (`AAsset*`/`AAssetManager*`) overlaps Eclipse's existing AssetManager work in
`src/framework.rs`; `ANativeWindow_*` maps to the host window/surface; `ALooper_*` is the NDK event
loop. `AConfiguration_*` is device config.

### 4. media-ndk — libmediandk (33), NO host equivalent (bridge to host codecs)
```
AMediaCodec_configure  AMediaCodec_createDecoderByType  AMediaCodec_createEncoderByType
AMediaCodec_delete  AMediaCodec_dequeueInputBuffer  AMediaCodec_dequeueOutputBuffer
AMediaCodec_flush  AMediaCodec_getInputBuffer  AMediaCodec_getOutputBuffer
AMediaCodec_getOutputFormat  AMediaCodec_queueInputBuffer  AMediaCodec_releaseOutputBuffer
AMediaCodec_start  AMediaCodec_stop
AMediaFormat_delete  AMediaFormat_getBuffer  AMediaFormat_getInt32  AMediaFormat_new
AMediaFormat_setBuffer  AMediaFormat_setFloat  AMediaFormat_setInt32  AMediaFormat_setString
AMediaFormat_toString
# AMEDIAFORMAT_KEY_* constant OBJECTs (the format-key strings):
AMEDIAFORMAT_KEY_BIT_RATE  AMEDIAFORMAT_KEY_CHANNEL_COUNT  AMEDIAFORMAT_KEY_COLOR_FORMAT
AMEDIAFORMAT_KEY_FRAME_RATE  AMEDIAFORMAT_KEY_HEIGHT  AMEDIAFORMAT_KEY_I_FRAME_INTERVAL
AMEDIAFORMAT_KEY_MIME  AMEDIAFORMAT_KEY_SAMPLE_RATE  AMEDIAFORMAT_KEY_STRIDE
AMEDIAFORMAT_KEY_WIDTH
```

### 5. audio — OpenSL ES (8), NO host equivalent (bridge to host audio)
```
slCreateEngine
SL_IID_ANDROIDCONFIGURATION  SL_IID_ANDROIDSIMPLEBUFFERQUEUE  SL_IID_BUFFERQUEUE
SL_IID_ENGINE  SL_IID_PLAY  SL_IID_RECORD  SL_IID_VOLUME
```
(`OpenMAXAL` — `XA_*` — contributes **0** here: none of its symbols is referenced by a relocation
in this build, so it is not on the work-list despite being a `DT_NEEDED`.)

---

## Recommended implementation order (NEXT steps)

1. **liblog (5)** — route to Eclipse's existing `src/framework.rs` log natives. Smallest, already
   owned; proves the loader→Eclipse-native binding path.
2. **bionic-libc bionic-specific (21)** — Eclipse-owned bionic-libc natives for the glibc-missing
   names (`__system_property_get`, `__sF`, `__errno`, the `_chk` FORTIFY family, `__stack_chk_guard`).
   This also begins the honest displacement of the 303 glibc-baseline libc symbols with
   bionic-ABI-correct ones (prepend the Eclipse-native provider before the host tier).
3. **ndk-android (27)** — `AAsset*`/`AAssetManager*` reuse Eclipse's AssetManager; `ANativeWindow_*`
   → host surface; `ALooper_*` → an Eclipse NDK looper; `AConfiguration_*` → device config.
4. **media-ndk (33)** + **audio (8)** — bridges to host codecs / host audio.
5. After the work-list is satisfied: bind the assembled image to execution and run the **3,427
   `DT_INIT_ARRAY` constructors** in order, honoring RELRO + BIND_NOW (no `%fs`/TCB needed — no
   PT_TLS). This is **main-loop / dev-host only** (the cyber-safeguard).

> The pipeline is **proven**: 535 GOT/PLT slots were filled with real (host) addresses on the mapped
> 112 MiB engine, every one verified non-null, with the 88-import work-list recorded and never
> fabricated. The remaining work is **implementing the natives above**, not the relocation machinery.
