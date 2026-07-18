//! Live engine-load path: map + relocate + resolve + run `DT_INIT_ARRAY` + call `JNI_OnLoad`.
//!
//! 2026-06-05: This is the **production wiring** of the proven loader pipeline into `eclipse run`.
//! The isolated diagnostic harness ([`super::init_run`]) established that Eclipse's own Rust loader
//! maps the 3 PT_LOAD, applies all 527,843 relocations, honors RELRO + BIND_NOW, fully resolves all
//! 584 imports (work-list 0), and runs **all 3,427** `DT_INIT_ARRAY` constructors deterministically
//! (EXIT=0). This module performs the SAME pipeline but, instead of `_exit`ing, **keeps the loaded
//! image alive for the process lifetime** (the engine + its background workers keep running) and
//! exposes the engine's exported `JNI_OnLoad` so the live ART VM can be handed to it.
//!
//! ## Why route libroblox through this loader (not the apkenv linker)
//! When Roblox's Java calls `System.loadLibrary("roblox")`, ART's `Runtime.nativeLoad` hands the
//! load to the vendored apkenv-era bionic shim linker, which **aborts on the engine's modern
//! relocations** (`R_X86_64_TPOFF64`/`DT_RELR`/`BIND_NOW` — `docs/bionic-loader-strategy.md` §1).
//! Eclipse instead loads `libroblox.so` through this Rust loader **before** driving the lifecycle, so
//! the engine is already mapped, relocated, initialized, and its `JNI_OnLoad` called against the real
//! ART `JavaVM` — registering its native methods — by the time the Java shell would request it. The
//! apkenv linker still serves the libs it CAN relocate; only the engine routes here.
//!
//! ## Persistence (no munmap)
//! [`LoadedEngine`] owns the [`LoadedImageSet`], whose `Drop` `munmap`s the 112 MiB mapping. The
//! engine's constructors spawn background worker threads ("RBX Worker A") that keep executing the
//! mapped text, so the image MUST stay mapped for the process lifetime. Low-level callers must keep a
//! returned [`LoadedEngine`] alive; the production [`PreloadedLib`] path enforces that invariant with
//! a private no-drop owner.
//!
//! ## `unsafe` scope
//! Two confined `unsafe` jumps into foreign code: the `DT_INIT_ARRAY` constructors (shared with the
//! diagnostic harness) and the `JNI_OnLoad(JavaVM*, void*)` call. Both carry a dated `// SAFETY:`.
//! The decode/map/reloc cores stay `#![forbid(unsafe_code)]`.

use std::collections::HashSet;
use std::ffi::{c_char, c_int, c_void};
use std::io::Write;
use std::mem::ManuallyDrop;
use std::path::Path;
use std::sync::Mutex;

use jni_sys::{
    jint, JavaVM, JNI_VERSION_10, JNI_VERSION_1_2, JNI_VERSION_1_4, JNI_VERSION_1_6,
    JNI_VERSION_1_8, JNI_VERSION_9,
};

use super::bionic_env::BionicEnv;
use super::elf::{DynSym, PF_X};
use super::link::Linker;
use super::map::host_page_size;
use super::resolve::{LoadedObjectProvider, Scope, SymbolProvider};

use super::init_run::{init_array_count, init_array_entry_offset};

/// The APK-internal directory holding the app's x86-64 JNI libraries.
const LIB_X86_64_DIR: &str = "lib/x86_64";

/// The file name of the x86-64 engine `.so` (the Roblox C++ engine).
const LIBROBLOX_FILENAME: &str = "libroblox.so";

/// Process-global set of app-native-lib sonames already loaded through Eclipse's Rust loader, so the
/// pre-load loop dedups (a soname requested twice — directly or as a sibling `DT_NEEDED` — is loaded
/// once). Only the soname strings are tracked here; the actual mappings are kept alive for the process
/// lifetime by the caller binding each returned [`LoadedEngine`] (no `munmap` while bound). A `Mutex`
/// guards concurrent access, though the pre-load loop runs on the single main thread.
///
/// 2026-06-05: stores only `String` (Send-safe) — the registry's job is dedup, not ownership; the
/// `LoadedEngine`s (which own the `!Send` mappings) stay owned by the main thread that loaded them.
static LOADED_SONAMES: Mutex<Option<HashSet<String>>> = Mutex::new(None);

/// Record `soname` as loaded; returns `true` if it was newly inserted, `false` if already present
/// (a duplicate the caller should skip). Pure registry bookkeeping (no mapping work).
fn register_soname(soname: &str) -> bool {
    let mut guard = LOADED_SONAMES.lock().unwrap_or_else(|e| e.into_inner());
    guard
        .get_or_insert_with(HashSet::new)
        .insert(soname.to_owned())
}

/// Whether `soname` has already been loaded through Eclipse's Rust loader.
fn soname_is_loaded(soname: &str) -> bool {
    let guard = LOADED_SONAMES.lock().unwrap_or_else(|e| e.into_inner());
    guard.as_ref().is_some_and(|s| s.contains(soname))
}

/// Whether a native library `name` — its `DT_SONAME` **or** its `lib/<abi>/` file name — has already
/// been loaded through Eclipse's Rust loader.
///
/// 2026-06-11: public so the framework's `Runtime.nativeLoad` interception can CONSULT this registry
/// (`docs/libroblox-init-run.md` §10/§11). When ART's `System.loadLibrary` resolves a name to a path
/// whose basename is a pre-loaded soname, the interception reports success **without** re-entering the
/// apkenv shim linker (which aborts on the engine libs' modern relocs). The registry is keyed by BOTH
/// the resolved soname and the input file name (see [`load_app_native_lib`]), so a consult by the
/// path basename matches regardless of whether `DT_SONAME` equals the file name.
#[must_use]
pub fn is_preloaded(name: &str) -> bool {
    soname_is_loaded(name)
}

/// The exported symbol ART invokes after loading a JNI library to register its native methods and
/// learn its required JNI version. Looked up in the engine's dynamic symbol table (see
/// [`LoadedEngine::jni_onload_addr`]).
const JNI_ONLOAD_SYMBOL: &str = "JNI_OnLoad";

/// A `libroblox.so` mapped, relocated, fully resolved, and (optionally) init-run by Eclipse's own
/// loader, kept alive for the process lifetime.
///
/// Owns the [`LoadedImageSet`](super::link::LoadedImageSet) (its `Drop` would `munmap` the image), so
/// holding a `LoadedEngine` keeps the engine's text/data mapped while its background workers run.
pub struct LoadedEngine {
    /// The whole loaded graph (root-only: just `libroblox.so` + env-provided deps). Kept alive — do
    /// NOT drop while the engine's workers run (they execute the mapped text).
    set: super::link::LoadedImageSet,
    /// `libroblox.so`'s load base (`set.objects[0].load_base()`), cached so `JNI_OnLoad` resolution
    /// and the init-array walk need no re-borrow of the image.
    base: u64,
    /// The engine's dynamic symbol table, cloned once at load so `JNI_OnLoad` (and any future export)
    /// resolves without re-parsing the mapped ELF.
    dynsyms: Vec<DynSym>,
    /// `DT_INIT_ARRAY` vaddr + size (bytes), located at load. `None` would be malformed (the engine
    /// has 3,427 constructors); carried as `Option` so the type mirrors the parsed `DynInfo`.
    init_array: Option<(u64, u64)>,
    /// Count of `DT_INIT_ARRAY` constructors that ran via [`Self::run_init_array`] (0 until run).
    constructors_run: usize,
}

impl LoadedEngine {
    /// `libroblox.so`'s load base.
    #[must_use]
    pub fn load_base(&self) -> u64 {
        self.base
    }

    /// Number of `DT_INIT_ARRAY` constructors that completed (0 before [`Self::run_init_array`]).
    #[must_use]
    pub fn constructors_run(&self) -> usize {
        self.constructors_run
    }

    /// The absolute runtime address of the engine's exported `JNI_OnLoad`, or `None` if the engine
    /// does not export one (it does — verified GLOBAL FUNC at vaddr `0x1f3d5b1`).
    ///
    /// Resolves the symbol the same way the relocation scope does: a [`LoadedObjectProvider`] over the
    /// engine's own exported definitions (defined, GLOBAL/WEAK, non-UND), then `base + st_value`. Pure
    /// (no `unsafe`, no foreign call) — just the symbol-table lookup.
    #[must_use]
    pub fn jni_onload_addr(&self) -> Option<u64> {
        let provider = LoadedObjectProvider::new(self.base, &self.dynsyms);
        provider
            .resolve(JNI_ONLOAD_SYMBOL)
            .map(|resolved| resolved.addr)
    }

    /// The absolute runtime address of an exported symbol `name` (a defined `GLOBAL`/`WEAK` export),
    /// or `None`. Resolves via the SAME [`LoadedObjectProvider`] the relocation scope uses
    /// (`base + st_value`), matching by the symbol's BASE name (a GNU symbol version such as
    /// `@@LIBROBLOX` lives in the version table, not in `st_name`, so the base name still matches).
    ///
    /// 2026-06-11: the pre-loaded-lib `Java_*` discovery-gap fix uses this to bind ART's declared
    /// native to the implementation's address in Eclipse's mapped image — ART cannot `dlsym` the lib
    /// itself (Eclipse mmap-pre-loaded it, so it is not in ART's `libraries_`).
    #[must_use]
    pub fn resolve_export(&self, name: &str) -> Option<u64> {
        LoadedObjectProvider::new(self.base, &self.dynsyms)
            .resolve(name)
            .map(|resolved| resolved.addr)
    }

    /// Every `Java_*` native this image DEFINES and exports, as `(symbol_name, absolute_addr)`. The
    /// general discovery-gap fix ([`super::jni_register::register_all_preloaded_natives`]) demangles
    /// each and RegisterNatives it with ART (which cannot `dlsym` an Eclipse-mmap-pre-loaded lib).
    ///
    /// 2026-06-11: a defined export is `st_shndx != SHN_UNDEF` (0) with `GLOBAL`/`WEAK` binding; the
    /// address is `base + st_value` (a GNU symbol version such as `@@LIBROBLOX` is not part of the
    /// `st_name`, so the base name is what `RegisterNatives` needs). libroblox exports ~499 of these.
    #[must_use]
    pub fn java_native_exports(&self) -> Vec<(String, u64)> {
        const SHN_UNDEF: u16 = 0;
        const STB_GLOBAL: u8 = 1;
        const STB_WEAK: u8 = 2;
        self.dynsyms
            .iter()
            .filter(|s| {
                s.shndx != SHN_UNDEF
                    && (s.bind == STB_GLOBAL || s.bind == STB_WEAK)
                    && s.name.starts_with("Java_")
            })
            .map(|s| (s.name.clone(), self.base.wrapping_add(s.value)))
            .collect()
    }
}

/// Errors from the live engine-load path. Carries the lib's APK entry name for context where the
/// failing operation is per-lib (so a pre-load loop over several libs names the one that failed).
#[derive(Debug)]
pub enum EngineLoadError {
    /// Opening or reading the app native lib (`lib/x86_64/<name>.so`) from the APK failed; carries
    /// the entry path and the underlying error.
    Apk(String, String),
    /// Staging the extracted `.so` to a temp path failed.
    Stage(String),
    /// The map / relocate / resolve pipeline failed (a real loader error).
    Link(String),
    /// One or more strong (non-weak) imports stayed unresolved — running constructors would jump
    /// through a null GOT slot. Carries the unresolved-RELOC count AND the sorted, de-duped symbol
    /// names (the work-list must be 0 for a sound run). The Display prints BOTH counts
    /// disambiguated — `{names} strong import(s) unresolved ({relocs} reloc(s))` — since one
    /// symbol may back several relocs (2026-06-12).
    ///
    /// 2026-06-12: the names are load-bearing diagnostics — the pre-load warning printed only a
    /// COUNT, so the 2 imports that failed `libbacktrace-native.so`'s pre-load (sending its
    /// `System.loadLibrary` into the apkenv shim linker's fatal NULL `_r_debug_ptr` write, core
    /// 866509) had to be reconstructed by offline readelf set-arithmetic instead of being read
    /// off the log.
    UnresolvedImports(usize, Vec<String>),
    /// The text segment is not executable after mapping (would fault the first constructor).
    TextNotExecutable(String),
    /// The mapped image lacks a `DT_INIT_ARRAY` (unexpected — the engine has 3,427 constructors).
    NoInitArray,
    /// A null constructor slot was found in `DT_INIT_ARRAY` (a base-relocation bug); carries the index.
    NullConstructor(usize),
    /// Reading a `DT_INIT_ARRAY` entry from the mapped image failed.
    ReadInitArray(String),
    /// The engine does not export `JNI_OnLoad` (it should).
    NoJniOnLoad,
}

impl std::fmt::Display for EngineLoadError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::Apk(entry, e) => write!(f, "read {entry} from APK: {e}"),
            Self::Stage(e) => write!(f, "stage libroblox.so: {e}"),
            Self::Link(e) => write!(f, "map/relocate/resolve libroblox.so: {e}"),
            Self::UnresolvedImports(n, names) => {
                // 2026-06-12: the import COUNT is `names.len()`; `n` is the unresolved RELOC count
                // (one symbol may back several relocs). The old wording printed the reloc count as
                // "import(s)", re-creating the exact count/name triage ambiguity the named output
                // exists to remove (live log line 60: "1 strong import(s)" for 1 name was only
                // coincidentally aligned).
                write!(
                    f,
                    "{} strong import(s) unresolved ({n} reloc(s)): {}",
                    names.len(),
                    names.join(", ")
                )
            }
            Self::TextNotExecutable(d) => write!(f, "engine text segment not executable: {d}"),
            Self::NoInitArray => write!(f, "mapped libroblox.so has no DT_INIT_ARRAY"),
            Self::NullConstructor(i) => {
                write!(f, "null DT_INIT_ARRAY constructor slot at index {i}")
            }
            Self::ReadInitArray(e) => write!(f, "read DT_INIT_ARRAY entry: {e}"),
            Self::NoJniOnLoad => write!(f, "libroblox.so does not export JNI_OnLoad"),
        }
    }
}

impl std::error::Error for EngineLoadError {}

/// Map + base-relocate + fully-resolve `libroblox.so` from `apk_path` using Eclipse's own Rust loader,
/// returning the live, persistent image (NOT `_exit`ed, NOT `munmap`ped). Does **not** run the
/// constructors — call [`LoadedEngine::run_init_array`] for that, then [`call_jni_onload`].
///
/// Thin wrapper over [`map_resolve_app_lib`] for the engine `.so` (no extra search dir — libroblox's
/// `DT_NEEDED` are all bionic, supplied by the scope). Preserved as the engine's named entry point so
/// the existing main-thread call site and tests are unchanged.
///
/// `log` receives flushed progress lines (the caller passes the run's stderr/stdout).
///
/// MUST be called on the process **main thread** (the foreign init code and the engine's workers
/// expect a real deep stack; the same contract as the diagnostic harness).
pub fn load_libroblox(
    apk_path: &Path,
    log: &mut impl Write,
) -> Result<LoadedEngine, EngineLoadError> {
    let engine = map_resolve_app_lib(apk_path, LIBROBLOX_FILENAME, None, log)?;
    // The engine has 3,427 constructors; a missing DT_INIT_ARRAY would be a real malformation (not
    // the optional-ctor case the generic path tolerates for lazy-native libs).
    if engine.init_array.is_none() {
        return Err(EngineLoadError::NoInitArray);
    }
    Ok(engine)
}

/// Map + base-relocate + fully-resolve one app native lib `lib/x86_64/<filename>` from `apk_path`
/// using Eclipse's own Rust loader, returning the live, persistent image (NOT `_exit`ed, NOT
/// `munmap`ped). Does **not** run constructors or `JNI_OnLoad` — that is [`load_app_native_lib`].
///
/// The pipeline is the proven one from the diagnostic harness ([`super::init_run`]): root-only load
/// (env-provided bionic deps, host fallback off) → base relocations (`R_X86_64_RELATIVE`) → RELRO →
/// the FULL Eclipse scope (`[LoadedObjectProvider(root)] + BionicEnv` with the Eclipse-native tier
/// prepended) applied to the symbol relocations. It then confirms the text segment is `PROT_EXEC`
/// and locates `DT_INIT_ARRAY` (each app JNI lib has one).
///
/// `search_dir`, when `Some`, is added to the linker's `DT_NEEDED` search path so a lib's **sibling
/// app-lib** dependency (e.g. `libeigen_lapack.so` `NEEDED libeigen_blas.so`) loads from the
/// already-extracted lib dir through this same loader. Bionic `DT_NEEDED` (`libc/m/dl/...`) still
/// resolve via the scope (they are not on disk under those bare sonames), never from the host.
///
/// MUST be called on the process **main thread** (the foreign init code and the engine's workers
/// expect a real deep stack; the same contract as the diagnostic harness).
fn map_resolve_app_lib(
    apk_path: &Path,
    filename: &str,
    search_dir: Option<&Path>,
    log: &mut impl Write,
) -> Result<LoadedEngine, EngineLoadError> {
    let entry = format!("{LIB_X86_64_DIR}/{filename}");
    let _ = writeln!(
        log,
        "engine-load: routing {entry} through Eclipse's Rust loader"
    );

    // ---- 1) Read the lib from the APK + stage it to a temp file ----------------------------------
    // Serve assets/* to the engine's AAssetManager natives from this same APK (idempotent).
    super::ndk_registry::set_apk_path(apk_path.to_path_buf());
    let mut apk = crate::apk::Apk::open(apk_path)
        .map_err(|e| EngineLoadError::Apk(entry.clone(), e.to_string()))?;
    let so_bytes = apk
        .read_entry(&entry)
        .map_err(|e| EngineLoadError::Apk(entry.clone(), e.to_string()))?;
    let _ = writeln!(log, "engine-load: {filename} = {} bytes", so_bytes.len());

    let dir = std::env::temp_dir().join(format!("eclipse-engine-{}", std::process::id()));
    std::fs::create_dir_all(&dir).map_err(|e| EngineLoadError::Stage(e.to_string()))?;
    let so_path = dir.join(filename);
    std::fs::write(&so_path, &so_bytes).map_err(|e| EngineLoadError::Stage(e.to_string()))?;

    // ---- 2) Map + base-relocate root-only (bionic deps env-provided; sibling app deps from disk) --
    // A `search_dir` lets a sibling app-lib `DT_NEEDED` load from the extracted lib dir through this
    // same loader; bionic sonames are not on disk there, so they stay env-provided (host fallback off).
    let search_paths: Vec<std::path::PathBuf> = search_dir
        .map(|d| vec![d.to_path_buf()])
        .unwrap_or_default();
    let linker = Linker::new(search_paths)
        .with_host_fallback(false)
        .with_tolerate_missing_deps(true);
    let mut set = linker
        .load(&so_path)
        .map_err(|e| EngineLoadError::Link(e.to_string()))?;
    let page = host_page_size();
    let _ = writeln!(
        log,
        "engine-load: mapped objects={} RELATIVE_applied={} RELRO_applied={}",
        set.objects.len(),
        set.stats.relative_applied,
        set.relro_applied
    );

    // ---- 3) Build the FULL Eclipse scope + apply the symbol relocations (work-list 0) -------------
    let base = set.objects[0].load_base();
    let soname = set.objects[0].soname.clone();
    let dynsyms = {
        let img = set.objects[0]
            .image()
            .map_err(|e| EngineLoadError::Link(e.to_string()))?;
        img.dynsyms.clone()
    };
    let mut scope = Scope::new();
    scope.push(Box::new(LoadedObjectProvider::new(base, &dynsyms)));
    for p in BionicEnv::with_host_baseline(true, true).into_providers() {
        scope.push(p);
    }
    let sym_stats = set
        .relocate_object_symbols_partial(&soname, &scope, page)
        .map_err(|e| EngineLoadError::Link(e.to_string()))?;
    let _ = writeln!(
        log,
        "engine-load: symbol relocs applied_nonnull={} weak_zero={} unresolved_strong={}",
        sym_stats.applied_nonnull, sym_stats.applied_weak_zero, sym_stats.unresolved_strong
    );
    if sym_stats.unresolved_strong != 0 {
        // A non-zero work-list means a constructor could jump through a null GOT slot — refuse to
        // run. Carry the NAMES (stats.unresolved is sorted + de-duped) so the caller's warning
        // identifies the missing natives on the log itself (2026-06-12 — see UnresolvedImports).
        return Err(EngineLoadError::UnresolvedImports(
            sym_stats.unresolved_strong,
            sym_stats.unresolved,
        ));
    }

    // ---- 4) Confirm the text segment is PROT_EXEC + locate DT_INIT_ARRAY (if any) ------------------
    // A `DT_INIT_ARRAY` is OPTIONAL here: most app JNI libs (e.g. `libzstd-jni`, the eigen/yuv libs)
    // ship none and register their `Java_*` natives lazily; only libroblox/libbacktrace-native carry
    // constructors. `load_libroblox` separately asserts the engine has one (its 3,427 ctors are not
    // optional). 2026-06-05.
    let init_array = {
        let img = set.objects[0]
            .image()
            .map_err(|e| EngineLoadError::Link(e.to_string()))?;
        // The mapper sets each PT_LOAD's final protection to its p_flags; confirm an executable
        // (PF_X) segment exists so any constructor's (or exported native's) jump lands in PROT_EXEC.
        if !img.loads.iter().any(|s| s.flags & PF_X != 0) {
            return Err(EngineLoadError::TextNotExecutable(
                "no PF_X segment in PT_LOAD table".to_string(),
            ));
        }
        img.dyn_info.init_array
    };
    match init_array {
        Some((vaddr, size)) => {
            let count = init_array_count(size);
            let _ = writeln!(
                log,
                "engine-load: text PROT_EXEC ✓; DT_INIT_ARRAY vaddr={vaddr:#x} -> {count} constructors"
            );
        }
        None => {
            let _ = writeln!(
                log,
                "engine-load: text PROT_EXEC ✓; no DT_INIT_ARRAY (lazy-native lib — no constructors)"
            );
        }
    }
    let _ = log.flush();

    // ---- 5) Publish every object of this kept-alive set to the module registry -------------------
    // 2026-06-12 (core 1223806): the engine's statically-linked libc++abi unwinder finds FDEs via
    // its `dl_iterate_phdr@LIBC` import — which binds to Eclipse's `module_registry` native. The
    // records must exist BEFORE any engine instruction runs (constructors/`JNI_OnLoad` come after
    // this fn returns), or the first C++ throw std::terminate-loops to death (61,497 iterations in
    // that core). Registration here is symmetric with `LoadedEngine::drop` (the dedup/error paths
    // drop the engine, unregistering the records before the images unmap — no dangling walk).
    for obj in &set.objects {
        let obj_dynsyms = if obj.load_base() == base {
            dynsyms.clone()
        } else {
            obj.image()
                .map(|img| img.dynsyms.clone())
                .unwrap_or_default()
        };
        match super::module_registry::ModuleRecord::for_image(
            &obj.path,
            &obj.bytes,
            &obj_dynsyms,
            obj.load_base(),
            obj.mapped.span() as u64,
        ) {
            Ok(rec) => super::module_registry::register_module(rec),
            // A well-formed mapped lib always derives (its phdrs sit in the first PT_LOAD); a
            // failure here loses dl_iterate_phdr/dladdr visibility for THIS object only — warn
            // loudly, never fabricate a record.
            Err(e) => {
                let _ = writeln!(
                    log,
                    "engine-load: WARNING: module-registry record for {} failed ({e}) — \
                     its PCs stay invisible to dl_iterate_phdr/dladdr",
                    obj.soname
                );
            }
        }
    }

    Ok(LoadedEngine {
        set,
        base,
        dynsyms,
        init_array,
        constructors_run: 0,
    })
}

impl Drop for LoadedEngine {
    fn drop(&mut self) {
        // 2026-06-12 (core 1223806): symmetric module-registry removal — a dropped engine (the
        // dedup-skip and error paths in `load_app_native_lib`) must not leave
        // dl_iterate_phdr/dladdr records pointing at munmapped memory. Runs before the field
        // drops `munmap` the set (Rust drops the body first, then the fields). A never-registered
        // base is a no-op.
        for obj in &self.set.objects {
            let _ = super::module_registry::unregister_module(obj.load_base());
        }
    }
}

impl LoadedEngine {
    /// Run the engine's `DT_INIT_ARRAY` constructors in order (the proven 3,427-ctor static-init).
    ///
    /// Each entry is an absolute runtime address (the base-relocation pass already rewrote each slot);
    /// they are called as `extern "C" fn(int, char**, char**)` with the bionic init-array convention
    /// `argc=1 / argv=["libroblox", NULL] / envp=[NULL]` (a `void(void)` ctor ignores the args).
    ///
    /// On success returns the count that completed. Unlike the diagnostic harness, this does NOT
    /// install a crash handler or `_exit` — the constructors are proven to complete deterministically
    /// (AGENTS.md §6 thread-lifecycle), so a fault here is a real regression the caller's normal crash
    /// handling surfaces. The engine spawns background worker threads during init; the caller MUST keep
    /// `self` alive afterward (the workers execute the mapped text).
    ///
    /// MUST be called on the process **main thread** (the foreign code expects a real deep stack).
    pub fn run_init_array(&mut self, log: &mut impl Write) -> Result<usize, EngineLoadError> {
        let (init_array_vaddr, init_arraysz) =
            self.init_array.ok_or(EngineLoadError::NoInitArray)?;
        let count = init_array_count(init_arraysz);
        let obj = &self.set.objects[0];

        // Snapshot the absolute constructor addresses (RELRO already made the array read-only, so the
        // snapshot matches the live slots).
        let mut entries: Vec<u64> = Vec::with_capacity(count);
        for i in 0..count {
            let off = init_array_entry_offset(init_array_vaddr, i) as usize;
            let addr = obj
                .mapped
                .read_u64(off)
                .map_err(|e| EngineLoadError::ReadInitArray(e.to_string()))?;
            if addr == 0 {
                return Err(EngineLoadError::NullConstructor(i));
            }
            entries.push(addr);
        }
        let _ = writeln!(
            log,
            "engine-load: running {count} DT_INIT_ARRAY constructors…"
        );
        let _ = log.flush();

        // Plausible (argc, argv, envp) for the bionic init-array convention.
        let arg0 = b"libroblox\0";
        let mut argv: [*mut c_char; 2] = [arg0.as_ptr() as *mut c_char, std::ptr::null_mut()];
        let mut envp: [*mut c_char; 1] = [std::ptr::null_mut()];
        let argc: c_int = 1;

        let mut completed = 0usize;
        for &addr in &entries {
            // SAFETY: 2026-06-05 — `addr` is a constructor function pointer read from the engine's
            // DT_INIT_ARRAY after the base-relocation pass rewrote each slot to its absolute runtime
            // address; the slot lies inside the mapped, relocated, RELRO-hardened image and points
            // into the PF_X (PROT_EXEC-confirmed) text segment. The gABI/bionic init-array convention
            // is `void(int,char**,char**)` (a `void(void)` ctor ignores the SysV-register args), so
            // calling through that signature is ABI-safe either way. The full 3,427-ctor run is proven
            // to complete deterministically (AGENTS.md §6). Runs on the process main thread.
            let ctor: extern "C" fn(c_int, *mut *mut c_char, *mut *mut c_char) =
                unsafe { std::mem::transmute::<u64, _>(addr) };
            ctor(argc, argv.as_mut_ptr(), envp.as_mut_ptr());
            completed += 1;
        }
        self.constructors_run = completed;
        let _ = writeln!(
            log,
            "engine-load: {completed}/{count} constructors completed ✓"
        );
        let _ = log.flush();
        Ok(completed)
    }
}

/// Call the engine's exported `JNI_OnLoad(JavaVM* vm, void* reserved)` with Eclipse's real ART
/// `JavaVM`, returning the JNI version it reports (e.g. `JNI_VERSION_1_6`).
///
/// ART calls `JNI_OnLoad` after loading a JNI library so the library can `RegisterNatives` its native
/// methods against the VM and declare the JNI version it needs. Routing libroblox through Eclipse's
/// own loader bypasses ART's `Runtime.nativeLoad`, so Eclipse must make this call itself, handing the
/// engine the SAME `JavaVM` ART booted (`runtime::Vm::as_raw`). After this, the engine's native
/// methods are registered against ART.
///
/// `java_vm` MUST be the live process `JavaVM*` from `JNI_CreateJavaVM` (non-null). A return value
/// `< JNI_VERSION_1_2` (e.g. `JNI_ERR == -1`) means the engine reported an error — the caller logs it.
///
/// MUST be called on the VM's main (JNI-attached) thread.
///
/// # Errors
/// [`EngineLoadError::NoJniOnLoad`] if the engine exports no `JNI_OnLoad`.
pub fn call_jni_onload(
    engine: &LoadedEngine,
    java_vm: *mut JavaVM,
    log: &mut impl Write,
) -> Result<jint, EngineLoadError> {
    let addr = engine
        .jni_onload_addr()
        .ok_or(EngineLoadError::NoJniOnLoad)?;
    let _ = writeln!(
        log,
        "engine-load: calling JNI_OnLoad @ base+{:#x} (abs {:#x}) with the ART JavaVM…",
        addr.wrapping_sub(engine.base),
        addr
    );
    let _ = log.flush();

    // SAFETY: 2026-06-05 — `addr` is the absolute runtime address of the engine's exported
    // `JNI_OnLoad` (GLOBAL FUNC), resolved from its dynamic symbol table as `load_base + st_value`;
    // it lies in the PF_X (PROT_EXEC-confirmed) text of the mapped, fully-relocated image. The JNI
    // C-ABI for `JNI_OnLoad` is `jint JNI_OnLoad(JavaVM*, void*)`. `java_vm` is the live process
    // `JavaVM*` from `JNI_CreateJavaVM` (the caller passes `runtime::Vm::as_raw`, non-null). Runs on
    // the JNI-attached main thread. The engine's full DT_INIT_ARRAY has already run, so its C++
    // runtime is initialized before this entry.
    let onload: extern "C" fn(*mut JavaVM, *mut c_void) -> jint =
        unsafe { std::mem::transmute::<u64, _>(addr) };
    let version = onload(java_vm, std::ptr::null_mut());

    let _ = writeln!(
        log,
        "engine-load: JNI_OnLoad returned {version:#x} ({})",
        describe_jni_version(version)
    );
    let _ = log.flush();
    Ok(version)
}

/// A fully initialized native image whose mapping must remain resident until the kernel tears down
/// the process.
///
/// 2026-07-17: this is deliberately a no-drop owner. `libroblox` constructors leave worker threads
/// alive after Android's blocking `destroyLuaApp` completes, and ART retains native entry pointers
/// from every preloaded JNI library. Letting ordinary Rust scope teardown drop [`LoadedEngine`]
/// therefore `munmap`ped code that foreign threads could still execute. [`ManuallyDrop`] makes the
/// already-documented process-lifetime contract structural, matching `runtime::boot` keeping
/// `libart.so` mapped while ART daemon threads remain. This is bounded to the one app/native-lib set
/// in this process; the kernel reclaims every mapping at process exit.
struct ProcessLifetimeEngine {
    _engine: ManuallyDrop<LoadedEngine>,
}

impl ProcessLifetimeEngine {
    fn new(engine: LoadedEngine) -> Self {
        Self {
            _engine: ManuallyDrop::new(engine),
        }
    }
}

/// The outcome of pre-loading one app native lib through Eclipse's Rust loader.
///
/// `None` for a lib already loaded (deduped — its soname was in the registry). `Some` carries the
/// process-lifetime native mapping plus what ran.
#[must_use]
pub struct PreloadedLib {
    /// The lib's resolved soname (the registry/dedup key).
    pub soname: String,
    /// `DT_INIT_ARRAY` constructors that ran (0 for a lazy-native lib with no init array).
    pub constructors_run: usize,
    /// `Some(version)` if the lib exported `JNI_OnLoad` and it was called (the returned JNI version),
    /// `None` if it exports none (a lazy-native lib whose `Java_*` methods ART binds on demand).
    pub jni_onload_version: Option<jint>,
    /// The initialized image. This private no-drop owner prevents lexical scope teardown from
    /// unmapping code still referenced by foreign workers or ART's registered native pointers.
    _engine: ProcessLifetimeEngine,
}

/// Pre-load one app native lib `lib/x86_64/<filename>` through Eclipse's own Rust loader — the SAME
/// proven pipeline that loads the engine — so it is mapped, relocated, fully resolved, init-run, and
/// (if it exports `JNI_OnLoad`) handed the real ART `JavaVM` BEFORE the framework lifecycle, instead of
/// falling to ART's `Runtime.nativeLoad` → the apkenv shim linker (which aborts on the modern relocs).
///
/// This is the generalization of [`load_libroblox`] to the app's sibling JNI libs (e.g. `libzstd-jni`,
/// which `androidx.startup` loads during `onCreate`). It:
/// 1. dedups by soname (returns `Ok(None)` if this lib was already pre-loaded — directly or as a
///    sibling `DT_NEEDED` pulled in by an earlier lib),
/// 2. maps + base-relocates + fully-resolves it via [`map_resolve_app_lib`] (bionic `DT_NEEDED`
///    resolve through the full `BionicEnv` scope; sibling **app-lib** `DT_NEEDED` load from
///    `search_dir` through this same loader — NOT apkenv),
/// 3. runs its `DT_INIT_ARRAY` constructors if it has any (most lazy-native libs have none),
/// 4. calls its exported `JNI_OnLoad(JavaVM*, void*)` with the live ART VM if it exports one (a lib
///    with none registers its `Java_*` natives lazily — that is sound, nothing to call),
/// 5. records the soname in the persistent registry and returns the kept-alive image.
///
/// `search_dir` is the dir the app libs were extracted to (so a sibling-lib `DT_NEEDED` resolves from
/// disk). `java_vm` MUST be the live process `JavaVM*` (non-null). MUST be called on the process
/// **main thread**, VM alive + JNI-attached.
pub fn load_app_native_lib(
    apk_path: &Path,
    filename: &str,
    java_vm: *mut JavaVM,
    search_dir: &Path,
    log: &mut impl Write,
) -> Result<Option<PreloadedLib>, EngineLoadError> {
    // 2026-06-12: early-fault tap (diagnostic — AGENTS.md §5 frontier: dump the engine's ORIGINAL
    // SIGSEGV before crashpad's broken logging path destroys the evidence). Installed here because
    // this is AFTER ART booted (this fn's contract requires the live `JavaVM*`, so everything
    // ART/sigchain installs is final and the captured pre-tap action is ART's true post-boot one)
    // and BEFORE any engine instruction can run (engine code executes only via the constructors /
    // `JNI_OnLoad` below and what they spawn/register). A failed install only loses the
    // diagnostic — never abort the boot for it.
    static EARLY_FAULT_TAP: std::sync::Once = std::sync::Once::new();
    EARLY_FAULT_TAP.call_once(|| {
        if let Err(e) = super::native_provider::install_early_fault_tap(libc::SIGSEGV) {
            let _ = writeln!(
                log,
                "engine-load: early-fault tap install failed ({e}) — continuing without the diagnostic"
            );
        }
    });

    // Dedup by the implied soname (the filename — an app lib's DT_SONAME equals its file name here).
    // The authoritative dedup is by resolved soname after the map (below); this cheap pre-check skips
    // a redundant 100+ MiB read+map when a lib was already pulled in as a sibling DT_NEEDED.
    if soname_is_loaded(filename) {
        let _ = writeln!(
            log,
            "engine-load: {filename} already loaded (deduped) — skipping"
        );
        return Ok(None);
    }

    let mut engine = map_resolve_app_lib(apk_path, filename, Some(search_dir), log)?;
    let soname = engine.set.objects[0].soname.clone();

    // 2026-06-12: publish the engine's mapped range to the early-fault tap BEFORE run_init_array
    // below (the first engine instruction), so the tap's engine-PC filter covers engine faults
    // from the very first constructor onward.
    if filename == LIBROBLOX_FILENAME {
        super::native_provider::publish_engine_text_range(
            engine.base,
            engine.set.objects[0].mapped.span() as u64,
        );
    }

    // The resolved soname is the authoritative dedup key (it may differ from the file name). If a
    // sibling DT_NEEDED already loaded this exact object, register_soname returns false → skip.
    if !register_soname(&soname) {
        let _ = writeln!(
            log,
            "engine-load: {soname} already loaded (deduped by soname) — skipping"
        );
        return Ok(None);
    }

    // 2026-06-11: also index by the input file name so the `Runtime.nativeLoad` consult
    // ([`is_preloaded`]) — which derives the soname from the PATH ART resolved — matches even when
    // `DT_SONAME` differs from the file name. (Idempotent; the soname above is the dedup decider.)
    if soname != filename {
        let _ = register_soname(filename);
    }

    // Run constructors only if this lib has a DT_INIT_ARRAY (lazy-native libs ship none).
    let constructors_run = if engine.init_array.is_some() {
        engine.run_init_array(log)?
    } else {
        0
    };

    // Call JNI_OnLoad only if the lib exports one (lazy-native libs register Java_* methods on demand).
    let jni_onload_version = if engine.jni_onload_addr().is_some() {
        Some(call_jni_onload(&engine, java_vm, log)?)
    } else {
        let _ = writeln!(
            log,
            "engine-load: {soname} exports no JNI_OnLoad (lazy-native lib — ART binds Java_* on demand)"
        );
        None
    };

    // 2026-06-11: discovery-gap fix — ART cannot `dlsym` this Eclipse-mmap-pre-loaded lib (it is not in
    // ART's `libraries_`), so its `Java_*` natives would surface as `UnsatisfiedLinkError`. RegisterNatives
    // the natives this lib provides directly with ART, binding each to its export address. Best-effort
    // (strictly additive — never regresses the boot). `java_vm` is the live process `JavaVM*` the caller
    // passed (`runtime::Vm::as_raw`, non-null) and we are on the JNI-attached main thread (the pre-load
    // context) — the documented pointer contract of these functions. The GENERAL pass reflects + binds
    // every exported `Java_*` native; if it binds nothing (systemic reflection failure) we fall back to
    // the hand-curated criticals so a regression cannot drop below the proven baseline.
    let bound = super::jni_register::register_all_preloaded_natives(
        java_vm,
        &engine.java_native_exports(),
        &soname,
        log,
    );
    if bound == 0 {
        super::jni_register::register_preloaded_natives(
            java_vm,
            |name| engine.resolve_export(name),
            log,
        );
    }

    Ok(Some(PreloadedLib {
        soname,
        constructors_run,
        jni_onload_version,
        _engine: ProcessLifetimeEngine::new(engine),
    }))
}

/// A short human label for a `JNI_OnLoad` return value (a JNI version constant or an error sentinel).
fn describe_jni_version(version: jint) -> &'static str {
    match version {
        JNI_VERSION_1_2 => "JNI_VERSION_1_2",
        JNI_VERSION_1_4 => "JNI_VERSION_1_4",
        JNI_VERSION_1_6 => "JNI_VERSION_1_6",
        JNI_VERSION_1_8 => "JNI_VERSION_1_8",
        JNI_VERSION_9 => "JNI_VERSION_9",
        JNI_VERSION_10 => "JNI_VERSION_10",
        v if v < 0 => "error (negative; JNI_OnLoad failed)",
        _ => "unrecognized JNI version",
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn jni_onload_symbol_name_is_the_jni_export() {
        // The exact exported symbol ART/Eclipse invokes — a stable part of the JNI ABI. A typo here
        // would silently make jni_onload_addr() return None (NoJniOnLoad) on a real load.
        assert_eq!(JNI_ONLOAD_SYMBOL, "JNI_OnLoad");
    }

    #[test]
    fn initialized_native_images_cannot_be_unmapped_by_scope_teardown() {
        // 2026-07-17 regression for the clean-close instruction-fetch MAPERR: if this wrapper ever
        // regains automatic drop glue, a local `Vec<PreloadedLib>` can again unregister + munmap
        // libroblox while its foreign workers are still alive. `needs_drop == false` is the strong
        // direction of the std contract: dropping this wrapper has no destructor side effect.
        assert!(!std::mem::needs_drop::<ProcessLifetimeEngine>());
    }

    #[test]
    fn libroblox_entry_is_the_x86_64_engine_path() {
        // The composed APK entry the loader reads (map_resolve_app_lib builds `<dir>/<filename>`).
        assert_eq!(LIB_X86_64_DIR, "lib/x86_64");
        assert_eq!(LIBROBLOX_FILENAME, "libroblox.so");
        assert_eq!(
            format!("{LIB_X86_64_DIR}/{LIBROBLOX_FILENAME}"),
            "lib/x86_64/libroblox.so"
        );
    }

    #[test]
    fn describe_jni_version_labels_the_common_constants() {
        assert_eq!(describe_jni_version(JNI_VERSION_1_6), "JNI_VERSION_1_6");
        assert_eq!(describe_jni_version(JNI_VERSION_1_8), "JNI_VERSION_1_8");
        // JNI_ERR (-1) and any negative is an error sentinel, not a version.
        assert_eq!(
            describe_jni_version(-1),
            "error (negative; JNI_OnLoad failed)"
        );
        // An unknown positive value is reported as unrecognized (not mislabelled as a known version).
        assert_eq!(
            describe_jni_version(0x7fff_0000),
            "unrecognized JNI version"
        );
    }

    #[test]
    fn jni_version_1_6_is_the_art_default() {
        // Eclipse boots ART at JNI 1.6 (runtime.rs); the engine is expected to request ≥ 1.6. Pin the
        // constant so a jni-sys bump that changed it would fail here, not silently at runtime.
        assert_eq!(JNI_VERSION_1_6, 0x0001_0006);
    }

    #[test]
    fn unresolved_imports_error_names_the_symbols() {
        // 2026-06-12: the pre-load failure warning printed only a COUNT, so core 866509's 2-symbol
        // work-list (__android_log_vprint + __umask_chk, the failed libbacktrace-native.so
        // pre-load that re-opened the apkenv delegation) had to be reconstructed by offline
        // readelf set-arithmetic. The Display output must NAME every unresolved import so the next
        // fall-through is identified from the boot log itself — AND it must disambiguate the
        // import count (names) from the reloc count (one symbol may back several relocs): the old
        // wording printed the reloc count as "import(s)", re-creating the triage ambiguity. The
        // fixture deliberately uses 3 relocs over 2 names so a conflation cannot pass.
        let e = EngineLoadError::UnresolvedImports(
            3,
            vec![
                "__android_log_vprint".to_string(),
                "__umask_chk".to_string(),
            ],
        );
        let msg = e.to_string();
        assert!(
            msg.contains("2 strong import(s) unresolved (3 reloc(s))"),
            "import count = the NAME count, reloc count separate + labelled: {msg}"
        );
        assert!(
            msg.contains("__android_log_vprint") && msg.contains("__umask_chk"),
            "names every unresolved import: {msg}"
        );
    }

    #[test]
    fn soname_registry_dedups_by_soname() {
        // The pre-load loop relies on register_soname returning true exactly once per soname so a lib
        // pulled in twice (directly + as a sibling DT_NEEDED) is mapped+init-run once. Uses unique test
        // sonames so the process-global registry is not polluted by / does not collide with other tests.
        let a = "libengine-test-dedup-A.so";
        let b = "libengine-test-dedup-B.so";
        assert!(!soname_is_loaded(a), "fresh soname must start unloaded");
        assert!(register_soname(a), "first insert is newly inserted (true)");
        assert!(soname_is_loaded(a), "after insert the soname reads loaded");
        assert!(
            !register_soname(a),
            "second insert of the same soname is a dedup (false)"
        );
        // A different soname is independent (the set keys per-soname, not a single flag).
        assert!(!soname_is_loaded(b));
        assert!(register_soname(b));
        assert!(!register_soname(b));
        // The first soname is still recorded (inserting b did not evict a).
        assert!(soname_is_loaded(a));
    }

    #[test]
    fn is_preloaded_reflects_registration() {
        // The public consult the framework's Runtime.nativeLoad interception uses: false before, true
        // after registering. Uses a unique soname so the process-global registry isn't polluted by /
        // doesn't collide with other tests.
        let s = "libengine-test-is-preloaded.so";
        assert!(!is_preloaded(s), "fresh soname must read not-preloaded");
        assert!(register_soname(s));
        assert!(
            is_preloaded(s),
            "after registration the public consult reports preloaded"
        );
    }

    #[test]
    fn preloaded_lib_fields_express_the_optional_paths() {
        // The generic pre-load distinguishes a lazy-native lib (no ctors, no JNI_OnLoad) from an
        // engine-class lib (ctors + JNI_OnLoad). report_preloaded()/the caller read exactly these
        // fields, so pin that a None jni_onload_version + 0 constructors is the lazy-native shape and
        // a Some(version) + n constructors is the engine shape (no hidden coupling between them).
        // (Pure field-shape check — no mapping; the real images come from the integration run.)
        fn classify(constructors_run: usize, jni_onload_version: Option<jint>) -> &'static str {
            match (constructors_run, jni_onload_version) {
                (0, None) => "lazy-native",
                (_, Some(_)) => "engine-class",
                (_, None) => "ctors-only",
            }
        }
        assert_eq!(classify(0, None), "lazy-native"); // e.g. libzstd-jni
        assert_eq!(classify(3427, Some(JNI_VERSION_1_6)), "engine-class"); // libroblox
        assert_eq!(classify(2, None), "ctors-only"); // e.g. libbacktrace-native's ctors w/o onload-call
    }
}
