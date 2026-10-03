use std::ffi::{c_char, c_void, CString, OsStr, OsString};
use std::fmt;
use std::os::unix::ffi::{OsStrExt, OsStringExt};
use std::path::{Path, PathBuf};
use std::process::Command;
use std::sync::OnceLock;

use directories::ProjectDirs;
use eclipse_config::temp_file::TempFile;
use eclipse_config::{Config, TouchMode};

use crate::apk::Manifest;
use crate::host_locale::HostLocale;
use crate::host_time_zone::HostTimeZone;

const DEFAULT_SDK_INT: u32 = 33;

const JAVA_SDK_INT_CEILING: u32 = 28;

pub(crate) const HEAP_MIB: u32 = 768;

const LIBART_DEFAULT: &str = "/usr/lib/art/libart.so";

const ART_SUBDIR: &str = "art";

const STOCK_FRAMEWORK_SUBDIR: &str = "android_translation_layer";

const BUNDLED_FRAMEWORK_DIR: &str = "framework";

const API_IMPL_JAR: &str = "api-impl.jar";

const ZONEINFO_SUBDIR: &str = "zoneinfo";

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

const APP_DEX_COMPILER_FILTER: &str = "verify";

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
struct HostCpu {
    ssse3: bool,
    sse4_1: bool,
    sse4_2: bool,
    avx: bool,
    avx2: bool,
    bmi1: bool,
    popcnt: bool,
}

impl HostCpu {
    #[cfg(target_arch = "x86_64")]
    fn detect() -> Self {
        Self {
            ssse3: is_x86_feature_detected!("ssse3"),
            sse4_1: is_x86_feature_detected!("sse4.1"),
            sse4_2: is_x86_feature_detected!("sse4.2"),
            avx: is_x86_feature_detected!("avx"),
            avx2: is_x86_feature_detected!("avx2"),
            bmi1: is_x86_feature_detected!("bmi1"),
            popcnt: is_x86_feature_detected!("popcnt"),
        }
    }

    fn missing_android_x86_64_feature(self) -> Option<&'static str> {
        [
            ("SSSE3", self.ssse3),
            ("SSE4.1", self.sse4_1),
            ("SSE4.2", self.sse4_2),
            ("POPCNT", self.popcnt),
        ]
        .into_iter()
        .find_map(|(name, present)| (!present).then_some(name))
    }

    fn art_feature_string(self) -> String {
        let features = [
            ("ssse3", self.ssse3),
            ("sse4.1", self.sse4_1),
            ("sse4.2", self.sse4_2),
            ("avx", self.avx),
            ("avx2", self.avx2 && self.bmi1),
            ("popcnt", self.popcnt),
        ];
        let mut out = String::with_capacity(64);
        for (token, present) in features {
            if !out.is_empty() {
                out.push(',');
            }
            if !present {
                out.push('-');
            }
            out.push_str(token);
        }
        out
    }
}

#[cfg(target_arch = "x86_64")]
#[must_use]
pub fn instruction_set_features() -> String {
    HostCpu::detect().art_feature_string()
}

pub fn android_cpu_baseline() -> Result<(), RuntimeError> {
    android_cpu_baseline_on(HostCpu::detect())
}

fn android_cpu_baseline_on(cpu: HostCpu) -> Result<(), RuntimeError> {
    match cpu.missing_android_x86_64_feature() {
        Some(feature) => Err(RuntimeError::CpuLacksFeature(feature)),
        None => Ok(()),
    }
}

#[cfg(not(target_arch = "x86_64"))]
compile_error!(
    "Eclipse's runtime targets the Android x86-64 Roblox engine; \
     instruction_set_features() needs an x86_64 host (no x86 ISA to detect on this arch)"
);

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct BootPlan {
    pub launcher_activity: String,

    pub sdk_int: u32,

    pub heap_mib: u32,

    pub disable_hspace_compact: bool,

    pub instruction_set_features: String,

    pub touch_mode: TouchMode,

    pub(crate) host_locale: Option<HostLocale>,

    pub(crate) host_time_zone: Option<HostTimeZone>,
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
            touch_mode: config.touch_mode,
            host_locale: HostLocale::from_env(|name| std::env::var_os(name)),
            host_time_zone: HostTimeZone::detect(),
        }
    }

    #[must_use]
    pub fn with_activity_override(mut self, activity: impl Into<String>) -> Self {
        self.launcher_activity = activity.into();
        self
    }

    #[must_use]
    pub fn java_sdk_int(&self) -> u32 {
        self.sdk_int.min(JAVA_SDK_INT_CEILING)
    }

    #[must_use]
    pub fn vm_options(&self) -> Vec<String> {
        let mut opts = Vec::with_capacity(10);
        opts.push(format!("-Xmx{}m", self.heap_mib));
        opts.push(format!("-XX:HeapGrowthLimit={}m", self.heap_mib));
        if self.disable_hspace_compact {
            opts.push("-XX:DisableHSpaceCompactForOOM".to_owned());
        }
        opts.push("-Xcompiler-option".to_owned());
        opts.push(format!("--compiler-filter={APP_DEX_COMPILER_FILTER}"));
        opts.push("-Xcompiler-option".to_owned());
        opts.push(format!(
            "--instruction-set-features={}",
            self.instruction_set_features
        ));

        opts.push(format!("-DBuild.VERSION.SDK_INT={}", self.java_sdk_int()));

        opts.push(format!("-Declipse.touch_mode={}", self.touch_mode.as_str()));
        if let Some(locale) = &self.host_locale {
            opts.push(format!("-Duser.locale={}", locale.language_tag()));
        }
        if let Some(zone) = &self.host_time_zone {
            opts.push(format!("-Duser.timezone={}", zone.id()));
        }
        opts
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
struct InstallLayout {
    bundled_libart: Option<PathBuf>,

    bundled_framework: Option<PathBuf>,
}

impl InstallLayout {
    fn current() -> Result<&'static Self, RuntimeError> {
        static CURRENT: OnceLock<InstallLayout> = OnceLock::new();
        if let Some(layout) = CURRENT.get() {
            return Ok(layout);
        }
        let exe = std::env::current_exe().map_err(RuntimeError::CurrentExe)?;
        Ok(CURRENT.get_or_init(|| Self::probe(&exe)))
    }

    fn probe(exe: &Path) -> Self {
        Self {
            bundled_libart: bundled_libart_path(exe).filter(|path| path.is_file()),
            bundled_framework: bundled_framework_dir(exe)
                .filter(|dir| bundled_overlay_is_present(dir)),
        }
    }
}

fn bundled_libart_path(exe: &Path) -> Option<PathBuf> {
    let prefix = exe.parent()?.parent()?;
    Some(prefix.join(ART_SUBDIR).join("libart.so"))
}

fn bundled_framework_dir(exe: &Path) -> Option<PathBuf> {
    exe.parent()
        .map(|exe_dir| exe_dir.join(BUNDLED_FRAMEWORK_DIR))
}

fn resolve_libart(explicit: Option<PathBuf>, bundled: Option<PathBuf>) -> PathBuf {
    explicit
        .or(bundled)
        .unwrap_or_else(|| PathBuf::from(LIBART_DEFAULT))
}

fn libart_location(layout: &InstallLayout) -> PathBuf {
    resolve_libart(env_path("ECLIPSE_LIBART"), layout.bundled_libart.clone())
}

fn libart_path(path: PathBuf) -> Result<PathBuf, RuntimeError> {
    if path.exists() {
        Ok(path)
    } else {
        Err(RuntimeError::LibartNotFound(path))
    }
}

pub fn find_libart() -> Result<PathBuf, RuntimeError> {
    libart_path(libart_location(InstallLayout::current()?))
}

fn stock_dex_root(libart: &Path) -> Option<PathBuf> {
    let lib_dir = libart.parent()?.parent()?;
    Some(lib_dir.join("java").join("dex"))
}

const LIBCORE_PRIMARY_JAR: &str = "core-oj-hostdex.jar";

#[derive(Debug, Clone, PartialEq, Eq)]
struct ArtBootPaths {
    image_location: PathBuf,

    boot_class_path: Option<OsString>,

    art_dir: PathBuf,
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

#[derive(Debug, Clone, PartialEq, Eq)]
enum FrameworkOverlay {
    Explicit(PathBuf),

    Bundled(PathBuf),

    DevCache(PathBuf),
}

impl FrameworkOverlay {
    fn dir(&self) -> &Path {
        match self {
            Self::Explicit(dir) | Self::Bundled(dir) | Self::DevCache(dir) => dir,
        }
    }
}

fn resolve_framework_overlay(
    explicit: Option<PathBuf>,
    bundled: Option<PathBuf>,
    dev_cache: Option<PathBuf>,
) -> Option<FrameworkOverlay> {
    explicit
        .map(FrameworkOverlay::Explicit)
        .or_else(|| bundled.map(FrameworkOverlay::Bundled))
        .or_else(|| dev_cache.map(FrameworkOverlay::DevCache))
}

fn bundled_overlay_is_present(dir: &Path) -> bool {
    dir.join(API_IMPL_JAR).is_file() && dir.join(ART_SUBDIR).join(ART_OVERLAY_MARKER).is_file()
}

fn framework_overlay(layout: &InstallLayout) -> Option<FrameworkOverlay> {
    resolve_framework_overlay(
        env_path("ECLIPSE_ANDROID_FRAMEWORK_DIR"),
        layout.bundled_framework.clone(),
        patched_overlay_dir(),
    )
}

fn overlay_art_dir(layout: &InstallLayout) -> Option<PathBuf> {
    framework_overlay(layout).map(|overlay| overlay.dir().join(ART_SUBDIR))
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

fn boot_image_in(art_dir: &Path) -> PathBuf {
    art_dir.join("oat").join("boot.art")
}

fn resolve_boot_image_location(
    explicit: Option<PathBuf>,
    overlay_art: Option<PathBuf>,
    overlay_ready: bool,
    stock_art_dir: Option<&Path>,
) -> Option<PathBuf> {
    if let Some(path) = explicit {
        Some(path)
    } else if let Some(art_dir) = overlay_art.filter(|_| overlay_ready) {
        Some(boot_image_in(&art_dir))
    } else {
        stock_art_dir.map(boot_image_in)
    }
}

fn find_art_boot_paths(layout: &InstallLayout) -> Result<ArtBootPaths, RuntimeError> {
    let libart = libart_location(layout);
    let stock_art_dir = stock_dex_root(&libart).map(|root| root.join(ART_SUBDIR));
    let explicit = env_path("ECLIPSE_ART_BOOT_IMAGE");
    let overlay_art = overlay_art_dir(layout);
    let overlay_ready = if explicit.is_none() {
        overlay_art
            .as_deref()
            .map(overlay_is_ready)
            .transpose()?
            .unwrap_or(false)
    } else {
        false
    };
    let image_location = resolve_boot_image_location(
        explicit,
        overlay_art,
        overlay_ready,
        stock_art_dir.as_deref(),
    )
    .ok_or(RuntimeError::StockDexRootUnresolved(libart))?;
    let art_dir = art_dir_from_image(&image_location)
        .ok_or_else(|| RuntimeError::BootImageNotFound(image_location.clone()))?;

    if overlay_ready {
        if let Some(missing) = first_missing_art_jar(&art_dir) {
            return Err(RuntimeError::ArtOverlayIncomplete(missing));
        }
    } else if !art_dir.join(LIBCORE_PRIMARY_JAR).is_file() {
        return Err(RuntimeError::BootImageNotFound(image_location));
    }

    let self_contained = stock_art_dir.as_deref() != Some(art_dir.as_path())
        && first_missing_art_jar(&art_dir).is_none();
    let boot_class_path = self_contained
        .then(|| boot_class_path_for(&art_dir))
        .transpose()?;

    Ok(ArtBootPaths {
        image_location,
        boot_class_path,
        art_dir,
    })
}

pub fn prepare_art_boot_environment() -> Result<(), RuntimeError> {
    prepare_art_boot_environment_on(HostCpu::detect(), InstallLayout::current)
}

fn prepare_art_boot_environment_on(
    cpu: HostCpu,
    layout: impl FnOnce() -> Result<&'static InstallLayout, RuntimeError>,
) -> Result<(), RuntimeError> {
    android_cpu_baseline_on(cpu)?;
    let paths = find_art_boot_paths(layout()?)?;
    if let Some(boot_class_path) = paths.boot_class_path {
        if std::env::var_os("BOOTCLASSPATH").as_ref() != Some(&boot_class_path) {
            unsafe { std::env::set_var("BOOTCLASSPATH", boot_class_path) };
        }
    }
    Ok(())
}

pub fn find_boot_image() -> Result<PathBuf, RuntimeError> {
    find_art_boot_paths(InstallLayout::current()?).map(|paths| paths.image_location)
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

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct FrameworkPaths {
    pub api_impl_jar: PathBuf,

    pub framework_res_apk: PathBuf,

    pub natives_dir: PathBuf,
}

fn patched_overlay_dir() -> Option<PathBuf> {
    ProjectDirs::from("", "", "eclipse").map(|d| d.cache_dir().join("framework-patched"))
}

fn overlay_framework_dir(overlay: Option<FrameworkOverlay>) -> Option<PathBuf> {
    match overlay? {
        FrameworkOverlay::Explicit(dir) | FrameworkOverlay::Bundled(dir) => Some(dir),
        FrameworkOverlay::DevCache(dir) => dir.join(API_IMPL_JAR).exists().then_some(dir),
    }
}

fn framework_dir(layout: &InstallLayout) -> Result<PathBuf, RuntimeError> {
    if let Some(dir) = overlay_framework_dir(framework_overlay(layout)) {
        return Ok(dir);
    }
    let libart = libart_location(layout);
    let Some(stock_root) = stock_dex_root(&libart) else {
        return Err(RuntimeError::StockDexRootUnresolved(libart));
    };
    let dir = stock_root.join(STOCK_FRAMEWORK_SUBDIR);
    tracing::warn!(
        framework_dir = %dir.display(),
        "no patched framework overlay found (bundled beside the executable or in the dev \
         cache); using the stock ATL framework, which lacks Roblox-required android.* \
         classes/fields (boot will fail in RobloxApplication.onCreate). Run \
         tools/framework-overlay/patch-framework.sh or set ECLIPSE_ANDROID_FRAMEWORK_DIR."
    );
    Ok(dir)
}

fn find_framework_in(dir: PathBuf) -> Result<FrameworkPaths, RuntimeError> {
    let api_impl_jar = dir.join(API_IMPL_JAR);
    if !api_impl_jar.exists() {
        return Err(RuntimeError::FrameworkNotFound(api_impl_jar));
    }
    Ok(FrameworkPaths {
        framework_res_apk: dir.join("framework-res.apk"),
        natives_dir: dir.join("natives"),
        api_impl_jar,
    })
}

pub fn find_framework() -> Result<FrameworkPaths, RuntimeError> {
    find_framework_in(framework_dir(InstallLayout::current()?)?)
}

#[must_use]
pub fn dalvik_cache_dir() -> Option<PathBuf> {
    dalvik_cache_dir_in(std::env::var_os("XDG_CACHE_HOME"), std::env::var_os("HOME"))
}

fn dalvik_cache_dir_in(
    xdg_cache_home: Option<OsString>,
    home: Option<OsString>,
) -> Option<PathBuf> {
    let root = match xdg_cache_home {
        Some(cache) => PathBuf::from(cache).join(ART_SUBDIR),
        None => PathBuf::from(home?).join(".cache").join(ART_SUBDIR),
    };
    Some(root.join("x86_64"))
}

#[must_use]
pub fn dalvik_cache_stem(location: &Path) -> Option<OsString> {
    let relative = location.as_os_str().as_bytes().strip_prefix(b"/")?;
    let stem: Vec<u8> = relative
        .iter()
        .map(|&byte| if byte == b'/' { b'@' } else { byte })
        .collect();
    Some(OsString::from_vec(stem))
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum NativeLibRoot {
    Cache(PathBuf),

    Override(PathBuf),
}

pub fn native_lib_root() -> Result<NativeLibRoot, RuntimeError> {
    if let Some(dir) = env_path("ECLIPSE_NATIVE_LIB_DIR") {
        return Ok(NativeLibRoot::Override(dir));
    }
    let dirs = ProjectDirs::from("", "", "eclipse").ok_or(RuntimeError::NoCacheDir)?;
    Ok(NativeLibRoot::Cache(dirs.cache_dir().join("native-libs")))
}

fn search_path(entries: &[&Path]) -> Result<OsString, RuntimeError> {
    let mut joined = OsString::new();
    for (index, entry) in entries.iter().enumerate() {
        if entry
            .as_os_str()
            .as_bytes()
            .contains(&SEARCH_PATH_SEPARATOR)
        {
            return Err(RuntimeError::SearchPathEntryHasSeparator(
                entry.to_path_buf(),
            ));
        }
        if index > 0 {
            joined.push(BIONIC_LDPATH_DELIM);
        }
        joined.push(entry);
    }
    Ok(joined)
}

fn class_path_option(fw: &FrameworkPaths, apk: &Path) -> Result<CString, RuntimeError> {
    make_os_option(
        "-Djava.class.path=",
        &search_path(&[&fw.api_impl_jar, apk, &fw.framework_res_apk])?,
    )
}

fn library_path_option(
    fw: &FrameworkPaths,
    app_lib_dir: Option<&Path>,
) -> Result<CString, RuntimeError> {
    make_os_option(
        "-Djava.library.path=",
        &library_search_path(fw, app_lib_dir)?,
    )
}

type JniCreateJavaVm = unsafe extern "system" fn(
    *mut *mut jni_sys::JavaVM,
    *mut *mut c_void,
    *mut c_void,
) -> jni_sys::jint;

type DlParseLibraryPath = unsafe extern "C" fn(*const c_char, *const c_char);

const BIONIC_LDPATH_DELIM: &str = ":";

const SEARCH_PATH_SEPARATOR: u8 = b':';

fn library_search_path(
    fw: &FrameworkPaths,
    app_lib_dir: Option<&Path>,
) -> Result<OsString, RuntimeError> {
    match app_lib_dir {
        Some(dir) => search_path(&[&fw.natives_dir, dir]),
        None => search_path(&[&fw.natives_dir]),
    }
}

pub fn whitelist_bionic_library_path(
    fw: &FrameworkPaths,
    app_lib_dir: Option<&Path>,
) -> Result<(), RuntimeError> {
    let path_c = make_os_option("", &library_search_path(fw, app_lib_dir)?)?;
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

const ECLIPSE_LIBM_SHIM: &[u8] = include_bytes!(env!("ECLIPSE_LIBM_SHIM_SO"));

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
    use std::os::unix::fs::PermissionsExt as _;

    let target = dir.join(ECLIPSE_LIBM_SONAME);
    if std::fs::read(&target).is_ok_and(|bytes| bytes == ECLIPSE_LIBM_SHIM) {
        return Ok(());
    }
    let mut temporary = TempFile::create(dir, ECLIPSE_LIBM_SONAME)
        .map_err(|e| RuntimeError::ProvisionSoname(dir.to_owned(), e))?;
    temporary
        .write_all(ECLIPSE_LIBM_SHIM)
        .and_then(|()| {
            std::fs::set_permissions(temporary.path(), std::fs::Permissions::from_mode(0o755))
        })
        .map_err(|e| RuntimeError::ProvisionSoname(temporary.path().to_owned(), e))?;
    temporary
        .persist(&target)
        .map_err(|e| RuntimeError::ProvisionSoname(target, e))
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
    let layout = InstallLayout::current()?;
    let libart = libart_path(libart_location(layout))?;
    let art_boot = find_art_boot_paths(layout)?;
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
        option_strings.push(zoneinfo_dir_option(&art_boot.art_dir)?);
    }
    for opt in plan.vm_options() {
        option_strings.push(make_cstring(opt)?);
    }
    crate::loader::ndk_registry::set_device_api_level(plan.java_sdk_int());
    if let Some(locale) = &plan.host_locale {
        crate::loader::ndk_registry::set_configuration_locale(locale.configuration_locale());
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
        let fw = find_framework_in(framework_dir(layout)?)?;
        option_strings.push(class_path_option(&fw, apk)?);
        option_strings.push(library_path_option(&fw, app_lib_dir)?);
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

fn zoneinfo_dir_option(art_dir: &Path) -> Result<CString, RuntimeError> {
    make_os_option(
        "-Declipse.zoneinfo_dir=",
        art_dir.join(ZONEINFO_SUBDIR).as_os_str(),
    )
}

fn make_os_option(prefix: &str, value: &OsStr) -> Result<CString, RuntimeError> {
    let mut bytes = Vec::with_capacity(prefix.len() + value.as_bytes().len());
    bytes.extend_from_slice(prefix.as_bytes());
    bytes.extend_from_slice(value.as_bytes());
    CString::new(bytes).map_err(|_| RuntimeError::OptionHasNul)
}

#[derive(Debug)]
pub enum RuntimeError {
    CpuLacksFeature(&'static str),

    CurrentExe(std::io::Error),

    LibartNotFound(PathBuf),

    StockDexRootUnresolved(PathBuf),

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

    SearchPathEntryHasSeparator(PathBuf),

    CreateVm(jni_sys::jint),

    NullEnv,
}

impl fmt::Display for RuntimeError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::CpuLacksFeature(feature) => write!(
                f,
                "this CPU lacks {feature}; the Android x86-64 Roblox client and Eclipse's ART \
                 need an x86-64 CPU with SSSE3, SSE4.1, SSE4.2 and POPCNT"
            ),
            Self::CurrentExe(e) => write!(
                f,
                "cannot resolve the Eclipse executable path to locate bundled ART and framework \
                 files: {e}"
            ),
            Self::LibartNotFound(p) => {
                write!(
                    f,
                    "vendored ART not found at {} (set ECLIPSE_LIBART to override)",
                    p.display()
                )
            }
            Self::StockDexRootUnresolved(p) => write!(
                f,
                "cannot derive the stock ART/ATL dex directory from libart path {} (expected \
                 <libdir>/art/libart.so with the jars under <libdir>/java/dex; set \
                 ECLIPSE_LIBART to such an absolute path)",
                p.display()
            ),
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
                write!(f, "failed to provision bionic soname {}: {e}", p.display())
            }
            Self::OptionHasNul => f.write_str("an ART VM option contained an interior NUL byte"),
            Self::SearchPathEntryHasSeparator(path) => write!(
                f,
                "{} contains ':', which ART and the Android linker read as a search-path \
                 separator; use a directory without ':' (Eclipse's own files follow \
                 XDG_DATA_HOME, XDG_CACHE_HOME, ECLIPSE_NATIVE_LIB_DIR and \
                 ECLIPSE_ANDROID_FRAMEWORK_DIR)",
                path.display()
            ),
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
            Self::CurrentExe(e)
            | Self::ArtOverlayMarkerRead(_, e)
            | Self::ProvisionSoname(_, e) => Some(e),
            _ => None,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::apk::Manifest;
    use eclipse_config::Config;

    fn manifest_with(target_sdk: Option<u32>) -> Manifest {
        Manifest {
            package: "com.roblox.client".to_owned(),
            launcher_activity: "com.roblox.client.startup.ActivitySplash".to_owned(),
            min_sdk: Some(26),
            target_sdk,
            large_heap: false,
        }
    }

    const ALL_FEATURES: HostCpu = HostCpu {
        ssse3: true,
        sse4_1: true,
        sse4_2: true,
        avx: true,
        avx2: true,
        bmi1: true,
        popcnt: true,
    };

    #[test]
    fn feature_string_format_present_absent() {
        assert_eq!(
            ALL_FEATURES.art_feature_string(),
            "ssse3,sse4.1,sse4.2,avx,avx2,popcnt"
        );

        let none = HostCpu {
            ssse3: false,
            sse4_1: false,
            sse4_2: false,
            avx: false,
            avx2: false,
            bmi1: false,
            popcnt: false,
        };
        assert_eq!(
            none.art_feature_string(),
            "-ssse3,-sse4.1,-sse4.2,-avx,-avx2,-popcnt"
        );
    }

    #[test]
    fn feature_string_mixed_keeps_order_and_prefix() {
        let baseline = HostCpu {
            avx: false,
            avx2: false,
            bmi1: false,
            popcnt: false,
            ..ALL_FEATURES
        };
        assert_eq!(
            baseline.art_feature_string(),
            "ssse3,sse4.1,sse4.2,-avx,-avx2,-popcnt"
        );
    }

    #[test]
    fn cpus_below_the_android_x86_64_baseline_are_named() {
        assert_eq!(ALL_FEATURES.missing_android_x86_64_feature(), None);
        let without_avx = HostCpu {
            avx: false,
            avx2: false,
            bmi1: false,
            ..ALL_FEATURES
        };
        assert_eq!(without_avx.missing_android_x86_64_feature(), None);
        let without_popcnt = HostCpu {
            popcnt: false,
            ..ALL_FEATURES
        };
        assert_eq!(
            without_popcnt.missing_android_x86_64_feature(),
            Some("POPCNT")
        );
        let core2 = HostCpu {
            sse4_1: false,
            sse4_2: false,
            popcnt: false,
            ..without_avx
        };
        assert_eq!(core2.missing_android_x86_64_feature(), Some("SSE4.1"));
        assert!(RuntimeError::CpuLacksFeature("SSE4.1")
            .to_string()
            .contains("lacks SSE4.1"));
    }

    #[test]
    fn art_preparation_refuses_a_cpu_below_the_baseline_before_the_install_layout() {
        let core2 = HostCpu {
            sse4_1: false,
            sse4_2: false,
            avx: false,
            avx2: false,
            bmi1: false,
            popcnt: false,
            ..ALL_FEATURES
        };
        let prepared = prepare_art_boot_environment_on(core2, || {
            panic!("the install layout was looked up for a CPU below the baseline")
        });
        assert!(matches!(
            prepared,
            Err(RuntimeError::CpuLacksFeature("SSE4.1"))
        ));
    }

    #[test]
    fn the_android_cpu_baseline_names_the_first_missing_feature() {
        let without_sse4_2 = HostCpu {
            sse4_2: false,
            popcnt: false,
            ..ALL_FEATURES
        };
        assert!(matches!(
            android_cpu_baseline_on(without_sse4_2),
            Err(RuntimeError::CpuLacksFeature("SSE4.2"))
        ));
        assert!(android_cpu_baseline_on(ALL_FEATURES).is_ok());
    }

    #[test]
    fn art_preparation_on_a_baseline_cpu_continues_to_the_install_layout() {
        let prepared =
            prepare_art_boot_environment_on(ALL_FEATURES, || Err(RuntimeError::NoCacheDir));
        assert!(matches!(prepared, Err(RuntimeError::NoCacheDir)));
    }

    #[test]
    fn avx2_is_reported_only_with_bmi1_because_art_emits_bmi1_for_it() {
        let without_bmi1 = HostCpu {
            bmi1: false,
            ..ALL_FEATURES
        };
        assert_eq!(
            without_bmi1.art_feature_string(),
            "ssse3,sse4.1,sse4.2,avx,-avx2,popcnt"
        );
    }

    #[test]
    fn art_boot_image_precedence_is_explicit_then_ready_overlay_then_stock() {
        let overlay = PathBuf::from("/cache/eclipse/framework-patched/art");
        let stock = Path::new("/usr/lib/java/dex/art");
        assert_eq!(
            resolve_boot_image_location(
                Some(PathBuf::from("/custom/art/oat/boot.art")),
                Some(overlay.clone()),
                true,
                Some(stock),
            ),
            Some(PathBuf::from("/custom/art/oat/boot.art"))
        );
        assert_eq!(
            resolve_boot_image_location(None, Some(overlay.clone()), true, Some(stock)),
            Some(overlay.join("oat/boot.art"))
        );
        assert_eq!(
            resolve_boot_image_location(None, Some(overlay.clone()), false, Some(stock)),
            Some(PathBuf::from("/usr/lib/java/dex/art/oat/boot.art"))
        );
        assert_eq!(
            resolve_boot_image_location(None, None, true, Some(stock)),
            Some(PathBuf::from("/usr/lib/java/dex/art/oat/boot.art")),
            "an impossible ready-without-directory state must degrade to stock without panicking"
        );
        assert_eq!(
            resolve_boot_image_location(None, Some(overlay), false, None),
            None,
            "an unresolvable stock directory must surface instead of inventing a path"
        );
    }

    #[test]
    fn stock_dex_root_follows_the_atl_and_art_rule_relative_to_libart() {
        assert_eq!(
            stock_dex_root(Path::new(LIBART_DEFAULT)),
            Some(PathBuf::from("/usr/lib/java/dex")),
            "the system libart must keep resolving to today's /usr/lib/java/dex paths"
        );
        assert_eq!(
            stock_dex_root(Path::new("/app/lib/art/libart.so")),
            Some(PathBuf::from("/app/lib/java/dex"))
        );
        assert_eq!(
            stock_dex_root(Path::new("/usr/lib64/art/libart.so")),
            Some(PathBuf::from("/usr/lib64/java/dex"))
        );
        assert_eq!(stock_dex_root(Path::new("libart.so")), None);
    }

    #[test]
    fn libart_precedence_is_explicit_then_bundled_then_system() {
        let explicit = PathBuf::from("/opt/art/lib/art/libart.so");
        let bundled = PathBuf::from("/app/lib/art/libart.so");
        assert_eq!(
            resolve_libart(Some(explicit.clone()), Some(bundled.clone())),
            explicit
        );
        assert_eq!(resolve_libart(None, Some(bundled.clone())), bundled);
        assert_eq!(resolve_libart(None, None), PathBuf::from(LIBART_DEFAULT));
    }

    #[test]
    fn install_layout_derives_bundled_paths_from_the_executable() {
        let exe = Path::new("/app/lib/eclipse/eclipse");
        assert_eq!(
            bundled_libart_path(exe),
            Some(PathBuf::from("/app/lib/art/libart.so"))
        );
        assert_eq!(
            bundled_framework_dir(exe),
            Some(PathBuf::from("/app/lib/eclipse/framework"))
        );
    }

    fn temp_tree(name: &str) -> PathBuf {
        let dir =
            std::env::temp_dir().join(format!("eclipse-runtime-{name}-{}", std::process::id()));
        std::fs::remove_dir_all(&dir).ok();
        std::fs::create_dir_all(&dir).expect("create temp tree");
        dir
    }

    #[test]
    fn bundled_install_is_detected_and_a_dev_build_falls_through() {
        let root = temp_tree("layout");
        let bundle_exe = root.join("bundle/lib/eclipse/eclipse");
        let framework = root.join("bundle/lib/eclipse/framework");
        std::fs::create_dir_all(framework.join("art")).expect("create framework");
        std::fs::create_dir_all(root.join("bundle/lib/art")).expect("create art dir");
        std::fs::write(&bundle_exe, b"").expect("write exe");
        std::fs::write(root.join("bundle/lib/art/libart.so"), b"").expect("write libart");
        std::fs::write(framework.join(API_IMPL_JAR), b"").expect("write api-impl");

        assert_eq!(
            InstallLayout::probe(&bundle_exe),
            InstallLayout {
                bundled_libart: Some(root.join("bundle/lib/art/libart.so")),
                bundled_framework: None,
            },
            "a bundled overlay without the ART readiness marker must not be selected"
        );
        std::fs::write(
            framework.join(ART_SUBDIR).join(ART_OVERLAY_MARKER),
            ART_OVERLAY_MARKER_CONTENT,
        )
        .expect("write marker");
        assert_eq!(
            InstallLayout::probe(&bundle_exe),
            InstallLayout {
                bundled_libart: Some(root.join("bundle/lib/art/libart.so")),
                bundled_framework: Some(framework.clone()),
            }
        );

        assert_eq!(
            InstallLayout::probe(&root.join("repo/target/release/eclipse")),
            InstallLayout {
                bundled_libart: None,
                bundled_framework: None,
            }
        );

        std::fs::remove_dir_all(&root).ok();
    }

    #[test]
    fn framework_overlay_precedence_is_explicit_then_bundled_then_dev_cache() {
        let explicit = PathBuf::from("/custom/fw");
        let bundled = PathBuf::from("/app/lib/eclipse/framework");
        let cache = PathBuf::from("/cache/eclipse/framework-patched");
        assert_eq!(
            resolve_framework_overlay(
                Some(explicit.clone()),
                Some(bundled.clone()),
                Some(cache.clone())
            ),
            Some(FrameworkOverlay::Explicit(explicit))
        );
        assert_eq!(
            resolve_framework_overlay(None, Some(bundled.clone()), Some(cache.clone())),
            Some(FrameworkOverlay::Bundled(bundled.clone()))
        );
        assert_eq!(
            resolve_framework_overlay(None, None, Some(cache.clone())),
            Some(FrameworkOverlay::DevCache(cache.clone()))
        );
        assert_eq!(resolve_framework_overlay(None, None, None), None);
        assert_eq!(
            FrameworkOverlay::Bundled(bundled.clone())
                .dir()
                .join(ART_SUBDIR),
            bundled.join("art"),
            "the ART overlay lives in the selected overlay's art directory"
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
        assert_eq!(plan.touch_mode, TouchMode::Off);
    }

    #[test]
    fn boot_plan_sdk_int_falls_back_when_manifest_omits_target() {
        let plan = BootPlan::new(&manifest_with(None), &Config::default());
        assert_eq!(plan.sdk_int, DEFAULT_SDK_INT);
    }

    fn compiler_options(vm: &[String]) -> Vec<&str> {
        vm.windows(2)
            .filter(|pair| pair[0] == "-Xcompiler-option")
            .map(|pair| pair[1].as_str())
            .collect()
    }

    #[test]
    fn vm_options_set_the_heap_limits() {
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
    }

    #[test]
    fn vm_options_forward_host_isa_features_to_the_compilers() {
        let plan = BootPlan::new(&manifest_with(Some(35)), &Config::default());
        let vm = plan.vm_options();
        let features = format!(
            "--instruction-set-features={}",
            plan.instruction_set_features
        );
        assert!(compiler_options(&vm).contains(&features.as_str()), "{vm:?}");
        assert_eq!(
            vm.iter().filter(|option| **option == features).count(),
            1,
            "the ISA flag appears once, as a compiler option: {vm:?}"
        );
    }

    #[test]
    fn vm_options_select_the_verify_filter_for_app_dex_compiles() {
        let plan = BootPlan::new(&manifest_with(Some(35)), &Config::default());
        assert!(
            compiler_options(&plan.vm_options()).contains(&"--compiler-filter=verify"),
            "{:?}",
            plan.vm_options()
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
        assert_eq!(plan.java_sdk_int(), 28);
        assert_eq!(low.java_sdk_int(), 21);
    }

    #[test]
    fn vm_options_publish_the_host_locale_as_the_java_default_locale() {
        let mut plan = BootPlan::new(&manifest_with(Some(35)), &Config::default());
        plan.host_locale = HostLocale::from_env(|name| {
            (name == "LANG").then(|| std::ffi::OsString::from("fr_FR.UTF-8"))
        });
        assert!(
            plan.vm_options()
                .contains(&"-Duser.locale=fr-FR".to_owned()),
            "{:?}",
            plan.vm_options()
        );

        plan.host_locale = None;
        assert!(
            !plan
                .vm_options()
                .iter()
                .any(|o| o.starts_with("-Duser.locale")),
            "without a host locale libcore keeps its en-US default: {:?}",
            plan.vm_options()
        );
    }

    #[test]
    fn vm_options_publish_the_host_time_zone_as_the_java_default_time_zone() {
        let mut plan = BootPlan::new(&manifest_with(Some(35)), &Config::default());
        let zoneinfo = crate::host_time_zone::tests::zoneinfo_with("vm-options", &["Europe/Paris"]);
        plan.host_time_zone = HostTimeZone::from_sources(
            &zoneinfo,
            Some(std::ffi::OsString::from("Europe/Paris")),
            None,
            None,
        );
        std::fs::remove_dir_all(&zoneinfo).ok();
        assert!(
            plan.vm_options()
                .contains(&"-Duser.timezone=Europe/Paris".to_owned()),
            "{:?}",
            plan.vm_options()
        );

        plan.host_time_zone = None;
        assert!(
            !plan
                .vm_options()
                .iter()
                .any(|o| o.starts_with("-Duser.timezone")),
            "without a host time zone libcore keeps its own default: {:?}",
            plan.vm_options()
        );
    }

    #[test]
    fn zoneinfo_dir_option_points_libcore_at_the_overlay_tzdata() {
        let option = zoneinfo_dir_option(Path::new("/app/lib/eclipse/framework/art"))
            .expect("no NUL in test path");
        assert_eq!(
            option.as_bytes(),
            b"-Declipse.zoneinfo_dir=/app/lib/eclipse/framework/art/zoneinfo"
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
    fn a_missing_libart_is_a_typed_error() {
        let r = libart_path(PathBuf::from("/nonexistent/eclipse/libart.so"));
        assert!(matches!(r, Err(RuntimeError::LibartNotFound(_))), "{r:?}");
    }

    #[test]
    fn a_missing_framework_is_a_typed_error() {
        let r = find_framework_in(PathBuf::from("/nonexistent/eclipse/fw"));
        assert!(
            matches!(r, Err(RuntimeError::FrameworkNotFound(_))),
            "{r:?}"
        );
    }

    #[test]
    fn framework_dir_prefers_overlays_and_needs_api_impl_only_in_the_dev_cache() {
        assert_eq!(
            overlay_framework_dir(Some(FrameworkOverlay::Explicit(PathBuf::from(
                "/custom/fw"
            )))),
            Some(PathBuf::from("/custom/fw"))
        );
        assert_eq!(
            overlay_framework_dir(Some(FrameworkOverlay::Bundled(PathBuf::from(
                "/app/lib/eclipse/framework"
            )))),
            Some(PathBuf::from("/app/lib/eclipse/framework"))
        );
        assert_eq!(overlay_framework_dir(None), None);

        let cache = temp_tree("dev-cache");
        assert_eq!(
            overlay_framework_dir(Some(FrameworkOverlay::DevCache(cache.clone()))),
            None,
            "a dev cache without api-impl.jar must fall back to the stock framework"
        );
        std::fs::write(cache.join(API_IMPL_JAR), b"").expect("write api-impl");
        assert_eq!(
            overlay_framework_dir(Some(FrameworkOverlay::DevCache(cache.clone()))),
            Some(cache.clone())
        );
        std::fs::remove_dir_all(&cache).ok();
    }

    #[test]
    fn dalvik_cache_follows_art_xdg_then_home_rule() {
        assert_eq!(
            dalvik_cache_dir_in(Some("/cache".into()), Some("/home/u".into())),
            Some(PathBuf::from("/cache/art/x86_64"))
        );
        assert_eq!(
            dalvik_cache_dir_in(None, Some("/home/u".into())),
            Some(PathBuf::from("/home/u/.cache/art/x86_64"))
        );
        assert_eq!(dalvik_cache_dir_in(None, None), None);
    }

    #[test]
    fn dalvik_cache_stem_mangles_absolute_locations_like_art() {
        assert_eq!(
            dalvik_cache_stem(Path::new("/x/data/eclipse/roblox/3170/base.apk")),
            Some(OsString::from("x@data@eclipse@roblox@3170@base.apk"))
        );
        assert_eq!(dalvik_cache_stem(Path::new("roblox/base.apk")), None);
    }

    #[test]
    fn class_path_option_orders_framework_apk_and_res() {
        let fw = FrameworkPaths {
            api_impl_jar: PathBuf::from("/fw/api-impl.jar"),
            framework_res_apk: PathBuf::from("/fw/framework-res.apk"),
            natives_dir: PathBuf::from("/fw/natives"),
        };
        let opt = class_path_option(&fw, Path::new("/apps/roblox.apk")).unwrap();
        assert_eq!(
            opt.to_str().unwrap(),
            "-Djava.class.path=/fw/api-impl.jar:/apps/roblox.apk:/fw/framework-res.apk"
        );
    }

    #[test]
    fn search_path_entries_that_contain_the_separator_are_refused() {
        let fw = FrameworkPaths {
            api_impl_jar: PathBuf::from("/fw/api-impl.jar"),
            framework_res_apk: PathBuf::from("/fw/framework-res.apk"),
            natives_dir: PathBuf::from("/fw/natives"),
        };
        let apk = Path::new("/media/u/Roblox:backup/base.apk");
        let err = class_path_option(&fw, apk).unwrap_err();
        assert!(
            matches!(&err, RuntimeError::SearchPathEntryHasSeparator(path) if path == apk),
            "{err:?}"
        );
        assert!(err.to_string().contains("Roblox:backup"), "{err}");

        let app_lib_dir = Path::new("/cache/a:b");
        for err in [
            library_path_option(&fw, Some(app_lib_dir)).unwrap_err(),
            library_search_path(&fw, Some(app_lib_dir)).unwrap_err(),
        ] {
            assert!(
                matches!(&err, RuntimeError::SearchPathEntryHasSeparator(path) if path == app_lib_dir),
                "{err:?}"
            );
        }
    }

    #[test]
    fn search_paths_keep_non_utf8_bytes() {
        use std::os::unix::ffi::OsStrExt as _;

        let dir = Path::new(OsStr::from_bytes(b"/cache/\xff/native-libs"));
        let fw = FrameworkPaths {
            api_impl_jar: PathBuf::from("/fw/api-impl.jar"),
            framework_res_apk: PathBuf::from("/fw/framework-res.apk"),
            natives_dir: PathBuf::from("/fw/natives"),
        };
        assert_eq!(
            library_search_path(&fw, Some(dir)).unwrap().as_bytes(),
            b"/fw/natives:/cache/\xff/native-libs"
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
            library_path_option(&fw, None).unwrap().to_str().unwrap(),
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
        let opt = library_path_option(&fw, Some(Path::new("/cache/eclipse/native-libs"))).unwrap();
        let opt = opt.to_str().unwrap();
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
        let path = library_search_path(&fw, Some(Path::new("/cache/eclipse/native-libs"))).unwrap();
        let path = path.to_str().unwrap();
        assert_eq!(path, "/fw/natives:/cache/eclipse/native-libs");

        assert_eq!(BIONIC_LDPATH_DELIM, ":");
        let parts: Vec<&str> = path.split(BIONIC_LDPATH_DELIM).collect();
        assert_eq!(parts, vec!["/fw/natives", "/cache/eclipse/native-libs"]);

        let lib_opt =
            library_path_option(&fw, Some(Path::new("/cache/eclipse/native-libs"))).unwrap();
        assert_eq!(
            lib_opt
                .to_str()
                .unwrap()
                .strip_prefix("-Djava.library.path="),
            Some(path)
        );
    }

    #[test]
    fn bionic_library_path_framework_only_when_no_app_lib() {
        let fw = FrameworkPaths {
            api_impl_jar: PathBuf::from("/fw/api-impl.jar"),
            framework_res_apk: PathBuf::from("/fw/framework-res.apk"),
            natives_dir: PathBuf::from("/fw/natives"),
        };
        assert_eq!(library_search_path(&fw, None).unwrap(), "/fw/natives");
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
        let bytes = ECLIPSE_LIBM_SHIM;
        assert_eq!(
            &bytes[..4],
            b"\x7fELF",
            "the shim must be a real ELF object"
        );

        let img = crate::loader::elf::ElfImage::parse(bytes).expect("decode shim ELF");
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

        let dir = temp_tree("libm-prov");
        provision_eclipse_libm(&dir).expect("provision libm shim");
        let provisioned = dir.join("libm.so");
        let copied = std::fs::read(&provisioned).expect("provisioned libm.so must exist");
        assert_eq!(
            copied, bytes,
            "provisioned libm.so must be the shim's bytes"
        );
        assert!(
            is_real_elf(&provisioned),
            "the provisioned shim must pass the apkenv real-ELF gate"
        );

        provision_eclipse_libm(&dir).expect("provision libm shim (idempotent)");
        assert!(provisioned.is_file());
        std::fs::remove_dir_all(&dir).ok();
    }

    #[test]
    fn eclipse_libm_provisioning_replaces_stale_same_size_copies_and_symlinks() {
        let dir = temp_tree("libm-stale");
        let provisioned = dir.join(ECLIPSE_LIBM_SONAME);

        let mut stale = ECLIPSE_LIBM_SHIM.to_vec();
        let last = stale.len() - 1;
        stale[last] ^= 0xff;
        std::fs::write(&provisioned, &stale).expect("write stale shim");
        provision_eclipse_libm(&dir).expect("replace the stale shim");
        assert_eq!(
            std::fs::read(&provisioned).expect("read provisioned shim"),
            ECLIPSE_LIBM_SHIM,
            "a stale shim of the same size from an older Eclipse must be replaced"
        );

        let host_lib = dir.join("libm.so.6");
        std::fs::write(&host_lib, b"\x7fELF-host").expect("write host lib");
        std::fs::remove_file(&provisioned).expect("remove shim");
        std::os::unix::fs::symlink(&host_lib, &provisioned).expect("link host lib");
        provision_eclipse_libm(&dir).expect("replace the host symlink");
        assert!(
            std::fs::symlink_metadata(&provisioned)
                .expect("stat provisioned shim")
                .file_type()
                .is_file(),
            "the soname must become Eclipse's own file, not a link to the host libm"
        );
        assert_eq!(
            std::fs::read(&host_lib).expect("read host lib"),
            b"\x7fELF-host",
            "the host library behind the old link must stay untouched"
        );

        std::fs::remove_dir_all(&dir).ok();
    }

    #[test]
    fn a_libm_shim_that_cannot_be_installed_leaves_no_temporary() {
        let dir = temp_tree("libm-blocked");
        std::fs::create_dir(dir.join(ECLIPSE_LIBM_SONAME)).expect("block the soname");

        let result = provision_eclipse_libm(&dir);
        let left: Vec<_> = std::fs::read_dir(&dir)
            .expect("list the lib dir")
            .map(|entry| entry.expect("lib dir entry").file_name())
            .collect();
        std::fs::remove_dir_all(&dir).ok();
        assert!(
            matches!(&result, Err(RuntimeError::ProvisionSoname(path, _)) if path.ends_with(ECLIPSE_LIBM_SONAME)),
            "{result:?}"
        );
        assert_eq!(left, [ECLIPSE_LIBM_SONAME]);
    }

    #[test]
    fn eclipse_libm_shim_math_values_are_correct() {
        use core::ffi::c_int;
        let dir = temp_tree("libm-math");
        provision_eclipse_libm(&dir).expect("provision libm shim");
        let lib = unsafe { libloading::Library::new(dir.join(ECLIPSE_LIBM_SONAME)) }
            .expect("dlopen the provisioned libm shim");
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
        drop(lib);
        std::fs::remove_dir_all(&dir).ok();
    }
}
