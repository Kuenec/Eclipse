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
//! mapped text, so the image MUST stay mapped for the process lifetime. The caller binds the returned
//! [`LoadedEngine`] for the whole run (it is never dropped while the workers run).
//!
//! ## `unsafe` scope
//! Two confined `unsafe` jumps into foreign code: the `DT_INIT_ARRAY` constructors (shared with the
//! diagnostic harness) and the `JNI_OnLoad(JavaVM*, void*)` call. Both carry a dated `// SAFETY:`.
//! The decode/map/reloc cores stay `#![forbid(unsafe_code)]`.

use std::ffi::{c_char, c_int, c_void};
use std::io::Write;
use std::path::Path;

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

/// The APK-internal path of the x86-64 engine `.so`.
const LIBROBLOX_ENTRY: &str = "lib/x86_64/libroblox.so";

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
}

/// Errors from the live engine-load path.
#[derive(Debug)]
pub enum EngineLoadError {
    /// Opening or reading `lib/x86_64/libroblox.so` from the APK failed.
    Apk(String),
    /// Staging the extracted `.so` to a temp path failed.
    Stage(String),
    /// The map / relocate / resolve pipeline failed (a real loader error).
    Link(String),
    /// One or more strong (non-weak) imports stayed unresolved — running constructors would jump
    /// through a null GOT slot. Carries the count (the work-list must be 0 for a sound run).
    UnresolvedImports(usize),
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
            Self::Apk(e) => write!(f, "read {LIBROBLOX_ENTRY} from APK: {e}"),
            Self::Stage(e) => write!(f, "stage libroblox.so: {e}"),
            Self::Link(e) => write!(f, "map/relocate/resolve libroblox.so: {e}"),
            Self::UnresolvedImports(n) => {
                write!(f, "{n} strong import(s) unresolved (work-list non-empty)")
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
/// The pipeline is the proven one from the diagnostic harness ([`super::init_run`]): root-only load
/// (env-provided bionic deps, host fallback off) → base relocations (`R_X86_64_RELATIVE`) → RELRO →
/// the FULL Eclipse scope (`[LoadedObjectProvider(libroblox)] + BionicEnv` with the Eclipse-native
/// tier prepended) applied to the symbol relocations (all 584 imports resolve, work-list 0). It then
/// confirms the text segment is `PROT_EXEC` and locates `DT_INIT_ARRAY`.
///
/// `log` receives flushed progress lines (the caller passes the run's stderr/stdout).
///
/// MUST be called on the process **main thread** (the foreign init code and the engine's workers
/// expect a real deep stack; the same contract as the diagnostic harness).
pub fn load_libroblox(
    apk_path: &Path,
    log: &mut impl Write,
) -> Result<LoadedEngine, EngineLoadError> {
    let _ = writeln!(
        log,
        "engine-load: routing {LIBROBLOX_ENTRY} through Eclipse's Rust loader"
    );

    // ---- 1) Read libroblox.so from the APK + stage it to a temp file -----------------------------
    // Serve assets/* to the engine's AAssetManager natives from this same APK (idempotent).
    super::ndk_registry::set_apk_path(apk_path.to_path_buf());
    let mut apk =
        crate::apk::Apk::open(apk_path).map_err(|e| EngineLoadError::Apk(e.to_string()))?;
    let so_bytes = apk
        .read_entry(LIBROBLOX_ENTRY)
        .map_err(|e| EngineLoadError::Apk(e.to_string()))?;
    let _ = writeln!(log, "engine-load: libroblox.so = {} bytes", so_bytes.len());

    let dir = std::env::temp_dir().join(format!("eclipse-engine-{}", std::process::id()));
    std::fs::create_dir_all(&dir).map_err(|e| EngineLoadError::Stage(e.to_string()))?;
    let so_path = dir.join("libroblox.so");
    std::fs::write(&so_path, &so_bytes).map_err(|e| EngineLoadError::Stage(e.to_string()))?;

    // ---- 2) Map + base-relocate root-only (deps env-provided, host fallback off) ------------------
    let linker = Linker::new(Vec::<std::path::PathBuf>::new())
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
        .relocate_object_symbols_partial("libroblox.so", &scope, page)
        .map_err(|e| EngineLoadError::Link(e.to_string()))?;
    let _ = writeln!(
        log,
        "engine-load: symbol relocs applied_nonnull={} weak_zero={} unresolved_strong={}",
        sym_stats.applied_nonnull, sym_stats.applied_weak_zero, sym_stats.unresolved_strong
    );
    if sym_stats.unresolved_strong != 0 {
        // A non-zero work-list means a constructor could jump through a null GOT slot — refuse to run.
        return Err(EngineLoadError::UnresolvedImports(
            sym_stats.unresolved_strong,
        ));
    }

    // ---- 4) Confirm the text segment is PROT_EXEC + locate DT_INIT_ARRAY --------------------------
    let init_array = {
        let img = set.objects[0]
            .image()
            .map_err(|e| EngineLoadError::Link(e.to_string()))?;
        // The mapper sets each PT_LOAD's final protection to its p_flags; confirm an executable
        // (PF_X) segment exists so the first constructor's jump lands in PROT_EXEC text.
        if !img.loads.iter().any(|s| s.flags & PF_X != 0) {
            return Err(EngineLoadError::TextNotExecutable(
                "no PF_X segment in PT_LOAD table".to_string(),
            ));
        }
        img.dyn_info.init_array
    };
    let init_array = init_array.ok_or(EngineLoadError::NoInitArray)?;
    let count = init_array_count(init_array.1);
    let _ = writeln!(
        log,
        "engine-load: text PROT_EXEC ✓; DT_INIT_ARRAY vaddr={:#x} -> {count} constructors",
        init_array.0
    );
    let _ = log.flush();

    Ok(LoadedEngine {
        set,
        base,
        dynsyms,
        init_array: Some(init_array),
        constructors_run: 0,
    })
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
    fn libroblox_entry_is_the_x86_64_engine_path() {
        assert_eq!(LIBROBLOX_ENTRY, "lib/x86_64/libroblox.so");
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
}
