//! Android runtime: ART boot, host detection & lifecycle (component-map C/I · 🟢 + 🔴 ART).
//!
//! Boots the **vendored AOSP ART** by `dlopen`-ing `libart.so` and calling its
//! `JNI_CreateJavaVM` with a boot image + the M0-validated VM options — see the boot diagram
//! in `docs/art-and-runtime.md` §3 and the evidence-based recipe in its "VM boot —
//! implementation plan". ART is **unavoidable** for Roblox (component-map §3) and sits **off
//! the gameplay hot path**, so it costs no FPS.
//!
//! ## What this module implements *now* (M1–M2)
//!  1. [`instruction_set_features`] — runtime host-CPU detection producing the real
//!     `dex2oat --instruction-set-features` string (detect-don't-assume, AGENTS.md §9; the M0
//!     Step 4 fix for ATL's hardcoded baseline ISA).
//!  2. [`BootPlan`] — the concrete ART launch parameters, derived from a verified APK
//!     [`Manifest`](crate::apk::Manifest) + a [`Config`](crate::config::Config) + host
//!     detection. [`BootPlan::vm_options`] are the `-X*` flags passed to `JNI_CreateJavaVM`;
//!     [`BootPlan::dex2oat_options`] is the dex2oat-only `--instruction-set-features` flag
//!     (a *separate* tool — never a `JavaVMOption`).
//!  3. [`boot`] — `dlopen` libart + `JNI_CreateJavaVM` to bring up the VM (returning `JNI_OK`
//!     with a live `JavaVM`+`JNIEnv`). 2026-06-04: verified to boot ART on this host from a
//!     bare process (no GTK4/Mesa/winit) — the decisive test of Eclipse's Step 3.5 thesis (a
//!     graphics-stack-free process has a clean low_4gb window, so ART boots where ATL+GTK4
//!     exhausted it). ART self-loads libcore's native backends via the translation linker
//!     (libart's `NEEDED libdl_bio`), so no explicit bionic_translation setup is needed.
//!     With an APK ([`find_framework`]) it also puts the app + `android.*` framework on the
//!     classpath, so ART loads Roblox's Java (verified: `FindClass` resolves `com.roblox.*`).
//!
//! ## Not here yet
//! Reaching Roblox's `onCreate` — driving the launcher Activity via JNI (ATL's recipe:
//! `Context.createApplication` → `ContentProvider.createContentProviders` →
//! `Application.onCreate` → `Activity.createMainActivity(window)`), with `System.loadLibrary`
//! pulling `libroblox.so` via the translation linker. That needs a *framework*: ATL's
//! `api-impl.jar` is GTK-coupled (its `create*` entry points take a `GtkWidget*` window), so
//! the production path is Eclipse's own **winit + `ash`/EGL** framework — see
//! `docs/art-and-runtime.md` ("VM boot — implementation plan").
//!
//! ## `unsafe`
//! 2026-06-04: this module is no longer `#![forbid(unsafe_code)]` — [`boot`] needs FFI. All
//! `unsafe` is confined to the `dlopen`/`JNI_CreateJavaVM` path in [`boot`] and carries
//! `// SAFETY:` notes; everything else (host detection, [`BootPlan`]) is safe. Under the
//! release `panic = "abort"` profile (§2.4), a panic can never unwind across this FFI; when we
//! later register Rust JNI *callbacks*, each must wrap its body in `catch_unwind` (§2.8).

use std::ffi::{c_char, c_void, CString};
use std::fmt;
use std::path::{Path, PathBuf};
use std::process::Command;

use directories::ProjectDirs;

use crate::apk::Manifest;
use crate::config::Config;

/// Default Android API level used when the manifest declares no `android:targetSdkVersion`.
///
/// 2026-06-04: Roblox v2.724.735 declares `targetSdk=35`, but `<uses-sdk>` is legitimately
/// optional, so [`Manifest::target_sdk`](crate::apk::Manifest) is an `Option`. ATL's own
/// boot uses `--sdk-int=33` (see `docs/m0-runbook.md`); 33 is the documented, conservative
/// fallback the framework layer is known to satisfy when the manifest is silent.
const DEFAULT_SDK_INT: u32 = 33;

/// M0-validated managed-heap cap in MiB (`-Xmx` / `-XX:HeapGrowthLimit`).
///
/// 2026-06-04: ATL's 256 MiB default OOMs Roblox during asset loading; the M0 bisect
/// (AGENTS.md §5) found 768 MiB clears the GC-thrash OOM and, paired with
/// `-XX:DisableHSpaceCompactForOOM`, fits a single contiguous reservation.
const HEAP_MIB: u32 = 768;

/// Default install path of the vendored ART VM library (Arch/AUR `art_standalone`).
/// Overridable via `ECLIPSE_LIBART` for other distros (detect-don't-assume, §9).
const LIBART_DEFAULT: &str = "/usr/lib/art/libart.so";
/// Default ART boot-image *location* (a key, not necessarily an existing file — ART compiles
/// it into the dalvik-cache, e.g. `~/.cache/art`, on first run). Overridable via
/// `ECLIPSE_ART_BOOT_IMAGE`.
const BOOT_IMAGE_DEFAULT: &str = "/usr/lib/java/dex/art/oat/boot.art";

/// The `dex2oat`/ART x86 instruction-set feature tokens, in ART's canonical emit order.
///
/// 2026-06-04: order and spellings are from AOSP ART
/// `runtime/arch/x86/instruction_set_features_x86.cc` (`GetFeatureString`): `ssse3, sse4.1,
/// sse4.2, avx, avx2, popcnt`, bare when present or `-`-prefixed when absent. Matches the
/// baseline string M0 Step 4 observed ATL's dex2oat emit. Each is a valid
/// `std::arch::is_x86_feature_detected!` name, so detection maps 1:1 onto emission.
const X86_FEATURE_TOKENS: [&str; 6] = ["ssse3", "sse4.1", "sse4.2", "avx", "avx2", "popcnt"];

/// Detect this host's x86-64 ISA features and format them as a `dex2oat`
/// `--instruction-set-features` value (present features by name, absent ones prefixed `-`).
///
/// The detect-don't-assume fix for the M0 Step 4 finding that ATL hardcodes a conservative
/// baseline ISA (§9): passing the real host ISA lets dex2oat emit better code (§6 perf).
/// Uses `std::arch::is_x86_feature_detected!` — a runtime `CPUID` query, not a compile-time
/// `target_feature` check — so the result reflects the machine the launcher runs on.
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

// 2026-06-04: Eclipse runs only the Android x86-64 build of Roblox (`lib/x86_64/libroblox.so`),
// so a non-x86_64 host cannot run the engine. Fail to compile with an actionable message rather
// than emit a bogus ISA string (detect-don't-assume, §9).
#[cfg(not(target_arch = "x86_64"))]
compile_error!(
    "Eclipse's runtime targets the Android x86-64 Roblox engine; \
     instruction_set_features() needs an x86_64 host (no x86 ISA to detect on this arch)"
);

/// Format the `--instruction-set-features` string from a feature-presence predicate.
///
/// Split out from [`instruction_set_features`] so formatting (token order, the `-` prefix,
/// comma joining) is testable without depending on the host CPU.
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

/// The graphics backend the boot will request for the surface.
///
/// Vulkan is the default (best FPS — lower driver overhead, explicit multithreaded submission,
/// AGENTS.md §6); OpenGL is the fallback, selected when
/// [`Config::use_opengl`](crate::config::Config) forces GL where Vulkan can't init.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum GraphicsBackend {
    /// Vulkan via `ash` (default; perf-first).
    Vulkan,
    /// OpenGL/EGL fallback (forced by `config.use_opengl`).
    OpenGl,
}

impl GraphicsBackend {
    /// A short, stable label for diagnostics / the dry-run plan.
    #[must_use]
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Vulkan => "Vulkan",
            Self::OpenGl => "OpenGL",
        }
    }
}

/// The concrete ART launch parameters, derived from a verified APK
/// [`Manifest`](crate::apk::Manifest), a [`Config`](crate::config::Config), and host detection.
///
/// Every field maps 1:1 to a real ART/dex2oat argument. Note the two distinct destinations:
/// [`vm_options`](BootPlan::vm_options) → `JNI_CreateJavaVM`'s `JavaVMOption` array;
/// [`dex2oat_options`](BootPlan::dex2oat_options) → the separate `dex2oat` invocation.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct BootPlan {
    /// Fully-qualified launcher Activity to start (manifest's resolved MAIN/LAUNCHER activity;
    /// overridable via [`BootPlan::with_activity_override`] to skip the splash).
    pub launcher_activity: String,
    /// Android API level handed to the framework layer (`--sdk-int`). From the manifest's
    /// `targetSdk`, or [`DEFAULT_SDK_INT`] when absent.
    pub sdk_int: u32,
    /// Managed-heap cap in MiB (`-Xmx` and `-XX:HeapGrowthLimit`). M0-validated value.
    pub heap_mib: u32,
    /// Whether to pass `-XX:DisableHSpaceCompactForOOM` (suppresses ART's second heap
    /// reservation so a single ≥640 MiB block fits — required for [`HEAP_MIB`], AGENTS.md §5).
    pub disable_hspace_compact: bool,
    /// The `dex2oat --instruction-set-features` value for this host (see
    /// [`instruction_set_features`]).
    pub instruction_set_features: String,
    /// The graphics backend for the surface (Vulkan default, OpenGL when forced).
    pub graphics_backend: GraphicsBackend,
}

impl BootPlan {
    /// Build the boot plan from a verified manifest and the effective config.
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
        }
    }

    /// Override the launcher activity (e.g. `"com.roblox.client.ActivityNativeMain"` to bypass
    /// the splash, as the M0 boot did).
    #[must_use]
    pub fn with_activity_override(mut self, activity: impl Into<String>) -> Self {
        self.launcher_activity = activity.into();
        self
    }

    /// The `-X`/`-XX` options passed to `JNI_CreateJavaVM`'s `JavaVMOption` array.
    ///
    /// VM options only — the M0-validated heap sizing. The boot image (`-Ximage:<path>`) is a
    /// discovered path added by [`boot`], not part of the logical plan. Crucially this does NOT
    /// include `--instruction-set-features` (a dex2oat flag — see [`dex2oat_options`]); mixing
    /// the two destinations would feed dex2oat flags to the VM.
    #[must_use]
    pub fn vm_options(&self) -> Vec<String> {
        let mut opts = Vec::with_capacity(3);
        opts.push(format!("-Xmx{}m", self.heap_mib));
        opts.push(format!("-XX:HeapGrowthLimit={}m", self.heap_mib));
        if self.disable_hspace_compact {
            opts.push("-XX:DisableHSpaceCompactForOOM".to_owned());
        }
        opts
    }

    /// The options passed to the separate `dex2oat` AOT invocation (currently just the host
    /// ISA — the M0 Step 4 fix). Kept distinct from [`vm_options`] because dex2oat is a
    /// different tool; `--instruction-set-features` is not a valid `JavaVMOption`.
    #[must_use]
    pub fn dex2oat_options(&self) -> Vec<String> {
        vec![format!(
            "--instruction-set-features={}",
            self.instruction_set_features
        )]
    }
}

/// Locate the vendored `libart.so` (`ECLIPSE_LIBART` override, else the default install path).
///
/// Returns [`RuntimeError::LibartNotFound`] with the path searched if it does not exist —
/// detect-don't-assume with an actionable error (§9), never a silent fallback.
pub fn find_libart() -> Result<PathBuf, RuntimeError> {
    let path = env_path("ECLIPSE_LIBART").unwrap_or_else(|| PathBuf::from(LIBART_DEFAULT));
    if path.exists() {
        Ok(path)
    } else {
        Err(RuntimeError::LibartNotFound(path))
    }
}

/// The primary libcore boot jar; its presence in the ART data dir means the boot image's
/// source is installed (the patched ART bakes the full boot-jar list, so it compiles the image
/// from these itself — see `docs/art-and-runtime.md`).
const LIBCORE_PRIMARY_JAR: &str = "core-oj-hostdex.jar";

/// Locate the ART boot-image location (`ECLIPSE_ART_BOOT_IMAGE` override, else the default).
///
/// The path is a *location key*, not an on-disk file: ART derives a dalvik-cache name from it
/// and compiles the image there on first run (e.g. `~/.cache/art`), and the literal
/// `.../oat/boot.art` need not — and on this install does not — exist. So validation checks
/// that the ART data dir holding the libcore boot jars is present (the location's grandparent,
/// e.g. `/usr/lib/java/dex/art/`, containing [`LIBCORE_PRIMARY_JAR`]), not the image file.
/// Returns [`RuntimeError::BootImageNotFound`] otherwise — detect-don't-assume (§9).
pub fn find_boot_image() -> Result<PathBuf, RuntimeError> {
    let path =
        env_path("ECLIPSE_ART_BOOT_IMAGE").unwrap_or_else(|| PathBuf::from(BOOT_IMAGE_DEFAULT));
    // `.../art/oat/boot.art` → grandparent `.../art/` holds the libcore boot jars.
    let installed = path
        .parent()
        .and_then(Path::parent)
        .map(|art_dir| art_dir.join(LIBCORE_PRIMARY_JAR).exists())
        .unwrap_or(false);
    if installed {
        Ok(path)
    } else {
        Err(RuntimeError::BootImageNotFound(path))
    }
}

/// Read an environment variable as a non-empty `PathBuf`.
fn env_path(var: &str) -> Option<PathBuf> {
    match std::env::var_os(var) {
        Some(v) if !v.is_empty() => Some(PathBuf::from(v)),
        _ => None,
    }
}

/// Default install dir of the vendored Android framework (ATL's `api-impl.jar`,
/// `framework-res.apk`, `natives/`). Overridable via `ECLIPSE_ANDROID_FRAMEWORK_DIR`.
const FRAMEWORK_DIR_DEFAULT: &str = "/usr/lib/java/dex/android_translation_layer";

/// Paths to the vendored Android framework Eclipse boots the app against.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct FrameworkPaths {
    /// `api-impl.jar` — the `android.*` framework reimplementation (on the VM classpath).
    pub api_impl_jar: PathBuf,
    /// `framework-res.apk` — the framework resources (on the VM classpath).
    pub framework_res_apk: PathBuf,
    /// The framework's JNI native backends dir (on `java.library.path`).
    pub natives_dir: PathBuf,
}

/// Locate the vendored Android framework (`ECLIPSE_ANDROID_FRAMEWORK_DIR` override, else default).
///
/// Returns [`RuntimeError::FrameworkNotFound`] if `api-impl.jar` is absent (detect-don't-assume,
/// §9).
///
/// 2026-06-04: this `api-impl.jar` is ATL's framework and is **GTK-coupled** — its
/// Activity/View/Surface backends and `createApplication`/`createMainActivity` take a
/// `GtkWidget*` window handle. It is sufficient to put the app's dex + the framework on the
/// classpath so ART loads Roblox's Java (verified: `FindClass` resolves `com.roblox.*`), but
/// driving the Activity to `onCreate` *through it* pulls in GTK. Eclipse's own winit/Vulkan
/// framework is the production replacement (component-map F) — see `docs/art-and-runtime.md`.
pub fn find_framework() -> Result<FrameworkPaths, RuntimeError> {
    let dir = env_path("ECLIPSE_ANDROID_FRAMEWORK_DIR")
        .unwrap_or_else(|| PathBuf::from(FRAMEWORK_DIR_DEFAULT));
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

/// Resolve the XDG cache directory the app's extracted native libs (`libroblox.so`, …) live in.
///
/// 2026-06-04: mirrors [`Config::config_path`](crate::config::Config)'s portable `directories`
/// pattern — `$XDG_CACHE_HOME/eclipse/native-libs` (`~/.cache/eclipse/native-libs` by default),
/// never a hardcoded `/tmp`/`/home`/username path (§9, CLAUDE.md "Build & Environment
/// Portability"). Overridable via `ECLIPSE_NATIVE_LIB_DIR` for distros/layouts whose cache base
/// differs (detect-don't-assume). Returns [`RuntimeError::NoCacheDir`] only when no home/cache
/// base can be determined (e.g. `$HOME` unset) — an actionable failure, not a silent fallback.
pub fn native_lib_cache_dir() -> Result<PathBuf, RuntimeError> {
    if let Some(dir) = env_path("ECLIPSE_NATIVE_LIB_DIR") {
        return Ok(dir);
    }
    let dirs = ProjectDirs::from("", "", "eclipse").ok_or(RuntimeError::NoCacheDir)?;
    Ok(dirs.cache_dir().join("native-libs"))
}

/// Build the `-Djava.class.path=` value: `api-impl.jar : APK : framework-res.apk` (ART's
/// classpath separator is `:`). Mirrors ATL's `create_vm` so the app's dex and the framework
/// both load into the VM.
fn class_path_option(fw: &FrameworkPaths, apk: &Path) -> String {
    format!(
        "-Djava.class.path={}:{}:{}",
        fw.api_impl_jar.display(),
        apk.display(),
        fw.framework_res_apk.display()
    )
}

/// Build the `-Djava.library.path=` value: the framework natives dir, then (when the app's
/// native libs have been extracted) the app-lib dir, `:`-joined.
///
/// 2026-06-04: the framework natives dir MUST stay **first** so the framework's own JNI
/// backends resolve as before; the extracted app-lib dir (holding `libroblox.so`, …) is
/// appended **second** so `System.loadLibrary("roblox")` can find the engine. ART's
/// `java.library.path` separator is `:` (the platform path separator on Linux).
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

/// `JNI_CreateJavaVM` as resolved from `libart.so`: `(JavaVM**, void**, void*) -> jint`.
type JniCreateJavaVm = unsafe extern "system" fn(
    *mut *mut jni_sys::JavaVM,
    *mut *mut c_void,
    *mut c_void,
) -> jni_sys::jint;

/// The bionic translation linker's library-path whitelisting entry point, as exported by
/// `libdl_bio.so.0` (a direct `NEEDED` of `libart.so`): `(const char *path, char *delim) -> void`.
///
/// 2026-06-05: `path` is a `:`-delimited list of directories appended to the shim linker's own
/// search path (`apkenv_ldpaths[]`); `delim` is the separator string (`":"`). It copies the parsed
/// directory strings internally, so the input buffers need only outlive the call. The signature is
/// the §4c-confirmed `void dl_parse_library_path(const char *path, char *delim)`. `delim` is
/// declared `*const c_char` (the function only reads it); the symbol's C prototype types it
/// non-const but a read-only `":"` literal is ABI-compatible and the function does not mutate it.
type DlParseLibraryPath = unsafe extern "C" fn(*const c_char, *const c_char);

/// The delimiter the bionic linker expects in a `dl_parse_library_path` path list (and the same
/// separator ART uses for `-Djava.library.path` — see [`library_path_option`]).
const BIONIC_LDPATH_DELIM: &str = ":";

/// Compose the `:`-delimited directory list whitelisted into the bionic shim linker's search path.
///
/// Mirrors [`library_path_option`]'s ordering exactly: the framework natives dir **first**, then
/// (when the app's native libs have been extracted) the app-lib dir **second**. Keeping the two
/// orderings identical means the bionic linker and ART's `java.library.path` agree on precedence,
/// so a `System.loadLibrary` name resolves to the same `.so` regardless of which layer locates it.
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

/// Whitelist the framework natives dir + the extracted app-lib dir in the bionic shim linker's
/// own library search path, so `System.loadLibrary` can resolve the extracted Roblox native libs
/// (`libzstd-jni-*.so`, …).
///
/// 2026-06-05 — root cause this fixes: ART boots with `-Djava.library.path` set (so the JVM hands
/// the shim linker the **absolute** path of the extracted `.so`), but the apkenv/bionic shim linker
/// has its **own** search-path array (`apkenv_ldpaths[]`) that it consults in `apkenv_load_library`;
/// a directory not present there is rejected as "library not found" even when the file exists at the
/// absolute path the JVM passed (observed: `libzstd-jni-1.5.7-6.so` extracted to the cache dir, 726 KB,
/// yet "not found"). The libdl_bio function `dl_parse_library_path` populates that array. Stock ATL
/// calls it before any `System.loadLibrary`; Eclipse must do the same.
///
/// The symbol is resolved from the **process-global scope**: [`boot`] dlopens `libart.so` with
/// `RTLD_GLOBAL`, which promotes its direct `NEEDED libdl_bio.so.0` (and that lib's exported
/// `dl_parse_library_path`) into the global scope, so `dlopen(NULL, RTLD_NOW|RTLD_GLOBAL)` — the
/// handle [`libloading::os::unix::Library::open(None, …)`] returns — can `dlsym` it. Therefore this
/// MUST be called **after** [`boot`] (so libart, hence libdl_bio, is loaded global) and **before** the
/// framework lifecycle drives any `System.loadLibrary`.
///
/// Returns [`RuntimeError::ResolveDlParse`] if the symbol is absent (e.g. libart was not opened
/// `RTLD_GLOBAL`, or this build of libdl_bio lacks it) — a clear typed failure, never a silent skip
/// (a silent skip would re-surface as the misleading "library not found" downstream).
pub fn whitelist_bionic_library_path(
    fw: &FrameworkPaths,
    app_lib_dir: Option<&Path>,
) -> Result<(), RuntimeError> {
    let path = bionic_library_path(fw, app_lib_dir);
    // Keep both CStrings alive across the call. dl_parse_library_path copies the parsed paths, but
    // we hold these regardless so no dangling pointer can be passed (soundness, not just reliance on
    // the callee's copy semantics).
    let path_c = make_cstring(path)?;
    let delim_c = make_cstring(BIONIC_LDPATH_DELIM.to_owned())?;

    // SAFETY: `dlopen(NULL, RTLD_NOW|RTLD_GLOBAL)` returns a handle that searches the process-global
    // scope; libart was opened RTLD_GLOBAL by `boot()`, so its NEEDED `libdl_bio.so.0` and the
    // `dl_parse_library_path` symbol it exports are resolvable here. libloading upholds the handle
    // invariants; the handle is dropped at end of scope (closing only this NULL reference, never
    // unmapping any library).
    let global = unsafe { libloading::os::unix::Library::open(None::<&Path>, LIBART_DLOPEN_FLAGS) }
        .map_err(RuntimeError::OpenGlobalScope)?;
    // SAFETY: the symbol has the `dl_parse_library_path` C ABI (verified exported by libdl_bio.so.0);
    // we cast to the matching `DlParseLibraryPath` fn type.
    let parse: libloading::os::unix::Symbol<DlParseLibraryPath> =
        unsafe { global.get(b"dl_parse_library_path\0") }.map_err(RuntimeError::ResolveDlParse)?;

    // SAFETY: `parse` is libdl_bio's `dl_parse_library_path`; `path_c`/`delim_c` are valid,
    // NUL-terminated C strings live for this call. The callee reads (and copies) both; it does not
    // retain the pointers past return.
    unsafe { parse(path_c.as_ptr(), delim_c.as_ptr()) };
    Ok(())
}

/// A bare Android soname the bionic shim linker resolves, paired with the host's *versioned*
/// ELF candidate filenames (most-current first) that actually provide that ABI.
///
/// 2026-06-05: the bionic shim linker (`linker.c`) resolves a `NEEDED` entry by searching its
/// `apkenv_ldpaths[]` for a file *named exactly the bare soname* (`libm.so`) and mmap-parsing it
/// as ELF. Android ships bare `.so` sonames; the host glibc ships *versioned* ones (`libm.so.6`),
/// and its bare `/usr/lib/libm.so` is a GNU **ld linker script** (ASCII text — `GROUP(libm.so.6 …)`),
/// not an ELF object, so the bionic linker cannot load it → "library 'libm.so' not found". Eclipse
/// must therefore put a real-ELF file named `libm.so` (a symlink to the host `libm.so.6`) on the
/// bionic search path — the same Android-soname → host-provider mapping that
/// `/usr/share/bionic_translation/cfg.d` performs for `libEGL.so → libEGL.so.1` etc., but for the
/// sonames cfg.d omits, done Eclipse-owned + portable instead of editing the system config.
struct BareSoname {
    /// The bare Android soname the engine's libs `NEEDED` (e.g. `"libm.so"`).
    soname: &'static str,
    /// Host versioned ELF filenames providing that ABI, most-current first (e.g. `"libm.so.6"`).
    /// Multiple entries allow a graceful match if a distro carries a different soversion.
    host_candidates: &'static [&'static str],
}

/// The bare host sonames Eclipse provisions onto the bionic search path as symlinks to a host
/// versioned ELF (the [`BareSoname`] → host-provider mechanism).
///
/// 2026-06-05: **currently empty.** `libm.so` USED to be here (symlinked to the host glibc
/// `libm.so.6`), but that was the *root cause* of the `androidx.startup`/zstd-jni SIGSEGV: the host
/// `libm.so.6` carries an `R_X86_64_TPOFF64` (modern TLS reloc — apkenv's "unknown reloc type 18") +
/// a `.relr.dyn` packed-reloc section the older apkenv shim linker cannot apply, so following
/// zstd-jni's `NEEDED libm.so` to it aborted the load. `libm.so` is now provided by Eclipse's own
/// **apkenv-loadable** shim ([`provision_eclipse_libm`]) — a clean-relocation, correct-math cdylib —
/// NOT a host symlink. `libc.so`/`libdl.so` still resolve via cfg.d / the shim linker's self-provide
/// (deliberately not listed). The table + [`find_host_lib`]/[`is_real_elf`] machinery stays as the
/// general mechanism for any FUTURE bare soname that genuinely IS satisfiable by a host versioned
/// ELF the apkenv linker can load (provision only what is genuinely needed, AGENTS.md "Simplicity
/// First").
const BIONIC_BARE_SONAMES: &[BareSoname] = &[];

/// The bare Android soname Eclipse provides via its own apkenv-loadable shim (not a host symlink).
const ECLIPSE_LIBM_SONAME: &str = "libm.so";

/// The Eclipse apkenv-loadable `libm` shim `.so`, built by `build.rs` from `crates/libm-shim` and its
/// absolute path baked in at compile time via `cargo:rustc-env=ECLIPSE_LIBM_SHIM_SO`.
///
/// 2026-06-05: this is a standalone `#![no_std]` cdylib re-exporting the pure-Rust `libm` crate's
/// CORRECT math under the C libm symbol names, with ONLY `R_X86_64_{64,GLOB_DAT,RELATIVE}` relocs
/// (no `R_X86_64_TPOFF64`, no RELR, no `NEEDED`, no PT_TLS) — so the apkenv shim linker CAN load it as
/// the app's `libm.so`. See `crates/libm-shim/src/lib.rs` and `build.rs::build_libm_shim`.
const ECLIPSE_LIBM_SHIM_SO: &str = env!("ECLIPSE_LIBM_SHIM_SO");

/// Standard host directories searched for a versioned ELF provider when `cc -print-file-name`
/// cannot resolve it (no compiler installed).
///
/// 2026-06-05: detect-don't-assume (AGENTS.md §9) — these are the conventional glibc lib dirs across
/// distros (multilib `/usr/lib64`, Debian/Ubuntu multiarch, classic `/lib`); the list is a *fallback*
/// scanned only if the portable `cc -print-file-name` route fails, and each candidate is verified to
/// be a real ELF (not a linker script) before use. Not a single hardcoded path — the first that
/// holds a real ELF wins.
const HOST_LIB_DIRS: &[&str] = &[
    "/usr/lib",
    "/lib",
    "/usr/lib64",
    "/lib64",
    "/usr/lib/x86_64-linux-gnu",
    "/lib/x86_64-linux-gnu",
];

/// Provision the bare host sonames the bionic shim linker needs ([`BIONIC_BARE_SONAMES`]) as
/// symlinks into `dir`, which must be on the bionic search path (see
/// [`whitelist_bionic_library_path`]).
///
/// For each bare soname (`libm.so`) this resolves the host's *real-ELF* versioned provider
/// (`libm.so.6`) portably ([`find_host_lib`]) and creates/refreshes a symlink `dir/<soname> →
/// <host provider>`. Idempotent: an existing correct symlink is left as-is; a stale/wrong link (or
/// any other file at that name) is replaced. So the bionic linker, searching `dir` for `libm.so`,
/// finds a real ELF and loads the host math lib — clearing `library 'libm.so' not found`.
///
/// Returns [`RuntimeError::HostLibNotFound`] if a required host provider is genuinely absent (no
/// `cc -print-file-name` match and none of [`HOST_LIB_DIRS`] holds a real ELF) — an actionable
/// typed failure naming what to install, never a silent skip (a skip would re-surface as the
/// misleading bionic "library not found"). Must run **before** the lifecycle drives any
/// `System.loadLibrary` that pulls a lib `NEEDED`-ing these sonames.
pub fn provision_bionic_sonames(dir: &Path) -> Result<(), RuntimeError> {
    std::fs::create_dir_all(dir).map_err(|e| RuntimeError::ProvisionSoname(dir.to_owned(), e))?;
    // `libm.so` is provided by Eclipse's own apkenv-loadable shim, NOT a host symlink (the host glibc
    // `libm.so.6` has modern relocs the apkenv linker cannot apply — the original SIGSEGV root cause).
    provision_eclipse_libm(dir)?;
    // Any FUTURE bare soname genuinely satisfiable by a host versioned ELF the apkenv linker can load
    // (currently none — see BIONIC_BARE_SONAMES) is symlinked here.
    for entry in BIONIC_BARE_SONAMES {
        let target = find_host_lib(entry)?;
        let link = dir.join(entry.soname);
        symlink_idempotent(&target, &link)?;
    }
    Ok(())
}

/// Provision the app's `libm.so` from Eclipse's apkenv-loadable shim ([`ECLIPSE_LIBM_SHIM_SO`]) by
/// **copying** it to `dir/libm.so`.
///
/// 2026-06-05 — root cause this fixes: the apkenv / `bionic_translation` shim linker resolves an app
/// lib's `NEEDED libm.so` by mmap-parsing a file named exactly `libm.so` on its search path. The host
/// glibc `libm.so.6` (which Eclipse previously symlinked there) carries an `R_X86_64_TPOFF64` (modern
/// TLS reloc — apkenv's "unknown reloc type 18") + a `.relr.dyn` packed-reloc section the older apkenv
/// linker cannot apply (and `NEEDED ld-linux-x86-64.so.2`), so loading it aborts (SIGSEGV) during
/// Roblox's `androidx.startup` `System.loadLibrary("zstd-jni")` (zstd-jni `NEEDED libm.so`). The
/// Eclipse shim is a clean-relocation ELF (`R_X86_64_{64,GLOB_DAT,RELATIVE}` only, no TLS, no
/// `NEEDED`) the apkenv linker CAN load, with CORRECT math (the pure-Rust `libm` crate).
///
/// A **copy** (not a symlink to the build-artifact path) keeps the provisioned `libm.so` valid after a
/// `cargo clean` removes `target/`, and avoids leaking the build machine's `target/` path into the
/// runtime search dir. Idempotent: an up-to-date copy (matching size) is left as-is; otherwise the
/// shim is (re)copied, replacing any stale file or wrong-target symlink at that name.
///
/// Returns [`RuntimeError::HostLibNotFound`] (soname `"libm.so"`) if the shim artifact is missing
/// (e.g. the source tree was moved after build without rebuilding) — an actionable typed failure, not
/// a silent skip that would re-surface as the apkenv "library not found"/abort.
fn provision_eclipse_libm(dir: &Path) -> Result<(), RuntimeError> {
    let shim = Path::new(ECLIPSE_LIBM_SHIM_SO);
    let link = dir.join(ECLIPSE_LIBM_SONAME);
    let shim_len = match std::fs::metadata(shim) {
        Ok(m) => m.len(),
        // The build-time-baked shim path no longer exists (tree moved without rebuild). Surface it.
        Err(_) => {
            return Err(RuntimeError::HostLibNotFound {
                soname: ECLIPSE_LIBM_SONAME,
                candidates: &[],
            })
        }
    };
    // Idempotent fast path: an existing regular-file copy of the right size is treated as up-to-date.
    // (A symlink at `link` returns its target's metadata via `metadata`; we only short-circuit for a
    // real file via `symlink_metadata` to avoid mistaking a stale symlink for a good copy.)
    if let Ok(meta) = std::fs::symlink_metadata(&link) {
        if meta.file_type().is_file() && meta.len() == shim_len {
            return Ok(());
        }
        // Stale file or a (wrong) symlink — remove before copying.
        std::fs::remove_file(&link).map_err(|e| RuntimeError::ProvisionSoname(link.clone(), e))?;
    }
    std::fs::copy(shim, &link).map_err(|e| RuntimeError::ProvisionSoname(link.clone(), e))?;
    Ok(())
}

/// Resolve the host's real-ELF provider for a bare soname, trying `cc -print-file-name` first
/// (the canonical, portable compiler-driver resolution) then scanning [`HOST_LIB_DIRS`].
///
/// Only a path that is a *real ELF object* is accepted: the host's bare `libm.so` is an ld linker
/// script (ASCII), which the bionic linker cannot parse — accepting it would not fix the failure.
/// `cc -print-file-name=<name>` echoes the input unchanged when it finds nothing, so its result is
/// validated by [`is_real_elf`] like any other candidate.
fn find_host_lib(entry: &BareSoname) -> Result<PathBuf, RuntimeError> {
    for candidate in entry.host_candidates {
        // Portable route: the compiler driver knows the real lib dir for this host/triple.
        if let Some(p) = cc_print_file_name(candidate) {
            if is_real_elf(&p) {
                return Ok(p);
            }
        }
        // Fallback: scan the conventional glibc lib dirs; first real ELF wins.
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

/// Ask the C compiler driver for the absolute path of a library filename
/// (`cc -print-file-name=libm.so.6`), canonicalized. `None` if no compiler is present, the command
/// fails, or the driver echoed the name unchanged (its "not found" behavior — a bare filename with
/// no directory, which `canonicalize` then rejects).
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
    // The driver echoes the input unchanged when it finds nothing (no directory component).
    if reported.parent().is_none_or(|p| p.as_os_str().is_empty()) {
        return None;
    }
    std::fs::canonicalize(reported).ok()
}

/// Whether `path` is a regular file beginning with the ELF magic (`\x7fELF`).
///
/// Excludes the GNU ld *linker scripts* glibc installs as bare `.so` names (e.g. `/usr/lib/libm.so`
/// = `GROUP(libm.so.6 …)` ASCII text): they are not ELF, so the bionic linker cannot load them, and
/// symlinking to one would not fix the failure. A short read is sufficient and cannot panic.
fn is_real_elf(path: &Path) -> bool {
    use std::io::Read;
    let Ok(mut f) = std::fs::File::open(path) else {
        return false;
    };
    let mut magic = [0u8; 4];
    f.read_exact(&mut magic).is_ok() && magic == *b"\x7fELF"
}

/// Create (or refresh) a symlink `link -> target`, idempotently.
///
/// If `link` already resolves to `target`, it is left untouched. Otherwise any existing entry at
/// `link` (a stale symlink, or any other file) is removed and a fresh symlink is created. The
/// "already correct" fast path keeps repeat boots cheap and avoids needless churn.
fn symlink_idempotent(target: &Path, link: &Path) -> Result<(), RuntimeError> {
    if let Ok(existing) = std::fs::read_link(link) {
        if existing == target {
            return Ok(());
        }
    }
    // Replace whatever is there (stale link / wrong target / regular file). `remove_file` removes
    // a symlink without following it; ignore "not present" so this stays idempotent.
    match std::fs::remove_file(link) {
        Ok(()) => {}
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => {}
        Err(e) => return Err(RuntimeError::ProvisionSoname(link.to_owned(), e)),
    }
    std::os::unix::fs::symlink(target, link)
        .map_err(|e| RuntimeError::ProvisionSoname(link.to_owned(), e))
}

/// dlopen flags for `libart.so` (see [`boot`]).
///
/// 2026-06-05: `RTLD_GLOBAL` is **required** (not the RTLD_LOCAL default of the cross-platform
/// `libloading::Library::new`): it promotes libart's symbols and its NEEDED `liblog.so`
/// (`__android_log_print`) into the process-global scope, so the JCA/WolfSSL provider that
/// `Context.<clinit>` loads during APK signature verification (`System.loadLibrary("wolfssljni")`)
/// resolves that symbol via the bionic shim's glibc-dlopen fallback. `RTLD_NOW` binds eagerly so a
/// missing symbol surfaces at load, not mid-lifecycle.
const LIBART_DLOPEN_FLAGS: std::os::raw::c_int =
    libloading::os::unix::RTLD_NOW | libloading::os::unix::RTLD_GLOBAL;

/// An owned handle to the live ART VM this process booted via [`boot`].
///
/// 2026-06-04: carries the raw `*mut jni_sys::JavaVM` that `JNI_CreateJavaVM` produced. The raw
/// pointer field alone makes `Vm` auto-`!Send` + `!Sync` (it is deliberately **not** marked
/// `unsafe impl Send`/`Sync`), which pins the VM to the thread that booted it — Eclipse's main
/// thread. That is the encoded thread/loop-ownership model: ART boots on the process main thread,
/// winit's event loop runs on that **same** main thread, and future JNI calls (e.g. `onCreate`)
/// happen from inside the event-loop callbacks on this already-attached main thread — never an
/// `AttachCurrentThread`/cross-thread `JNIEnv`. Holding the handle alive keeps a reachable VM on
/// the main thread for those later calls.
///
/// The libart mapping itself is kept resident for the process lifetime by [`boot`]'s
/// `Library::into_raw()` leak (see [`boot`]'s docs), so `Vm` does not own the `libloading::Library`
/// and has **no** `Drop`: tearing the VM down (`DestroyJavaVM` + unload) is a separately-designed
/// later increment, because a running VM's GC/JIT daemon threads still execute libart's code.
///
/// # Regression guard: `Vm` must stay `!Send` + `!Sync`
///
/// These doctests fail to compile iff someone makes `Vm` `Send`/`Sync` (e.g. by adding an
/// `unsafe impl Send for Vm`), which would break the main-thread pinning invariant above. They
/// pass today (compile error ⇒ `compile_fail` ⇒ test passes) because the raw-pointer field keeps
/// `Vm` auto-`!Send`/`!Sync`. Dependency-free (only `std` + the public `Vm` type).
///
/// ```compile_fail
/// fn assert_send<T: Send>() {}
/// assert_send::<eclipse::runtime::Vm>();
/// ```
///
/// ```compile_fail
/// fn assert_sync<T: Sync>() {}
/// assert_sync::<eclipse::runtime::Vm>();
/// ```
pub struct Vm {
    /// The `JavaVM` pointer returned by `JNI_CreateJavaVM`. The raw-pointer field makes `Vm`
    /// auto-`!Send`/`!Sync`, pinning it to the booting (main) thread. Read via [`Vm::as_raw`] by
    /// the framework lifecycle driver's main-thread JNI calls.
    vm: *mut jni_sys::JavaVM,
}

impl Vm {
    /// The live `*mut JavaVM` this process booted, for wrapping with the `jni` crate
    /// (`jni::vm::JavaVM::from_raw`) on this (main) thread.
    ///
    /// 2026-06-04: returns the raw pointer rather than the `jni` wrapper so `runtime` keeps no
    /// `jni`-crate dependency in its public API (the framework driver in [`crate::framework`]
    /// owns that). The pointer is non-null (verified by [`boot`]'s `NullEnv` check) and valid for
    /// the process lifetime (the VM is never destroyed — see [`boot`]'s never-unload note); `&self`
    /// borrows the `!Send`/`!Sync` `Vm`, so callers stay on the VM's main thread.
    #[must_use]
    pub fn as_raw(&self) -> *mut jni_sys::JavaVM {
        self.vm
    }
}

/// Boot the vendored ART VM for the given plan, optionally with an app on the classpath.
///
/// `dlopen`s the vendored `libart.so` and calls `JNI_CreateJavaVM` with the discovered boot
/// image + the plan's [`vm_options`](BootPlan::vm_options), succeeding when ART returns
/// `JNI_OK` with a live `JavaVM` + `JNIEnv`. ART completes all initialization — boot-image
/// load, libcore native backends, runtime threads, attaching this thread — before returning
/// `JNI_OK`, so that status is definitive proof the VM booted.
///
/// - `apk_path = None`: a **libcore-only** smoke boot — the decisive Step 3.5 thesis test (a
///   graphics-free process boots ART past the low_4gb wall; see the module docs).
/// - `apk_path = Some(apk)`: also put the **app on the classpath** — `api-impl.jar : APK :
///   framework-res.apk` ([`find_framework`]) plus the framework `java.library.path` — so ART
///   loads Roblox's Java (verified 2026-06-04: `FindClass` resolves `com.roblox.*`). This is
///   the foundation for reaching `onCreate`; driving the Activity itself needs the framework
///   (currently GTK-coupled — see [`find_framework`]).
///
/// `app_lib_dir` is the directory the app's native libs were extracted to
/// (see [`crate::apk::Apk::extract_native_libs`] + [`native_lib_cache_dir`]); when `Some`, it is
/// appended **after** the framework natives dir on `java.library.path` so
/// `System.loadLibrary("roblox")` resolves `libroblox.so`. It is only meaningful alongside
/// `apk_path`; passing it with `apk_path = None` has no effect (no classpath/library.path is set).
///
/// On success returns an owned [`Vm`] handle carrying the live `JavaVM` pointer. The handle is
/// `!Send`/`!Sync`, so it stays on the booting (main) thread — keep it alive (e.g. across the
/// winit event loop) to give the next increment's JNI calls a reachable VM on that thread.
///
/// 2026-06-04 (updated 2026-06-05): `libart.so` is intentionally **never unloaded** — the handle
/// is leaked via `Library::into_raw()` below: a running ART VM has daemon threads (GC/JIT)
/// executing libart's code, so unmapping it (which the `Library`'s `Drop` would do) is undefined
/// behavior — even at process exit, since those daemon threads are still alive when destructors
/// run. The mapping therefore lives for the process; the returned [`Vm`] deliberately does **not**
/// hold the `Library` and has no `Drop`. Proper `DestroyJavaVM`-then-unload teardown is a
/// separately-designed future increment. Consequently only one VM can be created per process
/// (`JNI_CreateJavaVM` returns an error on a second call).
///
/// 2026-06-05: libart is opened with `RTLD_NOW | RTLD_GLOBAL` (see the `boot` body) so libart's
/// symbols and its NEEDED `liblog.so` (`__android_log_print`) join the process-global scope — the
/// JCA/WolfSSL provider `Context.<clinit>` loads during APK signature verification depends on it.
pub fn boot(
    plan: &BootPlan,
    apk_path: Option<&Path>,
    app_lib_dir: Option<&Path>,
) -> Result<Vm, RuntimeError> {
    let libart = find_libart()?;
    let boot_image = find_boot_image()?;

    // Build the option strings; keep the CStrings alive across the JNI_CreateJavaVM call (the
    // JavaVMOption array holds borrowed pointers into them).
    let mut option_strings: Vec<CString> = Vec::new();
    option_strings.push(make_cstring(format!("-Ximage:{}", boot_image.display()))?);
    for opt in plan.vm_options() {
        option_strings.push(make_cstring(opt)?);
    }
    // With an APK, add the app + framework to the classpath (and the framework natives to
    // java.library.path) so Roblox's Java and the android.* framework load into the VM.
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
        // Ignore options ART does not recognize rather than fail the whole boot.
        ignoreUnrecognized: jni_sys::JNI_TRUE,
    };

    // 2026-06-05: dlopen libart with RTLD_NOW|RTLD_GLOBAL (not the cross-platform `Library::new`,
    // which is RTLD_LOCAL). RTLD_GLOBAL promotes libart AND its NEEDED deps (incl. `liblog.so`,
    // which libart links via its `${ORIGIN}` RPATH) into the process-global symbol scope. This
    // matches a direct-linked ATL executable, where those symbols are global. It is REQUIRED for
    // the lifecycle: `Context.<clinit>` verifies the APK signature (`PackageParser.collectCertificates`),
    // which loads the WolfSSL JCA provider via `System.loadLibrary("wolfssljni")`. `libwolfssljni.so`
    // leaves `__android_log_print` undefined (no `liblog.so` in its DT_NEEDED — it expects the symbol
    // already global). With RTLD_LOCAL the bionic shim's glibc-dlopen fallback failed with
    // "undefined symbol: __android_log_print", so `<clinit>` died with an UnsatisfiedLinkError
    // (an Error, not caught by its `catch (Exception)`), marking `Context` erroneous → step 1's
    // `GetStaticMethodID(Context.createApplication)` returned NULL. RTLD_GLOBAL makes the symbol
    // resolvable and unblocks the load (evidence: stock ATL loads the same lib "with glibc dlopen").
    let create: JniCreateJavaVm = {
        // SAFETY: dlopen of a path we verified exists; libloading upholds the handle invariants.
        // RTLD_NOW resolves all relocations eagerly (surfacing any missing symbol now, not later);
        // RTLD_GLOBAL adds libart's symbols + its NEEDED deps to the global scope (see above).
        let lib =
            unsafe { libloading::os::unix::Library::open(Some(&libart), LIBART_DLOPEN_FLAGS) }
                .map_err(RuntimeError::LoadLibart)?;
        // Resolve JNI_CreateJavaVM and copy out the (Copy) fn pointer, then leak the handle: a
        // running ART VM has daemon threads executing libart's code, so unmapping libart (which
        // `Library`'s Drop would do) is UB — even at process exit. Same rationale as the prior
        // `mem::forget(lib)`; `into_raw()` is the os::unix equivalent (consumes the handle without
        // closing it). RTLD_GLOBAL must persist for the whole process so later `System.loadLibrary`
        // loads keep resolving against libart's deps.
        // SAFETY: the symbol has the JNI_CreateJavaVM ABI; we cast to the matching fn type.
        let sym: libloading::os::unix::Symbol<JniCreateJavaVm> =
            unsafe { lib.get(b"JNI_CreateJavaVM\0") }.map_err(RuntimeError::ResolveSymbol)?;
        let create = *sym;
        lib.into_raw();
        create
    };

    let mut vm: *mut jni_sys::JavaVM = std::ptr::null_mut();
    let mut env: *mut c_void = std::ptr::null_mut();
    // SAFETY: `create` is libart's JNI_CreateJavaVM; `args`/`options`/`option_strings` are live
    // for the call; `vm`/`env` are valid out-pointers. libart stays mapped (leaked via `into_raw`).
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

    // libart is already kept mapped for the VM's lifetime (leaked via `into_raw()` above; its
    // daemon threads reference its code, so unmapping it would be UB).

    // 2026-06-12 (core 866509): ART's `Thread::Init` just registered a 32 KiB glibc-HEAP buffer
    // (vendored `thread_linux.cc` `SetUpAlternateSignalStack`, `new uint8_t[]`, no guard page) as
    // this main thread's alternate signal stack; the fatal-signal handler chain (Eclipse tap →
    // libsigchain → ART's unexpected-signal dump) measured ~79.2 KiB and overflowed it, silently
    // zero-filling live heap below `ss_sp` ("malloc(): unaligned tcache chunk detected" SIGABRT
    // mid-crash-report). Replace it NOW — after `JNI_CreateJavaVM`, so ART cannot overwrite it
    // again, and safe from ART's `TearDownAlternateSignalStack` `delete[]` of the current `ss_sp`
    // because Eclipse never destroys the VM or detaches this thread (`Vm` has no `Drop`). The
    // displaced 32 KiB ART buffer leaks once by design (freeing a foreign `operator new[]`
    // allocation would be unsound). Non-fatal on failure: the boot proceeds on ART's stack — the
    // pre-fix state, losing only the overflow protection.
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

    // JNI_OK with a live JavaVM + JNIEnv is definitive: ART loaded the boot image and libcore's
    // native backends and attached this thread before returning. Return the owned `Vm` handle so
    // the caller keeps the VM reachable on this (main) thread for the next increment's JNI calls;
    // driving the env (calls, classes, the Activity) is that next step — see the module docs.
    Ok(Vm { vm })
}

/// Build a `CString` from an ART option, mapping an embedded NUL to a typed error.
fn make_cstring(s: String) -> Result<CString, RuntimeError> {
    CString::new(s).map_err(|_| RuntimeError::OptionHasNul)
}

/// Errors from the runtime subsystem.
#[derive(Debug)]
pub enum RuntimeError {
    /// The vendored `libart.so` was not found at the searched path.
    LibartNotFound(PathBuf),
    /// The ART boot-image location's directory was not found.
    BootImageNotFound(PathBuf),
    /// The vendored Android framework (`api-impl.jar`) was not found.
    FrameworkNotFound(PathBuf),
    /// No home/cache base directory could be determined for the native-lib cache dir.
    NoCacheDir,
    /// `dlopen` of `libart.so` failed.
    LoadLibart(libloading::Error),
    /// Resolving the `JNI_CreateJavaVM` symbol failed.
    ResolveSymbol(libloading::Error),
    /// `dlopen(NULL, …)` to obtain a global-scope handle for `dl_parse_library_path` failed.
    OpenGlobalScope(libloading::Error),
    /// Resolving the bionic `dl_parse_library_path` symbol from the global scope failed (libart
    /// not opened `RTLD_GLOBAL`, or libdl_bio lacks the symbol).
    ResolveDlParse(libloading::Error),
    /// A required host library providing a bare Android soname was not found (no
    /// `cc -print-file-name` match and none of the standard host lib dirs holds a real ELF).
    HostLibNotFound {
        /// The bare Android soname that could not be satisfied (e.g. `"libm.so"`).
        soname: &'static str,
        /// The host versioned ELF filenames that were searched for (e.g. `["libm.so.6"]`).
        candidates: &'static [&'static str],
    },
    /// Provisioning a bare-soname symlink onto the bionic search path failed (create-dir, remove,
    /// or symlink I/O error at the given path).
    ProvisionSoname(PathBuf, std::io::Error),
    /// An ART option string contained an interior NUL byte.
    OptionHasNul,
    /// `JNI_CreateJavaVM` returned a non-`JNI_OK` status code.
    CreateVm(jni_sys::jint),
    /// `JNI_CreateJavaVM` reported success but returned a null `JavaVM` or `JNIEnv`.
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
        // All-absent must equal the conservative baseline ATL hardcoded (M0 Step 4).
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
        // The split's contract: VM options carry the heap flags and NEVER the dex2oat ISA flag
        // (passing --instruction-set-features to JNI_CreateJavaVM would be wrong).
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
        // 2026-06-05 regression guard: the createApplication frontier was blocked because libart was
        // dlopen'd RTLD_LOCAL (libloading's `Library::new` default), so its NEEDED `liblog.so`
        // (`__android_log_print`) was not global → the WolfSSL provider `Context.<clinit>` loads
        // failed to link → `Context` erroneous → `GetStaticMethodID(createApplication)` == NULL.
        // RTLD_GLOBAL is the fix; pin it (and eager RTLD_NOW) so a revert re-breaks this test.
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
        // Point the override at a path that does not exist → typed LibartNotFound (never panic).
        // SAFETY: single-threaded test; we set then remove the env var around the call.
        unsafe { std::env::set_var("ECLIPSE_LIBART", "/nonexistent/eclipse/libart.so") };
        let r = find_libart();
        unsafe { std::env::remove_var("ECLIPSE_LIBART") };
        assert!(matches!(r, Err(RuntimeError::LibartNotFound(_))), "{r:?}");
    }

    #[test]
    fn find_framework_reports_typed_error_for_missing_override() {
        // Override the framework dir to one with no api-impl.jar → typed FrameworkNotFound.
        // SAFETY: single-threaded test; set then remove the env var around the call.
        unsafe { std::env::set_var("ECLIPSE_ANDROID_FRAMEWORK_DIR", "/nonexistent/eclipse/fw") };
        let r = find_framework();
        unsafe { std::env::remove_var("ECLIPSE_ANDROID_FRAMEWORK_DIR") };
        assert!(
            matches!(r, Err(RuntimeError::FrameworkNotFound(_))),
            "{r:?}"
        );
    }

    #[test]
    fn class_path_option_orders_framework_apk_and_res() {
        // ATL's order: api-impl.jar : APK : framework-res.apk, ':'-separated, under -Djava.class.path.
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
        // No extracted app libs yet: just the framework natives dir.
        assert_eq!(
            library_path_option(&fw, None),
            "-Djava.library.path=/fw/natives"
        );
    }

    #[test]
    fn library_path_option_framework_first_then_app_lib_colon_joined() {
        // The exact invariant System.loadLibrary("roblox") depends on: the framework natives dir
        // stays FIRST, the extracted app-lib dir (holding libroblox.so) is appended SECOND, and
        // the two are ':'-joined under -Djava.library.path. Machine-independent (PathBufs only).
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
        // Guard the ordering + separator explicitly so a regression (app-lib first, or wrong
        // separator) fails loudly rather than silently breaking engine resolution.
        let value = opt.strip_prefix("-Djava.library.path=").expect("prefix");
        let parts: Vec<&str> = value.split(':').collect();
        assert_eq!(parts, vec!["/fw/natives", "/cache/eclipse/native-libs"]);
    }

    #[test]
    fn bionic_library_path_framework_first_then_app_lib_colon_joined() {
        // 2026-06-05 regression guard: the bionic shim linker's whitelist (dl_parse_library_path)
        // MUST receive the framework natives dir FIRST then the extracted app-lib dir, ':'-joined —
        // identical ordering/separator to `library_path_option` so the two layers agree on which
        // .so a `System.loadLibrary` name resolves to. A reorder or wrong delimiter re-surfaces as
        // the misleading bionic "library not found". Machine-independent (PathBufs only).
        let fw = FrameworkPaths {
            api_impl_jar: PathBuf::from("/fw/api-impl.jar"),
            framework_res_apk: PathBuf::from("/fw/framework-res.apk"),
            natives_dir: PathBuf::from("/fw/natives"),
        };
        let path = bionic_library_path(&fw, Some(Path::new("/cache/eclipse/native-libs")));
        assert_eq!(path, "/fw/natives:/cache/eclipse/native-libs");
        // The delimiter passed to dl_parse_library_path must be exactly the one used to join.
        assert_eq!(BIONIC_LDPATH_DELIM, ":");
        let parts: Vec<&str> = path.split(BIONIC_LDPATH_DELIM).collect();
        assert_eq!(parts, vec!["/fw/natives", "/cache/eclipse/native-libs"]);
        // The bionic whitelist and ART's java.library.path must stay in lockstep: the joined
        // directory list is exactly the java.library.path value minus its `-Djava.library.path=`
        // prefix. If either ordering drifts, this fails.
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
        // 2026-06-05 regression guard for the libm.so root cause: the host's bare `/usr/lib/libm.so`
        // is a GNU ld *linker script* (ASCII text), which the bionic linker cannot parse — accepting
        // it would NOT fix "library 'libm.so' not found". is_real_elf must reject it and accept only a
        // file beginning with the ELF magic. Host-independent (writes its own fixtures to a temp dir).
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
        // 2026-06-05 regression guard: provisioning must be idempotent — create the link, leave a
        // correct one untouched, and replace a stale/wrong one. Host-independent (temp dir + symlinks).
        let dir = std::env::temp_dir().join(format!("eclipse-symlink-test-{}", std::process::id()));
        std::fs::create_dir_all(&dir).expect("mk temp dir");
        let target_a = dir.join("libm.so.6");
        let target_b = dir.join("libm.so.7");
        std::fs::write(&target_a, b"\x7fELFa").expect("write a");
        std::fs::write(&target_b, b"\x7fELFb").expect("write b");
        let link = dir.join("libm.so");

        // Create.
        symlink_idempotent(&target_a, &link).expect("create");
        assert_eq!(std::fs::read_link(&link).expect("readlink"), target_a);

        // Keep (already correct — no error, still points at A).
        symlink_idempotent(&target_a, &link).expect("keep");
        assert_eq!(std::fs::read_link(&link).expect("readlink"), target_a);

        // Replace a stale link pointing at the wrong target.
        symlink_idempotent(&target_b, &link).expect("replace");
        assert_eq!(std::fs::read_link(&link).expect("readlink"), target_b);

        // Replace a non-symlink file occupying the name.
        std::fs::remove_file(&link).ok();
        std::fs::write(&link, b"not a link").expect("write regular file");
        symlink_idempotent(&target_a, &link).expect("replace regular file");
        assert_eq!(std::fs::read_link(&link).expect("readlink"), target_a);

        std::fs::remove_dir_all(&dir).ok();
    }

    #[test]
    fn host_symlinked_sonames_are_wellformed_and_libm_is_not_among_them() {
        // 2026-06-05: `libm.so` must NOT be host-symlinked (that was the SIGSEGV root cause — the host
        // glibc libm.so.6 has modern relocs the apkenv linker cannot apply); it is now provided by the
        // Eclipse apkenv-loadable shim instead. The host-symlink table is for sonames genuinely
        // satisfiable by a host versioned ELF the apkenv linker CAN load — each must be `.so`-named,
        // have ≥1 host candidate, and be unique. Host-independent.
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
    fn eclipse_libm_shim_is_apkenv_loadable_and_provisions_libm_so() {
        // 2026-06-05 regression guard for the zstd-jni/androidx.startup SIGSEGV root cause. The shim
        // `.so` build.rs produced (ECLIPSE_LIBM_SHIM_SO) MUST be a real ELF with NO modern relocs the
        // apkenv linker chokes on (R_X86_64_TPOFF64 = "unknown reloc type 18", or a packed `.relr.dyn`
        // section), and provisioning must place a copy at `<dir>/libm.so`. Host-independent (reads the
        // built artifact + a temp dir).
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

        // Parse the dynamic relocations via Eclipse's own ELF decoder and assert NONE is the modern
        // TLS reloc R_X86_64_TPOFF64 (type 18) — the exact reloc that aborts the apkenv linker.
        let img = crate::loader::elf::ElfImage::parse(&bytes).expect("decode shim ELF");
        for rela in img.relocations().expect("decode shim relocations") {
            assert_ne!(
                rela.r_type,
                crate::loader::reloc::R_X86_64_TPOFF64,
                "the libm shim regressed: an R_X86_64_TPOFF64 reloc would abort the apkenv linker"
            );
        }
        // A packed RELR section is equally unsupported by the older apkenv linker.
        assert!(
            img.relr().expect("decode shim relr").is_empty(),
            "the libm shim must have no RELR (packed) relocations — the apkenv linker cannot apply them"
        );

        // Provisioning copies the shim to `<dir>/libm.so`, idempotently.
        let dir = std::env::temp_dir().join(format!("eclipse-libm-prov-{}", std::process::id()));
        std::fs::create_dir_all(&dir).expect("mk temp dir");
        provision_eclipse_libm(&dir).expect("provision libm shim");
        let provisioned = dir.join("libm.so");
        let copied = std::fs::read(&provisioned).expect("provisioned libm.so must exist");
        assert_eq!(
            copied, bytes,
            "provisioned libm.so must be the shim's bytes"
        );
        // Idempotent second call leaves the up-to-date copy in place.
        provision_eclipse_libm(&dir).expect("provision libm shim (idempotent)");
        assert!(provisioned.is_file());
        std::fs::remove_dir_all(&dir).ok();
    }

    #[test]
    fn eclipse_libm_shim_math_values_are_correct() {
        // 2026-06-05: the shim must return CORRECT math, not stubs — a wrong `sin` would corrupt the
        // engine. dlopen the built shim and check representative symbols (single-arg f64/f32, two-arg,
        // and a pointer-out function) against known values. This is the correct-math half of the
        // regression guard (the reloc/RELR half is the test above). Host-independent (loads the built
        // artifact). `RTLD_LOCAL` (libloading default) keeps the shim's `sin`/`cos`/… from leaking
        // into the process-global scope.
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

            // A pointer-out function: frexp(8.0, &e) -> mantissa 0.5, exponent 4 (8.0 = 0.5 * 2^4).
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

    // The real ART VM boot is intentionally NOT an in-harness test: ART's `JNI_CreateJavaVM`
    // must run on a clean process *main* thread, but the cargo-test harness runs each test on a
    // worker thread, where ART aborts early with a `scoped_thread_state_change` check
    // (`!runtime->IsStarted()`). [`boot`] is therefore validated from the launcher's main thread
    // via `eclipse run <apk>` (the production entry point), mirroring the standalone C probe that
    // confirmed ART boots a libcore VM from a graphics-free process (the Step 3.5 thesis;
    // 2026-06-04). The host-thread-independent logic — discovery and the
    // `vm_options`/`dex2oat_options` split — is covered by the tests above.
}
