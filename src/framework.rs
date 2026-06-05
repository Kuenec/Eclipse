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
//! `attach_current_thread`, **binds Eclipse's own non-GTK backing for the two natives
//! `android.content.Context`'s static initializer calls** (`native_get_apk_path` +
//! `native_updateConfig`) via `RegisterNatives`, resolves the recipe's bootstrap classes
//! ([`CONTEXT_CLASS`]/[`APPLICATION_CLASS`]) with `find_class` to prove the typed-`Env` bridge
//! reaches the loaded `android.*` framework, and then **drives recipe steps 1–3** —
//! `Context.createApplication(J)` → `ContentProvider.createContentProviders()` →
//! `Application.onCreate()` — to reach `Application.onCreate` for a pure-Java APK. The recipe
//! steps are encoded as typed constants ([`STEP1_CREATE_APPLICATION`] … [`STEP5_ACTIVITY_ON_CREATE`]).
//!
//! ### Why passing `0` (a null handle) to `createApplication(J)` is safe for steps 1–3
//! 2026-06-05: steps 1–3 are **pure Java** — they only *store* the `jlong native_window` in an
//! `Application` field; they do **not** dereference it (`docs/art-and-runtime.md` "Tier A":
//! `createApplication`/`createContentProviders`/`Application.onCreate` invoke no native that
//! touches the handle). The handle is first dereferenced at step 4 (`Activity.createMainActivity`
//! → the Window natives), which is deferred. So `0` is a confirmed-safe placeholder for steps 1–3,
//! not a guess; the real Eclipse-owned handle arrives with the framework/Surface design
//! (component-map F) when steps 4–5 are driven.
//!
//! ### Why bind those two natives (the non-GTK backing — confirmed)
//! ATL's `api-impl.jar` declares `native_get_apk_path`/`native_updateConfig` and backs them in C
//! against GTK/GDK (`libtranslation_layer_main.so`). Eclipse must NOT pull in GTK (it re-crowds
//! the low_4gb window — AGENTS.md §5 Step 3.5), so it supplies its OWN Rust implementations and
//! binds them by name via `RegisterNatives` (which takes precedence over the lazy symbol-name
//! binding ATL relies on — JNI 1.1 spec). They are registered BEFORE `Context.<clinit>` can run
//! (`find_class` loads/links but does not initialize the class), so the static initializer finds
//! them already bound and GTK-free. Only these two are bound — they are the only natives the
//! static initializer reaches for the pure-Java demo APK (`Context.java` `static { … }`).
//!
//! ## What is deferred (and why)
//! Steps **4–5** — `Activity.createMainActivity(String, jlong, String)→Activity` and
//! `Activity.onCreate(Bundle)` — are **not** driven yet. They are gated on a single unresolved
//! input: the **window handle** passed as the `jlong`, which step 4's Window natives actually
//! *dereference* (unlike steps 1–3, which only store it). The vendored framework Eclipse loads is
//! ATL's GTK-coupled `api-impl.jar`, whose `create*` natives ultimately cast that `jlong` to a
//! `GtkWidget*`; the handle Eclipse's winit window yields is **not** a `GtkWidget*`, and the
//! committed recipe lists "the exact window-handle type Eclipse passes as the `jlong`" as
//! **UNCONFIRMED** (`docs/art-and-runtime.md` "UNCONFIRMED"). Passing a winit raw handle into a
//! GTK-expecting native would be a *guessed* pointer (CLAUDE.md: no guessing) and risks
//! type-confused dereference. So this increment drives steps 1–3 (which never dereference the
//! handle) and stops *before* the first handle-dereferencing call; driving step 4 onward is
//! unblocked by the framework/Surface design (component-map F) that defines Eclipse's own window
//! handle. See [`LifecycleProgress`].
//!
//! ## `unsafe`
//! 2026-06-05: confined to the JNI FFI surface, each block carrying a `// SAFETY:` note —
//! [`jni::vm::JavaVM::from_raw`] in [`drive_application_lifecycle`], the
//! [`NativeMethod::from_raw_parts`]/`register_native_methods` calls that bind the two Context
//! natives, and the [`FieldSignature::from_raw_parts`] pairing the `"I"` signature with
//! `JavaType::Int`. The JNI work runs under `attach_current_thread`; the driver closure is wrapped
//! in `std::panic::catch_unwind`, and each registered native body runs inside
//! [`EnvUnowned::with_env`] (which `catch_unwind`-wraps it), so a Rust panic can never unwind into
//! ART's C++ under the release `panic = "abort"` profile (AGENTS.md §2.8; CLAUDE.md).

use std::fmt;
use std::panic::AssertUnwindSafe;
use std::sync::OnceLock;

use jni::errors::LogErrorAndDefault;
use jni::objects::{JClass, JObject, JString};
use jni::signature::{FieldSignature, JavaType, Primitive};
use jni::strings::JNIStr;
use jni::vm::JavaVM;
use jni::{jni_sig, jni_str, Env, EnvUnowned, JValue, NativeMethod};

use crate::runtime::Vm;

// === Eclipse's own (non-GTK) backing for android.content.Context's static-init natives =========
//
// 2026-06-05: `android.content.Context`'s static initializer (ATL `api-impl/android/content/
// Context.java` `static { … }`, lines 113–155) invokes exactly two native methods before the
// launcher lifecycle begins — `native_updateConfig(Configuration)` (line 117) and
// `native_get_apk_path()` (lines 121, 136). ATL backs these in C against GTK/GDK
// (`api-impl-jni/content/android_content_Context.c`: `native_get_apk_path` returns
// `NewStringUTF(apk_path)`; `native_updateConfig` sets `Configuration.screenWidthDp`/
// `screenHeightDp` from `gdk_monitor_get_geometry`). Eclipse must NOT pull in GTK/GDK (it would
// re-crowd the low_4gb window — AGENTS.md §5 Step 3.5), so we bind our OWN Rust backing for those
// two symbols via `RegisterNatives` (which wins over name-based lazy binding — JNI 1.1 spec)
// BEFORE the class is statically initialized. Only these two are bound; the other Context natives
// (`nativeOpenFile`, `nativeExportUnifiedPush`, …) are NOT reached by static init for the pure-Java
// demo APK and remain unbound (deferred).

/// The real on-disk APK path `native_get_apk_path` returns. Stashed once by
/// [`register_context_natives`] before the natives are registered (hence before
/// `Context.<clinit>` can call them), then read by the native on the main thread.
///
/// 2026-06-05: a process-wide `OnceLock<String>` is the simplest sound carrier — the value is set
/// once before any native call and only read afterward, and the lifecycle runs solely on the
/// attached main thread, so there is no contention and no per-call allocation beyond the JNI
/// string the JVM copies. `Env::new_string` takes `impl AsRef<str>`, so a `String` suffices.
static APK_PATH: OnceLock<String> = OnceLock::new();

/// Configuration screen dimensions Eclipse reports at `Context.<clinit>` (density-independent
/// pixels). ATL reads these from the real monitor via GDK; Eclipse uses safe, non-zero defaults so
/// the framework's `Resources`/`Configuration` are well-formed without querying GTK/GDK. 720p-class
/// dp values are a neutral, widely-valid baseline; a real surface size can replace them once the
/// window/Surface design (component-map F) lands.
const DEFAULT_SCREEN_WIDTH_DP: i32 = 1280;
const DEFAULT_SCREEN_HEIGHT_DP: i32 = 720;

// JNI method names + descriptors for the two Context static-init natives, exactly as declared in
// `Context.java` (2026-06-05): `private static native String native_get_apk_path();` and
// `protected static native void native_updateConfig(Configuration config);`.
const NATIVE_GET_APK_PATH_NAME: &JNIStr = jni_str!("native_get_apk_path");
const NATIVE_GET_APK_PATH_SIG: &JNIStr = jni_str!("()Ljava/lang/String;");
const NATIVE_UPDATE_CONFIG_NAME: &JNIStr = jni_str!("native_updateConfig");
const NATIVE_UPDATE_CONFIG_SIG: &JNIStr = jni_str!("(Landroid/content/res/Configuration;)V");

/// `Context.native_get_apk_path()` → the real APK path as a `java.lang.String`.
///
/// JNI ABI: a `static` native, so the second argument is the `JClass`. The body runs inside
/// [`EnvUnowned::with_env`], which wraps it in `catch_unwind` internally so a Rust panic can never
/// unwind into ART's C++ (AGENTS.md §2.8; `panic = "abort"` kept). `resolve::<LogErrorAndDefault>`
/// returns a neutral default (a null `JString`) on any error/panic rather than propagating.
extern "system" fn native_get_apk_path<'local>(
    mut env: EnvUnowned<'local>,
    _class: JClass<'local>,
) -> JString<'local> {
    env.with_env(|env| -> jni::errors::Result<JString<'local>> {
        // Stashed by register_context_natives before this native could be called. Absent ⇒ a logic
        // error (registration without a path); surface it as a JNI error, not a panic/unwrap.
        let path = APK_PATH
            .get()
            .ok_or(jni::errors::Error::JniCall(jni::errors::JniError::Unknown))?;
        env.new_string(path)
    })
    .resolve::<LogErrorAndDefault>()
}

/// `Context.native_updateConfig(Configuration)` → set `screenWidthDp`/`screenHeightDp` to safe,
/// GTK-free defaults.
///
/// JNI ABI: a `static` native taking one object argument, so the parameters are
/// `(EnvUnowned, JClass, JObject config)`. Sets the two `int` fields ATL's GDK-backed version sets,
/// but with fixed defaults — Eclipse must NOT query GDK/GTK (AGENTS.md §5 Step 3.5). The body is
/// `catch_unwind`-guarded by `with_env`; `resolve::<LogErrorAndDefault>` returns the `()` default on
/// error/panic. `()` is the correct neutral value for this `void` native.
extern "system" fn native_update_config<'local>(
    mut env: EnvUnowned<'local>,
    _class: JClass<'local>,
    config: JObject<'local>,
) {
    env.with_env(|env| -> jni::errors::Result<()> {
        // SAFETY: "screenWidthDp"/"screenHeightDp" are `public int` fields of
        // android.content.res.Configuration (ATL `api-impl/android/content/res/Configuration.java`
        // lines 600/615), so the "I" signature paired with JavaType::Int is consistent — exactly
        // FieldSignature::from_raw_parts' invariant. `set_field` additionally re-checks the value
        // type against the field at runtime, so a mismatch returns a typed error, never UB.
        let int_sig =
            unsafe { FieldSignature::from_raw_parts(INT_SIG, JavaType::Primitive(Primitive::Int)) };
        env.set_field(
            &config,
            SCREEN_WIDTH_DP_FIELD,
            &int_sig,
            DEFAULT_SCREEN_WIDTH_DP.into(),
        )?;
        env.set_field(
            &config,
            SCREEN_HEIGHT_DP_FIELD,
            &int_sig,
            DEFAULT_SCREEN_HEIGHT_DP.into(),
        )?;
        Ok(())
    })
    .resolve::<LogErrorAndDefault>()
}

// Field names + the primitive-int signature used by native_update_config.
const SCREEN_WIDTH_DP_FIELD: &JNIStr = jni_str!("screenWidthDp");
const SCREEN_HEIGHT_DP_FIELD: &JNIStr = jni_str!("screenHeightDp");
const INT_SIG: &JNIStr = jni_str!("I");

/// Bind Eclipse's own (non-GTK) backing for `android.content.Context`'s two static-init natives.
///
/// Stashes the real `apk_path` for [`native_get_apk_path`], then locates `android/content/Context`
/// and registers both natives via `RegisterNatives`. MUST be called BEFORE anything triggers the
/// class's static initializer (`<clinit>`) — `find_class` loads and links the class but does not
/// initialize it (JNI spec: `<clinit>` runs on first active use), so registering here means the two
/// natives are already bound when `<clinit>` later calls them.
///
/// # Safety / soundness
/// `register_native_methods` is `unsafe`: the function pointers must match the declared JNI
/// signatures. They do, by construction — [`native_get_apk_path`]/[`native_update_config`] are
/// written to the exact `()Ljava/lang/String;` / `(Landroid/content/res/Configuration;)V`
/// descriptors. Each native body is `catch_unwind`-guarded via [`EnvUnowned::with_env`], so no Rust
/// panic can cross the JNI boundary (AGENTS.md §2.8).
fn register_context_natives(env: &mut Env, apk_path: &str) -> Result<(), FrameworkError> {
    // Set-once; a second call (only one boot per process) keeps the first value — harmless.
    let _ = APK_PATH.set(apk_path.to_owned());

    let class = env.find_class(CONTEXT_CLASS)?;
    let methods = [
        // SAFETY: each fn matches the paired signature (see the natives' docs); casting the
        // `extern "system"` fn to a `*mut c_void` is how `NativeMethod::from_raw_parts` takes it.
        unsafe {
            NativeMethod::from_raw_parts(
                NATIVE_GET_APK_PATH_NAME,
                NATIVE_GET_APK_PATH_SIG,
                native_get_apk_path as *mut std::ffi::c_void,
            )
        },
        // SAFETY: as above for native_update_config / native_updateConfig.
        unsafe {
            NativeMethod::from_raw_parts(
                NATIVE_UPDATE_CONFIG_NAME,
                NATIVE_UPDATE_CONFIG_SIG,
                native_update_config as *mut std::ffi::c_void,
            )
        },
    ];
    // SAFETY: `class` is the loaded android/content/Context; `methods` hold valid fn pointers whose
    // signatures match the class's `native` declarations (verified against Context.java, 2026-06-05).
    unsafe { env.register_native_methods(&class, &methods) }?;
    tracing::info!(
        class = "android/content/Context",
        "registered Eclipse's non-GTK backing for native_get_apk_path + native_updateConfig"
    );
    Ok(())
}

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
/// Step-2 class: `android.content.ContentProvider` (internal name) — hosts the `static`
/// `createContentProviders()` entry point.
pub const CONTENT_PROVIDER_CLASS: &JNIStr = jni_str!("android/content/ContentProvider");

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
/// This increment reaches [`ApplicationOnCreate`](LifecycleProgress::ApplicationOnCreate): it
/// proves the bridge, then drives recipe steps 1–3 to `Application.onCreate`. The
/// handle-dereferencing calls (step 4 onward) are deferred — see the module docs and
/// [`drive_application_lifecycle`].
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum LifecycleProgress {
    /// `find_class` resolved both [`CONTEXT_CLASS`] and [`APPLICATION_CLASS`] from the attached
    /// main thread: the `from_raw` + `attach_current_thread` + `find_class` bridge to the loaded
    /// `android.*` framework works. An intermediate milestone on the way to
    /// [`ApplicationOnCreate`](Self::ApplicationOnCreate).
    BridgeProven,
    /// Recipe steps 1–3 ran on the attached main thread: `Context.createApplication(0)` returned an
    /// `Application`, `ContentProvider.createContentProviders()` completed, and
    /// `Application.onCreate()` was invoked on the returned object. Steps 4–5 (which dereference the
    /// `jlong` window handle) remain deferred on the framework/Surface window-handle design.
    ApplicationOnCreate,
}

/// Drive the booted ART VM to Roblox's `Application.onCreate` (recipe steps 1–3).
///
/// Wraps the held [`Vm`]'s raw `*mut JavaVM` with [`jni::vm::JavaVM::from_raw`], attaches the
/// current (main) thread, binds Eclipse's own non-GTK backing for `android.content.Context`'s two
/// static-init natives (`native_get_apk_path` returns `apk_path`; `native_updateConfig` sets the
/// `Configuration` screen dims to safe defaults), resolves the recipe's bootstrap classes
/// ([`CONTEXT_CLASS`], [`APPLICATION_CLASS`]) to prove the typed-`Env` bridge reaches the loaded
/// `android.*` framework, then **drives steps 1–3**: `Context.createApplication(0)` →
/// `ContentProvider.createContentProviders()` → `Application.onCreate()`. Returns
/// [`LifecycleProgress::ApplicationOnCreate`] on success.
///
/// `apk_path` is the on-disk APK path the framework's `native_get_apk_path` must return (the same
/// path passed to [`runtime::boot`](crate::runtime::boot)); it is stashed before the natives are
/// registered, so it is available the moment `Context.<clinit>` calls the native.
///
/// MUST be called on the process **main thread** — the thread that booted the VM
/// ([`runtime::boot`](crate::runtime::boot)) and on which winit's event loop runs. `Vm` is
/// `!Send`/`!Sync`, so the borrow checker keeps the caller on that thread; the main thread is
/// already JNI-attached after `JNI_CreateJavaVM`, so `attach_current_thread` is cheap.
///
/// The JNI closure body is wrapped in `std::panic::catch_unwind` so a Rust panic can never
/// unwind into ART's C++ under the release `panic = "abort"` profile (AGENTS.md §2.8). Each step's
/// failure — including a pending Java exception — surfaces as the typed [`FrameworkError::Jni`]
/// (never a panic/unwrap); a thrown exception is additionally described to stderr (to name the next
/// missing native/class for the dev-host discovery loop) and cleared before returning.
///
/// # Deferred (not a failure)
/// This stops *before* step 4 (`Activity.createMainActivity`): steps 4–5 take a `jlong` window
/// handle that the Window natives **dereference**, and that handle's type is UNCONFIRMED for
/// Eclipse's (non-GTK) window — see the module docs. Steps 1–3 only *store* the handle, so the
/// safe placeholder `0` is passed; step 4 onward is unblocked by the framework/Surface design
/// (component-map F).
pub fn drive_application_lifecycle(
    vm: &Vm,
    apk_path: &str,
) -> Result<LifecycleProgress, FrameworkError> {
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
        match std::panic::catch_unwind(AssertUnwindSafe(|| drive_steps_1_to_3(env, apk_path))) {
            Ok(result) => result,
            Err(_) => Err(FrameworkError::Panicked),
        }
    })
}

/// Prove the bridge, then drive recipe steps 1–3 to `Application.onCreate`. Split out so the panic
/// guard in [`drive_application_lifecycle`] wraps a single named call.
///
/// All JNI calls go through [`checked`], so a thrown Java exception is described + cleared and
/// surfaced as the typed [`FrameworkError::Jni`] rather than left pending or panicking. The recipe
/// class names / descriptors are the [`RecipeStep`] constants ([`STEP1_CREATE_APPLICATION`] …);
/// the matching compile-time `jni_str!`/`jni_sig!` literals at the call sites are pinned equal to
/// those constants by the unit test `call_site_literals_match_recipe_constants` (single source of
/// truth, no per-call allocation or fallible runtime signature parse).
fn drive_steps_1_to_3(env: &mut Env, apk_path: &str) -> Result<LifecycleProgress, FrameworkError> {
    // Bind native_get_apk_path + native_updateConfig BEFORE Context's static initializer can run
    // (find_class loads/links the class but does not initialize it — JNI spec), so the two natives
    // are already resolvable, non-GTK, when <clinit> later calls them. RegisterNatives wins over
    // name-based lazy binding (JNI 1.1 spec), so ATL's GTK-backed symbols are not used.
    register_context_natives(env, apk_path)?;

    // Resolve the recipe's bootstrap classes — proves the from_raw + attach + find_class bridge to
    // the loaded android.* framework before any call. `find_class` takes a `&JNIStr`; the `jni_str!`
    // constants are MUTF-8 encoded at compile time.
    env.find_class(CONTEXT_CLASS)?;
    env.find_class(APPLICATION_CLASS)?;
    tracing::info!(
        context = STEP1_CREATE_APPLICATION.class,
        application = STEP3_APPLICATION_ON_CREATE.class,
        "framework bridge proven: Context static-init natives registered + bootstrap classes resolved via JNI"
    );

    // Step 1: `static Context.createApplication(jlong native_window) -> Application`. The handle is
    // passed as `0` (null): steps 1–3 only STORE it, never dereference it (module docs; deref begins
    // at the deferred step 4). `<clinit>` runs here on first active use of Context, calling the two
    // natives bound above. `.l()` unwraps the returned Application JObject; a wrong return type is a
    // typed error, not a panic.
    let context = env.find_class(CONTEXT_CLASS)?;
    let app = checked(env, "step 1 Context.createApplication", |env| {
        env.call_static_method(
            &context,
            jni_str!("createApplication"),
            jni_sig!("(J)Landroid/app/Application;"),
            &[JValue::Long(0)],
        )?
        .l()
    })?;

    // Step 2: `static ContentProvider.createContentProviders() -> void` — instantiate the
    // manifest-declared providers. `.v()` asserts the void return.
    let content_provider = env.find_class(CONTENT_PROVIDER_CLASS)?;
    checked(
        env,
        "step 2 ContentProvider.createContentProviders",
        |env| {
            env.call_static_method(
                &content_provider,
                jni_str!("createContentProviders"),
                jni_sig!("()V"),
                &[],
            )?
            .v()
        },
    )?;

    // Step 3: instance `Application.onCreate() -> void` on the object from step 1 — the app's Java
    // shell self-init. Reaching this is the increment's milestone.
    checked(env, "step 3 Application.onCreate", |env| {
        env.call_method(&app, jni_str!("onCreate"), jni_sig!("()V"), &[])?
            .v()
    })?;

    tracing::info!(
        "Application.onCreate reached: recipe steps 1–3 driven (createMainActivity/step 4 deferred — window handle UNCONFIRMED)"
    );
    Ok(LifecycleProgress::ApplicationOnCreate)
}

/// Run a single JNI step, turning a thrown Java exception into a typed [`FrameworkError::Jni`].
///
/// 2026-06-05: the closure's `&mut Env<'local>` and the returned `T` share the **named** outer
/// `'local`, so a local ref the step produces (e.g. step 1's `Application` `JObject<'local>`) is
/// tied to the attachment scope and stays usable in later steps — not to a short reborrow inside
/// this helper. An elided `&mut Env` here would pin `T` to that reborrow and reject any
/// lifetime-bearing return (`error: lifetime may not live long enough`).
///
/// 2026-06-05: the `jni` crate's `call_*` return `Err(Error::JavaException)` on a thrown exception
/// but **leave it pending** (verified in the crate source, env.rs "this will _not_ clear the
/// exception"). A still-pending exception poisons the next JNI call, so on any error we
/// `exception_describe` it (prints the Java stack trace to stderr — names the next missing
/// native/class for the dev-host discovery loop) and `exception_clear` it before returning. The
/// `exception_check` guard avoids describing when the error was not a Java throw (e.g. a Rust-side
/// `WrongJValueType`). No unwrap/expect — the typed error propagates via `?`.
fn checked<'local, T>(
    env: &mut Env<'local>,
    what: &str,
    op: impl FnOnce(&mut Env<'local>) -> Result<T, jni::errors::Error>,
) -> Result<T, FrameworkError> {
    match op(env) {
        Ok(value) => Ok(value),
        Err(e) => {
            if env.exception_check() {
                env.exception_describe();
                env.exception_clear();
            }
            tracing::error!(step = what, error = %e, "framework lifecycle step failed");
            Err(FrameworkError::Jni(e))
        }
    }
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

    #[test]
    fn call_site_literals_match_recipe_constants() {
        // 2026-06-05: the steps-1–3 call sites in `drive_steps_1_to_3` use inline compile-time
        // `jni_str!`/`jni_sig!` literals (not the `RecipeStep` constants, which the `jni` API cannot
        // take directly). Pin those literals equal to the documented constants so the two cannot
        // drift — a mismatch would call the wrong method/signature at boot with no compile error.
        // `jni_str!` yields a `&JNIStr` (the method name); `jni_sig!` yields a `MethodSignature`
        // whose `.sig()` is the raw descriptor `&JNIStr`.
        assert_eq!(
            jni_str!("createApplication").to_str(),
            STEP1_CREATE_APPLICATION.method
        );
        assert_eq!(
            jni_sig!("(J)Landroid/app/Application;").sig().to_str(),
            STEP1_CREATE_APPLICATION.descriptor
        );
        assert_eq!(
            jni_str!("createContentProviders").to_str(),
            STEP2_CREATE_CONTENT_PROVIDERS.method
        );
        assert_eq!(
            jni_sig!("()V").sig().to_str(),
            STEP2_CREATE_CONTENT_PROVIDERS.descriptor
        );
        assert_eq!(
            jni_str!("onCreate").to_str(),
            STEP3_APPLICATION_ON_CREATE.method
        );
        assert_eq!(
            jni_sig!("()V").sig().to_str(),
            STEP3_APPLICATION_ON_CREATE.descriptor
        );
    }

    #[test]
    fn context_native_names_and_sigs_match_context_java() {
        // Pin the two Context static-init native method names + JNI descriptors against
        // `Context.java` (2026-06-05): a transcription regression (wrong name or sig) would make
        // RegisterNatives throw NoSuchMethodError at boot. These are host-independent constants.
        assert_eq!(NATIVE_GET_APK_PATH_NAME.to_str(), "native_get_apk_path");
        assert_eq!(NATIVE_GET_APK_PATH_SIG.to_str(), "()Ljava/lang/String;");
        assert_eq!(NATIVE_UPDATE_CONFIG_NAME.to_str(), "native_updateConfig");
        assert_eq!(
            NATIVE_UPDATE_CONFIG_SIG.to_str(),
            "(Landroid/content/res/Configuration;)V"
        );
        // The Configuration int fields native_updateConfig writes, and their JNI type.
        assert_eq!(SCREEN_WIDTH_DP_FIELD.to_str(), "screenWidthDp");
        assert_eq!(SCREEN_HEIGHT_DP_FIELD.to_str(), "screenHeightDp");
        assert_eq!(INT_SIG.to_str(), "I");
    }
}
