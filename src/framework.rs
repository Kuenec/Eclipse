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
//! reaches the loaded `android.*` framework, and then **drives recipe steps 1–7** —
//! `Context.createApplication(J)` → `ContentProvider.createContentProviders()` →
//! `Application.onCreate()` → `Activity.createMainActivity(String, J, String)` →
//! `Activity.onCreate(Bundle)` → `Activity.onStart()` → `Activity.onResume()` — driving the launcher
//! Activity to the RESUMED (running/interactive) state for a pure-Java APK. The recipe steps are
//! encoded as typed constants ([`STEP1_CREATE_APPLICATION`] … [`STEP7_ACTIVITY_ON_RESUME`]).
//!
//! ### The `jlong` handle passed to `createApplication(J)` (real, Eclipse-owned)
//! 2026-06-05: step 1 now passes a **real Eclipse-owned window-registry handle** from
//! [`window_registry::allocate`] — the design-confirmed contract (`docs/art-and-runtime.md`
//! "Non-GTK Window/Surface backing — design"): the `jlong` is a generational-slab **registry
//! index**, NOT `Box::into_raw` and NOT a raw pointer, so a stale/fabricated handle is a
//! bounds+generation-checked `Err`, never UB. It is still safe for steps 1–3 because those are
//! **pure Java** — they only *store* the `jlong native_window` in an `Application` field; they do
//! **not** dereference it (`docs/art-and-runtime.md` "Tier A":
//! `createApplication`/`createContentProviders`/`Application.onCreate` invoke no native that touches
//! the handle). The handle is first dereferenced at step 4 (`Activity.createMainActivity` → the
//! Window/View natives), which now reuse the **same** handle (one window per launch), so the slot is
//! intentionally not freed during the run.
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
//! ## Steps 4–7 (driven against Eclipse-owned handles, 2026-06-05)
//! Steps **4–7** — `Activity.createMainActivity(String, jlong, String)→Activity`,
//! `Activity.onCreate(Bundle)`, then `Activity.onStart()` and `Activity.onResume()` (ATL's
//! `activity_start`, no-arg instance calls on the step-4 object that drive the launcher Activity to
//! the RESUMED state) — are now driven. The `jlong` is the **same Eclipse-owned
//! [`window_registry`] handle** step 1 received; because Eclipse owns BOTH sides of the `jlong` (it
//! supplies the non-GTK Window/View natives via `RegisterNatives`, which win over ATL's GTK
//! symbol-name binding — JNI 1.1 spec), the handle never reaches a `GtkWidget*` cast. The
//! handle-dereferencing Window/View natives the `setContentView` cascade reaches
//! (`Window.set_*`, the `View`/`ViewGroup`/`FrameLayout`/`TextView` `native_constructor`/`native_*`)
//! are bound minimal-and-sound against [`window_registry`]/[`view_registry`] — they record the
//! view-tree shape (class names, text, child edges) with **no** GTK widget and **no** real
//! layout/measure/draw; the ash/Vulkan surface + rendering is the deferred big build (AGENTS.md §5).
//! Each such native is added as the dev-host run surfaces it (`No implementation found …`). See
//! [`LifecycleProgress`].
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

use std::ffi::{c_char, c_int, c_void, CString};
use std::fmt;
use std::panic::AssertUnwindSafe;
use std::sync::OnceLock;
use std::time::Instant;

use jni::errors::LogErrorAndDefault;
use jni::objects::{JByteArray, JClass, JIntArray, JObject, JString};
use jni::refs::Reference;
use jni::signature::{FieldSignature, JavaType, Primitive};
use jni::strings::JNIStr;
use jni::sys::{jboolean, jfloat, jint, jlong, jshort};
use jni::vm::JavaVM;
use jni::{jni_sig, jni_str, Env, EnvUnowned, JValue, NativeMethod};

use crate::runtime::Vm;

pub mod asset_registry;
pub mod canvas_registry;
pub mod matrix_registry;
pub mod paint_registry;
pub mod path_registry;
pub mod theme_registry;
pub mod view_registry;
pub mod window_registry;
pub mod xml_registry;

/// Whether this ART build's `android.graphics.Canvas` supports Eclipse's draw cascade — i.e. its draw
/// ops bind as the modern-AOSP `nDraw*` natives AND a `Canvas(long)` ctor exists. Set by
/// [`register_canvas_natives`]: `true` if the `nDraw*` RegisterNatives succeeds, `false` if it throws
/// (the Canvas is GskCanvas/Bitmap-backed on this build — section note at `register_canvas_natives`).
/// [`drive_view_draw`] reads it to skip the cascade entirely when unsupported, so a missing
/// `Canvas(long)` ctor isn't re-attempted (and re-logged) every frame. Starts `false`; the lifecycle
/// driver always calls `register_canvas_natives` once before any frame, so it is set before the first
/// cascade. Atomic (lock-free, no dep) — read once per frame on the main thread.
static CANVAS_DRAW_SUPPORTED: std::sync::atomic::AtomicBool =
    std::sync::atomic::AtomicBool::new(false);

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
/// JNI field descriptor for a `java.lang.CharSequence` field (the `TypedValue.string` field that
/// `loadResourceValue` sets to a resolved pooled string).
const CHAR_SEQUENCE_SIG: &JNIStr = jni_str!("Ljava/lang/CharSequence;");

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

// === Eclipse's own (non-GTK) backing for android.util.Log.println_native ========================
//
// 2026-06-05: `android.util.Log` (ATL `api-impl/android/util/Log.java`) routes every log call
// (`v`/`d`/`i`/`w`/`e`/`println`/`wtf`) through the single native
// `static native int println_native(int bufID, int priority, String tag, String msg)` (line 367).
// ATL backs it in C (`api-impl-jni/android_util_Log.c`): it null-checks `msg` (→ -1), range-checks
// `bufID` against `LOG_ID_MAX` (= 4: LOG_ID_MAIN..LOG_ID_SYSTEM, util.h:23-30) (→ -1), then forwards
// to `__android_log_buf_write(bufID, priority, tag, msg)` (liblog) and returns its byte count. That
// path is GTK-free — it only writes to the Android log buffer (host: stderr/logcat). Eclipse's
// GTK-free equivalent forwards the `[tag] msg` to the `tracing` log (the `diagnostics` module) at the
// priority-mapped level and returns the message byte count, matching `Log.println`'s documented
// "number of bytes written" contract without pulling liblog or GTK.

/// `android.util.Log` (internal/slashed name for `find_class`) — hosts the single `println_native`.
pub const LOG_CLASS: &JNIStr = jni_str!("android/util/Log");

// JNI name + descriptor for the one Log native, exactly as declared in `Log.java` (2026-06-05):
// `public static native int println_native(int bufID, int priority, String tag, String msg);`.
const PRINTLN_NATIVE_NAME: &JNIStr = jni_str!("println_native");
const PRINTLN_NATIVE_SIG: &JNIStr = jni_str!("(IILjava/lang/String;Ljava/lang/String;)I");

/// `android.util.Log`'s priority constants (`Log.java` lines 56-81): the `priority` arg's meaning.
/// Used only to map to a `tracing` level for the GTK-free forward; an unknown value falls through to
/// a default level (never an error — ATL does not validate `priority`).
const LOG_PRIORITY_VERBOSE: jint = 2;
const LOG_PRIORITY_DEBUG: jint = 3;
const LOG_PRIORITY_INFO: jint = 4;
const LOG_PRIORITY_WARN: jint = 5;
const LOG_PRIORITY_ERROR: jint = 6;
const LOG_PRIORITY_ASSERT: jint = 7;

/// Number of Android log buffer IDs (`LOG_ID_MAIN`=0 … `LOG_ID_SYSTEM`=3, then `LOG_ID_MAX`), from
/// ATL `util.h` (`log_id_t`). `bufID` outside `0..LOG_ID_MAX` is rejected with `-1`, mirroring
/// `android_util_Log.c`'s `bufID < 0 || bufID >= LOG_ID_MAX` guard.
const LOG_ID_MAX: jint = 4;

/// `Log.println_native(int bufID, int priority, String tag, String msg)` → bytes written.
///
/// Mirrors ATL's `android_util_Log.c` observable behavior, GTK-free: returns `-1` if `msg` is null
/// or `bufID` is out of `0..LOG_ID_MAX`; otherwise forwards `[tag] msg` to `tracing` at the
/// priority-mapped level and returns the message's byte length (ATL returns
/// `__android_log_buf_write`'s byte count, which `Log.println` documents as "number of bytes
/// written").
///
/// JNI ABI: a `static` native, so the second argument is the `JClass`. The body runs inside
/// [`EnvUnowned::with_env`], which `catch_unwind`-wraps it so a Rust panic can never unwind into
/// ART's C++ (AGENTS.md §2.8; `panic = "abort"` kept). `resolve::<LogErrorAndDefault>` returns the
/// `jint` default (`0`) on any error/panic — a sound neutral byte count.
extern "system" fn println_native<'local>(
    mut env: EnvUnowned<'local>,
    _class: JClass<'local>,
    buf_id: jint,
    priority: jint,
    tag: JString<'local>,
    msg: JString<'local>,
) -> jint {
    env.with_env(|env| -> jni::errors::Result<jint> {
        // ATL: `if (msgObj == NULL) return -1;` — a null message is a caller error, not logged.
        if msg.is_null() {
            return Ok(-1);
        }
        // ATL: `if (bufID < 0 || bufID >= LOG_ID_MAX) return -1;`.
        if !(0..LOG_ID_MAX).contains(&buf_id) {
            return Ok(-1);
        }

        // ATL reads `tag` only if non-null (GetStringUTFChars else NULL); mirror that.
        let tag_str = if tag.is_null() {
            None
        } else {
            Some(tag.try_to_string(env)?)
        };
        let msg_str = msg.try_to_string(env)?;

        // GTK-free forward: route to `tracing` at the priority-mapped level (the diagnostics module),
        // the host equivalent of ATL's `__android_log_buf_write` → log buffer/stderr. `target` carries
        // the Android tag so existing `RUST_LOG`/EnvFilter setups can filter on it.
        let tag_ref = tag_str.as_deref().unwrap_or("");
        match priority {
            LOG_PRIORITY_VERBOSE => {
                tracing::trace!(target: "android.util.Log", tag = tag_ref, "{msg_str}")
            }
            LOG_PRIORITY_DEBUG => {
                tracing::debug!(target: "android.util.Log", tag = tag_ref, "{msg_str}")
            }
            LOG_PRIORITY_INFO => {
                tracing::info!(target: "android.util.Log", tag = tag_ref, "{msg_str}")
            }
            LOG_PRIORITY_WARN => {
                tracing::warn!(target: "android.util.Log", tag = tag_ref, "{msg_str}")
            }
            LOG_PRIORITY_ERROR | LOG_PRIORITY_ASSERT => {
                tracing::error!(target: "android.util.Log", tag = tag_ref, "{msg_str}")
            }
            // ATL does not validate `priority`; an unknown value still logs. Use info as the neutral
            // default rather than dropping the message.
            _ => tracing::info!(target: "android.util.Log", tag = tag_ref, priority, "{msg_str}"),
        }

        // ATL returns `__android_log_buf_write`'s byte count; report the message byte length, which
        // `Log.println` documents as the return value. `jint` is i32; a message longer than i32::MAX
        // bytes cannot occur in practice, but saturate to stay total (no overflow panic).
        Ok(jint::try_from(msg_str.len()).unwrap_or(jint::MAX))
    })
    .resolve::<LogErrorAndDefault>()
}

/// Bind Eclipse's own (non-GTK) backing for `android.util.Log`'s `println_native`.
///
/// Locates `android/util/Log` and registers the native via `RegisterNatives` (which wins over
/// name-based lazy binding — JNI 1.1 spec), so ATL's liblog-backed C symbol is not used. Like
/// [`register_context_natives`], this MUST run before anything triggers `Log`'s first active use;
/// it is registered before the lifecycle drive (ART resolves natives lazily during the lifecycle).
///
/// # Safety / soundness
/// `register_native_methods` is `unsafe`: the function pointer must match the declared JNI
/// signature. It does, by construction — [`println_native`] is written to the exact
/// `(IILjava/lang/String;Ljava/lang/String;)I` descriptor. The native body is `catch_unwind`-guarded
/// via [`EnvUnowned::with_env`], so no Rust panic can cross the JNI boundary (AGENTS.md §2.8).
fn register_log_natives(env: &mut Env) -> Result<(), FrameworkError> {
    let class = env.find_class(LOG_CLASS)?;
    let methods = [
        // SAFETY: `println_native` matches the paired signature (see the native's docs); casting the
        // `extern "system"` fn to a `*mut c_void` is how `NativeMethod::from_raw_parts` takes it.
        unsafe {
            NativeMethod::from_raw_parts(
                PRINTLN_NATIVE_NAME,
                PRINTLN_NATIVE_SIG,
                println_native as *mut std::ffi::c_void,
            )
        },
    ];
    // SAFETY: `class` is the loaded android/util/Log; `methods` holds a valid fn pointer whose
    // signature matches the class's `native` declaration (verified against Log.java, 2026-06-05).
    unsafe { env.register_native_methods(&class, &methods) }?;
    tracing::info!(
        class = "android/util/Log",
        "registered Eclipse's non-GTK backing for println_native"
    );
    Ok(())
}

// === Eclipse's own (non-GTK) backing for android.net.ConnectivityManager =========================
//
// 2026-06-11: ATL's `ConnectivityManager` (api-impl/android/net/ConnectivityManager.java) declares THREE
// `native` methods backed by ATL's GTK lib (`libtranslation_layer_main.so`), which Eclipse does NOT load:
// `registerNetworkCallback(NetworkRequest, NetworkCallback)`, `isActiveNetworkMetered()`, and
// `nativeGetNetworkAvailable()`. Roblox's `com.birbit.android.jobqueue` connectivity monitor calls
// `registerNetworkCallback` in `ActivitySplash.onCreate` (step 5), surfacing `UnsatisfiedLinkError`. Like
// the Context/Log/View natives, Eclipse binds its OWN GTK-free backing via `RegisterNatives` — durable
// (compiled into the binary, so it works without the framework-jar overlay). The host desktop network is
// treated as available + unmetered, and Eclipse delivers no connectivity callbacks (a sound no-op: Roblox
// degrades gracefully without connectivity-change events).

/// `android.net.ConnectivityManager` (internal/slashed name for `find_class`).
pub const CONNECTIVITY_MANAGER_CLASS: &JNIStr = jni_str!("android/net/ConnectivityManager");

// JNI names + descriptors, exactly as declared in ATL's ConnectivityManager.java (2026-06-11).
const CM_REGISTER_NETWORK_CALLBACK_NAME: &JNIStr = jni_str!("registerNetworkCallback");
const CM_REGISTER_NETWORK_CALLBACK_SIG: &JNIStr =
    jni_str!("(Landroid/net/NetworkRequest;Landroid/net/ConnectivityManager$NetworkCallback;)V");
const CM_IS_ACTIVE_NETWORK_METERED_NAME: &JNIStr = jni_str!("isActiveNetworkMetered");
const CM_IS_ACTIVE_NETWORK_METERED_SIG: &JNIStr = jni_str!("()Z");
const CM_NATIVE_GET_NETWORK_AVAILABLE_NAME: &JNIStr = jni_str!("nativeGetNetworkAvailable");
const CM_NATIVE_GET_NETWORK_AVAILABLE_SIG: &JNIStr = jni_str!("()Z");

/// `ConnectivityManager.registerNetworkCallback(NetworkRequest, NetworkCallback)` — no-op. Eclipse does
/// not deliver connectivity-change callbacks; Roblox's network monitor degrades gracefully without them.
/// Instance native (second arg is the `this` `JObject`). `with_env` `catch_unwind`-guards the body.
extern "system" fn cm_register_network_callback<'local>(
    mut env: EnvUnowned<'local>,
    _this: JObject<'local>,
    _request: JObject<'local>,
    _callback: JObject<'local>,
) {
    env.with_env(|_env| -> jni::errors::Result<()> { Ok(()) })
        .resolve::<LogErrorAndDefault>()
}

/// `ConnectivityManager.isActiveNetworkMetered()` → `false` (the host desktop network is unmetered).
extern "system" fn cm_is_active_network_metered<'local>(
    mut env: EnvUnowned<'local>,
    _this: JObject<'local>,
) -> jboolean {
    // jni 0.22 maps `jboolean` to Rust `bool`. The desktop host network is treated as unmetered.
    env.with_env(|_env| -> jni::errors::Result<jboolean> { Ok(false) })
        .resolve::<LogErrorAndDefault>()
}

/// `ConnectivityManager.nativeGetNetworkAvailable()` → `true` (the host desktop network is available).
extern "system" fn cm_native_get_network_available<'local>(
    mut env: EnvUnowned<'local>,
    _this: JObject<'local>,
) -> jboolean {
    // jni 0.22 maps `jboolean` to Rust `bool`. The desktop host network is treated as available.
    env.with_env(|_env| -> jni::errors::Result<jboolean> { Ok(true) })
        .resolve::<LogErrorAndDefault>()
}

/// Bind Eclipse's own (non-GTK) backing for `android.net.ConnectivityManager`'s three `native` methods.
///
/// Locates `android/net/ConnectivityManager` and registers the natives via `RegisterNatives` (which wins
/// over ATL's GTK-lib symbol binding — JNI 1.1 spec). Like [`register_log_natives`], it runs before the
/// lifecycle drive so the natives are bound before `ActivitySplash.onCreate`'s connectivity-monitor call.
///
/// # Safety / soundness
/// `register_native_methods` is `unsafe`: each fn pointer must match its declared JNI signature. They do,
/// by construction (the descriptors are taken verbatim from ATL's `ConnectivityManager.java`). Each body
/// is `catch_unwind`-guarded via [`EnvUnowned::with_env`], so no Rust panic can cross the JNI boundary.
fn register_connectivity_natives(env: &mut Env) -> Result<(), FrameworkError> {
    let class = env.find_class(CONNECTIVITY_MANAGER_CLASS)?;
    let methods = [
        // SAFETY: each fn matches its paired signature (verbatim from ConnectivityManager.java); casting
        // the `extern "system"` fn to `*mut c_void` is how `NativeMethod::from_raw_parts` takes it.
        unsafe {
            NativeMethod::from_raw_parts(
                CM_REGISTER_NETWORK_CALLBACK_NAME,
                CM_REGISTER_NETWORK_CALLBACK_SIG,
                cm_register_network_callback as *mut std::ffi::c_void,
            )
        },
        unsafe {
            NativeMethod::from_raw_parts(
                CM_IS_ACTIVE_NETWORK_METERED_NAME,
                CM_IS_ACTIVE_NETWORK_METERED_SIG,
                cm_is_active_network_metered as *mut std::ffi::c_void,
            )
        },
        unsafe {
            NativeMethod::from_raw_parts(
                CM_NATIVE_GET_NETWORK_AVAILABLE_NAME,
                CM_NATIVE_GET_NETWORK_AVAILABLE_SIG,
                cm_native_get_network_available as *mut std::ffi::c_void,
            )
        },
    ];
    // SAFETY: `class` is the loaded android/net/ConnectivityManager; `methods` hold valid fn pointers
    // whose signatures match the class's `native` declarations (verified against ConnectivityManager.java).
    unsafe { env.register_native_methods(&class, &methods) }?;
    tracing::info!(
        class = "android/net/ConnectivityManager",
        "registered Eclipse's non-GTK backing for registerNetworkCallback (no-op) + isActiveNetworkMetered (false) + nativeGetNetworkAvailable (true)"
    );
    Ok(())
}

// === Eclipse's own (non-GTK) backing for android.content.res.AssetManager.init ==================
//
// 2026-06-05: `android.content.res.AssetManager`'s constructors (ATL `api-impl/android/content/res/
// AssetManager.java`) call `init(android.os.Build.VERSION.RESOURCES_SDK_INT)` as the first native
// before any asset path is set (lines 112, 160); the declaration is
// `private native final void init(int sdk_version);` (line 779) — an INSTANCE native, JNI signature
// `(I)V`, exactly the signature ART reported missing. In AOSP/ATL, `init` creates the native
// asset-manager object and stores its handle in the `private long mObject` field
// ("For communication with native code." — line 87); subsequent natives (`native_setApkAssets`,
// `addAssetPathNative`, the `openAsset*`/resource lookups) consume `mObject`. ATL backs all of these
// in C against its own asset/zip machinery; Eclipse must NOT pull in that GTK-coupled native layer.
//
// MINIMAL STUB (to be refined when its behavior actually matters): Eclipse's `init` is a GTK-free
// no-op. `mObject` is left at its Java zero-init `0` — the AssetManager exists but holds no native
// asset table yet. This is sound, not behavior-faking: it lets `<init>` proceed past `init` so the
// re-run empirically reveals the NEXT native the constructor reaches (`native_setApkAssets(Object[],
// int)`, the first to touch `mObject`), which is the diagnostic discovery loop. When real asset
// access is needed, this native gains an Eclipse-owned asset-table handle (mirroring the
// window_registry pattern) stored back into `mObject` and the path/read natives are bound against it.

/// `android.content.res.AssetManager` (internal/slashed name for `find_class`) — hosts the `init`
/// native the constructor calls before setting any asset paths.
pub const ASSET_MANAGER_CLASS: &JNIStr = jni_str!("android/content/res/AssetManager");

// JNI name + descriptor for AssetManager's init native, exactly as declared in `AssetManager.java`
// (2026-06-05, line 779): `private native final void init(int sdk_version);`.
const ASSET_MANAGER_INIT_NAME: &JNIStr = jni_str!("init");
const ASSET_MANAGER_INIT_SIG: &JNIStr = jni_str!("(I)V");

// 2026-06-05: AssetManager is a DENYLISTED class (its native does asset/mmap/zip/parsing work), so
// this native is bound SIGNATURE-ONLY from the exact JNI signature ART reported missing
// (`No implementation found for void android.content.res.AssetManager.native_setApkAssets(
// java.lang.Object[], int)`), WITHOUT reading the class's Java or api-impl-jni C source. JNI
// descriptor `([Ljava/lang/Object;I)V` — an INSTANCE native (the receiver `this`, then an
// `Object[]` of ApkAssets, then an `int`). DISCOVERY-LOOP STUB: a sound GTK-free no-op so the
// constructor proceeds past it and the re-run reveals the NEXT native; to be refined once Eclipse
// has its own asset-table handle. `mObject` stays at its Java zero-init `0` (no native asset table
// is installed) — sound, not behavior-faking.
const ASSET_MANAGER_SET_APK_ASSETS_NAME: &JNIStr = jni_str!("native_setApkAssets");
const ASSET_MANAGER_SET_APK_ASSETS_SIG: &JNIStr = jni_str!("([Ljava/lang/Object;I)V");

// 2026-06-05: AssetManager is DENYLISTED, so this native is bound SIGNATURE-ONLY from the exact JNI
// signature ART reported missing (`No implementation found for void
// android.content.res.AssetManager.setConfiguration(int, int, java.lang.String, int ×14)`; mangled
// `...setConfiguration__IILjava_lang_String_2IIIIIIIIIIIIII`), WITHOUT reading the class's source.
// JNI descriptor `(IILjava/lang/String;IIIIIIIIIIIIII)V` — an INSTANCE native: 2 ints, a String,
// then 14 ints (configuration parameters: mcc/mnc/locale/orientation/density/etc., consumed by the
// asset/resource table). DISCOVERY-LOOP STUB: a sound GTK-free no-op so the constructor proceeds and
// the re-run reveals the NEXT native; refine once Eclipse has its own asset-table handle.
const ASSET_MANAGER_SET_CONFIGURATION_NAME: &JNIStr = jni_str!("setConfiguration");
const ASSET_MANAGER_SET_CONFIGURATION_SIG: &JNIStr =
    jni_str!("(IILjava/lang/String;IIIIIIIIIIIIII)V");

// 2026-06-05: `openXmlAssetNative(int cookie, String fileName)` is the native AOSP's
// `AssetManager.openXmlBlockAsset` calls to parse a binary-XML asset into a native tree and return a
// `long` handle (the framework wraps it as an `XmlBlock`/`XmlResourceParser`). The JNI signature ART
// reported missing was `(ILjava/lang/String;)J` (`No implementation found for long
// android.content.res.AssetManager.openXmlAssetNative(int, java.lang.String)`). The earlier no-op
// `0` return made `openXmlBlockAsset` throw `FileNotFoundException: Asset XML file:
// AndroidManifest.xml` (run log 2026-06-05), stalling `Context.<clinit>`. This is now a REAL
// Eclipse-owned backing (NOT ATL's C asset layer, NOT GTK): it reads `fileName` from the APK zip via
// the `apk` crate, parses it with the `axml` reader into an [`crate::apk::axml::XmlDocument`], stores
// it in the Eclipse-owned [`xml_registry`] generational slab, and returns the slab handle (≥ 1, never
// the reserved `0`). A genuine open failure (missing entry / parse error) returns `0`, which is the
// correct trigger for the framework's `FileNotFoundException` — not a fake success.
const ASSET_MANAGER_OPEN_XML_ASSET_NAME: &JNIStr = jni_str!("openXmlAssetNative");
const ASSET_MANAGER_OPEN_XML_ASSET_SIG: &JNIStr = jni_str!("(ILjava/lang/String;)J");

// 2026-06-05: `retrieveAttributes` is the styled-attribute path AOSP's `TypedArray` drives when the
// framework resolves a tag's framework attributes against `resources.arsc`. AssetManager is
// DENYLISTED, so this native is bound from the exact JNI signature ART reported missing
// (`No implementation found for boolean android.content.res.AssetManager.retrieveAttributes(long,
// int[], int, long, long)`, mangled `...retrieveAttributes__J_3IIJJ`, run log 2026-06-05) WITHOUT
// reading the class's Java or api-impl-jni C source. JNI descriptor `(J[IIJJ)Z` — an INSTANCE native
// whose args are `(long parseStateHandle, int[] attrs, int <parser/length>, long outValues, long
// outIndices)` returning a boolean (whether any non-default styled value was set). `outValues` and
// `outIndices` are raw pointers to native off-heap `int[]` buffers the framework's `TypedArray`
// allocated and sized; Eclipse fills them per the PUBLIC AOSP `TypedArray` ABI (see
// `retrieve_attributes` for the grounded layout). This is the genuine next asset subsystem
// (ARSC + the TypedArray ABI), not one more easy native.
const ASSET_MANAGER_RETRIEVE_ATTRIBUTES_NAME: &JNIStr = jni_str!("retrieveAttributes");
const ASSET_MANAGER_RETRIEVE_ATTRIBUTES_SIG: &JNIStr = jni_str!("(J[IIJJ)Z");

// 2026-06-05: `newTheme()` is the native AOSP's `Resources.newTheme()`/`AssetManager.createTheme()`
// calls to create a native theme object and return its `long` handle (the framework wraps it as a
// `Resources$Theme`; later `applyStyle`/`resolveAttributes`/`releaseTheme` consume the handle).
// Surfaced by the dev-host run during step 4 (`View.<init>` → `Context.obtainStyledAttributes` →
// `getTheme` → `Resources.newTheme` → `AssetManager.newTheme()`). AssetManager is DENYLISTED, so this
// is bound from the exact JNI signature ART reported missing (`No implementation found for long
// android.content.res.AssetManager.newTheme()`, run log 2026-06-05) WITHOUT reading the class's
// source. JNI descriptor `()J` — an INSTANCE native returning the theme handle. Backed non-GTK by the
// Eclipse-owned [`theme_registry`] (a generational-slab index, NOT a raw pointer — a stale/fabricated
// theme handle from a later native is a bounds+generation-checked `Err`, never UB).
const ASSET_MANAGER_NEW_THEME_NAME: &JNIStr = jni_str!("newTheme");
const ASSET_MANAGER_NEW_THEME_SIG: &JNIStr = jni_str!("()J");

// 2026-06-05: `applyThemeStyle(long theme, int styleRes, boolean force)` is the native AOSP's
// `Resources$Theme.applyStyle` calls to merge a style resource into a theme. Surfaced by the dev-host
// run during step 4 (`View.<init>` → `obtainStyledAttributes` → `getTheme` → `Theme.applyStyle` →
// `AssetManager.applyThemeStyle`). AssetManager is DENYLISTED, so this is bound from the exact JNI
// signature ART reported missing (`No implementation found for void
// android.content.res.AssetManager.applyThemeStyle(long, int, boolean)`, mangled `...__JIZ`, run log
// 2026-06-05) WITHOUT reading the class's source. JNI descriptor `(JIZ)V` — an INSTANCE native
// (theme handle, style resource id, force flag). Records the applied style id on the
// [`theme_registry`] theme (bounds+generation-checked — a bad theme handle is a typed Err, never UB);
// no real style resolution yet (the View cascade only needs the call to succeed).
const ASSET_MANAGER_APPLY_THEME_STYLE_NAME: &JNIStr = jni_str!("applyThemeStyle");
const ASSET_MANAGER_APPLY_THEME_STYLE_SIG: &JNIStr = jni_str!("(JIZ)V");

// 2026-06-05: `copyTheme(long dest, long source)` is the native AOSP's `Resources$Theme.setTo` calls
// to copy one theme's state into another. Surfaced by the dev-host run during step 4
// (`Theme.setTo` → `AssetManager.copyTheme`). AssetManager is DENYLISTED, so this is bound from the
// exact JNI signature ART reported missing (`No implementation found for void
// android.content.res.AssetManager.copyTheme(long, long)`, mangled `...__JJ`, run log 2026-06-05)
// WITHOUT reading the class's source. JNI descriptor `(JJ)V` — an INSTANCE native (dest handle,
// source handle). Copies the source [`theme_registry`] theme's recorded styles into the dest theme
// (both bounds+generation-checked — a bad handle is a typed Err, never UB).
const ASSET_MANAGER_COPY_THEME_NAME: &JNIStr = jni_str!("copyTheme");
const ASSET_MANAGER_COPY_THEME_SIG: &JNIStr = jni_str!("(JJ)V");

// 2026-06-05: `applyStyle(long theme, long parser, int defStyleAttr, int defStyleRes, int[] attrs,
// int length, long outValues, long outIndices)` is the THEME styled-attribute path AOSP's
// `Resources$Theme.obtainStyledAttributes` drives (the theme-resolved counterpart of the XML-driven
// `retrieveAttributes`). Surfaced by the dev-host run during step 4 (`View.<init>` →
// `Context.obtainStyledAttributes` → `Theme.obtainStyledAttributes` → `AssetManager.applyStyle`).
// AssetManager is DENYLISTED, so this is bound from the exact JNI signature ART reported missing
// (`No implementation found for void android.content.res.AssetManager.applyStyle(long, long, int,
// int, int[], int, long, long)`, mangled `...__JJII_3IIJJ`, run log 2026-06-05) WITHOUT reading the
// class's source. JNI descriptor `(JJII[IIJJ)V` — an INSTANCE native; `outValues`/`outIndices` are
// the same framework-allocated `TypedArray` off-heap buffers `retrieveAttributes` fills. A fresh
// View carries no theme-resolved style values yet, so Eclipse writes `TYPE_NULL` for every requested
// attribute (the View then uses its built-in defaults — sound AOSP fallback, NOT a value fake) and
// `outIndices[0] = 0`. Reuses the bounds-proven `fill_typed_array` writer.
const ASSET_MANAGER_APPLY_STYLE_NAME: &JNIStr = jni_str!("applyStyle");
const ASSET_MANAGER_APPLY_STYLE_SIG: &JNIStr = jni_str!("(JJII[IIJJ)V");

// 2026-06-05: `getResourceName(int resid)` is the native AOSP's `Resources.getResourceName` calls to
// turn a packed resource id into its full `package:type/entry` name. Surfaced by the dev-host run
// during step 5 (`MainActivity.onCreate` → `setContentView` → `LayoutInflater.inflate` →
// `Resources.getResourceName`). AssetManager is DENYLISTED, so this is bound from the exact JNI
// signature ART reported missing (`No implementation found for java.lang.String
// android.content.res.AssetManager.getResourceName(int)`, mangled `...__I`, run log 2026-06-05)
// WITHOUT reading the class's source. JNI descriptor `(I)Ljava/lang/String;` — an INSTANCE native.
// Backed by Eclipse's own [`apk::arsc`](crate::apk::arsc) `resources.arsc` reader: resolves the id's
// package/type/entry names and returns `package:type/entry`. Returns null for an unresolvable id
// (the framework then throws `NotFoundException` — the correct, non-faked outcome).
const ASSET_MANAGER_GET_RESOURCE_NAME_NAME: &JNIStr = jni_str!("getResourceName");
const ASSET_MANAGER_GET_RESOURCE_NAME_SIG: &JNIStr = jni_str!("(I)Ljava/lang/String;");

// 2026-06-11: `getResourcePackageName(int resid)` is the native AOSP's `Resources.getResourcePackageName`
// calls to turn a packed resource id into JUST its package name (the `package` of `package:type/entry`).
// Surfaced by the real Roblox run during `FirebaseInitProvider.onCreate` (→ `Resources.
// getResourcePackageName`). Bound from the exact JNI signature ART reported missing (`No implementation
// found for java.lang.String android.content.res.AssetManager.getResourcePackageName(int)`, mangled
// `...__I`, run log 2026-06-11) — an INSTANCE native, descriptor `(I)Ljava/lang/String;`. Backed by the
// same `apk::arsc` reader as `getResourceName`: returns the id's package name (`arsc::package_name` of
// the id's high-byte package), or null for an unresolvable id (→ `NotFoundException`, the non-faked outcome).
const ASSET_MANAGER_GET_RESOURCE_PACKAGE_NAME_NAME: &JNIStr = jni_str!("getResourcePackageName");
const ASSET_MANAGER_GET_RESOURCE_PACKAGE_NAME_SIG: &JNIStr = jni_str!("(I)Ljava/lang/String;");

// 2026-06-11: `getResourceIdentifier(String name, String defType, String defPackage)` is the REVERSE
// of `getResourceName` — AOSP's `Resources.getIdentifier` calls it to turn a resource NAME into its
// packed id (0 if absent). Surfaced by the real Roblox run during `FirebaseInitProvider.onCreate`.
// Bound from the exact JNI signature ART reported missing (`No implementation found for int
// android.content.res.AssetManager.getResourceIdentifier(java.lang.String, java.lang.String,
// java.lang.String)`, run log 2026-06-11), descriptor `(Ljava/lang/String;Ljava/lang/String;
// Ljava/lang/String;)I`, an INSTANCE native. Backed by the `apk::arsc` reverse lookup
// ([`arsc::ResTable::find_resource_id`]); returns 0 for an unknown name (AOSP's "not found").
const ASSET_MANAGER_GET_RESOURCE_IDENTIFIER_NAME: &JNIStr = jni_str!("getResourceIdentifier");
const ASSET_MANAGER_GET_RESOURCE_IDENTIFIER_SIG: &JNIStr =
    jni_str!("(Ljava/lang/String;Ljava/lang/String;Ljava/lang/String;)I");

// 2026-06-11: the asset-STREAM natives behind `AssetManager.open(fileName)` → an `AssetInputStream`.
// `openAsset(String,int)` (confirmed from the ART `No implementation found … openAsset(java.lang.
// String, int)` line, run log 2026-06-11, returns `long`) opens an asset and returns a handle; the
// read cycle (`readAsset`/`seekAsset`/`getAssetLength`/`getAssetRemainingLength`/`destroyAsset`)
// operates on it. Bound non-GTK against Eclipse's own [`asset_registry`] (jlong = slab index, never a
// raw pointer) + the `src/apk` reader (reads `assets/<fileName>`). The classic AOSP signatures
// (`readAsset(J[BII)I`, `seekAsset(JJI)J`, `getAssetLength(J)J`, `getAssetRemainingLength(J)J`,
// `destroyAsset(J)V`) are registered best-effort ([`register_asset_stream_natives`]) so a sig drift on
// the read cycle is logged + discovered, never breaking the main AssetManager natives.
const ASSET_MANAGER_OPEN_ASSET_NAME: &JNIStr = jni_str!("openAsset");
const ASSET_MANAGER_OPEN_ASSET_SIG: &JNIStr = jni_str!("(Ljava/lang/String;I)J");
const ASSET_MANAGER_READ_ASSET_NAME: &JNIStr = jni_str!("readAsset");
// ATL's readAsset takes the off/len as `long` (run log 2026-06-11: vtable shows
// `readAsset(long, byte[], long, long)`, mangled `__J_3BJJ`), NOT the classic AOSP `(J[BII)I`.
const ASSET_MANAGER_READ_ASSET_SIG: &JNIStr = jni_str!("(J[BJJ)I");
const ASSET_MANAGER_SEEK_ASSET_NAME: &JNIStr = jni_str!("seekAsset");
const ASSET_MANAGER_SEEK_ASSET_SIG: &JNIStr = jni_str!("(JJI)J");
const ASSET_MANAGER_GET_ASSET_LENGTH_NAME: &JNIStr = jni_str!("getAssetLength");
const ASSET_MANAGER_GET_ASSET_LENGTH_SIG: &JNIStr = jni_str!("(J)J");
const ASSET_MANAGER_GET_ASSET_REMAINING_LENGTH_NAME: &JNIStr = jni_str!("getAssetRemainingLength");
const ASSET_MANAGER_GET_ASSET_REMAINING_LENGTH_SIG: &JNIStr = jni_str!("(J)J");
const ASSET_MANAGER_DESTROY_ASSET_NAME: &JNIStr = jni_str!("destroyAsset");
const ASSET_MANAGER_DESTROY_ASSET_SIG: &JNIStr = jni_str!("(J)V");

// 2026-06-05: `loadResourceValue(int resid, short density, TypedValue outValue, boolean resolveRefs)`
// is the native AOSP's `AssetManager.getResourceValue`/`Resources.getValue` calls to resolve a
// resource id into a `Res_value` written onto a `TypedValue`. Surfaced by the dev-host run during
// step 5 (`setContentView` → `LayoutInflater.inflate` → `getLayout` → `loadXmlResourceParser` →
// `Resources.getValue` → `AssetManager.getResourceValue` → `loadResourceValue`). AssetManager is
// DENYLISTED, so this is bound from the exact JNI signature ART reported missing (`No implementation
// found for int android.content.res.AssetManager.loadResourceValue(int, short,
// android.util.TypedValue, boolean)`, mangled `...__ISLandroid_util_TypedValue_2Z`, run log
// 2026-06-05) WITHOUT reading the class's source. JNI descriptor `(ISLandroid/util/TypedValue;Z)I`.
// Backed by Eclipse's own `resources.arsc` reader: resolves the id and writes type/data (+ the
// resolved string for a `TYPE_STRING`, e.g. a layout file path) onto the public `TypedValue` fields.
// Returns the asset cookie (1) on success, 0 if the id is absent (the framework treats 0 as
// not-found — correct, not a fake value).
const ASSET_MANAGER_LOAD_RESOURCE_VALUE_NAME: &JNIStr = jni_str!("loadResourceValue");
const ASSET_MANAGER_LOAD_RESOURCE_VALUE_SIG: &JNIStr = jni_str!("(ISLandroid/util/TypedValue;Z)I");

// 2026-06-05: `loadThemeAttributeValue(long theme, int ident, TypedValue outValue, boolean
// resolveRefs)` is the native AOSP's `AssetManager.getThemeValue` / `Resources$Theme.resolveAttribute`
// calls to resolve a THEME attribute id (`?attr/foo`) against an applied theme into a `TypedValue`.
// Surfaced by the dev-host run during step 5 (accelerometerdemo's `setContentView` →
// `AppCompatDelegateImplV9.createSubDecor` → `Theme.resolveAttribute` → `AssetManager.getThemeValue`).
// AssetManager is DENYLISTED, so this is bound from the exact JNI signature ART reported missing (`No
// implementation found for int android.content.res.AssetManager.loadThemeAttributeValue(long, int,
// android.util.TypedValue, boolean)`, mangled `...__JILandroid_util_TypedValue_2Z`, run log
// 2026-06-05) WITHOUT reading the class's source. JNI descriptor `(JILandroid/util/TypedValue;Z)I` —
// an INSTANCE native. Backed by the theme handle's merged attribute map (built by applyThemeStyle):
// resolves `ident` via the same [`resolve_theme_attr`] reference-chasing the styled-attribute path
// uses and writes type/data/resourceId onto the public `TypedValue` fields. Returns the asset cookie
// (1) when the attribute is in the theme, 0 when absent (the framework treats 0 as not-resolved —
// correct, not a fake value).
const ASSET_MANAGER_LOAD_THEME_ATTRIBUTE_VALUE_NAME: &JNIStr = jni_str!("loadThemeAttributeValue");
const ASSET_MANAGER_LOAD_THEME_ATTRIBUTE_VALUE_SIG: &JNIStr =
    jni_str!("(JILandroid/util/TypedValue;Z)I");

/// `Res_value.dataType` for a string-pool reference (`TYPE_STRING`); its `data` is a value-pool index.
const RES_VALUE_TYPE_STRING: u8 = 0x03;
/// The single asset cookie Eclipse reports (one APK). `loadResourceValue` returns it on success.
const ECLIPSE_ASSET_COOKIE: jint = 1;

// === ATL TypedArray ABI: the per-attribute window layout the styled-attribute natives write ========
//
// 2026-06-05: ATL reuses the **standard AOSP (API 29+) `TypedArray` window layout** unchanged — the
// per-attribute window is `STYLE_NUM_ENTRIES = 7` ints: `[TYPE(0), DATA(1), ASSET_COOKIE(2),
// RESOURCE_ID(3), CHANGING_CONFIGURATIONS(4), DENSITY(5), SOURCE_RESOURCE_ID(6)]`. This was confirmed
// **empirically** from the dev-host run (a benign, allowed observation) WITHOUT reading the denylisted
// `TypedArray.java`/`AssetManager.java` source, then corroborated by reading the runtime framework's
// own `com.android.internal.R$styleable.View_id` constant (= 9) via reflection:
//
//   • The launcher inflates a `<TextView android:id="@id/0x7f030000">`; `LayoutInflater` +
//     `View.<init>` read the id via `TypedArray.getResourceId(View_id, NO_ID)` and call `setId`, which
//     is what `findViewById` later matches. With the wrong layout, `getResourceId` returned `NO_ID`,
//     so `findViewById(0x7f030000)` was `null` → `setText` NPE at `MainActivity.onCreate:16`.
//   • Probing the stride: writing the id into the full window at index `View_id * S` for stride `S`
//     and observing the NPE clear pinned `S = 7` (only S=7 made `getResourceId` resolve; S=6 did not).
//   • Probing the slots within the stride-7 window: only `TYPE@0` + `RESOURCE_ID@3` cleared the NPE
//     (TYPE@1/DATA@1/RESID@4 etc. did not). `DATA@1` follows from the standard layout and is what
//     `getInteger`/`getString` read (manifest integers + `<activity android:name>` resolve with it).
//
// The styled-attribute natives (`applyStyle`, `retrieveAttributes`) write into the SAME framework
// `int[]` indexed by the styleable position; `outValues` is sized `attrs.length * STYLE_NUM_ENTRIES`.
// The accessor-read slots are TYPE, DATA, and (for references like `android:id`) RESOURCE_ID; cookie /
// changing-config / density / source stay at the framework's zero pre-fill (not consumed here). A
// `TYPE_STRING` value's DATA is the XmlBlock string-pool index, resolved by `getString` via the XML
// string pool (cookie slot = 0 routes to `mXml.getPooledString(data)`, satisfied by the already-bound
// XML natives — no new native needed).
//
// THE ONE ABI ASSUMPTION (faithful): the run-confirmed `STYLE_NUM_ENTRIES = 7` stride with TYPE@0 /
// DATA@1 / RESOURCE_ID@3. A regression here would mis-place the entries; the offsets are pinned by
// `typed_array_window_layout_is_pinned` + the `fill_typed_array` bounds test below so a change fails loudly.

/// AOSP `TypedArray` per-attribute window stride in `outValues` (run-confirmed 2026-06-05; standard
/// AOSP API 29+ layout — see the ABI note above).
const STYLE_NUM_ENTRIES: usize = 7;
/// Offset of the `TypedValue.TYPE_*` byte within an attribute's window (AOSP = 0).
const STYLE_TYPE: usize = 0;
/// Offset of the `Res_value.data` word within an attribute's window (AOSP = 1). For a `TYPE_STRING`
/// this is the XmlBlock string-pool index `getString` resolves via the XML string pool.
const STYLE_DATA: usize = 1;
/// Offset of the asset cookie within an attribute's window (AOSP = 2). For an XML-block-sourced string
/// value (a manifest/layout inline attribute) this is set to [`XML_BLOCK_COOKIE`] (`-1`) so
/// `TypedArray.getString` resolves the value via `mXml.getPooledString(data)` (the XmlBlock's own Java
/// string pool, backed by the bound XML natives) rather than the native `AssetManager.getPooledString`.
const STYLE_ASSET_COOKIE: usize = 2;
/// Offset of the resolved resource id within an attribute's window (AOSP = 3) — what
/// `TypedArray.getResourceId` returns (e.g. `android:id` → the view's id for `findViewById`).
const STYLE_RESOURCE_ID: usize = 3;
/// The asset cookie AOSP's `TypedArray.getString` treats as "string lives in the XmlBlock's own pool"
/// (`cookie < 0`) — routing resolution to `mXml.getPooledString(data)` in Java (no native needed),
/// since Eclipse's inline string values come from the parsed XML block, not a separate asset.
const XML_BLOCK_COOKIE: i32 = -1;
/// `TypedValue.TYPE_NULL` — "no value" (the framework then uses the attribute's default). Written
/// into a requested attribute's `STYLE_TYPE` slot when that id is absent from the current tag.
const TYPE_NULL: i32 = 0;
/// `TypedValue.TYPE_REFERENCE` — a resource reference (`@id/foo`, `@drawable/bar`); its `data` is the
/// referenced resource id, which is also placed in the `STYLE_RESOURCE_ID` slot.
const TYPE_REFERENCE: u8 = 0x01;
/// `TypedValue.TYPE_ATTRIBUTE` — a theme-attribute reference (`?attr/foo`); like a reference for the
/// purpose of the `STYLE_RESOURCE_ID` slot.
const TYPE_ATTRIBUTE: u8 = 0x02;
/// `TypedValue.TYPE_STRING` — an interned string; its `data` is the source string-pool index, resolved
/// by `getString` via the XmlBlock pool (cookie [`XML_BLOCK_COOKIE`]).
const TYPE_STRING: u8 = 0x03;

/// `AssetManager.init(int sdk_version)` → GTK-free no-op (minimal stub, 2026-06-05).
///
/// JNI ABI: an INSTANCE native (the Java method is not `static`), so the second argument is the
/// `JObject` receiver (`this`), then the `int sdk_version` (the resources SDK version the
/// constructor passes). Per the AssetManager native-backing note above, this is intentionally a
/// no-op for now: it leaves the receiver's `mObject` native handle at `0` (Java zero-init) so the
/// AssetManager constructs without pulling ATL's C asset layer, and the re-run surfaces the next
/// native (`native_setApkAssets`) — the diagnostic discovery loop, not a behavior fake.
///
/// The body runs inside [`EnvUnowned::with_env`], which `catch_unwind`-wraps it so a Rust panic can
/// never unwind into ART's C++ (AGENTS.md §2.8; `panic = "abort"` kept). `resolve::<LogErrorAndDefault>`
/// returns the `()` default on any error/panic — the correct neutral value for this `void` native.
extern "system" fn asset_manager_init<'local>(
    mut env: EnvUnowned<'local>,
    _this: JObject<'local>,
    sdk_version: jint,
) {
    env.with_env(|_env| -> jni::errors::Result<()> {
        // 2026-06-05: minimal stub — no native asset table is created yet. Tracing records the call
        // (and the SDK version) so the dev-host boot log shows the constructor reached `init`.
        tracing::debug!(
            target: "android.content.res.AssetManager",
            sdk_version,
            "AssetManager.init: GTK-free no-op (native asset table deferred; mObject stays 0)"
        );
        Ok(())
    })
    .resolve::<LogErrorAndDefault>()
}

/// `AssetManager.native_setApkAssets(Object[] apkAssets, int invalidateCaches)` → GTK-free no-op
/// (signature-only discovery-loop stub, 2026-06-05).
///
/// JNI ABI: an INSTANCE native, so the parameters are `(EnvUnowned, JObject this, JObject apkAssets,
/// jint invalidateCaches)`. `apkAssets` is a `java.lang.Object[]`; it is a `jobject` at the ABI
/// level, taken as a `JObject` and **never dereferenced** (AssetManager is DENYLISTED — bound from
/// the ART-reported signature alone, without reading its source). No native asset table is installed
/// (`mObject` stays `0`); this lets the constructor proceed past `native_setApkAssets` so the re-run
/// surfaces the next native — the diagnostic discovery loop, not behavior-faking.
///
/// The body runs inside [`EnvUnowned::with_env`], which `catch_unwind`-wraps it so a Rust panic can
/// never unwind into ART's C++ (AGENTS.md §2.8; `panic = "abort"` kept). `resolve::<LogErrorAndDefault>`
/// returns the `()` default on any error/panic — the correct neutral value for this `void` native.
extern "system" fn asset_manager_set_apk_assets<'local>(
    mut env: EnvUnowned<'local>,
    _this: JObject<'local>,
    _apk_assets: JObject<'local>,
    invalidate_caches: jint,
) {
    env.with_env(|_env| -> jni::errors::Result<()> {
        // 2026-06-05: signature-only no-op (AssetManager denylisted). `_apk_assets` is intentionally
        // not inspected. Tracing records the call so the dev-host log shows the constructor reached it.
        tracing::debug!(
            target: "android.content.res.AssetManager",
            invalidate_caches,
            "AssetManager.native_setApkAssets: GTK-free no-op (asset table deferred; mObject stays 0)"
        );
        Ok(())
    })
    .resolve::<LogErrorAndDefault>()
}

/// `AssetManager.setConfiguration(int, int, String, int ×14)` → GTK-free no-op (signature-only
/// discovery-loop stub, 2026-06-05).
///
/// JNI ABI: an INSTANCE native, so the parameters are `(EnvUnowned, JObject this, ...17 args)`:
/// 2 ints, a `String` locale, then 14 configuration ints. The `String` is a `jobject` at the ABI
/// level, taken as a `JObject` and **never dereferenced** (AssetManager is DENYLISTED — bound from
/// the ART-reported signature alone, without reading its source). No native asset table exists
/// (`mObject` stays `0`), so there is nothing to reconfigure; this lets the framework proceed so the
/// re-run surfaces the next native — the diagnostic discovery loop, not behavior-faking.
///
/// The body runs inside [`EnvUnowned::with_env`], which `catch_unwind`-wraps it so a Rust panic can
/// never unwind into ART's C++ (AGENTS.md §2.8; `panic = "abort"` kept). `resolve::<LogErrorAndDefault>`
/// returns the `()` default on any error/panic — the correct neutral value for this `void` native.
//
// 2026-06-05: arity is fixed by the JNI signature ART reported (17 args); a signature-only stub must
// match it exactly. clippy's `too_many_arguments` does not fire on `extern "system"` fns, so no
// `#[expect]`/`#[allow]` is needed here.
extern "system" fn asset_manager_set_configuration<'local>(
    mut env: EnvUnowned<'local>,
    _this: JObject<'local>,
    mcc: jint,
    mnc: jint,
    _locale: JObject<'local>,
    _orientation: jint,
    _touchscreen: jint,
    _density: jint,
    _keyboard: jint,
    _keyboard_hidden: jint,
    _navigation: jint,
    _screen_width: jint,
    _screen_height: jint,
    _smallest_screen_width_dp: jint,
    _screen_width_dp: jint,
    _screen_height_dp: jint,
    _screen_layout: jint,
    _ui_mode: jint,
    _major_version: jint,
) {
    env.with_env(|_env| -> jni::errors::Result<()> {
        // 2026-06-05: signature-only no-op (AssetManager denylisted). The arg names mirror the
        // standard AOSP AssetManager.setConfiguration parameter order for documentation only; none
        // are inspected. `mcc`/`mnc` are traced as a low-noise call marker for the dev-host log.
        tracing::debug!(
            target: "android.content.res.AssetManager",
            mcc,
            mnc,
            "AssetManager.setConfiguration: GTK-free no-op (no native asset table; mObject stays 0)"
        );
        Ok(())
    })
    .resolve::<LogErrorAndDefault>()
}

/// `AssetManager.openXmlAssetNative(int cookie, String fileName)` → a real Eclipse-owned
/// [`xml_registry`] block handle (2026-06-05), or `0` on a genuine open failure.
///
/// JNI ABI: an INSTANCE native returning `jlong`, so the parameters are
/// `(EnvUnowned, JObject this, jint cookie, JString fileName)`. Reads `fileName` from the APK zip
/// (path stashed in [`APK_PATH`]) via [`crate::apk::Apk::read_entry`], parses it with
/// [`crate::apk::axml::parse_document`], stores the parsed [`crate::apk::axml::XmlDocument`] in
/// [`xml_registry`], and returns the slab handle (≥ 1, never `0`). A missing entry or parse failure
/// returns `0` — the "no asset" sentinel the framework turns into `FileNotFoundException` (correct
/// behavior, not a fake success). `cookie` is the APK-set index; Eclipse keys assets by the single
/// stashed APK path, so it is logged but not used to select an archive.
///
/// The body runs inside [`EnvUnowned::with_env`], which `catch_unwind`-wraps it so a Rust panic can
/// never unwind into ART's C++ (AGENTS.md §2.8; `panic = "abort"` kept). `resolve::<LogErrorAndDefault>`
/// returns the `jlong` default (`0`) on any error/panic — the same neutral "no asset" handle.
extern "system" fn asset_manager_open_xml_asset<'local>(
    mut env: EnvUnowned<'local>,
    _this: JObject<'local>,
    cookie: jint,
    file_name: JString<'local>,
) -> jlong {
    env.with_env(|env| -> jni::errors::Result<jlong> {
        if file_name.is_null() {
            // A null asset name cannot name an entry; return the "no asset" sentinel (the framework
            // throws FileNotFoundException), never a panic.
            return Ok(0);
        }
        let name = file_name.try_to_string(env)?;
        match open_xml_block(&name) {
            Ok(handle) => {
                tracing::debug!(
                    target: "android.content.res.AssetManager",
                    cookie,
                    asset = %name,
                    handle,
                    "AssetManager.openXmlAssetNative: parsed XML asset from APK (Eclipse axml)"
                );
                Ok(handle)
            }
            Err(e) => {
                // Genuine failure (entry absent, not binary-XML, malformed, or no APK path). Return
                // 0 so the framework raises FileNotFoundException — the correct, non-faked outcome.
                tracing::warn!(
                    target: "android.content.res.AssetManager",
                    cookie,
                    asset = %name,
                    error = %e,
                    "AssetManager.openXmlAssetNative: could not open XML asset → 0 (FileNotFound)"
                );
                Ok(0)
            }
        }
    })
    .resolve::<LogErrorAndDefault>()
}

/// `AssetManager.retrieveAttributes(long parseState, int[] attrs, int parser, long outValues,
/// long outIndices)` → whether any requested attribute was found on the current XML tag.
///
/// JNI ABI: an INSTANCE native returning `jboolean` (jni-sys `jboolean` = Rust `bool`), so the
/// parameters are `(EnvUnowned, JObject this, jlong parse_state, JIntArray attrs, jint parser,
/// jlong out_values, jlong out_indices)`. `out_values`/`out_indices` are raw pointers to native
/// off-heap `int[]` buffers the framework's `TypedArray` allocated and sized.
///
/// ## What this resolves (real XML-attribute extraction — no ARSC needed)
/// 2026-06-05: this is AOSP's *XML-attribute* `retrieveAttributes` (the variant the framework drives
/// while reading the manifest's `<activity>`/`<service>`/… tags), NOT the theme/style `applyStyle`
/// path. `attrs` is a list of **framework attribute resource ids** (e.g. `android.R.attr.name` =
/// `0x01010003`); for each, the native looks up the attribute on the current parse-state's XML
/// element whose decoded `name_resource` equals that id and writes its **inline `Res_value`**
/// (`value_type` + `value_data`) into that attribute's [`STYLE_NUM_ENTRIES`]-wide `outValues` window.
/// Those values are already decoded by Eclipse's own [`axml`](crate::apk::axml) parser
/// (`XmlAttribute.{name_resource,value_type,value_data}`), so **no `resources.arsc` decode is
/// required** — manifest attribute values are inline in the AXML and their ids come from the AXML
/// resource-map chunk. A minimal first pass that returned `TYPE_NULL` for every attribute made the
/// framework log "`<activity> does not specify android:name`" and `System.exit(1)` (run log
/// 2026-06-05), proving real per-attribute values are required here; this is the smallest sound step
/// that supplies them, grounded in data Eclipse already parses (not a new subsystem).
///
/// For each requested id the window's run-confirmed slots are filled: `STYLE_TYPE` (ATL offset 1) =
/// the value's `Res_value.dataType` (the same byte as `TypedValue.TYPE_*`), `STYLE_DATA` (ATL offset
/// 3) = the value's `Res_value.data` word (for a `TYPE_STRING` this is the XmlBlock string-pool
/// index). The remaining slots stay at the framework's zero pre-fill (their exact ATL offsets are not
/// yet confirmed). A requested id not present on the tag gets `STYLE_TYPE = TYPE_NULL` (the framework
/// then uses the attribute's default). `outIndices[0]` is the count of attributes that were found, and
/// `outIndices[1..=count]` are their 1-based positions in `attrs`. The return is `true` iff at least
/// one attribute was found. Both integer/boolean attributes (the boot advances past
/// `PackageParser.parsePackage`) AND `String`-valued attributes (e.g. `<activity android:name>`)
/// resolve: `TypedArray.getString` reads the string-pool index from DATA@3 (cookie slot = 0) and
/// resolves it via the XmlBlock string pool — satisfied by the already-bound XML natives, no new
/// native needed (run-confirmed 2026-06-05; see the `STYLE_*` constants' note).
///
/// ## Bounds soundness (the raw-pointer writes)
/// `out_values`/`out_indices` are written via `*mut i32` derived from the `jlong`s. The writes are
/// provably in bounds: the framework's `TypedArray` sizes `outValues` to `attrs.length *
/// STYLE_NUM_ENTRIES` ints and `outIndices` to `attrs.length + 1` ints (the AOSP TypedArray ABI), and
/// this native writes **only** offsets `< n * STYLE_NUM_ENTRIES` (outValues) and `<= n` (outIndices),
/// where `n = attrs.len()`. A `0` pointer means the framework provided no buffer; that buffer is then
/// skipped (no write). See [`fill_typed_array`] for the (`unsafe`) writes and their SAFETY argument.
///
/// The body runs inside [`EnvUnowned::with_env`], which `catch_unwind`-wraps it so a Rust panic can
/// never unwind into ART's C++ (AGENTS.md §2.8; `panic = "abort"` kept). `resolve::<LogErrorAndDefault>`
/// returns the `jboolean` default (`false`) on any error/panic — the same neutral "nothing resolved".
extern "system" fn asset_manager_retrieve_attributes<'local>(
    mut env: EnvUnowned<'local>,
    _this: JObject<'local>,
    parse_state: jlong,
    attrs: JIntArray<'local>,
    parser: jint,
    out_values: jlong,
    out_indices: jlong,
) -> jboolean {
    env.with_env(|env| -> jni::errors::Result<jboolean> {
        // Number of requested attributes — the single value that sizes both output buffers. A null
        // attrs array means nothing to resolve; honestly return false (no buffer write).
        if attrs.is_null() {
            return Ok(false);
        }
        let n = attrs.len(env)?;
        if n == 0 {
            // No requested ids: outIndices[0] = 0 (no entries), nothing in outValues. Still write the
            // count so the framework reads a defined value.
            fill_typed_array(out_values, out_indices, &[]);
            return Ok(false);
        }

        // Copy the requested framework attribute ids out of the Java int[] into a Rust buffer. A
        // jsize start of 0 + the array's own length is exactly in range (get_region bounds-checks).
        let mut ids = vec![0i32; n];
        let start = jint::try_from(0).unwrap_or(0);
        attrs.get_region(env, start, &mut ids)?;

        // Resolve each requested id against the current XML element's decoded attributes (by
        // name_resource). Build the per-attribute TypedArray windows; this reads only Eclipse's own
        // parsed axml data via the bounds+generation-checked registry (a bad parse_state handle is a
        // typed Err → no entries resolved, never UB).
        let entries = resolve_xml_attributes(parse_state, &ids);
        let changed = entries.iter().filter(|e| e.is_some()).count();

        // Write the windows + the changed-index list into the framework's off-heap buffers (bounded
        // to exactly the AOSP-sized regions; a 0 pointer is skipped). See fill_typed_array's SAFETY.
        fill_typed_array(out_values, out_indices, &entries);

        tracing::debug!(
            target: "android.content.res.AssetManager",
            parse_state,
            parser,
            attrs = n,
            changed,
            out_values_null = (out_values == 0),
            out_indices_null = (out_indices == 0),
            "AssetManager.retrieveAttributes: resolved manifest XML attributes by resource id"
        );
        // true iff at least one requested attribute was present on the tag.
        Ok(changed > 0)
    })
    .resolve::<LogErrorAndDefault>()
}

/// One resolved `Res_value` to place in a [`STYLE_NUM_ENTRIES`]-wide `outValues` window: the
/// `TypedValue.TYPE_*` code, the data word, and the resolved resource id (for references). `None` for
/// a requested id absent from the tag.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
struct TypedEntry {
    /// `Res_value.dataType` (== `TypedValue.TYPE_*`).
    value_type: i32,
    /// `Res_value.data` (for a string, the XmlBlock string-pool index).
    data: i32,
    /// The value's resolved resource id for the `STYLE_RESOURCE_ID` slot. For a `TYPE_REFERENCE` /
    /// `TYPE_ATTRIBUTE` (e.g. `android:id="@id/foo"`) this is the referenced id (== `data`), which is
    /// what `TypedArray.getResourceId` returns; for every other value type it is `0` (no resource id).
    resource_id: i32,
    /// The asset cookie for the `STYLE_ASSET_COOKIE` slot. [`XML_BLOCK_COOKIE`] (`-1`) for a
    /// `TYPE_STRING` (so `getString` resolves via the XmlBlock's own Java pool); `0` otherwise.
    asset_cookie: i32,
}

/// For each requested framework attribute id in `ids`, find the matching attribute on the current
/// element of XML parse-state `parse_state` (by decoded `name_resource`) and return its `Res_value`,
/// or `None` if the id is not present on the tag.
///
/// Reads only Eclipse's own parsed [`axml`](crate::apk::axml) data through the bounds+generation-
/// checked [`xml_registry::with_block`]: a stale/fabricated `parse_state` handle is a typed `Err`,
/// which yields all-`None` (no entries) — never a wild dereference or panic. Allocates one
/// `Vec<Option<TypedEntry>>` sized to `ids` (the launcher resolves a handful of attribute sets; this
/// is off the gameplay hot path).
fn resolve_xml_attributes(parse_state: jlong, ids: &[i32]) -> Vec<Option<TypedEntry>> {
    xml_registry::with_block(parse_state, |block| {
        let element = block.current_element();
        ids.iter()
            .map(|&id| {
                let element = element?;
                // Framework attribute ids are non-zero; a 0 here means "not a framework resource
                // attribute", which never matches a real requested id.
                let id_u32 = u32::from_ne_bytes(id.to_ne_bytes());
                let attr = element
                    .attributes
                    .iter()
                    .find(|a| a.name_resource != 0 && a.name_resource == id_u32)?;
                Some(resolve_inline_attr_value(attr.value_type, attr.value_data))
            })
            .collect()
    })
    .unwrap_or_else(|_| vec![None; ids.len()])
}

/// Resolve one inline XML attribute `(value_type, value_data)` to the [`TypedEntry`] that
/// `applyStyle`/`obtainStyledAttributes(AttributeSet, int[])` writes into the `TypedArray` window.
///
/// 2026-06-05: a concrete value (color/int/float/dimension/boolean/string) is returned directly. A
/// `TYPE_REFERENCE` (`@color/x`, `@drawable/y`, …) is FOLLOWED into `resources.arsc` to a concrete
/// `Res_value` (so e.g. a vector drawable's `android:fillColor="@color/c"` resolves to its ARGB
/// before `TypedArray.getColor` reads it — without this, `getColor` throws
/// `UnsupportedOperationException: Can't convert to color: type=0x1`, surfaced 2026-06-05 by
/// accelerometerdemo's `VectorDrawableCompat`). The original referenced id is kept in the
/// `STYLE_RESOURCE_ID` slot (what `getResourceId` returns). This mirrors [`resolve_theme_attr`]'s
/// reference chase, fixing the same-pattern gap where the THEME path resolved references but the
/// inline-XML path did not. Bounded by [`MAX_ATTR_RESOLVE_DEPTH`]; never panics. An unresolvable
/// reference (`@null`, a bag, or an absent target) keeps the reference itself (its resource id stays
/// useful to `getResourceId`) — the sound AOSP fallback, not a value fake.
fn resolve_inline_attr_value(value_type: u8, value_data: u32) -> TypedEntry {
    let mut cur_type = value_type;
    let mut cur_data = value_data;
    // getResourceId reports the FIRST referenced id (for a reference/attribute); 0 otherwise.
    let resource_id = if value_type == TYPE_REFERENCE || value_type == TYPE_ATTRIBUTE {
        u32_to_i32(value_data)
    } else {
        0
    };
    // Follow resource references (`@…`) to a concrete value. `TYPE_ATTRIBUTE` (`?attr/…`) has no
    // theme context here (this is an inline AttributeSet value, not a theme bag), so it is left as-is
    // for the framework to resolve against the active theme.
    for _ in 0..MAX_ATTR_RESOLVE_DEPTH {
        if cur_type != TYPE_REFERENCE || cur_data == 0 {
            break; // concrete value, a non-reference, or the explicit @null reference: done.
        }
        match resolve_res_value(cur_data) {
            Some(v) => {
                cur_type = u8::try_from(v.type_).unwrap_or(0);
                cur_data = u32::from_ne_bytes(v.data.to_ne_bytes());
            }
            // Target is a bag / absent: keep the reference itself (resource_id still set).
            None => break,
        }
    }
    // A string value lives in the XmlBlock's own pool; the XML_BLOCK_COOKIE routes getString to
    // mXml.getPooledString(data) in Java (no native). Other types: cookie 0.
    let asset_cookie = if cur_type == TYPE_STRING {
        XML_BLOCK_COOKIE
    } else {
        0
    };
    TypedEntry {
        value_type: i32::from(cur_type),
        data: u32_to_i32(cur_data),
        resource_id,
        asset_cookie,
    }
}

/// Reinterpret a `u32` `Res_value.data` word as the `i32` the TypedArray `int[]` stores (bit-for-bit;
/// the framework reads it back as the same 32 bits). `as` would also work, but `from_ne_bytes` makes
/// the bit-preservation explicit and lint-clean.
fn u32_to_i32(v: u32) -> i32 {
    i32::from_ne_bytes(v.to_ne_bytes())
}

/// Maximum theme parent-chain depth walked by [`merge_theme_style`]. Real Material/AppCompat chains
/// are ~6–8 deep (verified 2026-06-05: the accelerometer demo's AppTheme chain is 7 styles); this cap
/// (2026-06-05) sits well above any legitimate depth while bounding work and breaking any cycle a
/// malformed/hostile table might encode.
const MAX_THEME_PARENT_DEPTH: usize = 64;

/// Merge a style resource id's bag + its parent chain into `out` (attribute id → resolved value),
/// child overriding parent. Returns the number of attributes the chain contributed.
///
/// 2026-06-05: an AOSP theme is a `<style>` (a `resources.arsc` bag of attribute id → `Res_value`,
/// plus a parent style id). The activity's theme (from the manifest `android:theme` or the AppCompat
/// default) is applied via `applyThemeStyle(styleRes)`; resolving the theme's full attribute set —
/// which `obtainStyledAttributes(int[])` reads — requires walking the parent chain so AppCompat's own
/// attributes (`windowActionBar`/`colorPrimary`/…), defined up the chain in the app's bundled
/// AppCompat resources, are present. Walk from the applied style UPWARD: insert each attribute only if
/// absent, so the more-specific (child) value wins over the parent's. Parents can cross packages
/// (e.g. the app theme's chain ends in a framework `android:Theme.*`, package `0x01`), so each node is
/// read through [`arsc_bytes_for`] (framework table for package `0x01`, app table otherwise).
///
/// Total + bounded: [`MAX_THEME_PARENT_DEPTH`] caps the walk (breaking any cycle), a node whose ARSC
/// is missing/corrupt/absent simply ends the walk, and the underlying [`apk::arsc`](crate::apk::arsc)
/// decode is itself never-panicking. Re-parses the ARSC per node (off the gameplay hot path — themes
/// are set up once during activity create).
fn merge_theme_style(
    out: &mut std::collections::HashMap<i32, theme_registry::ThemeAttr>,
    style_res: u32,
) -> usize {
    let mut contributed = 0usize;
    let mut current = style_res;
    let mut visited = std::collections::HashSet::new();
    for _ in 0..MAX_THEME_PARENT_DEPTH {
        if current == 0 || !visited.insert(current) {
            break; // no parent, or a cycle — stop.
        }
        let Some(bytes) = arsc_bytes_for(current) else {
            break; // the table for this node is unavailable — end the walk.
        };
        let Ok(table) = crate::apk::arsc::parse_arsc(&bytes) else {
            break;
        };
        let Some(style) = table.resolve_style(current) else {
            break; // not a style/bag (or absent) — nothing more to merge.
        };
        for entry in &style.entries {
            // attr_id 0 is not a real attribute; skip it (defensive against malformed bags).
            if entry.attr_id == 0 {
                continue;
            }
            let key = u32_to_i32(entry.attr_id);
            // Insert only if absent → the child (seen first, walking upward) overrides the parent.
            out.entry(key).or_insert_with(|| {
                contributed += 1;
                theme_registry::ThemeAttr {
                    type_: entry.type_,
                    data: entry.data,
                }
            });
        }
        current = style.parent_id;
    }
    contributed
}

/// Maximum reference-chase depth when resolving a theme attribute's value to a concrete one
/// (`TYPE_REFERENCE`/`TYPE_ATTRIBUTE` hops). Real chains are 1–3 deep; this cap (2026-06-05) bounds
/// work and breaks any cycle.
const MAX_ATTR_RESOLVE_DEPTH: usize = 16;

/// Resolve one theme attribute id to the [`TypedEntry`] `obtainStyledAttributes(int[])` writes.
///
/// 2026-06-05: looks `attr_id` up in the theme's merged attribute map (`attrs`). For a concrete value
/// (boolean/int/color/dimension/…) returns it directly. For a `TYPE_REFERENCE` (`@id/@color/…`),
/// follows it into `resources.arsc` to a concrete `Res_value` (so e.g. `colorPrimary → @color/x →
/// ARGB` resolves) while keeping the original referenced id in the `STYLE_RESOURCE_ID` slot (what
/// `getResourceId` returns). For a `TYPE_ATTRIBUTE` (`?attr/foo`), looks the referenced attribute back
/// up in the SAME theme map (one theme-indirection hop). Returns `None` when the attribute is not in
/// the theme — the caller then writes `TYPE_NULL` (the framework uses the attribute's default), the
/// sound AOSP fallback, not a value fake. Bounded by [`MAX_ATTR_RESOLVE_DEPTH`]; never panics.
fn resolve_theme_attr(
    attrs: &std::collections::HashMap<i32, theme_registry::ThemeAttr>,
    attr_id: i32,
) -> Option<TypedEntry> {
    let mut cur = *attrs.get(&attr_id)?;
    // The resource id reported by getResourceId: for a reference, the FIRST referenced id.
    let mut resource_id = if cur.type_ == TYPE_REFERENCE {
        u32_to_i32(cur.data)
    } else {
        0
    };
    for _ in 0..MAX_ATTR_RESOLVE_DEPTH {
        match cur.type_ {
            // A theme-attribute reference (`?attr/foo`): re-resolve against the theme map.
            TYPE_ATTRIBUTE => {
                let next_id = u32_to_i32(cur.data);
                cur = *attrs.get(&next_id)?;
                if cur.type_ == TYPE_REFERENCE {
                    resource_id = u32_to_i32(cur.data);
                }
            }
            // A resource reference (`@color/x`): follow into resources.arsc to a concrete value.
            TYPE_REFERENCE => {
                // A 0 reference (`@null` / @0) is the explicit null value: keep it as a reference
                // with no concrete target (getResourceId returns 0).
                if cur.data == 0 {
                    break;
                }
                match resolve_res_value(cur.data) {
                    Some(v) => {
                        cur = theme_registry::ThemeAttr {
                            type_: u8::try_from(v.type_).unwrap_or(0),
                            data: u32::from_ne_bytes(v.data.to_ne_bytes()),
                        };
                        // If it points to ANOTHER reference, keep chasing and update resource_id.
                        if cur.type_ == TYPE_REFERENCE {
                            resource_id = v.data;
                        }
                    }
                    // The reference target is not a single value (a bag) or is absent: keep the
                    // reference itself (its resource id is still useful to getResourceId).
                    None => break,
                }
            }
            // A concrete value: done.
            _ => break,
        }
    }
    let asset_cookie = if cur.type_ == TYPE_STRING {
        XML_BLOCK_COOKIE
    } else {
        0
    };
    Some(TypedEntry {
        value_type: i32::from(cur.type_),
        data: u32_to_i32(cur.data),
        resource_id,
        asset_cookie,
    })
}

/// Resolve the requested attribute ids against a theme handle's merged attribute map, returning the
/// per-attribute [`TypedEntry`]s `obtainStyledAttributes(int[])` (the no-parser path) writes.
///
/// 2026-06-05: this is the theme-only branch of AOSP's `applyStyle`/`Theme.obtainStyledAttributes` —
/// AppCompat's `theme.obtainStyledAttributes(R.styleable.AppCompatTheme)` drives it with `parser == 0`.
/// A stale/fabricated theme handle (or an empty theme) yields all-`None` (every attribute `TYPE_NULL`)
/// — never UB — which is what triggered AppCompat's IllegalStateException before themes resolved.
fn resolve_theme_attributes(theme: jlong, ids: &[i32]) -> Vec<Option<TypedEntry>> {
    theme_registry::with_theme(theme, |t| {
        ids.iter()
            .map(|&id| resolve_theme_attr(&t.attrs, id))
            .collect()
    })
    .unwrap_or_else(|_| vec![None; ids.len()])
}

/// In-place: resolve any `entries` slot still holding a `TYPE_ATTRIBUTE` (`?attr/foo`) inline-XML value
/// against the active `theme`'s merged attribute map.
///
/// 2026-06-05: an inline `AttributeSet` value can be a theme reference (`android:background="?attr/…"`).
/// [`resolve_xml_attributes`] records it as a `TYPE_ATTRIBUTE` `TypedEntry` whose `data` is the
/// referenced attribute id (it has no theme to resolve against). AOSP's `TypedArray.getDrawable`/
/// `getColor`/… throw `UnsupportedOperationException` on an unresolved `TYPE_ATTRIBUTE`, so this hop
/// must happen before the framework reads the slot. [`resolve_theme_attr`] looks the referenced
/// attribute id up in the theme map and resolves its value (chasing references), exactly as AOSP's
/// `Theme.resolveAttribute` does. A stale/empty theme, or an attribute the theme does not define,
/// leaves the slot unchanged (the faithful "not in theme" outcome — not a fabricated value).
fn resolve_inline_theme_refs(theme: jlong, entries: &mut [Option<TypedEntry>]) {
    let _ = theme_registry::with_theme(theme, |t| {
        for slot in entries.iter_mut() {
            if let Some(entry) = slot {
                if entry.value_type == i32::from(TYPE_ATTRIBUTE) {
                    if let Some(resolved) = resolve_theme_attr(&t.attrs, entry.data) {
                        *slot = Some(resolved);
                    }
                }
            }
        }
    });
}

/// Fill the framework-allocated `TypedArray` output buffers from `entries` (one per requested
/// attribute, in request order): each `Some` writes its value's [`STYLE_TYPE`]/[`STYLE_DATA`] and —
/// for a reference — [`STYLE_RESOURCE_ID`] slots of its [`STYLE_NUM_ENTRIES`]-wide window (the rest
/// stay at the framework's zero pre-fill), each `None` writes `TYPE_NULL` into its window's
/// `STYLE_TYPE` slot; `outIndices[0]` is set to the number of `Some` entries, followed by their 1-based
/// request positions.
///
/// `out_values`/`out_indices` are the raw `jlong` pointers the framework passed; `0` means the
/// framework provided no buffer and that buffer is skipped (no write). The writes are bounded to the
/// AOSP-sized regions: offsets `< n * STYLE_NUM_ENTRIES` for `outValues` and `<= n` for `outIndices`,
/// where `n == entries.len()`.
///
/// # Safety
/// 2026-06-05: this performs raw `*mut i32` writes, justified by the AOSP `TypedArray` ABI (which ATL
/// reuses unchanged): the framework's `TypedArray` allocates `outValues` with `attrs.length *
/// STYLE_NUM_ENTRIES` ints and `outIndices` with `attrs.length + 1` ints, and passes their base
/// addresses as these two `jlong`s; `n = entries.len()` here IS `attrs.length` (`entries` is built
/// one-per-`ids` entry, and `ids.len()` is `attrs.len()` from `JIntArray::len`). For `outValues` every
/// written offset is `attr * STYLE_NUM_ENTRIES + slot` with `attr < n` and `slot ∈ {STYLE_TYPE,
/// STYLE_DATA, STYLE_RESOURCE_ID} < STYLE_NUM_ENTRIES`, hence `< n * STYLE_NUM_ENTRIES`. For
/// `outIndices` the written offsets are `0` (the count) and `1..=changed` where `changed <= n`, hence
/// `<= n`. Both are strictly inside the framework's allocation — no out-of-bounds access. A `0` pointer
/// is treated as "no buffer" and never dereferenced. The ABI assumption (documented at the `STYLE_*`
/// constants and pinned by `typed_array_window_layout_is_pinned`) is the run-confirmed
/// `STYLE_NUM_ENTRIES = 7` / TYPE@0 / DATA@1 / RESOURCE_ID@3 layout. Each `i32` is written to a
/// `.add(k)`-offset of a `*mut i32`; the buffers are framework-owned native `int[]`s (4-byte aligned
/// by construction), so the writes are aligned and non-overlapping.
fn fill_typed_array(out_values: jlong, out_indices: jlong, entries: &[Option<TypedEntry>]) {
    // n == entries.len() is the framework's attrs.length (see the # Safety note); used implicitly as
    // the iteration bound below — every offset stays < n*STYLE_NUM_ENTRIES (values) or <= n (indices).
    if out_values != 0 {
        let base = out_values as usize as *mut i32;
        for (attr, entry) in entries.iter().enumerate() {
            let window = attr * STYLE_NUM_ENTRIES; // < n * STYLE_NUM_ENTRIES for attr < n.
            match entry {
                Some(e) => {
                    // SAFETY: window + STYLE_RESOURCE_ID <= window + (STYLE_NUM_ENTRIES-1) <
                    // (attr+1)*STYLE_NUM_ENTRIES <= n*STYLE_NUM_ENTRIES = the framework's outValues
                    // int-count (see the fn-level # Safety). `base` is non-null (checked) and points
                    // at that framework-owned, 4-byte-aligned int[]. TYPE/DATA/RESOURCE_ID/COOKIE are the
                    // accessor-read slots; the rest stay at the framework's zero pre-fill (the neutral
                    // default — changing-config/density/source are not consumed by the launcher).
                    unsafe {
                        base.add(window + STYLE_TYPE).write(e.value_type);
                        base.add(window + STYLE_DATA).write(e.data);
                        base.add(window + STYLE_RESOURCE_ID).write(e.resource_id);
                        base.add(window + STYLE_ASSET_COOKIE).write(e.asset_cookie);
                    }
                }
                None => {
                    // SAFETY: window + STYLE_TYPE < n*STYLE_NUM_ENTRIES (as above). TYPE_NULL marks
                    // the attribute absent; the framework then uses its default.
                    unsafe { base.add(window + STYLE_TYPE).write(TYPE_NULL) };
                }
            }
        }
    }

    if out_indices != 0 {
        let base = out_indices as usize as *mut i32;
        // outIndices[0] = number of attributes found; [1..=count] = their 1-based request positions
        // (AOSP packs only the changed indices). count <= n, so the last write is at offset count <= n,
        // strictly inside the n+1-int allocation.
        let mut count: i32 = 0;
        for (attr, entry) in entries.iter().enumerate() {
            if entry.is_some() {
                count += 1;
                // SAFETY: count <= attr+1 <= n, so `count` is a valid offset into the n+1-int buffer.
                // `base` is non-null (checked) and 4-byte-aligned by construction. The 1-based request
                // position (attr+1) fits i32 (attr < n <= i32 array length).
                let pos = i32::try_from(attr + 1).unwrap_or(i32::MAX);
                unsafe { base.add(count as usize).write(pos) };
            }
        }
        // SAFETY: offset 0 is within the n+1-int buffer (always >= 1 int). Written last so a found
        // attribute's index write above never clobbers the count.
        unsafe { base.write(count) };
    }
}

/// `AssetManager.newTheme()` → a real Eclipse-owned [`theme_registry`] theme handle (2026-06-05).
///
/// JNI ABI: an INSTANCE native returning `jlong`, so the parameters are `(EnvUnowned, JObject this)`.
/// Allocates a [`theme_registry`] slot and returns its slab handle (≥ 1, never `0`). The framework
/// wraps it as a `Resources$Theme`; later theme natives (`applyStyle`/`resolveAttributes`/
/// `releaseTheme`) look it up through the bounds+generation-checked registry. Returns `0` (no theme)
/// only on a registry error — which the framework treats as a failed theme create, never a fake
/// success.
///
/// The body runs inside [`EnvUnowned::with_env`] (`catch_unwind`-wrapped, AGENTS.md §2.8;
/// `panic = "abort"` kept); `resolve::<LogErrorAndDefault>` returns the `jlong` default (`0`) on any
/// error/panic — a sound neutral "no theme" handle.
extern "system" fn asset_manager_new_theme<'local>(
    mut env: EnvUnowned<'local>,
    _this: JObject<'local>,
) -> jlong {
    env.with_env(|_env| -> jni::errors::Result<jlong> {
        match theme_registry::allocate() {
            Ok(handle) => {
                tracing::debug!(
                    target: "android.content.res.AssetManager",
                    handle,
                    "AssetManager.newTheme: allocated non-GTK theme-registry handle"
                );
                Ok(handle)
            }
            Err(e) => {
                tracing::warn!(
                    target: "android.content.res.AssetManager",
                    error = %e,
                    "AssetManager.newTheme: theme-registry allocate failed → 0 (no theme)"
                );
                Ok(0)
            }
        }
    })
    .resolve::<LogErrorAndDefault>()
}

/// `AssetManager.applyThemeStyle(long theme, int styleRes, boolean force)` → record the applied
/// style id on the [`theme_registry`] theme (2026-06-05).
///
/// JNI ABI: an INSTANCE native returning void, so the parameters are
/// `(EnvUnowned, JObject this, jlong theme, jint style_res, jboolean force)`. Looks the theme handle
/// up through the bounds+generation-checked [`theme_registry`] and appends `style_res` to its applied
/// styles (a stale/fabricated theme handle is a typed `Err` — logged + ignored, never UB). No real
/// style resolution is performed yet (the View cascade only needs the call to succeed without GTK).
///
/// The body runs inside [`EnvUnowned::with_env`] (`catch_unwind`-wrapped, AGENTS.md §2.8;
/// `panic = "abort"` kept); `resolve::<LogErrorAndDefault>` returns the `()` default on error/panic.
extern "system" fn asset_manager_apply_theme_style<'local>(
    mut env: EnvUnowned<'local>,
    _this: JObject<'local>,
    theme: jlong,
    style_res: jint,
    force: jboolean,
) {
    env.with_env(|_env| -> jni::errors::Result<()> {
        // Resolve the applied style's bag + its parent chain from resources.arsc into a fresh map
        // (child overriding parent). This happens OUTSIDE the registry lock (it reads the APK), then
        // the result is merged into the theme under the lock.
        let mut chain = std::collections::HashMap::new();
        let style_u32 = u32::from_ne_bytes(style_res.to_ne_bytes());
        let resolved = merge_theme_style(&mut chain, style_u32);

        let merged = theme_registry::with_theme(theme, |t| {
            t.styles.push(style_res);
            // `force` (AOSP): the applied style overrides existing theme values; otherwise it only
            // fills attributes the theme does not already define.
            for (attr, val) in &chain {
                if force {
                    t.attrs.insert(*attr, *val);
                } else {
                    t.attrs.entry(*attr).or_insert(*val);
                }
            }
            t.attrs.len()
        });
        match merged {
            Ok(total) => tracing::debug!(
                target: "android.content.res.AssetManager",
                theme,
                style_res = format_args!("0x{style_u32:08x}"),
                force,
                resolved,
                total,
                "AssetManager.applyThemeStyle: merged style + parent chain into non-GTK theme"
            ),
            Err(e) => tracing::debug!(
                target: "android.content.res.AssetManager",
                theme,
                style_res = format_args!("0x{style_u32:08x}"),
                error = %e,
                "AssetManager.applyThemeStyle: invalid theme handle (ignored)"
            ),
        }
        Ok(())
    })
    .resolve::<LogErrorAndDefault>()
}

/// `AssetManager.copyTheme(long dest, long source)` → copy the source [`theme_registry`] theme's
/// recorded styles into the dest theme (2026-06-05).
///
/// JNI ABI: an INSTANCE native returning void, so the parameters are
/// `(EnvUnowned, JObject this, jlong dest, jlong source)`. Reads the source theme's styles, then
/// writes them onto the dest theme — both through the bounds+generation-checked [`theme_registry`]
/// (a stale/fabricated handle on either side is a typed `Err`, logged + ignored, never UB). Two
/// separate `with_theme` locks (read-then-write) avoid holding the registry lock across both; the
/// launcher is single-threaded on the VM main thread, so no interleaving occurs.
///
/// The body runs inside [`EnvUnowned::with_env`] (`catch_unwind`-wrapped, AGENTS.md §2.8;
/// `panic = "abort"` kept); `resolve::<LogErrorAndDefault>` returns the `()` default on error/panic.
extern "system" fn asset_manager_copy_theme<'local>(
    mut env: EnvUnowned<'local>,
    _this: JObject<'local>,
    dest: jlong,
    source: jlong,
) {
    env.with_env(|_env| -> jni::errors::Result<()> {
        // Copy both the applied-style ids AND the merged attribute map (the latter is what
        // obtainStyledAttributes reads — copying only `styles` would leave the dest theme empty).
        let src = theme_registry::with_theme(source, |t| (t.styles.clone(), t.attrs.clone()));
        match src {
            Ok((styles, attrs)) => {
                if let Err(e) = theme_registry::with_theme(dest, |t| {
                    t.styles = styles;
                    t.attrs = attrs;
                }) {
                    tracing::debug!(
                        target: "android.content.res.AssetManager",
                        dest,
                        source,
                        error = %e,
                        "AssetManager.copyTheme: invalid dest theme handle (ignored)"
                    );
                } else {
                    tracing::debug!(
                        target: "android.content.res.AssetManager",
                        dest,
                        source,
                        "AssetManager.copyTheme: copied non-GTK theme styles"
                    );
                }
            }
            Err(e) => tracing::debug!(
                target: "android.content.res.AssetManager",
                dest,
                source,
                error = %e,
                "AssetManager.copyTheme: invalid source theme handle (ignored)"
            ),
        }
        Ok(())
    })
    .resolve::<LogErrorAndDefault>()
}

/// `AssetManager.applyStyle(long theme, long parser, int defStyleAttr, int defStyleRes,
/// int[] attrs, int length, long outValues, long outIndices)` → resolve each requested styled
/// attribute from the XML element (when `parser != 0`) and/or the theme's merged attribute map, and
/// write the per-attribute `TypedArray` windows.
///
/// JNI ABI: an INSTANCE native returning void. `outValues`/`outIndices` are the framework's
/// `TypedArray` off-heap buffers (same ABI as [`asset_manager_retrieve_attributes`]). 2026-06-05:
/// this is AOSP's combined `obtainStyledAttributes(AttributeSet, int[], defStyleAttr, defStyleRes)`.
/// Values layer theme < XML element (the inline XML attributes win; the theme fills the rest). The
/// **theme** path (`parser == 0`) is what `Theme.obtainStyledAttributes(int[])` drives — including
/// AppCompat's `theme.obtainStyledAttributes(R.styleable.AppCompatTheme)`; each requested attribute is
/// looked up in the theme's merged map (built by [`merge_theme_style`] from the applied style's bag +
/// parent chain in `resources.arsc`) and any `TYPE_REFERENCE`/`TYPE_ATTRIBUTE` is resolved to a
/// concrete value (see [`resolve_theme_attr`]). An attribute absent from both the XML and the theme
/// gets `TYPE_NULL` (the framework uses its built-in default — the sound AOSP fallback, not a value
/// fake). A stale/fabricated theme handle yields all-`None` for the theme part — never UB.
///
/// The body runs inside [`EnvUnowned::with_env`] (`catch_unwind`-wrapped, AGENTS.md §2.8;
/// `panic = "abort"` kept). `resolve::<LogErrorAndDefault>` returns the `()` default on error/panic.
//
// 2026-06-05: arity is fixed by the JNI signature ART reported (8 args after `this`); a stub must
// match it exactly. clippy's `too_many_arguments` does not fire on `extern "system"` fns.
extern "system" fn asset_manager_apply_style<'local>(
    mut env: EnvUnowned<'local>,
    _this: JObject<'local>,
    theme: jlong,
    parser: jlong,
    _def_style_attr: jint,
    _def_style_res: jint,
    attrs: JIntArray<'local>,
    _length: jint,
    out_values: jlong,
    out_indices: jlong,
) {
    env.with_env(|env| -> jni::errors::Result<()> {
        // Size the fill to the requested attribute count. A null attrs array means nothing to fill;
        // still write outIndices[0]=0 so the framework reads a defined count.
        let n = if attrs.is_null() { 0 } else { attrs.len(env)? };
        let mut entries = vec![None; n];
        // 2026-06-05: ATL's `applyStyle` IS the combined obtainStyledAttributes(AttributeSet, int[])
        // path. AOSP layers the result: theme < (def-style) < XML-style < explicit XML attributes. Two
        // distinct callers:
        //   • parser != 0 — a View constructor / inflater with an XML element: styled values come FIRST
        //     from the element's inline attributes (e.g. `android:id`, which LayoutInflater + `View.
        //     <init>` read via `getResourceId`), then the theme fills any attribute the XML did not set.
        //   • parser == 0 — `Theme.obtainStyledAttributes(int[])` (no XML): every value comes from the
        //     theme's merged attribute map. THIS is the path AppCompat's
        //     `theme.obtainStyledAttributes(R.styleable.AppCompatTheme)` drives; before themes resolved,
        //     it returned all-NULL → `windowActionBar` unset → the "Theme.AppCompat" IllegalStateException.
        if n != 0 {
            let mut ids = vec![0i32; n];
            attrs.get_region(env, 0, &mut ids)?;
            if parser != 0 {
                entries = resolve_xml_attributes(parser, &ids);
                // 2026-06-05: an inline XML attribute value can itself be a theme reference
                // (`?attr/foo`, `TYPE_ATTRIBUTE`) — e.g. AppCompat's `ActionBarView$HomeView`/
                // `ImageView` set `android:background="?attr/…"`. `resolve_xml_attributes` cannot
                // resolve it (it has no theme), so the unresolved `TYPE_ATTRIBUTE` would reach
                // `TypedArray.getDrawable`/`getColor`, which throw `UnsupportedOperationException:
                // Failed to resolve attribute at index N`. Resolve each such value HERE against the
                // active theme (the handle this native already holds) — the same theme map the
                // `parser == 0` path uses. Surfaced by multitouch.test's AppCompat ActionBar inflation.
                resolve_inline_theme_refs(theme, &mut entries);
            }
            // Theme fallback: fill any attribute not already resolved from the XML element. For
            // parser == 0 this resolves ALL of them from the theme.
            let theme_entries = resolve_theme_attributes(theme, &ids);
            for (slot, theme_entry) in entries.iter_mut().zip(theme_entries) {
                if slot.is_none() {
                    *slot = theme_entry;
                }
            }
        }
        let changed = entries.iter().filter(|e| e.is_some()).count();
        // Reuses the bounds-proven writer (writes only < n*STYLE_NUM_ENTRIES / <= n; a 0 ptr skipped).
        fill_typed_array(out_values, out_indices, &entries);
        tracing::debug!(
            target: "android.content.res.AssetManager",
            theme,
            parser,
            attrs = n,
            changed,
            "AssetManager.applyStyle: resolved styled attributes (XML element + theme, non-GTK)"
        );
        Ok(())
    })
    .resolve::<LogErrorAndDefault>()
}

/// `AssetManager.getResourceName(int resid)` → the resource's full `package:type/entry` name, or
/// null if it cannot be resolved (2026-06-05).
///
/// JNI ABI: an INSTANCE native returning a `String`, so the parameters are
/// `(EnvUnowned, JObject this, jint resid)`. Resolves the packed id via Eclipse's own
/// [`apk::arsc`](crate::apk::arsc) `resources.arsc` reader (see [`resolve_resource_name`]) and returns
/// `package:type/entry`. A null `JString` is returned for an unresolvable id — which the framework
/// turns into a `Resources.NotFoundException` (the correct outcome, not a fake name).
///
/// The body runs inside [`EnvUnowned::with_env`] (`catch_unwind`-wrapped, AGENTS.md §2.8;
/// `panic = "abort"` kept); `resolve::<LogErrorAndDefault>` returns a null `JString` on error/panic.
extern "system" fn asset_manager_get_resource_name<'local>(
    mut env: EnvUnowned<'local>,
    _this: JObject<'local>,
    resid: jint,
) -> JString<'local> {
    env.with_env(|env| -> jni::errors::Result<JString<'local>> {
        let resid_u32 = u32::from_ne_bytes(resid.to_ne_bytes());
        match resolve_resource_name(resid_u32) {
            Some(name) => {
                tracing::debug!(
                    target: "android.content.res.AssetManager",
                    resid = format_args!("0x{resid_u32:08x}"),
                    name = %name,
                    "AssetManager.getResourceName: resolved via resources.arsc"
                );
                env.new_string(name)
            }
            None => {
                tracing::warn!(
                    target: "android.content.res.AssetManager",
                    resid = format_args!("0x{resid_u32:08x}"),
                    "AssetManager.getResourceName: id not in resources.arsc → null (NotFoundException)"
                );
                Ok(JString::default())
            }
        }
    })
    .resolve::<LogErrorAndDefault>()
}

/// `AssetManager.getResourcePackageName(int resid)` → the resource id's package name, or null.
///
/// JNI ABI: an INSTANCE native (`(EnvUnowned, JObject this, jint)`). Mirrors
/// [`asset_manager_get_resource_name`] but returns only the package component via
/// [`resolve_resource_package_name`]. `resolve::<LogErrorAndDefault>` returns the default (null) on an
/// internal error/panic; an unresolvable id returns null explicitly (→ `NotFoundException`).
extern "system" fn asset_manager_get_resource_package_name<'local>(
    mut env: EnvUnowned<'local>,
    _this: JObject<'local>,
    resid: jint,
) -> JString<'local> {
    env.with_env(|env| -> jni::errors::Result<JString<'local>> {
        let resid_u32 = u32::from_ne_bytes(resid.to_ne_bytes());
        match resolve_resource_package_name(resid_u32) {
            Some(pkg) => {
                tracing::debug!(
                    target: "android.content.res.AssetManager",
                    resid = format_args!("0x{resid_u32:08x}"),
                    package = %pkg,
                    "AssetManager.getResourcePackageName: resolved via resources.arsc"
                );
                env.new_string(pkg)
            }
            None => {
                tracing::warn!(
                    target: "android.content.res.AssetManager",
                    resid = format_args!("0x{resid_u32:08x}"),
                    "AssetManager.getResourcePackageName: id not in resources.arsc → null (NotFoundException)"
                );
                Ok(JString::default())
            }
        }
    })
    .resolve::<LogErrorAndDefault>()
}

/// Resolve a packed resource id to JUST its package name via the matching `resources.arsc` (framework
/// table for package `0x01`, app table otherwise; see [`arsc_bytes_for`]). The package id is the id's
/// high byte. Returns `None` for any failure (no path, missing/corrupt ARSC, or a package the table
/// does not name) — never panics. Parses fresh per call (mirrors [`resolve_resource_name`]).
fn resolve_resource_package_name(resid: u32) -> Option<String> {
    let bytes = arsc_bytes_for(resid)?;
    let table = crate::apk::arsc::parse_arsc(&bytes).ok()?;
    let package_id = (resid >> 24) as u8;
    table.package_name(package_id).map(str::to_owned)
}

/// `AssetManager.getResourceIdentifier(String name, String defType, String defPackage)` → the packed
/// resource id, or 0 if not found (AOSP's `Resources.getIdentifier`).
///
/// JNI ABI: an INSTANCE native (`(EnvUnowned, JObject this, jstring, jstring, jstring)`). Resolves via
/// [`resolve_resource_identifier`]; `resolve::<LogErrorAndDefault>` returns the `jint` default (`0`) on
/// an internal error/panic, and a not-found name returns `0` explicitly — both the correct "no such id".
extern "system" fn asset_manager_get_resource_identifier<'local>(
    mut env: EnvUnowned<'local>,
    _this: JObject<'local>,
    name: JString<'local>,
    def_type: JString<'local>,
    def_package: JString<'local>,
) -> jint {
    env.with_env(|env| -> jni::errors::Result<jint> {
        if name.is_null() {
            return Ok(0);
        }
        let name = name.try_to_string(env)?;
        let read_opt = |s: &JString<'local>| -> jni::errors::Result<String> {
            if s.is_null() {
                Ok(String::new())
            } else {
                s.try_to_string(env)
            }
        };
        let def_type = read_opt(&def_type)?;
        let def_package = read_opt(&def_package)?;
        let resid = resolve_resource_identifier(&name, &def_type, &def_package);
        tracing::debug!(
            target: "android.content.res.AssetManager",
            name = %name,
            def_type = %def_type,
            def_package = %def_package,
            resid = format_args!("0x{resid:08x}"),
            "AssetManager.getResourceIdentifier"
        );
        // jint is i32; reinterpret the u32 id's bits (a valid id fits, 0 = not found).
        Ok(i32::from_ne_bytes(resid.to_ne_bytes()))
    })
    .resolve::<LogErrorAndDefault>()
}

/// Resolve `Resources.getIdentifier(name, defType, defPackage)` to a packed resource id (or `0`).
///
/// Parses the AOSP `[package:][type/]entry` form of `name` (falling back to `defType`/`defPackage` for
/// the type/package), selects the framework table for package `android` else the app table (via
/// [`arsc_bytes_for`]'s id-dispatch with a probe id), and reverse-looks-up the id with
/// [`arsc::ResTable::find_resource_id`](crate::apk::arsc::ResTable::find_resource_id). Returns `0`
/// (AOSP's "not found") for an empty entry, an unknown name, or any ARSC failure — never panics.
fn resolve_resource_identifier(name: &str, def_type: &str, def_package: &str) -> u32 {
    // Parse the optional "package:" prefix, then the optional "type/" prefix, then the entry.
    let (pkg_in_name, rest) = match name.split_once(':') {
        Some((p, r)) => (Some(p), r),
        None => (None, name),
    };
    let (type_in_name, entry) = match rest.split_once('/') {
        Some((t, e)) => (Some(t), e),
        None => (None, rest),
    };
    let entry = entry.trim();
    if entry.is_empty() {
        return 0;
    }
    let pick = |from_name: Option<&str>, default: &str| -> Option<String> {
        from_name
            .map(str::trim)
            .filter(|s| !s.is_empty())
            .map(str::to_owned)
            .or_else(|| {
                let d = default.trim();
                (!d.is_empty()).then(|| d.to_owned())
            })
    };
    let pkg = pick(pkg_in_name, def_package);
    let Some(typ) = pick(type_in_name, def_type) else {
        return 0; // no type → cannot identify a resource
    };
    // Pick the table by package: the framework ("android") table (package 0x01) vs the app table.
    let probe_id: u32 = if pkg.as_deref() == Some("android") {
        0x0100_0000
    } else {
        0x7f00_0000
    };
    let Some(bytes) = arsc_bytes_for(probe_id) else {
        return 0;
    };
    let Ok(table) = crate::apk::arsc::parse_arsc(&bytes) else {
        return 0;
    };
    table
        .find_resource_id(pkg.as_deref(), &typ, entry)
        .unwrap_or(0)
}

/// Read an asset from the booted APK via Eclipse's own `src/apk` reader. `None` if the APK path is
/// unset or the entry is absent/unreadable (the caller returns `0` → `FileNotFoundException`).
///
/// 2026-06-11: ATL's `AssetManager.open` passes `openAsset` the FULL APK-relative path (already
/// `assets/…`), unlike stock AOSP (a path relative to `assets/`). Accept BOTH: use the name as-is when
/// it already has the `assets/` prefix, else prepend it (so a double `assets/assets/…` can't happen).
fn read_asset_bytes(name: &str) -> Option<Vec<u8>> {
    let apk_path = APK_PATH.get()?;
    let mut apk = crate::apk::Apk::open(std::path::Path::new(apk_path)).ok()?;
    let entry = if name.starts_with("assets/") {
        name.to_owned()
    } else {
        format!("assets/{name}")
    };
    apk.read_entry(&entry).ok()
}

/// `AssetManager.openAsset(String fileName, int accessMode)` → an [`asset_registry`] handle, or `0`.
///
/// JNI ABI: an INSTANCE native. Reads `assets/<fileName>` via [`read_asset_bytes`] and stores it as an
/// open stream; the `accessMode` (random/streaming/buffer) is advisory and ignored (Eclipse buffers
/// the whole asset). Returns `0` on a missing asset (→ `FileNotFoundException`, the non-faked outcome).
extern "system" fn asset_manager_open_asset<'local>(
    mut env: EnvUnowned<'local>,
    _this: JObject<'local>,
    file_name: JString<'local>,
    _access_mode: jint,
) -> jlong {
    env.with_env(|env| -> jni::errors::Result<jlong> {
        if file_name.is_null() {
            return Ok(0);
        }
        let name = file_name.try_to_string(env)?;
        let Some(bytes) = read_asset_bytes(&name) else {
            tracing::warn!(
                target: "android.content.res.AssetManager",
                asset = %name,
                "AssetManager.openAsset: assets/<name> not found → 0 (FileNotFoundException)"
            );
            return Ok(0);
        };
        match asset_registry::store(bytes) {
            Ok(handle) => {
                tracing::debug!(
                    target: "android.content.res.AssetManager",
                    asset = %name,
                    "AssetManager.openAsset: opened via src/apk"
                );
                Ok(handle)
            }
            Err(e) => {
                tracing::warn!(
                    target: "android.content.res.AssetManager",
                    asset = %name, error = %e,
                    "AssetManager.openAsset: registry store failed → 0"
                );
                Ok(0)
            }
        }
    })
    .resolve::<LogErrorAndDefault>()
}

/// `AssetManager.readAsset(long asset, byte[] b, int off, int len)` → bytes read, or `-1` at EOF.
///
/// JNI ABI: an INSTANCE native. Reads up to `len` bytes from the stream's cursor into `b[off..]`.
/// Returns `-1` at EOF (AOSP contract) or on a stale handle; `resolve::<LogErrorAndDefault>` returns
/// `0` only on an internal JNI error (e.g. the array write throwing `ArrayIndexOutOfBounds`).
extern "system" fn asset_manager_read_asset<'local>(
    mut env: EnvUnowned<'local>,
    _this: JObject<'local>,
    asset: jlong,
    b: JByteArray<'local>,
    off: jlong,
    len: jlong,
) -> jint {
    env.with_env(|env| -> jni::errors::Result<jint> {
        if len <= 0 {
            return Ok(0);
        }
        // Bound the read to what actually fits in b[off..] so set_region can never throw
        // ArrayIndexOutOfBounds (a pending JNI exception ATL would surface as IOException).
        let array_len = i64::try_from(b.len(env).unwrap_or(0)).unwrap_or(i64::MAX);
        let off = off.clamp(0, array_len);
        let fits = (array_len - off).max(0);
        let want = usize::try_from(len.min(fits)).unwrap_or(0);
        if want == 0 {
            return Ok(0);
        }
        let read = match asset_registry::with_stream(asset, |s| {
            let mut tmp = vec![0u8; want];
            let n = s.read(&mut tmp);
            tmp.truncate(n);
            tmp
        }) {
            Ok(buf) => buf,
            Err(_) => return Ok(-1), // stale/fabricated handle → report EOF, never UB
        };
        if read.is_empty() {
            return Ok(-1); // AOSP readAsset returns -1 at EOF
        }
        // Java bytes are jbyte = i8; reinterpret each byte's bits (no lossy cast).
        let signed: Vec<i8> = read.iter().map(|&x| i8::from_ne_bytes([x])).collect();
        let start = jni::sys::jsize::try_from(off).unwrap_or(jni::sys::jsize::MAX);
        b.set_region(env, start, &signed)?;
        let n = read.len();
        tracing::debug!(
            target: "android.content.res.AssetManager",
            asset, off, len, returned = n,
            "AssetManager.readAsset"
        );
        Ok(i32::try_from(n).unwrap_or(jint::MAX))
    })
    .resolve::<LogErrorAndDefault>()
}

/// `AssetManager.seekAsset(long asset, long offset, int whence)` → the new cursor position, or `-1`.
extern "system" fn asset_manager_seek_asset<'local>(
    mut env: EnvUnowned<'local>,
    _this: JObject<'local>,
    asset: jlong,
    offset: jlong,
    whence: jint,
) -> jlong {
    env.with_env(|_env| -> jni::errors::Result<jlong> {
        Ok(asset_registry::with_stream(asset, |s| s.seek(offset, whence)).unwrap_or(-1))
    })
    .resolve::<LogErrorAndDefault>()
}

/// `AssetManager.getAssetLength(long asset)` → the asset's total length, or `-1` on a bad handle.
extern "system" fn asset_manager_get_asset_length<'local>(
    mut env: EnvUnowned<'local>,
    _this: JObject<'local>,
    asset: jlong,
) -> jlong {
    env.with_env(|_env| -> jni::errors::Result<jlong> {
        Ok(
            asset_registry::with_stream(asset, |s| i64::try_from(s.len()).unwrap_or(i64::MAX))
                .unwrap_or(-1),
        )
    })
    .resolve::<LogErrorAndDefault>()
}

/// `AssetManager.getAssetRemainingLength(long asset)` → bytes from the cursor to EOF, or `-1`.
extern "system" fn asset_manager_get_asset_remaining_length<'local>(
    mut env: EnvUnowned<'local>,
    _this: JObject<'local>,
    asset: jlong,
) -> jlong {
    env.with_env(|_env| -> jni::errors::Result<jlong> {
        Ok(
            asset_registry::with_stream(asset, |s| {
                i64::try_from(s.remaining()).unwrap_or(i64::MAX)
            })
            .unwrap_or(-1),
        )
    })
    .resolve::<LogErrorAndDefault>()
}

/// `AssetManager.destroyAsset(long asset)` → free the stream (idempotent on a stale handle).
extern "system" fn asset_manager_destroy_asset<'local>(
    mut env: EnvUnowned<'local>,
    _this: JObject<'local>,
    asset: jlong,
) {
    env.with_env(|_env| -> jni::errors::Result<()> {
        let _ = asset_registry::free(asset); // a double/stale free is a harmless no-op
        Ok(())
    })
    .resolve::<LogErrorAndDefault>()
}

/// Bind the asset-STREAM read cycle (`readAsset`/`seekAsset`/`getAssetLength`/
/// `getAssetRemainingLength`/`destroyAsset`) on `android/content/res/AssetManager`, BEST-EFFORT.
///
/// `openAsset` is bound in the main [`register_asset_manager_natives`] array (its signature is
/// confirmed). These follow-on natives use the classic AOSP signatures; if this ATL build declares any
/// differently, RegisterNatives throws and we clear+log it (the dev-host run then names the real
/// signature) instead of aborting — so a read-cycle sig drift never breaks the already-registered
/// `openAsset`/resource natives. On the standard signatures the full asset stream works in one bind.
fn register_asset_stream_natives(env: &mut Env) -> Result<(), FrameworkError> {
    // Register each native INDEPENDENTLY (best-effort) so the ones this ATL build DOES declare bind
    // even if a sibling does not: ATL has `getAssetLength(J)J`/`destroyAsset(J)V` but reads assets
    // without the classic `readAsset(J[BII)I` (a grouped bind would fail as a whole on that one). Each
    // entry's fn matches its paired signature; a `NoSuchMethodError` is cleared + logged, not fatal.
    let natives: [(&JNIStr, &JNIStr, *mut c_void); 5] = [
        (
            ASSET_MANAGER_READ_ASSET_NAME,
            ASSET_MANAGER_READ_ASSET_SIG,
            asset_manager_read_asset as *mut c_void,
        ),
        (
            ASSET_MANAGER_SEEK_ASSET_NAME,
            ASSET_MANAGER_SEEK_ASSET_SIG,
            asset_manager_seek_asset as *mut c_void,
        ),
        (
            ASSET_MANAGER_GET_ASSET_LENGTH_NAME,
            ASSET_MANAGER_GET_ASSET_LENGTH_SIG,
            asset_manager_get_asset_length as *mut c_void,
        ),
        (
            ASSET_MANAGER_GET_ASSET_REMAINING_LENGTH_NAME,
            ASSET_MANAGER_GET_ASSET_REMAINING_LENGTH_SIG,
            asset_manager_get_asset_remaining_length as *mut c_void,
        ),
        (
            ASSET_MANAGER_DESTROY_ASSET_NAME,
            ASSET_MANAGER_DESTROY_ASSET_SIG,
            asset_manager_destroy_asset as *mut c_void,
        ),
    ];
    let mut bound = 0u32;
    for (name, sig, ptr) in natives {
        let class = env.find_class(ASSET_MANAGER_CLASS)?;
        // SAFETY: `class` is the loaded AssetManager; `ptr` is an `extern "system"` fn whose signature
        // is `sig` by construction. A method this build doesn't declare throws (cleared best-effort).
        let method = unsafe { NativeMethod::from_raw_parts(name, sig, ptr) };
        match unsafe { env.register_native_methods(&class, std::slice::from_ref(&method)) } {
            Ok(()) => bound += 1,
            Err(_) => {
                if env.exception_check() {
                    env.exception_clear();
                }
                tracing::debug!(
                    class = "android/content/res/AssetManager",
                    method = %name.to_str(),
                    "asset-stream native not declared on this ATL build (skipped)"
                );
            }
        }
    }
    tracing::info!(
        class = "android/content/res/AssetManager",
        bound,
        "registered Eclipse's non-GTK asset-stream natives (per-native best-effort: readAsset/seekAsset/getAssetLength/getAssetRemainingLength/destroyAsset)"
    );
    Ok(())
}

/// The framework `resources.arsc` bytes (from `framework-res.apk`), cached once.
///
/// 2026-06-05: `android.R.*` ids live in package `0x01` (the AOSP framework resource table), which
/// the app's own `resources.arsc` (package `0x7f`) does not contain — e.g. `android.R.id.text1`
/// (`0x01020002`). The app side reads its ARSC fresh per call from the zip; here we read
/// `framework-res.apk`'s `resources.arsc` **once** and own the bytes for the rest of the process
/// (parsed per call into a borrowed [`ResTable`](crate::apk::arsc::ResTable), mirroring the app
/// path — no self-referential struct, no UB). The lifecycle runs solely on the attached main
/// thread, so `OnceLock` has no contention.
static FRAMEWORK_ARSC: OnceLock<Vec<u8>> = OnceLock::new();

/// Owned `resources.arsc` bytes for the table that serves `resid`, dispatched by its high byte
/// (the package id): `0x01` → the framework table ([`FRAMEWORK_ARSC`], from `framework-res.apk`),
/// anything else → the app table (read fresh from the APK at [`APK_PATH`], preserving the existing
/// per-call behavior). Returns `None` on any failure (no path, missing/corrupt entry) — never panics.
fn arsc_bytes_for(resid: u32) -> Option<Vec<u8>> {
    if (resid >> 24) as u8 == 0x01 {
        // Framework table: load+cache framework-res.apk's resources.arsc bytes once.
        if let Some(bytes) = FRAMEWORK_ARSC.get() {
            return Some(bytes.clone());
        }
        let fw = crate::runtime::find_framework().ok()?;
        let mut apk = crate::apk::Apk::open(&fw.framework_res_apk).ok()?;
        let bytes = apk.read_entry("resources.arsc").ok()?;
        // Race-free: another caller may have set it first; either way we end up with cached bytes.
        Some(FRAMEWORK_ARSC.get_or_init(|| bytes).clone())
    } else {
        // App table: read fresh from the APK zip (the launcher resolves few names; avoids holding a
        // borrowed ResTable across the JNI boundary).
        let apk_path = APK_PATH.get()?;
        let mut apk = crate::apk::Apk::open(std::path::Path::new(apk_path)).ok()?;
        apk.read_entry("resources.arsc").ok()
    }
}

/// Resolve a packed resource id to its full `package:type/entry` name via the matching `resources.arsc`.
///
/// Reads the right table's `resources.arsc` bytes via [`arsc_bytes_for`] (framework table for
/// package `0x01`, app table otherwise), parses it with
/// [`apk::arsc::parse_arsc`](crate::apk::arsc::parse_arsc), then composes the package name + 1-based
/// type name + entry (key) name. Returns `None` for any failure (no path, missing/corrupt ARSC,
/// or an id whose package/type/entry is absent) — never panics. Parses fresh per call (avoids
/// holding a borrowed `ResTable` across the JNI boundary, mirroring [`open_xml_block`]).
fn resolve_resource_name(resid: u32) -> Option<String> {
    let bytes = arsc_bytes_for(resid)?;
    let table = crate::apk::arsc::parse_arsc(&bytes).ok()?;

    let package_id = (resid >> 24) as u8;
    let type_id = ((resid >> 16) & 0xff) as u8;
    let resolved = table.resource_value(resid)?;
    let type_name = table.type_name(package_id, type_id).ok().flatten()?;
    let entry_name = table
        .key_name(package_id, resolved.key_index)
        .ok()
        .flatten()?;
    // AOSP getResourceName format is `package:type/entry`; the package name is optional in the ARSC.
    match table.package_name(package_id) {
        Some(pkg) => Some(format!("{pkg}:{type_name}/{entry_name}")),
        None => Some(format!("{type_name}/{entry_name}")),
    }
}

/// A resource value resolved from `resources.arsc` for `loadResourceValue`: the `Res_value`
/// type/data plus, for a `TYPE_STRING`, the resolved pooled string (e.g. a layout file path).
struct ResolvedResValue {
    type_: i32,
    data: i32,
    string: Option<String>,
}

/// Resolve a packed resource id to its `Res_value` (and pooled string for a `TYPE_STRING`) via the
/// matching `resources.arsc` (framework table for package `0x01`, app table otherwise; see
/// [`arsc_bytes_for`]). `None` for any failure (no path, missing/corrupt ARSC, complex/absent
/// entry) — never panics. Parses fresh per call (mirrors [`resolve_resource_name`]).
fn resolve_res_value(resid: u32) -> Option<ResolvedResValue> {
    let bytes = arsc_bytes_for(resid)?;
    let table = crate::apk::arsc::parse_arsc(&bytes).ok()?;
    let resolved = table.resource_value(resid)?;
    // A complex (bag/map) entry has no single Res_value; loadResourceValue cannot represent it here.
    if resolved.is_complex {
        return None;
    }
    let string = if resolved.type_ == RES_VALUE_TYPE_STRING {
        table.value_string(resolved.data).ok().flatten()
    } else {
        None
    };
    Some(ResolvedResValue {
        type_: i32::from(resolved.type_),
        data: u32_to_i32(resolved.data),
        string,
    })
}

/// `AssetManager.loadResourceValue(int resid, short density, TypedValue outValue, boolean
/// resolveRefs)` → write the resolved `Res_value` onto `outValue`; return the asset cookie or 0.
///
/// JNI ABI: an INSTANCE native returning `jint`, so the parameters are
/// `(EnvUnowned, JObject this, jint resid, jshort density, JObject out_value, jboolean resolve_refs)`.
/// Resolves `resid` via Eclipse's own [`apk::arsc`](crate::apk::arsc) reader and writes the public
/// `TypedValue` fields (`type`/`data`/`assetCookie`/`resourceId`/`density`, and `string` for a
/// `TYPE_STRING`). Returns [`ECLIPSE_ASSET_COOKIE`] on success, `0` if the id is absent/complex (the
/// framework then reports not-found — the correct outcome, not a fake value).
///
/// The body runs inside [`EnvUnowned::with_env`] (`catch_unwind`-wrapped, AGENTS.md §2.8;
/// `panic = "abort"` kept); `resolve::<LogErrorAndDefault>` returns the `jint` default (`0`) on
/// error/panic — the same neutral "not found".
extern "system" fn asset_manager_load_resource_value<'local>(
    mut env: EnvUnowned<'local>,
    _this: JObject<'local>,
    resid: jint,
    density: jshort,
    out_value: JObject<'local>,
    _resolve_refs: jboolean,
) -> jint {
    env.with_env(|env| -> jni::errors::Result<jint> {
        let resid_u32 = u32::from_ne_bytes(resid.to_ne_bytes());
        let Some(resolved) = resolve_res_value(resid_u32) else {
            tracing::warn!(
                target: "android.content.res.AssetManager",
                resid = format_args!("0x{resid_u32:08x}"),
                "AssetManager.loadResourceValue: id not in resources.arsc → 0 (not found)"
            );
            return Ok(0);
        };
        if out_value.is_null() {
            // No TypedValue to fill; report not-found rather than risk a null write.
            return Ok(0);
        }

        // SAFETY: "type"/"data"/"assetCookie"/"resourceId"/"density" are `public int` fields of
        // android.util.TypedValue (TypedValue.java lines 242/252/258/263/274), so the "I" signature
        // paired with JavaType::Int is consistent — exactly FieldSignature::from_raw_parts' invariant.
        // set_field re-checks the value type at runtime, so a mismatch is a typed error, never UB.
        let int_sig =
            unsafe { FieldSignature::from_raw_parts(INT_SIG, JavaType::Primitive(Primitive::Int)) };
        env.set_field(
            &out_value,
            jni_str!("type"),
            &int_sig,
            resolved.type_.into(),
        )?;
        env.set_field(&out_value, jni_str!("data"), &int_sig, resolved.data.into())?;
        env.set_field(
            &out_value,
            jni_str!("assetCookie"),
            &int_sig,
            ECLIPSE_ASSET_COOKIE.into(),
        )?;
        env.set_field(&out_value, jni_str!("resourceId"), &int_sig, resid.into())?;
        env.set_field(
            &out_value,
            jni_str!("density"),
            &int_sig,
            jint::from(density).into(),
        )?;
        // For a TYPE_STRING, set the `string` CharSequence field to the resolved pooled string (e.g.
        // the layout file path the framework opens). new_string yields a java.lang.String, which IS a
        // CharSequence, so the field set is type-correct.
        if let Some(s) = &resolved.string {
            let jstr = env.new_string(s)?;
            // SAFETY: `string` is a `public CharSequence` field of android.util.TypedValue
            // (TypedValue.java line 247), so the `Ljava/lang/CharSequence;` descriptor paired with
            // `JavaType::Object` is consistent — exactly FieldSignature::from_raw_parts' invariant.
            // set_field re-checks the value (a java.lang.String IS a CharSequence) at runtime.
            let cs_sig =
                unsafe { FieldSignature::from_raw_parts(CHAR_SEQUENCE_SIG, JavaType::Object) };
            env.set_field(
                &out_value,
                jni_str!("string"),
                &cs_sig,
                JValue::Object(&jstr),
            )?;
        }
        tracing::debug!(
            target: "android.content.res.AssetManager",
            resid = format_args!("0x{resid_u32:08x}"),
            type_ = resolved.type_,
            data = resolved.data,
            string = ?resolved.string,
            "AssetManager.loadResourceValue: wrote TypedValue from resources.arsc"
        );
        Ok(ECLIPSE_ASSET_COOKIE)
    })
    .resolve::<LogErrorAndDefault>()
}

/// `AssetManager.loadThemeAttributeValue(long theme, int ident, TypedValue outValue, boolean
/// resolveRefs)` → resolve theme attribute `ident` against the applied theme and write it onto
/// `outValue`; return the asset cookie or 0 (2026-06-05).
///
/// JNI ABI: an INSTANCE native returning `jint`, so the parameters are
/// `(EnvUnowned, JObject this, jlong theme, jint ident, JObject out_value, jboolean resolve_refs)`.
/// Looks `ident` up in the theme handle's merged attribute map (built by `applyThemeStyle`) via the
/// same [`resolve_theme_attr`] reference chase the styled-attribute path uses, then writes the public
/// `TypedValue` fields (`type`/`data`/`assetCookie`/`resourceId`/`density`, and `string` for a
/// `TYPE_STRING`). Returns [`ECLIPSE_ASSET_COOKIE`] when the attribute is present in the theme, `0`
/// when absent / the theme handle is stale (AOSP's `resolveAttribute` returns false for an unresolved
/// theme attribute — the correct outcome, not a faked value).
///
/// The body runs inside [`EnvUnowned::with_env`] (`catch_unwind`-wrapped, AGENTS.md §2.8;
/// `panic = "abort"` kept); `resolve::<LogErrorAndDefault>` returns the `jint` default (`0`) on
/// error/panic — the same neutral "not resolved".
extern "system" fn asset_manager_load_theme_attribute_value<'local>(
    mut env: EnvUnowned<'local>,
    _this: JObject<'local>,
    theme: jlong,
    ident: jint,
    out_value: JObject<'local>,
    _resolve_refs: jboolean,
) -> jint {
    env.with_env(|env| -> jni::errors::Result<jint> {
        if out_value.is_null() {
            // No TypedValue to fill; report not-resolved rather than risk a null write.
            return Ok(0);
        }
        // Resolve `ident` against the theme's merged attribute map (reference-chased to a concrete
        // value). A stale/empty theme or an absent attribute → None → not-resolved (0).
        let entry = theme_registry::with_theme(theme, |t| resolve_theme_attr(&t.attrs, ident))
            .ok()
            .flatten();
        let Some(entry) = entry else {
            tracing::debug!(
                target: "android.content.res.AssetManager",
                theme,
                ident = format_args!("0x{:08x}", u32::from_ne_bytes(ident.to_ne_bytes())),
                "AssetManager.loadThemeAttributeValue: attr not in theme → 0 (not resolved)"
            );
            return Ok(0);
        };

        // SAFETY: "type"/"data"/"assetCookie"/"resourceId" are `public int` fields of
        // android.util.TypedValue, so the "I" signature paired with JavaType::Int is consistent —
        // exactly FieldSignature::from_raw_parts' invariant. set_field re-checks the value type at
        // runtime, so a mismatch is a typed error, never UB.
        let int_sig =
            unsafe { FieldSignature::from_raw_parts(INT_SIG, JavaType::Primitive(Primitive::Int)) };
        env.set_field(
            &out_value,
            jni_str!("type"),
            &int_sig,
            entry.value_type.into(),
        )?;
        env.set_field(&out_value, jni_str!("data"), &int_sig, entry.data.into())?;
        env.set_field(
            &out_value,
            jni_str!("assetCookie"),
            &int_sig,
            ECLIPSE_ASSET_COOKIE.into(),
        )?;
        env.set_field(
            &out_value,
            jni_str!("resourceId"),
            &int_sig,
            entry.resource_id.into(),
        )?;
        tracing::debug!(
            target: "android.content.res.AssetManager",
            theme,
            ident = format_args!("0x{:08x}", u32::from_ne_bytes(ident.to_ne_bytes())),
            type_ = entry.value_type,
            data = entry.data,
            resource_id = entry.resource_id,
            "AssetManager.loadThemeAttributeValue: wrote TypedValue from theme attrs"
        );
        Ok(ECLIPSE_ASSET_COOKIE)
    })
    .resolve::<LogErrorAndDefault>()
}

/// Read `name` from the APK zip, parse it as binary XML, and store it as an [`xml_registry`] block.
///
/// Returns the non-zero block handle, or a typed [`AssetError`] on any failure (no stashed APK path,
/// missing entry, parse error, or registry error) — the caller maps that to the `0` "no asset"
/// sentinel. Opens the APK fresh per call (the launcher opens few XML assets; this avoids holding a
/// `ZipArchive` across the JNI boundary and keeps the asset state a single `OnceLock<String>` path).
fn open_xml_block(name: &str) -> Result<jlong, AssetError> {
    let apk_path = APK_PATH.get().ok_or(AssetError::NoApkPath)?;
    let mut apk = crate::apk::Apk::open(std::path::Path::new(apk_path))?;
    let bytes = apk.read_entry(name)?;
    let doc = crate::apk::axml::parse_document(&bytes)?;
    let handle = xml_registry::store(doc)?;
    Ok(handle)
}

/// Errors from opening an XML asset out of the APK for [`open_xml_block`]. Internal to the asset
/// backing; surfaced only as a log line + the `0` sentinel return (never panics across JNI).
#[derive(Debug)]
enum AssetError {
    /// No APK path was stashed before the asset call (a registration ordering bug).
    NoApkPath,
    /// Reading the entry from the APK zip failed (missing entry, zip error, I/O).
    Apk(crate::apk::ApkError),
    /// The entry was not parseable binary XML.
    Axml(crate::apk::axml::AxmlError),
    /// Storing the parsed block in the registry failed (poisoned mutex / slab full).
    Registry(xml_registry::XmlRegistryError),
}

impl fmt::Display for AssetError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::NoApkPath => f.write_str("no APK path was stashed for asset access"),
            Self::Apk(e) => write!(f, "APK read error: {e}"),
            Self::Axml(e) => write!(f, "binary-XML parse error: {e}"),
            Self::Registry(e) => write!(f, "xml-block registry error: {e}"),
        }
    }
}

impl From<crate::apk::ApkError> for AssetError {
    fn from(e: crate::apk::ApkError) -> Self {
        Self::Apk(e)
    }
}
impl From<crate::apk::axml::AxmlError> for AssetError {
    fn from(e: crate::apk::axml::AxmlError) -> Self {
        Self::Axml(e)
    }
}
impl From<xml_registry::XmlRegistryError> for AssetError {
    fn from(e: xml_registry::XmlRegistryError) -> Self {
        Self::Registry(e)
    }
}

/// Bind Eclipse's own (non-GTK) backing for `android.content.res.AssetManager`'s `init` native.
///
/// Locates `android/content/res/AssetManager` and registers the native via `RegisterNatives` (which
/// wins over name-based lazy binding — JNI 1.1 spec). Like [`register_context_natives`]/
/// [`register_log_natives`], this MUST run before anything triggers `AssetManager`'s first active use
/// (an `AssetManager` constructor); it is registered before the lifecycle drive, since ART resolves
/// natives lazily during the lifecycle and the framework builds an `AssetManager` early in init.
///
/// # Safety / soundness
/// `register_native_methods` is `unsafe`: the function pointer must match the declared JNI
/// signature. It does, by construction — [`asset_manager_init`] is written to the exact `(I)V`
/// descriptor as an instance native (`EnvUnowned, JObject this, jint`). The native body is
/// `catch_unwind`-guarded via [`EnvUnowned::with_env`], so no Rust panic can cross the JNI boundary
/// (AGENTS.md §2.8).
fn register_asset_manager_natives(env: &mut Env) -> Result<(), FrameworkError> {
    let class = env.find_class(ASSET_MANAGER_CLASS)?;
    let methods = [
        // SAFETY: `asset_manager_init` matches the paired `(I)V` signature as an instance native
        // (see the native's docs); casting the `extern "system"` fn to a `*mut c_void` is how
        // `NativeMethod::from_raw_parts` takes it.
        unsafe {
            NativeMethod::from_raw_parts(
                ASSET_MANAGER_INIT_NAME,
                ASSET_MANAGER_INIT_SIG,
                asset_manager_init as *mut std::ffi::c_void,
            )
        },
        // SAFETY: `asset_manager_set_apk_assets` matches the paired `([Ljava/lang/Object;I)V`
        // signature as an instance native (see the native's docs); casting the `extern "system"`
        // fn to a `*mut c_void` is how `NativeMethod::from_raw_parts` takes it.
        unsafe {
            NativeMethod::from_raw_parts(
                ASSET_MANAGER_SET_APK_ASSETS_NAME,
                ASSET_MANAGER_SET_APK_ASSETS_SIG,
                asset_manager_set_apk_assets as *mut std::ffi::c_void,
            )
        },
        // SAFETY: `asset_manager_set_configuration` matches the paired
        // `(IILjava/lang/String;IIIIIIIIIIIIII)V` signature as an instance native (see the native's
        // docs); casting the `extern "system"` fn to a `*mut c_void` is how
        // `NativeMethod::from_raw_parts` takes it.
        unsafe {
            NativeMethod::from_raw_parts(
                ASSET_MANAGER_SET_CONFIGURATION_NAME,
                ASSET_MANAGER_SET_CONFIGURATION_SIG,
                asset_manager_set_configuration as *mut std::ffi::c_void,
            )
        },
        // SAFETY: `asset_manager_open_xml_asset` matches the paired `(ILjava/lang/String;)J`
        // signature as an instance native returning `jlong` (see the native's docs); casting the
        // `extern "system"` fn to a `*mut c_void` is how `NativeMethod::from_raw_parts` takes it.
        unsafe {
            NativeMethod::from_raw_parts(
                ASSET_MANAGER_OPEN_XML_ASSET_NAME,
                ASSET_MANAGER_OPEN_XML_ASSET_SIG,
                asset_manager_open_xml_asset as *mut std::ffi::c_void,
            )
        },
        // SAFETY: `asset_manager_retrieve_attributes` matches the paired `(J[IIJJ)Z` signature as an
        // instance native returning `jboolean` (see the native's docs); casting the `extern "system"`
        // fn to a `*mut c_void` is how `NativeMethod::from_raw_parts` takes it.
        unsafe {
            NativeMethod::from_raw_parts(
                ASSET_MANAGER_RETRIEVE_ATTRIBUTES_NAME,
                ASSET_MANAGER_RETRIEVE_ATTRIBUTES_SIG,
                asset_manager_retrieve_attributes as *mut std::ffi::c_void,
            )
        },
        // SAFETY: `asset_manager_new_theme` matches the paired `()J` signature as an instance native
        // returning `jlong` (see the native's docs); casting the `extern "system"` fn to a
        // `*mut c_void` is how `NativeMethod::from_raw_parts` takes it.
        unsafe {
            NativeMethod::from_raw_parts(
                ASSET_MANAGER_NEW_THEME_NAME,
                ASSET_MANAGER_NEW_THEME_SIG,
                asset_manager_new_theme as *mut std::ffi::c_void,
            )
        },
        // SAFETY: `asset_manager_apply_theme_style` matches the paired `(JIZ)V` signature as an
        // instance native (see the native's docs); casting the `extern "system"` fn to a
        // `*mut c_void` is how `NativeMethod::from_raw_parts` takes it.
        unsafe {
            NativeMethod::from_raw_parts(
                ASSET_MANAGER_APPLY_THEME_STYLE_NAME,
                ASSET_MANAGER_APPLY_THEME_STYLE_SIG,
                asset_manager_apply_theme_style as *mut std::ffi::c_void,
            )
        },
        // SAFETY: `asset_manager_copy_theme` matches the paired `(JJ)V` signature as an instance
        // native (see the native's docs); casting the `extern "system"` fn to a `*mut c_void` is how
        // `NativeMethod::from_raw_parts` takes it.
        unsafe {
            NativeMethod::from_raw_parts(
                ASSET_MANAGER_COPY_THEME_NAME,
                ASSET_MANAGER_COPY_THEME_SIG,
                asset_manager_copy_theme as *mut std::ffi::c_void,
            )
        },
        // SAFETY: `asset_manager_apply_style` matches the paired `(JJII[IIJJ)V` signature as an
        // instance native (see the native's docs); casting the `extern "system"` fn to a
        // `*mut c_void` is how `NativeMethod::from_raw_parts` takes it.
        unsafe {
            NativeMethod::from_raw_parts(
                ASSET_MANAGER_APPLY_STYLE_NAME,
                ASSET_MANAGER_APPLY_STYLE_SIG,
                asset_manager_apply_style as *mut std::ffi::c_void,
            )
        },
        // SAFETY: `asset_manager_get_resource_name` matches the paired `(I)Ljava/lang/String;`
        // signature as an instance native (see the native's docs); casting the `extern "system"` fn
        // to a `*mut c_void` is how `NativeMethod::from_raw_parts` takes it.
        unsafe {
            NativeMethod::from_raw_parts(
                ASSET_MANAGER_GET_RESOURCE_NAME_NAME,
                ASSET_MANAGER_GET_RESOURCE_NAME_SIG,
                asset_manager_get_resource_name as *mut std::ffi::c_void,
            )
        },
        // SAFETY: `asset_manager_get_resource_package_name` matches the paired `(I)Ljava/lang/String;`
        // signature as an instance native (see the native's docs); casting the `extern "system"` fn to
        // a `*mut c_void` is how `NativeMethod::from_raw_parts` takes it.
        unsafe {
            NativeMethod::from_raw_parts(
                ASSET_MANAGER_GET_RESOURCE_PACKAGE_NAME_NAME,
                ASSET_MANAGER_GET_RESOURCE_PACKAGE_NAME_SIG,
                asset_manager_get_resource_package_name as *mut std::ffi::c_void,
            )
        },
        // SAFETY: `asset_manager_get_resource_identifier` matches the paired
        // `(Ljava/lang/String;Ljava/lang/String;Ljava/lang/String;)I` signature as an instance native
        // (see the native's docs); casting the `extern "system"` fn to a `*mut c_void` is how
        // `NativeMethod::from_raw_parts` takes it.
        unsafe {
            NativeMethod::from_raw_parts(
                ASSET_MANAGER_GET_RESOURCE_IDENTIFIER_NAME,
                ASSET_MANAGER_GET_RESOURCE_IDENTIFIER_SIG,
                asset_manager_get_resource_identifier as *mut std::ffi::c_void,
            )
        },
        // SAFETY: `asset_manager_open_asset` matches the paired `(Ljava/lang/String;I)J` signature as
        // an instance native (confirmed from the ART-reported line); casting the `extern "system"` fn
        // to a `*mut c_void` is how `NativeMethod::from_raw_parts` takes it. The read-cycle natives are
        // bound separately (best-effort) by `register_asset_stream_natives`.
        unsafe {
            NativeMethod::from_raw_parts(
                ASSET_MANAGER_OPEN_ASSET_NAME,
                ASSET_MANAGER_OPEN_ASSET_SIG,
                asset_manager_open_asset as *mut std::ffi::c_void,
            )
        },
        // SAFETY: `asset_manager_load_resource_value` matches the paired
        // `(ISLandroid/util/TypedValue;Z)I` signature as an instance native (see the native's docs);
        // casting the `extern "system"` fn to a `*mut c_void` is how `NativeMethod::from_raw_parts`
        // takes it.
        unsafe {
            NativeMethod::from_raw_parts(
                ASSET_MANAGER_LOAD_RESOURCE_VALUE_NAME,
                ASSET_MANAGER_LOAD_RESOURCE_VALUE_SIG,
                asset_manager_load_resource_value as *mut std::ffi::c_void,
            )
        },
        // SAFETY: `asset_manager_load_theme_attribute_value` matches the paired
        // `(JILandroid/util/TypedValue;Z)I` signature as an instance native (see the native's docs).
        unsafe {
            NativeMethod::from_raw_parts(
                ASSET_MANAGER_LOAD_THEME_ATTRIBUTE_VALUE_NAME,
                ASSET_MANAGER_LOAD_THEME_ATTRIBUTE_VALUE_SIG,
                asset_manager_load_theme_attribute_value as *mut std::ffi::c_void,
            )
        },
    ];
    // SAFETY: `class` is the loaded android/content/res/AssetManager; `methods` hold valid fn
    // pointers whose signatures match the class's `native` declarations (`init` verified against
    // AssetManager.java line 779; `native_setApkAssets` bound signature-only from the ART-reported
    // signature `([Ljava/lang/Object;I)V` — AssetManager is denylisted, 2026-06-05).
    unsafe { env.register_native_methods(&class, &methods) }?;
    tracing::info!(
        class = "android/content/res/AssetManager",
        "registered Eclipse's non-GTK backing for AssetManager.init + native_setApkAssets + setConfiguration + openXmlAssetNative + retrieveAttributes + newTheme + applyThemeStyle + copyTheme + applyStyle + getResourceName + getResourcePackageName + getResourceIdentifier + openAsset + loadResourceValue + loadThemeAttributeValue"
    );
    Ok(())
}

// === Eclipse's own (non-GTK) backing for android.content.res.XmlBlock parser natives ===========
//
// 2026-06-05: once `openXmlAssetNative` returns a real block handle (above), AOSP's framework wraps
// it as an `android.content.res.XmlBlock` and walks it through a set of `static native` methods on
// `XmlBlock` (the `XmlBlock.Parser`/`XmlResourceParser` event cursor). Discovered via the dev-host
// run, the first is `nativeCreateParseState(long block)` (`No implementation found for long
// android.content.res.XmlBlock.nativeCreateParseState(long)`, run log 2026-06-05). These are the
// standard AOSP `XmlBlock` parser natives (stable public XmlPullParser semantics), bound against the
// Eclipse-owned [`xml_registry`] block + cursor — NOT ATL's C asset layer, NOT GTK. Each new native
// the run surfaces is added here, implemented against the parsed [`crate::apk::axml::XmlDocument`].

/// `android.content.res.XmlBlock` (internal/slashed name for `find_class`) — hosts the parser walk
/// natives the framework calls on the handle `openXmlAssetNative` returned.
pub const XML_BLOCK_CLASS: &JNIStr = jni_str!("android/content/res/XmlBlock");

// `static native long nativeCreateParseState(long block)` — create a parser cursor over `block` and
// return a parse-state handle. JNI descriptor `(J)J`, from the ART-reported signature
// `long ...XmlBlock.nativeCreateParseState(long)` (run log 2026-06-05).
const XML_BLOCK_CREATE_PARSE_STATE_NAME: &JNIStr = jni_str!("nativeCreateParseState");
const XML_BLOCK_CREATE_PARSE_STATE_SIG: &JNIStr = jni_str!("(J)J");

// `static native int nativeNext(long state)` — advance the parser cursor and return the next
// XmlPullParser event. JNI descriptor `(J)I` (`int ...XmlBlock.nativeNext(long)`, run log 2026-06-05).
const XML_BLOCK_NEXT_NAME: &JNIStr = jni_str!("nativeNext");
const XML_BLOCK_NEXT_SIG: &JNIStr = jni_str!("(J)I");

// `static native void nativeDestroyParseState(long state)` — release the parser cursor. JNI
// descriptor `(J)V` (`void ...XmlBlock.nativeDestroyParseState(long)`, run log 2026-06-05).
const XML_BLOCK_DESTROY_PARSE_STATE_NAME: &JNIStr = jni_str!("nativeDestroyParseState");
const XML_BLOCK_DESTROY_PARSE_STATE_SIG: &JNIStr = jni_str!("(J)V");

// `static native String nativeGetName(long state)` — the current tag's name. JNI descriptor
// `(J)Ljava/lang/String;` (`String ...XmlBlock.nativeGetName(long)`, run log 2026-06-05).
const XML_BLOCK_GET_NAME_NAME: &JNIStr = jni_str!("nativeGetName");
const XML_BLOCK_GET_NAME_SIG: &JNIStr = jni_str!("(J)Ljava/lang/String;");

// `static native void nativeDestroy(long block)` — release the parsed block itself (distinct from
// nativeDestroyParseState). JNI descriptor `(J)V` (`void ...XmlBlock.nativeDestroy(long)`, run log
// 2026-06-05).
const XML_BLOCK_DESTROY_NAME: &JNIStr = jni_str!("nativeDestroy");
const XML_BLOCK_DESTROY_SIG: &JNIStr = jni_str!("(J)V");

// `static native int nativeGetAttributeIndex(long state, String namespace, String name)` — the
// index of the (namespace, name) attribute on the current tag, or -1. JNI descriptor
// `(JLjava/lang/String;Ljava/lang/String;)I` (run log 2026-06-05).
const XML_BLOCK_GET_ATTR_INDEX_NAME: &JNIStr = jni_str!("nativeGetAttributeIndex");
const XML_BLOCK_GET_ATTR_INDEX_SIG: &JNIStr = jni_str!("(JLjava/lang/String;Ljava/lang/String;)I");

/// The "attribute not found" sentinel AOSP's `XmlResourceParser` accessors return.
const XML_ATTR_NOT_FOUND: jint = -1;

// `static native String nativeGetAttributeStringValue(long state, int idx)` — the string value of
// the idx-th attribute on the current tag. JNI descriptor `(JI)Ljava/lang/String;` (run log
// 2026-06-05).
const XML_BLOCK_GET_ATTR_STRING_VALUE_NAME: &JNIStr = jni_str!("nativeGetAttributeStringValue");
const XML_BLOCK_GET_ATTR_STRING_VALUE_SIG: &JNIStr = jni_str!("(JI)Ljava/lang/String;");

// `static native int nativeGetAttributeDataType(long state, int idx)` — the `Res_value.dataType`
// byte of the idx-th attribute on the current tag (a `TypedValue.TYPE_*` constant). JNI descriptor
// `(JI)I` (`int ...XmlBlock.nativeGetAttributeDataType(long, int)`, run log 2026-06-05,
// accelerometerdemo's VectorDrawableCompat reads its <vector>/<path> attribute types via
// AttributeSet.getAttributeValue → TypedArrayUtils.getNamedFloat).
const XML_BLOCK_GET_ATTR_DATA_TYPE_NAME: &JNIStr = jni_str!("nativeGetAttributeDataType");
const XML_BLOCK_GET_ATTR_DATA_TYPE_SIG: &JNIStr = jni_str!("(JI)I");

// `static native int nativeGetAttributeCount(long state)` — the number of attributes on the current
// tag. JNI descriptor `(J)I` (`int ...XmlBlock.nativeGetAttributeCount(long)`, run log 2026-06-05,
// AppCompatColorStateListInflater iterating a <selector>'s attributes). Returns the current element's
// attribute count, or 0 when not on a tag / bad handle.
const XML_BLOCK_GET_ATTR_COUNT_NAME: &JNIStr = jni_str!("nativeGetAttributeCount");
const XML_BLOCK_GET_ATTR_COUNT_SIG: &JNIStr = jni_str!("(J)I");

// `static native int nativeGetAttributeResource(long state, int idx)` — the RESOURCE ID OF THE
// ATTRIBUTE'S NAME (`AttributeSet.getAttributeNameResource`), i.e. the framework attr id the attribute
// binds to (e.g. `android:color` → `0x010101...`), or 0 if the attribute's name is not a framework
// resource. JNI descriptor `(JI)I` (`int ...XmlBlock.nativeGetAttributeResource(long, int)`, run log
// 2026-06-05, AppCompatColorStateListInflater). This is the decoded `name_resource` Eclipse's axml
// reader already stores, NOT the value — distinct from nativeGetAttributeData (the value word).
const XML_BLOCK_GET_ATTR_RESOURCE_NAME: &JNIStr = jni_str!("nativeGetAttributeResource");
const XML_BLOCK_GET_ATTR_RESOURCE_SIG: &JNIStr = jni_str!("(JI)I");

// `static native int nativeGetAttributeData(long state, int idx)` — the `Res_value.data` word of the
// idx-th attribute on the current tag (the raw int / boolean / float-bits / packed-color / resource
// ref, paired with the dataType). JNI descriptor `(JI)I` (`int
// ...XmlBlock.nativeGetAttributeData(long, int)`, run log 2026-06-05). Consulted right after
// nativeGetAttributeDataType by AttributeSet / TypedArrayUtils to read the typed value.
const XML_BLOCK_GET_ATTR_DATA_NAME: &JNIStr = jni_str!("nativeGetAttributeData");
const XML_BLOCK_GET_ATTR_DATA_SIG: &JNIStr = jni_str!("(JI)I");

/// `TypedValue.TYPE_NULL` — the data type AOSP returns for an absent attribute. Matches AOSP's
/// `XmlBlock` returning `TYPE_NULL` (`0x00`) for an out-of-range index / not-on-a-tag.
const XML_TYPE_NULL: jint = 0x00;

// `static native int nativeGetLineNumber(long state)` — the current node's source line number (used
// only by `getPositionDescription` for error messages). JNI descriptor `(J)I` (run log 2026-06-05).
// Eclipse's axml reader does not track source line numbers, so this honestly returns -1 ("unknown"),
// which AOSP's XmlResourceParser uses when a line is unavailable.
const XML_BLOCK_GET_LINE_NUMBER_NAME: &JNIStr = jni_str!("nativeGetLineNumber");
const XML_BLOCK_GET_LINE_NUMBER_SIG: &JNIStr = jni_str!("(J)I");

// `static native String nativeGetPooledString(long state, int idx)` — the block's `idx`-th pooled
// string. JNI descriptor `(JI)Ljava/lang/String;` (`String ...XmlBlock.nativeGetPooledString(long,
// int)`, run log 2026-06-05). Reached when a `TYPE_STRING` styled attribute's `TypedArray` cookie is
// XML_BLOCK_COOKIE: `TypedArray.getString` calls `mXml.getPooledString(data)` where `data` is the
// source string-pool index, which routes here. Backed by [`xml_registry::XmlBlock::pooled_string`].
const XML_BLOCK_GET_POOLED_STRING_NAME: &JNIStr = jni_str!("nativeGetPooledString");
const XML_BLOCK_GET_POOLED_STRING_SIG: &JNIStr = jni_str!("(JI)Ljava/lang/String;");

/// The "unknown line" sentinel AOSP's `XmlResourceParser.getLineNumber` returns when unavailable.
const XML_LINE_UNKNOWN: jint = -1;

// org.xmlpull.v1.XmlPullParser event constants (stable public API) that AOSP's XmlBlock.Parser
// returns from nativeNext. Namespace nodes are tracked internally and NOT surfaced as pull events.
// (START_DOCUMENT=0 is the Java-side pre-first-`next` state, never returned by nativeNext, so it is
// not encoded here.)
const XML_EVENT_END_DOCUMENT: jint = 1;
const XML_EVENT_START_TAG: jint = 2;
const XML_EVENT_END_TAG: jint = 3;
const XML_EVENT_TEXT: jint = 4;

/// `XmlBlock.nativeCreateParseState(long block)` → a parse-state handle.
///
/// JNI ABI: a `static` native, so the second argument is the `JClass`, then the `jlong block`
/// handle. Eclipse's [`xml_registry`] block already owns its own parser cursor, so the parse state
/// **is** the block: this validates the handle (a bounds+generation-checked [`xml_registry::with_block`]
/// lookup — a stale/fabricated handle is rejected, never UB) and returns the same handle for the
/// subsequent walk natives to use. Returns `0` (the "no state" sentinel) if the handle is invalid.
///
/// The body runs inside [`EnvUnowned::with_env`] (`catch_unwind`-wrapped, AGENTS.md §2.8;
/// `panic = "abort"` kept); `resolve::<LogErrorAndDefault>` returns the `jlong` default (`0`) on
/// any error/panic — a sound neutral "no state" handle.
extern "system" fn xml_block_create_parse_state<'local>(
    mut env: EnvUnowned<'local>,
    _class: JClass<'local>,
    block: jlong,
) -> jlong {
    env.with_env(|_env| -> jni::errors::Result<jlong> {
        // Reset the cursor to the start of the document so each parser begins at START_DOCUMENT, and
        // validate the handle. A bad handle → 0 (the framework treats it as a failed parser create).
        match xml_registry::with_block(block, |b| {
            b.cursor = 0;
            b.current = None;
        }) {
            Ok(()) => Ok(block),
            Err(e) => {
                tracing::warn!(
                    target: "android.content.res.XmlBlock",
                    block,
                    error = %e,
                    "XmlBlock.nativeCreateParseState: invalid block handle → 0"
                );
                Ok(0)
            }
        }
    })
    .resolve::<LogErrorAndDefault>()
}

/// `XmlBlock.nativeNext(long state)` → the next XmlPullParser event constant.
///
/// JNI ABI: a `static` native (`JClass`, then the `jlong state` handle — the same registry handle
/// `nativeCreateParseState` returned). Advances the block's cursor past any namespace nodes (AOSP's
/// `XmlBlock.Parser` tracks namespaces internally and does not surface them as pull events) and maps
/// the node under the cursor to its [`XmlPullParser`](https://developer.android.com) event int:
/// `START_TAG`(2)/`END_TAG`(3)/`TEXT`(4); `END_DOCUMENT`(1) at/after the end. A bad/stale handle
/// yields `END_DOCUMENT` so the framework's walk loop terminates cleanly rather than spinning.
///
/// The body runs inside [`EnvUnowned::with_env`] (`catch_unwind`-wrapped, §2.8). `resolve` returns
/// the `jint` default (`0` = `START_DOCUMENT`) on error/panic — a neutral, non-advancing event.
extern "system" fn xml_block_next<'local>(
    mut env: EnvUnowned<'local>,
    _class: JClass<'local>,
    state: jlong,
) -> jint {
    env.with_env(|_env| -> jni::errors::Result<jint> {
        let event = xml_registry::with_block(state, |b| {
            // Skip namespace bookkeeping nodes; return the first content event (or end).
            loop {
                match b.next_event() {
                    Some(crate::apk::axml::XmlEventKind::StartTag(_)) => break XML_EVENT_START_TAG,
                    Some(crate::apk::axml::XmlEventKind::EndTag(_)) => break XML_EVENT_END_TAG,
                    Some(crate::apk::axml::XmlEventKind::Text(_)) => break XML_EVENT_TEXT,
                    Some(crate::apk::axml::XmlEventKind::StartNamespace(_))
                    | Some(crate::apk::axml::XmlEventKind::EndNamespace(_)) => continue,
                    None => break XML_EVENT_END_DOCUMENT,
                }
            }
        });
        match event {
            Ok(ev) => Ok(ev),
            Err(e) => {
                tracing::warn!(
                    target: "android.content.res.XmlBlock",
                    state,
                    error = %e,
                    "XmlBlock.nativeNext: invalid state handle → END_DOCUMENT"
                );
                Ok(XML_EVENT_END_DOCUMENT)
            }
        }
    })
    .resolve::<LogErrorAndDefault>()
}

/// `XmlBlock.nativeDestroyParseState(long state)` → release the parser cursor.
///
/// JNI ABI: a `static` native returning void. In Eclipse's model the parse state and the block are
/// the **same** [`xml_registry`] entry (one cursor per block), and the framework destroys the parse
/// state *before* the block (`nativeDestroy`). So this must NOT free the entry — doing so would
/// invalidate the block handle the framework still holds. It only validates the handle (a stale one
/// is logged + ignored); the entry is freed by [`xml_block_destroy`] (`nativeDestroy`). Idempotent;
/// never UB or panic — the registry rejects a bad handle.
///
/// The body runs inside [`EnvUnowned::with_env`] (`catch_unwind`-wrapped, §2.8); `resolve` returns
/// the `()` default on error/panic — the correct neutral value for this `void` native.
extern "system" fn xml_block_destroy_parse_state<'local>(
    mut env: EnvUnowned<'local>,
    _class: JClass<'local>,
    state: jlong,
) {
    env.with_env(|_env| -> jni::errors::Result<()> {
        // Validate-only (do NOT free): the block handle equals the parse-state handle in Eclipse's
        // model and is freed later by nativeDestroy. with_block bounds+generation-checks it.
        if let Err(e) = xml_registry::with_block(state, |_b| ()) {
            tracing::debug!(
                target: "android.content.res.XmlBlock",
                state,
                error = %e,
                "XmlBlock.nativeDestroyParseState: handle invalid (ignored)"
            );
        }
        Ok(())
    })
    .resolve::<LogErrorAndDefault>()
}

/// `XmlBlock.nativeGetName(long state)` → the current tag's local name as a `java.lang.String`.
///
/// JNI ABI: a `static` native returning a `String` (`JClass`, then the `jlong state`). Returns the
/// current element's resolved name when the cursor is on a start/end tag; a null `JString` otherwise
/// (text node, document edges, or an invalid handle) — AOSP's `XmlResourceParser.getName` returns
/// null when not positioned on a tag.
///
/// The body runs inside [`EnvUnowned::with_env`] (`catch_unwind`-wrapped, §2.8); `resolve` returns a
/// null `JString` on error/panic.
extern "system" fn xml_block_get_name<'local>(
    mut env: EnvUnowned<'local>,
    _class: JClass<'local>,
    state: jlong,
) -> JString<'local> {
    env.with_env(|env| -> jni::errors::Result<JString<'local>> {
        // Resolve the current element's name under the registry lock, then build the JString outside
        // it (new_string needs &mut Env; the lock guard is dropped first).
        let name =
            xml_registry::with_block(state, |b| b.current_element().and_then(|e| e.name.clone()))
                .ok()
                .flatten();
        match name {
            Some(n) => env.new_string(n),
            // Not on a tag (or bad handle): null name, matching getName()'s contract.
            None => Ok(JString::default()),
        }
    })
    .resolve::<LogErrorAndDefault>()
}

/// `XmlBlock.nativeDestroy(long block)` → release the parsed block.
///
/// JNI ABI: a `static` native returning void. Frees the [`xml_registry`] entry — the parsed document
/// is no longer needed once the framework finished walking the asset and destroyed its parse state.
/// A bad/stale/already-freed handle is logged and ignored (idempotent; the registry rejects it,
/// never UB or panic).
///
/// The body runs inside [`EnvUnowned::with_env`] (`catch_unwind`-wrapped, §2.8); `resolve` returns
/// the `()` default on error/panic.
extern "system" fn xml_block_destroy<'local>(
    mut env: EnvUnowned<'local>,
    _class: JClass<'local>,
    block: jlong,
) {
    env.with_env(|_env| -> jni::errors::Result<()> {
        if let Err(e) = xml_registry::free(block) {
            tracing::debug!(
                target: "android.content.res.XmlBlock",
                block,
                error = %e,
                "XmlBlock.nativeDestroy: handle already freed/invalid (ignored)"
            );
        }
        Ok(())
    })
    .resolve::<LogErrorAndDefault>()
}

/// `XmlBlock.nativeGetAttributeIndex(long state, String namespace, String name)` → the attribute's
/// index on the current tag, or `-1`.
///
/// JNI ABI: a `static` native (`JClass`, `jlong state`, then two `JString`s). Matches an attribute
/// by name and namespace: a null/empty `namespace` argument matches an attribute with no namespace
/// (AOSP treats the empty namespace as "no namespace"); a non-empty `namespace` must equal the
/// attribute's resolved namespace URI. Returns the 0-based index into the current element's
/// attributes, or `-1` if not found / not on a tag / bad handle.
///
/// The body runs inside [`EnvUnowned::with_env`] (`catch_unwind`-wrapped, §2.8); `resolve` returns
/// the `jint` default (`0`) on error/panic — but every real path returns an explicit value, so an
/// error yields `-1` (not found).
extern "system" fn xml_block_get_attribute_index<'local>(
    mut env: EnvUnowned<'local>,
    _class: JClass<'local>,
    state: jlong,
    namespace: JString<'local>,
    name: JString<'local>,
) -> jint {
    env.with_env(|env| -> jni::errors::Result<jint> {
        // A null name cannot match; not found.
        if name.is_null() {
            return Ok(XML_ATTR_NOT_FOUND);
        }
        let want_name = name.try_to_string(env)?;
        // Empty / null namespace means "no namespace" (AOSP semantics).
        let want_ns = if namespace.is_null() {
            String::new()
        } else {
            namespace.try_to_string(env)?
        };
        let idx = xml_registry::with_block(state, |b| {
            b.current_element().and_then(|e| {
                e.attributes.iter().position(|a| {
                    let a_ns = a.namespace.as_deref().unwrap_or("");
                    let a_name = a.name.as_deref().unwrap_or("");
                    a_name == want_name && a_ns == want_ns
                })
            })
        })
        .ok()
        .flatten();
        match idx {
            Some(i) => Ok(jint::try_from(i).unwrap_or(XML_ATTR_NOT_FOUND)),
            None => Ok(XML_ATTR_NOT_FOUND),
        }
    })
    .resolve::<LogErrorAndDefault>()
}

/// `XmlBlock.nativeGetAttributeStringValue(long state, int idx)` → the idx-th attribute's string
/// value as a `java.lang.String`, or null.
///
/// JNI ABI: a `static` native (`JClass`, `jlong state`, `jint idx`). Returns the resolved string for
/// a `TYPE_STRING` attribute; null for a non-string-typed attribute (AOSP's
/// `getAttributeValue`/`nativeGetAttributeStringValue` returns the pooled string only when the
/// value is string-typed), for an out-of-range index, when not on a tag, or for a bad handle.
///
/// The body runs inside [`EnvUnowned::with_env`] (`catch_unwind`-wrapped, §2.8); `resolve` returns a
/// null `JString` on error/panic.
extern "system" fn xml_block_get_attribute_string_value<'local>(
    mut env: EnvUnowned<'local>,
    _class: JClass<'local>,
    state: jlong,
    idx: jint,
) -> JString<'local> {
    env.with_env(|env| -> jni::errors::Result<JString<'local>> {
        let value = current_attribute(state, idx, |a| a.value_string.clone());
        match value.flatten() {
            Some(s) => env.new_string(s),
            None => Ok(JString::default()),
        }
    })
    .resolve::<LogErrorAndDefault>()
}

/// `XmlBlock.nativeGetAttributeDataType(long state, int idx)` → the idx-th attribute's
/// `Res_value.dataType` (a `TypedValue.TYPE_*` constant), or `TYPE_NULL`.
///
/// JNI ABI: a `static` native (`JClass`, `jlong state`, `jint idx`). Returns the attribute's parsed
/// `value_type` byte (e.g. `TYPE_STRING`=3, `TYPE_INT_DEC`=0x10, `TYPE_FLOAT`=4) widened to `jint` —
/// the exact byte Eclipse's axml reader stored from the binary XML `Res_value`. Returns
/// [`XML_TYPE_NULL`] (`0`) for an out-of-range index, when not on a tag, or for a bad handle, matching
/// AOSP `XmlBlock` returning `TYPE_NULL` for an absent attribute. This is the type discriminator
/// `AttributeSet.getAttributeValue`/`TypedArrayUtils.getNamedFloat` consult before reading the value.
///
/// The body runs inside [`EnvUnowned::with_env`] (`catch_unwind`-wrapped, §2.8); `resolve` returns
/// the `jint` default (`0` = `TYPE_NULL`) on error/panic — the correct neutral value.
extern "system" fn xml_block_get_attribute_data_type<'local>(
    mut env: EnvUnowned<'local>,
    _class: JClass<'local>,
    state: jlong,
    idx: jint,
) -> jint {
    env.with_env(|_env| -> jni::errors::Result<jint> {
        // value_type is the Res_value.dataType byte; widen to jint. Absent attr → TYPE_NULL.
        let data_type = current_attribute(state, idx, |a| jint::from(a.value_type));
        Ok(data_type.unwrap_or(XML_TYPE_NULL))
    })
    .resolve::<LogErrorAndDefault>()
}

/// `XmlBlock.nativeGetAttributeData(long state, int idx)` → the idx-th attribute's `Res_value.data`
/// word (the raw typed value), or `0`.
///
/// JNI ABI: a `static` native (`JClass`, `jlong state`, `jint idx`). Returns the attribute's parsed
/// `value_data` word — the raw 32-bit value whose interpretation is given by the paired
/// `nativeGetAttributeDataType` (an int for `TYPE_INT_*`, the IEEE-754 bits for `TYPE_FLOAT`, the
/// packed ARGB for `TYPE_*_COLOR`, the string-pool index for `TYPE_STRING`, the resource id for
/// `TYPE_REFERENCE`). The `u32` data word is reinterpreted as `jint` (the JNI return type) with the
/// same bit pattern — AOSP's `nativeGetAttributeData` returns the raw `Res_value.data` int unchanged.
/// Returns `0` for an out-of-range index, when not on a tag, or for a bad handle.
///
/// The body runs inside [`EnvUnowned::with_env`] (`catch_unwind`-wrapped, §2.8); `resolve` returns
/// the `jint` default (`0`) on error/panic — the correct neutral value.
extern "system" fn xml_block_get_attribute_data<'local>(
    mut env: EnvUnowned<'local>,
    _class: JClass<'local>,
    state: jlong,
    idx: jint,
) -> jint {
    env.with_env(|_env| -> jni::errors::Result<jint> {
        // value_data is the raw Res_value.data word; reinterpret the u32 bits as jint (AOSP returns
        // the raw int unchanged — TYPE_FLOAT carries IEEE-754 bits, colors are packed ARGB, etc.).
        let data = current_attribute(state, idx, |a| a.value_data as i32);
        Ok(data.unwrap_or(0))
    })
    .resolve::<LogErrorAndDefault>()
}

/// `XmlBlock.nativeGetAttributeCount(long state)` → the number of attributes on the current tag, or
/// `0`.
///
/// JNI ABI: a `static` native (`JClass`, `jlong state`). Returns the current element's attribute count
/// (what `XmlPullParser.getAttributeCount` returns, the loop bound the attribute accessors are indexed
/// within). Returns `0` when not on a start/end tag or for a bad handle (AOSP returns `0`/`-1` when no
/// attributes — `0` is the safe "no attributes" loop bound).
///
/// The body runs inside [`EnvUnowned::with_env`] (`catch_unwind`-wrapped, §2.8); `resolve` returns the
/// `jint` default (`0`) on error/panic — the correct neutral count.
extern "system" fn xml_block_get_attribute_count<'local>(
    mut env: EnvUnowned<'local>,
    _class: JClass<'local>,
    state: jlong,
) -> jint {
    env.with_env(|_env| -> jni::errors::Result<jint> {
        let count = xml_registry::with_block(state, |b| {
            b.current_element().map(|e| e.attributes.len()).unwrap_or(0)
        })
        .unwrap_or(0);
        Ok(jint::try_from(count).unwrap_or(jint::MAX))
    })
    .resolve::<LogErrorAndDefault>()
}

/// `XmlBlock.nativeGetAttributeResource(long state, int idx)` → the resource id of the idx-th
/// attribute's NAME (`getAttributeNameResource`), or `0`.
///
/// JNI ABI: a `static` native (`JClass`, `jlong state`, `jint idx`). Returns the decoded
/// `name_resource` Eclipse's axml reader stored for the attribute (the framework attr id its name binds
/// to, e.g. `android:color`), reinterpreted to `jint`; `0` when the name is not a framework resource,
/// for an out-of-range index, when not on a tag, or for a bad handle. Distinct from
/// `nativeGetAttributeData` (which returns the attribute's VALUE word).
///
/// The body runs inside [`EnvUnowned::with_env`] (`catch_unwind`-wrapped, §2.8); `resolve` returns the
/// `jint` default (`0`) on error/panic — the correct neutral value.
extern "system" fn xml_block_get_attribute_resource<'local>(
    mut env: EnvUnowned<'local>,
    _class: JClass<'local>,
    state: jlong,
    idx: jint,
) -> jint {
    env.with_env(|_env| -> jni::errors::Result<jint> {
        // name_resource is the decoded framework attr id of the attribute's NAME; reinterpret u32 bits.
        let res = current_attribute(state, idx, |a| u32_to_i32(a.name_resource));
        Ok(res.unwrap_or(0))
    })
    .resolve::<LogErrorAndDefault>()
}

/// `XmlBlock.nativeGetLineNumber(long state)` → the current node's source line, or `-1` (unknown).
///
/// JNI ABI: a `static` native (`JClass`, then the `jlong state`). Eclipse's axml reader does not
/// retain source line numbers (binary XML carries them but the reader discards them), so this
/// honestly returns [`XML_LINE_UNKNOWN`] (`-1`) — the value AOSP's `XmlResourceParser` uses when a
/// line is unavailable. It is only consumed by `getPositionDescription` for error messages, so `-1`
/// is correct, not a fake. Validates the handle so an invalid one is logged (still returns `-1`).
///
/// The body runs inside [`EnvUnowned::with_env`] (`catch_unwind`-wrapped, §2.8); `resolve` returns
/// the `jint` default (`0`) on error/panic, but every path returns an explicit value (`-1`).
extern "system" fn xml_block_get_line_number<'local>(
    mut env: EnvUnowned<'local>,
    _class: JClass<'local>,
    state: jlong,
) -> jint {
    env.with_env(|_env| -> jni::errors::Result<jint> {
        if let Err(e) = xml_registry::with_block(state, |_b| ()) {
            tracing::debug!(
                target: "android.content.res.XmlBlock",
                state,
                error = %e,
                "XmlBlock.nativeGetLineNumber: invalid state handle → -1 (unknown)"
            );
        }
        // axml does not track source lines; -1 ("unknown") is the honest AOSP sentinel.
        Ok(XML_LINE_UNKNOWN)
    })
    .resolve::<LogErrorAndDefault>()
}

/// `XmlBlock.nativeGetPooledString(long state, int idx)` → the block's `idx`-th pooled string, or null.
///
/// JNI ABI: a `static` native (`JClass`, `jlong state`, `jint idx`). Reached when a `TYPE_STRING`
/// styled attribute's `TypedArray` cookie is [`XML_BLOCK_COOKIE`] (`-1`): `TypedArray.getString` calls
/// `mXml.getPooledString(data)` with `data` = the source string-pool index, routing here. Returns the
/// block's pooled string at `idx` (via [`xml_registry::XmlBlock::pooled_string`]); a null `JString`
/// for a negative/out-of-range index or a bad handle (AOSP returns null for an absent pooled string).
///
/// The body runs inside [`EnvUnowned::with_env`] (`catch_unwind`-wrapped, §2.8); `resolve` returns a
/// null `JString` on error/panic.
extern "system" fn xml_block_get_pooled_string<'local>(
    mut env: EnvUnowned<'local>,
    _class: JClass<'local>,
    state: jlong,
    idx: jint,
) -> JString<'local> {
    env.with_env(|env| -> jni::errors::Result<JString<'local>> {
        // A negative index has no pooled string; usize::try_from rejects it cleanly.
        let value = usize::try_from(idx).ok().and_then(|i| {
            xml_registry::with_block(state, |b| b.pooled_string(i).map(str::to_owned))
                .ok()
                .flatten()
        });
        match value {
            Some(s) => env.new_string(s),
            None => Ok(JString::default()),
        }
    })
    .resolve::<LogErrorAndDefault>()
}

/// Run `f` against the idx-th attribute of the current element of block `state`.
///
/// Returns `Some(f(attr))` when the handle is valid, the cursor is on a start/end tag, and `idx` is
/// in range; `None` otherwise. Centralizes the handle + bounds checks the per-attribute `nativeGet*`
/// accessors share, so a bad handle or out-of-range index is always a clean `None`, never UB/panic.
fn current_attribute<R>(
    state: jlong,
    idx: jint,
    f: impl FnOnce(&crate::apk::axml::XmlAttribute) -> R,
) -> Option<R> {
    let i = usize::try_from(idx).ok()?;
    xml_registry::with_block(state, |b| {
        b.current_element().and_then(|e| e.attributes.get(i)).map(f)
    })
    .ok()
    .flatten()
}

/// Bind Eclipse's own (non-GTK) backing for `android.content.res.XmlBlock`'s parser natives.
///
/// Registered before the lifecycle drive, alongside the AssetManager natives, since the framework
/// constructs an `XmlBlock` parser during `Context.<clinit>` (reading `AndroidManifest.xml`). Each
/// native is added as the dev-host run surfaces it (`No implementation found …`).
///
/// # Safety / soundness
/// `register_native_methods` is `unsafe`: each fn pointer must match the declared JNI signature.
/// They do, by construction — each native is written to the exact descriptor the run reported. Every
/// native body is `catch_unwind`-guarded via [`EnvUnowned::with_env`], so no Rust panic crosses the
/// JNI boundary (AGENTS.md §2.8).
fn register_xml_block_natives(env: &mut Env) -> Result<(), FrameworkError> {
    let class = env.find_class(XML_BLOCK_CLASS)?;
    let methods = [
        // SAFETY: `xml_block_create_parse_state` matches the paired `(J)J` signature as a static
        // native; casting the `extern "system"` fn to a `*mut c_void` is how
        // `NativeMethod::from_raw_parts` takes it.
        unsafe {
            NativeMethod::from_raw_parts(
                XML_BLOCK_CREATE_PARSE_STATE_NAME,
                XML_BLOCK_CREATE_PARSE_STATE_SIG,
                xml_block_create_parse_state as *mut std::ffi::c_void,
            )
        },
        // SAFETY: `xml_block_next` matches the paired `(J)I` signature as a static native.
        unsafe {
            NativeMethod::from_raw_parts(
                XML_BLOCK_NEXT_NAME,
                XML_BLOCK_NEXT_SIG,
                xml_block_next as *mut std::ffi::c_void,
            )
        },
        // SAFETY: `xml_block_destroy_parse_state` matches the paired `(J)V` signature as a static
        // native.
        unsafe {
            NativeMethod::from_raw_parts(
                XML_BLOCK_DESTROY_PARSE_STATE_NAME,
                XML_BLOCK_DESTROY_PARSE_STATE_SIG,
                xml_block_destroy_parse_state as *mut std::ffi::c_void,
            )
        },
        // SAFETY: `xml_block_get_name` matches the paired `(J)Ljava/lang/String;` signature as a
        // static native.
        unsafe {
            NativeMethod::from_raw_parts(
                XML_BLOCK_GET_NAME_NAME,
                XML_BLOCK_GET_NAME_SIG,
                xml_block_get_name as *mut std::ffi::c_void,
            )
        },
        // SAFETY: `xml_block_destroy` matches the paired `(J)V` signature as a static native.
        unsafe {
            NativeMethod::from_raw_parts(
                XML_BLOCK_DESTROY_NAME,
                XML_BLOCK_DESTROY_SIG,
                xml_block_destroy as *mut std::ffi::c_void,
            )
        },
        // SAFETY: `xml_block_get_attribute_index` matches the paired
        // `(JLjava/lang/String;Ljava/lang/String;)I` signature as a static native.
        unsafe {
            NativeMethod::from_raw_parts(
                XML_BLOCK_GET_ATTR_INDEX_NAME,
                XML_BLOCK_GET_ATTR_INDEX_SIG,
                xml_block_get_attribute_index as *mut std::ffi::c_void,
            )
        },
        // SAFETY: `xml_block_get_attribute_string_value` matches the paired
        // `(JI)Ljava/lang/String;` signature as a static native.
        unsafe {
            NativeMethod::from_raw_parts(
                XML_BLOCK_GET_ATTR_STRING_VALUE_NAME,
                XML_BLOCK_GET_ATTR_STRING_VALUE_SIG,
                xml_block_get_attribute_string_value as *mut std::ffi::c_void,
            )
        },
        // SAFETY: `xml_block_get_attribute_count` matches the paired `(J)I` signature as a static
        // native.
        unsafe {
            NativeMethod::from_raw_parts(
                XML_BLOCK_GET_ATTR_COUNT_NAME,
                XML_BLOCK_GET_ATTR_COUNT_SIG,
                xml_block_get_attribute_count as *mut std::ffi::c_void,
            )
        },
        // SAFETY: `xml_block_get_attribute_resource` matches the paired `(JI)I` signature as a static
        // native.
        unsafe {
            NativeMethod::from_raw_parts(
                XML_BLOCK_GET_ATTR_RESOURCE_NAME,
                XML_BLOCK_GET_ATTR_RESOURCE_SIG,
                xml_block_get_attribute_resource as *mut std::ffi::c_void,
            )
        },
        // SAFETY: `xml_block_get_attribute_data_type` matches the paired `(JI)I` signature as a
        // static native.
        unsafe {
            NativeMethod::from_raw_parts(
                XML_BLOCK_GET_ATTR_DATA_TYPE_NAME,
                XML_BLOCK_GET_ATTR_DATA_TYPE_SIG,
                xml_block_get_attribute_data_type as *mut std::ffi::c_void,
            )
        },
        // SAFETY: `xml_block_get_attribute_data` matches the paired `(JI)I` signature as a static
        // native.
        unsafe {
            NativeMethod::from_raw_parts(
                XML_BLOCK_GET_ATTR_DATA_NAME,
                XML_BLOCK_GET_ATTR_DATA_SIG,
                xml_block_get_attribute_data as *mut std::ffi::c_void,
            )
        },
        // SAFETY: `xml_block_get_line_number` matches the paired `(J)I` signature as a static native.
        unsafe {
            NativeMethod::from_raw_parts(
                XML_BLOCK_GET_LINE_NUMBER_NAME,
                XML_BLOCK_GET_LINE_NUMBER_SIG,
                xml_block_get_line_number as *mut std::ffi::c_void,
            )
        },
        // SAFETY: `xml_block_get_pooled_string` matches the paired `(JI)Ljava/lang/String;` signature
        // as a static native.
        unsafe {
            NativeMethod::from_raw_parts(
                XML_BLOCK_GET_POOLED_STRING_NAME,
                XML_BLOCK_GET_POOLED_STRING_SIG,
                xml_block_get_pooled_string as *mut std::ffi::c_void,
            )
        },
    ];
    // SAFETY: `class` is the loaded android/content/res/XmlBlock; `methods` hold valid fn pointers
    // whose signatures match the class's `native` declarations (from the ART-reported signatures,
    // 2026-06-05).
    unsafe { env.register_native_methods(&class, &methods) }?;
    tracing::info!(
        class = "android/content/res/XmlBlock",
        "registered Eclipse's non-GTK backing for XmlBlock parser natives (nativeCreateParseState/nativeNext/nativeDestroyParseState/nativeGetName/nativeDestroy/nativeGetLineNumber/nativeGetPooledString)"
    );
    Ok(())
}

// === Eclipse's own (non-GTK) backing for android.os.Environment.native_get_app_data_dir =========
//
// 2026-06-05: `android.os.Environment` (ATL `api-impl/android/os/Environment.java`) declares
// `private static native String native_get_app_data_dir();` (line 336) — a STATIC native, JNI
// signature `()Ljava/lang/String;`, exactly the signature ART reported missing. Its only caller is
// `getExternalStorageDirectory()` (lines 328–334): `app_data_dir_file = new File(native_get_app_data_dir())`
// — i.e. the returned String is the app's external-storage / data directory root, cached as a `File`.
// ATL backs this in C; Eclipse must NOT pull GTK and must not hardcode an Android-device path like
// `/data` or `/sdcard` (those do not exist on the Linux host; §9 detect-don't-assume).
//
// Eclipse's GTK-free equivalent returns a REAL, portable, host-valid directory derived from the XDG
// data dir via `directories::ProjectDirs` — the same portable pattern as
// `runtime::native_lib_cache_dir` (`$XDG_DATA_HOME/eclipse/app-data`, never a hardcoded
// `/tmp`/`/home`/username/`/data` path; CLAUDE.md "Build & Environment Portability"). Returning a
// non-null path is required, not optional: the Java caller does `new File(<string>)`, so a null
// would throw `NullPointerException` and stall the lifecycle — a null "stub" would NOT let the
// lifecycle proceed. This is minimal-but-correct, grounded in the Java caller + standard Android
// semantics, not behavior-faking. The directory is not created here (Android's contract: the path
// may not yet exist — `getExternalStorageDirectory` docs); a later increment can `mkdirs` it when an
// app actually reads/writes app data.

/// `android.os.Environment` (internal/slashed name for `find_class`) — hosts the static
/// `native_get_app_data_dir` the framework calls from `getExternalStorageDirectory()`.
pub const ENVIRONMENT_CLASS: &JNIStr = jni_str!("android/os/Environment");

// JNI name + descriptor for Environment's native, exactly as declared in `Environment.java`
// (2026-06-05, line 336): `private static native String native_get_app_data_dir();`.
const GET_APP_DATA_DIR_NAME: &JNIStr = jni_str!("native_get_app_data_dir");
const GET_APP_DATA_DIR_SIG: &JNIStr = jni_str!("()Ljava/lang/String;");

/// Resolve Eclipse's portable per-app data directory (the value `native_get_app_data_dir` returns).
///
/// 2026-06-05: mirrors [`runtime::native_lib_cache_dir`](crate::runtime::native_lib_cache_dir)'s
/// portable `directories::ProjectDirs` pattern — `$XDG_DATA_HOME/eclipse/app-data`
/// (`~/.local/share/eclipse/app-data` by default), overridable via `ECLIPSE_APP_DATA_DIR`, never a
/// hardcoded `/data`/`/sdcard`/`/home`/`/tmp` path (§9, CLAUDE.md portability). Returns `None` only
/// when no home/data base can be determined (e.g. `$HOME` unset) — the native then surfaces a JNI
/// error rather than fabricating a path.
fn app_data_dir() -> Option<std::path::PathBuf> {
    if let Some(dir) = std::env::var_os("ECLIPSE_APP_DATA_DIR") {
        return Some(std::path::PathBuf::from(dir));
    }
    let dirs = directories::ProjectDirs::from("", "", "eclipse")?;
    Some(dirs.data_dir().join("app-data"))
}

/// `Environment.native_get_app_data_dir()` → the app's data directory path as a `java.lang.String`.
///
/// JNI ABI: a `static` native (the Java method is `static`), so the second argument is the `JClass`.
/// Returns a real, portable, host-valid directory (see the Environment native-backing note above);
/// the Java caller wraps it in `new File(...)`, so it must be non-null. The body runs inside
/// [`EnvUnowned::with_env`], which `catch_unwind`-wraps it so a Rust panic can never unwind into
/// ART's C++ (AGENTS.md §2.8; `panic = "abort"` kept). `resolve::<LogErrorAndDefault>` returns a
/// null `JString` only on an unrecoverable error (no XDG base, or string conversion failure) rather
/// than propagating — surfaced as a JNI error the dev-host log shows.
extern "system" fn native_get_app_data_dir<'local>(
    mut env: EnvUnowned<'local>,
    _class: JClass<'local>,
) -> JString<'local> {
    env.with_env(|env| -> jni::errors::Result<JString<'local>> {
        // No XDG/home base ⇒ we must not fabricate a path (§9); surface a JNI error → null JString.
        let dir =
            app_data_dir().ok_or(jni::errors::Error::JniCall(jni::errors::JniError::Unknown))?;
        // `to_string_lossy` keeps this total for non-UTF-8 paths (rare on Linux); a fabricated path
        // is never produced — `dir` is the resolved XDG/override directory.
        env.new_string(dir.to_string_lossy())
    })
    .resolve::<LogErrorAndDefault>()
}

/// Bind Eclipse's own (non-GTK) backing for `android.os.Environment`'s `native_get_app_data_dir`.
///
/// Locates `android/os/Environment` and registers the native via `RegisterNatives` (which wins over
/// name-based lazy binding — JNI 1.1 spec). Like [`register_context_natives`]/[`register_log_natives`]/
/// [`register_asset_manager_natives`], this MUST run before anything triggers `Environment`'s first
/// active use (`Environment.<clinit>` / `getExternalStorageDirectory`); it is registered before the
/// lifecycle drive, since ART resolves natives lazily during the lifecycle and the framework queries
/// external storage early in init.
///
/// # Safety / soundness
/// `register_native_methods` is `unsafe`: the function pointer must match the declared JNI
/// signature. It does, by construction — [`native_get_app_data_dir`] is written to the exact
/// `()Ljava/lang/String;` descriptor as a static native (`EnvUnowned, JClass`). The native body is
/// `catch_unwind`-guarded via [`EnvUnowned::with_env`], so no Rust panic can cross the JNI boundary
/// (AGENTS.md §2.8).
fn register_environment_natives(env: &mut Env) -> Result<(), FrameworkError> {
    let class = env.find_class(ENVIRONMENT_CLASS)?;
    let methods = [
        // SAFETY: `native_get_app_data_dir` matches the paired `()Ljava/lang/String;` signature as a
        // static native (see the native's docs); casting the `extern "system"` fn to a `*mut c_void`
        // is how `NativeMethod::from_raw_parts` takes it.
        unsafe {
            NativeMethod::from_raw_parts(
                GET_APP_DATA_DIR_NAME,
                GET_APP_DATA_DIR_SIG,
                native_get_app_data_dir as *mut std::ffi::c_void,
            )
        },
    ];
    // SAFETY: `class` is the loaded android/os/Environment; `methods` holds a valid fn pointer whose
    // signature matches the class's `native` declaration (verified against Environment.java line 336,
    // 2026-06-05).
    unsafe { env.register_native_methods(&class, &methods) }?;
    tracing::info!(
        class = "android/os/Environment",
        "registered Eclipse's non-GTK backing for native_get_app_data_dir"
    );
    Ok(())
}

// === Eclipse's own (non-GTK) backing for android.os.SystemClock's monotonic clock ==============
//
// 2026-06-05: the Roblox-APK run surfaced `No implementation found for long
// android.os.SystemClock.elapsedRealtime()` (run log /tmp/eclipse-roblox.log) — thrown from inside
// Roblox's own `com.roblox.client.RobloxApplication.<init>` during step 1 `Context.createApplication`
// (the demo APK never calls it, so it only appears under a real app). This is a benign framework
// timekeeping native, NOT an asset/resource/GTK/engine concern. `SystemClock.java` line 148 declares
//   `native public static long elapsedRealtime();`
// → static native `()J`, documented as "elapsed milliseconds since boot, including time spent in
// sleep" and "guaranteed to be monotonic" (SystemClock.java lines 52–56, 143–148). The load-bearing
// contract is MONOTONICITY (the value is used only as an interval-timing reference). Eclipse backs it
// GTK-free with a process-anchored monotonic clock (`std::time::Instant`, which uses CLOCK_MONOTONIC
// on Linux), returning milliseconds since the first call — sound, no `unsafe`, no libc dep, honoring
// the monotonic guarantee. ATL backs this in C; we do not read that source (denylisted) — the Java
// contract above is the ground truth.

/// `android.os.SystemClock` (internal/slashed name for `find_class`) — hosts the static
/// `elapsedRealtime` monotonic-clock native.
pub const SYSTEM_CLOCK_CLASS: &JNIStr = jni_str!("android/os/SystemClock");

// JNI name + descriptor for SystemClock's native, exactly as declared in `SystemClock.java`
// (2026-06-05, line 148): `native public static long elapsedRealtime();`.
const ELAPSED_REALTIME_NAME: &JNIStr = jni_str!("elapsedRealtime");
const ELAPSED_REALTIME_SIG: &JNIStr = jni_str!("()J");

// JNI name + descriptor for SystemClock.uptimeMillis, from the ART-reported signature `long
// android.os.SystemClock.uptimeMillis()` (run log 2026-06-05, accelerometerdemo's Handler.postDelayed
// timing): a static native, descriptor `()J`. AOSP defines uptimeMillis as "milliseconds since boot,
// not counting deep sleep" and "the basis for most interval timing" — its load-bearing contract is the
// same MONOTONICITY as elapsedRealtime, so it shares the same process-anchored monotonic source.
const UPTIME_MILLIS_NAME: &JNIStr = jni_str!("uptimeMillis");
const UPTIME_MILLIS_SIG: &JNIStr = jni_str!("()J");

/// Process-wide monotonic anchor for [`system_clock_elapsed_realtime`]. Set once on the first call,
/// so `elapsedRealtime()` returns milliseconds since the first query — a correct monotonic clock
/// (the contract guarantees monotonicity, not a true since-boot value). `Instant` is monotonic on
/// Linux (CLOCK_MONOTONIC); `OnceLock` makes the anchor sound across the VM/winit main thread.
static MONOTONIC_ANCHOR: OnceLock<Instant> = OnceLock::new();

/// `SystemClock.elapsedRealtime()` → monotonic milliseconds since the first call, as a `jlong`.
///
/// JNI ABI: a `static` native (the Java method is `static`), so the second argument is the `JClass`.
/// The body runs inside [`EnvUnowned::with_env`], which `catch_unwind`-wraps it so a Rust panic can
/// never unwind into ART's C++ (AGENTS.md §2.8; `panic = "abort"` kept). `resolve::<LogErrorAndDefault>`
/// returns the `jlong` default (`0`) on any error/panic — a sound neutral timestamp. The
/// `OnceLock::get_or_init` anchors the clock on first use; subsequent calls are monotonically
/// non-decreasing. `as_millis()` is `u128`; a 32-bit-day overflow cannot occur in a session, but the
/// `try_from`/`unwrap_or` saturation keeps the native total (no overflow panic).
extern "system" fn system_clock_elapsed_realtime<'local>(
    mut env: EnvUnowned<'local>,
    _class: JClass<'local>,
) -> jlong {
    env.with_env(|_env| -> jni::errors::Result<jlong> { Ok(monotonic_millis()) })
        .resolve::<LogErrorAndDefault>()
}

/// `SystemClock.uptimeMillis()` → monotonic milliseconds since the first call, as a `jlong`.
///
/// JNI ABI: a `static` native (the Java method is `static`), so the second argument is the `JClass`.
/// Shares [`MONOTONIC_ANCHOR`] with [`system_clock_elapsed_realtime`] — AOSP's `uptimeMillis` and
/// `elapsedRealtime` differ only in whether deep sleep is counted, which is irrelevant here (no device
/// sleep), and both must be monotonic. The body is `catch_unwind`-wrapped via [`EnvUnowned::with_env`]
/// (AGENTS.md §2.8); `resolve` returns the `jlong` default (`0`) on error/panic — a sound neutral
/// timestamp.
extern "system" fn system_clock_uptime_millis<'local>(
    mut env: EnvUnowned<'local>,
    _class: JClass<'local>,
) -> jlong {
    env.with_env(|_env| -> jni::errors::Result<jlong> { Ok(monotonic_millis()) })
        .resolve::<LogErrorAndDefault>()
}

/// Monotonic milliseconds since the process's first clock query (the shared body of
/// `elapsedRealtime`/`uptimeMillis`). Anchors [`MONOTONIC_ANCHOR`] on first use; subsequent calls are
/// non-decreasing. `as_millis()` is `u128`; the `try_from`/`unwrap_or` saturates (no overflow panic).
fn monotonic_millis() -> jlong {
    let elapsed_ms = MONOTONIC_ANCHOR
        .get_or_init(Instant::now)
        .elapsed()
        .as_millis();
    jlong::try_from(elapsed_ms).unwrap_or(jlong::MAX)
}

/// Bind Eclipse's own (non-GTK) backing for `android.os.SystemClock`'s `elapsedRealtime`.
///
/// Locates `android/os/SystemClock` and registers the native via `RegisterNatives` (which wins over
/// name-based lazy binding — JNI 1.1 spec). Like [`register_environment_natives`], this MUST run
/// before anything triggers `SystemClock`'s first active use; it is registered before the lifecycle
/// drive, since ART resolves natives lazily and a real app's `Application.<init>` may query the clock
/// during step 1 `Context.createApplication` (observed for Roblox's `RobloxApplication.<init>`).
///
/// # Safety / soundness
/// `register_native_methods` is `unsafe`: the function pointer must match the declared JNI
/// signature. It does, by construction — [`system_clock_elapsed_realtime`] is written to the exact
/// `()J` descriptor as a static native (`EnvUnowned, JClass`). The native body is `catch_unwind`-
/// guarded via [`EnvUnowned::with_env`], so no Rust panic can cross the JNI boundary (AGENTS.md §2.8).
fn register_system_clock_natives(env: &mut Env) -> Result<(), FrameworkError> {
    let class = env.find_class(SYSTEM_CLOCK_CLASS)?;
    let methods = [
        // SAFETY: `system_clock_elapsed_realtime` matches the paired `()J` signature as a static
        // native (see the native's docs); casting the `extern "system"` fn to a `*mut c_void` is how
        // `NativeMethod::from_raw_parts` takes it.
        unsafe {
            NativeMethod::from_raw_parts(
                ELAPSED_REALTIME_NAME,
                ELAPSED_REALTIME_SIG,
                system_clock_elapsed_realtime as *mut std::ffi::c_void,
            )
        },
        // SAFETY: `system_clock_uptime_millis` matches the paired `()J` signature as a static native.
        unsafe {
            NativeMethod::from_raw_parts(
                UPTIME_MILLIS_NAME,
                UPTIME_MILLIS_SIG,
                system_clock_uptime_millis as *mut std::ffi::c_void,
            )
        },
    ];
    // SAFETY: `class` is the loaded android/os/SystemClock; `methods` hold valid fn pointers whose
    // signatures match the class's `native` declarations (verified against SystemClock.java line 148 +
    // the ART-reported `uptimeMillis()J`, 2026-06-05).
    unsafe { env.register_native_methods(&class, &methods) }?;
    tracing::info!(
        class = "android/os/SystemClock",
        "registered Eclipse's non-GTK backing for elapsedRealtime + uptimeMillis"
    );
    Ok(())
}

// === Eclipse's own (non-GTK) backing for android.os.MessageQueue.nativeInit ====================
//
// 2026-06-05: step 0 (`Looper.prepareMainLooper`) builds the main thread's `MessageQueue`, whose
// constructor calls `nativeInit()` and stores the returned `long` as `mPtr`. The first native the
// dev-host run surfaces inside `prepareMainLooper` is `MessageQueue.nativeInit()` (`No
// implementation found for long android.os.MessageQueue.nativeInit()`, run log 2026-06-05 against
// com.ashwin.example.accelerometerdemo). AOSP's `MessageQueue.java` declares it
//   `private native long nativeInit();`
// — an INSTANCE native returning the native message-queue handle. `MessageQueue.<init>` then does
// `if (mPtr == 0) throw new IllegalStateException("Unable to allocate native queue");`, so the only
// Java-side contract is that the returned handle is **non-zero**.
//
// Eclipse drives the lifecycle on a single attached main thread and **never runs `Looper.loop()`**,
// so the queue's polling/wake/destroy natives (`nativePollOnce`/`nativeWake`/`nativeIsPolling`/
// `nativeDestroy`) are never invoked — none are bound, and if one ever were called it would raise a
// clean `UnsatisfiedLinkError` (not UB). Because the returned handle therefore has NO dereferencing
// consumer, a full generational-slab registry (as for window/view/paint handles, which ARE
// dereferenced by later natives) would be dead weight (Simplicity First, AGENTS.md §Surgical). The
// minimal-sound backing returns a stable non-zero sentinel that is plainly NOT a pointer, satisfying
// the `mPtr != 0` contract without faking any message-loop behavior. If a queue-consuming native is
// ever bound (i.e. the lifecycle starts running `Looper.loop()`), this must become a real registry
// handle (mirroring `paint_registry`) so the consumer can validate it — flagged here for that step.

/// `android.os.MessageQueue` (internal/slashed name for `find_class`) — hosts the `nativeInit`
/// queue-allocation native.
pub const MESSAGE_QUEUE_CLASS: &JNIStr = jni_str!("android/os/MessageQueue");

// JNI name + descriptor for MessageQueue's native, exactly as declared in AOSP's `MessageQueue.java`:
// `private native long nativeInit();` → an INSTANCE native, descriptor `()J`. (The ATL framework is
// AOSP-derived; the run's `No implementation found for long android.os.MessageQueue.nativeInit()`
// line + the `MessageQueue.<init> → nativeInit` stack confirm the name/arity/return.)
const MESSAGE_QUEUE_NATIVE_INIT_NAME: &JNIStr = jni_str!("nativeInit");
const MESSAGE_QUEUE_NATIVE_INIT_SIG: &JNIStr = jni_str!("()J");

/// The non-zero, non-pointer sentinel `MessageQueue.nativeInit()` returns as `mPtr`.
///
/// 2026-06-05: Java only checks `mPtr != 0`; this value is never dereferenced (no queue-consuming
/// native is bound — see the section comment). A small, recognizable, plainly-not-a-pointer constant.
const MESSAGE_QUEUE_HANDLE_SENTINEL: jlong = 0x4d51; // 'MQ' — a non-zero, non-pointer marker.

/// `MessageQueue.nativeInit()` → a stable non-zero handle (`mPtr`).
///
/// JNI ABI: an INSTANCE native returning `jlong`, so the parameters are `(EnvUnowned, JObject this)`.
/// `this` is not dereferenced. Returns [`MESSAGE_QUEUE_HANDLE_SENTINEL`] — non-zero so
/// `MessageQueue.<init>`'s `mPtr == 0` guard passes; never a pointer (the handle has no dereferencing
/// consumer in Eclipse's no-`Looper.loop()` lifecycle — see the section comment).
///
/// The body runs inside [`EnvUnowned::with_env`] (`catch_unwind`-wrapped, AGENTS.md §2.8;
/// `panic = "abort"` kept); `resolve::<LogErrorAndDefault>` returns the `jlong` default (`0`) on any
/// error/panic — but the body is infallible, so the sentinel is always returned.
extern "system" fn message_queue_native_init<'local>(
    mut env: EnvUnowned<'local>,
    _this: JObject<'local>,
) -> jlong {
    env.with_env(|_env| -> jni::errors::Result<jlong> {
        tracing::debug!(
            target: "android.os.MessageQueue",
            handle = MESSAGE_QUEUE_HANDLE_SENTINEL,
            "MessageQueue.nativeInit: returning non-GTK non-zero queue sentinel (no Looper.loop)"
        );
        Ok(MESSAGE_QUEUE_HANDLE_SENTINEL)
    })
    .resolve::<LogErrorAndDefault>()
}

/// Bind Eclipse's own (non-GTK) backing for `android.os.MessageQueue`'s `nativeInit`.
///
/// Locates `android/os/MessageQueue` and registers the native via `RegisterNatives` (which wins over
/// name-based lazy binding — JNI 1.1 spec). MUST run before step 0 (`Looper.prepareMainLooper`), which
/// constructs the main `MessageQueue` and calls `nativeInit`; it is registered before the lifecycle
/// drive alongside the other `android.os.*` natives.
///
/// # Safety / soundness
/// `register_native_methods` is `unsafe`: the function pointer must match the declared JNI signature.
/// It does, by construction — [`message_queue_native_init`] is written to the exact `()J` descriptor
/// as an instance native (`EnvUnowned, JObject this`). The native body is `catch_unwind`-guarded via
/// [`EnvUnowned::with_env`], so no Rust panic can cross the JNI boundary (AGENTS.md §2.8).
fn register_message_queue_natives(env: &mut Env) -> Result<(), FrameworkError> {
    let class = env.find_class(MESSAGE_QUEUE_CLASS)?;
    let methods = [
        // SAFETY: `message_queue_native_init` matches the paired `()J` signature as an instance
        // native (see the native's docs); casting the `extern "system"` fn to a `*mut c_void` is how
        // `NativeMethod::from_raw_parts` takes it.
        unsafe {
            NativeMethod::from_raw_parts(
                MESSAGE_QUEUE_NATIVE_INIT_NAME,
                MESSAGE_QUEUE_NATIVE_INIT_SIG,
                message_queue_native_init as *mut std::ffi::c_void,
            )
        },
    ];
    // SAFETY: `class` is the loaded android/os/MessageQueue; `methods` holds a valid fn pointer whose
    // signature matches the class's `native` declaration (AOSP `MessageQueue.java`,
    // `private native long nativeInit()`, 2026-06-05).
    unsafe { env.register_native_methods(&class, &methods) }?;
    tracing::info!(
        class = "android/os/MessageQueue",
        "registered Eclipse's non-GTK backing for nativeInit"
    );
    Ok(())
}

// === Eclipse's honest (no-sensor) backing for android.hardware.SensorManager ===================
//
// 2026-06-05: accelerometerdemo's MainActivity.initViews calls getSystemService(SENSOR_SERVICE) →
// SensorManager.getDefaultSensor(TYPE_ACCELEROMETER) → registerListener(listener, sensor, rate). ATL's
// SensorManager Java implements `registerListener` by calling the native
//   register_accelerometer_listener_native(SensorEventListener listener, Sensor sensor, int rate)
// — an INSTANCE native returning void (the run's `No implementation found for void
// android.hardware.SensorManager.register_accelerometer_listener_native(android.hardware.
// SensorEventListener, android.hardware.Sensor, int)` line + the `registerListener →
// register_accelerometer_listener_native` stack confirm the name/arity/return). Descriptor:
// `(Landroid/hardware/SensorEventListener;Landroid/hardware/Sensor;I)V`.
//
// This Linux desktop has NO accelerometer device. The TRUTHFUL behavior of `registerListener` against
// hardware that is not present is that no event source is wired up and the listener's
// `onSensorChanged` is never invoked — exactly what a real Android device does when an app registers a
// listener for a sensor it lacks (registration succeeds vacuously; no events ever arrive). This native
// therefore validates its arguments and returns without registering any source or fabricating any
// sensor sample (faking accelerometer data is forbidden, AGENTS.md §Core Principle). The app's listener
// stays dormant and its UI simply shows no readings — its normal no-sensor path. No GTK, no registry
// handle (nothing later dereferences anything this returns — it is void), and no event-delivery thread
// is started (none exists to start: there is no sensor). If a future host gains a real sensor source,
// this is the single seam to wire it to.

/// `android.hardware.SensorManager` (internal/slashed name for `find_class`) — hosts ATL's
/// accelerometer-listener registration native.
pub const SENSOR_MANAGER_CLASS: &JNIStr = jni_str!("android/hardware/SensorManager");

// JNI name + descriptor for ATL's SensorManager registration native, exactly as ART reported it
// missing (run log 2026-06-05, accelerometerdemo): `void register_accelerometer_listener_native(
// SensorEventListener, Sensor, int)` → an INSTANCE native, descriptor
// `(Landroid/hardware/SensorEventListener;Landroid/hardware/Sensor;I)V`.
const SENSOR_MANAGER_REGISTER_NAME: &JNIStr = jni_str!("register_accelerometer_listener_native");
const SENSOR_MANAGER_REGISTER_SIG: &JNIStr =
    jni_str!("(Landroid/hardware/SensorEventListener;Landroid/hardware/Sensor;I)V");

/// `SensorManager.register_accelerometer_listener_native(listener, sensor, rate)` → honest no-op.
///
/// JNI ABI: an INSTANCE native returning void, so the parameters are `(EnvUnowned, JObject this,
/// JObject listener, JObject sensor, jint rate)`. None of the objects are dereferenced. On this
/// no-accelerometer host the truthful behavior is to register no event source and deliver no
/// `onSensorChanged` callbacks — the same outcome a real device gives an app that registers a listener
/// for an absent sensor. No sensor data is fabricated.
///
/// The body runs inside [`EnvUnowned::with_env`] (`catch_unwind`-wrapped, AGENTS.md §2.8;
/// `panic = "abort"` kept); `resolve` returns the `()` default on any error/panic. The body is
/// infallible.
extern "system" fn sensor_manager_register_accelerometer_listener<'local>(
    mut env: EnvUnowned<'local>,
    _this: JObject<'local>,
    _listener: JObject<'local>,
    _sensor: JObject<'local>,
    rate: jint,
) {
    env.with_env(|_env| -> jni::errors::Result<()> {
        tracing::debug!(
            target: "android.hardware.SensorManager",
            rate,
            "SensorManager.register_accelerometer_listener_native: no accelerometer on this host; \
             registering no source, delivering no events (honest no-sensor)"
        );
        Ok(())
    })
    .resolve::<LogErrorAndDefault>()
}

/// Bind Eclipse's honest (no-sensor) backing for `android.hardware.SensorManager`.
///
/// Locates `android/hardware/SensorManager` and registers the native via `RegisterNatives` (which wins
/// over name-based lazy binding — JNI 1.1 spec). Registered before the lifecycle drive alongside the
/// other framework natives, since an app may register a sensor listener during `Activity.onCreate`
/// (accelerometerdemo does, in `initViews`).
///
/// # Safety / soundness
/// `register_native_methods` is `unsafe`: the function pointer must match the declared JNI signature.
/// It does, by construction — [`sensor_manager_register_accelerometer_listener`] is written to the
/// exact `(Landroid/hardware/SensorEventListener;Landroid/hardware/Sensor;I)V` descriptor as an
/// instance native. The body is `catch_unwind`-guarded via [`EnvUnowned::with_env`], so no Rust panic
/// can cross the JNI boundary (AGENTS.md §2.8).
fn register_sensor_manager_natives(env: &mut Env) -> Result<(), FrameworkError> {
    let class = env.find_class(SENSOR_MANAGER_CLASS)?;
    let methods = [
        // SAFETY: `sensor_manager_register_accelerometer_listener` matches the paired
        // `(Landroid/hardware/SensorEventListener;Landroid/hardware/Sensor;I)V` signature as an
        // instance native (see the native's docs); casting the `extern "system"` fn to a `*mut c_void`
        // is how `NativeMethod::from_raw_parts` takes it.
        unsafe {
            NativeMethod::from_raw_parts(
                SENSOR_MANAGER_REGISTER_NAME,
                SENSOR_MANAGER_REGISTER_SIG,
                sensor_manager_register_accelerometer_listener as *mut std::ffi::c_void,
            )
        },
    ];
    // SAFETY: `class` is the loaded android/hardware/SensorManager; `methods` holds a valid fn pointer
    // whose signature matches the class's `native` declaration (ART-reported `No implementation found`
    // line, 2026-06-05).
    unsafe { env.register_native_methods(&class, &methods) }?;
    tracing::info!(
        class = "android/hardware/SensorManager",
        "registered Eclipse's honest no-sensor backing for register_accelerometer_listener_native"
    );
    Ok(())
}

// === Eclipse's own (non-GTK) backing for android.view.View native peer construction =============
//
// 2026-06-05: step 4 (`Activity.createMainActivity`) constructs the launcher Activity, whose
// view hierarchy is fully native-handle-backed. The first native the dev-host run surfaces is
// `View.native_constructor(Context, AttributeSet)` (`No implementation found for long
// android.view.View.native_constructor(...)`, run log 2026-06-05). `View.java` line 1166 declares it
//   `protected native long native_constructor(Context context, AttributeSet attrs);` // GtkWidget
// — an INSTANCE native returning the native View peer handle (a `long`). ATL's C backing creates a
// GtkWidget; Eclipse must NOT pull in GTK (AGENTS.md §5 Step 3.5). Eclipse's GTK-free equivalent
// allocates a sound [`view_registry`] slot (keyed on the receiver's actual Java class name, so the
// recorded tree shows FrameLayout/TextView/etc.) and returns that slab handle — a generational index,
// NOT a raw pointer, so a stale/fabricated handle from later View natives is a bounds+generation-
// checked `Err`, never UB. No layout/measure/draw and no winit/Vulkan surface is created here (the
// deferred big build); only the view-tree metadata is recorded — sound, not behavior-faking.

/// `android.view.View` (internal/slashed name for `find_class`) — hosts the View peer natives.
pub const VIEW_CLASS: &JNIStr = jni_str!("android/view/View");

// === Eclipse → real Android MotionEvent touch dispatch (2026-06-05) ===========================
//
// 2026-06-05: the proper Android input event for a pointer touch is a `MotionEvent` routed to the
// hit View via `dispatchTouchEvent` (which itself calls `onTouchEvent` + the View's click
// detection). This is the faithful follow-up to INPUT v0's `performClick`-only path. We build the
// event with the PUBLIC Java factory `MotionEvent.obtain(...)` (a recycler-pool allocation, all
// Java; no Eclipse native is needed unless the ART surfaces one), dispatch it, then `recycle()` it.
// Single-pointer DOWN/UP only this increment; multi-touch/MOVE/key are the documented follow-ups.

/// `android.view.MotionEvent` (internal/slashed name for `find_class`) — hosts the public static
/// `obtain(...)` factory and the instance `recycle()`. 2026-06-05.
///
/// The touch-dispatch call sites use inline `jni_str!`/`jni_sig!` literals (single source of truth,
/// no runtime signature parse), pinned against this module's documented descriptors by the unit test
/// `motion_event_dispatch_descriptors_are_the_public_android_api`:
///   * `MotionEvent.obtain(long downTime, long eventTime, int action, float x, float y, int metaState)`
///     → `(JJIFFI)Landroid/view/MotionEvent;` (the public Java recycler-pool factory; Eclipse calls,
///     does not back it).
///   * `MotionEvent.recycle()` → `()V` (returns the event to the recycler pool).
///   * `View.dispatchTouchEvent(MotionEvent)` → `(Landroid/view/MotionEvent;)Z` (routes through
///     `onTouchEvent` + the View's click detection).
///
/// All three are stable, general public Android API.
pub const MOTION_EVENT_CLASS: &JNIStr = jni_str!("android/view/MotionEvent");

// JNI name + descriptor for View's native peer constructor, exactly as declared in `View.java`
// (2026-06-05, line 1166): `protected native long native_constructor(Context context, AttributeSet
// attrs);` → an instance native, descriptor `(Landroid/content/Context;Landroid/util/AttributeSet;)J`.
const VIEW_NATIVE_CONSTRUCTOR_NAME: &JNIStr = jni_str!("native_constructor");
const VIEW_NATIVE_CONSTRUCTOR_SIG: &JNIStr =
    jni_str!("(Landroid/content/Context;Landroid/util/AttributeSet;)J");

// JNI name + descriptor for View.native_setPadding, exactly as declared in `View.java` (2026-06-05,
// line 1310): `public native void native_setPadding(long widget, int left, int top, int right, int
// bottom);` → an instance native, descriptor `(JIIII)V`. Surfaced by the dev-host run during
// `View.<init>` (run log 2026-06-05). Padding is layout data Eclipse does not act on yet (no
// layout/draw without the deferred surface), so the backing validates the view handle and no-ops.
const VIEW_NATIVE_SET_PADDING_NAME: &JNIStr = jni_str!("native_setPadding");
const VIEW_NATIVE_SET_PADDING_SIG: &JNIStr = jni_str!("(JIIII)V");

// JNI name + descriptor for View.native_setLayoutParams, exactly as declared in `View.java`
// (2026-06-05, line 1167): `public native void native_setLayoutParams(long widget, int width, int
// height, int gravity, float weight, int leftMargin, int topMargin, int rightMargin, int
// bottomMargin);` → an instance native, descriptor `(JIIIFIIII)V`. Surfaced during `ViewGroup.addView`
// (run log 2026-06-05). Layout sizing/margins are data Eclipse does not act on yet (deferred layout),
// so the backing validates the view handle and no-ops.
const VIEW_NATIVE_SET_LAYOUT_PARAMS_NAME: &JNIStr = jni_str!("native_setLayoutParams");
const VIEW_NATIVE_SET_LAYOUT_PARAMS_SIG: &JNIStr = jni_str!("(JIIIFIIII)V");

// JNI name + descriptor for View.native_requestLayout, exactly as declared in `View.java`
// (2026-06-05, line 1175): `protected native void native_requestLayout(long widget);` → an instance
// native, descriptor `(J)V`. Surfaced during `ViewGroup.addView` → `View.requestLayout` (run log
// 2026-06-05). Layout invalidation is a no-op until real layout lands; the backing validates the
// handle and no-ops.
const VIEW_NATIVE_REQUEST_LAYOUT_NAME: &JNIStr = jni_str!("native_requestLayout");
const VIEW_NATIVE_REQUEST_LAYOUT_SIG: &JNIStr = jni_str!("(J)V");

// 2026-06-05: `View.setBackgroundDrawable` calls `native_setBackgroundDrawable(widget, drawable)` to
// attach a background drawable to a view. It was unreached until themes resolved
// `android:windowBackground` (which makes `setContentView → Window/View.setBackgroundDrawable` run);
// the ART error line gives the exact signature `void android.view.View.native_setBackgroundDrawable(
// long, long)` → `(JJ)V` instance. `drawable` is a Drawable peer handle (the non-pointer sentinel
// from `Drawable.native_constructor`), NOT a view/registry handle, so it is taken but not dereferenced;
// the real background draw is the deferred 2D/Skia path. Validates the view handle + no-op.
const VIEW_NATIVE_SET_BACKGROUND_DRAWABLE_NAME: &JNIStr = jni_str!("native_setBackgroundDrawable");
const VIEW_NATIVE_SET_BACKGROUND_DRAWABLE_SIG: &JNIStr = jni_str!("(JJ)V");

// 2026-06-05: `View.<init>` (and `setVisibility`/`setAlpha`) call `native_setVisibility(widget,
// visibility, alpha)` to push the view's visibility (VISIBLE=0/INVISIBLE=4/GONE=8) and alpha onto its
// native peer. Surfaced when AppCompat's sub-decor inflation constructed an `ActionBarContextView`
// (`View.<init>` → `native_setVisibility`, run log 2026-06-05). The ART error line gives the exact
// signature `void android.view.View.native_setVisibility(long, int, float)` → `(JIF)V` instance.
// Validates the view handle + no-op: the snapshot renderer does not yet consume visibility/alpha (a
// GONE view should be skipped in layout — documented follow-up), so recording them is not yet
// load-bearing; the handle check keeps it sound.
const VIEW_NATIVE_SET_VISIBILITY_NAME: &JNIStr = jni_str!("native_setVisibility");
const VIEW_NATIVE_SET_VISIBILITY_SIG: &JNIStr = jni_str!("(JIF)V");

// 2026-06-05: `View.setOnClickListener` calls `nativeSetOnClickListener(widget)` directly on the
// `android.view.View` class (multitouch.test's custom View registers a click listener — run log
// `No implementation found for void android.view.View.nativeSetOnClickListener(long)`). The same
// native was already bound on `ImageButton` (resolved per declaring class); the handler
// [`image_button_set_on_click_listener`] is class-agnostic (it marks the peer clickable in
// [`view_registry`]), so the View-class binding reuses it. Instance native, descriptor `(J)V`.
const VIEW_SET_ON_CLICK_LISTENER_NAME: &JNIStr = jni_str!("nativeSetOnClickListener");
const VIEW_SET_ON_CLICK_LISTENER_SIG: &JNIStr = jni_str!("(J)V");

// 2026-06-05: `View.setBackgroundColor` calls `native_setBackgroundColor(long widget, int color)` to
// set a solid background fill on the native peer; surfaced by multitouch.test (run log `No
// implementation found for void android.view.View.native_setBackgroundColor(long, int)`). `color` is
// `Color.argb`/`0xAARRGGBB`. Eclipse RECORDS it on the `view_registry` peer; the renderer fills the
// view's rect with this color (real fidelity over the synthetic depth color). Instance native, `(JI)V`.
const VIEW_SET_BACKGROUND_COLOR_NAME: &JNIStr = jni_str!("native_setBackgroundColor");
const VIEW_SET_BACKGROUND_COLOR_SIG: &JNIStr = jni_str!("(JI)V");

/// `View.native_constructor(Context, AttributeSet)` → a real Eclipse-owned [`view_registry`] handle.
///
/// JNI ABI: an INSTANCE native returning `jlong`, so the parameters are
/// `(EnvUnowned, JObject this, JObject context, JObject attrs)`. `context`/`attrs` are not
/// dereferenced (a GTK widget would consume them; Eclipse records metadata only). Resolves the
/// receiver's actual Java class name (`this.getClass().getName()`) so the recorded view tree names
/// the concrete subclass (e.g. `android.widget.FrameLayout`), allocates a [`view_registry`] slot with
/// it, and returns the slab handle (≥ 1, never `0`). On any failure returns `0` (no peer) — which the
/// framework treats as a failed construct, never a fake success.
///
/// The body runs inside [`EnvUnowned::with_env`] (`catch_unwind`-wrapped, AGENTS.md §2.8;
/// `panic = "abort"` kept); `resolve::<LogErrorAndDefault>` returns the `jlong` default (`0`) on any
/// error/panic — a sound neutral "no peer" handle.
extern "system" fn view_native_constructor<'local>(
    mut env: EnvUnowned<'local>,
    this: JObject<'local>,
    _context: JObject<'local>,
    _attrs: JObject<'local>,
) -> jlong {
    env.with_env(|env| -> jni::errors::Result<jlong> {
        let class_name = view_class_name(env, &this).unwrap_or_default();
        match view_registry::allocate(&class_name) {
            Ok(handle) => {
                // 2026-06-05: record a JNI global ref to the Java View object so a pointer click
                // resolved to this view (by handle) can dispatch View.performClick() on the REAL
                // object from the event loop (firing its OnClickListener). A failure to create the
                // global ref (or to store it) leaves the view non-dispatchable but still drawn —
                // logged, never fatal. `new_global_ref` over a non-null `this` is sound here.
                match env.new_global_ref(&this) {
                    Ok(global) => {
                        if let Err(e) = view_registry::set_jobject(handle, global) {
                            tracing::debug!(
                                target: "android.view.View",
                                class = %class_name,
                                handle,
                                error = %e,
                                "View.native_constructor: could not store view jobject (non-dispatchable)"
                            );
                        }
                    }
                    Err(e) => tracing::debug!(
                        target: "android.view.View",
                        class = %class_name,
                        handle,
                        error = %e,
                        "View.native_constructor: new_global_ref failed (view non-dispatchable)"
                    ),
                }
                tracing::debug!(
                    target: "android.view.View",
                    class = %class_name,
                    handle,
                    "View.native_constructor: allocated non-GTK view-registry peer"
                );
                Ok(handle)
            }
            Err(e) => {
                tracing::warn!(
                    target: "android.view.View",
                    class = %class_name,
                    error = %e,
                    "View.native_constructor: view-registry allocate failed → 0 (no peer)"
                );
                Ok(0)
            }
        }
    })
    .resolve::<LogErrorAndDefault>()
}

/// Resolve a Java object's concrete class name (dotted, e.g. `android.widget.FrameLayout`) via
/// `obj.getClass().getName()`. Returns `None` on any JNI error (the caller then records an empty
/// class name — harmless for the tree shape). Off the gameplay hot path (view construction only).
fn view_class_name(env: &mut Env, obj: &JObject) -> Option<String> {
    let class = env.get_object_class(obj).ok()?;
    let name = env
        .call_method(
            &class,
            jni_str!("getName"),
            jni_sig!("()Ljava/lang/String;"),
            &[],
        )
        .ok()?
        .l()
        .ok()?;
    // getName() returns a java.lang.String; cast_local is a safe, runtime-checked JObject→JString
    // cast (returns Err if it were ever not a String — never UB).
    let name = JString::cast_local(env, name).ok()?;
    name.try_to_string(env).ok()
}

/// `View.native_setPadding(long widget, int left, int top, int right, int bottom)` → record the
/// padding on the view's [`view_registry`] peer (2026-06-05).
///
/// JNI ABI: an INSTANCE native returning void, so the parameters are
/// `(EnvUnowned, JObject this, jlong widget, jint left, jint top, jint right, jint bottom)`. Padding
/// is the gap inside the view around its content; Eclipse's measure/layout pass (`graphics.rs`) honors
/// it when sizing/positioning. This records `[left, top, right, bottom]` onto the peer's
/// [`view_registry::LayoutParams::padding`] through the bounds+generation-checked registry (a
/// stale/fabricated handle is logged + ignored, never UB).
///
/// The body runs inside [`EnvUnowned::with_env`] (`catch_unwind`-wrapped, AGENTS.md §2.8;
/// `panic = "abort"` kept); `resolve::<LogErrorAndDefault>` returns the `()` default on error/panic.
extern "system" fn view_native_set_padding<'local>(
    mut env: EnvUnowned<'local>,
    _this: JObject<'local>,
    widget: jlong,
    left: jint,
    top: jint,
    right: jint,
    bottom: jint,
) {
    env.with_env(|_env| -> jni::errors::Result<()> {
        match view_registry::with_view(widget, |v| {
            v.layout.padding = [left, top, right, bottom];
        }) {
            Ok(()) => tracing::trace!(
                target: "android.view.View",
                widget, left, top, right, bottom,
                "View.native_setPadding: recorded padding on view peer"
            ),
            Err(e) => tracing::debug!(
                target: "android.view.View",
                widget,
                error = %e,
                "View.native_setPadding: invalid view handle (ignored)"
            ),
        }
        Ok(())
    })
    .resolve::<LogErrorAndDefault>()
}

/// `View.native_setLayoutParams(long widget, int width, int height, int gravity, float weight,
/// int leftMargin, int topMargin, int rightMargin, int bottomMargin)` → record the layout params on
/// the view's [`view_registry`] peer (2026-06-05).
///
/// JNI ABI: an INSTANCE native returning void (`View.java` line 1167). `width`/`height` use Android's
/// sentinels (`MATCH_PARENT` = -1, `WRAP_CONTENT` = -2, else exact px), `gravity` is the packed
/// `layout_gravity` bitmask, `weight` the `layout_weight`. Eclipse's measure/layout pass (`graphics.rs`)
/// consumes these to compute each view's absolute rect. This records them onto the peer's
/// [`view_registry::LayoutParams`] through the bounds+generation-checked registry (a bad handle is
/// logged + ignored, never UB), preserving any padding already set by `native_setPadding`.
///
/// The body runs inside [`EnvUnowned::with_env`] (`catch_unwind`-wrapped, §2.8); `resolve` returns
/// the `()` default on error/panic.
//
// 2026-06-05: arity is fixed by View.java's declaration (9 args after `this`); clippy's
// `too_many_arguments` does not fire on `extern "system"` fns.
extern "system" fn view_native_set_layout_params<'local>(
    mut env: EnvUnowned<'local>,
    _this: JObject<'local>,
    widget: jlong,
    width: jint,
    height: jint,
    gravity: jint,
    weight: f32,
    left_margin: jint,
    top_margin: jint,
    right_margin: jint,
    bottom_margin: jint,
) {
    env.with_env(|_env| -> jni::errors::Result<()> {
        match view_registry::with_view(widget, |v| {
            v.layout.width = width;
            v.layout.height = height;
            v.layout.gravity = gravity;
            v.layout.weight = weight;
            v.layout.margins = [left_margin, top_margin, right_margin, bottom_margin];
        }) {
            Ok(()) => tracing::trace!(
                target: "android.view.View",
                widget, width, height, gravity, weight,
                "View.native_setLayoutParams: recorded layout params on view peer"
            ),
            Err(e) => tracing::debug!(
                target: "android.view.View",
                widget,
                error = %e,
                "View.native_setLayoutParams: invalid view handle (ignored)"
            ),
        }
        Ok(())
    })
    .resolve::<LogErrorAndDefault>()
}

/// `View.native_requestLayout(long widget)` → validate handle; no-op (layout deferred, 2026-06-05).
///
/// JNI ABI: an INSTANCE native returning void (`View.java` line 1175). Layout invalidation is a no-op
/// until real layout lands; validates the `widget` handle through the bounds+generation-checked
/// [`view_registry`] (a bad handle is logged + ignored, never UB).
///
/// The body runs inside [`EnvUnowned::with_env`] (`catch_unwind`-wrapped, §2.8); `resolve` returns
/// the `()` default on error/panic.
extern "system" fn view_native_request_layout<'local>(
    mut env: EnvUnowned<'local>,
    _this: JObject<'local>,
    widget: jlong,
) {
    env.with_env(|_env| -> jni::errors::Result<()> {
        if let Err(e) = view_registry::with_view(widget, |_v| ()) {
            tracing::debug!(
                target: "android.view.View",
                widget,
                error = %e,
                "View.native_requestLayout: invalid view handle (ignored)"
            );
        }
        Ok(())
    })
    .resolve::<LogErrorAndDefault>()
}

/// `View.native_setBackgroundDrawable(long widget, long drawable)` → validate the view handle; no-op
/// (background draw deferred to the 2D/Skia path, 2026-06-05).
///
/// JNI ABI: an INSTANCE native returning void. `widget` is the view's [`view_registry`] handle;
/// `drawable` is a `Drawable` peer handle (the non-pointer sentinel from `Drawable.native_constructor`)
/// — taken but NOT dereferenced (it is not a registry handle). Validates the `widget` handle through
/// the bounds+generation-checked [`view_registry`] (a bad handle is logged + ignored, never UB) and
/// no-ops; the actual background rasterization is the deferred drawable/Skia render. Surfaced once
/// theme resolution let `setContentView → setBackgroundDrawable` run.
///
/// The body runs inside [`EnvUnowned::with_env`] (`catch_unwind`-wrapped, §2.8); `resolve` returns
/// the `()` default on error/panic.
extern "system" fn view_native_set_background_drawable<'local>(
    mut env: EnvUnowned<'local>,
    _this: JObject<'local>,
    widget: jlong,
    drawable: jlong,
) {
    env.with_env(|_env| -> jni::errors::Result<()> {
        if let Err(e) = view_registry::with_view(widget, |_v| ()) {
            tracing::debug!(
                target: "android.view.View",
                widget,
                drawable,
                error = %e,
                "View.native_setBackgroundDrawable: invalid view handle (ignored)"
            );
        } else {
            tracing::trace!(
                target: "android.view.View",
                widget,
                drawable,
                "View.native_setBackgroundDrawable: validated handle, no-op (drawable draw deferred)"
            );
        }
        Ok(())
    })
    .resolve::<LogErrorAndDefault>()
}

/// `View.native_setVisibility(long widget, int visibility, float alpha)` → validate the view handle;
/// no-op (visibility/alpha not yet consumed by the snapshot renderer, 2026-06-05).
///
/// JNI ABI: an INSTANCE native returning void. `widget` is the view's [`view_registry`] handle;
/// `visibility` is `View.VISIBLE`(0)/`INVISIBLE`(4)/`GONE`(8) and `alpha` is `[0,1]`. Validates the
/// handle through the bounds+generation-checked [`view_registry`] (a bad handle is logged + ignored,
/// never UB) and no-ops: the snapshot layout/draw pass does not yet honor visibility/alpha (a GONE
/// view should be excluded from layout — a documented follow-up), so recording them is not yet
/// load-bearing for advancing. Surfaced when AppCompat's sub-decor inflation built an
/// `ActionBarContextView` (`View.<init>` → `native_setVisibility`).
///
/// The body runs inside [`EnvUnowned::with_env`] (`catch_unwind`-wrapped, §2.8); `resolve` returns
/// the `()` default on error/panic.
extern "system" fn view_native_set_visibility<'local>(
    mut env: EnvUnowned<'local>,
    _this: JObject<'local>,
    widget: jlong,
    visibility: jint,
    alpha: f32,
) {
    env.with_env(|_env| -> jni::errors::Result<()> {
        if let Err(e) = view_registry::with_view(widget, |_v| ()) {
            tracing::debug!(
                target: "android.view.View",
                widget,
                visibility,
                alpha,
                error = %e,
                "View.native_setVisibility: invalid view handle (ignored)"
            );
        }
        Ok(())
    })
    .resolve::<LogErrorAndDefault>()
}

/// `View.native_setBackgroundColor(long widget, int color)` → record the solid ARGB background color
/// on the view's [`view_registry`] peer (2026-06-05).
///
/// JNI ABI: an INSTANCE native returning void. `widget` is the view's [`view_registry`] handle;
/// `color` is `Color.argb`/`0xAARRGGBB`. Eclipse records it through the bounds+generation-checked
/// [`view_registry::set_background_color`] (a bad handle is logged + ignored, never UB); the renderer's
/// layout pass fills the view's rect with this color for real fidelity (vs the synthetic depth color).
/// Surfaced 2026-06-05 by multitouch.test setting a background on its content view.
///
/// The body runs inside [`EnvUnowned::with_env`] (`catch_unwind`-wrapped, §2.8); `resolve` returns the
/// `()` default on error/panic.
extern "system" fn view_native_set_background_color<'local>(
    mut env: EnvUnowned<'local>,
    _this: JObject<'local>,
    widget: jlong,
    color: jint,
) {
    env.with_env(|_env| -> jni::errors::Result<()> {
        match view_registry::set_background_color(widget, color) {
            Ok(()) => tracing::trace!(
                target: "android.view.View",
                widget,
                color = format_args!("0x{:08x}", u32::from_ne_bytes(color.to_ne_bytes())),
                "View.native_setBackgroundColor: recorded background color on view peer"
            ),
            Err(e) => tracing::debug!(
                target: "android.view.View",
                widget,
                error = %e,
                "View.native_setBackgroundColor: invalid view handle (ignored)"
            ),
        }
        Ok(())
    })
    .resolve::<LogErrorAndDefault>()
}

/// Bind Eclipse's own (non-GTK) backing for `android.view.View`'s peer natives.
///
/// Registered before the lifecycle drive, alongside the other framework natives, since step 4
/// (`Activity.createMainActivity`) constructs Views during the lifecycle. Each new View native the
/// dev-host run surfaces (`No implementation found …`) is added here, implemented against
/// [`view_registry`].
///
/// # Safety / soundness
/// `register_native_methods` is `unsafe`: each fn pointer must match the declared JNI signature.
/// They do, by construction — each native is written to the exact descriptor declared in `View.java`.
/// Every native body is `catch_unwind`-guarded via [`EnvUnowned::with_env`], so no Rust panic crosses
/// the JNI boundary (AGENTS.md §2.8).
fn register_view_natives(env: &mut Env) -> Result<(), FrameworkError> {
    let class = env.find_class(VIEW_CLASS)?;
    let methods = [
        // SAFETY: `view_native_constructor` matches the paired
        // `(Landroid/content/Context;Landroid/util/AttributeSet;)J` signature as an instance native
        // (see the native's docs); casting the `extern "system"` fn to a `*mut c_void` is how
        // `NativeMethod::from_raw_parts` takes it.
        unsafe {
            NativeMethod::from_raw_parts(
                VIEW_NATIVE_CONSTRUCTOR_NAME,
                VIEW_NATIVE_CONSTRUCTOR_SIG,
                view_native_constructor as *mut std::ffi::c_void,
            )
        },
        // SAFETY: `view_native_set_padding` matches the paired `(JIIII)V` signature as an instance
        // native (see the native's docs); casting the `extern "system"` fn to a `*mut c_void` is how
        // `NativeMethod::from_raw_parts` takes it.
        unsafe {
            NativeMethod::from_raw_parts(
                VIEW_NATIVE_SET_PADDING_NAME,
                VIEW_NATIVE_SET_PADDING_SIG,
                view_native_set_padding as *mut std::ffi::c_void,
            )
        },
        // SAFETY: `view_native_set_layout_params` matches the paired `(JIIIFIIII)V` signature as an
        // instance native (see the native's docs); casting the `extern "system"` fn to a
        // `*mut c_void` is how `NativeMethod::from_raw_parts` takes it.
        unsafe {
            NativeMethod::from_raw_parts(
                VIEW_NATIVE_SET_LAYOUT_PARAMS_NAME,
                VIEW_NATIVE_SET_LAYOUT_PARAMS_SIG,
                view_native_set_layout_params as *mut std::ffi::c_void,
            )
        },
        // SAFETY: `view_native_request_layout` matches the paired `(J)V` signature as an instance
        // native (see the native's docs); casting the `extern "system"` fn to a `*mut c_void` is how
        // `NativeMethod::from_raw_parts` takes it.
        unsafe {
            NativeMethod::from_raw_parts(
                VIEW_NATIVE_REQUEST_LAYOUT_NAME,
                VIEW_NATIVE_REQUEST_LAYOUT_SIG,
                view_native_request_layout as *mut std::ffi::c_void,
            )
        },
        // SAFETY: `view_native_set_background_drawable` matches the paired `(JJ)V` signature as an
        // instance native (see the native's docs); casting the `extern "system"` fn to a
        // `*mut c_void` is how `NativeMethod::from_raw_parts` takes it.
        unsafe {
            NativeMethod::from_raw_parts(
                VIEW_NATIVE_SET_BACKGROUND_DRAWABLE_NAME,
                VIEW_NATIVE_SET_BACKGROUND_DRAWABLE_SIG,
                view_native_set_background_drawable as *mut std::ffi::c_void,
            )
        },
        // SAFETY: `view_native_set_visibility` matches the paired `(JIF)V` signature as an instance
        // native (see the native's docs); casting the `extern "system"` fn to a `*mut c_void` is how
        // `NativeMethod::from_raw_parts` takes it.
        unsafe {
            NativeMethod::from_raw_parts(
                VIEW_NATIVE_SET_VISIBILITY_NAME,
                VIEW_NATIVE_SET_VISIBILITY_SIG,
                view_native_set_visibility as *mut std::ffi::c_void,
            )
        },
        // SAFETY: `image_button_set_on_click_listener` is the class-agnostic `(J)V` instance native
        // that marks the peer clickable in `view_registry`; bound here for `View.nativeSetOnClickListener`
        // (surfaced by multitouch.test's custom View, run log 2026-06-05).
        unsafe {
            NativeMethod::from_raw_parts(
                VIEW_SET_ON_CLICK_LISTENER_NAME,
                VIEW_SET_ON_CLICK_LISTENER_SIG,
                image_button_set_on_click_listener as *mut std::ffi::c_void,
            )
        },
        // SAFETY: `view_native_set_background_color` matches the paired `(JI)V` signature as an instance
        // native (surfaced by multitouch.test, run log 2026-06-05).
        unsafe {
            NativeMethod::from_raw_parts(
                VIEW_SET_BACKGROUND_COLOR_NAME,
                VIEW_SET_BACKGROUND_COLOR_SIG,
                view_native_set_background_color as *mut std::ffi::c_void,
            )
        },
    ];
    // SAFETY: `class` is the loaded android/view/View; `methods` hold valid fn pointers whose
    // signatures match the class's `native` declarations (verified against View.java lines 1166/1310,
    // 2026-06-05; `native_setBackgroundDrawable`/`native_setVisibility`/`nativeSetOnClickListener` from
    // the ART No-implementation-found lines, 2026-06-05).
    unsafe { env.register_native_methods(&class, &methods) }?;
    tracing::info!(
        class = "android/view/View",
        "registered Eclipse's non-GTK backing for View.native_constructor + native_setPadding + native_setLayoutParams + native_requestLayout + native_setBackgroundDrawable + native_setVisibility + nativeSetOnClickListener + native_setBackgroundColor"
    );
    Ok(())
}

// === Eclipse's own (non-GTK) backing for android.view.ViewGroup tree wiring =====================
//
// 2026-06-05: `setContentView` → `LayoutInflater.rInflate` → `ViewGroup.addView` wires the inflated
// child views into their parent, surfacing `ViewGroup.native_addView(long parent, long child, int
// index, ViewGroup$LayoutParams params)` (run log 2026-06-05). `ViewGroup.java` (line 186) declares
//   `protected native void native_addView(long widget, long child, int index, LayoutParams params);`
// — an INSTANCE native. ATL's C backing reparents GtkWidgets; Eclipse must NOT pull GTK (AGENTS.md
// §5 Step 3.5), so it records the parent→child TREE EDGE in [`view_registry`] (the `children` field) —
// the actual view hierarchy, sound + handle-checked, with no GTK and no layout/draw (deferred). This
// is what `set_widget_as_root` + `native_addView` together make into a queryable view tree.

/// `android.view.ViewGroup` (internal/slashed name for `find_class`) — hosts the tree-wiring natives.
pub const VIEW_GROUP_CLASS: &JNIStr = jni_str!("android/view/ViewGroup");

// JNI name + descriptor for ViewGroup.native_addView, exactly as declared in `ViewGroup.java`
// (2026-06-05, line 186): `protected native void native_addView(long widget, long child, int index,
// LayoutParams params);` → an instance native, descriptor
// `(JJILandroid/view/ViewGroup$LayoutParams;)V`.
const VIEW_GROUP_NATIVE_ADD_VIEW_NAME: &JNIStr = jni_str!("native_addView");
const VIEW_GROUP_NATIVE_ADD_VIEW_SIG: &JNIStr =
    jni_str!("(JJILandroid/view/ViewGroup$LayoutParams;)V");

// JNI name + descriptor for ViewGroup.native_removeView, from the ART-reported signature `void
// android.view.ViewGroup.native_removeView(long, long)` (run log 2026-06-05, multitouch.test's
// `MultitouchTest.onCreate` re-parenting its content): an instance native, descriptor `(JJ)V` (the
// parent widget handle + the child widget handle).
const VIEW_GROUP_NATIVE_REMOVE_VIEW_NAME: &JNIStr = jni_str!("native_removeView");
const VIEW_GROUP_NATIVE_REMOVE_VIEW_SIG: &JNIStr = jni_str!("(JJ)V");

/// `ViewGroup.native_addView(long parent, long child, int index, ViewGroup.LayoutParams params)` →
/// record the parent→child tree edge in [`view_registry`].
///
/// JNI ABI: an INSTANCE native returning void, so the parameters are
/// `(EnvUnowned, JObject this, jlong parent, jlong child, jint index, JObject params)`. Validates the
/// `child` handle, then inserts it into the `parent` view's `children` at `index` (clamped into
/// range) — both through the bounds+generation-checked [`view_registry`] (a bad handle is logged +
/// ignored, never UB). `params` is not dereferenced (layout deferred). This builds the real view tree
/// edges without GTK.
///
/// The body runs inside [`EnvUnowned::with_env`] (`catch_unwind`-wrapped, AGENTS.md §2.8;
/// `panic = "abort"` kept); `resolve::<LogErrorAndDefault>` returns the `()` default on error/panic.
extern "system" fn view_group_native_add_view<'local>(
    mut env: EnvUnowned<'local>,
    _this: JObject<'local>,
    parent: jlong,
    child: jlong,
    index: jint,
    _params: JObject<'local>,
) {
    env.with_env(|_env| -> jni::errors::Result<()> {
        // Only record a valid child handle as an edge (a bad child would record a dangling edge).
        let child_ok = view_registry::with_view(child, |_v| ()).is_ok();
        match view_registry::with_view(parent, |p| {
            if child_ok {
                // Clamp the insertion index into [0, len]; AOSP allows index -1 (= append).
                let pos = if index < 0 {
                    p.children.len()
                } else {
                    (index as usize).min(p.children.len())
                };
                p.children.insert(pos, child);
            }
        }) {
            Ok(()) => tracing::debug!(
                target: "android.view.ViewGroup",
                parent,
                child,
                index,
                child_ok,
                "ViewGroup.native_addView: recorded parent→child tree edge (non-GTK)"
            ),
            Err(e) => tracing::debug!(
                target: "android.view.ViewGroup",
                parent,
                child,
                error = %e,
                "ViewGroup.native_addView: invalid parent handle (ignored)"
            ),
        }
        Ok(())
    })
    .resolve::<LogErrorAndDefault>()
}

/// `ViewGroup.native_removeView(long parent, long child)` → remove the parent→child tree edge in
/// [`view_registry`] (2026-06-05).
///
/// JNI ABI: an INSTANCE native returning void, so the parameters are
/// `(EnvUnowned, JObject this, jlong parent, jlong child)`. Removes the `child` handle from the
/// `parent` view's `children` list through the bounds+generation-checked [`view_registry`] (a bad
/// parent handle is logged + ignored, never UB). Mirrors [`view_group_native_add_view`]'s edge
/// recording so a view re-parented during `onCreate` (multitouch.test detaches its content view before
/// re-adding it) leaves the recorded tree consistent. Surfaced 2026-06-05 by multitouch.test.
///
/// The body runs inside [`EnvUnowned::with_env`] (`catch_unwind`-wrapped, §2.8); `resolve` returns the
/// `()` default on error/panic.
extern "system" fn view_group_native_remove_view<'local>(
    mut env: EnvUnowned<'local>,
    _this: JObject<'local>,
    parent: jlong,
    child: jlong,
) {
    env.with_env(|_env| -> jni::errors::Result<()> {
        match view_registry::with_view(parent, |p| {
            p.children.retain(|&c| c != child);
        }) {
            Ok(()) => tracing::debug!(
                target: "android.view.ViewGroup",
                parent,
                child,
                "ViewGroup.native_removeView: removed parent→child tree edge (non-GTK)"
            ),
            Err(e) => tracing::debug!(
                target: "android.view.ViewGroup",
                parent,
                child,
                error = %e,
                "ViewGroup.native_removeView: invalid parent handle (ignored)"
            ),
        }
        Ok(())
    })
    .resolve::<LogErrorAndDefault>()
}

/// Bind Eclipse's own (non-GTK) backing for `android.view.ViewGroup`'s tree-wiring natives.
///
/// Registered before step 4, alongside the View/Window natives. Each is implemented against
/// [`view_registry`]; new ViewGroup natives the run surfaces are added here.
///
/// # Safety / soundness
/// `register_native_methods` is `unsafe`: the fn pointer must match the declared JNI signature. It
/// does — [`view_group_native_add_view`] is written to ViewGroup.java line 186's exact descriptor.
/// The body is `catch_unwind`-guarded via [`EnvUnowned::with_env`] (AGENTS.md §2.8).
fn register_view_group_natives(env: &mut Env) -> Result<(), FrameworkError> {
    let class = env.find_class(VIEW_GROUP_CLASS)?;
    let methods = [
        // SAFETY: `view_group_native_add_view` matches the paired
        // `(JJILandroid/view/ViewGroup$LayoutParams;)V` signature as an instance native; casting the
        // `extern "system"` fn to a `*mut c_void` is how `NativeMethod::from_raw_parts` takes it.
        unsafe {
            NativeMethod::from_raw_parts(
                VIEW_GROUP_NATIVE_ADD_VIEW_NAME,
                VIEW_GROUP_NATIVE_ADD_VIEW_SIG,
                view_group_native_add_view as *mut std::ffi::c_void,
            )
        },
        // SAFETY: `view_group_native_remove_view` matches the paired `(JJ)V` signature as an instance
        // native (surfaced by multitouch.test re-parenting its content, run log 2026-06-05).
        unsafe {
            NativeMethod::from_raw_parts(
                VIEW_GROUP_NATIVE_REMOVE_VIEW_NAME,
                VIEW_GROUP_NATIVE_REMOVE_VIEW_SIG,
                view_group_native_remove_view as *mut std::ffi::c_void,
            )
        },
    ];
    // SAFETY: `class` is the loaded android/view/ViewGroup; the fn pointers' signatures match its
    // `native_addView` (ViewGroup.java line 186) and `native_removeView` (ART-reported line 2026-06-05)
    // declarations.
    unsafe { env.register_native_methods(&class, &methods) }?;
    tracing::info!(
        class = "android/view/ViewGroup",
        "registered Eclipse's non-GTK backing for ViewGroup.native_addView + native_removeView"
    );
    Ok(())
}

// === Eclipse's own (non-GTK) backing for android.graphics.Paint native objects =================
//
// 2026-06-05: a `<TextView>` constructs a `TextPaint`/`Paint` during step 5's `setContentView`,
// surfacing `Paint.native_create()` (run log 2026-06-05). `Paint` is on the graphics subsystem; ATL
// backs it in C against GTK/Cairo. Eclipse must NOT pull GTK (AGENTS.md §5 Step 3.5) and does NO
// drawing at onCreate (the ash/Vulkan render is the deferred big build), so a Paint is backed by the
// Eclipse-owned [`paint_registry`] — a generational-slab index (NOT a raw pointer), holding only the
// drawing config (color, text size). A fresh Paint with defaults is a valid Paint; recording its
// config soundly lets the TextView construct without GTK. Each Paint native the run surfaces is added
// here. (Paint's native signatures are taken from the ART `No implementation found` lines.)

/// `android.graphics.Paint` (internal/slashed name for `find_class`) — hosts the Paint natives.
pub const PAINT_CLASS: &JNIStr = jni_str!("android/graphics/Paint");

// JNI name + descriptor for Paint.native_create, from the ART-reported signature `long
// android.graphics.Paint.native_create()` (run log 2026-06-05): a static native, descriptor `()J`.
const PAINT_NATIVE_CREATE_NAME: &JNIStr = jni_str!("native_create");
const PAINT_NATIVE_CREATE_SIG: &JNIStr = jni_str!("()J");

// JNI name + descriptor for Paint.native_set_color, from the ART-reported signature `void
// android.graphics.Paint.native_set_color(long, int)` (run log 2026-06-05, accelerometerdemo's
// ColorDrawable.<init> → Paint.setColor): a static native (the mangled name takes the paint handle
// as its first arg), descriptor `(JI)V`.
const PAINT_NATIVE_SET_COLOR_NAME: &JNIStr = jni_str!("native_set_color");
const PAINT_NATIVE_SET_COLOR_SIG: &JNIStr = jni_str!("(JI)V");

// JNI name + descriptor for Paint.native_set_stroke_width, from the ART-reported signature `void
// android.graphics.Paint.native_set_stroke_width(long, float)` (run log 2026-06-05, multitouch.test's
// custom View `MultiTouch.<init>` → Paint.setStrokeWidth): a static native (the handle is the first
// arg), descriptor `(JF)V`.
const PAINT_NATIVE_SET_STROKE_WIDTH_NAME: &JNIStr = jni_str!("native_set_stroke_width");
const PAINT_NATIVE_SET_STROKE_WIDTH_SIG: &JNIStr = jni_str!("(JF)V");

// JNI name + descriptor for Paint.native_set_style, from the ART-reported signature `void
// android.graphics.Paint.native_set_style(long, int)` (run log 2026-06-05, multitouch.test's custom
// View `MultiTouch.<init>` → Paint.setStyle): a static native, descriptor `(JI)V`. The int is the
// `Paint.Style` ordinal (FILL=0, STROKE=1, FILL_AND_STROKE=2).
const PAINT_NATIVE_SET_STYLE_NAME: &JNIStr = jni_str!("native_set_style");
const PAINT_NATIVE_SET_STYLE_SIG: &JNIStr = jni_str!("(JI)V");

// JNI name + descriptor for Paint.native_set_text_size, from the ART-reported signature `void
// android.graphics.Paint.native_set_text_size(long, float)` (run log 2026-06-05, multitouch.test's
// custom View `MultiTouch.<init>` → Paint.setTextSize): a static native, descriptor `(JF)V`.
const PAINT_NATIVE_SET_TEXT_SIZE_NAME: &JNIStr = jni_str!("native_set_text_size");
const PAINT_NATIVE_SET_TEXT_SIZE_SIG: &JNIStr = jni_str!("(JF)V");

/// `Paint.native_create()` → a real Eclipse-owned [`paint_registry`] handle (2026-06-05).
///
/// JNI ABI: a `static` native returning `jlong` (the mangled name has no receiver-typed overload), so
/// the parameters are `(EnvUnowned, JClass)`. Allocates a [`paint_registry`] slot (default config)
/// and returns its slab handle (≥ 1, never `0`). On a registry error returns `0` (no paint).
///
/// The body runs inside [`EnvUnowned::with_env`] (`catch_unwind`-wrapped, AGENTS.md §2.8;
/// `panic = "abort"` kept); `resolve::<LogErrorAndDefault>` returns the `jlong` default (`0`) on any
/// error/panic — a sound neutral "no paint" handle.
extern "system" fn paint_native_create<'local>(
    mut env: EnvUnowned<'local>,
    _class: JClass<'local>,
) -> jlong {
    env.with_env(|_env| -> jni::errors::Result<jlong> {
        match paint_registry::allocate() {
            Ok(handle) => {
                tracing::debug!(
                    target: "android.graphics.Paint",
                    handle,
                    "Paint.native_create: allocated non-GTK paint-registry handle"
                );
                Ok(handle)
            }
            Err(e) => {
                tracing::warn!(
                    target: "android.graphics.Paint",
                    error = %e,
                    "Paint.native_create: paint-registry allocate failed → 0 (no paint)"
                );
                Ok(0)
            }
        }
    })
    .resolve::<LogErrorAndDefault>()
}

/// `Paint.native_set_color(long native_paint, int color)` → record the ARGB color on the paint
/// (2026-06-05).
///
/// JNI ABI: a `static` native returning void (the mangled name has no receiver-typed overload), so
/// the parameters are `(EnvUnowned, JClass, jlong native_paint, jint color)`. Writes `color` into the
/// paint's [`paint_registry`] slot (the same `color` field `PaintState` already holds). A bad/stale
/// handle is logged and ignored (the registry rejects it — never UB or panic).
///
/// The body runs inside [`EnvUnowned::with_env`] (`catch_unwind`-wrapped, §2.8); `resolve` returns the
/// `()` default on error/panic — the correct neutral value for this `void` native.
extern "system" fn paint_native_set_color<'local>(
    mut env: EnvUnowned<'local>,
    _class: JClass<'local>,
    native_paint: jlong,
    color: jint,
) {
    env.with_env(|_env| -> jni::errors::Result<()> {
        if let Err(e) = paint_registry::with_paint(native_paint, |p| p.color = color) {
            tracing::debug!(
                target: "android.graphics.Paint",
                native_paint,
                error = %e,
                "Paint.native_set_color: invalid paint handle (ignored)"
            );
        }
        Ok(())
    })
    .resolve::<LogErrorAndDefault>()
}

/// `Paint.native_set_stroke_width(long native_paint, float width)` → record the stroke width on the
/// paint (2026-06-05).
///
/// JNI ABI: a `static` native returning void, so the parameters are `(EnvUnowned, JClass, jlong
/// native_paint, float width)`. Writes `width` into the paint's [`paint_registry`] slot; the Canvas
/// stroke draws (`drawCircle`/`drawPath` with a STROKE style) read it for the tiny-skia `Stroke::width`.
/// A bad/stale handle is logged and ignored (the registry rejects it — never UB or panic). Surfaced
/// 2026-06-05 by multitouch.test's custom-View Paint setup.
///
/// The body runs inside [`EnvUnowned::with_env`] (`catch_unwind`-wrapped, §2.8); `resolve` returns the
/// `()` default on error/panic.
extern "system" fn paint_native_set_stroke_width<'local>(
    mut env: EnvUnowned<'local>,
    _class: JClass<'local>,
    native_paint: jlong,
    width: f32,
) {
    env.with_env(|_env| -> jni::errors::Result<()> {
        if let Err(e) = paint_registry::with_paint(native_paint, |p| p.stroke_width = width) {
            tracing::debug!(
                target: "android.graphics.Paint",
                native_paint,
                error = %e,
                "Paint.native_set_stroke_width: invalid paint handle (ignored)"
            );
        }
        Ok(())
    })
    .resolve::<LogErrorAndDefault>()
}

/// `Paint.native_set_style(long native_paint, int style)` → record the fill/stroke style on the paint
/// (2026-06-05).
///
/// JNI ABI: a `static` native returning void, so the parameters are `(EnvUnowned, JClass, jlong
/// native_paint, jint style)`. `style` is the AOSP `Paint.Style` ordinal (FILL=0, STROKE=1,
/// FILL_AND_STROKE=2), mapped via [`paint_registry::PaintStyle::from_ordinal`] (unknown → FILL). The
/// Canvas draws read it to choose tiny-skia fill vs stroke. A bad/stale handle is logged + ignored
/// (the registry rejects it — never UB or panic). Surfaced 2026-06-05 by multitouch.test's custom View.
///
/// The body runs inside [`EnvUnowned::with_env`] (`catch_unwind`-wrapped, §2.8); `resolve` returns the
/// `()` default on error/panic.
extern "system" fn paint_native_set_style<'local>(
    mut env: EnvUnowned<'local>,
    _class: JClass<'local>,
    native_paint: jlong,
    style: jint,
) {
    env.with_env(|_env| -> jni::errors::Result<()> {
        let resolved = paint_registry::PaintStyle::from_ordinal(style);
        if let Err(e) = paint_registry::with_paint(native_paint, |p| p.style = resolved) {
            tracing::debug!(
                target: "android.graphics.Paint",
                native_paint,
                style,
                error = %e,
                "Paint.native_set_style: invalid paint handle (ignored)"
            );
        }
        Ok(())
    })
    .resolve::<LogErrorAndDefault>()
}

/// `Paint.native_set_text_size(long native_paint, float size)` → record the text size on the paint
/// (2026-06-05).
///
/// JNI ABI: a `static` native returning void, so the parameters are `(EnvUnowned, JClass, jlong
/// native_paint, float size)`. Writes `size` into the paint's [`paint_registry`] slot (the `text_size`
/// field `PaintState` already holds; `Canvas.drawText` reads it). A bad/stale handle is logged +
/// ignored (the registry rejects it — never UB or panic). Surfaced 2026-06-05 by multitouch.test.
///
/// The body runs inside [`EnvUnowned::with_env`] (`catch_unwind`-wrapped, §2.8); `resolve` returns the
/// `()` default on error/panic.
extern "system" fn paint_native_set_text_size<'local>(
    mut env: EnvUnowned<'local>,
    _class: JClass<'local>,
    native_paint: jlong,
    size: f32,
) {
    env.with_env(|_env| -> jni::errors::Result<()> {
        if let Err(e) = paint_registry::with_paint(native_paint, |p| p.text_size = size) {
            tracing::debug!(
                target: "android.graphics.Paint",
                native_paint,
                error = %e,
                "Paint.native_set_text_size: invalid paint handle (ignored)"
            );
        }
        Ok(())
    })
    .resolve::<LogErrorAndDefault>()
}

/// Bind Eclipse's own (non-GTK) backing for `android.graphics.Paint`'s natives.
///
/// Registered before step 4, alongside the View/Window natives, since the View hierarchy's
/// `TextPaint`/`Paint` construct during step 5. Each native is implemented against [`paint_registry`].
///
/// # Safety / soundness
/// `register_native_methods` is `unsafe`: each fn pointer must match the declared JNI signature. They
/// do — each native is written to the exact descriptor the run reported. Every native body is
/// `catch_unwind`-guarded via [`EnvUnowned::with_env`] (AGENTS.md §2.8).
fn register_paint_natives(env: &mut Env) -> Result<(), FrameworkError> {
    let class = env.find_class(PAINT_CLASS)?;
    let methods = [
        // SAFETY: `paint_native_create` matches the paired `()J` signature as a static native;
        // casting the `extern "system"` fn to a `*mut c_void` is how `NativeMethod::from_raw_parts`
        // takes it.
        unsafe {
            NativeMethod::from_raw_parts(
                PAINT_NATIVE_CREATE_NAME,
                PAINT_NATIVE_CREATE_SIG,
                paint_native_create as *mut std::ffi::c_void,
            )
        },
        // SAFETY: `paint_native_set_color` matches the paired `(JI)V` signature as a static native.
        unsafe {
            NativeMethod::from_raw_parts(
                PAINT_NATIVE_SET_COLOR_NAME,
                PAINT_NATIVE_SET_COLOR_SIG,
                paint_native_set_color as *mut std::ffi::c_void,
            )
        },
        // SAFETY: `paint_native_set_stroke_width` matches the paired `(JF)V` signature as a static
        // native (surfaced by multitouch.test's custom-View Paint setup, run log 2026-06-05).
        unsafe {
            NativeMethod::from_raw_parts(
                PAINT_NATIVE_SET_STROKE_WIDTH_NAME,
                PAINT_NATIVE_SET_STROKE_WIDTH_SIG,
                paint_native_set_stroke_width as *mut std::ffi::c_void,
            )
        },
        // SAFETY: `paint_native_set_style` matches the paired `(JI)V` signature as a static native
        // (surfaced by multitouch.test's custom-View Paint setup, run log 2026-06-05).
        unsafe {
            NativeMethod::from_raw_parts(
                PAINT_NATIVE_SET_STYLE_NAME,
                PAINT_NATIVE_SET_STYLE_SIG,
                paint_native_set_style as *mut std::ffi::c_void,
            )
        },
        // SAFETY: `paint_native_set_text_size` matches the paired `(JF)V` signature as a static native
        // (surfaced by multitouch.test's custom-View Paint setup, run log 2026-06-05).
        unsafe {
            NativeMethod::from_raw_parts(
                PAINT_NATIVE_SET_TEXT_SIZE_NAME,
                PAINT_NATIVE_SET_TEXT_SIZE_SIG,
                paint_native_set_text_size as *mut std::ffi::c_void,
            )
        },
    ];
    // SAFETY: `class` is the loaded android/graphics/Paint; the fn pointers' signatures match its
    // `native_create`/`native_set_color`/`native_set_stroke_width`/`native_set_style`/
    // `native_set_text_size` declarations (from the ART-reported signatures, 2026-06-05).
    unsafe { env.register_native_methods(&class, &methods) }?;
    tracing::info!(
        class = "android/graphics/Paint",
        "registered Eclipse's non-GTK backing for Paint.native_create + native_set_color + native_set_stroke_width + native_set_style + native_set_text_size"
    );
    Ok(())
}

// === Eclipse's own (non-GTK) backing for android.graphics.Matrix native objects =================
//
// 2026-06-05: AppCompat's `VectorDrawableCompat.<init>` constructs an `android.graphics.Matrix`
// during step 5 (`setContentView` → `AppCompatDrawableManager.checkVectorDrawableSetup`), surfacing
// `long android.graphics.Matrix.native_create(long)` (run log 2026-06-05, accelerometerdemo). A
// `Matrix` is AOSP's 3x3 transform — **pure float math, no GPU/raster/GTK needed** — so it is backed
// by the Eclipse-owned [`matrix_registry`] generational slab (a slab index, NOT a raw pointer). The
// math is REAL and exact (3x3 multiply / perspective map), never a sentinel: a Matrix's transform is
// load-bearing for the vector-drawable geometry, so faking it is forbidden (AGENTS.md core principle).
// Each Matrix native is added here as the run surfaces it, with the descriptor taken from the exact
// ART `No implementation found` line + the AOSP `Matrix.java` native declarations.

/// `android.graphics.Matrix` (internal/slashed name for `find_class`) — hosts the Matrix natives.
pub const MATRIX_CLASS: &JNIStr = jni_str!("android/graphics/Matrix");

// JNI name + descriptor for Matrix.native_create, from the ART-reported signature `long
// android.graphics.Matrix.native_create(long)` (run log 2026-06-05): a static native, descriptor
// `(J)J`. The `long` arg is the source Matrix's native handle (`0` = a fresh identity matrix; a
// non-zero handle = copy that matrix), per AOSP `Matrix(Matrix src)` / `Matrix()`.
const MATRIX_NATIVE_CREATE_NAME: &JNIStr = jni_str!("native_create");
const MATRIX_NATIVE_CREATE_SIG: &JNIStr = jni_str!("(J)J");

// JNI name + descriptor for Matrix.finalizer, from the ART-reported signature `void
// android.graphics.Matrix.finalizer(long)` (run log 2026-06-05): a static native, descriptor `(J)V`.
// AOSP's `Matrix` registers `finalizer` as its `sNativeFinalizer` via `sun.misc.Cleaner`/`NativeAllocationRegistry`;
// it frees the native matrix object. Eclipse frees the matrix_registry slot (so the handle becomes
// stale and the slot can be reused) — runs on the GC/finalizer thread.
const MATRIX_FINALIZER_NAME: &JNIStr = jni_str!("finalizer");
const MATRIX_FINALIZER_SIG: &JNIStr = jni_str!("(J)V");

/// `Matrix.native_create(long src)` → a real Eclipse-owned [`matrix_registry`] handle (2026-06-05).
///
/// JNI ABI: a `static` native (`(J)J`), so the parameters are `(EnvUnowned, JClass, jlong src)`.
/// `src == 0` allocates a fresh identity matrix; a non-zero `src` allocates a COPY of that matrix's
/// value (exact, via [`matrix_registry::get`]). Returns the new slab handle (≥ 1, never `0`). On a
/// registry error returns `0` (AOSP treats a `0` native instance as the identity, so this degrades to
/// an identity Matrix rather than UB).
///
/// The body runs inside [`EnvUnowned::with_env`] (`catch_unwind`-wrapped, AGENTS.md §2.8;
/// `panic = "abort"` kept); `resolve::<LogErrorAndDefault>` returns the `jlong` default (`0`) on any
/// error/panic.
extern "system" fn matrix_native_create<'local>(
    mut env: EnvUnowned<'local>,
    _class: JClass<'local>,
    src: jlong,
) -> jlong {
    env.with_env(|_env| -> jni::errors::Result<jlong> {
        // Copy the source matrix's value (identity when src == 0), then allocate a new slab slot
        // holding that value — exact, no aliasing of the source slot.
        let value = match matrix_registry::get(src) {
            Ok(v) => v,
            Err(e) => {
                tracing::warn!(
                    target: "android.graphics.Matrix",
                    src,
                    error = %e,
                    "Matrix.native_create: source handle invalid → identity"
                );
                matrix_registry::Affine::IDENTITY
            }
        };
        match matrix_registry::allocate(value) {
            Ok(handle) => {
                tracing::debug!(
                    target: "android.graphics.Matrix",
                    src,
                    handle,
                    "Matrix.native_create: allocated non-GTK matrix-registry handle"
                );
                Ok(handle)
            }
            Err(e) => {
                tracing::warn!(
                    target: "android.graphics.Matrix",
                    error = %e,
                    "Matrix.native_create: matrix-registry allocate failed → 0 (identity)"
                );
                Ok(0)
            }
        }
    })
    .resolve::<LogErrorAndDefault>()
}

/// `Matrix.finalizer(long native_instance)` → free the Eclipse-owned [`matrix_registry`] slot
/// (2026-06-05).
///
/// JNI ABI: a `static` native returning void (AOSP registers it as the `sNativeFinalizer` run by
/// `NativeAllocationRegistry`/`Cleaner` on the GC/finalizer thread), so the parameters are
/// `(EnvUnowned, JClass, jlong native_instance)`. Frees the matrix slot so its handle becomes stale
/// and the slot can be reused. A `0` handle (the identity sentinel, which has no slot) or a
/// stale/already-freed handle is logged at debug and ignored (the registry rejects it — never UB or
/// double-free; the generational slab makes a freed handle permanently stale).
///
/// The body runs inside [`EnvUnowned::with_env`] (`catch_unwind`-wrapped, §2.8); `resolve` returns the
/// `()` default on error/panic — the correct neutral value for this `void` native.
extern "system" fn matrix_finalizer<'local>(
    mut env: EnvUnowned<'local>,
    _class: JClass<'local>,
    native_instance: jlong,
) {
    env.with_env(|_env| -> jni::errors::Result<()> {
        if let Err(e) = matrix_registry::free(native_instance) {
            tracing::debug!(
                target: "android.graphics.Matrix",
                native_instance,
                error = %e,
                "Matrix.finalizer: handle already freed / identity sentinel (ignored)"
            );
        }
        Ok(())
    })
    .resolve::<LogErrorAndDefault>()
}

/// Bind Eclipse's own (non-GTK) backing for `android.graphics.Matrix`'s natives.
///
/// Registered before step 4, alongside the View/Paint natives, since AppCompat's drawable manager
/// constructs a `Matrix` during step 5. Each native is implemented against [`matrix_registry`] with
/// exact 3x3 affine/perspective math (no GTK, no raster).
///
/// # Safety / soundness
/// `register_native_methods` is `unsafe`: each fn pointer must match the declared JNI signature. They
/// do — each native is written to the exact descriptor the run reported. Every native body is
/// `catch_unwind`-guarded via [`EnvUnowned::with_env`] (AGENTS.md §2.8).
fn register_matrix_natives(env: &mut Env) -> Result<(), FrameworkError> {
    let class = env.find_class(MATRIX_CLASS)?;
    let methods = [
        // SAFETY: `matrix_native_create` matches the paired `(J)J` signature as a static native;
        // casting the `extern "system"` fn to a `*mut c_void` is how `NativeMethod::from_raw_parts`
        // takes it.
        unsafe {
            NativeMethod::from_raw_parts(
                MATRIX_NATIVE_CREATE_NAME,
                MATRIX_NATIVE_CREATE_SIG,
                matrix_native_create as *mut std::ffi::c_void,
            )
        },
        // SAFETY: `matrix_finalizer` matches the paired `(J)V` signature as a static native.
        unsafe {
            NativeMethod::from_raw_parts(
                MATRIX_FINALIZER_NAME,
                MATRIX_FINALIZER_SIG,
                matrix_finalizer as *mut std::ffi::c_void,
            )
        },
    ];
    // SAFETY: `class` is the loaded android/graphics/Matrix; the fn pointers' signatures match its
    // `native_create`/`finalizer` declarations (from the ART-reported signatures, 2026-06-05).
    unsafe { env.register_native_methods(&class, &methods) }?;
    tracing::info!(
        class = "android/graphics/Matrix",
        "registered Eclipse's non-GTK backing for Matrix.native_create + finalizer"
    );
    Ok(())
}

// === Eclipse's own (non-GTK) backing for android.graphics.Path vector geometry ==================
//
// 2026-06-05: `AdaptiveIconDrawable.<init> → PathParser.createPathFromPathData → Path.moveTo` builds
// the adaptive-icon mask, surfacing `long android.graphics.Path.native_create_builder(long, long)`
// (run log 2026-06-05, AdaptiveIconDemo). This ART build routes `Path` construction through a builder:
// `Path.getBuilder()` calls `native_create_builder` once (lazily), then each `moveTo`/`lineTo`/
// `quadTo`/`cubicTo`/`close` is a native op on that builder handle. A `Path` is REAL vector geometry,
// so it is backed by the Eclipse-owned [`path_registry`] generational slab (a slab index, NOT a raw
// pointer). The geometry is recorded faithfully (the actual parsed coordinates) — never a sentinel;
// faking the shape is forbidden (AGENTS.md core principle). Each Path native is added here as the
// discovery loop surfaces it, with the descriptor taken from the exact ART `No implementation found`
// line + the AOSP `Path.java` native declarations.

/// `android.graphics.Path` (internal/slashed name for `find_class`) — hosts the Path natives.
pub const PATH_CLASS: &JNIStr = jni_str!("android/graphics/Path");

// JNI name + descriptor for Path.native_create_builder, from the ART-reported signature `long
// android.graphics.Path.native_create_builder(long, long)` (run log 2026-06-05): a static native,
// descriptor `(JJ)J`. The first `long` is the existing native path object to seed the builder from
// (`0` = empty); the second `long` is a reserve/hint AOSP passes through. Eclipse allocates a fresh
// path_registry geometry slot (seeded from the source path's geometry when non-zero) and returns its
// slab handle; the subsequent moveTo/lineTo/… ops mutate that slot's geometry.
const PATH_NATIVE_CREATE_BUILDER_NAME: &JNIStr = jni_str!("native_create_builder");
const PATH_NATIVE_CREATE_BUILDER_SIG: &JNIStr = jni_str!("(JJ)J");

// JNI names + descriptors for the Path builder mutation ops, from the ART-reported signatures (run
// log 2026-06-05) + AOSP `Path.java`'s native declarations. The first `long` of each is the builder
// handle returned by `native_create_builder`; the trailing `float`s are the contour coordinates.
// `native_move_to(long, float, float)` was confirmed surfaced by the discovery loop; `line_to`/
// `quad_to`/`cubic_to`/`close` follow the same builder-op pattern (each is bound here and confirmed/
// corrected by the loop). They record the REAL parsed geometry on the builder's path_registry slot.
const PATH_NATIVE_MOVE_TO_NAME: &JNIStr = jni_str!("native_move_to");
const PATH_NATIVE_MOVE_TO_SIG: &JNIStr = jni_str!("(JFF)V");
const PATH_NATIVE_LINE_TO_NAME: &JNIStr = jni_str!("native_line_to");
const PATH_NATIVE_LINE_TO_SIG: &JNIStr = jni_str!("(JFF)V");
const PATH_NATIVE_QUAD_TO_NAME: &JNIStr = jni_str!("native_quad_to");
const PATH_NATIVE_QUAD_TO_SIG: &JNIStr = jni_str!("(JFFFF)V");
const PATH_NATIVE_CUBIC_TO_NAME: &JNIStr = jni_str!("native_cubic_to");
const PATH_NATIVE_CUBIC_TO_SIG: &JNIStr = jni_str!("(JFFFFFF)V");
const PATH_NATIVE_CLOSE_NAME: &JNIStr = jni_str!("native_close");
const PATH_NATIVE_CLOSE_SIG: &JNIStr = jni_str!("(J)V");

// JNI name + descriptor for Path.native_create_path, from the ART-reported signature `long
// android.graphics.Path.native_create_path(long)` (run log 2026-06-05): a static native, descriptor
// `(J)J`. AOSP's `Path.getGskPath()`/`Path.<init>` calls it to FOLD the builder back into a finalized
// native path object — the `long` arg is the builder handle, the return is the finalized path's
// handle. Eclipse allocates a new path_registry slot holding a COPY of the builder's real geometry
// (the finalized, immutable path) and returns its slab handle.
const PATH_NATIVE_CREATE_PATH_NAME: &JNIStr = jni_str!("native_create_path");
const PATH_NATIVE_CREATE_PATH_SIG: &JNIStr = jni_str!("(J)J");

// JNI name + descriptor for Path.native_ref_path, from the ART-reported signature `long
// android.graphics.Path.native_ref_path(long)` (run log 2026-06-05): a static native, descriptor
// `(J)J`. In AOSP-GSK's refcounted model `Path.<init>` calls it to take ownership of the GSK path
// into `mNativePath`, returning the native handle. Eclipse's registry is a generational slab (not a
// refcount), so this allocates a new slot holding a COPY of the source geometry — independent
// ownership matching `Path(Path src)` semantics, never a shared-mutation alias across the slab.
const PATH_NATIVE_REF_PATH_NAME: &JNIStr = jni_str!("native_ref_path");
const PATH_NATIVE_REF_PATH_SIG: &JNIStr = jni_str!("(J)J");

/// `Path.native_create_builder(long nativePath, long reserve)` → a real Eclipse-owned
/// [`path_registry`] geometry handle (2026-06-05).
///
/// JNI ABI: a `static` native (`(JJ)J`), so the parameters are
/// `(EnvUnowned, JClass, jlong native_path, jlong reserve)`. `native_path == 0` allocates a fresh
/// empty path; a non-zero `native_path` seeds the builder with a COPY of that path's geometry (exact,
/// via [`path_registry::get`]) so `getBuilder` can continue appending to an existing `Path`. The
/// `reserve` hint is not load-bearing for a `Vec`-backed buffer (it grows on demand) and is logged
/// only. Returns the new slab handle (≥ 1, never `0`). On a registry error returns `0` (AOSP treats a
/// `0` native object as an empty path, so this degrades to an empty path rather than UB).
///
/// The body runs inside [`EnvUnowned::with_env`] (`catch_unwind`-wrapped, AGENTS.md §2.8;
/// `panic = "abort"` kept); `resolve::<LogErrorAndDefault>` returns the `jlong` default (`0`) on any
/// error/panic.
extern "system" fn path_native_create_builder<'local>(
    mut env: EnvUnowned<'local>,
    _class: JClass<'local>,
    native_path: jlong,
    reserve: jlong,
) -> jlong {
    env.with_env(|_env| -> jni::errors::Result<jlong> {
        // Seed the builder from the source path's geometry (empty when native_path == 0) — a COPY, so
        // it never aliases the source slot.
        let geometry = if native_path == 0 {
            path_registry::PathGeometry::default()
        } else {
            match path_registry::get(native_path) {
                Ok(g) => g,
                Err(e) => {
                    tracing::warn!(
                        target: "android.graphics.Path",
                        native_path,
                        error = %e,
                        "Path.native_create_builder: source handle invalid → empty path"
                    );
                    path_registry::PathGeometry::default()
                }
            }
        };
        match path_registry::allocate(geometry) {
            Ok(handle) => {
                tracing::debug!(
                    target: "android.graphics.Path",
                    native_path,
                    reserve,
                    handle,
                    "Path.native_create_builder: allocated non-GTK path-registry geometry handle"
                );
                Ok(handle)
            }
            Err(e) => {
                tracing::warn!(
                    target: "android.graphics.Path",
                    error = %e,
                    "Path.native_create_builder: path-registry allocate failed → 0 (empty path)"
                );
                Ok(0)
            }
        }
    })
    .resolve::<LogErrorAndDefault>()
}

/// Record a geometry op on the builder handle's [`path_registry`] slot. Shared by the move/line/quad/
/// cubic/close natives: it locates the slot (bounds+generation-checked), runs `op` against its real
/// geometry, and logs a debug line. A stale/fabricated handle is logged at warn and ignored (the
/// registry rejects it — never UB). `op_name` names the op for the log only.
fn path_record(
    handle: jlong,
    op_name: &'static str,
    op: impl FnOnce(&mut path_registry::PathGeometry),
) {
    match path_registry::with_path(handle, op) {
        Ok(()) => {
            tracing::trace!(
                target: "android.graphics.Path",
                handle,
                op = op_name,
                "Path builder op recorded on path-registry geometry"
            );
        }
        Err(e) => {
            tracing::warn!(
                target: "android.graphics.Path",
                handle,
                op = op_name,
                error = %e,
                "Path builder op: builder handle invalid (ignored)"
            );
        }
    }
}

/// `Path.native_move_to(long builder, float x, float y)` → record a `moveTo` on the builder's geometry.
///
/// JNI ABI: a `static` native returning void (`(JFF)V`), so the parameters are
/// `(EnvUnowned, JClass, jlong builder, jfloat x, jfloat y)`. Records the REAL coordinates on the
/// builder's [`path_registry`] slot. `catch_unwind`-guarded via `with_env`; `resolve` returns `()`.
extern "system" fn path_native_move_to<'local>(
    mut env: EnvUnowned<'local>,
    _class: JClass<'local>,
    builder: jlong,
    x: jfloat,
    y: jfloat,
) {
    env.with_env(|_env| -> jni::errors::Result<()> {
        path_record(builder, "moveTo", |g| g.move_to(x, y));
        Ok(())
    })
    .resolve::<LogErrorAndDefault>()
}

/// `Path.native_line_to(long builder, float x, float y)` → record a `lineTo` on the builder's geometry.
///
/// JNI ABI: a `static` native returning void (`(JFF)V`). See [`path_native_move_to`] for the contract.
extern "system" fn path_native_line_to<'local>(
    mut env: EnvUnowned<'local>,
    _class: JClass<'local>,
    builder: jlong,
    x: jfloat,
    y: jfloat,
) {
    env.with_env(|_env| -> jni::errors::Result<()> {
        path_record(builder, "lineTo", |g| g.line_to(x, y));
        Ok(())
    })
    .resolve::<LogErrorAndDefault>()
}

/// `Path.native_quad_to(long builder, float cx, float cy, float x, float y)` → record a quadratic
/// Bézier on the builder's geometry.
///
/// JNI ABI: a `static` native returning void (`(JFFFF)V`). See [`path_native_move_to`] for the contract.
extern "system" fn path_native_quad_to<'local>(
    mut env: EnvUnowned<'local>,
    _class: JClass<'local>,
    builder: jlong,
    cx: jfloat,
    cy: jfloat,
    x: jfloat,
    y: jfloat,
) {
    env.with_env(|_env| -> jni::errors::Result<()> {
        path_record(builder, "quadTo", |g| g.quad_to(cx, cy, x, y));
        Ok(())
    })
    .resolve::<LogErrorAndDefault>()
}

/// `Path.native_cubic_to(long builder, float c1x, float c1y, float c2x, float c2y, float x, float y)`
/// → record a cubic Bézier on the builder's geometry.
///
/// JNI ABI: a `static` native returning void (`(JFFFFFF)V`). See [`path_native_move_to`] for the
/// contract.
extern "system" fn path_native_cubic_to<'local>(
    mut env: EnvUnowned<'local>,
    _class: JClass<'local>,
    builder: jlong,
    c1x: jfloat,
    c1y: jfloat,
    c2x: jfloat,
    c2y: jfloat,
    x: jfloat,
    y: jfloat,
) {
    env.with_env(|_env| -> jni::errors::Result<()> {
        path_record(builder, "cubicTo", |g| g.cubic_to(c1x, c1y, c2x, c2y, x, y));
        Ok(())
    })
    .resolve::<LogErrorAndDefault>()
}

/// `Path.native_close(long builder)` → record a `close` on the builder's geometry.
///
/// JNI ABI: a `static` native returning void (`(J)V`). See [`path_native_move_to`] for the contract.
extern "system" fn path_native_close<'local>(
    mut env: EnvUnowned<'local>,
    _class: JClass<'local>,
    builder: jlong,
) {
    env.with_env(|_env| -> jni::errors::Result<()> {
        path_record(builder, "close", |g| g.close());
        Ok(())
    })
    .resolve::<LogErrorAndDefault>()
}

/// Allocate a new [`path_registry`] slot holding a COPY of `source`'s geometry, returning its slab
/// handle. Shared by `native_create_path` (fold builder → finalized path) and `native_ref_path` (take
/// independent ownership into a `Path`): both produce a new, independently-owned native path object
/// from a source handle in Eclipse's generational-slab model. A `0`/stale `source` yields an empty
/// path (logged); a registry-allocate error yields `0` (AOSP treats `0` as an empty native path →
/// degrades, never UB). `op_name` names the op for the log only.
fn path_clone_handle(source: jlong, op_name: &'static str) -> jlong {
    let geometry = if source == 0 {
        path_registry::PathGeometry::default()
    } else {
        match path_registry::get(source) {
            Ok(g) => g,
            Err(e) => {
                tracing::warn!(
                    target: "android.graphics.Path",
                    source,
                    op = op_name,
                    error = %e,
                    "Path clone: source handle invalid → empty path"
                );
                path_registry::PathGeometry::default()
            }
        }
    };
    match path_registry::allocate(geometry) {
        Ok(handle) => {
            tracing::debug!(
                target: "android.graphics.Path",
                source,
                handle,
                op = op_name,
                "Path clone: allocated independently-owned path-registry geometry"
            );
            handle
        }
        Err(e) => {
            tracing::warn!(
                target: "android.graphics.Path",
                op = op_name,
                error = %e,
                "Path clone: path-registry allocate failed → 0 (empty path)"
            );
            0
        }
    }
}

/// `Path.native_create_path(long builder)` → fold the builder into a finalized native path
/// (2026-06-05).
///
/// JNI ABI: a `static` native (`(J)J`), so the parameters are `(EnvUnowned, JClass, jlong builder)`.
/// Allocates a new [`path_registry`] slot holding a COPY of the builder's real geometry (the finalized
/// path) and returns its slab handle (via [`path_clone_handle`]).
///
/// `catch_unwind`-guarded via `with_env`; `resolve::<LogErrorAndDefault>` returns the `jlong` default
/// (`0`) on error/panic.
extern "system" fn path_native_create_path<'local>(
    mut env: EnvUnowned<'local>,
    _class: JClass<'local>,
    builder: jlong,
) -> jlong {
    env.with_env(|_env| -> jni::errors::Result<jlong> {
        Ok(path_clone_handle(builder, "native_create_path"))
    })
    .resolve::<LogErrorAndDefault>()
}

/// `Path.native_ref_path(long src)` → take independent ownership of the source path's geometry into a
/// `Path`'s `mNativePath` (2026-06-05).
///
/// JNI ABI: a `static` native (`(J)J`), so the parameters are `(EnvUnowned, JClass, jlong src)`.
/// Allocates a new [`path_registry`] slot holding a COPY of `src`'s geometry (via
/// [`path_clone_handle`]) — Eclipse's slab models AOSP-GSK's ref by independent ownership.
///
/// `catch_unwind`-guarded via `with_env`; `resolve::<LogErrorAndDefault>` returns the `jlong` default
/// (`0`) on error/panic.
extern "system" fn path_native_ref_path<'local>(
    mut env: EnvUnowned<'local>,
    _class: JClass<'local>,
    src: jlong,
) -> jlong {
    env.with_env(|_env| -> jni::errors::Result<jlong> {
        Ok(path_clone_handle(src, "native_ref_path"))
    })
    .resolve::<LogErrorAndDefault>()
}

/// Bind Eclipse's own (non-GTK) backing for `android.graphics.Path`'s natives.
///
/// Registered before step 4, alongside the View/Paint/Matrix natives, since a launcher's onCreate may
/// build a vector-drawable path during step 5 (AdaptiveIconDemo's `getDrawable` →
/// `AdaptiveIconDrawable.<init>` → `PathParser`). Each native is implemented against [`path_registry`]
/// recording the REAL parsed geometry (no GTK, no Skia-C).
///
/// # Safety / soundness
/// `register_native_methods` is `unsafe`: each fn pointer must match the declared JNI signature. They
/// do — each native is written to the exact descriptor the run reported. Every native body is
/// `catch_unwind`-guarded via [`EnvUnowned::with_env`] (AGENTS.md §2.8).
fn register_path_natives(env: &mut Env) -> Result<(), FrameworkError> {
    let class = env.find_class(PATH_CLASS)?;
    let methods = [
        // SAFETY: `path_native_create_builder` matches the paired `(JJ)J` signature as a static
        // native; casting the `extern "system"` fn to a `*mut c_void` is how
        // `NativeMethod::from_raw_parts` takes it.
        unsafe {
            NativeMethod::from_raw_parts(
                PATH_NATIVE_CREATE_BUILDER_NAME,
                PATH_NATIVE_CREATE_BUILDER_SIG,
                path_native_create_builder as *mut std::ffi::c_void,
            )
        },
        // SAFETY: `path_native_move_to` matches the paired `(JFF)V` signature as a static native.
        unsafe {
            NativeMethod::from_raw_parts(
                PATH_NATIVE_MOVE_TO_NAME,
                PATH_NATIVE_MOVE_TO_SIG,
                path_native_move_to as *mut std::ffi::c_void,
            )
        },
        // SAFETY: `path_native_line_to` matches the paired `(JFF)V` signature as a static native.
        unsafe {
            NativeMethod::from_raw_parts(
                PATH_NATIVE_LINE_TO_NAME,
                PATH_NATIVE_LINE_TO_SIG,
                path_native_line_to as *mut std::ffi::c_void,
            )
        },
        // SAFETY: `path_native_quad_to` matches the paired `(JFFFF)V` signature as a static native.
        unsafe {
            NativeMethod::from_raw_parts(
                PATH_NATIVE_QUAD_TO_NAME,
                PATH_NATIVE_QUAD_TO_SIG,
                path_native_quad_to as *mut std::ffi::c_void,
            )
        },
        // SAFETY: `path_native_cubic_to` matches the paired `(JFFFFFF)V` signature as a static native.
        unsafe {
            NativeMethod::from_raw_parts(
                PATH_NATIVE_CUBIC_TO_NAME,
                PATH_NATIVE_CUBIC_TO_SIG,
                path_native_cubic_to as *mut std::ffi::c_void,
            )
        },
        // SAFETY: `path_native_close` matches the paired `(J)V` signature as a static native.
        unsafe {
            NativeMethod::from_raw_parts(
                PATH_NATIVE_CLOSE_NAME,
                PATH_NATIVE_CLOSE_SIG,
                path_native_close as *mut std::ffi::c_void,
            )
        },
        // SAFETY: `path_native_create_path` matches the paired `(J)J` signature as a static native.
        unsafe {
            NativeMethod::from_raw_parts(
                PATH_NATIVE_CREATE_PATH_NAME,
                PATH_NATIVE_CREATE_PATH_SIG,
                path_native_create_path as *mut std::ffi::c_void,
            )
        },
        // SAFETY: `path_native_ref_path` matches the paired `(J)J` signature as a static native.
        unsafe {
            NativeMethod::from_raw_parts(
                PATH_NATIVE_REF_PATH_NAME,
                PATH_NATIVE_REF_PATH_SIG,
                path_native_ref_path as *mut std::ffi::c_void,
            )
        },
    ];
    // SAFETY: `class` is the loaded android/graphics/Path; the fn pointers' signatures match its
    // `native_create_builder`/`native_move_to`/… declarations (from the ART-reported signatures,
    // 2026-06-05).
    unsafe { env.register_native_methods(&class, &methods) }?;
    tracing::info!(
        class = "android/graphics/Path",
        "registered Eclipse's non-GTK backing for Path.native_create_builder + move/line/quad/cubic/close"
    );
    Ok(())
}

// === Eclipse's own (non-GTK) backing for android.graphics.Canvas draw natives =================
//
// 2026-06-05: a CUSTOM View's `onDraw(Canvas)` issues Canvas draw calls (e.g. multitouch.test's
// `MultiTouch.onDraw` draws touch circles). The draw-cascade driver ([`drive_view_draw`]) constructs
// a Java `Canvas` whose native backing is an Eclipse-owned [`canvas_registry`] slab handle (a tiny-skia
// `Pixmap`, NOT a GTK/Cairo/Skia-C context), then invokes `View.draw(Canvas)`; if Canvas's draw ops
// resolve to natives Eclipse can bind, they issue REAL tiny-skia fills/strokes into that Pixmap. The
// renderer then uploads the Pixmap as an RGBA GPU texture over the view's rect (`CanvasCompositor`).
//
// ⚠️ DEV-HOST DISCOVERY (run log 2026-06-05, multitouch.test, `/tmp/eclipse-draw.log`): THIS ART/ATL
// build's `android.graphics.Canvas` is NOT the modern-AOSP `nDraw*`-native shape. Its vtable dump shows
// the draw ops are **public Java methods** (`drawColor(int)`, `drawCircle(float,float,float,Paint)`,
// `drawRect(...)`, `drawPath(Path,Paint)`, …) backed by an `android.atl.GskCanvas gsk_canvas` field
// (GTK GSK render node) + a `Bitmap bitmap` field — there is NO `nDrawColor`/`nDrawRect`/… native and
// NO `Canvas(long)` constructor (only `Canvas()` and `Canvas(Bitmap)`). So binding `nDraw*` natives
// here throws `NoSuchMethodError`. RegisterNatives is therefore BEST-EFFORT (see
// [`register_canvas_natives`]): when the methods aren't natives on this build, registration is logged
// + skipped and the lifecycle still reaches RESUMED (the draw cascade then composites nothing — the
// view quads + text still draw). The durable faithful path on this build is a `Canvas(Bitmap)` whose
// Bitmap Eclipse owns (so the Java draw methods raster into Eclipse-readable pixels via the Bitmap/
// GskCanvas natives) — a separate Bitmap/GskCanvas subsystem build (deferred; AGENTS.md §5). The
// `canvas_registry` Pixmap raster + the RGBA composite are real + unit-tested and are reused unchanged
// once that consumer exists. The `nDraw*` names below are kept as the attempted binding (they are the
// modern-AOSP set); they are the right names on an AOSP-shaped Canvas build and harmlessly skipped on
// this GTK-backed one.

/// `android.graphics.Canvas` (internal/slashed name for `find_class`) — the class the draw-cascade
/// driver constructs + (best-effort) binds draw natives on. NOTE (2026-06-05): on this ATL build Canvas
/// is GskCanvas-backed with public-Java draw methods + only `Canvas()`/`Canvas(Bitmap)` ctors (no
/// `Canvas(long)`), so the binding + `Canvas(long)` construction are best-effort (see the section note).
pub const CANVAS_CLASS: &JNIStr = jni_str!("android/graphics/Canvas");

// JNI names + descriptors for the modern-AOSP `BaseCanvas` draw natives (bound static with the canvas
// handle as the first arg). Best-effort: skipped if absent on a GTK-backed Canvas build (see the
// section note). Each is paired with its `extern "system"` fn below; pinned by `canvas_native_names_and_sigs`.
const CANVAS_N_DRAW_COLOR_NAME: &JNIStr = jni_str!("nDrawColor");
const CANVAS_N_DRAW_COLOR_SIG: &JNIStr = jni_str!("(JI)V");
const CANVAS_N_DRAW_RECT_NAME: &JNIStr = jni_str!("nDrawRect");
const CANVAS_N_DRAW_RECT_SIG: &JNIStr = jni_str!("(JFFFFJ)V");
const CANVAS_N_DRAW_CIRCLE_NAME: &JNIStr = jni_str!("nDrawCircle");
const CANVAS_N_DRAW_CIRCLE_SIG: &JNIStr = jni_str!("(JFFFJ)V");
const CANVAS_N_DRAW_PATH_NAME: &JNIStr = jni_str!("nDrawPath");
const CANVAS_N_DRAW_PATH_SIG: &JNIStr = jni_str!("(JJJ)V");

/// Snapshot a [`paint_registry`] handle into a [`canvas_registry::PaintConfig`] for a Canvas draw.
///
/// 2026-06-05: reads the `Paint`'s recorded color/style/stroke-width under the paint lock and returns a
/// plain value, so the canvas lock and the paint lock are never held at once (no lock-order hazard).
/// A bad/stale/`0` paint handle (e.g. a draw with a default Paint Eclipse never saw construct) yields
/// [`canvas_registry::PaintConfig::default`] (opaque black, fill) — AOSP's default Paint, so the draw
/// is still real, never UB.
fn paint_config_from_handle(paint: jlong) -> canvas_registry::PaintConfig {
    paint_registry::with_paint(paint, |p| canvas_registry::PaintConfig {
        argb: p.color,
        style: p.style,
        stroke_width: p.stroke_width,
        // AOSP `Path.getFillType` defaults to WINDING; even-odd is per-path, not per-paint, so the
        // path's own geometry/fill carries it. Canvas circle/rect ignore it; drawPath reads the
        // geometry's recorded rule via the path handle below.
        even_odd: false,
    })
    .unwrap_or_default()
}

/// `Canvas.nDrawColor(long canvas, int color)` → fill the whole Pixmap with a solid ARGB color.
///
/// JNI ABI: a `static` native returning void (`(JI)V`), so the parameters are
/// `(EnvUnowned, JClass, jlong canvas, jint color)`. Issues a real [`canvas_registry`] `draw_color`
/// (tiny-skia `Pixmap::fill`). A bad/stale canvas handle is logged + ignored (never UB). This is the
/// op a custom View's `onDraw` typically issues first to clear its canvas.
///
/// The body runs inside [`EnvUnowned::with_env`] (`catch_unwind`-wrapped, AGENTS.md §2.8); `resolve`
/// returns the `()` default on error/panic.
extern "system" fn canvas_n_draw_color<'local>(
    mut env: EnvUnowned<'local>,
    _class: JClass<'local>,
    canvas: jlong,
    color: jint,
) {
    env.with_env(|_env| -> jni::errors::Result<()> {
        match canvas_registry::with_canvas(canvas, |c| c.draw_color(color)) {
            Ok(()) => tracing::trace!(
                target: "android.graphics.Canvas",
                canvas, color,
                "Canvas.nDrawColor: filled the Pixmap (real tiny-skia)"
            ),
            Err(e) => tracing::debug!(
                target: "android.graphics.Canvas",
                canvas, error = %e,
                "Canvas.nDrawColor: invalid canvas handle (ignored)"
            ),
        }
        Ok(())
    })
    .resolve::<LogErrorAndDefault>()
}

/// `Canvas.nDrawRect(long canvas, float left, float top, float right, float bottom, long paint)` →
/// fill/stroke an axis-aligned rectangle into the Pixmap.
///
/// JNI ABI: a `static` native returning void (`(JFFFFJ)V`). Reads the Paint config from the `paint`
/// handle ([`paint_config_from_handle`]) and issues a real [`canvas_registry`] `draw_rect`. Bad canvas
/// handle → logged + ignored.
extern "system" fn canvas_n_draw_rect<'local>(
    mut env: EnvUnowned<'local>,
    _class: JClass<'local>,
    canvas: jlong,
    left: jfloat,
    top: jfloat,
    right: jfloat,
    bottom: jfloat,
    paint: jlong,
) {
    env.with_env(|_env| -> jni::errors::Result<()> {
        let cfg = paint_config_from_handle(paint);
        match canvas_registry::with_canvas(canvas, |c| c.draw_rect(left, top, right, bottom, &cfg))
        {
            Ok(()) => tracing::trace!(
                target: "android.graphics.Canvas",
                canvas, left, top, right, bottom,
                "Canvas.nDrawRect: rasterized a rect (real tiny-skia)"
            ),
            Err(e) => tracing::debug!(
                target: "android.graphics.Canvas",
                canvas, error = %e,
                "Canvas.nDrawRect: invalid canvas handle (ignored)"
            ),
        }
        Ok(())
    })
    .resolve::<LogErrorAndDefault>()
}

/// `Canvas.nDrawCircle(long canvas, float cx, float cy, float radius, long paint)` → fill/stroke a
/// circle into the Pixmap (the op multitouch.test's `onDraw` issues per touch point).
///
/// JNI ABI: a `static` native returning void (`(JFFFJ)V`). Reads the Paint config from `paint` and
/// issues a real [`canvas_registry`] `draw_circle`. Bad canvas handle → logged + ignored.
extern "system" fn canvas_n_draw_circle<'local>(
    mut env: EnvUnowned<'local>,
    _class: JClass<'local>,
    canvas: jlong,
    cx: jfloat,
    cy: jfloat,
    radius: jfloat,
    paint: jlong,
) {
    env.with_env(|_env| -> jni::errors::Result<()> {
        let cfg = paint_config_from_handle(paint);
        match canvas_registry::with_canvas(canvas, |c| c.draw_circle(cx, cy, radius, &cfg)) {
            Ok(()) => tracing::trace!(
                target: "android.graphics.Canvas",
                canvas, cx, cy, radius,
                "Canvas.nDrawCircle: rasterized a circle (real tiny-skia)"
            ),
            Err(e) => tracing::debug!(
                target: "android.graphics.Canvas",
                canvas, error = %e,
                "Canvas.nDrawCircle: invalid canvas handle (ignored)"
            ),
        }
        Ok(())
    })
    .resolve::<LogErrorAndDefault>()
}

/// `Canvas.nDrawPath(long canvas, long path, long paint)` → fill/stroke an arbitrary contour into the
/// Pixmap from a [`path_registry`] geometry.
///
/// JNI ABI: a `static` native returning void (`(JJJ)V`). Snapshots the [`path_registry`] geometry +
/// its fill rule and the [`paint_registry`] config, then issues a real [`canvas_registry`] `draw_path`.
/// A bad canvas/path handle is logged + ignored (never UB). The geometry is COPIED out under the path
/// lock so the canvas lock and path lock are never held at once.
extern "system" fn canvas_n_draw_path<'local>(
    mut env: EnvUnowned<'local>,
    _class: JClass<'local>,
    canvas: jlong,
    path: jlong,
    paint: jlong,
) {
    env.with_env(|_env| -> jni::errors::Result<()> {
        // Copy the path geometry + fill rule out under the path lock (clone into an owned value).
        let geometry = path_registry::with_path(path, |g| g.clone());
        let Ok(geometry) = geometry else {
            tracing::debug!(
                target: "android.graphics.Canvas",
                canvas, path,
                "Canvas.nDrawPath: invalid path handle (ignored)"
            );
            return Ok(());
        };
        // 2026-06-05: `PathGeometry` records verbs+points only (no fill rule); AOSP `Path`'s default
        // fill type is WINDING, which `PaintConfig::default`/`paint_config_from_handle` already use.
        let cfg = paint_config_from_handle(paint);
        match canvas_registry::with_canvas(canvas, |c| c.draw_path(&geometry, &cfg)) {
            Ok(()) => tracing::trace!(
                target: "android.graphics.Canvas",
                canvas, path,
                "Canvas.nDrawPath: rasterized a path (real tiny-skia)"
            ),
            Err(e) => tracing::debug!(
                target: "android.graphics.Canvas",
                canvas, error = %e,
                "Canvas.nDrawPath: invalid canvas handle (ignored)"
            ),
        }
        Ok(())
    })
    .resolve::<LogErrorAndDefault>()
}

/// Bind Eclipse's own (non-GTK) backing for `android.graphics.Canvas`'s draw natives.
///
/// Registered before step 4 (alongside the other graphics natives) so the natives are resolvable the
/// moment a custom View's `onDraw(Canvas)` issues them during the draw cascade. Each is implemented
/// against [`canvas_registry`] (real tiny-skia raster) + [`paint_registry`]/[`path_registry`]. New
/// Canvas natives a dev-host run surfaces (`nDrawText`/`nDrawBitmap`/…) are added here.
///
/// # Safety / soundness
/// `register_native_methods` is `unsafe`: each fn pointer must match the declared JNI signature. They
/// do, by construction — each native is written to the modern-AOSP `BaseCanvas` descriptor (provenance
/// note above; the discovery loop confirms/corrects names on the dev host). Every native body is
/// `catch_unwind`-guarded via [`EnvUnowned::with_env`] (AGENTS.md §2.8).
fn register_canvas_natives(env: &mut Env) -> Result<(), FrameworkError> {
    let class = env.find_class(CANVAS_CLASS)?;
    let methods = [
        // SAFETY: `canvas_n_draw_color` matches the paired `(JI)V` signature as a static native;
        // casting the `extern "system"` fn to a `*mut c_void` is how `from_raw_parts` takes it.
        unsafe {
            NativeMethod::from_raw_parts(
                CANVAS_N_DRAW_COLOR_NAME,
                CANVAS_N_DRAW_COLOR_SIG,
                canvas_n_draw_color as *mut std::ffi::c_void,
            )
        },
        // SAFETY: `canvas_n_draw_rect` matches the paired `(JFFFFJ)V` signature as a static native.
        unsafe {
            NativeMethod::from_raw_parts(
                CANVAS_N_DRAW_RECT_NAME,
                CANVAS_N_DRAW_RECT_SIG,
                canvas_n_draw_rect as *mut std::ffi::c_void,
            )
        },
        // SAFETY: `canvas_n_draw_circle` matches the paired `(JFFFJ)V` signature as a static native.
        unsafe {
            NativeMethod::from_raw_parts(
                CANVAS_N_DRAW_CIRCLE_NAME,
                CANVAS_N_DRAW_CIRCLE_SIG,
                canvas_n_draw_circle as *mut std::ffi::c_void,
            )
        },
        // SAFETY: `canvas_n_draw_path` matches the paired `(JJJ)V` signature as a static native.
        unsafe {
            NativeMethod::from_raw_parts(
                CANVAS_N_DRAW_PATH_NAME,
                CANVAS_N_DRAW_PATH_SIG,
                canvas_n_draw_path as *mut std::ffi::c_void,
            )
        },
    ];
    // BEST-EFFORT (2026-06-05): on this ATL build Canvas has no `nDraw*` natives (it is GskCanvas-
    // backed with public-Java draw methods — section note), so RegisterNatives throws
    // `NoSuchMethodError`. That must NOT abort the lifecycle (the app still reaches RESUMED + the view
    // quads/text render; the draw cascade just composites nothing). So we attempt the bind and, on
    // failure, clear the pending exception + log it as the discovery signal, returning Ok. On an
    // AOSP-shaped Canvas build (where these natives exist) the bind succeeds and the cascade composites.
    // SAFETY: `class` is the loaded android/graphics/Canvas; the fn pointers' signatures match the
    // modern-AOSP BaseCanvas draw-native descriptors.
    match unsafe { env.register_native_methods(&class, &methods) } {
        Ok(()) => {
            // The nDraw* natives bound → this build's Canvas is the AOSP-shaped one the cascade can
            // drive. Enable it (drive_view_draw will construct Canvas(long) + run View.draw).
            CANVAS_DRAW_SUPPORTED.store(true, std::sync::atomic::Ordering::Release);
            tracing::info!(
                class = "android/graphics/Canvas",
                "registered Eclipse's non-GTK backing for Canvas.nDrawColor + nDrawRect + nDrawCircle + nDrawPath (real tiny-skia raster); draw cascade enabled"
            );
        }
        Err(e) => {
            // Clear the NoSuchMethodError RegisterNatives left pending so it can't poison the next JNI
            // call; log it as the discovery signal (this build backs Canvas via GskCanvas/Bitmap), and
            // DISABLE the cascade so drive_view_draw doesn't re-attempt (+ re-log) the missing
            // Canvas(long) ctor every frame.
            if env.exception_check() {
                env.exception_clear();
            }
            CANVAS_DRAW_SUPPORTED.store(false, std::sync::atomic::Ordering::Release);
            tracing::warn!(
                class = "android/graphics/Canvas",
                error = %e,
                "Canvas draw natives not bindable on this ART build (Canvas is GskCanvas/Bitmap-backed, not nDraw*-native); draw cascade DISABLED — view quads + text still render"
            );
        }
    }
    Ok(())
}

/// `true` if this ART build's `android.graphics.Canvas` supports the draw cascade (set by
/// [`register_canvas_natives`] after probing the `nDraw*` natives). [`drive_view_draw`] short-circuits
/// when this is `false` so the missing `Canvas(long)` ctor is not re-attempted every frame.
pub fn canvas_draw_supported() -> bool {
    CANVAS_DRAW_SUPPORTED.load(std::sync::atomic::Ordering::Acquire)
}

// === Eclipse's own (non-GTK) backing for android.widget.TextView native peer construction =======
//
// 2026-06-05: the launcher layout contains a `<TextView>`, so step 5 (`setContentView` →
// `LayoutInflater`) constructs an `android.widget.TextView`, surfacing
// `TextView.native_constructor(Context, AttributeSet)` (run log 2026-06-05). ART resolves natives
// per declaring class, and `TextView.java` (line 89) re-declares its own
// `protected native long native_constructor(Context, AttributeSet);` (same signature as
// `View.native_constructor`). The backing is class-agnostic (it records the receiver's ACTUAL class
// name into [`view_registry`]), so the SAME [`view_native_constructor`] fn is registered on
// `android/widget/TextView` — recording `android.widget.TextView` in the view tree. TextView-specific
// natives (`native_setText`, …) are added here as the run surfaces them.

/// `android.widget.TextView` (internal/slashed name for `find_class`) — re-declares `native_constructor`.
pub const TEXT_VIEW_CLASS: &JNIStr = jni_str!("android/widget/TextView");

// JNI name + descriptor for TextView.native_setText, exactly as declared in `TextView.java`
// (2026-06-05, line 111): `public native final void native_setText(String text);` → an INSTANCE
// native, descriptor `(Ljava/lang/String;)V`. Surfaced during step 5: both the inflated `<TextView
// android:text="Hello World!">` (TextView.<init> → setText) AND the launcher's own
// `findViewById(...).setText(...)` (MainActivity.onCreate:16) route here. ATL backs it against the
// GtkLabel; Eclipse records the text on the receiver's [`view_registry`] peer (no GTK, no draw).
const TEXT_VIEW_NATIVE_SET_TEXT_NAME: &JNIStr = jni_str!("native_setText");
const TEXT_VIEW_NATIVE_SET_TEXT_SIG: &JNIStr = jni_str!("(Ljava/lang/String;)V");

// JNI name + descriptor for View.widget — the `public long widget` field (`View.java` line 888) that
// holds the view's [`view_registry`] handle. An instance native like `native_setText` (which receives
// only the text, not the handle) reads it off `this` to find the peer to update.
const VIEW_WIDGET_FIELD_NAME: &JNIStr = jni_str!("widget");
const VIEW_WIDGET_FIELD_SIG: &JNIStr = jni_str!("J");

/// `TextView.native_setText(String text)` → record the text on the receiver's [`view_registry`] peer
/// (2026-06-05).
///
/// JNI ABI: an INSTANCE native returning void, so the parameters are
/// `(EnvUnowned, JObject this, JString text)`. The view-registry handle is the receiver's `widget`
/// field (`View.java` `public long widget`); this reads it off `this`, then records `text` on that
/// peer through the bounds+generation-checked [`view_registry`] (a stale/fabricated handle is logged +
/// ignored, never UB). No GTK label, no layout/draw — the text is metadata until the deferred render.
///
/// The body runs inside [`EnvUnowned::with_env`] (`catch_unwind`-wrapped, AGENTS.md §2.8;
/// `panic = "abort"` kept); `resolve::<LogErrorAndDefault>` returns the `()` default on error/panic.
extern "system" fn text_view_native_set_text<'local>(
    mut env: EnvUnowned<'local>,
    this: JObject<'local>,
    text: JString<'local>,
) {
    env.with_env(|env| -> jni::errors::Result<()> {
        let widget = view_widget_handle(env, &this);
        // A null text clears it (AOSP setText(null) → empty); record None vs Some.
        let value = if text.is_null() {
            None
        } else {
            Some(text.try_to_string(env)?)
        };
        match view_registry::with_view(widget, |v| v.text = value.clone()) {
            Ok(()) => tracing::debug!(
                target: "android.widget.TextView",
                widget,
                text = value.as_deref().unwrap_or(""),
                "TextView.native_setText: recorded text on non-GTK view peer"
            ),
            Err(e) => tracing::debug!(
                target: "android.widget.TextView",
                widget,
                error = %e,
                "TextView.native_setText: invalid view handle (ignored)"
            ),
        }
        Ok(())
    })
    .resolve::<LogErrorAndDefault>()
}

/// Read a View's `widget` (`long`) field off `this` — its [`view_registry`] handle. Returns `0` (the
/// reserved null handle, which the registry rejects) on any JNI error, so the caller still no-ops
/// soundly. Off the gameplay hot path (per text/attribute set during inflation).
fn view_widget_handle(env: &mut Env, this: &JObject) -> jlong {
    // SAFETY: "J" paired with JavaType::Long is consistent — FieldSignature::from_raw_parts' invariant;
    // `widget` is `public long` on View, the receiver's runtime supertype, so the read is type-correct.
    let sig = unsafe {
        FieldSignature::from_raw_parts(VIEW_WIDGET_FIELD_SIG, JavaType::Primitive(Primitive::Long))
    };
    env.get_field(this, VIEW_WIDGET_FIELD_NAME, &sig)
        .and_then(|v| v.j())
        .unwrap_or(0)
}

/// Bind Eclipse's own (non-GTK) backing for `android.widget.TextView`'s peer natives.
///
/// `native_constructor` (TextView.java line 89, same `(Landroid/content/Context;Landroid/util/
/// AttributeSet;)J` signature as View's) reuses the class-agnostic [`view_native_constructor`], which
/// records the receiver's actual class (`android.widget.TextView`) in [`view_registry`].
/// `native_setText` (TextView.java line 111) records the text on the receiver's peer. Registered
/// before step 4, alongside the View/Window natives.
///
/// # Safety / soundness
/// `register_native_methods` is `unsafe`: each fn pointer must match the declared JNI signature. They
/// do — each native is written to the exact descriptor declared in `TextView.java`. The bodies are
/// `catch_unwind`-guarded via [`EnvUnowned::with_env`] (AGENTS.md §2.8).
fn register_text_view_natives(env: &mut Env) -> Result<(), FrameworkError> {
    let class = env.find_class(TEXT_VIEW_CLASS)?;
    let methods = [
        // SAFETY: `view_native_constructor` matches the paired
        // `(Landroid/content/Context;Landroid/util/AttributeSet;)J` signature as an instance native
        // (shared with View.native_constructor); casting the `extern "system"` fn to a `*mut c_void`
        // is how `NativeMethod::from_raw_parts` takes it.
        unsafe {
            NativeMethod::from_raw_parts(
                VIEW_NATIVE_CONSTRUCTOR_NAME,
                VIEW_NATIVE_CONSTRUCTOR_SIG,
                view_native_constructor as *mut std::ffi::c_void,
            )
        },
        // SAFETY: `text_view_native_set_text` matches the paired `(Ljava/lang/String;)V` signature as
        // an instance native (TextView.java line 111); the cast is how `NativeMethod::from_raw_parts`
        // takes the fn pointer.
        unsafe {
            NativeMethod::from_raw_parts(
                TEXT_VIEW_NATIVE_SET_TEXT_NAME,
                TEXT_VIEW_NATIVE_SET_TEXT_SIG,
                text_view_native_set_text as *mut std::ffi::c_void,
            )
        },
    ];
    // SAFETY: `class` is the loaded android/widget/TextView; the fn pointers' signatures match its
    // `native_constructor`/`native_setText` declarations (verified against TextView.java lines 89/111,
    // 2026-06-05).
    unsafe { env.register_native_methods(&class, &methods) }?;
    tracing::info!(
        class = "android/widget/TextView",
        "registered Eclipse's non-GTK backing for TextView.native_constructor + native_setText"
    );
    Ok(())
}

// === Eclipse's own (non-GTK) backing for android.widget.ImageView native peer construction ======
//
// 2026-06-05: a launcher layout containing an `<ImageView>` (e.g. AdaptiveIconDemo) makes step 5
// (`setContentView` → `LayoutInflater`) construct an `android.widget.ImageView`, surfacing
// `ImageView.native_constructor(Context, AttributeSet)` (run log 2026-06-05 against AdaptiveIconDemo).
// Exactly like `TextView`, ART resolves natives per declaring class and `ImageView.java` re-declares
// its own `protected native long native_constructor(Context, AttributeSet);` (same signature as
// `View.native_constructor`). The backing is class-agnostic (records the receiver's ACTUAL class name
// into [`view_registry`]), so the SAME [`view_native_constructor`] fn is registered on
// `android/widget/ImageView` — recording `android.widget.ImageView` in the view tree. ImageView's
// image-source natives (`native_setImage*`) are added here if/when a run surfaces them; the demo's
// drawable rendering is the deferred render build (no GTK, no draw here).

/// `android.widget.ImageView` (internal/slashed name for `find_class`) — re-declares `native_constructor`.
pub const IMAGE_VIEW_CLASS: &JNIStr = jni_str!("android/widget/ImageView");

// 2026-06-05: `ImageView.setScaleType` calls `native_setScaleType(long, int)` to record the
// scale type on its native peer; surfaced by multitouch.test's AppCompat `ActionBarView`/`HomeView`
// `ImageView` (run log `No implementation found for void android.widget.ImageView.native_setScaleType(
// long, int)`). The scale type is a draw-time hint for how an ImageView fits its image to its bounds;
// no ImageView image-source native is bound yet (the layered-drawable bitmap path is deferred), so
// this validates the `widget` handle + no-ops. Instance native, descriptor `(JI)V`.
const IMAGE_VIEW_SET_SCALE_TYPE_NAME: &JNIStr = jni_str!("native_setScaleType");
const IMAGE_VIEW_SET_SCALE_TYPE_SIG: &JNIStr = jni_str!("(JI)V");

// 2026-06-05: `ImageView.setImageDrawable` calls `native_setDrawable(long widget, long drawable)` to
// attach the image drawable to its native peer; surfaced by multitouch.test's AppCompat ActionBar
// `HomeView` ImageView (run log `No implementation found for void android.widget.ImageView.
// native_setDrawable(long, long)`). `drawable` is a `Drawable` native handle. ImageView image drawing
// has no draw consumer yet (the layered-drawable bitmap raster is deferred), so this validates the
// `widget` view handle + no-ops. Instance native, descriptor `(JJ)V`.
const IMAGE_VIEW_SET_DRAWABLE_NAME: &JNIStr = jni_str!("native_setDrawable");
const IMAGE_VIEW_SET_DRAWABLE_SIG: &JNIStr = jni_str!("(JJ)V");

/// `ImageView.native_setScaleType(long widget, int scaleType)` → validate the handle; no-op
/// (2026-06-05).
///
/// JNI ABI: an INSTANCE native returning void. `widget` is the ImageView's [`view_registry`] handle;
/// `scaleType` is the `ImageView.ScaleType` ordinal. Scale type only affects how an image is fitted
/// when the ImageView draws it; no ImageView image-source native is bound (the layered-drawable bitmap
/// path is the deferred render build), so there is no draw consumer to record it for. Validates the
/// handle through the bounds+generation-checked [`view_registry`] (a bad handle is logged + ignored,
/// never UB) so a fabricated `widget` can never reach a wild dereference.
///
/// The body runs inside [`EnvUnowned::with_env`] (`catch_unwind`-wrapped, §2.8); `resolve` returns the
/// `()` default on error/panic.
extern "system" fn image_view_set_scale_type<'local>(
    mut env: EnvUnowned<'local>,
    _this: JObject<'local>,
    widget: jlong,
    scale_type: jint,
) {
    env.with_env(|_env| -> jni::errors::Result<()> {
        match view_registry::with_view(widget, |_v| ()) {
            Ok(()) => tracing::trace!(
                target: "android.widget.ImageView",
                widget,
                scale_type,
                "ImageView.native_setScaleType: validated handle (no-op; no image draw consumer yet)"
            ),
            Err(e) => tracing::debug!(
                target: "android.widget.ImageView",
                widget,
                error = %e,
                "ImageView.native_setScaleType: invalid view handle (ignored)"
            ),
        }
        Ok(())
    })
    .resolve::<LogErrorAndDefault>()
}

/// `ImageView.native_setDrawable(long widget, long drawable)` → validate the handle; no-op
/// (2026-06-05).
///
/// JNI ABI: an INSTANCE native returning void. `widget` is the ImageView's [`view_registry`] handle;
/// `drawable` is the image `Drawable`'s native handle. ImageView image drawing has no draw consumer
/// yet (the layered-drawable bitmap raster + composite for an ImageView's image is deferred), so this
/// validates the `widget` handle through the bounds+generation-checked [`view_registry`] (a bad handle
/// is logged + ignored, never UB) and no-ops. When the ImageView image raster lands, the `drawable`
/// handle is recorded on the peer here.
///
/// The body runs inside [`EnvUnowned::with_env`] (`catch_unwind`-wrapped, §2.8); `resolve` returns the
/// `()` default on error/panic.
extern "system" fn image_view_set_drawable<'local>(
    mut env: EnvUnowned<'local>,
    _this: JObject<'local>,
    widget: jlong,
    drawable: jlong,
) {
    env.with_env(|_env| -> jni::errors::Result<()> {
        match view_registry::with_view(widget, |_v| ()) {
            Ok(()) => tracing::trace!(
                target: "android.widget.ImageView",
                widget,
                drawable,
                "ImageView.native_setDrawable: validated handle (no-op; no image draw consumer yet)"
            ),
            Err(e) => tracing::debug!(
                target: "android.widget.ImageView",
                widget,
                error = %e,
                "ImageView.native_setDrawable: invalid view handle (ignored)"
            ),
        }
        Ok(())
    })
    .resolve::<LogErrorAndDefault>()
}

/// Bind Eclipse's own (non-GTK) backing for `android.widget.ImageView`'s peer natives.
///
/// `native_constructor` (same `(Landroid/content/Context;Landroid/util/AttributeSet;)J` signature as
/// View's/TextView's) reuses the class-agnostic [`view_native_constructor`], which records the
/// receiver's actual class (`android.widget.ImageView`) in [`view_registry`]. Registered before step 4,
/// alongside the View/TextView natives, because ART resolves natives per declaring class.
///
/// # Safety / soundness
/// `register_native_methods` is `unsafe`: the fn pointer must match the declared JNI signature. It does
/// — [`view_native_constructor`] is written to the exact `(Context, AttributeSet)J` descriptor as an
/// instance native. The body is `catch_unwind`-guarded via [`EnvUnowned::with_env`] (AGENTS.md §2.8).
fn register_image_view_natives(env: &mut Env) -> Result<(), FrameworkError> {
    let class = env.find_class(IMAGE_VIEW_CLASS)?;
    let methods = [
        // SAFETY: `view_native_constructor` matches the paired
        // `(Landroid/content/Context;Landroid/util/AttributeSet;)J` signature as an instance native
        // (shared with View/TextView native_constructor); casting the `extern "system"` fn to a
        // `*mut c_void` is how `NativeMethod::from_raw_parts` takes it.
        unsafe {
            NativeMethod::from_raw_parts(
                VIEW_NATIVE_CONSTRUCTOR_NAME,
                VIEW_NATIVE_CONSTRUCTOR_SIG,
                view_native_constructor as *mut std::ffi::c_void,
            )
        },
        // SAFETY: `image_view_set_scale_type` matches the paired `(JI)V` signature as an instance
        // native (surfaced by multitouch.test's ImageView, run log 2026-06-05).
        unsafe {
            NativeMethod::from_raw_parts(
                IMAGE_VIEW_SET_SCALE_TYPE_NAME,
                IMAGE_VIEW_SET_SCALE_TYPE_SIG,
                image_view_set_scale_type as *mut std::ffi::c_void,
            )
        },
        // SAFETY: `image_view_set_drawable` matches the paired `(JJ)V` signature as an instance native
        // (surfaced by multitouch.test's ImageView, run log 2026-06-05).
        unsafe {
            NativeMethod::from_raw_parts(
                IMAGE_VIEW_SET_DRAWABLE_NAME,
                IMAGE_VIEW_SET_DRAWABLE_SIG,
                image_view_set_drawable as *mut std::ffi::c_void,
            )
        },
    ];
    // SAFETY: `class` is the loaded android/widget/ImageView; the fn pointers' signatures match its
    // re-declared `native_constructor` (same as View/TextView), `native_setScaleType`, and
    // `native_setDrawable` (surfaced by the run lines 2026-06-05).
    unsafe { env.register_native_methods(&class, &methods) }?;
    tracing::info!(
        class = "android/widget/ImageView",
        "registered Eclipse's non-GTK backing for ImageView.native_constructor + native_setScaleType + native_setDrawable"
    );
    Ok(())
}

/// `android.widget.ImageButton` (internal/slashed name) — re-resolves `native_constructor` per class.
///
/// 2026-06-05: AppCompat's `Toolbar` builds an `AppCompatImageButton` (extends `ImageButton extends
/// ImageView`) for its navigation button; ART resolved `native_constructor` against the `ImageButton`
/// class (`No implementation found for long android.widget.ImageButton.native_constructor(Context,
/// AttributeSet)`, run log 2026-06-05). Same `(Context, AttributeSet)J` signature as View/ImageView,
/// so it reuses the class-agnostic [`view_native_constructor`] (records `android.widget.ImageButton`).
pub const IMAGE_BUTTON_CLASS: &JNIStr = jni_str!("android/widget/ImageButton");

// 2026-06-05: `View.setOnClickListener` calls `nativeSetOnClickListener(widget)` to mark the view
// clickable on its native peer; ART resolved it against the ImageButton class (`No implementation
// found for void android.widget.ImageButton.nativeSetOnClickListener(long)`, run log 2026-06-05, the
// Toolbar nav button). Instance native, descriptor `(J)V`. The draw-free lifecycle dispatches no
// input, so this validates the handle + no-ops (click dispatch is the deferred input/render build).
const IMAGE_BUTTON_SET_ON_CLICK_LISTENER_NAME: &JNIStr = jni_str!("nativeSetOnClickListener");
const IMAGE_BUTTON_SET_ON_CLICK_LISTENER_SIG: &JNIStr = jni_str!("(J)V");

/// `View.nativeSetOnClickListener(long widget)` → mark the view clickable on its [`view_registry`]
/// peer (2026-06-05).
///
/// JNI ABI: an INSTANCE native returning void. `widget` is the view's [`view_registry`] handle.
/// Android calls this from `View.setOnClickListener` to mark the native peer clickable; Eclipse
/// records `clickable = true` on the peer through the bounds+generation-checked [`view_registry`] (a
/// bad handle is logged + ignored, never UB). The renderer's hit-test then targets this view on a
/// pointer click and dispatches `View.performClick()` to it (the minimal click path, 2026-06-05);
/// full `MotionEvent`/`InputQueue` touch+move+key dispatch is the documented follow-up. Surfaced
/// 2026-06-05 by AppCompat's Toolbar setting its navigation button's click listener.
///
/// The body runs inside [`EnvUnowned::with_env`] (`catch_unwind`-wrapped, §2.8); `resolve` returns the
/// `()` default on error/panic.
extern "system" fn image_button_set_on_click_listener<'local>(
    mut env: EnvUnowned<'local>,
    _this: JObject<'local>,
    widget: jlong,
) {
    env.with_env(|_env| -> jni::errors::Result<()> {
        match view_registry::set_clickable(widget) {
            Ok(()) => tracing::debug!(
                target: "android.widget.ImageButton",
                widget,
                "View.nativeSetOnClickListener: marked view clickable (hit-test will target it)"
            ),
            Err(e) => tracing::debug!(
                target: "android.widget.ImageButton",
                widget,
                error = %e,
                "ImageButton.nativeSetOnClickListener: invalid view handle (ignored)"
            ),
        }
        Ok(())
    })
    .resolve::<LogErrorAndDefault>()
}

/// Bind Eclipse's own (non-GTK) backing for `android.widget.ImageButton`'s peer natives.
///
/// `native_constructor` reuses the class-agnostic [`view_native_constructor`] (records the receiver's
/// actual class in [`view_registry`]); `nativeSetOnClickListener` validates the handle + no-ops.
/// Registered before step 4, alongside the View/ImageView natives, because ART resolves natives per
/// declaring class.
///
/// # Safety / soundness
/// `register_native_methods` is `unsafe`: each fn pointer must match the declared JNI signature. They
/// do — `view_native_constructor` is the exact `(Context, AttributeSet)J` instance native and
/// `image_button_set_on_click_listener` the `(J)V` instance native. Each body is `catch_unwind`-guarded
/// via [`EnvUnowned::with_env`] (AGENTS.md §2.8).
fn register_image_button_natives(env: &mut Env) -> Result<(), FrameworkError> {
    let class = env.find_class(IMAGE_BUTTON_CLASS)?;
    let methods = [
        // SAFETY: `view_native_constructor` matches the paired
        // `(Landroid/content/Context;Landroid/util/AttributeSet;)J` signature as an instance native
        // (shared with View/ImageView native_constructor).
        unsafe {
            NativeMethod::from_raw_parts(
                VIEW_NATIVE_CONSTRUCTOR_NAME,
                VIEW_NATIVE_CONSTRUCTOR_SIG,
                view_native_constructor as *mut std::ffi::c_void,
            )
        },
        // SAFETY: `image_button_set_on_click_listener` matches the paired `(J)V` signature as an
        // instance native.
        unsafe {
            NativeMethod::from_raw_parts(
                IMAGE_BUTTON_SET_ON_CLICK_LISTENER_NAME,
                IMAGE_BUTTON_SET_ON_CLICK_LISTENER_SIG,
                image_button_set_on_click_listener as *mut std::ffi::c_void,
            )
        },
    ];
    // SAFETY: `class` is the loaded android/widget/ImageButton; the fn pointers' signatures match its
    // re-resolved `native_constructor`/`nativeSetOnClickListener` (surfaced by the run lines 2026-06-05).
    unsafe { env.register_native_methods(&class, &methods) }?;
    tracing::info!(
        class = "android/widget/ImageButton",
        "registered Eclipse's non-GTK backing for ImageButton.native_constructor + nativeSetOnClickListener"
    );
    Ok(())
}

// === Eclipse's own (non-GTK) backing for android.graphics.drawable.Drawable.native_constructor ===
//
// 2026-06-05: a launcher that loads a drawable in onCreate (e.g. AdaptiveIconDemo's
// `Context.getDrawable` → `Resources.loadDrawable` → `Drawable.createFromXml` →
// `AdaptiveIconDrawable.<init>` → `Drawable.<init>`) calls `Drawable.native_constructor()`, surfacing
// `No implementation found for long android.graphics.drawable.Drawable.native_constructor()` (run log
// 2026-06-05 against AdaptiveIconDemo). AOSP's `Drawable.java` declares it
//   `private native long native_constructor();`
// — an INSTANCE native (called from `Drawable.<init>` on `this`, no Java args) returning the native
// drawable peer handle (`Drawable.mNativePtr`). `Drawable.<init>` then registers `mNativePtr` for
// native-allocation cleanup, so the handle must be **non-zero**.
//
// Like `MessageQueue.nativeInit`, Eclipse drives the lifecycle WITHOUT a draw pass, so the drawable's
// drawing/bounds natives (`native_draw`/`native_setBounds`/…) are never invoked — none are bound, and
// if one ever were it would raise a clean `UnsatisfiedLinkError` (not UB). The returned handle thus
// has NO dereferencing consumer, so a full generational-slab registry would be dead weight (Simplicity
// First, AGENTS.md §Surgical). The minimal-sound backing returns a stable non-zero sentinel that is
// plainly NOT a pointer, satisfying the non-zero contract without faking any drawing. If a drawable
// draw/bounds native is ever bound (i.e. the deferred render build draws drawables), this must become
// a real registry handle (mirroring `paint_registry`) so the consumer can validate it — flagged here.

/// `android.graphics.drawable.Drawable` (internal/slashed name) — hosts the `native_constructor` peer
/// allocation native.
pub const DRAWABLE_CLASS: &JNIStr = jni_str!("android/graphics/drawable/Drawable");

// JNI name + descriptor for Drawable's native, exactly as declared in AOSP's `Drawable.java`:
// `private native long native_constructor();` → an INSTANCE native, descriptor `()J`. (Confirmed by
// the run's `No implementation found for long android.graphics.drawable.Drawable.native_constructor()`
// line + the `Drawable.<init> → native_constructor` stack.)
const DRAWABLE_NATIVE_CONSTRUCTOR_NAME: &JNIStr = jni_str!("native_constructor");
const DRAWABLE_NATIVE_CONSTRUCTOR_SIG: &JNIStr = jni_str!("()J");

// JNI name + descriptor for Drawable.native_unref, from the ART-reported signature `void
// android.graphics.drawable.Drawable.native_unref(long)` (run log 2026-06-05): a static native,
// descriptor `(J)V`. AOSP registers it as the drawable's native-allocation free callback (run on the
// GC/finalizer thread). The handle is the non-pointer [`DRAWABLE_HANDLE_SENTINEL`] (no registry slot
// backs it), so unref is a sound no-op.
const DRAWABLE_NATIVE_UNREF_NAME: &JNIStr = jni_str!("native_unref");
const DRAWABLE_NATIVE_UNREF_SIG: &JNIStr = jni_str!("(J)V");

/// The non-zero, non-pointer sentinel `Drawable.native_constructor()` returns as `mNativePtr`.
///
/// 2026-06-05: Java only needs `mNativePtr != 0` (for the native-allocation registration); this value
/// is never dereferenced (no drawable draw/bounds native is bound — see the section comment). A small,
/// recognizable, plainly-not-a-pointer constant.
const DRAWABLE_HANDLE_SENTINEL: jlong = 0x4452; // 'DR' — a non-zero, non-pointer marker.

/// `Drawable.native_constructor()` → a stable non-zero peer handle (`mNativePtr`).
///
/// JNI ABI: an INSTANCE native returning `jlong`, so the parameters are `(EnvUnowned, JObject this)`.
/// `this` is not dereferenced. Returns [`DRAWABLE_HANDLE_SENTINEL`] — non-zero so `Drawable.<init>`'s
/// native-allocation registration accepts it; never a pointer (the handle has no dereferencing consumer
/// in Eclipse's draw-free lifecycle — see the section comment).
///
/// The body runs inside [`EnvUnowned::with_env`] (`catch_unwind`-wrapped, AGENTS.md §2.8;
/// `panic = "abort"` kept); `resolve::<LogErrorAndDefault>` returns the `jlong` default (`0`) on any
/// error/panic — but the body is infallible, so the sentinel is always returned.
extern "system" fn drawable_native_constructor<'local>(
    mut env: EnvUnowned<'local>,
    _this: JObject<'local>,
) -> jlong {
    env.with_env(|_env| -> jni::errors::Result<jlong> {
        tracing::debug!(
            target: "android.graphics.drawable.Drawable",
            handle = DRAWABLE_HANDLE_SENTINEL,
            "Drawable.native_constructor: returning non-GTK non-zero drawable sentinel (no draw pass)"
        );
        Ok(DRAWABLE_HANDLE_SENTINEL)
    })
    .resolve::<LogErrorAndDefault>()
}

/// `Drawable.native_unref(long native_ptr)` → free the native drawable peer (2026-06-05).
///
/// JNI ABI: a `static` native returning void (AOSP runs it as the drawable's native-allocation free
/// callback on the GC/finalizer thread), so the parameters are `(EnvUnowned, JClass, jlong native_ptr)`.
/// `native_ptr` is the non-pointer [`DRAWABLE_HANDLE_SENTINEL`] (no registry slot backs it — see the
/// section comment), so unref is a sound no-op. It is NOT dereferenced.
///
/// The body runs inside [`EnvUnowned::with_env`] (`catch_unwind`-wrapped, §2.8); `resolve` returns the
/// `()` default on error/panic — the correct neutral value for this `void` native.
extern "system" fn drawable_native_unref<'local>(
    mut env: EnvUnowned<'local>,
    _class: JClass<'local>,
    native_ptr: jlong,
) {
    env.with_env(|_env| -> jni::errors::Result<()> {
        tracing::trace!(
            target: "android.graphics.drawable.Drawable",
            native_ptr,
            "Drawable.native_unref: no-op (sentinel handle, no registry slot)"
        );
        Ok(())
    })
    .resolve::<LogErrorAndDefault>()
}

/// Bind Eclipse's own (non-GTK) backing for `android.graphics.drawable.Drawable`'s `native_constructor`.
///
/// Locates `android/graphics/drawable/Drawable` and registers the native via `RegisterNatives` (which
/// wins over name-based lazy binding — JNI 1.1 spec). MUST run before step 4, since a launcher's
/// onCreate may load a drawable. Registered alongside the View/widget natives.
///
/// # Safety / soundness
/// `register_native_methods` is `unsafe`: the function pointer must match the declared JNI signature.
/// It does — [`drawable_native_constructor`] is written to the exact `()J` descriptor as an instance
/// native (`EnvUnowned, JObject this`). The native body is `catch_unwind`-guarded via
/// [`EnvUnowned::with_env`], so no Rust panic can cross the JNI boundary (AGENTS.md §2.8).
fn register_drawable_natives(env: &mut Env) -> Result<(), FrameworkError> {
    let class = env.find_class(DRAWABLE_CLASS)?;
    let methods = [
        // SAFETY: `drawable_native_constructor` matches the paired `()J` signature as an instance
        // native (see the native's docs); casting the `extern "system"` fn to a `*mut c_void` is how
        // `NativeMethod::from_raw_parts` takes it.
        unsafe {
            NativeMethod::from_raw_parts(
                DRAWABLE_NATIVE_CONSTRUCTOR_NAME,
                DRAWABLE_NATIVE_CONSTRUCTOR_SIG,
                drawable_native_constructor as *mut std::ffi::c_void,
            )
        },
        // SAFETY: `drawable_native_unref` matches the paired `(J)V` signature as a static native.
        unsafe {
            NativeMethod::from_raw_parts(
                DRAWABLE_NATIVE_UNREF_NAME,
                DRAWABLE_NATIVE_UNREF_SIG,
                drawable_native_unref as *mut std::ffi::c_void,
            )
        },
    ];
    // SAFETY: `class` is the loaded android/graphics/drawable/Drawable; the fn pointers' signatures
    // match its `native_constructor`/`native_unref` (AOSP `Drawable.java` + the ART-reported signatures,
    // surfaced by the run lines 2026-06-05).
    unsafe { env.register_native_methods(&class, &methods) }?;
    tracing::info!(
        class = "android/graphics/drawable/Drawable",
        "registered Eclipse's non-GTK backing for Drawable.native_constructor + native_unref"
    );
    Ok(())
}

// === Eclipse's own (non-GTK) backing for android.view.Window native window setup ================
//
// 2026-06-05: step 4 (`Activity.createMainActivity` → `internalCreateActivity` → `Window.<init>` →
// `Window.set_native_window`) wires the launcher's Window onto the native window handle. The Window
// natives the dev-host run surfaces are declared in `View.java`'s sibling `Window.java` (lines
// 184–188, android/view — read for the exact modifiers/signatures, NOT content/res):
//   L188 `private static native void set_jobject(long ptr, Window obj);`            → static (JLandroid/view/Window;)V
//   L185 `private native void set_title(long native_window, String title);`         → instance (JLjava/lang/String;)V
//   L187 `public native void set_layout(long native_window, int width, int height);`→ instance (JII)V
//   L184 `public native void set_widget_as_root(long native_window, long widget);`  → instance (JJ)V
// The `long native_window` is the SAME Eclipse-owned [`window_registry`] handle steps 1–4 received,
// so these dereference it through the bounds+generation-checked registry — NEVER a GtkWidget* cast,
// never UB. ATL's C backing drives GTK; Eclipse records the window metadata (jobject set, title,
// layout, root view handle) with no GTK and no real surface/layout/draw (the deferred big build).

/// `android.view.Window` (internal/slashed name for `find_class`) — hosts the window-setup natives.
pub const WINDOW_CLASS: &JNIStr = jni_str!("android/view/Window");

// JNI names + descriptors for Window's natives, exactly as declared in `Window.java` (2026-06-05).
// `set_jobject` is STATIC (line 188); the others are instance methods on the Java Window.
const WINDOW_SET_JOBJECT_NAME: &JNIStr = jni_str!("set_jobject");
const WINDOW_SET_JOBJECT_SIG: &JNIStr = jni_str!("(JLandroid/view/Window;)V");
const WINDOW_SET_TITLE_NAME: &JNIStr = jni_str!("set_title");
const WINDOW_SET_TITLE_SIG: &JNIStr = jni_str!("(JLjava/lang/String;)V");
const WINDOW_SET_LAYOUT_NAME: &JNIStr = jni_str!("set_layout");
const WINDOW_SET_LAYOUT_SIG: &JNIStr = jni_str!("(JII)V");
const WINDOW_SET_WIDGET_AS_ROOT_NAME: &JNIStr = jni_str!("set_widget_as_root");
const WINDOW_SET_WIDGET_AS_ROOT_SIG: &JNIStr = jni_str!("(JJ)V");
// 2026-06-05: `Window.setBackgroundDrawable` (ATL) calls `remove_gtk_background(native_window)` to
// drop any prior GTK background before applying a new one. It was unreached until themes resolved
// `android:windowBackground` (which makes `setContentView → setBackgroundDrawable` run); the ART
// error line gives the exact signature `void android.view.Window.remove_gtk_background(long)` → `(J)V`
// instance. Eclipse is non-GTK (no GTK background exists), so this is a validate-handle no-op,
// matching the other Window natives.
const WINDOW_REMOVE_GTK_BACKGROUND_NAME: &JNIStr = jni_str!("remove_gtk_background");
const WINDOW_REMOVE_GTK_BACKGROUND_SIG: &JNIStr = jni_str!("(J)V");

/// `Window.set_jobject(long ptr, Window obj)` → record that the Java Window back-reference is set on
/// the [`window_registry`] window.
///
/// JNI ABI: a STATIC native returning void (`Window.java` line 188), so the parameters are
/// `(EnvUnowned, JClass, jlong ptr, JObject window)`. `ptr` is the Eclipse-owned window-registry
/// handle; `window` is the Java `android.view.Window` (not dereferenced — Eclipse records only that
/// it was set; the design's `WindowState.jobject` is the documented slot for the real `GlobalRef` a
/// later input/lifecycle increment will store). Validates the handle through the
/// bounds+generation-checked [`window_registry`] (a bad handle is logged + ignored, never UB).
///
/// The body runs inside [`EnvUnowned::with_env`] (`catch_unwind`-wrapped, AGENTS.md §2.8;
/// `panic = "abort"` kept); `resolve::<LogErrorAndDefault>` returns the `()` default on error/panic.
extern "system" fn window_set_jobject<'local>(
    mut env: EnvUnowned<'local>,
    _class: JClass<'local>,
    ptr: jlong,
    _window: JObject<'local>,
) {
    env.with_env(|_env| -> jni::errors::Result<()> {
        // Record "jobject set" on the window slot. The unit placeholder is the design's documented
        // stand-in for the real GlobalRef (window_registry.rs ViewState.jobject docs).
        match window_registry::with_window(ptr, |w| w.jobject = Some(())) {
            Ok(()) => tracing::debug!(
                target: "android.view.Window",
                ptr,
                "Window.set_jobject: recorded Java Window back-reference on non-GTK window"
            ),
            Err(e) => tracing::debug!(
                target: "android.view.Window",
                ptr,
                error = %e,
                "Window.set_jobject: invalid window handle (ignored)"
            ),
        }
        Ok(())
    })
    .resolve::<LogErrorAndDefault>()
}

/// `Window.set_title(long native_window, String title)` → store the title on the [`window_registry`]
/// window (applied to the real winit window when one is associated).
///
/// JNI ABI: an INSTANCE native returning void (`Window.java` line 185), so the parameters are
/// `(EnvUnowned, JObject this, jlong native_window, JString title)`. Reads the title string and
/// stores it on the window slot (the design's `WindowState.title`); a null title stores empty. The
/// handle is bounds+generation-checked (a bad handle is logged + ignored, never UB).
///
/// The body runs inside [`EnvUnowned::with_env`] (`catch_unwind`-wrapped, §2.8); `resolve` returns
/// the `()` default on error/panic.
extern "system" fn window_set_title<'local>(
    mut env: EnvUnowned<'local>,
    _this: JObject<'local>,
    native_window: jlong,
    title: JString<'local>,
) {
    env.with_env(|env| -> jni::errors::Result<()> {
        let title_str = if title.is_null() {
            String::new()
        } else {
            title.try_to_string(env)?
        };
        match window_registry::with_window(native_window, |w| w.title = title_str.clone()) {
            Ok(()) => tracing::debug!(
                target: "android.view.Window",
                native_window,
                title = %title_str,
                "Window.set_title: stored window title (non-GTK)"
            ),
            Err(e) => tracing::debug!(
                target: "android.view.Window",
                native_window,
                error = %e,
                "Window.set_title: invalid window handle (ignored)"
            ),
        }
        Ok(())
    })
    .resolve::<LogErrorAndDefault>()
}

/// `Window.set_layout(long native_window, int width, int height)` → validate the window handle;
/// no-op (layout deferred, 2026-06-05).
///
/// JNI ABI: an INSTANCE native returning void (`Window.java` line 187). Window layout sizing is
/// applied once a real winit window/surface is associated (deferred); for now this validates the
/// `native_window` handle through the bounds+generation-checked [`window_registry`] (a bad handle is
/// logged + ignored, never UB) and no-ops, letting the Window setup proceed.
///
/// The body runs inside [`EnvUnowned::with_env`] (`catch_unwind`-wrapped, §2.8); `resolve` returns
/// the `()` default on error/panic.
extern "system" fn window_set_layout<'local>(
    mut env: EnvUnowned<'local>,
    _this: JObject<'local>,
    native_window: jlong,
    width: jint,
    height: jint,
) {
    env.with_env(|_env| -> jni::errors::Result<()> {
        if let Err(e) = window_registry::with_window(native_window, |_w| ()) {
            tracing::debug!(
                target: "android.view.Window",
                native_window,
                error = %e,
                "Window.set_layout: invalid window handle (ignored)"
            );
        } else {
            tracing::trace!(
                target: "android.view.Window",
                native_window, width, height,
                "Window.set_layout: validated handle, no-op (layout deferred)"
            );
        }
        Ok(())
    })
    .resolve::<LogErrorAndDefault>()
}

/// `Window.remove_gtk_background(long native_window)` → validate the window handle; no-op (Eclipse is
/// non-GTK, so there is no GTK background to remove, 2026-06-05).
///
/// JNI ABI: an INSTANCE native returning void. ATL's `Window.setBackgroundDrawable` calls this to drop
/// any prior GTK background before applying a new one; on Eclipse the Window is non-GTK (it has no GTK
/// background), so this validates the `native_window` handle through the bounds+generation-checked
/// [`window_registry`] (a bad handle is logged + ignored, never UB) and no-ops. A new
/// `android:windowBackground` is recorded by the renderer's view tree, not a GTK widget. Surfaced once
/// theme resolution let `setContentView → setBackgroundDrawable` run.
///
/// The body runs inside [`EnvUnowned::with_env`] (`catch_unwind`-wrapped, §2.8); `resolve` returns
/// the `()` default on error/panic.
extern "system" fn window_remove_gtk_background<'local>(
    mut env: EnvUnowned<'local>,
    _this: JObject<'local>,
    native_window: jlong,
) {
    env.with_env(|_env| -> jni::errors::Result<()> {
        if let Err(e) = window_registry::with_window(native_window, |_w| ()) {
            tracing::debug!(
                target: "android.view.Window",
                native_window,
                error = %e,
                "Window.remove_gtk_background: invalid window handle (ignored)"
            );
        } else {
            tracing::trace!(
                target: "android.view.Window",
                native_window,
                "Window.remove_gtk_background: validated handle, no-op (non-GTK)"
            );
        }
        Ok(())
    })
    .resolve::<LogErrorAndDefault>()
}

/// `Window.set_widget_as_root(long native_window, long widget)` → record the root view handle on the
/// [`window_registry`] window (the content-view tree root).
///
/// JNI ABI: an INSTANCE native returning void (`Window.java` line 184), so the parameters are
/// `(EnvUnowned, JObject this, jlong native_window, jlong widget)`. `widget` is the
/// [`view_registry`] handle of the View made the window's content root. Validates BOTH handles
/// (window + view, each bounds+generation-checked — a bad handle is logged + ignored, never UB). The
/// root view handle is recorded as the window's sole child edge so the view tree's root is known
/// without GTK; no real attach/layout/draw is performed (deferred surface).
///
/// The body runs inside [`EnvUnowned::with_env`] (`catch_unwind`-wrapped, §2.8); `resolve` returns
/// the `()` default on error/panic.
extern "system" fn window_set_widget_as_root<'local>(
    mut env: EnvUnowned<'local>,
    _this: JObject<'local>,
    native_window: jlong,
    widget: jlong,
) {
    env.with_env(|_env| -> jni::errors::Result<()> {
        // Validate the view handle (the root) before recording it as the window's root edge.
        let view_ok = view_registry::with_view(widget, |_v| ()).is_ok();
        // Publish the content-root handle so the renderer's per-frame snapshot draws this subtree
        // (the single source of truth for "what is on screen"); clear it if the view handle is bad.
        view_registry::set_active_root(if view_ok { widget } else { 0 });
        match window_registry::with_window(native_window, |w| {
            // The window's "children" is its single content root; replace any prior root.
            w.root_view = if view_ok { Some(widget) } else { None };
        }) {
            Ok(()) => tracing::debug!(
                target: "android.view.Window",
                native_window,
                widget,
                view_ok,
                "Window.set_widget_as_root: recorded content-root view handle (non-GTK)"
            ),
            Err(e) => tracing::debug!(
                target: "android.view.Window",
                native_window,
                widget,
                error = %e,
                "Window.set_widget_as_root: invalid window handle (ignored)"
            ),
        }
        Ok(())
    })
    .resolve::<LogErrorAndDefault>()
}

/// Bind Eclipse's own (non-GTK) backing for `android.view.Window`'s window-setup natives.
///
/// Registered before the lifecycle drive, alongside the other framework natives, since step 4
/// (`Activity.createMainActivity`) sets up the Window during the lifecycle. Each is implemented
/// against [`window_registry`] / [`view_registry`].
///
/// # Safety / soundness
/// `register_native_methods` is `unsafe`: each fn pointer must match the declared JNI signature.
/// They do, by construction — each native is written to the exact descriptor declared in
/// `Window.java` (lines 184–188). Every native body is `catch_unwind`-guarded via
/// [`EnvUnowned::with_env`], so no Rust panic crosses the JNI boundary (AGENTS.md §2.8).
fn register_window_natives(env: &mut Env) -> Result<(), FrameworkError> {
    let class = env.find_class(WINDOW_CLASS)?;
    let methods = [
        // SAFETY: `window_set_jobject` matches the paired `(JLandroid/view/Window;)V` signature as a
        // static native; the cast is how `NativeMethod::from_raw_parts` takes the fn pointer.
        unsafe {
            NativeMethod::from_raw_parts(
                WINDOW_SET_JOBJECT_NAME,
                WINDOW_SET_JOBJECT_SIG,
                window_set_jobject as *mut std::ffi::c_void,
            )
        },
        // SAFETY: `window_set_title` matches the paired `(JLjava/lang/String;)V` signature as an
        // instance native.
        unsafe {
            NativeMethod::from_raw_parts(
                WINDOW_SET_TITLE_NAME,
                WINDOW_SET_TITLE_SIG,
                window_set_title as *mut std::ffi::c_void,
            )
        },
        // SAFETY: `window_set_layout` matches the paired `(JII)V` signature as an instance native.
        unsafe {
            NativeMethod::from_raw_parts(
                WINDOW_SET_LAYOUT_NAME,
                WINDOW_SET_LAYOUT_SIG,
                window_set_layout as *mut std::ffi::c_void,
            )
        },
        // SAFETY: `window_set_widget_as_root` matches the paired `(JJ)V` signature as an instance
        // native.
        unsafe {
            NativeMethod::from_raw_parts(
                WINDOW_SET_WIDGET_AS_ROOT_NAME,
                WINDOW_SET_WIDGET_AS_ROOT_SIG,
                window_set_widget_as_root as *mut std::ffi::c_void,
            )
        },
        // SAFETY: `window_remove_gtk_background` matches the paired `(J)V` signature as an instance
        // native.
        unsafe {
            NativeMethod::from_raw_parts(
                WINDOW_REMOVE_GTK_BACKGROUND_NAME,
                WINDOW_REMOVE_GTK_BACKGROUND_SIG,
                window_remove_gtk_background as *mut std::ffi::c_void,
            )
        },
    ];
    // SAFETY: `class` is the loaded android/view/Window; `methods` hold valid fn pointers whose
    // signatures match the class's `native` declarations (verified against Window.java lines 184–188,
    // 2026-06-05; `remove_gtk_background` from the ART No-implementation-found line, 2026-06-05).
    unsafe { env.register_native_methods(&class, &methods) }?;
    tracing::info!(
        class = "android/view/Window",
        "registered Eclipse's non-GTK backing for Window.set_jobject + set_title + set_layout + set_widget_as_root + remove_gtk_background"
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

/// `android.os.Looper` (internal name) — hosts the `static prepareMainLooper()` entry point.
///
/// 2026-06-05: the lifecycle runs on a single JNI-attached main thread that has no Android
/// `Looper` of its own. Android's `Handler.<init>` requires `Looper.myLooper() != null`, and a
/// real Activity (e.g. any `AppCompatActivity`/`FragmentActivity`, which build a `Handler` in a
/// field initializer) is constructed during step 4 — so the main `Looper` must be prepared first,
/// matching ATL's recipe (its boot sequence starts with `prepare_main_looper`). The pure-Java
/// `demo_app` Activity never touched a `Handler`, so this gap only surfaced with the second app
/// (`com.ashwin.example.accelerometerdemo`): step 4 threw
/// `RuntimeException: Can't create handler inside thread that has not called Looper.prepare()`.
pub const LOOPER_CLASS: &JNIStr = jni_str!("android/os/Looper");

/// Step 1: `static Context.createApplication(jlong native_window) -> Application`.
///
/// 2026-06-05: descriptor RE-VERIFIED against the compiled framework. `api-impl.jar` packages a
/// single `classes.dex` (no per-class `.class`), so `javap` cannot read it; the ground truth is
/// the api-impl source the jar is built from: `Context.java` L164
/// `static Application createApplication(long native_window)` → package-private **static**,
/// descriptor `(J)Landroid/app/Application;`. This matches the constant exactly — the earlier
/// `GetStaticMethodID(... createApplication ...) returning NULL` was NOT a signature mismatch but a
/// failed `Context.<clinit>` (an `UnsatisfiedLinkError` from the WolfSSL JCA provider load left the
/// class erroneous → method-ID lookup returns NULL). Fixed in `runtime::boot` (RTLD_GLOBAL); see §6.
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
/// Step 4: `static Activity.createMainActivity(String className, jlong native_window, String uri)
/// -> Activity`. The `className` is the launcher Activity's dotted Java class name (from the
/// manifest's MAIN/LAUNCHER intent-filter); `native_window` is the same Eclipse-owned
/// [`window_registry`] handle step 1 received (step 4's Window natives dereference it); `uri` is the
/// launch URI (`null` for a plain launch). 2026-06-05: driven (the prior "deferred" note is stale).
pub const STEP4_CREATE_MAIN_ACTIVITY: RecipeStep = RecipeStep {
    class: "android/app/Activity",
    method: "createMainActivity",
    descriptor: "(Ljava/lang/String;JLjava/lang/String;)Landroid/app/Activity;",
};
/// Step 5: instance `Activity.onCreate(Bundle) -> void` (on the step-4 object), invoked with a
/// `null` `Bundle` (a fresh launch has no saved instance state). 2026-06-05: driven.
pub const STEP5_ACTIVITY_ON_CREATE: RecipeStep = RecipeStep {
    class: "android/app/Activity",
    method: "onCreate",
    descriptor: "(Landroid/os/Bundle;)V",
};
/// Step 6: instance `Activity.onStart() -> void` (on the step-4 object). The first half of ATL's
/// `activity_start` (`main.c`): after `onCreate`, the launcher Activity is moved to the STARTED
/// state. No-arg instance method. 2026-06-05: driven.
pub const STEP6_ACTIVITY_ON_START: RecipeStep = RecipeStep {
    class: "android/app/Activity",
    method: "onStart",
    descriptor: "()V",
};
/// Step 7: instance `Activity.onResume() -> void` (on the step-4 object). The second half of ATL's
/// `activity_start`: the Activity reaches the RESUMED (running/interactive) state — the app is now
/// live. No-arg instance method. 2026-06-05: driven.
pub const STEP7_ACTIVITY_ON_RESUME: RecipeStep = RecipeStep {
    class: "android/app/Activity",
    method: "onResume",
    descriptor: "()V",
};
/// The `android.app.Activity` class (internal name) — hosts the `static` `createMainActivity` entry
/// point (step 4) and the instance `onCreate(Bundle)`/`onStart()`/`onResume()` (steps 5–7).
pub const ACTIVITY_CLASS: &JNIStr = jni_str!("android/app/Activity");

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
/// 2026-06-05: the driver now attempts the full recipe 1–7. It reaches
/// [`ApplicationOnCreate`](LifecycleProgress::ApplicationOnCreate) (steps 1–3 proven), then drives
/// step 4 (`Activity.createMainActivity`) and step 5 (`Activity.onCreate`) —
/// [`ActivityOnCreate`](LifecycleProgress::ActivityOnCreate) — then steps 6–7 (`Activity.onStart` +
/// `Activity.onResume`, ATL's `activity_start`), reaching
/// [`ActivityResumed`](LifecycleProgress::ActivityResumed). Step 4 onward consume the `jlong` window
/// handle, which the Window/View natives **dereference** (unlike steps 1–3, which only store it) —
/// those natives are bound non-GTK against [`window_registry`]/[`view_registry`] as the dev-host run
/// surfaces them.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum LifecycleProgress {
    /// `find_class` resolved both [`CONTEXT_CLASS`] and [`APPLICATION_CLASS`] from the attached
    /// main thread: the `from_raw` + `attach_current_thread` + `find_class` bridge to the loaded
    /// `android.*` framework works. An intermediate milestone on the way to
    /// [`ApplicationOnCreate`](Self::ApplicationOnCreate).
    BridgeProven,
    /// Recipe steps 1–3 ran on the attached main thread: `Context.createApplication(window)` returned
    /// an `Application`, `ContentProvider.createContentProviders()` completed, and
    /// `Application.onCreate()` was invoked on the returned object.
    ApplicationOnCreate,
    /// Recipe steps 4–5 also ran: `Activity.createMainActivity(className, window, uri)` returned an
    /// `Activity` and `Activity.onCreate(null Bundle)` was invoked on it. The launcher Activity's
    /// `onCreate` reached.
    ActivityOnCreate,
    /// Recipe steps 6–7 also ran: `Activity.onStart()` then `Activity.onResume()` were invoked on the
    /// step-4 `Activity` object (ATL's `activity_start`). The launcher Activity reached the RESUMED
    /// (running/interactive) state — the increment's milestone.
    ActivityResumed,
}

// === Eclipse's interception of ART's java.lang.Runtime.nativeLoad ================================
//
// 2026-06-11: ART's `System.loadLibrary(name)` → `Runtime.nativeLoad(path, loader, caller)` →
// `art::JavaVMExt::LoadNativeLibrary(path)` → `bionic_dlopen(path)` (the apkenv shim linker). For the
// app's engine JNI libs (e.g. `libzstd-jni`, loaded by `androidx.startup` during `onCreate`) the
// apkenv linker ABORTS — it cannot apply their modern relocations / its dependency-graph walk
// NULL-derefs (docs/libroblox-init-run.md §9–§11) → SIGSEGV. Eclipse already PRE-LOADS those libs
// through its own Rust loader (src/loader/engine.rs): mapped + relocated + fully-resolved (+
// `JNI_OnLoad` called for the engine) BEFORE the lifecycle. The MISSING half was the CONSULT — making
// `Runtime.nativeLoad` report a pre-loaded soname as already-loaded so it never re-enters apkenv.
//
// This binds Eclipse's own `Runtime.nativeLoad` via RegisterNatives (wins over the libcore native).
// Per call:
//   * derive the soname from the resolved path; if Eclipse pre-loaded it → return `null` = SUCCESS
//     (ART's `nativeLoad` contract: `null` on success, an error `String` on failure), skipping
//     apkenv entirely — this is the documented §10/§11 "registry consult";
//   * else → DELEGATE to ART's REAL `JavaVMExt::LoadNativeLibrary` so every OTHER library (e.g.
//     `libwolfssljni`, a discovery-based lib loaded during cert verification) loads through ART's
//     normal path — its handle goes into `libraries_`, its `JNI_OnLoad` runs, its `Java_*` symbols
//     stay discoverable — EXACTLY as before this interception existed (zero regression).
//
// With NO pre-loaded lib in the registry (e.g. a pure-Java demo APK) the interception is a pure
// passthrough (delegates everything), so it is a no-op there. It may run on a background thread
// (Roblox loads zstd-jni on its `AppStartupTaskM` thread); all of it is thread-safe (the registry is
// Mutex-guarded; delegation re-enters ART with the calling thread's `JNIEnv`, which ART locks).

unsafe extern "C" {
    /// Delegate one `nativeLoad` to ART's real `art::JavaVMExt::LoadNativeLibrary` via the C++ shim
    /// (`src/loader/native_load_shim.cpp`), which builds the `std::string` args with the host
    /// libstdc++ (correct ABI) and calls `load_fn` (the runtime-`dlsym`'d member-function address,
    /// invoked with an explicit `this` per the Itanium C++ ABI for a non-virtual member). Returns 1
    /// on success (lib loaded), 0 on failure (error copied NUL-terminated into `err_buf`).
    fn eclipse_art_load_native_library(
        load_fn: *mut c_void,
        vm: *mut c_void,
        env: *mut c_void,
        path: *const c_char,
        class_loader: *mut c_void,
        caller_class: *mut c_void,
        err_buf: *mut c_char,
        err_cap: usize,
    ) -> c_int;
}

/// The mangled symbol of `art::JavaVMExt::LoadNativeLibrary(JNIEnv*, const std::string&, jobject,
/// jclass, std::string*)`, exported (`T`) by `libart.so` (verified `nm -D`). `dlsym`'d from the global
/// scope (libart is opened RTLD_GLOBAL by `runtime::boot`) to delegate non-pre-loaded `nativeLoad`
/// calls to ART's real loader. 2026-06-11.
const ART_LOAD_NATIVE_LIBRARY_SYMBOL: &[u8] =
    b"_ZN3art9JavaVMExt17LoadNativeLibraryEP7_JNIEnvRKNSt7__cxx1112basic_stringIcSt11char_traitsIcESaIcEEEP8_jobjectP7_jclassPS8_\0";

/// The runtime address of ART's `JavaVMExt::LoadNativeLibrary` (resolved once, cached). `None` if the
/// symbol is absent (libart not RTLD_GLOBAL, or a build without it) — then the interception cannot
/// delegate and reports a load *failure* for a non-pre-loaded lib rather than silently faking success.
fn art_load_native_library_fn() -> Option<*mut c_void> {
    static FN: OnceLock<usize> = OnceLock::new();
    let addr = *FN.get_or_init(|| {
        // SAFETY: 2026-06-11 — `dlsym(RTLD_DEFAULT, name)` with a NUL-terminated C symbol name.
        // `libart.so` is opened RTLD_GLOBAL by `runtime::boot` (so its symbols are in the global
        // scope) before any lifecycle/native runs. `dlsym` returns null if the symbol is absent.
        let p = unsafe {
            libc::dlsym(
                libc::RTLD_DEFAULT,
                ART_LOAD_NATIVE_LIBRARY_SYMBOL.as_ptr() as *const c_char,
            )
        };
        p as usize
    });
    (addr != 0).then_some(addr as *mut c_void)
}

/// The library soname = the final path component of the resolved load path ART hands `nativeLoad`
/// (e.g. `/…/native-libs/libzstd-jni-1.5.7-6.so` → `libzstd-jni-1.5.7-6.so`). Pure; unit-tested.
fn soname_from_load_path(path: &str) -> &str {
    path.rsplit('/').next().unwrap_or(path)
}

/// `java.lang.Runtime` (internal/slashed name) — hosts the `nativeLoad` Eclipse intercepts.
const RUNTIME_CLASS: &JNIStr = jni_str!("java/lang/Runtime");
// JNI name + descriptor for `Runtime.nativeLoad`, the API-26+ libcore form
// `private static native String nativeLoad(String filename, ClassLoader loader, Class<?> caller);`
// (the `caller` arg matches `LoadNativeLibrary`'s `jclass caller_class`). A signature mismatch makes
// RegisterNatives throw (best-effort handled in [`register_runtime_native_load_natives`]).
const NATIVE_LOAD_NAME: &JNIStr = jni_str!("nativeLoad");
const NATIVE_LOAD_SIG: &JNIStr =
    jni_str!("(Ljava/lang/String;Ljava/lang/ClassLoader;Ljava/lang/Class;)Ljava/lang/String;");

/// `Runtime.nativeLoad(String filename, ClassLoader loader, Class caller)` → `null` on success, an
/// error `String` on failure (ART's contract).
///
/// Eclipse's interception: a pre-loaded soname (Eclipse's Rust loader already mapped + relocated +
/// resolved it) → `null` (success, apkenv skipped); otherwise DELEGATE to ART's real
/// `LoadNativeLibrary` so the lib loads through ART's normal path unchanged.
///
/// JNI ABI: a `static` native, so the second argument is the `JClass`. The body runs inside
/// [`EnvUnowned::with_env`] (`catch_unwind`-wrapped so a Rust panic can never unwind into ART's C++,
/// AGENTS.md §2.8). `resolve::<LogErrorAndDefault>` returns the default (a null `JString`) only on an
/// internal JNI error/panic; the normal success/failure outcomes are returned explicitly.
extern "system" fn runtime_native_load<'local>(
    mut env: EnvUnowned<'local>,
    _class: JClass<'local>,
    filename: JString<'local>,
    loader: JObject<'local>,
    caller: JClass<'local>,
) -> JString<'local> {
    env.with_env(|env| -> jni::errors::Result<JString<'local>> {
        // A null filename has no soname to consult — fall through to delegation (ART treats a null
        // path as "the running executable", which its loader handles).
        let path = if filename.is_null() {
            String::new()
        } else {
            filename.try_to_string(env)?
        };

        // 1) CONSULT Eclipse's pre-load registry by soname. A pre-loaded lib is already mapped +
        //    relocated + resolved (+ JNI_OnLoad called for the engine) — report success, skip apkenv.
        if !path.is_empty() && crate::loader::engine::is_preloaded(soname_from_load_path(&path)) {
            tracing::info!(
                soname = soname_from_load_path(&path),
                "Runtime.nativeLoad: already pre-loaded by Eclipse's Rust loader — reporting success (apkenv skipped)"
            );
            return Ok(JString::default()); // null == success per ART's nativeLoad contract
        }

        // 2) NOT pre-loaded: delegate to ART's real LoadNativeLibrary so this lib loads through ART's
        //    normal path (handle in libraries_, JNI_OnLoad, Java_* discovery) exactly as before.
        let Some(load_fn) = art_load_native_library_fn() else {
            // Cannot delegate: report a real failure (an error String) rather than fake success —
            // returning null would wrongly mark the lib loaded and its natives would never bind.
            return env.new_string(format!(
                "Eclipse: cannot load \"{path}\": ART JavaVMExt::LoadNativeLibrary not found (is libart RTLD_GLOBAL?)"
            ));
        };
        let java_vm = env.get_java_vm()?.get_raw() as *mut c_void; // JavaVMExt* == the JavaVM*
        let raw_env = env.get_raw() as *mut c_void;
        let loader_raw = loader.as_raw() as *mut c_void;
        let caller_raw = caller.as_raw() as *mut c_void;
        // A path with an interior NUL cannot be a real file path → a load failure.
        let c_path = match CString::new(path.as_str()) {
            Ok(s) => s,
            Err(_) => return env.new_string(format!("Eclipse: invalid library path \"{path}\"")),
        };
        let mut err_buf = [0u8; 1024];
        // SAFETY: 2026-06-11 — `load_fn` is the dlsym'd `JavaVMExt::LoadNativeLibrary` (a non-virtual
        // member, callable as a free fn with explicit `this` per the Itanium C++ ABI); `java_vm` is
        // the live `JavaVMExt*` (ART upcasts `JavaVM*`→`JavaVMExt*`, same address); `raw_env` is the
        // calling thread's `JNIEnv*`; `loader_raw`/`caller_raw` are the JNI args; `c_path` is a valid
        // NUL-terminated path held alive across the call; `err_buf` is a 1 KiB out buffer. The C++
        // shim builds the `std::string` args with the host libstdc++ ART also links. This re-enters
        // ART's loader exactly as the libcore `Runtime_nativeLoad` native does (same JNI context).
        let ok = unsafe {
            eclipse_art_load_native_library(
                load_fn,
                java_vm,
                raw_env,
                c_path.as_ptr(),
                loader_raw,
                caller_raw,
                err_buf.as_mut_ptr() as *mut c_char,
                err_buf.len(),
            )
        };
        if ok == 1 {
            Ok(JString::default()) // success
        } else {
            // Build the error String from the shim's NUL-terminated buffer and return it (failure).
            let end = err_buf.iter().position(|&b| b == 0).unwrap_or(err_buf.len());
            let msg = String::from_utf8_lossy(&err_buf[..end]).into_owned();
            let msg = if msg.is_empty() {
                format!("Eclipse: failed to load \"{path}\"")
            } else {
                msg
            };
            env.new_string(msg)
        }
    })
    .resolve::<LogErrorAndDefault>()
}

/// Bind Eclipse's `Runtime.nativeLoad` interception on `java/lang/Runtime`.
///
/// BEST-EFFORT: if this libcore's `nativeLoad` has a different arity/signature, RegisterNatives
/// throws — we describe+clear+log it and leave ART's original `nativeLoad` in place (so non-engine
/// libs are unaffected) rather than aborting the boot. The dev-host log then shows `Runtime`'s method
/// table (as for Canvas), naming the real signature. A pre-loaded lib is still pre-loaded; only the
/// apkenv-skip is missing if registration fails.
///
/// # Safety / soundness
/// `register_native_methods` is `unsafe`: the fn pointer must match the declared JNI signature. It
/// does, by construction — [`runtime_native_load`] is written to the exact `(Ljava/lang/String;
/// Ljava/lang/ClassLoader;Ljava/lang/Class;)Ljava/lang/String;` descriptor. The body is
/// `catch_unwind`-guarded via [`EnvUnowned::with_env`], so no Rust panic can cross the JNI boundary.
fn register_runtime_native_load_natives(env: &mut Env) -> Result<(), FrameworkError> {
    let class = env.find_class(RUNTIME_CLASS)?;
    let methods = [
        // SAFETY: `runtime_native_load` matches the paired signature (see its docs); casting the
        // `extern "system"` fn to a `*mut c_void` is how `NativeMethod::from_raw_parts` takes it.
        unsafe {
            NativeMethod::from_raw_parts(
                NATIVE_LOAD_NAME,
                NATIVE_LOAD_SIG,
                runtime_native_load as *mut c_void,
            )
        },
    ];
    // SAFETY: `class` is the loaded java/lang/Runtime; the fn pointer's signature matches the
    // declared native. On a signature mismatch ART throws (handled best-effort below).
    match unsafe { env.register_native_methods(&class, &methods) } {
        Ok(()) => {
            tracing::info!(
                class = "java/lang/Runtime",
                "registered Eclipse's Runtime.nativeLoad interception (pre-loaded libs skip apkenv; others delegate to ART's LoadNativeLibrary)"
            );
        }
        Err(e) => {
            // Clear any pending exception so it can't poison the next JNI call; log it as the
            // discovery signal (the dev-host run then dumps Runtime's method table = the real sig).
            if env.exception_check() {
                env.exception_clear();
            }
            tracing::warn!(
                class = "java/lang/Runtime",
                error = %e,
                "could not register Runtime.nativeLoad interception (signature mismatch?); apkenv path unchanged — engine libs still pre-loaded"
            );
        }
    }
    Ok(())
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
/// # Steps 4–7
/// Drives step 4 (`Activity.createMainActivity(launcher_activity, window, null)`), step 5
/// (`Activity.onCreate(null Bundle)`), then step 6 (`Activity.onStart()`) and step 7
/// (`Activity.onResume()`) after steps 1–3. `launcher_activity` is the dotted Java class name of the
/// manifest's MAIN/LAUNCHER Activity. The `jlong` window handle is the same Eclipse-owned
/// [`window_registry`] handle steps 1–3 received; step 4's Window/View natives dereference it (bound
/// non-GTK against [`window_registry`]/[`view_registry`]). Steps 6–7 are ATL's `activity_start`: they
/// call `onStart()` then `onResume()` on the step-4 `Activity` object (no args), driving it to the
/// RESUMED state. Returns [`LifecycleProgress::ActivityResumed`] on success; if a step's native is
/// not yet bound the run's `No implementation found` line names the next one to add (the dev-host
/// discovery loop).
pub fn drive_application_lifecycle(
    vm: &Vm,
    apk_path: &str,
    launcher_activity: &str,
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
        match std::panic::catch_unwind(AssertUnwindSafe(|| {
            drive_lifecycle(env, apk_path, launcher_activity)
        })) {
            Ok(result) => result,
            Err(_) => Err(FrameworkError::Panicked),
        }
    })
}

/// Dispatch a click to the [`view_registry`] view identified by `handle` by calling the public Java
/// `View.performClick()Z` on its recorded global object — firing the registered `OnClickListener`.
///
/// 2026-06-05: the minimal SOUND click path. The winit event loop (`graphics.rs`) hit-tests the laid-
/// out view tree on a primary pointer press+release, then calls this with the hit view's handle and a
/// borrow of the held [`Vm`]. The `&Vm` keeps the VM alive (and pins us to its main thread) for the
/// call; the event loop runs on that same JNI-attached main thread, so re-attaching is cheap and the
/// VM is reachable. The full `MotionEvent`/`InputQueue` touch+move+key dispatch is the documented
/// follow-up.
///
/// The JNI work is `catch_unwind`-guarded so a Rust panic can never unwind into ART's C++; a thrown
/// Java exception is described + cleared by [`checked`]. Returns `true` iff `performClick` ran and
/// returned `true` (Android: a listener was invoked); `false`/typed `Err` otherwise — never a panic
/// across the boundary.
///
/// # Errors
/// [`FrameworkError::NullVm`] if the VM pointer is null; [`FrameworkError::Jni`] on a JNI/Java error;
/// [`FrameworkError::Panicked`] if a panic was caught at the boundary.
pub fn dispatch_click_to_view(
    vm: &Vm,
    handle: view_registry::ViewHandle,
) -> Result<bool, FrameworkError> {
    let raw = vm.as_raw();
    if raw.is_null() {
        return Err(FrameworkError::NullVm);
    }
    // SAFETY: `raw` is the live `*mut JavaVM` `boot()` produced, kept alive by the `&Vm` borrow for
    // this call (verified non-null above); `from_raw`'s contract is exactly that. It returns the
    // process VM singleton (idempotent across calls).
    let java_vm = unsafe { JavaVM::from_raw(raw) };
    java_vm.attach_current_thread(|env: &mut Env| {
        match std::panic::catch_unwind(AssertUnwindSafe(|| perform_click(env, handle))) {
            Ok(result) => result,
            Err(_) => Err(FrameworkError::Panicked),
        }
    })
}

/// Call `View.performClick()Z` on the global object recorded for `handle`. Returns `false` (not an
/// error) when the handle is valid but has no recorded global object (a non-dispatchable view) or is
/// stale/fabricated — those are normal "nothing to click" outcomes the event loop ignores. A thrown
/// Java exception inside `performClick` is turned into a typed [`FrameworkError::Jni`] by [`checked`].
fn perform_click(env: &mut Env, handle: view_registry::ViewHandle) -> Result<bool, FrameworkError> {
    // Hold the registry lock only long enough to read the global ref into a `call_method`. The
    // closure makes exactly one JNI call (no registry re-entry), so the lock contract of
    // `with_jobject` is honored.
    let result = view_registry::with_jobject(handle, |global| {
        checked(env, "View.performClick", |env| {
            env.call_method(
                global.as_obj(),
                jni_str!("performClick"),
                jni_sig!("()Z"),
                &[],
            )?
            .z()
        })
    });
    match result {
        Ok(Some(Ok(clicked))) => Ok(clicked),
        Ok(Some(Err(e))) => Err(e),
        // Valid handle but no recorded jobject: a non-dispatchable view — nothing to click.
        Ok(None) => Ok(false),
        // Stale/fabricated handle or poisoned lock: nothing to click (logged, not fatal).
        Err(e) => {
            tracing::debug!(handle, error = %e, "performClick: view not dispatchable (ignored)");
            Ok(false)
        }
    }
}

/// A single-pointer touch action — the subset Eclipse dispatches this increment (DOWN/UP). 2026-06-05:
/// `MOVE` and multi-touch/key are documented follow-ups. The discriminant maps to the public Android
/// `MotionEvent` action code via [`Self::code`] (`ACTION_DOWN = 0`, `ACTION_UP = 1`).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum MotionAction {
    /// `MotionEvent.ACTION_DOWN` — the pointer first contacts the view (press).
    Down,
    /// `MotionEvent.ACTION_UP` — the pointer leaves the view (release); the View's own click
    /// detection fires its `OnClickListener` on an UP that completes a tap.
    Up,
}

impl MotionAction {
    /// The public Android `MotionEvent` action code (`android.view.MotionEvent.ACTION_*`): a stable
    /// part of the Android API. `ACTION_DOWN = 0`, `ACTION_UP = 1`. Pure (GPU/VM-free) so it is
    /// unit-testable; passed verbatim as the `action` arg to `MotionEvent.obtain`.
    pub fn code(self) -> jint {
        match self {
            Self::Down => 0, // MotionEvent.ACTION_DOWN
            Self::Up => 1,   // MotionEvent.ACTION_UP
        }
    }
}

/// Dispatch a real Android [`MotionEvent`] of `action` at window pixel `(x, y)` to the
/// [`view_registry`] view identified by `handle`, by building the event with the public Java factory
/// `MotionEvent.obtain(...)` and routing it through `View.dispatchTouchEvent(MotionEvent)`.
///
/// 2026-06-05: the faithful Android-input follow-up to [`dispatch_click_to_view`] (which only fired
/// `View.performClick`). The winit event loop (`graphics.rs`) calls this with `Down` on a primary
/// press over a clickable hit view, then `Up` on the release over the same view; the View's own click
/// detection then fires the registered `OnClickListener` from the UP. The event's `downTime`/
/// `eventTime` come from `SystemClock.uptimeMillis()` (Eclipse's bound monotonic clock), exactly as
/// Android's input pipeline times them; `metaState` is `0` (no modifier keys this increment). Full
/// `MotionEvent` MOVE/multi-touch/key dispatch is the documented follow-up.
///
/// Mirrors [`dispatch_click_to_view`]'s soundness: attaches the held VM's main thread (the `&Vm`
/// borrow keeps it alive + pins us to that thread), wraps the JNI work in `catch_unwind` so a Rust
/// panic can never unwind into ART's C++ (`panic = "abort"`, §2.8), and routes every JNI call through
/// [`checked`] (a thrown Java exception is described + cleared into a typed [`FrameworkError::Jni`]).
/// The obtained `MotionEvent` is `recycle()`d after dispatch on every path.
///
/// Returns `true` iff `dispatchTouchEvent` returned `true` (the View consumed the event); `false`
/// when the view has no dispatchable Java object, or `dispatchTouchEvent` returned `false`; a typed
/// `Err` on a VM/JNI/Java error — never a panic across the boundary.
///
/// # Errors
/// [`FrameworkError::NullVm`] if the VM pointer is null; [`FrameworkError::Jni`] on a JNI/Java error;
/// [`FrameworkError::Panicked`] if a panic was caught at the boundary.
pub fn dispatch_touch_to_view(
    vm: &Vm,
    handle: view_registry::ViewHandle,
    action: MotionAction,
    x: f32,
    y: f32,
) -> Result<bool, FrameworkError> {
    let raw = vm.as_raw();
    if raw.is_null() {
        return Err(FrameworkError::NullVm);
    }
    // SAFETY: `raw` is the live `*mut JavaVM` `boot()` produced, kept alive by the `&Vm` borrow for
    // this call (verified non-null above); `from_raw`'s contract is exactly that. It returns the
    // process VM singleton (idempotent across calls), same as `dispatch_click_to_view`.
    let java_vm = unsafe { JavaVM::from_raw(raw) };
    java_vm.attach_current_thread(|env: &mut Env| {
        match std::panic::catch_unwind(AssertUnwindSafe(|| touch_view(env, handle, action, x, y))) {
            Ok(result) => result,
            Err(_) => Err(FrameworkError::Panicked),
        }
    })
}

/// Build a [`MotionEvent`] for `action` at `(x, y)` and dispatch it to the global object recorded for
/// `handle` via `View.dispatchTouchEvent`, then `recycle()` the event. Returns `false` (not an error)
/// when the handle is valid but has no recorded global object (a non-dispatchable view) or is
/// stale/fabricated — the event loop treats those as no-ops. A thrown Java exception is turned into a
/// typed [`FrameworkError::Jni`] by [`checked`].
fn touch_view(
    env: &mut Env,
    handle: view_registry::ViewHandle,
    action: MotionAction,
    x: f32,
    y: f32,
) -> Result<bool, FrameworkError> {
    // Hold the registry lock only long enough to dispatch (the closure makes JNI calls but never
    // re-enters the registry), honoring `with_jobject`'s contract.
    let result = view_registry::with_jobject(handle, |global| {
        // Monotonic event time from Eclipse's bound SystemClock.uptimeMillis (the time Android's
        // input pipeline stamps a MotionEvent with). One call serves as both downTime and eventTime
        // for a single isolated press/release — adequate for the single-pointer DOWN/UP this increment.
        let system_clock = env.find_class(SYSTEM_CLOCK_CLASS)?;
        let now = checked(env, "SystemClock.uptimeMillis", |env| {
            env.call_static_method(
                &system_clock,
                jni_str!("uptimeMillis"),
                jni_sig!("()J"),
                &[],
            )?
            .j()
        })?;

        // MotionEvent.obtain(downTime, eventTime, action, x, y, metaState) — the public Java factory.
        let motion_event_class = env.find_class(MOTION_EVENT_CLASS)?;
        let event = checked(env, "MotionEvent.obtain", |env| {
            env.call_static_method(
                &motion_event_class,
                jni_str!("obtain"),
                jni_sig!("(JJIFFI)Landroid/view/MotionEvent;"),
                &[
                    JValue::Long(now),
                    JValue::Long(now),
                    JValue::Int(action.code()),
                    JValue::Float(x),
                    JValue::Float(y),
                    JValue::Int(0), // metaState: no modifier keys this increment
                ],
            )?
            .l()
        })?;

        // View.dispatchTouchEvent(event) — routes through onTouchEvent + the View's click detection.
        let consumed = checked(env, "View.dispatchTouchEvent", |env| {
            env.call_method(
                global.as_obj(),
                jni_str!("dispatchTouchEvent"),
                jni_sig!("(Landroid/view/MotionEvent;)Z"),
                &[JValue::Object(&event)],
            )?
            .z()
        });

        // Recycle the event on EVERY path (success or dispatch error) — it is pooled, not GC-freed
        // promptly, so returning it avoids exhausting the recycler over many touches. A recycle
        // failure is logged but does not mask the dispatch result.
        if let Err(e) = checked(env, "MotionEvent.recycle", |env| {
            env.call_method(&event, jni_str!("recycle"), jni_sig!("()V"), &[])?
                .v()
        }) {
            tracing::debug!(handle, error = %e, "MotionEvent.recycle failed (ignored)");
        }
        consumed
    });
    match result {
        Ok(Some(Ok(consumed))) => Ok(consumed),
        Ok(Some(Err(e))) => Err(e),
        // Valid handle but no recorded jobject: a non-dispatchable view — nothing to touch.
        Ok(None) => Ok(false),
        // Stale/fabricated handle or poisoned lock: nothing to touch (logged, not fatal).
        Err(e) => {
            tracing::debug!(handle, error = %e, "dispatchTouchEvent: view not dispatchable (ignored)");
            Ok(false)
        }
    }
}

/// A view that should have its `onDraw(Canvas)` driven: its [`view_registry`] handle and the pixel
/// size of its laid-out rect (the [`canvas_registry`] Pixmap is allocated at this size).
///
/// 2026-06-05: computed by the renderer's GPU-free layout pass (`graphics::layout_views`) and handed
/// to [`drive_view_draw`]; kept a plain `Copy` value so the geometry stays GPU-free and the framework
/// crate need not depend on the renderer's layout types.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct DrawTarget {
    /// The view's [`view_registry`] handle (the same `jlong` the native peer holds).
    pub handle: view_registry::ViewHandle,
    /// The laid-out width in pixels (`>= 1`; the renderer clamps a degenerate rect out).
    pub width: u32,
    /// The laid-out height in pixels (`>= 1`).
    pub height: u32,
}

/// One drawn custom view: its [`view_registry`] handle paired with the [`canvas_registry`] handle of
/// the Pixmap its `onDraw(Canvas)` rasterized into. The renderer uploads the Pixmap over the view's
/// rect, then [`canvas_registry::free`]s the handle.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct DrawnCanvas {
    /// The custom view whose `onDraw` ran.
    pub view: view_registry::ViewHandle,
    /// The Pixmap-backed Canvas its `onDraw` rasterized into.
    pub canvas: canvas_registry::CanvasHandle,
}

/// Drive `View.draw(Canvas)` for each custom view in `targets`, rasterizing each into an Eclipse-owned
/// [`canvas_registry`] Pixmap, and return the `(view, canvas)` pairs that drew successfully.
///
/// 2026-06-05: the DRAW CASCADE. Android's `ViewRootImpl.performTraversals` is what normally calls
/// `View.draw(Canvas)`; Eclipse's minimal lifecycle never runs it, so a custom View's `onDraw` (e.g.
/// multitouch.test's touch circles) never fires. This drives it directly: for each target it allocates
/// a Pixmap-backed Canvas sized to the view's laid-out rect, constructs a Java
/// `android.graphics.Canvas(long nativeCanvas)` over that handle, and invokes `View.draw(Canvas)` on
/// the view's recorded global object (which dispatches into the View's `onDraw` + the bound
/// [Canvas draw natives](register_canvas_natives), filling the Pixmap with REAL tiny-skia raster). The
/// renderer then uploads each returned Pixmap as an RGBA texture over the owning view's rect (the
/// composite) and frees it.
///
/// MUST be called on the VM/winit main thread (the `&Vm` borrow keeps the VM alive + pins us there).
/// The JNI work is wrapped in `catch_unwind` so a Rust panic can never unwind into ART's C++
/// (`panic = "abort"`, §2.8); every JNI call routes through [`checked`] (a thrown Java exception is
/// described + cleared into a typed error, never left pending). A target whose Canvas can't be built,
/// whose view has no recorded Java object, or whose `draw` throws is skipped (its Pixmap is freed) —
/// the others still draw. Returns the successfully-drawn canvases; an empty `Vec` on no VM-reachable
/// drawable target (never a panic across the boundary).
///
/// # Errors
/// [`FrameworkError::NullVm`] if the VM pointer is null; [`FrameworkError::Jni`] only on an attach
/// failure (per-target Java errors are skipped, not surfaced).
pub fn drive_view_draw(
    vm: &Vm,
    targets: &[DrawTarget],
) -> Result<Vec<DrawnCanvas>, FrameworkError> {
    // Skip the cascade entirely on a Canvas build that can't back it (no nDraw* natives / no
    // Canvas(long) ctor — set by register_canvas_natives). Avoids re-attempting + re-logging the
    // missing constructor every frame; the view quads + text still render.
    if targets.is_empty() || !canvas_draw_supported() {
        return Ok(Vec::new());
    }
    let raw = vm.as_raw();
    if raw.is_null() {
        return Err(FrameworkError::NullVm);
    }
    // SAFETY: `raw` is the live `*mut JavaVM` `boot()` produced, kept alive by the `&Vm` borrow for
    // this call (verified non-null above); `from_raw`'s contract is exactly that.
    let java_vm = unsafe { JavaVM::from_raw(raw) };
    java_vm.attach_current_thread(|env: &mut Env| {
        match std::panic::catch_unwind(AssertUnwindSafe(|| draw_targets(env, targets))) {
            Ok(result) => result,
            Err(_) => Err(FrameworkError::Panicked),
        }
    })
}

/// Run the draw cascade body: build a `Canvas` + invoke `View.draw` for each target, collecting the
/// drawn `(view, canvas)` pairs. Split out so the panic guard in [`drive_view_draw`] wraps one call.
/// A per-target failure frees that target's Canvas (if allocated) and continues — never aborts the
/// whole cascade. Always returns `Ok` (per-target errors are logged, not propagated).
fn draw_targets(env: &mut Env, targets: &[DrawTarget]) -> Result<Vec<DrawnCanvas>, FrameworkError> {
    // Resolve the Canvas class + its `(long)` constructor once for the whole cascade.
    let canvas_class = env.find_class(CANVAS_CLASS)?;
    let mut drawn = Vec::with_capacity(targets.len());
    for t in targets {
        // Allocate the Pixmap-backed Canvas at the view's laid-out size (rejects 0/oversize).
        let canvas_handle = match canvas_registry::allocate(t.width, t.height) {
            Ok(h) => h,
            Err(e) => {
                tracing::debug!(view = t.handle, w = t.width, h = t.height, error = %e,
                    "draw cascade: canvas allocate failed (skipped)");
                continue;
            }
        };
        // Construct `new android.graphics.Canvas(canvas_handle)` — the standard AOSP public ctor whose
        // `long` is the native canvas handle (here the Eclipse slab index). The draw natives this
        // Canvas's ops resolve to are bound by `register_canvas_natives`.
        let canvas_obj = match checked(env, "Canvas.<init>(long)", |env| {
            env.new_object(
                &canvas_class,
                jni_sig!("(J)V"),
                &[JValue::Long(canvas_handle)],
            )
        }) {
            Ok(o) => o,
            Err(e) => {
                tracing::debug!(view = t.handle, canvas = canvas_handle, error = %e,
                    "draw cascade: Canvas.<init> failed (skipped)");
                let _ = canvas_registry::free(canvas_handle);
                continue;
            }
        };
        // Invoke `View.draw(Canvas)` on the view's recorded global object — this dispatches the View's
        // own draw pass (background + onDraw + children), so the custom view's `onDraw` runs and its
        // Canvas ops raster into the Pixmap. `with_jobject` holds the registry lock only to read the
        // global ref; the single JNI call inside does not re-enter the registry (lock contract honored).
        let result = view_registry::with_jobject(t.handle, |global| {
            checked(env, "View.draw(Canvas)", |env| {
                env.call_method(
                    global.as_obj(),
                    jni_str!("draw"),
                    jni_sig!("(Landroid/graphics/Canvas;)V"),
                    &[JValue::Object(&canvas_obj)],
                )?
                .v()
            })
        });
        match result {
            Ok(Some(Ok(()))) => {
                tracing::debug!(
                    view = t.handle,
                    canvas = canvas_handle,
                    w = t.width,
                    h = t.height,
                    "draw cascade: View.draw(Canvas) ran — onDraw rasterized into the Pixmap"
                );
                drawn.push(DrawnCanvas {
                    view: t.handle,
                    canvas: canvas_handle,
                });
            }
            // A Java exception, a non-dispatchable view, or a stale handle: free the Canvas + skip.
            other => {
                if let Ok(Some(Err(e))) = &other {
                    tracing::debug!(view = t.handle, error = %e, "draw cascade: View.draw threw (skipped)");
                } else {
                    tracing::trace!(view = t.handle, "draw cascade: view not drawable (skipped)");
                }
                let _ = canvas_registry::free(canvas_handle);
            }
        }
    }
    Ok(drawn)
}

/// Prove the bridge, then drive recipe steps 1–5 to the launcher Activity's `onCreate`. Split out so
/// the panic guard in [`drive_application_lifecycle`] wraps a single named call.
///
/// All JNI calls go through [`checked`], so a thrown Java exception is described + cleared and
/// surfaced as the typed [`FrameworkError::Jni`] rather than left pending or panicking. The recipe
/// class names / descriptors are the [`RecipeStep`] constants ([`STEP1_CREATE_APPLICATION`] …);
/// the matching compile-time `jni_str!`/`jni_sig!` literals at the call sites are pinned equal to
/// those constants by the unit test `call_site_literals_match_recipe_constants` (single source of
/// truth, no per-call allocation or fallible runtime signature parse).
fn drive_lifecycle(
    env: &mut Env,
    apk_path: &str,
    launcher_activity: &str,
) -> Result<LifecycleProgress, FrameworkError> {
    // Bind native_get_apk_path + native_updateConfig BEFORE Context's static initializer can run
    // (find_class loads/links the class but does not initialize it — JNI spec), so the two natives
    // are already resolvable, non-GTK, when <clinit> later calls them. RegisterNatives wins over
    // name-based lazy binding (JNI 1.1 spec), so ATL's GTK-backed symbols are not used.
    register_context_natives(env, apk_path)?;
    // Bind android.util.Log.println_native on its own class. ART resolves natives lazily during the
    // lifecycle, so all discovered natives are registered (per class) BEFORE step 1; the framework
    // logs heavily during init, so this must be bound before createApplication touches Log.
    register_log_natives(env)?;
    // Bind android.content.res.AssetManager.init on its own class — the framework builds an
    // AssetManager early in init (Resources/asset access), so this must be bound before step 1.
    register_asset_manager_natives(env)?;
    // Bind the asset-STREAM read cycle (readAsset/seekAsset/getAssetLength/.../destroyAsset) on the
    // same class, best-effort (a sig drift logs + is discovered, never breaks the natives above). A
    // real app's Application.onCreate opens assets (Roblox's startup tasks do), so bind before step 1.
    register_asset_stream_natives(env)?;
    // Bind android.content.res.XmlBlock's parser natives on its own class — once openXmlAssetNative
    // returns a real block handle, the framework walks it via XmlBlock (reading AndroidManifest.xml
    // during Context.<clinit>), so these must be bound before step 1.
    register_xml_block_natives(env)?;
    // Bind android.os.Environment.native_get_app_data_dir on its own class — the framework queries
    // external storage early in init (`getExternalStorageDirectory`), so this must be bound before
    // step 1.
    register_environment_natives(env)?;
    // Bind android.os.SystemClock.elapsedRealtime on its own class — a real app's Application.<init>
    // may query the monotonic clock during step 1 (observed for Roblox's RobloxApplication.<init>),
    // so this must be bound before step 1. GTK-free; std::time::Instant-backed.
    register_system_clock_natives(env)?;
    // Bind android.os.MessageQueue.nativeInit on its own class — step 0 (Looper.prepareMainLooper)
    // builds the main thread's MessageQueue, which calls nativeInit() in its constructor, so this must
    // be bound before step 0. GTK-free; returns a non-zero non-pointer sentinel (no Looper.loop runs).
    register_message_queue_natives(env)?;
    // Bind android.hardware.SensorManager's accelerometer-listener registration native — an app may
    // register a sensor listener during Activity.onCreate (accelerometerdemo does, in initViews). Honest
    // no-sensor backing: registers no source, delivers no events (this Linux desktop has no accelerometer).
    register_sensor_manager_natives(env)?;
    // Bind android.net.ConnectivityManager's three GTK-backed natives (registerNetworkCallback /
    // isActiveNetworkMetered / nativeGetNetworkAvailable) — Roblox's jobqueue connectivity monitor calls
    // registerNetworkCallback in ActivitySplash.onCreate (step 5). Non-GTK no-op/available backing.
    register_connectivity_natives(env)?;
    // Bind android.view.View's peer natives on its own class — step 4 (createMainActivity) constructs
    // the launcher Activity's View hierarchy, so these must be bound before step 4. Bound non-GTK
    // against view_registry; each new View native the run surfaces is added to register_view_natives.
    register_view_natives(env)?;
    // Bind android.view.Window's window-setup natives on its own class — step 4 wires the launcher's
    // Window onto the native window handle (set_jobject/set_title/set_layout/set_widget_as_root), so
    // these must be bound before step 4. Bound non-GTK against window_registry/view_registry.
    register_window_natives(env)?;
    // Bind android.widget.TextView's peer natives on its own class — the launcher layout inflates a
    // <TextView> during step 5, and ART resolves natives per declaring class (TextView re-declares
    // native_constructor), so this must be bound before step 4. Reuses the View constructor backing.
    register_text_view_natives(env)?;
    // Bind android.widget.ImageView's native_constructor on its own class — a launcher layout may
    // inflate an <ImageView> during step 5 (e.g. AdaptiveIconDemo), and ART resolves natives per
    // declaring class (ImageView re-declares native_constructor), so this must be bound before step 4.
    // Reuses the class-agnostic View constructor backing (records android.widget.ImageView in the tree).
    register_image_view_natives(env)?;
    // Bind android.widget.ImageButton's native_constructor on its own class — AppCompat's Toolbar
    // builds an AppCompatImageButton (extends ImageButton) during step 5's setContentView, and ART
    // resolves natives per declaring class, so this must be bound before step 4. Reuses the
    // class-agnostic View constructor backing (records android.widget.ImageButton in the tree).
    register_image_button_natives(env)?;
    // Bind android.graphics.drawable.Drawable's native_constructor on its own class — a launcher's
    // onCreate may load a drawable during step 5 (e.g. AdaptiveIconDemo's getDrawable), so this must be
    // bound before step 4. GTK-free; returns a non-zero non-pointer sentinel (no draw pass runs).
    register_drawable_natives(env)?;
    // Bind android.view.ViewGroup's tree-wiring natives on its own class — setContentView's
    // LayoutInflater wires children via ViewGroup.addView during step 5, so this must be bound before
    // step 4. Bound non-GTK against view_registry (records the tree edges).
    register_view_group_natives(env)?;
    // Bind android.graphics.Paint's natives on its own class — the View hierarchy's TextPaint/Paint
    // construct during step 5's setContentView, so this must be bound before step 4. Bound non-GTK
    // against paint_registry (config only; no drawing).
    register_paint_natives(env)?;
    // Bind android.graphics.Matrix's natives on its own class — AppCompat's VectorDrawableCompat
    // constructs a Matrix during step 5's setContentView (drawable manager), so this must be bound
    // before step 4. Bound non-GTK against matrix_registry with exact 3x3 affine math (no drawing).
    register_matrix_natives(env)?;
    // Bind android.graphics.Path's geometry natives on its own class — a launcher's onCreate may
    // build a vector-drawable path during step 5 (e.g. AdaptiveIconDemo's getDrawable →
    // AdaptiveIconDrawable → PathParser → Path.moveTo), so this must be bound before step 4. Bound
    // non-GTK against path_registry, recording the REAL parsed contour geometry (no GTK, no Skia-C).
    register_path_natives(env)?;
    // Bind android.graphics.Canvas's draw natives on its own class — a CUSTOM View's onDraw(Canvas)
    // issues these during the draw cascade ([`drive_view_draw`], after RESUMED), so they must be bound
    // before the cascade runs. Bound non-GTK against canvas_registry (real tiny-skia raster).
    register_canvas_natives(env)?;
    // Intercept java.lang.Runtime.nativeLoad BEFORE step 1: Context.<clinit>'s APK signature
    // verification does System.loadLibrary("wolfssljni") (delegated to ART's real loader, unchanged),
    // and Application.onCreate's androidx.startup does System.loadLibrary("zstd-jni") — which Eclipse
    // PRE-LOADED through its Rust loader, so the interception reports it already-loaded and skips the
    // apkenv shim linker (which SIGSEGVs on it). Best-effort: a libcore nativeLoad-signature mismatch
    // logs + leaves ART's original in place (docs/libroblox-init-run.md §10/§11).
    register_runtime_native_load_natives(env)?;

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

    // Step 0: `static Looper.prepareMainLooper() -> void` on the attached main thread, matching
    // ATL's recipe (its boot sequence's first step is `prepare_main_looper`). Android's
    // `Handler.<init>` requires `Looper.myLooper() != null`; a real launcher Activity constructs a
    // `Handler` in a field initializer (every `AppCompatActivity`/`FragmentActivity` does), so the
    // main `Looper` must exist before step 4 builds the Activity — otherwise the Activity ctor throws
    // `RuntimeException: Can't create handler inside thread that has not called Looper.prepare()`
    // (2026-06-05, surfaced by com.ashwin.example.accelerometerdemo; the pure-Java demo_app Activity
    // never touched a Handler, so this gap was previously latent). Idempotent for the process: the
    // main Looper is prepared once. `.v()` asserts the void return.
    let looper_class = env.find_class(LOOPER_CLASS)?;
    checked(env, "step 0 Looper.prepareMainLooper", |env| {
        env.call_static_method(
            &looper_class,
            jni_str!("prepareMainLooper"),
            jni_sig!("()V"),
            &[],
        )?
        .v()
    })?;

    // Step 1: `static Context.createApplication(jlong native_window) -> Application`.
    // 2026-06-05: the handle is now a REAL Eclipse-owned registry handle (was the placeholder `0`):
    // `window_registry::allocate()` reserves a generational-slab slot and returns its packed `jlong`
    // (`docs/art-and-runtime.md` "Non-GTK Window/Surface backing — design"). This is the
    // design-confirmed contract — a genuine Eclipse-owned handle, not a raw pointer — and is still
    // safe for steps 1–3, which only STORE the handle and never dereference it (deref begins at the
    // deferred step 4; "Tier A"). A stale/fabricated handle would be a bounds+generation-checked
    // `Err`, never UB. The slot is intentionally NOT freed during the short run: it stays valid for
    // step 4 (a later, dev-host-gated increment) and the process exits with the window closed.
    // `<clinit>` runs here on first active use of Context, calling the two natives bound above.
    // `.l()` unwraps the returned Application JObject; a wrong return type is a typed error.
    let window_handle = window_registry::allocate()?;
    let context = env.find_class(CONTEXT_CLASS)?;
    let app = checked(env, "step 1 Context.createApplication", |env| {
        env.call_static_method(
            &context,
            jni_str!("createApplication"),
            jni_sig!("(J)Landroid/app/Application;"),
            &[JValue::Long(window_handle)],
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
    // shell self-init.
    checked(env, "step 3 Application.onCreate", |env| {
        env.call_method(&app, jni_str!("onCreate"), jni_sig!("()V"), &[])?
            .v()
    })?;
    tracing::info!("Application.onCreate reached: recipe steps 1–3 driven");

    // Step 4: `static Activity.createMainActivity(String className, jlong native_window, String uri)
    // -> Activity`. `className` is the launcher Activity's dotted Java class name; `native_window` is
    // the SAME Eclipse-owned window-registry handle step 1 received (one window per launch), which
    // step 4's Window/View natives now dereference through window_registry/view_registry (bounds+
    // generation-checked — a bad handle is a typed Err, never UB); `uri` is null (a plain launch, no
    // deep-link). This is the first call that triggers the Window/setContentView/View native cascade.
    // `.l()` unwraps the returned Activity JObject; a wrong return type is a typed error.
    let activity_class = env.find_class(ACTIVITY_CLASS)?;
    let class_name_jstr = env.new_string(launcher_activity)?;
    let activity = checked(env, "step 4 Activity.createMainActivity", |env| {
        env.call_static_method(
            &activity_class,
            jni_str!("createMainActivity"),
            jni_sig!("(Ljava/lang/String;JLjava/lang/String;)Landroid/app/Activity;"),
            &[
                JValue::Object(&class_name_jstr),
                JValue::Long(window_handle),
                // null uri (a fresh launch has no launch URI). A null JObject is the JNI null arg.
                JValue::Object(&JObject::null()),
            ],
        )?
        .l()
    })?;

    // Step 5: instance `Activity.onCreate(Bundle) -> void` on the object from step 4, with a null
    // Bundle (a fresh launch has no saved instance state). Reaching this — which runs the Activity's
    // setContentView → View-hierarchy inflation — is the increment's milestone. `.v()` asserts void.
    checked(env, "step 5 Activity.onCreate", |env| {
        env.call_method(
            &activity,
            jni_str!("onCreate"),
            jni_sig!("(Landroid/os/Bundle;)V"),
            &[JValue::Object(&JObject::null())],
        )?
        .v()
    })?;
    tracing::info!(
        activity = launcher_activity,
        "Activity.onCreate reached: recipe steps 1–5 driven (launcher Activity onCreate)"
    );

    // Step 6: instance `Activity.onStart() -> void` on the step-4 object — the first half of ATL's
    // `activity_start` (`main.c`): the launcher Activity moves to the STARTED state, running the
    // app's own `onStart` override. No args. `.v()` asserts the void return.
    checked(env, "step 6 Activity.onStart", |env| {
        env.call_method(&activity, jni_str!("onStart"), jni_sig!("()V"), &[])?
            .v()
    })?;

    // Step 7: instance `Activity.onResume() -> void` on the step-4 object — the second half of
    // `activity_start`: the Activity reaches the RESUMED (running/interactive) state, running the
    // app's own `onResume` override. No args. `.v()` asserts the void return.
    checked(env, "step 7 Activity.onResume", |env| {
        env.call_method(&activity, jni_str!("onResume"), jni_sig!("()V"), &[])?
            .v()
    })?;

    tracing::info!(
        activity = launcher_activity,
        "Activity resumed: recipe steps 1–7 driven (launcher Activity onStart + onResume)"
    );
    Ok(LifecycleProgress::ActivityResumed)
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
    /// Allocating the Eclipse-owned window-registry handle passed to `createApplication` failed.
    WindowRegistry(window_registry::WindowRegistryError),
}

impl fmt::Display for FrameworkError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::NullVm => f.write_str("framework driver received a null JavaVM pointer"),
            Self::Jni(e) => write!(f, "JNI error driving the framework lifecycle: {e}"),
            Self::Panicked => {
                f.write_str("a panic was caught at the framework JNI boundary (not propagated)")
            }
            Self::WindowRegistry(e) => write!(f, "window-registry handle allocation failed: {e}"),
        }
    }
}

impl std::error::Error for FrameworkError {
    fn source(&self) -> Option<&(dyn std::error::Error + 'static)> {
        match self {
            Self::Jni(e) => Some(e),
            Self::WindowRegistry(e) => Some(e),
            Self::NullVm | Self::Panicked => None,
        }
    }
}

// 2026-06-05: `?` on `window_registry::allocate()` in `drive_steps_1_to_3` folds a registry error
// into the typed `WindowRegistry` variant (no unwrap/expect, §2.8).
impl From<window_registry::WindowRegistryError> for FrameworkError {
    fn from(e: window_registry::WindowRegistryError) -> Self {
        Self::WindowRegistry(e)
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
        // Steps 6–7: Activity.onStart/onResume are no-arg void instance methods (2026-06-05).
        assert_eq!(STEP6_ACTIVITY_ON_START.class, "android/app/Activity");
        assert_eq!(STEP6_ACTIVITY_ON_START.method, "onStart");
        assert_eq!(STEP6_ACTIVITY_ON_START.descriptor, "()V");
        assert_eq!(STEP7_ACTIVITY_ON_RESUME.class, "android/app/Activity");
        assert_eq!(STEP7_ACTIVITY_ON_RESUME.method, "onResume");
        assert_eq!(STEP7_ACTIVITY_ON_RESUME.descriptor, "()V");
    }

    // 2026-06-05: the MotionEvent touch-dispatch path. The action codes are the stable public Android
    // `MotionEvent.ACTION_*` constants — a regression here would dispatch the wrong gesture (e.g. a
    // DOWN that never lifts). Pure data, host-thread-independent (no VM), so unit-tested in-harness.
    #[test]
    fn motion_action_codes_match_public_android_constants() {
        // android.view.MotionEvent.ACTION_DOWN = 0, ACTION_UP = 1 (public Android API).
        assert_eq!(MotionAction::Down.code(), 0, "ACTION_DOWN must be 0");
        assert_eq!(MotionAction::Up.code(), 1, "ACTION_UP must be 1");
    }

    // 2026-06-05: pin the touch-dispatch class + the call-site `jni_str!`/`jni_sig!` literals against
    // the documented public Android API (single source of truth — the call sites in `touch_view` use
    // these exact literals). A transcription regression (wrong descriptor / method name) fails loudly,
    // the same regression guard the recipe descriptors use. `jni_sig!` yields a `MethodSignature`; its
    // `.sig()` is the `&JNIStr` descriptor we compare.
    #[test]
    fn motion_event_dispatch_descriptors_are_the_public_android_api() {
        assert_eq!(MOTION_EVENT_CLASS.to_str(), "android/view/MotionEvent");
        // MotionEvent.obtain(downTime, eventTime, action, x, y, metaState) → MotionEvent.
        assert_eq!(jni_str!("obtain").to_str(), "obtain");
        assert_eq!(
            jni_sig!("(JJIFFI)Landroid/view/MotionEvent;")
                .sig()
                .to_str(),
            "(JJIFFI)Landroid/view/MotionEvent;"
        );
        // View.dispatchTouchEvent(MotionEvent) → boolean.
        assert_eq!(
            jni_str!("dispatchTouchEvent").to_str(),
            "dispatchTouchEvent"
        );
        assert_eq!(
            jni_sig!("(Landroid/view/MotionEvent;)Z").sig().to_str(),
            "(Landroid/view/MotionEvent;)Z"
        );
        // MotionEvent.recycle() → void.
        assert_eq!(jni_str!("recycle").to_str(), "recycle");
        assert_eq!(jni_sig!("()V").sig().to_str(), "()V");
        // SystemClock.uptimeMillis() → long (the event-time source; matches the bound native).
        assert_eq!(
            jni_str!("uptimeMillis").to_str(),
            UPTIME_MILLIS_NAME.to_str()
        );
        assert_eq!(jni_sig!("()J").sig().to_str(), "()J");
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
        // Step 4 createMainActivity + step 5 Activity.onCreate call-site literals (2026-06-05).
        assert_eq!(
            jni_str!("createMainActivity").to_str(),
            STEP4_CREATE_MAIN_ACTIVITY.method
        );
        assert_eq!(
            jni_sig!("(Ljava/lang/String;JLjava/lang/String;)Landroid/app/Activity;")
                .sig()
                .to_str(),
            STEP4_CREATE_MAIN_ACTIVITY.descriptor
        );
        assert_eq!(
            jni_str!("onCreate").to_str(),
            STEP5_ACTIVITY_ON_CREATE.method
        );
        assert_eq!(
            jni_sig!("(Landroid/os/Bundle;)V").sig().to_str(),
            STEP5_ACTIVITY_ON_CREATE.descriptor
        );
        // Step 6 onStart + step 7 onResume call-site literals (2026-06-05).
        assert_eq!(jni_str!("onStart").to_str(), STEP6_ACTIVITY_ON_START.method);
        assert_eq!(
            jni_sig!("()V").sig().to_str(),
            STEP6_ACTIVITY_ON_START.descriptor
        );
        assert_eq!(
            jni_str!("onResume").to_str(),
            STEP7_ACTIVITY_ON_RESUME.method
        );
        assert_eq!(
            jni_sig!("()V").sig().to_str(),
            STEP7_ACTIVITY_ON_RESUME.descriptor
        );
        // The step-4–7 Activity class internal (slashed) name used by find_class.
        assert_eq!(ACTIVITY_CLASS.to_str(), "android/app/Activity");
        assert_eq!(STEP4_CREATE_MAIN_ACTIVITY.class, "android/app/Activity");
        assert_eq!(STEP5_ACTIVITY_ON_CREATE.class, "android/app/Activity");
        assert_eq!(STEP6_ACTIVITY_ON_START.class, "android/app/Activity");
        assert_eq!(STEP7_ACTIVITY_ON_RESUME.class, "android/app/Activity");
    }

    #[test]
    fn resolve_theme_attr_returns_concrete_values_and_none_for_missing() {
        // GPU-free guard for the theme obtainStyledAttributes(int[]) value resolution. A concrete
        // (non-reference) attribute is returned verbatim; an attribute absent from the theme is None
        // (the caller then writes TYPE_NULL → the framework default, NOT a fake value — the exact
        // behavior that, when the map was empty, threw AppCompat's IllegalStateException).
        use crate::framework::theme_registry::ThemeAttr;
        let mut attrs = std::collections::HashMap::new();
        // windowActionBar (0x7f010058) = TYPE_INT_BOOLEAN(0x12) true — the AppCompat check attribute.
        let win_action_bar = u32_to_i32(0x7f01_0058);
        attrs.insert(
            win_action_bar,
            ThemeAttr {
                type_: 0x12,
                data: 0xffff_ffff,
            },
        );
        let e = resolve_theme_attr(&attrs, win_action_bar).expect("present attr resolves");
        assert_eq!(e.value_type, 0x12, "TYPE_INT_BOOLEAN preserved");
        assert_eq!(
            e.data,
            u32_to_i32(0xffff_ffff),
            "boolean true data preserved"
        );
        assert_eq!(e.resource_id, 0, "a concrete value has no resource id");

        // A missing attribute is None (→ TYPE_NULL by the caller).
        assert!(
            resolve_theme_attr(&attrs, u32_to_i32(0x7f01_9999)).is_none(),
            "an attribute absent from the theme must be None, not a fabricated value"
        );
    }

    #[test]
    fn resolve_theme_attr_follows_theme_attribute_indirection_and_breaks_cycles() {
        // A `?attr/foo` (TYPE_ATTRIBUTE) value re-resolves against the SAME theme map (one hop), and a
        // self/loop reference is bounded (no infinite loop / panic — totality under panic=abort).
        use crate::framework::theme_registry::ThemeAttr;
        let mut attrs = std::collections::HashMap::new();
        let alias = u32_to_i32(0x7f01_0001);
        let target = u32_to_i32(0x7f01_0002);
        // alias = ?attr/target ; target = a concrete int 7.
        attrs.insert(
            alias,
            ThemeAttr {
                type_: TYPE_ATTRIBUTE,
                data: u32::from_ne_bytes(target.to_ne_bytes()),
            },
        );
        attrs.insert(
            target,
            ThemeAttr {
                type_: 0x10,
                data: 7,
            },
        );
        let e = resolve_theme_attr(&attrs, alias).expect("indirection resolves");
        assert_eq!(e.value_type, 0x10, "resolved to the target's concrete type");
        assert_eq!(e.data, 7, "resolved to the target's concrete data");

        // A self-referential ?attr cycle must terminate (bounded by MAX_ATTR_RESOLVE_DEPTH).
        let mut cyc = std::collections::HashMap::new();
        let a = u32_to_i32(0x7f01_00aa);
        cyc.insert(
            a,
            ThemeAttr {
                type_: TYPE_ATTRIBUTE,
                data: u32::from_ne_bytes(a.to_ne_bytes()),
            },
        );
        // Must return (not hang); the value stays the unresolved attribute reference.
        let e = resolve_theme_attr(&cyc, a).expect("cycle terminates with a value");
        assert_eq!(e.value_type, i32::from(TYPE_ATTRIBUTE));
    }

    #[test]
    fn resolve_theme_attributes_reads_a_registered_theme_and_is_total_on_bad_handles() {
        // The theme handle path: a registered theme's merged attrs resolve in request order; a
        // stale/fabricated handle yields all-None (every requested attr → TYPE_NULL), never UB.
        use crate::framework::theme_registry;
        let theme = theme_registry::allocate().expect("allocate theme");
        let attr_a = u32_to_i32(0x7f01_0058);
        let attr_b = u32_to_i32(0x7f01_00a9);
        theme_registry::with_theme(theme, |t| {
            t.attrs.insert(
                attr_a,
                theme_registry::ThemeAttr {
                    type_: 0x12,
                    data: 1,
                },
            );
        })
        .expect("populate theme");

        // attr_a present, attr_b absent → Some/None in request order.
        let out = resolve_theme_attributes(theme, &[attr_a, attr_b]);
        assert_eq!(out.len(), 2);
        assert!(out[0].is_some(), "registered attr resolves");
        assert!(out[1].is_none(), "unset attr is None (→ TYPE_NULL default)");

        // A fabricated handle yields all-None of the right length (no panic, no UB).
        let bogus = i64::MAX;
        let out = resolve_theme_attributes(bogus, &[attr_a, attr_b]);
        assert_eq!(out, vec![None, None]);

        theme_registry::free(theme).expect("free theme");
    }

    #[test]
    fn resolve_inline_theme_refs_resolves_attribute_values_against_the_theme() {
        // 2026-06-05 root-cause regression guard for multitouch.test's AppCompat ActionBar inflation:
        // an inline XML attribute whose value is a `?attr/foo` (TYPE_ATTRIBUTE) must be resolved
        // against the active theme before the framework reads it; otherwise TypedArray.getDrawable/
        // getColor throw `UnsupportedOperationException: Failed to resolve attribute at index N`.
        use crate::framework::theme_registry;
        let theme = theme_registry::allocate().expect("allocate theme");
        // The theme defines attr 0x7f010001 = a concrete int 42.
        let referenced_attr = 0x7f01_0001u32;
        theme_registry::with_theme(theme, |t| {
            t.attrs.insert(
                u32_to_i32(referenced_attr),
                theme_registry::ThemeAttr {
                    type_: 0x10, // TYPE_INT_DEC
                    data: 42,
                },
            );
        })
        .expect("populate theme");

        // Slot 0: an inline `?attr/0x7f010001` value (what resolve_xml_attributes records). Slot 1: an
        // already-concrete value (must be left untouched). Slot 2: None (must stay None).
        let mut entries = vec![
            Some(TypedEntry {
                value_type: i32::from(TYPE_ATTRIBUTE),
                data: u32_to_i32(referenced_attr),
                resource_id: u32_to_i32(referenced_attr),
                asset_cookie: 0,
            }),
            Some(TypedEntry {
                value_type: 0x1c, // TYPE_INT_COLOR_ARGB8 — concrete
                data: 0x1234_5678,
                resource_id: 0,
                asset_cookie: 0,
            }),
            None,
        ];
        resolve_inline_theme_refs(theme, &mut entries);

        let resolved = entries[0].expect("the ?attr value resolved against the theme");
        assert_eq!(
            resolved.value_type, 0x10,
            "resolved to the theme attr's type"
        );
        assert_eq!(resolved.data, 42, "resolved to the theme attr's data");
        assert_eq!(
            entries[1].expect("concrete slot untouched").data,
            0x1234_5678
        );
        assert!(entries[2].is_none(), "absent slot stays None");

        // An attribute the theme does not define is left as the unresolved reference (faithful
        // "not in theme" outcome, not a fabricated value).
        let mut undefined = vec![Some(TypedEntry {
            value_type: i32::from(TYPE_ATTRIBUTE),
            data: u32_to_i32(0x7f01_9999),
            resource_id: u32_to_i32(0x7f01_9999),
            asset_cookie: 0,
        })];
        resolve_inline_theme_refs(theme, &mut undefined);
        assert_eq!(
            undefined[0].expect("slot present").value_type,
            i32::from(TYPE_ATTRIBUTE),
            "an attr absent from the theme stays an unresolved reference"
        );

        theme_registry::free(theme).expect("free theme");
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

    #[test]
    fn log_native_name_sig_and_class_match_log_java() {
        // Pin android.util.Log.println_native's class, method name, and JNI descriptor against
        // `Log.java` (line 367) + the generated header `android_util_Log.h` (Signature
        // `(IILjava/lang/String;Ljava/lang/String;)I`): a transcription regression would make
        // RegisterNatives throw NoSuchMethodError at boot. Host-independent constants.
        assert_eq!(LOG_CLASS.to_str(), "android/util/Log");
        assert_eq!(PRINTLN_NATIVE_NAME.to_str(), "println_native");
        assert_eq!(
            PRINTLN_NATIVE_SIG.to_str(),
            "(IILjava/lang/String;Ljava/lang/String;)I"
        );
        // Buffer-ID upper bound mirrors ATL `util.h` log_id_t (LOG_ID_MAIN..LOG_ID_SYSTEM, then MAX).
        assert_eq!(LOG_ID_MAX, 4);
        // Priority constants mirror Log.java VERBOSE=2 … ASSERT=7.
        assert_eq!(LOG_PRIORITY_VERBOSE, 2);
        assert_eq!(LOG_PRIORITY_DEBUG, 3);
        assert_eq!(LOG_PRIORITY_INFO, 4);
        assert_eq!(LOG_PRIORITY_WARN, 5);
        assert_eq!(LOG_PRIORITY_ERROR, 6);
        assert_eq!(LOG_PRIORITY_ASSERT, 7);
    }

    #[test]
    fn asset_manager_init_name_sig_and_class_match_asset_manager_java() {
        // Pin android.content.res.AssetManager.init's class, method name, and JNI descriptor against
        // `AssetManager.java` line 779 (`private native final void init(int sdk_version);` → `(I)V`):
        // a transcription regression would make RegisterNatives throw NoSuchMethodError at boot, or
        // (worse) bind the wrong arity. Host-independent constants.
        assert_eq!(
            ASSET_MANAGER_CLASS.to_str(),
            "android/content/res/AssetManager"
        );
        assert_eq!(ASSET_MANAGER_INIT_NAME.to_str(), "init");
        assert_eq!(ASSET_MANAGER_INIT_SIG.to_str(), "(I)V");
        // native_setApkAssets bound signature-only (AssetManager denylisted) from the ART-reported
        // signature; pin name + descriptor so a transcription regression throws NoSuchMethodError.
        assert_eq!(
            ASSET_MANAGER_SET_APK_ASSETS_NAME.to_str(),
            "native_setApkAssets"
        );
        assert_eq!(
            ASSET_MANAGER_SET_APK_ASSETS_SIG.to_str(),
            "([Ljava/lang/Object;I)V"
        );
        // setConfiguration bound signature-only (AssetManager denylisted) from the ART-reported
        // signature; pin name + descriptor so a transcription regression throws NoSuchMethodError.
        assert_eq!(
            ASSET_MANAGER_SET_CONFIGURATION_NAME.to_str(),
            "setConfiguration"
        );
        assert_eq!(
            ASSET_MANAGER_SET_CONFIGURATION_SIG.to_str(),
            "(IILjava/lang/String;IIIIIIIIIIIIII)V"
        );
        // openXmlAssetNative bound signature-only (AssetManager denylisted) from the ART-reported
        // signature; pin name + descriptor so a transcription regression throws NoSuchMethodError.
        assert_eq!(
            ASSET_MANAGER_OPEN_XML_ASSET_NAME.to_str(),
            "openXmlAssetNative"
        );
        assert_eq!(
            ASSET_MANAGER_OPEN_XML_ASSET_SIG.to_str(),
            "(ILjava/lang/String;)J"
        );
        // retrieveAttributes bound signature-only (AssetManager denylisted) from the ART-reported
        // signature `(J[IIJJ)Z` (mangled `...retrieveAttributes__J_3IIJJ`); pin name + descriptor so
        // a transcription regression throws NoSuchMethodError at boot.
        assert_eq!(
            ASSET_MANAGER_RETRIEVE_ATTRIBUTES_NAME.to_str(),
            "retrieveAttributes"
        );
        assert_eq!(ASSET_MANAGER_RETRIEVE_ATTRIBUTES_SIG.to_str(), "(J[IIJJ)Z");
        // newTheme bound signature-only (AssetManager denylisted) from the ART-reported signature
        // `()J`; pin name + descriptor so a transcription regression throws NoSuchMethodError at boot.
        assert_eq!(ASSET_MANAGER_NEW_THEME_NAME.to_str(), "newTheme");
        assert_eq!(ASSET_MANAGER_NEW_THEME_SIG.to_str(), "()J");
        // applyThemeStyle bound signature-only (AssetManager denylisted) from the ART-reported
        // signature `(JIZ)V` (mangled `...__JIZ`); pin name + descriptor so a transcription
        // regression throws NoSuchMethodError at boot.
        assert_eq!(
            ASSET_MANAGER_APPLY_THEME_STYLE_NAME.to_str(),
            "applyThemeStyle"
        );
        assert_eq!(ASSET_MANAGER_APPLY_THEME_STYLE_SIG.to_str(), "(JIZ)V");
        // copyTheme bound signature-only (AssetManager denylisted) from the ART-reported signature
        // `(JJ)V` (mangled `...__JJ`); pin name + descriptor so a regression throws NoSuchMethodError.
        assert_eq!(ASSET_MANAGER_COPY_THEME_NAME.to_str(), "copyTheme");
        assert_eq!(ASSET_MANAGER_COPY_THEME_SIG.to_str(), "(JJ)V");
        // applyStyle bound signature-only (AssetManager denylisted) from the ART-reported signature
        // `(JJII[IIJJ)V` (mangled `...__JJII_3IIJJ`); pin name + descriptor so a transcription
        // regression throws NoSuchMethodError at boot.
        assert_eq!(ASSET_MANAGER_APPLY_STYLE_NAME.to_str(), "applyStyle");
        assert_eq!(ASSET_MANAGER_APPLY_STYLE_SIG.to_str(), "(JJII[IIJJ)V");
        // getResourceName bound signature-only (AssetManager denylisted) from the ART-reported
        // signature `(I)Ljava/lang/String;` (mangled `...__I`); pin name + descriptor.
        assert_eq!(
            ASSET_MANAGER_GET_RESOURCE_NAME_NAME.to_str(),
            "getResourceName"
        );
        assert_eq!(
            ASSET_MANAGER_GET_RESOURCE_NAME_SIG.to_str(),
            "(I)Ljava/lang/String;"
        );
        // loadResourceValue bound signature-only (AssetManager denylisted) from the ART-reported
        // signature `(ISLandroid/util/TypedValue;Z)I` (mangled `...__ISLandroid_util_TypedValue_2Z`);
        // pin name + descriptor. Also pin the TypedValue field constants it sets.
        assert_eq!(
            ASSET_MANAGER_LOAD_RESOURCE_VALUE_NAME.to_str(),
            "loadResourceValue"
        );
        assert_eq!(
            ASSET_MANAGER_LOAD_RESOURCE_VALUE_SIG.to_str(),
            "(ISLandroid/util/TypedValue;Z)I"
        );
        // loadThemeAttributeValue bound signature-only (AssetManager denylisted) from the ART-reported
        // signature `(JILandroid/util/TypedValue;Z)I` (mangled `...__JILandroid_util_TypedValue_2Z`,
        // run log 2026-06-05, accelerometerdemo's Theme.resolveAttribute); pin name + descriptor.
        assert_eq!(
            ASSET_MANAGER_LOAD_THEME_ATTRIBUTE_VALUE_NAME.to_str(),
            "loadThemeAttributeValue"
        );
        assert_eq!(
            ASSET_MANAGER_LOAD_THEME_ATTRIBUTE_VALUE_SIG.to_str(),
            "(JILandroid/util/TypedValue;Z)I"
        );
        assert_eq!(CHAR_SEQUENCE_SIG.to_str(), "Ljava/lang/CharSequence;");
        assert_eq!(RES_VALUE_TYPE_STRING, 0x03);
        assert_eq!(ECLIPSE_ASSET_COOKIE, 1);
        // Pin the run-confirmed AOSP TypedArray window layout the styled-attribute natives write
        // (corrected 2026-06-05: stride 7, TYPE@0, DATA@1, ASSET_COOKIE@2, RESOURCE_ID@3 — the
        // standard AOSP API 29+ layout, corroborated by the runtime `R.styleable.View_id`=9 read).
        // RESOURCE_ID@3 is what `getResourceId` returns for `android:id`; without it `findViewById`
        // returns null and `setText` NPEs in `MainActivity.onCreate`. A stride/offset regression
        // (which would re-break getResourceId/getString/getInteger and mis-place TypedValue entries)
        // fails loudly here.
        assert_eq!(STYLE_NUM_ENTRIES, 7);
        assert_eq!(STYLE_TYPE, 0);
        assert_eq!(STYLE_DATA, 1);
        assert_eq!(STYLE_ASSET_COOKIE, 2);
        assert_eq!(STYLE_RESOURCE_ID, 3);
        assert_eq!(TYPE_NULL, 0);
        assert_eq!(TYPE_REFERENCE, 0x01);
        assert_eq!(TYPE_ATTRIBUTE, 0x02);
        assert_eq!(TYPE_STRING, 0x03);
        assert_eq!(XML_BLOCK_COOKIE, -1);
    }

    #[test]
    fn fill_typed_array_writes_exact_bounds_values_and_indices() {
        // SOUNDNESS guard for the raw-pointer writes in the styled-attribute natives (no VM needed):
        // the writes must stay strictly inside the AOSP-sized buffers (n * STYLE_NUM_ENTRIES ints for
        // outValues, n + 1 for outIndices), write the accessor slots for each found attribute,
        // TYPE_NULL for each absent one, and pack outIndices[0]=count + the 1-based positions.
        //
        // Sentinel-bracketed buffers detect any out-of-bounds write: a leading + trailing guard cell
        // must keep its sentinel. entries: [string, absent, reference, absent] (mixed).
        let entries = [
            Some(TypedEntry {
                value_type: i32::from(TYPE_STRING),
                data: 0x18,
                resource_id: 0,
                asset_cookie: XML_BLOCK_COOKIE,
            }),
            None,
            Some(TypedEntry {
                value_type: i32::from(TYPE_REFERENCE),
                data: 0x7f03_0000,
                resource_id: 0x7f03_0000,
                asset_cookie: 0,
            }),
            None,
        ];
        let n = entries.len();
        let vals_len = n * STYLE_NUM_ENTRIES;
        let idx_len = n + 1;

        let mut values = vec![-1i32; vals_len + 2]; // [guard][n*7 values][guard]
        let mut indices = vec![-1i32; idx_len + 2]; // [guard][n+1 indices][guard]

        let v_ptr = values[1..1 + vals_len].as_mut_ptr() as jlong;
        let i_ptr = indices[1..1 + idx_len].as_mut_ptr() as jlong;
        fill_typed_array(v_ptr, i_ptr, &entries);

        // Guards untouched (no underflow / overflow write).
        assert_eq!(values[0], -1, "outValues underflow guard");
        assert_eq!(values[vals_len + 1], -1, "outValues overflow guard");
        assert_eq!(indices[0], -1, "outIndices underflow guard");
        assert_eq!(indices[idx_len + 1], -1, "outIndices overflow guard");

        // Found attributes (0 and 2): TYPE@0, DATA@1, COOKIE@2, RESOURCE_ID@3 are written; the
        // remaining slots stay at the caller value (the framework's zero pre-fill in real use).
        let written = [
            STYLE_TYPE,
            STYLE_DATA,
            STYLE_ASSET_COOKIE,
            STYLE_RESOURCE_ID,
        ];
        for (attr, e) in [(0usize, &entries[0]), (2usize, &entries[2])] {
            let win = 1 + attr * STYLE_NUM_ENTRIES;
            let e = e.unwrap();
            assert_eq!(values[win + STYLE_TYPE], e.value_type, "STYLE_TYPE @0");
            assert_eq!(values[win + STYLE_DATA], e.data, "STYLE_DATA @1");
            assert_eq!(
                values[win + STYLE_ASSET_COOKIE],
                e.asset_cookie,
                "STYLE_ASSET_COOKIE @2"
            );
            assert_eq!(
                values[win + STYLE_RESOURCE_ID],
                e.resource_id,
                "STYLE_RESOURCE_ID @3"
            );
            for slot in 0..STYLE_NUM_ENTRIES {
                if !written.contains(&slot) {
                    assert_eq!(values[win + slot], -1, "unwritten slot untouched");
                }
            }
        }
        // Absent attributes (1 and 3): only STYLE_TYPE @0 = TYPE_NULL written.
        for attr in [1usize, 3usize] {
            let win = 1 + attr * STYLE_NUM_ENTRIES;
            assert_eq!(values[win + STYLE_TYPE], TYPE_NULL, "absent → TYPE_NULL @0");
            for slot in 0..STYLE_NUM_ENTRIES {
                if slot != STYLE_TYPE {
                    assert_eq!(values[win + slot], -1, "absent: other slots untouched");
                }
            }
        }

        // outIndices: [0] = count of found (2); [1..=2] = 1-based positions (1 and 3).
        assert_eq!(indices[1], 2, "outIndices[0] = number found");
        assert_eq!(indices[2], 1, "first found at request position 1 (1-based)");
        assert_eq!(
            indices[3], 3,
            "second found at request position 3 (1-based)"
        );
        assert_eq!(indices[1 + 3], -1, "outIndices beyond count untouched");
    }

    #[test]
    fn fill_typed_array_reference_resource_id_is_at_style_resource_id_slot() {
        // REGRESSION for the findViewById fix (2026-06-05): a REFERENCE-typed `android:id` must place
        // its referenced id in the STYLE_RESOURCE_ID slot, which is what `TypedArray.getResourceId`
        // reads to set the view's id. Pin offset = View_id-style window + STYLE_RESOURCE_ID, so a layout
        // regression that mis-places it (re-breaking findViewById/setText) fails loudly here.
        let id = 0x7f03_0000i32;
        let entries = [Some(TypedEntry {
            value_type: i32::from(TYPE_REFERENCE),
            data: id,
            resource_id: id,
            asset_cookie: 0,
        })];
        let mut values = vec![0i32; STYLE_NUM_ENTRIES];
        let v_ptr = values.as_mut_ptr() as jlong;
        fill_typed_array(v_ptr, 0, &entries);
        assert_eq!(values[STYLE_TYPE], i32::from(TYPE_REFERENCE));
        assert_eq!(
            values[STYLE_RESOURCE_ID], id,
            "getResourceId reads the referenced id from STYLE_RESOURCE_ID"
        );
    }

    #[test]
    fn fill_typed_array_null_pointers_are_a_no_op() {
        // A 0 ("no buffer") pointer for either output must be skipped — never dereferenced. A
        // non-empty entries slice ensures the loop body would run if the guard were missing.
        let entries = [Some(TypedEntry {
            value_type: i32::from(TYPE_STRING),
            data: 1,
            resource_id: 0,
            asset_cookie: XML_BLOCK_COOKIE,
        })];
        fill_typed_array(0, 0, &entries);
    }

    #[test]
    fn fill_typed_array_zero_attrs_writes_only_changed_count() {
        // n == 0: outValues has no windows; outIndices is a single int (the count) set to 0.
        let mut indices = [-1i32; 3]; // [guard][count][guard]
        let i_ptr = indices[1..2].as_mut_ptr() as jlong;
        fill_typed_array(0, i_ptr, &[]);
        assert_eq!(indices[0], -1, "underflow guard untouched");
        assert_eq!(indices[1], 0, "outIndices[0] = 0 with zero attrs");
        assert_eq!(indices[2], -1, "overflow guard untouched");
    }

    #[test]
    fn u32_to_i32_preserves_all_bits() {
        // The Res_value.data word must be stored bit-for-bit (the framework reads back the same 32
        // bits); spot-check the boundary values incl. the 0xffffffff bool-true the manifest uses.
        for &v in &[0u32, 1, 0x7fff_ffff, 0x8000_0000, 0xffff_ffff, 0x0101_0003] {
            assert_eq!(u32_to_i32(v).to_ne_bytes(), v.to_ne_bytes());
        }
    }

    #[test]
    fn xml_block_native_names_sigs_and_class_match_art_reported() {
        // Pin android.content.res.XmlBlock's parser-native class, method names, and JNI descriptors
        // against the exact signatures ART reported missing (run log 2026-06-05) and the standard
        // AOSP XmlBlock parser ABI. A transcription regression would make RegisterNatives throw
        // NoSuchMethodError at boot (or bind a wrong arity). Host-independent constants.
        assert_eq!(XML_BLOCK_CLASS.to_str(), "android/content/res/XmlBlock");
        assert_eq!(
            XML_BLOCK_CREATE_PARSE_STATE_NAME.to_str(),
            "nativeCreateParseState"
        );
        assert_eq!(XML_BLOCK_CREATE_PARSE_STATE_SIG.to_str(), "(J)J");
        assert_eq!(XML_BLOCK_NEXT_NAME.to_str(), "nativeNext");
        assert_eq!(XML_BLOCK_NEXT_SIG.to_str(), "(J)I");
        assert_eq!(
            XML_BLOCK_DESTROY_PARSE_STATE_NAME.to_str(),
            "nativeDestroyParseState"
        );
        assert_eq!(XML_BLOCK_DESTROY_PARSE_STATE_SIG.to_str(), "(J)V");
        assert_eq!(XML_BLOCK_GET_NAME_NAME.to_str(), "nativeGetName");
        assert_eq!(XML_BLOCK_GET_NAME_SIG.to_str(), "(J)Ljava/lang/String;");
        assert_eq!(XML_BLOCK_DESTROY_NAME.to_str(), "nativeDestroy");
        assert_eq!(XML_BLOCK_DESTROY_SIG.to_str(), "(J)V");
        assert_eq!(
            XML_BLOCK_GET_ATTR_INDEX_NAME.to_str(),
            "nativeGetAttributeIndex"
        );
        assert_eq!(
            XML_BLOCK_GET_ATTR_INDEX_SIG.to_str(),
            "(JLjava/lang/String;Ljava/lang/String;)I"
        );
        assert_eq!(
            XML_BLOCK_GET_ATTR_STRING_VALUE_NAME.to_str(),
            "nativeGetAttributeStringValue"
        );
        assert_eq!(
            XML_BLOCK_GET_ATTR_STRING_VALUE_SIG.to_str(),
            "(JI)Ljava/lang/String;"
        );
        // nativeGetAttributeCount: `(J)I` — surfaced 2026-06-05 by AppCompatColorStateListInflater.
        assert_eq!(
            XML_BLOCK_GET_ATTR_COUNT_NAME.to_str(),
            "nativeGetAttributeCount"
        );
        assert_eq!(XML_BLOCK_GET_ATTR_COUNT_SIG.to_str(), "(J)I");
        // nativeGetAttributeResource: `(JI)I` — the attribute NAME's resource id
        // (getAttributeNameResource), surfaced 2026-06-05 by AppCompatColorStateListInflater.
        assert_eq!(
            XML_BLOCK_GET_ATTR_RESOURCE_NAME.to_str(),
            "nativeGetAttributeResource"
        );
        assert_eq!(XML_BLOCK_GET_ATTR_RESOURCE_SIG.to_str(), "(JI)I");
        // nativeGetAttributeDataType/Data: `(JI)I` — surfaced 2026-06-05 by VectorDrawableCompat
        // reading <vector>/<path> attribute types+values via AttributeSet/TypedArrayUtils.
        assert_eq!(
            XML_BLOCK_GET_ATTR_DATA_TYPE_NAME.to_str(),
            "nativeGetAttributeDataType"
        );
        assert_eq!(XML_BLOCK_GET_ATTR_DATA_TYPE_SIG.to_str(), "(JI)I");
        assert_eq!(
            XML_BLOCK_GET_ATTR_DATA_NAME.to_str(),
            "nativeGetAttributeData"
        );
        assert_eq!(XML_BLOCK_GET_ATTR_DATA_SIG.to_str(), "(JI)I");
        // nativeGetLineNumber: `(J)I`, returns -1 (axml does not track source lines).
        assert_eq!(
            XML_BLOCK_GET_LINE_NUMBER_NAME.to_str(),
            "nativeGetLineNumber"
        );
        assert_eq!(XML_BLOCK_GET_LINE_NUMBER_SIG.to_str(), "(J)I");
        assert_eq!(
            XML_BLOCK_GET_POOLED_STRING_NAME.to_str(),
            "nativeGetPooledString"
        );
        assert_eq!(
            XML_BLOCK_GET_POOLED_STRING_SIG.to_str(),
            "(JI)Ljava/lang/String;"
        );
        assert_eq!(XML_LINE_UNKNOWN, -1);
        // XmlPullParser event constants nativeNext maps to (stable public API).
        assert_eq!(XML_EVENT_END_DOCUMENT, 1);
        assert_eq!(XML_EVENT_START_TAG, 2);
        assert_eq!(XML_EVENT_END_TAG, 3);
        assert_eq!(XML_EVENT_TEXT, 4);
        assert_eq!(XML_ATTR_NOT_FOUND, -1);
    }

    #[test]
    fn environment_native_name_sig_and_class_match_environment_java() {
        // Pin android.os.Environment.native_get_app_data_dir's class, method name, and JNI descriptor
        // against `Environment.java` line 336 (`private static native String native_get_app_data_dir();`
        // → `()Ljava/lang/String;`): a transcription regression would make RegisterNatives throw
        // NoSuchMethodError at boot. Host-independent constants.
        assert_eq!(ENVIRONMENT_CLASS.to_str(), "android/os/Environment");
        assert_eq!(GET_APP_DATA_DIR_NAME.to_str(), "native_get_app_data_dir");
        assert_eq!(GET_APP_DATA_DIR_SIG.to_str(), "()Ljava/lang/String;");
    }

    #[test]
    fn system_clock_native_name_sig_and_class_match_system_clock_java() {
        // Pin android.os.SystemClock.elapsedRealtime's class, method name, and JNI descriptor against
        // `SystemClock.java` line 148 (`native public static long elapsedRealtime();` → `()J`) and the
        // ART `No implementation found for long android.os.SystemClock.elapsedRealtime()` line: a
        // transcription regression would make RegisterNatives throw NoSuchMethodError at boot (or the
        // native go unbound, re-throwing the UnsatisfiedLinkError that this binding cleared).
        // Host-independent constants.
        assert_eq!(SYSTEM_CLOCK_CLASS.to_str(), "android/os/SystemClock");
        assert_eq!(ELAPSED_REALTIME_NAME.to_str(), "elapsedRealtime");
        assert_eq!(ELAPSED_REALTIME_SIG.to_str(), "()J");
        // uptimeMillis() → `()J`, surfaced 2026-06-05 by Handler.postDelayed; same monotonic source.
        assert_eq!(UPTIME_MILLIS_NAME.to_str(), "uptimeMillis");
        assert_eq!(UPTIME_MILLIS_SIG.to_str(), "()J");
    }

    #[test]
    fn runtime_native_load_name_sig_and_class_match_art() {
        // Pin java.lang.Runtime.nativeLoad's class, method, and JNI descriptor (the API-26+ libcore
        // form `nativeLoad(String, ClassLoader, Class) -> String`, whose `Class caller` matches
        // JavaVMExt::LoadNativeLibrary's `jclass caller_class`). A drift would make RegisterNatives
        // throw (best-effort: the boot log then dumps Runtime's method table). Host-independent.
        assert_eq!(RUNTIME_CLASS.to_str(), "java/lang/Runtime");
        assert_eq!(NATIVE_LOAD_NAME.to_str(), "nativeLoad");
        assert_eq!(
            NATIVE_LOAD_SIG.to_str(),
            "(Ljava/lang/String;Ljava/lang/ClassLoader;Ljava/lang/Class;)Ljava/lang/String;"
        );
        // The delegation target: the exact mangled `art::JavaVMExt::LoadNativeLibrary` exported by
        // libart.so (verified `nm -D`), NUL-terminated for `dlsym`. A typo silently disables
        // delegation (non-pre-loaded libs would then report a load failure, not crash).
        assert!(ART_LOAD_NATIVE_LIBRARY_SYMBOL.starts_with(b"_ZN3art9JavaVMExt17LoadNativeLibrary"));
        assert_eq!(*ART_LOAD_NATIVE_LIBRARY_SYMBOL.last().unwrap(), 0u8);
    }

    #[test]
    fn soname_from_load_path_returns_the_basename() {
        // The consult derives the soname from the path ART resolved, to match the engine pre-load
        // registry. Full path → basename; a bare soname → itself; no slash → itself. Stays total
        // (no panic) on degenerate inputs.
        assert_eq!(
            soname_from_load_path("/home/u/.cache/eclipse/native-libs/libzstd-jni-1.5.7-6.so"),
            "libzstd-jni-1.5.7-6.so"
        );
        assert_eq!(soname_from_load_path("libroblox.so"), "libroblox.so");
        assert_eq!(
            soname_from_load_path("/usr/lib/libwolfssljni.so"),
            "libwolfssljni.so"
        );
        assert_eq!(soname_from_load_path(""), "");
        assert_eq!(soname_from_load_path("/a/b/"), "");
    }

    #[test]
    fn asset_manager_get_resource_package_name_name_sig_match_art_reported() {
        // Pin AssetManager.getResourcePackageName's name + JNI descriptor against the exact signature
        // ART reported missing (`No implementation found for java.lang.String
        // android.content.res.AssetManager.getResourcePackageName(int)`, mangled `...__I`, run log
        // 2026-06-11 from FirebaseInitProvider.onCreate). A drift re-throws the UnsatisfiedLinkError.
        assert_eq!(
            ASSET_MANAGER_GET_RESOURCE_PACKAGE_NAME_NAME.to_str(),
            "getResourcePackageName"
        );
        assert_eq!(
            ASSET_MANAGER_GET_RESOURCE_PACKAGE_NAME_SIG.to_str(),
            "(I)Ljava/lang/String;"
        );
    }

    #[test]
    fn asset_manager_get_resource_identifier_name_sig_match_art_reported() {
        // Pin AssetManager.getResourceIdentifier's name + JNI descriptor against the exact signature
        // ART reported missing (`No implementation found for int
        // android.content.res.AssetManager.getResourceIdentifier(java.lang.String, java.lang.String,
        // java.lang.String)`, run log 2026-06-11 from FirebaseInitProvider.onCreate).
        assert_eq!(
            ASSET_MANAGER_GET_RESOURCE_IDENTIFIER_NAME.to_str(),
            "getResourceIdentifier"
        );
        assert_eq!(
            ASSET_MANAGER_GET_RESOURCE_IDENTIFIER_SIG.to_str(),
            "(Ljava/lang/String;Ljava/lang/String;Ljava/lang/String;)I"
        );
    }

    #[test]
    fn asset_manager_stream_native_names_and_sigs_are_the_classic_aosp_set() {
        // openAsset's signature is confirmed from the ART-reported line; the read cycle uses the
        // classic AOSP signatures (bound best-effort). Pin them so a transcription drift is caught.
        assert_eq!(ASSET_MANAGER_OPEN_ASSET_NAME.to_str(), "openAsset");
        assert_eq!(
            ASSET_MANAGER_OPEN_ASSET_SIG.to_str(),
            "(Ljava/lang/String;I)J"
        );
        assert_eq!(ASSET_MANAGER_READ_ASSET_NAME.to_str(), "readAsset");
        // ATL's readAsset uses long off/len (run log 2026-06-11), not the classic AOSP int/int.
        assert_eq!(ASSET_MANAGER_READ_ASSET_SIG.to_str(), "(J[BJJ)I");
        assert_eq!(ASSET_MANAGER_SEEK_ASSET_NAME.to_str(), "seekAsset");
        assert_eq!(ASSET_MANAGER_SEEK_ASSET_SIG.to_str(), "(JJI)J");
        assert_eq!(
            ASSET_MANAGER_GET_ASSET_LENGTH_NAME.to_str(),
            "getAssetLength"
        );
        assert_eq!(ASSET_MANAGER_GET_ASSET_LENGTH_SIG.to_str(), "(J)J");
        assert_eq!(
            ASSET_MANAGER_GET_ASSET_REMAINING_LENGTH_NAME.to_str(),
            "getAssetRemainingLength"
        );
        assert_eq!(ASSET_MANAGER_DESTROY_ASSET_NAME.to_str(), "destroyAsset");
        assert_eq!(ASSET_MANAGER_DESTROY_ASSET_SIG.to_str(), "(J)V");
    }

    #[test]
    fn resolve_resource_identifier_parses_name_forms_and_returns_zero_when_unresolvable() {
        // Pure parse/fallback logic (no ARSC needed for the not-found paths): an empty entry → 0; a
        // name with no type and no defType → 0. The real reverse lookup is covered by an arsc test;
        // here we pin that the AOSP `[package:][type/]entry` parsing + 0-fallback never panics.
        assert_eq!(resolve_resource_identifier("", "string", "com.x"), 0);
        assert_eq!(resolve_resource_identifier("foo", "", ""), 0); // no type → not identifiable
        assert_eq!(resolve_resource_identifier("type/", "", ""), 0); // empty entry → 0
    }

    #[test]
    fn message_queue_native_name_sig_and_class_match_art_reported() {
        // Pin android.os.MessageQueue.nativeInit's class, method name, and JNI descriptor against the
        // exact signature ART reported missing (`No implementation found for long
        // android.os.MessageQueue.nativeInit()`, run log 2026-06-05) + AOSP `MessageQueue.java`'s
        // `private native long nativeInit();`: a transcription regression would make RegisterNatives
        // throw NoSuchMethodError at step 0 (or the native go unbound, re-throwing the
        // UnsatisfiedLinkError that blocked Looper.prepareMainLooper). Host-independent constants.
        assert_eq!(MESSAGE_QUEUE_CLASS.to_str(), "android/os/MessageQueue");
        assert_eq!(MESSAGE_QUEUE_NATIVE_INIT_NAME.to_str(), "nativeInit");
        assert_eq!(MESSAGE_QUEUE_NATIVE_INIT_SIG.to_str(), "()J");
        // The Looper class whose static prepareMainLooper() (step 0) builds the MessageQueue.
        assert_eq!(LOOPER_CLASS.to_str(), "android/os/Looper");
        // Java's MessageQueue.<init> requires mPtr != 0; the sentinel must be non-zero (and is a
        // plainly-non-pointer marker, never dereferenced — see register_message_queue_natives docs).
        assert_ne!(MESSAGE_QUEUE_HANDLE_SENTINEL, 0);
    }

    #[test]
    fn sensor_manager_native_name_sig_and_class_match_art_reported() {
        // Pin android.hardware.SensorManager.register_accelerometer_listener_native's class, method
        // name, and JNI descriptor against the exact signature ART reported missing (`No implementation
        // found for void android.hardware.SensorManager.register_accelerometer_listener_native(
        // android.hardware.SensorEventListener, android.hardware.Sensor, int)`, run log 2026-06-05): a
        // transcription regression would make RegisterNatives throw NoSuchMethodError, or the native go
        // unbound and re-throw the UnsatisfiedLinkError that blocked accelerometerdemo's onCreate.
        // Host-independent constants.
        assert_eq!(
            SENSOR_MANAGER_CLASS.to_str(),
            "android/hardware/SensorManager"
        );
        assert_eq!(
            SENSOR_MANAGER_REGISTER_NAME.to_str(),
            "register_accelerometer_listener_native"
        );
        assert_eq!(
            SENSOR_MANAGER_REGISTER_SIG.to_str(),
            "(Landroid/hardware/SensorEventListener;Landroid/hardware/Sensor;I)V"
        );
    }

    #[test]
    fn monotonic_anchor_clock_is_non_decreasing() {
        // The elapsedRealtime contract guarantees monotonicity (SystemClock.java lines 52–56). Prove
        // the process-anchored clock never goes backwards across calls. Host-independent (uses only
        // the monotonic Instant anchor, no JNI). Anchor on first read, then read again.
        let anchor = MONOTONIC_ANCHOR.get_or_init(Instant::now);
        let first = anchor.elapsed().as_millis();
        let second = anchor.elapsed().as_millis();
        assert!(
            second >= first,
            "elapsedRealtime must be monotonic: {first} then {second}"
        );
    }

    #[test]
    fn view_native_names_sigs_and_class_match_view_java() {
        // Pin android.view.View's peer-native class, method name, and JNI descriptor against
        // `View.java` line 1166 (`protected native long native_constructor(Context context,
        // AttributeSet attrs);` → `(Landroid/content/Context;Landroid/util/AttributeSet;)J`) and the
        // exact signature ART reported missing (run log 2026-06-05): a transcription regression would
        // make RegisterNatives throw NoSuchMethodError at boot. Host-independent constants.
        assert_eq!(VIEW_CLASS.to_str(), "android/view/View");
        assert_eq!(VIEW_NATIVE_CONSTRUCTOR_NAME.to_str(), "native_constructor");
        assert_eq!(
            VIEW_NATIVE_CONSTRUCTOR_SIG.to_str(),
            "(Landroid/content/Context;Landroid/util/AttributeSet;)J"
        );
        // native_setPadding: View.java line 1310 → `(JIIII)V`.
        assert_eq!(VIEW_NATIVE_SET_PADDING_NAME.to_str(), "native_setPadding");
        assert_eq!(VIEW_NATIVE_SET_PADDING_SIG.to_str(), "(JIIII)V");
        // native_setLayoutParams: View.java line 1167 → `(JIIIFIIII)V`.
        assert_eq!(
            VIEW_NATIVE_SET_LAYOUT_PARAMS_NAME.to_str(),
            "native_setLayoutParams"
        );
        assert_eq!(VIEW_NATIVE_SET_LAYOUT_PARAMS_SIG.to_str(), "(JIIIFIIII)V");
        // native_requestLayout: View.java line 1175 → `(J)V`.
        assert_eq!(
            VIEW_NATIVE_REQUEST_LAYOUT_NAME.to_str(),
            "native_requestLayout"
        );
        assert_eq!(VIEW_NATIVE_REQUEST_LAYOUT_SIG.to_str(), "(J)V");
        // native_setBackgroundDrawable: the ART No-implementation-found line → `(JJ)V`.
        assert_eq!(
            VIEW_NATIVE_SET_BACKGROUND_DRAWABLE_NAME.to_str(),
            "native_setBackgroundDrawable"
        );
        assert_eq!(VIEW_NATIVE_SET_BACKGROUND_DRAWABLE_SIG.to_str(), "(JJ)V");
        // native_setVisibility: the ART No-implementation-found line → `(JIF)V` (long widget, int
        // visibility, float alpha), surfaced 2026-06-05 by AppCompat sub-decor View.<init>.
        assert_eq!(
            VIEW_NATIVE_SET_VISIBILITY_NAME.to_str(),
            "native_setVisibility"
        );
        assert_eq!(VIEW_NATIVE_SET_VISIBILITY_SIG.to_str(), "(JIF)V");
        // nativeSetOnClickListener(long) → `(J)V`, surfaced 2026-06-05 by multitouch.test's custom View
        // (same native already bound on ImageButton; reuses the class-agnostic handler).
        assert_eq!(
            VIEW_SET_ON_CLICK_LISTENER_NAME.to_str(),
            "nativeSetOnClickListener"
        );
        assert_eq!(VIEW_SET_ON_CLICK_LISTENER_SIG.to_str(), "(J)V");
        // native_setBackgroundColor(long, int) → `(JI)V`, surfaced 2026-06-05 by multitouch.test
        // (records the ARGB background on the view_registry peer). Pinned to the ART-reported sig.
        assert_eq!(
            VIEW_SET_BACKGROUND_COLOR_NAME.to_str(),
            "native_setBackgroundColor"
        );
        assert_eq!(VIEW_SET_BACKGROUND_COLOR_SIG.to_str(), "(JI)V");
        // The View.widget field (the view_registry handle on `this`) instance natives read.
        assert_eq!(VIEW_WIDGET_FIELD_NAME.to_str(), "widget");
        assert_eq!(VIEW_WIDGET_FIELD_SIG.to_str(), "J");
        // TextView re-declares native_constructor (same signature); pin its class internal name.
        assert_eq!(TEXT_VIEW_CLASS.to_str(), "android/widget/TextView");
        // TextView.native_setText: TextView.java line 111 → instance `(Ljava/lang/String;)V`.
        assert_eq!(TEXT_VIEW_NATIVE_SET_TEXT_NAME.to_str(), "native_setText");
        assert_eq!(
            TEXT_VIEW_NATIVE_SET_TEXT_SIG.to_str(),
            "(Ljava/lang/String;)V"
        );
    }

    #[test]
    fn window_native_names_sigs_and_class_match_window_java() {
        // Pin android.view.Window's window-setup native class, method names, and JNI descriptors
        // against `Window.java` lines 184–188 (set_jobject is static; the rest are instance) and the
        // exact signatures ART reported missing (run log 2026-06-05): a transcription regression would
        // make RegisterNatives throw NoSuchMethodError at boot. Host-independent constants.
        assert_eq!(WINDOW_CLASS.to_str(), "android/view/Window");
        assert_eq!(WINDOW_SET_JOBJECT_NAME.to_str(), "set_jobject");
        assert_eq!(WINDOW_SET_JOBJECT_SIG.to_str(), "(JLandroid/view/Window;)V");
        assert_eq!(WINDOW_SET_TITLE_NAME.to_str(), "set_title");
        assert_eq!(WINDOW_SET_TITLE_SIG.to_str(), "(JLjava/lang/String;)V");
        assert_eq!(WINDOW_SET_LAYOUT_NAME.to_str(), "set_layout");
        assert_eq!(WINDOW_SET_LAYOUT_SIG.to_str(), "(JII)V");
        assert_eq!(
            WINDOW_SET_WIDGET_AS_ROOT_NAME.to_str(),
            "set_widget_as_root"
        );
        assert_eq!(WINDOW_SET_WIDGET_AS_ROOT_SIG.to_str(), "(JJ)V");
        assert_eq!(
            WINDOW_REMOVE_GTK_BACKGROUND_NAME.to_str(),
            "remove_gtk_background"
        );
        assert_eq!(WINDOW_REMOVE_GTK_BACKGROUND_SIG.to_str(), "(J)V");
    }

    #[test]
    fn paint_native_name_sig_and_class_match_art_reported() {
        // Pin android.graphics.Paint.native_create's class, method name, and JNI descriptor against
        // the exact signature ART reported missing (run log 2026-06-05): a transcription regression
        // would make RegisterNatives throw NoSuchMethodError at boot. Host-independent constants.
        assert_eq!(PAINT_CLASS.to_str(), "android/graphics/Paint");
        assert_eq!(PAINT_NATIVE_CREATE_NAME.to_str(), "native_create");
        assert_eq!(PAINT_NATIVE_CREATE_SIG.to_str(), "()J");
        // native_set_color(long, int) — surfaced 2026-06-05 by ColorDrawable.<init> → Paint.setColor.
        assert_eq!(PAINT_NATIVE_SET_COLOR_NAME.to_str(), "native_set_color");
        assert_eq!(PAINT_NATIVE_SET_COLOR_SIG.to_str(), "(JI)V");
        // native_set_stroke_width(long, float) — surfaced 2026-06-05 by multitouch.test's custom-View
        // Paint.setStrokeWidth. Pinned to the exact ART-reported descriptor.
        assert_eq!(
            PAINT_NATIVE_SET_STROKE_WIDTH_NAME.to_str(),
            "native_set_stroke_width"
        );
        assert_eq!(PAINT_NATIVE_SET_STROKE_WIDTH_SIG.to_str(), "(JF)V");
        // native_set_style(long, int) — surfaced 2026-06-05 by multitouch.test's custom-View
        // Paint.setStyle. Pinned to the exact ART-reported descriptor.
        assert_eq!(PAINT_NATIVE_SET_STYLE_NAME.to_str(), "native_set_style");
        assert_eq!(PAINT_NATIVE_SET_STYLE_SIG.to_str(), "(JI)V");
        // native_set_text_size(long, float) — surfaced 2026-06-05 by multitouch.test's custom-View
        // Paint.setTextSize. Pinned to the exact ART-reported descriptor.
        assert_eq!(
            PAINT_NATIVE_SET_TEXT_SIZE_NAME.to_str(),
            "native_set_text_size"
        );
        assert_eq!(PAINT_NATIVE_SET_TEXT_SIZE_SIG.to_str(), "(JF)V");
    }

    #[test]
    fn matrix_native_name_sig_and_class_match_art_reported() {
        // Pin android.graphics.Matrix.native_create's class, method name, and JNI descriptor against
        // the exact signature ART reported missing (`No implementation found for long
        // android.graphics.Matrix.native_create(long)`, run log 2026-06-05, accelerometerdemo): a
        // transcription regression would make RegisterNatives throw NoSuchMethodError when AppCompat's
        // VectorDrawableCompat constructs a Matrix. The arg is the source Matrix native handle (0 =
        // identity). Host-independent constants.
        assert_eq!(MATRIX_CLASS.to_str(), "android/graphics/Matrix");
        assert_eq!(MATRIX_NATIVE_CREATE_NAME.to_str(), "native_create");
        assert_eq!(MATRIX_NATIVE_CREATE_SIG.to_str(), "(J)J");
        // finalizer(long) → `(J)V`, surfaced 2026-06-05; frees the matrix_registry slot.
        assert_eq!(MATRIX_FINALIZER_NAME.to_str(), "finalizer");
        assert_eq!(MATRIX_FINALIZER_SIG.to_str(), "(J)V");
    }

    #[test]
    fn path_native_names_sigs_and_class_match_art_reported() {
        // Pin android.graphics.Path's class + the geometry natives' names/descriptors against the exact
        // signatures ART reported missing during AdaptiveIconDemo's adaptive-icon path build (run logs
        // 2026-06-05: `native_create_builder(long, long)`, then `native_move_to(long, float, float)`,
        // then `native_create_path(long)`, then `native_ref_path(long)`). A transcription regression
        // would make RegisterNatives throw NoSuchMethodError when PathParser builds the mask path. The
        // line/quad/cubic/close ops follow the same builder-op pattern (the loop bound them with no new
        // UnsatisfiedLinkError). Host-independent constants.
        assert_eq!(PATH_CLASS.to_str(), "android/graphics/Path");
        assert_eq!(
            PATH_NATIVE_CREATE_BUILDER_NAME.to_str(),
            "native_create_builder"
        );
        assert_eq!(PATH_NATIVE_CREATE_BUILDER_SIG.to_str(), "(JJ)J");
        assert_eq!(PATH_NATIVE_MOVE_TO_NAME.to_str(), "native_move_to");
        assert_eq!(PATH_NATIVE_MOVE_TO_SIG.to_str(), "(JFF)V");
        assert_eq!(PATH_NATIVE_LINE_TO_NAME.to_str(), "native_line_to");
        assert_eq!(PATH_NATIVE_LINE_TO_SIG.to_str(), "(JFF)V");
        assert_eq!(PATH_NATIVE_QUAD_TO_NAME.to_str(), "native_quad_to");
        assert_eq!(PATH_NATIVE_QUAD_TO_SIG.to_str(), "(JFFFF)V");
        assert_eq!(PATH_NATIVE_CUBIC_TO_NAME.to_str(), "native_cubic_to");
        assert_eq!(PATH_NATIVE_CUBIC_TO_SIG.to_str(), "(JFFFFFF)V");
        assert_eq!(PATH_NATIVE_CLOSE_NAME.to_str(), "native_close");
        assert_eq!(PATH_NATIVE_CLOSE_SIG.to_str(), "(J)V");
        assert_eq!(PATH_NATIVE_CREATE_PATH_NAME.to_str(), "native_create_path");
        assert_eq!(PATH_NATIVE_CREATE_PATH_SIG.to_str(), "(J)J");
        assert_eq!(PATH_NATIVE_REF_PATH_NAME.to_str(), "native_ref_path");
        assert_eq!(PATH_NATIVE_REF_PATH_SIG.to_str(), "(J)J");
    }

    #[test]
    fn canvas_native_names_and_sigs() {
        // Pin android.graphics.Canvas's class + the draw natives' names/descriptors. These are the
        // modern-AOSP `BaseCanvas` draw-native set bound static-with-handle (see the provenance note at
        // register_canvas_natives); the dev-host discovery loop confirms/corrects them on a real run.
        // A transcription regression here would silently bind the wrong descriptor — this catches it.
        assert_eq!(CANVAS_CLASS.to_str(), "android/graphics/Canvas");
        assert_eq!(CANVAS_N_DRAW_COLOR_NAME.to_str(), "nDrawColor");
        assert_eq!(CANVAS_N_DRAW_COLOR_SIG.to_str(), "(JI)V");
        assert_eq!(CANVAS_N_DRAW_RECT_NAME.to_str(), "nDrawRect");
        assert_eq!(CANVAS_N_DRAW_RECT_SIG.to_str(), "(JFFFFJ)V");
        assert_eq!(CANVAS_N_DRAW_CIRCLE_NAME.to_str(), "nDrawCircle");
        assert_eq!(CANVAS_N_DRAW_CIRCLE_SIG.to_str(), "(JFFFJ)V");
        assert_eq!(CANVAS_N_DRAW_PATH_NAME.to_str(), "nDrawPath");
        assert_eq!(CANVAS_N_DRAW_PATH_SIG.to_str(), "(JJJ)V");
    }

    #[test]
    fn paint_config_from_handle_reads_paint_then_defaults_when_invalid() {
        // The draw cascade snapshots a paint_registry handle into a canvas_registry PaintConfig. A real
        // handle reflects the recorded color/style/stroke-width; a bad/0 handle yields AOSP's default
        // Paint (opaque black, fill) so a draw with an unseen default Paint is still real, never UB.
        let p = paint_registry::allocate().expect("allocate paint");
        paint_registry::with_paint(p, |s| {
            s.color = 0x80AB_CDEFu32 as i32;
            s.style = paint_registry::PaintStyle::Stroke;
            s.stroke_width = 3.5;
        })
        .expect("configure paint");
        let cfg = paint_config_from_handle(p);
        assert_eq!(cfg.argb, 0x80AB_CDEFu32 as i32);
        assert_eq!(cfg.style, paint_registry::PaintStyle::Stroke);
        assert_eq!(cfg.stroke_width, 3.5);
        paint_registry::free(p).expect("free paint");
        // A fabricated/0 handle falls back to the default Paint (opaque black, fill).
        let def = paint_config_from_handle(0);
        assert_eq!(def.argb, canvas_registry::PaintConfig::default().argb);
        assert_eq!(def.style, paint_registry::PaintStyle::Fill);
    }

    #[test]
    fn draw_target_and_drawn_canvas_are_plain_copy_values() {
        // DrawTarget/DrawnCanvas carry only opaque handles + pixel dims across the draw-cascade boundary
        // (GPU-free, Copy). This pins their field meaning so the renderer + framework agree.
        let t = DrawTarget {
            handle: 42,
            width: 100,
            height: 50,
        };
        let t2 = t; // Copy, not move.
        assert_eq!(t, t2);
        assert_eq!(t.width, 100);
        assert_eq!(t.height, 50);
        let d = DrawnCanvas {
            view: 42,
            canvas: 7,
        };
        assert_eq!(d.view, t.handle);
        assert_eq!(d.canvas, 7);
    }

    #[test]
    fn image_view_class_is_slashed_internal_name() {
        // Pin android.widget.ImageView's internal name: ImageView re-declares native_constructor (same
        // signature as View/TextView, surfaced by the run line 2026-06-05), and ART resolves natives per
        // declaring class, so register_image_view_natives must find the class by this exact name or
        // RegisterNatives throws NoClassDefFoundError. The shared constructor name/sig are pinned by
        // view_native_names_sigs_and_class_match_view_java. Host-independent constant.
        assert_eq!(IMAGE_VIEW_CLASS.to_str(), "android/widget/ImageView");
        // native_setScaleType(long, int) → `(JI)V`, surfaced 2026-06-05 by multitouch.test's
        // AppCompat ActionBar ImageView (no-op handle-validation). Pinned to the ART-reported sig.
        assert_eq!(
            IMAGE_VIEW_SET_SCALE_TYPE_NAME.to_str(),
            "native_setScaleType"
        );
        assert_eq!(IMAGE_VIEW_SET_SCALE_TYPE_SIG.to_str(), "(JI)V");
        // native_setDrawable(long, long) → `(JJ)V`, surfaced 2026-06-05 by multitouch.test's
        // AppCompat ActionBar ImageView (no-op handle-validation). Pinned to the ART-reported sig.
        assert_eq!(IMAGE_VIEW_SET_DRAWABLE_NAME.to_str(), "native_setDrawable");
        assert_eq!(IMAGE_VIEW_SET_DRAWABLE_SIG.to_str(), "(JJ)V");
    }

    #[test]
    fn image_button_class_is_slashed_internal_name() {
        // Pin android.widget.ImageButton's internal name: AppCompat's Toolbar builds an
        // AppCompatImageButton (extends ImageButton) and ART resolved native_constructor against the
        // ImageButton class (run log 2026-06-05), so register_image_button_natives must find the class
        // by this exact name or RegisterNatives throws NoClassDefFoundError. The shared constructor
        // name/sig are pinned by view_native_names_sigs_and_class_match_view_java. Host-independent.
        assert_eq!(IMAGE_BUTTON_CLASS.to_str(), "android/widget/ImageButton");
        // nativeSetOnClickListener(long) → `(J)V`, surfaced 2026-06-05 by Toolbar nav button (no-op).
        assert_eq!(
            IMAGE_BUTTON_SET_ON_CLICK_LISTENER_NAME.to_str(),
            "nativeSetOnClickListener"
        );
        assert_eq!(IMAGE_BUTTON_SET_ON_CLICK_LISTENER_SIG.to_str(), "(J)V");
    }

    #[test]
    fn drawable_native_name_sig_and_class_match_art_reported() {
        // Pin android.graphics.drawable.Drawable.native_constructor's class, method name, and JNI
        // descriptor against the exact signature ART reported missing (`No implementation found for long
        // android.graphics.drawable.Drawable.native_constructor()`, run log 2026-06-05) + AOSP
        // `Drawable.java`'s `private native long native_constructor();`: a transcription regression
        // would make RegisterNatives throw NoSuchMethodError when a launcher loads a drawable.
        // Host-independent constants.
        assert_eq!(
            DRAWABLE_CLASS.to_str(),
            "android/graphics/drawable/Drawable"
        );
        assert_eq!(
            DRAWABLE_NATIVE_CONSTRUCTOR_NAME.to_str(),
            "native_constructor"
        );
        assert_eq!(DRAWABLE_NATIVE_CONSTRUCTOR_SIG.to_str(), "()J");
        // native_unref(long) → `(J)V`, surfaced 2026-06-05; the drawable peer free callback (no-op).
        assert_eq!(DRAWABLE_NATIVE_UNREF_NAME.to_str(), "native_unref");
        assert_eq!(DRAWABLE_NATIVE_UNREF_SIG.to_str(), "(J)V");
        // Java's Drawable.<init> registers mNativePtr for native-allocation cleanup; it must be non-zero
        // (and is a plainly-non-pointer marker, never dereferenced — see register_drawable_natives docs).
        assert_ne!(DRAWABLE_HANDLE_SENTINEL, 0);
    }

    #[test]
    fn view_group_native_name_sig_and_class_match_view_group_java() {
        // Pin android.view.ViewGroup.native_addView's class, method name, and JNI descriptor against
        // `ViewGroup.java` line 186 (`protected native void native_addView(long widget, long child,
        // int index, LayoutParams params);` → `(JJILandroid/view/ViewGroup$LayoutParams;)V`) and the
        // exact signature ART reported missing (run log 2026-06-05): a transcription regression would
        // make RegisterNatives throw NoSuchMethodError at boot. Host-independent constants.
        assert_eq!(VIEW_GROUP_CLASS.to_str(), "android/view/ViewGroup");
        assert_eq!(VIEW_GROUP_NATIVE_ADD_VIEW_NAME.to_str(), "native_addView");
        assert_eq!(
            VIEW_GROUP_NATIVE_ADD_VIEW_SIG.to_str(),
            "(JJILandroid/view/ViewGroup$LayoutParams;)V"
        );
        // native_removeView(long, long) → `(JJ)V`, surfaced 2026-06-05 by multitouch.test re-parenting
        // its content view (removes the parent→child edge). Pinned to the ART-reported descriptor.
        assert_eq!(
            VIEW_GROUP_NATIVE_REMOVE_VIEW_NAME.to_str(),
            "native_removeView"
        );
        assert_eq!(VIEW_GROUP_NATIVE_REMOVE_VIEW_SIG.to_str(), "(JJ)V");
    }

    /// A hand-built `resources.arsc` for a package whose id is `package_id`, with one type (id 1)
    /// holding one simple entry (id 0) carrying `TYPE_INT_DEC` data 7. Mirrors `arsc::build_fixture`
    /// but is parameterized on the package id so a host-independent framework table (id 0x01) can be
    /// built. Kept local to this guard (the only place needing a 0x01-package fixture).
    fn build_arsc_package(package_id: u32) -> Vec<u8> {
        fn u16(v: &mut Vec<u8>, x: u16) {
            v.extend_from_slice(&x.to_le_bytes());
        }
        fn u32(v: &mut Vec<u8>, x: u32) {
            v.extend_from_slice(&x.to_le_bytes());
        }
        // Empty global value string pool (RES_STRING_POOL_TYPE = 0x0001, header-only 28 bytes).
        let mut pool = Vec::new();
        u16(&mut pool, 0x0001);
        u16(&mut pool, 28);
        u32(&mut pool, 28);
        u32(&mut pool, 0);
        u32(&mut pool, 0);
        u32(&mut pool, 0);
        u32(&mut pool, 28);
        u32(&mut pool, 0);
        // Type chunk (RES_TABLE_TYPE_TYPE = 0x0201): type id 1, one simple entry.
        let mut type_chunk = Vec::new();
        u16(&mut type_chunk, 0x0201);
        u16(&mut type_chunk, 20); // headerSize (0-length config)
        u32(&mut type_chunk, 40); // size = 20 + 4 + 8 + 8
        type_chunk.push(1); // type id 1
        type_chunk.push(0); // res0
        u16(&mut type_chunk, 0); // res1
        u32(&mut type_chunk, 1); // entryCount
        u32(&mut type_chunk, 24); // entriesStart = header(20) + offsets(4)
        u32(&mut type_chunk, 0); // entry 0 offset
        u16(&mut type_chunk, 8); // ResTable_entry size
        u16(&mut type_chunk, 0); // flags (simple)
        u32(&mut type_chunk, 0); // key index
        u16(&mut type_chunk, 8); // Res_value size
        type_chunk.push(0); // res0
        type_chunk.push(0x10); // dataType = TYPE_INT_DEC
        u32(&mut type_chunk, 7); // data
                                 // Package chunk (RES_TABLE_PACKAGE_TYPE = 0x0200): header(8)+id(4)+name[128]u16(256)+
                                 // 4×u32(16) = 284 bytes (matches arsc::PACKAGE_HEADER_MIN), then the type chunk.
        const PKG_HEADER: usize = 284;
        let mut pkg = Vec::new();
        u16(&mut pkg, 0x0200);
        u16(&mut pkg, PKG_HEADER as u16);
        u32(&mut pkg, (PKG_HEADER + type_chunk.len()) as u32);
        u32(&mut pkg, package_id);
        pkg.resize(pkg.len() + 256, 0); // name[128] u16
        u32(&mut pkg, 0); // typeStrings (absent)
        u32(&mut pkg, 0); // lastPublicType
        u32(&mut pkg, 0); // keyStrings (absent)
        u32(&mut pkg, 0); // lastPublicKey
        debug_assert_eq!(pkg.len(), PKG_HEADER);
        pkg.extend_from_slice(&type_chunk);
        // Table chunk (RES_TABLE_TYPE = 0x0002): 12-byte header + pool + package.
        let mut table = Vec::new();
        u16(&mut table, 0x0002);
        u16(&mut table, 12);
        u32(&mut table, (12 + pool.len() + pkg.len()) as u32);
        u32(&mut table, 1); // packageCount
        table.extend_from_slice(&pool);
        table.extend_from_slice(&pkg);
        table
    }

    /// Regression guard for the by-package dispatch (2026-06-05): an id whose high byte is `0x01`
    /// (the framework package, e.g. `android.R.*`) must be served by `framework-res.apk`'s table, not
    /// the app's `resources.arsc` (package `0x7f`). Before the fix, only the app table was loaded, so
    /// every `0x01`-package lookup returned `None`. Builds a host-independent synthetic
    /// `framework-res.apk` in a temp dir (no machine assumptions), points
    /// `ECLIPSE_ANDROID_FRAMEWORK_DIR` at it, and asserts `arsc_bytes_for(0x01…)` yields a table
    /// whose package id is `0x01`.
    #[test]
    fn arsc_bytes_for_routes_framework_package_to_framework_res_apk() {
        use std::io::Write;

        // Unique temp dir holding a dummy api-impl.jar (find_framework requires it) + a synthetic
        // framework-res.apk whose resources.arsc declares package 0x01.
        let dir = std::env::temp_dir().join(format!(
            "eclipse-fwarsc-{}-{:?}",
            std::process::id(),
            std::thread::current().id()
        ));
        std::fs::create_dir_all(&dir).expect("create temp framework dir");
        std::fs::write(dir.join("api-impl.jar"), b"dummy").expect("write api-impl.jar");

        let arsc = build_arsc_package(0x01);
        let apk_bytes = {
            let mut zw = zip::ZipWriter::new(std::io::Cursor::new(Vec::new()));
            let opts: zip::write::FileOptions<'_, ()> = zip::write::FileOptions::default()
                .compression_method(zip::CompressionMethod::Stored);
            zw.start_file("resources.arsc", opts).expect("zip entry");
            zw.write_all(&arsc).expect("write arsc");
            zw.finish().expect("finish zip").into_inner()
        };
        std::fs::write(dir.join("framework-res.apk"), &apk_bytes).expect("write framework-res.apk");

        // SAFETY: set_var is unsafe (Rust 2024); this test owns the var for its duration and removes
        // it before returning. No other test reads ECLIPSE_ANDROID_FRAMEWORK_DIR concurrently here.
        unsafe {
            std::env::set_var("ECLIPSE_ANDROID_FRAMEWORK_DIR", &dir);
        }

        let bytes = arsc_bytes_for(0x0101_0000).expect("framework id routes to a loadable table");
        let table = crate::apk::arsc::parse_arsc(&bytes).expect("framework arsc parses");
        assert_eq!(
            table.package_ids(),
            vec![0x01],
            "high-byte-0x01 id must be served by the framework table (package 0x01)"
        );
        let v = table
            .resource_value(0x0101_0000)
            .expect("framework entry resolves");
        assert_eq!(
            v.data, 7,
            "resolved from the framework table, not the app table"
        );

        unsafe {
            std::env::remove_var("ECLIPSE_ANDROID_FRAMEWORK_DIR");
        }
        let _ = std::fs::remove_dir_all(&dir);
    }
}
