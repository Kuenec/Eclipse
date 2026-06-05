//! Android framework backends + lifecycle driver (component-map E · 🟢 Rust native side).
//!
//! The `android.*` framework is reimplemented as Java-on-ART (`api-impl.jar`); this module
//! is the **native (JNI) side** of those classes — the part ATL writes in C, we write in
//! Rust via the `jni` crate. It also drives the launcher lifecycle from native: the JNI call
//! sequence that takes a booted ART VM ([`runtime::boot`](crate::runtime::boot)) to Roblox's
//! `Application.onCreate` and beyond.
//!
//! ## What this module implements *now* (M2)
//! [`drive_application_lifecycle`] runs the **window-independent foundation** of the confirmed
//! onCreate recipe (`docs/art-and-runtime.md` "onCreate JNI recipe (confirmed)"): it wraps the
//! held VM with [`jni::vm::JavaVM::from_raw`], attaches the (already-attached) main thread via
//! `attach_current_thread`, and resolves the recipe's bootstrap classes
//! ([`CONTEXT_CLASS`]/[`APPLICATION_CLASS`]) with `find_class` to prove the typed-`Env` bridge
//! reaches the loaded `android.*` framework. The recipe steps are encoded as typed constants
//! ([`STEP1_CREATE_APPLICATION`] … [`STEP5_ACTIVITY_ON_CREATE`]).
//!
//! ## What is deferred (and why)
//! Step 1 itself — `Context.createApplication(J)→Application` — and steps 2–5 are **not** driven
//! yet. They are gated on a single unresolved input: the **window handle** passed as the
//! `jlong`. The vendored framework Eclipse loads is ATL's GTK-coupled `api-impl.jar`, whose
//! `create*` natives ultimately cast that `jlong` to a `GtkWidget*`; the handle Eclipse's winit
//! window yields is **not** a `GtkWidget*`, and the committed recipe lists "the exact
//! window-handle type Eclipse passes as the `jlong`" as **UNCONFIRMED**
//! (`docs/art-and-runtime.md` "UNCONFIRMED"). Passing a winit raw handle into a GTK-expecting
//! native would be a *guessed* pointer (CLAUDE.md: no guessing) and risks type-confused
//! dereference. So this increment builds the grounded bridge and stops *before* the first
//! window-dependent call; driving step 1 onward is unblocked by the framework/Surface design
//! (component-map F) that defines Eclipse's own window handle. See [`LifecycleProgress`].
//!
//! ## `unsafe`
//! 2026-06-04: confined to the single [`jni::vm::JavaVM::from_raw`] call in
//! [`drive_application_lifecycle`], which carries a `// SAFETY:` note. The JNI work runs under
//! `attach_current_thread`, and the closure body is additionally wrapped in
//! `std::panic::catch_unwind` so a Rust panic can never unwind into ART's C++ under the release
//! `panic = "abort"` profile (AGENTS.md §2.8; CLAUDE.md).

use std::fmt;
use std::panic::AssertUnwindSafe;

use jni::strings::JNIStr;
use jni::vm::JavaVM;
use jni::{jni_str, Env};

use crate::runtime::Vm;

// === The confirmed onCreate recipe, encoded as typed constants ===================
//
// 2026-06-04: class internal names (slashed, for `find_class`) and JNI method descriptors,
// transcribed from `docs/art-and-runtime.md` "onCreate JNI recipe (confirmed)" (sourced from
// ATL's `src/main-executable/main.c` + `api-impl/android/{content,app}/*.java`). The window is
// passed as a `jlong`/intptr_t handle, NOT an `android.view.Surface` object.

/// Step-1 bootstrap class: `android.content.Context` (internal/slashed name for `find_class`).
/// Hosts the `static` `createApplication(J)` entry point that begins the lifecycle.
pub const CONTEXT_CLASS: &JNIStr = jni_str!("android/content/Context");
/// The `android.app.Application` class (internal name) — the object step 1 returns and step 3
/// (`Application.onCreate`) is invoked on.
pub const APPLICATION_CLASS: &JNIStr = jni_str!("android/app/Application");

/// Step 1 (deferred): `static Context.createApplication(jlong native_window) -> Application`.
pub const STEP1_CREATE_APPLICATION: RecipeStep = RecipeStep {
    class: "android/content/Context",
    method: "createApplication",
    descriptor: "(J)Landroid/app/Application;",
};
/// Step 2 (deferred): `static ContentProvider.createContentProviders() -> void`.
pub const STEP2_CREATE_CONTENT_PROVIDERS: RecipeStep = RecipeStep {
    class: "android/content/ContentProvider",
    method: "createContentProviders",
    descriptor: "()V",
};
/// Step 3 (deferred): instance `Application.onCreate() -> void` (on the step-1 object).
pub const STEP3_APPLICATION_ON_CREATE: RecipeStep = RecipeStep {
    class: "android/app/Application",
    method: "onCreate",
    descriptor: "()V",
};
/// Step 4 (deferred): `static Activity.createMainActivity(String, jlong, String) -> Activity`.
pub const STEP4_CREATE_MAIN_ACTIVITY: RecipeStep = RecipeStep {
    class: "android/app/Activity",
    method: "createMainActivity",
    descriptor: "(Ljava/lang/String;JLjava/lang/String;)Landroid/app/Activity;",
};
/// Step 5 (deferred): instance `Activity.onCreate(Bundle) -> void` (on the step-4 object).
pub const STEP5_ACTIVITY_ON_CREATE: RecipeStep = RecipeStep {
    class: "android/app/Activity",
    method: "onCreate",
    descriptor: "(Landroid/os/Bundle;)V",
};

/// One step of the confirmed launcher-lifecycle JNI recipe: a `class.method` and its JNI
/// descriptor. Encoded so the (still-deferred, window-dependent) call sites bind verified
/// descriptors rather than inline string literals.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct RecipeStep {
    /// Java class internal (slashed) name, e.g. `android/content/Context`.
    pub class: &'static str,
    /// Method name, e.g. `createApplication`.
    pub method: &'static str,
    /// JNI method descriptor, e.g. `(J)Landroid/app/Application;`.
    pub descriptor: &'static str,
}

/// How far the lifecycle driver progressed before stopping.
///
/// This increment reaches [`BridgeProven`](LifecycleProgress::BridgeProven): the typed-`Env`
/// bridge resolved the recipe's bootstrap classes against the loaded framework. The
/// window-dependent calls (step 1 onward) are deferred — see the module docs and
/// [`drive_application_lifecycle`].
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum LifecycleProgress {
    /// `find_class` resolved both [`CONTEXT_CLASS`] and [`APPLICATION_CLASS`] from the attached
    /// main thread: the `from_raw` + `attach_current_thread` + `find_class` bridge to the loaded
    /// `android.*` framework works. The window-dependent `createApplication(J)` call (step 1) is
    /// the next increment, blocked only on the framework/Surface window-handle design.
    BridgeProven,
}

/// Drive the booted ART VM toward Roblox's `Application.onCreate` — the grounded foundation.
///
/// Wraps the held [`Vm`]'s raw `*mut JavaVM` with [`jni::vm::JavaVM::from_raw`], attaches the
/// current (main) thread, and resolves the recipe's bootstrap classes ([`CONTEXT_CLASS`],
/// [`APPLICATION_CLASS`]) to prove the typed-`Env` bridge reaches the loaded `android.*`
/// framework. Returns [`LifecycleProgress::BridgeProven`] on success.
///
/// MUST be called on the process **main thread** — the thread that booted the VM
/// ([`runtime::boot`](crate::runtime::boot)) and on which winit's event loop runs. `Vm` is
/// `!Send`/`!Sync`, so the borrow checker keeps the caller on that thread; the main thread is
/// already JNI-attached after `JNI_CreateJavaVM`, so `attach_current_thread` is cheap.
///
/// The JNI closure body is wrapped in `std::panic::catch_unwind` so a Rust panic can never
/// unwind into ART's C++ under the release `panic = "abort"` profile (AGENTS.md §2.8). On a
/// pending Java exception the typed [`FrameworkError::Jni`] is returned (never a panic/unwrap).
///
/// # Deferred (not a failure)
/// This stops *before* step 1 (`createApplication(J)`): that and steps 2–5 take a `jlong` window
/// handle whose type is UNCONFIRMED for Eclipse's (non-GTK) window — see the module docs. Calling
/// them with a guessed handle is forbidden (CLAUDE.md); they are unblocked by the framework/Surface
/// design (component-map F).
pub fn drive_application_lifecycle(vm: &Vm) -> Result<LifecycleProgress, FrameworkError> {
    // SAFETY: `vm.as_raw()` is the live `*mut JavaVM` that this process's `JNI_CreateJavaVM`
    // returned (verified non-null by `boot()`'s `NullEnv` check), supporting JNI 1.6 ≥ 1.4 —
    // exactly `from_raw`'s contract. We guard null here too so a null can never reach
    // `from_raw`'s internal `assert!` (which would panic). `&Vm` keeps the VM alive and pins us
    // to its (main) thread for the duration.
    let raw = vm.as_raw();
    if raw.is_null() {
        return Err(FrameworkError::NullVm);
    }
    let java_vm = unsafe { JavaVM::from_raw(raw) };

    // Run the JNI work under attach_current_thread (the main thread is already attached, so this
    // is cheap and does not detach on return). Wrap the closure body in catch_unwind so a panic
    // from inside JNI/ART can never unwind across the FFI boundary (panic = "abort"; §2.8).
    java_vm.attach_current_thread(|env: &mut Env| {
        match std::panic::catch_unwind(AssertUnwindSafe(|| prove_bridge(env))) {
            Ok(result) => result,
            Err(_) => Err(FrameworkError::Panicked),
        }
    })
}

/// Resolve the recipe's bootstrap classes via `find_class` to prove the bridge to the loaded
/// `android.*` framework. Split out so the panic guard in [`drive_application_lifecycle`] wraps a
/// single named call.
fn prove_bridge(env: &mut Env) -> Result<LifecycleProgress, FrameworkError> {
    // Step-1 host class. `find_class` takes a `&JNIStr`; the `jni_str!` constants are MUTF-8
    // encoded at compile time. A pending Java exception surfaces as the typed Jni error (via the
    // `From<jni::errors::Error>` impl below) through `?`.
    env.find_class(CONTEXT_CLASS)?;
    env.find_class(APPLICATION_CLASS)?;
    tracing::info!(
        context = STEP1_CREATE_APPLICATION.class,
        application = STEP3_APPLICATION_ON_CREATE.class,
        "framework bridge proven: bootstrap classes resolved via JNI (createApplication deferred — window handle UNCONFIRMED)"
    );
    Ok(LifecycleProgress::BridgeProven)
}

/// Errors from the framework lifecycle driver.
#[derive(Debug)]
pub enum FrameworkError {
    /// The held VM pointer was null (would violate `JavaVM::from_raw`'s contract).
    NullVm,
    /// A JNI/Java-side error (class not found, pending exception, …) — the typed `jni` error.
    Jni(jni::errors::Error),
    /// A Rust panic was caught at the JNI boundary (it must never unwind into ART's C++).
    Panicked,
}

impl fmt::Display for FrameworkError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::NullVm => f.write_str("framework driver received a null JavaVM pointer"),
            Self::Jni(e) => write!(f, "JNI error driving the framework lifecycle: {e}"),
            Self::Panicked => {
                f.write_str("a panic was caught at the framework JNI boundary (not propagated)")
            }
        }
    }
}

impl std::error::Error for FrameworkError {
    fn source(&self) -> Option<&(dyn std::error::Error + 'static)> {
        match self {
            Self::Jni(e) => Some(e),
            Self::NullVm | Self::Panicked => None,
        }
    }
}

// 2026-06-04: `JavaVM::attach_current_thread` requires the callback's error type implement
// `From<jni::errors::Error>` (it folds its own attach errors into it). Mapping a `jni` error to
// the typed `Jni` variant keeps the boundary typed (no unwrap/expect, §2.8).
impl From<jni::errors::Error> for FrameworkError {
    fn from(e: jni::errors::Error) -> Self {
        Self::Jni(e)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    // The live JNI sequence (from_raw/attach/find_class) is NOT an in-harness test: it needs a
    // booted ART VM on the process main thread, but the cargo-test harness runs tests on worker
    // threads where ART aborts (`scoped_thread_state_change`) — the same constraint documented for
    // `runtime::boot`. It is validated from `main()` via `eclipse run <demo.apk>`. The host-thread-
    // independent data — the encoded recipe — is unit-tested here.

    #[test]
    fn recipe_descriptors_match_confirmed_spec() {
        // Pin the confirmed JNI descriptors so a transcription regression fails loudly.
        assert_eq!(STEP1_CREATE_APPLICATION.class, "android/content/Context");
        assert_eq!(STEP1_CREATE_APPLICATION.method, "createApplication");
        assert_eq!(
            STEP1_CREATE_APPLICATION.descriptor,
            "(J)Landroid/app/Application;"
        );
        assert_eq!(STEP2_CREATE_CONTENT_PROVIDERS.descriptor, "()V");
        assert_eq!(STEP3_APPLICATION_ON_CREATE.class, "android/app/Application");
        assert_eq!(STEP3_APPLICATION_ON_CREATE.descriptor, "()V");
        assert_eq!(
            STEP4_CREATE_MAIN_ACTIVITY.descriptor,
            "(Ljava/lang/String;JLjava/lang/String;)Landroid/app/Activity;"
        );
        assert_eq!(
            STEP5_ACTIVITY_ON_CREATE.descriptor,
            "(Landroid/os/Bundle;)V"
        );
    }

    #[test]
    fn bootstrap_class_constants_are_slashed_internal_names() {
        // find_class needs slashed internal names, not dotted; guard against a dotted regression.
        // `JNIStr::to_str` returns the MUTF-8-decoded `Cow<str>`; these ASCII names round-trip.
        assert_eq!(CONTEXT_CLASS.to_str(), "android/content/Context");
        assert_eq!(APPLICATION_CLASS.to_str(), "android/app/Application");
    }
}
