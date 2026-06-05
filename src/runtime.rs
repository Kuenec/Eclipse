//! Android runtime: ART boot plan, host detection & lifecycle (component-map C/I · 🟢 + 🔴 ART).
//!
//! This crate eventually boots the **vendored AOSP ART** (`dlopen` `libart`,
//! `JNI_CreateJavaVM` with the boot image + bootclasspath + classpath = our framework jar :
//! Roblox APK), registers the framework native backends, and drives the Activity lifecycle
//! (`onCreate` …) — see the boot diagram in `docs/art-and-runtime.md` §3.
//!
//! ART is **unavoidable** for Roblox (see component-map §3) and sits **off the gameplay hot
//! path**, so it costs no FPS.
//!
//! ## What this module implements *now* (M1, no FFI yet)
//! Two pure-Rust, side-effect-free pieces that the documented boot needs *before* the VM is
//! ever loaded:
//!  1. [`instruction_set_features`] — runtime host-CPU detection that produces the real
//!     `dex2oat --instruction-set-features` string (detect-don't-assume, AGENTS.md §9). M0
//!     Step 4 found ATL hardcodes a conservative baseline ISA
//!     (`-ssse3,-sse4.1,-sse4.2,-avx,-avx2,-popcnt`); feeding dex2oat the *actual* host ISA
//!     lets it emit better code (perf priority, AGENTS.md §6).
//!  2. [`BootPlan`] — the concrete ART launch parameters the documented boot will pass,
//!     derived from a verified APK [`Manifest`](crate::apk::Manifest) + a
//!     [`Config`](crate::config::Config) + host detection. Every field maps 1:1 to a real
//!     ART/dex2oat argument; [`BootPlan::art_options`] renders them.
//!
//! ## What is deliberately *not* here yet
//! The actual VM boot (`dlopen` libart + `JNI_CreateJavaVM` + a `winit` window + an `ash`
//! Vulkan surface) is the charter's highest-risk, last step. It is represented by
//! [`boot`], which returns [`RuntimeError::NotImplemented`] rather than faking a boot. When
//! that path lands it introduces the crate's first `unsafe` (FFI/JNI), at which point this
//! module's `#![forbid(unsafe_code)]` must be lifted **and every `extern "C"`/JNI boundary
//! must wrap its body in `std::panic::catch_unwind`** so a Rust panic can never unwind into
//! ART's C++ under the release `panic = "abort"` profile (AGENTS.md §2.8).

// 2026-06-04: This increment is pure host detection + plan construction — no FFI, so no
// unsafe. Forbidding it here gives a hard guarantee for the current code (matching
// `src/apk`). The imminent VM-boot path (dlopen libart / JNI_CreateJavaVM / ash) is the
// only place unsafe is justified (AGENTS.md §2.3); lift this attribute when [`boot`] is
// implemented and confine the unsafe to that path, each block carrying a `// SAFETY:` note.
#![forbid(unsafe_code)]

use std::fmt;

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
/// `-XX:DisableHSpaceCompactForOOM`, fits a single contiguous reservation. This is the
/// largest value M0 validated as bootable; see AGENTS.md §5 for the full bisect table.
const HEAP_MIB: u32 = 768;

/// The `dex2oat`/ART x86 instruction-set feature tokens, in ART's canonical emit order.
///
/// 2026-06-04: order and token spellings are taken from AOSP ART
/// `runtime/arch/x86/instruction_set_features_x86.cc` (`GetFeatureString`): it appends
/// `ssse3, sse4.1, sse4.2, avx, avx2, popcnt`, each as the bare token when present or
/// prefixed with `-` when absent. This matches the exact baseline string M0 Step 4 observed
/// ATL's dex2oat emit (`-ssse3,-sse4.1,-sse4.2,-avx,-avx2,-popcnt`). Each token here is also
/// a valid `std::arch::is_x86_feature_detected!` name, so detection maps 1:1 onto emission.
const X86_FEATURE_TOKENS: [&str; 6] = ["ssse3", "sse4.1", "sse4.2", "avx", "avx2", "popcnt"];

/// Detect this host's x86-64 ISA features and format them as a `dex2oat`
/// `--instruction-set-features` value.
///
/// Present features are listed by name, absent ones prefixed with `-`, e.g.
/// `"ssse3,sse4.1,sse4.2,-avx,-avx2,popcnt"`. This is the detect-don't-assume fix for the
/// M0 Step 4 finding that ATL hardcodes a conservative baseline ISA (AGENTS.md §9): passing
/// the *real* host ISA lets dex2oat emit better code for the libcore boot image (perf
/// priority, AGENTS.md §6).
///
/// Detection uses `std::arch::is_x86_feature_detected!`, which performs a runtime `CPUID`
/// query (not a compile-time `target_feature` check), so the result reflects the machine the
/// launcher actually runs on, not the build host.
#[cfg(target_arch = "x86_64")]
#[must_use]
pub fn instruction_set_features() -> String {
    // Map each ART x86 token to the matching std::arch CPUID probe. is_x86_feature_detected!
    // requires a string *literal*, so the arm list is written out rather than iterating the
    // token array. The match is exhaustive over X86_FEATURE_TOKENS — the test
    // `feature_tokens_all_have_a_detector` enforces that no token is left without a probe.
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
    format_feature_string(|token| detected(token))
}

// 2026-06-04: Eclipse runs only the Android **x86-64** build of Roblox
// (`lib/x86_64/libroblox.so`), so a non-x86_64 host cannot run the engine at all. Rather
// than emit a bogus or all-absent x86 ISA string on such a host, fail to compile with an
// actionable message (detect-don't-assume, AGENTS.md §9). When/if another engine ABI is
// ever targeted, add its real per-arch detector here instead of removing this guard.
#[cfg(not(target_arch = "x86_64"))]
compile_error!(
    "Eclipse's runtime targets the Android x86-64 Roblox engine; \
     instruction_set_features() needs an x86_64 host (no x86 ISA to detect on this arch)"
);

/// Format the `--instruction-set-features` string from a feature-presence predicate.
///
/// Split out from [`instruction_set_features`] so the formatting (token order, the `-`
/// prefix for absent features, comma joining) is testable without depending on the host
/// CPU. `present` answers whether a given [`X86_FEATURE_TOKENS`] entry is available.
fn format_feature_string(present: impl Fn(&str) -> bool) -> String {
    // Pre-size: each token plus a separator and possible '-'. Avoids reallocation while
    // building the small fixed-length string.
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
/// Vulkan is the default (best FPS — lower driver overhead, explicit multithreaded
/// submission, AGENTS.md §6); OpenGL is the fallback, selected when
/// [`Config::use_opengl`](crate::config::Config) is set to force GL where Vulkan can't init.
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

/// The concrete ART launch parameters the documented boot (`docs/art-and-runtime.md` §3)
/// will pass, derived from a verified APK [`Manifest`](crate::apk::Manifest), a
/// [`Config`](crate::config::Config), and host detection.
///
/// Every field maps 1:1 to a real ART/dex2oat argument — there is no speculative
/// configuration here. [`BootPlan::art_options`] renders the `-X`/`-XX`/dex2oat option
/// strings these fields imply.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct BootPlan {
    /// Fully-qualified launcher Activity to start. Defaults to the manifest's resolved
    /// MAIN/LAUNCHER activity; can be overridden (e.g. `ActivityNativeMain`) to skip the
    /// splash — see [`BootPlan::with_activity_override`].
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
    ///
    /// - `launcher_activity` = the manifest's resolved MAIN/LAUNCHER activity (use
    ///   [`BootPlan::with_activity_override`] to skip the splash).
    /// - `sdk_int` = `manifest.target_sdk`, or [`DEFAULT_SDK_INT`] when the manifest omits
    ///   `<uses-sdk>`.
    /// - `heap_mib` / `disable_hspace_compact` = the M0-validated heap settings.
    /// - `instruction_set_features` = detected from the host CPU.
    /// - `graphics_backend` = OpenGL iff `config.use_opengl`, else Vulkan (perf-first).
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

    /// Override the launcher activity (e.g. `"com/roblox/client/ActivityNativeMain"` to
    /// bypass the splash, as the M0 boot did).
    #[must_use]
    pub fn with_activity_override(mut self, activity: impl Into<String>) -> Self {
        self.launcher_activity = activity.into();
        self
    }

    /// Render the ART `-X`/`-XX` and `dex2oat` option strings this plan implies, in the
    /// order the documented boot passes them.
    ///
    /// These are exactly the strings handed to `JNI_CreateJavaVM`'s `JavaVMOption` array
    /// (the `-X*` heap flags) and to `dex2oat` (the `--instruction-set-features` flag). The
    /// launcher activity, `sdk_int`, and graphics backend are launch *inputs* rather than
    /// ART VM options, so they are reported in the dry-run plan but are not part of this
    /// `-X` list.
    #[must_use]
    pub fn art_options(&self) -> Vec<String> {
        let mut opts = Vec::with_capacity(4);
        // M0-validated heap sizing (AGENTS.md §5): cap the managed heap and its growth
        // limit, and disable the second (compaction) reservation so a single block fits.
        opts.push(format!("-Xmx{}m", self.heap_mib));
        opts.push(format!("-XX:HeapGrowthLimit={}m", self.heap_mib));
        if self.disable_hspace_compact {
            opts.push("-XX:DisableHSpaceCompactForOOM".to_owned());
        }
        // Real host ISA for dex2oat codegen (M0 Step 4 fix; AGENTS.md §9).
        opts.push(format!(
            "--instruction-set-features={}",
            self.instruction_set_features
        ));
        opts
    }
}

/// Boot the ART VM for the given plan.
///
/// **Not implemented yet** — always returns [`RuntimeError::NotImplemented`]. The real
/// implementation (`dlopen` libart, then `JNI_CreateJavaVM` with the boot image,
/// bootclasspath and classpath, a `winit` window, and an `ash`/EGL surface from
/// [`BootPlan::graphics_backend`]) is the charter's highest-risk, last step and requires the
/// vendored ART to be linked. This function exists so the boot entry point is named and the
/// rest of the launcher can build a plan against it without faking a boot.
///
// TODO(runtime-FFI): implement the VM boot. It introduces the crate's first `unsafe`
// (FFI/JNI), so lift this module's `#![forbid(unsafe_code)]` then and confine the unsafe to
// the boot path. Per AGENTS.md §2.8, with the release `panic = "abort"` profile EVERY
// `extern "C"`/JNI callback must wrap its body in `std::panic::catch_unwind` so a Rust panic
// can never unwind into ART's C++.
pub fn boot(_plan: &BootPlan) -> Result<(), RuntimeError> {
    Err(RuntimeError::NotImplemented)
}

/// Errors from the runtime subsystem.
#[derive(Debug)]
pub enum RuntimeError {
    /// The ART VM boot is not implemented yet (the vendored ART is not linked). See [`boot`].
    NotImplemented,
}

impl fmt::Display for RuntimeError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::NotImplemented => f.write_str(
                "ART VM boot is not implemented yet (vendored ART not linked; FFI boot pending)",
            ),
        }
    }
}

impl std::error::Error for RuntimeError {}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::apk::Manifest;
    use crate::config::Config;

    /// A crafted manifest with an explicit `target_sdk` (no APK / no network needed).
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
        // All features present => every token is bare; all absent => every token has '-'.
        // This pins the dex2oat formatting contract (token order, comma join, '-' prefix)
        // verified 2026-06-04 against ART's instruction_set_features_x86.cc GetFeatureString.
        let all = format_feature_string(|_| true);
        assert_eq!(all, "ssse3,sse4.1,sse4.2,avx,avx2,popcnt");
        let none = format_feature_string(|_| false);
        assert_eq!(none, "-ssse3,-sse4.1,-sse4.2,-avx,-avx2,-popcnt");
        // The all-absent form must equal the conservative baseline ATL hardcoded (M0 Step 4).
        assert_eq!(none, "-ssse3,-sse4.1,-sse4.2,-avx,-avx2,-popcnt");
    }

    #[test]
    fn feature_string_mixed_keeps_order_and_prefix() {
        // A mixed predicate (only SSE-family present) must keep ART's canonical order and
        // mark exactly the absent features with '-'.
        let s = format_feature_string(|t| matches!(t, "ssse3" | "sse4.1" | "sse4.2"));
        assert_eq!(s, "ssse3,sse4.1,sse4.2,-avx,-avx2,-popcnt");
    }

    #[cfg(target_arch = "x86_64")]
    #[test]
    fn instruction_set_features_self_consistent_with_std_arch() {
        // The public detector must agree with std::arch token-for-token on THIS host, so the
        // test is valid on any x86_64 CPU (no fixed feature set assumed — detect-don't-assume).
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

    #[cfg(target_arch = "x86_64")]
    #[test]
    fn instruction_set_features_shape_is_valid() {
        // Shape invariants that hold regardless of which features the host has: exactly six
        // comma-separated tokens, each an X86_FEATURE_TOKENS entry optionally prefixed '-'.
        let s = instruction_set_features();
        let parts: Vec<&str> = s.split(',').collect();
        assert_eq!(parts.len(), X86_FEATURE_TOKENS.len());
        for (part, token) in parts.iter().zip(X86_FEATURE_TOKENS) {
            let bare = part.strip_prefix('-').unwrap_or(part);
            assert_eq!(bare, token, "token out of order or misspelled in {s:?}");
        }
    }

    #[test]
    fn boot_plan_derives_fields_from_manifest_and_config() {
        let manifest = manifest_with(Some(35));
        let config = Config::default(); // use_opengl == false
        let plan = BootPlan::new(&manifest, &config);

        assert_eq!(
            plan.launcher_activity,
            "com.roblox.client.startup.ActivitySplash"
        );
        assert_eq!(plan.sdk_int, 35); // from manifest.target_sdk
        assert_eq!(plan.heap_mib, HEAP_MIB);
        assert!(plan.disable_hspace_compact);
        assert_eq!(plan.graphics_backend, GraphicsBackend::Vulkan); // default
        assert_eq!(plan.instruction_set_features, instruction_set_features());
    }

    #[test]
    fn boot_plan_sdk_int_falls_back_when_manifest_omits_target() {
        // Documented fallback: no targetSdk in the manifest => DEFAULT_SDK_INT (never invented).
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
        assert_eq!(plan.graphics_backend.as_str(), "OpenGL");
    }

    #[test]
    fn boot_plan_vulkan_when_use_opengl_false() {
        let config = Config {
            use_opengl: false,
            ..Config::default()
        };
        let plan = BootPlan::new(&manifest_with(Some(35)), &config);
        assert_eq!(plan.graphics_backend, GraphicsBackend::Vulkan);
        assert_eq!(plan.graphics_backend.as_str(), "Vulkan");
    }

    #[test]
    fn boot_plan_activity_override_replaces_launcher() {
        let plan = BootPlan::new(&manifest_with(Some(35)), &Config::default())
            .with_activity_override("com/roblox/client/ActivityNativeMain");
        assert_eq!(
            plan.launcher_activity,
            "com/roblox/client/ActivityNativeMain"
        );
        // Override changes only the activity; other derived fields are untouched.
        assert_eq!(plan.sdk_int, 35);
        assert_eq!(plan.heap_mib, HEAP_MIB);
    }

    #[test]
    fn art_options_contain_heap_and_feature_strings() {
        let plan = BootPlan::new(&manifest_with(Some(35)), &Config::default());
        let opts = plan.art_options();
        assert!(opts.contains(&"-Xmx768m".to_owned()), "{opts:?}");
        assert!(
            opts.contains(&"-XX:HeapGrowthLimit=768m".to_owned()),
            "{opts:?}"
        );
        assert!(
            opts.contains(&"-XX:DisableHSpaceCompactForOOM".to_owned()),
            "{opts:?}"
        );
        let isa = format!("--instruction-set-features={}", instruction_set_features());
        assert!(opts.contains(&isa), "{opts:?}");
    }

    #[test]
    fn art_options_omit_hspace_flag_when_disabled() {
        // The DisableHSpaceCompactForOOM flag is conditional: prove it is absent when the
        // plan does not request it (so art_options reflects the field, not a constant).
        let mut plan = BootPlan::new(&manifest_with(Some(35)), &Config::default());
        plan.disable_hspace_compact = false;
        let opts = plan.art_options();
        assert!(
            !opts
                .iter()
                .any(|o| o.contains("DisableHSpaceCompactForOOM")),
            "{opts:?}"
        );
        // Heap and ISA flags are still present.
        assert!(opts.contains(&"-Xmx768m".to_owned()), "{opts:?}");
    }

    #[test]
    fn boot_is_not_implemented_and_is_typed() {
        let plan = BootPlan::new(&manifest_with(Some(35)), &Config::default());
        let err = boot(&plan).expect_err("boot must be unimplemented");
        assert!(matches!(err, RuntimeError::NotImplemented));
        // Display is actionable (mentions the pending FFI / vendored ART).
        let msg = err.to_string();
        assert!(msg.contains("not implemented"), "{msg}");
    }

    #[test]
    fn feature_tokens_all_have_a_detector() {
        // Guards the 1:1 mapping invariant: every X86_FEATURE_TOKENS entry must be handled by
        // the detector match in instruction_set_features (and the test mirror above). If a
        // token were added without a probe, format_feature_string would treat it as absent
        // here, diverging from a real detector — this asserts the known set is exactly six.
        assert_eq!(X86_FEATURE_TOKENS.len(), 6);
        for token in X86_FEATURE_TOKENS {
            assert!(!token.is_empty());
            // Tokens must be lowercase ASCII (dex2oat is case-sensitive).
            assert_eq!(token.to_ascii_lowercase(), token);
        }
    }
}
