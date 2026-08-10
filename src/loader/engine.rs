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
use super::elf::{DynSym, LoadSegment, PF_W, PF_X};
use super::link::Linker;
use super::map::host_page_size;
use super::resolve::{LoadedObjectProvider, Scope, SymbolProvider};

use super::init_run::{init_array_count, init_array_entry_offset};

const LIB_X86_64_DIR: &str = "lib/x86_64";

const LIBROBLOX_FILENAME: &str = "libroblox.so";

const FRAMERATE_CAP_ENGINE_FEATURE: &str = "GameBasicSettingsFramerateCap";

const FORCE_FMOD_OPENSL_DEBUG_FLAG: &str = "DebugFmodUseAndroidOpenSl";

const FMOD_AAUDIO_FALLBACK_FLAG: &str = "FmodFallbackAaudioToOpensl";

const FAST_BOOL_NAMESPACE: u32 = 1;
const DEBUG_FAST_BOOL_NAMESPACE: u32 = 2;

#[derive(Clone, Copy)]
struct HostBoolOverride {
    name: &'static str,
    namespace: u32,
}

const HOST_BOOL_OVERRIDES: [HostBoolOverride; 3] = [
    HostBoolOverride {
        name: FRAMERATE_CAP_ENGINE_FEATURE,
        namespace: FAST_BOOL_NAMESPACE,
    },
    HostBoolOverride {
        name: FORCE_FMOD_OPENSL_DEBUG_FLAG,
        namespace: DEBUG_FAST_BOOL_NAMESPACE,
    },
    HostBoolOverride {
        name: FMOD_AAUDIO_FALLBACK_FLAG,
        namespace: FAST_BOOL_NAMESPACE,
    },
];

static HOST_BOOL_OVERRIDE_ADDRESSES: Mutex<Vec<(&'static str, usize)>> = Mutex::new(Vec::new());

fn locate_registered_bool_vaddr(
    bytes: &[u8],
    loads: &[LoadSegment],
    name: &str,
    namespace: u32,
) -> Result<u64, String> {
    const LEA_RSI_RIP: &[u8; 3] = b"\x48\x8d\x35";
    const LEA_RDX_RIP: &[u8; 3] = b"\x48\x8d\x15";
    const MOV_ECX_IMM32: u8 = 0xb9;
    const BOOL_VALUE_KIND: &[u8; 6] = b"\x41\xb8\x04\x00\x00\x00";
    const PATTERN_LEN: usize = 7 + 7 + 5 + BOOL_VALUE_KIND.len();

    fn file_vaddr(loads: &[LoadSegment], file_offset: usize, len: usize) -> Option<u64> {
        let file_offset = u64::try_from(file_offset).ok()?;
        let len = u64::try_from(len).ok()?;
        let file_end = file_offset.checked_add(len)?;
        loads.iter().find_map(|segment| {
            let segment_end = segment.file_offset.checked_add(segment.file_size)?;
            (file_offset >= segment.file_offset && file_end <= segment_end)
                .then(|| segment.vaddr + (file_offset - segment.file_offset))
        })
    }

    fn rip_target(instruction_vaddr: u64, displacement: &[u8]) -> Option<u64> {
        let displacement = i32::from_le_bytes(displacement.try_into().ok()?) as i64;
        let next = instruction_vaddr.checked_add(7)?;
        if displacement >= 0 {
            next.checked_add(displacement as u64)
        } else {
            next.checked_sub(displacement.unsigned_abs())
        }
    }

    let name_bytes = name.as_bytes();
    let mut name_vaddrs = Vec::new();
    for (offset, window) in bytes.windows(name_bytes.len() + 1).enumerate() {
        if &window[..name_bytes.len()] == name_bytes && window[name_bytes.len()] == 0 {
            if let Some(vaddr) = file_vaddr(loads, offset, name_bytes.len() + 1) {
                name_vaddrs.push(vaddr);
            }
        }
    }
    name_vaddrs.sort_unstable();
    name_vaddrs.dedup();
    if name_vaddrs.is_empty() {
        return Err(format!(
            "registered boolean name {name:?} is absent from the ELF"
        ));
    }

    let mut candidates = Vec::new();
    for segment in loads.iter().filter(|segment| segment.flags & PF_X != 0) {
        let Ok(file_start) = usize::try_from(segment.file_offset) else {
            continue;
        };
        let Some(file_end_u64) = segment.file_offset.checked_add(segment.file_size) else {
            continue;
        };
        let Ok(file_end) = usize::try_from(file_end_u64) else {
            continue;
        };
        let Some(code) = bytes.get(file_start..file_end) else {
            continue;
        };
        for local in 0..=code.len().saturating_sub(PATTERN_LEN) {
            let thunk = &code[local..local + PATTERN_LEN];
            if &thunk[..3] != LEA_RSI_RIP
                || &thunk[7..10] != LEA_RDX_RIP
                || thunk[14] != MOV_ECX_IMM32
                || u32::from_le_bytes(thunk[15..19].try_into().expect("four-byte namespace"))
                    != namespace
                || &thunk[19..] != BOOL_VALUE_KIND
            {
                continue;
            }
            let Some(instruction_vaddr) = segment.vaddr.checked_add(local as u64) else {
                continue;
            };
            let Some(name_vaddr) = rip_target(instruction_vaddr, &thunk[3..7]) else {
                continue;
            };
            if !name_vaddrs.contains(&name_vaddr) {
                continue;
            }
            let Some(object_vaddr) = rip_target(instruction_vaddr + 7, &thunk[10..14]) else {
                continue;
            };
            let writable = loads.iter().any(|candidate| {
                candidate.flags & PF_W != 0
                    && object_vaddr >= candidate.vaddr
                    && object_vaddr < candidate.vaddr.saturating_add(candidate.mem_size)
            });
            if writable {
                candidates.push(object_vaddr);
            }
        }
    }
    candidates.sort_unstable();
    candidates.dedup();
    match candidates.as_slice() {
        [vaddr] => Ok(*vaddr),
        [] => Err(format!(
            "registered boolean thunk for {name:?} (namespace {namespace}) was not found"
        )),
        _ => Err(format!(
            "registered boolean thunk for {name:?} was ambiguous: {candidates:x?}"
        )),
    }
}

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
    set: super::link::LoadedImageSet,

    base: u64,

    dynsyms: Vec<DynSym>,

    init_array: Option<(u64, u64)>,

    constructors_run: usize,
}

impl LoadedEngine {
    #[must_use]
    pub fn load_base(&self) -> u64 {
        self.base
    }

    #[must_use]
    pub fn constructors_run(&self) -> usize {
        self.constructors_run
    }

    #[must_use]
    pub fn jni_onload_addr(&self) -> Option<u64> {
        let provider = LoadedObjectProvider::new(self.base, &self.dynsyms);
        provider
            .resolve(JNI_ONLOAD_SYMBOL)
            .map(|resolved| resolved.addr)
    }

    #[must_use]
    pub fn resolve_export(&self, name: &str) -> Option<u64> {
        LoadedObjectProvider::new(self.base, &self.dynsyms)
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
            .map(|s| (s.name.clone(), self.base.wrapping_add(s.value)))
            .collect()
    }

    fn enable_registered_bool(
        &mut self,
        name: &str,
        namespace: u32,
    ) -> Result<(bool, usize), String> {
        let object = self
            .set
            .objects
            .first_mut()
            .ok_or_else(|| "loaded image set has no root object".to_string())?;
        let vaddr = {
            let image = object.image().map_err(|error| error.to_string())?;
            locate_registered_bool_vaddr(&object.bytes, &image.loads, name, namespace)?
        };
        let offset = usize::try_from(vaddr)
            .map_err(|_| format!("registered boolean {name:?} vaddr {vaddr:#x} exceeds usize"))?;
        if offset >= object.mapped.span() {
            return Err(format!(
                "registered boolean {name:?} vaddr {vaddr:#x} lies outside mapped span {:#x}",
                object.mapped.span()
            ));
        }
        let address = object
            .mapped
            .load_base()
            .checked_add(vaddr)
            .ok_or_else(|| format!("registered boolean {name:?} address overflow"))?;

        let slot = address as *mut u8;
        let previous = unsafe { slot.read() };
        if previous > 1 {
            return Err(format!(
                "registered boolean {name:?} backing byte at {vaddr:#x} was {previous}, expected bool"
            ));
        }

        unsafe { slot.write(1) };
        Ok((previous != 0, address as usize))
    }
}

pub fn reapply_host_bool_overrides() -> usize {
    let addresses = HOST_BOOL_OVERRIDE_ADDRESSES
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner);
    for &(_name, address) in addresses.iter() {
        unsafe { std::ptr::write_volatile(address as *mut u8, 1) };
    }
    addresses.len()
}

#[derive(Debug)]
pub enum EngineLoadError {
    Apk(String, String),

    Stage(String),

    Link(String),

    UnresolvedImports(usize, Vec<String>),

    TextNotExecutable(String),

    NoInitArray,

    NullConstructor(usize),

    ReadInitArray(String),

    NoJniOnLoad,
}

impl std::fmt::Display for EngineLoadError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::Apk(entry, e) => write!(f, "read {entry} from APK: {e}"),
            Self::Stage(e) => write!(f, "stage libroblox.so: {e}"),
            Self::Link(e) => write!(f, "map/relocate/resolve libroblox.so: {e}"),
            Self::UnresolvedImports(n, names) => {
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

pub fn load_libroblox(
    apk_path: &Path,
    log: &mut impl Write,
) -> Result<LoadedEngine, EngineLoadError> {
    let engine = map_resolve_app_lib(apk_path, LIBROBLOX_FILENAME, None, log)?;

    if engine.init_array.is_none() {
        return Err(EngineLoadError::NoInitArray);
    }
    Ok(engine)
}

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
        return Err(EngineLoadError::UnresolvedImports(
            sym_stats.unresolved_strong,
            sym_stats.unresolved,
        ));
    }

    let init_array = {
        let img = set.objects[0]
            .image()
            .map_err(|e| EngineLoadError::Link(e.to_string()))?;

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
        for obj in &self.set.objects {
            let _ = super::module_registry::unregister_module(obj.load_base());
        }
    }
}

impl LoadedEngine {
    pub fn run_init_array(&mut self, log: &mut impl Write) -> Result<usize, EngineLoadError> {
        let (init_array_vaddr, init_arraysz) =
            self.init_array.ok_or(EngineLoadError::NoInitArray)?;
        let count = init_array_count(init_arraysz);
        let obj = &self.set.objects[0];

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

        let arg0 = b"libroblox\0";
        let mut argv: [*mut c_char; 2] = [arg0.as_ptr() as *mut c_char, std::ptr::null_mut()];
        let mut envp: [*mut c_char; 1] = [std::ptr::null_mut()];
        let argc: c_int = 1;

        let mut completed = 0usize;
        for &addr in &entries {
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
    apk_path: &Path,
    filename: &str,
    java_vm: *mut JavaVM,
    search_dir: &Path,
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

    if soname_is_loaded(filename) {
        let _ = writeln!(
            log,
            "engine-load: {filename} already loaded (deduped) — skipping"
        );
        return Ok(None);
    }

    let mut engine = map_resolve_app_lib(apk_path, filename, Some(search_dir), log)?;
    let soname = engine.set.objects[0].soname.clone();

    if filename == LIBROBLOX_FILENAME {
        super::native_provider::publish_engine_text_range(
            engine.base,
            engine.set.objects[0].mapped.span() as u64,
        );
    }

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

    let constructors_run = if engine.init_array.is_some() {
        engine.run_init_array(log)?
    } else {
        0
    };

    if filename == LIBROBLOX_FILENAME {
        let mut located = Vec::with_capacity(HOST_BOOL_OVERRIDES.len());
        for override_ in HOST_BOOL_OVERRIDES {
            match engine.enable_registered_bool(override_.name, override_.namespace) {
                Ok((was_enabled, address)) => {
                    located.push((override_.name, address));
                    let state = if was_enabled {
                        "already enabled"
                    } else {
                        "enabled"
                    };
                    let purpose = match override_.name {
                        FRAMERATE_CAP_ENGINE_FEATURE => "native Maximum Frame Rate setting",
                        FORCE_FMOD_OPENSL_DEBUG_FLAG => "force FMOD onto Eclipse's OpenSL output",
                        FMOD_AAUDIO_FALLBACK_FLAG => "fallback from unavailable AAudio to OpenSL",
                        _ => "host runtime compatibility",
                    };
                    let _ = writeln!(log, "engine-load: {} {state} ({purpose}) ✓", override_.name);
                }
                Err(error) => {
                    let _ = writeln!(
                        log,
                        "engine-load: WARNING: could not enable {} ({error})",
                        override_.name
                    );
                }
            }
        }
        *HOST_BOOL_OVERRIDE_ADDRESSES
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner) = located;
        let _ = log.flush();
    }

    let jni_onload_version = if engine.jni_onload_addr().is_some() {
        Some(call_jni_onload(&engine, java_vm, log)?)
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

    fn registered_bool_locator_fixture(
        namespace: u32,
        value_kind: u32,
    ) -> (Vec<u8>, Vec<LoadSegment>, u64) {
        let mut bytes = vec![0_u8; 0x300];
        let feature = FRAMERATE_CAP_ENGINE_FEATURE.as_bytes();
        bytes[0x40..0x40 + feature.len()].copy_from_slice(feature);
        bytes[0x40 + feature.len()] = 0;

        let loads = vec![
            LoadSegment {
                file_offset: 0,
                vaddr: 0x1000,
                file_size: 0x100,
                mem_size: 0x100,
                flags: super::super::elf::PF_R,
                align: 0x1000,
            },
            LoadSegment {
                file_offset: 0x100,
                vaddr: 0x4000,
                file_size: 0x100,
                mem_size: 0x100,
                flags: super::super::elf::PF_R | PF_X,
                align: 0x1000,
            },
            LoadSegment {
                file_offset: 0x200,
                vaddr: 0x9000,
                file_size: 0x80,
                mem_size: 0x100,
                flags: super::super::elf::PF_R | PF_W,
                align: 0x1000,
            },
        ];
        let instruction_vaddr = 0x4000_u64 + 0x20;
        let name_vaddr = 0x1000_u64 + 0x40;
        let bool_vaddr = 0x9050_u64;
        let name_disp = i32::try_from(name_vaddr as i64 - (instruction_vaddr + 7) as i64)
            .expect("name displacement");
        let bool_disp = i32::try_from(bool_vaddr as i64 - (instruction_vaddr + 14) as i64)
            .expect("bool displacement");
        let code = &mut bytes[0x120..0x120 + 25];
        code[..3].copy_from_slice(b"\x48\x8d\x35");
        code[3..7].copy_from_slice(&name_disp.to_le_bytes());
        code[7..10].copy_from_slice(b"\x48\x8d\x15");
        code[10..14].copy_from_slice(&bool_disp.to_le_bytes());
        code[14] = 0xb9;
        code[15..19].copy_from_slice(&namespace.to_le_bytes());
        code[19..21].copy_from_slice(b"\x41\xb8");
        code[21..25].copy_from_slice(&value_kind.to_le_bytes());
        (bytes, loads, bool_vaddr)
    }

    #[test]
    fn registered_bool_locator_resolves_rip_relative_writable_bool() {
        let (bytes, loads, expected) = registered_bool_locator_fixture(FAST_BOOL_NAMESPACE, 4);
        assert_eq!(
            locate_registered_bool_vaddr(
                &bytes,
                &loads,
                FRAMERATE_CAP_ENGINE_FEATURE,
                FAST_BOOL_NAMESPACE,
            )
            .unwrap(),
            expected
        );
    }

    #[test]
    fn registered_bool_locator_rejects_wrong_namespace_or_non_bool_kind() {
        let (wrong_namespace, loads, _) =
            registered_bool_locator_fixture(DEBUG_FAST_BOOL_NAMESPACE, 4);
        let error = locate_registered_bool_vaddr(
            &wrong_namespace,
            &loads,
            FRAMERATE_CAP_ENGINE_FEATURE,
            FAST_BOOL_NAMESPACE,
        )
        .unwrap_err();
        assert!(error.contains("was not found"), "{error}");

        let (wrong_kind, loads, _) = registered_bool_locator_fixture(FAST_BOOL_NAMESPACE, 3);
        let error = locate_registered_bool_vaddr(
            &wrong_kind,
            &loads,
            FRAMERATE_CAP_ENGINE_FEATURE,
            FAST_BOOL_NAMESPACE,
        )
        .unwrap_err();
        assert!(error.contains("was not found"), "{error}");
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
    fn libroblox_entry_is_the_x86_64_engine_path() {
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
