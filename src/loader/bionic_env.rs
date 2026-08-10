use std::collections::BTreeMap;
use std::ffi::CString;

use super::elf::DynSym;
use super::native_provider::EclipseNativeProvider;
use super::reloc::{self, Rela};
use super::resolve::{HostDlsymProvider, ResolvedSym, Scope, SymbolProvider};

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

#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub enum ImportCategory {
    BionicLibc,

    Math,

    Pthread,

    Dl,

    CxaRuntime,

    Liblog,

    NdkAndroid,

    EglGles,

    MediaNdk,

    Audio,

    Uncategorized,
}

impl ImportCategory {
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

pub fn classify_import(name: &str) -> ImportCategory {
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

    if is_dl(name) {
        return ImportCategory::Dl;
    }
    if is_cxa_runtime(name) {
        return ImportCategory::CxaRuntime;
    }
    if name.starts_with("pthread_") {
        return ImportCategory::Pthread;
    }

    if is_math(name) {
        return ImportCategory::Math;
    }
    if is_bionic_libc(name) {
        return ImportCategory::BionicLibc;
    }
    ImportCategory::Uncategorized
}

fn is_egl_gles(name: &str) -> bool {
    if let Some(rest) = name.strip_prefix("gl") {
        if rest.chars().next().is_some_and(|c| c.is_ascii_uppercase()) {
            return true;
        }
    }
    name.starts_with("egl")
}

fn is_media_ndk(name: &str) -> bool {
    name.starts_with("AMedia") || name.starts_with("AMEDIA")
}

fn is_audio(name: &str) -> bool {
    name.starts_with("slCreateEngine")
        || name.starts_with("xaCreateEngine")
        || name.starts_with("slQueryNum")
        || name.starts_with("xaQueryNum")
        || name.starts_with("SL_IID_")
        || name.starts_with("XA_IID_")
}

fn is_liblog(name: &str) -> bool {
    name.starts_with("__android_log_")
        || name == "android_set_abort_message"
        || name.starts_with("__android_set_abort")
}

fn is_ndk_android(name: &str) -> bool {
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

fn is_dl(name: &str) -> bool {
    matches!(
        name,
        "dlopen" | "dlsym" | "dlclose" | "dlerror" | "dladdr" | "dlvsym" | "android_dlopen_ext"
    )
}

fn is_cxa_runtime(name: &str) -> bool {
    matches!(
        name,
        "__cxa_atexit" | "__cxa_finalize" | "__cxa_thread_atexit_impl" | "__cxa_atexit_impl"
    )
}

fn is_math(name: &str) -> bool {
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

fn is_bionic_libc(name: &str) -> bool {
    if name.starts_with("__") {
        return true;
    }

    name.chars()
        .next()
        .is_some_and(|c| c.is_ascii_alphabetic() || c == '_')
}

#[derive(Debug, Clone, Default)]
pub struct ImportReport {
    pub total: usize,

    pub by_category: BTreeMap<&'static str, Vec<String>>,

    pub host_resolved: Vec<String>,

    pub host_unresolved: Vec<String>,

    pub category_counts: BTreeMap<&'static str, (usize, usize)>,
}

impl ImportReport {
    pub fn resolved_count(&self) -> usize {
        self.host_resolved.len()
    }

    pub fn unresolved_count(&self) -> usize {
        self.host_unresolved.len()
    }
}

pub fn categorize_imports(relas: &[Rela], dynsyms: &[DynSym], scope: &Scope) -> ImportReport {
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
            continue;
        };

        if sym.shndx != SHN_UNDEF || sym.name.is_empty() {
            continue;
        }
        if seen.insert(sym.name.clone(), ()).is_some() {
            continue;
        }
        report.total += 1;

        let cat = classify_import(&sym.name).label();
        report
            .by_category
            .entry(cat)
            .or_default()
            .push(sym.name.clone());

        let resolved = scope
            .resolve(&sym.name)
            .is_some_and(|r: ResolvedSym| r.addr != 0);
        let counts = report.category_counts.entry(cat).or_insert((0, 0));
        if resolved {
            report.host_resolved.push(sym.name.clone());
            counts.0 += 1;
        } else {
            if sym.bind != STB_WEAK {
                report.host_unresolved.push(sym.name.clone());
            }
            counts.1 += 1;
        }
    }

    report.host_resolved.sort();
    report.host_unresolved.sort();
    for v in report.by_category.values_mut() {
        v.sort();
    }
    report
}

pub struct BionicEnv {
    scope: Scope,

    missing_gl: Vec<String>,

    host_libc_present: bool,

    eclipse_natives_present: bool,
}

impl BionicEnv {
    pub fn with_host_baseline(try_host_gl: bool, eclipse_natives: bool) -> Self {
        let mut scope = Scope::new();
        let mut missing_gl = Vec::new();

        if eclipse_natives {
            scope.push(Box::new(EclipseNativeProvider::with_bionic_natives()));
        }

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

        scope.push(Box::new(HostDlsymProvider));

        Self {
            scope,
            missing_gl,
            host_libc_present: true,
            eclipse_natives_present: eclipse_natives,
        }
    }

    pub fn empty() -> Self {
        Self {
            scope: Scope::new(),
            missing_gl: vec!["libEGL.so".to_string(), "libGLESv2.so".to_string()],
            host_libc_present: false,
            eclipse_natives_present: false,
        }
    }

    pub fn missing_gl(&self) -> &[String] {
        &self.missing_gl
    }

    pub fn host_libc_present(&self) -> bool {
        self.host_libc_present
    }

    pub fn eclipse_natives_present(&self) -> bool {
        self.eclipse_natives_present
    }

    pub fn scope(&self) -> &Scope {
        &self.scope
    }

    pub fn into_scope(self) -> Scope {
        self.scope
    }

    pub fn into_providers(self) -> Vec<Box<dyn SymbolProvider>> {
        self.scope.into_providers()
    }
}

pub struct DlopenLibProvider {
    handle: HostLibHandle,

    soname: String,
}

struct HostLibHandle(*mut libc::c_void);

unsafe impl Send for HostLibHandle {}

unsafe impl Sync for HostLibHandle {}

impl DlopenLibProvider {
    pub fn open(soname: &str) -> Option<Self> {
        let cname = CString::new(soname).ok()?;

        let handle = unsafe { libc::dlopen(cname.as_ptr(), libc::RTLD_NOW | libc::RTLD_LOCAL) };
        if handle.is_null() {
            return None;
        }
        Some(Self {
            handle: HostLibHandle(handle),
            soname: soname.to_string(),
        })
    }

    pub fn soname(&self) -> &str {
        &self.soname
    }
}

impl SymbolProvider for DlopenLibProvider {
    fn resolve(&self, name: &str) -> Option<ResolvedSym> {
        let cname = CString::new(name).ok()?;

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
            sym_type: 2,
            shndx: 0,
        }
    }
    fn def(name: &str) -> DynSym {
        DynSym {
            name: name.to_string(),
            value: 0x10,
            size: 0,
            bind: 1,
            sym_type: 2,
            shndx: 1,
        }
    }

    fn glob_dat(sym_index: u32) -> Rela {
        Rela {
            offset: 0x1000 + u64::from(sym_index) * 8,
            sym_index,
            r_type: reloc::R_X86_64_GLOB_DAT,
            addend: 0,
        }
    }

    #[test]
    fn classify_gl_and_egl() {
        assert_eq!(classify_import("glUseProgram"), ImportCategory::EglGles);
        assert_eq!(classify_import("glDrawElements"), ImportCategory::EglGles);
        assert_eq!(classify_import("eglGetError"), ImportCategory::EglGles);
        assert_eq!(classify_import("eglSwapBuffers"), ImportCategory::EglGles);

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
        assert!(!ImportCategory::NdkAndroid.host_baseline_possible());
        assert!(!ImportCategory::MediaNdk.host_baseline_possible());
        assert!(!ImportCategory::Audio.host_baseline_possible());
        assert!(!ImportCategory::Liblog.host_baseline_possible());

        assert!(ImportCategory::BionicLibc.host_baseline_possible());
        assert!(ImportCategory::EglGles.host_baseline_possible());
        assert!(ImportCategory::Pthread.host_baseline_possible());
    }

    #[test]
    fn categorize_splits_imports_and_skips_definitions() {
        let syms = vec![
            undef("strlen", 1),
            undef("AAssetManager_open", 1),
            undef("glUseProgram", 1),
            undef("__gmon_start__", 2),
            def("an_internal_definition"),
        ];

        let relas = vec![
            glob_dat(0),
            glob_dat(1),
            glob_dat(2),
            glob_dat(3),
            glob_dat(0),
            glob_dat(4),
        ];

        let env = BionicEnv::empty();
        let report = categorize_imports(&relas, &syms, env.scope());

        assert_eq!(report.total, 4);

        assert!(!report
            .by_category
            .values()
            .flatten()
            .any(|n| n == "an_internal_definition"));

        assert_eq!(
            report.by_category.get("bionic-libc").map(Vec::as_slice),
            Some(["__gmon_start__".to_string(), "strlen".to_string()].as_slice())
        );
        assert_eq!(
            report.by_category.get("ndk-android").map(Vec::as_slice),
            Some(["AAssetManager_open".to_string()].as_slice())
        );

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
        let syms = vec![
            undef("memcpy", 1),
            undef("malloc", 1),
            undef("AAssetManager_open", 1),
        ];
        let relas = vec![glob_dat(0), glob_dat(1), glob_dat(2)];

        let env = BionicEnv::with_host_baseline(false, false);
        let report = categorize_imports(&relas, &syms, env.scope());
        assert_eq!(report.total, 3);

        assert!(report.host_resolved.contains(&"memcpy".to_string()));
        assert!(report.host_resolved.contains(&"malloc".to_string()));
        assert!(report
            .host_unresolved
            .contains(&"AAssetManager_open".to_string()));
        assert_eq!(report.resolved_count(), 2);
        assert_eq!(report.unresolved_count(), 1);
    }

    #[test]
    fn bionic_env_host_baseline_has_libc_tier() {
        let env = BionicEnv::with_host_baseline(false, false);
        assert!(env.host_libc_present());
        assert!(!env.eclipse_natives_present());

        let got = env.scope().resolve("memcpy");
        assert!(got.is_some_and(|r| r.addr != 0));

        assert_eq!(env.missing_gl().len(), 2);
    }

    #[test]
    fn bionic_env_eclipse_natives_win_over_host() {
        let env = BionicEnv::with_host_baseline(false, true);
        assert!(env.eclipse_natives_present());

        assert!(
            env.scope().resolve("__errno").is_some_and(|r| r.addr != 0),
            "the Eclipse tier must resolve __errno (glibc lacks this exact name)"
        );

        assert!(env
            .scope()
            .resolve("__strlen_chk")
            .is_some_and(|r| r.addr != 0));

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
        let Some(p) = DlopenLibProvider::open("libc.so.6") else {
            eprintln!("dlopen_provider_resolves_from_libc: no libc.so.6; skipping");
            return;
        };
        assert_eq!(p.soname(), "libc.so.6");
        assert!(p.resolve("memcpy").is_some_and(|r| r.addr != 0));
        assert_eq!(p.resolve("__eclipse_no_such_symbol_zzz__"), None);

        assert_eq!(p.resolve("bad\0name"), None);
    }

    #[test]
    fn dlopen_provider_absent_lib_is_none() {
        assert!(DlopenLibProvider::open("libeclipse_definitely_no_such_lib_4f2a.so").is_none());
    }
}
