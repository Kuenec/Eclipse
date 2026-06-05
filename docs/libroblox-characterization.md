# libroblox.so characterization (x86-64 engine) — 2026-06-05

Intel for Eclipse's own bionic loader, gathered by parsing the **bytes** of the APK-shipped
x86-64 native libraries with Eclipse's own `src/loader/elf.rs` decoder (benign data parse — no
exec, no mmap), cross-checked against `readelf`/`llvm-readelf` on an extracted copy. Every number
below is a **real parsed value**, not an estimate.

- **APK:** `~/eclipse-m0/apk/v2.724.735/roblox-2.724.735-merged.apk` (224,452,121 bytes).
- **Only ABI shipped:** `lib/x86_64/` (the merged single-arch APK — no arm64/armv7 here). The
  x86-64 engine path is correct: Eclipse + its reloc core are x86-64.
- **Tools:** `eclipse::loader::elf::ElfImage` (own decoder); cross-check `readelf 2.46`,
  `llvm-readelf` (the latter decodes Android-packed `APS2` relocations, which GNU readelf cannot).

---

## 1. lib/x86_64/*.so entries (names + sizes, all stored uncompressed)

| entry | size (bytes) |
|---|---:|
| **libroblox.so** | **111,823,960** (~111 MiB) |
| libbacktrace-native.so | 5,896,832 |
| libzstd-jni-1.5.7-6.so | 726,088 |
| librenderscript-toolkit.so | 388,520 |
| libeigen_blas.so | 249,184 |
| libimage_processing_util_jni.so | 49,496 |
| libeigen_lapack.so | 4,032 |
| libsurface_util_jni.so | 4,912 |
| libtrampoline.so | 4,920 |
| libdatastore_shared_counter.so | 6,224 |
| libyuv_shared.so | 3,752 |

`lib/x86_64/libroblox.so` **is present** (the ~111 MB engine, confirmed). All entries are
`Stored` (uncompressed → directly mmap-able).

---

## 2. libroblox.so — headline characterization

### ELF identity
- **ELFCLASS64 / EM_X86_64 / ET_DYN** (little-endian) — confirmed (an `ElfImage::parse` success
  enforces all four; `readelf -h` agrees).
- Built with **NDK r28c**, **Android API level 26** (`.note.android.ident`).
- **SONAME** = `libroblox.so`.

### Segments
- **3 × PT_LOAD** (`R-X` / `RW` / `RW`).
- Mapped span: vaddr `0x0 .. 0x70b4a80` → **~112.7 MiB** of address space (memsz sum `0x70ad480`).
- **PT_TLS: ABSENT.** libroblox.so has **no thread-local-storage template**.
- **PT_GNU_RELRO: present** (`0x65af000`, memsz `0x4ad000`) → the loader must `mprotect` that
  range read-only after relocation.
- **BIND_NOW: yes** (`DT_FLAGS = BIND_NOW` + `DT_FLAGS_1 = DF_1_NOW`) → all relocations,
  including `.rela.plt`, must be applied eagerly at load.

### DT_NEEDED (first-level bionic dependency graph) — 10 libs
```
libOpenMAXAL.so  libmediandk.so  libOpenSLES.so  libGLESv2.so  libEGL.so
libandroid.so    liblog.so       libm.so         libdl.so      libc.so
```
**None of these ship in `lib/x86_64/`** — every one is a bionic/NDK system library the
**bionic environment must provide** (the shim surface). Split by responsibility:

| NEEDED | who provides it | notes |
|---|---|---|
| libc.so, libm.so, libdl.so | bionic shim (Eclipse-owned) | core C runtime + math + dlfcn |
| liblog.so | bionic shim | `__android_log_print` (already process-global via the RTLD_GLOBAL fix, §5) |
| libandroid.so | bionic shim / NDK | NDK `AAssetManager_*`, `ANativeWindow_*`, `ALooper_*`, `AConfiguration_*`, `AInputQueue` |
| libEGL.so, libGLESv2.so | **host GL forwarding** | the engine renders via GLES2/EGL (NOT Vulkan — 0 `vk*` imports) |
| libOpenSLES.so, libOpenMAXAL.so | bionic shim / host audio | OpenSL ES + OpenMAX AL audio |
| libmediandk.so | bionic shim / host media | `AMediaCodec_*`/`AMediaFormat_*` (23 imports) |

> Historical note: `libmediandk.so` + `libOpenMAXAL.so` were exactly the libs the old
> `framework-worklist.txt` flagged as absent system-wide (§5). They are now confirmed as
> first-level `DT_NEEDED` of the engine itself, not optional.

### Relocations — REAL histogram (whole object)
`.rela.dyn` is **Android-packed (`SHT_ANDROID_RELA`, magic `APS2`)** at `DT_ANDROID_RELA`
(`0x60000011`) / `DT_ANDROID_RELASZ` (`0x60000012`); `.rela.plt` is standard `SHT_RELA`.

| reloc type | count | section | handled by reloc.rs today? |
|---|---:|---|---|
| R_X86_64_RELATIVE | 527,208 | `.rela.dyn` (APS2) | ✅ yes |
| R_X86_64_GLOB_DAT | 67 | `.rela.dyn` (APS2) | ✅ yes |
| R_X86_64_64 | 22 | `.rela.dyn` (APS2) | ✅ yes |
| R_X86_64_JUMP_SLOT | 546 | `.rela.plt` (std RELA) | ✅ yes |
| **total** | **527,843** | | |

**No `TPOFF64`, no `DTPMOD64`/`DTPOFF64` (general-dynamic TLS), no `COPY`, no `IRELATIVE`, no
`DT_RELR`.** Every relocation **type** libroblox uses is already implemented in
`src/loader/reloc.rs`.

> ⚠️ **THE GAP IS THE PACKING, NOT THE TYPES.** `elf.rs::relocations()` reads only standard
> `DT_RELA`/`DT_JMPREL`/`DT_RELR`. libroblox has **no** `DT_RELA` — its 527,297 dynamic relocs
> live in the **APS2-packed** table at the OS-specific tags. So Eclipse's decoder currently sees
> **only 546** of libroblox's 527,843 relocations (the PLT JUMP_SLOTs). The 527,208 RELATIVE base
> relocs that the whole image depends on are **invisible** until elf.rs learns the APS2 format.
> This is the **#1 new loader work item.**

### Symbols (imports the bionic env must resolve)
- Dynamic symbols (GNU_HASH-authoritative, via `llvm-readelf`): **1096**.
- **UND (imported) symbols: 584.** Categorized sample:

| category | count | examples |
|---|---:|---|
| libc (bionic) — string/mem/syscall/FORTIFY | ~360 | `__memcpy_chk`, `__open_2`, `strlen`, `mmap`, `clock_gettime`, `syscall`, `__errno`, `__register_atfork` |
| GLES2 / EGL | 91 | `glUseProgram`, `glBindFramebuffer`, `eglGetError`, `glDrawElements` |
| pthread | 45 | `pthread_create`, `pthread_mutex_lock`, `pthread_key_create` |
| android NDK (libandroid) | 31 | `AAssetManager_open`, `ANativeWindow_fromSurface`, `ALooper_pollOnce`, `AConfiguration_getScreenWidthDp`, `__android_log_print` |
| libmediandk | 23 | `AMediaCodec_dequeueOutputBuffer`, `AMediaFormat_delete` |
| OpenSL ES / OpenMAX AL | 8 | `slCreateEngine`, `SL_IID_ENGINE`, `SL_IID_BUFFERQUEUE` |
| dl | 6 | `dlopen`, `dlsym`, `dlerror`, `dladdr` |
| C++ runtime hooks | 3 | `__cxa_atexit`, `__cxa_finalize`, `__cxa_thread_atexit_impl` |
| libm (math) | remainder | `atan2f`, `pow`, `sin`, `tan` (some via GLOB_DAT) |

- **libc++ is STATICALLY linked + internalized** — only 3 `__cxa_*` hooks are imported and **zero**
  `std::`/`_ZNSt*` symbols are imported or exported. There is **no `libc++_shared.so` in
  DT_NEEDED**, so the bionic env does NOT need to provide a C++ runtime for libroblox itself.
- **No Vulkan.** 0 `vk*` imports — the engine's x86-64 build renders through **GLES2 + EGL**.

### Init / fini
- **DT_INIT_ARRAY present, size 27,416 bytes → 3,427 constructors** to run after relocation
  (the runtime-tail must execute these in order). DT_FINI_ARRAY = 24 bytes → 3 destructors.
- No legacy `DT_INIT` (init is entirely via the array).

### elf.rs known limitation surfaced here (honest, not a fix in scope)
`elf.rs::parse_dynsyms` derives the symbol count heuristically (reads from `DT_SYMTAB` up to
`DT_STRTAB`). In libroblox the **VERSYM/VERDEF/VERNEED/GNU_HASH** tables sit *between* SYMTAB
(`0x318`) and STRTAB (`0x8128`), so the heuristic reads `(0x8128-0x318)/24 = 1344` entries — **248
too many** (the trailing 248 are misparsed version/hash bytes). The authoritative GNU_HASH count is
**1096** (UND 584). This over-read is **harmless to relocation** (real relocs only index valid
symbol slots `< 1096`; nothing references the trailing garbage), but the raw `dynsyms.len()` /
UND count from elf.rs (1344 / 611) is an over-estimate for any object that interleaves versioning
between symtab and strtab. Fixing it (derive the count from GNU_HASH's bucket/chain walk) is a
**follow-up**, not part of this characterization.

---

## 3. The other 10 x86-64 libs — one-level summary

All 10 use **standard `SHT_RELA`** (NOT APS2) — `elf.rs` decodes their relocs **in full**, and the
counts match `llvm-readelf` exactly. **None has PT_TLS.** Reloc types across all 10 are only
`RELATIVE` / `GLOB_DAT` / `JUMP_SLOT` / `R_X86_64_64` — **all already handled.**

| lib | DT_NEEDED | relocs (elf.rs, by type) | BIND_NOW |
|---|---|---|---|
| libbacktrace-native.so | liblog, libz, libdl, libm, libc | RELATIVE 11415, 64×5897, JUMP_SLOT 4651, GLOB_DAT 755 | yes |
| libzstd-jni-1.5.7-6.so | libm, libdl, libc | JUMP_SLOT 380, 64×51, RELATIVE 23, GLOB_DAT 9 | no |
| librenderscript-toolkit.so | libjnigraphics, liblog, libdl, libm, libc | RELATIVE 1263, 64×525, JUMP_SLOT 209, GLOB_DAT 49 | yes |
| libeigen_blas.so | libm, libdl, libc | RELATIVE 1206, 64×373, JUMP_SLOT 72, GLOB_DAT 22 | yes |
| libimage_processing_util_jni.so | liblog, libandroid, libjnigraphics, libm, libdl, libc | JUMP_SLOT 18, RELATIVE 4 | yes |
| libsurface_util_jni.so | libandroid, libm, libdl, libc | JUMP_SLOT 9, RELATIVE 3 | yes |
| libdatastore_shared_counter.so | libm, libdl, libc | JUMP_SLOT 11, RELATIVE 3 | yes |
| libtrampoline.so | liblog, libdl, libc | JUMP_SLOT 7, RELATIVE 2 | yes (no SONAME) |
| libeigen_lapack.so | libeigen_blas, libm, libdl, libc | RELATIVE 3, JUMP_SLOT 3, 64×1 | yes |
| libyuv_shared.so | libm, libdl, libc | JUMP_SLOT 3, RELATIVE 3 | yes |

Extra NDK libs these pull in (beyond libroblox's set): **`libz.so`** (libbacktrace) and
**`libjnigraphics.so`** (renderscript, image_processing) — both must also be in the bionic env.

---

## 4. What this means for the loader

### (a) Reloc types — already handled vs new
- **Already handled (reloc.rs):** `R_X86_64_RELATIVE`, `R_X86_64_GLOB_DAT`, `R_X86_64_JUMP_SLOT`,
  `R_X86_64_64`, `R_X86_64_TPOFF64`, `DT_RELR`. This covers **100% of the relocation *types*** the
  entire x86-64 native set uses.
- **NEW work — APS2 packed-relocation decode (the real gap):** libroblox's 527,297 dynamic relocs
  are in the Android **`APS2`** packed format at `DT_ANDROID_RELA (0x60000011)` /
  `DT_ANDROID_RELASZ (0x60000012)`. `elf.rs` must learn to (1) recognize those two OS-specific
  dynamic tags, (2) decode the APS2 stream (magic `APS2`, then SLEB128 group/count/flags/offset/
  addend deltas) into the same `Vec<reloc::Rela>` it already produces. The decoded `Rela`s feed the
  **existing** `reloc.rs` unchanged. This is a **pure decoder addition** in the spirit of the
  existing total byte-parsers (bounds-checked SLEB128 → typed `ElfError`), `#![forbid(unsafe_code)]`.
- **NOT needed for this target (confirmed absent across all 11 libs):** general-dynamic TLS
  (`DTPMOD64`/`DTPOFF64`), `COPY` relocs, `IRELATIVE`/ifunc, and even `TPOFF64` (libroblox has no
  PT_TLS at all). The previously-feared "`unknown reloc type 18` / RELR / BIND_NOW wall" is a
  **non-issue for libroblox**: it has no TPOFF64, no RELR, and BIND_NOW is already supported.

### (b) Bionic-env surface to provide
The shim must expose these sonames (none ship in the APK): **libc.so, libm.so, libdl.so,
liblog.so, libandroid.so, libEGL.so, libGLESv2.so, libOpenSLES.so, libOpenMAXAL.so,
libmediandk.so** (+ **libz.so, libjnigraphics.so** for the helper libs). Symbol surface to satisfy:
**584 UND imports** for libroblox — dominated by **bionic libc (~360)**, then **GLES2/EGL (91)**,
**pthread (45)**, **NDK libandroid (31)**, **libmediandk (23)**, **OpenSL/MAXAL (8)**, **dl (6)**.
EGL/GLES2 route to **host GL forwarding**; the rest are Eclipse-owned bionic-shim natives. No C++
runtime needed (static libc++). No Vulkan loader needed for the engine (GLES2/EGL path).

### (c) Dep libs to load
The dependency-graph linker (`link.rs`) already loads a transitive `DT_NEEDED` BFS. For libroblox
the first level is the 10 bionic sonames above; with host-dlsym fallback OFF (so host glibc can't
satisfy bionic imports), each must resolve to an Eclipse-provided bionic `.so`/shim object that
**exports** the 584 imports. The helper libs add `libz.so`/`libjnigraphics.so`.

### (d) Updated next steps for the runtime tail
1. **APS2 decoder in `elf.rs`** (the gating new work) — recognize `DT_ANDROID_RELA`/`…RELASZ`,
   decode the packed stream into `Vec<reloc::Rela>`; regression: a fixture + a gated real-file
   assert that libroblox's decoded reloc count == 527,297 and the type histogram matches.
2. **Bionic-env provider objects** — stand up the shim sonames as `SymbolProvider`s exporting the
   584-symbol surface (libc/m/dl/log/android/SLES/MAXAL/mediandk) + route EGL/GLES2 to host GL.
3. **Runtime integration tail (main-loop / dev-host only):** bind the assembled image to execution —
   `%fs`/TCB is **not** needed by libroblox (no PT_TLS), simplifying this step; then run the
   **3,427 DT_INIT_ARRAY constructors** in order after relocation, honoring **PT_GNU_RELRO**
   (mprotect RO) and **BIND_NOW** (eager PLT). `IRELATIVE` ifunc execution is **not** required for
   libroblox (0 IRELATIVE).
4. Point `link.rs` at the bionic env + libroblox once (1) and (2) land.

> Net: the engine-load frontier is **narrower than feared**. The only genuinely new decode work for
> libroblox is the **APS2 packed-relocation reader** in `elf.rs`; the reloc applier, symbol scope,
> and dep-graph linker are already sufficient (no TLS, no ifunc, no COPY for this target).

---

## Reproduce

```sh
# x86_64 lib entries + sizes (Eclipse apk reader exercises the same central directory)
unzip -lv ~/eclipse-m0/apk/v2.724.735/roblox-2.724.735-merged.apk | grep 'lib/x86_64/'

# elf.rs-decoded headline facts (gated test; skips cleanly if the APK is absent)
cargo test -p eclipse --lib loader::elf::tests::real_libroblox -- --nocapture

# ground-truth cross-check (llvm-readelf decodes the APS2 .rela.dyn; GNU readelf cannot)
llvm-readelf -r <extracted libroblox.so> | grep -oE 'R_X86_64_[A-Z0-9_]+' | sort | uniq -c
```
