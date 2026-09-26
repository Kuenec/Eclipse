use std::collections::HashSet;
use std::ffi::{c_char, c_int, c_void};
use std::io::Write;
use std::mem::ManuallyDrop;
use std::path::{Path, PathBuf};
use std::sync::Mutex;

use jni_sys::{
    jint, JavaVM, JNI_VERSION_10, JNI_VERSION_1_2, JNI_VERSION_1_4, JNI_VERSION_1_6,
    JNI_VERSION_1_8, JNI_VERSION_9,
};

use super::bionic_env::BionicEnv;
use super::elf::{DynSym, ElfImage, PF_X};
use super::link::{Linker, LoadedObject};
use super::map::{host_page_size, MappedObject};
use super::resolve::{LoadedObjectProvider, Scope, SymbolProvider};

use super::init_run::{constructor_addresses, init_array_count};

const LIBROBLOX_FILENAME: &str = "libroblox.so";

static LOADED_SONAMES: Mutex<Option<HashSet<String>>> = Mutex::new(None);

fn register_soname(soname: &str) -> bool {
    let mut guard = LOADED_SONAMES.lock().unwrap_or_else(|e| e.into_inner());
    guard
        .get_or_insert_with(HashSet::new)
        .insert(soname.to_owned())
}

fn soname_is_loaded(soname: &str) -> bool {
    let guard = LOADED_SONAMES.lock().unwrap_or_else(|e| e.into_inner());
    guard.as_ref().is_some_and(|s| s.contains(soname))
}

#[must_use]
pub fn is_preloaded(name: &str) -> bool {
    soname_is_loaded(name)
}

const JNI_ONLOAD_SYMBOL: &str = "JNI_OnLoad";

pub struct LoadedEngine {
    mapped: MappedObject,

    soname: String,

    dynsyms: Vec<DynSym>,

    init: Option<u64>,

    init_array: Option<(u64, u64)>,

    constructors_run: usize,
}

impl LoadedEngine {
    #[must_use]
    pub fn load_base(&self) -> u64 {
        self.mapped.load_base()
    }

    #[must_use]
    pub fn constructors_run(&self) -> usize {
        self.constructors_run
    }

    #[must_use]
    pub fn jni_onload_addr(&self) -> Option<u64> {
        let provider = LoadedObjectProvider::new(self.load_base(), &self.dynsyms);
        provider
            .resolve(JNI_ONLOAD_SYMBOL)
            .map(|resolved| resolved.addr)
    }

    #[must_use]
    pub fn resolve_export(&self, name: &str) -> Option<u64> {
        LoadedObjectProvider::new(self.load_base(), &self.dynsyms)
            .resolve(name)
            .map(|resolved| resolved.addr)
    }

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
            .map(|s| (s.name.clone(), self.load_base().wrapping_add(s.value)))
            .collect()
    }
}

#[derive(Debug)]
pub enum EngineLoadError {
    Link(String),

    UnresolvedImports(usize, Vec<String>),

    TextNotExecutable(String),

    ReadInitArray(String),
}

impl std::fmt::Display for EngineLoadError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::Link(e) => write!(f, "map/relocate/resolve {e}"),
            Self::UnresolvedImports(n, names) => {
                write!(
                    f,
                    "{} strong import(s) unresolved ({n} reloc(s)): {}",
                    names.len(),
                    names.join(", ")
                )
            }
            Self::TextNotExecutable(d) => write!(f, "engine text segment not executable: {d}"),
            Self::ReadInitArray(e) => write!(f, "read DT_INIT_ARRAY entry: {e}"),
        }
    }
}

impl std::error::Error for EngineLoadError {}

fn map_resolve_app_lib(
    lib_dir: &Path,
    filename: &str,
    log: &mut impl Write,
) -> Result<LoadedEngine, EngineLoadError> {
    let so_path = lib_dir.join(filename);
    let _ = writeln!(
        log,
        "engine-load: routing {} through Eclipse's Rust loader",
        so_path.display()
    );

    let linker = Linker::new(Vec::<PathBuf>::new())
        .with_host_fallback(false)
        .with_tolerate_missing_deps(true);
    let mut set = linker
        .load(&so_path)
        .map_err(|e| EngineLoadError::Link(e.to_string()))?;
    let page = host_page_size();
    let _ = writeln!(
        log,
        "engine-load: mapped {filename} RELATIVE_applied={} RELRO_applied={}",
        set.stats.relative_applied, set.relro_applied
    );

    let LoadedObject {
        soname,
        path,
        bytes,
        mut mapped,
        ..
    } = set.objects.swap_remove(0);
    let link_error = |e: &dyn std::fmt::Display| EngineLoadError::Link(format!("{soname}: {e}"));
    let img = ElfImage::parse(&bytes).map_err(|e| link_error(&e))?;
    let mut scope = Scope::new();
    scope.push(Box::new(LoadedObjectProvider::new(
        mapped.load_base(),
        &img.dynsyms,
    )));
    for p in BionicEnv::with_host_baseline(true, true).into_providers() {
        scope.push(p);
    }
    let sym_stats = mapped
        .relocate_symbols_partial(&img, &scope, page)
        .map_err(|e| link_error(&e))?;
    let _ = writeln!(
        log,
        "engine-load: symbol relocs applied_nonnull={} weak_zero={} unresolved_strong={}",
        sym_stats.applied_nonnull, sym_stats.applied_weak_zero, sym_stats.unresolved_strong
    );
    if sym_stats.unresolved_strong != 0 {
        return Err(EngineLoadError::UnresolvedImports(
            sym_stats.unresolved_strong,
            sym_stats.unresolved,
        ));
    }

    if !img.loads.iter().any(|s| s.flags & PF_X != 0) {
        return Err(EngineLoadError::TextNotExecutable(
            "no PF_X segment in PT_LOAD table".to_string(),
        ));
    }
    let init_array = img.dyn_info.init_array;
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

    let engine = LoadedEngine {
        mapped,
        soname,
        dynsyms: img.dynsyms,
        init: img.dyn_info.init,
        init_array,
        constructors_run: 0,
    };
    match super::module_registry::ModuleRecord::for_image(
        &path,
        &bytes,
        &engine.dynsyms,
        engine.load_base(),
        engine.mapped.span() as u64,
    ) {
        Ok(rec) => super::module_registry::register_module(rec),

        Err(e) => {
            let _ = writeln!(
                log,
                "engine-load: WARNING: module-registry record for {} failed ({e}) — \
                 its PCs stay invisible to dl_iterate_phdr/dladdr",
                engine.soname
            );
        }
    }

    Ok(engine)
}

impl Drop for LoadedEngine {
    fn drop(&mut self) {
        let _ = super::module_registry::unregister_module(self.load_base());
    }
}

impl LoadedEngine {
    fn constructor_addresses(&self) -> Result<Vec<u64>, EngineLoadError> {
        constructor_addresses(&self.mapped, self.init, self.init_array)
            .map_err(|e| EngineLoadError::ReadInitArray(e.to_string()))
    }

    fn run_constructors(&mut self, constructors: &[u64], log: &mut impl Write) -> usize {
        if constructors.is_empty() {
            return 0;
        }
        let count = constructors.len();
        let _ = writeln!(log, "engine-load: running {count} constructors…");
        let _ = log.flush();

        let arg0 = b"libroblox\0";
        let mut argv: [*mut c_char; 2] = [arg0.as_ptr() as *mut c_char, std::ptr::null_mut()];
        let mut envp: [*mut c_char; 1] = [std::ptr::null_mut()];
        let argc: c_int = 1;

        let mut completed = 0usize;
        for &addr in constructors {
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
        completed
    }
}

fn call_jni_onload(
    engine: &LoadedEngine,
    addr: u64,
    java_vm: *mut JavaVM,
    log: &mut impl Write,
) -> jint {
    let _ = writeln!(
        log,
        "engine-load: calling JNI_OnLoad @ base+{:#x} (abs {:#x}) with the ART JavaVM…",
        addr.wrapping_sub(engine.load_base()),
        addr
    );
    let _ = log.flush();

    let onload: extern "C" fn(*mut JavaVM, *mut c_void) -> jint =
        unsafe { std::mem::transmute::<u64, _>(addr) };
    let version = onload(java_vm, std::ptr::null_mut());

    let _ = writeln!(
        log,
        "engine-load: JNI_OnLoad returned {version:#x} ({})",
        describe_jni_version(version)
    );
    let _ = log.flush();
    version
}

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

#[must_use]
pub struct PreloadedLib {
    pub soname: String,

    pub constructors_run: usize,

    pub jni_onload_version: Option<jint>,

    _engine: ProcessLifetimeEngine,
}

pub fn load_app_native_lib(
    lib_dir: &Path,
    filename: &str,
    java_vm: *mut JavaVM,
    log: &mut impl Write,
) -> Result<Option<PreloadedLib>, EngineLoadError> {
    static EARLY_FAULT_TAP: std::sync::Once = std::sync::Once::new();
    EARLY_FAULT_TAP.call_once(|| {
        if let Err(e) = super::native_provider::install_early_fault_tap(libc::SIGSEGV) {
            let _ = writeln!(
                log,
                "engine-load: early-fault tap install failed ({e}) — continuing without the diagnostic"
            );
        }
    });
    link_and_initialize(lib_dir, filename, java_vm, log)
}

fn link_and_initialize(
    lib_dir: &Path,
    filename: &str,
    java_vm: *mut JavaVM,
    log: &mut impl Write,
) -> Result<Option<PreloadedLib>, EngineLoadError> {
    if soname_is_loaded(filename) {
        let _ = writeln!(
            log,
            "engine-load: {filename} already loaded (deduped) — skipping"
        );
        return Ok(None);
    }

    let mut engine = map_resolve_app_lib(lib_dir, filename, log)?;
    let constructors = engine.constructor_addresses()?;
    let jni_onload = engine.jni_onload_addr();
    let soname = engine.soname.clone();

    if !register_soname(&soname) {
        let _ = writeln!(
            log,
            "engine-load: {soname} already loaded (deduped by soname) — skipping"
        );
        return Ok(None);
    }

    if soname != filename {
        let _ = register_soname(filename);
    }

    if filename == LIBROBLOX_FILENAME {
        super::native_provider::publish_engine_text_range(
            engine.load_base(),
            engine.mapped.span() as u64,
        );
    }

    let constructors_run = engine.run_constructors(&constructors, log);

    let jni_onload_version = if let Some(addr) = jni_onload {
        Some(call_jni_onload(&engine, addr, java_vm, log))
    } else {
        let _ = writeln!(
            log,
            "engine-load: {soname} exports no JNI_OnLoad (lazy-native lib — ART binds Java_* on demand)"
        );
        None
    };

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

    use super::super::link::tests::{
        build_so, build_so_with_constructors, build_so_with_unmapped_init_array, temp_dir, write_so,
    };
    use super::super::module_registry::walk_support::collect_registered;

    fn registered_module_paths() -> Vec<PathBuf> {
        collect_registered()
            .into_iter()
            .map(|module| PathBuf::from(module.name))
            .collect()
    }

    #[test]
    fn app_lib_is_mapped_from_the_lib_dir_without_temp_staging() {
        let filename = "libengine-test-stagecheck.so";
        let lib_dir = temp_dir("engine-stagecheck");
        write_so(&lib_dir, filename, &build_so(filename, &[], None, None));
        let sentinel = lib_dir.join("sentinel");
        std::fs::write(&sentinel, b"sentinel").expect("write sentinel");
        let predictable_stage =
            std::env::temp_dir().join(format!("eclipse-engine-{}", std::process::id()));
        std::fs::create_dir_all(&predictable_stage).expect("plant stage dir");
        std::os::unix::fs::symlink(&sentinel, predictable_stage.join(filename))
            .expect("plant stage symlink");

        let engine = map_resolve_app_lib(&lib_dir, filename, &mut std::io::sink())
            .expect("map the lib from the lib dir");

        assert_eq!(
            std::fs::read(&sentinel).expect("read sentinel"),
            b"sentinel"
        );
        let registered = collect_registered()
            .into_iter()
            .find(|module| module.addr == engine.load_base())
            .expect("the mapped lib is registered for dl_iterate_phdr/dladdr");
        assert_eq!(PathBuf::from(registered.name), lib_dir.join(filename));

        drop(engine);
        std::fs::remove_dir_all(&predictable_stage).ok();
        std::fs::remove_dir_all(&lib_dir).ok();
    }

    #[test]
    fn app_lib_dependencies_in_the_lib_dir_are_not_mapped() {
        let root = "libengine-test-root.so";
        let dep = "libengine-test-dep.so";
        let lib_dir = temp_dir("engine-deps");
        write_so(&lib_dir, root, &build_so(root, &[dep], None, None));
        write_so(&lib_dir, dep, &build_so(dep, &[], Some("dep_export"), None));

        let engine =
            map_resolve_app_lib(&lib_dir, root, &mut std::io::sink()).expect("map the root lib");

        let registered = registered_module_paths();
        assert!(registered.contains(&lib_dir.join(root)));
        assert!(!registered.contains(&lib_dir.join(dep)));

        drop(engine);
        assert!(!registered_module_paths().contains(&lib_dir.join(root)));
        std::fs::remove_dir_all(&lib_dir).ok();
    }

    static CONSTRUCTOR_ORDER: Mutex<Vec<&'static str>> = Mutex::new(Vec::new());

    extern "C" fn record_dt_init() {
        CONSTRUCTOR_ORDER.lock().unwrap().push("DT_INIT");
    }

    extern "C" fn record_first(_: c_int, _: *mut *mut c_char, _: *mut *mut c_char) {
        CONSTRUCTOR_ORDER.lock().unwrap().push("first");
    }

    extern "C" fn record_second(_: c_int, _: *mut *mut c_char, _: *mut *mut c_char) {
        CONSTRUCTOR_ORDER.lock().unwrap().push("second");
    }

    #[test]
    fn constructors_run_dt_init_first_and_skip_null_and_minus_one_slots() {
        let filename = "libengine-test-ctor-order.so";
        let lib_dir = temp_dir("engine-ctor-order");
        let so = build_so_with_constructors(
            filename,
            Some(record_dt_init as *const () as u64),
            &[
                0,
                u64::MAX,
                record_first as *const () as u64,
                record_second as *const () as u64,
            ],
        );
        write_so(&lib_dir, filename, &so);

        let lib = link_and_initialize(
            &lib_dir,
            filename,
            std::ptr::null_mut(),
            &mut std::io::sink(),
        )
        .expect("load the constructor fixture")
        .expect("first load is not deduped");

        assert_eq!(lib.constructors_run, 3);
        assert_eq!(
            *CONSTRUCTOR_ORDER.lock().unwrap(),
            ["DT_INIT", "first", "second"]
        );
        std::fs::remove_dir_all(&lib_dir).ok();
    }

    #[test]
    fn failed_constructor_lookup_leaves_the_soname_unregistered() {
        let filename = "libengine-test-unmapped-init-array.so";
        let lib_dir = temp_dir("engine-unmapped-init-array");
        write_so(
            &lib_dir,
            filename,
            &build_so_with_unmapped_init_array(filename),
        );

        let result = link_and_initialize(
            &lib_dir,
            filename,
            std::ptr::null_mut(),
            &mut std::io::sink(),
        );

        assert!(matches!(result, Err(EngineLoadError::ReadInitArray(_))));
        assert!(!is_preloaded(filename));
        std::fs::remove_dir_all(&lib_dir).ok();
    }

    #[test]
    fn jni_onload_symbol_name_is_the_jni_export() {
        assert_eq!(JNI_ONLOAD_SYMBOL, "JNI_OnLoad");
    }

    #[test]
    fn initialized_native_images_cannot_be_unmapped_by_scope_teardown() {
        assert!(!std::mem::needs_drop::<ProcessLifetimeEngine>());
    }

    #[test]
    fn describe_jni_version_labels_the_common_constants() {
        assert_eq!(describe_jni_version(JNI_VERSION_1_6), "JNI_VERSION_1_6");
        assert_eq!(describe_jni_version(JNI_VERSION_1_8), "JNI_VERSION_1_8");

        assert_eq!(
            describe_jni_version(-1),
            "error (negative; JNI_OnLoad failed)"
        );

        assert_eq!(
            describe_jni_version(0x7fff_0000),
            "unrecognized JNI version"
        );
    }

    #[test]
    fn jni_version_1_6_is_the_art_default() {
        assert_eq!(JNI_VERSION_1_6, 0x0001_0006);
    }

    #[test]
    fn unresolved_imports_error_names_the_symbols() {
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
        let a = "libengine-test-dedup-A.so";
        let b = "libengine-test-dedup-B.so";
        assert!(!soname_is_loaded(a), "fresh soname must start unloaded");
        assert!(register_soname(a), "first insert is newly inserted (true)");
        assert!(soname_is_loaded(a), "after insert the soname reads loaded");
        assert!(
            !register_soname(a),
            "second insert of the same soname is a dedup (false)"
        );

        assert!(!soname_is_loaded(b));
        assert!(register_soname(b));
        assert!(!register_soname(b));

        assert!(soname_is_loaded(a));
    }

    #[test]
    fn is_preloaded_reflects_registration() {
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
        fn classify(constructors_run: usize, jni_onload_version: Option<jint>) -> &'static str {
            match (constructors_run, jni_onload_version) {
                (0, None) => "lazy-native",
                (_, Some(_)) => "engine-class",
                (_, None) => "ctors-only",
            }
        }
        assert_eq!(classify(0, None), "lazy-native");
        assert_eq!(classify(3427, Some(JNI_VERSION_1_6)), "engine-class");
        assert_eq!(classify(2, None), "ctors-only");
    }
}
