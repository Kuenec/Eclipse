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

use std::ffi::{c_void, CString};
use std::fmt;
use std::path::{Path, PathBuf};

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

/// Build the `-Djava.library.path=` value (the framework natives dir). The app's own extracted
/// native libs (`libroblox.so`, …) are added when `System.loadLibrary` support lands.
fn library_path_option(fw: &FrameworkPaths) -> String {
    format!("-Djava.library.path={}", fw.natives_dir.display())
}

/// `JNI_CreateJavaVM` as resolved from `libart.so`: `(JavaVM**, void**, void*) -> jint`.
type JniCreateJavaVm = unsafe extern "system" fn(
    *mut *mut jni_sys::JavaVM,
    *mut *mut c_void,
    *mut c_void,
) -> jni_sys::jint;

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
/// `libart.so` is intentionally **never unloaded**: a running ART VM has daemon threads
/// (GC/JIT) executing libart's code, so unmapping it would be undefined behavior. The VM lives
/// for the process; proper `DestroyJavaVM`-then-unload teardown is future work. Consequently
/// only one VM can be created per process (`JNI_CreateJavaVM` returns an error on a second call).
pub fn boot(plan: &BootPlan, apk_path: Option<&Path>) -> Result<(), RuntimeError> {
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
        option_strings.push(make_cstring(library_path_option(&fw))?);
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

    // SAFETY: dlopen of a path we verified exists; libloading upholds the handle invariants.
    let lib = unsafe { libloading::Library::new(&libart) }.map_err(RuntimeError::LoadLibart)?;

    // Resolve JNI_CreateJavaVM and copy out the (Copy) fn pointer so the Symbol's borrow of
    // `lib` is released before we `forget(lib)` below.
    let create: JniCreateJavaVm = {
        // SAFETY: the symbol has the JNI_CreateJavaVM ABI; we cast to the matching fn type.
        let sym: libloading::Symbol<JniCreateJavaVm> =
            unsafe { lib.get(b"JNI_CreateJavaVM\0") }.map_err(RuntimeError::ResolveSymbol)?;
        *sym
    };

    let mut vm: *mut jni_sys::JavaVM = std::ptr::null_mut();
    let mut env: *mut c_void = std::ptr::null_mut();
    // SAFETY: `create` is libart's JNI_CreateJavaVM; `args`/`options`/`option_strings` are live
    // for the call; `vm`/`env` are valid out-pointers. `lib` is still loaded (not yet forgotten).
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

    // Keep libart mapped for the VM's lifetime (its daemon threads reference its code).
    std::mem::forget(lib);

    // JNI_OK with a live JavaVM + JNIEnv is definitive: ART loaded the boot image and libcore's
    // native backends and attached this thread before returning. Driving the env (calls,
    // classes, the Activity) is the next step — see the module docs.
    Ok(())
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
    /// `dlopen` of `libart.so` failed.
    LoadLibart(libloading::Error),
    /// Resolving the `JNI_CreateJavaVM` symbol failed.
    ResolveSymbol(libloading::Error),
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
            Self::LoadLibart(e) => write!(f, "failed to dlopen libart.so: {e}"),
            Self::ResolveSymbol(e) => write!(f, "failed to resolve JNI_CreateJavaVM: {e}"),
            Self::OptionHasNul => f.write_str("an ART VM option contained an interior NUL byte"),
            Self::CreateVm(rc) => write!(f, "JNI_CreateJavaVM failed (status {rc})"),
            Self::NullEnv => f.write_str("JNI_CreateJavaVM returned a null JNIEnv"),
        }
    }
}

impl std::error::Error for RuntimeError {
    fn source(&self) -> Option<&(dyn std::error::Error + 'static)> {
        match self {
            Self::LoadLibart(e) | Self::ResolveSymbol(e) => Some(e),
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
        assert_eq!(library_path_option(&fw), "-Djava.library.path=/fw/natives");
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
