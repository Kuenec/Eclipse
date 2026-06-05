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
   register in the bionic namespace.
4. **Open questions** (§5) to resolve in the main loop *before* writing shim code.

Nothing in §4 is implemented. The just-landed `java.library.path` / native-lib-extraction
wiring (§2) is the prerequisite that is done; the shim layer is what remains.

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

## 5. Open questions / UNCONFIRMED items (resolve in the main loop before coding the shim)

Mark these as the entry checklist for the deferred step. None is settled by the digest or
the safe files; do not write shim code until each is confirmed against the actual source /
a probe in the main loop.

- **UNCONFIRMED — re-export mechanism that satisfies the bionic resolver.** Whether a shim
  `.so` whose own `DT_NEEDED` points at the bionic provider (`libandroid.so.0` /
  `libOpenSLES.so.1`) is enough for the bionic linker to forward `libroblox.so`'s
  relocations, **or** whether the shim must physically define/alias each imported symbol.
  ELF "re-export" (a thin `.so` that NEEDs the real provider) is the assumed approach but is
  not verified against the vendored linker's symbol-registration behavior.
- **UNCONFIRMED — `cfg.d` vs ldpath precedence / sufficiency.** Whether adding a `cfg.d`
  soname mapping alone resolves the `DT_NEEDED`, whether the shim file on the ldpath alone
  suffices, or whether **both** are required. §3.3 shows the mapping format but not the
  resolution precedence between the two.
- **UNCONFIRMED — exact symbol set `libroblox.so` imports from each NDK soname.** The
  *families* are known to be fully covered by ATL (§2), but the precise per-soname imported
  symbol list (to size each shim's export table and avoid missing a relocation) must be read
  from `libroblox.so` with `readelf` in the main loop.
- **UNCONFIRMED — `libm.so` / `libc.so` / `libdl.so` bionic-alias handling.** The work-list
  notes `libm.so` needs a bionic alias/fallback (host has `libm.so.6`). `cfg.d` maps
  `libc.so`→`libc_bio.so.0` but has **no** `libm.so` entry. Whether `libm`/`libdl`/`liblog`
  already resolve in the bionic namespace for `libroblox.so` (vs needing an added mapping)
  is unconfirmed and must be checked at the same time as the media shims.
- **UNCONFIRMED — bionic-ABI build recipe for the shims.** How to compile a "proper
  bionic-ABI .so" (vs a host glibc .so, which §3.2 proved crashes) on this host — toolchain,
  whether ATL ships a build mode for translation-layer providers, or whether linking against
  the `*_bio.so.0` providers is sufficient. This is the practical blocker and is entirely
  unverified.
- **UNCONFIRMED — whether `bionic_android_dlopen_ext` ignoring `dlextinfo` matters here.**
  §3.1 notes it ignores `dlextinfo`; whether the engine relies on `android_dlopen_ext`
  namespace flags (and is therefore affected) is unknown.

---

## 6. References

- `~/eclipse-m0/framework-worklist.txt` — the scratch work-list promoted into this note
  (author's own notes; outside the repo).
- `/usr/share/bionic_translation/cfg.d/bionic_translation.cfg` — installed soname-alias map
  (§3.3, read 2026-06-04).
- `vendor/atl/thirdparty/bionic_translation/linker/` (`linker.c`) and
  `/usr/lib/libdl_bio.so.0` — the loader source/binary of record (referenced, not opened
  here; do not enumerate its symbol-walk mechanics in a workflow subagent — cyber-safeguard).
- AGENTS.md §5 (Living State, next-actions) and §6 (Decisions Log, 2026-06-04 engine-load /
  native-lib-extraction entries) — the surrounding M2 state.
- `docs/component-map.md` — bionic loader as the #1 Rust-port priority, deferred to last.
