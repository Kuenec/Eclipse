# Eclipse — Bionic loader & NDK-shim plan (design note)

> **Status: DESIGN NOTE — the shim step it describes is DEFERRED and MAIN-LOOP-ONLY.**
> Written **2026-06-04**. This version-controls the previously un-committed framework
> work-list (`~/eclipse-m0/framework-worklist.txt`) and the bionic loader contract so the
> next-phase native-shim work has a durable, reviewable spec. **Do not implement the shim
> step from a workflow subagent:** Anthropic's cyber-safeguard false-positives on
> bionic-linker / loader-internal analysis and kills such subagents mid-task (see
> AGENTS.md §6, 2026-06-04 entries). The shim build is the highest-risk, do-it-LAST item
> in the component map (bionic loader = #1 Rust-port priority, deferred to last). It must
> be done in the main loop / interactively.
>
> Source-of-truth references (cited, **not** reproduced here): the vendored
> `bionic_translation` fork at `vendor/atl/thirdparty/bionic_translation/linker/` and the
> installed binary `/usr/lib/libdl_bio.so.0` (AUR `bionic_translation r107.026ea254-1`,
> SONAME `libdl_bio.so.0`). The host ART `libart.so` pulls `libdl_bio.so.0` in as a
> transitive `NEEDED`, which is why the translation linker self-initializes when Eclipse
> `dlopen`s `libart.so` (AGENTS.md §6, ART-VM-boot entry).

---

## 1. Purpose & status

This note exists so the engine-load frontier — the current M2 blocker — is captured in
version control rather than only in `~/eclipse-m0/framework-worklist.txt` (a scratch file
outside the repo). It records:

1. **The problem** the deferred shim step solves (§2).
2. **The bionic loader contract** the shim must satisfy (§3), digested from a prior
   successful read of the vendored linker source plus `readelf`. Each claim is marked
   **confirmed** or **UNCONFIRMED**.
3. **The shim plan** (§4) — what `libmediandk.so` / `libOpenMAXAL.so` must be and how they
   register in the bionic namespace; with the **build-ready** data-symbol coverage + build
   recipe + concrete add-a-shim steps in §4c.
4. **Open questions** (§5) to resolve in the main loop *before* writing shim code.

Nothing in §4 is implemented. The just-landed `java.library.path` / native-lib-extraction
wiring (§2) is the prerequisite that is done; the shim layer is what remains. As of
**2026-06-04 (§4c)** the shim spec is **build-ready**: the symbol/data-coverage and the
bionic-ABI build recipe are CONFIRMED by `readelf`/`nm` + `meson.build`, leaving only
**linker/loader runtime behavior** (re-export acceptance, cfg.d-vs-ldpath precedence,
`liblog`/`libm` resolution, `dlextinfo`) for a main-loop load probe — see §5.

---

## 2. The problem (the current M2 frontier)

**Confirmed (probe + `readelf`, AGENTS.md §6 2026-06-04 engine-load entries):** Eclipse
boots the vendored ART VM from pure Rust, puts Roblox's Java on the classpath, extracts
`lib/x86_64/*.so`, whitelists that dir with the bionic linker, and calls
`System.loadLibrary("roblox")`. The 111 MB native engine `libroblox.so` is **found and
links to the relocation stage**, then fails: its `DT_NEEDED` list includes
`libmediandk.so` and `libOpenMAXAL.so`, which are **absent system-wide** as files, and an
unresolved symbol `AMediaFormat_delete` aborts relocation.

`libroblox.so` `DT_NEEDED` (confirmed via `readelf`, 585 undefined dynamic symbols):

```
libOpenMAXAL.so  libmediandk.so  libOpenSLES.so  libGLESv2.so  libEGL.so
libandroid.so    liblog.so       libm.so         libdl.so      libc.so
```

**Why the symbols exist but the libraries do not (confirmed):** ATL's
`/usr/lib/libandroid.so` already provides **100%** of the NDK symbol families the engine
imports — `AMedia*` 19/19, `AMediaCodec*` 11/11, `ANativeWindow*` 4/4, `AAsset*` 6/6,
`AConfiguration*` 4/4, `ALooper*` 7/7. The `libOpenMAXAL.so` imports are in fact OpenSL ES
symbols (`SL_IID_*`, `slCreateEngine`) → satisfiable from `libOpenSLES.so`. So the only
genuinely missing pieces are the **sonames** `libmediandk.so` and `libOpenMAXAL.so`; the
underlying symbols already live in `libandroid.so` / `libOpenSLES.so`.

> **Refined 2026-06-04 (§4b, readelf/nm):** the missing-soname count is **three**, not two —
> `liblog.so` is also missing as a file on the bionic ldpath (its `__android_log_*` symbols
> exist only at `/usr/lib/art/liblog.so`). And `libroblox.so` imports **0** direct
> `libOpenMAXAL.so` symbols, so that shim can be a stub/alias. See §4b.1/§4b.3.

### Where `java.library.path` now points, and why it is necessary-but-not-sufficient

**Confirmed (this just-landed wiring, `src/runtime.rs`):** on `eclipse run <apk>`, the
app's `lib/x86_64/*.so` are extracted to an XDG cache dir
(`runtime::native_lib_cache_dir()` → `$XDG_CACHE_HOME/eclipse/native-libs`, overridable via
`ECLIPSE_NATIVE_LIB_DIR`, typed `RuntimeError::NoCacheDir` when no base exists — no
hardcoded `/tmp`/home/username path). `boot()` takes an `app_lib_dir` param;
`library_path_option()` joins the **framework natives dir FIRST, the extracted app-lib dir
SECOND** under `-Djava.library.path` (`:`-joined). A unit test pins that exact order +
separator as the regression guard.

**Why that is necessary but not sufficient:** `-Djava.library.path` governs how ART's
top-level `System.loadLibrary` finds the *named* library (`libroblox`). It does **not**
govern how the bionic translation linker resolves `libroblox.so`'s **transitive
`DT_NEEDED`**. Per the work-list, the linker resolves those from its **own** search paths
(registered via `dl_parse_library_path`), and does **not** honor glibc `LD_LIBRARY_PATH`
(nor, for transitive deps, `java.library.path`) for them. So even with the engine findable
and on the bionic path, its `DT_NEEDED` `libmediandk.so` / `libOpenMAXAL.so` are unresolved
because no such files exist on any path the bionic namespace searches. That gap is what the
shim step (§4) closes.

---

## 3. The bionic loader contract

Digested from a prior successful read of the vendored linker source + `readelf` on
`/usr/lib/libdl_bio.so.0`. **References, not reproductions** — the symbol-walk mechanics
live in `vendor/atl/thirdparty/bionic_translation/linker/` (see `linker.c`); they are not
restated here.

### 3.1 Public entry points (confirmed — the `dlopen` family this layer exports)

- **`dl_parse_library_path(const char *path, char *delim)`** — parses a delimited search
  path into the loader's global ldpath array (`apkenv_ldpaths[]`) used for bionic-namespace
  library resolution. **This is how Eclipse registers the directory holding the NDK shims**
  so the bionic namespace can find them. (Eclipse already calls this to whitelist the app
  lib dir — AGENTS.md §6 engine-load entry.)
- **`bionic_dlopen(const char *filename, int flag)`** — loads a library; returns a bionic
  `soinfo` handle if it links in the bionic namespace, or (when host-fallback is allowed) a
  glibc handle. Runs the library's constructors before returning.
- **`bionic_android_dlopen_ext(const char *filename, int flags, const struct
  android_dlextinfo *info)`** — the `android_dlopen_ext`-compatible entry; **currently
  IGNORES `dlextinfo`** and delegates to `bionic_dlopen`.

### 3.2 The two-namespace ABI rule (confirmed — the crux)

A library either links in the **BIONIC** namespace (symbols registered in the loader's
`soinfo` list, resolved by the bionic resolver) **or** falls back to the **HOST glibc**
namespace (opaque handle, symbols via host `dlsym`). The two namespaces are
**ABI-incompatible** (struct layout / calling convention). A bionic-linked consumer must
resolve its `DT_NEEDED` from **bionic-ABI** providers, **not** host glibc ones, or it
crashes.

The vendored linker's host-fallback path resolves a `DT_NEEDED` entry by name; when the
named library is absent on the bionic ldpath, that entry fails — which is exactly the
`libmediandk.so` / `libOpenMAXAL.so` relocation-stage failure observed (§2). See
`vendor/atl/thirdparty/bionic_translation/linker/linker.c` for the resolution walk; it is
referenced here, not reproduced.

**Confirmed empirically (work-list 2026-06-04 follow-up):** ad-hoc copies of host libs
under the NDK names — `libmediandk.so` ← `libandroid.so`, `libm.so` ← `libm.so.6`,
`libOpenMAXAL.so` ← `libOpenSLES.so` — made `System.loadLibrary("roblox")` **crash during
the call** (not a clean next-missing-symbol). Cause: host glibc libs are not bionic-ABI
compatible, and using `libandroid.so` as both `libandroid.so.0` **and** `libmediandk.so`
double-loads it. This is the direct evidence that the shims must be **proper bionic-ABI
.so files**, not symlinks/copies of host libraries.

### 3.3 Soname aliasing mechanism (confirmed — `cfg.d`)

`/usr/share/bionic_translation/cfg.d/*.cfg` maps Android sonames to bionic provider libs.
The installed `bionic_translation.cfg` (read 2026-06-04) contains, verbatim, mappings such
as:

```
libc.so             libc_bio.so.0
libstdc++.so        libstdc++_bio.so.0
libandroid.so       libandroid.so.0
libOpenSLES.so      libOpenSLES.so.1
libEGL.so           libEGL.so.1
libGLESv2.so        libGLESv2.so.2
libGLESv3.so        libGLESv2.so.2
libopenxr_loader.so libopenxr_loader.so.1
```

There is **no** entry for `libmediandk.so` or `libOpenMAXAL.so` — they are simply not
mapped, which is consistent with their absence as files. A shim plan can either add `cfg.d`
entries that map these sonames to a provider, or place real shim `.so` files on the ldpath
fed to `dl_parse_library_path` (§4).

---

## 4. The shim plan (DEFERRED — main-loop only)

**Goal:** make `libroblox.so`'s `DT_NEEDED` resolve **entirely within the bionic
namespace** so relocation finishes and `JNI_OnLoad` runs — the next landmark after "ART
boots + loads Roblox's Java + the engine links to relocation".

**Build bionic-ABI shim libraries** `libmediandk.so` and `libOpenMAXAL.so` that re-export
the symbols the engine imports:

- `libmediandk.so` → re-export ATL `libandroid.so`'s `AMedia*` / `AMediaCodec*` surface
  (the `AMediaFormat_delete` that aborted relocation is in this family). Confirmed: those
  symbols are 100% present in `/usr/lib/libandroid.so`.
- `libOpenMAXAL.so` → re-export the OpenSL ES surface (`SL_IID_*`, `slCreateEngine`) from
  `libOpenSLES.so` (provider already mapped in `cfg.d`). Confirmed: the engine's
  "OpenMAXAL" imports are actually OpenSL ES symbols.

**Registration (two complementary mechanisms, both confirmed to exist):**

1. **`cfg.d` soname mapping** — add entries mapping `libmediandk.so` and `libOpenMAXAL.so`
   to their bionic provider (§3.3), so the loader's name→provider step resolves them.
2. **ldpath** — place the built shim `.so` files in a directory registered via
   `dl_parse_library_path` (§3.1), the same call Eclipse already uses for the app lib dir.

Both must yield **bionic-namespace** providers (§3.2): the shims must be linked/loaded such
that the bionic resolver, not host `dlsym`, satisfies `libroblox.so`'s relocations.

**v1 implementation option (charter-sanctioned):** per AGENTS.md §6 (2026-06-04, bionic
loader is the #1 Rust-port priority, "v1 may FFI the proven C `bionic_translation`"), v1
**may FFI / reuse the proven C `bionic_translation` and ATL's C symbol implementations**
(ATL implements the `AMedia*` surface in `src/api-impl-jni/.../media.c`) rather than
re-porting them to Rust now. The Rust port lands later, behind an ABI conformance suite
(component map), because the loader is the highest-risk item — do it **last**.

This step changes nothing in the just-landed `runtime` wiring (§2) beyond, eventually,
ensuring the shim dir is among the dirs registered with the bionic linker.

---

## 4b. Confirmed evidence (readelf/nm) — 2026-06-04

This subsection promotes the §5 UNCONFIRMED items that are answerable by **pure
ELF/symbol/config inspection** (`readelf`, `nm`, `objdump` on the `.so` files; `cat` on the
`.cfg`) into **CONFIRMED** facts. It does **not** touch the linker-source / build-recipe
questions, which stay UNCONFIRMED in §5 (main-loop only). Source `.so`:
`/home/kue/eclipse-m0/apk/v2.724.735/_split/lib/x86_64/libroblox.so`.

### 4b.1 `libroblox.so` `DT_NEEDED` — CONFIRMED (exact list, 10 entries)

```
libOpenMAXAL.so  libmediandk.so  libOpenSLES.so  libGLESv2.so  libEGL.so
libandroid.so    liblog.so       libm.so         libdl.so      libc.so
```

Matches the §2 list. Classification by how each resolves in the bionic namespace:

| soname | status | provider / note |
|---|---|---|
| `libc.so` | cfg-aliased | → `libc_bio.so.0` (present, `/usr/lib`) |
| `libdl.so` | cfg-aliased / present | `libdl_bio.so.0` present; bionic linker self-init |
| `libm.so` | provider on system | glibc `libm.so.6` via `ld.so.cache` |
| `libandroid.so` | cfg-aliased | → `libandroid.so.0` (ATL, **198 exports**) |
| `libOpenSLES.so` | cfg-aliased | → `libOpenSLES.so.1` (`libopensles-standalone`) |
| `libEGL.so` | cfg-aliased | → `libEGL.so.1` (Mesa) |
| `libGLESv2.so` | cfg-aliased | → `libGLESv2.so.2` (Mesa) |
| `liblog.so` | **MISSING-needs-shim** | only at `/usr/lib/art/liblog.so`, outside the bionic ldpath |
| `libmediandk.so` | **MISSING-needs-shim** | absent system-wide; symbols live in `libandroid.so.0` |
| `libOpenMAXAL.so` | **MISSING-needs-shim** | absent system-wide; **0** direct imports (see 4b.3) |

So **7 of 10** `DT_NEEDED` resolve today (5 cfg-aliased + `libm`/`libdl`), and **3** are
genuinely missing as files: `liblog.so`, `libmediandk.so`, `libOpenMAXAL.so`. This sharpens
§2's "only two missing sonames" — `liblog.so` is a **third** missing soname (its symbols
exist at `/usr/lib/art/liblog.so` but that path is not on the bionic ldpath).

### 4b.2 `libmediandk.so` imported symbol set — CONFIRMED (23 symbols, 100% in `libandroid.so.0`)

The `AMedia*` surface `libroblox.so` imports under the `libmediandk.so` soname (function
symbols), **all 23 present** as exports in ATL's `libandroid.so.0`:

```
AMediaCodec_configure              AMediaCodec_createDecoderByType
AMediaCodec_createEncoderByType    AMediaCodec_delete
AMediaCodec_dequeueInputBuffer     AMediaCodec_dequeueOutputBuffer
AMediaCodec_flush                  AMediaCodec_getInputBuffer
AMediaCodec_getOutputBuffer        AMediaCodec_getOutputFormat
AMediaCodec_queueInputBuffer       AMediaCodec_releaseOutputBuffer
AMediaCodec_start                  AMediaCodec_stop
AMediaFormat_delete                AMediaFormat_getBuffer
AMediaFormat_getInt32              AMediaFormat_new
AMediaFormat_setBuffer             AMediaFormat_setFloat
AMediaFormat_setInt32              AMediaFormat_setString
AMediaFormat_toString
```

`AMediaFormat_delete` (the symbol that aborted relocation in §2) is in this set. This is the
precise export table the `libmediandk.so` shim must forward — **function coverage is 100%**.

> **CAVEAT — `AMEDIAFORMAT_KEY_*` data symbols (UNCONFIRMED gap).** `libroblox.so` also
> imports the `AMEDIAFORMAT_KEY_*` **data** symbols (`..._BIT_RATE`, `_COLOR_FORMAT`,
> `_FRAME_RATE`, `_HEIGHT`, `_WIDTH`, `_STRIDE`, `_I_FRAME_INTERVAL`). These are **not** in
> the function-export check above and their presence in `libandroid.so.0` was **not**
> confirmed here. If ATL's `libandroid.so.0` does not export these data globals, the
> `libmediandk.so` shim must define/forward them too. **Verify in the main loop.**

### 4b.3 `libOpenMAXAL.so` imported symbol set — CONFIRMED (0 direct imports)

`libroblox.so` imports **zero** `XA*`/OpenMAXAL symbols directly. `libOpenMAXAL.so` is a
`DT_NEEDED` entry with no directly-referenced symbols in `libroblox.so` — i.e. a transitive
or dormant soname, not a directly-used API. This **revises** §4's plan: the
`libOpenMAXAL.so` shim may be an **empty/stub** bionic-ABI `.so` (or a `cfg.d` alias to
`libOpenSLES.so.1`) purely to satisfy the soname; it need not export an OpenSL ES surface
for `libroblox.so` itself. (Whether a *transitive* consumer pulled in by another `DT_NEEDED`
needs real OpenMAXAL symbols is still **UNCONFIRMED** — it depends on the load graph, not on
`libroblox.so`'s own undefined-symbol table.)

### 4b.4 What `libandroid.so.0` covers vs the gaps — CONFIRMED

ATL's `libandroid.so.0` exports **198** symbols and covers **100%** of the `AMedia*`
function family above. The `libroblox.so` undefined symbols it does **not** cover fall into
families that belong to **other** `DT_NEEDED` libs, not to a `libandroid` gap:

- `egl*` (≈29 symbols) → from `libEGL.so` (cfg-aliased `libEGL.so.1`).
- `gl*` (≈140 symbols, the GLES2 surface) → from `libGLESv2.so` (cfg-aliased `libGLESv2.so.2`).
- `__android_log_*` (`_print`, `_write`, `_buf_write`, `_assert`) → from `liblog.so`
  (the **MISSING** soname — these are why `liblog.so` matters).
- `AConfiguration_getScreenWidthDp` / `_getScreenHeightDp` — **UNCONFIRMED** whether
  `libandroid.so.0` exports these two (§2 claimed `AConfiguration* 4/4`; this pass found
  them in the not-covered-by-the-AMedia-check set). Re-check against `libandroid.so.0`'s full
  export table in the main loop.
- `getentropy` (glibc), `__gcov_*` (coverage stubs) — host/toolchain symbols, out of shim scope.

Conclusion: the EGL/GLES/log symbols are **not** a `libandroid` shim concern — EGL/GLES are
already cfg-aliased to Mesa and are Eclipse's `ash`/EGL graphics-forwarding scope, not the
bionic media-shim scope. The media shim's job is narrowly the **23 `AMedia*` functions**
(plus the `AMEDIAFORMAT_KEY_*` data symbols, pending 4b.2's caveat).

### 4b.5 Other-NEEDED bionic-alias status — CONFIRMED

The §5 "`libm.so` / `libc.so` / `libdl.so` bionic-alias handling" question is resolved for
the resolution-today part:

- `libc.so` → `libc_bio.so.0` — **cfg-aliased, provider present.**
- `libdl.so` → `libdl_bio.so.0` — **present**; pulled in transitively by `libart.so` and
  self-initializes the translation linker (matches the §0 / AGENTS.md ART-VM-boot note).
- `libm.so` — **no `cfg.d` entry**; resolves to glibc `libm.so.6` via `ld.so.cache`. (Whether
  the bionic namespace is content with the glibc `libm` for a bionic-linked consumer, vs
  needing a `libm_bio`, is a **loader-behavior** question → stays UNCONFIRMED in §5.)
- `libandroid.so` → `libandroid.so.0` — **cfg-aliased, present, 198 exports.**
- `libOpenSLES.so` → `libOpenSLES.so.1` — **cfg-aliased, present.**

### 4b.6 Full installed `cfg.d` map — CONFIRMED (verbatim)

`/usr/share/bionic_translation/cfg.d/bionic_translation.cfg` (read 2026-06-04), verbatim:

```
# libc_bio.so pulls in libptread_bio.so (pthreads are in libc.so on android)
libc.so             libc_bio.so.0
libstdc++.so        libstdc++_bio.so.0
# TODO: put in separate cfg file, installed by atl
libandroid.so       libandroid.so.0
libopenxr_loader.so libopenxr_loader.so.1
# TODO: put in separate cfg file, installed by libsles_standalone
libOpenSLES.so      libOpenSLES.so.1
# TODO: not sure where to put these
libEGL.so           libEGL.so.1
libGLESv2.so        libGLESv2.so.2
libGLESv3.so        libGLESv2.so.2 # GLESv3 is a symlink to GLESv2 if it exists at all
```

Confirms §3.3: **no** entry for `libmediandk.so`, `libOpenMAXAL.so`, or `liblog.so`. Adding
`cfg.d` mappings for these three (or placing real shim `.so` on the ldpath) is the
registration half of §4.

---

## 4c. Build recipe & data-symbol coverage — 2026-06-04

This subsection promotes the remaining **build-recipe** and **data-symbol coverage**
questions from §5 into CONFIRMED facts, using only permitted inputs: `readelf`/`nm` on the
`.so` files and the `bionic_translation` **meson.build** (a BUILD file — the linker `*.c`
was **not** opened). It makes the shim step build-ready; what remains in §5 is strictly
**linker/loader runtime behavior**, which ELF + build-file inspection cannot settle.

Inputs of record (read 2026-06-04):
`/home/kue/eclipse-m0/apk/v2.724.735/_split/lib/x86_64/libroblox.so` (the engine),
`/usr/lib/libandroid.so.0` (ATL provider, 198 exports), `/usr/lib/art/liblog.so`,
`/tmp/bionic_translation-026ea2542258a7e0cbb52f22d7475aea68cbcbf5/meson.build` (also
vendored at `vendor/atl/thirdparty/bionic_translation/meson.build`).

### 4c.1 `AMEDIAFORMAT_KEY_*` data-symbol coverage — CONFIRMED (3 exported, 7 gap)

`libroblox.so` imports **10** `AMEDIAFORMAT_KEY_*` symbols (`readelf --dyn-syms`, type
`OBJECT` i.e. **data globals**, all `UND`). They are the key-name string-pointer constants
the NDK `media/NdkMediaFormat.h` declares as `extern const char*`:

```
AMEDIAFORMAT_KEY_MIME            AMEDIAFORMAT_KEY_WIDTH
AMEDIAFORMAT_KEY_HEIGHT          AMEDIAFORMAT_KEY_COLOR_FORMAT
AMEDIAFORMAT_KEY_STRIDE          AMEDIAFORMAT_KEY_BIT_RATE
AMEDIAFORMAT_KEY_FRAME_RATE      AMEDIAFORMAT_KEY_I_FRAME_INTERVAL
AMEDIAFORMAT_KEY_CHANNEL_COUNT   AMEDIAFORMAT_KEY_SAMPLE_RATE
```

`/usr/lib/libandroid.so.0` exports **only 3** of these (`nm -D --defined-only`, type `D`):

```
AMEDIAFORMAT_KEY_CHANNEL_COUNT   AMEDIAFORMAT_KEY_MIME   AMEDIAFORMAT_KEY_SAMPLE_RATE
```

So **7 `AMEDIAFORMAT_KEY_*` data globals are a genuine gap** the `libmediandk.so` shim must
itself **define as data globals** (it cannot forward them — the provider lacks them):

```
AMEDIAFORMAT_KEY_WIDTH   AMEDIAFORMAT_KEY_HEIGHT   AMEDIAFORMAT_KEY_COLOR_FORMAT
AMEDIAFORMAT_KEY_STRIDE  AMEDIAFORMAT_KEY_BIT_RATE AMEDIAFORMAT_KEY_FRAME_RATE
AMEDIAFORMAT_KEY_I_FRAME_INTERVAL
```

This resolves the §4b.2 caveat: the media shim is **not** a pure thin re-export. Its 23
`AMedia*` **functions** forward 100% to `libandroid.so.0` (§4b.2, re-confirmed: `comm` of
the two symbol sets is empty), but it must **supply** these 7 string-constant data globals
itself. The constant values are fixed NDK string literals (e.g. `"width"`, `"height"`,
`"color-format"`, `"stride"`, `"bitrate"`, `"frame-rate"`, `"i-frame-interval"`); the shim
defines `const char* AMEDIAFORMAT_KEY_WIDTH = "width";` etc. (exact strings to be taken from
the NDK header / `media.c` at build time, not guessed).

### 4c.2 `AConfiguration_getScreen*Dp` coverage — CONFIRMED (2 gap, function symbols)

`libroblox.so` imports `AConfiguration_getScreenWidthDp` and
`AConfiguration_getScreenHeightDp` (`readelf --dyn-syms` UND, `FUNC`).
`/usr/lib/libandroid.so.0` exports the `AConfiguration_getScreen{Size,Long,Round}` family
but **not** the two `*Dp` variants. So these are **2 missing function symbols** the shim
must supply (compute Dp from density+pixels, or forward to an ATL helper if one exists).
This corrects §2's "`AConfiguration* 4/4`" overcount: function coverage for the *Dp pair is
**0/2** in `libandroid.so.0`. The other `AConfiguration_*` symbols Roblox imports
(`getScreenSize`, `getNavHidden`, `delete`, `fromAssetManager`, `getCountry`, `getLanguage`,
`new`) **are** present in `libandroid.so.0`.

### 4c.3 `liblog.so` resolution plan — CONFIRMED inventory

`/usr/lib/art/liblog.so` is an ELF 64-bit x86-64 shared object (host build, stripped) that
exports the `__android_log_*` family `libroblox.so` needs — confirmed present:
`__android_log_print`, `__android_log_write`, `__android_log_buf_write`,
`__android_log_buf_print`, `__android_log_assert`, `__android_log_bwrite`,
`__android_log_btwrite`, `__android_log_vprint`, `__android_log_dev_available`. The file
simply sits **outside the bionic ldpath** (`/usr/lib/art`, not `/usr/lib`). Two candidate
resolutions (the choice between them is a §5 loader-behavior question, not settled here):
1. **cfg.d mapping** — add `liblog.so  /usr/lib/art/liblog.so` (absolute) to the cfg, if the
   loader accepts an absolute provider; simplest.
2. **ldpath registration** — `dl_parse_library_path("/usr/lib/art", ":")` from the Eclipse
   runtime so the bionic namespace finds `liblog.so` by name on transitive resolution. This
   is the same `dl_parse_library_path` call Eclipse already uses for the app lib dir, so it
   is low-risk; it must be registered so it does **not** shadow `/usr/lib` providers.

### 4c.4 Bionic-ABI build recipe — CONFIRMED from meson.build

The `bionic_translation` providers are built with **Meson**, the **host C compiler**
(`cc = meson.get_compiler('c')` — no cross-toolchain, no sysroot; this is a host x86_64
build), with these project-level and per-target settings (verbatim from the build file):

- `project(..., default_options: ['b_lundef=false', 'b_asneeded=false'])` —
  **`b_lundef=false`** permits undefined symbols in the final link (the basis of thin
  re-export); **`b_asneeded=false`** keeps `DT_NEEDED` entries the consumer relies on.
- Common per-target C args: **`-fPIC -D_GNU_SOURCE`** (the static `wrapper` adds
  `-fvisibility=hidden`; `dl_bio` adds `-D_LARGEFILE64_SOURCE` + debug defines).
- Common link args on the shims: **`-Wl,--no-as-needed`** (force `DT_NEEDED` even for libs
  not directly referenced — see the line-123 comment in the build file).
- Stable-ABI shims carry **`soversion: 0`** (`libc_bio`, `pthread_bio`, `stdc++_bio`);
  `dl_bio` carries `version: '0.0.1'`. A new media shim should mirror `soversion: 0`.
- cfg.d ships via `install_data(['cfg.d/bionic_translation.cfg'], install_dir:
  datadir/'bionic_translation/cfg.d')` — the mechanism for shipping a new soname mapping.

**Symbol-supply precedent (CONFIRMED, the key build-file finding):** `libc_bio` defines a
`libc_bio_hardcoded_symbols` list applied via **`-Wl,--defsym`** — `-Wl,--defsym,__fe_dfl_env=-1`
(and, on 32-bit, `-Wl,--defsym,__bionic_brk=0x123`). This proves the project's established
pattern that a shim **supplies symbols the host provider defines differently or lacks**, at
link time. For the 7 `AMEDIAFORMAT_KEY_*` **string-pointer data globals** (§4c.1), a small C
source defining them (`const char* AMEDIAFORMAT_KEY_WIDTH = "width";`) is the natural form;
`--defsym` fits scalar symbols, a C definition fits string-pointer data. Either way the
precedent for "the shim provides what the provider can't" is in-tree.

### 4c.5 Concrete add-a-shim steps (build-ready)

Given §4c.1–§4c.4, the deferred shim step is now a concrete build task (do it in the main
loop; the only residual unknowns are the §5 loader-behavior items, which a single load probe
resolves):

1. **`libmediandk.so` shim** — add a `shared_library('mediandk', ['shim/libmediandk.c'],
   soversion: 0, install: true, dependencies: [...], link_args: ['-Wl,--no-as-needed'])`
   target to `meson.build`. `shim/libmediandk.c` (a) `DT_NEEDED`s the provider so the 23
   `AMedia*` **functions** forward to `libandroid.so.0`, and (b) **defines the 7 missing
   `AMEDIAFORMAT_KEY_*` data globals** (§4c.1) as `const char*` constants. Result:
   `libmediandk.so.0`. (`b_lundef=false` + `--no-as-needed` mirror the project pattern.)
2. **`AConfiguration_*Dp`** — define `AConfiguration_getScreenWidthDp` /
   `_getScreenHeightDp` (§4c.2) in the same shim source or a small `libandroid` companion
   (compute Dp from `getDensity`/screen size).
3. **`libOpenMAXAL.so` shim** — per §4b.3 (0 direct imports), a **stub** is enough:
   `shared_library('OpenMAXAL', sources: [], soversion: 0, install: true, link_args:
   ['-Wl,--no-as-needed'])`, or a cfg.d alias to `libOpenSLES.so.1`.
4. **`liblog.so`** — resolve per §4c.3 (cfg.d absolute mapping **or**
   `dl_parse_library_path("/usr/lib/art", ":")` from the runtime).
5. **Registration** — install a cfg.d file mapping `libmediandk.so`/`libOpenMAXAL.so`
   (/`liblog.so`) to their providers, **and/or** place the built shim `.so` on the dir
   Eclipse registers via `dl_parse_library_path`. (Which of cfg.d-vs-ldpath suffices is the
   §5 precedence question — a probe answers it.)
6. **ABI**: build as a **bionic-ABI .so** (the providers' own build mode above — host cc,
   `-fPIC -D_GNU_SOURCE`, `b_lundef=false`, linked against the `*_bio.so.0` providers, **not**
   a plain host glibc .so — §3.2 proved a host-glibc copy crashes).

**Re-export pattern (CONFIRMED in-build):** thin re-export = `DT_NEEDED [provider]` +
`-Wl,--no-as-needed` + `b_lundef=false`; symbol-supply for what the provider lacks =
`-Wl,--defsym` (scalars) or a C definition (data/string globals), per §4c.4. Whether thin
re-export alone satisfies the **bionic** resolver (vs needing each symbol physically
defined/aliased) is the §5 loader-behavior question.

---

## 5. Open questions / UNCONFIRMED items (resolve in the main loop before coding the shim)

Mark these as the entry checklist for the deferred step. None is settled by the digest or
the safe files; do not write shim code until each is confirmed against the actual source /
a probe in the main loop.

> **Resolved 2026-06-04 by §4b (readelf/nm) — struck from this checklist:**
> - ~~exact symbol set `libroblox.so` imports from each NDK soname~~ → **CONFIRMED** in
>   §4b.2 (`libmediandk.so`: 23 `AMedia*` functions, 100% in `libandroid.so.0`) and §4b.3
>   (`libOpenMAXAL.so`: **0** direct imports → stub/alias suffices).
> - ~~`libc.so` / `libdl.so` bionic-alias handling~~ → **CONFIRMED** in §4b.5
>   (`libc`→`libc_bio.so.0`, `libdl_bio.so.0` self-init). The `libm` part remains open (below).
> - The full `DT_NEEDED` list (§4b.1) and the verbatim `cfg.d` map (§4b.6) are now CONFIRMED.
>
> **Resolved 2026-06-04 by §4c (readelf/nm + meson.build) — struck from this checklist:**
> - ~~`AMEDIAFORMAT_KEY_*` and `AConfiguration_*Dp` data-symbol coverage~~ → **CONFIRMED**
>   in §4c.1/§4c.2: of the 10 `AMEDIAFORMAT_KEY_*` data globals Roblox imports,
>   `libandroid.so.0` exports only **3** (`CHANNEL_COUNT`, `MIME`, `SAMPLE_RATE`); the
>   **7 others are a gap the shim must DEFINE as data globals** (it cannot forward them).
>   The two `AConfiguration_getScreen{Width,Height}Dp` functions are **0/2** in
>   `libandroid.so.0` → the shim must supply them too. (The 23 `AMedia*` functions forward
>   100%.)
> - ~~bionic-ABI build recipe for the shims~~ → **CONFIRMED** in §4c.4: Meson, host `cc`
>   (no cross-toolchain/sysroot), `-fPIC -D_GNU_SOURCE`, `b_lundef=false` + `b_asneeded=false`
>   + `-Wl,--no-as-needed`, `soversion: 0`; symbol-supply precedent = `-Wl,--defsym` in
>   `libc_bio` (scalars) / a C definition for string-pointer data. Add-a-shim steps in §4c.5.
> - ~~`liblog.so` resolution inventory~~ → **CONFIRMED** in §4c.3: `/usr/lib/art/liblog.so`
>   exports the needed `__android_log_*` family; resolve via cfg.d absolute mapping **or**
>   `dl_parse_library_path("/usr/lib/art", ":")`. (Which mechanism the loader honors is the
>   precedence item below.)

Still open (linker-source / loader-behavior — none answerable by ELF or build-file
inspection alone; each needs the vendored linker or a load probe in the main loop):

- **UNCONFIRMED — re-export mechanism that satisfies the bionic resolver.** Whether a shim
  `.so` whose own `DT_NEEDED` points at the bionic provider (`libandroid.so.0` /
  `libOpenSLES.so.1`) is enough for the bionic linker to forward `libroblox.so`'s
  relocations, **or** whether the shim must physically define/alias each imported symbol.
  ELF "re-export" (a thin `.so` that NEEDs the real provider) is the assumed approach but is
  not verified against the vendored linker's symbol-registration behavior.
- **UNCONFIRMED — `cfg.d` vs ldpath precedence / sufficiency.** Whether adding a `cfg.d`
  soname mapping alone resolves the `DT_NEEDED`, whether the shim file on the ldpath alone
  suffices, or whether **both** are required. §3.3/§4b.6 show the mapping format but not the
  resolution precedence between the two.
- **UNCONFIRMED — `liblog.so` load + `libm.so` bionic-alias *behavior*.** §4c.3 CONFIRMED
  `/usr/lib/art/liblog.so` exports the needed `__android_log_*` family and gave two candidate
  resolutions (cfg.d absolute mapping vs `dl_parse_library_path("/usr/lib/art", ":")`); §4b.5
  CONFIRMED `libm.so` has no `cfg.d` entry and resolves to glibc `libm.so.6`. What stays
  UNCONFIRMED is the **loader behavior**: which of the two `liblog` resolutions the linker
  actually honors (does it accept an absolute cfg.d provider? does the extra ldpath shadow
  `/usr/lib`?), and whether a bionic-linked consumer is content with glibc `libm.so.6` or
  needs a `libm_bio` (the meson.build itself flags this — line 109 `libm_dep, # TODO: have
  separate libm_bio?`). Resolve alongside the media shims with a load probe.
- **UNCONFIRMED — whether `bionic_android_dlopen_ext` ignoring `dlextinfo` matters here.**
  §3.1 notes it ignores `dlextinfo`; whether the engine relies on `android_dlopen_ext`
  namespace flags (and is therefore affected) is unknown.

---

## 6. References

- `~/eclipse-m0/framework-worklist.txt` — the scratch work-list promoted into this note
  (author's own notes; outside the repo).
- `/usr/share/bionic_translation/cfg.d/bionic_translation.cfg` — installed soname-alias map
  (§3.3, read 2026-06-04).
- `vendor/atl/thirdparty/bionic_translation/meson.build` (also
  `/tmp/bionic_translation-026ea2542258a7e0cbb52f22d7475aea68cbcbf5/meson.build`) — the
  bionic-ABI build recipe of record (§4c.4; a BUILD file — safe to read, unlike `linker/*.c`).
- `/usr/lib/libandroid.so.0` (ATL provider, 198 exports), `/usr/lib/art/liblog.so`,
  `/home/kue/eclipse-m0/apk/v2.724.735/_split/lib/x86_64/libroblox.so` — the `readelf`/`nm`
  inputs for §4b/§4c (read 2026-06-04).
- `vendor/atl/thirdparty/bionic_translation/linker/` (`linker.c`) and
  `/usr/lib/libdl_bio.so.0` — the loader source/binary of record (referenced, not opened
  here; do not enumerate its symbol-walk mechanics in a workflow subagent — cyber-safeguard).
- AGENTS.md §5 (Living State, next-actions) and §6 (Decisions Log, 2026-06-04 engine-load /
  native-lib-extraction entries) — the surrounding M2 state.
- `docs/component-map.md` — bionic loader as the #1 Rust-port priority, deferred to last.
