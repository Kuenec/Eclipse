use std::collections::BTreeMap;

use crate::loader::elf::DynSym;
use crate::loader::reloc::{self, Rela};
use crate::loader::resolve::{ResolvedSym, Scope};

#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub(crate) enum ImportCategory {
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
    pub(crate) fn label(self) -> &'static str {
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
}

pub(crate) fn classify_import(name: &str) -> ImportCategory {
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
pub(crate) struct ImportReport {
    pub(crate) total: usize,

    pub(crate) by_category: BTreeMap<&'static str, Vec<String>>,

    pub(crate) host_resolved: Vec<String>,

    pub(crate) host_unresolved: Vec<String>,

    pub(crate) category_counts: BTreeMap<&'static str, (usize, usize)>,
}

impl ImportReport {
    pub(crate) fn resolved_count(&self) -> usize {
        self.host_resolved.len()
    }

    pub(crate) fn unresolved_count(&self) -> usize {
        self.host_unresolved.len()
    }
}

pub(crate) fn categorize_imports(
    relas: &[Rela],
    dynsyms: &[DynSym],
    scope: &Scope,
) -> ImportReport {
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

mod tests {
    use super::*;
    use crate::loader::bionic_env::BionicEnv;

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

        let report = categorize_imports(&relas, &syms, &Scope::new());

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
}
