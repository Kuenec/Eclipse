use std::ffi::{c_char, c_void, CString, OsStr, OsString};
use std::fmt;
use std::os::unix::ffi::OsStrExt;
use std::path::{Path, PathBuf};
use std::process::Command;

use directories::ProjectDirs;

use crate::apk::Manifest;
use crate::config::{Config, TouchMode};

const DEFAULT_SDK_INT: u32 = 33;

pub(crate) const HEAP_MIB: u32 = 768;

const LIBART_DEFAULT: &str = "/usr/lib/art/libart.so";

const BOOT_IMAGE_DEFAULT: &str = "/usr/lib/java/dex/art/oat/boot.art";

const ART_DATA_DIR_DEFAULT: &str = "/usr/lib/java/dex/art";

const ART_OVERLAY_MARKER: &str = ".eclipse-art-overlay-v1";
const ART_OVERLAY_MARKER_CONTENT: &str = "eclipse-art-overlay-v1\n";

const ART_BOOT_JARS: [&str; 10] = [
    "core-oj-hostdex.jar",
    "apachehttp-hostdex.jar",
    "apache-xml-hostdex.jar",
    "bouncycastle-hostdex.jar",
    "core-junit-hostdex.jar",
    "core-libart-hostdex.jar",
    "hamcrest-hostdex.jar",
    "junit-runner-hostdex.jar",
    "okhttp-hostdex.jar",
    "wolfssljni-hostdex.jar",
];

const X86_FEATURE_TOKENS: [&str; 6] = ["ssse3", "sse4.1", "sse4.2", "avx", "avx2", "popcnt"];

#[cfg(target_arch = "x86_64")]
#[must_use]
pub fn instruction_set_features() -> String {
    let detected = |token: &str| -> bool {
        match token {
            "ssse3" => is_x86_feature_detected!("ssse3"),
            "sse4.1" => is_x86_feature_detected!("sse4.1"),
            "sse4.2" => is_x86_feature_detected!("sse4.2"),
            "avx" => is_x86_feature_detected!("avx"),
            "avx2" => is_x86_feature_detected!("avx2"),
            "popcnt" => is_x86_feature_detected!("popcnt"),
            _ => false,
        }
    };
    format_feature_string(detected)
}

#[cfg(not(target_arch = "x86_64"))]
compile_error!(
    "Eclipse's runtime targets the Android x86-64 Roblox engine; \
     instruction_set_features() needs an x86_64 host (no x86 ISA to detect on this arch)"
);

fn format_feature_string(present: impl Fn(&str) -> bool) -> String {
    let mut out = String::with_capacity(64);
    for token in X86_FEATURE_TOKENS {
        if !out.is_empty() {
            out.push(',');
        }
        if !present(token) {
            out.push('-');
        }
        out.push_str(token);
    }
    out
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum GraphicsBackend {
    Vulkan,

    OpenGl,
}

impl GraphicsBackend {
    #[must_use]
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Vulkan => "Vulkan",
            Self::OpenGl => "OpenGL",
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct BootPlan {
    pub launcher_activity: String,

    pub sdk_int: u32,

    pub heap_mib: u32,

    pub disable_hspace_compact: bool,

    pub instruction_set_features: String,

    pub graphics_backend: GraphicsBackend,

    pub touch_mode: TouchMode,
}

impl BootPlan {
    #[must_use]
    pub fn new(manifest: &Manifest, config: &Config) -> Self {
        Self {
            launcher_activity: manifest.launcher_activity.clone(),
            sdk_int: manifest.target_sdk.unwrap_or(DEFAULT_SDK_INT),
            heap_mib: HEAP_MIB,
            disable_hspace_compact: true,
            instruction_set_features: instruction_set_features(),
            graphics_backend: if config.use_opengl {
                GraphicsBackend::OpenGl
            } else {
                GraphicsBackend::Vulkan
            },
            touch_mode: config.touch_mode,
        }
    }

    #[must_use]
    pub fn with_activity_override(mut self, activity: impl Into<String>) -> Self {
        self.launcher_activity = activity.into();
        self
    }

    #[must_use]
    pub fn vm_options(&self) -> Vec<String> {
        let mut opts = Vec::with_capacity(5);
        opts.push(format!("-Xmx{}m", self.heap_mib));
        opts.push(format!("-XX:HeapGrowthLimit={}m", self.heap_mib));
        if self.disable_hspace_compact {
            opts.push("-XX:DisableHSpaceCompactForOOM".to_owned());
        }

        opts.push(format!("-DBuild.VERSION.SDK_INT={}", self.sdk_int.min(28)));

        opts.push(format!("-Declipse.touch_mode={}", self.touch_mode.as_str()));
        opts
    }

    #[must_use]
    pub fn dex2oat_options(&self) -> Vec<String> {
        vec![format!(
            "--instruction-set-features={}",
            self.instruction_set_features
        )]
    }
}

pub fn find_libart() -> Result<PathBuf, RuntimeError> {
    let path = env_path("ECLIPSE_LIBART").unwrap_or_else(|| PathBuf::from(LIBART_DEFAULT));
    if path.exists() {
        Ok(path)
    } else {
        Err(RuntimeError::LibartNotFound(path))
    }
}

const LIBCORE_PRIMARY_JAR: &str = "core-oj-hostdex.jar";

#[derive(Debug, Clone, PartialEq, Eq)]
struct ArtBootPaths {
    image_location: PathBuf,

    boot_class_path: Option<OsString>,
}

fn art_dir_from_image(image_location: &Path) -> Option<PathBuf> {
    image_location
        .parent()
        .and_then(Path::parent)
        .map(Path::to_path_buf)
}

fn first_missing_art_jar(art_dir: &Path) -> Option<PathBuf> {
    ART_BOOT_JARS
        .iter()
        .map(|name| art_dir.join(name))
        .find(|path| !path.is_file())
}

fn boot_class_path_for(art_dir: &Path) -> Result<OsString, RuntimeError> {
    std::env::join_paths(ART_BOOT_JARS.iter().map(|name| art_dir.join(name)))
        .map_err(RuntimeError::BootClassPathJoin)
}

fn overlay_art_dir() -> Option<PathBuf> {
    env_path("ECLIPSE_ANDROID_FRAMEWORK_DIR")
        .or_else(patched_overlay_dir)
        .map(|dir| dir.join("art"))
}

fn overlay_is_ready(art_dir: &Path) -> Result<bool, RuntimeError> {
    let marker = art_dir.join(ART_OVERLAY_MARKER);
    match std::fs::read_to_string(&marker) {
        Ok(content) if content == ART_OVERLAY_MARKER_CONTENT => Ok(true),
        Ok(_) => Err(RuntimeError::ArtOverlayMarkerInvalid(marker)),
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => Ok(false),
        Err(error) => Err(RuntimeError::ArtOverlayMarkerRead(marker, error)),
    }
}

fn resolve_boot_image_location(
    explicit: Option<PathBuf>,
    overlay_art: Option<PathBuf>,
    overlay_ready: bool,
) -> PathBuf {
    if let Some(path) = explicit {
        path
    } else if let Some(art_dir) = overlay_art.filter(|_| overlay_ready) {
        art_dir.join("oat").join("boot.art")
    } else {
        PathBuf::from(BOOT_IMAGE_DEFAULT)
    }
}

fn find_art_boot_paths() -> Result<ArtBootPaths, RuntimeError> {
    let explicit = env_path("ECLIPSE_ART_BOOT_IMAGE");
    let overlay_art = overlay_art_dir();
    let overlay_ready = if explicit.is_none() {
        overlay_art
            .as_deref()
            .map(overlay_is_ready)
            .transpose()?
            .unwrap_or(false)
    } else {
        false
    };
    let image_location =
        resolve_boot_image_location(explicit.clone(), overlay_art.clone(), overlay_ready);
    let art_dir = art_dir_from_image(&image_location)
        .ok_or_else(|| RuntimeError::BootImageNotFound(image_location.clone()))?;

    if overlay_ready {
        if let Some(missing) = first_missing_art_jar(&art_dir) {
            return Err(RuntimeError::ArtOverlayIncomplete(missing));
        }
    } else if !art_dir.join(LIBCORE_PRIMARY_JAR).is_file() {
        return Err(RuntimeError::BootImageNotFound(image_location));
    }

    let self_contained =
        art_dir != Path::new(ART_DATA_DIR_DEFAULT) && first_missing_art_jar(&art_dir).is_none();
    let boot_class_path = self_contained
        .then(|| boot_class_path_for(&art_dir))
        .transpose()?;

    Ok(ArtBootPaths {
        image_location,
        boot_class_path,
    })
}

pub fn prepare_art_boot_environment() -> Result<(), RuntimeError> {
    let paths = find_art_boot_paths()?;
    if let Some(boot_class_path) = paths.boot_class_path {
        if std::env::var_os("BOOTCLASSPATH").as_ref() != Some(&boot_class_path) {
            unsafe { std::env::set_var("BOOTCLASSPATH", boot_class_path) };
        }
    }
    Ok(())
}

pub fn find_boot_image() -> Result<PathBuf, RuntimeError> {
    find_art_boot_paths().map(|paths| paths.image_location)
}

fn env_path(var: &str) -> Option<PathBuf> {
    match std::env::var_os(var) {
        Some(v) if !v.is_empty() => Some(PathBuf::from(v)),
        _ => None,
    }
}

fn vm_options_from_env(raw: Option<&std::ffi::OsStr>) -> Vec<String> {
    raw.map(|s| {
        s.to_string_lossy()
            .split(';')
            .map(str::trim)
            .filter(|s| !s.is_empty())
            .map(ToOwned::to_owned)
            .collect()
    })
    .unwrap_or_default()
}

const FRAMEWORK_DIR_DEFAULT: &str = "/usr/lib/java/dex/android_translation_layer";

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct FrameworkPaths {
    pub api_impl_jar: PathBuf,

    pub framework_res_apk: PathBuf,

    pub natives_dir: PathBuf,
}

fn patched_overlay_dir() -> Option<PathBuf> {
    ProjectDirs::from("", "", "eclipse").map(|d| d.cache_dir().join("framework-patched"))
}

fn resolve_framework_dir(
    env_override: Option<PathBuf>,
    overlay_dir: Option<PathBuf>,
    overlay_present: bool,
) -> PathBuf {
    if let Some(dir) = env_override {
        return dir;
    }
    if overlay_present {
        if let Some(dir) = overlay_dir {
            return dir;
        }
    }
    PathBuf::from(FRAMEWORK_DIR_DEFAULT)
}

fn framework_dir() -> PathBuf {
    let env_override = env_path("ECLIPSE_ANDROID_FRAMEWORK_DIR");
    let overlay = patched_overlay_dir();
    let overlay_present = overlay
        .as_ref()
        .is_some_and(|d| d.join("api-impl.jar").exists());
    let dir = resolve_framework_dir(env_override.clone(), overlay, overlay_present);
    if env_override.is_none() && !overlay_present {
        tracing::warn!(
            framework_dir = %dir.display(),
            "no patched framework overlay found; using the stock ATL framework, which lacks \
             Roblox-required android.* classes/fields (boot will fail in \
             RobloxApplication.onCreate). Run tools/framework-overlay/patch-framework.sh or set \
             ECLIPSE_ANDROID_FRAMEWORK_DIR."
        );
    }
    dir
}

pub fn find_framework() -> Result<FrameworkPaths, RuntimeError> {
    let dir = framework_dir();
    let api_impl_jar = dir.join("api-impl.jar");
    if !api_impl_jar.exists() {
        return Err(RuntimeError::FrameworkNotFound(api_impl_jar));
    }
    Ok(FrameworkPaths {
        framework_res_apk: dir.join("framework-res.apk"),
        natives_dir: dir.join("natives"),
        api_impl_jar,
    })
}

pub fn native_lib_cache_dir() -> Result<PathBuf, RuntimeError> {
    if let Some(dir) = env_path("ECLIPSE_NATIVE_LIB_DIR") {
        return Ok(dir);
    }
    let dirs = ProjectDirs::from("", "", "eclipse").ok_or(RuntimeError::NoCacheDir)?;
    Ok(dirs.cache_dir().join("native-libs"))
}

fn class_path_option(fw: &FrameworkPaths, apk: &Path) -> String {
    format!(
        "-Djava.class.path={}:{}:{}",
        fw.api_impl_jar.display(),
        apk.display(),
        fw.framework_res_apk.display()
    )
}

fn library_path_option(fw: &FrameworkPaths, app_lib_dir: Option<&Path>) -> String {
    match app_lib_dir {
        Some(dir) => format!(
            "-Djava.library.path={}:{}",
            fw.natives_dir.display(),
            dir.display()
        ),
        None => format!("-Djava.library.path={}", fw.natives_dir.display()),
    }
}

type JniCreateJavaVm = unsafe extern "system" fn(
    *mut *mut jni_sys::JavaVM,
    *mut *mut c_void,
    *mut c_void,
) -> jni_sys::jint;

type DlParseLibraryPath = unsafe extern "C" fn(*const c_char, *const c_char);

const BIONIC_LDPATH_DELIM: &str = ":";

fn bionic_library_path(fw: &FrameworkPaths, app_lib_dir: Option<&Path>) -> String {
    match app_lib_dir {
        Some(dir) => format!(
            "{}{BIONIC_LDPATH_DELIM}{}",
            fw.natives_dir.display(),
            dir.display()
        ),
        None => fw.natives_dir.display().to_string(),
    }
}

pub fn whitelist_bionic_library_path(
    fw: &FrameworkPaths,
    app_lib_dir: Option<&Path>,
) -> Result<(), RuntimeError> {
    let path = bionic_library_path(fw, app_lib_dir);

    let path_c = make_cstring(path)?;
    let delim_c = make_cstring(BIONIC_LDPATH_DELIM.to_owned())?;

    let global = unsafe { libloading::os::unix::Library::open(None::<&Path>, LIBART_DLOPEN_FLAGS) }
        .map_err(RuntimeError::OpenGlobalScope)?;

    let parse: libloading::os::unix::Symbol<DlParseLibraryPath> =
        unsafe { global.get(b"dl_parse_library_path\0") }.map_err(RuntimeError::ResolveDlParse)?;

    unsafe { parse(path_c.as_ptr(), delim_c.as_ptr()) };
    Ok(())
}

struct BareSoname {
    soname: &'static str,

    host_candidates: &'static [&'static str],
}

const BIONIC_BARE_SONAMES: &[BareSoname] = &[];

const ECLIPSE_LIBM_SONAME: &str = "libm.so";

const ECLIPSE_LIBM_SHIM_SO: &str = env!("ECLIPSE_LIBM_SHIM_SO");

const HOST_LIB_DIRS: &[&str] = &[
    "/usr/lib",
    "/lib",
    "/usr/lib64",
    "/lib64",
    "/usr/lib/x86_64-linux-gnu",
    "/lib/x86_64-linux-gnu",
];

pub fn provision_bionic_sonames(dir: &Path) -> Result<(), RuntimeError> {
    std::fs::create_dir_all(dir).map_err(|e| RuntimeError::ProvisionSoname(dir.to_owned(), e))?;

    provision_eclipse_libm(dir)?;

    for entry in BIONIC_BARE_SONAMES {
        let target = find_host_lib(entry)?;
        let link = dir.join(entry.soname);
        symlink_idempotent(&target, &link)?;
    }
    Ok(())
}

fn provision_eclipse_libm(dir: &Path) -> Result<(), RuntimeError> {
    let shim = Path::new(ECLIPSE_LIBM_SHIM_SO);
    let link = dir.join(ECLIPSE_LIBM_SONAME);
    let shim_len = match std::fs::metadata(shim) {
        Ok(m) => m.len(),

        Err(_) => {
            return Err(RuntimeError::HostLibNotFound {
                soname: ECLIPSE_LIBM_SONAME,
                candidates: &[],
            })
        }
    };

    if let Ok(meta) = std::fs::symlink_metadata(&link) {
        if meta.file_type().is_file() && meta.len() == shim_len {
            return Ok(());
        }

        std::fs::remove_file(&link).map_err(|e| RuntimeError::ProvisionSoname(link.clone(), e))?;
    }
    std::fs::copy(shim, &link).map_err(|e| RuntimeError::ProvisionSoname(link.clone(), e))?;
    Ok(())
}

fn find_host_lib(entry: &BareSoname) -> Result<PathBuf, RuntimeError> {
    for candidate in entry.host_candidates {
        if let Some(p) = cc_print_file_name(candidate) {
            if is_real_elf(&p) {
                return Ok(p);
            }
        }

        for base in HOST_LIB_DIRS {
            let p = Path::new(base).join(candidate);
            if is_real_elf(&p) {
                return Ok(p);
            }
        }
    }
    Err(RuntimeError::HostLibNotFound {
        soname: entry.soname,
        candidates: entry.host_candidates,
    })
}

fn cc_print_file_name(name: &str) -> Option<PathBuf> {
    let cc = std::env::var_os("CC").unwrap_or_else(|| "cc".into());
    let out = Command::new(cc)
        .arg(format!("-print-file-name={name}"))
        .output()
        .ok()?;
    if !out.status.success() {
        return None;
    }
    let line = String::from_utf8(out.stdout).ok()?;
    let reported = Path::new(line.trim());

    if reported.parent().is_none_or(|p| p.as_os_str().is_empty()) {
        return None;
    }
    std::fs::canonicalize(reported).ok()
}

fn is_real_elf(path: &Path) -> bool {
    use std::io::Read;
    let Ok(mut f) = std::fs::File::open(path) else {
        return false;
    };
    let mut magic = [0u8; 4];
    f.read_exact(&mut magic).is_ok() && magic == *b"\x7fELF"
}

fn symlink_idempotent(target: &Path, link: &Path) -> Result<(), RuntimeError> {
    if let Ok(existing) = std::fs::read_link(link) {
        if existing == target {
            return Ok(());
        }
    }

    match std::fs::remove_file(link) {
        Ok(()) => {}
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => {}
        Err(e) => return Err(RuntimeError::ProvisionSoname(link.to_owned(), e)),
    }
    std::os::unix::fs::symlink(target, link)
        .map_err(|e| RuntimeError::ProvisionSoname(link.to_owned(), e))
}

const LIBART_DLOPEN_FLAGS: std::os::raw::c_int =
    libloading::os::unix::RTLD_NOW | libloading::os::unix::RTLD_GLOBAL;

pub struct Vm {
    vm: *mut jni_sys::JavaVM,
}

impl Vm {
    #[must_use]
    pub fn as_raw(&self) -> *mut jni_sys::JavaVM {
        self.vm
    }
}

pub fn boot(
    plan: &BootPlan,
    apk_path: Option<&Path>,
    app_lib_dir: Option<&Path>,
) -> Result<Vm, RuntimeError> {
    let libart = find_libart()?;
    let art_boot = find_art_boot_paths()?;
    let boot_image = &art_boot.image_location;

    let mut option_strings: Vec<CString> = Vec::new();
    option_strings.push(make_cstring(format!("-Ximage:{}", boot_image.display()))?);
    if let Some(boot_class_path) = art_boot.boot_class_path.as_deref() {
        let actual = std::env::var_os("BOOTCLASSPATH");
        if actual.as_deref() != Some(boot_class_path) {
            return Err(RuntimeError::BootClassPathEnvironment {
                expected: boot_class_path.to_os_string(),
                actual,
            });
        }

        option_strings.push(make_os_option("-Xbootclasspath:", boot_class_path)?);
        option_strings.push(make_os_option(
            "-Xbootclasspath-locations:",
            boot_class_path,
        )?);
    }
    for opt in plan.vm_options() {
        option_strings.push(make_cstring(opt)?);
    }

    for opt in vm_options_from_env(std::env::var_os("ECLIPSE_VM_OPTIONS").as_deref()) {
        tracing::warn!(
            opt = opt.as_str(),
            "ECLIPSE_VM_OPTIONS is adding a dev-host VM option — this VM is NOT the shipped \
             configuration"
        );
        option_strings.push(make_cstring(opt)?);
    }

    if let Some(apk) = apk_path {
        let fw = find_framework()?;
        option_strings.push(make_cstring(class_path_option(&fw, apk))?);
        option_strings.push(make_cstring(library_path_option(&fw, app_lib_dir))?);
    }
    let mut options: Vec<jni_sys::JavaVMOption> = option_strings
        .iter()
        .map(|s| jni_sys::JavaVMOption {
            optionString: s.as_ptr().cast_mut(),
            extraInfo: std::ptr::null_mut(),
        })
        .collect();

    let mut args = jni_sys::JavaVMInitArgs {
        version: jni_sys::JNI_VERSION_1_6,
        nOptions: options.len() as jni_sys::jint,
        options: options.as_mut_ptr(),

        ignoreUnrecognized: jni_sys::JNI_TRUE,
    };

    let create: JniCreateJavaVm = {
        let lib =
            unsafe { libloading::os::unix::Library::open(Some(&libart), LIBART_DLOPEN_FLAGS) }
                .map_err(RuntimeError::LoadLibart)?;

        let sym: libloading::os::unix::Symbol<JniCreateJavaVm> =
            unsafe { lib.get(b"JNI_CreateJavaVM\0") }.map_err(RuntimeError::ResolveSymbol)?;
        let create = *sym;
        lib.into_raw();
        create
    };

    let mut vm: *mut jni_sys::JavaVM = std::ptr::null_mut();
    let mut env: *mut c_void = std::ptr::null_mut();

    let rc = unsafe {
        create(
            &mut vm,
            &mut env,
            (&mut args as *mut jni_sys::JavaVMInitArgs).cast(),
        )
    };
    if rc != jni_sys::JNI_OK {
        return Err(RuntimeError::CreateVm(rc));
    }
    if vm.is_null() || env.is_null() {
        return Err(RuntimeError::NullEnv);
    }

    match crate::loader::native_provider::install_guarded_altstack() {
        Ok(st) => {
            println!(
                "main-thread alternate signal stack: Eclipse guard-paged {} KiB @ {:#x} (replaces ART's heap-backed 32 KiB) ✓",
                st.ss_size / 1024,
                st.ss_sp
            );
        }
        Err(e) => {
            eprintln!("# WARNING: guard-paged altstack install failed (continuing on ART's heap-backed stack): {e}");
        }
    }

    Ok(Vm { vm })
}

fn make_cstring(s: String) -> Result<CString, RuntimeError> {
    CString::new(s).map_err(|_| RuntimeError::OptionHasNul)
}

fn make_os_option(prefix: &str, value: &OsStr) -> Result<CString, RuntimeError> {
    let mut bytes = Vec::with_capacity(prefix.len() + value.as_bytes().len());
    bytes.extend_from_slice(prefix.as_bytes());
    bytes.extend_from_slice(value.as_bytes());
    CString::new(bytes).map_err(|_| RuntimeError::OptionHasNul)
}

#[derive(Debug)]
pub enum RuntimeError {
    LibartNotFound(PathBuf),

    BootImageNotFound(PathBuf),

    BootClassPathJoin(std::env::JoinPathsError),

    BootClassPathEnvironment {
        expected: OsString,

        actual: Option<OsString>,
    },

    ArtOverlayMarkerInvalid(PathBuf),

    ArtOverlayMarkerRead(PathBuf, std::io::Error),

    ArtOverlayIncomplete(PathBuf),

    FrameworkNotFound(PathBuf),

    NoCacheDir,

    LoadLibart(libloading::Error),

    ResolveSymbol(libloading::Error),

    OpenGlobalScope(libloading::Error),

    ResolveDlParse(libloading::Error),

    HostLibNotFound {
        soname: &'static str,

        candidates: &'static [&'static str],
    },

    ProvisionSoname(PathBuf, std::io::Error),

    OptionHasNul,

    CreateVm(jni_sys::jint),

    NullEnv,
}

impl fmt::Display for RuntimeError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::LibartNotFound(p) => {
                write!(
                    f,
                    "vendored ART not found at {} (set ECLIPSE_LIBART to override)",
                    p.display()
                )
            }
            Self::BootImageNotFound(p) => {
                write!(
                    f,
                    "ART boot image not found near {} (set ECLIPSE_ART_BOOT_IMAGE to override)",
                    p.display()
                )
            }
            Self::BootClassPathJoin(error) => {
                write!(f, "could not encode the ART boot class path: {error}")
            }
            Self::BootClassPathEnvironment { expected, actual } => write!(
                f,
                "the patched ART overlay was selected but BOOTCLASSPATH was not prepared before \
                 worker threads started (expected {:?}, found {:?}); call \
                 runtime::prepare_art_boot_environment at launcher startup",
                expected, actual
            ),
            Self::ArtOverlayMarkerInvalid(path) => write!(
                f,
                "ART overlay readiness marker {} has an unsupported value; rebuild it with \
                 tools/framework-overlay/patch-framework.sh",
                path.display()
            ),
            Self::ArtOverlayMarkerRead(path, error) => write!(
                f,
                "could not read ART overlay readiness marker {}: {error}",
                path.display()
            ),
            Self::ArtOverlayIncomplete(path) => write!(
                f,
                "ART overlay readiness marker is present but required boot jar {} is missing; \
                 rebuild it with tools/framework-overlay/patch-framework.sh",
                path.display()
            ),
            Self::FrameworkNotFound(p) => {
                write!(
                    f,
                    "Android framework not found at {} (set ECLIPSE_ANDROID_FRAMEWORK_DIR to override)",
                    p.display()
                )
            }
            Self::NoCacheDir => f.write_str(
                "could not determine a cache directory for extracted native libs \
                 (set ECLIPSE_NATIVE_LIB_DIR to override)",
            ),
            Self::LoadLibart(e) => write!(f, "failed to dlopen libart.so: {e}"),
            Self::ResolveSymbol(e) => write!(f, "failed to resolve JNI_CreateJavaVM: {e}"),
            Self::OpenGlobalScope(e) => {
                write!(f, "failed to open the process-global symbol scope: {e}")
            }
            Self::ResolveDlParse(e) => write!(
                f,
                "failed to resolve dl_parse_library_path from libdl_bio.so.0 \
                 (is libart opened RTLD_GLOBAL?): {e}"
            ),
            Self::HostLibNotFound { soname, candidates } => write!(
                f,
                "no host library found to provide the bare Android soname '{soname}' \
                 (searched for {candidates:?} via `cc -print-file-name` and the standard host lib \
                 dirs); install the host package that provides one of these (e.g. glibc)"
            ),
            Self::ProvisionSoname(p, e) => {
                write!(
                    f,
                    "failed to provision a bionic-soname symlink at {}: {e}",
                    p.display()
                )
            }
            Self::OptionHasNul => f.write_str("an ART VM option contained an interior NUL byte"),
            Self::CreateVm(rc) => write!(f, "JNI_CreateJavaVM failed (status {rc})"),
            Self::NullEnv => f.write_str("JNI_CreateJavaVM returned a null JNIEnv"),
        }
    }
}

impl std::error::Error for RuntimeError {
    fn source(&self) -> Option<&(dyn std::error::Error + 'static)> {
        match self {
            Self::LoadLibart(e)
            | Self::ResolveSymbol(e)
            | Self::OpenGlobalScope(e)
            | Self::ResolveDlParse(e) => Some(e),
            Self::BootClassPathJoin(e) => Some(e),
            Self::ArtOverlayMarkerRead(_, e) => Some(e),
            Self::ProvisionSoname(_, e) => Some(e),
            _ => None,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::apk::Manifest;
    use crate::config::Config;

    fn manifest_with(target_sdk: Option<u32>) -> Manifest {
        Manifest {
            package: "com.roblox.client".to_owned(),
            launcher_activity: "com.roblox.client.startup.ActivitySplash".to_owned(),
            min_sdk: Some(26),
            target_sdk,
            large_heap: false,
        }
    }

    #[test]
    fn feature_string_format_present_absent() {
        assert_eq!(
            format_feature_string(|_| true),
            "ssse3,sse4.1,sse4.2,avx,avx2,popcnt"
        );

        assert_eq!(
            format_feature_string(|_| false),
            "-ssse3,-sse4.1,-sse4.2,-avx,-avx2,-popcnt"
        );
    }

    #[test]
    fn feature_string_mixed_keeps_order_and_prefix() {
        let s = format_feature_string(|t| matches!(t, "ssse3" | "sse4.1" | "sse4.2"));
        assert_eq!(s, "ssse3,sse4.1,sse4.2,-avx,-avx2,-popcnt");
    }

    #[test]
    fn art_boot_image_precedence_is_explicit_then_ready_overlay_then_stock() {
        let overlay = PathBuf::from("/cache/eclipse/framework-patched/art");
        assert_eq!(
            resolve_boot_image_location(
                Some(PathBuf::from("/custom/art/oat/boot.art")),
                Some(overlay.clone()),
                true,
            ),
            PathBuf::from("/custom/art/oat/boot.art")
        );
        assert_eq!(
            resolve_boot_image_location(None, Some(overlay.clone()), true),
            overlay.join("oat/boot.art")
        );
        assert_eq!(
            resolve_boot_image_location(None, Some(overlay), false),
            PathBuf::from(BOOT_IMAGE_DEFAULT)
        );
        assert_eq!(
            resolve_boot_image_location(None, None, true),
            PathBuf::from(BOOT_IMAGE_DEFAULT),
            "an impossible ready-without-directory state must degrade to stock without panicking"
        );
    }

    #[test]
    fn art_boot_class_path_keeps_the_pinned_order_and_uses_one_identity() {
        let art_dir = Path::new("/cache/eclipse/framework-patched/art");
        let joined = boot_class_path_for(art_dir).expect("test paths contain no separator");
        let paths: Vec<PathBuf> = std::env::split_paths(&joined).collect();
        let expected: Vec<PathBuf> = ART_BOOT_JARS
            .iter()
            .map(|name| art_dir.join(name))
            .collect();
        assert_eq!(paths, expected);

        let bytes = make_os_option("-Xbootclasspath:", &joined).expect("no NUL in test path");
        assert_eq!(
            bytes.as_bytes(),
            format!("-Xbootclasspath:{}", joined.to_string_lossy()).as_bytes()
        );
    }

    #[cfg(target_arch = "x86_64")]
    #[test]
    fn instruction_set_features_self_consistent_with_std_arch() {
        let expected = format_feature_string(|token| match token {
            "ssse3" => is_x86_feature_detected!("ssse3"),
            "sse4.1" => is_x86_feature_detected!("sse4.1"),
            "sse4.2" => is_x86_feature_detected!("sse4.2"),
            "avx" => is_x86_feature_detected!("avx"),
            "avx2" => is_x86_feature_detected!("avx2"),
            "popcnt" => is_x86_feature_detected!("popcnt"),
            _ => false,
        });
        assert_eq!(instruction_set_features(), expected);
    }

    #[test]
    fn boot_plan_derives_fields_from_manifest_and_config() {
        let plan = BootPlan::new(&manifest_with(Some(35)), &Config::default());
        assert_eq!(
            plan.launcher_activity,
            "com.roblox.client.startup.ActivitySplash"
        );
        assert_eq!(plan.sdk_int, 35);
        assert_eq!(plan.heap_mib, HEAP_MIB);
        assert!(plan.disable_hspace_compact);
        assert_eq!(plan.graphics_backend, GraphicsBackend::Vulkan);
        assert_eq!(plan.touch_mode, TouchMode::Off);
    }

    #[test]
    fn boot_plan_sdk_int_falls_back_when_manifest_omits_target() {
        let plan = BootPlan::new(&manifest_with(None), &Config::default());
        assert_eq!(plan.sdk_int, DEFAULT_SDK_INT);
    }

    #[test]
    fn boot_plan_use_opengl_selects_opengl_backend() {
        let config = Config {
            use_opengl: true,
            ..Config::default()
        };
        let plan = BootPlan::new(&manifest_with(Some(35)), &config);
        assert_eq!(plan.graphics_backend, GraphicsBackend::OpenGl);
    }

    #[test]
    fn vm_options_are_heap_only_no_dex2oat_flag() {
        let plan = BootPlan::new(&manifest_with(Some(35)), &Config::default());
        let vm = plan.vm_options();
        assert!(vm.contains(&"-Xmx768m".to_owned()), "{vm:?}");
        assert!(
            vm.contains(&"-XX:HeapGrowthLimit=768m".to_owned()),
            "{vm:?}"
        );
        assert!(
            vm.contains(&"-XX:DisableHSpaceCompactForOOM".to_owned()),
            "{vm:?}"
        );
        assert!(
            !vm.iter().any(|o| o.contains("instruction-set-features")),
            "VM options must not contain the dex2oat ISA flag: {vm:?}"
        );
    }

    #[test]
    fn vm_options_propagate_clamped_sdk_int() {
        let plan = BootPlan::new(&manifest_with(Some(35)), &Config::default());
        let vm = plan.vm_options();
        assert!(
            vm.contains(&"-DBuild.VERSION.SDK_INT=28".to_owned()),
            "manifest targetSdk=35 must be clamped to 28: {vm:?}"
        );
        assert!(
            !vm.iter()
                .any(|o| o.contains("SDK_INT=23") || o.contains("SDK_INT=35")),
            "must neither fall back to 23 nor exceed the androidx API-29 switch: {vm:?}"
        );

        let low = BootPlan::new(&manifest_with(Some(21)), &Config::default());
        assert!(
            low.vm_options()
                .contains(&"-DBuild.VERSION.SDK_INT=21".to_owned()),
            "a sub-28 target is propagated verbatim"
        );
    }

    #[test]
    fn vm_options_publish_sober_touch_mode_to_the_framework() {
        for (touch_mode, expected) in [
            (TouchMode::Off, "-Declipse.touch_mode=off"),
            (TouchMode::On, "-Declipse.touch_mode=on"),
            (TouchMode::FakeOff, "-Declipse.touch_mode=fake-off"),
        ] {
            let config = Config {
                touch_mode,
                ..Config::default()
            };
            let options = BootPlan::new(&manifest_with(Some(35)), &config).vm_options();
            assert!(options.contains(&expected.to_owned()), "{options:?}");
        }
    }

    #[test]
    fn dex2oat_options_carry_only_the_isa_flag() {
        let plan = BootPlan::new(&manifest_with(Some(35)), &Config::default());
        let d = plan.dex2oat_options();
        let isa = format!("--instruction-set-features={}", instruction_set_features());
        assert_eq!(d, vec![isa]);
    }

    #[test]
    fn vm_options_omit_hspace_flag_when_disabled() {
        let mut plan = BootPlan::new(&manifest_with(Some(35)), &Config::default());
        plan.disable_hspace_compact = false;
        let vm = plan.vm_options();
        assert!(
            !vm.iter().any(|o| o.contains("DisableHSpaceCompactForOOM")),
            "{vm:?}"
        );
        assert!(vm.contains(&"-Xmx768m".to_owned()), "{vm:?}");
    }

    #[test]
    fn libart_dlopen_flags_are_global_and_eager() {
        assert_ne!(
            LIBART_DLOPEN_FLAGS & libloading::os::unix::RTLD_GLOBAL,
            0,
            "libart must be dlopen'd RTLD_GLOBAL so liblog/__android_log_print is process-global"
        );
        assert_ne!(
            LIBART_DLOPEN_FLAGS & libloading::os::unix::RTLD_NOW,
            0,
            "libart must be dlopen'd RTLD_NOW so a missing symbol surfaces at load, not mid-lifecycle"
        );
    }

    #[test]
    fn find_libart_reports_typed_error_for_missing_override() {
        unsafe { std::env::set_var("ECLIPSE_LIBART", "/nonexistent/eclipse/libart.so") };
        let r = find_libart();
        unsafe { std::env::remove_var("ECLIPSE_LIBART") };
        assert!(matches!(r, Err(RuntimeError::LibartNotFound(_))), "{r:?}");
    }

    #[test]
    fn find_framework_reports_typed_error_for_missing_override() {
        unsafe { std::env::set_var("ECLIPSE_ANDROID_FRAMEWORK_DIR", "/nonexistent/eclipse/fw") };
        let r = find_framework();
        unsafe { std::env::remove_var("ECLIPSE_ANDROID_FRAMEWORK_DIR") };
        assert!(
            matches!(r, Err(RuntimeError::FrameworkNotFound(_))),
            "{r:?}"
        );
    }

    #[test]
    fn framework_dir_precedence_prefers_overlay_over_stock() {
        let overlay = PathBuf::from("/cache/eclipse/framework-patched");
        let stock = PathBuf::from(FRAMEWORK_DIR_DEFAULT);

        assert_eq!(
            resolve_framework_dir(
                Some(PathBuf::from("/custom/fw")),
                Some(overlay.clone()),
                true
            ),
            PathBuf::from("/custom/fw")
        );

        assert_eq!(
            resolve_framework_dir(None, Some(overlay.clone()), true),
            overlay
        );

        assert_eq!(resolve_framework_dir(None, Some(overlay), false), stock);

        assert_eq!(resolve_framework_dir(None, None, false), stock.clone());
    }

    #[test]
    fn class_path_option_orders_framework_apk_and_res() {
        let fw = FrameworkPaths {
            api_impl_jar: PathBuf::from("/fw/api-impl.jar"),
            framework_res_apk: PathBuf::from("/fw/framework-res.apk"),
            natives_dir: PathBuf::from("/fw/natives"),
        };
        let opt = class_path_option(&fw, Path::new("/apps/roblox.apk"));
        assert_eq!(
            opt,
            "-Djava.class.path=/fw/api-impl.jar:/apps/roblox.apk:/fw/framework-res.apk"
        );
    }

    #[test]
    fn library_path_option_points_at_framework_natives() {
        let fw = FrameworkPaths {
            api_impl_jar: PathBuf::from("/fw/api-impl.jar"),
            framework_res_apk: PathBuf::from("/fw/framework-res.apk"),
            natives_dir: PathBuf::from("/fw/natives"),
        };

        assert_eq!(
            library_path_option(&fw, None),
            "-Djava.library.path=/fw/natives"
        );
    }

    #[test]
    fn library_path_option_framework_first_then_app_lib_colon_joined() {
        let fw = FrameworkPaths {
            api_impl_jar: PathBuf::from("/fw/api-impl.jar"),
            framework_res_apk: PathBuf::from("/fw/framework-res.apk"),
            natives_dir: PathBuf::from("/fw/natives"),
        };
        let opt = library_path_option(&fw, Some(Path::new("/cache/eclipse/native-libs")));
        assert_eq!(
            opt,
            "-Djava.library.path=/fw/natives:/cache/eclipse/native-libs"
        );

        let value = opt.strip_prefix("-Djava.library.path=").expect("prefix");
        let parts: Vec<&str> = value.split(':').collect();
        assert_eq!(parts, vec!["/fw/natives", "/cache/eclipse/native-libs"]);
    }

    #[test]
    fn bionic_library_path_framework_first_then_app_lib_colon_joined() {
        let fw = FrameworkPaths {
            api_impl_jar: PathBuf::from("/fw/api-impl.jar"),
            framework_res_apk: PathBuf::from("/fw/framework-res.apk"),
            natives_dir: PathBuf::from("/fw/natives"),
        };
        let path = bionic_library_path(&fw, Some(Path::new("/cache/eclipse/native-libs")));
        assert_eq!(path, "/fw/natives:/cache/eclipse/native-libs");

        assert_eq!(BIONIC_LDPATH_DELIM, ":");
        let parts: Vec<&str> = path.split(BIONIC_LDPATH_DELIM).collect();
        assert_eq!(parts, vec!["/fw/natives", "/cache/eclipse/native-libs"]);

        let lib_opt = library_path_option(&fw, Some(Path::new("/cache/eclipse/native-libs")));
        assert_eq!(
            lib_opt.strip_prefix("-Djava.library.path="),
            Some(path.as_str())
        );
    }

    #[test]
    fn bionic_library_path_framework_only_when_no_app_lib() {
        let fw = FrameworkPaths {
            api_impl_jar: PathBuf::from("/fw/api-impl.jar"),
            framework_res_apk: PathBuf::from("/fw/framework-res.apk"),
            natives_dir: PathBuf::from("/fw/natives"),
        };
        assert_eq!(bionic_library_path(&fw, None), "/fw/natives");
    }

    #[test]
    fn is_real_elf_rejects_linker_script_accepts_elf_magic() {
        let dir = std::env::temp_dir().join(format!("eclipse-elf-test-{}", std::process::id()));
        std::fs::create_dir_all(&dir).expect("mk temp dir");

        let script = dir.join("libm.so");
        std::fs::write(
            &script,
            b"/* GNU ld script */\nGROUP ( /usr/lib/libm.so.6 )\n",
        )
        .expect("write script");
        assert!(
            !is_real_elf(&script),
            "a linker-script .so must be rejected (it is not loadable ELF)"
        );

        let elf = dir.join("libm.so.6");
        std::fs::write(&elf, b"\x7fELF\x02\x01\x01\x00rest-of-header").expect("write elf");
        assert!(is_real_elf(&elf), "a file with ELF magic must be accepted");

        assert!(
            !is_real_elf(&dir.join("does-not-exist.so")),
            "a missing file must be rejected, not panic"
        );

        std::fs::remove_dir_all(&dir).ok();
    }

    #[test]
    fn symlink_idempotent_creates_keeps_and_replaces() {
        let dir = std::env::temp_dir().join(format!("eclipse-symlink-test-{}", std::process::id()));
        std::fs::create_dir_all(&dir).expect("mk temp dir");
        let target_a = dir.join("libm.so.6");
        let target_b = dir.join("libm.so.7");
        std::fs::write(&target_a, b"\x7fELFa").expect("write a");
        std::fs::write(&target_b, b"\x7fELFb").expect("write b");
        let link = dir.join("libm.so");

        symlink_idempotent(&target_a, &link).expect("create");
        assert_eq!(std::fs::read_link(&link).expect("readlink"), target_a);

        symlink_idempotent(&target_a, &link).expect("keep");
        assert_eq!(std::fs::read_link(&link).expect("readlink"), target_a);

        symlink_idempotent(&target_b, &link).expect("replace");
        assert_eq!(std::fs::read_link(&link).expect("readlink"), target_b);

        std::fs::remove_file(&link).ok();
        std::fs::write(&link, b"not a link").expect("write regular file");
        symlink_idempotent(&target_a, &link).expect("replace regular file");
        assert_eq!(std::fs::read_link(&link).expect("readlink"), target_a);

        std::fs::remove_dir_all(&dir).ok();
    }

    #[test]
    fn host_symlinked_sonames_are_wellformed_and_libm_is_not_among_them() {
        assert!(
            !BIONIC_BARE_SONAMES.iter().any(|s| s.soname == "libm.so"),
            "libm.so must be Eclipse-shim-provided, NEVER host-symlinked (the host libm.so.6 modern \
             relocs abort the apkenv linker)"
        );
        for entry in BIONIC_BARE_SONAMES {
            assert!(
                entry.soname.ends_with(".so"),
                "a bare Android soname ends in .so: {}",
                entry.soname
            );
            assert!(
                !entry.host_candidates.is_empty(),
                "{} needs at least one host candidate",
                entry.soname
            );
            let count = BIONIC_BARE_SONAMES
                .iter()
                .filter(|s| s.soname == entry.soname)
                .count();
            assert_eq!(count, 1, "duplicate soname entry: {}", entry.soname);
        }
    }

    #[test]
    fn vm_options_from_env_defaults_to_none_and_splits_on_semicolons_never_colons() {
        use std::ffi::OsStr;

        assert!(vm_options_from_env(None).is_empty());
        assert!(vm_options_from_env(Some(OsStr::new(""))).is_empty());
        assert!(vm_options_from_env(Some(OsStr::new("  ;; ; "))).is_empty());

        assert_eq!(
            vm_options_from_env(Some(OsStr::new("-Xmethod-trace-file:/tmp/t.bin"))),
            vec!["-Xmethod-trace-file:/tmp/t.bin".to_owned()]
        );

        assert_eq!(
            vm_options_from_env(Some(OsStr::new(
                " -Xmethod-trace ;; -Xmethod-trace-file:/tmp/t.bin ; -Xmethod-trace-file-size:8000000 ;"
            ))),
            vec![
                "-Xmethod-trace".to_owned(),
                "-Xmethod-trace-file:/tmp/t.bin".to_owned(),
                "-Xmethod-trace-file-size:8000000".to_owned(),
            ]
        );
    }

    #[test]
    fn eclipse_libm_shim_is_apkenv_loadable_and_provisions_libm_so() {
        let shim = Path::new(ECLIPSE_LIBM_SHIM_SO);
        let bytes = std::fs::read(shim).expect("the build.rs-built libm shim .so must exist");
        assert_eq!(
            &bytes[..4],
            b"\x7fELF",
            "the shim must be a real ELF object"
        );
        assert!(
            is_real_elf(shim),
            "the shim must pass the apkenv real-ELF gate"
        );

        let img = crate::loader::elf::ElfImage::parse(&bytes).expect("decode shim ELF");
        for rela in img.relocations().expect("decode shim relocations") {
            assert_ne!(
                rela.r_type,
                crate::loader::reloc::R_X86_64_TPOFF64,
                "the libm shim regressed: an R_X86_64_TPOFF64 reloc would abort the apkenv linker"
            );
        }

        assert!(
            img.relr().expect("decode shim relr").is_empty(),
            "the libm shim must have no RELR (packed) relocations — the apkenv linker cannot apply them"
        );

        let dir = std::env::temp_dir().join(format!("eclipse-libm-prov-{}", std::process::id()));
        std::fs::create_dir_all(&dir).expect("mk temp dir");
        provision_eclipse_libm(&dir).expect("provision libm shim");
        let provisioned = dir.join("libm.so");
        let copied = std::fs::read(&provisioned).expect("provisioned libm.so must exist");
        assert_eq!(
            copied, bytes,
            "provisioned libm.so must be the shim's bytes"
        );

        provision_eclipse_libm(&dir).expect("provision libm shim (idempotent)");
        assert!(provisioned.is_file());
        std::fs::remove_dir_all(&dir).ok();
    }

    #[test]
    fn eclipse_libm_shim_math_values_are_correct() {
        use core::ffi::c_int;
        let lib = unsafe { libloading::Library::new(ECLIPSE_LIBM_SHIM_SO) }
            .expect("dlopen the built libm shim");
        const EPS: f64 = 1e-12;
        const EPSF: f32 = 1e-6;
        unsafe {
            let sin: libloading::Symbol<unsafe extern "C" fn(f64) -> f64> =
                lib.get(b"sin\0").expect("sin");
            assert!(
                (sin(core::f64::consts::FRAC_PI_2) - 1.0).abs() < EPS,
                "sin(pi/2)=1"
            );
            assert!(sin(0.0).abs() < EPS, "sin(0)=0");

            let cos: libloading::Symbol<unsafe extern "C" fn(f64) -> f64> =
                lib.get(b"cos\0").expect("cos");
            assert!((cos(0.0) - 1.0).abs() < EPS, "cos(0)=1");
            assert!(cos(core::f64::consts::PI).abs() - 1.0 < EPS, "cos(pi)=-1");

            let pow: libloading::Symbol<unsafe extern "C" fn(f64, f64) -> f64> =
                lib.get(b"pow\0").expect("pow");
            assert!((pow(2.0, 10.0) - 1024.0).abs() < EPS, "2^10=1024");
            assert!((pow(9.0, 0.5) - 3.0).abs() < EPS, "9^0.5=3 (sqrt)");

            let log: libloading::Symbol<unsafe extern "C" fn(f64) -> f64> =
                lib.get(b"log\0").expect("log");
            assert!((log(core::f64::consts::E) - 1.0).abs() < EPS, "ln(e)=1");

            let exp: libloading::Symbol<unsafe extern "C" fn(f64) -> f64> =
                lib.get(b"exp\0").expect("exp");
            assert!((exp(0.0) - 1.0).abs() < EPS, "exp(0)=1");

            let fmod: libloading::Symbol<unsafe extern "C" fn(f64, f64) -> f64> =
                lib.get(b"fmod\0").expect("fmod");
            assert!((fmod(10.0, 3.0) - 1.0).abs() < EPS, "fmod(10,3)=1");

            let atan2: libloading::Symbol<unsafe extern "C" fn(f64, f64) -> f64> =
                lib.get(b"atan2\0").expect("atan2");
            assert!(
                (atan2(1.0, 1.0) - core::f64::consts::FRAC_PI_4).abs() < EPS,
                "atan2(1,1)=pi/4"
            );

            let sinf: libloading::Symbol<unsafe extern "C" fn(f32) -> f32> =
                lib.get(b"sinf\0").expect("sinf");
            assert!((sinf(0.0)).abs() < EPSF, "sinf(0)=0");

            let powf: libloading::Symbol<unsafe extern "C" fn(f32, f32) -> f32> =
                lib.get(b"powf\0").expect("powf");
            assert!((powf(2.0, 8.0) - 256.0).abs() < EPSF, "2^8=256 (f32)");

            let frexp: libloading::Symbol<unsafe extern "C" fn(f64, *mut c_int) -> f64> =
                lib.get(b"frexp\0").expect("frexp");
            let mut e: c_int = 0;
            let m = frexp(8.0, &mut e);
            assert!(
                (m - 0.5).abs() < EPS && e == 4,
                "frexp(8)=(0.5, 4), got ({m}, {e})"
            );
        }
    }
}
