//! Pure-Rust **bionic-environment resolution scope** — the loader's first bionic-env cut.
//!
//! 2026-06-05: [`link`](super::link) maps + base-relocates `libroblox.so` end-to-end (527,208
//! `R_X86_64_RELATIVE`) and records its **635 symbol relocations** (67 `GLOB_DAT` + 22
//! `R_X86_64_64` + 546 `JUMP_SLOT`) against ~584 undefined (UND) bionic imports across 10
//! `DT_NEEDED` sonames (`libc`/`m`/`dl`/`log`/`android`/`EGL`/`GLESv2`/`OpenSLES`/`OpenMAXAL`/
//! `mediandk`) as deferred — there were **no providers** in that root-only pass. This module
//! supplies the **first** provider scope tailored to the engine: a [`BionicEnv`] that resolves
//! the subset of those imports the **host** can soundly provide, categorizes every UND import
//! into the Eclipse-bionic-native work-list, and (via [`super::link::Linker`]) applies the
//! host-resolvable symbol relocations — proving the symbol-relocation pipeline on the real engine.
//!
//! ## HONEST BASELINE CAVEAT (dated 2026-06-05) — read this first
//! The host symbols this scope resolves are **glibc / host-GL** addresses. They are a
//! **relocation-pipeline BASELINE**, *not* bionic-ABI-correct execution. bionic (Android's libc)
//! and glibc differ in `struct` layout, `errno` mechanism, `pthread_t`/mutex internals, `FILE`,
//! TLS/TCB model, and `__cxa_*`/atexit semantics. Filling a GOT slot with a glibc address makes
//! the relocation *land*, but **calling** that glibc routine with bionic-shaped arguments (or
//! sharing glibc/bionic state across the boundary) is **not** correct. So this module does **not**
//! make `libroblox.so` runnable — it proves the symbol-resolution + GOT-fill mechanism and
//! produces the exact work-list of imports that need **Eclipse-owned bionic natives** (the next
//! steps). The [`BionicEnv`] is deliberately **configurable** so those Eclipse-owned bionic
//! providers can later be prepended *before* the host tier, displacing glibc for the libc surface.
//!
//! ## Clean-room provenance
//! Everything here is grounded only in the **public** ELF symbol-resolution + `dlsym(3)`/
//! `dlopen(3)` semantics, the public NDK/bionic symbol *names* (header-documented API surface),
//! and Eclipse's own [`super::resolve`]/[`super::reloc`] code. No dynamic-linker / bionic / NDK
//! *source* was read; symbol classification is by documented name conventions, not by reading any
//! shim. Parsing `libroblox.so`'s bytes as data is benign.
//!
//! ## Safety
//! Only the host providers need `unsafe` (the `dlopen`/`dlsym` FFI), each confined to one small
//! block with a dated `// SAFETY:` note. [`super::reloc`] and [`super::elf`] stay
//! `#![forbid(unsafe_code)]`; this module's only `unsafe` is the FFI in [`DlopenLibProvider`].

use std::collections::BTreeMap;
use std::ffi::CString;

use super::elf::DynSym;
use super::native_provider::EclipseNativeProvider;
use super::reloc::{self, Rela};
use super::resolve::{HostDlsymProvider, ResolvedSym, Scope, SymbolProvider};

// ---- The DT_NEEDED soname buckets libroblox declares (the bionic-env surface) -------------------

/// The 10 first-level `DT_NEEDED` sonames `libroblox.so` declares — every one a bionic/NDK system
/// library the bionic environment must provide (none ship in the APK). Used to label the
/// categorization buckets in the work-list report. (From
/// `docs/libroblox-characterization.md` §2, cross-checked vs `readelf -d`.)
pub const LIBROBLOX_DT_NEEDED: [&str; 10] = [
    "libc.so",
    "libm.so",
    "libdl.so",
    "liblog.so",
    "libandroid.so",
    "libEGL.so",
    "libGLESv2.so",
    "libOpenSLES.so",
    "libOpenMAXAL.so",
    "libmediandk.so",
];

// ---- Import categories (the Eclipse-bionic-native work-list) ------------------------------------

/// The category an undefined import falls into for the Eclipse-bionic-native **work-list**. The
/// ordering of variants is the priority order the doc + AGENTS.md call out (libc/m/dl/pthread first,
/// then NDK/GL/audio/media). Each category names *who must provide it next* and whether the host can
/// stand in as a baseline.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub enum ImportCategory {
    /// Core bionic libc the host glibc *can* stand in for as a baseline (string/mem/syscall/FORTIFY
    /// wrappers, `stdio`, `time`, …) — but bionic-ABI-correct execution needs Eclipse-owned natives.
    BionicLibc,
    /// Math (`libm`) — host glibc libm is a sound baseline for the pure-math subset (`sin`/`pow`/…).
    Math,
    /// POSIX threads (`pthread_*`) — host glibc pthread is a *baseline only*; `pthread_t`/mutex/TLS
    /// internals differ from bionic, so a real run needs Eclipse-owned bionic pthread.
    Pthread,
    /// `dlfcn` (`dlopen`/`dlsym`/`dlerror`/`dladdr`) — must route to Eclipse's **own** loader, not
    /// host `dlopen` (host `dlopen` would load host ELF, not bionic objects). Host = baseline only.
    Dl,
    /// C++ runtime hooks (`__cxa_atexit`/`__cxa_finalize`/`__cxa_thread_atexit_impl`) — bionic-owned
    /// atexit/finalize semantics; host has them but with glibc semantics (baseline only).
    CxaRuntime,
    /// `liblog` (`__android_log_print`, `__android_log_write`, …) — Eclipse already owns these in its
    /// framework; the loader must route them to Eclipse's log natives, not the host (no host equiv).
    Liblog,
    /// NDK `libandroid` (`AAssetManager_*`/`ANativeWindow_*`/`ALooper_*`/`AInputQueue_*`/
    /// `AConfiguration_*`/`AChoreographer_*`) — **no host equivalent**; Eclipse-owned NDK natives.
    NdkAndroid,
    /// EGL/GLES2 (`egl*`/`gl*`) — route to the **host GL** (or ANGLE) bridge. Host `libEGL`/
    /// `libGLESv2` *do* exist on most Linux desktops, so these resolve as a real baseline.
    EglGles,
    /// `libmediandk` (`AMediaCodec_*`/`AMediaFormat_*`/`AMediaExtractor_*`) — **no host equivalent**;
    /// Eclipse-owned media bridge to host codecs.
    MediaNdk,
    /// OpenSL ES / OpenMAX AL audio (`slCreateEngine`, `SL_IID_*`, `XAEngineItf`, …) — **no host
    /// equivalent**; Eclipse-owned audio bridge to host audio.
    Audio,
    /// Anything not matched by the name conventions above — surfaced explicitly so nothing is
    /// silently dropped (expected to be empty / tiny for libroblox).
    Uncategorized,
}

impl ImportCategory {
    /// A short, stable label for reports.
    pub fn label(self) -> &'static str {
        match self {
            ImportCategory::BionicLibc => "bionic-libc",
            ImportCategory::Math => "libm",
            ImportCategory::Pthread => "pthread",
            ImportCategory::Dl => "dl",
            ImportCategory::CxaRuntime => "cxa-runtime",
            ImportCategory::Liblog => "liblog",
            ImportCategory::NdkAndroid => "ndk-android",
            ImportCategory::EglGles => "egl-gles",
            ImportCategory::MediaNdk => "media-ndk",
            ImportCategory::Audio => "audio",
            ImportCategory::Uncategorized => "uncategorized",
        }
    }

    /// True if the host can stand in for this category as a **baseline** (relocation lands, but not
    /// bionic-ABI-correct). False = there is **no host equivalent** (NDK/media/audio) → an
    /// Eclipse-owned native is the *only* path even for a baseline.
    pub fn host_baseline_possible(self) -> bool {
        matches!(
            self,
            ImportCategory::BionicLibc
                | ImportCategory::Math
                | ImportCategory::Pthread
                | ImportCategory::Dl
                | ImportCategory::CxaRuntime
                | ImportCategory::EglGles
        )
    }
}

/// Classify an undefined import **by its symbol name** into an [`ImportCategory`]. This is the
/// clean-room categorization: it uses only the public NDK/bionic name conventions (documented API
/// prefixes), never reads any shim. Prefix matches (`gl`/`egl`/`AMedia`/…) are checked before the
/// curated bionic-libc/pthread/dl/cxa name sets.
///
/// The order matters: the most specific NDK/GL/media/audio prefixes are matched first, then the
/// curated libc/pthread/dl/cxa/log sets, then a math heuristic, then `Uncategorized`.
pub fn classify_import(name: &str) -> ImportCategory {
    // 1) Exact / prefix NDK + GL + media + audio surfaces (no host bionic equivalent except GL).
    if is_egl_gles(name) {
        return ImportCategory::EglGles;
    }
    if is_media_ndk(name) {
        return ImportCategory::MediaNdk;
    }
    if is_audio(name) {
        return ImportCategory::Audio;
    }
    if is_liblog(name) {
        return ImportCategory::Liblog;
    }
    if is_ndk_android(name) {
        return ImportCategory::NdkAndroid;
    }
    // 2) Curated bionic libc-adjacent sets.
    if is_dl(name) {
        return ImportCategory::Dl;
    }
    if is_cxa_runtime(name) {
        return ImportCategory::CxaRuntime;
    }
    if name.starts_with("pthread_") {
        return ImportCategory::Pthread;
    }
    // 3) libc vs libm: a curated math-name set is the only clean way to split (both are bionic libc
    //    families); everything else with a libc-shape is bionic-libc.
    if is_math(name) {
        return ImportCategory::Math;
    }
    if is_bionic_libc(name) {
        return ImportCategory::BionicLibc;
    }
    ImportCategory::Uncategorized
}

/// EGL/GLES2: the documented `egl*`/`gl*` entry points (and the `EGL`/`GL` typed constants the
/// engine may import as objects).
fn is_egl_gles(name: &str) -> bool {
    // `gl` followed by an uppercase letter (glUseProgram, glBindFramebuffer) or `egl` prefix.
    if let Some(rest) = name.strip_prefix("gl") {
        if rest.chars().next().is_some_and(|c| c.is_ascii_uppercase()) {
            return true;
        }
    }
    name.starts_with("egl")
}

/// `libmediandk`: `AMediaCodec_*`/`AMediaFormat_*`/`AMediaExtractor_*`/`AMediaDrm_*`/`AMedia*` and
/// the `AMEDIA*`/`AMEDIAFORMAT_KEY_*` constants.
fn is_media_ndk(name: &str) -> bool {
    name.starts_with("AMedia") || name.starts_with("AMEDIA")
}

/// OpenSL ES / OpenMAX AL audio: `slCreateEngine`/`xaCreateEngine`, the `SL_IID_*`/`XA_IID_*`
/// interface-id constants, and the `slQueryNumSupportedEngineInterfaces`-style entry points.
fn is_audio(name: &str) -> bool {
    name.starts_with("slCreateEngine")
        || name.starts_with("xaCreateEngine")
        || name.starts_with("slQueryNum")
        || name.starts_with("xaQueryNum")
        || name.starts_with("SL_IID_")
        || name.starts_with("XA_IID_")
}

/// `liblog`: the Android log entry points (Eclipse already owns these in its framework).
fn is_liblog(name: &str) -> bool {
    name.starts_with("__android_log_")
        || name == "android_set_abort_message"
        || name.starts_with("__android_set_abort")
}

/// NDK `libandroid`: the `A<Capital>...` capability families that are NOT media (already split out):
/// `AAssetManager`/`AAsset`/`ANativeWindow`/`ANativeActivity`/`ALooper`/`AInputQueue`/`AInputEvent`/
/// `AKeyEvent`/`AMotionEvent`/`AConfiguration`/`AChoreographer`/`ASensor`/`ASharedMemory`/`ATrace`/
/// `AStorageManager`/`AHardwareBuffer`/`ANativeWindow`/`AObbInfo`/`obb`/`A<Cap>`.
fn is_ndk_android(name: &str) -> bool {
    // The NDK convention: a capital-A prefix on a CamelCase family name with an underscore method,
    // e.g. `AAssetManager_open`, `ANativeWindow_fromSurface`, `ALooper_pollOnce`. Media (`AMedia*`)
    // is matched earlier, so it cannot reach here.
    const NDK_FAMILIES: [&str; 18] = [
        "AAssetManager",
        "AAsset",
        "ANativeWindow",
        "ANativeActivity",
        "ALooper",
        "AInputQueue",
        "AInputEvent",
        "AKeyEvent",
        "AMotionEvent",
        "AConfiguration",
        "AChoreographer",
        "ASensor",
        "ASensorManager",
        "ASensorEventQueue",
        "ASharedMemory",
        "ATrace",
        "AHardwareBuffer",
        "ANativeActivity",
    ];
    NDK_FAMILIES.iter().any(|fam| {
        name.strip_prefix(fam)
            .is_some_and(|rest| rest.starts_with('_') || rest.is_empty())
    })
}

/// `dlfcn`: the six documented entry points.
fn is_dl(name: &str) -> bool {
    matches!(
        name,
        "dlopen" | "dlsym" | "dlclose" | "dlerror" | "dladdr" | "dlvsym" | "android_dlopen_ext"
    )
}

/// C++ runtime hooks bionic exports from libc.
fn is_cxa_runtime(name: &str) -> bool {
    matches!(
        name,
        "__cxa_atexit" | "__cxa_finalize" | "__cxa_thread_atexit_impl" | "__cxa_atexit_impl"
    )
}

/// A curated **math** name set — the `libm` surface the engine imports. Split from bionic-libc so
/// the report attributes them to `libm.so`. (Conservative: only well-known libm names; an unmatched
/// `*f`/`*l` math name falls through to bionic-libc, which is harmless for the work-list since both
/// are bionic-provided.)
fn is_math(name: &str) -> bool {
    // Common float/double/long-double math entry points the engine uses. Suffix-stripping handles
    // the `f`/`l` variants (sinf/sinl) uniformly.
    let core = name
        .strip_suffix('f')
        .or_else(|| name.strip_suffix('l'))
        .unwrap_or(name);
    const MATH: [&str; 40] = [
        "acos",
        "asin",
        "atan",
        "atan2",
        "cbrt",
        "ceil",
        "copysign",
        "cos",
        "cosh",
        "exp",
        "exp2",
        "expm1",
        "fabs",
        "fdim",
        "floor",
        "fma",
        "fmax",
        "fmin",
        "fmod",
        "frexp",
        "hypot",
        "ldexp",
        "lgamma",
        "log",
        "log10",
        "log1p",
        "log2",
        "logb",
        "modf",
        "nearbyint",
        "pow",
        "remainder",
        "rint",
        "round",
        "sin",
        "sinh",
        "sqrt",
        "tan",
        "tanh",
        "trunc",
    ];
    MATH.contains(&core)
}

/// A bionic-libc classification: anything that looks like a C-runtime symbol the host glibc could
/// stand in for. This is the broad catch for the ~360 libc imports — FORTIFY `__*_chk`, the `__*`
/// internal wrappers (`__errno`, `__open_2`, `__register_atfork`), and plain libc names. It is the
/// LAST positive classifier (after the specific NDK/GL/audio/media/pthread/dl/cxa sets), so it only
/// catches what those did not.
fn is_bionic_libc(name: &str) -> bool {
    // A bionic-internal or FORTIFY wrapper, or any lowercase-leading C identifier (the libc shape).
    // Reaching here means it is not GL/NDK/media/audio/pthread/dl/cxa/log/math, so attributing the
    // remainder to bionic libc is correct for the work-list (they are the libc/`__*` surface).
    if name.starts_with("__") {
        return true; // FORTIFY (`__memcpy_chk`) / internal (`__errno`, `__stack_chk_fail`)
    }
    // A plain libc-style name (starts with an ASCII letter or `_`), e.g. `strlen`, `mmap`, `syscall`.
    name.chars()
        .next()
        .is_some_and(|c| c.is_ascii_alphabetic() || c == '_')
}

// ---- Per-bucket / per-category categorization result --------------------------------------------

/// The categorization of every UND import of an object, grouped both by [`ImportCategory`]
/// (the work-list) and by host-resolvability. Built by [`categorize_imports`].
#[derive(Debug, Clone, Default)]
pub struct ImportReport {
    /// Total undefined (imported) symbols considered.
    pub total: usize,
    /// Per-category: the sorted list of import names in that category.
    pub by_category: BTreeMap<&'static str, Vec<String>>,
    /// Names the scope resolved to a **non-null** host address (the baseline subset).
    pub host_resolved: Vec<String>,
    /// Names the scope did **not** resolve (the Eclipse-bionic-native work-list, ungrouped).
    pub host_unresolved: Vec<String>,
    /// Per-category counts of (resolved, unresolved) — the headline table.
    pub category_counts: BTreeMap<&'static str, (usize, usize)>,
}

impl ImportReport {
    /// Number of imports resolved by the scope (host baseline subset).
    pub fn resolved_count(&self) -> usize {
        self.host_resolved.len()
    }

    /// Number of imports the scope could not resolve (the work-list).
    pub fn unresolved_count(&self) -> usize {
        self.host_unresolved.len()
    }
}

/// Categorize the engine's **relocation-referenced** undefined imports against `scope`: for each
/// distinct symbol that a symbol relocation (`GLOB_DAT`/`JUMP_SLOT`/`R_X86_64_64`) names, classify it
/// into an [`ImportCategory`] (work-list) and record whether `scope` resolves it to a non-null
/// address (host baseline). A *weak* undef the scope leaves unresolved is **not** a work-list gap
/// (the psABI resolves it to 0 legally); only **strong** unresolved imports are the work-list. The
/// returned [`ImportReport`] is deterministic (sorted) for stable reporting/tests.
///
/// 2026-06-05 — this iterates the **relocations**, not the raw dynamic symbol table, on purpose: it
/// is exactly the set of imports the loader must satisfy (a reloc names them), and it is immune to
/// `elf.rs`'s documented symtab over-read (the heuristic reads trailing VERSYM/GNU_HASH bytes as
/// extra UND "symbols"; real relocs only index valid symbol slots, so a reloc-driven categorization
/// never sees that garbage). This makes the categorization split match the partial-apply pass by
/// construction (both walk the relocations through the same scope). Each symbol is counted once even
/// if several relocations reference it.
pub fn categorize_imports(relas: &[Rela], dynsyms: &[DynSym], scope: &Scope) -> ImportReport {
    // STB_WEAK == 2 (public gABI); a weak-undef the scope doesn't define resolves to 0 (not a gap).
    const STB_WEAK: u8 = 2;
    const SHN_UNDEF: u16 = 0;

    let mut report = ImportReport::default();
    let mut seen: BTreeMap<String, ()> = BTreeMap::new();

    for r in relas {
        let is_symbol_reloc = matches!(
            r.r_type,
            reloc::R_X86_64_GLOB_DAT | reloc::R_X86_64_JUMP_SLOT | reloc::R_X86_64_64
        );
        if !is_symbol_reloc {
            continue;
        }
        let Some(sym) = dynsyms.get(r.sym_index as usize) else {
            continue; // out-of-range index (malformed) — skip, never panic
        };
        // A symbol reloc against a defined symbol is a self-reference, not an import; skip it. We
        // only categorize the UNDEFINED ones (the imports the bionic env must supply).
        if sym.shndx != SHN_UNDEF || sym.name.is_empty() {
            continue;
        }
        if seen.insert(sym.name.clone(), ()).is_some() {
            continue; // de-dup by name (several relocs may name one import)
        }
        report.total += 1;

        let cat = classify_import(&sym.name).label();
        report
            .by_category
            .entry(cat)
            .or_default()
            .push(sym.name.clone());

        // Resolve against the scope. A non-null hit = host baseline resolved.
        let resolved = scope
            .resolve(&sym.name)
            .is_some_and(|r: ResolvedSym| r.addr != 0);
        let counts = report.category_counts.entry(cat).or_insert((0, 0));
        if resolved {
            report.host_resolved.push(sym.name.clone());
            counts.0 += 1;
        } else {
            // A weak-undef the scope leaves unresolved is legal (→0), NOT a work-list gap; only a
            // STRONG unresolved import is recorded as needing an Eclipse-owned native.
            if sym.bind != STB_WEAK {
                report.host_unresolved.push(sym.name.clone());
            }
            counts.1 += 1;
        }
    }

    // Determinism: sort every list + the per-category lists.
    report.host_resolved.sort();
    report.host_unresolved.sort();
    for v in report.by_category.values_mut() {
        v.sort();
    }
    report
}

// ---- The BionicEnv scope ------------------------------------------------------------------------

/// A configurable, ordered bionic-environment resolution [`Scope`] for `libroblox.so`.
///
/// 2026-06-05 — the provider order (System V gABI first-match) is:
/// 1. **Eclipse-owned bionic natives** — an [`EclipseNativeProvider`] PREPENDED before the host
///    tier, so Eclipse's bionic-ABI-correct implementations win over the host-glibc baseline for the
///    names it registers (liblog 3 fixed-arity + bionic-libc 15 in this cut). Present when built with
///    [`BionicEnv::with_host_baseline`]'s `eclipse_natives = true`.
/// 2. **Host GL** — if the host has `libEGL.so`/`libGLESv2.so`, a [`DlopenLibProvider`] over each
///    (the engine renders via EGL/GLES2). If absent, skipped and recorded in [`Self::missing_gl`].
/// 3. **Host libc/m/dl/pthread** — a single [`HostDlsymProvider`] (`dlsym(RTLD_DEFAULT)`) over the
///    host process (glibc/libm/libdl/libpthread are all in the default search order). This is the
///    BASELINE tier (see the module-level honest caveat).
///
/// The host tiers prove the relocation pipeline and are **not** bionic-ABI-correct; the prepended
/// Eclipse tier (1) is where bionic-correct natives displace the glibc baseline. Build with
/// [`BionicEnv::with_host_baseline`] and read [`BionicEnv::into_scope`].
///
/// 2026-06-12 — recorded-only host-baseline hazards (no code; flagged during the core-947663
/// destructor-order forensics; each needs its own evidence before a fix): (a) the cross-allocator
/// class — host-baseline natives that RETURN malloc'd memory the caller must free (`vasprintf`,
/// `realpath(.., NULL)`, `strdup`-family) hand out glibc-heap blocks; the engine's `free` also
/// resolves to host glibc today so the pair is consistent, but any future Eclipse-owned
/// `malloc`/`free` displacement must move these in the SAME change or frees cross allocators;
/// (b) the struct-shape class — bionic `mallinfo` returns the `size_t`-field struct (80 B on LP64)
/// vs glibc's `int`-field one (40 B): a host fall-through under-fills the caller's 80-byte buffer
/// (the `__sF`/`__fread_chk` shape-mismatch class; no engine import proven to call it yet).
pub struct BionicEnv {
    scope: Scope,
    /// Host GL sonames that could NOT be `dlopen`ed (recorded so EGL/GLES are reported unresolved).
    missing_gl: Vec<String>,
    /// Whether the host libc/m/dl/pthread tier (`HostDlsymProvider`) is present.
    host_libc_present: bool,
    /// Whether the prepended Eclipse-owned bionic-native tier (`EclipseNativeProvider`) is present.
    eclipse_natives_present: bool,
}

impl BionicEnv {
    /// Build the bionic-env scope: optionally an [`EclipseNativeProvider`] PREPENDED first (Eclipse's
    /// bionic-correct natives win), then host GL (if present), then the host libc/m/dl/pthread
    /// `dlsym(RTLD_DEFAULT)` baseline tier.
    ///
    /// `try_host_gl` controls whether to `dlopen` the host `libEGL.so`/`libGLESv2.so` (set false to
    /// model a GL-less host, e.g. a headless CI box, in which case EGL/GLES report unresolved).
    /// `eclipse_natives` controls whether to prepend the Eclipse-owned bionic natives (liblog 3
    /// fixed-arity + bionic-libc 15) before the host tier — when true, those names resolve to Eclipse
    /// addresses, NOT host glibc.
    pub fn with_host_baseline(try_host_gl: bool, eclipse_natives: bool) -> Self {
        let mut scope = Scope::new();
        let mut missing_gl = Vec::new();

        // 0) Eclipse-owned bionic natives FIRST (prepended) so they win over the host baseline for
        //    the names they register (gABI first-match). This is the bionic-ABI-correct tier.
        if eclipse_natives {
            scope.push(Box::new(EclipseNativeProvider::with_bionic_natives()));
        }

        // 1) Host GL providers (EGL/GLES2 → host GL forwarding), first among the HOST tiers so a
        //    `gl*`/`egl*` symbol resolves from the GL libs rather than the host process default scope.
        if try_host_gl {
            for soname in ["libEGL.so", "libGLESv2.so"] {
                match DlopenLibProvider::open(soname) {
                    Some(p) => {
                        scope.push(Box::new(p));
                    }
                    None => missing_gl.push(soname.to_string()),
                }
            }
        } else {
            missing_gl.push("libEGL.so".to_string());
            missing_gl.push("libGLESv2.so".to_string());
        }

        // 2) Host libc/m/dl/pthread tier (RTLD_DEFAULT covers all of them in the host process).
        scope.push(Box::new(HostDlsymProvider));

        Self {
            scope,
            missing_gl,
            host_libc_present: true,
            eclipse_natives_present: eclipse_natives,
        }
    }

    /// Build an **empty** bionic-env scope (no host tier) — models a host with no usable libc/GL, or
    /// a future all-Eclipse-native build before any native is wired. Resolves nothing; every import
    /// is reported as the work-list. Used by tests + for a pure "what is the full work-list" pass.
    pub fn empty() -> Self {
        Self {
            scope: Scope::new(),
            missing_gl: vec!["libEGL.so".to_string(), "libGLESv2.so".to_string()],
            host_libc_present: false,
            eclipse_natives_present: false,
        }
    }

    /// The host GL sonames that could not be `dlopen`ed (EGL/GLES reported unresolved).
    pub fn missing_gl(&self) -> &[String] {
        &self.missing_gl
    }

    /// Whether the host libc/m/dl/pthread baseline tier is present.
    pub fn host_libc_present(&self) -> bool {
        self.host_libc_present
    }

    /// Whether the prepended Eclipse-owned bionic-native tier is present (liblog + bionic-libc).
    pub fn eclipse_natives_present(&self) -> bool {
        self.eclipse_natives_present
    }

    /// Borrow the underlying [`Scope`] (e.g. to [`categorize_imports`] against it).
    pub fn scope(&self) -> &Scope {
        &self.scope
    }

    /// Consume into the underlying [`Scope`] (e.g. to hand to a resolver / reloc pass).
    pub fn into_scope(self) -> Scope {
        self.scope
    }

    /// Consume into the ordered provider list (so a caller can build a full gABI scope
    /// `[LoadedObjectProvider(own-object), env-providers...]` by prepending the object's own
    /// provider, then chaining these). 2026-06-05: used by the bionic-env apply path in `link.rs`.
    pub fn into_providers(self) -> Vec<Box<dyn SymbolProvider>> {
        self.scope.into_providers()
    }
}

// ---- A dlopen'd-host-library provider (for host EGL/GLES) ----------------------------------------

/// A [`SymbolProvider`] backed by a **specific** host shared library opened with `dlopen` — used for
/// host `libEGL.so`/`libGLESv2.so`, which are not necessarily in the process's default `dlsym`
/// scope until opened. `resolve(name)` is `dlsym(handle, name)`; a non-null result is a strong host
/// definition (a real exported symbol), a null result is `None`.
///
/// The opened handle is **kept for the process lifetime** (never `dlclose`d): the addresses it hands
/// out are written into the relocated GOT/PLT, which must stay valid for as long as the relocated
/// object lives. Leaking the handle (a single, process-global render lib) is the correct ownership
/// model here and avoids dangling GOT slots; it is not a growth leak (opened once).
pub struct DlopenLibProvider {
    /// The `dlopen` handle (opaque). Kept alive for the process lifetime by storing it here and
    /// never closing it.
    handle: HostLibHandle,
    /// The soname (for diagnostics).
    soname: String,
}

/// A wrapper around a raw `dlopen` handle that makes the provider `Send`/`Sync`-safe to store.
struct HostLibHandle(*mut libc::c_void);

// SAFETY: 2026-06-05 — the handle is an opaque `dlopen` token used only as the first argument to
// `dlsym`, which is thread-safe (the dynamic loader serializes internally). We never dereference it
// in Rust, never mutate through it, and never close it (process-lifetime). Sharing it across threads
// is sound: `dlsym(handle, name)` is reentrant-safe and has no per-handle interior mutability we own.
unsafe impl Send for HostLibHandle {}
// SAFETY: see the `Send` note — read-only `dlsym` lookups against an immutable, never-closed handle.
unsafe impl Sync for HostLibHandle {}

impl DlopenLibProvider {
    /// `dlopen(soname, RTLD_NOW | RTLD_LOCAL)` the host library. Returns `None` if the host lacks it
    /// (so the caller records EGL/GLES as unresolved rather than failing). `RTLD_LOCAL` keeps the
    /// lib's symbols out of the global scope (we look them up explicitly via this handle), and
    /// `RTLD_NOW` binds eagerly so a broken lib fails to open rather than at first use.
    pub fn open(soname: &str) -> Option<Self> {
        let cname = CString::new(soname).ok()?;
        // SAFETY: 2026-06-05 — `dlopen(ptr, flags)` reads a NUL-terminated C string at `ptr` and
        // returns an opaque handle or NULL. `cname` owns a valid NUL-terminated buffer that outlives
        // the call. The flags are the standard `RTLD_NOW | RTLD_LOCAL`. The call loads (or finds an
        // already-loaded) host library; it has no effect on our memory and cannot write through our
        // pointer. A NULL return (lib absent / failed to load) is handled below as `None`.
        let handle = unsafe { libc::dlopen(cname.as_ptr(), libc::RTLD_NOW | libc::RTLD_LOCAL) };
        if handle.is_null() {
            return None;
        }
        Some(Self {
            handle: HostLibHandle(handle),
            soname: soname.to_string(),
        })
    }

    /// The soname this provider opened (diagnostics).
    pub fn soname(&self) -> &str {
        &self.soname
    }
}

impl SymbolProvider for DlopenLibProvider {
    fn resolve(&self, name: &str) -> Option<ResolvedSym> {
        let cname = CString::new(name).ok()?; // interior NUL → not a valid C symbol
                                              // SAFETY: 2026-06-05 — `dlsym(handle, ptr)` reads a NUL-terminated C string at `ptr` and
                                              // returns the symbol's address in `handle`'s library or NULL. `self.handle.0` is a live,
                                              // never-closed `dlopen` handle (see `HostLibHandle`'s SAFETY); `cname` owns a valid
                                              // NUL-terminated buffer outliving the call. The returned address is opaque (we only test it
                                              // for non-null and pass it on as a `u64`); the call cannot write through our pointers.
        let ptr = unsafe { libc::dlsym(self.handle.0, cname.as_ptr()) };
        if ptr.is_null() {
            None
        } else {
            Some(ResolvedSym {
                addr: ptr as u64,
                weak: false,
            })
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn undef(name: &str, bind: u8) -> DynSym {
        DynSym {
            name: name.to_string(),
            value: 0,
            size: 0,
            bind,
            sym_type: 2, // STT_FUNC
            shndx: 0,    // SHN_UNDEF → an import
        }
    }
    fn def(name: &str) -> DynSym {
        DynSym {
            name: name.to_string(),
            value: 0x10,
            size: 0,
            bind: 1,     // STB_GLOBAL
            sym_type: 2, // STT_FUNC
            shndx: 1,    // a real section → a definition, NOT an import
        }
    }
    /// A `GLOB_DAT` relocation referencing dynsym index `sym_index` (categorization walks relocs).
    fn glob_dat(sym_index: u32) -> Rela {
        Rela {
            offset: 0x1000 + u64::from(sym_index) * 8,
            sym_index,
            r_type: reloc::R_X86_64_GLOB_DAT,
            addend: 0,
        }
    }

    // ---- classification ------------------------------------------------------------------------

    #[test]
    fn classify_gl_and_egl() {
        assert_eq!(classify_import("glUseProgram"), ImportCategory::EglGles);
        assert_eq!(classify_import("glDrawElements"), ImportCategory::EglGles);
        assert_eq!(classify_import("eglGetError"), ImportCategory::EglGles);
        assert_eq!(classify_import("eglSwapBuffers"), ImportCategory::EglGles);
        // `glob`/`globfree` (libc) must NOT be miscategorized as GL (no uppercase after `gl`).
        assert_eq!(classify_import("glob"), ImportCategory::BionicLibc);
    }

    #[test]
    fn classify_ndk_media_audio_log() {
        assert_eq!(
            classify_import("AAssetManager_open"),
            ImportCategory::NdkAndroid
        );
        assert_eq!(
            classify_import("ANativeWindow_fromSurface"),
            ImportCategory::NdkAndroid
        );
        assert_eq!(
            classify_import("ALooper_pollOnce"),
            ImportCategory::NdkAndroid
        );
        assert_eq!(
            classify_import("AConfiguration_getScreenWidthDp"),
            ImportCategory::NdkAndroid
        );
        assert_eq!(
            classify_import("AMediaCodec_dequeueOutputBuffer"),
            ImportCategory::MediaNdk
        );
        assert_eq!(
            classify_import("AMediaFormat_delete"),
            ImportCategory::MediaNdk
        );
        assert_eq!(classify_import("slCreateEngine"), ImportCategory::Audio);
        assert_eq!(classify_import("SL_IID_ENGINE"), ImportCategory::Audio);
        assert_eq!(
            classify_import("__android_log_print"),
            ImportCategory::Liblog
        );
    }

    #[test]
    fn classify_libc_pthread_dl_cxa_math() {
        assert_eq!(classify_import("__memcpy_chk"), ImportCategory::BionicLibc);
        assert_eq!(classify_import("strlen"), ImportCategory::BionicLibc);
        assert_eq!(classify_import("mmap"), ImportCategory::BionicLibc);
        assert_eq!(classify_import("__errno"), ImportCategory::BionicLibc);
        assert_eq!(
            classify_import("pthread_mutex_lock"),
            ImportCategory::Pthread
        );
        assert_eq!(classify_import("dlopen"), ImportCategory::Dl);
        assert_eq!(classify_import("__cxa_atexit"), ImportCategory::CxaRuntime);
        assert_eq!(classify_import("pow"), ImportCategory::Math);
        assert_eq!(classify_import("atan2f"), ImportCategory::Math);
        assert_eq!(classify_import("sinf"), ImportCategory::Math);
    }

    #[test]
    fn host_baseline_possibility_is_correct() {
        // NDK / media / audio have NO host equivalent → not even a baseline.
        assert!(!ImportCategory::NdkAndroid.host_baseline_possible());
        assert!(!ImportCategory::MediaNdk.host_baseline_possible());
        assert!(!ImportCategory::Audio.host_baseline_possible());
        assert!(!ImportCategory::Liblog.host_baseline_possible());
        // libc/m/pthread/dl/cxa/GL can be host baselines.
        assert!(ImportCategory::BionicLibc.host_baseline_possible());
        assert!(ImportCategory::EglGles.host_baseline_possible());
        assert!(ImportCategory::Pthread.host_baseline_possible());
    }

    // ---- categorize_imports over a small fixture -----------------------------------------------

    #[test]
    fn categorize_splits_imports_and_skips_definitions() {
        let syms = vec![
            undef("strlen", 1),             // 0: bionic-libc (host-resolvable)
            undef("AAssetManager_open", 1), // 1: ndk-android (no host equiv → work-list)
            undef("glUseProgram", 1),       // 2: egl-gles
            undef("__gmon_start__", 2), // 3: WEAK undef → unresolved is legal, NOT a work-list gap
            def("an_internal_definition"), // 4: a definition, not an import → ignored
        ];
        // Relocations: each import referenced once, plus a SECOND reloc against strlen (the same
        // import is counted once), plus a reloc against the definition (idx 4 — not an import, skipped).
        let relas = vec![
            glob_dat(0), // strlen
            glob_dat(1), // AAssetManager_open
            glob_dat(2), // glUseProgram
            glob_dat(3), // __gmon_start__ (weak)
            glob_dat(0), // strlen again → de-duped
            glob_dat(4), // a definition → not an import, skipped
        ];
        // Empty scope: nothing resolves, so every STRONG import is a work-list gap; the weak one is
        // legal-unresolved (NOT in the work-list).
        let env = BionicEnv::empty();
        let report = categorize_imports(&relas, &syms, env.scope());

        // 4 distinct imports counted (strlen once, AAssetManager_open, glUseProgram, __gmon_start__).
        assert_eq!(report.total, 4);
        // The definition is not an import.
        assert!(!report
            .by_category
            .values()
            .flatten()
            .any(|n| n == "an_internal_definition"));
        // Categories present. `by_category` lists ALL imports of that category regardless of
        // resolvability, including the weak `__gmon_start__` (a `__`-prefixed bionic-libc name).
        assert_eq!(
            report.by_category.get("bionic-libc").map(Vec::as_slice),
            Some(["__gmon_start__".to_string(), "strlen".to_string()].as_slice())
        );
        assert_eq!(
            report.by_category.get("ndk-android").map(Vec::as_slice),
            Some(["AAssetManager_open".to_string()].as_slice())
        );
        // With an EMPTY scope nothing resolves: the 3 STRONG imports are the work-list; the WEAK
        // `__gmon_start__` is legal-unresolved and NOT in the work-list.
        assert_eq!(report.resolved_count(), 0);
        let mut wl = report.host_unresolved.clone();
        wl.sort();
        assert_eq!(
            wl,
            vec![
                "AAssetManager_open".to_string(),
                "glUseProgram".to_string(),
                "strlen".to_string(),
            ]
        );
        assert!(!report
            .host_unresolved
            .contains(&"__gmon_start__".to_string()));
    }

    #[test]
    fn categorize_resolves_host_libc_subset() {
        // With the host baseline tier, a real libc symbol resolves; an NDK one never does.
        let syms = vec![
            undef("memcpy", 1),             // 0: host libc → resolved
            undef("malloc", 1),             // 1: host libc → resolved
            undef("AAssetManager_open", 1), // 2: NDK → no host equiv → work-list
        ];
        let relas = vec![glob_dat(0), glob_dat(1), glob_dat(2)];
        // try_host_gl=false to keep the test independent of host GL presence; libc tier still on.
        // eclipse_natives=false: this test exercises the host-baseline tier specifically.
        let env = BionicEnv::with_host_baseline(false, false);
        let report = categorize_imports(&relas, &syms, env.scope());
        assert_eq!(report.total, 3);
        // The two libc symbols resolve from the host process; the NDK one does not.
        assert!(report.host_resolved.contains(&"memcpy".to_string()));
        assert!(report.host_resolved.contains(&"malloc".to_string()));
        assert!(report
            .host_unresolved
            .contains(&"AAssetManager_open".to_string()));
        assert_eq!(report.resolved_count(), 2);
        assert_eq!(report.unresolved_count(), 1);
    }

    // ---- BionicEnv scope ordering --------------------------------------------------------------

    #[test]
    fn bionic_env_host_baseline_has_libc_tier() {
        let env = BionicEnv::with_host_baseline(false, false);
        assert!(env.host_libc_present());
        assert!(!env.eclipse_natives_present());
        // The host libc tier resolves a known libc symbol through the scope.
        let got = env.scope().resolve("memcpy");
        assert!(got.is_some_and(|r| r.addr != 0));
        // GL disabled → both GL sonames recorded missing.
        assert_eq!(env.missing_gl().len(), 2);
    }

    #[test]
    fn bionic_env_eclipse_natives_win_over_host() {
        // With the Eclipse-native tier prepended, a bionic-libc name glibc lacks (`__errno`) resolves
        // to an Eclipse address, and a name glibc HAS (`memcpy`) still resolves (from the host tier).
        let env = BionicEnv::with_host_baseline(false, true);
        assert!(env.eclipse_natives_present());
        // `__errno` is NOT a glibc symbol (glibc has `__errno_location`) — only the Eclipse tier
        // provides it, proving the prepended tier is consulted.
        assert!(
            env.scope().resolve("__errno").is_some_and(|r| r.addr != 0),
            "the Eclipse tier must resolve __errno (glibc lacks this exact name)"
        );
        // `__strlen_chk` likewise comes from the Eclipse tier (glibc lacks the bionic name).
        assert!(env
            .scope()
            .resolve("__strlen_chk")
            .is_some_and(|r| r.addr != 0));
        // A plain host symbol still resolves via the host tier behind the Eclipse one.
        assert!(env.scope().resolve("memcpy").is_some_and(|r| r.addr != 0));
    }

    #[test]
    fn bionic_env_empty_resolves_nothing() {
        let env = BionicEnv::empty();
        assert!(!env.host_libc_present());
        assert_eq!(env.scope().resolve("memcpy"), None);
    }

    #[test]
    fn dlopen_provider_resolves_from_libc() {
        // `libc.so.6` is present on every glibc host; open it and resolve a known symbol. (Skip
        // cleanly if a host somehow lacks it — never fail/fabricate.)
        let Some(p) = DlopenLibProvider::open("libc.so.6") else {
            eprintln!("dlopen_provider_resolves_from_libc: no libc.so.6; skipping");
            return;
        };
        assert_eq!(p.soname(), "libc.so.6");
        assert!(p.resolve("memcpy").is_some_and(|r| r.addr != 0));
        assert_eq!(p.resolve("__eclipse_no_such_symbol_zzz__"), None);
        // An interior NUL is not a valid C symbol → None (no panic).
        assert_eq!(p.resolve("bad\0name"), None);
    }

    #[test]
    fn dlopen_provider_absent_lib_is_none() {
        assert!(DlopenLibProvider::open("libeclipse_definitely_no_such_lib_4f2a.so").is_none());
    }
}
